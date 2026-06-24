// ═══════════════════════════════════════════════════════════════════════════
//  IsoLocator — expresses "where the ISO lives"
// ═══════════════════════════════════════════════════════════════════════════
//
//  Purpose:
//    Encapsulates the location information that Choosable already knows
//    (partition GUID, file path) so it can be passed to GRUB / initramfs /
//    casper without relying solely on iso-scan/filename=.
//
//  IsoLocation:
//    Immutable struct describing the physical location of an ISO file.
//
//  IsoLocator:
//    Trait for resolving an ISO's location.
//    - FileBackedIsoLocator: ISO file on a USB partition (implemented).
//    - Additional backends (HTTP, PXE, RAM disk) can be added by
//      implementing this trait.

use crate::fs::PayloadEntry;
use crate::protocol::Guid;

/// Physical location of an ISO file on a block device.
///
/// This is information Choosable already knows — partition, path, LBA.
/// It enables passing richer boot parameters to the kernel instead of
/// relying entirely on the initramfs scanning every block device.
#[derive(Clone, Copy)]
pub struct IsoLocation {
    /// GPT partition GUID (or MBR signature as fallback)
    pub partition_guid: Guid,
    /// Partition number (1-based)
    pub partition_number: u32,
    /// File path within the partition (UTF-8, leading `/`)
    pub file_path: [u8; 256],
    /// Valid length of `file_path`
    pub file_path_len: usize,
    /// ISO file size in bytes
    pub file_size: u64,
    /// Partition start LBA (512-byte sectors)
    pub part1_lba: u64,
    /// ISO file start LBA (512-byte sectors)
    pub iso_lba: u64,
}

impl IsoLocation {
    /// Returns the file path as a `&[u8]` slice.
    pub fn path(&self) -> &[u8] {
        &self.file_path[..self.file_path_len]
    }

    /// Returns the byte offset of the ISO file relative to partition start.
    /// This is `(iso_lba - part1_lba) * 512`.
    pub fn offset_bytes(&self) -> u64 {
        (self.iso_lba - self.part1_lba) * 512
    }

    /// Returns just the file name (everything after the last `/`).
    pub fn file_name(&self) -> &[u8] {
        let path = self.path();
        if let Some(pos) = path.iter().rposition(|&c| c == b'/') {
            &path[pos + 1..]
        } else {
            path
        }
    }

    /// Returns the file path with the leading `/` stripped.
    /// Used for iso-scan/filename= boot parameters.
    pub fn path_without_leading_slash(&self) -> &[u8] {
        let p = self.path();
        if p.first() == Some(&b'/') {
            &p[1..]
        } else {
            p
        }
    }
}

/// Trait for resolving the physical location of a boot payload.
///
/// Choosable knows exactly where the payload lives; this trait exposes that
/// information.  Implementations can cover local block devices, HTTP sources,
/// PXE targets, RAM disks, etc.
pub trait IsoLocator: Sync {
    /// Resolve and return the ISO's location.
    fn locate(&self) -> IsoLocation;
}

/// Generalized boot payload locator.
///
/// Abstraction over `IsoLocator`, `WimLocator`, `VhdLocator`, `EfiLocator`
/// — anything that can produce a physical location for a boot payload.
/// This is the trait used by strategy dispatch and boot preparation.
pub trait BootPayloadLocator: Sync {
    type Location;

    /// Resolve and return the payload's physical location.
    fn locate(&self) -> Self::Location;

    /// Human-readable type name for logging (e.g. "ISO", "WIM", "VHD").
    fn payload_type(&self) -> &'static str;
}

impl BootPayloadLocator for FileBackedIsoLocator {
    type Location = IsoLocation;

    fn locate(&self) -> IsoLocation {
        self.location
    }

    fn payload_type(&self) -> &'static str {
        "ISO"
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  FileBackedIsoLocator — ISO file on a USB partition
// ═══════════════════════════════════════════════════════════════════════════

/// Locator for an ISO that is a regular file on a block-device partition.
///
/// Constructed from an `IsoEntry` (already found by the filesystem scanner)
/// together with the partition metadata.
pub struct FileBackedIsoLocator {
    pub location: IsoLocation,
}

impl FileBackedIsoLocator {
    /// Build a locator from a payload entry and partition information.
    pub fn from_payload_entry(
        entry: &PayloadEntry,
        partition_guid: Guid,
        partition_number: u32,
        part1_lba: u64,
    ) -> Self {
        let mut file_path = [0u8; 256];
        let name_len = entry.name_len.min(255);
        file_path[..name_len].copy_from_slice(&entry.name[..name_len]);

        FileBackedIsoLocator {
            location: IsoLocation {
                partition_guid,
                partition_number,
                file_path,
                file_path_len: name_len,
                file_size: entry.file_size,
                part1_lba,
                iso_lba: entry.file_start_lba,
            },
        }
    }
}

impl IsoLocator for FileBackedIsoLocator {
    fn locate(&self) -> IsoLocation {
        self.location
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  WimPayloadLocator — Windows Imaging Format payload
// ═══════════════════════════════════════════════════════════════════════════

pub struct WimPayloadLocator {
    pub location: IsoLocation,
}

impl WimPayloadLocator {
    pub fn from_payload_entry(entry: &PayloadEntry, partition_guid: Guid, partition_number: u32, part1_lba: u64) -> Self {
        let mut file_path = [0u8; 256];
        let name_len = entry.name_len.min(255);
        file_path[..name_len].copy_from_slice(&entry.name[..name_len]);
        WimPayloadLocator {
            location: IsoLocation {
                partition_guid,
                partition_number,
                file_path,
                file_path_len: name_len,
                file_size: entry.file_size,
                part1_lba,
                iso_lba: entry.file_start_lba,
            },
        }
    }
}

impl BootPayloadLocator for WimPayloadLocator {
    type Location = IsoLocation;
    fn locate(&self) -> IsoLocation { self.location }
    fn payload_type(&self) -> &'static str { "WIM" }
}

// ═══════════════════════════════════════════════════════════════════════════
//  VhdPayloadLocator — VHD/VHDX virtual disk payload
// ═══════════════════════════════════════════════════════════════════════════

pub struct VhdPayloadLocator {
    pub location: IsoLocation,
    pub is_vhdx: bool,
}

impl VhdPayloadLocator {
    pub fn from_payload_entry(entry: &PayloadEntry, partition_guid: Guid, partition_number: u32, part1_lba: u64) -> Self {
        let mut file_path = [0u8; 256];
        let name_len = entry.name_len.min(255);
        file_path[..name_len].copy_from_slice(&entry.name[..name_len]);
        let is_vhdx = matches!(entry.payload_type, crate::fs::PayloadType::Vhdx);
        VhdPayloadLocator {
            location: IsoLocation {
                partition_guid,
                partition_number,
                file_path,
                file_path_len: name_len,
                file_size: entry.file_size,
                part1_lba,
                iso_lba: entry.file_start_lba,
            },
            is_vhdx,
        }
    }
}

impl BootPayloadLocator for VhdPayloadLocator {
    type Location = IsoLocation;
    fn locate(&self) -> IsoLocation { self.location }
    fn payload_type(&self) -> &'static str { if self.is_vhdx { "VHDX" } else { "VHD" } }
}

// ═══════════════════════════════════════════════════════════════════════════
//  EfiPayloadLocator — raw EFI executable payload
// ═══════════════════════════════════════════════════════════════════════════

pub struct EfiPayloadLocator {
    pub location: IsoLocation,
}

impl EfiPayloadLocator {
    pub fn from_payload_entry(entry: &PayloadEntry, partition_guid: Guid, partition_number: u32, part1_lba: u64) -> Self {
        let mut file_path = [0u8; 256];
        let name_len = entry.name_len.min(255);
        file_path[..name_len].copy_from_slice(&entry.name[..name_len]);
        EfiPayloadLocator {
            location: IsoLocation {
                partition_guid,
                partition_number,
                file_path,
                file_path_len: name_len,
                file_size: entry.file_size,
                part1_lba,
                iso_lba: entry.file_start_lba,
            },
        }
    }
}

impl BootPayloadLocator for EfiPayloadLocator {
    type Location = IsoLocation;
    fn locate(&self) -> IsoLocation { self.location }
    fn payload_type(&self) -> &'static str { "EFI" }
}