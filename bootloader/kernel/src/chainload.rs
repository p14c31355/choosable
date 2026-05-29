// ═══════════════════════════════════════════════════════════════════════════
//  Chainload: Protected Mode → Real Mode transition + far jump to 0x7C00
// ═══════════════════════════════════════════════════════════════════════════
//
//  Strategy (minimum inline assembly):
//   1. GDT + realmode trampoline are Rust static / const byte arrays.
//   2. Before the mode switch, Rust code copies the trampoline to low
//      memory (0x0600) and writes the boot cookie at 0x7B00.
//   3. A single #[naked] function with inline asm performs:
//        CLI → LGDT → far-jump to 32-bit compat mode →
//        disable PG / PAE / LME / PE → far-jump to 0x0000:0x0600.
//
//  The key insight: we **never** switch to a 16-bit code segment with
//  limit=0xFFFF.  Instead we stay in the 32-bit code segment (limit=4 GiB)
//  through the entire transition, then far-jump directly to the pre-copied
//  low-memory trampoline after clearing CR0.PE.

use core::arch::naked_asm;

// ── GDT ────────────────────────────────────────────────────────────────
// Only 3 entries (null, 32-bit code, 32-bit data).

const GDT_NULL: u64 = 0x0000000000000000;

/// 32-bit code: base=0, limit=4 GiB (G=1 → 4K granularity), D=1
const GDT_CODE32: u64 = 0x00CF9A000000FFFF;

/// 32-bit data: base=0, limit=4 GiB (G=1), writable
const GDT_DATA32: u64 = 0x00CF92000000FFFF;

#[repr(C, align(8))]
struct GdtTable([u64; 3]);

#[used]
#[link_section = ".rodata"]
static GDT: GdtTable = GdtTable([GDT_NULL, GDT_CODE32, GDT_DATA32]);

/// GDTR pseudo-descriptor: 2-byte limit + 8-byte base.
#[repr(C, packed)]
struct Gdtr {
    limit: u16,
    base: u64,
}

#[used]
#[link_section = ".data"]
static mut GDTR: Gdtr = Gdtr {
    limit: 0,
    base: 0,
};

/// Initialise the GDTR with the runtime address of the GDT.
/// Must be called once before do_mode_switch().
pub fn init_gdtr() {
    unsafe {
        GDTR = Gdtr {
            limit: (core::mem::size_of::<GdtTable>() - 1) as u16,
            base: &GDT as *const GdtTable as u64,
        };
    }
}

// ── Realmode trampoline bytecode ──────────────────────────────────────
//
// Position-independent 16-bit code.  Copied to physical 0x0600 before
// the mode switch.  Executed as the first real-mode code after the
// far-jump from unreal mode.
//
// Assembly (Intel syntax):
//   mov  ax, 0x0000
//   mov  ds, ax
//   mov  es, ax
//   mov  fs, ax
//   mov  gs, ax
//   mov  ss, ax
//   mov  sp, 0x7000
//   sti
//   mov  dl, 0x80               ; boot drive = first hard disk
//   ljmp 0x0000:0x7C00          ; chainload ISO boot image

const TRAMPOLINE: [u8; 24] = [
    0xB8, 0x00, 0x00,             // mov ax, 0
    0x8E, 0xD8,                   // mov ds, ax
    0x8E, 0xC0,                   // mov es, ax
    0x8E, 0xE0,                   // mov fs, ax
    0x8E, 0xE8,                   // mov gs, ax
    0x8E, 0xD0,                   // mov ss, ax
    0xBC, 0x00, 0x70,             // mov sp, 0x7000
    0xFB,                         // sti
    0xB2, 0x80,                   // mov dl, 0x80
    0xEA, 0x00, 0x7C, 0x00, 0x00, // ljmp 0x0000:0x7C00
];

/// Copy the real-mode trampoline to its final location at 0x0600.
pub fn copy_trampoline() {
    unsafe {
        let dst = core::slice::from_raw_parts_mut(0x0600 as *mut u8, TRAMPOLINE.len());
        dst.copy_from_slice(&TRAMPOLINE);
    }
}

// ── Mode switch ───────────────────────────────────────────────────────

/// Transition from 64-bit long mode → real mode → chainload to 0x7C00.
///
/// **Preconditions** (caller must ensure):
/// - `copy_trampoline()` has been called.
/// - Boot cookie (0x544F4F42) has been written to physical address 0x7B00.
/// - `init_gdtr()` has been called.
///
/// Never returns.
#[unsafe(naked)]
pub unsafe extern "C" fn do_mode_switch() -> ! {
    naked_asm!(
        // 1. Disable interrupts
        "cli",

        // 2. Load GDT from our static GDTR (RIP-relative addressing)
        "lgdt [rip + {gdtr}]",

        // 3. Far jump to 32-bit compat mode (selector 0x08)
        "lea rax, [rip + 2f]",
        "push 0x08",
        "push rax",
        "retfq",

        // ── We are now in 32-bit compat mode ──────────────────────
        // CS.D=1, limit=4 GiB → can execute 32-bit code anywhere in memory
        "2:",
        ".code32",

        // 4. Disable paging (CR0.PG)
        "mov eax, cr0",
        "and eax, 0x7FFFFFFF",
        "mov cr0, eax",

        // 5. Disable PAE (CR4.PAE)
        "mov eax, cr4",
        "and eax, 0xFFFFFFDF",
        "mov cr4, eax",

        // 6. Disable long mode (EFER.LME)
        "mov ecx, 0xC0000080",
        "rdmsr",
        "and eax, 0xFFFFFEFF",
        "wrmsr",

        // 7. Disable protected mode (CR0.PE)
        //    After this instruction the CPU is in "unreal mode" —
        //    real-mode semantics but the CS descriptor cache still
        //    holds D=1 / limit=4 GiB, so we can execute the far-jump.
        "mov eax, cr0",
        "and eax, 0xFFFFFFFE",
        "mov cr0, eax",

        // 8. Far jump to real-mode trampoline at 0x0000:0x0600
        //    EA [32-bit offset] [16-bit segment]
        //    D=1 means default operand size is 32 bits, so the
        //    offset field is 4 bytes (0x0600) and the segment is
        //    2 bytes (0x0000).  This loads CS = segment*16 = 0
        //    and EIP = offset = 0x0600, beginning trampoline execution.
        ".byte 0xEA",
        ".long 0x0600",
        ".word 0x0000",

        gdtr = sym GDTR,
    )
}
