#![no_std]
#![no_main]

mod ata;
mod fs;
mod io;
mod iso;
mod kbd;
mod menu;
mod part;
mod vga;

use core::panic::PanicInfo;
use fs::{scan_filesystem, DirEntry, FsCtx, FsType};

// ═══════════════════════════════════════════════════════════════════════════
//  Entry point
// ═══════════════════════════════════════════════════════════════════════════

#[no_mangle]
pub extern "C" fn _start() -> ! {
    vga::vga_clear(0x07);
    vga::vga_print(0, 0, b"  Choosable Kernel v0.4  ", 0x1F);
    vga::vga_print(1, 0, b"==========================", 0x17);

    let (parts, part_count) = part::read_partitions();
    if part_count == 0 {
        vga::vga_print(5, 2, b"No partitions found. Halted.", 0x0C);
        loop {
            unsafe { core::arch::asm!("hlt") }
        }
    }

    vga::vga_print(2, 0, b"Reading partition 1...", 0x07);

    let part1_lba = parts[0].start_lba;
    let mut vbr = [0u8; 512];
    if !ata::ata_read_sector(part1_lba as u64, &mut vbr) {
        vga::vga_print(3, 2, b"Failed to read VBR.", 0x0C);
        loop {
            unsafe { core::arch::asm!("hlt") }
        }
    }

    // Detect filesystem
    let fs = if &vbr[3..11] == b"EXFAT   " {
        FsType::Exfat
    } else if &vbr[3..11] == b"NTFS    " {
        FsType::Ntfs
    } else if &vbr[0x52..0x5A] == b"FAT32   " {
        FsType::Fat32
    } else {
        vga::vga_print(3, 2, b"Unknown filesystem.", 0x0C);
        loop {
            unsafe { core::arch::asm!("hlt") }
        }
    };

    // Parse BPB into FsCtx
    let mut ctx = FsCtx {
        fs,
        part1_lba,
        spc: 0,
        fat_start: 0,
        fat_len: 0,
        heap_start: 0,
        root_cluster: 0,
        mft_start_lba: 0,
        sectors_per_cluster: 0,
        mft_record_size: 0,
    };

    match fs {
        FsType::Exfat => {
            let spc_shift = vbr[109] as u32;
            if spc_shift > 16 {
                vga::vga_print(3, 2, b"Invalid SectorsPerClusterShift.", 0x0C);
                loop {
                    unsafe { core::arch::asm!("hlt") }
                }
            }
            let cluster_bytes = (1u32 << spc_shift) * 512;
            let fat_off =
                u32::from_le_bytes([vbr[80], vbr[81], vbr[82], vbr[83]]) as u64;
            let fat_len =
                u32::from_le_bytes([vbr[84], vbr[85], vbr[86], vbr[87]]) as u64;
            let heap_off =
                u32::from_le_bytes([vbr[88], vbr[89], vbr[90], vbr[91]]) as u64;
            let root_cluster =
                u32::from_le_bytes([vbr[96], vbr[97], vbr[98], vbr[99]]);

            ctx.spc = cluster_bytes / 512;
            ctx.fat_start = part1_lba as u64 + fat_off;
            ctx.fat_len = fat_len;
            ctx.heap_start = part1_lba as u64 + heap_off;
            ctx.root_cluster = root_cluster;

            vga::vga_print(4, 0, b"exFAT detected. Scanning...", 0x07);
        }
        FsType::Fat32 => {
            let spc = vbr[13] as u32; // sectors per cluster
            if spc == 0 {
                vga::vga_print(3, 2, b"Invalid sectors per cluster.", 0x0C);
                loop {
                    unsafe { core::arch::asm!("hlt") }
                }
            }
            let reserved = u16::from_le_bytes([vbr[14], vbr[15]]) as u64;
            let num_fats = vbr[16] as u64;
            let fat_sectors =
                u32::from_le_bytes([vbr[36], vbr[37], vbr[38], vbr[39]])
                    as u64;
            let root_cluster =
                u32::from_le_bytes([vbr[44], vbr[45], vbr[46], vbr[47]]);

            let fat_start = part1_lba as u64 + reserved as u64;
            let data_start = fat_start + num_fats * fat_sectors;

            ctx.spc = spc;
            ctx.fat_start = fat_start;
            ctx.fat_len = fat_sectors;
            ctx.heap_start = data_start;
            ctx.root_cluster = root_cluster;

            vga::vga_print(4, 0, b"FAT32 detected. Scanning...", 0x07);
        }
        FsType::Ntfs => {
            let spc = vbr[13] as u32; // sectors per cluster
            if spc == 0 {
                vga::vga_print(3, 2, b"Invalid sectors per cluster.", 0x0C);
                loop {
                    unsafe { core::arch::asm!("hlt") }
                }
            }
            let cluster_bytes = spc as u64 * 512;
            // MFT start cluster is at offset 0x30 (48) in NTFS BPB
            let mft_lcn =
                i64::from_le_bytes(vbr[0x30..0x38].try_into().unwrap());
            let mft_start_lba =
                part1_lba as u64 + (mft_lcn as u64) * spc as u64;
            // MFT record size: clus_per_mft_record at offset 0x40 (64)
            let cpmr_raw = vbr[0x40] as i8;
            let mft_record_size: u64 = if cpmr_raw > 0 {
                cpmr_raw as u64 * cluster_bytes
            } else if cpmr_raw >= -12 {
                1u64 << (-cpmr_raw)
            } else {
                0
            };
            if mft_record_size == 0 || mft_record_size > 4096 {
                vga::vga_print(3, 2, b"Invalid MFT record size.", 0x0C);
                loop {
                    unsafe { core::arch::asm!("hlt") }
                }
            }

            ctx.spc = spc;
            ctx.sectors_per_cluster = spc;
            ctx.mft_start_lba = mft_start_lba;
            ctx.mft_record_size = mft_record_size;
            ctx.heap_start = part1_lba as u64;

            vga::vga_print(4, 0, b"NTFS detected. Scanning...", 0x07);
        }
    }

    // Scan root directory for .iso files
    let mut iso_files: [DirEntry; 64] = unsafe { core::mem::zeroed() };
    let mut iso_count: usize = 0;
    scan_filesystem(&ctx, &mut iso_files, &mut iso_count);

    // Show menu → boot_iso() → chainload_iso() (never returns)
    menu::show_menu(&iso_files, iso_count, &ctx);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("hlt") }
    }
}