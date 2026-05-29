// ═══════════════════════════════════════════════════════════════════════════
//  Virtual Block I/O — ISO file presented as a CD-ROM device
// ═══════════════════════════════════════════════════════════════════════════
//
//  Creates a synthetic EFI_BLOCK_IO_PROTOCOL on a new handle, backed by
//  the raw ISO file data on the real disk.  Child EFI images (GRUB etc.)
//  can discover this device and read the ISO filesystem transparently.

use core::ffi::c_void;

use crate::disk::read_sector;
use crate::protocol::{
    BlockIoMedia, BlockIoProtocol, BootServices, MemoryType, VirtualBlockIo,
    EFI_SUCCESS, BLOCK_IO_PROTOCOL_GUID,
};

/// ReadBlocks implementation for the virtual CD-ROM.
///
/// Translates LBA (in 2048-byte CD sectors) → real disk LBA
/// (ISO file offset converted to 512-byte disk sectors), then reads
/// into the caller's buffer.
unsafe extern "efiapi" fn vblock_read(
    this: *mut BlockIoProtocol,
    _media_id: u32,
    lba: u64,
    buffer_size: usize,
    buffer: *mut c_void,
) -> usize {
    let vbio = &*(this as *const VirtualBlockIo);

    // CD-ROM sector = 2048 bytes.
    // Real disk sector = 512 bytes.
    // Absolute disk LBA = iso_lba + (cd_lba * 4).
    let cd_byte_offset = lba * 2048;
    let disk_lba = vbio.iso_lba + lba * 4;
    let byte_count = buffer_size.min(2048);

    let dst = core::slice::from_raw_parts_mut(buffer as *mut u8, byte_count);

    let mut offset: usize = 0;
    for i in 0..4usize {
        if offset >= byte_count {
            break;
        }
        let mut sec = [0u8; 512];
        if !read_sector(
            &*vbio.real_bio_ptr,
            vbio.real_bio_ptr,
            vbio.real_media_id,
            disk_lba + i as u64,
            &mut sec,
        ) {
            return 0x8000_0000_0000_0002; // EFI_DEVICE_ERROR
        }
        let to_copy = byte_count - offset;
        let to_copy = if to_copy > 512 { 512 } else { to_copy };
        dst[offset..offset + to_copy].copy_from_slice(&sec[..to_copy]);
        offset += to_copy;
        let _ = cd_byte_offset; // suppress unused warning
    }

    EFI_SUCCESS
}

/// Create a virtual Block I/O handle representing the ISO as a CD-ROM.
///
/// Returns the new handle.  The returned `VirtualBlockIo` lives in
/// pool memory and must not be freed — it persists for the lifetime
/// of the child image.
pub fn create_virtual_cdrom(
    bs: &mut BootServices,
    iso_lba: u64,
    real_bio_ptr: *mut BlockIoProtocol,
    real_media_id: u32,
    iso_size_bytes: u64,
) -> Option<*mut c_void> {
    // Allocate the VirtualBlockIo structure from pool
    let mut ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.allocate_pool)(
            MemoryType::EfiLoaderData,
            core::mem::size_of::<VirtualBlockIo>(),
            &mut ptr,
        )
    };
    if status != EFI_SUCCESS || ptr.is_null() {
        return None;
    }
    let vbio: &mut VirtualBlockIo = unsafe { &mut *(ptr as *mut VirtualBlockIo) };

    // ── Fill in the BlockIoProtocol ────────────────────
    let iso_sectors = iso_size_bytes / 2048;
    vbio.protocol = BlockIoProtocol {
        bio_rev: 0x0001_0000_0000_0001, // revision 1.1
        media: &mut vbio.media as *mut BlockIoMedia,
        bio_rst: core::ptr::null_mut(),
        read_blocks: vblock_read,
        bio_w: core::ptr::null_mut(),    // read-only
        bio_f: core::ptr::null_mut(),    // no flush
    };

    // ── Fill in BlockIoMedia ───────────────────────────
    vbio.media = BlockIoMedia {
        mid: 0,                // media ID
        bim_rm: 0,             // removable: no
        bim_mp: 0,             // media present: no (firmware checks this)
        bim_lp: 0,             // logical partition
        bim_ro: 1,             // read-only
        bim_wc: 0,             // write caching: none
        bim_bs: 2048,          // block size (CD-ROM sector)
        bim_ia: 0,             // IO alignment
        bim_lb: iso_sectors,   // last block
    };

    // ── Context ────────────────────────────────────────
    vbio.iso_lba = iso_lba;
    vbio.real_bio_ptr = real_bio_ptr;
    vbio.real_media_id = real_media_id;

    // ── Install protocol on a new handle ───────────────
    let mut new_handle: *mut c_void = core::ptr::null_mut();
    let install_status = unsafe {
        (bs.install_protocol_interface)(
            &mut new_handle,
            &BLOCK_IO_PROTOCOL_GUID,
            0,                                      // EFI_NATIVE_INTERFACE
            vbio as *mut VirtualBlockIo as *mut c_void,
        )
    };
    if install_status != EFI_SUCCESS {
        unsafe { (bs.free_pool)(ptr); }
        return None;
    }

    Some(new_handle)
}