// ═══════════════════════════════════════════════════════════════════════════
//  ISO Boot Strategy — detects ISO type and patches grub.cfg
// ═══════════════════════════════════════════════════════════════════════════
//
//  Architecture:
//    BootStrategy trait: detects the ISO distro and injects the correct
//    kernel command-line parameters so the initramfs can find the ISO on
//    the real USB drive.
//
//  Current strategies:
//    - CasperStrategy: Ubuntu / Mint / Pop!_OS / Debian-live (casper
//      initramfs).  Injects "boot=casper rootwait rootdelay=300 debug
//      live-media=UUID=$UUID iso-scan/filename=$ISO_PATH".
//    - LiveOSStrategy: Fedora / RHEL / CentOS (dracut-based).  Injects
//      "rd.live.image rootdelay=300 live-media=UUID=$UUID
//      iso-scan/filename=$ISO_PATH".
//
//  IsoLocation integration:
//    When an IsoLocation is available (via IsoLocator), richer boot
//    parameters can be injected alongside iso-scan/filename=, giving the
//    initramfs multiple ways to locate the original ISO partition.

use core::ffi::c_void;

use crate::iso_fs::IsoFsCtx;
use crate::locator::IsoLocation;
use crate::protocol::{BootServices, MemoryType, EFI_SUCCESS};

pub struct PatchInput<'a> {
    pub original: &'a [u8],
    pub iso_name: &'a [u8],
    pub bs: *mut BootServices,
    /// FAT32 volume serial of the real USB partition (formatted "XXXX-XXXX\0")
    pub live_media_uuid: &'a [u8; 10],
    /// Physical location of the ISO (from IsoLocator)
    pub iso_location: Option<&'a IsoLocation>,
}

pub struct PatchOutput {
    pub buf: *mut u8,
    pub size: usize,
}

pub trait BootStrategy: Sync {
    fn detect(&self, ctx: &IsoFsCtx) -> bool;
    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        let _ = inp;
        None
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Count the number of `linux /` `linuxefi ` lines in the input.
fn count_linux_lines(data: &[u8]) -> usize {
    let mut count = 0usize;
    let mut pos = 0usize;
    while pos < data.len() {
        let start = pos;
        while pos < data.len() && data[pos] != b'\n' {
            pos += 1;
        }
        let line = &data[start..pos];
        let mut trim_start = 0;
        while trim_start < line.len()
            && (line[trim_start] == b' ' || line[trim_start] == b'\t')
        {
            trim_start += 1;
        }
        let trimmed = &line[trim_start..];
        if trimmed.starts_with(b"linux ") || trimmed.starts_with(b"linuxefi ") {
            count += 1;
        }
        if pos < data.len() {
            pos += 1;
        }
    }
    if count == 0 {
        1
    } else {
        count
    }
}

/// Returns true if the UUID string is empty (all zeros).
fn uuid_is_empty(uuid: &[u8; 10]) -> bool {
    uuid[0] == b'0'
        && uuid[1] == b'0'
        && uuid[2] == b'0'
        && uuid[3] == b'0'
        && uuid[4] == b'-'
        && uuid[5] == b'0'
        && uuid[6] == b'0'
        && uuid[7] == b'0'
        && uuid[8] == b'0'
}

/// Shared patch logic: inject `pre` + iso_name immediately after the
/// kernel path in `linux`/`linuxefi` lines (right after `vmlinuz`).
///
/// `pre` should end with `iso-scan/filename=` (or equivalent), and the
/// iso_name is appended after it.  When a non-zero `live_media_uuid` is
/// present, it is injected as ` live-media=UUID=$UUID ` immediately
/// before `iso-scan/filename=`.
fn patch_common(inp: &PatchInput, pre: &[u8]) -> Option<PatchOutput> {
    let bs = unsafe { &mut *inp.bs };
    let name = inp.iso_name;

    // Build the full injection string.
    // Layout: `pre` + iso_name.
    // If UUID is non-zero, inject ` live-media=UUID=$UUID ` just before
    // the `iso-scan/filename=` suffix inside `pre`.
    //
    // We re-build `inj` by searching for `iso-scan/filename=` in `pre`
    // and inserting the UUID before it.

    let iso_scan = b"iso-scan/filename=";
    let uuid_tag = b"live-media=UUID=";

    // Find where iso-scan/filename= lives inside pre.
    let scan_pos = pre
        .windows(iso_scan.len())
        .position(|w| w == iso_scan);

    // Total injection length: pre + iso_name + optional UUID param.
    let uuid_extra = if !uuid_is_empty(inp.live_media_uuid) && scan_pos.is_some() {
        uuid_tag.len() + 9 + 1 // "live-media=UUID=" + "XXXX-XXXX" + " "
    } else {
        0
    };

    let inj_len = pre.len() + name.len() + uuid_extra;
    let orig_len = inp.original.len();

    let linux_lines = count_linux_lines(inp.original);
    let new_size = orig_len + inj_len * linux_lines + 256;
    let mut patch_ptr: *mut c_void = core::ptr::null_mut();
    let status =
        unsafe { (bs.allocate_pool)(MemoryType::EfiLoaderData, new_size, &mut patch_ptr) };
    if status != EFI_SUCCESS || patch_ptr.is_null() {
        return None;
    }
    let out = unsafe { core::slice::from_raw_parts_mut(patch_ptr as *mut u8, new_size) };

    // Build `inj` buffer.
    let mut inj = [0u8; 320];
    let mut inj_pos = 0usize;

    if let Some(sp) = scan_pos {
        // Insert everything before iso-scan/filename=.
        let before = &pre[..sp];
        inj[inj_pos..inj_pos + before.len()].copy_from_slice(before);
        inj_pos += before.len();

        // Insert live-media=UUID=$UUID if UUID is non-zero.
        if !uuid_is_empty(inp.live_media_uuid) {
            inj[inj_pos..inj_pos + uuid_tag.len()]
                .copy_from_slice(uuid_tag);
            inj_pos += uuid_tag.len();
            let uuid_src = &inp.live_media_uuid[..9]; // "XXXX-XXXX"
            inj[inj_pos..inj_pos + 9].copy_from_slice(uuid_src);
            inj_pos += 9;
            inj[inj_pos] = b' ';
            inj_pos += 1;
        }

        // Insert iso-scan/filename= and the rest of `pre`.
        let rest = &pre[sp..];
        inj[inj_pos..inj_pos + rest.len()].copy_from_slice(rest);
        inj_pos += rest.len();
    } else {
        // No iso-scan/filename= marker — just copy pre as-is.
        inj[inj_pos..inj_pos + pre.len()].copy_from_slice(pre);
        inj_pos += pre.len();
    }

    // Append iso_name.
    let nb = name.len().min(255);
    inj[inj_pos..inj_pos + nb].copy_from_slice(&name[..nb]);
    inj_pos += nb;
    let max_inj_final = inj_pos;

    let mut src = 0usize;
    let mut dst = 0usize;
    while src < orig_len {
        let ch = inp.original[src];
        out[dst] = ch;
        dst += 1;
        src += 1;

        if ch == b'\n' || src == orig_len {
            let line_start = if dst > 0 {
                let mut ls = dst - 1;
                while ls > 0 && out[ls - 1] != b'\n' {
                    ls -= 1;
                }
                ls
            } else {
                0
            };

            let line_bytes = &out[line_start..dst];

            let is_linux_line = {
                let mut ts = 0;
                while ts < line_bytes.len()
                    && (line_bytes[ts] == b' ' || line_bytes[ts] == b'\t')
                {
                    ts += 1;
                }
                let t = &line_bytes[ts..];
                (t.starts_with(b"linux ")
                    || t.starts_with(b"linux\t")
                    || t.starts_with(b"linuxefi ")
                    || t.starts_with(b"linuxefi\t"))
                    && !line_bytes
                        .windows(18)
                        .any(|w| w == b"iso-scan/filename=")
            };

            if is_linux_line {
                // Skip "linux " (or "linuxefi ") and kernel path tokens.
                let mut token_start = line_start;
                while token_start < dst
                    && (out[token_start] == b' ' || out[token_start] == b'\t')
                {
                    token_start += 1;
                }
                while token_start < dst
                    && out[token_start] != b' '
                    && out[token_start] != b'\t'
                    && out[token_start] != b'\n'
                    && out[token_start] != b'\r'
                {
                    token_start += 1;
                }
                while token_start < dst
                    && (out[token_start] == b' ' || out[token_start] == b'\t')
                {
                    token_start += 1;
                }
                while token_start < dst
                    && out[token_start] != b' '
                    && out[token_start] != b'\t'
                    && out[token_start] != b'\n'
                    && out[token_start] != b'\r'
                {
                    token_start += 1;
                }
                let inject_at = token_start;

                let suffix_len = dst - inject_at;
                for i in (0..suffix_len).rev() {
                    out[inject_at + max_inj_final + i] = out[inject_at + i];
                }
                out[inject_at..inject_at + max_inj_final]
                    .copy_from_slice(&inj[..max_inj_final]);
                dst += max_inj_final;
            }
        }
    }

    let final_len = dst;
    Some(PatchOutput {
        buf: patch_ptr as *mut u8,
        size: final_len,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
//  CasperStrategy — Ubuntu / Mint / Pop!_OS / Debian-live
// ═══════════════════════════════════════════════════════════════════════════

pub struct CasperStrategy;

impl BootStrategy for CasperStrategy {
    fn detect(&self, ctx: &IsoFsCtx) -> bool {
        let name = &ctx.iso_name[..ctx.iso_name_len];
        let lower = |b: u8| b | 0x20;
        name.windows(6).any(|w| {
            lower(w[0]) == b'u'
                && lower(w[1]) == b'b'
                && lower(w[2]) == b'u'
                && lower(w[3]) == b'n'
                && lower(w[4]) == b't'
                && lower(w[5]) == b'u'
        }) || name.windows(4).any(|w| {
            lower(w[0]) == b'm'
                && lower(w[1]) == b'i'
                && lower(w[2]) == b'n'
                && lower(w[3]) == b't'
        }) || name.windows(6).any(|w| {
            lower(w[0]) == b'd'
                && lower(w[1]) == b'e'
                && lower(w[2]) == b'b'
                && lower(w[3]) == b'i'
                && lower(w[4]) == b'a'
                && lower(w[5]) == b'n'
        }) || name
            .windows(3)
            .any(|w| lower(w[0]) == b'p' && lower(w[1]) == b'o' && lower(w[2]) == b'p')
    }

    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        // Strip leading '/' — casper expects paths without it.
        let trimmed = if inp.iso_name.first() == Some(&b'/') {
            &inp.iso_name[1..]
        } else {
            inp.iso_name
        };

        // live-media=UUID=$UUID is injected inside patch_common before
        // iso-scan/filename=, so the initramfs can immediately identify
        // which block device holds the ISO without a blind scan.
        let inp2 = PatchInput {
            original: inp.original,
            iso_name: trimmed,
            bs: inp.bs,
            live_media_uuid: inp.live_media_uuid,
            iso_location: inp.iso_location,
        };
        patch_common(
            &inp2,
            b" boot=casper rootwait rootdelay=300 debug iso-scan/filename=",
        )
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
        name.windows(6).any(|w| {
            lower(w[0]) == b'f'
                && lower(w[1]) == b'e'
                && lower(w[2]) == b'd'
                && lower(w[3]) == b'o'
                && lower(w[4]) == b'r'
                && lower(w[5]) == b'a'
        }) || name.windows(4).any(|w| {
            lower(w[0]) == b'r'
                && lower(w[1]) == b'h'
                && lower(w[2]) == b'e'
                && lower(w[3]) == b'l'
        }) || name.windows(6).any(|w| {
            lower(w[0]) == b'c'
                && lower(w[1]) == b'e'
                && lower(w[2]) == b'n'
                && lower(w[3]) == b't'
                && lower(w[4]) == b'o'
                && lower(w[5]) == b's'
        })
    }

    fn patch(&self, inp: &PatchInput) -> Option<PatchOutput> {
        let inp2 = PatchInput {
            original: inp.original,
            iso_name: inp.iso_name,
            bs: inp.bs,
            live_media_uuid: inp.live_media_uuid,
            iso_location: inp.iso_location,
        };
        patch_common(
            &inp2,
            b" rd.live.image rootdelay=300 iso-scan/filename=",
        )
    }
}

unsafe impl Sync for LiveOSStrategy {}

// ═══════════════════════════════════════════════════════════════════════════
//  Registry
// ═══════════════════════════════════════════════════════════════════════════

static STRATEGIES: &[&dyn BootStrategy] = &[&LiveOSStrategy, &CasperStrategy];

/// Patch grub.cfg with the correct kernel boot parameters.
///
/// When `iso_location` is provided (Some), richer parameters are available
/// for injecting alongside the standard iso-scan/filename= approach.  This
/// gives the initramfs multiple fallback strategies to find the original
/// ISO partition — especially useful on filesystems that GRUB on the ISO
/// cannot read (exFAT, NTFS, ReFS, ext4).
pub fn patch_grub_cfg(
    ctx: &IsoFsCtx,
    original: &[u8],
    bs: *mut BootServices,
    iso_location: Option<&IsoLocation>,
) -> Option<PatchOutput> {
    let strategy: &dyn BootStrategy = STRATEGIES
        .iter()
        .find(|s| s.detect(ctx))
        .copied()
        .unwrap_or(&CasperStrategy);
    let inp = PatchInput {
        original,
        iso_name: &ctx.iso_name[..ctx.iso_name_len],
        bs,
        live_media_uuid: &ctx.live_media_uuid,
        iso_location,
    };
    strategy.patch(&inp)
}