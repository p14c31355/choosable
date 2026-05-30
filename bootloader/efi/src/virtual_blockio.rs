// ═══════════════════════════════════════════════════════════════════════════
//  Virtual Block I/O — ISO file presented as a CD-ROM device
// ═══════════════════════════════════════════════════════════════════════════
//
//  Creates a synthetic EFI_BLOCK_IO_PROTOCOL on a new handle, backed by
//  the raw ISO file data on the real disk.  Child EFI images (GRUB etc.)
//  can discover this device and read the ISO filesystem transparently.
//
//  Patches are applied at the Block I/O layer:
//   1. Directory entry redirect: when GRUB reads the directory sector
//      containing grub.cfg's entry, the Extent LBA and Data Length fields
//      are overwritten to point to the patched file appended at the end
//      of the virtual CD-ROM.
//   2. Appended patched file: the new grub.cfg content is served from
//      sectors beyond the original ISO's LastBlock.

use core::ffi::c_void;

use crate::iso_fs::IsoFsInstance;
use crate::protocol::{
    BlockIoMedia, BlockIoProtocol, BootServices, MemoryType, SystemTable, VirtualBlockIo,
    EFI_SUCCESS, EFI_BAD_BUFFER_SIZE, EFI_DEVICE_ERROR, BLOCK_IO_PROTOCOL_GUID,
    DEVICE_PATH_PROTOCOL_GUID, SIMPLE_FILE_SYSTEM_PROTOCOL_GUID,
};

/// ReadBlocks implementation for the virtual CD-ROM.
unsafe extern "efiapi" fn vblock_read(
    this: *mut BlockIoProtocol,
    _media_id: u32,
    lba: u64,
    buffer_size: usize,
    buffer: *mut c_void,
) -> usize {
    let vbio = &*(this as *const VirtualBlockIo);

    if buffer_size % 2048 != 0 {
        return EFI_BAD_BUFFER_SIZE;
    }

    let num_blocks = buffer_size / 2048;
    let dst = core::slice::from_raw_parts_mut(buffer as *mut u8, buffer_size);

    for b in 0..num_blocks {
        let block_lba = lba + b as u64;
        let block_offset = b * 2048;

        // Case 1: Directory entry sector — intercept and overwrite the entry
        if vbio.dir_entry_patched && block_lba == vbio.dir_entry_sector as u64 {
            // Read the real directory sector first
            let disk_lba = vbio.iso_lba + block_lba * 4;
            let status = unsafe {
                ((*vbio.real_bio_ptr).read_blocks)(
                    vbio.real_bio_ptr,
                    vbio.real_media_id,
                    disk_lba,
                    2048,
                    dst.as_mut_ptr().add(block_offset) as *mut c_void,
                )
            };
            if status != EFI_SUCCESS {
                return EFI_DEVICE_ERROR;
            }
            // Overwrite the Extent LBA (offset +2) and Data Length (offset +10)
            let entry = &mut dst[block_offset..block_offset + 2048];
            let off = vbio.dir_entry_offset as usize;
            if off + 14 <= 2048 {
                entry[off + 2..off + 6].copy_from_slice(&vbio.dir_entry_new_extent.to_le_bytes());
                entry[off + 10..off + 14].copy_from_slice(&vbio.dir_entry_new_size.to_le_bytes());
            }
        }
        // Case 2: Patched file sector — served from appended data
        else if vbio.patched_file_sectors > 0
            && !vbio.patched_file_buf.is_null()
            && block_lba >= vbio.patched_file_sector as u64
            && block_lba < vbio.patched_file_sector as u64 + vbio.patched_file_sectors as u64
        {
            let patch_offset = ((block_lba - vbio.patched_file_sector as u64) as usize) * 2048;
            let src = unsafe {
                core::slice::from_raw_parts(
                    vbio.patched_file_buf.add(patch_offset),
                    2048,
                )
            };
            dst[block_offset..block_offset + 2048].copy_from_slice(src);
        }
        // Case 3: Normal read from real ISO
        else {
            let disk_lba = vbio.iso_lba + block_lba * 4;
            let status = unsafe {
                ((*vbio.real_bio_ptr).read_blocks)(
                    vbio.real_bio_ptr,
                    vbio.real_media_id,
                    disk_lba,
                    2048,
                    dst.as_mut_ptr().add(block_offset) as *mut c_void,
                )
            };
            if status != EFI_SUCCESS {
                return EFI_DEVICE_ERROR;
            }
        }
    }

    EFI_SUCCESS
}

/// Create a virtual Block I/O + Device Path handle representing ISO as CD-ROM.
///
/// Returns `(handle, device_path_ptr, vbio_ptr, sfs_instance)`.  All must NOT be freed.
pub fn create_virtual_cdrom(
    bs: &mut BootServices,
    st: *mut SystemTable,
    iso_lba: u64,
    real_bio_ptr: *mut BlockIoProtocol,
    real_media_id: u32,
    iso_size_bytes: u64,
    iso_name: &[u8],
) -> Option<(*mut c_void, *mut c_void, *mut VirtualBlockIo, *mut IsoFsInstance)> {
    // ═════════════════════════════════════════════════════════════
    // 1. Build CD-ROM DevicePath
    // ═════════════════════════════════════════════════════════════
    const CDROM_NODE: [u8; 24] = {
        let mut n = [0u8; 24];
        n[0] = 0x04;
        n[1] = 0x02;
        n[2] = 24u8.to_le_bytes()[0];
        n[3] = 0x00;
        n
    };
    const END_NODE: [u8; 4] = [0x7F, 0xFF, 0x04, 0x00];

    let dp_len = CDROM_NODE.len() + END_NODE.len();
    let mut dp_ptr: *mut c_void = core::ptr::null_mut();
    let dp_status = unsafe {
        (bs.allocate_pool)(MemoryType::EfiLoaderData, dp_len, &mut dp_ptr)
    };
    if dp_status != EFI_SUCCESS || dp_ptr.is_null() {
        return None;
    }
    let dp = dp_ptr as *mut u8;
    unsafe {
        dp.copy_from_nonoverlapping(CDROM_NODE.as_ptr(), CDROM_NODE.len());
        *(dp.add(8) as *mut u64) = 0u64.to_le();
        *(dp.add(16) as *mut u64) = (iso_size_bytes / 2048).to_le();
        dp.add(CDROM_NODE.len())
            .copy_from_nonoverlapping(END_NODE.as_ptr(), END_NODE.len());
    }

    // ═════════════════════════════════════════════════════════════
    // 2. Allocate VirtualBlockIo
    // ═════════════════════════════════════════════════════════════
    let mut ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.allocate_pool)(
            MemoryType::EfiLoaderData,
            core::mem::size_of::<VirtualBlockIo>(),
            &mut ptr,
        )
    };
    if status != EFI_SUCCESS || ptr.is_null() {
        unsafe { (bs.free_pool)(dp_ptr); }
        return None;
    }
    let vbio: &mut VirtualBlockIo = unsafe { &mut *(ptr as *mut VirtualBlockIo) };

    let iso_sectors = iso_size_bytes / 2048;
    vbio.protocol = BlockIoProtocol {
        bio_rev: 0x0001_0000_0000_0001,
        media: &mut vbio.media as *mut BlockIoMedia,
        bio_rst: core::ptr::null_mut(),
        read_blocks: vblock_read,
        bio_w: core::ptr::null_mut(),
        bio_f: core::ptr::null_mut(),
    };

    vbio.media = BlockIoMedia {
        mid: 0,
        bim_rm: 0,
        bim_mp: 1,
        bim_lp: 0,
        bim_ro: 1,
        bim_wc: 0,
        bim_bs: 2048,
        bim_ia: 0,
        bim_lb: iso_sectors, // original last block (patched file appends beyond this)
    };

    vbio.iso_lba = iso_lba;
    vbio.real_bio_ptr = real_bio_ptr;
    vbio.real_media_id = real_media_id;

    // Initialize patch fields
    vbio.patched_file_sector = 0;
    vbio.patched_file_sectors = 0;
    vbio.patched_file_buf = core::ptr::null_mut();
    vbio.dir_entry_sector = 0;
    vbio.dir_entry_offset = 0;
    vbio.dir_entry_new_extent = 0;
    vbio.dir_entry_new_size = 0;
    vbio.dir_entry_patched = false;

    // ═════════════════════════════════════════════════════════════
    // 3. Install BlockIO protocol (creates the handle)
    // ═════════════════════════════════════════════════════════════
    let mut new_handle: *mut c_void = core::ptr::null_mut();
    let install_status = unsafe {
        (bs.install_protocol_interface)(
            &mut new_handle,
            &BLOCK_IO_PROTOCOL_GUID,
            0,
            vbio as *mut VirtualBlockIo as *mut c_void,
        )
    };
    if install_status != EFI_SUCCESS {
        unsafe {
            (bs.free_pool)(ptr);
            (bs.free_pool)(dp_ptr);
        }
        return None;
    }

    // ═════════════════════════════════════════════════════════════
    // 4. Install DevicePath protocol on the same handle
    // ═════════════════════════════════════════════════════════════
    let dp_status2 = unsafe {
        (bs.install_protocol_interface)(
            &mut new_handle,
            &DEVICE_PATH_PROTOCOL_GUID,
            0,
            dp_ptr,
        )
    };
    if dp_status2 != EFI_SUCCESS {
        unsafe { (bs.free_pool)(dp_ptr); }
    }

    // ═════════════════════════════════════════════════════════════
    // 5. Install ISO9660 SimpleFileSystem protocol on the same handle
    // ═════════════════════════════════════════════════════════════
    let iso_fs_instance = crate::iso_fs::create_iso_fs(
        bs, st, real_bio_ptr, real_media_id, iso_lba, iso_size_bytes, iso_name,
    );
    if !iso_fs_instance.is_null() {
        let sfs_status = unsafe {
            (bs.install_protocol_interface)(
                &mut new_handle,
                &SIMPLE_FILE_SYSTEM_PROTOCOL_GUID,
                0,
                iso_fs_instance as *mut c_void,
            )
        };
        if sfs_status != EFI_SUCCESS {
            unsafe { (bs.free_pool)(iso_fs_instance as *mut c_void); }
        }
    }

    let vbio_ptr = vbio as *mut VirtualBlockIo;
    Some((new_handle, dp_ptr, vbio_ptr, iso_fs_instance))
}