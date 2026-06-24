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

pub struct HookTargetSet {
    pub live: bool,
    pub live_premount: bool,
    pub casper_premount: bool,
    pub casper_bottom: bool,
}

pub trait BootStrategy: Sync {
    fn detect(&self, ctx: &IsoFsCtx) -> bool;
    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> { let _ = inp; None }
    fn hook_targets(&self) -> HookTargetSet {
        HookTargetSet { live: true, live_premount: true, casper_premount: true, casper_bottom: true }
    }
    fn needs_sr_mod(&self) -> bool { false }
}

fn allocate_output(bs: &mut BootServices, orig_len: usize, extra: usize) -> Option<(*mut u8, usize)> {
    let new_size = orig_len + extra + 256;
    let mut ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe { (bs.allocate_pool)(MemoryType::EfiLoaderData, new_size, &mut ptr) };
    if status != EFI_SUCCESS || ptr.is_null() { return None; }
    Some((ptr as *mut u8, new_size))
}

fn count_matching_lines(orig: &[u8]) -> (usize, usize) {
    let mut linux_count = 0;
    let mut initrd_count = 0;
    let mut pos = 0;
    while pos < orig.len() {
        let start = pos;
        while pos < orig.len() && orig[pos] != b'\n' { pos += 1; }
        let line = &orig[start..pos];
        let mut ts = 0;
        while ts < line.len() && (line[ts] == b' ' || line[ts] == b'\t') { ts += 1; }
        let t = &line[ts..];
        if t.starts_with(b"linux ") || t.starts_with(b"linux\t")
            || t.starts_with(b"linuxefi ") || t.starts_with(b"linuxefi\t")
        { linux_count += 1; }
        else if t.starts_with(b"initrd ") || t.starts_with(b"initrd\t")
            || t.starts_with(b"initrdefi ") || t.starts_with(b"initrdefi\t") { initrd_count += 1; }
        if pos < orig.len() { pos += 1; }
    }
    (linux_count, initrd_count)
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
    let dedup_slice = &dedup_buf[..1 + name_len];

    let iso_path: Option<&[u8]> = inp.iso_location.map(|loc| loc.path());
    let mut eol_buf = [0u8; 320];
    let eol_extra_dynamic: &[u8] = if !linux_eol_extra.is_empty() && linux_eol_extra.ends_with(b"=") {
        if let Some(path) = iso_path {
            let plen = linux_eol_extra.len();
            if plen < 320 {
                let pl = path.len().min(320 - plen);
                eol_buf[..plen].copy_from_slice(linux_eol_extra);
                eol_buf[plen..plen + pl].copy_from_slice(&path[..pl]);
                &eol_buf[..plen + pl]
            } else {
                linux_eol_extra
            }
        } else { linux_eol_extra }
    } else { linux_eol_extra };

    let (linux_count, initrd_count) = count_matching_lines(orig);
    let extra = linux_count * (linux_extra.len() + eol_extra_dynamic.len())
        + initrd_count * initrd_extra.len();
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
            let mut ts = 0;
            while ts < line.len() && (line[ts] == b' ' || line[ts] == b'\t') { ts += 1; }
            let t = &line[ts..];

            if (t.starts_with(b"linux ") || t.starts_with(b"linux\t")
                || t.starts_with(b"linuxefi ") || t.starts_with(b"linuxefi\t"))
                && !line.windows(linux_extra.len()).any(|w| w == linux_extra)
                && !linux_extra.is_empty()
            {
                let needs_eol = !eol_extra_dynamic.is_empty();
                let inject_at = find_second_arg_end(line_start, out, dst);
                shift_and_inject(out, inject_at, &mut dst, linux_extra);
                if needs_eol {
                    // Handle \r\n line endings: inject before \r if present.
                    if dst > 0 && out[dst - 1] == b'\n' {
                        let mut inject_at = dst - 1;
                        if dst > 1 && out[dst - 2] == b'\r' { inject_at -= 1; }
                        shift_and_inject(out, inject_at, &mut dst, eol_extra_dynamic);
                    } else {
                        out[dst..dst + eol_extra_dynamic.len()].copy_from_slice(eol_extra_dynamic);
                        dst += eol_extra_dynamic.len();
                    }
                }
            }
            else if (t.starts_with(b"initrd ") || t.starts_with(b"initrd\t")
                || t.starts_with(b"initrdefi ") || t.starts_with(b"initrdefi\t"))
                && dedup_slice.len() <= line.len()
                && !line.windows(dedup_slice.len()).any(|w| w == dedup_slice)
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

fn matches_any_lower(name: &[u8], patterns: &[&[u8]]) -> bool {
    patterns.iter().any(|pat| name.windows(pat.len()).any(|w| {
        w.iter().zip(pat.iter()).all(|(&a, &b)| (a | 0x20) == b)
    }))
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
        // Premount hook mounts ISO at /cdrom via losetup BEFORE casper.
        // Casper auto-detects /cdrom when boot=casper is set.
        // live-media=LABEL=Choosable acts as a hint so casper knows which
        // device to scan if the premount hook fails for any reason.
        // Do NOT inject iso-scan/filename= — it forces casper's 20iso_scan
        // to mount the real partition (fails on exFAT/NTFS).
        patch_grub_cfg_impl(
            inp,
            b" boot=casper live-media=LABEL=Choosable",
            b"", // no eol override — premount handles /cdrom, casper auto-detects it
            inp.premount_target_name,
        )
    }

    fn needs_sr_mod(&self) -> bool { true }

    fn hook_targets(&self) -> HookTargetSet {
        HookTargetSet { live: false, live_premount: false, casper_premount: true, casper_bottom: true }
    }
}

unsafe impl Sync for CasperStrategy {}

// ═══════════════════════════════════════════════════════════════════════════
//  LiveBootStrategy (Debian Live)
// ═══════════════════════════════════════════════════════════════════════════

pub struct LiveBootStrategy;

impl BootStrategy for LiveBootStrategy {
    fn detect(&self, ctx: &IsoFsCtx) -> bool {
        matches_any_lower(&ctx.iso_name[..ctx.iso_name_len], &[b"debian"])
    }

    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        patch_grub_cfg_impl(
            inp,
            b" boot=live live-media=removable",
            b" findiso=",
            inp.premount_target_name,
        )
    }

    fn needs_sr_mod(&self) -> bool { false }

    fn hook_targets(&self) -> HookTargetSet {
        HookTargetSet { live: true, live_premount: true, casper_premount: false, casper_bottom: false }
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

pub fn get_hook_targets(ctx: &IsoFsCtx) -> HookTargetSet {
    let strategy: &dyn BootStrategy = STRATEGIES.iter().find(|s| s.detect(ctx)).copied().unwrap_or(&CasperStrategy);
    strategy.hook_targets()
}

pub fn needs_sr_mod(ctx: &IsoFsCtx) -> bool {
    let strategy: &dyn BootStrategy = STRATEGIES.iter().find(|s| s.detect(ctx)).copied().unwrap_or(&CasperStrategy);
    strategy.needs_sr_mod()
}