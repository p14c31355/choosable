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
    EfiFileInfo, EfiTime, Guid, SystemTable, VirtualBlockIo,
    EFI_SUCCESS, EFI_NOT_FOUND, EFI_INVALID_PARAMETER,
    EFI_UNSUPPORTED, EFI_BAD_BUFFER_SIZE, EFI_DEVICE_ERROR,
    EFI_WRITE_PROTECTED, FILE_INFO_GUID, EFI_OUT_OF_RESOURCES,
};

// ═══════════════════════════════════════════════════════════════════════════
//  Shared context (one per volume)
// ═══════════════════════════════════════════════════════════════════════════

pub struct IsoFsCtx {
    pub real_bio_ptr: *mut BlockIoProtocol,
    pub real_media_id: u32,
    pub iso_lba: u64,
    pub iso_size_bytes: u64,
    pub root_lba: u32,
    pub root_size: u32,
    pub bs: *mut BootServices,
    pub st: *mut SystemTable,
    pub iso_name: [u8; 128],
    pub iso_name_len: usize,
    pub live_media_uuid: [u8; 10],
    /// Premount cpio data (served as synthetic file)
    pub premount_cpio_buf: *mut u8,
    pub premount_cpio_size: usize,
    /// ISO9660 name used as premount injection target (e.g. "MD5SUM.TXT")
    pub premount_target_name: [u8; 16],
    pub premount_target_name_len: usize,
    /// Detected distro family (set after scanning ISO directory structure).
    pub boot_kind: crate::boot_kind::BootKind,
    pub bootloader_type: crate::boot_kind::BootloaderType,
    /// Virtual Block I/O handle — when set, SFS reads go through the
    /// virtual Block I/O (which applies patched grub.cfg, PVD edits, and
    /// premount CPIO injection) instead of reading directly from the
    /// physical disk.
    pub vbio_ptr: *mut VirtualBlockIo,
}

#[repr(C)]
pub struct IsoFsInstance {
    pub sfs: SimpleFileSystemProtocol,
    pub ctx: IsoFsCtx,
}

#[repr(C)]
pub struct VirtualFile {
    pub file: FileProtocol,
    ctx: *const IsoFsCtx,
    is_dir: bool,
    extent_lba: u32,
    extent_size: u32,
    position: u64,
    needs_grub_patch: bool,
    patched_buf: *mut u8,
    patched_size: u64,
    patched: bool,
    /// Serves premount cpio data from memory (no ISO extent)
    is_synthetic: bool,
    synthetic_buf: *mut u8,
    synthetic_size: usize,
    /// When this directory handle serves as root, this flag tracks whether
    /// the synthetic premount entry has already been injected into readdir.
    synthetic_injected: bool,
}

// ═══════════════════════════════════════════════════════════════════════════
//  ISO9660 low-level read helpers
// ═══════════════════════════════════════════════════════════════════════════

fn read_iso_sector(ctx: &IsoFsCtx, iso_sector: u32, buf: &mut [u8; 2048]) -> bool {
    let disk_lba = ctx.iso_lba + iso_sector as u64 * 4;
    let bio_ref = unsafe { &*ctx.real_bio_ptr };
    let status = unsafe {
        (bio_ref.read_blocks)(ctx.real_bio_ptr, ctx.real_media_id, disk_lba, 2048, buf.as_mut_ptr() as *mut c_void)
    };
    status == EFI_SUCCESS
}

fn read_extent_data(ctx: &IsoFsCtx, lba: u32, extent_size: u32, offset: u64, buf: &mut [u8]) -> usize {
    if offset >= extent_size as u64 { return 0; }
    let max_read = (extent_size as u64 - offset).min(buf.len() as u64) as usize;
    let mut remaining = max_read;
    let mut dst_off = 0usize;
    let start_sector = (offset / 2048) as u32;
    let start_byte_in_sector = (offset % 2048) as usize;
    let mut scratch = [0u8; 2048];
    let mut first = true;
    let mut cur_sector = lba + start_sector;
    while remaining > 0 {
        if !read_iso_sector(ctx, cur_sector, &mut scratch) { break; }
        let src_off = if first { start_byte_in_sector } else { 0 };
        first = false;
        let copy = remaining.min(2048 - src_off);
        buf[dst_off..dst_off + copy].copy_from_slice(&scratch[src_off..src_off + copy]);
        dst_off += copy; remaining -= copy; cur_sector += 1;
    }
    dst_off
}

fn iso_name_effective_len(name: &[u8], name_len: usize) -> usize {
    if name_len >= 2 && name[name_len - 2] == b';' { name_len - 2 } else { name_len }
}

fn match_iso_name(iso_name: &[u8], iso_name_len: usize, ucs2_name: &[u16]) -> bool {
    let eff_len = iso_name_effective_len(iso_name, iso_name_len);
    let name_bytes: [u8; 256] = {
        let mut arr = [0u8; 256];
        let mut i = 0;
        while i < ucs2_name.len() && i < 256 {
            let cp = ucs2_name[i]; if cp == 0 { break; }
            arr[i] = if cp < 0x80 { cp as u8 } else { b'?' };
            i += 1;
        }
        arr
    };
    let name_len = ucs2_name.iter().position(|&c| c == 0).unwrap_or(ucs2_name.len());
    let name_slice = &name_bytes[..name_len.min(255)];
    if eff_len != name_slice.len() { return false; }
    for i in 0..eff_len {
        if iso_name[i].to_ascii_uppercase() != name_slice[i].to_ascii_uppercase() { return false; }
    }
    true
}

fn lookup_in_dir(ctx: &IsoFsCtx, dir_lba: u32, dir_size: u32, name: &[u16]) -> Option<(u32, u32, bool)> {
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
                let child_extent = u32::from_le_bytes(scratch[offset + 2..offset + 6].try_into().unwrap());
                let child_size = u32::from_le_bytes(scratch[offset + 10..offset + 14].try_into().unwrap());
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

// ── Synthetic premount file detection ──

/// Check if the UCS-2 path component matches a target byte string case-insensitively.
/// Handles ISO9660 version suffix (e.g. ";1") by stripping it from the component
/// before comparison.
fn path_component_matches(path: &[u16], target: &[u8]) -> bool {
    // Find the last path separator to extract filename component.
    let mut start = 0usize;
    for i in (0..path.len()).rev() {
        if path[i] == b'\\' as u16 || path[i] == b'/' as u16 {
            start = i + 1;
            break;
        }
    }
    let component = &path[start..];
    if component.is_empty() {
        return false;
    }

    // Strip ISO9660 version suffix ";" + digits from component, e.g. "MD5SUM.TXT;1" → "MD5SUM.TXT"
    let comp_len = if component.len() >= 2 && component[component.len() - 2] == b';' as u16 {
        component.len() - 2
    } else {
        component.len()
    };

    if comp_len != target.len() {
        return false;
    }
    let lower = |c: u16| if (b'A' as u16..=b'Z' as u16).contains(&c) { c | 0x20 } else { c };
    let target_lower = |b: u8| b | 0x20;
    for i in 0..target.len() {
        if lower(component[i]) != target_lower(target[i]) as u16 {
            return false;
        }
    }
    true
}

// ═══════════════════════════════════════════════════════════════════════════
//  SimpleFileSystemProtocol::open_volume
// ═══════════════════════════════════════════════════════════════════════════

unsafe extern "efiapi" fn sfs_open_volume(this: *mut SimpleFileSystemProtocol, root: *mut *mut FileProtocol) -> usize {
    if root.is_null() { return EFI_INVALID_PARAMETER; }
    let instance = &*(this as *const IsoFsInstance);
    let ctx = &instance.ctx;
    let bs = unsafe { &mut *ctx.bs };
    let mut ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe { (bs.allocate_pool)(crate::protocol::MemoryType::EfiLoaderData, core::mem::size_of::<VirtualFile>(), &mut ptr) };
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
    vf.patched_size = 0; vf.patched = false;
    vf.is_synthetic = false;
    vf.synthetic_buf = core::ptr::null_mut();
    vf.synthetic_size = 0;
    vf.synthetic_injected = false;
    *root = ptr as *mut FileProtocol;
    EFI_SUCCESS
}

// ═══════════════════════════════════════════════════════════════════════════
//  FileProtocol implementations
// ═══════════════════════════════════════════════════════════════════════════

fn resolve_path(ctx: &IsoFsCtx, start_lba: u32, start_size: u32, path: &[u16]) -> Option<(u32, u32, bool)> {
    if path.is_empty() || (path.len() == 1 && path[0] == b'\\' as u16) { return Some((start_lba, start_size, true)); }
    let mut pos = 0usize;
    let mut last_paren_backslash: Option<usize> = None;
    let mut i = 0;
    while i + 1 < path.len() {
        if path[i] == b')' as u16 && path[i + 1] == b'\\' as u16 { last_paren_backslash = Some(i + 1); }
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
        cur_lba = child_lba; cur_size = child_size;
        if !has_more { return Some((cur_lba, cur_size, is_dir)); }
    }
    Some((cur_lba, cur_size, false))
}

unsafe extern "efiapi" fn file_open(
    this: *mut FileProtocol, new_handle: *mut *mut FileProtocol,
    file_name: *const u16, open_mode: u64, _attributes: u64,
) -> usize {
    if file_name.is_null() || new_handle.is_null() { return EFI_INVALID_PARAMETER; }
    let vf = unsafe { &*(this as *const VirtualFile) };
    if !vf.is_dir { return EFI_UNSUPPORTED; }
    let ctx = unsafe { &*vf.ctx };

    let name_slice = unsafe {
        let mut len = 0usize;
        while *file_name.add(len) != 0 { len += 1; if len > 256 { return EFI_INVALID_PARAMETER; } }
        core::slice::from_raw_parts(file_name, len)
    };

    // ── Synthetic premount cpio file (served at root level) ──
    let is_synthetic = ctx.premount_cpio_size > 0
        && !ctx.premount_cpio_buf.is_null()
        && (path_component_matches(name_slice, &ctx.premount_target_name[..ctx.premount_target_name_len])
            || path_component_matches(name_slice, b"PREMOUNT.CPIO"));
    if is_synthetic {
        let bs = unsafe { &mut *ctx.bs };
        let mut ptr: *mut c_void = core::ptr::null_mut();
        let status = unsafe { (bs.allocate_pool)(crate::protocol::MemoryType::EfiLoaderData, core::mem::size_of::<VirtualFile>(), &mut ptr) };
        if status != EFI_SUCCESS || ptr.is_null() { return EFI_OUT_OF_RESOURCES; }
        let svf = unsafe { &mut *(ptr as *mut VirtualFile) };
        svf.file = FileProtocol {
            revision: 0x0001_0000_0000_0001,
            open: file_open, close: file_close, delete: file_delete,
            read: file_read_file, write: file_write_ro,
            get_position: file_get_position, set_position: file_set_position,
            get_info: file_get_info, set_info: file_set_info_ro, flush: file_flush,
        };
        svf.ctx = ctx as *const IsoFsCtx;
        svf.is_dir = false;
        svf.extent_lba = 0;
        svf.extent_size = ctx.premount_cpio_size as u32;
        svf.position = 0;
        svf.needs_grub_patch = false;
        svf.patched_buf = core::ptr::null_mut();
        svf.patched_size = 0; svf.patched = false;
        svf.is_synthetic = true;
        svf.synthetic_buf = ctx.premount_cpio_buf;
        svf.synthetic_size = ctx.premount_cpio_size;
        svf.synthetic_injected = false;
        *new_handle = ptr as *mut FileProtocol;
        let _ = open_mode;
        return EFI_SUCCESS;
    }

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

    let fp = match alloc_virtual_file_internal(ctx, child_lba, child_size, is_dir, is_cfg) {
        Some(p) => p,
        None => return EFI_OUT_OF_RESOURCES,
    };
    let _ = open_mode;

    // Eagerly patch .cfg
    // NOTE: We skip patching here because IsoLocation is not available
    // at SFS open time. The BlockIO patching flow (patch_grub_cfg_blockio)
    // handles the real location-dependent patching when the full ISO
    // context is available. This prevents partial/inconsistent patches
    // where findiso= or choosable.iso_offset= would be left empty.
    if is_cfg {
        let vf2 = unsafe { &mut *(fp as *mut VirtualFile) };
        // Mark as needs_grub_patch=true but don't eagerly patch here.
        // GRUB will read via BlockIO where the full patch can occur.
        vf2.needs_grub_patch = true;
    }

    *new_handle = fp;
    EFI_SUCCESS
}

fn alloc_virtual_file_internal(ctx: &IsoFsCtx, lba: u32, size: u32, is_dir: bool, needs_grub_patch: bool) -> Option<*mut FileProtocol> {
    let bs = unsafe { &mut *ctx.bs };
    let mut ptr: *mut c_void = core::ptr::null_mut();
    if unsafe { (bs.allocate_pool)(crate::protocol::MemoryType::EfiLoaderData, core::mem::size_of::<VirtualFile>(), &mut ptr) } != EFI_SUCCESS || ptr.is_null() { return None; }
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
    vf.patched_buf = core::ptr::null_mut(); vf.patched_size = 0; vf.patched = false;
    vf.is_synthetic = false;
    vf.synthetic_buf = core::ptr::null_mut(); vf.synthetic_size = 0;
    vf.synthetic_injected = false;
    Some(ptr as *mut FileProtocol)
}

unsafe extern "efiapi" fn file_close(this: *mut FileProtocol) -> usize {
    let vf = unsafe { &mut *(this as *mut VirtualFile) };
    let ctx = unsafe { &*vf.ctx };
    let bs = unsafe { &mut *ctx.bs };
    if !vf.patched_buf.is_null() { unsafe { (bs.free_pool)(vf.patched_buf as *mut c_void) }; }
    unsafe { (bs.free_pool)(this as *mut c_void) }; EFI_SUCCESS
}

unsafe extern "efiapi" fn file_delete(this: *mut FileProtocol) -> usize { let _ = file_close(this); EFI_WRITE_PROTECTED }

unsafe extern "efiapi" fn file_read_file(this: *mut FileProtocol, buffer_size: *mut usize, buffer: *mut c_void) -> usize {
    if buffer_size.is_null() || buffer.is_null() { return EFI_INVALID_PARAMETER; }
    let vf = unsafe { &mut *(this as *mut VirtualFile) };
    if vf.is_dir { return EFI_UNSUPPORTED; }
    let ctx = unsafe { &*vf.ctx };
    let size = unsafe { *buffer_size };
    if size == 0 { return EFI_SUCCESS; }

    // Synthetic file (premount cpio from memory)
    if vf.is_synthetic && !vf.synthetic_buf.is_null() {
        // Guard against position overflow (set_position may exceed size)
        if vf.position >= vf.synthetic_size as u64 {
            unsafe { *buffer_size = 0; }
            return EFI_SUCCESS;
        }
        let avail = (vf.synthetic_size as u64 - vf.position) as usize;
        let to_copy = size.min(avail);
        if to_copy == 0 { unsafe { *buffer_size = 0; } return EFI_SUCCESS; }
        let src = unsafe { core::slice::from_raw_parts(vf.synthetic_buf.add(vf.position as usize), to_copy) };
        let dst = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, to_copy) };
        dst.copy_from_slice(src);
        vf.position += to_copy as u64;
        unsafe { *buffer_size = to_copy; }
        return EFI_SUCCESS;
    }

    // Patched file (grub.cfg)
    if vf.patched && !vf.patched_buf.is_null() {
        // Guard against position overflow (set_position may exceed size)
        if vf.position >= vf.patched_size {
            unsafe { *buffer_size = 0; }
            return EFI_SUCCESS;
        }
        let avail = (vf.patched_size - vf.position) as usize;
        let to_copy = size.min(avail);
        if to_copy == 0 { unsafe { *buffer_size = 0; } return EFI_SUCCESS; }
        let src = unsafe { core::slice::from_raw_parts(vf.patched_buf.add(vf.position as usize), to_copy) };
        let dst = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, to_copy) };
        dst.copy_from_slice(src);
        vf.position += to_copy as u64;
        unsafe { *buffer_size = to_copy; }
        return EFI_SUCCESS;
    }

    // Normal read from ISO extent
    let dst = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, size) };
    let read = read_extent_data(ctx, vf.extent_lba, vf.extent_size, vf.position, dst);
    vf.position += read as u64;
    unsafe { *buffer_size = read; }
    EFI_SUCCESS
}

unsafe extern "efiapi" fn file_read_dir(this: *mut FileProtocol, buffer_size: *mut usize, buffer: *mut c_void) -> usize {
    if buffer_size.is_null() || buffer.is_null() { return EFI_INVALID_PARAMETER; }
    let vf = unsafe { &mut *(this as *mut VirtualFile) };
    if !vf.is_dir { return EFI_UNSUPPORTED; }
    let ctx = unsafe { &*vf.ctx };
    let buf_sz = unsafe { *buffer_size };
    let dst = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, buf_sz) };
    let dir_size = vf.extent_size;
    let total_sectors = ((dir_size as u64 + 2047) / 2048) as u32;
    let mut scratch = [0u8; 2048]; let mut dst_off = 0usize; let mut finished = false;
    if vf.position == 0 {
        vf.synthetic_injected = false;
    }
    let mut sector_idx = (vf.position >> 16) as u32; let mut byte_offset = (vf.position & 0xFFFF) as usize;

    // Phase 2: after all real directory entries, inject synthetic premount entry.
    let phase2 = sector_idx >= total_sectors && (vf.position & 0x10000) != 0 && !vf.synthetic_injected;
    if phase2 {
        let have_cpio = ctx.premount_cpio_size > 0 && !ctx.premount_cpio_buf.is_null();
        if have_cpio {
            let synthetic_name: &[u8] = if ctx.premount_target_name_len > 0 {
                &ctx.premount_target_name[..ctx.premount_target_name_len]
            } else {
                b"PREMOUNT.CPIO"
            };
            let raw_size = core::mem::size_of::<EfiFileInfo>() + (synthetic_name.len() + 1) * 2;
            let required_size = (raw_size + 7) & !7;
            if dst_off + required_size <= buf_sz {
                let info = EfiFileInfo {
                    size: required_size as u64,
                    file_size: ctx.premount_cpio_size as u64,
                    physical_size: ctx.premount_cpio_size as u64,
                    create_time: unsafe { core::mem::zeroed() },
                    last_access_time: unsafe { core::mem::zeroed() },
                    modification_time: unsafe { core::mem::zeroed() },
                    attribute: 0,
                };
                let ib = unsafe {
                    core::slice::from_raw_parts(&info as *const EfiFileInfo as *const u8, core::mem::size_of::<EfiFileInfo>())
                };
                dst[dst_off..dst_off + ib.len()].copy_from_slice(ib);
                dst_off += ib.len();
                for j in 0..synthetic_name.len() {
                    let ch = synthetic_name[j] as u16;
                    dst[dst_off] = ch as u8;
                    dst[dst_off + 1] = (ch >> 8) as u8;
                    dst_off += 2;
                }
                dst[dst_off] = 0; dst[dst_off + 1] = 0; dst_off += 2;
                for _ in 0..(required_size - raw_size) { dst[dst_off] = 0; dst_off += 1; }
                vf.synthetic_injected = true;
                vf.position = ((total_sectors as u64) << 16) | 0x20000;
                unsafe { *buffer_size = dst_off; }
                return EFI_SUCCESS;
            } else if dst_off == 0 {
                unsafe { *buffer_size = required_size; }
                return crate::protocol::EFI_BUFFER_TOO_SMALL;
            }
            // buffer full, retry next call
            unsafe { *buffer_size = dst_off; }
            return EFI_SUCCESS;
        }
        // no cpio; mark EOD
        vf.position = ((total_sectors as u64) << 16) | 0x20000;
        unsafe { *buffer_size = 0; }
        return EFI_SUCCESS;
    }

    if sector_idx >= total_sectors { unsafe { *buffer_size = 0; } return EFI_SUCCESS; }
    if !read_iso_sector(ctx, vf.extent_lba + sector_idx, &mut scratch) { return EFI_DEVICE_ERROR; }
    while !finished && dst_off + core::mem::size_of::<EfiFileInfo>() + 2 <= buf_sz {
        if byte_offset + 34 > 2048 || (byte_offset > 0 && scratch[byte_offset] == 0) {
            sector_idx += 1; byte_offset = 0;
            if sector_idx >= total_sectors { finished = true; break; }
            if !read_iso_sector(ctx, vf.extent_lba + sector_idx, &mut scratch) { vf.position = ((sector_idx as u64) << 16) | (byte_offset as u64); return EFI_DEVICE_ERROR; }
            if scratch[0] == 0 { finished = true; break; }
        }
        let record_len = scratch[byte_offset] as usize;
        if record_len == 0 { sector_idx += 1; byte_offset = 0;
            if sector_idx >= total_sectors { finished = true; break; }
            if !read_iso_sector(ctx, vf.extent_lba + sector_idx, &mut scratch) { vf.position = ((sector_idx as u64) << 16) | (byte_offset as u64); return EFI_DEVICE_ERROR; }
            continue;
        }
        if record_len < 34 || byte_offset + record_len > 2048 { finished = true; break; }
        let name_len = scratch[byte_offset + 32] as usize; let name_offset = byte_offset + 33;
        if 33 + name_len > record_len || name_offset + name_len > 2048 { finished = true; break; }
        let iso_name = &scratch[name_offset..name_offset + name_len];
        let ef_len = iso_name_effective_len(iso_name, name_len);
        let child_size = u32::from_le_bytes(scratch[byte_offset + 10..byte_offset + 14].try_into().unwrap());
        let flags = scratch[byte_offset + 25]; let is_dir = flags & 0x02 != 0;
        let raw_size = core::mem::size_of::<EfiFileInfo>() + (ef_len + 1) * 2;
        let required_size = (raw_size + 7) & !7;
        if dst_off + required_size > buf_sz { if dst_off == 0 { unsafe { *buffer_size = required_size; } return crate::protocol::EFI_BUFFER_TOO_SMALL; } break; }
        let ct: EfiTime = unsafe { core::mem::zeroed() }; let lat: EfiTime = unsafe { core::mem::zeroed() }; let mt: EfiTime = unsafe { core::mem::zeroed() };
        let info = EfiFileInfo { size: required_size as u64, file_size: child_size as u64, physical_size: child_size as u64, create_time: ct, last_access_time: lat, modification_time: mt, attribute: if is_dir { 1 } else { 0 } };
        let ib = unsafe { core::slice::from_raw_parts(&info as *const EfiFileInfo as *const u8, core::mem::size_of::<EfiFileInfo>()) };
        dst[dst_off..dst_off + ib.len()].copy_from_slice(ib); dst_off += ib.len();
        for j in 0..ef_len { let ch = iso_name[j] as u16; dst[dst_off] = ch as u8; dst[dst_off+1] = (ch>>8) as u8; dst_off += 2; }
        dst[dst_off] = 0; dst[dst_off+1] = 0; dst_off += 2;
        for _ in 0..(required_size - raw_size) { dst[dst_off] = 0; dst_off += 1; }
        byte_offset += record_len;
    }
    if finished {
        // Transition to phase 2: inject synthetic entry on next call
        vf.position = ((total_sectors as u64) << 16) | 0x10000;
    } else {
        vf.position = ((sector_idx as u64) << 16) | (byte_offset as u64);
    }
    unsafe { *buffer_size = dst_off; }
    EFI_SUCCESS
}

unsafe extern "efiapi" fn file_write_ro(_this: *mut FileProtocol, _buffer_size: *mut usize, _buffer: *mut c_void) -> usize { EFI_WRITE_PROTECTED }

unsafe extern "efiapi" fn file_get_position(this: *mut FileProtocol, position: *mut u64) -> usize {
    if position.is_null() { return EFI_INVALID_PARAMETER; }
    let vf = unsafe { &*(this as *const VirtualFile) };
    unsafe { *position = vf.position; } EFI_SUCCESS
}

unsafe extern "efiapi" fn file_set_position(this: *mut FileProtocol, position: u64) -> usize {
    let vf = unsafe { &mut *(this as *mut VirtualFile) };
    if vf.is_dir { if position != 0 { return EFI_UNSUPPORTED; } vf.position = 0; return EFI_SUCCESS; }
    let max: u64 = if vf.patched { vf.patched_size }
        else if vf.is_synthetic { vf.synthetic_size as u64 }
        else { vf.extent_size as u64 };
    vf.position = if position > max { max } else { position };
    EFI_SUCCESS
}

unsafe extern "efiapi" fn file_get_info(this: *mut FileProtocol, information_type: *const Guid, buffer_size: *mut usize, buffer: *mut c_void) -> usize {
    if information_type.is_null() || buffer_size.is_null() { return EFI_INVALID_PARAMETER; }
    let info_type = unsafe { &*information_type };
    if info_type.d1 != FILE_INFO_GUID.d1 || info_type.d2 != FILE_INFO_GUID.d2 || info_type.d3 != FILE_INFO_GUID.d3 || info_type.d4 != FILE_INFO_GUID.d4 { return EFI_UNSUPPORTED; }
    let vf = unsafe { &*(this as *const VirtualFile) };
    let required_size = core::mem::size_of::<EfiFileInfo>() + 2;
    let buf_sz = unsafe { *buffer_size };
    if buf_sz < required_size { unsafe { *buffer_size = required_size; } return crate::protocol::EFI_BUFFER_TOO_SMALL; }
    if buffer.is_null() { return EFI_SUCCESS; }
    let create_time: EfiTime = unsafe { core::mem::zeroed() };
    let last_access_time: EfiTime = unsafe { core::mem::zeroed() };
    let modification_time: EfiTime = unsafe { core::mem::zeroed() };
    let fs: u64 = if vf.patched { vf.patched_size }
        else if vf.is_synthetic { vf.synthetic_size as u64 }
        else { vf.extent_size as u64 };
    let info = EfiFileInfo { size: required_size as u64, file_size: fs, physical_size: fs, create_time, last_access_time, modification_time, attribute: if vf.is_dir { 0x0000_0000_0000_0001 } else { 0 } };
    let dst = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, buf_sz) };
    let ib = unsafe { core::slice::from_raw_parts(&info as *const EfiFileInfo as *const u8, core::mem::size_of::<EfiFileInfo>()) };
    dst[..ib.len()].copy_from_slice(ib);
    dst[ib.len()] = 0; dst[ib.len()+1] = 0;
    unsafe { *buffer_size = required_size; }
    EFI_SUCCESS
}

unsafe extern "efiapi" fn file_set_info_ro(_this: *mut FileProtocol, _information_type: *const Guid, _buffer_size: usize, _buffer: *mut c_void) -> usize { EFI_WRITE_PROTECTED }

unsafe extern "efiapi" fn file_flush(_this: *mut FileProtocol) -> usize { EFI_SUCCESS }

// ═══════════════════════════════════════════════════════════════════════════
//  Public constructor
// ═══════════════════════════════════════════════════════════════════════════

pub fn create_iso_fs(
    bs: &mut BootServices, st: *mut SystemTable,
    real_bio_ptr: *mut BlockIoProtocol, real_media_id: u32,
    iso_lba: u64, iso_size_bytes: u64, iso_name: &[u8],
    live_media_uuid: &[u8; 10],
    premount_cpio_buf: *mut u8, premount_cpio_size: usize,
) -> *mut IsoFsInstance {
    let mut ptr: *mut c_void = core::ptr::null_mut();
    if unsafe { (bs.allocate_pool)(crate::protocol::MemoryType::EfiLoaderData, core::mem::size_of::<IsoFsInstance>(), &mut ptr) } != EFI_SUCCESS || ptr.is_null() { return core::ptr::null_mut(); }
    let instance = unsafe { &mut *(ptr as *mut IsoFsInstance) };
    let name_len = iso_name.len().min(127);
    let mut name_arr = [0u8; 128]; name_arr[..name_len].copy_from_slice(&iso_name[..name_len]);
    let mut uuid_arr = [0u8; 10]; uuid_arr.copy_from_slice(&live_media_uuid[..10]);
    instance.ctx = IsoFsCtx {
        real_bio_ptr, real_media_id, iso_lba, iso_size_bytes,
        root_lba: 0, root_size: 0, bs: bs as *mut BootServices, st,
        iso_name: name_arr, iso_name_len: name_len,
        live_media_uuid: uuid_arr,
        premount_cpio_buf, premount_cpio_size,
        premount_target_name: [0u8; 16],
        premount_target_name_len: 0,
        boot_kind: crate::boot_kind::BootKind::Unknown,
        bootloader_type: crate::boot_kind::BootloaderType::Grub,
        vbio_ptr: core::ptr::null_mut(),
    };
    if let Some((rlb, rsz)) = parse_pvd(&instance.ctx) { instance.ctx.root_lba = rlb; instance.ctx.root_size = rsz; }
    else { unsafe { (bs.free_pool)(ptr); } return core::ptr::null_mut(); }
    instance.sfs = SimpleFileSystemProtocol { revision: 0x0001_0000_0000_0001, open_volume: sfs_open_volume };
    ptr as *mut IsoFsInstance
}