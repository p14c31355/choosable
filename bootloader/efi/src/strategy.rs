// ═══════════════════════════════════════════════════════════════════════════
//  GRUB config patcher — injects kernel cmdline arguments and premount initrd
// ═══════════════════════════════════════════════════════════════════════════
//
//  This module has been slimmed-down.  The heavyweight `BootStrategy` trait
//  and its name-based detection are gone — they live in `boot_kind.rs` now.
//  The only remaining public entry point is `patch_grub_cfg()` which takes
//  a `BootKind` determined by scanning the ISO directory structure.

use core::ffi::c_void;
use crate::boot_kind::BootKind;
use crate::locator::IsoLocation;
use crate::protocol::{BootServices, MemoryType, EFI_SUCCESS};

// ═══════════════════════════════════════════════════════════════════════════
//  Input / output types
// ═══════════════════════════════════════════════════════════════════════════

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

// ═══════════════════════════════════════════════════════════════════════════
//  Internal helpers
// ═══════════════════════════════════════════════════════════════════════════

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

/// Remove all occurrences of ISO locator arguments (findiso=, iso-scan/filename=, choosable.iso_offset=)
/// from the output buffer between line_start and dst, and adjust dst accordingly.
fn remove_iso_locator_args(out: &mut [u8], line_start: usize, dst: &mut usize) {
    let patterns: &[&[u8]] = &[
        b"findiso=", b"iso-scan/filename=", b"choosable.iso_offset=",
        b"choosable.part_guid=", b"choosable.part_num=", b"choosable.iso_path=", b"choosable.iso_size=",
    ];

    let mut pos = line_start;
    let mut write_pos = line_start;

    while pos < *dst {
        let mut found_pattern = false;

        // Check if we're at the start of an ISO locator argument
        for pattern in patterns {
            if pos + pattern.len() <= *dst && &out[pos..pos + pattern.len()] == *pattern {
                // Found a pattern - skip it and its value
                pos += pattern.len();
                // Skip the argument value (until next space, tab, or newline)
                while pos < *dst && out[pos] != b' ' && out[pos] != b'\t' && out[pos] != b'\n' && out[pos] != b'\r' {
                    pos += 1;
                }
                // Skip trailing whitespace (but keep newlines)
                while pos < *dst && (out[pos] == b' ' || out[pos] == b'\t') {
                    pos += 1;
                }
                found_pattern = true;
                break;
            }
        }

        if !found_pattern {
            if write_pos != pos {
                out[write_pos] = out[pos];
            }
            write_pos += 1;
            pos += 1;
        }
    }

    *dst = write_pos;
}

// ═══════════════════════════════════════════════════════════════════════════
//  Core patching engine
// ═══════════════════════════════════════════════════════════════════════════

fn patch_grub_cfg_impl(
    inp: &PatchInput,
    linux_extra: &[u8],
    linux_eol_extra: &[u8],
    premount_target_name_p: &[u8],
) -> Option<PatchOutput> {
    let bs = unsafe { &mut *inp.bs };
    let orig = inp.original;

    let effective_target: &[u8] = if premount_target_name_p.is_empty() { b"PREMOUNT.CPIO" } else { premount_target_name_p };

    let mut initrd_extra_buf = [0u8; 32];
    initrd_extra_buf[0] = b' '; initrd_extra_buf[1] = b'/';
    let name_len = effective_target.len().min(30);
    initrd_extra_buf[2..2 + name_len].copy_from_slice(&effective_target[..name_len]);
    let initrd_extra = &initrd_extra_buf[..2 + name_len];

    let mut dedup_buf = [0u8; 32];
    dedup_buf[0] = b'/';
    dedup_buf[1..1 + name_len].copy_from_slice(&effective_target[..name_len]);
    let dedup_slice = &dedup_buf[..1 + name_len];

    // Build the dynamic value for distro-specific EOL args (findiso= for DebianLive, iso-scan/filename= for Casper).
    // If iso_location is None, skip EOL injection entirely to avoid bare "findiso=" or "iso-scan/filename=".
    let mut eol_buf = [0u8; 320];
    let eol_extra_dynamic = if !linux_eol_extra.is_empty() && linux_eol_extra.ends_with(b"=") {
        inp.iso_location.map_or(b"".as_ref(), |loc| {
            let plen = linux_eol_extra.len();
            if linux_eol_extra == b" findiso=" {
                let path = loc.path(); let pl = path.len().min(320 - plen);
                eol_buf[..plen].copy_from_slice(linux_eol_extra);
                eol_buf[plen..plen + pl].copy_from_slice(&path[..pl]);
                &eol_buf[..plen + pl]
            } else if linux_eol_extra == b" iso-scan/filename=" {
                let fname = loc.file_name(); let pl = fname.len().min(320 - plen);
                eol_buf[..plen].copy_from_slice(linux_eol_extra);
                eol_buf[plen..plen + pl].copy_from_slice(&fname[..pl]);
                &eol_buf[..plen + pl]
            } else {
                &b""[..]
            }
        })
    } else { &b""[..] };

    // NOTE: choosable.iso_offset= and other choosable.* params are now injected
    // as part of linux_extra (via BootKind::linux_extra). We no longer inject
    // them separately here to avoid duplication.

    // Count blscfg lines for BLS-aware distros (Fedora 40+)
    let blscfg_count = orig.windows(6).filter(|w| *w == b"blscfg" || *w == b"BLSCFG").count();
    let has_blscfg = blscfg_count > 0;

    // Count options lines for systemd-boot
    let options_count = {
        let mut count = 0;
        let mut pos = 0;
        while pos < orig.len() {
            let start = pos;
            while pos < orig.len() && orig[pos] != b'\n' { pos += 1; }
            let line = &orig[start..pos];
            let mut ts = 0;
            while ts < line.len() && (line[ts] == b' ' || line[ts] == b'\t') { ts += 1; }
            let t = &line[ts..];
            if t.starts_with(b"options ") || t.starts_with(b"options\t") {
                count += 1;
            }
            if pos < orig.len() { pos += 1; }
        }
        count
    };

    let (linux_count, initrd_count) = count_matching_lines(orig);
    let extra = linux_count * (linux_extra.len() + eol_extra_dynamic.len())
        + initrd_count * initrd_extra.len()
        + if has_blscfg { blscfg_count * (b"set kernelopts=\"$kernelopts".len() + linux_extra.len() + b"\"\n".len()) } else { 0 }
        + options_count * linux_extra.len();
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
                // Remove any existing ISO locator arguments to prevent duplicates
                // and ensure our dynamic value takes precedence.
                if !eol_extra_dynamic.is_empty() {
                    remove_iso_locator_args(out, line_start, &mut dst);
                }

                // Inject linux_extra after the kernel path (second
                // argument).  This position is always before any "---"
                // separator, so kernel parameters are guaranteed to be
                // received by the kernel.
                let inject_at = find_second_arg_end(line_start, out, dst);
                shift_and_inject(out, inject_at, &mut dst, linux_extra);

                // Inject iso-scan/filename= (or findiso=) as a second
                // argument block IMMEDIATELY AFTER linux_extra, with a
                // leading space.  This places it before any "---" and
                // ensures it's separated by whitespace from both the
                // preceding arg and the following arg/separator.
                if !eol_extra_dynamic.is_empty() {
                    let inj2 = inject_at + linux_extra.len();
                    shift_and_inject(out, inj2, &mut dst, eol_extra_dynamic);
                }
            }
            else if has_blscfg
                && (t.starts_with(b"blscfg") || t.starts_with(b"blscfg\n") || t.starts_with(b"BLSCFG") || t.starts_with(b"BLSCFG\n"))
                && !linux_extra.is_empty()
            {
                // Fedora 40+ BLS: inject `set kernelopts="$kernelopts <linux_extra>"` BEFORE the blscfg line.
                // This APPENDS linux_extra to the existing $kernelopts instead of replacing it.
                let mut bls_line = [0u8; 512];
                let mut bls_len = 0usize;
                bls_line[bls_len..][..15].copy_from_slice(b"set kernelopts=");
                bls_len += 15;
                bls_line[bls_len] = b'"'; bls_len += 1;
                // Preserve existing $kernelopts
                let preserve = b"$kernelopts";
                let pl = preserve.len().min(512 - bls_len - 2);
                bls_line[bls_len..bls_len + pl].copy_from_slice(&preserve[..pl]);
                bls_len += pl;
                // Append linux_extra after existing kernelopts
                if !linux_extra.is_empty() {
                    // linux_extra already starts with a space, so just append it
                    let content = linux_extra;
                    let cl = content.len().min(512 - bls_len - 2);
                    bls_line[bls_len..bls_len + cl].copy_from_slice(&content[..cl]);
                    bls_len += cl;
                }
                bls_line[bls_len] = b'"'; bls_len += 1;
                bls_line[bls_len] = b'\n'; bls_len += 1;

                // Inject BEFORE the current blscfg line (at line_start)
                let inject_at = line_start;
                shift_and_inject(out, inject_at, &mut dst, &bls_line[..bls_len]);
            }
            else if t.starts_with(b"options ") || t.starts_with(b"options\t")
            {
                // systemd-boot .conf format: inject cmdline args onto the options line.
                // First remove any existing ISO locator arguments to prevent duplicates.
                remove_iso_locator_args(out, line_start, &mut dst);

                // linux_extra starts with a space; strip it for appending to options.
                let extra_content = if linux_extra.starts_with(b" ") { &linux_extra[1..] } else { linux_extra };
                if !extra_content.is_empty() {
                    // Prepend space if the line doesn't already end with one
                    let need_space = dst > line_start && out[dst - 1] != b' ' && out[dst - 1] != b'\t';
                    let mut opt_buf = [0u8; 320];
                    let mut ob = 0usize;
                    if need_space { opt_buf[ob] = b' '; ob += 1; }
                    opt_buf[ob..ob + extra_content.len()].copy_from_slice(extra_content);
                    ob += extra_content.len();
                    let inject_at = if dst > 0 && (out[dst - 1] == b'\n' || out[dst - 1] == b'\r') {
                        dst - if out[dst - 1] == b'\n' { 1 } else { 0 }
                    } else { dst };
                    shift_and_inject(out, inject_at, &mut dst, &opt_buf[..ob]);
                }
            }
            else if (t.starts_with(b"initrd ") || t.starts_with(b"initrd\t")
                || t.starts_with(b"initrdefi ") || t.starts_with(b"initrdefi\t"))
                && !effective_target.is_empty()
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

// ── Decimal string builder (shared) ────────────────────────────────────

fn build_decimal_buf(mut v: u64) -> ([u8; 21], usize) {
    let mut digits = [0u8; 21];
    let mut dpos = 20;
    if v == 0 { digits[20] = b'0'; }
    else {
        loop {
            digits[dpos] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 { break; }
            dpos -= 1;
        }
    }
    (digits, dpos)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Internal helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Patch a GRUB configuration file to inject the kernel cmdline arguments and
/// premount initrd required for filesystem-independent boot.
///
/// * `original` — raw content of the grub.cfg file.
/// * `boot_kind` — detected distro family; controls which cmdline is injected.
/// * `iso_name` — ISO filename (used for Pop!_OS toram detection only).
/// * `iso_location` — where the ISO lives on disk (for choosable.iso_offset=).
/// * `premount_target_name` — filename of the injected premount CPIO in the
///    ISO root directory (e.g. "MD5SUM.TXT" or "PREMOUNT.CPIO").
/// * `bs` — UEFI boot services.
pub fn patch_grub_cfg(
    original: &[u8],
    boot_kind: BootKind,
    iso_name: &[u8],
    iso_location: Option<&IsoLocation>,
    premount_target_name: &[u8],
    bs: *mut BootServices,
) -> Option<PatchOutput> {
    let is_popos = boot_kind == BootKind::Casper
        && matches_any_lower(iso_name, &[b"pop", b"pop-os", b"popos"]);

    let mut linux_extra_buf = [0u8; 512];
    let linux_extra = boot_kind.linux_extra(is_popos, iso_location, &mut linux_extra_buf)
        .unwrap_or(b"");
    let linux_eol_extra = boot_kind.linux_eol_extra();

    let mut live_media_uuid = [0u8; 10];

    let inp = PatchInput {
        original,
        iso_name,
        bs,
        live_media_uuid: &live_media_uuid,
        iso_location,
        premount_target_name,
    };

    patch_grub_cfg_impl(&inp, &linux_extra, linux_eol_extra, premount_target_name)
}
