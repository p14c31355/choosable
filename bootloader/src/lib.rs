//! Choosable bootloader crate.
//!
//! Provides:
//! - `BOOT_IMG`: 440-byte MBR boot sector (x86 machine code, const array)
//! - `STAGE2_BIN`: Stage2 BIOS bootloader flat binary
//! - `EFI_BIN`: UEFI bootloader PE binary (BOOTX64.EFI)
//!
//! No external assembler dependencies — all machine code is generated
//! by build.rs using a pure Rust x86 instruction encoder.

pub mod mbr;
pub mod stage2;
pub mod efi;

// Include auto-generated constants from build.rs
include!(concat!(env!("OUT_DIR"), "/generated.rs"));