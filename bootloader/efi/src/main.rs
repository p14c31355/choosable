#![no_std]
#![no_main]

use core::panic::PanicInfo;

const EFI_SUCCESS: usize = 0;
const EFI_BUFFER_TOO_SMALL: usize = 0x8000000000000005usize;

// ── Filesystem type ─────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq)]
enum FsType {
    Exfat,
    Fat32,
    Ntfs,
}

// ── Filesystem context (kept for re-scanning) ──────────────────────
struct FsCtx {
    fs: FsType,
    part1_lba: u64,
    // exFAT / FAT32
    spc: u32,
    fat_start: u64,
    fat_len: u64,
    heap_start: u64,
    root_cluster: u32,
    // NTFS
    mft_start_lba: u64,
    sectors_per_cluster: u32,
    bytes_per_cluster: u64,
    mft_record_size: u64,
}

#[no_mangle]
extern "efiapi" fn efi_main(
    image_handle: *mut core::ffi::c_void,
    system_table: *mut SystemTable,
) -> ! {
    let st = unsafe { &mut *system_table };
    let bs = unsafe { &mut *st.boot_services };

    banner(st);

    let disk_handle = match find_disk_handle(bs, image_handle) {
        Some(h) => h,
        None => die(st, b"ERROR: No disk device found.\r\n\0"),
    };

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

    // Read MBR
    let mut mbr: [u8; 512] = [0; 512];
    if unsafe { (bio_ref.read_blocks)(bio_ptr, mid, 0, 512, mbr.as_mut_ptr() as _) } != EFI_SUCCESS
    {
        die(st, b"ERROR: Cannot read MBR.\r\n\0");
    }

    // Find partition 1
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

    // Detect filesystem type
    let fs = if &vbr[3..11] == b"EXFAT   " {
        FsType::Exfat
    } else if &vbr[3..11] == b"NTFS    " {
        FsType::Ntfs
    } else if &vbr[0x52..0x5A] == b"FAT32   " {
        FsType::Fat32
    } else {
        // Fallback: check FAT32 at 0x52
        if &vbr[0x52..0x5A] == b"FAT32   " {
            FsType::Fat32
        } else {
            print_raw(st, b"Unknown filesystem on partition 1.\r\n\0");
            print_hex(st, b"  First 16 bytes: ", u64::from_le_bytes(vbr[0..8].try_into().unwrap()));
            print_hex(st, b"  ", u64::from_le_bytes(vbr[8..16].try_into().unwrap()));
            print_raw(st, b"\r\n\0");
            halt_or_reboot(st);
        }
    };

    // Parse BPB
    let mut ctx = FsCtx {
        fs,
        part1_lba,
        spc: 0,
        fat_start: 0,
        fat_len: 0,
        heap_start: 0,
        root_cluster: 0,
        mft_start_lba: 0,
        sectors_per_cluster: 0,
        bytes_per_cluster: 0,
        mft_record_size: 0,
    };

    match fs {
        FsType::Exfat => {
            let spc_shift = vbr[109] as u32;
            if spc_shift > 16 {
                die(st, b"ERROR: Invalid SectorsPerClusterShift.\r\n\0");
            }
            let cluster_bytes = (1u32 << spc_shift) * 512;
            let fat_off = u32::from_le_bytes([vbr[80], vbr[81], vbr[82], vbr[83]]) as u64;
            let fat_len = u32::from_le_bytes([vbr[84], vbr[85], vbr[86], vbr[87]]) as u64;
            let heap_off = u32::from_le_bytes([vbr[88], vbr[89], vbr[90], vbr[91]]) as u64;
            let root_cluster = u32::from_le_bytes([vbr[96], vbr[97], vbr[98], vbr[99]]);

            ctx.spc = cluster_bytes / 512;
            ctx.fat_start = part1_lba + fat_off;
            ctx.fat_len = fat_len;
            ctx.heap_start = part1_lba + heap_off;
            ctx.root_cluster = root_cluster;

            print_raw(st, b"exFAT detected. Scanning...\r\n\0");
        }
        FsType::Fat32 => {
            let spc = vbr[13] as u32; // sectors per cluster
            if spc == 0 {
                die(st, b"ERROR: Invalid sectors per cluster.\r\n\0");
            }
            let reserved = u16::from_le_bytes([vbr[14], vbr[15]]) as u64;
            let num_fats = vbr[16] as u64;
            let fat_sectors = u32::from_le_bytes([vbr[36], vbr[37], vbr[38], vbr[39]]) as u64;
            let root_cluster = u32::from_le_bytes([vbr[44], vbr[45], vbr[46], vbr[47]]);

            let fat_start = part1_lba + reserved;
            let data_start = fat_start + num_fats * fat_sectors;

            ctx.spc = spc;
            ctx.fat_start = fat_start;
            ctx.fat_len = fat_sectors;
            ctx.heap_start = data_start;
            ctx.root_cluster = root_cluster;

            print_raw(st, b"FAT32 detected. Scanning...\r\n\0");
        }
        FsType::Ntfs => {
            let spc = vbr[13] as u32; // sectors per cluster
            if spc == 0 {
                die(st, b"ERROR: Invalid sectors per cluster.\r\n\0");
            }
            let cluster_bytes = spc as u64 * 512;
            // MFT start cluster is at offset 0x30 (48) in NTFS BPB
            let mft_lcn = i64::from_le_bytes(vbr[0x30..0x38].try_into().unwrap());
            let mft_start_lba = part1_lba + (mft_lcn as u64) * spc as u64;
            // MFT record size: clus_per_mft_record at offset 0x40 (64)
            let cpmr_raw = vbr[0x40] as i8;
            let mft_record_size: u64 = if cpmr_raw > 0 {
                cpmr_raw as u64 * cluster_bytes
            } else if cpmr_raw >= -12 {
                1u64 << (-cpmr_raw)
            } else {
                0
            };
            if mft_record_size == 0 || mft_record_size > 4096 {
                die(st, b"ERROR: Invalid MFT record size.\r\n\0");
            }

            ctx.spc = spc;
            ctx.sectors_per_cluster = spc;
            ctx.bytes_per_cluster = cluster_bytes;
            ctx.mft_start_lba = mft_start_lba;
            ctx.mft_record_size = mft_record_size;
            ctx.heap_start = part1_lba; // partition start (NTFS doesn't use heap_start)

            print_raw(st, b"NTFS detected. Scanning...\r\n\0");
        }
    }

    // Scan root directory
    let mut iso_count: usize = 0;
    let mut iso_files: [IsoEntry; 64] = unsafe { core::mem::zeroed() };
    scan_directory(bio_ref, bio_ptr, mid, &ctx, &mut iso_files, &mut iso_count);

    // Show menu
    show_menu(st, &iso_files, iso_count, &ctx, bio_ref, bio_ptr, mid);
    halt_or_reboot(st);
}

// ═══════════════════════════════════════════════════════════════════
//  Shared scan dispatcher
// ═══════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
struct IsoEntry {
    name: [u8; 256],
    name_len: usize,
    file_start_lba: u64,   // LBA of first sector of the ISO file
    file_size: u64,
    file_size_bytes: u64,
}
unsafe impl core::marker::Send for IsoEntry {}
unsafe impl core::marker::Sync for IsoEntry {}

fn scan_directory(
    bio: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    ctx: &FsCtx,
    files: &mut [IsoEntry; 64],
    count: &mut usize,
) {
    match ctx.fs {
        FsType::Exfat => {
            scan_exfat_dir(
                bio, bio_ptr, mid, ctx.root_cluster, ctx.spc, ctx.fat_start, ctx.fat_len,
                ctx.heap_start, files, count,
            );
        }
        FsType::Fat32 => {
            scan_fat32_dir(
                bio, bio_ptr, mid, ctx.root_cluster, ctx.spc, ctx.fat_start, ctx.heap_start,
                files, count,
            );
        }
        FsType::Ntfs => {
            scan_ntfs_dir(
                bio, bio_ptr, mid, ctx, files, count,
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  exFAT directory scanner
// ═══════════════════════════════════════════════════════════════════

fn scan_exfat_dir(
    bio: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    root_cluster: u32,
    spc: u32,
    fat_start: u64,
    _fat_len: u64,
    heap_start: u64,
    files: &mut [IsoEntry; 64],
    count: &mut usize,
) {
    let mut cluster = root_cluster;
    let mut buf: [u8; 512] = [0; 512];
    let mut carry: [u8; 608] = [0; 608];
    let mut carry_len: usize = 0;

    loop {
        for si in 0..spc {
            let lba = heap_start + (cluster as u64 - 2) * spc as u64 + si as u64;
            if unsafe { (bio.read_blocks)(bio_ptr, mid, lba, 512, buf.as_mut_ptr() as _) }
                != EFI_SUCCESS
            {
                return;
            }
            let total = carry_len + 512;
            let mut combined = [0u8; 1152];
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
                    let sec_count = entries[pos * 32 + 1] as usize;
                    let total_ents = 1 + sec_count;
                    if pos + total_ents > n_entries {
                        let rem = total - pos * 32;
                        if rem <= carry.len() {
                            carry[..rem].copy_from_slice(&entries[pos * 32..]);
                            carry_len = rem;
                        } else {
                            carry_len = 0;
                        }
                        break;
                    }
                    let stream_off = (pos + 1) * 32;
                    if entries[stream_off] != 0xC0 {
                        pos += total_ents;
                        continue;
                    }
                    let start_cl = u32::from_le_bytes([
                        entries[stream_off + 20], entries[stream_off + 21],
                        entries[stream_off + 22], entries[stream_off + 23],
                    ]);
                    let fsize = u64::from_le_bytes([
                        entries[stream_off + 24], entries[stream_off + 25],
                        entries[stream_off + 26], entries[stream_off + 27],
                        entries[stream_off + 28], entries[stream_off + 29],
                        entries[stream_off + 30], entries[stream_off + 31],
                    ]);
                    let name_ents = (sec_count).saturating_sub(1);
                    let name_len = entries[stream_off + 3] as usize;
                    let mut name_buf = [0u8; 256];
                    let mut name_pos = 0usize;

                    for ne in 0..name_ents {
                        let noff = (pos + 2 + ne) * 32;
                        if entries[noff] != 0xC1 {
                            break;
                        }
                        let to_copy = name_len.saturating_sub(name_pos).min(15).min(256 - name_pos);
                        if to_copy == 0 {
                            break;
                        }
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
                        let file_lba = heap_start + (start_cl as u64 - 2) * spc as u64;
                        files[*count] = IsoEntry {
                            name: name_buf, name_len: name_pos,
                            file_start_lba: file_lba, file_size: fsize, file_size_bytes: fsize,
                        };
                        *count += 1;
                    }
                    pos += total_ents;
                    continue;
                }
                pos += 1;
            }
        }
        // FAT chain
        let fat_entry_lba = fat_start + (cluster as u64 * 4 / 512);
        let fat_entry_off = (cluster as usize * 4) % 512;
        let mut fat_buf: [u8; 512] = [0; 512];
        if unsafe { (bio.read_blocks)(bio_ptr, mid, fat_entry_lba, 512, fat_buf.as_mut_ptr() as _) }
            != EFI_SUCCESS
        {
            return;
        }
        let next = u32::from_le_bytes([
            fat_buf[fat_entry_off], fat_buf[fat_entry_off + 1],
            fat_buf[fat_entry_off + 2], fat_buf[fat_entry_off + 3],
        ]);
        if next < 2 || next >= 0xFFFFFFF8 {
            break;
        }
        cluster = next;
    }
}

// ═══════════════════════════════════════════════════════════════════
//  FAT32 directory scanner
// ═══════════════════════════════════════════════════════════════════

fn fat32_read_sector(
    bio: &BlockIoProtocol, bio_ptr: *mut BlockIoProtocol, mid: u32,
    lba: u64, buf: &mut [u8; 512],
) -> bool {
    unsafe { (bio.read_blocks)(bio_ptr, mid, lba, 512, buf.as_mut_ptr() as _) == EFI_SUCCESS }
}

fn fat32_next_cluster(
    bio: &BlockIoProtocol, bio_ptr: *mut BlockIoProtocol, mid: u32,
    fat_start: u64, cluster: u32,
) -> u32 {
    let fat_entry_lba = fat_start + (cluster as u64 * 4 / 512);
    let fat_entry_off = (cluster as usize * 4) % 512;
    let mut fat_buf: [u8; 512] = [0; 512];
    if unsafe { (bio.read_blocks)(bio_ptr, mid, fat_entry_lba, 512, fat_buf.as_mut_ptr() as _) }
        != EFI_SUCCESS
    {
        return 0x0FFFFFFF;
    }
    u32::from_le_bytes([
        fat_buf[fat_entry_off], fat_buf[fat_entry_off + 1],
        fat_buf[fat_entry_off + 2], fat_buf[fat_entry_off + 3],
    ]) & 0x0FFFFFFF
}

fn scan_fat32_dir(
    bio: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    root_cluster: u32,
    spc: u32,
    fat_start: u64,
    data_start: u64,
    files: &mut [IsoEntry; 64],
    count: &mut usize,
) {
    if root_cluster == 0 {
        return;
    }
    let mut cluster = root_cluster;
    let mut buf = [0u8; 512];
    let mut lfn_buf = [0u8; 256];
    let mut lfn_len: usize = 0;
    let mut lfn_seq: u8 = 0;
    let mut lfn_checksum: u8 = 0;

    loop {
        for si in 0..spc {
            let lba = data_start + (cluster as u64 - 2) * spc as u64 + si as u64;
            if !fat32_read_sector(bio, bio_ptr, mid, lba, &mut buf) {
                return;
            }
            for e in 0..(512 / 32) {
                let off = e * 32;
                let first = buf[off];
                if first == 0 {
                    return;
                }
                if first == 0xE5 {
                    // Deleted entry; reset LFN state
                    if lfn_len > 0 {
                        lfn_len = 0;
                    }
                    continue;
                }
                let attr = buf[off + 11];
                if attr == 0x0F {
                    // LFN entry
                    let seq = first & 0x1F;
                    let is_last = first & 0x40;
                    let checksum = buf[off + 13];
                    if is_last != 0 {
                        // New LFN chain
                        lfn_seq = seq;
                        lfn_checksum = checksum;
                        lfn_len = 0;
                    }
                    if seq != lfn_seq || checksum != lfn_checksum {
                        continue; // skip out-of-order LFN
                    }
                    if lfn_seq == 0 {
                        continue;
                    }
                    lfn_seq -= 1;
                    let chars = [
                        buf[off + 1], buf[off + 3], buf[off + 5], buf[off + 7], buf[off + 9],
                        buf[off + 14], buf[off + 16], buf[off + 18], buf[off + 20],
                        buf[off + 22], buf[off + 24], buf[off + 28], buf[off + 30],
                    ];
                    for &c in &chars {
                        if c == 0x00 || c == 0xFF {
                            break;
                        }
                        if lfn_len < 255 && c < 0x80 {
                            lfn_buf[lfn_len] = c;
                            lfn_len += 1;
                        } else if lfn_len < 255 {
                            lfn_buf[lfn_len] = b'?';
                            lfn_len += 1;
                        }
                    }
                    continue;
                }
                // Regular entry
                if attr & 0x08 != 0 {
                    // Volume label, skip
                    if lfn_len > 0 {
                        lfn_len = 0;
                    }
                    continue;
                }
                // Determine name
                let mut name_buf = [0u8; 256];
                let mut name_len: usize;
                let lfn_valid = lfn_len > 0 && lfn_buf[0] != 0;

                if lfn_valid {
                    let copy = lfn_len.min(255);
                    name_buf[..copy].copy_from_slice(&lfn_buf[..copy]);
                    name_len = copy;
                } else {
                    // 8.3 short name
                    let mut nlen = 0;
                    for j in 0..8 {
                        let c = buf[off + j];
                        if c == 0x20 {
                            break;
                        }
                        if nlen < 255 {
                            name_buf[nlen] = c;
                            nlen += 1;
                        }
                    }
                    if buf[off + 8] != 0x20 {
                        if nlen < 255 {
                            name_buf[nlen] = b'.';
                            nlen += 1;
                        }
                        for j in 8..11 {
                            let c = buf[off + j];
                            if c == 0x20 {
                                break;
                            }
                            if nlen < 255 {
                                name_buf[nlen] = c;
                                nlen += 1;
                            }
                        }
                    }
                    name_len = nlen;
                }
                // reset LFN
                lfn_len = 0;

                // Check for .iso
                let is_iso = name_len >= 4
                    && name_buf[name_len - 4..name_len].eq_ignore_ascii_case(b".iso");

                if is_iso && *count < 64 {
                    let file_cl = u32::from_le_bytes([
                        buf[off + 20], buf[off + 21],
                        buf[off + 26], buf[off + 27],
                    ]);
                    let file_sz = u32::from_le_bytes([
                        buf[off + 28], buf[off + 29], buf[off + 30], buf[off + 31],
                    ]);
                    let file_lba = data_start + (file_cl as u64 - 2) * spc as u64;
                    files[*count] = IsoEntry {
                        name: name_buf, name_len,
                        file_start_lba: file_lba,
                        file_size: file_sz as u64,
                        file_size_bytes: file_sz as u64,
                    };
                    *count += 1;
                }
            }
        }
        let next = fat32_next_cluster(bio, bio_ptr, mid, fat_start, cluster);
        if next < 2 || next >= 0x0FFFFFF0 {
            break;
        }
        cluster = next;
    }
}

// ═══════════════════════════════════════════════════════════════════
//  NTFS directory scanner
// ═══════════════════════════════════════════════════════════════════

fn read_sector(
    bio: &BlockIoProtocol, bio_ptr: *mut BlockIoProtocol, mid: u32,
    lba: u64, buf: &mut [u8; 512],
) -> bool {
    unsafe { (bio.read_blocks)(bio_ptr, mid, lba, 512, buf.as_mut_ptr() as _) == EFI_SUCCESS }
}

fn scan_ntfs_dir(
    bio: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    ctx: &FsCtx,
    files: &mut [IsoEntry; 64],
    count: &mut usize,
) {
    // Read $MFT record 5 ($Root directory)
    let mft_rec_lba = ctx.mft_start_lba + 5 * (ctx.mft_record_size / 512);
    let mut rec_buf = [0u8; 4096];
    let rec_size = ctx.mft_record_size as usize;
    if rec_size > 4096 {
        return;
    }

    // Read MFT record
    for i in 0..(rec_size / 512) {
        let mut sector = [0u8; 512];
        if !read_sector(bio, bio_ptr, mid, mft_rec_lba + i as u64, &mut sector) {
            return;
        }
        let off = i * 512;
        rec_buf[off..off + 512].copy_from_slice(&sector);
    }

    // Fixup array: word at offset 4 tells us the fixup count
    let fixup_off = u16::from_le_bytes([rec_buf[4], rec_buf[5]]) as usize;
    let fixup_count = u16::from_le_bytes([rec_buf[6], rec_buf[7]]) as usize;
    if fixup_off > 0 && fixup_off < rec_size && fixup_count > 1 {
        let fixup_val = u16::from_le_bytes([rec_buf[fixup_off], rec_buf[fixup_off + 1]]);
        for i in 1..fixup_count {
            let pos = i * 512 - 2;
            if pos + 2 <= rec_size {
                rec_buf[pos] = fixup_val as u8;
                rec_buf[pos + 1] = (fixup_val >> 8) as u8;
            }
        }
    }

    // Parse attributes starting at offset of first attribute (word at 0x14)
    let attrs_off = u16::from_le_bytes([rec_buf[0x14], rec_buf[0x15]]) as usize;
    if attrs_off >= rec_size {
        return;
    }

    parse_ntfs_attrs(bio, bio_ptr, mid, ctx, &rec_buf[attrs_off..], rec_size - attrs_off, files, count);
}

fn parse_ntfs_attrs(
    bio: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    ctx: &FsCtx,
    attrs: &[u8],
    _remaining: usize,
    files: &mut [IsoEntry; 64],
    count: &mut usize,
) {
    let mut off = 0usize;
    while off + 4 < attrs.len() {
        let atype = u32::from_le_bytes([attrs[off], attrs[off + 1], attrs[off + 2], attrs[off + 3]]);
        if atype == 0xFFFFFFFF {
            break;
        }
        if atype == 0 {
            break;
        }
        let alen = if off + 7 < attrs.len() {
            u32::from_le_bytes([attrs[off + 4], attrs[off + 5], attrs[off + 6], attrs[off + 7]]) as usize
        } else {
            break;
        };
        if alen < 8 || off + alen > attrs.len() {
            break;
        }
        let is_nonresident = attrs[off + 8] != 0;
        let name_len = attrs[off + 9] as usize;
        let val_off = u16::from_le_bytes([attrs[off + 0x14], attrs[off + 0x15]]) as usize + name_len;

        if atype == 0x90 {
            // $INDEX_ROOT — parse directory entries
            if !is_nonresident && val_off < alen {
                let index_data = &attrs[off + val_off..off + alen];
                parse_ntfs_index_root(bio, bio_ptr, mid, ctx, index_data, files, count);
            }
        } else if atype == 0xA0 {
            // $INDEX_ALLOCATION — skip for now (directories with many files)
        }
        off += alen;
    }
}

fn parse_ntfs_index_root(
    bio: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    ctx: &FsCtx,
    data: &[u8],
    files: &mut [IsoEntry; 64],
    count: &mut usize,
) {
    if data.len() < 20 {
        return;
    }
    // Offset 0x10: entries offset (from $INDEX_ROOT attribute data start)
    let entries_off = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize + 0x10;
    if entries_off >= data.len() {
        return;
    }
    let entries = &data[entries_off..];
    parse_ntfs_index_entries(bio, bio_ptr, mid, ctx, entries, files, count);
}

fn parse_ntfs_index_entries(
    bio: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    ctx: &FsCtx,
    mut entries: &[u8],
    files: &mut [IsoEntry; 64],
    count: &mut usize,
) {
    while entries.len() >= 0x50 {
        // MFT reference (file record number) at offset 0
        let mft_ref = u64::from_le_bytes(entries[0..8].try_into().unwrap());
        let mft_rec = (mft_ref & 0xFFFFFFFFFFFF) as u32; // low 48 bits
        let _mft_seq = (mft_ref >> 48) as u16;

        let ent_len = u16::from_le_bytes([entries[8], entries[9]]) as usize;
        if ent_len < 8 || ent_len > entries.len() {
            break;
        }
        let flags = entries[12];
        let fn_off = 0x10usize; // $FILE_NAME attribute starts at offset 0x10 from index entry start

        if fn_off + 66 <= entries.len()
            && (flags & 0x02) == 0 // not a subdirectory (we only care about files)
        {
            let name_len = entries[fn_off + 64] as usize;
            if name_len > 0 && name_len <= 255 && fn_off + 66 + name_len * 2 <= entries.len() {
                let mut name_buf = [0u8; 256];
                let mut np = 0;
                for j in 0..name_len {
                    if fn_off + 66 + j * 2 + 1 < entries.len() {
                        let lo = entries[fn_off + 66 + j * 2];
                        let _hi = entries[fn_off + 66 + j * 2 + 1];
                        if lo < 0x80 && lo != 0 && np < 255 {
                            name_buf[np] = lo;
                            np += 1;
                        } else if lo != 0 && np < 255 {
                            name_buf[np] = b'?';
                            np += 1;
                        }
                    }
                }
                let is_iso = np >= 4 && name_buf[np - 4..np].eq_ignore_ascii_case(b".iso");
                if is_iso && *count < 64 && mft_rec > 0 {
                    if let Some((lba, sz)) = get_ntfs_file_lba(bio, bio_ptr, mid, ctx, mft_rec) {
                        files[*count] = IsoEntry {
                            name: name_buf, name_len: np,
                            file_start_lba: lba,
                            file_size: sz, file_size_bytes: sz,
                        };
                        *count += 1;
                    }
                }
            }
        }
        if ent_len == 0 {
            break;
        }
        entries = &entries[ent_len..];
    }
}

fn get_ntfs_file_lba(
    bio: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    ctx: &FsCtx,
    mft_rec: u32,
) -> Option<(u64, u64)> {
    let rec_size = ctx.mft_record_size as usize;
    if rec_size > 4096 {
        return None;
    }
    let lba = ctx.mft_start_lba + mft_rec as u64 * (rec_size as u64 / 512);
    let mut rec = [0u8; 4096];
    for i in 0..(rec_size / 512) {
        let off = i * 512;
        let mut sector = [0u8; 512];
        if !read_sector(bio, bio_ptr, mid, lba + i as u64, &mut sector) {
            return None;
        }
        rec[off..off + 512].copy_from_slice(&sector);
    }

    // Fixup
    let fixup_off = u16::from_le_bytes([rec[4], rec[5]]) as usize;
    let fixup_count = u16::from_le_bytes([rec[6], rec[7]]) as usize;
    if fixup_off > 0 && fixup_off < rec_size && fixup_count > 1 {
        let fixup_val = u16::from_le_bytes([rec[fixup_off], rec[fixup_off + 1]]);
        for i in 1..fixup_count {
            let pos = i * 512 - 2;
            if pos + 2 <= rec_size {
                rec[pos] = fixup_val as u8;
                rec[pos + 1] = (fixup_val >> 8) as u8;
            }
        }
    }

    let attrs_off = u16::from_le_bytes([rec[0x14], rec[0x15]]) as usize;
    if attrs_off >= rec_size {
        return None;
    }
    parse_ntfs_data_attr(ctx, &rec[attrs_off..], rec_size - attrs_off)
}

fn parse_ntfs_data_attr(ctx: &FsCtx, attrs: &[u8], _rem: usize) -> Option<(u64, u64)> {
    let mut off = 0usize;
    while off + 4 < attrs.len() {
        let atype = u32::from_le_bytes([attrs[off], attrs[off + 1], attrs[off + 2], attrs[off + 3]]);
        if atype == 0xFFFFFFFF || atype == 0 {
            break;
        }
        let alen = u32::from_le_bytes([attrs[off + 4], attrs[off + 5], attrs[off + 6], attrs[off + 7]]) as usize;
        if alen < 8 || off + alen > attrs.len() {
            break;
        }
        if atype == 0x80 {
            // $DATA attribute
            let is_nonresident = attrs[off + 8] != 0;
            if is_nonresident {
                if alen < 56 {
                    break;
                }
                // Parse data runs
                let run_off = u16::from_le_bytes([attrs[off + 0x20], attrs[off + 0x21]]) as usize;
                let file_size = u64::from_le_bytes(attrs[off + 0x30..off + 0x38].try_into().unwrap());
                if run_off > 0 && off + run_off + 1 < attrs.len() {
                    let run_bytes = &attrs[off + run_off..off + alen];
                    let mut lcn: u64 = 0;
                    for i in 0..1 {
                        if i < run_bytes.len() {
                            let hdr = run_bytes[i];
                            if hdr == 0 {
                                break;
                            }
                            let len_bytes = (hdr & 0x0F) as usize;
                            let off_bytes = ((hdr >> 4) & 0x0F) as usize;
                            if i + 1 + len_bytes + off_bytes <= run_bytes.len() {
                                let clen = parse_varlen_le(&run_bytes[i + 1..], len_bytes);
                                let coff = parse_varlen_le_signed(&run_bytes[i + 1 + len_bytes..], off_bytes);
                                lcn = (lcn as i64 + coff) as u64;
                                if clen > 0 {
                                    let iso_lba = ctx.part1_lba + lcn * ctx.sectors_per_cluster as u64;
                                    return Some((iso_lba, file_size));
                                }
                            }
                        }
                    }
                }
            }
            break;
        }
        off += alen;
    }
    None
}

fn parse_varlen_le(data: &[u8], n: usize) -> u64 {
    let mut val: u64 = 0;
    let end = data.len().min(n).min(8);
    for (i, &b) in data[..end].iter().enumerate() {
        val |= (b as u64) << (i * 8);
    }
    val
}

fn parse_varlen_le_signed(data: &[u8], n: usize) -> i64 {
    if n == 0 || n > 8 {
        return 0;
    }
    let val = parse_varlen_le(data, n);
    let bits = (n * 8) as u64;
    if n < 8 && (val & (1u64 << (bits - 1))) != 0 {
        (val as i64) - (1i64 << bits)
    } else {
        val as i64
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Boot menu (FS-type-agnostic)
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
    ctx: &FsCtx,
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
        let num = (i + 1) as u64;
        let (sb, sl) = format_u64_buf(num);
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
    print_raw(st, b"Enter number to boot (or 'r' to scan for .iso): \0");

    let bs = unsafe { &mut *st.boot_services };
    if !st.con_in.is_null() {
        let ci = unsafe { &mut *(st.con_in as *mut SimpleTextInput) };
        loop {
            let mut k = Key { sc: 0, uc: 0 };
            if unsafe { (ci.read_key_stroke)(ci as *mut _, &mut k) } != EFI_SUCCESS {
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
                    boot_iso(st, files, idx, ctx, bio, bio_ptr, mid);
                }
            } else if ch == b'0' && count >= 10 {
                boot_iso(st, files, 9, ctx, bio, bio_ptr, mid);
            } else if ch == b'r' || ch == b'R' {
                print_raw(st, b"\r\nRe-scanning...\r\n\0");
                let mut new_files: [IsoEntry; 64] = unsafe { core::mem::zeroed() };
                let mut new_count: usize = 0;
                scan_directory(bio, bio_ptr, mid, ctx, &mut new_files, &mut new_count);
                show_menu(st, &new_files, new_count, ctx, bio, bio_ptr, mid);
                return;
            }
        }
    }
}

fn boot_iso(
    st: &mut SystemTable,
    files: &[IsoEntry; 64],
    idx: usize,
    _ctx: &FsCtx,
    bio: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
) -> ! {
    print_raw(st, b"\r\nBooting ISO...\r\n\0");
    let iso_lba = files[idx].file_start_lba;

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
    let num_pages = (boot_sector_count as usize * 512 + 4095) / 4096;
    let mut pages: u64 = 0x80000;
    let status = unsafe {
        (bs.allocate_pages)(
            AllocateType::AllocateAddress,
            MemoryType::EfiLoaderData,
            num_pages,
            &mut pages,
        )
    };
    if status != EFI_SUCCESS {
        pages = 0;
        let status = unsafe {
            (bs.allocate_pages)(
                AllocateType::AllocateAnyPages,
                MemoryType::EfiLoaderData,
                num_pages,
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

    let safe_dest = 0x80000 as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(dest, safe_dest, boot_sector_count as usize * 512);
    }

    let cookie_ptr = 0x7B00usize as *mut u32;
    unsafe { *cookie_ptr = 0x544F4F42u32 };

    print_raw(st, b"Boot image loaded. Rebooting...\r\n\0");
    let rt = unsafe { &mut *st.runtime_services };
    unsafe { (rt.reset_system)(ResetType::ResetWarm, 0, 0, core::ptr::null_mut()) };
    loop {
        unsafe { core::arch::asm!("hlt") }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  GPT partition search
// ═══════════════════════════════════════════════════════════════════

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
    if &hdr_sec[0..8] != b"EFI PART" {
        return 0;
    }
    let entries_lba = u64::from_le_bytes(hdr_sec[72..80].try_into().unwrap());
    let n = u32::from_le_bytes(hdr_sec[80..84].try_into().unwrap());
    let sz = u32::from_le_bytes(hdr_sec[84..88].try_into().unwrap());
    if sz == 0 || n == 0 {
        return 0;
    }

    let basic_data_guid: [u8; 16] = [
        0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99,
        0xC7,
    ];

    let mut sec: [u8; 512] = [0; 512];
    let mut current_lba: u64 = 0;
    let mut loaded = false;
    for i in 0..n.min(128) {
        let eoff = i as usize * sz as usize;
        let lba = entries_lba + (eoff / 512) as u64;
        let boff = eoff % 512;
        if boff + 40 > 512 {
            continue;
        }
        if !loaded || lba != current_lba {
            if unsafe { (bio_ref.read_blocks)(bio_ptr, mid, lba, 512, sec.as_mut_ptr() as _) }
                != EFI_SUCCESS
            {
                break;
            }
            current_lba = lba;
            loaded = true;
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

// ═══════════════════════════════════════════════════════════════════
//  Disk handle discovery
// ═══════════════════════════════════════════════════════════════════

fn find_disk_handle(
    bs: &mut BootServices,
    _image_handle: *mut core::ffi::c_void,
) -> Option<*mut core::ffi::c_void> {
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
        if unsafe { (*bio).media }.is_null() {
            continue;
        }
        let media = unsafe { &*((*bio).media) };
        if media.bim_lp == 0 {
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
    bs_18: *mut core::ffi::c_void,
    bs_20: *mut core::ffi::c_void,
    allocate_pages: unsafe extern "efiapi" fn(AllocateType, MemoryType, usize, *mut u64) -> usize,
    bs_30: *mut core::ffi::c_void,
    bs_38: *mut core::ffi::c_void,
    bs_40: *mut core::ffi::c_void,
    free_pool: unsafe extern "efiapi" fn(*mut core::ffi::c_void) -> usize,
    bs_50: *mut core::ffi::c_void,
    bs_58: *mut core::ffi::c_void,
    bs_60: *mut core::ffi::c_void,
    bs_68: *mut core::ffi::c_void,
    bs_70: *mut core::ffi::c_void,
    bs_78: *mut core::ffi::c_void,
    bs_80: *mut core::ffi::c_void,
    bs_88: *mut core::ffi::c_void,
    bs_90: *mut core::ffi::c_void,
    handle_protocol: unsafe extern "efiapi" fn(
        *mut core::ffi::c_void,
        *const Guid,
        *mut *mut core::ffi::c_void,
    ) -> usize,
    bs_a0: *mut core::ffi::c_void,
    bs_a8: *mut core::ffi::c_void,
    bs_b0: *mut core::ffi::c_void,
    bs_b8: *mut core::ffi::c_void,
    bs_c0: *mut core::ffi::c_void,
    bs_c8: *mut core::ffi::c_void,
    bs_d0: *mut core::ffi::c_void,
    bs_d8: *mut core::ffi::c_void,
    bs_e0: *mut core::ffi::c_void,
    bs_e8: *mut core::ffi::c_void,
    bs_f0: *mut core::ffi::c_void,
    stall: unsafe extern "efiapi" fn(usize) -> usize,
    bs_100: *mut core::ffi::c_void,
    bs_108: *mut core::ffi::c_void,
    bs_110: *mut core::ffi::c_void,
    bs_118: *mut core::ffi::c_void,
    bs_120: *mut core::ffi::c_void,
    bs_128: *mut core::ffi::c_void,
    bs_130: *mut core::ffi::c_void,
    locate_handle_buffer: unsafe extern "efiapi" fn(
        LocateSearchType,
        *const Guid,
        *mut core::ffi::c_void,
        *mut usize,
        *mut *mut *mut core::ffi::c_void,
    ) -> usize,
    bs_140: *mut core::ffi::c_void,
    bs_148: *mut core::ffi::c_void,
    bs_150: *mut core::ffi::c_void,
    bs_158: *mut core::ffi::c_void,
    bs_160: *mut core::ffi::c_void,
    bs_168: *mut core::ffi::c_void,
    bs_170: *mut core::ffi::c_void,
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
    bim_rm: u8,
    bim_mp: u8,
    bim_lp: u8,
    bim_ro: u8,
    bim_wc: u8,
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
    ResetWarm = 1,
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
