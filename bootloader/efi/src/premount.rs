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
    pub cpio_buf: *mut u8,
    pub cpio_size: usize,
    /// Actual allocated size of `cpio_buf` (equal to `cpio_estimate`).
    /// Always >= cpio_size.  Callers must not write beyond this bound.
    pub cpio_alloc_size: usize,
    pub iso_offset_bytes: u64,
}

pub trait EarlyBootFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle>;
}

pub struct CasperFixup;
impl EarlyBootFixup for CasperFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle> {
        let iso = ctx.selected_iso()?;
        let rel = iso.file_start_lba - ctx.partition_start_lba;
        prepare_premount_initrd(bs, rel, true)
    }
}

pub struct LiveBootFixup;
impl EarlyBootFixup for LiveBootFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle> {
        let iso = ctx.selected_iso()?;
        let rel = iso.file_start_lba - ctx.partition_start_lba;
        prepare_premount_initrd(bs, rel, false)
    }
}

pub struct DracutFixup;
impl EarlyBootFixup for DracutFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle> {
        let iso = ctx.selected_iso()?;
        let rel = iso.file_start_lba - ctx.partition_start_lba;
        prepare_premount_initrd(bs, rel, false)
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

fn build_premount_script(offset_bytes: u64, needs_sr_mod: bool) -> [u8; 4096] {
    let mut script = [0u8; 4096];

    let sr_mod_line: &[u8] = if needs_sr_mod {
        b"modprobe sr_mod 2>/dev/null\n"
    } else {
        b""
    };

    let src_template = b"\
#!/bin/sh
echo 'choosable OFFSET start' >/tmp/choosable.log
modprobe loop 2>/dev/null
modprobe iso9660 2>/dev/null
SRMOD
mkdir -p /cdrom /lib/live/mount/medium 2>/dev/null
if ! command -v losetup >/dev/null 2>&1; then
  echo 'choosable: losetup not found' >>/tmp/choosable.log
fi
echo 'choosable: scanning partitions...' >/dev/console
while read -r major minor blocks name; do
  case \"$name\" in
    loop*|ram*|dm-*|sr*) continue ;;
    *[0-9]) ;;
    *) continue ;;
  esac
  dev=\"/dev/$name\"
  [ -b \"$dev\" ] || continue
  echo \"choosable: trying $name\" >/dev/console
  echo \"try $name\" >>/tmp/choosable.log
  LOOP=/dev/loop0
  if [ -b \"$LOOP\" ] && losetup \"$LOOP\" >/dev/null 2>&1; then
    LOOP=$(losetup -f 2>/dev/null) || continue
  fi
  [ -n \"$LOOP\" ] || continue
  losetup -o OFFSET \"$LOOP\" \"$dev\" 2>>/tmp/choosable.log || {
    if [ \"$LOOP\" = \"/dev/loop0\" ]; then
      LOOP=$(losetup -f 2>/dev/null) || continue
      losetup -o OFFSET \"$LOOP\" \"$dev\" 2>>/tmp/choosable.log || continue
    else
      continue
    fi
  }
  echo \"loopok $name $LOOP\" >>/tmp/choosable.log
  mount -t iso9660 -o ro \"$LOOP\" /cdrom 2>>/tmp/choosable.log || {
    echo \"choosable: mount iso9660 failed on $name\" >/dev/console
    losetup -d \"$LOOP\" 2>/dev/null
    continue
  }
  mount --make-rshared /cdrom 2>>/tmp/choosable.log
  echo \"mntok $name\" >>/tmp/choosable.log
  mount -o bind /cdrom /lib/live/mount/medium 2>>/tmp/choosable.log
  echo \"choosable: mounted ISO at /cdrom from $name\" >/dev/console
  FOUND=0
  for d in /cdrom; do
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
    break
  fi
  echo \"choosable: no content found on $name\" >/dev/console
  echo \"notfound $name\" >>/tmp/choosable.log
  umount /lib/live/mount/medium 2>/dev/null
  umount /cdrom 2>/dev/null
  losetup -d \"$LOOP\" 2>/dev/null
done < /proc/partitions
echo 'choosable: gave up - no ISO found on any partition' >/dev/console
echo 'gaveup' >>/tmp/choosable.log
";

    let off_str = format_decimal_u64(offset_bytes);
    let mut off_start = 0;
    while off_start < 20 && off_str[off_start] == b'0' { off_start += 1; }

    let mut pos = 0usize;
    let bytes = src_template;
    let sr_mod_len = sr_mod_line.len();
    let mut i = 0;
    while i < bytes.len() {
        if i + 5 <= bytes.len()
            && bytes[i] == b'S' && bytes[i+1] == b'R'
            && bytes[i+2] == b'M' && bytes[i+3] == b'O'
            && bytes[i+4] == b'D'
        {
            for j in 0..sr_mod_len {
                if pos < 4095 { script[pos] = sr_mod_line[j]; pos += 1; }
            }
            i += 5;
        } else if i + 6 <= bytes.len()
            && bytes[i] == b'O' && bytes[i+1] == b'F' && bytes[i+2] == b'F'
            && bytes[i+3] == b'S' && bytes[i+4] == b'E' && bytes[i+5] == b'T'
        {
            for j in off_start..21 {
                if pos < 4095 { script[pos] = off_str[j]; pos += 1; }
            }
            i += 6;
        } else {
            if pos < 4095 { script[pos] = bytes[i]; pos += 1; }
            i += 1;
        }
    }

    script
}

fn cpio_newc_header(buf: &mut [u8], name: &[u8], file_size: u32, mode: u32) -> usize {
    let magic = b"070701";
    let name_len = name.len() as u32 + 1;
    let padded_name_len = ((110 + name_len as usize + 3) & !3) - 110;
    let header_fields: [u32; 13] = [1, mode, 0, 0, 1, 0, file_size, 0, 0, 0, 0, name_len, 0];
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

pub fn prepare_premount_initrd(
    bs: &mut BootServices,
    relative_sector_offset: u64,
    needs_sr_mod: bool,
) -> Option<PremountBundle> {
    let offset_bytes = relative_sector_offset * 512;

    let script = build_premount_script(offset_bytes, needs_sr_mod);
    let script_len = script.iter().position(|&c| c == 0).unwrap_or(4095);

    let entry_size = |name_len: usize, data_len: usize| -> usize {
        let padded_name_len = ((110 + name_len + 1 + 3) & !3) - 110;
        110 + padded_name_len + data_len + 3
    };
    let cpio_estimate = entry_size(24, script_len)  // scripts/live/00choosable
        + entry_size(33, script_len)  // scripts/live-premount/00choosable
        + entry_size(35, script_len)  // scripts/casper-premount/00choosable
        + entry_size(33, script_len)  // scripts/casper-bottom/00choosable
        + entry_size(10, 0);          // TRAILER!!!
    let mut cpio_ptr: *mut c_void = core::ptr::null_mut();
    if unsafe { (bs.allocate_pool)(MemoryType::EfiLoaderData, cpio_estimate, &mut cpio_ptr) } != EFI_SUCCESS || cpio_ptr.is_null() { return None; }
    let cpio = unsafe { core::slice::from_raw_parts_mut(cpio_ptr as *mut u8, cpio_estimate) };
    let mut off = 0usize;

    let mut append_entry = |cpio: &mut [u8], off: &mut usize, name: &[u8], data: &[u8], mode: u32| -> bool {
        let name_len = name.len() + 1;
        let padded_name_len = ((110 + name_len + 3) & !3) - 110;
        let hdr_len = 110 + padded_name_len;
        let pad = (4 - ((*off + hdr_len + data.len()) & 3)) & 3;
        if *off + hdr_len + data.len() + pad > cpio.len() { return false; }
        let actual = cpio_newc_header(&mut cpio[*off..], name, data.len() as u32, mode);
        *off += actual;
        cpio[*off..*off + data.len()].copy_from_slice(data);
        *off += data.len();
        for _ in 0..pad { cpio[*off] = 0; *off += 1; }
        true
    };

    if !append_entry(cpio, &mut off, b"scripts/live/00choosable", &script[..script_len], 0o100755) { unsafe { (bs.free_pool)(cpio_ptr); } return None; }
    if !append_entry(cpio, &mut off, b"scripts/live-premount/00choosable", &script[..script_len], 0o100755) { unsafe { (bs.free_pool)(cpio_ptr); } return None; }
    if !append_entry(cpio, &mut off, b"scripts/casper-premount/00choosable", &script[..script_len], 0o100755) { unsafe { (bs.free_pool)(cpio_ptr); } return None; }
    if !append_entry(cpio, &mut off, b"scripts/casper-bottom/00choosable", &script[..script_len], 0o100755) { unsafe { (bs.free_pool)(cpio_ptr); } return None; }
    if !append_entry(cpio, &mut off, b"TRAILER!!!", b"", 0) { unsafe { (bs.free_pool)(cpio_ptr); } return None; }

    Some(PremountBundle { cpio_buf: cpio_ptr as *mut u8, cpio_size: off, cpio_alloc_size: cpio_estimate, iso_offset_bytes: offset_bytes })
}