// ═══════════════════════════════════════════════════════════════════════════
//  ISO Boot Strategy — detects ISO type and patches grub.cfg
// ═══════════════════════════════════════════════════════════════════════════
//
//  Architecture:
//    BootStrategy trait: detects the ISO distro and injects the correct
//    kernel command-line parameters so the initramfs can find the root
//    filesystem.
//
//  Key insight:
//    Choosable serves the ISO as a virtual CD-ROM via BlockIoIsoSource +
//    VirtualBlockIo + SIMPLE_FILE_SYSTEM_PROTOCOL.  The kernel boots from
//    this virtual CD-ROM, and casper/dracut CAN read ISO9660 natively.
//
//    Injecting `iso-scan/filename=` is actively harmful when the USB is
//    formatted with exFAT/NTFS because the initramfs lacks the kernel
//    modules to mount those filesystems — the scan/mount attempt fails and
//    drops to BusyBox.
//
//    Instead, we tell casper to read `/casper/filesystem.squashfs`
//    directly from the CD-ROM device via `file=/cdrom/preseed/...`.
//
//  Current strategies:
//    - CasperStrategy: Ubuntu / Mint / Pop!_OS / Debian-live.
//    - LiveOSStrategy: Fedora / RHEL / CentOS (dracut-based).

use core::ffi::c_void;

use crate::iso_fs::IsoFsCtx;
use crate::locator::IsoLocation;
use crate::protocol::{BootServices, MemoryType, EFI_SUCCESS};

pub struct PatchInput<'a> {
    pub original: &'a [u8],
    pub iso_name: &'a [u8],
    pub bs: *mut BootServices,
    /// Volume serial of the real USB partition (formatted "XXXX-XXXX\0")
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

/// Shared patch logic: inject `pre` immediately after the kernel path in
/// `linux`/`linuxefi` lines (right after `vmlinuz`).
///
/// This is a simplified version that injects the parameters directly
/// without any iso-scan/filename= or iso_name concatenation.
fn patch_common(inp: &PatchInput, pre: &[u8]) -> Option<PatchOutput> {
    let bs = unsafe { &mut *inp.bs };
    let inj_len = pre.len();
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

    let inj = pre;

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
                    out[inject_at + inj_len + i] = out[inject_at + i];
                }
                out[inject_at..inject_at + inj_len].copy_from_slice(inj);
                dst += inj_len;
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
//
//  Does NOT inject iso-scan/filename=.  Instead injects file=/cdrom/...
//  which tells casper the root is on the CD-ROM device.  The virtual
//  CD-ROM is the ISO9660 filesystem, so /casper/filesystem.squashfs is
//  directly readable by casper without any USB partition mount.

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
        // No iso-scan/filename=.  The virtual CD-ROM already has
        // /casper/filesystem.squashfs.  file=/cdrom/ tells casper to
        // look at the CD-ROM device, which is the ISO9660 FS we serve.
        // rootdelay=300 gives slow USB controllers time to enumerate
        // (needed for keyboard, not for rootfs — rootfs is on CD).
        let inp2 = PatchInput {
            original: inp.original,
            iso_name: inp.iso_name,
            bs: inp.bs,
            live_media_uuid: inp.live_media_uuid,
            iso_location: inp.iso_location,
        };
        patch_common(
            &inp2,
            b" boot=casper rootwait rootdelay=300 debug file=/cdrom/preseed/ubuntu.seed",
        )
    }
}

unsafe impl Sync for CasperStrategy {}

// ═══════════════════════════════════════════════════════════════════════════
//  LiveOSStrategy — Fedora / RHEL / CentOS (dracut-based)
// ═══════════════════════════════════════════════════════════════════════════
//
//  Does NOT inject iso-scan/filename=.  Instead injects rd.live.ram which
//  copies the entire squashfs to RAM from the boot device (virtual CD-ROM).

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
            b" rd.live.image rootdelay=300 root=live:CDLABEL=Fedora",
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
/// Does NOT inject iso-scan/filename= because the virtual CD-ROM already
/// contains the ISO9660 filesystem with /casper/filesystem.squashfs.
/// The kernel and initramfs boot from the virtual CD-ROM and casper can
/// read the squashfs directly — no USB partition mount required.
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