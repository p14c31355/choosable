// ═══════════════════════════════════════════════════════════════════════════
//  ISO Boot Strategy — detects ISO type and patches grub.cfg accordingly
// ═══════════════════════════════════════════════════════════════════════════

use core::ffi::c_void;

use crate::iso_fs::IsoFsCtx;
use crate::protocol::{BootServices, MemoryType, EFI_SUCCESS};

/// Information about the ISO and its grub.cfg used during patch generation.
pub struct PatchInput<'a> {
    pub original: &'a [u8],
    pub iso_name: &'a [u8],
    pub bs: *mut BootServices,
}

/// Result of patching: pool-allocated buffer and its size.
pub struct PatchOutput {
    pub buf: *mut u8,
    pub size: usize,
}

// ═══════════════════════════════════════════════════════════════════════════
//  BootStrategy trait
// ═══════════════════════════════════════════════════════════════════════════

/// Detects the ISO distribution and generates the appropriate grub.cfg patch.
/// # Safety
/// Implementors must be Sync (no interior mutability) to be stored in static.
pub trait BootStrategy: Sync {
    /// Verify this strategy applies to the ISO.
    fn detect(&self, _ctx: &IsoFsCtx) -> bool;

    /// Patch grub.cfg.  Returns pool-allocated replacement, or None.
    fn patch(&self, _inp: &PatchInput) -> Option<PatchOutput> { None }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Common helper: inject a string into every "linux" kernel command line
// ═══════════════════════════════════════════════════════════════════════════

/// Search the original for "linux "/ "linuxefi " lines and inject `injection`
/// bytes right before the line-ending newline (or at the end if no newline).
fn inject_into_linux_lines(
    bs: *mut BootServices,
    original: &[u8],
    injection: &[u8],
) -> Option<PatchOutput> {
    let inj_len = injection.len();
    let bs_ref = unsafe { &mut *bs };

    // Count how many lines we need to patch
    let mut num_lines = 0usize;
    let mut i = 0;
    while i + 8 <= original.len() {
        // need at least 8 bytes ahead to check "linuxefi "
        if &original[i..i+6] == b"linux " || &original[i..i+8] == b"linuxefi" {
            num_lines += 1;
        }
        // advance to next line
        while i < original.len() && original[i] != b'\n' {
            i += 1;
        }
        if i < original.len() { i += 1; }
    }
    // also check if the last 6-7 bytes might be "linux " / "linuxefi"
    {
        let mut j = original.len().saturating_sub(7);
        while j < original.len() {
            if j + 6 <= original.len() && &original[j..j+6] == b"linux " { num_lines += 1; }
            if j + 8 <= original.len() && &original[j..j+8] == b"linuxefi" { num_lines += 1; }
            j += 1;
        }
    }
    if num_lines == 0 {
        return None; // nothing to patch
    }

    let new_size = original.len() + num_lines * inj_len + num_lines;
    let mut ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs_ref.allocate_pool)(MemoryType::EfiLoaderData, new_size, &mut ptr)
    };
    if status != EFI_SUCCESS || ptr.is_null() {
        return None;
    }
    let out = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, new_size) };

    let mut src = 0usize;
    let mut dst = 0usize;

    while src < original.len() {
        let ch = original[src];
        out[dst] = ch;
        dst += 1;

        if ch == b'\n' || src == original.len() - 1 {
            // This is the end of a line. Check if the line started with "linux " / "linuxefi "
            let line_start = if dst > 0 {
                let mut ls = dst - 1;
                while ls > 0 && out[ls - 1] != b'\n' {
                    ls -= 1;
                }
                ls
            } else {
                0
            };

            let line = &out[line_start..dst];
            let is_kernel_line = line.starts_with(b"linux ")
                || line.starts_with(b"linuxefi ");

            if is_kernel_line {
                // Inject right before the final character (which is \n or the last byte)
                let saved = out[dst - 1];
                dst -= 1;
                out[dst..dst + inj_len].copy_from_slice(injection);
                dst += inj_len;
                out[dst] = saved;
                dst += 1;
            }
        }

        src += 1;
    }

    Some(PatchOutput {
        buf: ptr as *mut u8,
        size: dst,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
//  CasperStrategy — Ubuntu / Mint / Pop!_OS / Debian-live
// ═══════════════════════════════════════════════════════════════════════════

pub struct CasperStrategy;

impl BootStrategy for CasperStrategy {
    fn detect(&self, _ctx: &IsoFsCtx) -> bool {
        true // default strategy, always matches
    }

    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        let bs = inp.bs;
        let bs_ref = unsafe { &mut *bs };
        let name = inp.iso_name;
        let param = b" iso-scan/filename=/";
        let inj_len = param.len() + name.len();

        let mut inj_ptr: *mut c_void = core::ptr::null_mut();
        let status = unsafe {
            (bs_ref.allocate_pool)(MemoryType::EfiLoaderData, inj_len, &mut inj_ptr)
        };
        if status != EFI_SUCCESS || inj_ptr.is_null() {
            return None;
        }
        let inj = unsafe { core::slice::from_raw_parts_mut(inj_ptr as *mut u8, inj_len) };
        inj[..param.len()].copy_from_slice(param);
        inj[param.len()..].copy_from_slice(name);

        let result = inject_into_linux_lines(bs, inp.original, inj);

        // Free the temporary injection buffer
        unsafe { (bs_ref.free_pool)(inj_ptr); }

        result
    }
}

/// Required for storing in static
unsafe impl Sync for CasperStrategy {}

// ═══════════════════════════════════════════════════════════════════════════
//  LiveOSStrategy — Fedora / RHEL / CentOS (dracut-based)
// ═══════════════════════════════════════════════════════════════════════════

pub struct LiveOSStrategy;

impl BootStrategy for LiveOSStrategy {
    fn detect(&self, _ctx: &IsoFsCtx) -> bool {
        true // detected via /LiveOS
    }

    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        let bs = inp.bs;
        let bs_ref = unsafe { &mut *bs };
        let name = inp.iso_name;
        let prefix = b" rd.live.image iso-scan/filename=/";
        let inj_len = prefix.len() + name.len();

        let mut inj_ptr: *mut c_void = core::ptr::null_mut();
        let status = unsafe {
            (bs_ref.allocate_pool)(MemoryType::EfiLoaderData, inj_len, &mut inj_ptr)
        };
        if status != EFI_SUCCESS || inj_ptr.is_null() {
            return None;
        }
        let inj = unsafe { core::slice::from_raw_parts_mut(inj_ptr as *mut u8, inj_len) };
        inj[..prefix.len()].copy_from_slice(prefix);
        inj[prefix.len()..].copy_from_slice(name);

        let result = inject_into_linux_lines(bs, inp.original, inj);
        unsafe { (bs_ref.free_pool)(inj_ptr); }
        result
    }
}

unsafe impl Sync for LiveOSStrategy {}

// ═══════════════════════════════════════════════════════════════════════════
//  Public registry
// ═══════════════════════════════════════════════════════════════════════════

static STRATEGIES: &[&dyn BootStrategy] = &[
    &LiveOSStrategy,
    &CasperStrategy,
];

pub fn detect(_ctx: &IsoFsCtx) -> &dyn BootStrategy {
    for s in STRATEGIES {
        if s.detect(_ctx) {
            return *s;
        }
    }
    &CasperStrategy
}

pub fn patch_grub_cfg(
    ctx: &IsoFsCtx,
    original: &[u8],
    bs: *mut BootServices,
) -> Option<PatchOutput> {
    let strategy = detect(ctx);
    let inp = PatchInput {
        original,
        iso_name: &ctx.iso_name[..ctx.iso_name_len],
        bs,
    };
    strategy.patch(&inp)
}