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
    EFI_UNSUPPORTED, EFI_BAD_BUFFER_SIZE,
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
}

#[repr(C)]
pub struct IsoFsInstance {
    pub sfs: SimpleFileSystemProtocol,
    ctx: IsoFsCtx,
}

/// Per-open-file (or directory) state
#[repr(C)]
pub struct VirtualFile {
    pub file: FileProtocol,
    ctx: *const IsoFsCtx,
    is_dir: bool,
    extent_lba: u32,   // ISO 2048-byte sector
    extent_size: u32,  // bytes
    position: u64,      // current read offset
    /// If true, file_read_file will inject iso-scan/filename= into the buffer
    needs_grub_patch: bool,
    /// Pool-allocated patched copy of the file (only for grub.cfg)
    patched_buf: *mut u8,
    /// Size of the patched buffer
    patched_size: u64,
    /// Whether the patch has been applied
    patched: bool,
}

// ═══════════════════════════════════════════════════════════════════════════
//  ISO9660 low-level read helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Read one ISO 2048-byte sector (4 × 512-byte disk sectors) into buf.
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

/// Read raw bytes from an ISO extent into a buffer.
/// `lba` is in ISO 2048-byte sectors. `offset` + `len` must be within `extent_size`.
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

/// Strip ";1" version suffix from ISO9660 name, return effective length.
fn iso_name_effective_len(name: &[u8], name_len: usize) -> usize {
    if name_len >= 2 && name[name_len - 2] == b';' {
        name_len - 2
    } else {
        name_len
    }
}

/// Case-insensitive UCS-2 to ISO9660 name comparison.
fn match_iso_name(iso_name: &[u8], iso_name_len: usize, ucs2_name: &[u16]) -> bool {
    let eff_len = iso_name_effective_len(iso_name, iso_name_len);
    // Convert UCS-2 to ASCII for comparison (dropping high byte)
    let name_bytes: [u8; 256] = {
        let mut arr = [0u8; 256];
        let mut i = 0;
        while i < ucs2_name.len() && i < 256 {
            let cp = ucs2_name[i];
            if cp == 0 {
                break;
            }
            arr[i] = if cp < 0x80 { cp as u8 } else { b'?' };
            i += 1;
        }
        arr
    };
    let name_len = ucs2_name.iter().position(|&c| c == 0).unwrap_or(ucs2_name.len());
    let name_slice = &name_bytes[..name_len.min(255)];

    if eff_len != name_slice.len() {
        return false;
    }
    for i in 0..eff_len {
        if iso_name[i].to_ascii_uppercase() != name_slice[i].to_ascii_uppercase() {
            return false;
        }
    }
    true
}

/// Search an ISO9660 directory extent for a child entry.
/// Returns (child_extent_lba, child_size_bytes, is_directory) or None.
fn lookup_in_dir(
    ctx: &IsoFsCtx,
    dir_lba: u32,
    dir_size: u32,
    name: &[u16],
) -> Option<(u32, u32, bool)> {
    let total_sectors = ((dir_size as u64 + 2047) / 2048) as u32;
    let mut scratch = [0u8; 2048];

    // Print the search target once
    if !ctx.st.is_null() {
        let st_ref = unsafe { &mut *ctx.st };
        print_raw(st_ref, b"[SFS]   lookup '");
        let mut tmp = [0u8; 64];
        let mut n = 0;
        for &c in name {
            if c == 0 || n >= 63 { break; }
            tmp[n] = if c < 0x80 { c as u8 } else { b'?' };
            n += 1;
        }
        print_raw(st_ref, &tmp[..n]);
        print_raw(st_ref, b"' in dir LBA=\0");
        // simple number print
        let mut lbabuf = [b'0'; 10];
        let mut val = dir_lba as u64;
        let mut p = 9usize;
        loop {
            lbabuf[p] = b'0' + (val % 10) as u8;
            val /= 10;
            if val == 0 { break; }
            if p == 0 { break; }
            p -= 1;
        }
        print_raw(st_ref, &lbabuf[p..]);
        print_raw(st_ref, b" size=\0");
        let mut szbuf = [b'0'; 10];
        val = dir_size as u64;
        p = 9usize;
        loop {
            szbuf[p] = b'0' + (val % 10) as u8;
            val /= 10;
            if val == 0 { break; }
            if p == 0 { break; }
            p -= 1;
        }
        print_raw(st_ref, &szbuf[p..]);
        print_raw(st_ref, b"\r\n\0");
    }

    for s in 0..total_sectors {
        if !read_iso_sector(ctx, dir_lba + s, &mut scratch) {
            return None;
        }
        let mut offset: usize = 0;
        while offset + 34 <= 2048 {
            let record_len = scratch[offset] as usize;
            if record_len == 0 {
                // skip to next sector
                break;
            }
            if record_len < 34 || offset + record_len > 2048 {
                break;
            }
            let name_len = scratch[offset + 32] as usize;
            let name_offset = offset + 33;
            if name_offset + name_len > 2048 {
                break;
            }
            let iso_name = &scratch[name_offset..name_offset + name_len];

            // Log every entry name
            if !ctx.st.is_null() {
                let st_ref = unsafe { &mut *ctx.st };
                print_raw(st_ref, b"[SFS]     entry '");
                // Copy iso_name up to name_len, strip ;1 for display
                let mut ebuf = [0u8; 64];
                let mut elen = 0;
                for j in 0..name_len.min(63) {
                    let b = iso_name[j];
                    if b == 0 { break; }
                    ebuf[j] = if b < 0x80 { b } else { b'?' };
                    elen += 1;
                }
                let eff = if elen >= 2 && ebuf[elen - 2] == b';' { elen - 2 } else { elen };
                print_raw(st_ref, &ebuf[..eff]);
                print_raw(st_ref, b"'\r\n\0");
            }

            if match_iso_name(iso_name, name_len, name) {
                // ".." entry (parent dir) has special handling:
                // Extent of 0 = parent is root (same extent)
                let child_extent = u32::from_le_bytes(
                    scratch[offset + 2..offset + 6].try_into().unwrap(),
                );
                let child_size = u32::from_le_bytes(
                    scratch[offset + 10..offset + 14].try_into().unwrap(),
                );
                let flags = scratch[offset + 25];
                let is_dir = flags & 0x02 != 0;
                if !ctx.st.is_null() {
                    let st_ref = unsafe { &mut *ctx.st };
                    print_raw(st_ref, b"[SFS]     -> MATCH\r\n\0");
                }
                return Some((child_extent, child_size, is_dir));
            }
            offset += record_len;
        }
    }
    None
}

/// Parse Primary Volume Descriptor (ISO sector 16) to get root dir record.
fn parse_pvd(ctx: &IsoFsCtx) -> Option<(u32, u32)> {
    let mut pvd = [0u8; 2048];
    if !read_iso_sector(ctx, 16, &mut pvd) {
        return None;
    }
    // Check descriptor type == 1 and identifier "CD001"
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        // Try UDF AVDP at sector 256 as fallback
        // (Not fully implemented; return None for now)
        return None;
    }
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
    let instance = &*(this as *const IsoFsInstance);
    let ctx = &instance.ctx;

    // Debug: log that SFS open_volume was called
    if !ctx.st.is_null() {
        let st_ref = unsafe { &mut *ctx.st };
        print_raw(st_ref, b"[SFS] open_volume called\r\n\0");
    }

    // Allocate a VirtualFile for the root directory
    let bs = unsafe { &mut *ctx.bs };
    let mut ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.allocate_pool)(
            crate::protocol::MemoryType::EfiLoaderData,
            core::mem::size_of::<VirtualFile>(),
            &mut ptr,
        )
    };
    if status != EFI_SUCCESS || ptr.is_null() {
        return EFI_OUT_OF_RESOURCES;
    }

    let vf = unsafe { &mut *(ptr as *mut VirtualFile) };
    vf.file = FileProtocol {
        revision: 0x0001_0000_0000_0001,
        open: file_open,
        close: file_close,
        delete: file_delete,
        read: file_read_dir,  // root is a directory
        write: file_write_ro,
        get_position: file_get_position,
        set_position: file_set_position,
        get_info: file_get_info,
        set_info: file_set_info_ro,
        flush: file_flush,
    };
    vf.ctx = ctx as *const IsoFsCtx;
    vf.is_dir = true;
    vf.extent_lba = ctx.root_lba;
    vf.extent_size = ctx.root_size;
    vf.position = 0;

    *root = ptr as *mut FileProtocol;
    EFI_SUCCESS
}

// ═══════════════════════════════════════════════════════════════════════════
//  FileProtocol implementations
// ═══════════════════════════════════════════════════════════════════════════

/// Walk the ISO directory tree to resolve a multi-component UCS-2 path.
/// Returns (extent_lba, extent_size, is_dir) for the final component.
/// `start_lba` and `start_size` define the starting directory extent.
fn resolve_path(
    ctx: &IsoFsCtx,
    start_lba: u32,
    start_size: u32,
    path: &[u16],
) -> Option<(u32, u32, bool)> {
    if path.is_empty() || (path.len() == 1 && path[0] == b'\\' as u16) {
        // Empty path or just "\" → return the starting directory itself
        return Some((start_lba, start_size, true));
    }

    // Strip DevicePath text prefix like "CDROM(0x0)\" or "HD(1,GPT,...)\"
    // UEFI applications may pass the full DevicePath text as a file path.
    // The device text ends with ")", followed by "\" and the filesystem path.
    let mut pos = 0usize;
    let mut last_paren_backslash: Option<usize> = None;
    let mut i = 0;
    while i + 1 < path.len() {
        if path[i] == b')' as u16 && path[i + 1] == b'\\' as u16 {
            last_paren_backslash = Some(i + 1); // position of the backslash after )
        }
        i += 1;
    }
    if let Some(start) = last_paren_backslash {
        // Start from the backslash after the device prefix
        pos = start;
    }

    // Skip leading backslash(es)
    while pos < path.len() && path[pos] == b'\\' as u16 {
        pos += 1;
    }
    if pos >= path.len() {
        return Some((start_lba, start_size, true));
    }

    let mut cur_lba = start_lba;
    let mut cur_size = start_size;

    // Walk component by component
    while pos < path.len() {
        // Find the end of this component
        let comp_start = pos;
        while pos < path.len() && path[pos] != b'\\' as u16 {
            pos += 1;
        }
        let component = &path[comp_start..pos];

        // Look up this component in the current directory
        let (child_lba, child_size, is_dir) =
            lookup_in_dir(ctx, cur_lba, cur_size, component)?;

        // Skip the backslash after the component
        if pos < path.len() && path[pos] == b'\\' as u16 {
            pos += 1;
        }

        // If there are more components, this must be a directory
        let has_more = pos < path.len() && {
            // Check if remaining is non-empty (not just trailing backslash)
            let rem = &path[pos..];
            !rem.is_empty() && rem[0] != 0
        };

        if has_more && !is_dir {
            return None; // intermediate component must be a directory
        }

        cur_lba = child_lba;
        cur_size = child_size;

        if !has_more {
            return Some((cur_lba, cur_size, is_dir));
        }
    }

    Some((cur_lba, cur_size, false))
}

/// Allocate and initialize a VirtualFile from ISO extent info.
fn alloc_virtual_file(
    ctx: &IsoFsCtx,
    lba: u32,
    size: u32,
    is_dir: bool,
    needs_grub_patch: bool,
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
    if status != EFI_SUCCESS || ptr.is_null() {
        return None;
    }

    let vf = unsafe { &mut *(ptr as *mut VirtualFile) };
    vf.file = FileProtocol {
        revision: 0x0001_0000_0000_0001,
        open: file_open,
        close: file_close,
        delete: file_delete,
        read: if is_dir { file_read_dir } else { file_read_file },
        write: file_write_ro,
        get_position: file_get_position,
        set_position: file_set_position,
        get_info: file_get_info,
        set_info: file_set_info_ro,
        flush: file_flush,
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

    Some(ptr as *mut FileProtocol)
}

/// Open a child file or subdirectory within a directory.
/// Handles multi-component paths (e.g. `\EFI\BOOT\grubx64.efi`).
unsafe extern "efiapi" fn file_open(
    this: *mut FileProtocol,
    new_handle: *mut *mut FileProtocol,
    file_name: *const u16,
    open_mode: u64,
    _attributes: u64,
) -> usize {
    if file_name.is_null() || new_handle.is_null() {
        return EFI_INVALID_PARAMETER;
    }

    let vf = unsafe { &*(this as *const VirtualFile) };
    if !vf.is_dir {
        return EFI_UNSUPPORTED;
    }

    let ctx = unsafe { &*vf.ctx };

    // Debug: log the OPEN request
    if !ctx.st.is_null() {
        let st_ref = unsafe { &mut *ctx.st };
        print_raw(st_ref, b"[SFS] OPEN: \0");
        // Print up to first 64 UCS-2 chars as ASCII
        let mut name_buf = [0u8; 128];
        let mut name_pos = 0usize;
        let p = file_name;
        while name_pos < 127 {
            let ch = unsafe { *p.add(name_pos) };
            if ch == 0 {
                break;
            }
            name_buf[name_pos] = if ch < 0x80 { ch as u8 } else { b'?' };
            name_pos += 1;
        }
        name_buf[name_pos] = b'\0';
        if name_pos > 0 {
            let msg_slice: &[u8] = &name_buf[..name_pos];
            print_raw(st_ref, msg_slice);
        }
        print_raw(st_ref, b"\r\n\0");
    }

    // Convert UCS-2 file name to a slice (null-terminated)
    let name_slice = unsafe {
        let mut len = 0usize;
        while *file_name.add(len) != 0 {
            len += 1;
            if len > 256 {
                return EFI_INVALID_PARAMETER;
            }
        }
        core::slice::from_raw_parts(file_name, len)
    };

    // Resolve multi-component path
    let resolved = resolve_path(ctx, vf.extent_lba, vf.extent_size, name_slice);
    let (child_lba, child_size, is_dir) = match resolved {
        Some(v) => {
            let (lba, sz, dir) = v;
            if !ctx.st.is_null() {
                let st_ref = unsafe { &mut *ctx.st };
                print_raw(st_ref, b"[SFS]   -> OK (LBA=\0");
                let mut lbabuf = [b'0'; 16];
                let mut lba_val = lba as u64;
                let mut pos = 15usize;
                loop {
                    lbabuf[pos] = b'0' + (lba_val % 10) as u8;
                    lba_val /= 10;
                    if lba_val == 0 || pos == 0 {
                        break;
                    }
                    pos -= 1;
                }
                print_raw(st_ref, &lbabuf[pos..]);
                print_raw(st_ref, b", size=\0");
                let mut szbuf = [b'0'; 16];
                let mut sz_val = sz as u64;
                pos = 15usize;
                loop {
                    szbuf[pos] = b'0' + (sz_val % 10) as u8;
                    sz_val /= 10;
                    if sz_val == 0 || pos == 0 {
                        break;
                    }
                    pos -= 1;
                }
                print_raw(st_ref, &szbuf[pos..]);
                print_raw(st_ref, b")\r\n\0");
            }
            (lba, sz, dir)
        }
        None => {
            if !ctx.st.is_null() {
                let st_ref = unsafe { &mut *ctx.st };
                print_raw(st_ref, b"[SFS]   -> NOT FOUND\r\n\0");
            }
            return EFI_NOT_FOUND;
        }
    };

    // Determine if this is grub.cfg (needs iso-scan/filename patch)
    let is_grub_cfg = if !is_dir && ctx.iso_name_len > 0 && name_slice.len() >= 8 {
        let n = name_slice.len();
        let s: [u8; 8] = [
            name_slice[n - 8] as u8,
            name_slice[n - 7] as u8,
            name_slice[n - 6] as u8,
            name_slice[n - 5] as u8,
            name_slice[n - 4] as u8,
            name_slice[n - 3] as u8,
            name_slice[n - 2] as u8,
            name_slice[n - 1] as u8,
        ];
        // case-insensitive comparison with "grub.cfg"
        (s[0] | 0x20) == b'g' && (s[1] | 0x20) == b'r' && (s[2] | 0x20) == b'u'
            && (s[3] | 0x20) == b'b' && s[4] == b'.' && (s[5] | 0x20) == b'c'
            && (s[6] | 0x20) == b'f' && (s[7] | 0x20) == b'g'
    } else {
        false
    };

    if is_grub_cfg && !ctx.st.is_null() {
        let st_ref = unsafe { &mut *ctx.st };
        print_raw(st_ref, b"[SFS]   -> grub.cfg detected, will patch with iso-scan/filename=");
        print_raw(st_ref, &ctx.iso_name[..ctx.iso_name_len]);
        print_raw(st_ref, b"\r\n\0");
    }

    // Allocate and initialize the VirtualFile
    let fp = match alloc_virtual_file(ctx, child_lba, child_size, is_dir, is_grub_cfg) {
        Some(p) => p,
        None => return EFI_OUT_OF_RESOURCES,
    };

    // ── If this is grub.cfg, eagerly generate the patch now ───
    // GRUB calls GetInfo before Read, so patched size must be known early.
    if is_grub_cfg {
        let vf_patch = unsafe { &mut *(fp as *mut VirtualFile) };
        if !vf_patch.patched {
            // Read original content
            let orig_size = vf_patch.extent_size as usize;
            let bs = unsafe { &mut *ctx.bs };
            let mut tmp_ptr: *mut c_void = core::ptr::null_mut();
            let tmp_status = unsafe {
                (bs.allocate_pool)(
                    crate::protocol::MemoryType::EfiLoaderData,
                    orig_size,
                    &mut tmp_ptr,
                )
            };
            if tmp_status != EFI_SUCCESS || tmp_ptr.is_null() {
                // Continue without patch
            } else {
                let tmp_buf = unsafe { core::slice::from_raw_parts_mut(tmp_ptr as *mut u8, orig_size) };
                let mut total_read = 0usize;
                while total_read < orig_size {
                    let rem = (orig_size - total_read).min(2048);
                    let r = read_extent_data(ctx, child_lba, child_size, total_read as u64,
                        &mut tmp_buf[total_read..total_read + rem]);
                    if r == 0 { break; }
                    total_read += r;
                }

                let patch = crate::strategy::patch_grub_cfg(ctx, &tmp_buf[..total_read], ctx.bs);
                if let Some(p) = patch {
                    vf_patch.patched_buf = p.buf;
                    vf_patch.patched_size = p.size as u64;
                    vf_patch.patched = true;

                    if !ctx.st.is_null() {
                        let st_ref = unsafe { &mut *ctx.st };
                        print_raw(st_ref, b"[SFS] grub.cfg patched eagerly: ");
                        let mut nb = [0u8; 16];
                        let mut nv = p.size as u64;
                        let mut np = 15usize;
                        loop {
                            nb[np] = b'0' + (nv % 10) as u8;
                            nv /= 10;
                            if nv == 0 || np == 0 { break; }
                            np -= 1;
                        }
                        print_raw(st_ref, &nb[np..]);
                        print_raw(st_ref, b" bytes\r\n\0");
                    }
                }
                unsafe { (bs.free_pool)(tmp_ptr); }
            }
        }
    }

    let _ = open_mode;
    *new_handle = fp;
    EFI_SUCCESS
}

/// Close a file handle and free its pool allocation (and patched buffer if any).
unsafe extern "efiapi" fn file_close(this: *mut FileProtocol) -> usize {
    let vf = unsafe { &mut *(this as *mut VirtualFile) };
    let ctx = unsafe { &*vf.ctx };
    let bs = unsafe { &mut *ctx.bs };
    // Free patched buffer if allocated
    if !vf.patched_buf.is_null() {
        unsafe { (bs.free_pool)(vf.patched_buf as *mut c_void) };
        vf.patched_buf = core::ptr::null_mut();
    }
    unsafe { (bs.free_pool)(this as *mut c_void) };
    EFI_SUCCESS
}

/// Delete — not supported (read-only)
unsafe extern "efiapi" fn file_delete(_this: *mut FileProtocol) -> usize {
    EFI_WRITE_PROTECTED
}

/// Read from a regular file. For grub.cfg, injects iso-scan/filename= on the
/// first read so initramfs can find the ISO on the real USB disk.
unsafe extern "efiapi" fn file_read_file(
    this: *mut FileProtocol,
    buffer_size: *mut usize,
    buffer: *mut c_void,
) -> usize {
    if buffer_size.is_null() || buffer.is_null() {
        return EFI_INVALID_PARAMETER;
    }

    let vf = unsafe { &mut *(this as *mut VirtualFile) };
    if vf.is_dir {
        return EFI_UNSUPPORTED;
    }

    let ctx = unsafe { &*vf.ctx };
    let size = unsafe { *buffer_size };
    if size == 0 {
        return EFI_SUCCESS;
    }

    // Serve from patched buffer if available (patched eagerly in file_open)
    if vf.patched && !vf.patched_buf.is_null() {
        let avail = (vf.patched_size - vf.position as u64) as usize;
        let to_copy = size.min(avail);
        if to_copy == 0 {
            unsafe { *buffer_size = 0; }
            return EFI_SUCCESS;
        }
        let src = unsafe {
            core::slice::from_raw_parts(
                vf.patched_buf.add(vf.position as usize),
                to_copy,
            )
        };
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

/// Read from a directory — not supported (return error).
unsafe extern "efiapi" fn file_read_dir(
    _this: *mut FileProtocol,
    _buffer_size: *mut usize,
    _buffer: *mut c_void,
) -> usize {
    EFI_UNSUPPORTED
}

/// Write — read-only media
unsafe extern "efiapi" fn file_write_ro(
    _this: *mut FileProtocol,
    _buffer_size: *mut usize,
    _buffer: *mut c_void,
) -> usize {
    EFI_WRITE_PROTECTED
}

/// Get current file position.
unsafe extern "efiapi" fn file_get_position(
    this: *mut FileProtocol,
    position: *mut u64,
) -> usize {
    if position.is_null() {
        return EFI_INVALID_PARAMETER;
    }
    let vf = unsafe { &*(this as *const VirtualFile) };
    unsafe { *position = vf.position; }
    EFI_SUCCESS
}

/// Set file position (seek).
unsafe extern "efiapi" fn file_set_position(
    this: *mut FileProtocol,
    position: u64,
) -> usize {
    let vf = unsafe { &mut *(this as *mut VirtualFile) };
    // Use patched size for grub.cfg, otherwise original extent size
    let max_pos = if vf.patched { vf.patched_size } else { vf.extent_size as u64 };
    if position > max_pos {
        vf.position = max_pos;
    } else {
        vf.position = position;
    }
    EFI_SUCCESS
}

/// Get file/directory information (EFI_FILE_INFO).
unsafe extern "efiapi" fn file_get_info(
    this: *mut FileProtocol,
    information_type: *const Guid,
    buffer_size: *mut usize,
    buffer: *mut c_void,
) -> usize {
    if information_type.is_null() || buffer_size.is_null() {
        return EFI_INVALID_PARAMETER;
    }

    let info_type = unsafe { &*information_type };
    if info_type.d1 != FILE_INFO_GUID.d1
        || info_type.d2 != FILE_INFO_GUID.d2
        || info_type.d3 != FILE_INFO_GUID.d3
        || info_type.d4 != FILE_INFO_GUID.d4
    {
        return EFI_UNSUPPORTED;
    }

    let vf = unsafe { &*(this as *const VirtualFile) };
    let file_name: [u16; 1] = [0]; // empty name for the file itself

    // Calculate required size: EfiFileInfo header + UCS-2 null-terminated name
    let required_size =
        core::mem::size_of::<EfiFileInfo>() + file_name.len() * 2;

    let buf_sz = unsafe { *buffer_size };
    if buf_sz < required_size {
        unsafe { *buffer_size = required_size; }
        return crate::protocol::EFI_BUFFER_TOO_SMALL;
    }

    // Allow NULL buffer for size-query pattern (standard UEFI convention)
    if buffer.is_null() {
        return EFI_SUCCESS;
    }

    // Zero-fill EfiTime (not Copy, so create three zeroed instances)
    let create_time: EfiTime = unsafe { core::mem::zeroed() };
    let last_access_time: EfiTime = unsafe { core::mem::zeroed() };
    let modification_time: EfiTime = unsafe { core::mem::zeroed() };

    // Report patched size for grub.cfg
    let file_size = if vf.patched { vf.patched_size } else { vf.extent_size as u64 };

    let info = EfiFileInfo {
        size: required_size as u64,
        file_size,
        physical_size: file_size,
        create_time,
        last_access_time,
        modification_time,
        attribute: if vf.is_dir { 0x0000_0000_0000_0001 } else { 0 }, // EFI_FILE_DIRECTORY if dir
    };

    let dst = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, buf_sz) };

    // Copy EfiFileInfo header
    let info_bytes = unsafe {
        core::slice::from_raw_parts(
            &info as *const EfiFileInfo as *const u8,
            core::mem::size_of::<EfiFileInfo>(),
        )
    };
    dst[..info_bytes.len()].copy_from_slice(info_bytes);

    // Append UCS-2 file name (empty null-terminated string)
    let name_offset = core::mem::size_of::<EfiFileInfo>();
    dst[name_offset] = 0;
    dst[name_offset + 1] = 0;

    unsafe { *buffer_size = required_size; }
    EFI_SUCCESS
}

/// Set info — read-only
unsafe extern "efiapi" fn file_set_info_ro(
    _this: *mut FileProtocol,
    _information_type: *const Guid,
    _buffer_size: usize,
    _buffer: *mut c_void,
) -> usize {
    EFI_WRITE_PROTECTED
}

/// Flush — noop for read-only
unsafe extern "efiapi" fn file_flush(_this: *mut FileProtocol) -> usize {
    EFI_SUCCESS
}

// ═══════════════════════════════════════════════════════════════════════════
//  Public constructor
// ═══════════════════════════════════════════════════════════════════════════

/// Create an IsoFsInstance, parse the ISO9660 PVD, and return a pool-allocated
/// pointer. Returns null on failure.
pub fn create_iso_fs(
    bs: &mut BootServices,
    st: *mut SystemTable,
    real_bio_ptr: *mut BlockIoProtocol,
    real_media_id: u32,
    iso_lba: u64,
    iso_size_bytes: u64,
    iso_name: &[u8],
) -> *mut IsoFsInstance {
    // ── Allocate IsoFsInstance ──────────────────────────────────────
    let mut ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.allocate_pool)(
            crate::protocol::MemoryType::EfiLoaderData,
            core::mem::size_of::<IsoFsInstance>(),
            &mut ptr,
        )
    };
    if status != EFI_SUCCESS || ptr.is_null() {
        return core::ptr::null_mut();
    }

    let instance = unsafe { &mut *(ptr as *mut IsoFsInstance) };

    // ── Copy ISO file name ────────────────────────────────────────
    let name_len = iso_name.len().min(127);
    let mut name_arr = [0u8; 128];
    name_arr[..name_len].copy_from_slice(&iso_name[..name_len]);

    // ── Fill context ────────────────────────────────────────────────
    instance.ctx = IsoFsCtx {
        real_bio_ptr,
        real_media_id,
        iso_lba,
        iso_size_bytes,
        root_lba: 0,
        root_size: 0,
        bs: bs as *mut BootServices,
        st,
        iso_name: name_arr,
        iso_name_len: name_len,
    };

    // ── Parse ISO9660 PVD ──────────────────────────────────────────
    if let Some((root_lba, root_size)) = parse_pvd(&instance.ctx) {
        instance.ctx.root_lba = root_lba;
        instance.ctx.root_size = root_size;
    } else {
        // Failed to parse; free and return null
        unsafe { (bs.free_pool)(ptr); }
        return core::ptr::null_mut();
    }

    // ── Fill SimpleFileSystemProtocol ───────────────────────────────
    instance.sfs = SimpleFileSystemProtocol {
        revision: 0x0001_0000_0000_0001,
        open_volume: sfs_open_volume,
    };

    ptr as *mut IsoFsInstance
}