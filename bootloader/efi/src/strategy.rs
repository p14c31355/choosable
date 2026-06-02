// ═══════════════════════════════════════════════════════════════════════════
//  ISO Boot Strategy — patches grub.cfg for filesystem-independent boot
// ═══════════════════════════════════════════════════════════════════════════

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

fn allocate_output(bs: &mut BootServices, size: usize) -> Option<(*mut u8, usize)> {
    let mut ptr: *mut c_void = core::ptr::null_mut();
    if unsafe { (bs.allocate_pool)(MemoryType::EfiLoaderData, size, &mut ptr) } != EFI_SUCCESS || ptr.is_null() { return None; }
    Some((ptr as *mut u8, size))
}

fn matches_any_lower(name: &[u8], patterns: &[&[u8]]) -> bool {
    patterns.iter().any(|pat| name.windows(pat.len()).any(|w| {
        w.iter().zip(pat.iter()).all(|(&a, &b)| (a | 0x20) == b)
    }))
}

// ── Helpers: find the extent of a `key=value` token in a line ─────
fn find_token_extent(line: &[u8], key: &[u8]) -> Option<(usize, usize)> {
    // returns (strip_start, strip_end) where strip_start may include
    // a leading space.
    if let Some(pos) = line.windows(key.len()).position(|w| w == key) {
        let mut end = pos + key.len();
        while end < line.len() && line[end] != b' ' && line[end] != b'\t'
            && line[end] != b'\n' && line[end] != b'\r'
        { end += 1; }
        let start = if pos > 0 && line[pos - 1] == b' ' { pos - 1 } else { pos };
        Some((start, end))
    } else {
        None
    }
}

/// Check whether a line starts with a linux/linuxefi command (after trimming).
fn is_linux_line(line: &[u8]) -> bool {
    let mut ts = 0;
    while ts < line.len() && (line[ts] == b' ' || line[ts] == b'\t') { ts += 1; }
    if ts >= line.len() { return false; }
    let t = &line[ts..];
    t.starts_with(b"linux ") || t.starts_with(b"linux\t")
        || t.starts_with(b"linuxefi ") || t.starts_with(b"linuxefi\t")
}

/// Compute total extra bytes needed for injections (minus stripped tokens).
fn compute_extra(
    orig: &[u8],
    linux_extra: &[u8],
    linux_eol_extra: &[u8],
    initrd_extra: &[u8],
    initrd_dedup: &[u8],
) -> usize {
    let mut extra: isize = 0;
    let strip_keys: &[&[u8]] = &[b"iso-scan/filename=", b"findiso="];
    let mut pos = 0;
    while pos < orig.len() {
        let ls = pos;
        while pos < orig.len() && orig[pos] != b'\n' { pos += 1; }
        let line = &orig[ls..pos];

        if is_linux_line(line) {
            extra += linux_extra.len() as isize;
            if !linux_eol_extra.is_empty() {
                extra += linux_eol_extra.len() as isize;
            }
            for &key in strip_keys {
                if let Some((s, e)) = find_token_extent(line, key) {
                    extra -= (e - s) as isize;
                }
            }
        } else {
            let mut ts = 0;
            while ts < line.len() && (line[ts] == b' ' || line[ts] == b'\t') { ts += 1; }
            if ts < line.len() && (line[ts..].starts_with(b"initrd ") || line[ts..].starts_with(b"initrd\t"))
                && initrd_dedup.len() <= line.len()
                && !line.windows(initrd_dedup.len()).any(|w| w == initrd_dedup)
            {
                extra += initrd_extra.len() as isize;
            }
        }
        if pos < orig.len() { pos += 1; }
    }
    if extra < 0 { extra = 0; }
    extra as usize
}

fn patch_grub_cfg_impl(
    inp: &PatchInput,
    linux_extra: &[u8],
    linux_eol_extra: &[u8],
    premount_target_name: &[u8],
) -> Option<PatchOutput> {
    let bs = unsafe { &mut *inp.bs };
    let orig = inp.original;

    let effective_target: &[u8] = if premount_target_name.is_empty() { b"PREMOUNT.CPIO" } else { premount_target_name };

    let mut initrd_extra_buf = [0u8; 32];
    initrd_extra_buf[0] = b' '; initrd_extra_buf[1] = b'/';
    let name_len = effective_target.len().min(30);
    initrd_extra_buf[2..2 + name_len].copy_from_slice(&effective_target[..name_len]);
    let initrd_extra = &initrd_extra_buf[..2 + name_len];

    let mut dedup_buf = [0u8; 32];
    dedup_buf[0] = b'/';
    dedup_buf[1..1 + name_len].copy_from_slice(&effective_target[..name_len]);
    let initrd_dedup = &dedup_buf[..1 + name_len];

    // Build eol extra with path
    let iso_path: Option<&[u8]> = inp.iso_location.map(|loc| loc.path());
    let mut eol_buf = [0u8; 320];
    let eol_extra_dynamic: &[u8] = if !linux_eol_extra.is_empty() && linux_eol_extra.ends_with(b"=") {
        if let Some(path) = iso_path {
            let plen = linux_eol_extra.len();
            let pl = path.len().min(320 - plen);
            eol_buf[..plen].copy_from_slice(linux_eol_extra);
            eol_buf[plen..plen + pl].copy_from_slice(&path[..pl]);
            &eol_buf[..plen + pl]
        } else { linux_eol_extra }
    } else { linux_eol_extra };

    let extra = compute_extra(orig, linux_extra, eol_extra_dynamic, initrd_extra, initrd_dedup);
    let (out_ptr, out_cap) = allocate_output(bs, orig.len() + extra + 256)?;
    let out = unsafe { core::slice::from_raw_parts_mut(out_ptr, out_cap) };

    let strip_keys: &[&[u8]] = &[b"iso-scan/filename=", b"findiso="];
    let mut src = 0usize;
    let mut dst = 0usize;

    while src < orig.len() {
        // look ahead to find if this is a linux line with tokens to strip
        if is_linux_line(&orig[src..]) {
            // find end of line
            let mut line_end = src;
            while line_end < orig.len() && orig[line_end] != b'\n' { line_end += 1; }
            let line = &orig[src..line_end];

            // Compute which byte ranges to skip
            let mut skip_ranges: [(usize, usize); 4] = [(0,0), (0,0), (0,0), (0,0)];
            let mut sr_count = 0;
            for &key in strip_keys {
                if let Some((s, e)) = find_token_extent(line, key) {
                    if sr_count < 4 {
                        skip_ranges[sr_count] = (s, e);
                        sr_count += 1;
                    }
                }
            }
            // sort by start
            for i in 0..sr_count {
                for j in i+1..sr_count {
                    if skip_ranges[i].0 > skip_ranges[j].0 {
                        let tmp = skip_ranges[i];
                        skip_ranges[i] = skip_ranges[j];
                        skip_ranges[j] = tmp;
                    }
                }
            }

            // Copy line byte-by-byte, skipping stripped ranges
            let mut lp = 0usize;
            while lp < line.len() {
                let ch = line[lp];
                // check if lp falls in any skip range
                let mut skip = false;
                for i in 0..sr_count {
                    if lp >= skip_ranges[i].0 && lp < skip_ranges[i].1 {
                        skip = true;
                        // advance lp to end of this skip range
                        lp = skip_ranges[i].1;
                        break;
                    }
                }
                if skip { continue; }
                out[dst] = ch;
                dst += 1;
                lp += 1;
            }

            // Now inject linux_extra after the second arg
            // Find the line in output buffer
            let line_start = dst - line.len() + {
                let mut stripped_total = 0usize;
                for i in 0..sr_count {
                    stripped_total += skip_ranges[i].1 - skip_ranges[i].0;
                }
                stripped_total
            }; // approximate — we'll recalculate
            // Actually, find the real line_start by scanning back
            let mut line_start = if dst > 0 { dst - 1 } else { 0 };
            // Scan back to find the beginning of this line in the output
            // The line in output starts at position (dst - bytes_copied_so_far_for_this_line)
            // We know we started with line.len() bytes and removed some
            let bytes_before = {
                let mut n = 0usize;
                let mut i = src;
                while i < line_end {
                    let mut skip_this = false;
                    for r in 0..sr_count {
                        let rel = i - src;
                        if rel >= skip_ranges[r].0 && rel < skip_ranges[r].1 {
                            skip_this = true;
                            i = src + skip_ranges[r].1;
                            break;
                        }
                    }
                    if !skip_this { n += 1; i += 1; }
                }
                n
            };
            line_start = dst.saturating_sub(bytes_before);

            // Check dedup: only inject if linux_extra not already present
            let existing = &out[line_start..dst];
            if !existing.windows(linux_extra.len()).any(|w| w == linux_extra) {
                // inject after second arg
                let inject_at = find_second_arg_end(line_start, out, dst);
                shift_and_inject(out, inject_at, &mut dst, linux_extra);
                if !eol_extra_dynamic.is_empty() {
                    if dst > 0 && out[dst - 1] == b'\n' {
                        shift_and_inject(out, dst - 1, &mut dst, eol_extra_dynamic);
                    } else {
                        out[dst..dst + eol_extra_dynamic.len()].copy_from_slice(eol_extra_dynamic);
                        dst += eol_extra_dynamic.len();
                    }
                }
            }

            // Copy the newline if present
            if line_end < orig.len() {
                out[dst] = orig[line_end];
                dst += 1;
            }
            src = line_end + if line_end < orig.len() { 1 } else { 0 };
        } else {
            // Non-linux line: copy verbatim, then check for initrd injection
            let ls = src;
            while src < orig.len() && orig[src] != b'\n' {
                out[dst] = orig[src];
                dst += 1;
                src += 1;
            }
            // copy newline
            if src < orig.len() {
                out[dst] = orig[src];
                dst += 1;
                src += 1;
            }

            let line_end = dst;
            let line_start = if line_end > 0 {
                let mut lls = line_end - 1;
                while lls > 0 && out[lls - 1] != b'\n' { lls -= 1; }
                lls
            } else { 0 };
            let line = &out[line_start..line_end];

            let mut ts = 0;
            while ts < line.len() && (line[ts] == b' ' || line[ts] == b'\t') { ts += 1; }
            if ts < line.len() && (line[ts..].starts_with(b"initrd ") || line[ts..].starts_with(b"initrd\t"))
                && initrd_dedup.len() <= line.len()
                && !line.windows(initrd_dedup.len()).any(|w| w == initrd_dedup)
            {
                let mut inject_at = dst;
                if dst > 0 && out[dst - 1] == b'\n' {
                    inject_at -= 1;
                    if dst > 1 && out[dst - 2] == b'\r' { inject_at -= 1; }
                }
                shift_and_inject(out, inject_at, &mut dst, initrd_extra);
            }
        }
    }

    Some(PatchOutput { buf: out_ptr, size: dst })
}

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
//  CasperStrategy (Ubuntu, Mint, Pop!_OS)
// ═══════════════════════════════════════════════════════════════════════════

pub struct CasperStrategy;

impl BootStrategy for CasperStrategy {
    fn detect(&self, ctx: &IsoFsCtx) -> bool {
        matches_any_lower(&ctx.iso_name[..ctx.iso_name_len], &[b"ubuntu", b"mint", b"pop"])
    }

    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        // Premount hook mounts ISO at /cdrom via losetup.
        // iso-scan/filename= acts as safety net — if the hook failed,
        // iso-scan has a path to search for. Existing
        // iso-scan/filename= tokens in the original grub.cfg are
        // STRIPPED before we inject our own.
        patch_grub_cfg_impl(
            inp,
            b" boot=casper",             // inject after vmlinuz
            b" iso-scan/filename=",      // eol; path auto-appended from IsoLocation
            inp.premount_target_name,
        )
    }
}

unsafe impl Sync for CasperStrategy {}

// ═══════════════════════════════════════════════════════════════════════════
//  LiveBootStrategy (Debian Live — uses boot=live, NOT boot=casper)
// ═══════════════════════════════════════════════════════════════════════════

pub struct LiveBootStrategy;

impl BootStrategy for LiveBootStrategy {
    fn detect(&self, ctx: &IsoFsCtx) -> bool {
        matches_any_lower(&ctx.iso_name[..ctx.iso_name_len], &[b"debian"])
    }

    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        patch_grub_cfg_impl(
            inp,
            b" boot=live live-media=removable", // inject after vmlinuz
            b" findiso=",                // eol; path auto-appended
            inp.premount_target_name,
        )
    }
}

unsafe impl Sync for LiveBootStrategy {}

// ═══════════════════════════════════════════════════════════════════════════
//  LiveOSStrategy
// ═══════════════════════════════════════════════════════════════════════════

pub struct LiveOSStrategy;

impl BootStrategy for LiveOSStrategy {
    fn detect(&self, ctx: &IsoFsCtx) -> bool {
        matches_any_lower(&ctx.iso_name[..ctx.iso_name_len], &[b"fedora", b"rhel", b"centos"])
    }

    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        patch_grub_cfg_impl(inp, b" rd.live.image rootdelay=300", b"", inp.premount_target_name)
    }
}

unsafe impl Sync for LiveOSStrategy {}

// ═══════════════════════════════════════════════════════════════════════════
//  Registry
// ═══════════════════════════════════════════════════════════════════════════

static STRATEGIES: &[&dyn BootStrategy] = &[&LiveOSStrategy, &CasperStrategy, &LiveBootStrategy];

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