// ═══════════════════════════════════════════════════════════════════════════
//  ISO El Torito Boot Record parser + chainloader
// ═══════════════════════════════════════════════════════════════════════════

use crate::ata::{ata_read_sector, ata_read_sectors};
use crate::fs::{DirEntry, FsCtx};
use crate::vga::vga_print;

/// El Torito Boot Catalog Validation Entry (header)
/// Always located at sector 17 (2048 bytes into the ISO)
const BOOT_RECORD_LBA_OFFSET: u64 = 17;

/// Boot cookie address and magic
const BOOT_COOKIE_ADDR: u32 = 0x7B00;
const BOOT_COOKIE_MAGIC: u32 = 0x544F4F42; // "BOOT"

/// Where the ISO boot image gets loaded (conventional memory)
const BIOS_BOOT_ADDR: u32 = 0x7C00;

/// Parse the El Torito Boot Record and return (boot_image_iso_lba, boot_sector_count).
/// Returns false if the file doesn't have a valid Boot Record or Boot Catalog.
fn parse_el_torito(
    iso_lba: u64,
    out_image_lba: &mut u32,
    out_sector_count: &mut u16,
    error_row: &mut usize,
) -> bool {
    // ── Boot Record Validation Entry (sector 17) ──────────────
    let boot_rec_lba = iso_lba + BOOT_RECORD_LBA_OFFSET * 4;
    let mut boot_rec = [0u8; 512];
    if !ata_read_sector(boot_rec_lba, &mut boot_rec) {
        vga_print(*error_row, 2, b"Failed to read Boot Record (sector 17).", 0x0C);
        *error_row += 1;
        return false;
    }
    // Boot Record identifier: byte 0 = 0, bytes 1..6 = "CD001"
    if &boot_rec[1..6] != b"CD001" {
        vga_print(*error_row, 2, b"Invalid Boot Record (no CD001 signature).", 0x0C);
        *error_row += 1;
        return false;
    }

    // Catalog ISO LBA is at offset 0x47 (71) — 4 bytes little-endian
    let catalog_iso_lba =
        u32::from_le_bytes([boot_rec[0x47], boot_rec[0x48], boot_rec[0x49], boot_rec[0x4A]]);
    let catalog_lba = iso_lba + catalog_iso_lba as u64 * 4;

    // ── Boot Catalog ──────────────────────────────────────────
    let mut catalog = [0u8; 512];
    if !ata_read_sector(catalog_lba, &mut catalog) {
        vga_print(*error_row, 2, b"Failed to read Boot Catalog.", 0x0C);
        *error_row += 1;
        return false;
    }

    // Scan for a bootable entry: type 0x88 (BIOS) or 0x90 (UEFI, ignored here)
    for i in 0..(512 / 32) {
        let off = i * 32;
        let etype = catalog[off];
        if etype == 0x88 {
            let count =
                u16::from_le_bytes([catalog[off + 6], catalog[off + 7]]);
            let image_iso_lba = u32::from_le_bytes([
                catalog[off + 8],
                catalog[off + 9],
                catalog[off + 10],
                catalog[off + 11],
            ]);
            *out_sector_count = if count == 0 { 4 } else { count };
            *out_image_lba = image_iso_lba;
            return true;
        }
        // Also accept 0x90 (UEFI), although we'll boot it as BIOS for now
        if etype == 0x90 {
            let count =
                u16::from_le_bytes([catalog[off + 6], catalog[off + 7]]);
            let image_iso_lba = u32::from_le_bytes([
                catalog[off + 8],
                catalog[off + 9],
                catalog[off + 10],
                catalog[off + 11],
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

    // Direct read into 0x7C00 — we're in ring 0 so this is fine
    let dst = unsafe { core::slice::from_raw_parts_mut(BIOS_BOOT_ADDR as *mut u8, total_bytes) };
    if !ata_read_sectors(image_absolute_lba, dst, sector_count as u32) {
        vga_print(10, 2, b"Failed to read boot image sectors.", 0x0C);
        return false;
    }
    true
}

// ═══════════════════════════════════════════════════════════════════════════
//  Protected Mode → Real Mode transition + far jump to 0x7C00
// ═══════════════════════════════════════════════════════════════════════════
//
//  The problem with `outb(0x64, 0xFE)` (CPU reset) is that the BIOS
//  re-initialises everything and reads the MBR again → Choosable starts
//  over.  To actually chainload, we must:
//
//   1. Disable interrupts (CLI)
//   2. Load a GDT with 16-bit real-mode-compatible descriptors
//   3. Jump to 16-bit code segment
//   4. Disable paging / long mode (CR0)
//   5. Load real-mode segment registers
//   6. Set up a real-mode stack (SS:SP)
//   7. Set DL = boot drive (we use 0x80)
//   8. Write boot cookie at 0x7B00
//   9. far JMP 0000:7C00
//
//  This code is written as inline assembly inside a naked function.
// ═══════════════════════════════════════════════════════════════════════════

core::arch::global_asm!(
    ".section .text.realmode_gdt,\"ax\",@progbits",
    ".align 8",
    "realmode_gdt:",

    // Entry 0 (selector 0x00): null descriptor
    ".quad 0",

    // Entry 1 (selector 0x08): 32-bit compat code segment
    // Base=0, Limit=0xFFFFF, D=1 (32-bit), L=0 (not 64-bit)
    ".word 0xFFFF",   // limit low (bits 0-15)
    ".word 0x0000",   // base low (bits 0-15)
    ".byte 0x00",     // base mid (bits 16-23)
    ".byte 0x9A",     // access: P=1, DPL=0, S=1, Type=1010 (code/exec/read)
    ".byte 0xCF",     // flags: G=1 (4K granularity), D=1 (32-bit), limit high=0xF
    ".byte 0x00",     // base high (bits 24-31)

    // Entry 2 (selector 0x10): 16-bit code segment (real-mode compatible)
    // Base=0, Limit=0xFFFF, D=0 (16-bit)
    ".word 0xFFFF",   // limit low
    ".word 0x0000",   // base low
    ".byte 0x00",     // base mid
    ".byte 0x9A",     // access: P=1, DPL=0, S=1, Type=1010 (code/exec/read)
    ".byte 0x00",     // flags: G=0 (byte granularity), D=0 (16-bit), limit high=0
    ".byte 0x00",     // base high

    // Entry 3 (selector 0x18): 16-bit data segment (real-mode compatible)
    // Base=0, Limit=0xFFFF
    ".word 0xFFFF",
    ".word 0x0000",
    ".byte 0x00",
    ".byte 0x92",     // access: P=1, DPL=0, S=1, Type=0010 (data/read/write)
    ".byte 0x00",
    ".byte 0x00",

    "realmode_gdt_end:",
    "realmode_gdtr:",
    ".word realmode_gdt_end - realmode_gdt - 1",
    ".quad realmode_gdt",
);

extern "C" {
    fn realmode_gdtr();
    fn do_chainload();
}

core::arch::global_asm!(
    ".global do_chainload",
    "do_chainload:",

    // ── 1. Disable interrupts ─────────────────────────
    "cli",

    // ── 2. Load our GDT ───────────────────────────────
    "lgdt [rip + realmode_gdtr]",

    // ── 3. Switch to 32-bit compat mode (selector 0x08) ─
    "lea rax, [rip + 2f]",
    "push 0x08",
    "push rax",
    "retfq",
    "2:",
    ".code32",

    // ── 4. Disable paging, PAE, and long mode ────────
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

    // ── 5. Disable protected mode → real mode ────────
    "mov eax, cr0",
    "and eax, 0xFFFFFFFE",
    "mov cr0, eax",

    // ── 6. Set up real-mode segments ─────────────────
    ".code16",
    // Far jump to 16-bit code segment (selector 0x10)
    // ljmp $0x10, $3f — hand-encoded:
    //   opcode 0xEA, 4-byte offset (3f), 2-byte selector (0x0010)
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

    // ── 7. Re-enable interrupts ──────────────────────
    "sti",

    // ── 8. Set DL = boot drive ───────────────────────
    "mov dl, 0x80",

    // ── 9. Jump to ISO boot image at 0x7C00 ─────────
    // ljmp $0, $0x7C00 — hand-encoded:
    //   opcode 0xEA, 2-byte offset (0x7C00), 2-byte selector (0x0000)
    ".byte 0xEA",
    ".word 0x7C00",
    ".word 0x0000",
);

/// Write boot cookie and invoke the global-asm chainloader.
/// Never returns.
#[no_mangle]
pub fn chainload_iso() -> ! {
    let cookie_ptr = BOOT_COOKIE_ADDR as *mut u32;
    unsafe {
        *cookie_ptr = BOOT_COOKIE_MAGIC;
        do_chainload();
    }
    loop {
        unsafe { core::arch::asm!("hlt") }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Public API: boot_iso
// ═══════════════════════════════════════════════════════════════════════════

/// Load and chainload an ISO file.  Called from the boot menu.
/// Never returns.
pub fn boot_iso(file: &DirEntry, _ctx: &FsCtx) -> ! {
    use crate::vga::vga_clear;

    vga_clear(0x0E);
    vga_print(2, 5, b"Loading ISO boot sector...", 0x0F);

    let iso_lba = file.file_start_lba;
    let mut error_row: usize = 4;

    let mut boot_image_iso_lba: u32 = 0;
    let mut boot_sector_count: u16 = 4;

    if !parse_el_torito(
        iso_lba,
        &mut boot_image_iso_lba,
        &mut boot_sector_count,
        &mut error_row,
    ) {
        // parse_el_torito already printed error messages.
        // Halt — caller (menu) would need to handle fallback but
        // boot_iso never returns.
        vga_print(error_row + 2, 2, b"ISO boot failed. Press any key to halt...", 0x07);
        while crate::kbd::kbd_wait_key() == 0 {}
        loop {
            unsafe { core::arch::asm!("hlt") }
        }
    }

    vga_print(7, 5, b"Loading boot image...", 0x07);

    if !load_boot_image(iso_lba, boot_image_iso_lba, boot_sector_count) {
        vga_print(10, 2, b"ISO boot failed. Press any key to halt...", 0x07);
        while crate::kbd::kbd_wait_key() == 0 {}
        loop {
            unsafe { core::arch::asm!("hlt") }
        }
    }

    vga_print(10, 5, b"Chainloading...", 0x0F);

    // chainload_iso() handles real-mode transition and far-jump.
    // It never returns.
    chainload_iso()
}
