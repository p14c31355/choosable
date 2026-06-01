// ═══════════════════════════════════════════════════════════════════════════
//  ISO Boot Strategy — patches grub.cfg for filesystem-independent boot
// ═══════════════════════════════════════════════════════════════════════════
//
//  Patches both "linux" and "initrd" lines:
//    linux  → appends "boot=casper rootwait rootdelay=300 debug"
//    initrd → appends " /choosable/premount.cpio"
//
//  The premount cpio is a synthetic file on the virtual CD-ROM that
//  creates a loop device from the raw partition at the known ISO offset.
//  It works on any filesystem (exFAT, NTFS, ext4, ReFS) because it
//  bypasses the filesystem layer entirely via losetup -o $OFFSET.

use core::ffi::c_void;
use crate::iso_fs::IsoFsCtx;
use crate::locator::IsoLocation;
use crate::protocol::{BootServices, MemoryType, EFI_SUCCESS};

pub struct PatchInput<'a> {
    pub original: &'a [u8],
    pub iso_name: &'a [u8],
    pub bs: *mut BootServices,
    pub live_media_uuid: &'a [u8; 10],
    pub iso_location: Option<&'a IsoLocation>,
    /// ISO9660 name of the file whose directory entry was overwritten for
    /// premount cpio injection (e.g. "MD5SUM.TXT" or "UBUNTU").
    /// Used to construct the initrd line injection dynamically so GRUB
    /// finds the synthetic file under its original name.
    pub premount_target_name: &'a [u8],
}

pub struct PatchOutput {
    pub buf: *mut u8,
    pub size: usize,
}

pub trait BootStrategy: Sync {
    fn detect(&self, ctx: &IsoFsCtx) -> bool;
    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> { let _ = inp; None }
}

fn allocate_output(bs: &mut BootServices, orig_len: usize, extra: usize) -> Option<(*mut u8, usize)> {
    let new_size = orig_len + extra + 256;
    let mut ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe { (bs.allocate_pool)(MemoryType::EfiLoaderData, new_size, &mut ptr) };
    if status != EFI_SUCCESS || ptr.is_null() { return None; }
    Some((ptr as *mut u8, new_size))
}

/// Count matching linux/initrd lines in the original grub.cfg to determine
/// how much extra space is needed for all injections.
fn count_matching_lines(orig: &[u8]) -> (usize, usize) {
    let mut linux_count = 0;
    let mut initrd_count = 0;
    let mut pos = 0;
    while pos < orig.len() {
        let start = pos;
        while pos < orig.len() && orig[pos] != b'\n' {
            pos += 1;
        }
        let line = &orig[start..pos];
        let mut ts = 0;
        while ts < line.len() && (line[ts] == b' ' || line[ts] == b'\t') {
            ts += 1;
        }
        let t = &line[ts..];
        if t.starts_with(b"linux ") || t.starts_with(b"linux\t")
            || t.starts_with(b"linuxefi ") || t.starts_with(b"linuxefi\t")
        {
            linux_count += 1;
        } else if t.starts_with(b"initrd ") || t.starts_with(b"initrd\t") {
            initrd_count += 1;
        }
        if pos < orig.len() {
            pos += 1; // skip \n
        }
    }
    (linux_count, initrd_count)
}

fn patch_grub_cfg_impl(inp: &PatchInput, linux_extra: &[u8], premount_target_name: &[u8]) -> Option<PatchOutput> {
    let bs = unsafe { &mut *inp.bs };
    let orig = inp.original;

    // Use the premount target name if set, otherwise fall back to
    // "PREMOUNT.CPIO" — the SFS layer always serves synthetic cpio data
    // under this name when premount_cpio_buf is set.
    let effective_target: &[u8] = if premount_target_name.is_empty() {
        b"PREMOUNT.CPIO"
    } else {
        premount_target_name
    };

    // Build " /<target_name>" for initrd line injection.
    let mut initrd_extra_buf = [0u8; 32];
    initrd_extra_buf[0] = b' ';
    initrd_extra_buf[1] = b'/';
    let name_len = effective_target.len().min(30);
    initrd_extra_buf[2..2 + name_len].copy_from_slice(&effective_target[..name_len]);
    let initrd_extra = &initrd_extra_buf[..2 + name_len];

    // Build "/<target_name>" for dedup check.
    let mut dedup_buf = [0u8; 32];
    dedup_buf[0] = b'/';
    dedup_buf[1..1 + name_len].copy_from_slice(&effective_target[..name_len]);
    let dedup_slice = &dedup_buf[..1 + name_len];

    // Count matching lines first so the output buffer is large enough for
    // all injections (typical grub.cfg has multiple menu entries).
    let (linux_count, initrd_count) = count_matching_lines(orig);
    let extra = linux_count * linux_extra.len() + initrd_count * initrd_extra.len();
    let (out_ptr, out_cap) = allocate_output(bs, orig.len(), extra)?;
    let out = unsafe { core::slice::from_raw_parts_mut(out_ptr, out_cap) };

    let mut src = 0usize;
    let mut dst = 0usize;

    while src < orig.len() {
        let ch = orig[src];
        out[dst] = ch;
        dst += 1;
        src += 1;

        if ch == b'\n' || src == orig.len() {
            let line_start = if dst > 0 {
                let mut ls = dst - 1;
                while ls > 0 && out[ls - 1] != b'\n' { ls -= 1; }
                ls
            } else { 0 };
            let line = &out[line_start..dst];

            // Trim leading whitespace
            let mut ts = 0;
            while ts < line.len() && (line[ts] == b' ' || line[ts] == b'\t') { ts += 1; }
            let t = &line[ts..];

            // ── linux / linuxefi lines ──
            if (t.starts_with(b"linux ") || t.starts_with(b"linux\t")
                || t.starts_with(b"linuxefi ") || t.starts_with(b"linuxefi\t"))
                && !line.windows(18).any(|w| w == b"iso-scan/filename=")
            {
                let inject_at = find_second_arg_end(line_start, out, dst);
                shift_and_inject(out, inject_at, &mut dst, linux_extra);
            }
            // ── initrd lines ──
            else if (t.starts_with(b"initrd ") || t.starts_with(b"initrd\t"))
                && dedup_slice.len() <= line.len()
                && !line.windows(dedup_slice.len()).any(|w| w == dedup_slice)
            {
                // Inject before the line ending:
                //   "initrd /path\n" → "initrd /path /<target_name>\n"
                let mut inject_at = dst;
                // Step back over \n
                if dst > 0 && out[dst - 1] == b'\n' {
                    inject_at -= 1;
                    // Step back over \r if present (\r\n)
                    if dst > 1 && out[dst - 2] == b'\r' {
                        inject_at -= 1;
                    }
                }
                shift_and_inject(out, inject_at, &mut dst, initrd_extra);
            }
        }
    }

    Some(PatchOutput { buf: out_ptr, size: dst })
}

/// Find position after the second whitespace-separated token on the line
/// (linux [kernel_path] → inject after vmlinuz path)
fn find_second_arg_end(line_start: usize, out: &[u8], dst: usize) -> usize {
    let mut pos = line_start;
    while pos < dst && (out[pos] == b' ' || out[pos] == b'\t') { pos += 1; }
    while pos < dst && out[pos] != b' ' && out[pos] != b'\t' && out[pos] != b'\n' && out[pos] != b'\r' { pos += 1; }
    while pos < dst && (out[pos] == b' ' || out[pos] == b'\t') { pos += 1; }
    while pos < dst && out[pos] != b' ' && out[pos] != b'\t' && out[pos] != b'\n' && out[pos] != b'\r' { pos += 1; }
    pos
}

fn shift_and_inject(out: &mut [u8], inject_at: usize, dst: &mut usize, data: &[u8]) {
    let suffix_len = *dst - inject_at;
    for i in (0..suffix_len).rev() {
        out[inject_at + data.len() + i] = out[inject_at + i];
    }
    out[inject_at..inject_at + data.len()].copy_from_slice(data);
    *dst += data.len();
}

// ═══════════════════════════════════════════════════════════════════════════
//  CasperStrategy
// ═══════════════════════════════════════════════════════════════════════════

fn matches_any_lower(name: &[u8], patterns: &[&[u8]]) -> bool {
    patterns.iter().any(|pat| name.windows(pat.len()).any(|w| {
        w.iter().zip(pat.iter()).all(|(&a, &b)| (a | 0x20) == b)
    }))
}

pub struct CasperStrategy;

impl BootStrategy for CasperStrategy {
    fn detect(&self, ctx: &IsoFsCtx) -> bool {
        matches_any_lower(
            &ctx.iso_name[..ctx.iso_name_len],
            &[b"ubuntu", b"mint", b"debian", b"pop"],
        )
    }

    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        patch_grub_cfg_impl(
            inp,
            b" boot=casper break=bottom",
            inp.premount_target_name,
        )
    }
}

unsafe impl Sync for CasperStrategy {}

// ═══════════════════════════════════════════════════════════════════════════
//  LiveOSStrategy
// ═══════════════════════════════════════════════════════════════════════════

pub struct LiveOSStrategy;

impl BootStrategy for LiveOSStrategy {
    fn detect(&self, ctx: &IsoFsCtx) -> bool {
        matches_any_lower(
            &ctx.iso_name[..ctx.iso_name_len],
            &[b"fedora", b"rhel", b"centos"],
        )
    }

    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        patch_grub_cfg_impl(
            inp,
            b" rd.live.image rootdelay=300",
            inp.premount_target_name,
        )
    }
}

unsafe impl Sync for LiveOSStrategy {}

// ═══════════════════════════════════════════════════════════════════════════
//  Registry
// ═══════════════════════════════════════════════════════════════════════════

static STRATEGIES: &[&dyn BootStrategy] = &[&LiveOSStrategy, &CasperStrategy];

pub fn patch_grub_cfg(ctx: &IsoFsCtx, original: &[u8], bs: *mut BootServices, iso_location: Option<&IsoLocation>) -> Option<PatchOutput> {
    let strategy: &dyn BootStrategy = STRATEGIES.iter().find(|s| s.detect(ctx)).copied().unwrap_or(&CasperStrategy);
    strategy.patch(&PatchInput {
        original,
        iso_name: &ctx.iso_name[..ctx.iso_name_len],
        bs,
        live_media_uuid: &ctx.live_media_uuid,
        iso_location,
        premount_target_name: &ctx.premount_target_name[..ctx.premount_target_name_len],
    })
}