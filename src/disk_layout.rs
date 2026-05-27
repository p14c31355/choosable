use crate::constants::*;
use std::io::{Read, Seek, SeekFrom, Write};
use crate::error::{ChoosableError, Result};

/// MBR Partition Table Entry (16 bytes)
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct PartitionTableEntry {
    pub active: u8,           // 0x00 or 0x80
    pub start_head: u8,
    pub start_sector_cylinder: u16, // bits 0-5: sector, bits 6-15: cylinder
    pub fs_flag: u8,
    pub end_head: u8,
    pub end_sector_cylinder: u16,
    pub start_lba: u32,
    pub sector_count: u32,
}

impl PartitionTableEntry {
    pub const fn empty() -> Self {
        PartitionTableEntry {
            active: 0,
            start_head: 0,
            start_sector_cylinder: 0,
            fs_flag: 0,
            end_head: 0,
            end_sector_cylinder: 0,
            start_lba: 0,
            sector_count: 0,
        }
    }
}

/// MBR (Master Boot Record) - 512 bytes
#[derive(Debug, Clone)]
#[repr(C, packed)]
pub struct Mbr {
    pub boot_code: [u8; 446],
    pub partitions: [PartitionTableEntry; 4],
    pub signature_55: u8,
    pub signature_aa: u8,
}

impl Mbr {
    /// Read MBR from a reader at current position
    pub fn read<R: Read>(reader: &mut R) -> Result<Self> {
        let mut buf = [0u8; 512];
        reader.read_exact(&mut buf)?;

        // Validate signature
        if buf[510] != MBR_SIGNATURE_55 || buf[511] != MBR_SIGNATURE_AA {
            return Err(ChoosableError::InvalidMbrSignature(buf[510], buf[511]));
        }

        // Read from buffer into struct using unsafe (on-disk layout is packed)
        let mbr: Mbr = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const Mbr) };
        Ok(mbr)
    }

    /// Write MBR to a writer at current position
    pub fn write<W: Write>(&self, writer: &mut W) -> Result<()> {
        let ptr = self as *const Mbr as *const u8;
        let bytes = unsafe { std::slice::from_raw_parts(ptr, 512) };
        writer.write_all(bytes)?;
        Ok(())
    }

    /// Check if MBR has GPT protective partition (part1 fs_flag == 0xEE)
    pub fn is_gpt_protective(&self) -> bool {
        self.partitions[0].fs_flag == PART_TYPE_GPT_PROTECTIVE
    }

    /// Get partition style: 0 = MBR, 1 = GPT
    pub fn partition_style(&self) -> u32 {
        if self.is_gpt_protective() { 1 } else { 0 }
    }

    /// Get the first partition's start LBA
    pub fn part1_start_lba(&self) -> u32 {
        self.partitions[0].start_lba
    }

    /// Get the first partition's sector count
    pub fn part1_sector_count(&self) -> u32 {
        self.partitions[0].sector_count
    }

    /// Get the second partition's start LBA
    pub fn part2_start_lba(&self) -> u32 {
        self.partitions[1].start_lba
    }

    /// Get the second partition's sector count
    pub fn part2_sector_count(&self) -> u32 {
        self.partitions[1].sector_count
    }

    /// Get the first partition's active flag
    pub fn part1_active(&self) -> u8 {
        self.partitions[0].active
    }

    /// Get the second partition's active flag
    pub fn part2_active(&self) -> u8 {
        self.partitions[1].active
    }

    /// Create a new empty MBR with signature
    pub fn new_empty() -> Self {
        Mbr {
            boot_code: [0u8; 446],
            partitions: [PartitionTableEntry::empty(); 4],
            signature_55: MBR_SIGNATURE_55,
            signature_aa: MBR_SIGNATURE_AA,
        }
    }
}

/// GUID structure (16 bytes)
pub type Guid = [u8; 16];

/// GPT Header (92 bytes)
#[derive(Debug, Clone)]
#[repr(C, packed)]
pub struct GptHeader {
    pub signature: [u8; 8],
    pub revision: [u8; 4],
    pub header_size: u32,
    pub header_crc32: u32,
    pub reserved1: [u8; 4],
    pub efi_start_lba: u64,
    pub efi_backup_lba: u64,
    pub part_area_start_lba: u64,
    pub part_area_end_lba: u64,
    pub disk_guid: Guid,
    pub part_table_start_lba: u64,
    pub part_table_num_entries: u32,
    pub part_table_entry_size: u32,
    pub part_table_crc32: u32,
    pub reserved2: [u8; 420],
}

impl GptHeader {
    /// Read GPT header from a reader at current position
    pub fn read<R: Read>(reader: &mut R) -> Result<Self> {
        let mut buf = [0u8; 92];
        reader.read_exact(&mut buf)?;

        let header: GptHeader = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GptHeader) };

        // Validate signature (read via raw pointer to avoid unaligned ref)
        let sig_ptr = &header.signature as *const [u8; 8];
        let sig = unsafe { std::ptr::read_unaligned(sig_ptr) };
        if &sig != GPT_SIGNATURE {
            return Err(ChoosableError::InvalidGptSignature);
        }

        Ok(header)
    }

    /// Write GPT header to a writer at current position
    pub fn write<W: Write>(&self, writer: &mut W) -> Result<()> {
        let ptr = self as *const GptHeader as *const u8;
        let bytes = unsafe { std::slice::from_raw_parts(ptr, 92) };
        writer.write_all(bytes)?;
        Ok(())
    }

    /// Create a new empty GPT header
    pub fn new(disk_size_bytes: u64, disk_guid: Guid) -> Self {
        let total_sectors = disk_size_bytes / SECTOR_SIZE;
        GptHeader {
            signature: *GPT_SIGNATURE,
            revision: [0x00, 0x00, 0x01, 0x00], // GPT revision 1.0
            header_size: 92,
            header_crc32: 0, // Will be computed later
            reserved1: [0u8; 4],
            efi_start_lba: 1,
            efi_backup_lba: total_sectors - 1,
            part_area_start_lba: VENTOY_PART1_START_SECTOR,
            part_area_end_lba: total_sectors - 34, // At least 33 sectors for backup GPT + 1
            disk_guid,
            part_table_start_lba: 2,
            part_table_num_entries: 128,
            part_table_entry_size: 128,
            part_table_crc32: 0, // Will be computed later
            reserved2: [0u8; 420],
        }
    }
}

/// GPT Partition Table Entry (128 bytes)
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct GptPartitionEntry {
    pub part_type_guid: Guid,
    pub unique_part_guid: Guid,
    pub start_lba: u64,
    pub end_lba: u64,
    pub attributes: u64,
    pub name: [u16; 36],
}

impl GptPartitionEntry {
    pub const fn empty() -> Self {
        GptPartitionEntry {
            part_type_guid: [0u8; 16],
            unique_part_guid: [0u8; 16],
            start_lba: 0,
            end_lba: 0,
            attributes: 0,
            name: [0u16; 36],
        }
    }
}

/// Complete GPT structure (MBR + GPT Header + Partition Table)
#[derive(Debug, Clone)]
pub struct GptInfo {
    pub protective_mbr: Mbr,
    pub header: GptHeader,
    pub partitions: [GptPartitionEntry; 128],
}

impl GptInfo {
    /// Read complete GPT from disk
    pub fn read_from_disk<R: Read + Seek>(reader: &mut R) -> Result<Self> {
        // Read protective MBR at LBA 0
        reader.seek(SeekFrom::Start(0))?;
        let protective_mbr = Mbr::read(reader)?;

        // Read GPT header at LBA 1
        reader.seek(SeekFrom::Start(SECTOR_SIZE))?;
        let header = GptHeader::read(reader)?;

        // Read partition table - use ptr::read_unaligned for packed struct
        let part_start = header.part_table_start_lba * SECTOR_SIZE;
        reader.seek(SeekFrom::Start(part_start))?;
        let mut partitions = [GptPartitionEntry::empty(); 128];
        for entry in partitions.iter_mut() {
            let mut buf = [0u8; 128];
            reader.read_exact(&mut buf)?;
            *entry = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GptPartitionEntry) };
        }

        Ok(GptInfo {
            protective_mbr,
            header,
            partitions,
        })
    }

    /// Create new GPT layout for Ventoy installation
    pub fn new_ventoy(disk_size_bytes: u64, disk_guid: Guid) -> Self {
        let mut protective_mbr = Mbr::new_empty();
        // GPT protective partition
        protective_mbr.partitions[0].fs_flag = PART_TYPE_GPT_PROTECTIVE;
        protective_mbr.partitions[0].start_lba = 1;
        protective_mbr.partitions[0].sector_count = 0xFFFFFFFF;

        let header = GptHeader::new(disk_size_bytes, disk_guid);

        let efi_part_size_sectors = VENTOY_EFI_PART_SIZE / SECTOR_SIZE;

        let mut partitions = [GptPartitionEntry::empty(); 128];

        // Partition 1: Ventoy data (exFAT/NTFS)
        let part1_end = (disk_size_bytes / SECTOR_SIZE) - efi_part_size_sectors - 34; // Leave room for backup GPT + EFI part
        partitions[0].part_type_guid = GPT_TYPE_BASIC_DATA;
        partitions[0].unique_part_guid = generate_guid();
        partitions[0].start_lba = VENTOY_PART1_START_SECTOR;
        partitions[0].end_lba = part1_end - 1;
        partitions[0].attributes = 0;
        // Copy GPT_PART1_NAME into the array (use raw ptr arithmetic for packed struct)
        // name field offset in GptPartitionEntry: part_type(16)+unique_part(16)+start_lba(8)+end_lba(8)+attributes(8) = 56
        let name_slice: &[u16] = GPT_PART1_NAME;
        let len = name_slice.len().min(36);
        let entry_ptr = &partitions[0] as *const GptPartitionEntry as *const u8;
        let name_ptr = unsafe { entry_ptr.add(56) as *mut u16 };
        for i in 0..len {
            unsafe { std::ptr::write_unaligned(name_ptr.add(i), name_slice[i]); }
        }

        // Partition 2: VTOYEFI
        partitions[1].part_type_guid = GPT_TYPE_EFI_SYSTEM;
        partitions[1].unique_part_guid = generate_guid();
        partitions[1].start_lba = part1_end;
        partitions[1].end_lba = part1_end + efi_part_size_sectors - 1;
        partitions[1].attributes = GPT_ATTR_VTOYEFI;
        // Name: "VTOYEFI"
        let mut vtoyefi_name = [0u16; 36];
        vtoyefi_name[0] = 'V' as u16;
        vtoyefi_name[1] = 'T' as u16;
        vtoyefi_name[2] = 'O' as u16;
        vtoyefi_name[3] = 'Y' as u16;
        vtoyefi_name[4] = 'E' as u16;
        vtoyefi_name[5] = 'F' as u16;
        vtoyefi_name[6] = 'I' as u16;
        partitions[1].name = vtoyefi_name;

        GptInfo {
            protective_mbr,
            header,
            partitions,
        }
    }

    /// Write complete GPT to disk
    pub fn write_to_disk<W: Write + Seek>(&self, writer: &mut W) -> Result<()> {
        // Write protective MBR
        writer.seek(SeekFrom::Start(0))?;
        self.protective_mbr.write(writer)?;

        // Write GPT header
        writer.seek(SeekFrom::Start(SECTOR_SIZE))?;
        self.header.write(writer)?;

        // Write partition table
        writer.seek(SeekFrom::Start(self.header.part_table_start_lba * SECTOR_SIZE))?;
        for entry in &self.partitions {
            let ptr = entry as *const GptPartitionEntry as *const u8;
            let bytes = unsafe { std::slice::from_raw_parts(ptr, 128) };
            writer.write_all(bytes)?;
        }

        // Write backup GPT header at end of disk
        let backup_offset = self.header.efi_backup_lba * SECTOR_SIZE;
        writer.seek(SeekFrom::Start(backup_offset))?;
        self.header.write(writer)?;

        // Write backup partition table (before backup header)
        let backup_part_offset = (self.header.efi_backup_lba - 32) * SECTOR_SIZE;
        writer.seek(SeekFrom::Start(backup_part_offset))?;
        for entry in &self.partitions {
            let ptr = entry as *const GptPartitionEntry as *const u8;
            let bytes = unsafe { std::slice::from_raw_parts(ptr, 128) };
            writer.write_all(bytes)?;
        }

        Ok(())
    }
}

/// Generate a random GUID
pub fn generate_guid() -> Guid {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = now.as_nanos();
    let mut guid = [0u8; 16];

    // Simple pseudo-random GUID from time
    guid[0] = (nanos >> 0) as u8;
    guid[1] = (nanos >> 8) as u8;
    guid[2] = (nanos >> 16) as u8;
    guid[3] = (nanos >> 24) as u8;
    guid[4] = (nanos >> 32) as u8;
    guid[5] = (nanos >> 40) as u8;
    guid[6] = (nanos >> 48) as u8;
    guid[7] = (nanos >> 56) as u8;
    // Variant: 10xx
    guid[8] = 0x80 | ((nanos >> 4) & 0x3F) as u8;
    guid[9] = (nanos >> 10) as u8;
    // Version: 0100
    guid[10] = 0x40 | ((nanos >> 16) & 0x0F) as u8;
    guid[11] = (nanos >> 22) as u8;
    guid[12] = (nanos >> 28) as u8;
    guid[13] = (nanos >> 34) as u8;
    guid[14] = (nanos >> 40) as u8;
    guid[15] = (nanos >> 46) as u8;

    guid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mbr_new_empty() {
        let mbr = Mbr::new_empty();
        assert_eq!(mbr.signature_55, 0x55);
        assert_eq!(mbr.signature_aa, 0xAA);
    }

    #[test]
    fn test_mbr_partition_style() {
        let mut mbr = Mbr::new_empty();
        assert_eq!(mbr.partition_style(), 0); // MBR

        mbr.partitions[0].fs_flag = 0xEE;
        assert!(mbr.is_gpt_protective());
        assert_eq!(mbr.partition_style(), 1); // GPT
    }

    #[test]
    fn test_gpt_header_creation() {
        let guid = [0u8; 16];
        let header = GptHeader::new(64 * SIZE_1GB, guid);
        assert_eq!(&header.signature, GPT_SIGNATURE);
    }
}