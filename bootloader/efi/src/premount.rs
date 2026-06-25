// ═══════════════════════════════════════════════════════════════════════════
//  Premount initrd builder — creates loop device from raw partition offset
// ═══════════════════════════════════════════════════════════════════════════

use core::ffi::c_void;
use crate::boot_context::BootContext;
use crate::protocol::{BootServices, MemoryType, EFI_SUCCESS};

pub struct PremountBundle {
    pub cpio_buf: *mut u8,
    pub cpio_size: usize,
    pub cpio_alloc_size: usize,
    pub iso_offset_bytes: u64,
}

pub trait EarlyBootFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle>;
}

// ── Macro-generated fixup structs ────────────────────────────────────────

macro_rules! fixup {
    ($name:ident, $ctor:path $(, $arg:literal)?) => {
        pub struct $name;
        impl EarlyBootFixup for $name {
            fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle> {
                let p = ctx.selected_payload()?;
                let rel = p.file_start_lba - ctx.partition_start_lba;
                let name_bytes = &p.name[..p.name_len.min(p.name.len())];
                $ctor(bs, rel $(, $arg)?, name_bytes)
            }
        }
    };
}

fixup!(CasperFixup,         prepare_premount_initrd, true);
fixup!(LiveBootFixup,       prepare_premount_initrd, false);
fixup!(DracutFixup,         prepare_premount_initrd, false);
fixup!(AlpinePremountFixup, prepare_premount_initrd, false);
fixup!(ArchFixup,           prepare_arch_initrd);
fixup!(AlpineFixup,         prepare_alpine_initrd);

pub struct WindowsPEFixup;
impl EarlyBootFixup for WindowsPEFixup {
    fn build_initrd(&self, _ctx: &BootContext, _bs: &mut BootServices) -> Option<PremountBundle> { None }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Template engine — substitutes OFFSET / SRMOD / LOGNAME placeholders
// ═══════════════════════════════════════════════════════════════════════════

/// Copy template bytes to `dst`, replacing a marker of `marker_len` bytes
/// (matching `marker`) with `replacement`. `buf_cap` = max bytes written.
fn subst_template(dst: &mut [u8], src: &[u8], marker: &[u8], replacement: &[u8], buf_cap: usize) -> usize {
    let mut pos = 0;
    let mut i = 0;
    while i < src.len() {
        if i + marker.len() <= src.len() && &src[i..i + marker.len()] == marker {
            let max = replacement.len().min(buf_cap.saturating_sub(pos));
            dst[pos..pos + max].copy_from_slice(&replacement[..max]);
            pos += max;
            i += marker.len();
        } else {
            if pos < buf_cap { dst[pos] = src[i]; pos += 1; }
            i += 1;
        }
    }
    pos
}

fn strip_leading_zeros(buf: &[u8; 21]) -> &[u8] {
    let mut s = 0;
    while s < 20 && buf[s] == b'0' { s += 1; }
    if s >= 20 { &buf[20..] } else { &buf[s..] }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Arch-specific premount initrd builder
// ═══════════════════════════════════════════════════════════════════════════

pub fn prepare_arch_initrd(
    bs: &mut BootServices,
    relative_sector_offset: u64,
    iso_name: &[u8],
) -> Option<PremountBundle> {
    let offset_bytes = relative_sector_offset * 512;
    let premount_script = build_premount_script(offset_bytes, false);
    let premount_len = premount_script.iter().position(|&c| c == 0).unwrap_or(8191);

    let bottom_script = build_bottom_script(iso_name);
    let bottom_len = bottom_script.iter().position(|&c| c == 0).unwrap_or(2047);

    // Arch /init wrapper — exec's /init.orig after premount
    let mut wrapper = [0u8; 8192];
    let wrapper_src = b"\
#!/bin/sh
# Choosable Arch init wrapper
mkdir -p /tmp 2>/dev/null
exec >/tmp/choosable.log 2>&1
echo 'choosable (arch): premount starting' >/dev/console
modprobe loop 2>/dev/null;modprobe iso9660 2>/dev/null
modprobe exfat 2>/dev/null;modprobe ntfs3 2>/dev/null
modprobe virtio_blk 2>/dev/null;modprobe virtio_pci 2>/dev/null
for i in 0 1 2 3 4 5 6 7;do [ -b /dev/loop$i ]||mknod /dev/loop$i b 7 $i 2>/dev/null;done
[ -d /cdrom ]||mkdir -p /cdrom /run/archiso/bootmnt 2>/dev/null
sleep 5
N=0;while [ $N -lt 30 ];do N=$((N+1))
while read -r a b c d;do case $d in loop*|ram*|dm-*|sr*)continue;;*[0-9]);;*)continue;;esac
dev=/dev/$d;[ -b $dev ]||continue
LOOP=;for i in 0 1 2 3 4 5 6 7;do L=/dev/loop$i
losetup $L >/dev/null 2>&1&&continue
losetup -o OFFSET $L $dev 2>/dev/null||{ losetup -d $L 2>/dev/null;continue;}
LOOP=$L;break;done;[ -n \"$LOOP\" ]||continue
mount -t iso9660 -o ro $LOOP /cdrom 2>/dev/null||{ losetup -d $LOOP 2>/dev/null;continue;}
[ -d /cdrom/arch ]&&{ mkdir -p /run/archiso/bootmnt 2>/dev/null;mount -o bind /cdrom /run/archiso/bootmnt 2>/dev/null;break 2;}
umount /cdrom 2>/dev/null;losetup -d $LOOP 2>/dev/null
done</proc/partitions
sleep 1;done
echo 'choosable (arch): done, exec /init.orig' >/dev/console
if [ -x /init.orig ];then exec /init.orig \"$@\"
else echo 'choosable (arch): /init.orig not found, trying /init' >/dev/console
[ -x /init ]&&exec /init \"$@\";fi
exec /bin/sh
";
    let dec = format_decimal_u64(offset_bytes);
    let off_slice = strip_leading_zeros(&dec);
    let wrapper_len = subst_template(&mut wrapper, wrapper_src, b"OFFSET", off_slice, 8191);

    let names: &[&[u8]] = &[b"init", b"hooks/choosable", b"scripts/casper-premount/00choosable", b"scripts/casper-bottom/00choosable"];
    let data: &[&[u8]] = &[&wrapper[..wrapper_len], &premount_script[..premount_len], &premount_script[..premount_len], &bottom_script[..bottom_len]];
    build_cpio(bs, names, data, offset_bytes)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Alpine-specific premount initrd builder
// ═══════════════════════════════════════════════════════════════════════════

pub fn prepare_alpine_initrd(
    bs: &mut BootServices,
    relative_sector_offset: u64,
    _iso_name: &[u8],
) -> Option<PremountBundle> {
    let offset_bytes = relative_sector_offset * 512;
    let dec = format_decimal_u64(offset_bytes);
    let off_slice = strip_leading_zeros(&dec);

    let mut script = [0u8; 4096];
    let src = b"\
#!/bin/sh
mount -t proc none /proc 2>/dev/null
mount -t sysfs none /sys 2>/dev/null
mount -t devtmpfs none /dev 2>/dev/null
modprobe loop 2>/dev/null;modprobe iso9660 2>/dev/null
for i in 0 1 2 3 4 5 6 7;do [ -b /dev/loop$i ]||mknod /dev/loop$i b 7 $i 2>/dev/null;done
[ -d /cdrom ]||mkdir -p /cdrom 2>/dev/null
for dev in /dev/[sv]d*[0-9] /dev/nvme*n1p* /dev/mmcblk*p*;do
 [ -b \"$dev\" ]||continue
 for i in 0 1 2 3 4 5 6 7;do
  L=/dev/loop$i
  losetup $L >/dev/null 2>&1&&continue
  losetup -o OFFSET $L \"$dev\" 2>/dev/null||continue
  mount -t iso9660 -ro $L /cdrom 2>/dev/null||{ losetup -d $L 2>/dev/null;continue;}
  if [ -f /cdrom/.alpine-release ]||[ -f /cdrom/.ALPINE_RELEASE ];then
   exec /init \"$@\"
  fi
  umount /cdrom 2>/dev/null;losetup -d $L 2>/dev/null
 done
done
exec /init \"$@\"
";
    let script_len = subst_template(&mut script, src, b"OFFSET", off_slice, 4095);

    build_cpio(bs, &[b"init.choosable"], &[&script[..script_len]], offset_bytes)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Pure functions (testable)
// ═══════════════════════════════════════════════════════════════════════════

fn hex_nibble(n: u8) -> u8 {
    if n < 10 { b'0' + n } else { b'A' + (n - 10) }
}

fn format_decimal_u64(v: u64) -> [u8; 21] {
    let mut buf = [b'0'; 21];
    let mut val = v;
    let mut pos = 20;
    if val == 0 { return buf; }
    loop {
        buf[pos] = b'0' + (val % 10) as u8;
        val /= 10;
        if val == 0 { break; }
        pos -= 1;
    }
    buf
}

fn sanitize_filename(input: &[u8]) -> ([u8; 320], usize) {
    let mut buf = [0u8; 320];
    let mut pos = 0;
    for &b in input {
        if b == 0 || pos >= 319 { break; }
        buf[pos] = match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => b,
            _ => b'_',
        };
        pos += 1;
    }
    (buf, pos)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Script builders
// ═══════════════════════════════════════════════════════════════════════════

fn build_premount_script(offset_bytes: u64, needs_sr_mod: bool) -> [u8; 8192] {
    let mut script = [0u8; 8192];
    let sr_mod_line: &[u8] = if needs_sr_mod { b"modprobe sr_mod 2>/dev/null\n" } else { b"" };
    let dec = format_decimal_u64(offset_bytes);
    let off_slice = strip_leading_zeros(&dec);

    let src = b"\
#!/bin/sh
mkdir -p /tmp 2>/dev/null
exec >/tmp/choosable.log 2>&1
echo 'choosable: starting premount' >/dev/console
modprobe loop 2>/dev/null;modprobe iso9660 2>/dev/null
modprobe exfat 2>/dev/null;modprobe ntfs3 2>/dev/null
modprobe virtio_blk 2>/dev/null;modprobe virtio_pci 2>/dev/null
SRMOD
for i in 0 1 2 3 4 5 6 7;do [ -b /dev/loop$i ]||mknod /dev/loop$i b 7 $i 2>/dev/null;done
[ -d /cdrom ]||mkdir -p /cdrom /lib/live/mount/medium 2>/dev/null
sleep 5
N=0;while [ $N -lt 60 ];do N=$((N+1))
while read -r a b c d;do case $d in loop*|ram*|dm-*|sr*)continue;;*[0-9]);;*)continue;;esac
dev=/dev/$d;[ -b $dev ]||continue
LOOP=;for i in 0 1 2 3 4 5 6 7;do L=/dev/loop$i
losetup $L >/dev/null 2>&1&&continue
losetup -o OFFSET $L $dev 2>/dev/null||{ losetup -d $L 2>/dev/null;continue;}
LOOP=$L;break;done;[ -n \"$LOOP\" ]||continue
mount -t iso9660 -o ro $LOOP /cdrom 2>/dev/null||{ losetup -d $LOOP 2>/dev/null;continue;}
mount --make-rshared /cdrom 2>/dev/null
mount -o bind /cdrom /lib/live/mount/medium 2>/dev/null
[ -f /cdrom/casper/filesystem.squashfs ]&&return 0
[ -d /cdrom/pop-os/casper_persist ]&&return 0
[ -f /cdrom/casper/filesystem.* ]&&return 0
[ -f /cdrom/live/filesystem.squashfs ]&&return 0
[ -f /cdrom/LiveOS/squashfs.img ]&&return 0
[ -f /cdrom/.alpine-release ]&&return 0
[ -f /cdrom/.ALPINE_RELEASE ]&&return 0
[ -d /cdrom/apks ]&&return 0
[ -d /cdrom/arch ]&&{ mkdir -p /run/archiso/bootmnt 2>/dev/null;mount -o bind /cdrom /run/archiso/bootmnt 2>/dev/null;return 0;}
[ -f /cdrom/.disk/info ]&&return 0
[ -d /cdrom/distros ]&&return 0
[ -f /cdrom/casper/vmlinuz ]&&return 0
umount /lib/live/mount/medium 2>/dev/null
umount /cdrom 2>/dev/null
losetup -d $LOOP 2>/dev/null
done</proc/partitions
sleep 1;done
echo 'choosable: gave up - no ISO found on any partition' >/dev/console
return 0
";
    let mut pos = 0;
    let mut i = 0;
    while i < src.len() {
        if i + 5 <= src.len() && &src[i..i+5] == b"SRMOD" {
            let max = sr_mod_line.len().min(8191 - pos);
            script[pos..pos + max].copy_from_slice(&sr_mod_line[..max]);
            pos += max; i += 5;
        } else if i + 6 <= src.len() && &src[i..i+6] == b"OFFSET" {
            let max = off_slice.len().min(8191 - pos);
            script[pos..pos + max].copy_from_slice(&off_slice[..max]);
            pos += max; i += 6;
        } else {
            if pos < 8191 { script[pos] = src[i]; pos += 1; }
            i += 1;
        }
    }
    script
}

fn build_bottom_script(iso_name: &[u8]) -> [u8; 2048] {
    let mut script = [0u8; 2048];
    let (safe_name, safe_len) = sanitize_filename(iso_name);

    let logline = {
        let mut buf = [0u8; 320];
        let prefix = b"Choosable-";
        let suffix = b".log";
        buf[..prefix.len()].copy_from_slice(prefix);
        let n = safe_len.min(250);
        buf[prefix.len()..prefix.len() + n].copy_from_slice(&safe_name[..n]);
        let end = prefix.len() + n;
        buf[end..end + suffix.len()].copy_from_slice(suffix);
        let l = (end + suffix.len()).min(319);
        (buf, l)
    };
    let logline_slice = &logline.0[..logline.1];

    let src = b"\
#!/bin/sh
exec >/dev/null 2>&1
modprobe exfat 2>/dev/null;modprobe ntfs3 2>/dev/null
mkdir -p /tmp
while read a b c d;do case $d in loop*|ram*|dm-*|sr*)continue;;*[0-9]);;*)continue;;esac
dev=/dev/$d;[ -b $dev ]||continue
mount -t exfat -o rw $dev /tmp 2>/dev/null||mount -t ntfs3 -o rw $dev /tmp 2>/dev/null||continue
(dmesg;cat /proc/cmdline)>/tmp/LOGNAME
umount /tmp;exit 0
done</proc/partitions
";
    let mut pos = 0;
    let mut i = 0;
    while i < src.len() {
        if i + 7 <= src.len() && &src[i..i+7] == b"LOGNAME" {
            let max = logline_slice.len().min(2047 - pos);
            script[pos..pos + max].copy_from_slice(&logline_slice[..max]);
            pos += max; i += 7;
        } else {
            if pos < 2047 { script[pos] = src[i]; pos += 1; }
            i += 1;
        }
    }
    script
}

// ═══════════════════════════════════════════════════════════════════════════
//  CPIO helpers
// ═══════════════════════════════════════════════════════════════════════════

fn cpio_entry_size(name_len: usize, data_len: usize) -> usize {
    let padded_name_len = ((110 + name_len + 1 + 3) & !3) - 110;
    110 + padded_name_len + data_len + 3
}

fn cpio_newc_header(buf: &mut [u8], name: &[u8], file_size: u32, mode: u32) -> usize {
    buf[..6].copy_from_slice(b"070701");
    let name_len = name.len() as u32 + 1;
    let padded_name_len = ((110 + name_len as usize + 3) & !3) - 110;
    let fields: [u32; 13] = [1, mode, 0, 0, 1, 0, file_size, 0, 0, 0, 0, name_len, 0];
    let mut pos = 6usize;
    for &v in &fields {
        for shift in [28, 24, 20, 16, 12, 8, 4, 0] {
            buf[pos] = hex_nibble(((v >> shift) & 0xF) as u8);
            pos += 1;
        }
    }
    buf[pos..pos + name.len()].copy_from_slice(name);
    pos += name.len();
    buf[pos] = 0; pos += 1;
    while pos < 110 + padded_name_len { buf[pos] = 0; pos += 1; }
    110 + padded_name_len
}

fn cpio_append_entry(cpio: &mut [u8], off: &mut usize, name: &[u8], data: &[u8], mode: u32) -> bool {
    let nlen = name.len() + 1;
    let pnlen = ((110 + nlen + 3) & !3) - 110;
    let hlen = 110 + pnlen;
    let pad = (4 - ((*off + hlen + data.len()) & 3)) & 3;
    if *off + hlen + data.len() + pad > cpio.len() { return false; }
    *off += cpio_newc_header(&mut cpio[*off..], name, data.len() as u32, mode);
    cpio[*off..*off + data.len()].copy_from_slice(data);
    *off += data.len();
    for _ in 0..pad { cpio[*off] = 0; *off += 1; }
    true
}

/// Allocates a CPIO buffer, appends all `names`/`data` pairs + TRAILER!!!,
/// and returns a `PremountBundle`.  Caller must set `cpio_alloc_size` to the
/// pool allocation size.
fn build_cpio(
    bs: &mut BootServices,
    names: &[&[u8]],
    data: &[&[u8]],
    offset_bytes: u64,
) -> Option<PremountBundle> {
    let estimate = names.iter().zip(data.iter())
        .map(|(&n, &d)| cpio_entry_size(n.len(), d.len()))
        .sum::<usize>()
        + cpio_entry_size(10, 0);
    let alloc_size = (estimate + 2047) & !2047;

    let mut cpio_ptr: *mut c_void = core::ptr::null_mut();
    if unsafe { (bs.allocate_pool)(MemoryType::EfiLoaderData, alloc_size, &mut cpio_ptr) } != EFI_SUCCESS || cpio_ptr.is_null() { return None; }
    let cpio = unsafe { core::slice::from_raw_parts_mut(cpio_ptr as *mut u8, alloc_size) };
    let mut off = 0usize;

    for (&name, &d) in names.iter().zip(data.iter()) {
        if !cpio_append_entry(cpio, &mut off, name, d, 0o100755) {
            unsafe { (bs.free_pool)(cpio_ptr); }
            return None;
        }
    }
    if !cpio_append_entry(cpio, &mut off, b"TRAILER!!!", b"", 0) {
        unsafe { (bs.free_pool)(cpio_ptr); }
        return None;
    }
    Some(PremountBundle { cpio_buf: cpio_ptr as *mut u8, cpio_size: off, cpio_alloc_size: alloc_size, iso_offset_bytes: offset_bytes })
}

// ═══════════════════════════════════════════════════════════════════════════
//  Public builder
// ═══════════════════════════════════════════════════════════════════════════

pub fn prepare_premount_initrd(
    bs: &mut BootServices,
    relative_sector_offset: u64,
    needs_sr_mod: bool,
    iso_name: &[u8],
) -> Option<PremountBundle> {
    let offset_bytes = relative_sector_offset * 512;
    let premount_script = build_premount_script(offset_bytes, needs_sr_mod);
    let premount_len = premount_script.iter().position(|&c| c == 0).unwrap_or(8191);
    let bottom_script = build_bottom_script(iso_name);
    let bottom_len = bottom_script.iter().position(|&c| c == 0).unwrap_or(2047);

    let names: &[&[u8]] = &[
        b"scripts/local-premount/00choosable",
        b"scripts/init-premount/00choosable",
        b"scripts/live/00choosable",
        b"scripts/live-premount/00choosable",
        b"scripts/casper-premount/00choosable",
        b"scripts/casper-bottom/00choosable",
    ];
    let data: &[&[u8]] = &[
        &premount_script[..premount_len],
        &premount_script[..premount_len],
        &premount_script[..premount_len],
        &premount_script[..premount_len],
        &premount_script[..premount_len],
        &bottom_script[..bottom_len],
    ];
    build_cpio(bs, names, data, offset_bytes)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_nibble() {
        assert_eq!(hex_nibble(0), b'0');
        assert_eq!(hex_nibble(9), b'9');
        assert_eq!(hex_nibble(10), b'A');
        assert_eq!(hex_nibble(15), b'F');
    }

    #[test]
    fn test_format_decimal_u64() {
        let r = format_decimal_u64(0);
        assert_eq!(&r[20..], b"0");
        let r = format_decimal_u64(42);
        assert_eq!(&r[19..], b"42");
        let r = format_decimal_u64(12345678901234567890);
        // Just verify it doesn't panic and contains digits
        assert!(r.iter().all(|&c| c >= b'0' && c <= b'9'));
    }

    #[test]
    fn test_strip_leading_zeros() {
        let buf = format_decimal_u64(42);
        let s = strip_leading_zeros(&buf);
        assert_eq!(s, b"42");
        let buf = format_decimal_u64(0);
        let s = strip_leading_zeros(&buf);
        assert_eq!(s, b"0");
    }

    #[test]
    fn test_sanitize_filename() {
        let (buf, len) = sanitize_filename(b"Hello World.iso");
        assert_eq!(&buf[..len], b"Hello_World.iso");
        let (buf, len) = sanitize_filename(b"test-v1.0_file.iso");
        assert_eq!(&buf[..len], b"test-v1.0_file.iso");
        let (buf, len) = sanitize_filename(b"bad\xffname.iso");
        assert_eq!(&buf[..len], b"bad_name.iso");
    }

    #[test]
    fn test_cpio_entry_size() {
        // TRAILER!!! (10 chars) => name_len=10, data=0
        let sz = cpio_entry_size(10, 0);
        assert!(sz >= 110 + 1 + 10 + 1); // header + null + name + pad
    }

    #[test]
    fn test_cpio_newc_header_trailer() {
        let mut buf = [0u8; 128];
        let sz = cpio_newc_header(&mut buf, b"TRAILER!!!", 0, 0);
        assert!(sz > 0 && sz < 128);
        // Magic check
        assert_eq!(&buf[..6], b"070701");
        // Name should be null-terminated
        assert_eq!(buf[sz - 1], 0);
    }

    #[test]
    fn test_subst_template() {
        let mut dst = [0u8; 64];
        let src = b"hello OFFSET world";
        let len = subst_template(&mut dst, src, b"OFFSET", b"12345", 63);
        assert_eq!(&dst[..len], b"hello 12345 world");
    }

    #[test]
    fn test_subst_template_no_match() {
        let mut dst = [0u8; 64];
        let src = b"hello world";
        let len = subst_template(&mut dst, src, b"OFFSET", b"12345", 63);
        assert_eq!(&dst[..len], b"hello world");
    }

    #[test]
    fn test_subst_template_multiple() {
        let mut dst = [0u8; 128];
        let src = b"aOFFSETbOFFSETc";
        let len = subst_template(&mut dst, src, b"OFFSET", b"X", 127);
        assert_eq!(&dst[..len], b"aXbXc");
    }

    #[test]
    fn test_subst_template_truncation() {
        let mut dst = [0u8; 10];
        let src = b"hello OFFSET world";
        let len = subst_template(&mut dst, src, b"OFFSET", b"12345678901234", 9);
        // Fits only "hello 123" in 10 bytes (cap=9)
        assert_eq!(&dst[..len], b"hello 123");
    }
}