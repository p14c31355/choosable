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

pub struct CasperFixup;
impl EarlyBootFixup for CasperFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle> {
        let p = ctx.selected_payload()?;
        let rel = p.file_start_lba - ctx.partition_start_lba;
        let name_bytes = &p.name[..p.name_len.min(p.name.len())];
        prepare_premount_initrd(bs, rel, true, name_bytes)
    }
}

pub struct LiveBootFixup;
impl EarlyBootFixup for LiveBootFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle> {
        let p = ctx.selected_payload()?;
        let rel = p.file_start_lba - ctx.partition_start_lba;
        let name_bytes = &p.name[..p.name_len.min(p.name.len())];
        prepare_premount_initrd(bs, rel, false, name_bytes)
    }
}

pub struct DracutFixup;
impl EarlyBootFixup for DracutFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle> {
        let p = ctx.selected_payload()?;
        let rel = p.file_start_lba - ctx.partition_start_lba;
        let name_bytes = &p.name[..p.name_len.min(p.name.len())];
        prepare_premount_initrd(bs, rel, false, name_bytes)
    }
}

/// ArchISO fixup — mounts ISO at /run/archiso/bootmnt for archiso initramfs hook.
pub struct ArchFixup;
impl EarlyBootFixup for ArchFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle> {
        let p = ctx.selected_payload()?;
        let rel = p.file_start_lba - ctx.partition_start_lba;
        let name_bytes = &p.name[..p.name_len.min(p.name.len())];
        prepare_premount_initrd(bs, rel, false, name_bytes)
    }
}

/// Windows PE fixup — no premount initrd needed (WinPE uses boot.sdi + boot.wim via BCD).
pub struct WindowsPEFixup;
impl EarlyBootFixup for WindowsPEFixup {
    fn build_initrd(&self, _ctx: &BootContext, _bs: &mut BootServices) -> Option<PremountBundle> {
        None
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Alpine fixup — custom /init.choosable (Alpine has no hook mechanism)
// ═══════════════════════════════════════════════════════════════════════════

pub struct AlpineFixup;
impl EarlyBootFixup for AlpineFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle> {
        let p = ctx.selected_payload()?;
        let rel = p.file_start_lba - ctx.partition_start_lba;
        let name_bytes = &p.name[..p.name_len.min(p.name.len())];
        prepare_alpine_initrd(bs, rel, name_bytes)
    }
}

/// AlpinePremount fixup — uses the standard premount initrd (casper-style)
/// instead of a custom /init.choosable script.  This is the recommended
/// path for Alpine because it doesn't rely on Alpine's own initramfs
/// behavior.
pub struct AlpinePremountFixup;
impl EarlyBootFixup for AlpinePremountFixup {
    fn build_initrd(&self, ctx: &BootContext, bs: &mut BootServices) -> Option<PremountBundle> {
        let p = ctx.selected_payload()?;
        let rel = p.file_start_lba - ctx.partition_start_lba;
        let name_bytes = &p.name[..p.name_len.min(p.name.len())];
        // Alpine doesn't need sr_mod since it doesn't have a real CD-ROM
        // driver dependency; loop+iso9660 are enough.
        prepare_premount_initrd(bs, rel, false, name_bytes)
    }
}

/// Build a CPIO with `/init.choosable` that mounts the ISO and then exec's
/// the distro's original `/init`.  This is necessary for Alpine Linux whose
/// initramfs does NOT source casper-premount or live-premount hooks.
pub fn prepare_alpine_initrd(
    bs: &mut BootServices,
    relative_sector_offset: u64,
    iso_name: &[u8],
) -> Option<PremountBundle> {
    let offset_bytes = relative_sector_offset * 512;

    // Build the /init.choosable script
    let off_str = format_decimal_u64(offset_bytes);
    let mut off_start = 0;
    while off_start < 20 && off_str[off_start] == b'0' { off_start += 1; }
    if off_start >= 20 { off_start = 19; }
    let off_slice = &off_str[off_start..];

    // Template: mount proc/sys/dev, create loop, mount ISO, then exec /init
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

    let mut pos = 0usize;
    let mut i = 0;
    while i < src.len() {
        if i + 6 <= src.len()
            && src[i] == b'O' && src[i+1] == b'F' && src[i+2] == b'F'
            && src[i+3] == b'S' && src[i+4] == b'E' && src[i+5] == b'T'
        {
            for j in 0..off_slice.len() {
                if pos < 4095 { script[pos] = off_slice[j]; pos += 1; }
            }
            i += 6;
        } else {
            if pos < 4095 { script[pos] = src[i]; pos += 1; }
            i += 1;
        }
    }
    let script_len = script.iter().position(|&c| c == 0).unwrap_or(4095);

    // Build CPIO with a single file: /init.choosable
    let name = b"init.choosable";
    let name_len = name.len() + 1;
    let padded_name_len = ((110 + name_len + 3) & !3) - 110;
    let hdr_len = 110 + padded_name_len;
    let pad = (4 - ((hdr_len + script_len) & 3)) & 3;
    let total = hdr_len + script_len + pad + 110 + 10 + 0 + 3; // header + data + trailer roughly
    let alloc_size = (total + 2047) & !2047;

    let mut cpio_ptr: *mut c_void = core::ptr::null_mut();
    if unsafe { (bs.allocate_pool)(MemoryType::EfiLoaderData, alloc_size, &mut cpio_ptr) } != EFI_SUCCESS || cpio_ptr.is_null() { return None; }
    let cpio = unsafe { core::slice::from_raw_parts_mut(cpio_ptr as *mut u8, alloc_size) };
    let mut off = 0usize;

    let mut append_entry = |cpio: &mut [u8], off: &mut usize, name: &[u8], data: &[u8], mode: u32| -> bool {
        let nlen = name.len() + 1;
        let pnlen = ((110 + nlen + 3) & !3) - 110;
        let hlen = 110 + pnlen;
        let p = (4 - ((*off + hlen + data.len()) & 3)) & 3;
        if *off + hlen + data.len() + p > cpio.len() { return false; }
        let actual = cpio_newc_header(&mut cpio[*off..], name, data.len() as u32, mode);
        *off += actual;
        cpio[*off..*off + data.len()].copy_from_slice(data);
        *off += data.len();
        for _ in 0..p { cpio[*off] = 0; *off += 1; }
        true
    };

    if !append_entry(cpio, &mut off, b"init.choosable", &script[..script_len], 0o100755) {
        unsafe { (bs.free_pool)(cpio_ptr); }
        return None;
    }
    if !append_entry(cpio, &mut off, b"TRAILER!!!", b"", 0) {
        unsafe { (bs.free_pool)(cpio_ptr); }
        return None;
    }

    Some(PremountBundle { cpio_buf: cpio_ptr as *mut u8, cpio_size: off, cpio_alloc_size: alloc_size, iso_offset_bytes: offset_bytes })
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

fn sanitize_filename(input: &[u8]) -> ([u8; 320], usize) {
    let mut buf = [0u8; 320];
    let mut pos = 0;
    for &b in input {
        if b == 0 { break; }
        if pos >= 319 { break; }
        let safe = match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => b,
            b' ' => b'_',
            _ => b'_',
        };
        buf[pos] = safe; pos += 1;
    }
    (buf, pos)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Premount script
// ═══════════════════════════════════════════════════════════════════════════

fn build_premount_script(offset_bytes: u64, needs_sr_mod: bool) -> [u8; 4096] {
    let mut script = [0u8; 4096];

    let sr_mod_line: &[u8] = if needs_sr_mod {
        b"modprobe sr_mod 2>/dev/null\n"
    } else {
        b""
    };

    let src_template = b"\
#!/bin/sh
mkdir -p /tmp 2>/dev/null
exec >/tmp/choosable.log 2>&1
echo 'choosable: starting premount' >/dev/console
modprobe loop 2>/dev/null;modprobe iso9660 2>/dev/null
SRMOD
for i in 0 1 2 3 4 5 6 7;do [ -b /dev/loop$i ]||mknod /dev/loop$i b 7 $i 2>/dev/null;done
[ -d /cdrom ]||mkdir -p /cdrom /lib/live/mount/medium 2>/dev/null
N=0;while [ $N -lt 30 ];do N=$((N+1))
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
[ -f /cdrom/live/filesystem.squashfs ]&&return 0
[ -f /cdrom/LiveOS/squashfs.img ]&&return 0
[ -f /cdrom/.alpine-release ]&&return 0
[ -f /cdrom/.ALPINE_RELEASE ]&&return 0
[ -d /cdrom/apks ]&&return 0
[ -d /cdrom/arch ]&&{ mkdir -p /run/archiso/bootmnt 2>/dev/null;mount -o bind /cdrom /run/archiso/bootmnt 2>/dev/null;return 0;}
umount /lib/live/mount/medium 2>/dev/null
umount /cdrom 2>/dev/null
losetup -d $LOOP 2>/dev/null
done</proc/partitions
sleep 1;done
echo 'choosable: gave up - no ISO found on any partition' >/dev/console
";

    let off_str = format_decimal_u64(offset_bytes);
    let mut off_start = 0;
    while off_start < 20 && off_str[off_start] == b'0' { off_start += 1; }
    if off_start >= 20 { off_start = 19; }
    let off_slice = &off_str[off_start..];

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
            for j in 0..off_slice.len() {
                if pos < 4095 { script[pos] = off_slice[j]; pos += 1; }
            }
            i += 6;
        } else {
            if pos < 4095 { script[pos] = bytes[i]; pos += 1; }
            i += 1;
        }
    }

    script
}

// ═══════════════════════════════════════════════════════════════════════════
//  Bottom script
// ═══════════════════════════════════════════════════════════════════════════

fn build_bottom_script(iso_name: &[u8]) -> [u8; 2048] {
    let mut script = [0u8; 2048];
    let (safe_name, safe_len) = sanitize_filename(iso_name);

    let logname_buf = {
        let mut buf = [0u8; 320];
        let prefix = b"Choosable-";
        let suffix = b".log";
        let mut pos = 0;
        buf[pos..pos + prefix.len()].copy_from_slice(prefix); pos += prefix.len();
        let n = safe_len.min(250);
        buf[pos..pos + n].copy_from_slice(&safe_name[..n]); pos += n;
        buf[pos..pos + suffix.len()].copy_from_slice(suffix); pos += suffix.len();
        let mut result = [0u8; 320];
        let l = pos.min(319);
        result[..l].copy_from_slice(&buf[..l]);
        (result, l)
    };
    let logline = &logname_buf.0[..logname_buf.1];

    let src_template = b"\
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

    let mut pos = 0usize;
    let bytes = src_template;
    let mut i = 0;
    while i < bytes.len() {
        if i + 7 <= bytes.len()
            && bytes[i] == b'L' && bytes[i+1] == b'O'
            && bytes[i+2] == b'G' && bytes[i+3] == b'N'
            && bytes[i+4] == b'A' && bytes[i+5] == b'M'
            && bytes[i+6] == b'E'
        {
            for j in 0..logline.len() {
                if pos < 2047 { script[pos] = logline[j]; pos += 1; }
            }
            i += 7;
        } else {
            if pos < 2047 { script[pos] = bytes[i]; pos += 1; }
            i += 1;
        }
    }

    script
}

// ═══════════════════════════════════════════════════════════════════════════
//  CPIO helpers
// ═══════════════════════════════════════════════════════════════════════════

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
    let premount_len = premount_script.iter().position(|&c| c == 0).unwrap_or(4095);

    let bottom_script = build_bottom_script(iso_name);
    let bottom_len = bottom_script.iter().position(|&c| c == 0).unwrap_or(2047);

    let entry_size = |name_len: usize, data_len: usize| -> usize {
        let padded_name_len = ((110 + name_len + 1 + 3) & !3) - 110;
        110 + padded_name_len + data_len + 3
    };
    let cpio_estimate = entry_size(24, premount_len)
        + entry_size(33, premount_len)
        + entry_size(35, premount_len)
        + entry_size(33, bottom_len)
        + entry_size(10, 0);
    let cpio_alloc_size = (cpio_estimate + 2047) & !2047;
    let mut cpio_ptr: *mut c_void = core::ptr::null_mut();
    if unsafe { (bs.allocate_pool)(MemoryType::EfiLoaderData, cpio_alloc_size, &mut cpio_ptr) } != EFI_SUCCESS || cpio_ptr.is_null() { return None; }
    let cpio = unsafe { core::slice::from_raw_parts_mut(cpio_ptr as *mut u8, cpio_alloc_size) };
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

    if !append_entry(cpio, &mut off, b"scripts/live/00choosable", &premount_script[..premount_len], 0o100755) { unsafe { (bs.free_pool)(cpio_ptr); } return None; }
    if !append_entry(cpio, &mut off, b"scripts/live-premount/00choosable", &premount_script[..premount_len], 0o100755) { unsafe { (bs.free_pool)(cpio_ptr); } return None; }
    if !append_entry(cpio, &mut off, b"scripts/casper-premount/00choosable", &premount_script[..premount_len], 0o100755) { unsafe { (bs.free_pool)(cpio_ptr); } return None; }
    if !append_entry(cpio, &mut off, b"scripts/casper-bottom/00choosable", &bottom_script[..bottom_len], 0o100755) { unsafe { (bs.free_pool)(cpio_ptr); } return None; }
    if !append_entry(cpio, &mut off, b"TRAILER!!!", b"", 0) { unsafe { (bs.free_pool)(cpio_ptr); } return None; }

    Some(PremountBundle { cpio_buf: cpio_ptr as *mut u8, cpio_size: off, cpio_alloc_size, iso_offset_bytes: offset_bytes })
}