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
    let name = path.strip_prefix("/dev/").unwrap_or(path);
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
    let sys_path = format!("/sys/class/block/{}", name);
    if let Ok(canonical) = std::fs::canonicalize(sys_path) {
        canonical.to_string_lossy().contains("/usb")
    } else {
        false
    }
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

    let block_dir = Path::new("/sys/class/block");
    if !block_dir.exists() {
        return Ok(disks);
    }

    for entry in std::fs::read_dir(block_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with("loop")
            || name_str.starts_with("ram")
            || name_str.starts_with("dm-")
            || name_str.starts_with("sr")
        {
            continue;
        }

        let disk_path = format!("/dev/{}", name_str);

        if !Path::new(&disk_path).exists() {
            continue;
        }

        if !is_whole_disk(&disk_path) {
            continue;
        }

        let size_bytes = match get_disk_size(&disk_path) {
            Ok(sz) => sz,
            Err(_) => continue,
        };
        let model = get_disk_model(&disk_path);
        let removable = is_removable_disk(&disk_path);
        let is_usb = is_usb_disk(&disk_path);
        let sector_size_logical = get_sector_size_logical(&disk_path);
        let sector_size_physical = get_sector_size_physical(&disk_path);

        disks.push(PhyDriveInfo {
            phy_drive: 0,
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

    disks.sort_by(|a, b| {
        b.is_usb.cmp(&a.is_usb)
            .then(b.removable.cmp(&a.removable))
            .then(b.size_bytes.cmp(&a.size_bytes))
    });

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
pub fn detect_choosable(disk_path: &str, _size_bytes: u64) -> Result<(bool, Option<String>, Option<u64>, Option<u64>, Mbr)> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .open(disk_path)?;

    let mbr = match Mbr::read(&mut file) {
        Ok(m) => m,
        Err(ChoosableError::InvalidMbrSignature(_, _)) => {
            return Ok((false, None, None, None, Mbr::new_empty()));
        }
        Err(e) => return Err(e),
    };

    if mbr.is_gpt_protective() {
        let gpt = GptInfo::read_from_disk(&mut file)?;

        if gpt.partitions[0].start_lba != CHOOSABLE_PART1_START_SECTOR {
            return Ok((false, None, None, None, mbr));
        }

        let efi_part_size_sectors = CHOOSABLE_EFI_PART_SIZE / SECTOR_SIZE;

        if gpt.partitions[1].start_lba != gpt.partitions[0].end_lba + 1
            || (gpt.partitions[1].end_lba + 1 - gpt.partitions[1].start_lba) != efi_part_size_sectors
        {
            return Ok((false, None, None, None, mbr));
        }

        let expected = ['C' as u16, 'Z' as u16, 'B' as u16, 'L' as u16, 'E' as u16, 'F' as u16, 'I' as u16];
        let name_arr = gpt.partitions[1].name;
        if name_arr[..7] != expected {
            return Ok((false, None, None, None, mbr));
        }

        let part2_start = gpt.partitions[1].start_lba * SECTOR_SIZE;
        let version = read_choosable_version(disk_path, part2_start)?;

        Ok((true, version, Some(part2_start), Some(gpt.partitions[1].attributes), mbr))
    } else {
        if mbr.partitions[0].start_lba != CHOOSABLE_PART1_START_SECTOR as u32 {
            return Ok((false, None, None, None, mbr));
        }

        let part1_end = mbr.partitions[0].start_lba + mbr.partitions[0].sector_count;
        let efi_part_size_sectors = (CHOOSABLE_EFI_PART_SIZE / SECTOR_SIZE) as u32;

        if mbr.partitions[1].start_lba != part1_end
            || mbr.partitions[1].sector_count != efi_part_size_sectors
        {
            return Ok((false, None, None, None, mbr));
        }

        let part2_start = mbr.partitions[1].start_lba as u64 * SECTOR_SIZE;
        let version = read_choosable_version(disk_path, part2_start)?;

        Ok((true, version, Some(part2_start), None, mbr))
    }
}

/// PartitionSlice: wraps a file to expose a sub-range as a standalone Read + Write + Seek handle.
/// Avoids loading the entire 32 MiB EFI partition into memory.
pub struct PartitionSlice<R> {
    inner: R,
    start_offset: u64,
    size: u64,
    current_pos: u64,
}

impl<R: Read + Seek> PartitionSlice<R> {
    pub fn new(inner: R, start_offset: u64, size: u64) -> Self {
        PartitionSlice { inner, start_offset, size, current_pos: 0 }
    }
}

impl<R: Read + Seek> Read for PartitionSlice<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.current_pos >= self.size {
            return Ok(0);
        }
        // Sync inner file position.  The inner file may have been advanced
        // by other callers (fatfs internal FAT reads, etc.) without going
        // through PartitionSlice::Seek.
        let target = self.start_offset + self.current_pos;
        self.inner.seek(SeekFrom::Start(target))?;

        let max_to_read = (self.size - self.current_pos) as usize;
        let buf_to_read = if buf.len() > max_to_read {
            &mut buf[..max_to_read]
        } else {
            buf
        };
        let bytes_read = self.inner.read(buf_to_read)?;
        self.current_pos += bytes_read as u64;
        Ok(bytes_read)
    }
}

impl<R: Write + Seek> Write for PartitionSlice<R> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.current_pos >= self.size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "write exceeds partition size",
            ));
        }
        // Ensure the inner file is at the correct absolute position.
        // Reads may have advanced it beyond what current_pos tracks
        // (e.g. fatfs reads the FAT into internal buffers, advancing the file position).
        let target = self.start_offset + self.current_pos;
        self.inner.seek(SeekFrom::Start(target))?;

        let max_to_write = (self.size - self.current_pos) as usize;
        let buf_to_write = if buf.len() > max_to_write { &buf[..max_to_write] } else { buf };
        let bytes_written = self.inner.write(buf_to_write)?;
        self.current_pos += bytes_written as u64;
        Ok(bytes_written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<R: Read + Seek> Seek for PartitionSlice<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(offset) => self.start_offset + offset,
            SeekFrom::Current(offset) => (self.start_offset + self.current_pos)
                .checked_add_signed(offset)
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid seek"))?,
            SeekFrom::End(offset) => (self.start_offset + self.size)
                .checked_add_signed(offset)
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid seek"))?,
        };
        let actual = self.inner.seek(SeekFrom::Start(target))?;
        self.current_pos = actual.saturating_sub(self.start_offset);
        Ok(self.current_pos)
    }
}

/// Read Choosable version string from EFI partition (FAT12/16/32) using fatfs
fn read_choosable_version(disk_path: &str, part2_start_byte: u64) -> Result<Option<String>> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .open(disk_path)?;

    file.seek(SeekFrom::Start(part2_start_byte))?;
    let slice = PartitionSlice::new(file, part2_start_byte, CHOOSABLE_EFI_PART_SIZE);

    let fs = match fatfs::FileSystem::new(slice, fatfs::FsOptions::new()) {
        Ok(fs) => fs,
        Err(_) => return Ok(None),
    };

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
        if let Some(pos) = line.find("CHOOSABLE_VERSION=") {
            let after = &line[pos + "CHOOSABLE_VERSION=".len()..];
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

    let zeros = [0u8; SECTOR_SIZE as usize];
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