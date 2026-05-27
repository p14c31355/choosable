/// Size constants
pub const SIZE_1KB: u64 = 1024;
pub const SIZE_1MB: u64 = 1024 * 1024;
pub const SIZE_2MB: u64 = 2048 * 1024;
pub const SIZE_1GB: u64 = 1024 * 1024 * 1024;
pub const SIZE_1TB: u64 = 1024 * 1024 * 1024 * 1024;

/// 32 GiB - threshold for FAT32
pub const FAT32_MAX_LIMIT: u64 = 32 * SIZE_1GB;

/// Choosable EFI partition size: 32 MiB
pub const CHOOSABLE_EFI_PART_SIZE: u64 = 32 * SIZE_1MB;

/// Choosable partition 1 start sector (in 512-byte sectors)
pub const CHOOSABLE_PART1_START_SECTOR: u64 = 2048;

/// Choosable EFI partition GPT attribute (hidden + required)
pub const CHOOSABLE_EFI_PART_ATTR: u64 = 0x8000_0000_0000_0000;

/// Choosable image file section count in 512-byte sectors
pub const CHOOSABLE_SECTOR_NUM: u64 = 65536; // 32 MiB (Ventoy: VENTOY_SECTOR_NUM=65536)

/// Choosable part 1 + part 2 total size in MiB
pub const CHOOSABLE_PART_SIZE_MB: u64 = 33; // min: 1 MiB part1 + 32 MiB EFI part

/// Maximum physical drives
pub const CHOOSABLE_MAX_PHY_DRIVE: usize = 128;

/// MBR sector size
pub const SECTOR_SIZE: u64 = 512;

/// MBR boot signature
pub const MBR_SIGNATURE_55: u8 = 0x55;
pub const MBR_SIGNATURE_AA: u8 = 0xAA;

/// Partition types
pub const PART_TYPE_GPT_PROTECTIVE: u8 = 0xEE;
pub const PART_TYPE_EFI_SYSTEM: u8 = 0xEF;

/// File paths within Choosable installation directory
pub const CHOOSABLE_FILE_BOOT_IMG: &str = "boot/boot.img";
pub const CHOOSABLE_FILE_STG1_IMG: &str = "boot/core.img.xz";
pub const CHOOSABLE_FILE_DISK_IMG: &str = "choosable/choosable.disk.img.xz";
pub const CHOOSABLE_FILE_VERSION: &str = "choosable/version";
pub const CHOOSABLE_FILE_LOG: &str = "log.txt";

/// GPT partition names
pub const GPT_PART1_NAME: &[u16] = &[
    'C' as u16, 'Z' as u16, 'B' as u16, 'L' as u16, 'E' as u16, 'F' as u16, 'I' as u16,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// Well-known GPT partition type GUIDs
pub const GPT_TYPE_EFI_SYSTEM: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11,
    0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];

pub const GPT_TYPE_BASIC_DATA: [u8; 16] = [
    0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44,
    0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
];

/// VTSI (Choosable Stream Image) constants
pub const VTSI_IMG_MAGIC: u64 = 0x0000_594F_544E_4556; // "CHOOSABLE\0\0"
pub const VTSI_IMG_MAX_SEG: usize = 128;
pub const VTSI_FOOTER_SIZE: usize = 512;

/// MBR partition table active flag
pub const PART_ACTIVE: u8 = 0x80;
pub const PART_INACTIVE: u8 = 0x00;

/// EFI partition hidden+required GPT attribute
pub const GPT_ATTR_VTOYEFI: u64 = CHOOSABLE_EFI_PART_ATTR;

/// GPT signature string
pub const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// Default Choosable partition label
pub const DEFAULT_CHOOSABLE_LABEL: &str = "Choosable";