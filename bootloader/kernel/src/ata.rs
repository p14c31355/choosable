// ═══════════════════════════════════════════════════════════════════════════
//  ATA PIO disk I/O
// ═══════════════════════════════════════════════════════════════════════════

use crate::io::{inb, inw, outb};

pub const ATA_PRIMARY_BASE: u16 = 0x1F0;
const ATA_DATA: u16 = ATA_PRIMARY_BASE + 0;
const ATA_SECCOUNT: u16 = ATA_PRIMARY_BASE + 2;
const ATA_LBA_LO: u16 = ATA_PRIMARY_BASE + 3;
const ATA_LBA_MID: u16 = ATA_PRIMARY_BASE + 4;
const ATA_LBA_HI: u16 = ATA_PRIMARY_BASE + 5;
const ATA_DRIVE: u16 = ATA_PRIMARY_BASE + 6;
const ATA_STATUS: u16 = ATA_PRIMARY_BASE + 7;
const ATA_CMD: u16 = ATA_PRIMARY_BASE + 7;

pub fn ata_read_sector(lba: u64, buf: &mut [u8; 512]) -> bool {
    let status = inb(ATA_STATUS);
    if status == 0xFF {
        return false;
    }
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

/// Read multiple consecutive sectors into a contiguous buffer.
/// `sector_count` sectors × 512 bytes each.
pub fn ata_read_sectors(lba: u64, dst: &mut [u8], sector_count: u32) -> bool {
    for i in 0..sector_count {
        let off = i as usize * 512;
        if off + 512 > dst.len() {
            return false;
        }
        let mut buf: [u8; 512] = [0; 512];
        if !ata_read_sector(lba + i as u64, &mut buf) {
            return false;
        }
        dst[off..off + 512].copy_from_slice(&buf);
    }
    true
}