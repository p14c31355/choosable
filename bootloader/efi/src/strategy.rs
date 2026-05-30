// ═══════════════════════════════════════════════════════════════════════════
//  ISO Boot Strategy — detects ISO type and patches grub.cfg accordingly
// ═══════════════════════════════════════════════════════════════════════════

use core::ffi::c_void;

use crate::iso_fs::IsoFsCtx;
use crate::protocol::{BootServices, MemoryType, EFI_SUCCESS};

pub struct PatchInput<'a> {
    pub original: &'a [u8],
    pub iso_name: &'a [u8],
    pub bs: *mut BootServices,
}

pub struct PatchOutput {
    pub buf: *mut u8,
    pub size: usize,
}

pub trait BootStrategy: Sync {
    fn detect(&self, _ctx: &IsoFsCtx) -> bool;
    fn patch(&self, _inp: &PatchInput) -> Option<PatchOutput> { None }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Count the number of `linux `/`linuxefi ` lines in the input.
fn count_linux_lines(data: &[u8]) -> usize {
    let mut count = 0usize;
    let mut pos = 0usize;
    while pos < data.len() {
        let start = pos;
        while pos < data.len() && data[pos] != b'\n' { pos += 1; }
        let line = &data[start..pos];
        let mut trim_start = 0;
        while trim_start < line.len() && (line[trim_start] == b' ' || line[trim_start] == b'\t') {
            trim_start += 1;
        }
        let trimmed = &line[trim_start..];
        if trimmed.starts_with(b"linux ") || trimmed.starts_with(b"linuxefi ") {
            count += 1;
        }
        if pos < data.len() { pos += 1; }
    }
    if count == 0 { 1 } else { count }
}

/// Shared patch logic: inject `pre` + iso_name immediately after the
/// kernel path in `linux`/`linuxefi` lines (right after `vmlinuz`),
/// then truncate output to original file size so GRUB reads all of it.
/// The tail of the original line (file=/cdrom, quiet, splash, maybe-ubiquity, ---)
/// may be clipped by truncation — that's intentional and harmless.
fn patch_common(inp: &PatchInput, pre: &[u8]) -> Option<PatchOutput> {
    let bs = unsafe { &mut *inp.bs };
    let name = inp.iso_name;
    let inj_len = pre.len() + name.len();
    let orig_len = inp.original.len();

    // Buffer: copy of original + injection per linux line + margin
    let new_size = orig_len + inj_len * 4 + 256;
    let mut patch_ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe { (bs.allocate_pool)(MemoryType::EfiLoaderData, new_size, &mut patch_ptr) };
    if status != EFI_SUCCESS || patch_ptr.is_null() {
        return None;
    }
    let out = unsafe { core::slice::from_raw_parts_mut(patch_ptr as *mut u8, new_size) };

    // Build injection string
    let mut inj = [0u8; 256];
    let max_inj = inj_len.min(255);
    inj[..pre.len()].copy_from_slice(pre);
    let nb = name.len().min(max_inj - pre.len());
    inj[pre.len()..pre.len() + nb].copy_from_slice(&name[..nb]);
    let max_inj_final = pre.len() + nb;

    let mut src = 0usize;
    let mut dst = 0usize;
    while src < orig_len {
        let ch = inp.original[src];
        out[dst] = ch;
        dst += 1;
        src += 1;

        // After each complete line, check if it's a linux line and inject
        if ch == b'\n' || src == orig_len {
            // Find line start (after previous newline)
            let line_start = if dst > 0 {
                let mut ls = dst - 1;
                while ls > 0 && out[ls - 1] != b'\n' { ls -= 1; }
                ls
            } else { 0 };

            let line_bytes = &out[line_start..dst];

            // Trim leading whitespace to detect linux / linuxefi
            let is_linux_line = {
                let mut ts = 0;
                while ts < line_bytes.len() && (line_bytes[ts] == b' ' || line_bytes[ts] == b'\t') { ts += 1; }
                let t = &line_bytes[ts..];
                t.starts_with(b"linux ") || t.starts_with(b"linuxefi ")
            };

            if is_linux_line {
                // Find the kernel path token position: right after
                // "linux" SP <kernel_path>
                let mut token_start = line_start;
                while token_start < dst && (out[token_start] == b' ' || out[token_start] == b'\t') { token_start += 1; }
                while token_start < dst && out[token_start] != b' ' && out[token_start] != b'\t' && out[token_start] != b'\n' && out[token_start] != b'\r' { token_start += 1; }
                while token_start < dst && (out[token_start] == b' ' || out[token_start] == b'\t') { token_start += 1; }
                while token_start < dst && out[token_start] != b' ' && out[token_start] != b'\t' && out[token_start] != b'\n' && out[token_start] != b'\r' { token_start += 1; }
                let inject_at = token_start; // right after kernel path

                // Shift everything after inject_at right by max_inj_final
                let suffix_len = dst - inject_at;
                for i in (0..suffix_len).rev() {
                    out[inject_at + max_inj_final + i] = out[inject_at + i];
                }
                out[inject_at..inject_at + max_inj_final].copy_from_slice(&inj[..max_inj_final]);
                dst += max_inj_final;
            }
        }
    }

    // No truncation needed — with directory entry redirect, the patched
    // file can be any size (the extent LBA and Data Length are rewritten).
    let final_len = dst;
    Some(PatchOutput { buf: patch_ptr as *mut u8, size: final_len })
}

// ═══════════════════════════════════════════════════════════════════════════
//  CasperStrategy — Ubuntu / Mint / Pop!_OS / Debian-live
// ═══════════════════════════════════════════════════════════════════════════

pub struct CasperStrategy;

impl BootStrategy for CasperStrategy {
    fn detect(&self, ctx: &IsoFsCtx) -> bool {
        let name = &ctx.iso_name[..ctx.iso_name_len];
        let lower = |b: u8| b | 0x20;
        name.windows(6).any(|w| lower(w[0]) == b'u' && lower(w[1]) == b'b' && lower(w[2]) == b'u' && lower(w[3]) == b'n' && lower(w[4]) == b't' && lower(w[5]) == b'u')
            || name.windows(4).any(|w| lower(w[0]) == b'm' && lower(w[1]) == b'i' && lower(w[2]) == b'n' && lower(w[3]) == b't')
            || name.windows(6).any(|w| lower(w[0]) == b'd' && lower(w[1]) == b'e' && lower(w[2]) == b'b' && lower(w[3]) == b'i' && lower(w[4]) == b'a' && lower(w[5]) == b'n')
            || name.windows(3).any(|w| lower(w[0]) == b'p' && lower(w[1]) == b'o' && lower(w[2]) == b'p')
    }

    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        patch_common(inp, b" rootdelay=5 debug iso-scan/filename=/")
    }
}

unsafe impl Sync for CasperStrategy {}

// ═══════════════════════════════════════════════════════════════════════════
//  LiveOSStrategy — Fedora / RHEL / CentOS (dracut-based)
// ═══════════════════════════════════════════════════════════════════════════

pub struct LiveOSStrategy;

impl BootStrategy for LiveOSStrategy {
    fn detect(&self, ctx: &IsoFsCtx) -> bool {
        let name = &ctx.iso_name[..ctx.iso_name_len];
        let lower = |b: u8| b | 0x20;
        name.windows(6).any(|w| lower(w[0]) == b'f' && lower(w[1]) == b'e' && lower(w[2]) == b'd' && lower(w[3]) == b'o' && lower(w[4]) == b'r' && lower(w[5]) == b'a')
            || name.windows(4).any(|w| lower(w[0]) == b'r' && lower(w[1]) == b'h' && lower(w[2]) == b'e' && lower(w[3]) == b'l')
            || name.windows(6).any(|w| lower(w[0]) == b'c' && lower(w[1]) == b'e' && lower(w[2]) == b'n' && lower(w[3]) == b't' && lower(w[4]) == b'o' && lower(w[5]) == b's')
    }

    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        patch_common(inp, b" rd.live.image rootdelay=15 iso-scan/filename=/")
    }
}

unsafe impl Sync for LiveOSStrategy {}

// ═══════════════════════════════════════════════════════════════════════════
//  Registry
// ═══════════════════════════════════════════════════════════════════════════

static STRATEGIES: &[&dyn BootStrategy] = &[&LiveOSStrategy, &CasperStrategy];

pub fn patch_grub_cfg(ctx: &IsoFsCtx, original: &[u8], bs: *mut BootServices) -> Option<PatchOutput> {
    let strategy: &dyn BootStrategy = STRATEGIES.iter()
        .find(|s| s.detect(ctx))
        .copied()
        .unwrap_or(&CasperStrategy);
    strategy.patch(&PatchInput { original, iso_name: &ctx.iso_name[..ctx.iso_name_len], bs })
}