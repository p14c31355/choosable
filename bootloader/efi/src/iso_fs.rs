// ═══════════════════════════════════════════════════════════════════════════
//  ISO9660 EFI Simple File System Protocol
// ═══════════════════════════════════════════════════════════════════════════
//
//  Provides an ISO9660 read-only filesystem view to GRUB (or any UEFI
//  application) so it can find /EFI/BOOT/grub.cfg, load modules, etc.
//  Installed on the virtual CD-ROM handle alongside BlockI/O.
//
//  Architecture:
//    IsoFsInstance  ── one per volume, holds SimpleFileSystemProtocol
//    VirtualFile    ── one per open file/directory, holds FileProtocol
//
//  All memory is allocated from EFI BootServices pool.

use core::ffi::c_void;

use crate::protocol::{
    BlockIoProtocol, BootServices, FileProtocol, SimpleFileSystemProtocol,
    EfiFileInfo, EfiTime, Guid, SystemTable,
    EFI_SUCCESS, EFI_NOT_FOUND, EFI_INVALID_PARAMETER,
    EFI_UNSUPPORTED, EFI_BAD_BUFFER_SIZE, EFI_DEVICE_ERROR,
    EFI_WRITE_PROTECTED, FILE_INFO_GUID, EFI_OUT_OF_RESOURCES,
};

use crate::output::print_raw;

// ═══════════════════════════════════════════════════════════════════════════
//  Shared context (one per volume)
// ═══════════════════════════════════════════════════════════════════════════

pub struct IsoFsCtx {
    /// Pointer to real Block I/O for reading the ISO file from disk
    pub real_bio_ptr: *mut BlockIoProtocol,
    /// Real media ID
    pub real_media_id: u32,
    /// ISO file start LBA in 512-byte disk sectors
    pub iso_lba: u64,
    /// ISO file total size in bytes
    pub iso_size_bytes: u64,
    /// ISO9660 root directory extent LBA (in ISO 2048-byte sectors)
    pub root_lba: u32,
    /// ISO9660 root directory extent size in bytes
    pub root_size: u32,
    /// BootServices pointer (for pool allocation)
    pub bs: *mut BootServices,
    /// SystemTable pointer (for debug output)
    pub st: *mut SystemTable,
    /// ISO file name on the real USB drive (e.g. "ubuntu-24.04.iso")
    pub iso_name: [u8; 128],
    pub iso_name_len: usize,
    /// FAT32 volume serial number of the real USB partition (formatted "XXXX-XXXX\0")
    pub live_media_uuid: [u8; 10],
    /// Premount cpio buffer (appended to initrd reads)
    pub premount_cpio_buf: *mut u8,
    /// Premount cpio size in bytes
    pub premount_cpio_size: usize,
}

#[repr(C)]
pub struct IsoFsInstance {
    pub sfs: SimpleFileSystemProtocol,
    pub ctx: IsoFsCtx,
}

/// Per-open-file (or directory) state
#[repr(C)]
pub struct VirtualFile {
    pub file: FileProtocol,
    ctx: *const IsoFsCtx,
    is_dir: bool,
    extent_lba: u32,   // ISO 2048-byte sector
    extent_size: u32,  // bytes in ISO
    position: u64,      // current read offset
    /// If true, file_read_file will inject iso-scan/filename= into the buffer
    needs_grub_patch: bool,
    /// Pool-allocated patched copy of the file (only for grub.cfg)
    patched_buf: *mut u8,
    /// Size of the patched buffer
    patched_size: u64,
    /// Whether the patch has been applied
    patched: bool,
    /// If this is an initrd file, append premount cpio after the original data.
    /// The effective file size becomes extent_size + premount_cpio_size.
    is_initrd: bool,
}

// ═══════════════════════════════════════════════════════════════════════════
//  ISO9660 low-level read helpers
// ═══════════════════════════════════════════════════════════════════════════

fn read_iso_sector(
    ctx: &IsoFsCtx,
    iso_sector: u32,
    buf: &mut [u8; 2048],
) -> bool {
    let disk_lba = ctx.iso_lba + iso_sector as u64 * 4;
    let bio_ref = unsafe { &*ctx.real_bio_ptr };
    let status = unsafe {
        (bio_ref.read_blocks)(
            ctx.real_bio_ptr,
            ctx.real_media_id,
            disk_lba,
            2048,
            buf.as_mut_ptr() as *mut c_void,
        )
    };
    status == EFI_SUCCESS
}

fn read_extent_data(
    ctx: &IsoFsCtx,
    lba: u32,
    extent_size: u32,
    offset: u64,
    buf: &mut [u8],
) -> usize {
    if offset >= extent_size as u64 {
        return 0;
    }
    let max_read = (extent_size as u64 - offset).min(buf.len() as u64) as usize;
    let mut remaining = max_read;
    let mut dst_off = 0usize;

    let start_sector = (offset / 2048) as u32;
    let start_byte_in_sector = (offset % 2048) as usize;

    let mut scratch = [0u8; 2048];
    let mut first = true;
    let mut cur_sector = lba + start_sector;

    while remaining > 0 {
        if !read_iso_sector(ctx, cur_sector, &mut scratch) {
            break;
        }
        let src_off = if first { start_byte_in_sector } else { 0 };
        first = false;
        let copy = remaining.min(2048 - src_off);
        buf[dst_off..dst_off + copy].copy_from_slice(&scratch[src_off..src_off + copy]);
        dst_off += copy;
        remaining -= copy;
        cur_sector += 1;
    }
    dst_off
}

// ═══════════════════════════════════════════════════════════════════════════
//  ISO9660 directory lookup
// ═══════════════════════════════════════════════════════════════════════════

fn iso_name_effective_len(name: &[u8], name_len: usize) -> usize {
    if name_len >= 2 && name[name_len - 2] == b';' {
        name_len - 2
    } else {
        name_len
    }
}

fn match_iso_name(iso_name: &[u8], iso_name_len: usize, ucs2_name: &[u16]) -> bool {
    let eff_len = iso_name_effective_len(iso_name, iso_name_len);
    let name_bytes: [u8; 256] = {
        let mut arr = [0u8; 256];
        let mut i = 0;
        while i < ucs2_name.len() && i < 256 {
            let cp = ucs2_name[i];
            if cp == 0 { break; }
            arr[i] = if cp < 0x80 { cp as u8 } else { b'?' };
            i += 1;
        }
        arr
    };
    let name_len = ucs2_name.iter().position(|&c| c == 0).unwrap_or(ucs2_name.len());
    let name_slice = &name_bytes[..name_len.min(255)];
    if eff_len != name_slice.len() { return false; }
    for i in 0..eff_len {
        if iso_name[i].to_ascii_uppercase() != name_slice[i].to_ascii_uppercase() {
            return false;
        }
    }
    true
}

fn lookup_in_dir(
    ctx: &IsoFsCtx,
    dir_lba: u32,
    dir_size: u32,
    name: &[u16],
) -> Option<(u32, u32, bool)> {
    let total_sectors = ((dir_size as u64 + 2047) / 2048) as u32;
    let mut scratch = [0u8; 2048];
    for s in 0..total_sectors {
        if !read_iso_sector(ctx, dir_lba + s, &mut scratch) { return None; }
        let mut offset: usize = 0;
        while offset + 34 <= 2048 {
            let record_len = scratch[offset] as usize;
            if record_len == 0 { break; }
            if record_len < 34 || offset + record_len > 2048 { break; }
            let name_len = scratch[offset + 32] as usize;
            let name_offset = offset + 33;
            if 33 + name_len > record_len || name_offset + name_len > 2048 { break; }
            let iso_name = &scratch[name_offset..name_offset + name_len];
            if match_iso_name(iso_name, name_len, name) {
                let child_extent = u32::from_le_bytes(
                    scratch[offset + 2..offset + 6].try_into().unwrap(),
                );
                let child_size = u32::from_le_bytes(
                    scratch[offset + 10..offset + 14].try_into().unwrap(),
                );
                let flags = scratch[offset + 25];
                let is_dir = flags & 0x02 != 0;
                return Some((child_extent, child_size, is_dir));
            }
            offset += record_len;
        }
    }
    None
}

fn parse_pvd(ctx: &IsoFsCtx) -> Option<(u32, u32)> {
    let mut pvd = [0u8; 2048];
    if !read_iso_sector(ctx, 16, &mut pvd) { return None; }
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" { return None; }
    let root_extent = u32::from_le_bytes(pvd[158..162].try_into().unwrap());
    let root_size = u32::from_le_bytes(pvd[166..170].try_into().unwrap());
    Some((root_extent, root_size))
}

// ═══════════════════════════════════════════════════════════════════════════
//  SimpleFileSystemProtocol::open_volume
// ═══════════════════════════════════════════════════════════════════════════

unsafe extern "efiapi" fn sfs_open_volume(
    this: *mut SimpleFileSystemProtocol,
    root: *mut *mut FileProtocol,
) -> usize {
    if root.is_null() { return EFI_INVALID_PARAMETER; }
    let instance = &*(this as *const IsoFsInstance);
    let ctx = &instance.ctx;
    let bs = unsafe { &mut *ctx.bs };
    let mut ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.allocate_pool)(
            crate::protocol::MemoryType::EfiLoaderData,
            core::mem::size_of::<VirtualFile>(),
            &mut ptr,
        )
    };
    if status != EFI_SUCCESS || ptr.is_null() { return EFI_OUT_OF_RESOURCES; }
    let vf = unsafe { &mut *(ptr as *mut VirtualFile) };
    vf.file = FileProtocol {
        revision: 0x0001_0000_0000_0001,
        open: file_open, close: file_close, delete: file_delete,
        read: file_read_dir, write: file_write_ro,
        get_position: file_get_position, set_position: file_set_position,
        get_info: file_get_info, set_info: file_set_info_ro, flush: file_flush,
    };
    vf.ctx = ctx as *const IsoFsCtx;
    vf.is_dir = true;
    vf.extent_lba = ctx.root_lba;
    vf.extent_size = ctx.root_size;
    vf.position = 0;
    vf.needs_grub_patch = false;
    vf.patched_buf = core::ptr::null_mut();
    vf.patched_size = 0;
    vf.patched = false;
    vf.is_initrd = false;
    *root = ptr as *mut FileProtocol;
    EFI_SUCCESS
}

// ═══════════════════════════════════════════════════════════════════════════
//  FileProtocol implementations
// ═══════════════════════════════════════════════════════════════════════════

fn resolve_path(
    ctx: &IsoFsCtx,
    start_lba: u32,
    start_size: u32,
    path: &[u16],
) -> Option<(u32, u32, bool)> {
    if path.is_empty() || (path.len() == 1 && path[0] == b'\\' as u16) {
        return Some((start_lba, start_size, true));
    }
    let mut pos = 0usize;
    let mut last_paren_backslash: Option<usize> = None;
    let mut i = 0;
    while i + 1 < path.len() {
        if path[i] == b')' as u16 && path[i + 1] == b'\\' as u16 {
            last_paren_backslash = Some(i + 1);
        }
        i += 1;
    }
    if let Some(start) = last_paren_backslash { pos = start; }
    while pos < path.len() && path[pos] == b'\\' as u16 { pos += 1; }
    if pos >= path.len() { return Some((start_lba, start_size, true)); }
    let mut cur_lba = start_lba;
    let mut cur_size = start_size;
    while pos < path.len() {
        let comp_start = pos;
        while pos < path.len() && path[pos] != b'\\' as u16 { pos += 1; }
        let component = &path[comp_start..pos];
        let (child_lba, child_size, is_dir) = lookup_in_dir(ctx, cur_lba, cur_size, component)?;
        if pos < path.len() && path[pos] == b'\\' as u16 { pos += 1; }
        let has_more = pos < path.len() && !(&path[pos..]).is_empty() && path[pos] != 0;
        if has_more && !is_dir { return None; }
        cur_lba = child_lba;
        cur_size = child_size;
        if !has_more { return Some((cur_lba, cur_size, is_dir)); }
    }
    Some((cur_lba, cur_size, false))
}

fn alloc_virtual_file(
    ctx: &IsoFsCtx,
    lba: u32,
    size: u32,
    is_dir: bool,
    needs_grub_patch: bool,
    is_initrd: bool,
) -> Option<*mut FileProtocol> {
    let bs = unsafe { &mut *ctx.bs };
    let mut ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.allocate_pool)(
            crate::protocol::MemoryType::EfiLoaderData,
            core::mem::size_of::<VirtualFile>(),
            &mut ptr,
        )
    };
    if status != EFI_SUCCESS || ptr.is_null() { return None; }
    let vf = unsafe { &mut *(ptr as *mut VirtualFile) };
    vf.file = FileProtocol {
        revision: 0x0001_0000_0000_0001,
        open: file_open, close: file_close, delete: file_delete,
        read: if is_dir { file_read_dir } else { file_read_file },
        write: file_write_ro,
        get_position: file_get_position, set_position: file_set_position,
        get_info: file_get_info, set_info: file_set_info_ro, flush: file_flush,
    };
    vf.ctx = ctx as *const IsoFsCtx;
    vf.is_dir = is_dir;
    vf.extent_lba = lba;
    vf.extent_size = size;
    vf.position = 0;
    vf.needs_grub_patch = needs_grub_patch;
    vf.patched_buf = core::ptr::null_mut();
    vf.patched_size = 0;
    vf.patched = false;
    vf.is_initrd = is_initrd;
    Some(ptr as *mut FileProtocol)
}

/// Return the effective file size: original extent + premount cpio if initrd.
fn effective_size(vf: &VirtualFile) -> u64 {
    if vf.patched {
        vf.patched_size
    } else if vf.is_initrd {
        let ctx = unsafe { &*vf.ctx };
        vf.extent_size as u64 + ctx.premount_cpio_size as u64
    } else {
        vf.extent_size as u64
    }
}

/// Check if a filename matches known initrd patterns.
fn detect_initrd(name: &[u16]) -> bool {
    let n = name.len();
    // Match "initrd", "initrd.gz", "initrd.lz", "initrd.xz" etc.
    // Also match "initrd" at the start (initrd, initrd.img, ...)
    if n >= 6 {
        let s: [u16; 6] = [name[0], name[1], name[2], name[3], name[4], name[5]];
        let lower = |c: u16| if (b'A' as u16..=b'Z' as u16).contains(&c) { c | 0x20 } else { c };
        if lower(s[0]) == b'i' as u16
            && lower(s[1]) == b'n' as u16
            && lower(s[2]) == b'i' as u16
            && lower(s[3]) == b't' as u16
            && lower(s[4]) == b'r' as u16
            && lower(s[5]) == b'd' as u16
        {
            return true;
        }
    }
    false
}

unsafe extern "efiapi" fn file_open(
    this: *mut FileProtocol,
    new_handle: *mut *mut FileProtocol,
    file_name: *const u16,
    open_mode: u64,
    _attributes: u64,
) -> usize {
    if file_name.is_null() || new_handle.is_null() { return EFI_INVALID_PARAMETER; }
    let vf = unsafe { &*(this as *const VirtualFile) };
    if !vf.is_dir { return EFI_UNSUPPORTED; }
    let ctx = unsafe { &*vf.ctx };

    let name_slice = unsafe {
        let mut len = 0usize;
        while *file_name.add(len) != 0 {
            len += 1;
            if len > 256 { return EFI_INVALID_PARAMETER; }
        }
        core::slice::from_raw_parts(file_name, len)
    };

    let resolved = resolve_path(ctx, vf.extent_lba, vf.extent_size, name_slice);
    let (child_lba, child_size, is_dir) = match resolved {
        Some(v) => v,
        None => return EFI_NOT_FOUND,
    };

    let is_cfg = if !is_dir && name_slice.len() >= 4 {
        let n = name_slice.len();
        let s: [u8; 4] = [
            (name_slice[n - 4] as u8) | 0x20,
            (name_slice[n - 3] as u8) | 0x20,
            (name_slice[n - 2] as u8) | 0x20,
            (name_slice[n - 1] as u8) | 0x20,
        ];
        s[0] == b'.' && s[1] == b'c' && s[2] == b'f' && s[3] == b'g'
    } else { false };

    let initrd_detect = !is_dir && detect_initrd(name_slice) && ctx.premount_cpio_size > 0;

    let fp = match alloc_virtual_file(ctx, child_lba, child_size, is_dir, is_cfg, initrd_detect) {
        Some(p) => p,
        None => return EFI_OUT_OF_RESOURCES,
    };

    // Eagerly patch .cfg files
    if is_cfg {
        let vf_patch = unsafe { &mut *(fp as *mut VirtualFile) };
        if !vf_patch.patched {
            let orig_size = vf_patch.extent_size as usize;
            let bs = unsafe { &mut *ctx.bs };
            let mut tmp_ptr: *mut c_void = core::ptr::null_mut();
            let tmp_status = unsafe {
                (bs.allocate_pool)(crate::protocol::MemoryType::EfiLoaderData, orig_size, &mut tmp_ptr)
            };
            if tmp_status == EFI_SUCCESS && !tmp_ptr.is_null() {
                let tmp_buf = unsafe { core::slice::from_raw_parts_mut(tmp_ptr as *mut u8, orig_size) };
                let mut total_read = 0usize;
                while total_read < orig_size {
                    let rem = (orig_size - total_read).min(2048);
                    let r = read_extent_data(ctx, child_lba, child_size, total_read as u64,
                        &mut tmp_buf[total_read..total_read + rem]);
                    if r == 0 { break; }
                    total_read += r;
                }
                let patch = crate::strategy::patch_grub_cfg(ctx, &tmp_buf[..total_read], ctx.bs, None);
                if let Some(p) = patch {
                    vf_patch.patched_buf = p.buf;
                    vf_patch.patched_size = p.size as u64;
                    vf_patch.patched = true;
                }
                unsafe { (bs.free_pool)(tmp_ptr); }
            }
        }
    }

    let _ = open_mode;
    *new_handle = fp;
    EFI_SUCCESS
}

unsafe extern "efiapi" fn file_close(this: *mut FileProtocol) -> usize {
    let vf = unsafe { &mut *(this as *mut VirtualFile) };
    let ctx = unsafe { &*vf.ctx };
    let bs = unsafe { &mut *ctx.bs };
    if !vf.patched_buf.is_null() {
        unsafe { (bs.free_pool)(vf.patched_buf as *mut c_void) };
        vf.patched_buf = core::ptr::null_mut();
    }
    unsafe { (bs.free_pool)(this as *mut c_void) };
    EFI_SUCCESS
}

unsafe extern "efiapi" fn file_delete(this: *mut FileProtocol) -> usize {
    let _ = file_close(this);
    EFI_WRITE_PROTECTED
}

unsafe extern "efiapi" fn file_read_file(
    this: *mut FileProtocol,
    buffer_size: *mut usize,
    buffer: *mut c_void,
) -> usize {
    if buffer_size.is_null() || buffer.is_null() { return EFI_INVALID_PARAMETER; }
    let vf = unsafe { &mut *(this as *mut VirtualFile) };
    if vf.is_dir { return EFI_UNSUPPORTED; }
    let ctx = unsafe { &*vf.ctx };
    let size = unsafe { *buffer_size };
    if size == 0 { return EFI_SUCCESS; }

    // Case 1: patched file (grub.cfg)
    if vf.patched && !vf.patched_buf.is_null() {
        let avail = (vf.patched_size - vf.position as u64) as usize;
        let to_copy = size.min(avail);
        if to_copy == 0 {
            unsafe { *buffer_size = 0; }
            return EFI_SUCCESS;
        }
        let src = unsafe { core::slice::from_raw_parts(vf.patched_buf.add(vf.position as usize), to_copy) };
        let dst = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, to_copy) };
        dst.copy_from_slice(src);
        vf.position += to_copy as u64;
        unsafe { *buffer_size = to_copy; }
        return EFI_SUCCESS;
    }

    // Case 2: initrd file with premount cpio appended
    if vf.is_initrd && ctx.premount_cpio_size > 0 && !ctx.premount_cpio_buf.is_null() {
        let orig_end = vf.extent_size as u64;
        let combined_end = orig_end + ctx.premount_cpio_size as u64;

        if vf.position >= combined_end {
            unsafe { *buffer_size = 0; }
            return EFI_SUCCESS;
        }

        // If position is past the original extent, serve from premount cpio
        if vf.position >= orig_end {
            let cpio_offset = (vf.position - orig_end) as usize;
            let avail = ctx.premount_cpio_size - cpio_offset;
            let to_copy = size.min(avail);
            let src = unsafe {
                core::slice::from_raw_parts(ctx.premount_cpio_buf.add(cpio_offset), to_copy)
            };
            let dst = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, to_copy) };
            dst.copy_from_slice(src);
            vf.position += to_copy as u64;
            unsafe { *buffer_size = to_copy; }
            return EFI_SUCCESS;
        }

        // Normal read from ISO extent (max up to orig_end)
        let max_from_orig = (orig_end - vf.position) as usize;
        let from_orig = size.min(max_from_orig);
        let dst = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, from_orig) };
        let read = read_extent_data(ctx, vf.extent_lba, vf.extent_size, vf.position, dst);
        vf.position += read as u64;
        let total_read = read;

        // If we've reached the end of the original data and there's room,
        // also include premount cpio data in this read
        if read >= from_orig && from_orig < size && vf.position >= orig_end {
            let cpio_max = (combined_end - vf.position) as usize;
            let cpio_to_read = cpio_max.min(size - from_orig);
            if cpio_to_read > 0 {
                let cpio_offset = (vf.position - orig_end) as usize;
                let src = unsafe {
                    core::slice::from_raw_parts(ctx.premount_cpio_buf.add(cpio_offset), cpio_to_read)
                };
                let dst2 = unsafe {
                    core::slice::from_raw_parts_mut(buffer as *mut u8, size)
                };
                dst2[from_orig..from_orig + cpio_to_read].copy_from_slice(src);
                vf.position += cpio_to_read as u64;
                unsafe { *buffer_size = from_orig + cpio_to_read; }
                return EFI_SUCCESS;
            }
        }

        unsafe { *buffer_size = total_read; }
        return EFI_SUCCESS;
    }

    // Case 3: normal read from ISO extent
    let dst = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, size) };
    let read = read_extent_data(ctx, vf.extent_lba, vf.extent_size, vf.position, dst);
    vf.position += read as u64;
    unsafe { *buffer_size = read; }
    EFI_SUCCESS
}

unsafe extern "efiapi" fn file_read_dir(
    this: *mut FileProtocol,
    buffer_size: *mut usize,
    buffer: *mut c_void,
) -> usize {
    if buffer_size.is_null() || buffer.is_null() { return EFI_INVALID_PARAMETER; }
    let vf = unsafe { &mut *(this as *mut VirtualFile) };
    if !vf.is_dir { return EFI_UNSUPPORTED; }
    let ctx = unsafe { &*vf.ctx };
    let buf_sz = unsafe { *buffer_size };
    let dst = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, buf_sz) };
    let dir_size = vf.extent_size;
    let total_sectors = ((dir_size as u64 + 2047) / 2048) as u32;
    let mut scratch = [0u8; 2048];
    let mut dst_off = 0usize;
    let mut finished = false;
    let mut sector_idx = (vf.position >> 16) as u32;
    let mut byte_offset = (vf.position & 0xFFFF) as usize;
    if sector_idx >= total_sectors { unsafe { *buffer_size = 0; } return EFI_SUCCESS; }
    if !read_iso_sector(ctx, vf.extent_lba + sector_idx, &mut scratch) { return EFI_DEVICE_ERROR; }
    while !finished && dst_off + core::mem::size_of::<EfiFileInfo>() + 2 <= buf_sz {
        if byte_offset + 34 > 2048 || (byte_offset > 0 && scratch[byte_offset] == 0) {
            sector_idx += 1; byte_offset = 0;
            if sector_idx >= total_sectors { finished = true; break; }
            if !read_iso_sector(ctx, vf.extent_lba + sector_idx, &mut scratch) {
                vf.position = ((sector_idx as u64) << 16) | (byte_offset as u64);
                return EFI_DEVICE_ERROR;
            }
            if scratch[0] == 0 { finished = true; break; }
        }
        let record_len = scratch[byte_offset] as usize;
        if record_len == 0 {
            sector_idx += 1; byte_offset = 0;
            if sector_idx >= total_sectors { finished = true; break; }
            if !read_iso_sector(ctx, vf.extent_lba + sector_idx, &mut scratch) {
                vf.position = ((sector_idx as u64) << 16) | (byte_offset as u64);
                return EFI_DEVICE_ERROR;
            }
            continue;
        }
        if record_len < 34 || byte_offset + record_len > 2048 { finished = true; break; }
        let name_len = scratch[byte_offset + 32] as usize;
        let name_offset = byte_offset + 33;
        if 33 + name_len > record_len || name_offset + name_len > 2048 { finished = true; break; }
        let iso_name = &scratch[name_offset..name_offset + name_len];
        let ef_len = iso_name_effective_len(iso_name, name_len);
        let child_size = u32::from_le_bytes(scratch[byte_offset + 10..byte_offset + 14].try_into().unwrap());
        let flags = scratch[byte_offset + 25];
        let is_dir = flags & 0x02 != 0;
        let ucs2_name_len = ef_len;
        let raw_size = core::mem::size_of::<EfiFileInfo>() + (ucs2_name_len + 1) * 2;
        let required_size = (raw_size + 7) & !7;
        if dst_off + required_size > buf_sz {
            if dst_off == 0 { unsafe { *buffer_size = required_size; } return crate::protocol::EFI_BUFFER_TOO_SMALL; }
            break;
        }
        let ct: EfiTime = unsafe { core::mem::zeroed() };
        let lat: EfiTime = unsafe { core::mem::zeroed() };
        let mt: EfiTime = unsafe { core::mem::zeroed() };
        let info = EfiFileInfo {
            size: required_size as u64,
            file_size: child_size as u64,
            physical_size: child_size as u64,
            create_time: ct, last_access_time: lat, modification_time: mt,
            attribute: if is_dir { 1 } else { 0 },
        };
        let info_bytes = unsafe {
            core::slice::from_raw_parts(&info as *const EfiFileInfo as *const u8, core::mem::size_of::<EfiFileInfo>())
        };
        dst[dst_off..dst_off + info_bytes.len()].copy_from_slice(info_bytes);
        dst_off += info_bytes.len();
        for j in 0..ef_len {
            let ch = iso_name[j] as u16;
            dst[dst_off] = ch as u8; dst[dst_off + 1] = (ch >> 8) as u8;
            dst_off += 2;
        }
        dst[dst_off] = 0; dst[dst_off + 1] = 0; dst_off += 2;
        let padding = required_size - raw_size;
        for _ in 0..padding { dst[dst_off] = 0; dst_off += 1; }
        byte_offset += record_len;
        vf.position = ((sector_idx as u64) << 16) | (byte_offset as u64);
    }
    if finished { vf.position = ((total_sectors as u64) << 16) | 0x10000; }
    unsafe { *buffer_size = dst_off; }
    EFI_SUCCESS
}

unsafe extern "efiapi" fn file_write_ro(
    _this: *mut FileProtocol,
    _buffer_size: *mut usize,
    _buffer: *mut c_void,
) -> usize { EFI_WRITE_PROTECTED }

unsafe extern "efiapi" fn file_get_position(
    this: *mut FileProtocol,
    position: *mut u64,
) -> usize {
    if position.is_null() { return EFI_INVALID_PARAMETER; }
    let vf = unsafe { &*(this as *const VirtualFile) };
    unsafe { *position = vf.position; }
    EFI_SUCCESS
}

unsafe extern "efiapi" fn file_set_position(
    this: *mut FileProtocol,
    position: u64,
) -> usize {
    let vf = unsafe { &mut *(this as *mut VirtualFile) };
    if vf.is_dir {
        if position != 0 { return EFI_UNSUPPORTED; }
        vf.position = 0;
        return EFI_SUCCESS;
    }
    let max_pos = effective_size(vf);
    vf.position = if position > max_pos { max_pos } else { position };
    EFI_SUCCESS
}

unsafe extern "efiapi" fn file_get_info(
    this: *mut FileProtocol,
    information_type: *const Guid,
    buffer_size: *mut usize,
    buffer: *mut c_void,
) -> usize {
    if information_type.is_null() || buffer_size.is_null() { return EFI_INVALID_PARAMETER; }
    let info_type = unsafe { &*information_type };
    if info_type.d1 != FILE_INFO_GUID.d1
        || info_type.d2 != FILE_INFO_GUID.d2
        || info_type.d3 != FILE_INFO_GUID.d3
        || info_type.d4 != FILE_INFO_GUID.d4
    { return EFI_UNSUPPORTED; }
    let vf = unsafe { &*(this as *const VirtualFile) };
    let file_name: [u16; 1] = [0];
    let required_size = core::mem::size_of::<EfiFileInfo>() + file_name.len() * 2;
    let buf_sz = unsafe { *buffer_size };
    if buf_sz < required_size { unsafe { *buffer_size = required_size; } return crate::protocol::EFI_BUFFER_TOO_SMALL; }
    if buffer.is_null() { return EFI_SUCCESS; }
    let create_time: EfiTime = unsafe { core::mem::zeroed() };
    let last_access_time: EfiTime = unsafe { core::mem::zeroed() };
    let modification_time: EfiTime = unsafe { core::mem::zeroed() };
    let file_size = effective_size(vf);
    let info = EfiFileInfo {
        size: required_size as u64,
        file_size,
        physical_size: file_size,
        create_time, last_access_time, modification_time,
        attribute: if vf.is_dir { 0x0000_0000_0000_0001 } else { 0 },
    };
    let dst = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, buf_sz) };
    let info_bytes = unsafe {
        core::slice::from_raw_parts(&info as *const EfiFileInfo as *const u8, core::mem::size_of::<EfiFileInfo>())
    };
    dst[..info_bytes.len()].copy_from_slice(info_bytes);
    let name_offset = core::mem::size_of::<EfiFileInfo>();
    dst[name_offset] = 0; dst[name_offset + 1] = 0;
    unsafe { *buffer_size = required_size; }
    EFI_SUCCESS
}

unsafe extern "efiapi" fn file_set_info_ro(
    _this: *mut FileProtocol,
    _information_type: *const Guid,
    _buffer_size: usize,
    _buffer: *mut c_void,
) -> usize { EFI_WRITE_PROTECTED }

unsafe extern "efiapi" fn file_flush(_this: *mut FileProtocol) -> usize { EFI_SUCCESS }

// ═══════════════════════════════════════════════════════════════════════════
//  Public constructor
// ═══════════════════════════════════════════════════════════════════════════

pub fn create_iso_fs(
    bs: &mut BootServices,
    st: *mut SystemTable,
    real_bio_ptr: *mut BlockIoProtocol,
    real_media_id: u32,
    iso_lba: u64,
    iso_size_bytes: u64,
    iso_name: &[u8],
    live_media_uuid: &[u8; 10],
    premount_cpio_buf: *mut u8,
    premount_cpio_size: usize,
) -> *mut IsoFsInstance {
    let mut ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.allocate_pool)(
            crate::protocol::MemoryType::EfiLoaderData,
            core::mem::size_of::<IsoFsInstance>(),
            &mut ptr,
        )
    };
    if status != EFI_SUCCESS || ptr.is_null() { return core::ptr::null_mut(); }
    let instance = unsafe { &mut *(ptr as *mut IsoFsInstance) };
    let name_len = iso_name.len().min(127);
    let mut name_arr = [0u8; 128];
    name_arr[..name_len].copy_from_slice(&iso_name[..name_len]);
    let mut uuid_arr = [0u8; 10];
    uuid_arr.copy_from_slice(&live_media_uuid[..10]);
    instance.ctx = IsoFsCtx {
        real_bio_ptr, real_media_id, iso_lba, iso_size_bytes,
        root_lba: 0, root_size: 0,
        bs: bs as *mut BootServices, st,
        iso_name: name_arr, iso_name_len: name_len,
        live_media_uuid: uuid_arr,
        premount_cpio_buf,
        premount_cpio_size,
    };
    if let Some((root_lba, root_size)) = parse_pvd(&instance.ctx) {
        instance.ctx.root_lba = root_lba;
        instance.ctx.root_size = root_size;
    } else {
        unsafe { (bs.free_pool)(ptr); }
        return core::ptr::null_mut();
    }
    instance.sfs = SimpleFileSystemProtocol {
        revision: 0x0001_0000_0000_0001,
        open_volume: sfs_open_volume,
    };
    ptr as *mut IsoFsInstance
}