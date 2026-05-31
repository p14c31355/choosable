// ═══════════════════════════════════════════════════════════════════════════
//  MBR / GPT partition table parser
// ═══════════════════════════════════════════════════════════════════════════

use crate::ata::ata_read_sector;

#[derive(Clone, Copy, Default)]
pub struct Partition {
    pub start_lba: u32,
    pub sector_count: u32,
    pub fs_type: u8,
}

pub fn read_partitions() -> ([Partition; 4], usize) {
    let mut buf = [0u8; 512];
    if !ata_read_sector(0, &mut buf) {
        return ([Partition::default(); 4], 0);
    }
    let mut parts = [Partition::default(); 4];
    let mut count = 0;

    let has_gpt = (0..4).any(|i| buf[446 + i * 16 + 4] == 0xEE);

    if has_gpt {
        let mut hdr = [0u8; 512];
        if ata_read_sector(1, &mut hdr) && &hdr[0..8] == b"EFI PART" {
            let entries_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
            let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap());
            let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap());
            if sz > 0 && n > 0 {
                let basic_data: [u8; 16] = [
                    0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7,
                    0x26, 0x99, 0xC7,
                ];
                let mut sec = [0u8; 512];
                let mut current_lba = 0u64;
                let mut loaded = false;
                for i in 0..n.min(128) {
                    let eoff = i as usize * sz as usize;
                    let lba = entries_lba + (eoff / 512) as u64;
                    let boff = eoff % 512;
                    if boff + 48 > 512 { continue; }
                    if !loaded || lba != current_lba {
                        if !ata_read_sector(lba, &mut sec) { break; }
                        current_lba = lba;
                        loaded = true;
                    }
                    if sec[boff..boff + 16] == basic_data {
                        let start_lba = u64::from_le_bytes(
                            sec[boff + 32..boff + 40].try_into().unwrap(),
                        );
                        let end_lba = u64::from_le_bytes(
                            sec[boff + 40..boff + 48].try_into().unwrap(),
                        );
                        let sectors = (end_lba + 1).saturating_sub(start_lba);
                        if sectors > 0 {
                            parts[count] = Partition {
                                start_lba: start_lba as u32,
                                sector_count: sectors as u32,
                                fs_type: 0x07,
                            };
                            count += 1;
                        }
                        break;
                    }
                }
            }
        }
        return (parts, count);
    }

    for i in 0..4 {
        let off = 446 + i * 16;
        let fs = buf[off + 4];
        let lba =
            u32::from_le_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]]);
        let sec = u32::from_le_bytes([
            buf[off + 12],
            buf[off + 13],
            buf[off + 14],
            buf[off + 15],
        ]);
        if sec > 0 {
            parts[count] = Partition {
                start_lba: lba,
                sector_count: sec,
                fs_type: fs,
            };
            count += 1;
        }
    }
    (parts, count)
}