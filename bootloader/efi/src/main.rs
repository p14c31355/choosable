#![no_std]
#![no_main]

mod disk;
mod fs;
mod iso;
mod iso_fs;
mod output;
mod protocol;
mod strategy;
mod virtual_blockio;

use core::ffi::c_void;
use core::panic::PanicInfo;
use disk::{find_disk_handle, find_gpt_data_partition, read_sector};
use fs::{scan_directory, FsCtx, FsType, IsoEntry};
use output::{banner, die, print_hex, print_raw};
use protocol::{
    BlockIoProtocol, SystemTable, BLOCK_IO_PROTOCOL_GUID, EFI_SUCCESS,
};

#[no_mangle]
extern "efiapi" fn efi_main(
    image_handle: *mut c_void,
    system_table: *mut SystemTable,
) -> ! {
    let st = unsafe { &mut *system_table };
    let bs = unsafe { &mut *st.boot_services };

    banner(st);

    let disk_handle = match find_disk_handle(bs, image_handle) {
        Some(h) => h,
        None => die(st, b"ERROR: No disk device found.\r\n\0"),
    };

    let mut bio: *mut BlockIoProtocol = core::ptr::null_mut();
    if unsafe {
        (bs.handle_protocol)(
            disk_handle,
            &BLOCK_IO_PROTOCOL_GUID,
            &mut bio as *mut _ as _,
        )
    } != EFI_SUCCESS
        || bio.is_null()
    {
        die(st, b"ERROR: No Block I/O on disk.\r\n\0");
    }
    let bio_ptr = bio;
    let bio_ref = unsafe { &*bio };
    let mid = if !bio_ref.media.is_null() {
        unsafe { (*bio_ref.media).mid }
    } else {
        0
    };

    // Read MBR
    let mut mbr: [u8; 512] = [0; 512];
    if !read_sector(bio_ref, bio_ptr, mid, 0, &mut mbr) {
        die(st, b"ERROR: Cannot read MBR.\r\n\0");
    }

    // Find partition 1
    let mut part1_lba: u64 = 0;
    let mut is_gpt = false;
    for i in 0..4 {
        let off = 446 + i * 16;
        let fs_type = mbr[off + 4];
        let lba =
            u32::from_le_bytes([mbr[off + 8], mbr[off + 9], mbr[off + 10], mbr[off + 11]]);
        let sec = u32::from_le_bytes([
            mbr[off + 12],
            mbr[off + 13],
            mbr[off + 14],
            mbr[off + 15],
        ]);
        if fs_type == 0xEE && sec > 0 {
            is_gpt = true;
        }
        if sec == 0 || fs_type == 0xEE {
            continue;
        }
        part1_lba = lba as u64;
        break;
    }

    if part1_lba == 0 && is_gpt {
        print_raw(st, b"GPT detected, searching for data partition...\r\n\0");
        part1_lba = find_gpt_data_partition(st, bio_ref, bio_ptr, mid);
    }
    if part1_lba == 0 {
        die(st, b"ERROR: No partition 1 found.\r\n\0");
    }

    // Read partition 1 VBR
    let mut vbr: [u8; 512] = [0; 512];
    if !read_sector(bio_ref, bio_ptr, mid, part1_lba, &mut vbr) {
        die(st, b"ERROR: Cannot read partition 1.\r\n\0");
    }

    // Detect filesystem type
    let fs = if &vbr[3..11] == b"EXFAT   " {
        FsType::Exfat
    } else if &vbr[3..11] == b"NTFS    " {
        FsType::Ntfs
    } else if &vbr[0x52..0x5A] == b"FAT32   " {
        FsType::Fat32
    } else {
        // Fallback: check FAT32 at 0x52
        if &vbr[0x52..0x5A] == b"FAT32   " {
            FsType::Fat32
        } else {
            print_raw(
                st,
                b"Unknown filesystem on partition 1.\r\n\0",
            );
            print_hex(
                st,
                b"  First 16 bytes: ",
                u64::from_le_bytes(vbr[0..8].try_into().unwrap()),
            );
            print_hex(
                st,
                b"  ",
                u64::from_le_bytes(vbr[8..16].try_into().unwrap()),
            );
            print_raw(st, b"\r\n\0");
            output::halt_or_reboot(st);
        }
    };

    // Parse BPB
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
        bytes_per_cluster: 0,
        mft_record_size: 0,
    };

    match fs {
        FsType::Exfat => {
            let spc_shift = vbr[109] as u32;
            if spc_shift > 16 {
                die(st, b"ERROR: Invalid SectorsPerClusterShift.\r\n\0");
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
            ctx.fat_start = part1_lba + fat_off;
            ctx.fat_len = fat_len;
            ctx.heap_start = part1_lba + heap_off;
            ctx.root_cluster = root_cluster;

            print_raw(st, b"exFAT detected. Scanning...\r\n\0");
        }
        FsType::Fat32 => {
            let spc = vbr[13] as u32; // sectors per cluster
            if spc == 0 {
                die(st, b"ERROR: Invalid sectors per cluster.\r\n\0");
            }
            let reserved = u16::from_le_bytes([vbr[14], vbr[15]]) as u64;
            let num_fats = vbr[16] as u64;
            let fat_sectors =
                u32::from_le_bytes([vbr[36], vbr[37], vbr[38], vbr[39]]) as u64;
            let root_cluster =
                u32::from_le_bytes([vbr[44], vbr[45], vbr[46], vbr[47]]);

            let fat_start = part1_lba + reserved;
            let data_start = fat_start + num_fats * fat_sectors;

            ctx.spc = spc;
            ctx.fat_start = fat_start;
            ctx.fat_len = fat_sectors;
            ctx.heap_start = data_start;
            ctx.root_cluster = root_cluster;

            print_raw(st, b"FAT32 detected. Scanning...\r\n\0");
        }
        FsType::Ntfs => {
            let spc = vbr[13] as u32; // sectors per cluster
            if spc == 0 {
                die(st, b"ERROR: Invalid sectors per cluster.\r\n\0");
            }
            let cluster_bytes = spc as u64 * 512;
            // MFT start cluster is at offset 0x30 (48) in NTFS BPB
            let mft_lcn =
                i64::from_le_bytes(vbr[0x30..0x38].try_into().unwrap());
            let mft_start_lba =
                part1_lba + (mft_lcn as u64) * spc as u64;
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
                die(st, b"ERROR: Invalid MFT record size.\r\n\0");
            }

            ctx.spc = spc;
            ctx.sectors_per_cluster = spc;
            ctx.bytes_per_cluster = cluster_bytes;
            ctx.mft_start_lba = mft_start_lba;
            ctx.mft_record_size = mft_record_size;
            ctx.heap_start = part1_lba; // partition start (NTFS doesn't use heap_start)

            print_raw(st, b"NTFS detected. Scanning...\r\n\0");
        }
    }

    // Scan root directory
    let mut iso_count: usize = 0;
    let mut iso_files: [IsoEntry; 64] = unsafe { core::mem::zeroed() };
    scan_directory(bio_ref, bio_ptr, mid, &ctx, &mut iso_files, &mut iso_count);

    // Show menu — never returns
    iso::show_menu(st, image_handle, disk_handle, &iso_files, iso_count, &ctx, bio_ref, bio_ptr, mid);
    // Unreachable, but the compiler needs explicit diverging:
    loop { unsafe { core::arch::asm!("hlt") } }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("hlt") }
    }
}