// ═══════════════════════════════════════════════════════════════════════════
//  ISO El Torito Boot Record parser + UEFI → real mode chainloader
// ═══════════════════════════════════════════════════════════════════════════
//
//  All mode-switch code is pre-encoded as const byte arrays and copied
//  to low physical memory before ExitBootServices.  The single #[naked]
//  function does only CLI + LGDT + retfq.

use core::arch::naked_asm;
use core::ffi::c_void;

use crate::disk::read_sector;
use crate::fs::{IsoEntry, FsCtx};
use crate::output::{die, format_u64_buf, halt_or_reboot, print_raw};
use crate::protocol::{
    AllocateType, BlockIoProtocol, BootServices, MemoryDescriptor, MemoryType, SystemTable,
    EFI_SUCCESS,
};

// ═══════════════════════════════════════════════════════════════════════════
//  GDT (3 entries)
// ═══════════════════════════════════════════════════════════════════════════

const GDT_NULL: u64   = 0x0000000000000000;
const GDT_CODE32: u64 = 0x00CF9A000000FFFF; // D=1, L=0, G=1 → 4 GiB, 32-bit
const GDT_CODE16: u64 = 0x00009A000000FFFF; // D=0, L=0, G=0 → 64 KiB, 16-bit

#[repr(C, align(8))]
struct GdtTable([u64; 3]);

#[used]
#[link_section = ".rodata"]
static GDT: GdtTable = GdtTable([GDT_NULL, GDT_CODE32, GDT_CODE16]);

#[repr(C, packed)]
struct Gdtr { limit: u16, base: u64 }

#[used]
#[link_section = ".data"]
static mut GDTR: Gdtr = Gdtr { limit: 0, base: 0 };

// ═══════════════════════════════════════════════════════════════════════════
//  Mode-switch bytecode (32-bit, copied to 0x0500)
// ═══════════════════════════════════════════════════════════════════════════
//
//    mov   eax, cr0
//    and   eax, 0x7FFFFFFF       ; clear PG
//    mov   cr0, eax
//    mov   eax, cr3
//    mov   cr3, eax              ; flush TLB
//    mov   eax, cr4
//    and   eax, ~0x20            ; clear PAE
//    mov   cr4, eax
//    mov   ecx, 0xC0000080
//    rdmsr
//    and   eax, ~0x100           ; clear LME
//    wrmsr
//    jmpf  0x0010:0x00000600

const MODE_SWITCH: [u8; 49] = [
    0x0F, 0x20, 0xC0,                   // mov eax, cr0
    0x25, 0xFF, 0xFF, 0xFF, 0x7F,       // and eax, 0x7FFFFFFF
    0x0F, 0x22, 0xC0,                   // mov cr0, eax
    0x0F, 0x20, 0xD8,                   // mov eax, cr3
    0x0F, 0x22, 0xD8,                   // mov cr3, eax
    0x0F, 0x20, 0xE0,                   // mov eax, cr4
    0x25, 0xDF, 0xFF, 0xFF, 0xFF,       // and eax, ~0x20
    0x0F, 0x22, 0xE0,                   // mov cr4, eax
    0xB9, 0x80, 0x00, 0x00, 0xC0,       // mov ecx, 0xC0000080
    0x0F, 0x32,                         // rdmsr
    0x25, 0xFF, 0xFE, 0xFF, 0xFF,       // and eax, ~0x100
    0x0F, 0x30,                         // wrmsr
    0xEA,
    0x00, 0x06, 0x00, 0x00,
    0x10, 0x00,
];

// ═══════════════════════════════════════════════════════════════════════════
//  TRAMP_A (16-bit protected-mode, copied to 0x0600)
// ═══════════════════════════════════════════════════════════════════════════
//
//    mov   eax, cr0
//    and   eax, ~1                 ; clear PE
//    mov   cr0, eax
//    jmpf  0x0000:0x0614

const TRAMP_A: [u8; 17] = [
    0x66, 0x0F, 0x20, 0xC0,
    0x66, 0x83, 0xE0, 0xFE,
    0x66, 0x0F, 0x22, 0xC0,
    0xEA,
    0x14, 0x06,
    0x00, 0x00,
];

// ═══════════════════════════════════════════════════════════════════════════
//  TRAMP_B (real-mode, copied to 0x0614)
// ═══════════════════════════════════════════════════════════════════════════
//
//    mov   ax, 0
//    mov   ds, ax
//    mov   es, ax
//    mov   fs, ax
//    mov   gs, ax
//    mov   ss, ax
//    mov   sp, 0x7000
//    sti
//    mov   dl, 0x80
//    jmpf  0x0000:0x7C00

const TRAMP_B: [u8; 24] = [
    0xB8, 0x00, 0x00,
    0x8E, 0xD8,
    0x8E, 0xC0,
    0x8E, 0xE0,
    0x8E, 0xE8,
    0x8E, 0xD0,
    0xBC, 0x00, 0x70,
    0xFB,
    0xB2, 0x80,
    0xEA, 0x00, 0x7C, 0x00, 0x00,
];

/// Copy all trampoline bytecodes to low memory.
/// Must be called **before** ExitBootServices.
fn copy_trampolines() {
    unsafe {
        // Initialise GDTR
        GDTR = Gdtr {
            limit: (core::mem::size_of::<GdtTable>() - 1) as u16,
            base: &GDT as *const GdtTable as u64,
        };
        // Copy code
        core::slice::from_raw_parts_mut(0x0500 as *mut u8, MODE_SWITCH.len())
            .copy_from_slice(&MODE_SWITCH);
        core::slice::from_raw_parts_mut(0x0600 as *mut u8, TRAMP_A.len())
            .copy_from_slice(&TRAMP_A);
        core::slice::from_raw_parts_mut(0x0614 as *mut u8, TRAMP_B.len())
            .copy_from_slice(&TRAMP_B);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Mode-switch entry (naked function)
// ═══════════════════════════════════════════════════════════════════════════

#[unsafe(naked)]
unsafe extern "C" fn jump_to_real_mode() -> ! {
    naked_asm!(
        "cli",
        "lgdt [rip + {gdtr}]",
        "mov rax, 0x0500",
        "push 0x08",
        "push rax",
        "retfq",
        gdtr = sym GDTR,
    )
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
    let mut tmp_addr: u64 = 0x100000;
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

    // ── Get memory map ──────────────────────────────────────────
    let mut map_size: usize = 0;
    let mut map_key: u64 = 0;
    let mut desc_size: u64 = 0;
    let mut desc_version: u32 = 0;

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

    map_size += desc_size as usize * 4;
    if map_size > 65536 {
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

    // Copy boot image from temp to 0x7C00
    let boot_addr = 0x7C00usize as *mut u8;
    unsafe {
        for i in 0..total_bytes {
            *boot_addr.add(i) = *dest.add(i);
        }
    }

    // Copy trampolines to low memory (must be done before ExitBootServices
    // so the memory is accessible in long mode identity-mapped region).
    copy_trampolines();

    // Exit boot services
    let image_handle = core::ptr::null_mut::<c_void>();
    let exit_status = unsafe { (bs.exit_boot_services)(image_handle, map_key) };
    if exit_status != EFI_SUCCESS {
        // Try again with updated map key
        unsafe {
            (bs.get_memory_map)(
                &mut map_size,
                map_ptr as *mut MemoryDescriptor,
                &mut map_key,
                &mut desc_size,
                &mut desc_version,
            );
        }
        let _ = unsafe { (bs.exit_boot_services)(image_handle, map_key) };
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
    halt_or_reboot(st)
}