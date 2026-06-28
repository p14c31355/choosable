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

use crate::fs::{PayloadEntry, PayloadType};
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
    /// true if the disk uses MBR partitioning (affects PARTUUID format)
    pub is_mbr: bool,
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
    pub fn path(&self) -> &[u8] { &self.file_path[..self.file_path_len] }
    pub fn offset_bytes(&self) -> u64 { (self.iso_lba - self.part1_lba) * 512 }
    pub fn file_name(&self) -> &[u8] {
        let path = self.path();
        if let Some(pos) = path.iter().rposition(|&c| c == b'/') { &path[pos + 1..] } else { path }
    }
    pub fn path_without_leading_slash(&self) -> &[u8] {
        let p = self.path();
        if p.first() == Some(&b'/') { &p[1..] } else { p }
    }

    fn from_payload_entry(entry: &PayloadEntry, partition_guid: Guid, partition_number: u32, part1_lba: u64, is_mbr: bool) -> Self {
        let mut file_path = [0u8; 256];
        let name_len = entry.name_len.min(256);
        file_path[..name_len].copy_from_slice(&entry.name[..name_len]);
        IsoLocation { partition_guid, partition_number, is_mbr, file_path, file_path_len: name_len,
            file_size: entry.file_size, part1_lba, iso_lba: entry.file_start_lba }
    }
}

/// Trait for resolving the physical location of a boot payload.
pub trait IsoLocator: Sync { fn locate(&self) -> IsoLocation; }

pub trait BootPayloadLocator: Sync {
    type Location;
    fn locate(&self) -> Self::Location;
    fn payload_type(&self) -> &'static str;
}

// ── Macro-generated simple payload locators ──────────────────────────────
macro_rules! payload_locator_simple {
    ($name:ident, $type_str:expr) => {
        pub struct $name { pub location: IsoLocation }
        impl $name {
            pub fn from_payload_entry(entry: &PayloadEntry, partition_guid: Guid, partition_number: u32, part1_lba: u64, is_mbr: bool) -> Self {
                $name { location: IsoLocation::from_payload_entry(entry, partition_guid, partition_number, part1_lba, is_mbr) }
            }
        }
        impl $crate::locator::BootPayloadLocator for $name {
            type Location = $crate::locator::IsoLocation;
            fn locate(&self) -> $crate::locator::IsoLocation { self.location }
            fn payload_type(&self) -> &'static str { $type_str }
        }
    };
}

payload_locator_simple!(FileBackedIsoLocator, "ISO");
payload_locator_simple!(WimPayloadLocator, "WIM");
payload_locator_simple!(EfiPayloadLocator, "EFI");

pub struct VhdPayloadLocator { pub location: IsoLocation, pub is_vhdx: bool }
impl VhdPayloadLocator {
    pub fn from_payload_entry(entry: &PayloadEntry, partition_guid: Guid, partition_number: u32, part1_lba: u64, is_mbr: bool) -> Self {
        let is_vhdx = matches!(entry.payload_type, PayloadType::Vhdx);
        VhdPayloadLocator { location: IsoLocation::from_payload_entry(entry, partition_guid, partition_number, part1_lba, is_mbr), is_vhdx }
    }
}
impl BootPayloadLocator for VhdPayloadLocator {
    type Location = IsoLocation;
    fn locate(&self) -> IsoLocation { self.location }
    fn payload_type(&self) -> &'static str { if self.is_vhdx { "VHDX" } else { "VHD" } }
}

impl IsoLocator for FileBackedIsoLocator { fn locate(&self) -> IsoLocation { self.location } }

// ═══════════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(name: &[u8], lba: u64, size: u64) -> PayloadEntry {
        let name_len = name.len().min(256);
        let mut n = [0u8; 256];
        n[..name_len].copy_from_slice(&name[..name_len]);
        PayloadEntry { name: n, name_len, file_start_lba: lba, file_size: size, payload_type: PayloadType::Iso }
    }

    #[test]
    fn iso_location_offset_bytes() {
        let e = make_entry(b"/test.iso", 100, 1024);
        let loc = IsoLocation::from_payload_entry(&e, Guid { d1: 0, d2: 0, d3: 0, d4: [0; 8] }, 1, 50, false);
        assert_eq!(loc.offset_bytes(), (100 - 50) * 512);
        assert_eq!(loc.file_name(), b"test.iso");
        assert_eq!(loc.path_without_leading_slash(), b"test.iso");
    }

    #[test]
    fn iso_location_slash_handling() {
        let e = make_entry(b"/boot/ubuntu.iso", 0, 0);
        let loc = IsoLocation::from_payload_entry(&e, Guid { d1: 0, d2: 0, d3: 0, d4: [0; 8] }, 1, 0, false);
        assert_eq!(loc.file_name(), b"ubuntu.iso");
        assert_eq!(loc.path_without_leading_slash(), b"boot/ubuntu.iso");
    }

    #[test]
    fn iso_location_no_slash() {
        let e = make_entry(b"ubuntu.iso", 0, 0);
        let loc = IsoLocation::from_payload_entry(&e, Guid { d1: 0, d2: 0, d3: 0, d4: [0; 8] }, 1, 0, false);
        assert_eq!(loc.file_name(), b"ubuntu.iso");
        assert_eq!(loc.path_without_leading_slash(), b"ubuntu.iso");
    }

    #[test]
    fn payload_locators_create_correctly() {
        let e = make_entry(b"/test.iso", 0, 0);
        let guid = Guid { d1: 1, d2: 2, d3: 3, d4: [4, 5, 6, 7, 8, 9, 10, 11] };
        let f = FileBackedIsoLocator::from_payload_entry(&e, guid, 1, 0, false);
        assert_eq!(f.payload_type(), "ISO");
        assert_eq!(f.location.partition_number, 1);

        let w = WimPayloadLocator::from_payload_entry(&e, guid, 2, 0, false);
        assert_eq!(w.payload_type(), "WIM");
        assert_eq!(w.location.partition_number, 2);

        let ef = EfiPayloadLocator::from_payload_entry(&e, guid, 3, 0, false);
        assert_eq!(ef.payload_type(), "EFI");
        assert_eq!(ef.location.partition_number, 3);

        let vhd_entry = {
            let mut n = [0u8; 256];
            n[..9].copy_from_slice(b"/test.vhd");
            PayloadEntry { name: n, name_len: 9, file_start_lba: 0, file_size: 0, payload_type: PayloadType::Vhd }
        };
        let v = VhdPayloadLocator::from_payload_entry(&vhd_entry, guid, 4, 0, false);
        assert_eq!(v.is_vhdx, false);
        assert_eq!(v.payload_type(), "VHD");
    }
}