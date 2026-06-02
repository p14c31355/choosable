// ═══════════════════════════════════════════════════════════════════════════
//  Premount initrd builder — creates loop device from raw partition offset
// ═══════════════════════════════════════════════════════════════════════════
//
//  When the USB is formatted with a filesystem whose kernel module is not
//  included in the ISO's initramfs (exFAT, NTFS, ReFS, ...), the kernel
//  cannot mount the USB partition to find the ISO.  Casper/dracut falls
//  through to BusyBox.
//
//  This module solves the problem by:
//    1. Building a tiny cpio "premount" initrd containing a shell script
//       that creates a loop device directly from the raw partition at the
//       known ISO offset (no filesystem mount needed).
//    2. The script uses losetup -o $OFFSET to expose the ISO as /dev/loop0,
//       then mounts it as iso9660 at /cdrom before casper runs.
//    3. The offset is passed via kernel command line: choosable.iso_offset=$N
//
//  No squashfs reading is done here — the initrd is under 2 KB and boots
//  instantly.  The kernel reads squashfs from the loop device on demand.

use core::ffi::c_void;
use crate::boot_context::BootContext;
use crate::protocol::{BootServices, MemoryType, EFI_SUCCESS};

pub struct PremountBundle {
    /// Pool-allocated cpio archive
    pub cpio_buf: *mut u8,
    /// cpio archive size in bytes
    pub cpio_size: usize,
    /// ISO file start LBA on the real partition (512-byte sectors)
    pub iso_offset_bytes: u64,
}

/// Trait for early-boot fixups injected via initrd.
///
/// Implementations build a cpio archive containing hook scripts that
/// run before the distro's own initramfs scripts (e.g. casper, live-boot,
/// dracut).  This allows filesystem-independent loopback mounting of
/// the payload without relying on kernel modules for the host filesystem.
pub trait EarlyBootFixup {
    /// Build an initrd cpio archive that will be injected into the
    /// boot chain (appended to the initrd line / served as synthetic file).
    ///
    /// Returns `None` if the fixup cannot be built (e.g. OOM).
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle>;
}

// ═══════════════════════════════════════════════════════════════════════════
//  CasperFixup — Ubuntu / Mint / Pop!_OS (casper initramfs)
// ═══════════════════════════════════════════════════════════════════════════

pub struct CasperFixup;

impl EarlyBootFixup for CasperFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle> {
        let iso = ctx.selected_iso();
        let relative_sector_offset = iso.file_start_lba - ctx.partition_start_lba;
        prepare_premount_initrd(bs, relative_sector_offset)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  LiveBootFixup — Debian Live (live-boot initramfs)
// ═══════════════════════════════════════════════════════════════════════════

pub struct LiveBootFixup;

impl EarlyBootFixup for LiveBootFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle> {
        let iso = ctx.selected_iso();
        let relative_sector_offset = iso.file_start_lba - ctx.partition_start_lba;
        prepare_premount_initrd(bs, relative_sector_offset)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  DracutFixup — Fedora / RHEL / CentOS (dracut initramfs)
// ═══════════════════════════════════════════════════════════════════════════

pub struct DracutFixup;

impl EarlyBootFixup for DracutFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle> {
        let iso = ctx.selected_iso();
        let relative_sector_offset = iso.file_start_lba - ctx.partition_start_lba;
        prepare_premount_initrd(bs, relative_sector_offset)
    }
}

fn hex_nibble(n: u8) -> u8 {
    if n < 10 { b'0' + n } else { b'A' + (n - 10) }
}

fn format_decimal_u64(v: u64) -> [u8; 21] {
    let mut buf = [b'0'; 21];
    let mut val = v;
    let mut pos = 20;
    if val == 0 { buf[20] = b'0'; }
    else {
        loop {
            buf[pos] = b'0' + (val % 10) as u8;
            val /= 10;
            if val == 0 { break; }
            pos -= 1;
        }
    }
    buf
}

/// Build the premount shell script.
/// `OFFSET` is replaced with the decimal byte offset of the ISO on the
/// real partition (iso_lba * 512).
fn build_premount_script(offset_bytes: u64) -> [u8; 2048] {
    let mut script = [0u8; 2048];
    // BusyBox ash does NOT support glob expansion in for-loops, but it
    // DOES support while-read loops with /proc/partitions, which gives
    // us a dynamic list of all block devices — no hardcoded names needed.
    //
    // Uses `losetup -f` to find a free loop device instead of hardcoding
    // /dev/loop0, which may already be in use (e.g. by snap packages).
    //
    // Mount strategy:
    //   /cdrom                  — Ubuntu/casper checks this
    //   /lib/live/mount/medium  — Debian live-boot checks this
    //   We mount at /mnt/choosable first, then bind-mount to both paths
    //   so both initramfs frameworks find the ISO.
    //
    // IMPORTANT: This script is sourced by the hook runner (casper or
    // live-boot).  Use 'return', NEVER 'exit' — exit kills the runner.
    let src = b"\
#!/bin/sh
echo 'choosable OFFSET start' >/tmp/choosable.log
# Load essential kernel modules (may not be auto-loaded in all initramfs environments)
modprobe loop 2>/dev/null
modprobe iso9660 2>/dev/null
mkdir -p /cdrom /lib/live/mount/medium 2>/dev/null
MNT=/mnt/choosable
mkdir -p \"$MNT\" 2>/dev/null
# Check that losetup is available; fall through gracefully if not.
if ! command -v losetup >/dev/null 2>&1; then
  echo 'choosable: losetup not found' >>/tmp/choosable.log
fi
# Write initial debug to /dev/console so it's visible even if /tmp isn't writable
echo 'choosable: scanning partitions...' >/dev/console
while read -r major minor blocks name; do
  # Explicitly skip whole-disk devices so losetup uses partition-relative offsets.
  case \"$name\" in
    loop*|ram*|dm-*|sr*) continue ;;
    sd[a-z]|nvme[0-9]*n[0-9]|mmcblk[0-9]*|vd[a-z]|hd[a-z]) continue ;;
    sd[a-z][0-9]*|nvme[0-9]*n[0-9]*p[0-9]*|mmcblk[0-9]*p[0-9]*|vd[a-z][0-9]*) ;;
    *) continue ;;
  esac
  dev=\"/dev/$name\"
  [ -b \"$dev\" ] || continue
  echo \"choosable: trying $name\" >/dev/console
  echo \"try $name\" >>/tmp/choosable.log
  LOOP=$(losetup -f 2>/dev/null) || continue
  [ -n \"$LOOP\" ] || continue
  losetup -o OFFSET \"$LOOP\" \"$dev\" 2>>/tmp/choosable.log || continue
  sleep 1
  echo \"loopok $name\" >>/tmp/choosable.log
  mount -t iso9660 -o ro \"$LOOP\" \"$MNT\" 2>>/tmp/choosable.log || {
    echo \"choosable: mount iso9660 failed on $name\" >/dev/console
    continue
  }
  echo \"mntok $name\" >>/tmp/choosable.log
  # Bind-mount to casper's expected path
  mount -o bind \"$MNT\" /cdrom 2>>/tmp/choosable.log
  # Bind-mount to live-boot's expected path
  mount -o bind \"$MNT\" /lib/live/mount/medium 2>>/tmp/choosable.log
  echo \"choosable: mounted ISO at /cdrom from $name\" >/dev/console
  # Check for recognizable ISO content at either location
  FOUND=0
  for d in \"$MNT\" /cdrom; do
    if [ -f \"$d/casper/filesystem.squashfs\" ] || \
       [ -f \"$d/casper/filesystem.squashfs.gpg\" ] || \
       [ -f \"$d/live/filesystem.squashfs\" ] || \
       [ -f \"$d/LiveOS/squashfs.img\" ] || \
       [ -f \"$d/images/install.img\" ] || \
       [ -f \"$d/.disk/info\" ] || \
       [ -f \"$d/dists/stable/Release\" ]; then
      FOUND=1
    fi
  done
  if [ \"$FOUND\" = \"1\" ]; then
    echo \"choosable: FOUND content on $name\" >/dev/console
    echo \"found $name\" >>/tmp/choosable.log
    return 0
  fi
  echo \"choosable: no content found on $name\" >/dev/console
  echo \"notfound $name\" >>/tmp/choosable.log
  umount /cdrom 2>/dev/null
  umount /lib/live/mount/medium 2>/dev/null
  umount \"$MNT\" 2>/dev/null
  losetup -d \"$LOOP\" 2>/dev/null
done < /proc/partitions
echo 'choosable: gave up - no ISO found on any partition' >/dev/console
echo 'gaveup' >>/tmp/choosable.log
# Do NOT return 1 - casper hook runner may abort the entire chain on non-zero.
# Let the built-in 20iso_scan run as fallback.
return 0
";

    let off_str = format_decimal_u64(offset_bytes);
    // Find first non-zero digit position in off_str
    let mut off_start = 0;
    while off_start < 20 && off_str[off_start] == b'0' { off_start += 1; }

    let mut pos = 0usize;
    let bytes = src;
    let mut i = 0;
    while i < bytes.len() {
        if i + 6 <= bytes.len()
            && bytes[i] == b'O' && bytes[i+1] == b'F' && bytes[i+2] == b'F'
            && bytes[i+3] == b'S' && bytes[i+4] == b'E' && bytes[i+5] == b'T'
        {
            for j in off_start..21 {
                if pos < 2047 { script[pos] = off_str[j]; pos += 1; }
            }
            i += 6;
        } else {
            if pos < 2047 { script[pos] = bytes[i]; pos += 1; }
            i += 1;
        }
    }

    script
}

/// Generate a cpio "newc" archive header for a file.
fn cpio_newc_header(
    buf: &mut [u8],
    name: &[u8],
    file_size: u32,
    mode: u32,
) -> usize {
    let magic = b"070701";
    let name_len = name.len() as u32 + 1;
    let padded_name_len = ((110 + name_len as usize + 3) & !3) - 110;
    let header_fields: [u32; 13] = [
        1, mode, 0, 0, 1, 0, file_size, 0, 0, 0, 0, name_len, 0,
    ];
    let header_buf_len = 6 + 13 * 8;
    buf[..6].copy_from_slice(magic);
    let mut pos = 6usize;
    for &v in &header_fields {
        let s = [
            hex_nibble(((v >> 28) & 0xF) as u8),
            hex_nibble(((v >> 24) & 0xF) as u8),
            hex_nibble(((v >> 20) & 0xF) as u8),
            hex_nibble(((v >> 16) & 0xF) as u8),
            hex_nibble(((v >> 12) & 0xF) as u8),
            hex_nibble(((v >> 8) & 0xF) as u8),
            hex_nibble(((v >> 4) & 0xF) as u8),
            hex_nibble((v & 0xF) as u8),
        ];
        buf[pos..pos + 8].copy_from_slice(&s);
        pos += 8;
    }
    buf[pos..pos + name.len()].copy_from_slice(name);
    pos += name.len();
    buf[pos] = 0; pos += 1;
    while pos < header_buf_len + padded_name_len { buf[pos] = 0; pos += 1; }
    header_buf_len + padded_name_len
}

/// Build a premount cpio archive and return it.
///
/// `relative_sector_offset` is the ISO file's start position within the
/// partition, in 512-byte sectors (i.e., iso_lba - part1_lba).
pub fn prepare_premount_initrd(
    bs: &mut BootServices,
    relative_sector_offset: u64,
) -> Option<PremountBundle> {
    let offset_bytes = relative_sector_offset * 512;

    let script = build_premount_script(offset_bytes);
    let script_len = script.iter().position(|&c| c == 0).unwrap_or(2047);

    // cpio grows with multiple entries — safe margin
    // 5 entries (4× script + trailer)
    let cpio_estimate = 12288usize;
    let mut cpio_ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.allocate_pool)(MemoryType::EfiLoaderData, cpio_estimate, &mut cpio_ptr)
    };
    if status != EFI_SUCCESS || cpio_ptr.is_null() { return None; }
    let cpio = unsafe { core::slice::from_raw_parts_mut(cpio_ptr as *mut u8, cpio_estimate) };
    let mut off = 0usize;

    let mut append_entry = |cpio: &mut [u8], off: &mut usize, name: &[u8], data: &[u8], mode: u32| -> bool {
        let name_len = name.len() + 1;
        let padded_name_len = ((110 + name_len + 3) & !3) - 110;
        let hdr_len = 110 + padded_name_len;
        let pad = (4 - ((*off + hdr_len + data.len()) & 3)) & 3;
        if *off + hdr_len + data.len() + pad > cpio.len() {
            return false;
        }
        let actual_hdr_len = cpio_newc_header(&mut cpio[*off..], name, data.len() as u32, mode);
        *off += actual_hdr_len;
        let data_start = *off;
        cpio[data_start..data_start + data.len()].copy_from_slice(data);
        *off += data.len();
        for _ in 0..pad { cpio[*off] = 0; *off += 1; }
        true
    };

    // CRITICAL: Do NOT inject empty directory entries — cpio
    // concatenation replaces existing directories, which would
    // delete the ISO's original scripts.
    // Only add the file entries; directories already exist.
    //
    // STRATEGY: Inject 00choosable into hook directories so it runs
    // BEFORE the distro's built-in iso-scan scripts (typically
    // 20iso_scan).  If 00choosable succeeds, the ISO is already
    // mounted and 20iso_scan finds it.  If it fails, 20iso_scan
    // runs normally as a fallback — we NEVER override 20iso_scan.
    //
    // Hook directories:
    //   scripts/live/               — Debian live-boot (main)
    //   scripts/live-premount/      — Debian live-boot (premount)
    //   scripts/casper-premount/    — Ubuntu/casper (premount)
    //   scripts/casper-bottom/      — Ubuntu/casper (bottom)
    //
    // File: scripts/live/00choosable (Debian live-boot primary)
    if !append_entry(cpio, &mut off, b"scripts/live/00choosable", &script[..script_len], 0o100755) {
        unsafe { (bs.free_pool)(cpio_ptr); }
        return None;
    }
    // File: scripts/live-premount/00choosable (Debian live-boot premount)
    if !append_entry(cpio, &mut off, b"scripts/live-premount/00choosable", &script[..script_len], 0o100755) {
        unsafe { (bs.free_pool)(cpio_ptr); }
        return None;
    }
    // File: scripts/casper-premount/00choosable (Ubuntu/casper primary)
    if !append_entry(cpio, &mut off, b"scripts/casper-premount/00choosable", &script[..script_len], 0o100755) {
        unsafe { (bs.free_pool)(cpio_ptr); }
        return None;
    }
    // File: scripts/casper-bottom/00choosable (Ubuntu/casper bottom)
    if !append_entry(cpio, &mut off, b"scripts/casper-bottom/00choosable", &script[..script_len], 0o100755) {
        unsafe { (bs.free_pool)(cpio_ptr); }
        return None;
    }
    // Trailer
    if !append_entry(cpio, &mut off, b"TRAILER!!!", b"", 0) {
        unsafe { (bs.free_pool)(cpio_ptr); }
        return None;
    }

    Some(PremountBundle {
        cpio_buf: cpio_ptr as *mut u8,
        cpio_size: off,
        iso_offset_bytes: offset_bytes,
    })
}