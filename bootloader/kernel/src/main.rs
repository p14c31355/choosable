#![no_std]
#![no_main]

use core::panic::PanicInfo;

// ═══════════════════════════════════════════════════════════════════════════
//  VGA text mode
// ═══════════════════════════════════════════════════════════════════════════

const VGA: *mut u8 = 0xB8000 as *mut u8;
const VGA_COLS: usize = 80;
const VGA_ROWS: usize = 25;

fn vga_clear(attr: u8) {
    for i in 0..(VGA_COLS * VGA_ROWS) {
        unsafe {
            *VGA.add(i * 2) = b' ';
            *VGA.add(i * 2 + 1) = attr;
        }
    }
}

fn vga_print(row: usize, col: usize, s: &[u8], attr: u8) {
    if row >= VGA_ROWS || col >= VGA_COLS {
        return;
    }
    let base = (row * VGA_COLS + col) * 2;
    let buf_end = VGA_COLS * VGA_ROWS * 2;
    let mut off = base;
    for &ch in s {
        if ch == 0 {
            break;
        }
        if off >= buf_end {
            break;
        }
        unsafe {
            *VGA.add(off) = ch;
            *VGA.add(off + 1) = attr;
        }
        off += 2;
    }
}

fn vga_print_byte(val: u8, row: usize, col: usize, attr: u8) {
    if val < 10 {
        vga_print(row, col, &[(val + b'0')], attr);
    } else {
        vga_print(row, col, &[(val - 10 + b'A')], attr);
    }
}

fn vga_print_u32(v: u32, row: usize, col: usize, attr: u8) {
    for i in 0..8 {
        let b = (v >> (28 - i * 4)) as u8 & 0xF;
        vga_print_byte(b, row, col + i, attr);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  I/O port helpers
// ═══════════════════════════════════════════════════════════════════════════

fn outb(port: u16, val: u8) {
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") val) }
}
fn inb(port: u16) -> u8 {
    let v: u8;
    unsafe { core::arch::asm!("in al, dx", out("al") v, in("dx") port) };
    v
}

fn outw(port: u16, val: u16) {
    unsafe { core::arch::asm!("out dx, ax", in("dx") port, in("ax") val) }
}
fn inw(port: u16) -> u16 {
    let v: u16;
    unsafe { core::arch::asm!("in ax, dx", out("ax") v, in("dx") port) };
    v
}

// ═══════════════════════════════════════════════════════════════════════════
//  ATA PIO disk I/O
// ═══════════════════════════════════════════════════════════════════════════

const ATA_PRIMARY_BASE: u16 = 0x1F0;
const ATA_DATA: u16 = ATA_PRIMARY_BASE + 0;
const ATA_ERR: u16 = ATA_PRIMARY_BASE + 1;
const ATA_SECCOUNT: u16 = ATA_PRIMARY_BASE + 2;
const ATA_LBA_LO: u16 = ATA_PRIMARY_BASE + 3;
const ATA_LBA_MID: u16 = ATA_PRIMARY_BASE + 4;
const ATA_LBA_HI: u16 = ATA_PRIMARY_BASE + 5;
const ATA_DRIVE: u16 = ATA_PRIMARY_BASE + 6;
const ATA_STATUS: u16 = ATA_PRIMARY_BASE + 7;
const ATA_CMD: u16 = ATA_PRIMARY_BASE + 7;

fn ata_read_sector(lba: u64, buf: &mut [u8; 512]) -> bool {
    while inb(ATA_STATUS) & 0x80 != 0 {}
    while inb(ATA_STATUS) & 0x08 != 0 {}

    let use_lba48 = lba > 0x0FFF_FFFF;
    if use_lba48 {
        outb(ATA_DRIVE, 0x40);
        outb(ATA_SECCOUNT, 0);
        outb(ATA_LBA_LO, (lba >> 24) as u8);
        outb(ATA_LBA_MID, (lba >> 32) as u8);
        outb(ATA_LBA_HI, (lba >> 40) as u8);
        outb(ATA_SECCOUNT, 1);
        outb(ATA_LBA_LO, lba as u8);
        outb(ATA_LBA_MID, (lba >> 8) as u8);
        outb(ATA_LBA_HI, (lba >> 16) as u8);
        outb(ATA_CMD, 0x24);
    } else {
        outb(ATA_DRIVE, 0xE0 | ((lba >> 24) as u8 & 0x0F));
        outb(ATA_SECCOUNT, 1);
        outb(ATA_LBA_LO, lba as u8);
        outb(ATA_LBA_MID, (lba >> 8) as u8);
        outb(ATA_LBA_HI, (lba >> 16) as u8);
        outb(ATA_CMD, 0x20);
    }

    for _ in 0..4 {
        let _ = inb(ATA_STATUS);
    }

    let mut timeout = 0xFFFFF;
    loop {
        let status = inb(ATA_STATUS);
        if status & 0x01 != 0 {
            return false;
        }
        if status & 0x08 != 0 {
            break;
        }
        timeout -= 1;
        if timeout == 0 {
            return false;
        }
    }

    let buf16 = buf.as_mut_ptr() as *mut u16;
    for i in 0..256 {
        unsafe {
            core::ptr::write_unaligned(buf16.add(i), inw(ATA_DATA));
        }
    }
    true
}

// ═══════════════════════════════════════════════════════════════════════════
//  Simple keyboard input
// ═══════════════════════════════════════════════════════════════════════════

fn kbd_poll() -> Option<u8> {
    if inb(0x64) & 1 == 0 {
        return None;
    }
    Some(inb(0x60))
}

fn kbd_wait_key() -> u8 {
    loop {
        while inb(0x64) & 2 != 0 {}
        if let Some(sc) = kbd_poll() {
            return sc;
        }
    }
}

fn scancode_to_ascii(sc: u8) -> Option<u8> {
    match sc {
        0x02..=0x0A => Some(b'1' + (sc - 0x02)),
        0x0B => Some(b'0'),
        0x13 => Some(b'r'),
        0x1F => Some(b'R'),
        0x1C => Some(b'\n'),
        _ => None,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  MBR partition table parser
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
struct Partition {
    start_lba: u32,
    sector_count: u32,
    fs_type: u8,
}

fn read_partitions() -> ([Partition; 4], usize) {
    let mut buf = [0u8; 512];
    if !ata_read_sector(0, &mut buf) {
        return ([Partition { start_lba: 0, sector_count: 0, fs_type: 0 }; 4], 0);
    }
    let mut parts = [Partition { start_lba: 0, sector_count: 0, fs_type: 0 }; 4];
    let mut count = 0;

    let mut has_gpt = false;
    for i in 0..4 {
        if buf[446 + i * 16 + 4] == 0xEE {
            has_gpt = true;
            break;
        }
    }

    if has_gpt {
        let mut hdr = [0u8; 512];
        if ata_read_sector(1, &mut hdr) && &hdr[0..8] == b"EFI PART" {
            let entries_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
            let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap());
            let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap());
            if sz > 0 && n > 0 {
                let basic_data: [u8; 16] = [
                    0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44,
                    0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
                ];
                let mut sec = [0u8; 512];
                if ata_read_sector(entries_lba, &mut sec) {
                    let max = (512 / sz as usize).min(n as usize).min(128);
                    for i in 0..max {
                        let eoff = i * sz as usize;
                        if sec[eoff..eoff + 16] == basic_data {
                            let lba = u32::from_le_bytes(sec[eoff + 32..eoff + 36].try_into().unwrap());
                            let sectors = u32::from_le_bytes(sec[eoff + 40..eoff + 44].try_into().unwrap());
                            if sectors > 0 {
                                parts[count] = Partition { start_lba: lba, sector_count: sectors, fs_type: 0x07 };
                                count += 1;
                            }
                            break;
                        }
                    }
                }
            }
        }
        return (parts, count);
    }

    for i in 0..4 {
        let off = 446 + i * 16;
        let fs = buf[off + 4];
        let lba = u32::from_le_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]]);
        let sec = u32::from_le_bytes([buf[off + 12], buf[off + 13], buf[off + 14], buf[off + 15]]);
        if sec > 0 {
            parts[count] = Partition { start_lba: lba, sector_count: sec, fs_type: fs };
            count += 1;
        }
    }
    (parts, count)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Filesystem context (unified)
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
enum FsType {
    Exfat,
    Fat32,
    Ntfs,
}

struct FsCtx {
    fs: FsType,
    part1_lba: u32,
    // exFAT / FAT32
    spc: u32,
    fat_start: u64,
    fat_len: u64,
    heap_start: u64,
    root_cluster: u32,
    // NTFS
    mft_start_lba: u64,
    sectors_per_cluster: u32,
    mft_record_size: u64,
}

// ═══════════════════════════════════════════════════════════════════════════
//  Shared FAT entry reader (works for both exFAT and FAT32)
// ═══════════════════════════════════════════════════════════════════════════

fn read_fat_entry_raw(fat_start: u64, cluster: u32) -> u32 {
    let fat_sector = fat_start + (cluster as u64 * 4 / 512);
    let fat_offset = (cluster as u64 * 4 % 512) as usize;
    let mut buf = [0u8; 512];
    if !ata_read_sector(fat_sector, &mut buf) {
        return 0xFFFFFFFF;
    }
    u32::from_le_bytes([buf[fat_offset], buf[fat_offset + 1], buf[fat_offset + 2], buf[fat_offset + 3]])
}

fn fat32_next_cluster(fat_start: u64, cluster: u32) -> u32 {
    read_fat_entry_raw(fat_start, cluster) & 0x0FFFFFFF
}

fn exfat_next_cluster(fat_start: u64, cluster: u32) -> u32 {
    read_fat_entry_raw(fat_start, cluster)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Directory entry
// ═══════════════════════════════════════════════════════════════════════════

struct DirEntry {
    name: [u8; 256],
    name_len: usize,
    file_start_lba: u64,
    file_size: u64,
}

impl DirEntry {
    fn zero() -> Self {
        DirEntry { name: [0u8; 256], name_len: 0, file_start_lba: 0, file_size: 0 }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  exFAT scanner
// ═══════════════════════════════════════════════════════════════════════════

const EXFAT_ENTRY_FILE: u8 = 0x85;
const EXFAT_ENTRY_NAME: u8 = 0xC1;

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

fn scan_exfat_dir(ctx: &FsCtx, files: &mut [DirEntry], file_count: &mut usize) {
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
                    let size = u64::from_le_bytes([
                        entries[stream_off + 24], entries[stream_off + 25],
                        entries[stream_off + 26], entries[stream_off + 27],
                        entries[stream_off + 28], entries[stream_off + 29],
                        entries[stream_off + 30], entries[stream_off + 31],
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
                        let to_copy = (name_len - name_pos).min(15);
                        if to_copy == 0 {
                            break;
                        }
                        name_pos += utf16le_to_ascii(
                            &entries[noff + 2..], to_copy * 2, &mut name_buf[name_pos..],
                        );
                    }

                    if name_pos >= 4
                        && name_buf[name_pos - 4..name_pos].eq_ignore_ascii_case(b".iso")
                        && *file_count < max_files
                    {
                        let file_lba = ctx.heap_start + (start_cl as u64 - 2) * spc as u64;
                        files[*file_count] = DirEntry {
                            name: name_buf, name_len: name_pos,
                            file_start_lba: file_lba, file_size: size,
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

fn scan_fat32_dir(ctx: &FsCtx, files: &mut [DirEntry], file_count: &mut usize) {
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
                    let file_cl = u32::from_le_bytes([buf[off + 20], buf[off + 21], buf[off + 26], buf[off + 27]]);
                    let file_sz = u32::from_le_bytes([buf[off + 28], buf[off + 29], buf[off + 30], buf[off + 31]]);
                    let file_lba = data_start + (file_cl as u64 - 2) * spc as u64;
                    files[*file_count] = DirEntry {
                        name: name_buf, name_len: nlen,
                        file_start_lba: file_lba, file_size: file_sz as u64,
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
    let attrs = &rec[attrs_off..];
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
            let is_nonresident = attrs[off + 8] != 0;
            if is_nonresident {
                let run_off = u16::from_le_bytes([attrs[off + 0x20], attrs[off + 0x21]]) as usize;
                let file_size = u64::from_le_bytes(attrs[off + 0x30..off + 0x38].try_into().unwrap());
                if run_off > 0 && off + run_off + 1 < attrs.len() {
                    let run_bytes = &attrs[off + run_off..off + alen];
                    let mut lcn: u64 = 0;
                    if run_bytes.len() > 0 {
                        let hdr = run_bytes[0];
                        if hdr != 0 {
                            let len_bytes = (hdr & 0x0F) as usize;
                            let off_bytes = ((hdr >> 4) & 0x0F) as usize;
                            if 1 + len_bytes + off_bytes <= run_bytes.len() {
                                let _clen = parse_varlen_le(&run_bytes[1..], len_bytes);
                                let coff = parse_varlen_le_signed(&run_bytes[1 + len_bytes..], off_bytes);
                                lcn = (lcn as i64 + coff) as u64;
                                let iso_lba = ctx.part1_lba as u64 + lcn * ctx.sectors_per_cluster as u64;
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

fn scan_ntfs_dir(ctx: &FsCtx, files: &mut [DirEntry], file_count: &mut usize) {
    let rec_size = ctx.mft_record_size as usize;
    if rec_size > 4096 {
        return;
    }
    let mft_rec_lba = ctx.mft_start_lba + 5 * (rec_size as u64 / 512);
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

    let attrs_off = u16::from_le_bytes([rec_buf[0x14], rec_buf[0x15]]) as usize;
    if attrs_off >= rec_size {
        return;
    }
    let attrs = &rec_buf[attrs_off..];
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
        if atype == 0x90 {
            let is_nonresident = attrs[off + 8] != 0;
            let name_len = attrs[off + 9] as usize;
            let val_off = u16::from_le_bytes([attrs[off + 0x14], attrs[off + 0x15]]) as usize + name_len;
            if !is_nonresident && val_off < alen {
                let index_data = &attrs[off + val_off..off + alen];
                if index_data.len() >= 20 {
                    let entries_off = u32::from_le_bytes([index_data[0], index_data[1], index_data[2], index_data[3]]) as usize + 0x10;
                    if entries_off < index_data.len() {
                        let mut entries = &index_data[entries_off..];
                        while entries.len() >= 0x50 {
                            let mft_ref = u64::from_le_bytes(entries[0..8].try_into().unwrap());
                            let mft_rec = (mft_ref & 0xFFFFFFFFFFFF) as u32;
                            let ent_len = u16::from_le_bytes([entries[8], entries[9]]) as usize;
                            if ent_len < 8 || ent_len > entries.len() {
                                break;
                            }
                            let flags = entries[12];
                            let fn_off = 0x52usize;
                            if fn_off + 1 < entries.len() && (flags & 0x02) == 0 {
                                let name_len = entries[fn_off + 0x41] as usize;
                                if name_len > 0 && name_len <= 255 && fn_off + 0x42 + name_len * 2 <= entries.len() {
                                    let mut name_buf = [0u8; 256];
                                    let mut np = 0;
                                    for j in 0..name_len {
                                        let lo = entries[fn_off + 0x42 + j * 2];
                                        if lo < 0x80 && lo != 0 && np < 255 {
                                            name_buf[np] = lo;
                                            np += 1;
                                        } else if lo != 0 && np < 255 {
                                            name_buf[np] = b'?';
                                            np += 1;
                                        }
                                    }
                                    let is_iso = np >= 4 && name_buf[np - 4..np].eq_ignore_ascii_case(b".iso");
                                    if is_iso && *file_count < 64 && mft_rec > 0 {
                                        if let Some((lba, sz)) = get_ntfs_file_info(ctx, mft_rec) {
                                            files[*file_count] = DirEntry {
                                                name: name_buf, name_len: np,
                                                file_start_lba: lba, file_size: sz,
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

fn scan_filesystem(ctx: &FsCtx, files: &mut [DirEntry], count: &mut usize) {
    match ctx.fs {
        FsType::Exfat => scan_exfat_dir(ctx, files, count),
        FsType::Fat32 => scan_fat32_dir(ctx, files, count),
        FsType::Ntfs => scan_ntfs_dir(ctx, files, count),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Boot menu
// ═══════════════════════════════════════════════════════════════════════════

fn format_u64(mut v: u64, buf: &mut [u8; 20]) -> &[u8] {
    let mut pos = buf.len();
    if v == 0 {
        pos -= 1;
        buf[pos] = b'0';
    }
    while v > 0 && pos > 0 {
        pos -= 1;
        buf[pos] = (v % 10) as u8 + b'0';
        v /= 10;
    }
    &buf[pos..]
}

fn show_menu(files: &[DirEntry], count: usize, ctx: &FsCtx) -> ! {
    vga_clear(0x07);
    vga_print(0, 8, b"=== Choosable ISO Boot Menu ===", 0x1F);

    if count == 0 {
        vga_print(4, 10, b"No ISO files found.", 0x0C);
        vga_print(6, 8, b"Press any key to halt...", 0x07);
        kbd_wait_key();
        loop { unsafe { core::arch::asm!("hlt") } }
    }

    for i in 0..count {
        let row = 3 + i;
        if row > 22 {
            break;
        }
        let mut num_buf = [0u8; 20];
        let num_str = format_u64((i + 1) as u64, &mut num_buf);
        vga_print(row, 1, num_str, 0x0A);
        vga_print(row, 1 + num_str.len(), b". ", 0x07);
        if files[i].name_len > 0 {
            vga_print(row, 4, &files[i].name[..files[i].name_len], 0x07);
        }
        let size_mb = files[i].file_size / (1024 * 1024);
        let mut size_buf = [0u8; 20];
        let size_str = format_u64(size_mb, &mut size_buf);
        vga_print(row, 50, b"(", 0x07);
        vga_print(row, 51, size_str, 0x07);
        vga_print(row, 51 + size_str.len(), b" MiB)", 0x07);
    }

    let prompt_row = 3 + count + 1;
    vga_print(prompt_row, 1, b"Enter number (or 'r' to refresh): ", 0x07);

    loop {
        let sc = kbd_wait_key();
        let ch = scancode_to_ascii(sc);
        match ch {
            Some(b'r') | Some(b'R') => {
                let mut new_files: [DirEntry; 64] = unsafe { core::mem::zeroed() };
                let mut new_count: usize = 0;
                scan_filesystem(ctx, &mut new_files, &mut new_count);
                show_menu(&new_files, new_count, ctx);
            }
            Some(b'\n') => continue,
            Some(d) if d.is_ascii_digit() => {
                let idx = if d == b'0' { 9 } else { (d - b'1') as usize };
                if idx < count {
                    boot_iso(&files[idx], ctx);
                }
            }
            _ => {}
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Consts
// ═══════════════════════════════════════════════════════════════════════════

const BIOS_BOOT_ADDR: u32 = 0x7C00;
const BOOT_COOKIE_ADDR: u32 = 0x7B00;
const BOOT_COOKIE_MAGIC: u32 = 0x544F4F42;

fn read_iso_sectors(cluster_lba: u64, mut target_phys: u32, sector_count: u32) -> bool {
    let mut buf = [0u8; 512];
    for i in 0..sector_count {
        if !ata_read_sector(cluster_lba + i as u64, &mut buf) {
            return false;
        }
        let dst = target_phys as *mut u8;
        for j in 0..512 {
            unsafe { *dst.add(j) = buf[j]; }
        }
        target_phys += 512;
    }
    true
}

fn boot_iso(file: &DirEntry, ctx: &FsCtx) -> ! {
    vga_clear(0x0E);
    vga_print(2, 5, b"Loading ISO boot sector...", 0x0F);

    let iso_lba = file.file_start_lba;
    let boot_record_lba = iso_lba + 17 * 4;

    let mut boot_rec = [0u8; 512];
    if !ata_read_sector(boot_record_lba, &mut boot_rec) {
        vga_print(4, 2, b"Failed to read Boot Record.", 0x0C);
        kbd_wait_key();
        show_menu(&[], 0, ctx);
    }
    if &boot_rec[1..6] != b"CD001" {
        vga_print(4, 2, b"Invalid Boot Record.", 0x0C);
        kbd_wait_key();
        show_menu(&[], 0, ctx);
    }

    let catalog_iso_lba = u32::from_le_bytes([boot_rec[0x47], boot_rec[0x48], boot_rec[0x49], boot_rec[0x4A]]);
    let catalog_lba = iso_lba + catalog_iso_lba as u64 * 4;

    let mut catalog = [0u8; 512];
    if !ata_read_sector(catalog_lba, &mut catalog) {
        vga_print(5, 2, b"Failed to read Boot Catalog.", 0x0C);
        kbd_wait_key();
        show_menu(&[], 0, ctx);
    }

    let mut boot_image_iso_lba: u32 = 0;
    let mut boot_sector_count: u16 = 1;
    let mut found = false;
    for i in 0..(512 / 32) {
        let off = i * 32;
        if catalog[off] == 0x88 || catalog[off] == 0x90 {
            boot_sector_count = u16::from_le_bytes([catalog[off + 6], catalog[off + 7]]);
            if boot_sector_count == 0 {
                boot_sector_count = 4;
            }
            boot_image_iso_lba = u32::from_le_bytes([
                catalog[off + 8], catalog[off + 9], catalog[off + 10], catalog[off + 11],
            ]);
            found = true;
            break;
        }
    }

    if !found {
        vga_print(5, 2, b"No bootable entry found.", 0x0C);
        kbd_wait_key();
        show_menu(&[], 0, ctx);
    }

    let boot_image_lba = iso_lba + boot_image_iso_lba as u64 * 4;
    vga_print(7, 5, b"Loading boot image...", 0x07);

    if !read_iso_sectors(boot_image_lba, BIOS_BOOT_ADDR, boot_sector_count as u32) {
        vga_print(10, 2, b"Failed to read boot image.", 0x0C);
        kbd_wait_key();
        show_menu(&[], 0, ctx);
    }

    vga_print(10, 5, b"Rebooting...", 0x0F);
    let cookie_ptr = BOOT_COOKIE_ADDR as *mut u32;
    unsafe { *cookie_ptr = BOOT_COOKIE_MAGIC; }

    while inb(0x64) & 2 != 0 {
        unsafe { core::arch::asm!("pause") }
    }
    outb(0x64, 0xFE);
    loop { unsafe { core::arch::asm!("hlt") } }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Entry point
// ═══════════════════════════════════════════════════════════════════════════

#[no_mangle]
pub extern "C" fn _start() -> ! {
    vga_clear(0x07);
    vga_print(0, 0, b"  Choosable Kernel v0.3  ", 0x1F);
    vga_print(1, 0, b"==========================", 0x17);

    let (parts, part_count) = read_partitions();
    if part_count == 0 {
        vga_print(5, 2, b"No partitions found. Halted.", 0x0C);
        loop { unsafe { core::arch::asm!("hlt") } }
    }

    vga_print(2, 0, b"Reading partition 1...", 0x07);

    let part1_lba = parts[0].start_lba;
    let mut vbr = [0u8; 512];
    if !ata_read_sector(part1_lba as u64, &mut vbr) {
        vga_print(3, 2, b"Failed to read VBR.", 0x0C);
        loop { unsafe { core::arch::asm!("hlt") } }
    }

    // Detect filesystem
    let fs = if &vbr[3..11] == b"EXFAT   " {
        FsType::Exfat
    } else if &vbr[3..11] == b"NTFS    " {
        FsType::Ntfs
    } else if &vbr[0x52..0x5A] == b"FAT32   " {
        FsType::Fat32
    } else {
        vga_print(3, 2, b"Unknown filesystem.", 0x0C);
        loop { unsafe { core::arch::asm!("hlt") } }
    };

    // Parse BPB into FsCtx
    let mut ctx = FsCtx {
        fs, part1_lba,
        spc: 0, fat_start: 0, fat_len: 0, heap_start: 0, root_cluster: 0,
        mft_start_lba: 0, sectors_per_cluster: 0, mft_record_size: 0,
    };

    match fs {
        FsType::Exfat => {
            let spc_shift = vbr[109] as u32;
            if spc_shift >= 25 {
                vga_print(3, 2, b"Invalid SectorsPerClusterShift.", 0x0C);
                loop { unsafe { core::arch::asm!("hlt") } }
            }
            let cluster_bytes = (1u32 << spc_shift) * 512;
            let fat_off = u32::from_le_bytes([vbr[80], vbr[81], vbr[82], vbr[83]]) as u64;
            let fat_len = u32::from_le_bytes([vbr[84], vbr[85], vbr[86], vbr[87]]) as u64;
            let heap_off = u32::from_le_bytes([vbr[88], vbr[89], vbr[90], vbr[91]]) as u64;
            let root_cluster = u32::from_le_bytes([vbr[96], vbr[97], vbr[98], vbr[99]]);

            ctx.spc = cluster_bytes / 512;
            ctx.fat_start = part1_lba as u64 + fat_off;
            ctx.fat_len = fat_len;
            ctx.heap_start = part1_lba as u64 + heap_off;
            ctx.root_cluster = root_cluster;
            vga_print(3, 0, b"exFAT detected.", 0x0A);
        }
        FsType::Fat32 => {
            let spc = vbr[13] as u32;
            if spc == 0 {
                vga_print(3, 2, b"Invalid SPC.", 0x0C);
                loop { unsafe { core::arch::asm!("hlt") } }
            }
            let reserved = u16::from_le_bytes([vbr[14], vbr[15]]) as u64;
            let num_fats = vbr[16] as u64;
            let fat_sectors = u32::from_le_bytes([vbr[36], vbr[37], vbr[38], vbr[39]]) as u64;
            let root_cluster = u32::from_le_bytes([vbr[44], vbr[45], vbr[46], vbr[47]]);

            ctx.spc = spc;
            ctx.fat_start = part1_lba as u64 + reserved;
            ctx.fat_len = fat_sectors;
            ctx.heap_start = ctx.fat_start + num_fats * fat_sectors;
            ctx.root_cluster = root_cluster;
            vga_print(3, 0, b"FAT32 detected.", 0x0A);
        }
        FsType::Ntfs => {
            let spc = vbr[13] as u32;
            if spc == 0 {
                vga_print(3, 2, b"Invalid SPC.", 0x0C);
                loop { unsafe { core::arch::asm!("hlt") } }
            }
            let cluster_bytes = spc as u64 * 512;
            let mft_lcn = i64::from_le_bytes(vbr[0x30..0x38].try_into().unwrap());
            let mft_start_lba = part1_lba as u64 + (mft_lcn as u64) * spc as u64;
            let cpmr_raw = i32::from_le_bytes(vbr[0x40..0x44].try_into().unwrap());
            let mft_record_size: u64 = if cpmr_raw > 0 {
                cpmr_raw as u64 * cluster_bytes
            } else {
                (1u64 << (-cpmr_raw)) as u64
            };
            if mft_record_size == 0 || mft_record_size > 4096 {
                vga_print(3, 2, b"Invalid MFT record size.", 0x0C);
                loop { unsafe { core::arch::asm!("hlt") } }
            }
            ctx.spc = spc;
            ctx.sectors_per_cluster = spc;
            ctx.mft_start_lba = mft_start_lba;
            ctx.mft_record_size = mft_record_size;
            ctx.heap_start = part1_lba as u64;
            vga_print(3, 0, b"NTFS detected.", 0x0A);
        }
    }

    let mut files: [DirEntry; 64] = unsafe { core::mem::zeroed() };
    let mut file_count: usize = 0;
    scan_filesystem(&ctx, &mut files, &mut file_count);
    show_menu(&files, file_count, &ctx);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop { unsafe { core::arch::asm!("hlt") } }
}