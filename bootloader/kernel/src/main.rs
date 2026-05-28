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
        unsafe { *VGA.add(i * 2) = b' '; *VGA.add(i * 2 + 1) = attr; }
    }
}

fn vga_print(row: usize, col: usize, s: &[u8], attr: u8) {
    let mut off = (row * VGA_COLS + col) * 2;
    for &ch in s {
        if ch == 0 { break; }
        unsafe { *VGA.add(off) = ch; *VGA.add(off + 1) = attr; }
        off += 2;
    }
}

fn vga_print_byte(val: u8, row: usize, col: usize, attr: u8) {
    fn hex(b: u8) -> u8 { if b < 10 { b + b'0' } else { b - 10 + b'A' } }
    let mut buf = [hex(val >> 4), hex(val & 0xF)];
    vga_print(row, col, &buf, attr);
}

fn vga_print_u32(v: u32, row: usize, col: usize, attr: u8) {
    for i in 0..8 {
        vga_print_byte((v >> (28 - i * 4)) as u8, row, col + i, attr);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  I/O port helpers
// ═══════════════════════════════════════════════════════════════════════════

fn outb(port: u16, val: u8)  { unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") val) } }
fn inb(port: u16) -> u8      { let v: u8; unsafe { core::arch::asm!("in al, dx", out("al") v, in("dx") port) }; v }

fn outw(port: u16, val: u16) { unsafe { core::arch::asm!("out dx, ax", in("dx") port, in("ax") val) } }
fn inw(port: u16) -> u16     { let v:u16; unsafe { core::arch::asm!("in ax, dx", out("ax") v, in("dx") port) }; v }

fn outl(port: u16, val: u32) { unsafe { core::arch::asm!("out dx, eax", in("dx") port, in("eax") val) } }
fn inl(port: u16) -> u32     { let v:u32; unsafe { core::arch::asm!("in eax, dx", out("eax") v, in("dx") port) }; v }

// ═══════════════════════════════════════════════════════════════════════════
//  ATA PIO disk I/O
// ═══════════════════════════════════════════════════════════════════════════

const ATA_PRIMARY_BASE: u16 = 0x1F0;
const ATA_DATA:     u16 = ATA_PRIMARY_BASE + 0; // 16-bit data
const ATA_ERR:      u16 = ATA_PRIMARY_BASE + 1;
const ATA_SECCOUNT: u16 = ATA_PRIMARY_BASE + 2;
const ATA_LBA_LO:   u16 = ATA_PRIMARY_BASE + 3;
const ATA_LBA_MID:  u16 = ATA_PRIMARY_BASE + 4;
const ATA_LBA_HI:   u16 = ATA_PRIMARY_BASE + 5;
const ATA_DRIVE:    u16 = ATA_PRIMARY_BASE + 6;
const ATA_STATUS:   u16 = ATA_PRIMARY_BASE + 7;
const ATA_CMD:      u16 = ATA_PRIMARY_BASE + 7;

fn ata_read_sector(lba: u32, buf: &mut [u8; 512]) -> bool {
    // Wait for drive ready (BSY=0, DRQ=0)
    while inb(ATA_STATUS) & 0x80 != 0 {}  // BSY
    while inb(ATA_STATUS) & 0x08 != 0 {}  // wait DRQ=0

    // Select drive, LBA mode
    outb(ATA_DRIVE, 0xE0 | ((lba >> 24) as u8 & 0x0F));
    outb(ATA_SECCOUNT, 1);
    outb(ATA_LBA_LO,  lba as u8);
    outb(ATA_LBA_MID, (lba >> 8) as u8);
    outb(ATA_LBA_HI,  (lba >> 16) as u8);
    outb(ATA_CMD, 0x20); // READ SECTORS

    // Wait for data ready (BSY=0, DRQ=1, ERR=0)
    let mut timeout = 0xFFFFF;
    loop {
        let status = inb(ATA_STATUS);
        if status & 0x01 != 0 { return false; } // ERR
        if status & 0x08 != 0 { break; }        // DRQ
        timeout -= 1;
        if timeout == 0 { return false; }
    }

    // Read 256 words
    let buf16 = buf.as_mut_ptr() as *mut u16;
    for i in 0..256 {
        unsafe { *buf16.add(i) = inw(ATA_DATA); }
    }
    true
}

// ═══════════════════════════════════════════════════════════════════════════
//  Simple keyboard input
// ═══════════════════════════════════════════════════════════════════════════

fn kbd_poll() -> Option<u8> {
    // Check if data available (port 0x64 bit 0)
    if inb(0x64) & 1 == 0 { return None; }
    Some(inb(0x60))
}

// Wait for a keypress, return scancode (simple)
fn kbd_wait_key() -> u8 {
    loop {
        // Busy-wait until not empty (bit 1 = input buffer full → wait)
        while inb(0x64) & 2 != 0 {}
        if let Some(sc) = kbd_poll() { return sc; }
    }
}

// US QWERTY scancode→ASCII lookup (subset for menu navigation)
fn scancode_to_ascii(sc: u8) -> Option<u8> {
    match sc {
        0x02..=0x0A => Some(b'1' + (sc - 0x02)),  // 1-9
        0x0B => Some(b'0'),                         // 0
        0x10 => Some(b'q'), 0x11 => Some(b'w'), 0x12 => Some(b'e'), 0x13 => Some(b'r'),
        0x14 => Some(b't'), 0x15 => Some(b'y'), 0x16 => Some(b'u'), 0x17 => Some(b'i'),
        0x18 => Some(b'o'), 0x19 => Some(b'p'),
        0x1E => Some(b'a'), 0x1F => Some(b's'), 0x20 => Some(b'd'), 0x21 => Some(b'f'),
        0x22 => Some(b'g'), 0x23 => Some(b'h'), 0x24 => Some(b'j'), 0x25 => Some(b'k'),
        0x26 => Some(b'l'),
        0x2C => Some(b'z'), 0x2D => Some(b'x'), 0x2E => Some(b'c'), 0x2F => Some(b'v'),
        0x30 => Some(b'b'), 0x31 => Some(b'n'), 0x32 => Some(b'm'),
        0x39 => Some(b' '),
        0x1C => Some(b'\n'), // Enter
        0x0E => Some(0x08),  // Backspace
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
        return ([Partition{start_lba:0,sector_count:0,fs_type:0};4], 0);
    }
    let mut parts = [Partition{start_lba:0,sector_count:0,fs_type:0};4];
    let mut count = 0;
    for i in 0..4 {
        let off = 446 + i * 16;
        let fs = buf[off + 4];
        let lba = u32::from_le_bytes([buf[off+8], buf[off+9], buf[off+10], buf[off+11]]);
        let sec = u32::from_le_bytes([buf[off+12], buf[off+13], buf[off+14], buf[off+15]]);
        if sec > 0 {
            parts[count] = Partition { start_lba: lba, sector_count: sec, fs_type: fs };
            count += 1;
        }
    }
    (parts, count)
}

// ═══════════════════════════════════════════════════════════════════════════
//  exFAT VBR parser (minimal — gets cluster heap start, cluster size, FAT)
// ═══════════════════════════════════════════════════════════════════════════

struct ExfatInfo {
    cluster_size_bytes: u32,    // bytes per cluster (2^SectorsPerClusterShift * 512)
    fat_offset_sectors: u64,    // FAT table start (sector)
    fat_length_sectors: u64,    // FAT table length in sectors
    cluster_heap_offset_sectors: u64, // first cluster sector
    root_dir_cluster: u32,      // cluster number of root directory
}

fn parse_exfat_vbr(part_start_lba: u32) -> Option<ExfatInfo> {
    let mut buf = [0u8; 512];
    if !ata_read_sector(part_start_lba, &mut buf) { return None; }

    // Check exFAT signature at offset 3: "EXFAT   "
    if &buf[3..11] != b"EXFAT   " { return None; }

    let sector_shift = buf[108]; // SectorsPerClusterShift (offset 108 in VBR)
    let cluster_count = u32::from_le_bytes([buf[92], buf[93], buf[94], buf[95]]);
    let fat_offset = u32::from_le_bytes([buf[80], buf[81], buf[82], buf[83]]);
    let fat_length = u32::from_le_bytes([buf[84], buf[85], buf[86], buf[87]]);
    let cluster_heap_offset = u32::from_le_bytes([buf[88], buf[89], buf[90], buf[91]]);
    let root_dir_cluster = u32::from_le_bytes([buf[96], buf[97], buf[98], buf[99]]);

    Some(ExfatInfo {
        cluster_size_bytes: (1u32 << sector_shift) * 512,
        fat_offset_sectors: part_start_lba as u64 + fat_offset as u64,
        fat_length_sectors: fat_length as u64,
        cluster_heap_offset_sectors: part_start_lba as u64 + cluster_heap_offset as u64,
        root_dir_cluster,
    })
}

// Read a sector from within a cluster chain
fn read_cluster_sector(info: &ExfatInfo, cluster: u32, sector_in_cluster: u32, buf: &mut [u8; 512]) -> bool {
    if cluster < 2 { return false; }
    let sector = info.cluster_heap_offset_sectors
        + (cluster - 2) as u64 * (info.cluster_size_bytes as u64 / 512)
        + sector_in_cluster as u64;
    ata_read_sector(sector as u32, buf)
}

// Read FAT entry for a cluster (returns next cluster number, or 0xFFFFFFFF for end-of-chain)
fn read_fat_entry(info: &ExfatInfo, cluster: u32) -> u32 {
    // FAT entry is 4 bytes per cluster
    let fat_sector = info.fat_offset_sectors + (cluster as u64 * 4 / 512);
    let fat_offset = (cluster as u64 * 4 % 512) as usize;
    let mut buf = [0u8; 512];
    if !ata_read_sector(fat_sector as u32, &mut buf) { return 0xFFFFFFFF; }
    u32::from_le_bytes([buf[fat_offset], buf[fat_offset+1], buf[fat_offset+2], buf[fat_offset+3]])
}

// ═══════════════════════════════════════════════════════════════════════════
//  Directory entry types (exFAT uses 32-byte entries in sets)
// ═══════════════════════════════════════════════════════════════════════════

const EXFAT_ENTRY_FILE: u8 = 0x85;
const EXFAT_ENTRY_NAME: u8 = 0xC1;

struct DirEntry {
    name: [u8; 256],    // file name (UTF-16→ASCII simplified)
    name_len: usize,
    is_iso: bool,       // .iso extension
    start_cluster: u32,
    file_size: u64,
}

// Simplified UTF-16 to ASCII conversion (handles ASCII-range BMP characters)
fn utf16le_to_ascii(src: &[u8], max_bytes: usize, dst: &mut [u8]) -> usize {
    let mut di = 0;
    let mut si = 0;
    let limit = src.len().min(max_bytes);
    while si + 1 < limit && di < dst.len() {
        let lo = src[si];
        let hi = src[si+1];
        let cp = lo as u16 | ((hi as u16) << 8);
        si += 2;
        if cp == 0 { break; }
        if cp < 0x80 { dst[di] = cp as u8; di += 1; }
        else { dst[di] = b'?'; di += 1; }
    }
    di
}

fn scan_directory(info: &ExfatInfo, root_cluster: u32, files: &mut [DirEntry], file_count: &mut usize) {
    let max_files = files.len();
    let mut cluster = root_cluster;
    let sectors_per_cluster = info.cluster_size_bytes / 512;

    'outer: loop {
        for sec in 0..sectors_per_cluster {
            let mut buf = [0u8; 512];
            if !read_cluster_sector(info, cluster, sec, &mut buf) { return; }

            let entries = &buf; // 512 bytes = 16 x 32-byte entries
            let mut i = 0;
            while i < 16 {
                let off = i * 32;
                let entry_type = entries[off];
                if entry_type == 0 { return; } // end of directory
                if entry_type == EXFAT_ENTRY_FILE {
                    let attrs = u16::from_le_bytes([entries[off+4], entries[off+5]]);
                    let _is_dir = attrs & 0x10 != 0;

                    // Stream Extension entry (0xC0) must follow the File entry
                    let stream_off = off + 32;
                    if i + 2 < 16 && entries[stream_off] == 0xC0 {
                        let start_cl = u32::from_le_bytes([
                            entries[stream_off + 20],
                            entries[stream_off + 21],
                            entries[stream_off + 22],
                            entries[stream_off + 23],
                        ]);
                        let size = u64::from_le_bytes([
                            entries[stream_off + 24], entries[stream_off + 25],
                            entries[stream_off + 26], entries[stream_off + 27],
                            entries[stream_off + 28], entries[stream_off + 29],
                            entries[stream_off + 30], entries[stream_off + 31],
                        ]);

                        // File Name entry (0xC1) follows the Stream Extension entry
                        let name_off = off + 64;
                        if entries[name_off] == EXFAT_ENTRY_NAME {
                            let name_len = entries[stream_off + 3] as usize; // name length from Stream Extension
                            let mut name_buf = [0u8; 256];
                            let name_actual = utf16le_to_ascii(&entries[name_off + 2..], name_len * 2, &mut name_buf);
                            let name_str = &name_buf[..name_actual];

                            // Check if it's an ISO file
                            let is_iso = name_actual >= 4
                                && (name_str[name_actual-4..].eq_ignore_ascii_case(b".iso")
                                ||  name_str[name_actual-4..].eq_ignore_ascii_case(b".ISO"));

                            if is_iso && *file_count < max_files {
                                let mut n = [0u8; 256];
                                n[..name_actual].copy_from_slice(name_str);
                                files[*file_count] = DirEntry {
                                    name: n,
                                    name_len: name_actual,
                                    is_iso: true,
                                    start_cluster: start_cl,
                                    file_size: size,
                                };
                                *file_count += 1;
                            }
                            i += 3; // skip file + stream + name entries
                            continue;
                        }
                    }
                }
                i += 1;
            }
        }

        // Next cluster in chain
        let next = read_fat_entry(info, cluster);
        if next < 2 || next >= 0xFFFFFFF8 { break; }
        cluster = next;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Boot menu
// ═══════════════════════════════════════════════════════════════════════════

fn show_menu(files: &[DirEntry], count: usize, part_lba: u32, info: &ExfatInfo) -> ! {
    vga_clear(0x07);

    vga_print(0, 8, b"=== Choosable ISO Boot Menu ===", 0x1F);
    vga_print(1, 0, b"===============================================", 0x17);

    if count == 0 {
        vga_print(4, 10, b"No ISO files found on this drive.", 0x0C);
        vga_print(6, 8,  b"Press any key to halt...", 0x07);
        kbd_wait_key();
        loop { unsafe { core::arch::asm!("hlt") } }
    }

    for i in 0..count {
        let row = 3 + i;
        if row > 22 { break; }
        let num = (i + 1) as u8;
        vga_print_byte(num, row, 1, 0x0A);
        vga_print(row, 3, b". ", 0x07);
        vga_print(row, 5, &files[i].name[..files[i].name_len], 0x0F);

        // Show file size
        let size_mb = files[i].file_size / (1024 * 1024);
        let mut size_buf = [0u8; 20];
        let size_str = format_u64(size_mb, &mut size_buf);
        vga_print(row, 50, b" (", 0x07);
        vga_print(row, 52, size_str, 0x07);
        vga_print(row, 52 + size_str.len(), b" MiB)", 0x07);
    }

    let prompt_row = 3 + count + 1;
    vga_print(prompt_row, 1, b"Enter number to boot (or 'r' to refresh): ", 0x07);

    loop {
        let sc = kbd_wait_key();
        let ch = scancode_to_ascii(sc);

        match ch {
            Some(b'r') | Some(b'R') => {
                // Refresh: re-scan partition
                let mut new_files: [DirEntry; 64] = unsafe { core::mem::zeroed() };
                let mut new_count: usize = 0;
                scan_directory(info, info.root_dir_cluster, &mut new_files, &mut new_count);
                show_menu(&new_files, new_count, part_lba, info);
            }
            Some(b'\n') => continue,
            Some(d) if d.is_ascii_digit() => {
                let idx = (d - b'1') as usize;
                if idx < count {
                    boot_iso(&files[idx], part_lba, info);
                }
            }
            _ => {}
        }
    }
}

/// El Torito Boot Record offsets (sector 17 in ISO9660)
const ET_BOOT_RECORD_SECTOR: u64 = 17;

/// Physical address where MBR was loaded (BIOS convention)
const BIOS_BOOT_ADDR: u32 = 0x7C00;

/// Boot cookie address and magic value
const BOOT_COOKIE_ADDR: u32 = 0x7DF0;
const BOOT_COOKIE_MAGIC: u32 = 0x544F4F42; // "BOOT" (little-endian)

/// Read `sector_count` sectors starting at ISO cluster offset `cluster_lba` into a physical buffer
fn read_iso_sectors(cluster_lba: u64, mut target_phys: u32, sector_count: u32) -> bool {
    let mut buf = [0u8; 512];
    for i in 0..sector_count {
        if !ata_read_sector((cluster_lba + i as u64) as u32, &mut buf) {
            return false;
        }
        // Copy to physical memory (stage2 identity-maps low 1GB, so phys == virt)
        let dst = target_phys as *mut u8;
        for j in 0..512 {
            unsafe { *dst.add(j) = buf[j]; }
        }
        target_phys += 512;
    }
    true
}

fn boot_iso(file: &DirEntry, _part_lba: u32, info: &ExfatInfo) -> ! {
    vga_clear(0x0E);
    vga_print(2, 5, b"Loading ISO boot sector...", 0x0F);

    // Calculate the starting LBA of the ISO file on disk
    let iso_start_lba = info.cluster_heap_offset_sectors
        + (file.start_cluster as u64 - 2) * (info.cluster_size_bytes as u64 / 512);

    // Step 1: Read El Torito Boot Record (sector 17 of the ISO)
    // El Torito Boot Record is at byte 0x8000 in an ISO 9660 image (assuming 2048-byte sectors)
    // Wait -- our ISO is stored as raw bytes in exFAT, so sectors are 512 bytes.
    // ISO9660 Primary Volume Descriptor is at logical sector 16 (16 * 2048 bytes).
    // With 512-byte sectors, that's at offset 16 * 2048 / 512 = 64 sectors into the file.
    // The Boot Record is at sector 17 of the ISO = offset 17 * 2048 / 512 = 68 sectors.
    // Actually, the El Torito Boot Record sits at ISO9660 sector 17 (after PVD at sector 16).
    // Since ISO9660 sectors are 2048 bytes, and we use 512-byte sectors:
    let pvd_lba = iso_start_lba + 16 * 4; // PVD at ISO sector 16 → 16*4 = 64 of our sectors
    let boot_record_lba = iso_start_lba + 17 * 4; // Boot Record at ISO sector 17 → 17*4 = 68 of our sectors

    // Read PVD to get the root directory info (needed for file access, but we skip that)
    // Read El Torito Boot Record
    let mut boot_rec = [0u8; 512];
    if !ata_read_sector(boot_record_lba as u32, &mut boot_rec) {
        vga_print(4, 2, b"Failed to read El Torito Boot Record.", 0x0C);
        kbd_wait_key();
        show_menu(&[], 0, 0, info);
    }

    // Validate Boot Record signature (offset 0 in the 2048-byte sector = offset 0 in our 512-byte * 4)
    // Boot Record Validation Entry starts at offset 0 → identifier byte 0 = 0x01
    // Wait, we read 512 bytes, but the 2048-byte Boot Record has multiple fields.
    // The first sector (512 bytes) of the Boot Record contains the Validation Entry at offset 0.
    if boot_rec[0] != 0x01 {
        vga_print(4, 2, b"Invalid Boot Record identifier (expected 0x01).", 0x0C);
        kbd_wait_key();
        show_menu(&[], 0, 0, info);
    }

    // Validation Entry: offset 0x47 (71) = Boot Catalog LBA (4 bytes)
    // Wait, the El Torito spec:
    // Byte 0: Header ID (0x01)
    // Bytes 0x47-0x4A: Absolute pointer to Boot Catalog (LBA in ISO9660 sectors)
    let catalog_iso_lba = u32::from_le_bytes([boot_rec[0x47], boot_rec[0x48], boot_rec[0x49], boot_rec[0x4A]]);
    // Convert ISO9660 LBA to our 512-byte sector LBA
    let catalog_lba = iso_start_lba + catalog_iso_lba as u64 * 4;

    vga_print(3, 5, b"El Torito catalog at LBA: ", 0x07);
    let mut lba_str = [0u8; 20];
    let lba_slice = format_u64(catalog_lba, &mut lba_str);
    vga_print(3, 30, lba_slice, 0x0A);

    // Step 2: Read Boot Catalog to find the default boot entry
    let mut catalog = [0u8; 512];
    if !ata_read_sector(catalog_lba as u32, &mut catalog) {
        vga_print(5, 2, b"Failed to read Boot Catalog.", 0x0C);
        kbd_wait_key();
        show_menu(&[], 0, 0, info);
    }

    // Boot Catalog:
    // Byte 0: Header ID (0x01)
    // Byte 1: Platform ID (0 = 80x86)
    // Bytes 0x1E: Checksum word
    // Then entry records at offset 32 (0x20):
    //   Byte 0: Boot Indicator (0x88 = bootable)
    //   Byte 1: Boot media type (0 = no emulation)
    //   Bytes 8-11: Load segment (or 0 for default 0x7C0)
    //   Bytes 28-31: Start LBA of boot image (in ISO9660 sectors)

    // Find the first default entry (0x88)
    let mut boot_image_iso_lba: u32 = 0;
    let mut boot_sector_count: u16 = 1;
    let mut boot_load_seg: u16 = 0x07C0; // default: 0x7C0:0x0000 = phys 0x7C00

    let mut found = false;
    for i in 0..(512 / 32) {
        let off = i * 32;
        if catalog[off] == 0x88 { // Bootable entry
            let media_type = catalog[off + 1];
            boot_load_seg = u16::from_le_bytes([catalog[off + 8], catalog[off + 9]]);
            if boot_load_seg == 0 { boot_load_seg = 0x07C0; }
            boot_sector_count = u16::from_le_bytes([catalog[off + 12], catalog[off + 13]]);
            // Sector count: if 0, use the full emulated size
            if boot_sector_count == 0 { boot_sector_count = 4; } // default 4 emulated sectors (2048 bytes)
            boot_image_iso_lba = u32::from_le_bytes([catalog[off + 28], catalog[off + 29], catalog[off + 30], catalog[off + 31]]);

            vga_print(4, 5, b"Found boot entry, type: ", 0x07);
            vga_print_byte(media_type, 4, 27, 0x0A);
            vga_print(5, 5, b"Boot image sectors: ", 0x07);
            let mut sc_str = [0u8; 20];
            let sc_slice = format_u64(boot_sector_count as u64, &mut sc_str);
            vga_print(5, 25, sc_slice, 0x0A);

            found = true;
            break;
        }
    }

    if !found {
        vga_print(5, 2, b"No bootable El Torito entry found.", 0x0C);
        kbd_wait_key();
        show_menu(&[], 0, 0, info);
    }

    // Step 3: Read boot image into physical memory at 0x7C00
    let boot_image_lba = iso_start_lba + boot_image_iso_lba as u64 * 4;

    vga_print(7, 5, b"Loading boot image to 0x7C00...", 0x07);
    vga_print(8, 5, b"Boot image LBA: ", 0x07);
    let mut bi_str = [0u8; 20];
    let bi_slice = format_u64(boot_image_lba, &mut bi_str);
    vga_print(8, 21, bi_slice, 0x0A);

    if !read_iso_sectors(boot_image_lba, BIOS_BOOT_ADDR, boot_sector_count as u32) {
        vga_print(10, 2, b"Failed to read boot image.", 0x0C);
        kbd_wait_key();
        show_menu(&[], 0, 0, info);
    }

    // Step 4: Set boot cookie and warm-reboot
    vga_print(10, 5, b"Setting boot cookie and rebooting...", 0x0F);

    // Write boot cookie magic at 0x7DF0
    let cookie_ptr = BOOT_COOKIE_ADDR as *mut u32;
    unsafe { *cookie_ptr = BOOT_COOKIE_MAGIC; }

    // Trigger keyboard controller reset (warm boot)
    // Wait for keyboard controller to be ready
    while inb(0x64) & 2 != 0 { unsafe { core::arch::asm!("pause") } }
    outb(0x64, 0xFE); // Pulse CPU reset line

    // If reset fails, halt
    loop { unsafe { core::arch::asm!("hlt") } }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Simple u64→string formatter (fixed buffer)
// ═══════════════════════════════════════════════════════════════════════════

fn format_u64(mut v: u64, buf: &mut [u8; 20]) -> &[u8] {
    if v == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut pos = buf.len();
    while v > 0 && pos > 0 {
        pos -= 1;
        buf[pos] = (v % 10) as u8 + b'0';
        v /= 10;
    }
    &buf[pos..]
}

// ═══════════════════════════════════════════════════════════════════════════
//  Entry point
// ═══════════════════════════════════════════════════════════════════════════

#[no_mangle]
pub extern "C" fn _start() -> ! {
    vga_clear(0x07);

    vga_print(0, 0, b"  Choosable Kernel v0.2  ", 0x1F);
    vga_print(1, 0, b"==========================", 0x17);

    // Read partition table from MBR
    let (parts, part_count) = read_partitions();

    vga_print(2, 0, b"Partitions found: ", 0x07);
    let mut pc_str = [0u8; 20];
    let pc_slice = format_u64(part_count as u64, &mut pc_str);
    vga_print(2, 18, pc_slice, 0x0A);

    // Show partition info
    for i in 0..part_count {
        let row = 3 + i;
        if row > 10 { break; }
        let num = (i+1) as u8 + b'0';
        vga_print_byte(num, row, 1, 0x0A);
        vga_print(row, 2, b". FS:", 0x07);
        vga_print_byte(parts[i].fs_type, row, 7, 0x0F);
        vga_print(row, 10, b" LBA:", 0x07);
        vga_print_u32(parts[i].start_lba, row, 15, 0x0B);
        vga_print(row, 24, b" Sectors:", 0x07);
        vga_print_u32(parts[i].sector_count, row, 33, 0x0B);
    }

    // Try to parse exFAT on partition 1 (our data partition)
    if part_count >= 1 {
        vga_print(5, 0, b"Scanning partition 1 for ISO files...", 0x07);

        if let Some(exfat) = parse_exfat_vbr(parts[0].start_lba) {
            vga_print(6, 0, b"exFAT detected. Cluster size: ", 0x0A);
            let mut cs_str = [0u8; 20];
            let cs_slice = format_u64(exfat.cluster_size_bytes as u64, &mut cs_str);
            vga_print(6, 29, cs_slice, 0x0A);
            vga_print(6, 29 + cs_slice.len(), b" bytes", 0x07);

            // Scan root directory
            let mut files: [DirEntry; 64] = unsafe { core::mem::zeroed() };
            let mut file_count: usize = 0;
            scan_directory(&exfat, exfat.root_dir_cluster, &mut files, &mut file_count);

            show_menu(&files, file_count, parts[0].start_lba, &exfat);
        } else {
            vga_print(7, 2, b"No valid exFAT filesystem on partition 1.", 0x0C);
        }
    } else {
        vga_print(5, 2, b"No partitions found. Halted.", 0x0C);
    }

    loop { unsafe { core::arch::asm!("hlt") } }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop { unsafe { core::arch::asm!("hlt") } }
}

// Zeroed array helper for DirEntry (needed because we can't derive Default in no_std without alloc)
impl DirEntry {
    fn zero() -> Self {
        DirEntry {
            name: [0u8; 256],
            name_len: 0,
            is_iso: false,
            start_cluster: 0,
            file_size: 0,
        }
    }
}