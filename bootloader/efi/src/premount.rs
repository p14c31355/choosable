// ═══════════════════════════════════════════════════════════════════════════
//  Premount initrd builder — eliminates iso-scan/filename= dependency
// ═══════════════════════════════════════════════════════════════════════════
//
//  When the USB is formatted with a filesystem whose kernel module is not
//  included in the ISO's initramfs (exFAT, NTFS, ReFS, ...), the kernel
//  cannot mount the USB partition to find the ISO.  Casper/dracut falls
//  through to BusyBox.
//
//  This module solves the problem at the root by:
//    1. Reading filesystem.squashfs from the ISO into a UEFI-reserved
//       memory region (survives ExitBootServices).
//    2. Building a tiny cpio "premount" initrd containing a shell script
//       that reads the squashfs from /dev/mem and loop-mounts it at /cdrom
//       before casper runs.
//    3. Adding kernel parameters (memmap=) to reserve the memory region.
//
//  The result: the kernel boots directly into the squashfs without ever
//  needing to mount the real USB filesystem.  The USB filesystem becomes
//  completely transparent — exFAT, NTFS, ext4, ReFS — all work.

use core::ffi::c_void;
use crate::protocol::{BootServices, MemoryType, EFI_SUCCESS};

/// A premount initrd bundle: the cpio archive data + kernel cmdline args.
pub struct PremountBundle {
    /// Pool-allocated cpio archive
    pub cpio_buf: *mut u8,
    /// cpio archive size in bytes
    pub cpio_size: usize,
    /// Physical address of the squashfs data (for memmap=)
    pub squashfs_addr: u64,
    /// Size of the squashfs data in bytes (for memmap=)
    pub squashfs_size: u64,
}

/// Size of the squashfs pre-allocation (most Ubuntu ISOs have a squashfs
/// under 3 GiB).  We allocate 3 GiB to be safe.  This is a UEFI
/// AllocateMaxAddress allocation so it lives below 4 GiB and the kernel
/// can access it via /dev/mem.
const SQUASHFS_MAX_SIZE: u64 = 3 * 1024 * 1024 * 1024; // 3 GiB

/// Generate a cpio "newc" archive header for a file.
/// Returns the number of bytes written to `buf`.
fn cpio_newc_header(
    buf: &mut [u8],
    name: &[u8],
    file_size: u32,
    mode: u32,
) -> usize {
    // "newc" magic: 070701
    let magic = b"070701";
    // Build the header fields (all zero-padded hex, 8 chars)
    // inode = 1, mode, uid=0, gid=0, nlink=1, mtime=0
    // filesize, devmajor=0, devminor=0, rdevmajor=0, rdevminor=0
    // namesize, check=0 (no checksum)

    let name_len = name.len() as u32 + 1; // include NUL terminator
    let padded_name_len = (name_len as usize + 3) & !3;

    // Write header as "070701" + 13 x 8-char hex fields
    let header_fields: [u32; 13] = [
        1,              // inode
        mode,           // mode
        0,              // uid
        0,              // gid
        1,              // nlink
        0,              // mtime
        file_size,      // filesize
        0,              // devmajor
        0,              // devminor
        0,              // rdevmajor (0x0301 for /dev/mem? no, just 0)
        0,              // rdevminor
        name_len,       // namesize
        0,              // check (no CRC)
    ];

    let header_buf_len = 6 + 13 * 8;
    buf[..6].copy_from_slice(magic);
    let mut pos = 6usize;
    for &v in &header_fields {
        // Format as 8-char zero-padded uppercase hex
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

    // File name (NUL-terminated, padded to 4-byte boundary)
    buf[pos..pos + name.len()].copy_from_slice(name);
    pos += name.len();
    buf[pos] = 0; // NUL terminator
    pos += 1;
    while pos < header_buf_len + padded_name_len {
        buf[pos] = 0;
        pos += 1;
    }

    header_buf_len + padded_name_len
}

fn hex_nibble(n: u8) -> u8 {
    if n < 10 { b'0' + n } else { b'A' + (n - 10) }
}

/// Build the premount shell script.
/// $SQUASHFS_ADDR and $SQUASHFS_SIZE are replaced at runtime with hex values.
fn build_premount_script(squashfs_addr: u64, squashfs_size: u64) -> [u8; 512] {
    let mut script = [0u8; 512];
    let src = b"\
#!/bin/sh
# Choosable premount - reads squashfs from /dev/mem, loop-mounts at /cdrom
echo '[choosable] premount: squashfs at 0xADDR (SIZE bytes)'
mkdir -p /cdrom
dd if=/dev/mem of=/tmp/choosable.sq bs=4K skip=SKIP count=CNT 2>/dev/null
if [ -s /tmp/choosable.sq ]; then
  mount -t squashfs -o loop,ro /tmp/choosable.sq /cdrom
  echo '[choosable] premount: squashfs mounted at /cdrom'
  ls -la /cdrom/casper/ 2>/dev/null || true
fi
";

    let mut pos = 0usize;
    let bytes = src;
    let mut i = 0;
    while i < bytes.len() {
        if i + 5 <= bytes.len() && bytes[i] == b'S' && bytes[i + 1] == b'K' && bytes[i + 2] == b'I' && bytes[i + 3] == b'P' {
            // Replace SKIP with (squashfs_addr / 4096) as decimal
            let skip_val = squashfs_addr / 4096;
            let s = format_decimal_u64(skip_val);
            for &c in &s { if pos < 511 { script[pos] = c; pos += 1; } }
            i += 4;
        } else if i + 4 <= bytes.len() && bytes[i] == b'C' && bytes[i + 1] == b'N' && bytes[i + 2] == b'T' {
            // Replace CNT with (squashfs_size / 4096) as decimal
            let cnt_val = squashfs_size / 4096;
            let s = format_decimal_u64(cnt_val);
            for &c in &s { if pos < 511 { script[pos] = c; pos += 1; } }
            i += 3;
        } else if i + 5 <= bytes.len() && bytes[i] == b'A' && bytes[i + 1] == b'D' && bytes[i + 2] == b'D' && bytes[i + 3] == b'R' {
            // Replace ADDR with hex address
            let s = format_hex_u64(squashfs_addr);
            for &c in &s { if pos < 511 { script[pos] = c; pos += 1; } }
            i += 4;
        } else if i + 4 <= bytes.len() && bytes[i] == b'S' && bytes[i + 1] == b'I' && bytes[i + 2] == b'Z' && bytes[i + 3] == b'E' {
            // Replace SIZE with decimal size
            let s = format_decimal_u64(squashfs_size);
            for &c in &s { if pos < 511 { script[pos] = c; pos += 1; } }
            i += 4;
        } else {
            if pos < 511 { script[pos] = bytes[i]; pos += 1; }
            i += 1;
        }
    }

    script
}

fn format_hex_u64(v: u64) -> [u8; 18] {
    let mut buf = [b'0'; 18];
    buf[0] = b'0';
    buf[1] = b'x';
    let mut pos = 17;
    let mut val = v;
    loop {
        buf[pos] = hex_nibble((val & 0xF) as u8);
        val >>= 4;
        if pos == 2 || val == 0 { break; }
        pos -= 1;
    }
    buf
}

fn format_decimal_u64(v: u64) -> [u8; 21] {
    let mut buf = [b'0'; 21];
    let mut val = v;
    let mut pos = 20;
    if val == 0 {
        buf[20] = b'0';
    } else {
        loop {
            buf[pos] = b'0' + (val % 10) as u8;
            val /= 10;
            if val == 0 { break; }
            pos -= 1;
        }
    }
    buf
}

/// Allocate pages below 4 GiB for the squashfs data (kernel-accessible).
fn allocate_squashfs_pages(
    bs: &mut BootServices,
    size: u64,
) -> Option<(u64, *mut u8)> {
    let pages = ((size + 4095) / 4096) as usize;
    let mut addr: u64 = 0xFFFFFFFF; // AllocateMaxAddress: allocate below 4 GiB

    // Use allocate_pages with AllocateMaxAddress type
    let status = unsafe {
        (bs.allocate_pages)(
            crate::protocol::AllocateType::AllocateMaxAddress,
            MemoryType::EfiLoaderData,
            pages,
            &mut addr,
        )
    };
    if status != EFI_SUCCESS || addr == 0 {
        return None;
    }
    Some((addr, addr as *mut u8))
}

/// Read a file extent from the ISO into a destination buffer.
/// Returns the number of bytes actually read.
fn read_iso_file(
    bs: &mut BootServices,
    bio_ptr: *mut crate::protocol::BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
    extent_lba: u32,
    extent_size: u32,
    dst: *mut u8,
    dst_size: u64,
) -> u64 {
    let bio_ref = unsafe { &*bio_ptr };
    let read_size = (extent_size as u64).min(dst_size);

    // Read directly via Block I/O — the file extent is contiguous on the
    // ISO, which is contiguous on the real disk.
    let disk_lba = iso_lba + extent_lba as u64 * 4; // 4 x 512-byte sectors per ISO sector
    let byte_len = read_size as usize;
    let status = unsafe {
        (bio_ref.read_blocks)(bio_ptr, mid, disk_lba, byte_len, dst as *mut c_void)
    };
    if status != EFI_SUCCESS {
        return 0;
    }
    read_size
}

/// Walk ISO9660 directory to find `/casper/filesystem.squashfs` and read it
/// into UEFI-reserved memory pages.  Returns a PremountBundle with the cpio
/// premount initrd ready to be injected.
pub fn prepare_premount_initrd(
    bs: &mut BootServices,
    bio_ptr: *mut crate::protocol::BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
    iso_size_bytes: u64,
) -> Option<PremountBundle> {
    // Step 1: Read ISO PVD to get root directory
    let disk_lba_pvd = iso_lba + 16 * 4; // PVD is at ISO sector 16
    let mut pvd = [0u8; 2048];
    {
        let bio_ref = unsafe { &*bio_ptr };
        let status = unsafe {
            (bio_ref.read_blocks)(bio_ptr, mid, disk_lba_pvd, 2048, pvd.as_mut_ptr() as *mut c_void)
        };
        if status != EFI_SUCCESS || pvd[0] != 1 || &pvd[1..6] != b"CD001" {
            return None;
        }
    }
    let root_extent = u32::from_le_bytes(pvd[158..162].try_into().unwrap());
    let root_size = u32::from_le_bytes(pvd[166..170].try_into().unwrap());

    // Step 2: Walk /CASPER/FILESYSTEM.SQUASHFS (casper directory varies — try casper and live)
    let targets: [&[u8]; 2] = [b"CASPER", b"LIVE"];
    for &dir_name in &targets {
        // Find directory entry
        let dir_info = find_iso_dir_entry(bs, bio_ptr, mid, iso_lba, root_extent, root_size, dir_name);
        let (dir_lba, dir_size) = match dir_info {
            Some(d) => d,
            None => continue,
        };

        // Find filesystem.squashfs inside
        let file_info = find_iso_dir_entry(bs, bio_ptr, mid, iso_lba, dir_lba, dir_size, b"FILESYSTEM.SQUASHFS");
        let (file_lba, file_size) = match file_info {
            Some(f) => f,
            None => continue,
        };

        // Step 3: Allocate reserved pages for the squashfs
        let (phys_addr, dst) = match allocate_squashfs_pages(bs, file_size as u64) {
            Some(a) => a,
            None => return None,
        };

        // Step 4: Read squashfs into reserved pages
        let read = read_iso_file(bs, bio_ptr, mid, iso_lba, file_lba, file_size, dst, file_size as u64);
        if read < file_size as u64 {
            return None;
        }

        // Step 5: Build premount cpio archive
        let script = build_premount_script(phys_addr, file_size as u64);

        // cpio header for the script + script data + cpio trailer + dir entry for /scripts/casper-premount/
        // Layout:
        // 1. Dir entry: "scripts" (mode 040755)
        // 2. Dir entry: "scripts/casper-premount" (mode 040755)
        // 3. File entry: "scripts/casper-premount/choosable" (mode 0100755)
        // 4. Trailer

        // Estimate cpio size: each header ~110 bytes + name padding, file data padded to 4.
        // 3 dir/file entries + trailer ≈ 500 bytes + script (512 bytes) ≈ 1024 bytes
        let cpio_estimate = 2048usize;
        let mut cpio_ptr: *mut c_void = core::ptr::null_mut();
        let status = unsafe {
            (bs.allocate_pool)(MemoryType::EfiLoaderData, cpio_estimate, &mut cpio_ptr)
        };
        if status != EFI_SUCCESS || cpio_ptr.is_null() {
            return None;
        }
        let cpio = unsafe { core::slice::from_raw_parts_mut(cpio_ptr as *mut u8, cpio_estimate) };
        let mut off = 0usize;

        // Helper to append an entry
        let mut append_entry = |cpio: &mut [u8], off: &mut usize, name: &[u8], data: &[u8], mode: u32| {
            let hdr_len = cpio_newc_header(&mut cpio[*off..], name, data.len() as u32, mode);
            *off += hdr_len;
            let data_start = *off;
            cpio[data_start..data_start + data.len()].copy_from_slice(data);
            *off += data.len();
            // Pad to 4-byte boundary
            let pad = (4 - (*off & 3)) & 3;
            for _ in 0..pad { cpio[*off] = 0; *off += 1; }
        };

        // Directory: scripts/
        append_entry(cpio, &mut off, b"scripts", b"", 0o40755);
        // Directory: scripts/casper-premount/
        append_entry(cpio, &mut off, b"scripts/casper-premount", b"", 0o40755);
        // File: scripts/casper-premount/choosable
        let script_slice = &script[..script.iter().position(|&c| c == 0).unwrap_or(511)];
        append_entry(cpio, &mut off, b"scripts/casper-premount/choosable", script_slice, 0o100755);

        // Trailer: "TRAILER!!!"
        append_entry(cpio, &mut off, b"TRAILER!!!", b"", 0);

        let final_size = off;

        return Some(PremountBundle {
            cpio_buf: cpio_ptr as *mut u8,
            cpio_size: final_size,
            squashfs_addr: phys_addr,
            squashfs_size: file_size as u64,
        });
    }

    None
}

/// Search an ISO9660 directory for a child entry by name.
/// Returns (extent_lba, extent_size) or None.
fn find_iso_dir_entry(
    bs: &mut BootServices,
    bio_ptr: *mut crate::protocol::BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
    dir_lba: u32,
    dir_size: u32,
    name: &[u8],
) -> Option<(u32, u32)> {
    let bio_ref = unsafe { &*bio_ptr };
    let total_sectors = ((dir_size as u64 + 2047) / 2048) as u32;
    let mut scratch = [0u8; 2048];

    for s in 0..total_sectors {
        let disk_lba = iso_lba + (dir_lba + s) as u64 * 4;
        let status = unsafe {
            (bio_ref.read_blocks)(bio_ptr, mid, disk_lba, 2048, scratch.as_mut_ptr() as *mut c_void)
        };
        if status != EFI_SUCCESS { return None; }

        let mut offset: usize = 0;
        while offset + 34 <= 2048 {
            let record_len = scratch[offset] as usize;
            if record_len == 0 { break; }
            if record_len < 34 || offset + record_len > 2048 { break; }
            let name_len = scratch[offset + 32] as usize;
            let name_offset = offset + 33;
            if name_offset + name_len > 2048 { break; }

            let eff_len = if name_len >= 2 && scratch[name_offset + name_len - 2] == b';' {
                name_len - 2
            } else {
                name_len
            };

            if eff_len == name.len() {
                let mut matched = true;
                for i in 0..name.len() {
                    if scratch[name_offset + i].to_ascii_uppercase() != name[i].to_ascii_uppercase() {
                        matched = false;
                        break;
                    }
                }
                if matched {
                    let extent = u32::from_le_bytes(scratch[offset + 2..offset + 6].try_into().unwrap());
                    let size = u32::from_le_bytes(scratch[offset + 10..offset + 14].try_into().unwrap());
                    return Some((extent, size));
                }
            }
            offset += record_len;
        }
    }
    None
}