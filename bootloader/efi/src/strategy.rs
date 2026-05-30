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

// ═══════════════════════════════════════════════════════════════════════════
//  CasperStrategy — Ubuntu / Mint / Pop!_OS / Debian-live
// ═══════════════════════════════════════════════════════════════════════════

pub struct CasperStrategy;

impl BootStrategy for CasperStrategy {
    fn detect(&self, _ctx: &IsoFsCtx) -> bool { true }

    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        let bs = unsafe { &mut *inp.bs };
        let name = inp.iso_name;

        // Injection: " rootdelay=15 iso-scan/filename=/NAME.iso"
        let pre = b" rootdelay=15 iso-scan/filename=/";
        let inj_len = pre.len() + name.len();

        // Count actual linux/linuxefi lines for exact allocation
        let linux_lines = count_linux_lines(inp.original);

        // Header to fix "variable root isn't set": "set root=cd0\n"
        let header = b"set root=cd0\n";
        let new_size = header.len() + inp.original.len() + inj_len * linux_lines + 128;

        let mut patch_ptr: *mut c_void = core::ptr::null_mut();
        let status = unsafe { (bs.allocate_pool)(MemoryType::EfiLoaderData, new_size, &mut patch_ptr) };
        if status != EFI_SUCCESS || patch_ptr.is_null() {
            return None;
        }
        let out = unsafe { core::slice::from_raw_parts_mut(patch_ptr as *mut u8, new_size) };

        // Copy header
        out[..header.len()].copy_from_slice(header);
        let mut off = header.len();

        // Build injection string
        let mut inj = [0u8; 256];
        let max_inj = inj_len.min(255);
        inj[..pre.len()].copy_from_slice(pre);
        let nb = name.len().min(max_inj - pre.len());
        inj[pre.len()..pre.len() + nb].copy_from_slice(&name[..nb]);

        let max_inj_final = pre.len() + nb;

        // Walk original and inject into linux/linuxefi lines
        let mut src = 0usize;
        let mut dst = off;
        while src < inp.original.len() {
            let ch = inp.original[src];
            out[dst] = ch;
            dst += 1;

            if ch == b'\n' || src == inp.original.len() - 1 {
                let line_start = if dst > off {
                    let mut ls = dst - 1;
                    while ls > off && out[ls - 1] != b'\n' { ls -= 1; }
                    ls
                } else { off };

                let line_end = dst;
                let line_slice = &out[line_start..line_end];
                // Trim leading whitespace — linux/linuxefi lines are often indented
                let mut trim_start = 0;
                while trim_start < line_slice.len() && (line_slice[trim_start] == b' ' || line_slice[trim_start] == b'\t') {
                    trim_start += 1;
                }
                let trimmed = &line_slice[trim_start..];
                if trimmed.starts_with(b"linux ") || trimmed.starts_with(b"linuxefi ") {
                    // Find injection point: before "---" if present, else at end
                    let mut inject_at = if ch == b'\n' { dst - 1 } else { dst };
                    if line_end > line_start + 4 {
                        let mut search = line_end - 4;
                        while search > line_start {
                            if out[search] == b'-' && out[search+1] == b'-' && out[search+2] == b'-' {
                                if search == line_start || out[search-1] == b' ' {
                                    inject_at = search;
                                    break;
                                }
                            }
                            search -= 1;
                        }
                    }
                    let suffix_len = dst - inject_at;
                    for i in (0..suffix_len).rev() {
                        out[inject_at + max_inj_final + i] = out[inject_at + i];
                    }
                    out[inject_at..inject_at + max_inj_final].copy_from_slice(&inj[..max_inj_final]);
                    dst += max_inj_final;
                }
            }
            src += 1;
        }

        Some(PatchOutput { buf: patch_ptr as *mut u8, size: dst })
    }
}

unsafe impl Sync for CasperStrategy {}

// ═══════════════════════════════════════════════════════════════════════════
//  LiveOSStrategy — Fedora / RHEL / CentOS (dracut-based)
// ═══════════════════════════════════════════════════════════════════════════

pub struct LiveOSStrategy;

impl BootStrategy for LiveOSStrategy {
    fn detect(&self, _ctx: &IsoFsCtx) -> bool { true }

    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        let bs = unsafe { &mut *inp.bs };
        let name = inp.iso_name;
        let pre = b" rd.live.image rootdelay=15 iso-scan/filename=/";
        let inj_len = pre.len() + name.len();

        let linux_lines = count_linux_lines(inp.original);

        let header = b"set root=cd0\n";
        let new_size = header.len() + inp.original.len() + inj_len * linux_lines + 128;

        let mut patch_ptr: *mut c_void = core::ptr::null_mut();
        let status = unsafe { (bs.allocate_pool)(MemoryType::EfiLoaderData, new_size, &mut patch_ptr) };
        if status != EFI_SUCCESS || patch_ptr.is_null() { return None; }
        let out = unsafe { core::slice::from_raw_parts_mut(patch_ptr as *mut u8, new_size) };

        out[..header.len()].copy_from_slice(header);
        let mut off = header.len();

        let mut inj = [0u8; 256];
        let max_inj = inj_len.min(255);
        inj[..pre.len()].copy_from_slice(pre);
        let nb = name.len().min(max_inj - pre.len());
        inj[pre.len()..pre.len() + nb].copy_from_slice(&name[..nb]);
        let max_inj_final = pre.len() + nb;

        let mut src = 0usize;
        let mut dst = off;
        while src < inp.original.len() {
            let ch = inp.original[src];
            out[dst] = ch;
            dst += 1;
            if ch == b'\n' || src == inp.original.len() - 1 {
                let line_start = if dst > off {
                    let mut ls = dst - 1;
                    while ls > off && out[ls - 1] != b'\n' { ls -= 1; }
                    ls
                } else { off };
                let line_end = dst;
                let line_slice = &out[line_start..line_end];
                // Trim leading whitespace — linux/linuxefi lines are often indented
                let mut trim_start = 0;
                while trim_start < line_slice.len() && (line_slice[trim_start] == b' ' || line_slice[trim_start] == b'\t') {
                    trim_start += 1;
                }
                let trimmed = &line_slice[trim_start..];
                if trimmed.starts_with(b"linux ") || trimmed.starts_with(b"linuxefi ") {
                    let mut inject_at = if ch == b'\n' { dst - 1 } else { dst };
                    if line_end > line_start + 4 {
                        let mut search = line_end - 4;
                        while search > line_start {
                            if out[search] == b'-' && out[search+1] == b'-' && out[search+2] == b'-' {
                                if search == line_start || out[search-1] == b' ' {
                                    inject_at = search;
                                    break;
                                }
                            }
                            search -= 1;
                        }
                    }
                    let suffix_len = dst - inject_at;
                    for i in (0..suffix_len).rev() {
                        out[inject_at + max_inj_final + i] = out[inject_at + i];
                    }
                    out[inject_at..inject_at + max_inj_final].copy_from_slice(&inj[..max_inj_final]);
                    dst += max_inj_final;
                }
            }
            src += 1;
        }

        Some(PatchOutput { buf: patch_ptr as *mut u8, size: dst })
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