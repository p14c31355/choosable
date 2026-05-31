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
use crate::protocol::{BootServices, MemoryType, EFI_SUCCESS};

pub struct PremountBundle {
    /// Pool-allocated cpio archive
    pub cpio_buf: *mut u8,
    /// cpio archive size in bytes
    pub cpio_size: usize,
    /// ISO file start LBA on the real partition (512-byte sectors)
    pub iso_offset_bytes: u64,
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
fn build_premount_script(offset_bytes: u64) -> [u8; 512] {
    let mut script = [0u8; 512];
    let src = b"\
#!/bin/sh
# Choosable premount: loop-mounts ISO from raw partition offset
echo '[choosable] premount starting, offset=OFFSET'
for dev in /dev/sd[a-z][0-9]* /dev/nvme[0-9]*p[0-9]* /dev/mmcblk[0-9]*p[0-9]*; do
  [ -b \"$dev\" ] || continue
  losetup -o OFFSET /dev/loop0 \"$dev\" 2>/dev/null || continue
  if mount -t iso9660 -o ro /dev/loop0 /cdrom 2>/dev/null; then
    echo '[choosable] premount: ISO mounted at /cdrom via '$dev
    ls /cdrom/casper/filesystem.squashfs 2>/dev/null && break
    umount /cdrom 2>/dev/null
    losetup -d /dev/loop0 2>/dev/null
  fi
done
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
                if pos < 511 { script[pos] = off_str[j]; pos += 1; }
            }
            i += 6;
        } else {
            if pos < 511 { script[pos] = bytes[i]; pos += 1; }
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
    let padded_name_len = (name_len as usize + 3) & !3;
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
pub fn prepare_premount_initrd(
    bs: &mut BootServices,
    iso_lba: u64,
) -> Option<PremountBundle> {
    let offset_bytes = iso_lba * 512;

    let script = build_premount_script(offset_bytes);
    let script_len = script.iter().position(|&c| c == 0).unwrap_or(1023);

    let cpio_estimate = 2048usize;
    let mut cpio_ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.allocate_pool)(MemoryType::EfiLoaderData, cpio_estimate, &mut cpio_ptr)
    };
    if status != EFI_SUCCESS || cpio_ptr.is_null() { return None; }
    let cpio = unsafe { core::slice::from_raw_parts_mut(cpio_ptr as *mut u8, cpio_estimate) };
    let mut off = 0usize;

    let mut append_entry = |cpio: &mut [u8], off: &mut usize, name: &[u8], data: &[u8], mode: u32| -> bool {
        let name_len = name.len() + 1;
        let padded_name_len = (name_len + 3) & !3;
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

    // Directory: scripts/
    if !append_entry(cpio, &mut off, b"scripts", b"", 0o40755) { return None; }
    // Directory: scripts/casper-premount/
    if !append_entry(cpio, &mut off, b"scripts/casper-premount", b"", 0o40755) { return None; }
    // File: scripts/casper-premount/choosable
    if !append_entry(cpio, &mut off, b"scripts/casper-premount/choosable", &script[..script_len], 0o100755) { return None; }
    // Trailer
    if !append_entry(cpio, &mut off, b"TRAILER!!!", b"", 0) { return None; }

    Some(PremountBundle {
        cpio_buf: cpio_ptr as *mut u8,
        cpio_size: off,
        iso_offset_bytes: offset_bytes,
    })
}