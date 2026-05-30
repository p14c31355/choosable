// ═══════════════════════════════════════════════════════════════════════════
//  Filesystem support — exFAT, FAT32, NTFS directory scanners
// ═══════════════════════════════════════════════════════════════════════════

use crate::ata::ata_read_sector;

#[derive(Clone, Copy)]
pub enum FsType {
    Exfat,
    Fat32,
    Ntfs,
}

pub struct FsCtx {
    pub fs: FsType,
    pub part1_lba: u32,
    // exFAT / FAT32
    pub spc: u32,
    pub fat_start: u64,
    pub fat_len: u64,
    pub heap_start: u64,
    pub root_cluster: u32,
    // NTFS
    pub mft_start_lba: u64,
    pub sectors_per_cluster: u32,
    pub mft_record_size: u64,
}

#[derive(Clone, Copy)]
pub struct DirEntry {
    pub name: [u8; 256],
    pub name_len: usize,
    pub file_start_lba: u64,
    pub file_size: u64,
}

impl DirEntry {
    pub fn zero() -> Self {
        DirEntry {
            name: [0u8; 256],
            name_len: 0,
            file_start_lba: 0,
            file_size: 0,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Shared FAT entry reader
// ═══════════════════════════════════════════════════════════════════════════

fn read_fat_entry_raw(fat_start: u64, cluster: u32) -> u32 {
    let fat_sector = fat_start + (cluster as u64 * 4 / 512);
    let fat_offset = (cluster as u64 * 4 % 512) as usize;
    let mut buf = [0u8; 512];
    if !ata_read_sector(fat_sector, &mut buf) {
        return 0xFFFFFFFF;
    }
    u32::from_le_bytes([
        buf[fat_offset],
        buf[fat_offset + 1],
        buf[fat_offset + 2],
        buf[fat_offset + 3],
    ])
}

fn fat32_next_cluster(fat_start: u64, cluster: u32) -> u32 {
    read_fat_entry_raw(fat_start, cluster) & 0x0FFFFFFF
}

fn exfat_next_cluster(fat_start: u64, cluster: u32) -> u32 {
    read_fat_entry_raw(fat_start, cluster)
}

// ═══════════════════════════════════════════════════════════════════════════
//  UTF-16LE → ASCII helper
// ═══════════════════════════════════════════════════════════════════════════

fn utf16le_to_ascii(src: &[u8], max_bytes: usize, dst: &mut [u8]) -> usize {
    let mut di = 0;
    let mut si = 0;
    let limit = src.len().min(max_bytes);
    while si + 1 < limit && di < dst.len() {
        let lo = src[si];
        let hi = src[si + 1];
        let cp = lo as u16 | ((hi as u16) << 8);
        si += 2;
        if cp == 0 {
            break;
        }
        if cp < 0x80 {
            dst[di] = cp as u8;
            di += 1;
        } else {
            dst[di] = b'?';
            di += 1;
        }
    }
    di
}

// ═══════════════════════════════════════════════════════════════════════════
//  exFAT scanner
// ═══════════════════════════════════════════════════════════════════════════

const EXFAT_ENTRY_FILE: u8 = 0x85;
const EXFAT_ENTRY_NAME: u8 = 0xC1;

fn scan_exfat_dir(
    ctx: &FsCtx,
    files: &mut [DirEntry],
    file_count: &mut usize,
) {
    let mut cluster = ctx.root_cluster;
    let mut buf: [u8; 512] = [0; 512];
    let mut carry: [u8; 608] = [0; 608];
    let mut carry_len: usize = 0;
    let max_files = files.len();
    let spc = ctx.spc;

    loop {
        for si in 0..spc {
            let lba = ctx.heap_start + (cluster as u64 - 2) * spc as u64 + si as u64;
            if !ata_read_sector(lba, &mut buf) {
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
                if etype == EXFAT_ENTRY_FILE {
                    let sec_count = entries[pos * 32 + 1] as usize;
                    if sec_count < 1 {
                        pos += 1;
                        continue;
                    }
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
                        entries[stream_off + 20],
                        entries[stream_off + 21],
                        entries[stream_off + 22],
                        entries[stream_off + 23],
                    ]);
                    let size = u64::from_le_bytes([
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
                        if entries[noff] != EXFAT_ENTRY_NAME {
                            break;
                        }
                        let to_copy = name_len
                            .saturating_sub(name_pos)
                            .min(15)
                            .min(256 - name_pos);
                        if to_copy == 0 {
                            break;
                        }
                        name_pos += utf16le_to_ascii(
                            &entries[noff + 2..],
                            to_copy * 2,
                            &mut name_buf[name_pos..],
                        );
                    }

                    if name_pos >= 4
                        && name_buf[name_pos - 4..name_pos].eq_ignore_ascii_case(b".iso")
                        && *file_count < max_files
                    {
                        let file_lba =
                            ctx.heap_start + (start_cl as u64 - 2) * spc as u64;
                        files[*file_count] = DirEntry {
                            name: name_buf,
                            name_len: name_pos,
                            file_start_lba: file_lba,
                            file_size: size,
                        };
                        *file_count += 1;
                    }
                    pos += total_ents;
                    continue;
                }
                pos += 1;
            }
        }
        let next = exfat_next_cluster(ctx.fat_start, cluster);
        if next < 2 || next >= 0xFFFFFFF8 {
            break;
        }
        cluster = next;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  FAT32 scanner
// ═══════════════════════════════════════════════════════════════════════════

fn scan_fat32_dir(
    ctx: &FsCtx,
    files: &mut [DirEntry],
    file_count: &mut usize,
) {
    if ctx.root_cluster == 0 {
        return;
    }
    let mut cluster = ctx.root_cluster;
    let mut buf = [0u8; 512];
    let max_files = files.len();
    let spc = ctx.spc;
    let data_start = ctx.heap_start;
    let fat_start = ctx.fat_start;

    loop {
        for si in 0..spc {
            let lba = data_start + (cluster as u64 - 2) * spc as u64 + si as u64;
            if !ata_read_sector(lba, &mut buf) {
                return;
            }
            for e in 0..(512 / 32) {
                let off = e * 32;
                let first = buf[off];
                if first == 0 {
                    return;
                }
                if first == 0xE5 {
                    continue;
                }
                let attr = buf[off + 11];
                if attr == 0x0F {
                    continue; // skip LFN, use 8.3
                }
                if attr & 0x08 != 0 {
                    continue; // volume label
                }
                // 8.3 short name
                let mut name_buf = [0u8; 256];
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
                let is_iso = nlen >= 4
                    && name_buf[nlen - 4..nlen].eq_ignore_ascii_case(b".iso");
                if is_iso && *file_count < max_files {
                    let file_cl = u32::from_le_bytes([
                        buf[off + 26],
                        buf[off + 27],
                        buf[off + 20],
                        buf[off + 21],
                    ]);
                    let file_sz = u32::from_le_bytes([
                        buf[off + 28],
                        buf[off + 29],
                        buf[off + 30],
                        buf[off + 31],
                    ]);
                    let file_lba =
                        data_start + (file_cl as u64 - 2) * spc as u64;
                    files[*file_count] = DirEntry {
                        name: name_buf,
                        name_len: nlen,
                        file_start_lba: file_lba,
                        file_size: file_sz as u64,
                    };
                    *file_count += 1;
                }
            }
        }
        let next = fat32_next_cluster(fat_start, cluster);
        if next < 2 || next >= 0x0FFFFFF0 {
            break;
        }
        cluster = next;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  NTFS scanner (minimal)
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

fn get_ntfs_file_info(ctx: &FsCtx, mft_rec: u32) -> Option<(u64, u64)> {
    let rec_size = ctx.mft_record_size as usize;
    if rec_size > 4096 {
        return None;
    }
    let lba = ctx.mft_start_lba + mft_rec as u64 * (rec_size as u64 / 512);
    let mut rec = [0u8; 4096];
    for i in 0..(rec_size / 512) {
        let mut sector = [0u8; 512];
        if !ata_read_sector(lba + i as u64, &mut sector) {
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
    let attrs = &rec[attrs_off..];
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
                    if run_bytes.len() > 0 {
                        let hdr = run_bytes[0];
                        if hdr != 0 {
                            let len_bytes = (hdr & 0x0F) as usize;
                            let off_bytes =
                                ((hdr >> 4) & 0x0F) as usize;
                            if 1 + len_bytes + off_bytes <= run_bytes.len() {
                                let _clen = parse_varlen_le(
                                    &run_bytes[1..],
                                    len_bytes,
                                );
                                let coff = parse_varlen_le_signed(
                                    &run_bytes[1 + len_bytes..],
                                    off_bytes,
                                );
                                lcn = (lcn as i64 + coff) as u64;
                                let iso_lba = ctx.part1_lba as u64
                                    + lcn * ctx.sectors_per_cluster as u64;
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
    ctx: &FsCtx,
    files: &mut [DirEntry],
    file_count: &mut usize,
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
        if !ata_read_sector(mft_rec_lba + i as u64, &mut sector) {
            return;
        }
        let off = i * 512;
        rec_buf[off..off + 512].copy_from_slice(&sector);
    }

    // Fixup
    let fixup_off = u16::from_le_bytes([rec_buf[4], rec_buf[5]]) as usize;
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
    let attrs = &rec_buf[attrs_off..];
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
        if atype == 0x90 {
            let is_nonresident = attrs[off + 8] != 0;
            let name_len = attrs[off + 9] as usize;
            let val_off = u16::from_le_bytes([
                attrs[off + 0x14],
                attrs[off + 0x15],
            ]) as usize
                + name_len;
            if !is_nonresident && val_off < alen {
                let index_data = &attrs[off + val_off..off + alen];
                if index_data.len() >= 20 {
                    let entries_off = u32::from_le_bytes([
                        index_data[0],
                        index_data[1],
                        index_data[2],
                        index_data[3],
                    ]) as usize
                        + 0x10;
                    if entries_off < index_data.len() {
                        let mut entries = &index_data[entries_off..];
                        while entries.len() >= 0x50 {
                            let mft_ref = u64::from_le_bytes(
                                entries[0..8].try_into().unwrap(),
                            );
                            let mft_rec =
                                (mft_ref & 0xFFFFFFFFFFFF) as u32;
                            let ent_len = u16::from_le_bytes([
                                entries[8],
                                entries[9],
                            ]) as usize;
                            if ent_len < 8 || ent_len > entries.len()
                            {
                                break;
                            }
                            let flags = entries[12];
                            let fn_off = 0x10usize;
                            if fn_off + 66 <= entries.len()
                                && (flags & 0x02) == 0
                            {
                                let name_len =
                                    entries[fn_off + 64] as usize;
                                if name_len > 0
                                    && name_len <= 255
                                    && fn_off
                                        + 66
                                        + name_len * 2
                                        <= entries.len()
                                {
                                    let mut name_buf = [0u8; 256];
                                    let mut np = 0;
                                    for j in 0..name_len {
                                        let lo = entries
                                            [fn_off + 66 + j * 2];
                                        if lo < 0x80
                                            && lo != 0
                                            && np < 255
                                        {
                                            name_buf[np] = lo;
                                            np += 1;
                                        } else if lo != 0
                                            && np < 255
                                        {
                                            name_buf[np] = b'?';
                                            np += 1;
                                        }
                                    }
                                    let is_iso = np >= 4
                                        && name_buf[np - 4..np]
                                            .eq_ignore_ascii_case(
                                                b".iso",
                                            );
                                    if is_iso
                                        && *file_count < 64
                                        && mft_rec > 0
                                    {
                                        if let Some((lba, sz)) =
                                            get_ntfs_file_info(
                                                ctx, mft_rec,
                                            )
                                        {
                                            files[*file_count] =
                                                DirEntry {
                                                    name: name_buf,
                                                    name_len: np,
                                                    file_start_lba: lba,
                                                    file_size: sz,
                                                };
                                            *file_count += 1;
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
                }
            }
        }
        off += alen;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Unified scan dispatcher
// ═══════════════════════════════════════════════════════════════════════════

pub fn scan_filesystem(
    ctx: &FsCtx,
    files: &mut [DirEntry],
    count: &mut usize,
) {
    match ctx.fs {
        FsType::Exfat => scan_exfat_dir(ctx, files, count),
        FsType::Fat32 => scan_fat32_dir(ctx, files, count),
        FsType::Ntfs => scan_ntfs_dir(ctx, files, count),
    }
}