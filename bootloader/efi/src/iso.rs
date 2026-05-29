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
use crate::output::{die, format_u64_buf, halt_or_reboot, print_raw};
use crate::protocol::{
    AllocateType, BlockIoProtocol, BootServices, LoadedImageProtocol, MemoryType, SystemTable,
    DevicePathProtocol, VirtualBlockIo, EFI_SUCCESS, LOADED_IMAGE_PROTOCOL_GUID, BLOCK_IO_PROTOCOL_GUID,
};

// ═══════════════════════════════════════════════════════════════════════════
//  ISO9660 on-disk structures
// ═══════════════════════════════════════════════════════════════════════════

/// ISO9660 directory record (variable-length, minimum 34 bytes)
#[repr(C, packed)]
struct DirRecordHdr {
    len: u8,
    ext_attr_len: u8,
    extent: [u8; 8],
    size: [u8; 8],
    _date: [u8; 7],
    flags: u8,
    _unit_size: u8,
    _gap_size: u8,
    _vol_seq: [u8; 4],
    name_len: u8,
}

fn extent_le(extent: &[u8; 8]) -> u32 {
    u32::from_le_bytes([extent[0], extent[1], extent[2], extent[3]])
}

fn size_le(size: &[u8; 8]) -> u32 {
    u32::from_le_bytes([size[0], size[1], size[2], size[3]])
}

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

    let mut offset: usize = 0;
    for s in 0..sector_count {
        let mut iso_sec = [0u8; 2048];
        if !read_iso_sector(bio_ref, bio_ptr, mid, iso_lba, lba + s, &mut iso_sec) {
            unsafe { (bs.free_pool)(ptr); }
            return None;
        }
        let to_copy = if offset + 2048 > buf_len { buf_len - offset } else { 2048 };
        unsafe {
            core::ptr::copy_nonoverlapping(iso_sec.as_ptr(), ptr_u8.add(offset), to_copy);
        }
        offset += to_copy;
    }
    Some((ptr_u8, byte_len.min(buf_len as u32)))
}

// ═══════════════════════════════════════════════════════════════════════════
//  ISO9660 directory walker
// ═══════════════════════════════════════════════════════════════════════════

/// Get root directory record from the Primary Volume Descriptor (sector 16).
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
    // Root directory record at offset 156 in PVD
    let hdr: &DirRecordHdr = unsafe { &*(pvd[156..].as_ptr() as *const DirRecordHdr) };
    Some((extent_le(&hdr.extent), size_le(&hdr.size)))
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
    let total_sectors = ((dir_size as u64 + 2047) / 2048) as u32;
    for s in 0..total_sectors {
        if !read_iso_sector(bio_ref, bio_ptr, mid, iso_lba, dir_lba + s, scratch) {
            return None;
        }
        let mut offset: usize = 0;
        while offset + core::mem::size_of::<DirRecordHdr>() <= 2048 && offset < dir_size as usize {
            let hdr: &DirRecordHdr =
                unsafe { &*(scratch[offset..].as_ptr() as *const DirRecordHdr) };
            if hdr.len == 0 {
                break;
            }
            let name_len = hdr.name_len as usize;
            let name_offset = offset + core::mem::size_of::<DirRecordHdr>();
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
                    return Some((extent_le(&hdr.extent), size_le(&hdr.size)));
                }
            }
            offset += hdr.len as usize;
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

// ═══════════════════════════════════════════════════════════════════════════
//  UEFI chainload
// ═══════════════════════════════════════════════════════════════════════════

/// Build a minimal DevicePath for the ISO's EFI executable:
///   HD(partition1) / File("\\EFI\\BOOT\\BOOTX64.EFI")
/// Returns a pool-allocated pointer, or null on failure.
fn build_iso_device_path(
    bs: &mut BootServices,
    part1_lba: u64,
) -> *mut c_void {
    // ── Hard Drive media device path node (42 bytes) ──────
    const HD_NODE: [u8; 42] = {
        let mut n = [0u8; 42];
        n[0] = 0x04; // Type: MEDIA_DEVICE_PATH
        n[1] = 0x01; // SubType: HARD_DRIVE
        // Length (2 bytes LE) = 42 → 0x002A
        n[2] = 0x2A;
        n[3] = 0x00;
        // PartitionNumber = 1 (bytes 4-7)
        n[4] = 1;
        // n[5..7] = 0
        // PartitionStart (bytes 8-15, placeholder)
        // PartitionSize (bytes 16-23, 0 = unknown)
        // PartitionSignature (bytes 24-39, zero)
        // PartitionFormat = 0x02 (GPT) at byte 40
        n[40] = 0x02;
        // SignatureType = 0x00 (None) at byte 41
        n
    };

    // ── File path node (48 bytes = 4 header + 44 UCS-2) ───
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

    let total = HD_NODE.len() + 4 + file_name.len() * 2 + END_NODE.len();
    // = 42 + 4 + 44 + 4 = 94

    let mut ptr: *mut c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.allocate_pool)(MemoryType::EfiLoaderData, total, &mut ptr)
    };
    if status != EFI_SUCCESS || ptr.is_null() {
        return core::ptr::null_mut();
    }
    let dp = ptr as *mut u8;

    unsafe {
        // HD node
        let mut off = 0usize;
        dp.copy_from_nonoverlapping(HD_NODE.as_ptr(), HD_NODE.len());
        off += HD_NODE.len();
        // Patch PartitionStart (bytes 8-15) with actual partition LBA
        *(dp.add(8) as *mut u64) = part1_lba.to_le();

        // File path node header
        dp.add(off).write_volatile(0x04u8); // Type: MEDIA_DEVICE_PATH
        off += 1;
        dp.add(off).write_volatile(0x04u8); // SubType: FILE_PATH
        off += 1;
        let file_bytes = (file_name.len() * 2) as u16; // 44 bytes including null
        *(dp.add(off) as *mut u16) = (4 + file_bytes).to_le(); // total node length
        off += 2;
        // File path body
        core::ptr::copy_nonoverlapping(file_name.as_ptr() as *const u8, dp.add(off), file_name.len() * 2);
        off += file_name.len() * 2;

        // End node
        dp.add(off).copy_from_nonoverlapping(END_NODE.as_ptr(), END_NODE.len());
    }

    ptr
}

/// Load and start an EFI executable from within an ISO.
///
/// This is the **correct** UEFI chainload path.  No real-mode transition,
/// no BIOS INT 13h dependency.
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

    // ── Create virtual CD-ROM from the ISO file ──────────────────────
    let cdrom_tuple = crate::virtual_blockio::create_virtual_cdrom(
        bs, iso_lba, bio_ptr, mid, iso_size,
    );
    let (device_handle, cdrom_dp) = match cdrom_tuple {
        Some((h, dp)) => (h, dp),
        None => {
            print_raw(st, b"ERROR: Failed to create virtual CD-ROM.\r\n\0");
            return;
        }
    };

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
    let device_path = build_iso_device_path(bs, part1_lba);

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
                // Use CD-ROM DevicePath as FilePath for proper boot device identification
                // (cdrom_dp is the device path of the CD-ROM itself;
                //  device_path is the HD+FilePath node for /EFI/BOOT/BOOTX64.EFI.
                //  Both are valid; we use the file path for LoadedImageProtocol.)
                (*lip).file_path = device_path;
                if !cdrom_dp.is_null() {
                    let _ = cdrom_dp; // already installed on the handle
                }
            }
        }
    } else {
        unsafe { (bs.free_pool)(buf_ptr as _); }
        print_raw(st, b"ERROR: LoadImage failed.\r\n\0");
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
/// Never returns on success; returns on failure so the menu can continue.
pub fn boot_iso(
    st: &mut SystemTable,
    image_handle: *mut c_void,
    disk_handle: *mut c_void,
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
        } else {
            match k.sc {
                0x1C => b'\n',
                0x13 => b'r',
                0x1F => b'R',
                _ => 0x00,
            }
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