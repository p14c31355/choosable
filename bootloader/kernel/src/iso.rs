// ═══════════════════════════════════════════════════════════════════════════
//  ISO El Torito Boot Record parser + chainloader entry point
// ═══════════════════════════════════════════════════════════════════════════

use crate::ata::{ata_read_sector, ata_read_sectors};
use crate::chainload;
use crate::fs::{DirEntry, FsCtx};
use crate::vga::vga_print;

/// El Torito Boot Catalog Validation Entry (header)
const BOOT_RECORD_LBA_OFFSET: u64 = 17;

/// Where the ISO boot image gets loaded (conventional memory)
const BIOS_BOOT_ADDR: u32 = 0x7C00;

/// Parse the El Torito Boot Record and return (boot_image_iso_lba, boot_sector_count).
fn parse_el_torito(
    iso_lba: u64,
    out_image_lba: &mut u32,
    out_sector_count: &mut u16,
    error_row: &mut usize,
) -> bool {
    let boot_rec_lba = iso_lba + BOOT_RECORD_LBA_OFFSET * 4;
    let mut boot_rec = [0u8; 512];
    if !ata_read_sector(boot_rec_lba, &mut boot_rec) {
        vga_print(*error_row, 2, b"Failed to read Boot Record (sector 17).", 0x0C);
        *error_row += 1;
        return false;
    }
    if &boot_rec[1..6] != b"CD001" {
        vga_print(*error_row, 2, b"Invalid Boot Record (no CD001 signature).", 0x0C);
        *error_row += 1;
        return false;
    }

    let catalog_iso_lba =
        u32::from_le_bytes([boot_rec[0x47], boot_rec[0x48], boot_rec[0x49], boot_rec[0x4A]]);
    let catalog_lba = iso_lba + catalog_iso_lba as u64 * 4;

    let mut catalog = [0u8; 512];
    if !ata_read_sector(catalog_lba, &mut catalog) {
        vga_print(*error_row, 2, b"Failed to read Boot Catalog.", 0x0C);
        *error_row += 1;
        return false;
    }

    for i in 0..(512 / 32) {
        let off = i * 32;
        let etype = catalog[off];
        if etype == 0x88 || etype == 0x90 {
            let count = u16::from_le_bytes([catalog[off + 6], catalog[off + 7]]);
            let image_iso_lba = u32::from_le_bytes([
                catalog[off + 8], catalog[off + 9], catalog[off + 10], catalog[off + 11],
            ]);
            *out_sector_count = if count == 0 { 4 } else { count };
            *out_image_lba = image_iso_lba;
            return true;
        }
    }
    vga_print(*error_row, 2, b"No bootable entry (type 0x88/0x90) in catalog.", 0x0C);
    *error_row += 1;
    false
}

/// Read ISO boot image sectors from disk into memory at BIOS_BOOT_ADDR.
fn load_boot_image(iso_lba: u64, image_iso_lba: u32, sector_count: u16) -> bool {
    let image_absolute_lba = iso_lba + image_iso_lba as u64 * 4;
    let total_bytes = sector_count as usize * 512;
    let dst = unsafe { core::slice::from_raw_parts_mut(BIOS_BOOT_ADDR as *mut u8, total_bytes) };
    if !ata_read_sectors(image_absolute_lba, dst, sector_count as u32) {
        vga_print(10, 2, b"Failed to read boot image sectors.", 0x0C);
        return false;
    }
    true
}

/// Copy pre-encoded trampolines to low memory, then initiate the mode-switch
/// pipeline: long mode → 32-bit compat → 16-bit prot → real mode → 0x7C00.
/// Never returns.
fn chainload_iso() -> ! {
    chainload::copy_mode_switch_code();
    chainload::copy_trampolines();
    unsafe { chainload::do_mode_switch() }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Public API
// ═══════════════════════════════════════════════════════════════════════════

/// Load and chainload an ISO file.  Called from the boot menu.  Never returns.
pub fn boot_iso(file: &DirEntry, _ctx: &FsCtx) -> ! {
    use crate::vga::vga_clear;

    vga_clear(0x0E);
    vga_print(2, 5, b"Loading ISO boot sector...", 0x0F);

    let iso_lba = file.file_start_lba;
    let mut error_row: usize = 4;
    let mut boot_image_iso_lba: u32 = 0;
    let mut boot_sector_count: u16 = 4;

    if !parse_el_torito(iso_lba, &mut boot_image_iso_lba, &mut boot_sector_count, &mut error_row) {
        vga_print(error_row + 2, 2, b"ISO boot failed. Press any key to halt...", 0x07);
        while crate::kbd::kbd_wait_key() == 0 {}
        loop { unsafe { core::arch::asm!("hlt") } }
    }

    vga_print(7, 5, b"Loading boot image...", 0x07);
    if !load_boot_image(iso_lba, boot_image_iso_lba, boot_sector_count) {
        vga_print(10, 2, b"ISO boot failed. Press any key to halt...", 0x07);
        while crate::kbd::kbd_wait_key() == 0 {}
        loop { unsafe { core::arch::asm!("hlt") } }
    }

    vga_print(10, 5, b"Chainloading...", 0x0F);
    chainload_iso()
}