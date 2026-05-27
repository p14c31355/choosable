use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use crate::constants::*;
use crate::disk_layout::{GptPartitionEntry, Mbr, GptInfo};
use crate::error::{ChoosableError, Result};

/// Physical drive information
#[derive(Debug, Clone)]
pub struct PhyDriveInfo {
    pub phy_drive: usize,
    pub disk_path: String,
    pub size_bytes: u64,
    pub model: String,
    pub removable: bool,
    pub is_usb: bool,
    pub sector_size_logical: u32,
    pub sector_size_physical: u32,
    pub partition_style: Option<u32>, // 0=MBR, 1=GPT
    pub choosable_version: Option<String>,
    pub secure_boot: Option<bool>,
    pub mbr: Option<Mbr>,
    pub part2_start_sector: Option<u64>,
    pub part2_gpt_attr: Option<u64>,
}

/// Check if a disk is a whole disk (not a partition)
pub fn is_whole_disk(path: &str) -> bool {
    // On Linux: check if /sys/class/block/{name}/start exists
    // If it exists, it's a partition; otherwise it's a whole disk
    let name = path.trim_start_matches("/dev/");
    let start_file = format!("/sys/class/block/{}/start", name);
    !Path::new(&start_file).exists()
}

/// Get disk size from /sys/class/block/{name}/size
pub fn get_disk_size(path: &str) -> Result<u64> {
    let name = path.trim_start_matches("/dev/");
    let size_path = format!("/sys/class/block/{}/size", name);
    let size_str = std::fs::read_to_string(&size_path)
        .map_err(|_| ChoosableError::DiskNotFound(
            format!("Cannot read size for disk {}", path)
        ))?;
    let sectors: u64 = size_str.trim().parse()
        .map_err(|_| ChoosableError::Generic(format!("Invalid size for disk {}", path)))?;
    Ok(sectors * SECTOR_SIZE)
}

/// Get disk model string
pub fn get_disk_model(path: &str) -> String {
    let name = path.trim_start_matches("/dev/");
    let model_path = format!("/sys/class/block/{}/device/model", name);
    std::fs::read_to_string(&model_path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| String::from("Unknown"))
}

/// Check if disk is USB-based
pub fn is_usb_disk(path: &str) -> bool {
    let name = path.trim_start_matches("/dev/");
    // Check if the device's transport is USB
    let transport_path = format!("/sys/class/block/{}/device/../transport", name);
    if let Ok(link) = std::fs::read_link(&transport_path) {
        if let Some(transport) = link.file_name() {
            return transport == "usb";
        }
    }
    // Alternative: check subsystem
    let subsystem_path = format!("/sys/class/block/{}/device/../subsystem", name);
    if let Ok(link) = std::fs::read_link(&subsystem_path) {
        if let Some(subsystem) = link.file_name() {
            return subsystem == "usb";
        }
    }
    false
}

/// Check if disk is removable
pub fn is_removable_disk(path: &str) -> bool {
    let name = path.trim_start_matches("/dev/");
    let removable_path = format!("/sys/class/block/{}/removable", name);
    std::fs::read_to_string(&removable_path)
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

/// Get logical sector size
pub fn get_sector_size_logical(path: &str) -> u32 {
    let name = path.trim_start_matches("/dev/");
    let path = format!("/sys/class/block/{}/queue/logical_block_size", name);
    std::fs::read_to_string(&path)
        .map(|s| s.trim().parse().unwrap_or(512))
        .unwrap_or(512)
}

/// Get physical sector size
pub fn get_sector_size_physical(path: &str) -> u32 {
    let name = path.trim_start_matches("/dev/");
    let path = format!("/sys/class/block/{}/queue/physical_block_size", name);
    std::fs::read_to_string(&path)
        .map(|s| s.trim().parse().unwrap_or(512))
        .unwrap_or(512)
}

/// Enumerate all block devices in the system
pub fn enumerate_disks() -> Result<Vec<PhyDriveInfo>> {
    let mut disks = Vec::new();

    // Read /sys/class/block for block devices
    let block_dir = Path::new("/sys/class/block");
    if !block_dir.exists() {
        return Ok(disks);
    }

    for entry in std::fs::read_dir(block_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip loop, ram, dm, and other virtual devices
        if name_str.starts_with("loop")
            || name_str.starts_with("ram")
            || name_str.starts_with("dm-")
            || name_str.starts_with("sr")
        {
            continue;
        }

        let disk_path = format!("/dev/{}", name_str);

        // Skip if not a block device
        if !Path::new(&disk_path).exists() {
            continue;
        }

        // Skip if it's a partition
        if !is_whole_disk(&disk_path) {
            continue;
        }

        let size_bytes = get_disk_size(&disk_path)?;
        let model = get_disk_model(&disk_path);
        let removable = is_removable_disk(&disk_path);
        let is_usb = is_usb_disk(&disk_path);
        let sector_size_logical = get_sector_size_logical(&disk_path);
        let sector_size_physical = get_sector_size_physical(&disk_path);

        disks.push(PhyDriveInfo {
            phy_drive: 0, // Will be assigned later
            disk_path,
            size_bytes,
            model,
            removable,
            is_usb,
            sector_size_logical,
            sector_size_physical,
            partition_style: None,
            choosable_version: None,
            secure_boot: None,
            mbr: None,
            part2_start_sector: None,
            part2_gpt_attr: None,
        });
    }

    // Sort: USB/removable first, then by size (largest first)
    disks.sort_by(|a, b| {
        b.is_usb.cmp(&a.is_usb)
            .then(b.removable.cmp(&a.removable))
            .then(b.size_bytes.cmp(&a.size_bytes))
    });

    // Assign phy_drive IDs
    for (i, disk) in disks.iter_mut().enumerate() {
        disk.phy_drive = i;
    }

    Ok(disks)
}

/// Read the MBR from a disk
pub fn read_mbr(disk_path: &str) -> Result<Mbr> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .open(disk_path)?;

    Mbr::read(&mut file)
}

/// Read the MBR and detect if it's a Choosable disk
pub fn detect_choosable(disk_path: &str, size_bytes: u64) -> Result<(bool, Option<String>, Option<u64>, Option<u64>, Mbr)> {
    // Open disk
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .open(disk_path)?;

    let mbr = Mbr::read(&mut file)?;

    // Check partition layout for Choosable signature
    if mbr.is_gpt_protective() {
        // GPT disk - check partition table
        let gpt = GptInfo::read_from_disk(&mut file)?;

        // Check partition 1 starts at 2048
        if gpt.partitions[0].start_lba != CHOOSABLE_PART1_START_SECTOR {
            return Ok((false, None, None, None, mbr));
        }

        let efi_part_size_sectors = CHOOSABLE_EFI_PART_SIZE / SECTOR_SIZE;

        // Check partition 2 is right after partition 1 and has correct size
        if gpt.partitions[1].start_lba != gpt.partitions[0].end_lba + 1
            || (gpt.partitions[1].end_lba + 1 - gpt.partitions[1].start_lba) != efi_part_size_sectors
        {
            return Ok((false, None, None, None, mbr));
        }

        // Check partition 2 name is "VTOYEFI" (use ptr arithmetic for packed struct)
        // name field is at offset 56 in GptPartitionEntry (16+16+8+8+8 = 56)
        let entry_ptr = &gpt.partitions[1] as *const GptPartitionEntry as *const u8;
        let name_offset = 56usize; // offset_of(GptPartitionEntry, name)
        let name_ptr = unsafe { entry_ptr.add(name_offset) as *const u16 };
        let mut name_arr = [0u16; 36];
        for i in 0..36 {
            name_arr[i] = unsafe { std::ptr::read_unaligned(name_ptr.add(i)) };
        }
        let expected: Vec<u16> = vec!['V' as u16, 'T' as u16, 'O' as u16, 'Y' as u16, 'E' as u16, 'F' as u16, 'I' as u16];
        if name_arr[..7] != expected[..] {
            return Ok((false, None, None, None, mbr));
        }

        // Read choosable version from partition 2
        let part2_start = gpt.partitions[1].start_lba * SECTOR_SIZE;
        let version = read_choosable_version(&disk_path, part2_start)?;

        Ok((true, version, Some(part2_start), Some(gpt.partitions[1].attributes), mbr))
    } else {
        // MBR disk

        // Check partition 1 starts at 2048
        if mbr.partitions[0].start_lba != CHOOSABLE_PART1_START_SECTOR as u32 {
            return Ok((false, None, None, None, mbr));
        }

        let part1_end = mbr.partitions[0].start_lba + mbr.partitions[0].sector_count;
        let efi_part_size_sectors = (CHOOSABLE_EFI_PART_SIZE / SECTOR_SIZE) as u32;

        // Check partition 2 starts right after partition 1 and has correct size
        if mbr.partitions[1].start_lba != part1_end
            || mbr.partitions[1].sector_count != efi_part_size_sectors
        {
            return Ok((false, None, None, None, mbr));
        }

        // Check partition 2 is type EF (EFI System) - not strictly required for MBR Choosable
        // (Choosable uses 0xEF for the EFI partition in MBR mode)

        // Read choosable version from partition 2
        let part2_start = mbr.partitions[1].start_lba as u64 * SECTOR_SIZE;
        let version = read_choosable_version(&disk_path, part2_start)?;

        Ok((true, version, Some(mbr.partitions[1].start_lba as u64), None, mbr))
    }
}

/// Read Choosable version string from EFI partition (FAT12/16/32) using fatfs
fn read_choosable_version(disk_path: &str, part2_start_byte: u64) -> Result<Option<String>> {
    use std::io::Cursor;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .open(disk_path)?;

    let partition_size = CHOOSABLE_EFI_PART_SIZE;
    let mut buf = vec![0u8; partition_size as usize];
    file.seek(SeekFrom::Start(part2_start_byte))?;
    file.read_exact(&mut buf)?;

    let cursor = Cursor::new(buf);

    let fs = fatfs::FileSystem::new(cursor, fatfs::FsOptions::new())
        .map_err(|e| ChoosableError::Generic(format!("Failed to parse FAT: {}", e)))?;

    let root_dir = fs.root_dir();
    let grub_dir = match root_dir.open_dir("grub") {
        Ok(d) => d,
        Err(_) => return Ok(None),
    };

    let mut cfg_file = match grub_dir.open_file("grub.cfg") {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };

    let mut content = String::new();
    cfg_file.read_to_string(&mut content).ok();

    for line in content.lines() {
        if let Some(pos) = line.find("VENTOY_VERSION=") {
            let after = &line[pos + "VENTOY_VERSION=".len()..];
            let version = after.trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace());
            if !version.is_empty() {
                return Ok(Some(version.to_string()));
            }
        }
    }

    Ok(None)
}

/// Open disk for reading and writing
pub fn open_disk_readwrite(disk_path: &str) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(disk_path)
        .map_err(|e| ChoosableError::DiskNotFound(
            format!("Cannot open {} for read/write: {}", disk_path, e)
        ))
}

/// Open disk for reading only
pub fn open_disk_readonly(disk_path: &str) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .open(disk_path)
        .map_err(|e| ChoosableError::DiskNotFound(
            format!("Cannot open {} for reading: {}", disk_path, e)
        ))
}

/// Write zeros to a range of sectors on a disk
pub fn write_zeros<W: Write + Seek>(
    writer: &mut W,
    start_sector: u64,
    num_sectors: u64,
) -> Result<()> {
    let start_offset = start_sector * SECTOR_SIZE;
    let total_bytes = num_sectors * SECTOR_SIZE;

    writer.seek(SeekFrom::Start(start_offset))?;

    let zeros = vec![0u8; SECTOR_SIZE as usize];
    let mut remaining = total_bytes;
    while remaining > 0 {
        let write_size = std::cmp::min(remaining, SECTOR_SIZE) as usize;
        writer.write_all(&zeros[..write_size])?;
        remaining -= write_size as u64;
    }

    Ok(())
}

/// Read data from disk at specified sector offset
pub fn read_sectors<R: Read + Seek>(
    reader: &mut R,
    start_sector: u64,
    num_sectors: usize,
) -> Result<Vec<u8>> {
    let start_offset = start_sector * SECTOR_SIZE;
    let total_bytes = num_sectors * SECTOR_SIZE as usize;

    reader.seek(SeekFrom::Start(start_offset))?;

    let mut buf = vec![0u8; total_bytes];
    reader.read_exact(&mut buf)?;

    Ok(buf)
}

/// Write data to disk at specified sector offset
pub fn write_sectors<W: Write + Seek>(
    writer: &mut W,
    start_sector: u64,
    data: &[u8],
) -> Result<()> {
    let start_offset = start_sector * SECTOR_SIZE;
    writer.seek(SeekFrom::Start(start_offset))?;
    writer.write_all(data)?;
    Ok(())
}

/// Get partition device name for a given disk and partition number
pub fn get_partition_name(disk_path: &str, part_num: u32) -> String {
    let base = disk_path.trim_start_matches("/dev/");

    // For NVMe devices: /dev/nvme0n1p1
    // For SCSI/SATA: /dev/sda1
    // For mmc: /dev/mmcblk0p1
    if base.starts_with("nvme") || base.starts_with("mmcblk") {
        format!("{}p{}", disk_path, part_num)
    } else {
        format!("{}{}", disk_path, part_num)
    }
}

/// Get partition start sector from sysfs
pub fn get_partition_start_sector(part_path: &str) -> Result<u64> {
    let name = part_path.trim_start_matches("/dev/");
    let start_path = format!("/sys/class/block/{}/start", name);
    let start_str = std::fs::read_to_string(&start_path)
        .map_err(|_| ChoosableError::Generic(
            format!("Cannot read start sector for partition {}", part_path)
        ))?;
    start_str.trim().parse::<u64>()
        .map_err(|_| ChoosableError::Generic(format!("Invalid start sector for {}", part_path)))
}

/// Get partition size in sectors from sysfs
pub fn get_partition_size_sectors(part_path: &str) -> Result<u64> {
    let name = part_path.trim_start_matches("/dev/");
    let size_path = format!("/sys/class/block/{}/size", name);
    let size_str = std::fs::read_to_string(&size_path)
        .map_err(|_| ChoosableError::Generic(
            format!("Cannot read size for partition {}", part_path)
        ))?;
    size_str.trim().parse::<u64>()
        .map_err(|_| ChoosableError::Generic(format!("Invalid size for {}", part_path)))
}

/// Check if 4K native sector (both logical and physical sector size = 4096)
pub fn is_4k_native(disk_path: &str) -> bool {
    get_sector_size_logical(disk_path) == 4096 && get_sector_size_physical(disk_path) == 4096
}

/// Get human-readable size in GB
pub fn human_readable_gb(size_bytes: u64) -> u64 {
    size_bytes / SIZE_1GB
}

/// Read a file from disk (in the current working directory or installation directory)
pub fn read_install_file(path: &str) -> Result<Vec<u8>> {
    std::fs::read(path)
        .map_err(|e| ChoosableError::Generic(format!("Cannot read {}: {}", path, e)))
}