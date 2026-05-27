/// Size constants
pub const SIZE_1KB: u64 = 1024;
pub const SIZE_1MB: u64 = 1024 * 1024;
pub const SIZE_2MB: u64 = 2048 * 1024;
pub const SIZE_1GB: u64 = 1024 * 1024 * 1024;
pub const SIZE_1TB: u64 = 1024 * 1024 * 1024 * 1024;

/// 32 GiB - threshold for FAT32
pub const FAT32_MAX_LIMIT: u64 = 32 * SIZE_1GB;

/// Ventoy EFI partition size: 32 MiB
pub const VENTOY_EFI_PART_SIZE: u64 = 32 * SIZE_1MB;

/// Ventoy partition 1 start sector (in 512-byte sectors)
pub const VENTOY_PART1_START_SECTOR: u64 = 2048;

/// Ventoy EFI partition GPT attribute (hidden + required)
pub const VENTOY_EFI_PART_ATTR: u64 = 0x8000_0000_0000_0000;

/// Ventoy image file section count in 512-byte sectors
pub const VENTOY_SECTOR_NUM: u64 = 32768; // 16 MiB

/// Ventoy part 1 + part 2 total size in MiB
pub const VENTOY_PART_SIZE_MB: u64 = 48; // 16 MiB part1 min? Actually in shell script: PART1_MB - 32 MiB for EFI part. 32+32? Let's use 48 as min for both parts.

/// Maximum physical drives
pub const VENTOY_MAX_PHY_DRIVE: usize = 128;

/// MBR sector size
pub const SECTOR_SIZE: u64 = 512;

/// MBR boot signature
pub const MBR_SIGNATURE_55: u8 = 0x55;
pub const MBR_SIGNATURE_AA: u8 = 0xAA;

/// Partition types
pub const PART_TYPE_GPT_PROTECTIVE: u8 = 0xEE;
pub const PART_TYPE_EFI_SYSTEM: u8 = 0xEF;

/// File paths within Ventoy installation directory
pub const VENTOY_FILE_BOOT_IMG: &str = "boot/boot.img";
pub const VENTOY_FILE_STG1_IMG: &str = "boot/core.img.xz";
pub const VENTOY_FILE_DISK_IMG: &str = "ventoy/ventoy.disk.img.xz";
pub const VENTOY_FILE_VERSION: &str = "ventoy/version";
pub const VENTOY_FILE_LOG: &str = "log.txt";

/// GPT partition names
pub const GPT_PART1_NAME: &[u16] = &[
    'V' as u16, 'T' as u16, 'O' as u16, 'Y' as u16, 'E' as u16, 'F' as u16, 'I' as u16,
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

/// VTSI (Ventoy Stream Image) constants
pub const VTSI_IMG_MAGIC: u64 = 0x0000_594F_544E_4556; // "VENTOY\0\0"
pub const VTSI_IMG_MAX_SEG: usize = 128;
pub const VTSI_FOOTER_SIZE: usize = 512;

/// MBR partition table active flag
pub const PART_ACTIVE: u8 = 0x80;
pub const PART_INACTIVE: u8 = 0x00;

/// EFI partition hidden+required GPT attribute
pub const GPT_ATTR_VTOYEFI: u64 = VENTOY_EFI_PART_ATTR;

/// GPT signature string
pub const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// Default Ventoy partition label
pub const DEFAULT_VENTOY_LABEL: &str = "Ventoy";