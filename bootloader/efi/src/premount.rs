// ═══════════════════════════════════════════════════════════════════════════
//  Premount initrd builder — embeds /init.choosable static ELF binary
// ═══════════════════════════════════════════════════════════════════════════
//
//  The binary is compiled separately via x86_64-unknown-linux-musl:
//     cd bootloader/premount-init && cargo build --target x86_64-unknown-linux-musl --release
//
//  EFI build.rs automatically builds it before compiling the EFI module.

use core::ffi::c_void;
use crate::boot_context::BootContext;
use crate::protocol::{BootServices, MemoryType, EFI_SUCCESS};

pub struct PremountBundle {
    pub cpio_buf: *mut u8,
    pub cpio_size: usize,
    pub cpio_alloc_size: usize,
    pub iso_offset_bytes: u64,
}

pub trait EarlyBootFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle>;
}

// ── Fixup structs (one per distro family) ────────────────────────────

macro_rules! fixup {
    ($($name:ident),+ $(,)?) => {
        $(
            pub struct $name;
            impl EarlyBootFixup for $name {
                fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle> {
                    let p = ctx.selected_payload()?;
                    let rel = p.file_start_lba - ctx.partition_start_lba;
                    build_premount_cpio(bs, rel)
                }
            }
        )+
    };
}

fixup!(CasperFixup, LiveBootFixup, DracutFixup, AlpinePremountFixup, ArchFixup, AlpineFixup);

pub struct WindowsPEFixup;
impl EarlyBootFixup for WindowsPEFixup {
    fn build_initrd(&self, _ctx: &BootContext, _bs: &mut BootServices) -> Option<PremountBundle> { None }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Embedded premount-init binary
// ═══════════════════════════════════════════════════════════════════════════
//  Pre-compiled with x86_64-unknown-linux-musl for fully-static ELF.
//  Runs as /init.choosable in any initramfs (no /bin/sh required).

const PREMOUNT_INIT_BIN: &[u8] = include_bytes!("../premount-init.bin");

// ═══════════════════════════════════════════════════════════════════════════
//  CPIO helpers
// ═══════════════════════════════════════════════════════════════════════════

fn hex_nibble(n: u8) -> u8 {
    if n < 10 { b'0' + n } else { b'A' + (n - 10) }
}

fn cpio_entry_size(name_len: usize, data_len: usize) -> usize {
    let padded_name_len = ((110 + name_len + 1 + 3) & !3) - 110;
    110 + padded_name_len + data_len + 3
}

fn cpio_newc_header(buf: &mut [u8], name: &[u8], file_size: u32, mode: u32) -> usize {
    buf[..6].copy_from_slice(b"070701");
    let name_len = name.len() as u32 + 1;
    let padded_name_len = ((110 + name_len as usize + 3) & !3) - 110;
    let fields: [u32; 13] = [1, mode, 0, 0, 1, 0, file_size, 0, 0, 0, 0, name_len, 0];
    let mut pos = 6usize;
    for &v in &fields {
        for shift in [28, 24, 20, 16, 12, 8, 4, 0] {
            buf[pos] = hex_nibble(((v >> shift) & 0xF) as u8);
            pos += 1;
        }
    }
    buf[pos..pos + name.len()].copy_from_slice(name);
    pos += name.len();
    buf[pos] = 0; pos += 1;
    while pos < 110 + padded_name_len { buf[pos] = 0; pos += 1; }
    110 + padded_name_len
}

fn cpio_append_entry(cpio: &mut [u8], off: &mut usize, name: &[u8], data: &[u8], mode: u32) -> bool {
    let nlen = name.len() + 1;
    let pnlen = ((110 + nlen + 3) & !3) - 110;
    let hlen = 110 + pnlen;
    let pad = (4 - ((*off + hlen + data.len()) & 3)) & 3;
    if *off + hlen + data.len() + pad > cpio.len() { return false; }
    *off += cpio_newc_header(&mut cpio[*off..], name, data.len() as u32, mode);
    cpio[*off..*off + data.len()].copy_from_slice(data);
    *off += data.len();
    for _ in 0..pad { cpio[*off] = 0; *off += 1; }
    true
}

/// Build a minimal CPIO containing the /init.choosable ELF binary.
fn build_premount_cpio(bs: &mut BootServices, relative_sector_offset: u64) -> Option<PremountBundle> {
    let offset_bytes = relative_sector_offset * 512;
    let names: &[&[u8]] = &[b"init.choosable"];
    let data: &[&[u8]] = &[PREMOUNT_INIT_BIN];

    let estimate = names.iter().zip(data.iter())
        .map(|(&n, &d)| cpio_entry_size(n.len(), d.len()) + 8)
        .sum::<usize>()
        + cpio_entry_size(10, 0) + 8;
    let alloc_size = (estimate + 2047) & !2047;

    let mut cpio_ptr: *mut c_void = core::ptr::null_mut();
    if unsafe { (bs.allocate_pool)(MemoryType::EfiLoaderData, alloc_size, &mut cpio_ptr) } != EFI_SUCCESS || cpio_ptr.is_null() { return None; }
    let cpio = unsafe { core::slice::from_raw_parts_mut(cpio_ptr as *mut u8, alloc_size) };
    let mut off = 0usize;

    for (&name, &d) in names.iter().zip(data.iter()) {
        if !cpio_append_entry(cpio, &mut off, name, d, 0o100755) {
            unsafe { (bs.free_pool)(cpio_ptr); }
            return None;
        }
    }
    if !cpio_append_entry(cpio, &mut off, b"TRAILER!!!", b"", 0) {
        unsafe { (bs.free_pool)(cpio_ptr); }
        return None;
    }
    Some(PremountBundle { cpio_buf: cpio_ptr as *mut u8, cpio_size: off, cpio_alloc_size: alloc_size, iso_offset_bytes: offset_bytes })
}

// ── Public entry points (for backward compatibility) ─────────────────

macro_rules! prep_initrd_fn {
    ($($fn:ident),+ $(,)?) => {
        $( pub fn $fn(bs: &mut BootServices, rel: u64, _iso_name: &[u8]) -> Option<PremountBundle> { build_premount_cpio(bs, rel) } )+
    };
}

prep_initrd_fn!(prepare_generic_initrd, prepare_arch_initrd, prepare_alpine_initrd, prepare_dracut_initrd);

pub fn prepare_premount_initrd(bs: &mut BootServices, rel: u64, _sr: bool, _iso_name: &[u8]) -> Option<PremountBundle> {
    build_premount_cpio(bs, rel)
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_nibble() {
        assert_eq!(hex_nibble(0), b'0');
        assert_eq!(hex_nibble(9), b'9');
        assert_eq!(hex_nibble(10), b'A');
        assert_eq!(hex_nibble(15), b'F');
    }

    #[test]
    fn test_cpio_entry_size() {
        let sz = cpio_entry_size(10, 0);
        assert!(sz >= 110 + 1 + 10 + 1);
    }

    #[test]
    fn test_cpio_newc_header_trailer() {
        let mut buf = [0u8; 128];
        let sz = cpio_newc_header(&mut buf, b"TRAILER!!!", 0, 0);
        assert!(sz > 0 && sz < 128);
        assert_eq!(&buf[..6], b"070701");
    }
}