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

/// Trait for resolving the physical location of an ISO.
///
/// Choosable knows exactly where the ISO lives; this trait exposes that
/// information as an `IsoLocation`.  Implementations can cover local
/// block devices, HTTP sources, PXE targets, RAM disks, etc.
pub trait IsoLocator: Sync {
    /// Resolve and return the ISO's location.
    fn locate(&self) -> IsoLocation;
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
    /// Build a locator from an IsoEntry and partition information.
    pub fn from_iso_entry(
        entry: &crate::fs::IsoEntry,
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

unsafe impl Sync for FileBackedIsoLocator {}