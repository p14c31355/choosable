// ═══════════════════════════════════════════════════════════════════════════
//  ISO9660 directory parser + UEFI chainloader
// ═══════════════════════════════════════════════════════════════════════════
//
//  On pure UEFI systems (no CSM), the BIOS El Torito boot image cannot work
//  (no INT 13h, no legacy CD emulation).  The correct path is:
//    1. Parse ISO9660 Primary Volume Descriptor → get root directory
//    2. Walk /EFI/BOOT/ → find BOOTX64.EFI
//    3. Read it into a contiguous buffer
//    4. Call LoadImage() + StartImage()

use core::ffi::c_void;

use crate::disk::read_sector;
use crate::fs::{IsoEntry, FsCtx};
use crate::output::{format_u64_buf, halt_or_reboot, print_raw};
use crate::protocol::{
    BlockIoProtocol, BootServices, LoadedImageProtocol, MemoryType, SystemTable,
    DevicePathProtocol, VirtualBlockIo, EFI_SUCCESS, LOADED_IMAGE_PROTOCOL_GUID,
};

use crate::strategy;

// ═══════════════════════════════════════════════════════════════════════════
//  ISO9660 on-disk structures and helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Read one 2048-byte ISO logical sector (4 × 512-byte disk sectors)
fn read_iso_sector(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
    sector: u32,
    buf: &mut [u8; 2048],
) -> bool {
    let disk_lba = iso_lba + sector as u64 * 4;
    for i in 0..4usize {
        let mut sec = [0u8; 512];
        if !read_sector(bio_ref, bio_ptr, mid, disk_lba + i as u64, &mut sec) {
            return false;
        }
        buf[i * 512..(i + 1) * 512].copy_from_slice(&sec);
    }
    true
}

/// Read any size extent (LBA + byte length) into a heap buffer.
/// Caller must call `bs.free_pool` to release the allocation.
fn read_extent(
    bs: &mut BootServices,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
    lba: u32,
    byte_len: u32,
) -> Option<(*mut u8, u32)> {
    let sector_count = ((byte_len as u64 + 2047) / 2048) as u32;
    let buf_len = sector_count as usize * 2048;

    let mut ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.allocate_pool)(MemoryType::EfiLoaderData, buf_len, &mut ptr)
    };
    if status != EFI_SUCCESS || ptr.is_null() {
        return None;
    }
    let ptr_u8: *mut u8 = ptr as *mut u8;

    // Read the entire extent directly into the pool buffer
    let disk_lba = iso_lba + lba as u64 * 4;
    let read_status = unsafe {
        (bio_ref.read_blocks)(bio_ptr, mid, disk_lba, buf_len, ptr)
    };
    if read_status != EFI_SUCCESS {
        unsafe { (bs.free_pool)(ptr); }
        return None;
    }
    Some((ptr_u8, byte_len.min(buf_len as u32)))
}

// ═══════════════════════════════════════════════════════════════════════════
//  ISO9660 directory walker
// ═══════════════════════════════════════════════════════════════════════════

/// Get root directory record from the Primary Volume Descriptor (sector 16).
/// Returns (extent_lba, extent_size) in ISO sectors/bytes.
fn get_root_dir(
    st: &mut SystemTable,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
) -> Option<(u32, u32)> {
    let mut pvd = [0u8; 2048];
    if !read_iso_sector(bio_ref, bio_ptr, mid, iso_lba, 16, &mut pvd) {
        print_raw(st, b"Failed to read ISO PVD.\r\n\0");
        return None;
    }
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        print_raw(st, b"Invalid ISO PVD signature.\r\n\0");
        return None;
    }
    // Root directory record at offset 156 in PVD.
    // Fields (offsets from record start):
    //   extent LE @ +2 (4 bytes)
    //   size LE   @ +10 (4 bytes)
    let root_extent = u32::from_le_bytes(pvd[158..162].try_into().unwrap());
    let root_size = u32::from_le_bytes(pvd[166..170].try_into().unwrap());
    Some((root_extent, root_size))
}

/// Search an ISO9660 directory extent for a child by name (case-insensitive).
/// Returns (child_extent_lba, child_size_in_bytes) or None.
fn find_in_dir(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
    dir_lba: u32,
    dir_size: u32,
    name: &[u8],
    scratch: &mut [u8; 2048],
) -> Option<(u32, u32)> {
    find_in_dir_with_loc(bio_ref, bio_ptr, mid, iso_lba, dir_lba, dir_size, name, scratch)
        .map(|(extent, size, _sector, _offset)| (extent, size))
}

/// Search an ISO9660 directory extent for a child by name, returning the
/// file extent AND the location of its directory entry (sector + byte offset).
/// Returns (extent_lba, extent_size, dir_sector, byte_offset_within_sector).
fn find_in_dir_with_loc(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
    dir_lba: u32,
    dir_size: u32,
    name: &[u8],
    scratch: &mut [u8; 2048],
) -> Option<(u32, u32, u32, u32)> {
    let total_sectors = ((dir_size as u64 + 2047) / 2048) as u32;
    for s in 0..total_sectors {
        if !read_iso_sector(bio_ref, bio_ptr, mid, iso_lba, dir_lba + s, scratch) {
            return None;
        }
        let mut offset: usize = 0;
        while offset + 34 <= 2048 && offset < (dir_size as usize).saturating_sub(s as usize * 2048) {
            let record_len = scratch[offset] as usize;
            if record_len == 0 { break; }
            if offset + record_len > 2048 { break; }
            let name_len = scratch[offset + 32] as usize;
            let name_offset = offset + 33;
            if name_offset + name_len > 2048 { break; }
            let effective_len = if name_len >= 2 && scratch[name_offset + name_len - 2] == b';' {
                name_len - 2
            } else {
                name_len
            };
            if effective_len == name.len() {
                let mut matched = true;
                for i in 0..name.len() {
                    let a = scratch[name_offset + i].to_ascii_uppercase();
                    let b = name[i].to_ascii_uppercase();
                    if a != b { matched = false; break; }
                }
                if matched {
                    let child_extent = u32::from_le_bytes(scratch[offset + 2..offset + 6].try_into().unwrap());
                    let child_size = u32::from_le_bytes(scratch[offset + 10..offset + 14].try_into().unwrap());
                    let dir_sector = dir_lba + s;
                    let byte_offset = offset as u32;
                    return Some((child_extent, child_size, dir_sector, byte_offset));
                }
            }
            offset += record_len;
        }
    }
    None
}

/// Search an ISO9660 directory extent for a child by name (case-insensitive).
/// Returns (child_extent_lba, child_size_in_bytes) or None.
fn _find_in_dir(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
    dir_lba: u32,
    dir_size: u32,
    name: &[u8],
    scratch: &mut [u8; 2048],
) -> Option<(u32, u32)> {
    let total_sectors = ((dir_size as u64 + 2047) / 2048) as u32;
    for s in 0..total_sectors {
        if !read_iso_sector(bio_ref, bio_ptr, mid, iso_lba, dir_lba + s, scratch) {
            return None;
        }
        let mut offset: usize = 0;
        // ISO9660 directory record layout (minimum 34 bytes):
        //   +0  len (1)
        //   +2  extent LE (4)
        //   +10 size LE (4)
        //   +32 name_len (1)
        //   +33 name (variable)
        while offset + 34 <= 2048 && offset < dir_size as usize {
            let record_len = scratch[offset] as usize;
            if record_len == 0 {
                break;
            }
            if offset + record_len > 2048 {
                break;
            }
            let name_len = scratch[offset + 32] as usize;
            let name_offset = offset + 33;
            if name_offset + name_len > 2048 {
                break;
            }
            // ISO9660 names may have ";1" version suffix — strip it
            let effective_len = if name_len >= 2 && scratch[name_offset + name_len - 2] == b';' {
                name_len - 2
            } else {
                name_len
            };
            if effective_len == name.len() {
                let mut matched = true;
                for i in 0..name.len() {
                    let a = scratch[name_offset + i].to_ascii_uppercase();
                    let b = name[i].to_ascii_uppercase();
                    if a != b {
                        matched = false;
                        break;
                    }
                }
                if matched {
                    let child_extent = u32::from_le_bytes(scratch[offset + 2..offset + 6].try_into().unwrap());
                    let child_size = u32::from_le_bytes(scratch[offset + 10..offset + 14].try_into().unwrap());
                    return Some((child_extent, child_size));
                }
            }
            offset += record_len;
        }
    }
    None
}

/// Resolve path "/EFI/BOOT/BOOTX64.EFI" within the ISO directory tree.
fn find_efi_boot(
    st: &mut SystemTable,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
) -> Option<(u32, u32)> {
    let (root_lba, root_size) = get_root_dir(st, bio_ref, bio_ptr, mid, iso_lba)?;
    let mut scratch = [0u8; 2048];

    // 1. Find /EFI
    let (efi_lba, efi_size) = find_in_dir(
        bio_ref, bio_ptr, mid, iso_lba,
        root_lba, root_size, b"EFI", &mut scratch,
    )?;

    // 2. Find /EFI/BOOT
    let (boot_lba, boot_size) = find_in_dir(
        bio_ref, bio_ptr, mid, iso_lba,
        efi_lba, efi_size, b"BOOT", &mut scratch,
    )?;

    // 3. Find /EFI/BOOT/BOOTX64.EFI
    find_in_dir(
        bio_ref, bio_ptr, mid, iso_lba,
        boot_lba, boot_size, b"BOOTX64.EFI", &mut scratch,
    )
}

/// .CFG candidate entry with path metadata.
#[derive(Copy, Clone)]
struct CfgEntry {
    ext_lba: u32, ext_size: u32, dir_sector: u32, dir_offset: u32,
    path: [u8; 64], path_len: usize,
}

/// Add a .CFG candidate entry to the list (deduplicated by extent LBA).
fn add_cfg_entry(
    entries: &mut [CfgEntry; 8],
    entry_count: &mut usize,
    ext_lba: u32, ext_size: u32, dir_sector: u32, dir_offset: u32,
    path: &[u8],
) {
    if *entry_count >= 8 { return; }
    let mut dup = false;
    for j in 0..*entry_count {
        if entries[j].ext_lba == ext_lba { dup = true; break; }
    }
    if dup { return; }
    let plen = path.len().min(63);
    let mut buf = [0u8; 64];
    buf[..plen].copy_from_slice(&path[..plen]);
    entries[*entry_count] = CfgEntry {
        ext_lba, ext_size, dir_sector, dir_offset,
        path: buf, path_len: plen,
    };
    *entry_count += 1;
}

/// Try to patch a single candidate file. Returns true if successfully patched.
fn try_patch_candidate(
    st: &mut SystemTable,
    bs: &mut BootServices,
    vb: &mut VirtualBlockIo,
    sfs_instance: *mut crate::iso_fs::IsoFsInstance,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
    iso_name: &[u8],
    ext_lba: u32, ext_size: u32, dir_sector: u32, dir_offset: u32,
) -> bool {
    let (orig_ptr, orig_len) = match read_extent(bs, bio_ref, bio_ptr, mid, iso_lba, ext_lba, ext_size) {
        Some(v) => v,
        None => return false,
    };
    let orig = unsafe { core::slice::from_raw_parts(orig_ptr, orig_len as usize) };

    // Check for linux/linuxefi line
    let has_linux = (orig.len() >= 6 && orig.windows(6).any(|w| w == b"linux "))
        || (orig.len() >= 8 && orig.windows(8).any(|w| w == b"linuxefi"));

    if !has_linux {
        unsafe { (bs.free_pool)(orig_ptr as *mut c_void); }
        return false;
    }

    // Build IsoFsCtx
    let mut iso_name_arr = [0u8; 128];
    let nlen = iso_name.len().min(127);
    iso_name_arr[..nlen].copy_from_slice(&iso_name[..nlen]);
    let ctx = crate::iso_fs::IsoFsCtx {
        real_bio_ptr: bio_ptr,
        real_media_id: mid,
        iso_lba,
        iso_size_bytes: vb.media.bim_lb * 2048,
        root_lba: 0, root_size: 0,
        bs: bs as *mut BootServices,
        st: core::ptr::null_mut(),
        iso_name: iso_name_arr, iso_name_len: nlen,
        old_extent: 0, old_size: 0,
        new_extent: 0, new_size: 0,
        redirect_active: false,
    };

    let patch = strategy::patch_grub_cfg(&ctx, orig, bs as *mut BootServices);
    unsafe { (bs.free_pool)(orig_ptr as *mut c_void); }

    let (patched_buf, patched_size) = match patch {
        Some(p) => (p.buf, p.size),
        None => return false,
    };

    let sector_aligned_patch = ((patched_size + 2047) / 2048) * 2048;
    let mut patch_block_ptr: *mut c_void = core::ptr::null_mut();
    let s = unsafe { (bs.allocate_pool)(MemoryType::EfiLoaderData, sector_aligned_patch, &mut patch_block_ptr) };
    if s != EFI_SUCCESS || patch_block_ptr.is_null() {
        unsafe { (bs.free_pool)(patched_buf as *mut c_void); }
        return false;
    }
    let patch_dst = unsafe { core::slice::from_raw_parts_mut(patch_block_ptr as *mut u8, sector_aligned_patch) };
    patch_dst[..patched_size].copy_from_slice(unsafe { core::slice::from_raw_parts(patched_buf, patched_size) });
    for j in patched_size..sector_aligned_patch { patch_dst[j] = 0; }
    unsafe { (bs.free_pool)(patched_buf as *mut c_void); }

    let orig_end_sector = vb.media.bim_lb + 1;
    vb.patched_file_sector = orig_end_sector as u32;
    vb.patched_file_sectors = (sector_aligned_patch / 2048) as u32;
    vb.patched_file_buf = patch_block_ptr as *mut u8;
    vb.dir_entry_sector = dir_sector;
    vb.dir_entry_offset = dir_offset;
    vb.dir_entry_new_extent = vb.patched_file_sector;
    vb.dir_entry_new_size = patched_size as u32;
    vb.dir_entry_patched = true;
    vb.media.bim_lb = orig_end_sector + vb.patched_file_sectors as u64 - 1;

    // Also set SFS redirect so SimpleFileSystem sees the patched file
    if !sfs_instance.is_null() {
        let sfs = unsafe { &mut *sfs_instance };
        sfs.ctx.old_extent = ext_lba;
        sfs.ctx.old_size = ext_size;
        sfs.ctx.new_extent = vb.patched_file_sector;
        sfs.ctx.new_size = patched_size as u32;
        sfs.ctx.redirect_active = true;
    }

    print_raw(st, b"[grub.cfg] PATCHED OK: orig=\0");
    let mut nbuf = [0u8; 16];
    let mut nv = orig_len as u64; let mut np = 15;
    loop { nbuf[np] = b'0' + (nv % 10) as u8; nv /= 10; if nv == 0 || np == 0 { break; } np -= 1; }
    print_raw(st, &nbuf[np..]);
    print_raw(st, b" -> new=\0");
    let mut nv2 = patched_size as u64; let mut np2 = 15;
    loop { nbuf[np2] = b'0' + (nv2 % 10) as u8; nv2 /= 10; if nv2 == 0 || np2 == 0 { break; } np2 -= 1; }
    print_raw(st, &nbuf[np2..]);
    print_raw(st, b", dir_sector=\0");
    let mut nv4 = dir_sector as u64; let mut np4 = 15;
    loop { nbuf[np4] = b'0' + (nv4 % 10) as u8; nv4 /= 10; if nv4 == 0 || np4 == 0 { break; } np4 -= 1; }
    print_raw(st, &nbuf[np4..]);
    print_raw(st, b"\r\n\0");
    true
}

/// Find, patch grub.cfg, and install it via directory entry redirect.
/// Collects known-path .CFG files first (fast), prints them, then tries to patch.
/// Falls back to recursive scan only as last resort.
fn patch_grub_cfg_blockio(
    st: &mut SystemTable,
    bs: &mut BootServices,
    vbio: *mut VirtualBlockIo,
    sfs_instance: *mut crate::iso_fs::IsoFsInstance,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
    iso_name: &[u8],
) {
    if vbio.is_null() { return; }
    let vb = unsafe { &mut *vbio };

    let mut pvd = [0u8; 2048];
    if !read_iso_sector(bio_ref, bio_ptr, mid, iso_lba, 16, &mut pvd) { return; }
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" { return; }
    let root_lba = u32::from_le_bytes(pvd[158..162].try_into().unwrap());
    let root_size = u32::from_le_bytes(pvd[166..170].try_into().unwrap());
    let mut scratch = [0u8; 2048];

    // Collect entries
    let mut entries: [CfgEntry; 8] = unsafe { core::mem::zeroed() };
    let mut entry_count = 0usize;

    // Phase 1: Collect from known paths (fast, no recursive scan)
    let known_paths: [(&[u8], &[u8], &[u8], &[u8]); 4] = [
        (b"BOOT", b"GRUB", b"GRUB.CFG",     b"/boot/grub/grub.cfg"),
        (b"BOOT", b"GRUB", b"LOOPBACK.CFG", b"/boot/grub/loopback.cfg"),
        (b"EFI", b"BOOT", b"GRUB.CFG",      b"/EFI/BOOT/grub.cfg"),
        (b"",     b"",    b"GRUB.CFG",      b"/grub.cfg"),
    ];

    for (dir1, dir2, filename, path) in &known_paths {
        let entry = if dir1.is_empty() {
            find_in_dir_with_loc(bio_ref, bio_ptr, mid, iso_lba, root_lba, root_size, filename, &mut scratch)
        } else {
            let d1 = find_in_dir(bio_ref, bio_ptr, mid, iso_lba, root_lba, root_size, dir1, &mut scratch);
            if let Some(d1_entry) = d1 {
                let d2 = find_in_dir(bio_ref, bio_ptr, mid, iso_lba, d1_entry.0, d1_entry.1, dir2, &mut scratch);
                if let Some(d2_entry) = d2 {
                    find_in_dir_with_loc(bio_ref, bio_ptr, mid, iso_lba, d2_entry.0, d2_entry.1, filename, &mut scratch)
                } else { None }
            } else { None }
        };
        if let Some((ext_lba, ext_size, dir_sector, dir_offset)) = entry {
            add_cfg_entry(&mut entries, &mut entry_count, ext_lba, ext_size, dir_sector, dir_offset, path);
        }
    }

    // Phase 2: If nothing found, recursive scan
    if entry_count == 0 {
        let mut raw_entries: [(u32, u32, u32, u32); 32] = [(0,0,0,0); 32];
        let mut raw_count = 0usize;
        recursive_find_cfg_with_loc(
            bio_ref, bio_ptr, mid, iso_lba, root_lba, root_size,
            &mut scratch, &mut raw_entries, &mut raw_count,
        );
        for i in 0..raw_count {
            let (ext_lba, ext_size, dir_sector, dir_offset) = raw_entries[i];
            if ext_size == 0 { continue; }
            // Generic path for recursively found entries
            add_cfg_entry(&mut entries, &mut entry_count, ext_lba, ext_size, dir_sector, dir_offset, b"/<recursive>.cfg");
        }
    }

    if entry_count == 0 {
        print_raw(st, b"[grub.cfg] No .CFG files found in ISO.\r\n\0");
        return;
    }

    // Print all found entries
    print_raw(st, b"[grub.cfg] Found \0");
    let mut nbuf = [0u8; 16];
    let mut nv = entry_count as u64; let mut np = 15;
    loop { nbuf[np] = b'0' + (nv % 10) as u8; nv /= 10; if nv == 0 || np == 0 { break; } np -= 1; }
    print_raw(st, &nbuf[np..]);
    print_raw(st, b" .CFG entries:\r\n\0");
    for i in 0..entry_count {
        print_raw(st, b"  #\0");
        let mut nv2 = i as u64; let mut np2 = 15;
        loop { nbuf[np2] = b'0' + (nv2 % 10) as u8; nv2 /= 10; if nv2 == 0 || np2 == 0 { break; } np2 -= 1; }
        print_raw(st, &nbuf[np2..]);
        print_raw(st, b" \0");
        print_raw(st, &entries[i].path[..entries[i].path_len]);
        print_raw(st, b"\r\n\0");
    }

    // Try each entry
    for i in 0..entry_count {
        let (ext_lba, ext_size, dir_sector, dir_offset, path, path_len) =
            (entries[i].ext_lba, entries[i].ext_size,
             entries[i].dir_sector, entries[i].dir_offset,
             &entries[i].path, entries[i].path_len);

        print_raw(st, b"[grub.cfg] Trying #\0");
        let mut nv2 = i as u64; let mut np2 = 15;
        loop { nbuf[np2] = b'0' + (nv2 % 10) as u8; nv2 /= 10; if nv2 == 0 || np2 == 0 { break; } np2 -= 1; }
        print_raw(st, &nbuf[np2..]);
        print_raw(st, b" \0");
        print_raw(st, &path[..path_len]);
        print_raw(st, b"...\r\n\0");

        if try_patch_candidate(st, bs, vb, sfs_instance, bio_ref, bio_ptr, mid, iso_lba, iso_name,
            ext_lba, ext_size, dir_sector, dir_offset) {
            return;
        }
    }

    print_raw(st, b"[grub.cfg] No patchable .CFG found (none have 'linux' line).\r\n\0");
}

/// Recursively walk ISO9660 directories collecting ALL .CFG file entries
/// with their full location info (extent_lba, extent_size, dir_sector, dir_offset).
fn recursive_find_cfg_with_loc(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
    dir_lba: u32,
    dir_size: u32,
    scratch: &mut [u8; 2048],
    entries: &mut [(u32, u32, u32, u32); 32],
    entry_count: &mut usize,
) {
    if *entry_count >= 32 { return; }
    let total_sectors = ((dir_size as u64 + 2047) / 2048) as u32;
    for s in 0..total_sectors {
        if !read_iso_sector(bio_ref, bio_ptr, mid, iso_lba, dir_lba + s, scratch) { return; }
        let mut offset: usize = 0;
        while offset + 34 <= 2048 && offset < (dir_size as usize).saturating_sub(s as usize * 2048) {
            let record_len = scratch[offset] as usize;
            if record_len == 0 { break; }
            if offset + record_len > 2048 { break; }
            let name_len = scratch[offset + 32] as usize;
            let name_offset = offset + 33;
            if name_offset + name_len > 2048 { break; }
            let flags = scratch[offset + 25];
            let is_dir = flags & 0x02 != 0;
            let extent = u32::from_le_bytes(scratch[offset + 2..offset + 6].try_into().unwrap());
            let size = u32::from_le_bytes(scratch[offset + 10..offset + 14].try_into().unwrap());

            let skip = if name_len == 1 && scratch[name_offset] == 0 { true }
                else if name_len == 1 && scratch[name_offset] == 1 { true }
                else { false };

            if !skip {
                let eff_len = if name_len >= 2 && scratch[name_offset + name_len - 2] == b';' {
                    name_len - 2
                } else {
                    name_len
                };
                let has_cfg = eff_len >= 4 && {
                    let ofs = name_offset + eff_len - 4;
                    (scratch[ofs] | 0x20) == b'.' && (scratch[ofs+1] | 0x20) == b'c'
                        && (scratch[ofs+2] | 0x20) == b'f' && (scratch[ofs+3] | 0x20) == b'g'
                };
                if has_cfg && !is_dir && *entry_count < 32 {
                    let dir_sector = dir_lba + s;
                    let dir_offset = offset as u32;
                    // Check duplicates
                    let mut dup = false;
                    for i in 0..*entry_count {
                        if entries[i].0 == extent { dup = true; break; }
                    }
                    if !dup {
                        entries[*entry_count] = (extent, size, dir_sector, dir_offset);
                        *entry_count += 1;
                    }
                }
                if is_dir && extent != dir_lba && *entry_count < 32 {
                    recursive_find_cfg_with_loc(
                        bio_ref, bio_ptr, mid, iso_lba,
                        extent, size, scratch,
                        entries, entry_count,
                    );
                }
            }
            offset += record_len;
        }
    }
}

/// Recursively walk ISO9660 directories to find any file ending in `.CFG`.
fn recursive_find_cfg(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    iso_lba: u64,
    dir_lba: u32,
    dir_size: u32,
    scratch: &mut [u8; 2048],
    candidates: &mut [Option<(u32, u32)>; 32],
    cand_count: &mut usize,
) {
    let total_sectors = ((dir_size as u64 + 2047) / 2048) as u32;
    for s in 0..total_sectors {
        if !read_iso_sector(bio_ref, bio_ptr, mid, iso_lba, dir_lba + s, scratch) {
            return;
        }
        let mut offset: usize = 0;
        while offset + 34 <= 2048 && offset < (dir_size as usize).saturating_sub(s as usize * 2048) {
            let record_len = scratch[offset] as usize;
            if record_len == 0 {
                break;
            }
            if offset + record_len > 2048 {
                break;
            }
            let name_len = scratch[offset + 32] as usize;
            let name_offset = offset + 33;
            if name_offset + name_len > 2048 {
                break;
            }
            let flags = scratch[offset + 25];
            let is_dir = flags & 0x02 != 0;
            let extent = u32::from_le_bytes(scratch[offset + 2..offset + 6].try_into().unwrap());
            let size = u32::from_le_bytes(scratch[offset + 10..offset + 14].try_into().unwrap());

            // Skip "." and ".."
            let skip = if name_len == 1 && scratch[name_offset] == 0 { true }
                else if name_len == 1 && scratch[name_offset] == 1 { true }
                else { false };

            if !skip {
                // Check if name ends with ".CFG" (case-insensitive, strip ";1")
                let eff_len = if name_len >= 2 && scratch[name_offset + name_len - 2] == b';' {
                    name_len - 2
                } else {
                    name_len
                };
                let has_cfg = eff_len >= 4 && {
                    let ofs = name_offset + eff_len - 4;
                    (scratch[ofs] | 0x20) == b'.' && (scratch[ofs+1] | 0x20) == b'c'
                        && (scratch[ofs+2] | 0x20) == b'f' && (scratch[ofs+3] | 0x20) == b'g'
                };
                if has_cfg && !is_dir && *cand_count < 32 {
                    // Check for duplicates
                    let mut dup = false;
                    for j in 0..*cand_count {
                        if let Some((l, _)) = candidates[j] { if l == extent { dup = true; break; } }
                    }
                    if !dup {
                        candidates[*cand_count] = Some((extent, size));
                        *cand_count += 1;
                    }
                }

                // Recurse into subdirectories
                if is_dir && extent != dir_lba {
                    recursive_find_cfg(bio_ref, bio_ptr, mid, iso_lba, extent, size, scratch, candidates, cand_count);
                }
            }
            offset += record_len;
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  UEFI chainload
// ═══════════════════════════════════════════════════════════════════════════

/// Build a DevicePath for the ISO's EFI executable:
///   CDROM(0) / File("\\EFI\\BOOT\\BOOTX64.EFI")
///
/// Uses a CD-ROM Media Device Path node so that UEFI (and the chainloaded
/// shim/GRUB) recognize this as a CD-ROM device and route file access
/// through the SimpleFileSystem protocol installed on the virtual CD-ROM handle.
/// Returns a pool-allocated pointer, or null on failure.
fn build_iso_device_path(
    bs: &mut BootServices,
    iso_size_bytes: u64,
) -> *mut c_void {
    // ── CD-ROM Media Device Path node (24 bytes) ─────────
    const CDROM_NODE: [u8; 24] = {
        let mut n = [0u8; 24];
        n[0] = 0x04; // Type: MEDIA_DEVICE_PATH
        n[1] = 0x02; // SubType: CDROM
        n[2] = 24u8; // Length = 24 (little-endian)
        n[3] = 0x00;
        // BootEntry (4 bytes at offset 4) = 0
        // PartitionStart (8 bytes at offset 8) = 0 (start of virtual CD)
        // PartitionSize (8 bytes at offset 16) = ISO size in 2048-byte sectors
        n
    };

    // ── File path node (UCS-2 string) ────────────────────
    let file_name: [u16; 22] = [
        b'\\' as u16, b'E' as u16, b'F' as u16, b'I' as u16,
        b'\\' as u16, b'B' as u16, b'O' as u16, b'O' as u16, b'T' as u16,
        b'\\' as u16,
        b'B' as u16, b'O' as u16, b'O' as u16, b'T' as u16, b'X' as u16,
        b'6' as u16, b'4' as u16, b'.' as u16, b'E' as u16, b'F' as u16, b'I' as u16,
        0x0000u16, // null terminator
    ];

    // ── End device path node (4 bytes) ────────────────────
    const END_NODE: [u8; 4] = [0x7F, 0xFF, 0x04, 0x00];

    // CD-ROM node (24) + FilePath header (4) + UCS-2 string (44 bytes) + End (4) = 76
    let file_body_bytes = file_name.len() * 2;
    let total = CDROM_NODE.len() + 4 + file_body_bytes + END_NODE.len();

    let mut ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.allocate_pool)(MemoryType::EfiLoaderData, total, &mut ptr)
    };
    if status != EFI_SUCCESS || ptr.is_null() {
        return core::ptr::null_mut();
    }
    let dp = ptr as *mut u8;

    unsafe {
        // CD-ROM node
        let mut off = 0usize;
        dp.copy_from_nonoverlapping(CDROM_NODE.as_ptr(), CDROM_NODE.len());
        off += CDROM_NODE.len();
        // Patch PartitionStart = 0 (virtual CD starts at LBA 0)
        // Patch PartitionSize = ISO size in 2048-byte sectors
        // (offsets 8 and 16 within the CD-ROM node)
        *(dp.add(8) as *mut u64) = 0u64.to_le();
        *(dp.add(16) as *mut u64) = (iso_size_bytes / 2048).to_le();

        // File path node header
        dp.add(off).write_volatile(0x04u8); // Type: MEDIA_DEVICE_PATH
        off += 1;
        dp.add(off).write_volatile(0x04u8); // SubType: FILE_PATH
        off += 1;
        let file_node_len = (4 + file_body_bytes) as u16;
        *(dp.add(off) as *mut u16) = file_node_len.to_le();
        off += 2;
        // File path body
        core::ptr::copy_nonoverlapping(
            file_name.as_ptr() as *const u8,
            dp.add(off),
            file_body_bytes,
        );
        off += file_body_bytes;

        // End node
        dp.add(off).copy_from_nonoverlapping(END_NODE.as_ptr(), END_NODE.len());
    }

    ptr
}

/// Load and start an EFI executable from within an ISO.
fn uefi_chainload_iso(
    st: &mut SystemTable,
    image_handle: *mut c_void,
    part1_lba: u64,
    files: &[IsoEntry; 64],
    idx: usize,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
) {
    let iso_lba = files[idx].file_start_lba;
    let iso_size = files[idx].file_size;

    let bs = unsafe { &mut *st.boot_services };

    // Disable watchdog timer to prevent firmware reset during chainload
    unsafe {
        (bs.set_watchdog_timer)(0, 0x10000, 0, core::ptr::null());
    }

    // ── ISO file name (for grub.cfg iso-scan/filename injection)
    let iso_name = &files[idx].name[..files[idx].name_len.min(files[idx].name.len())];

    // ── Create virtual CD-ROM from the ISO file ──────────────────────
    let cdrom_tuple = crate::virtual_blockio::create_virtual_cdrom(
        bs, st as *mut SystemTable, iso_lba, bio_ptr, mid, iso_size, iso_name,
    );
    let (device_handle, cdrom_dp, vbio_ptr, sfs_instance) = match cdrom_tuple {
        Some((h, dp, vb, sfs)) => (h, dp, vb, sfs),
        None => {
            print_raw(st, b"ERROR: Failed to create virtual CD-ROM.\r\n\0");
            return;
        }
    };

    // ── Patch grub.cfg at BlockIO+SFS level ────────────────────
    patch_grub_cfg_blockio(st, bs, vbio_ptr, sfs_instance, bio_ref, bio_ptr, mid, iso_lba, iso_name);

    let (efi_lba, efi_size) = match find_efi_boot(st, bio_ref, bio_ptr, mid, iso_lba) {
        Some(v) => v,
        None => {
            print_raw(st, b"ERROR: /EFI/BOOT/BOOTX64.EFI not found in ISO.\r\n\0");
            return;
        }
    };

    // Read the EFI executable into a pool-allocated buffer
    let (buf_ptr, buf_len) = match read_extent(bs, bio_ref, bio_ptr, mid, iso_lba, efi_lba, efi_size) {
        Some(v) => v,
        None => {
            print_raw(st, b"ERROR: Failed to read EFI executable from ISO.\r\n\0");
            return;
        }
    };

    // Build a proper DevicePath so the child image can find its files
    let device_path = build_iso_device_path(bs, iso_size);

    print_raw(st, b"Loading EFI image...\r\n\0");

    // LoadImage
    let mut child_handle: *mut c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.load_image)(
            false,               // BootPolicy
            image_handle,        // ParentImageHandle
            device_path as *mut DevicePathProtocol, // DevicePath
            buf_ptr,             // SourceBuffer
            buf_len as u64,      // SourceSize
            &mut child_handle,
        )
    };

    // Free source buffer immediately — firmware has copied the image
    unsafe { (bs.free_pool)(buf_ptr as _); }

    // Patch LoadedImageProtocol.DeviceHandle + FilePath
    if status == EFI_SUCCESS {
        let mut lip: *mut LoadedImageProtocol = core::ptr::null_mut();
        let lip_status = unsafe {
            (bs.handle_protocol)(
                child_handle,
                &LOADED_IMAGE_PROTOCOL_GUID,
                &mut lip as *mut _ as _,
            )
        };
        if lip_status == EFI_SUCCESS && !lip.is_null() {
            unsafe {
                // Point DeviceHandle to the virtual CD-ROM (not USB disk)
                (*lip).device_handle = device_handle;
                (*lip).file_path = device_path;
                if !cdrom_dp.is_null() {
                    let _ = cdrom_dp; // already installed on the handle
                }
            }
        }
    } else {
        print_raw(st, b"ERROR: LoadImage failed.\r\n\0");
        unsafe { (bs.free_pool)(device_path); }
        return;
    }

    print_raw(st, b"Starting EFI image...\r\n\0");

    // StartImage
    let mut exit_data_size: u64 = 0;
    let mut exit_data: *mut u16 = core::ptr::null_mut();
    let status2 = unsafe {
        (bs.start_image)(child_handle, &mut exit_data_size, &mut exit_data)
    };

    // If we get here, the child image returned
    print_raw(st, b"WARNING: Image returned with status 0x");
    crate::output::print_hex(st, b"", status2 as u64);
    print_raw(st, b"\r\n\0");
}

// ═══════════════════════════════════════════════════════════════════════════
//  Chainloader entry point
// ═══════════════════════════════════════════════════════════════════════════

/// Boot an ISO by chainloading its UEFI bootloader (/EFI/BOOT/BOOTX64.EFI).
/// Returns on failure so the menu can continue.
pub fn boot_iso(
    st: &mut SystemTable,
    image_handle: *mut c_void,
    _disk_handle: *mut c_void,
    part1_lba: u64,
    files: &[IsoEntry; 64],
    idx: usize,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
) {
    print_raw(st, b"\r\nBooting ISO (UEFI chainload)...\r\n\0");
    uefi_chainload_iso(st, image_handle, part1_lba, files, idx, bio_ref, bio_ptr, mid);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Boot menu
// ═══════════════════════════════════════════════════════════════════════════

use crate::fs::scan_directory;

pub fn show_menu(
    st: &mut SystemTable,
    image_handle: *mut c_void,
    disk_handle: *mut c_void,
    files: &[IsoEntry; 64],
    count: usize,
    ctx: &FsCtx,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
) -> ! {
    if count == 0 {
        print_raw(st, b"\r\nNo ISO files found on partition 1.\r\n\0");
        halt_or_reboot(st);
    }
    print_raw(st, b"\r\n=== Choosable UEFI Boot Menu ===\r\n\0");
    for i in 0..count.min(20) {
        let (sb, sl) = format_u64_buf((i + 1) as u64);
        print_raw(st, b" ");
        print_raw(st, &sb[20 - sl..]);
        print_raw(st, b". ");
        if files[i].name_len > 0 && files[i].name[0] != 0 {
            print_raw(st, &files[i].name[..files[i].name_len]);
        }
        let size_mb = files[i].file_size / (1024 * 1024);
        let (sb2, sl2) = format_u64_buf(size_mb);
        print_raw(st, b" (");
        print_raw(st, &sb2[20 - sl2..]);
        print_raw(st, b" MiB)\r\n\0");
    }
    print_raw(st, b"Enter number to boot (or 'r' to scan): \0");

    use crate::protocol::{Key, SimpleTextInput};
    loop {
        let mut k = Key { sc: 0, uc: 0 };
        if !st.con_in.is_null() {
            let ci = unsafe { &mut *(st.con_in as *mut SimpleTextInput) };
            if unsafe { (ci.read_key_stroke)(ci as *mut _, &mut k) } != EFI_SUCCESS {
                continue;
            }
        }
        let ch = if k.uc >= 0x20 && k.uc < 0x7F {
            k.uc as u8
        } else if k.uc == 0x0D || k.uc == 0x0A {
            b'\n'
        } else {
            0x00
        };
        if (b'1'..=b'9').contains(&ch) {
            let idx = (ch - b'1') as usize;
            if idx < count {
                boot_iso(st, image_handle, disk_handle, ctx.part1_lba, files, idx, bio_ref, bio_ptr, mid);
            }
        } else if ch == b'0' && count >= 10 {
            boot_iso(st, image_handle, disk_handle, ctx.part1_lba, files, 9, bio_ref, bio_ptr, mid);
        } else if ch == b'r' || ch == b'R' {
            print_raw(st, b"\r\nRe-scanning...\r\n\0");
            let mut new_files: [IsoEntry; 64] = unsafe { core::mem::zeroed() };
            let mut new_count: usize = 0;
            scan_directory(bio_ref, bio_ptr, mid, ctx, &mut new_files, &mut new_count);
            show_menu(st, image_handle, disk_handle, &new_files, new_count, ctx, bio_ref, bio_ptr, mid);
        }
    }
    halt_or_reboot(st)
}