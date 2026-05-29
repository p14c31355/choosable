// ═══════════════════════════════════════════════════════════════════════════
//  ISO El Torito Boot Record parser + UEFI chainloader
// ═══════════════════════════════════════════════════════════════════════════
//
//  Instead of ResetSystem (which re-enters firmware and re-reads the MBR,
//  returning to Choosable), we:
//    1. Load the boot image to conventional memory (0x7C00)
//    2. Get the UEFI memory map
//    3. ExitBootServices
//    4. Transition from long mode → real mode
//    5. Far-jump to 0x7C00

use core::ffi::c_void;

use crate::disk::read_sector;
use crate::fs::{IsoEntry, FsCtx};
use crate::output::{die, format_u64_buf, halt_or_reboot, print_raw};
use crate::protocol::{
    AllocateType, BlockIoProtocol, BootServices, MemoryDescriptor, MemoryType, SystemTable,
    EFI_SUCCESS,
};

// ═══════════════════════════════════════════════════════════════════════════
//  GDT for 64→32→16→real mode transition (same as kernel/iso.rs)
// ═══════════════════════════════════════════════════════════════════════════

core::arch::global_asm!(
    ".align 8",
    "realmode_gdt:",

    // Entry 0 (selector 0x00): null
    ".quad 0",

    // Entry 1 (selector 0x08): 32-bit compat code (base=0, limit=4G)
    ".word 0xFFFF",
    ".word 0x0000",
    ".byte 0x00",
    ".byte 0x9A",
    ".byte 0xCF",
    ".byte 0x00",

    // Entry 2 (selector 0x10): 16-bit code (base=0, limit=64K)
    ".word 0xFFFF",
    ".word 0x0000",
    ".byte 0x00",
    ".byte 0x9A",
    ".byte 0x00",
    ".byte 0x00",

    // Entry 3 (selector 0x18): 16-bit data (base=0, limit=64K)
    ".word 0xFFFF",
    ".word 0x0000",
    ".byte 0x00",
    ".byte 0x92",
    ".byte 0x00",
    ".byte 0x00",

    "realmode_gdt_end:",
    "realmode_gdtr:",
    ".word realmode_gdt_end - realmode_gdt - 1",
    ".quad realmode_gdt",
);

extern "C" {
    fn realmode_gdtr();
    fn realmode_gdt();
}

/// Transition from 64-bit long mode to real mode and jump to 0x7C00.
/// This is called after ExitBootServices.
///
/// SAFETY: Never returns. Destroys the execution environment.
extern "C" {
    fn efi_jump_to_rm();
}

core::arch::global_asm!(
    ".global efi_jump_to_rm",
    "efi_jump_to_rm:",

    "cli",
    "lgdt [rip + realmode_gdtr]",

    "lea rax, [rip + 2f]",
    "push 0x08",
    "push rax",
    "retfq",
    "2:",
    ".code32",

    "mov eax, cr0",
    "and eax, 0x7FFFFFFF",
    "mov cr0, eax",

    "mov eax, cr4",
    "and eax, 0xFFFFFFDF",
    "mov cr4, eax",

    "mov ecx, 0xC0000080",
    "rdmsr",
    "and eax, 0xFFFFFEFF",
    "wrmsr",

    "mov eax, cr0",
    "and eax, 0xFFFFFFFE",
    "mov cr0, eax",

    ".code16",
    // Hand-encoded ljmp $0x10, $3f
    ".byte 0xEA",
    ".long 3f",
    ".word 0x0010",
    "3:",

    "mov ax, 0x18",
    "mov ds, ax",
    "mov es, ax",
    "mov fs, ax",
    "mov gs, ax",
    "mov ss, ax",
    "mov sp, 0x7000",

    "mov dl, 0x80",
    "sti",

    // Hand-encoded ljmp $0, $0x7C00
    ".byte 0xEA",
    ".word 0x7C00",
    ".word 0x0000",
);

unsafe fn jump_to_real_mode() -> ! {
    efi_jump_to_rm();
    loop {
        core::arch::asm!("hlt");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  El Torito parser
// ═══════════════════════════════════════════════════════════════════════════

fn parse_el_torito(
    st: &mut SystemTable,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
) -> Option<(u32, u16)> {
    let boot_rec_lba = iso_lba + 17 * 4;
    let mut boot_rec = [0u8; 512];
    if !read_sector(bio_ref, bio_ptr, mid, boot_rec_lba, &mut boot_rec) {
        print_raw(st, b"Failed to read Boot Record.\r\n\0");
        return None;
    }
    if &boot_rec[1..6] != b"CD001" {
        print_raw(st, b"Invalid Boot Record.\r\n\0");
        return None;
    }

    let catalog_iso_lba =
        u32::from_le_bytes([boot_rec[0x47], boot_rec[0x48], boot_rec[0x49], boot_rec[0x4A]]);
    let catalog_lba = iso_lba + catalog_iso_lba as u64 * 4;

    let mut catalog = [0u8; 512];
    if !read_sector(bio_ref, bio_ptr, mid, catalog_lba, &mut catalog) {
        print_raw(st, b"Failed to read Boot Catalog.\r\n\0");
        return None;
    }

    // Prefer BIOS entry (0x88), fall back to UEFI (0x90)
    let mut best: Option<(u32, u16)> = None;
    for i in 0..16 {
        let off = i * 32;
        match catalog[off] {
            0x88 => {
                let count = u16::from_le_bytes([catalog[off + 6], catalog[off + 7]]);
                let lba = u32::from_le_bytes([
                    catalog[off + 8],
                    catalog[off + 9],
                    catalog[off + 10],
                    catalog[off + 11],
                ]);
                return Some((lba, if count == 0 { 4 } else { count }));
            }
            0x90 if best.is_none() => {
                let count = u16::from_le_bytes([catalog[off + 6], catalog[off + 7]]);
                let lba = u32::from_le_bytes([
                    catalog[off + 8],
                    catalog[off + 9],
                    catalog[off + 10],
                    catalog[off + 11],
                ]);
                best = Some((lba, if count == 0 { 4 } else { count }));
            }
            _ => {}
        }
    }
    if best.is_none() {
        print_raw(st, b"No bootable entry in catalog.\r\n\0");
    }
    best
}

// ═══════════════════════════════════════════════════════════════════════════
//  Chainloader entry point
// ═══════════════════════════════════════════════════════════════════════════

/// Load ISO boot image, exit UEFI boot services, transition to real mode,
/// and jump to the boot image.  Never returns.
pub fn boot_iso(
    st: &mut SystemTable,
    files: &[IsoEntry; 64],
    idx: usize,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
) -> ! {
    print_raw(st, b"\r\nBooting ISO...\r\n\0");

    let iso_lba = files[idx].file_start_lba;

    let (boot_image_iso_lba, sector_count) =
        match parse_el_torito(st, bio_ref, bio_ptr, mid, iso_lba) {
            Some(v) => v,
            None => die(st, b"ERROR: Cannot parse El Torito.\r\n\0"),
        };

    let total_bytes = sector_count as usize * 512;

    let bs = unsafe { &mut *st.boot_services };

    // Allocate pages at 1 MiB as a temporary read buffer
    let num_pages = (total_bytes + 4095) / 4096;
    let mut tmp_addr: u64 = 0x100000; // 1 MiB
    let status = unsafe {
        (bs.allocate_pages)(
            AllocateType::AllocateAddress,
            MemoryType::EfiLoaderData,
            num_pages,
            &mut tmp_addr,
        )
    };
    if status != EFI_SUCCESS {
        die(st, b"ERROR: Cannot allocate temp buffer.\r\n\0");
    }

    // Read boot image sectors into temp buffer
    let boot_image_lba = iso_lba + boot_image_iso_lba as u64 * 4;
    let dest = tmp_addr as *mut u8;
    for s in 0..sector_count {
        let mut sector_buf = [0u8; 512];
        if !read_sector(bio_ref, bio_ptr, mid, boot_image_lba + s as u64, &mut sector_buf) {
            die(st, b"ERROR: Failed to read boot image.\r\n\0");
        }
        unsafe {
            for (j, &b) in sector_buf.iter().enumerate() {
                *dest.add(s as usize * 512 + j) = b;
            }
        }
    }

    print_raw(st, b"Boot image loaded.\r\n\0");

    // ── ExitBootServices ──────────────────────────────────────
    // Get memory map to obtain the map key.
    let mut map_size: usize = 0;
    let mut map_key: u64 = 0;
    let mut desc_size: u64 = 0;
    let mut desc_version: u32 = 0;

    // First call to get required buffer size
    let mut dummy_desc: MemoryDescriptor = MemoryDescriptor {
        ty: 0,
        pad: 0,
        phys_start: 0,
        virt_start: 0,
        num_pages: 0,
        attr: 0,
    };
    unsafe {
        (bs.get_memory_map)(
            &mut map_size,
            &mut dummy_desc,
            &mut map_key,
            &mut desc_size,
            &mut desc_version,
        );
    }

    // Use a 64 KB static buffer for the memory map
    map_size += desc_size as usize * 4;
    if map_size > 65536 {
        map_size = 65536;
        die(st, b"ERROR: Memory map too large.\r\n\0");
    }
    static mut MEM_MAP_BUF: [u8; 65536] = [0u8; 65536];
    let map_ptr = unsafe { MEM_MAP_BUF.as_mut_ptr() };

    unsafe {
        (bs.get_memory_map)(
            &mut map_size,
            map_ptr as *mut MemoryDescriptor,
            &mut map_key,
            &mut desc_size,
            &mut desc_version,
        );
    }

    // Now copy boot image from temp to 0x7C00
    let boot_addr = 0x7C00usize as *mut u8;
    unsafe {
        for i in 0..total_bytes {
            *boot_addr.add(i) = *dest.add(i);
        }
    }

    // Write boot cookie
    let cookie_ptr = 0x7B00usize as *mut u32;
    unsafe {
        *cookie_ptr = 0x544F4F42u32; // "BOOT"
    }

    // Exit boot services
    let image_handle = core::ptr::null_mut::<c_void>(); // We'll pass 0 — UEFI spec allows NULL
    let exit_status = unsafe { (bs.exit_boot_services)(image_handle, map_key) };
    if exit_status != EFI_SUCCESS {
        // Try again — map key may have changed
        unsafe {
            (bs.get_memory_map)(
                &mut map_size,
                map_ptr as *mut MemoryDescriptor,
                &mut map_key,
                &mut desc_size,
                &mut desc_version,
            );
        }
        let exit_status2 = unsafe { (bs.exit_boot_services)(image_handle, map_key) };
        if exit_status2 != EFI_SUCCESS {
            // Last resort: just go for it
        }
    }

    // ── Transition to real mode and jump ──────────────────────
    unsafe { jump_to_real_mode() }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Boot menu
// ═══════════════════════════════════════════════════════════════════════════

use crate::fs::scan_directory;

pub fn show_menu(
    st: &mut SystemTable,
    files: &[IsoEntry; 64],
    count: usize,
    ctx: &FsCtx,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
) -> ! {
    if count == 0 {
        print_raw(st, b"\r\nNo ISO files found on partition 1.\r\n\0");
        halt_or_reboot(st);
    }
    print_raw(st, b"\r\n=== Choosable UEFI Boot Menu ===\r\n\0");
    for i in 0..count.min(20) {
        let (sb, sl) = format_u64_buf((i + 1) as u64);
        print_raw(st, b" ");
        print_raw(st, &sb[20 - sl..]);
        print_raw(st, b". ");
        if files[i].name_len > 0 && files[i].name[0] != 0 {
            print_raw(st, &files[i].name[..files[i].name_len]);
        }
        let size_mb = files[i].file_size / (1024 * 1024);
        let (sb2, sl2) = format_u64_buf(size_mb);
        print_raw(st, b" (");
        print_raw(st, &sb2[20 - sl2..]);
        print_raw(st, b" MiB)\r\n\0");
    }
    print_raw(st, b"Enter number to boot (or 'r' to scan): \0");

    use crate::protocol::{Key, SimpleTextInput};
    loop {
        let mut k = Key { sc: 0, uc: 0 };
        if !st.con_in.is_null() {
            let ci = unsafe { &mut *(st.con_in as *mut SimpleTextInput) };
            if unsafe { (ci.read_key_stroke)(ci as *mut _, &mut k) } != EFI_SUCCESS {
                continue;
            }
        }
        let ch = if k.uc >= 0x20 && k.uc < 0x7F {
            k.uc as u8
        } else {
            match k.sc {
                0x1C => b'\n',
                0x13 => b'r',
                0x1F => b'R',
                _ => 0x00,
            }
        };
        if (b'1'..=b'9').contains(&ch) {
            let idx = (ch - b'1') as usize;
            if idx < count {
                boot_iso(st, files, idx, bio_ref, bio_ptr, mid);
            }
        } else if ch == b'0' && count >= 10 {
            boot_iso(st, files, 9, bio_ref, bio_ptr, mid);
        } else if ch == b'r' || ch == b'R' {
            print_raw(st, b"\r\nRe-scanning...\r\n\0");
            let mut new_files: [IsoEntry; 64] = unsafe { core::mem::zeroed() };
            let mut new_count: usize = 0;
            scan_directory(bio_ref, bio_ptr, mid, ctx, &mut new_files, &mut new_count);
            show_menu(st, &new_files, new_count, ctx, bio_ref, bio_ptr, mid);
        }
    }
    // All inner branches diverge via boot_iso() or recursive show_menu().
    // Satisfy -> ! with an unreachable diverging tail.
    halt_or_reboot(st)
}
