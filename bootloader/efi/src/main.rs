#![no_std]
#![no_main]

use core::panic::PanicInfo;

const EFI_SUCCESS: usize = 0;
const EFI_BUFFER_TOO_SMALL: usize = 0x8000000000000005usize;

#[no_mangle]
extern "efiapi" fn efi_main(
    image_handle: *mut core::ffi::c_void,
    system_table: *mut SystemTable,
) -> ! {
    let st = unsafe { &mut *system_table };
    let bs = unsafe { &mut *st.boot_services };

    banner(st);

    // Find a whole-disk Block I/O handle
    let disk_handle = match find_disk_handle(bs, image_handle) {
        Some(h) => h,
        None => die(st, b"ERROR: No disk device found.\r\n\0"),
    };

    // Get Block I/O protocol on the disk handle
    let mut bio: *mut BlockIoProtocol = core::ptr::null_mut();
    if unsafe {
        (bs.handle_protocol)(
            disk_handle,
            &BLOCK_IO_PROTOCOL_GUID,
            &mut bio as *mut _ as _,
        )
    } != EFI_SUCCESS
        || bio.is_null()
    {
        die(st, b"ERROR: No Block I/O on disk.\r\n\0");
    }
    let bio_ptr = bio;
    let bio_ref = unsafe { &*bio };
    let mid = if !bio_ref.media.is_null() {
        unsafe { (*bio_ref.media).mid }
    } else {
        0
    };

    // Read MBR (LBA 0)
    let mut mbr: [u8; 512] = [0; 512];
    if unsafe { (bio_ref.read_blocks)(bio_ptr, mid, 0, 512, mbr.as_mut_ptr() as _) } != EFI_SUCCESS
    {
        die(st, b"ERROR: Cannot read MBR.\r\n\0");
    }

    // Find partition 1 — try MBR first, then GPT fallback
    let mut part1_lba: u64 = 0;
    let mut is_gpt = false;
    for i in 0..4 {
        let off = 446 + i * 16;
        let fs = mbr[off + 4];
        let lba = u32::from_le_bytes([mbr[off + 8], mbr[off + 9], mbr[off + 10], mbr[off + 11]]);
        let sec = u32::from_le_bytes([mbr[off + 12], mbr[off + 13], mbr[off + 14], mbr[off + 15]]);
        if fs == 0xEE && sec > 0 {
            is_gpt = true;
        }
        if sec == 0 || fs == 0xEE {
            continue;
        }
        part1_lba = lba as u64;
        break;
    }

    if part1_lba == 0 && is_gpt {
        // GPT protective MBR — search GPT partition table for Basic Data Partition
        print_raw(st, b"GPT detected, searching for data partition...\r\n\0");
        part1_lba = find_gpt_data_partition(st, bio_ref, bio_ptr, mid);
    }

    if part1_lba == 0 {
        die(st, b"ERROR: No partition 1 found.\r\n\0");
    }

    // Read partition 1 VBR
    let mut vbr: [u8; 512] = [0; 512];
    if unsafe { (bio_ref.read_blocks)(bio_ptr, mid, part1_lba, 512, vbr.as_mut_ptr() as _) }
        != EFI_SUCCESS
    {
        die(st, b"ERROR: Cannot read partition 1.\r\n\0");
    }

    // Parse exFAT
    if &vbr[3..11] != b"EXFAT   " {
        print_raw(st, b"Partition 1 is not exFAT. First 16 bytes:\r\n\0");
        print_hex(st, b"  ", u64::from_le_bytes(vbr[0..8].try_into().unwrap()));
        print_hex(st, b" ", u64::from_le_bytes(vbr[8..16].try_into().unwrap()));
        print_raw(st, b"\r\n\0");
        die(st, b"Only exFAT is supported.\r\n\0");
    }

    // vbr[0x6D] = SectorsPerClusterShift (NOT vbr[0x6C]=BytesPerSectorShift)
    let spc_shift = vbr[109] as u32;
    if spc_shift >= 25 {
        die(st, b"ERROR: Invalid SectorsPerClusterShift.\r\n\0");
    }
    let cluster_bytes = (1u32 << spc_shift) * 512;
    let fat_off = u32::from_le_bytes([vbr[80], vbr[81], vbr[82], vbr[83]]) as u64;
    let fat_len = u32::from_le_bytes([vbr[84], vbr[85], vbr[86], vbr[87]]) as u64;
    let heap_off = u32::from_le_bytes([vbr[88], vbr[89], vbr[90], vbr[91]]) as u64;
    let root_cluster = u32::from_le_bytes([vbr[96], vbr[97], vbr[98], vbr[99]]);

    let fat_start = part1_lba + fat_off;
    let heap_start = part1_lba + heap_off;
    let sec_per_cluster = cluster_bytes / 512;

    // Debug: print exFAT parameters
    print_raw(st, b"exFAT: spc=");
    print_hex(st, b"0x", sec_per_cluster as u64);
    print_raw(st, b" root_cl=");
    print_hex(st, b"0x", root_cluster as u64);
    print_raw(st, b" fat_off=");
    print_hex(st, b"0x", fat_off);
    print_raw(st, b" heap_off=");
    print_hex(st, b"0x", heap_off);
    print_raw(st, b" part1_lba=");
    print_hex(st, b"0x", part1_lba);
    print_raw(st, b"\r\n\0");

    // Save root cluster for re-scan support
    unsafe {
        ROOT_CLUSTER = root_cluster;
    }

    // Scan root directory for .iso files
    let mut iso_count: usize = 0;
    let mut iso_files: [IsoEntry; 64] = unsafe { core::mem::zeroed() };
    scan_exfat_dir(
        bio_ref,
        bio_ptr,
        mid,
        root_cluster,
        sec_per_cluster,
        fat_start,
        fat_len,
        heap_start,
        &mut iso_files,
        &mut iso_count,
    );

    // Show menu
    show_menu(
        st,
        &iso_files,
        iso_count,
        part1_lba,
        sec_per_cluster,
        fat_start,
        fat_len,
        heap_start,
        bio_ref,
        bio_ptr,
        mid,
    );
    halt_or_reboot(st);
}

// ═══════════════════════════════════════════════════════════════════
//  exFAT directory scanner
// ═══════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
struct IsoEntry {
    name: [u8; 256],
    name_len: usize,
    start_cluster: u32,
    file_size: u64,
    file_size_bytes: u64,
}
unsafe impl core::marker::Send for IsoEntry {}
unsafe impl core::marker::Sync for IsoEntry {}

fn scan_exfat_dir(
    bio: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    root_cluster: u32,
    spc: u32,
    fat_start: u64,
    fat_len: u64,
    heap_start: u64,
    files: &mut [IsoEntry; 64],
    count: &mut usize,
) {
    let mut cluster = root_cluster;
    let mut buf: [u8; 512] = [0; 512];
    // Small carry-over buffer for entry sets that cross sector boundaries
    let mut carry: [u8; 608] = [0; 608];
    let mut carry_len: usize = 0;

    'outer: loop {
        for si in 0..spc {
            let lba = heap_start + (cluster as u64 - 2) * spc as u64 + si as u64;
            if unsafe { (bio.read_blocks)(bio_ptr, mid, lba, 512, buf.as_mut_ptr() as _) }
                != EFI_SUCCESS
            {
                return;
            }

            // Combine carry-over + new sector
            let total = carry_len + 512;
            // Use a small stack buffer — entry sets are at most 19*32=608 bytes
            let mut combined = [0u8; 1152]; // 640 + 512
            combined[..carry_len].copy_from_slice(&carry[..carry_len]);
            combined[carry_len..total].copy_from_slice(&buf);
            carry_len = 0;

            let entries = &combined[..total];
            let n_entries = total / 32;
            let mut pos = 0usize;
            while pos < n_entries {
                let etype = entries[pos * 32];
                if etype == 0 {
                    return;
                }
                if etype == 0x85 {
                    // File entry
                    let sec_count = entries[pos * 32 + 1] as usize;
                    let total_ents = 1 + sec_count;
                    if pos + total_ents > n_entries {
                        // Entry set spans into next sector
                        let rem = total - pos * 32;
                        if rem <= carry.len() {
                            carry[..rem].copy_from_slice(&entries[pos * 32..]);
                            carry_len = rem;
                        } else {
                            carry_len = 0;
                        }
                        break; // continue to next sector
                    }
                    // Stream extension follows at pos+1
                    let stream_off = (pos + 1) * 32;
                    if entries[stream_off] != 0xC0 {
                        pos += total_ents;
                        continue;
                    }
                    let start_cl = u32::from_le_bytes([
                        entries[stream_off + 20],
                        entries[stream_off + 21],
                        entries[stream_off + 22],
                        entries[stream_off + 23],
                    ]);
                    let fsize = u64::from_le_bytes([
                        entries[stream_off + 24],
                        entries[stream_off + 25],
                        entries[stream_off + 26],
                        entries[stream_off + 27],
                        entries[stream_off + 28],
                        entries[stream_off + 29],
                        entries[stream_off + 30],
                        entries[stream_off + 31],
                    ]);
                    let name_ents = (sec_count as usize).saturating_sub(1);
                    let mut name_len = entries[stream_off + 3] as usize;
                    let mut name_buf = [0u8; 256];
                    let mut name_pos = 0usize;

                    for ne in 0..name_ents {
                        let noff = (pos + 2 + ne) * 32;
                        if entries[noff] != 0xC1 {
                            break;
                        }
                        let to_copy = (name_len - name_pos).min(15);
                        if to_copy == 0 {
                            break;
                        }
                        // UTF-16LE → ASCII
                        for j in 0..to_copy {
                            let lo = entries[noff + 2 + j * 2];
                            let hi = entries[noff + 3 + j * 2];
                            let cp = lo as u16 | ((hi as u16) << 8);
                            if cp == 0 {
                                break;
                            }
                            name_buf[name_pos + j] = if cp < 0x80 { cp as u8 } else { b'?' };
                        }
                        name_pos += to_copy;
                    }

                    if name_pos >= 4
                        && name_buf[name_pos - 4..name_pos].eq_ignore_ascii_case(b".iso")
                        && *count < 64
                    {
                        files[*count] = IsoEntry {
                            name: name_buf,
                            name_len: name_pos,
                            start_cluster: start_cl,
                            file_size: fsize,
                            file_size_bytes: fsize,
                        };
                        *count += 1;
                    }
                    pos += total_ents;
                    continue;
                }
                pos += 1;
            }
        }
        // Next cluster in FAT chain
        let fat_entry_lba = fat_start + (cluster as u64 * 4 / 512);
        let fat_entry_off = (cluster as usize * 4) % 512;
        let mut fat_buf: [u8; 512] = [0; 512];
        if unsafe { (bio.read_blocks)(bio_ptr, mid, fat_entry_lba, 512, fat_buf.as_mut_ptr() as _) }
            != EFI_SUCCESS
        {
            return;
        }
        let next = u32::from_le_bytes([
            fat_buf[fat_entry_off],
            fat_buf[fat_entry_off + 1],
            fat_buf[fat_entry_off + 2],
            fat_buf[fat_entry_off + 3],
        ]);
        if next < 2 || next >= 0xFFFFFFF8 {
            break;
        }
        cluster = next;
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Boot menu
// ═══════════════════════════════════════════════════════════════════

fn format_u64_buf(v: u64) -> ([u8; 20], usize) {
    let mut buf = [0u8; 20];
    if v == 0 {
        buf[0] = b'0';
        return (buf, 1);
    }
    let mut pos = 20;
    let mut n = v;
    while n > 0 && pos > 0 {
        pos -= 1;
        buf[pos] = (n % 10) as u8 + b'0';
        n /= 10;
    }
    (buf, 20 - pos)
}

fn show_menu(
    st: &mut SystemTable,
    files: &[IsoEntry; 64],
    count: usize,
    part1_lba: u64,
    spc: u32,
    fat_start: u64,
    fat_len: u64,
    heap_start: u64,
    bio: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
) {
    if count == 0 {
        print_raw(st, b"\r\nNo ISO files found on partition 1.\r\n\0");
        print_raw(st, b"Press any key to reboot.\r\n\0");
        return;
    }
    print_raw(st, b"\r\n=== Choosable UEFI Boot Menu ===\r\n\0");
    for i in 0..count.min(20) {
        let num = (i + 1) as u8;
        let num_s = [b' ', num + b'0', b'.', b' '];
        print_raw(st, &num_s);
        if files[i].name_len > 0 && files[i].name[0] != 0 {
            print_raw(st, &files[i].name[..files[i].name_len]);
        }
        let size_mb = files[i].file_size / (1024 * 1024);
        let (sb, sl) = format_u64_buf(size_mb);
        print_raw(st, b" (");
        print_raw(st, &sb[20 - sl..]);
        print_raw(st, b" MiB)\r\n\0");
    }
    print_raw(st, b"Enter number to boot (or 'r' to scan for .iso): \0");

    let bs = unsafe { &mut *st.boot_services };
    if !st.con_in.is_null() {
        let ci = unsafe { &mut *(st.con_in as *mut SimpleTextInput) };
        loop {
            let mut k = Key { sc: 0, uc: 0 };
            let status = unsafe { (ci.read_key_stroke)(ci as *mut _, &mut k) };
            if status != EFI_SUCCESS {
                continue;
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
                    boot_iso(
                        st, files, idx, part1_lba, spc, fat_start, fat_len, heap_start, bio,
                        bio_ptr, mid,
                    );
                }
            } else if (b'0'..=b'9').contains(&ch) {
                let idx = (ch - b'0') as usize;
                if idx == 1 && count >= 10 {
                    if count >= 10 {
                        boot_iso(
                            st,
                            files,
                            idx + 9,
                            part1_lba,
                            spc,
                            fat_start,
                            fat_len,
                            heap_start,
                            bio,
                            bio_ptr,
                            mid,
                        );
                    }
                }
            } else if ch == b'r' || ch == b'R' {
                print_raw(st, b"\r\nRe-scanning...\r\n\0");
                let mut new_files: [IsoEntry; 64] = unsafe { core::mem::zeroed() };
                let mut new_count: usize = 0;
                scan_exfat_dir(
                    bio,
                    bio_ptr,
                    mid,
                    root_cluster_from_first_scan(),
                    spc,
                    fat_start,
                    fat_len,
                    heap_start,
                    &mut new_files,
                    &mut new_count,
                );
                show_menu(
                    st, &new_files, new_count, part1_lba, spc, fat_start, fat_len, heap_start, bio,
                    bio_ptr, mid,
                );
                return;
            }
        }
    }
}

static mut ROOT_CLUSTER: u32 = 0;
fn root_cluster_from_first_scan() -> u32 {
    unsafe { ROOT_CLUSTER }
}

fn boot_iso(
    st: &mut SystemTable,
    files: &[IsoEntry; 64],
    idx: usize,
    part1_lba: u64,
    spc: u32,
    fat_start: u64,
    fat_len: u64,
    heap_start: u64,
    bio: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
) -> ! {
    print_raw(st, b"\r\nBooting ISO...\r\n\0");
    let iso_lba = heap_start + (files[idx].start_cluster as u64 - 2) * spc as u64;

    let boot_rec_lba = iso_lba + 17 * 4;
    let mut brec: [u8; 512] = [0; 512];
    if unsafe { (bio.read_blocks)(bio_ptr, mid, boot_rec_lba, 512, brec.as_mut_ptr() as _) }
        != EFI_SUCCESS
    {
        print_raw(st, b"Failed to read Boot Record.\r\n\0");
        halt_or_reboot(st);
    }
    if &brec[1..6] != b"CD001" {
        print_raw(st, b"Invalid Boot Record.\r\n\0");
        halt_or_reboot(st);
    }

    let catalog_iso_lba = u32::from_le_bytes([brec[0x47], brec[0x48], brec[0x49], brec[0x4A]]);
    let catalog_lba = iso_lba + catalog_iso_lba as u64 * 4;

    let mut cat: [u8; 512] = [0; 512];
    if unsafe { (bio.read_blocks)(bio_ptr, mid, catalog_lba, 512, cat.as_mut_ptr() as _) }
        != EFI_SUCCESS
    {
        print_raw(st, b"Failed to read Boot Catalog.\r\n\0");
        halt_or_reboot(st);
    }

    let mut boot_image_lba: u64 = 0;
    let mut boot_sector_count: u16 = 4;
    let mut found = false;
    for i in 0..16 {
        let off = i * 32;
        if cat[off] == 0x88 || cat[off] == 0x90 {
            boot_sector_count = u16::from_le_bytes([cat[off + 6], cat[off + 7]]);
            if boot_sector_count == 0 {
                boot_sector_count = 4;
            }
            boot_image_lba = iso_lba
                + u32::from_le_bytes([cat[off + 8], cat[off + 9], cat[off + 10], cat[off + 11]])
                    as u64
                    * 4;
            found = true;
            break;
        }
    }
    if !found {
        print_raw(st, b"No bootable entry in catalog.\r\n\0");
        halt_or_reboot(st);
    }

    let bs = unsafe { &mut *st.boot_services };
    let mut pages: u64 = 0x100000;
    let status = unsafe {
        (bs.allocate_pages)(
            AllocateType::AllocateAddress,
            MemoryType::EfiLoaderData,
            1,
            &mut pages,
        )
    };
    if status != EFI_SUCCESS {
        pages = 0;
        let status = unsafe {
            (bs.allocate_pages)(
                AllocateType::AllocateAnyPages,
                MemoryType::EfiLoaderData,
                1,
                &mut pages,
            )
        };
        if status != EFI_SUCCESS {
            print_raw(st, b"AllocatePages failed.\r\n\0");
            halt_or_reboot(st);
        }
    }
    let dest = pages as *mut u8;
    let mut sector_buf: [u8; 512] = [0; 512];
    for s in 0..boot_sector_count {
        if unsafe {
            (bio.read_blocks)(
                bio_ptr,
                mid,
                boot_image_lba + s as u64,
                512,
                sector_buf.as_mut_ptr() as _,
            )
        } != EFI_SUCCESS
        {
            print_raw(st, b"Failed to read boot image.\r\n\0");
            halt_or_reboot(st);
        }
        for (j, &b) in sector_buf.iter().enumerate() {
            unsafe {
                *dest.add(s as usize * 512 + j) = b;
            }
        }
    }

    let cookie_ptr = 0x7B00usize as *mut u32;
    unsafe { *cookie_ptr = 0x544F4F42u32 };

    print_raw(st, b"Boot image loaded. Rebooting...\r\n\0");
    let rt = unsafe { &mut *st.runtime_services };
    unsafe { (rt.reset_system)(ResetType::ResetCold, 0, 0, core::ptr::null_mut()) };
    loop {
        unsafe { core::arch::asm!("hlt") }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Disk handle discovery
// ═══════════════════════════════════════════════════════════════════

/// Search GPT partition table for a Microsoft Basic Data Partition
/// (type GUID: EBD0A0A2-B9E5-4433-87C0-68B6B72699C7).
fn find_gpt_data_partition(
    st: &mut SystemTable,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
) -> u64 {
    let mut hdr_sec: [u8; 512] = [0; 512];
    if unsafe { (bio_ref.read_blocks)(bio_ptr, mid, 1, 512, hdr_sec.as_mut_ptr() as _) }
        != EFI_SUCCESS
    {
        return 0;
    }
    let hdr = &hdr_sec[..92];
    if &hdr[0..8] != b"EFI PART" {
        return 0;
    }
    let entries_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
    let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap());
    let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap());
    if sz == 0 || n == 0 {
        return 0;
    }

    // Basic Data GUID (EBD0A0A2-B9E5-4433-87C0-68B6B72699C7) — matches constants.rs GPT_TYPE_BASIC_DATA
    let basic_data_guid: [u8; 16] = [
        0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99,
        0xC7,
    ];

    let mut sec: [u8; 512] = [0; 512];
    for i in 0..n.min(128) {
        let eoff = i as usize * sz as usize;
        let lba = entries_lba + (eoff / 512) as u64;
        let boff = eoff % 512;
        if boff + 16 > 512 {
            continue;
        }
        if i as usize % (512usize / sz as usize) == 0 || i == 0 {
            if unsafe { (bio_ref.read_blocks)(bio_ptr, mid, lba, 512, sec.as_mut_ptr() as _) }
                != EFI_SUCCESS
            {
                break;
            }
        }
        if sec[boff..boff + 16] == basic_data_guid {
            let start_lba = u64::from_le_bytes(sec[boff + 32..boff + 40].try_into().unwrap());
            print_raw(st, b"Found GPT Basic Data at LBA ");
            print_hex(st, b"0x", start_lba);
            print_raw(st, b"\r\n\0");
            return start_lba;
        }
    }
    0
}

fn find_disk_handle(
    bs: &mut BootServices,
    _image_handle: *mut core::ffi::c_void,
) -> Option<*mut core::ffi::c_void> {
    // Locate all Block I/O handles and pick the first whole-disk one.
    // LoadedImageProtocol.device_handle often points to the ESP partition
    // handle (LogicalPartition=true), whose LBA 0 is a VBR, not the MBR.
    // Using Media.LogicalPartition ensures we read the real MBR/GPT table.
    let mut num: usize = 0;
    let mut buf: *mut *mut core::ffi::c_void = core::ptr::null_mut();
    if unsafe {
        (bs.locate_handle_buffer)(
            LocateSearchType::ByProtocol,
            &BLOCK_IO_PROTOCOL_GUID,
            core::ptr::null_mut(),
            &mut num,
            &mut buf,
        )
    } != EFI_SUCCESS
        || buf.is_null()
        || num == 0
    {
        return None;
    }

    let handles = unsafe { core::slice::from_raw_parts(buf, num) };
    let mut result: Option<*mut core::ffi::c_void> = None;

    for &h in handles {
        let mut bio: *mut BlockIoProtocol = core::ptr::null_mut();
        if unsafe {
            (bs.handle_protocol)(h, &BLOCK_IO_PROTOCOL_GUID, &mut bio as *mut _ as _)
        } != EFI_SUCCESS
            || bio.is_null()
        {
            continue;
        }
        let media = unsafe { &*((*bio).media) };
        // LogicalPartition=false → whole-disk device
        if !media.bim_lp {
            result = Some(h);
            break;
        }
    }

    unsafe { (bs.free_pool)(buf as *mut core::ffi::c_void) };
    result
}

// ═══════════════════════════════════════════════════════════════════
//  Output helpers
// ═══════════════════════════════════════════════════════════════════

fn banner(st: &mut SystemTable) {
    if !st.con_out.is_null() {
        let con = unsafe { &mut *(st.con_out as *mut SimpleTextOutput) };
        prints(con, b"\r\n========================================\r\n\0");
        prints(con, b"        Choosable UEFI Bootloader       \r\n\0");
        prints(con, b"========================================\r\n\0");
    }
}

fn prints(co: &mut SimpleTextOutput, s: &[u8]) {
    let mut buf = [0u16; 256];
    let len = s.len().min(255);
    for (i, &b) in s[..len].iter().enumerate() {
        buf[i] = b as u16;
    }
    buf[len] = 0;
    unsafe { (co.output_string)(co as *mut SimpleTextOutput, buf.as_ptr()) };
}

fn print_raw(st: &mut SystemTable, s: &[u8]) {
    if !st.con_out.is_null() {
        prints(unsafe { &mut *(st.con_out as *mut SimpleTextOutput) }, s);
    }
}
fn die(st: &mut SystemTable, s: &[u8]) -> ! {
    print_raw(st, s);
    halt_or_reboot(st);
}
fn print_hex(st: &mut SystemTable, prefix: &[u8], val: u64) {
    print_raw(st, prefix);
    for i in (0..16).rev() {
        print_raw(
            st,
            &[b"0123456789ABCDEF"[((val >> (i * 4)) & 0xF) as usize]],
        );
    }
}

fn system_reset(st: &mut SystemTable) -> ! {
    let rt = unsafe { &mut *st.runtime_services };
    unsafe { (rt.reset_system)(ResetType::ResetCold, 0, 0, core::ptr::null_mut()) };
    loop {
        unsafe { core::arch::asm!("hlt") }
    }
}

fn halt_or_reboot(st: &mut SystemTable) -> ! {
    let bs = unsafe { &mut *st.boot_services };
    if !st.con_in.is_null() {
        let ci = unsafe { &mut *(st.con_in as *mut SimpleTextInput) };
        print_raw(st, b"Press any key to reboot.\r\n\0");
        for _ in 0..300 {
            unsafe { (bs.stall)(100_000) };
            let mut k = Key { sc: 0, uc: 0 };
            if unsafe { (ci.read_key_stroke)(ci as *mut _, &mut k) } == EFI_SUCCESS {
                system_reset(st);
            }
        }
    }
    system_reset(st);
}
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("hlt") }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  UEFI Types
// ═══════════════════════════════════════════════════════════════════

#[repr(C)]
struct SystemTable {
    hdr: TableHeader,
    fw: *const u16,
    rev: u32,
    pad_st0: u32,
    cih: *mut core::ffi::c_void,
    con_in: *mut core::ffi::c_void,
    coh: *mut core::ffi::c_void,
    con_out: *mut core::ffi::c_void,
    seh: *mut core::ffi::c_void,
    stderr: *mut core::ffi::c_void,
    runtime_services: *mut RuntimeServices,
    boot_services: *mut BootServices,
    nte: usize,
    ct: *mut core::ffi::c_void,
}
#[repr(C)]
struct TableHeader {
    sig: u64,
    rev: u32,
    hsz: u32,
    crc32: u32,
    reserved: u32,
}
#[repr(C)]
struct SimpleTextOutput {
    sto_reset: unsafe extern "efiapi" fn(*mut Self, bool) -> usize,
    output_string: unsafe extern "efiapi" fn(*mut Self, *const u16) -> usize,
    sto_p10: *mut core::ffi::c_void,
    sto_p18: *mut core::ffi::c_void,
    sto_p20: *mut core::ffi::c_void,
    sto_p28: *mut core::ffi::c_void,
    sto_clear_screen: unsafe extern "efiapi" fn(*mut Self) -> usize,
    sto_p38: *mut core::ffi::c_void,
    sto_p40: *mut core::ffi::c_void,
    sto_p48: *mut core::ffi::c_void,
}
#[repr(C)]
struct SimpleTextInput {
    sti_reset: unsafe extern "efiapi" fn(*mut Self, bool) -> usize,
    read_key_stroke: unsafe extern "efiapi" fn(*mut Self, *mut Key) -> usize,
    sti_wait: *mut core::ffi::c_void,
}
#[repr(C)]
struct Key {
    sc: u16,
    uc: u16,
}
#[repr(C)]
struct BootServices {
    hdr: TableHeader,
    bs_18: *mut core::ffi::c_void, // 0x18 RaiseTPL
    bs_20: *mut core::ffi::c_void, // 0x20 RestoreTPL
    allocate_pages: unsafe extern "efiapi" fn(AllocateType, MemoryType, usize, *mut u64) -> usize, // 0x28
    bs_30: *mut core::ffi::c_void, // 0x30 FreePages
    bs_38: *mut core::ffi::c_void, // 0x38 GetMemoryMap
    bs_40: *mut core::ffi::c_void, // 0x40 AllocatePool
    free_pool: unsafe extern "efiapi" fn(*mut core::ffi::c_void) -> usize, // 0x48 FreePool
    bs_50: *mut core::ffi::c_void, // 0x50 CreateEvent
    bs_58: *mut core::ffi::c_void, // 0x58 SetTimer
    bs_60: *mut core::ffi::c_void, // 0x60 WaitForEvent
    bs_68: *mut core::ffi::c_void, // 0x68 SignalEvent
    bs_70: *mut core::ffi::c_void, // 0x70 CloseEvent
    bs_78: *mut core::ffi::c_void, // 0x78 CheckEvent
    bs_80: *mut core::ffi::c_void, // 0x80 InstallProtocolInterface
    bs_88: *mut core::ffi::c_void, // 0x88 ReinstallProtocolInterface
    bs_90: *mut core::ffi::c_void, // 0x90 UninstallProtocolInterface
    handle_protocol: unsafe extern "efiapi" fn(
        *mut core::ffi::c_void,
        *const Guid,
        *mut *mut core::ffi::c_void,
    ) -> usize, // 0x98
    bs_a0: *mut core::ffi::c_void, // 0xA0 Reserved
    bs_a8: *mut core::ffi::c_void, // 0xA8 RegisterProtocolNotify
    bs_b0: *mut core::ffi::c_void, // 0xB0 LocateHandle
    bs_b8: *mut core::ffi::c_void, // 0xB8 LocateDevicePath
    bs_c0: *mut core::ffi::c_void, // 0xC0 InstallConfigurationTable
    bs_c8: *mut core::ffi::c_void, // 0xC8 LoadImage
    bs_d0: *mut core::ffi::c_void, // 0xD0 StartImage
    bs_d8: *mut core::ffi::c_void, // 0xD8 Exit
    bs_e0: *mut core::ffi::c_void, // 0xE0 UnloadImage
    bs_e8: *mut core::ffi::c_void, // 0xE8 ExitBootServices
    bs_f0: *mut core::ffi::c_void, // 0xF0 GetNextMonotonicCount
    stall: unsafe extern "efiapi" fn(usize) -> usize, // 0xF8 Stall
    bs_100: *mut core::ffi::c_void, // 0x100 SetWatchdogTimer
    bs_108: *mut core::ffi::c_void, // 0x108 ConnectController
    bs_110: *mut core::ffi::c_void, // 0x110 DisconnectController
    bs_118: *mut core::ffi::c_void, // 0x118 OpenProtocol
    bs_120: *mut core::ffi::c_void, // 0x120 CloseProtocol
    bs_128: *mut core::ffi::c_void, // 0x128 OpenProtocolInformation
    bs_130: *mut core::ffi::c_void, // 0x130 ProtocolsPerHandle
    locate_handle_buffer: unsafe extern "efiapi" fn(
        LocateSearchType,
        *const Guid,
        *mut core::ffi::c_void,
        *mut usize,
        *mut *mut *mut core::ffi::c_void,
    ) -> usize, // 0x138
    bs_140: *mut core::ffi::c_void, // 0x140 LocateProtocol
    bs_148: *mut core::ffi::c_void, // 0x148 InstallMultipleProtocolInterfaces
    bs_150: *mut core::ffi::c_void, // 0x150 UninstallMultipleProtocolInterfaces
    bs_158: *mut core::ffi::c_void, // 0x158 CalculateCrc32
    bs_160: *mut core::ffi::c_void, // 0x160 CopyMem
    bs_168: *mut core::ffi::c_void, // 0x168 SetMem
    bs_170: *mut core::ffi::c_void, // 0x170 CreateEventEx
}
#[repr(C)]
struct RuntimeServices {
    hdr: TableHeader,
    rs_18: *mut core::ffi::c_void,
    rs_20: *mut core::ffi::c_void,
    rs_28: *mut core::ffi::c_void,
    rs_30: *mut core::ffi::c_void,
    rs_38: *mut core::ffi::c_void,
    rs_40: *mut core::ffi::c_void,
    rs_48: *mut core::ffi::c_void,
    rs_50: *mut core::ffi::c_void,
    rs_58: *mut core::ffi::c_void,
    rs_60: *mut core::ffi::c_void,
    reset_system: unsafe extern "efiapi" fn(ResetType, usize, usize, *mut core::ffi::c_void) -> !,
}
#[repr(C)]
struct BlockIoProtocol {
    bio_rev: u64,
    media: *mut BlockIoMedia,
    bio_rst: *mut core::ffi::c_void,
    read_blocks:
        unsafe extern "efiapi" fn(*mut Self, u32, u64, usize, *mut core::ffi::c_void) -> usize,
    bio_w: *mut core::ffi::c_void,
    bio_f: *mut core::ffi::c_void,
}
#[repr(C)]
struct BlockIoMedia {
    mid: u32,
    bim_rm: bool,
    bim_mp: bool,
    bim_lp: bool,
    bim_ro: bool,
    bim_wc: bool,
    bim_bs: u32,
    bim_ia: u32,
    bim_lb: u64,
}
#[repr(C)]
struct Guid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}
#[repr(C)]
struct LoadedImageProtocol {
    _rev: u32,
    _p: *mut core::ffi::c_void,
    _st: *mut core::ffi::c_void,
    device_handle: *mut core::ffi::c_void,
}
#[derive(Clone, Copy)]
#[repr(u32)]
enum ResetType {
    ResetCold = 0,
}
#[derive(Clone, Copy)]
#[repr(u32)]
enum LocateSearchType {
    ByProtocol = 2,
}
#[derive(Clone, Copy)]
#[repr(u32)]
enum AllocateType {
    AllocateAnyPages = 0,
    AllocateAddress = 2,
}
#[derive(Clone, Copy)]
#[repr(u32)]
enum MemoryType {
    EfiLoaderData = 1,
}

const LOADED_IMAGE_PROTOCOL_GUID: Guid = Guid {
    d1: 0x5B1B31A1,
    d2: 0x9562,
    d3: 0x11D2,
    d4: [0x8E, 0x3F, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B],
};
const SIMPLE_FILE_SYSTEM_GUID: Guid = Guid {
    d1: 0x0964e5b2,
    d2: 0x6459,
    d3: 0x11d2,
    d4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};
const BLOCK_IO_PROTOCOL_GUID: Guid = Guid {
    d1: 0x964e5b21,
    d2: 0x6459,
    d3: 0x11d2,
    d4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};
