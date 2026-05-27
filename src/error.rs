use thiserror::Error;

#[derive(Error, Debug)]
pub enum ChoosableError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Disk not found: {0}")]
    DiskNotFound(String),

    #[error("Disk is a partition, not a whole disk: {0}")]
    IsPartition(String),

    #[error("Disk already contains Choosable (version {0}). Use -u to update or -I to force install")]
    AlreadyInstalled(String),

    #[error("Disk does not contain Choosable. Use -i to install")]
    NotChoosableDisk,

    #[error("Disk is too small. Required: {required} bytes, available: {available} bytes")]
    DiskTooSmall { required: u64, available: u64 },

    #[error("4K native sector disk detected. Choosable does not support 4K native disks")]
    FourKNativeSector,

    #[error("MBR disk over 2TB. MBR does not support disks > 2TB. Use -g for GPT")]
    MbrOverflow,

    #[error("Failed to read MBR from disk: {0}")]
    MbrReadError(String),

    #[error("Invalid MBR signature: expected 0x55AA, got 0x{0:02X}{1:02X}")]
    InvalidMbrSignature(u8, u8),

    #[error("Invalid GPT signature: expected 'EFI PART'")]
    InvalidGptSignature,

    #[error("Choosable partition layout mismatch: {0}")]
    PartitionLayoutError(String),

    #[error("FAT/exFAT format failed")]
    FormatFailed,

    #[error("Disk write failed at offset {offset}: {source}")]
    WriteError {
        offset: u64,
        #[source]
        source: std::io::Error,
    },

    #[error("Disk read failed at offset {offset}: {source}")]
    ReadError {
        offset: u64,
        #[source]
        source: std::io::Error,
    },

    #[error("Unsupported filesystem in partition 1: {0}")]
    UnsupportedFilesystem(String),

    #[error("Required tool not found: {0}")]
    ToolNotFound(String),

    #[error("{0}")]
    Generic(String),
}

pub type Result<T> = std::result::Result<T, ChoosableError>;