// ═══════════════════════════════════════════════════════════════════════════
//  Filesystem support — exFAT, FAT32, NTFS directory scanners (UEFI)
// ═══════════════════════════════════════════════════════════════════════════

use crate::disk::read_sector;
use crate::protocol::{BlockIoProtocol, EFI_SUCCESS};

#[derive(Clone, Copy, PartialEq)]
pub enum FsType {
    Exfat,
    Fat32,
    Ntfs,
}

pub struct FsCtx {
    pub fs: FsType,
    pub part1_lba: u64,
    // exFAT / FAT32
    pub spc: u32,
    pub fat_start: u64,
    pub fat_len: u64,
    pub heap_start: u64,
    pub root_cluster: u32,
    // NTFS
    pub mft_start_lba: u64,
    pub sectors_per_cluster: u32,
    pub bytes_per_cluster: u64,
    pub mft_record_size: u64,
}

#[derive(Clone, Copy)]
pub struct IsoEntry {
    pub name: [u8; 256],
    pub name_len: usize,
    pub file_start_lba: u64,
    pub file_size: u64,
}

// ═══════════════════════════════════════════════════════════════════════════
//  Shared FAT entry reader
// ═══════════════════════════════════════════════════════════════════════════

fn fat_read_sector(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    lba: u64,
    buf: &mut [u8; 512],
) -> bool {
    read_sector(bio_ref, bio_ptr, mid, lba, buf)
}

fn read_fat_entry(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    fat_start: u64,
    cluster: u32,
) -> u32 {
    let fat_lba = fat_start + (cluster as u64 * 4 / 512);
    let fat_off = (cluster as usize * 4) % 512;
    let mut fat_buf: [u8; 512] = [0; 512];
    if !fat_read_sector(bio_ref, bio_ptr, mid, fat_lba, &mut fat_buf) {
        return 0xFFFFFFFF;
    }
    u32::from_le_bytes([
        fat_buf[fat_off],
        fat_buf[fat_off + 1],
        fat_buf[fat_off + 2],
        fat_buf[fat_off + 3],
    ])
}

// ═══════════════════════════════════════════════════════════════════════════
//  Parse variable-length little-endian integers (NTFS)
// ═══════════════════════════════════════════════════════════════════════════

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

// ═══════════════════════════════════════════════════════════════════════════
//  exFAT scanner
// ═══════════════════════════════════════════════════════════════════════════

fn scan_exfat_dir(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    root_cluster: u32,
    spc: u32,
    fat_start: u64,
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
            if !fat_read_sector(bio_ref, bio_ptr, mid, lba, &mut buf) {
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
                    if sec_count < 1 {
                        pos += 1;
                        continue;
                    }
                    let total_ents = 1 + sec_count;
                    if pos + total_ents > n_entries {
                        let rem = total - pos * 32;
                        if rem <= carry.len() {
                            carry[..rem]
                                .copy_from_slice(&entries[pos * 32..]);
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
                    let name_ents = sec_count.saturating_sub(1);
                    let name_len = entries[stream_off + 3] as usize;
                    let mut name_buf = [0u8; 256];
                    let mut name_pos = 0usize;

                    for ne in 0..name_ents {
                        let noff = (pos + 2 + ne) * 32;
                        if entries[noff] != 0xC1 {
                            break;
                        }
                        let to_copy = name_len
                            .saturating_sub(name_pos)
                            .min(15)
                            .min(256 - name_pos);
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
                            name_buf[name_pos + j] =
                                if cp < 0x80 { cp as u8 } else { b'?' };
                        }
                        name_pos += to_copy;
                    }

                    if name_pos >= 4
                        && name_buf[name_pos - 4..name_pos]
                            .eq_ignore_ascii_case(b".iso")
                        && *count < 64
                    {
                        let file_lba = heap_start
                            + (start_cl as u64 - 2) * spc as u64;
                        files[*count] = IsoEntry {
                            name: name_buf,
                            name_len: name_pos,
                            file_start_lba: file_lba,
                            file_size: fsize,
                        };
                        *count += 1;
                    }
                    pos += total_ents;
                    continue;
                }
                pos += 1;
            }
        }
        let next = read_fat_entry(bio_ref, bio_ptr, mid, fat_start, cluster);
        if next < 2 || next >= 0xFFFFFFF8 {
            break;
        }
        cluster = next;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  FAT32 scanner
// ═══════════════════════════════════════════════════════════════════════════

fn fat32_next_cluster(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    fat_start: u64,
    cluster: u32,
) -> u32 {
    read_fat_entry(bio_ref, bio_ptr, mid, fat_start, cluster) & 0x0FFFFFFF
}

fn scan_fat32_dir(
    bio_ref: &BlockIoProtocol,
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
            if !fat_read_sector(bio_ref, bio_ptr, mid, lba, &mut buf) {
                return;
            }
            for e in 0..(512 / 32) {
                let off = e * 32;
                let first = buf[off];
                if first == 0 {
                    return;
                }
                if first == 0xE5 {
                    if lfn_len > 0 {
                        lfn_len = 0;
                    }
                    continue;
                }
                let attr = buf[off + 11];
                if attr == 0x0F {
                    let seq = first & 0x1F;
                    let is_last = first & 0x40;
                    let checksum = buf[off + 13];
                    if seq == 0 || seq > 20 {
                        continue;
                    }
                    let chars = [
                        buf[off + 1],
                        buf[off + 3],
                        buf[off + 5],
                        buf[off + 7],
                        buf[off + 9],
                        buf[off + 14],
                        buf[off + 16],
                        buf[off + 18],
                        buf[off + 20],
                        buf[off + 22],
                        buf[off + 24],
                        buf[off + 28],
                        buf[off + 30],
                    ];
                    if is_last != 0 {
                        lfn_seq = seq;
                        lfn_checksum = checksum;
                        let mut num_chars = 0;
                        for &c in &chars {
                            if c == 0x00 || c == 0xFF {
                                break;
                            }
                            num_chars += 1;
                        }
                        lfn_len = ((seq as usize - 1) * 13 + num_chars)
                            .min(255);
                    }
                    if seq != lfn_seq || checksum != lfn_checksum {
                        continue;
                    }
                    lfn_seq -= 1;
                    let write_start = (seq as usize - 1) * 13;
                    let mut char_idx = 0;
                    for &c in &chars {
                        if c == 0x00 || c == 0xFF {
                            break;
                        }
                        if write_start + char_idx < 255 {
                            lfn_buf[write_start + char_idx] =
                                if c < 0x80 { c } else { b'?' };
                            char_idx += 1;
                        }
                    }
                    continue;
                }
                if attr & 0x08 != 0 {
                    if lfn_len > 0 {
                        lfn_len = 0;
                    }
                    continue;
                }
                let mut name_buf = [0u8; 256];
                let mut name_len: usize;
                let lfn_valid = lfn_len > 0 && lfn_buf[0] != 0;

                if lfn_valid {
                    let copy = lfn_len.min(255);
                    name_buf[..copy].copy_from_slice(&lfn_buf[..copy]);
                    name_len = copy;
                } else {
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
                lfn_len = 0;

                let is_iso = name_len >= 4
                    && name_buf[name_len - 4..name_len]
                        .eq_ignore_ascii_case(b".iso");

                if is_iso && *count < 64 {
                    let file_cl = u32::from_le_bytes([
                        buf[off + 20],
                        buf[off + 21],
                        buf[off + 26],
                        buf[off + 27],
                    ]);
                    let file_sz = u32::from_le_bytes([
                        buf[off + 28],
                        buf[off + 29],
                        buf[off + 30],
                        buf[off + 31],
                    ]);
                    let file_lba = data_start
                        + (file_cl as u64 - 2) * spc as u64;
                    files[*count] = IsoEntry {
                        name: name_buf,
                        name_len,
                        file_start_lba: file_lba,
                        file_size: file_sz as u64,
                    };
                    *count += 1;
                }
            }
        }
        let next = fat32_next_cluster(
            bio_ref, bio_ptr, mid, fat_start, cluster,
        );
        if next < 2 || next >= 0x0FFFFFF0 {
            break;
        }
        cluster = next;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  NTFS scanner
// ═══════════════════════════════════════════════════════════════════════════

fn get_ntfs_file_lba(
    bio_ref: &BlockIoProtocol,
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
        let mut sector = [0u8; 512];
        if !fat_read_sector(bio_ref, bio_ptr, mid, lba + i as u64, &mut sector) {
            return None;
        }
        let off = i * 512;
        rec[off..off + 512].copy_from_slice(&sector);
    }

    // Fixup
    let fixup_off = u16::from_le_bytes([rec[4], rec[5]]) as usize;
    let fixup_count = u16::from_le_bytes([rec[6], rec[7]]) as usize;
    if fixup_off > 0 && fixup_off + 2 <= rec_size && fixup_count > 1 {
        let fixup_val =
            u16::from_le_bytes([rec[fixup_off], rec[fixup_off + 1]]);
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

    parse_ntfs_data_attr(ctx, &rec[attrs_off..])
}

fn parse_ntfs_data_attr(ctx: &FsCtx, attrs: &[u8]) -> Option<(u64, u64)> {
    let mut off = 0usize;
    while off + 4 < attrs.len() {
        let atype = u32::from_le_bytes([
            attrs[off],
            attrs[off + 1],
            attrs[off + 2],
            attrs[off + 3],
        ]);
        if atype == 0xFFFFFFFF || atype == 0 {
            break;
        }
        if off + 8 > attrs.len() {
            break;
        }
        let alen = u32::from_le_bytes([
            attrs[off + 4],
            attrs[off + 5],
            attrs[off + 6],
            attrs[off + 7],
        ]) as usize;
        if alen < 24 || off + alen > attrs.len() {
            break;
        }
        if atype == 0x80 {
            let is_nonresident = attrs[off + 8] != 0;
            if is_nonresident {
                if alen < 56 {
                    break;
                }
                let run_off = u16::from_le_bytes([
                    attrs[off + 0x20],
                    attrs[off + 0x21],
                ]) as usize;
                let file_size = u64::from_le_bytes(
                    attrs[off + 0x30..off + 0x38]
                        .try_into()
                        .unwrap(),
                );
                if run_off > 0 && run_off < alen && off + run_off < attrs.len() {
                    let run_bytes = &attrs[off + run_off..off + alen];
                    let mut lcn: u64 = 0;
                    for i in 0..1 {
                        if i < run_bytes.len() {
                            let hdr = run_bytes[i];
                            if hdr == 0 {
                                break;
                            }
                            let len_bytes = (hdr & 0x0F) as usize;
                            let off_bytes =
                                ((hdr >> 4) & 0x0F) as usize;
                            if i + 1 + len_bytes + off_bytes
                                <= run_bytes.len()
                            {
                                let _clen = parse_varlen_le(
                                    &run_bytes[i + 1..],
                                    len_bytes,
                                );
                                let coff = parse_varlen_le_signed(
                                    &run_bytes[i + 1 + len_bytes..],
                                    off_bytes,
                                );
                                lcn = (lcn as i64 + coff) as u64;
                                let iso_lba = ctx.part1_lba
                                    + lcn
                                        * ctx.sectors_per_cluster
                                            as u64;
                                return Some((iso_lba, file_size));
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

fn scan_ntfs_dir(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    ctx: &FsCtx,
    files: &mut [IsoEntry; 64],
    count: &mut usize,
) {
    let rec_size = ctx.mft_record_size as usize;
    if rec_size > 4096 {
        return;
    }
    let mft_rec_lba =
        ctx.mft_start_lba + 5 * (rec_size as u64 / 512);
    let mut rec_buf = [0u8; 4096];
    for i in 0..(rec_size / 512) {
        let mut sector = [0u8; 512];
        if !fat_read_sector(bio_ref, bio_ptr, mid, mft_rec_lba + i as u64, &mut sector) {
            return;
        }
        let off = i * 512;
        rec_buf[off..off + 512].copy_from_slice(&sector);
    }

    // Fixup
    let fixup_off =
        u16::from_le_bytes([rec_buf[4], rec_buf[5]]) as usize;
    let fixup_count =
        u16::from_le_bytes([rec_buf[6], rec_buf[7]]) as usize;
    if fixup_off > 0 && fixup_off + 2 <= rec_size && fixup_count > 1 {
        let fixup_val =
            u16::from_le_bytes([rec_buf[fixup_off], rec_buf[fixup_off + 1]]);
        for i in 1..fixup_count {
            let pos = i * 512 - 2;
            if pos + 2 <= rec_size {
                rec_buf[pos] = fixup_val as u8;
                rec_buf[pos + 1] = (fixup_val >> 8) as u8;
            }
        }
    }

    let attrs_off =
        u16::from_le_bytes([rec_buf[0x14], rec_buf[0x15]]) as usize;
    if attrs_off >= rec_size {
        return;
    }

    parse_ntfs_attrs(
        bio_ref,
        bio_ptr,
        mid,
        ctx,
        &rec_buf[attrs_off..],
        files,
        count,
    );
}

fn parse_ntfs_attrs(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    ctx: &FsCtx,
    attrs: &[u8],
    files: &mut [IsoEntry; 64],
    count: &mut usize,
) {
    let mut off = 0usize;
    while off + 4 < attrs.len() {
        let atype = u32::from_le_bytes([
            attrs[off],
            attrs[off + 1],
            attrs[off + 2],
            attrs[off + 3],
        ]);
        if atype == 0xFFFFFFFF || atype == 0 {
            break;
        }
        if off + 8 > attrs.len() {
            break;
        }
        let alen = u32::from_le_bytes([
            attrs[off + 4],
            attrs[off + 5],
            attrs[off + 6],
            attrs[off + 7],
        ]) as usize;
        if alen < 24 || off + alen > attrs.len() {
            break;
        }
        let is_nonresident = attrs[off + 8] != 0;
        let name_len = attrs[off + 9] as usize;
        let val_off = u16::from_le_bytes([
            attrs[off + 0x14],
            attrs[off + 0x15],
        ]) as usize
            + name_len;

        if atype == 0x90 {
            if !is_nonresident && val_off < alen {
                let index_data =
                    &attrs[off + val_off..off + alen];
                parse_ntfs_index_root(
                    bio_ref, bio_ptr, mid, ctx, index_data,
                    files, count,
                );
            }
        }
        off += alen;
    }
}

fn parse_ntfs_index_root(
    bio_ref: &BlockIoProtocol,
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
    let entries_off = u32::from_le_bytes([
        data[0], data[1], data[2], data[3],
    ]) as usize
        + 0x10;
    if entries_off >= data.len() {
        return;
    }
    parse_ntfs_index_entries(
        bio_ref, bio_ptr, mid, ctx,
        &data[entries_off..],
        files, count,
    );
}

fn parse_ntfs_index_entries(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    ctx: &FsCtx,
    mut entries: &[u8],
    files: &mut [IsoEntry; 64],
    count: &mut usize,
) {
    while entries.len() >= 0x50 {
        let mft_ref =
            u64::from_le_bytes(entries[0..8].try_into().unwrap());
        let mft_rec = (mft_ref & 0xFFFFFFFFFFFF) as u32;

        let ent_len =
            u16::from_le_bytes([entries[8], entries[9]]) as usize;
        if ent_len < 8 || ent_len > entries.len() {
            break;
        }
        let flags = entries[12];
        let fn_off = 0x10usize;

        if fn_off + 66 <= entries.len() && (flags & 0x02) == 0 {
            let name_len = entries[fn_off + 64] as usize;
            if name_len > 0
                && name_len <= 255
                && fn_off + 66 + name_len * 2 <= entries.len()
            {
                let mut name_buf = [0u8; 256];
                let mut np = 0;
                for j in 0..name_len {
                    if fn_off + 66 + j * 2 + 1 < entries.len() {
                        let lo = entries[fn_off + 66 + j * 2];
                        if lo < 0x80 && lo != 0 && np < 255 {
                            name_buf[np] = lo;
                            np += 1;
                        } else if lo != 0 && np < 255 {
                            name_buf[np] = b'?';
                            np += 1;
                        }
                    }
                }
                let is_iso = np >= 4
                    && name_buf[np - 4..np]
                        .eq_ignore_ascii_case(b".iso");
                if is_iso && *count < 64 && mft_rec > 0 {
                    if let Some((lba, sz)) =
                        get_ntfs_file_lba(
                            bio_ref, bio_ptr, mid, ctx,
                            mft_rec,
                        )
                    {
                        files[*count] = IsoEntry {
                            name: name_buf,
                            name_len: np,
                            file_start_lba: lba,
                            file_size: sz,
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

// ═══════════════════════════════════════════════════════════════════════════
//  Unified scan dispatcher
// ═══════════════════════════════════════════════════════════════════════════

pub fn scan_directory(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    ctx: &FsCtx,
    files: &mut [IsoEntry; 64],
    count: &mut usize,
) {
    match ctx.fs {
        FsType::Exfat => scan_exfat_dir(
            bio_ref, bio_ptr, mid, ctx.root_cluster, ctx.spc,
            ctx.fat_start, ctx.heap_start, files, count,
        ),
        FsType::Fat32 => scan_fat32_dir(
            bio_ref, bio_ptr, mid, ctx.root_cluster, ctx.spc,
            ctx.fat_start, ctx.heap_start, files, count,
        ),
        FsType::Ntfs => {
            scan_ntfs_dir(bio_ref, bio_ptr, mid, ctx, files, count);
        }
    }
}