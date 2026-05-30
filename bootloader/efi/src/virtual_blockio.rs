// ═══════════════════════════════════════════════════════════════════════════
//  Virtual Block I/O — ISO file presented as a CD-ROM device
// ═══════════════════════════════════════════════════════════════════════════
//
//  Creates a synthetic EFI_BLOCK_IO_PROTOCOL on a new handle, backed by
//  the raw ISO file data on the real disk.  Child EFI images (GRUB etc.)
//  can discover this device and read the ISO filesystem transparently.

use core::ffi::c_void;

use crate::protocol::{
    BlockIoMedia, BlockIoProtocol, BootServices, MemoryType, SystemTable, VirtualBlockIo,
    EFI_SUCCESS, BLOCK_IO_PROTOCOL_GUID, DEVICE_PATH_PROTOCOL_GUID,
    SIMPLE_FILE_SYSTEM_PROTOCOL_GUID,
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

    if buffer_size % 2048 != 0 {
        return 0x8000_0000_0000_0004; // EFI_BAD_BUFFER_SIZE
    }

    let num_blocks = buffer_size / 2048;
    let dst = core::slice::from_raw_parts_mut(buffer as *mut u8, buffer_size);

    for b in 0..num_blocks {
        let block_lba = lba + b as u64;
        let disk_lba = vbio.iso_lba + block_lba * 4;
        let block_offset = b * 2048;

        // Read all 4 sectors (2048 bytes) in a single read_blocks call
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
            return 0x8000_0000_0000_0002; // EFI_DEVICE_ERROR
        }
    }

    EFI_SUCCESS
}

/// Create a virtual Block I/O + Device Path handle representing ISO as CD-ROM.
///
/// Returns `(handle, device_path_ptr)`.  Both must NOT be freed.
pub fn create_virtual_cdrom(
    bs: &mut BootServices,
    st: *mut SystemTable,
    iso_lba: u64,
    real_bio_ptr: *mut BlockIoProtocol,
    real_media_id: u32,
    iso_size_bytes: u64,
) -> Option<(*mut c_void, *mut c_void)> {
    // ═════════════════════════════════════════════════════════════
    // 1. Build CD-ROM DevicePath
    // ═════════════════════════════════════════════════════════════
    // CD-ROM Media Device Path node (24 bytes):
    //   Type=0x04 (MEDIA_DEVICE_PATH), SubType=0x02 (CD_ROM)
    const CDROM_NODE: [u8; 24] = {
        let mut n = [0u8; 24];
        n[0] = 0x04;  // Type: MEDIA_DEVICE_PATH
        n[1] = 0x02;  // SubType: MEDIA_CDROM_DP
        n[2] = 24u8.to_le_bytes()[0]; // Length = 24 (little-endian)
        n[3] = 0x00;
        // BootEntry (4 bytes at offset 4) = 0 (default entry)
        // PartitionStart (8 bytes at offset 8) = ISO LBA in bytes
        // PartitionSize (8 bytes at offset 16) = ISO size in bytes
        n
    };
    // End node (4 bytes)
    const END_NODE: [u8; 4] = [0x7F, 0xFF, 0x04, 0x00];

    let dp_len = CDROM_NODE.len() + END_NODE.len(); // 28 bytes
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
        // Patch PartitionStart (offset 8) = 0 (start of virtual CD-ROM)
        *(dp.add(8) as *mut u64) = 0u64.to_le();
        // Patch PartitionSize (offset 16) = size in 2048-byte blocks
        *(dp.add(16) as *mut u64) = (iso_size_bytes / 2048).to_le();
        // Append end node
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
        bim_mp: 1,             // media present: yes
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

    // ═════════════════════════════════════════════════════════════
    // 3. Install BlockIO protocol (creates the handle)
    // ═════════════════════════════════════════════════════════════
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
            &mut new_handle,            // same handle, now non-NULL
            &DEVICE_PATH_PROTOCOL_GUID,
            0,
            dp_ptr,
        )
    };
    if dp_status2 != EFI_SUCCESS {
        // Non-fatal — BlockIO is installed, but device path failed.
        // Free dp_ptr since it wasn't claimed by the handle.
        unsafe { (bs.free_pool)(dp_ptr); }
    }

    // ═════════════════════════════════════════════════════════════
    // 5. Install ISO9660 SimpleFileSystem protocol on the same handle
    // ═════════════════════════════════════════════════════════════
    let iso_fs_instance = crate::iso_fs::create_iso_fs(
        bs, st, real_bio_ptr, real_media_id, iso_lba, iso_size_bytes,
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
            // Free the ISO FS instance if installation failed
            unsafe { (bs.free_pool)(iso_fs_instance as *mut c_void); }
        }
    }

    Some((new_handle, dp_ptr))
}