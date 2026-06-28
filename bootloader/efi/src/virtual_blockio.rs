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

// ── Common block read implementation shared by all VirtualMedia types ─

fn backing_block_size(bio_ptr: *mut BlockIoProtocol) -> u32 {
    unsafe {
        if (*bio_ptr).media.is_null() { 512 }
        else { (*(*bio_ptr).media).bim_bs }
    }
}

fn read_block_at(
    real_bio_ptr: *mut BlockIoProtocol, real_media_id: u32,
    block_lba: u64, start_lba: u64, block_size: u32, total_blocks: u64,
    dst: &mut [u8], dst_offset: usize,
) -> bool {
    let block_size_u64 = block_size as u64;
    if block_lba >= total_blocks
        || block_size == 0 || block_size > 65536
        || dst_offset as u64 + block_size_u64 > dst.len() as u64
    { return false; }
    let bbs = backing_block_size(real_bio_ptr) as u64;
    if bbs == 0 || block_size_u64 % bbs != 0 { return false; }
    let sectors_per_block = block_size_u64 / bbs;
    let disk_lba = match block_lba.checked_mul(sectors_per_block)
        .and_then(|off| start_lba.checked_add(off))
    { Some(lba) => lba, None => return false };
    unsafe {
        ((*real_bio_ptr).read_blocks)(
            real_bio_ptr, real_media_id,
            disk_lba, block_size as usize,
            dst.as_mut_ptr().add(dst_offset) as *mut c_void,
        ) == EFI_SUCCESS
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  IsoMedia — ISO backed by a real disk extent
// ═══════════════════════════════════════════════════════════════════════════

pub struct IsoMedia {
    pub iso_lba: u64,
    pub real_bio_ptr: *mut BlockIoProtocol,
    pub real_media_id: u32,
    pub total_blocks: u64,
}

impl VirtualMedia for IsoMedia {
    fn read_block(&self, block_lba: u64, dst: &mut [u8], dst_offset: usize) -> bool {
        read_block_at(self.real_bio_ptr, self.real_media_id, block_lba, self.iso_lba, 2048, self.total_blocks, dst, dst_offset)
    }
    fn block_count(&self) -> u64 { self.total_blocks }
    fn block_size(&self) -> u32 { 2048 }
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
        let bs = self.sector_size.max(512);
        read_block_at(self.real_bio_ptr, self.real_media_id, block_lba, self.start_lba, bs, self.total_blocks, dst, dst_offset)
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
        let bs = self.sector_size.max(512);
        read_block_at(self.real_bio_ptr, self.real_media_id, block_lba, self.data_lba, bs, self.total_blocks, dst, dst_offset)
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

        // ── Directory entry patches ──────────────────────────────
        // Both grub.cfg and initrd directory entries may be in the same sector,
        // so we need to read the sector once and apply both patches if needed.
        let needs_grub_patch = vbio.dir_entry_patched && block_lba == vbio.dir_entry_sector as u64;
        let needs_initrd_patch = vbio.initrd_ext_active && block_lba == vbio.initrd_entry_sector as u64;

        if needs_grub_patch || needs_initrd_patch {
            if !read_real_iso_sector(vbio, block_lba, dst, block_offset) { return EFI_DEVICE_ERROR; }

            if needs_grub_patch {
                patch_dir_entry(&mut dst[block_offset..block_offset + 2048],
                    vbio.dir_entry_offset as usize, vbio.dir_entry_new_extent, vbio.dir_entry_new_size);
            }

            if needs_initrd_patch {
                // Calculate total size: original initrd + CPIO extension, aligned to sector boundary
                let initrd_sectors = (vbio.initrd_orig_size as u64 + 2047) / 2048;
                let new_size = (initrd_sectors + vbio.initrd_ext_sectors as u64) * 2048;
                patch_dir_entry(&mut dst[block_offset..block_offset + 2048],
                    vbio.initrd_entry_offset as usize, vbio.initrd_base_lba, new_size as u32);
            }
            continue;
        }

        // ── Initrd extension: redirect reads beyond original initrd to CPIO buffer ──
        // When GRUB reads the extended initrd, sectors beyond the original file
        // should come from the CPIO buffer, not from the actual ISO sectors
        // (which may contain unrelated files).
        if vbio.initrd_ext_active {
            let initrd_sectors = (vbio.initrd_orig_size as u64 + 2047) / 2048;
            let initrd_end = vbio.initrd_base_lba as u64 + initrd_sectors;
            let ext_end = initrd_end + vbio.initrd_ext_sectors as u64;
            // If GRUB is reading the CPIO extension region
            if block_lba >= initrd_end && block_lba < ext_end {
                let off = (block_lba - initrd_end) as usize * 2048;
                let cpio = unsafe { core::slice::from_raw_parts(vbio.premount_cpio_buf as *const u8, vbio.premount_cpio_size as usize) };
                let to_copy = (vbio.premount_cpio_size as usize).saturating_sub(off).min(2048);
                if to_copy > 0 {
                    dst[block_offset..block_offset + to_copy].copy_from_slice(&cpio[off..off + to_copy]);
                }
                // Zero-fill remainder of the sector
                for j in (block_offset + to_copy)..(block_offset + 2048) {
                    dst[j] = 0;
                }
                continue;
            }
        }

        // ── Memory-backed patches (grub.cfg content) ────────────────
        if serve_memory_sector(vbio.patched_file_buf, vbio.patched_file_sectors, vbio.patched_file_sector, block_lba, dst, block_offset)
        {
            continue;
        }

        // ── Read from real ISO disc ─────────────────────────────────
        if !read_real_iso_sector(vbio, block_lba, dst, block_offset) { return EFI_DEVICE_ERROR; }

        // ── PVD patches (sector 16) ────────────────────────────────
        if block_lba == 16 {
            let new_vol_size = (vbio.media.bim_lb + 1) as u32;
            let off = block_offset;
            // Volume Space Size: bytes 80-83 (LE), 84-87 (BE)
            dst[off + 80..off + 84].copy_from_slice(&new_vol_size.to_le_bytes());
            dst[off + 84..off + 88].copy_from_slice(&new_vol_size.to_be_bytes());

            // If initrd extension is active, update initrd file's
            // PVD root directory record size so GRUB can find it.
            // (The actual initrd directory entry is patched in its
            //  parent directory sector, not in PVD root record.)
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

    // Initialize initrd extension fields
    vbio.premount_cpio_buf = core::ptr::null_mut();
    vbio.premount_cpio_size = 0;
    vbio.initrd_base_lba = 0;
    vbio.initrd_orig_size = 0;
    vbio.initrd_ext_sectors = 0;
    vbio.initrd_cpio_start_lba = 0;
    vbio.initrd_entry_sector = 0;
    vbio.initrd_entry_offset = 0;
    vbio.initrd_ext_active = false;

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