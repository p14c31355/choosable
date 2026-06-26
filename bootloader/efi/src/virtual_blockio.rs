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

// ═══════════════════════════════════════════════════════════════════════════
//  VirtualMedia trait — abstract over virtual block devices
// ═══════════════════════════════════════════════════════════════════════════
//
//  All virtual media types (ISO, IMG, VHD, RAM disk) implement this trait
//  so they can be served through the same EFI Block I/O protocol.
//
//  Implementations:
//    IsoMedia   — ISO 9660 filesystem served from an ISO file
//    ImgMedia   — raw disk image (.img)
//    VhdMedia   — virtual hard disk (.vhd, .vhdx)
//    RamMedia   — RAM disk (for synthetic files like premount cpio)

/// Abstracts block-level read access to a virtual medium.
///
/// The trait decouples the UEFI Block I/O protocol machinery from
/// the actual data source, making it easy to add new media types.
pub trait VirtualMedia {
    /// Read a single 2048-byte logical block at `block_lba` into `dst`
    /// at `dst_offset`.  Returns `true` on success.
    fn read_block(&self, block_lba: u64, dst: &mut [u8], dst_offset: usize) -> bool;

    /// Returns the total number of blocks on this medium.
    fn block_count(&self) -> u64;

    /// Returns the block size in bytes (typically 2048 for optical media).
    fn block_size(&self) -> u32;

    /// Media type name for logging (e.g. "ISO", "IMG", "VHD").
    fn media_type(&self) -> &'static str;
}

// ═══════════════════════════════════════════════════════════════════════════
//  IsoMedia — ISO backed by a real disk extent
// ═══════════════════════════════════════════════════════════════════════════

/// Virtual medium backed by an ISO file on a physical block device.
///
/// Reads are translated from virtual CD-ROM LBA to physical disk LBA.
pub struct IsoMedia {
    /// ISO file start LBA on the physical disk
    pub iso_lba: u64,
    /// Physical Block I/O protocol pointer
    pub real_bio_ptr: *mut BlockIoProtocol,
    /// Physical media ID
    pub real_media_id: u32,
    /// Total number of 2048-byte blocks in the ISO
    pub total_blocks: u64,
}

impl VirtualMedia for IsoMedia {
    fn read_block(&self, block_lba: u64, dst: &mut [u8], dst_offset: usize) -> bool {
        if block_lba >= self.total_blocks {
            return false;
        }
        // Guard against overflow: check that dst_offset + 2048 won't overflow and is in bounds
        if dst_offset > dst.len() || dst.len() - dst_offset < 2048 {
            return false;
        }
        // Validate that 2048-byte ISO sectors are an exact multiple of backing block size
        let backing_block_size = unsafe {
            if (*self.real_bio_ptr).media.is_null() {
                512
            } else {
                (*(*self.real_bio_ptr).media).bim_bs
            }
        };
        if backing_block_size == 0 || 2048 % backing_block_size != 0 {
            return false;
        }
        let sectors_per_iso_block = 2048 / backing_block_size;
        let disk_lba = self.iso_lba + block_lba * sectors_per_iso_block as u64;
        unsafe {
            ((*self.real_bio_ptr).read_blocks)(
                self.real_bio_ptr,
                self.real_media_id,
                disk_lba,
                2048,
                dst.as_mut_ptr().add(dst_offset) as *mut c_void,
            ) == EFI_SUCCESS
        }
    }

    fn block_count(&self) -> u64 {
        self.total_blocks
    }

    fn block_size(&self) -> u32 {
        2048
    }

    fn media_type(&self) -> &'static str { "ISO" }
}

// ═══════════════════════════════════════════════════════════════════════════
//  RawMedia — raw disk image (.img) backed by a physical disk extent
// ═══════════════════════════════════════════════════════════════════════════

pub struct RawMedia {
    pub start_lba: u64,
    pub total_blocks: u64,
    pub real_bio_ptr: *mut BlockIoProtocol,
    pub real_media_id: u32,
    pub sector_size: u32,
}

impl VirtualMedia for RawMedia {
    fn read_block(&self, block_lba: u64, dst: &mut [u8], dst_offset: usize) -> bool {
        if block_lba >= self.total_blocks { return false; }
        let sector_size = self.sector_size.max(512) as u64;
        if sector_size > 65536 { return false; }
        if dst_offset as u64 + sector_size > dst.len() as u64 { return false; }
        // Validate that virtual sector size is an exact multiple of backing block size
        let backing_block_size = unsafe {
            if (*self.real_bio_ptr).media.is_null() {
                512
            } else {
                (*(*self.real_bio_ptr).media).bim_bs
            }
        } as u64;
        if backing_block_size == 0 || sector_size % backing_block_size != 0 {
            return false;
        }
        let sectors_per_block = sector_size / backing_block_size;
        let disk_lba = match block_lba
            .checked_mul(sectors_per_block)
            .and_then(|off| self.start_lba.checked_add(off))
        {
            Some(lba) => lba,
            None => return false,
        };
        unsafe {
            ((*self.real_bio_ptr).read_blocks)(
                self.real_bio_ptr, self.real_media_id,
                disk_lba, sector_size as usize,
                dst.as_mut_ptr().add(dst_offset) as *mut c_void,
            ) == EFI_SUCCESS
        }
    }
    fn block_count(&self) -> u64 { self.total_blocks }
    fn block_size(&self) -> u32 { self.sector_size.max(512) }
    fn media_type(&self) -> &'static str { "IMG" }
}

// ═══════════════════════════════════════════════════════════════════════════
//  VhdMedia — FIXED VHD only (contiguous data layout)
// ═══════════════════════════════════════════════════════════════════════════
//
//  This implementation is restricted to FIXED VHD images where all data
//  occupies a contiguous extent starting at `data_lba`.  Dynamic VHD and
//  VHDX formats require block allocation table (BAT) mapping, which is
//  NOT implemented here.  Any dynamic or differencing VHD/VHDX will produce
//  incorrect reads and must not use this VirtualMedia implementation.

pub struct VhdMedia {
    pub data_lba: u64,
    pub total_blocks: u64,
    pub real_bio_ptr: *mut BlockIoProtocol,
    pub real_media_id: u32,
    pub sector_size: u32,
}

impl VirtualMedia for VhdMedia {
    fn read_block(&self, block_lba: u64, dst: &mut [u8], dst_offset: usize) -> bool {
        if block_lba >= self.total_blocks { return false; }
        let sector_size = self.sector_size.max(512) as u64;
        if sector_size > 65536 { return false; }
        if dst_offset as u64 + sector_size > dst.len() as u64 { return false; }
        // Validate that virtual sector size is an exact multiple of backing block size
        let backing_block_size = unsafe {
            if (*self.real_bio_ptr).media.is_null() {
                512
            } else {
                (*(*self.real_bio_ptr).media).bim_bs
            }
        } as u64;
        if backing_block_size == 0 || sector_size % backing_block_size != 0 {
            return false;
        }
        let sectors_per_block = sector_size / backing_block_size;
        let disk_lba = match block_lba
            .checked_mul(sectors_per_block)
            .and_then(|off| self.data_lba.checked_add(off))
        {
            Some(lba) => lba,
            None => return false,
        };
        unsafe {
            ((*self.real_bio_ptr).read_blocks)(
                self.real_bio_ptr, self.real_media_id,
                disk_lba, sector_size as usize,
                dst.as_mut_ptr().add(dst_offset) as *mut c_void,
            ) == EFI_SUCCESS
        }
    }
    fn block_count(&self) -> u64 { self.total_blocks }
    fn block_size(&self) -> u32 { self.sector_size.max(512) }
    fn media_type(&self) -> &'static str { "VHD" }
}

/// ReadBlocks implementation for the virtual CD-ROM.
/// Helper: patch ISO9660 directory entry extent + data length (LE + BE)
fn patch_dir_entry(entry: &mut [u8], off: usize, new_extent: u32, new_size: u32) {
    if off + 18 > entry.len() { return; }
    entry[off + 2..off + 6].copy_from_slice(&new_extent.to_le_bytes());
    entry[off + 6..off + 10].copy_from_slice(&new_extent.to_be_bytes());
    entry[off + 10..off + 14].copy_from_slice(&new_size.to_le_bytes());
    entry[off + 14..off + 18].copy_from_slice(&new_size.to_be_bytes());
}

/// Helper: read one ISO sector (2048B) from real disk into dst at offset
fn read_real_iso_sector(vbio: &VirtualBlockIo, iso_sector: u64, dst: &mut [u8], dst_off: usize) -> bool {
    // Get backing media block size
    let backing_block_size = unsafe {
        if (*vbio.real_bio_ptr).media.is_null() {
            512
        } else {
            (*(*vbio.real_bio_ptr).media).bim_bs
        }
    };
    if backing_block_size == 0 || 2048 % backing_block_size != 0 {
        return false;
    }
    let sectors_per_iso_block = 2048 / backing_block_size;
    let disk_lba = vbio.iso_lba + iso_sector * sectors_per_iso_block as u64;
    unsafe {
        ((*vbio.real_bio_ptr).read_blocks)(
            vbio.real_bio_ptr,
            vbio.real_media_id,
            disk_lba,
            2048,
            dst.as_mut_ptr().add(dst_off) as *mut c_void,
        ) == EFI_SUCCESS
    }
}

/// Helper: serve sector from a memory buffer
fn serve_memory_sector(
    buf: *mut u8,
    buf_sectors: u32,
    buf_sector_start: u32,
    block_lba: u64,
    dst: &mut [u8],
    block_offset: usize,
) -> bool {
    if buf.is_null() || buf_sectors == 0 { return false; }
    let start = buf_sector_start as u64;
    if block_lba < start || block_lba >= start + buf_sectors as u64 { return false; }
    let off = (block_lba - start) as usize * 2048;
    let src = unsafe { core::slice::from_raw_parts(buf.add(off), 2048) };
    dst[block_offset..block_offset + 2048].copy_from_slice(src);
    true
}

unsafe extern "efiapi" fn vblock_read(
    this: *mut BlockIoProtocol,
    _media_id: u32,
    lba: u64,
    buffer_size: usize,
    buffer: *mut c_void,
) -> usize {
    if buffer_size % 2048 != 0 { return EFI_BAD_BUFFER_SIZE; }

    let vbio = &*(this as *const VirtualBlockIo);
    let dst = core::slice::from_raw_parts_mut(buffer as *mut u8, buffer_size);

    for b in 0..(buffer_size / 2048) {
        let block_lba = lba + b as u64;
        let block_offset = b * 2048;

        let is_dir_patched = vbio.dir_entry_patched && block_lba == vbio.dir_entry_sector as u64;
        let is_premount_patched = vbio.premount_entry_patched && block_lba == vbio.premount_entry_sector as u64;
        let is_premount_injected = vbio.premount_entry_injected && block_lba == vbio.premount_entry_sector as u64;
        let sect_handled = is_dir_patched || is_premount_patched || is_premount_injected;

        if is_dir_patched {
            if !read_real_iso_sector(vbio, block_lba, dst, block_offset) { return EFI_DEVICE_ERROR; }
            patch_dir_entry(&mut dst[block_offset..block_offset + 2048],
                vbio.dir_entry_offset as usize, vbio.dir_entry_new_extent, vbio.dir_entry_new_size);
        }

        if is_premount_patched {
            if !is_dir_patched && !read_real_iso_sector(vbio, block_lba, dst, block_offset) { return EFI_DEVICE_ERROR; }
            patch_dir_entry(&mut dst[block_offset..block_offset + 2048],
                vbio.premount_entry_offset as usize, vbio.premount_entry_new_extent, vbio.premount_entry_new_size);
            // Also rename the ISO9660 directory entry to "PREMOUNT.CPIO;1"
            // so GRUB can resolve the /PREMOUNT.CPIO path from grub.cfg.
            if vbio.premount_entry_rename {
                let off = vbio.premount_entry_offset as usize;
                let name = b"PREMOUNT.CPIO;1";
                // ISO9660 name length at byte 32, record length at byte 0
                let name_len_byte = name.len() as u8;
                if off < 2048 && block_offset + off < dst.len() {
                    let record_len = dst[block_offset + off] as usize;
                    // Check both sector boundary and record length can fit the new name
                    if off + 33 + name.len() <= 2048 && 33 + name.len() <= record_len {
                        dst[block_offset + off + 32] = name_len_byte;
                        dst[block_offset + off + 33..block_offset + off + 33 + name.len()]
                            .copy_from_slice(name);
                    }
                }
            }
        }

        if is_premount_injected {
            // The injected sector lives beyond the original ISO end
            // and must be served purely from memory (unless it was
            // injected within the existing root directory via EOD).
            // When root_relocated is true, the injected entry is part
            // of the relocated root buf, so this path should not fire.
            if !is_dir_patched && !is_premount_patched && !vbio.premount_root_relocated {
                if !read_real_iso_sector(vbio, block_lba, dst, block_offset) { return EFI_DEVICE_ERROR; }
            } else {
                for j in 0..2048 { dst[block_offset + j] = 0; }
            }
            // Overwrite with the pre-built synthetic directory record
            let off = vbio.premount_entry_offset as usize;
            let sz = vbio.premount_entry_injected_size as usize;
            if off + sz <= 2048 {
                dst[block_offset + off..block_offset + off + sz]
                    .copy_from_slice(&vbio.premount_entry_injected_blob[..sz]);
            }
        }

        if sect_handled { continue; }

        if serve_memory_sector(vbio.premount_root_buf, vbio.premount_root_sectors, vbio.premount_root_start_sector, block_lba, dst, block_offset)
            || serve_memory_sector(vbio.premount_file_buf, vbio.premount_file_sectors, vbio.premount_file_sector, block_lba, dst, block_offset)
            || serve_memory_sector(vbio.patched_file_buf, vbio.patched_file_sectors, vbio.patched_file_sector, block_lba, dst, block_offset)
        {
            continue;
        }

        if !read_real_iso_sector(vbio, block_lba, dst, block_offset) { return EFI_DEVICE_ERROR; }

        // Patch PVD at sector 16 so GRUB's ISO9660 driver accepts
        // extent references that point to appended sectors and sees
        // the updated root directory size after synthetic injection.
        if block_lba == 16 {
            let new_vol_size = (vbio.media.bim_lb + 1) as u32;
            let off = block_offset;
            // Volume Space Size: bytes 80-83 (LE), 84-87 (BE)
            dst[off + 80..off + 84].copy_from_slice(&new_vol_size.to_le_bytes());
            dst[off + 84..off + 88].copy_from_slice(&new_vol_size.to_be_bytes());

            // Override Volume ID to "CHOOSABLE" (space-padded to 32 bytes)
            // so that LABEL=Choosable kernel cmdline parameters
            // (live-media, archisodevice, rd.live.image, etc.) work.
            // ISO9660 Volume Identifier: bytes 40-71 (32 bytes, space-padded).
            {
                let label = b"CHOOSABLE                       ";
                // label is exactly 32 bytes
                dst[off + 40..off + 72].copy_from_slice(&label[0..32]);
            }

            // If premount entry was injected (not patched over existing),
            // also update the root directory record data length in PVD
            // so GRUB walks past the synthetic PREMOUNT.CPIO record.
            if vbio.premount_entry_injected && vbio.premount_new_root_size > 0 {
                // Root Dir Record Data Length: bytes 166-169 (LE), 170-173 (BE)
                dst[off + 166..off + 170].copy_from_slice(&vbio.premount_new_root_size.to_le_bytes());
                dst[off + 170..off + 174].copy_from_slice(&vbio.premount_new_root_size.to_be_bytes());
            }

            // When root directory is relocated (copied to a new
            // contiguous extent at the end of the virtual CD-ROM),
            // redirect PVD root_lba and root_size to the relocated extent.
            if vbio.premount_root_relocated && vbio.premount_root_start_sector > 0 {
                // Root Dir Record Extent: bytes 158-161 (LE), 162-165 (BE)
                dst[off + 158..off + 162].copy_from_slice(&vbio.premount_root_start_sector.to_le_bytes());
                dst[off + 162..off + 166].copy_from_slice(&vbio.premount_root_start_sector.to_be_bytes());
                // Root Dir Record Data Length: bytes 166-169 (LE), 170-173 (BE)
                dst[off + 166..off + 170].copy_from_slice(&vbio.premount_new_root_size.to_le_bytes());
                dst[off + 170..off + 174].copy_from_slice(&vbio.premount_new_root_size.to_be_bytes());
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
    live_media_uuid: &[u8; 10],
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
        // Partition Start — must be 0 for virtual ISO-backed CD-ROM.
        // Some firmware rejects the DevicePath if this is set to the
        // volume size / last_block value.
        let partition_start = 0u64;
        *(dp.add(16) as *mut u64) = partition_start.to_le();
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
        bim_lb: iso_sectors - 1, // original last block (patched file appends beyond this)
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

    // Initialize premount fields
    vbio.premount_cpio_buf = core::ptr::null_mut();
    vbio.premount_cpio_size = 0;
    vbio.premount_squashfs_addr = 0;
    vbio.premount_squashfs_size = 0;
    vbio.premount_entry_sector = 0;
    vbio.premount_entry_offset = 0;
    vbio.premount_entry_new_extent = 0;
    vbio.premount_entry_new_size = 0;
    vbio.premount_entry_patched = false;
    vbio.premount_entry_rename = false;
    vbio.premount_file_sector = 0;
    vbio.premount_file_sectors = 0;
    vbio.premount_file_buf = core::ptr::null_mut();
    vbio.premount_entry_injected = false;
    vbio.premount_entry_injected_blob = [0u8; 128];
    vbio.premount_entry_injected_size = 0;
    vbio.premount_new_root_size = 0;
    vbio.premount_root_relocated = false;
    vbio.premount_root_buf = core::ptr::null_mut();
    vbio.premount_root_buf_size = 0;
    vbio.premount_root_sectors = 0;
    vbio.premount_root_start_sector = 0;

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
        // dp_ptr is NOT freed here - we still return it in the tuple
        // The caller may need it even if the protocol install failed
    }

    // ═════════════════════════════════════════════════════════════
    // 5. Install ISO9660 SimpleFileSystem protocol on the same handle
    // ═════════════════════════════════════════════════════════════
    let iso_fs_instance = crate::iso_fs::create_iso_fs(
        bs, st, real_bio_ptr, real_media_id, iso_lba, iso_size_bytes, iso_name, live_media_uuid,
        core::ptr::null_mut(), 0, // premount set later via vbio
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

    // Verify SFS is installed to prevent Use-After-Free if install failed
    let mut sfs_proto: *mut c_void = core::ptr::null_mut();
    let sfs_installed = !iso_fs_instance.is_null() && unsafe {
        (bs.handle_protocol)(new_handle, &SIMPLE_FILE_SYSTEM_PROTOCOL_GUID, &mut sfs_proto) == EFI_SUCCESS
    };
    let final_sfs = if sfs_installed { iso_fs_instance } else { core::ptr::null_mut() };

    let vbio_ptr = vbio as *mut VirtualBlockIo;
    Some((new_handle, dp_ptr, vbio_ptr, final_sfs))
}