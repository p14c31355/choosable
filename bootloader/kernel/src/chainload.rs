// ═══════════════════════════════════════════════════════════════════════════
//  Chainload: Long Mode → Real Mode transition pipeline
// ═══════════════════════════════════════════════════════════════════════════
//
//  All mode-switch code is pre-encoded as const byte arrays and copied
//  to low physical memory before the transition begins.  The single
//  #[naked] function does only CLI + LGDT + retfq.
//
//  Physical layout after copy:
//    0x0500  MODE_SWITCH  (32-bit code: disable PG/PAE/LME, jmpf 0x10:0x0600)
//    0x0600  TRAMP_A      (16-bit prot code: disable PE,        jmpf 0x00:0x0614)
//    0x0614  TRAMP_B      (real-mode code: init segments,      jmpf 0x00:0x7C00)

use core::arch::naked_asm;

// ═══════════════════════════════════════════════════════════════════════════
//  GDT
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

pub fn init_gdtr() {
    unsafe {
        GDTR = Gdtr {
            limit: (core::mem::size_of::<GdtTable>() - 1) as u16,
            base: &GDT as *const GdtTable as u64,
        };
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  MODE_SWITCH — 32-bit code at 0x0500
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
//    jmpf  0x0010:0x00000600     ; enter 16-bit protected mode

const MODE_SWITCH: [u8; 49] = [
    0x0F, 0x20, 0xC0,                   // mov eax, cr0
    0x25, 0xFF, 0xFF, 0xFF, 0x7F,       // and eax, 0x7FFFFFFF
    0x0F, 0x22, 0xC0,                   // mov cr0, eax
    0x0F, 0x20, 0xD8,                   // mov eax, cr3
    0x0F, 0x22, 0xD8,                   // mov cr3, eax  (flush TLB)
    0x0F, 0x20, 0xE0,                   // mov eax, cr4
    0x25, 0xDF, 0xFF, 0xFF, 0xFF,       // and eax, ~0x20
    0x0F, 0x22, 0xE0,                   // mov cr4, eax
    0xB9, 0x80, 0x00, 0x00, 0xC0,       // mov ecx, 0xC0000080
    0x0F, 0x32,                         // rdmsr
    0x25, 0xFF, 0xFE, 0xFF, 0xFF,       // and eax, ~0x100
    0x0F, 0x30,                         // wrmsr
    0xEA,                               // jmpf
    0x00, 0x06, 0x00, 0x00,             // offset = 0x0600
    0x10, 0x00,                         // selector = 0x10
];

pub fn copy_mode_switch_code() {
    unsafe {
        core::slice::from_raw_parts_mut(0x0500 as *mut u8, MODE_SWITCH.len())
            .copy_from_slice(&MODE_SWITCH);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  TRAMP_A — 16-bit protected-mode code at 0x0600
// ═══════════════════════════════════════════════════════════════════════════
//
//    mov   eax, cr0
//    and   eax, ~1                 ; clear PE → real mode
//    mov   cr0, eax
//    jmpf  0x0000:0x0614           ; enter 16-bit real mode

const TRAMP_A: [u8; 17] = [
    0x66, 0x0F, 0x20, 0xC0,       // mov eax, cr0
    0x66, 0x83, 0xE0, 0xFE,       // and eax, ~1
    0x66, 0x0F, 0x22, 0xC0,       // mov cr0, eax
    0xEA,                         // jmpf
    0x14, 0x06,                   // offset = 0x0614
    0x00, 0x00,                   // segment = 0x0000
];

// ═══════════════════════════════════════════════════════════════════════════
//  TRAMP_B — real-mode code at 0x0614
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

pub fn copy_trampolines() {
    unsafe {
        core::slice::from_raw_parts_mut(0x0600 as *mut u8, TRAMP_A.len())
            .copy_from_slice(&TRAMP_A);
        core::slice::from_raw_parts_mut(0x0614 as *mut u8, TRAMP_B.len())
            .copy_from_slice(&TRAMP_B);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Entry — naked function
// ═══════════════════════════════════════════════════════════════════════════

#[unsafe(naked)]
pub unsafe extern "C" fn do_mode_switch() -> ! {
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