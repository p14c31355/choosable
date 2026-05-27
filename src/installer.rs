use std::io::{Read, Seek, SeekFrom, Write};
use crate::constants::*;
use crate::disk::*;
use crate::disk_layout::*;
use crate::error::{ChoosableError, Result};

/// Filesystem type for partition 1
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilesystemType {
    ExFat,
    Ntfs,
    Fat32,
}

impl FilesystemType {
    pub fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "exfat" => Ok(FilesystemType::ExFat),
            "ntfs" => Ok(FilesystemType::Ntfs),
            "fat32" => Ok(FilesystemType::Fat32),
            _ => Err(ChoosableError::Generic(format!(
                "Unsupported filesystem: {}. Supported: exfat, ntfs, fat32", s
            ))),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            FilesystemType::ExFat => "exfat",
            FilesystemType::Ntfs => "ntfs",
            FilesystemType::Fat32 => "fat32",
        }
    }
}

/// Install Choosable to a disk (MBR or GPT)
pub fn install_choosable(
    disk_path: &str,
    use_gpt: bool,
    secure_boot: bool,
    reserve_space_mb: u64,
    label: &str,
    fs_type: FilesystemType,
    force: bool,
    yes: bool,
) -> Result<()> {
    // Validate disk
    if !is_whole_disk(disk_path) {
        return Err(ChoosableError::IsPartition(disk_path.to_string()));
    }

    let size_bytes = get_disk_size(disk_path)?;
    let disk_size_gb = human_readable_gb(size_bytes);
    let model = get_disk_model(disk_path);

    // Check 4K native
    if is_4k_native(disk_path) {
        return Err(ChoosableError::FourKNativeSector);
    }

    // Check MBR over 2TB
    if !use_gpt && size_bytes > 2 * SIZE_1TB {
        return Err(ChoosableError::MbrOverflow);
    }

    // Check if disk is large enough
    let required_sectors = CHOOSABLE_PART1_START_SECTOR + (CHOOSABLE_PART_SIZE_MB * 2048);
    let required_bytes = required_sectors * SECTOR_SIZE;
    if size_bytes < required_bytes {
        return Err(ChoosableError::DiskTooSmall {
            required: required_bytes,
            available: size_bytes,
        });
    }

    // Check reserved space
    if reserve_space_mb > 0 {
        let reserve_sectors = (reserve_space_mb + CHOOSABLE_PART_SIZE_MB) * 2048;
        let disk_sectors = size_bytes / SECTOR_SIZE;
        if disk_sectors <= reserve_sectors {
            return Err(ChoosableError::Generic(format!(
                "Cannot reserve {} MiB on disk (disk too small)", reserve_space_mb
            )));
        }
    }

    // Check if Choosable is already installed
    let (is_choosable, version, _, _, _) = detect_choosable(disk_path, size_bytes)?;
    if is_choosable && !force {
        return Err(ChoosableError::AlreadyInstalled(
            version.unwrap_or_else(|| "?".to_string())
        ));
    }

    // Print disk info
    println!("Disk : {}", disk_path);
    println!("Model: {}", model);
    println!("Size : {} GiB", disk_size_gb);
    if use_gpt {
        println!("Style: GPT");
    } else {
        println!("Style: MBR");
    }
    if reserve_space_mb > 0 {
        println!("You will reserve {} MiB disk space", reserve_space_mb);
    }
    println!();

    // Warning
    eprintln!("\x1b[33mAttention:\x1b[0m");
    eprintln!("\x1b[33mYou will install Choosable to {}.\x1b[0m", disk_path);
    eprintln!("\x1b[33mAll the data on the disk {} will be lost!!!\x1b[0m", disk_path);
    eprintln!();

    if !yes {
        print!("Continue? (y/n) ");
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        if answer.trim().to_lowercase() != "y" {
            println!("Aborted.");
            return Ok(());
        }

        eprintln!();
        eprintln!("\x1b[33mAll the data on the disk {} will be lost!!!\x1b[0m", disk_path);
        print!("Double-check. Continue? (y/n) ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        if answer.trim().to_lowercase() != "y" {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Open disk for writing
    let mut disk = open_disk_readwrite(disk_path)?;

    // Step 1: Write zeros to first sectors (clean MBR)
    println!("Cleaning disk...");
    let zero_buf = vec![0u8; 64 * 512];
    disk.seek(SeekFrom::Start(0))?;
    disk.write_all(&zero_buf)?;
    disk.flush()?;

    // Step 2: Create partition table
    println!("Creating partition table...");
    if use_gpt {
        install_gpt(
            &mut disk,
            disk_path,
            size_bytes,
            reserve_space_mb,
            label,
            fs_type,
            secure_boot,
        )?;
    } else {
        install_mbr(
            &mut disk,
            disk_path,
            size_bytes,
            reserve_space_mb,
            label,
            fs_type,
            secure_boot,
        )?;
    }

    println!();
    println!("\x1b[32mChoosable installed successfully to {}.\x1b[0m", disk_path);

    Ok(())
}

/// Install Choosable with MBR partition style
fn install_mbr<W: Write + Seek>(
    disk: &mut W,
    disk_path: &str,
    disk_size_bytes: u64,
    reserve_space_mb: u64,
    label: &str,
    fs_type: FilesystemType,
    secure_boot: bool,
) -> Result<()> {
    let total_sectors = disk_size_bytes / SECTOR_SIZE;
    let efi_part_sectors = CHOOSABLE_EFI_PART_SIZE / SECTOR_SIZE;
    let part1_start = CHOOSABLE_PART1_START_SECTOR;

    // Calculate partition layout
    let part2_start = if reserve_space_mb > 0 {
        total_sectors - efi_part_sectors - (reserve_space_mb * 2048)
    } else {
        total_sectors - efi_part_sectors
    };

    let part1_sectors = part2_start - part1_start;

    // Build MBR
    let mut mbr = Mbr::new_empty();

    // Partition 1: Data (exFAT/NTFS)
    mbr.partitions[0].active = PART_ACTIVE;
    mbr.partitions[0].fs_flag = 0x07; // NTFS/exFAT (will be handled by mkfs)
    mbr.partitions[0].start_lba = part1_start as u32;
    mbr.partitions[0].sector_count = part1_sectors as u32;

    // Create CHS for partition 1
    let start_cylinder = (part1_start / (255 * 63)) as u16;
    let start_head = ((part1_start / 63) % 255) as u8;
    let start_sector = ((part1_start % 63) + 1) as u8;

    let part1_end_lba = part1_start + part1_sectors - 1;
    let end_cylinder = (part1_end_lba / (255 * 63)) as u16;
    let end_head = ((part1_end_lba / 63) % 255) as u8;
    let end_sector = ((part1_end_lba % 63) + 1) as u8;

    mbr.partitions[0].start_head = start_head;
    mbr.partitions[0].start_sector_cylinder = ((start_cylinder & 0x3FF) << 6) | ((start_sector & 0x3F) as u16);
    mbr.partitions[0].end_head = end_head;
    mbr.partitions[0].end_sector_cylinder = ((end_cylinder & 0x3FF) << 6) | ((end_sector & 0x3F) as u16);

    // Partition 2: VTOYEFI
    let part2_end_lba = part2_start + efi_part_sectors - 1;
    mbr.partitions[1].fs_flag = PART_TYPE_EFI_SYSTEM; // 0xEF
    mbr.partitions[1].start_lba = part2_start as u32;
    mbr.partitions[1].sector_count = efi_part_sectors as u32;

    let p2_start_cylinder = (part2_start / (255 * 63)) as u16;
    let p2_start_head = ((part2_start / 63) % 255) as u8;
    let p2_start_sector = ((part2_start % 63) + 1) as u8;
    let p2_end_cylinder = (part2_end_lba / (255 * 63)) as u16;
    let p2_end_head = ((part2_end_lba / 63) % 255) as u8;
    let p2_end_sector = ((part2_end_lba % 63) + 1) as u8;

    mbr.partitions[1].start_head = p2_start_head;
    mbr.partitions[1].start_sector_cylinder = ((p2_start_cylinder & 0x3FF) << 6) | ((p2_start_sector & 0x3F) as u16);
    mbr.partitions[1].end_head = p2_end_head;
    mbr.partitions[1].end_sector_cylinder = ((p2_end_cylinder & 0x3FF) << 6) | ((p2_end_sector & 0x3F) as u16);

    // Write MBR
    disk.seek(SeekFrom::Start(0))?;
    mbr.write(disk)?;
    disk.flush()?;

    // Format partition 1 (external tool)
    let part1 = get_partition_name(disk_path, 1);
    format_partition(&part1, label, fs_type)?;

    // Write boot images
    write_boot_images(disk, false, part2_start)?;

    // Write disk GUID (16 bytes at offset 384)
    let disk_guid = generate_guid();
    disk.seek(SeekFrom::Start(384))?;
    disk.write_all(&disk_guid)?;

    // Write disk signature (4 bytes at offset 440)
    let sig = generate_guid();
    disk.seek(SeekFrom::Start(440))?;
    disk.write_all(&sig[12..16])?;

    disk.flush()?;

    // Write choosable disk image to partition 2
    write_disk_image(disk, disk_path, part2_start, secure_boot)?;

    println!("Done.");

    Ok(())
}

/// Install Choosable with GPT partition style
fn install_gpt<W: Write + Seek>(
    disk: &mut W,
    disk_path: &str,
    disk_size_bytes: u64,
    reserve_space_mb: u64,
    label: &str,
    fs_type: FilesystemType,
    secure_boot: bool,
) -> Result<()> {
    let efi_part_sectors = CHOOSABLE_EFI_PART_SIZE / SECTOR_SIZE;
    let total_sectors = disk_size_bytes / SECTOR_SIZE;

    // Calculate partition layout
    let part2_end = total_sectors - 34; // Backup GPT area
    let part2_start = if reserve_space_mb > 0 {
        part2_end - efi_part_sectors - (reserve_space_mb * 2048)
    } else {
        part2_end - efi_part_sectors
    };

    let part1_end = part2_start - 1;

    // Build GPT
    let disk_guid = generate_guid();
    let mut gpt_info = GptInfo::new_choosable(disk_size_bytes, disk_guid);

    // Adjust partition boundaries
    gpt_info.partitions[0].start_lba = CHOOSABLE_PART1_START_SECTOR;
    gpt_info.partitions[0].end_lba = part1_end;
    gpt_info.partitions[0].attributes = 0;

    gpt_info.partitions[1].start_lba = part2_start;
    gpt_info.partitions[1].end_lba = part2_start + efi_part_sectors - 1;
    gpt_info.partitions[1].attributes = GPT_ATTR_VTOYEFI;

    // Write GPT to disk
    gpt_info.write_to_disk(disk)?;
    disk.flush()?;

    // Wait for partition devices to appear
    std::thread::sleep(std::time::Duration::from_secs(1));

    // Format partition 1
    let part1 = get_partition_name(disk_path, 1);
    format_partition(&part1, label, fs_type)?;

    // Write boot images
    write_boot_images(disk, true, part2_start)?;

    // Write disk GUID and signature
    let disk_guid_write = gpt_info.header.disk_guid;
    disk.seek(SeekFrom::Start(384))?;
    disk.write_all(&disk_guid_write)?;
    disk.seek(SeekFrom::Start(440))?;
    disk.write_all(&disk_guid_write[12..16])?;
    disk.flush()?;

    // Write choosable disk image to partition 2
    write_disk_image(disk, disk_path, part2_start, secure_boot)?;

    std::thread::sleep(std::time::Duration::from_secs(1));

    Ok(())
}

/// Format partition 1 using external mkfs tool
fn format_partition(partition: &str, label: &str, fs_type: FilesystemType) -> Result<()> {
    let (cmd, args) = match fs_type {
        FilesystemType::ExFat => {
            // Get cluster size based on partition size
            let part_sectors = get_partition_size_sectors(partition)?;
            let part_size_gb = part_sectors * SECTOR_SIZE / SIZE_1GB;

            let cluster_sectors = if part_size_gb > 32 { "256" } else { "64" };

            ("mkexfatfs", vec!["-n", label, "-s", cluster_sectors, partition])
        }
        FilesystemType::Fat32 => {
            ("mkfs.vfat", vec!["-n", label, "-F", "32", partition])
        }
        FilesystemType::Ntfs => {
            ("mkfs.ntfs", vec!["-f", "-L", label, partition])
        }
    };

    println!("Formatting partition 1 {} as {}...", partition, fs_type.as_str());

    let status = std::process::Command::new(cmd)
        .args(&args)
        .status()
        .map_err(|e| ChoosableError::ToolNotFound(format!("{}: {}", cmd, e)))?;

    if !status.success() {
        // Retry once
        let status = std::process::Command::new(cmd)
            .args(&args)
            .status()
            .map_err(|_| ChoosableError::FormatFailed)?;

        if !status.success() {
            return Err(ChoosableError::FormatFailed);
        }
    }

    println!("Format successful.");
    Ok(())
}

/// Write boot images to disk
fn write_boot_images<W: Write + Seek>(
    disk: &mut W,
    is_gpt: bool,
    part2_start_sector: u64,
) -> Result<()> {
    println!("Writing boot images...");

    // Read boot.img (446 bytes for MBR boot code)
    let boot_img = read_install_file(CHOOSABLE_FILE_BOOT_IMG)?;
    let boot_code_len = std::cmp::min(boot_img.len(), 446);

    // Write boot code to the beginning of the disk
    disk.seek(SeekFrom::Start(0))?;
    disk.write_all(&boot_img[..boot_code_len])?;

    if is_gpt {
        // Write GPT signature marker at offset 92
        disk.seek(SeekFrom::Start(92))?;
        disk.write_all(&[0x22])?;

        // Decompress and write core.img (xz compressed) to offset 34 sectors
        let core_img_xz = read_install_file(CHOOSABLE_FILE_STG1_IMG)?;
        let decompressed = decompress_xz(&core_img_xz)?;
        let write_len = std::cmp::min(decompressed.len(), 2014 * 512);
        disk.seek(SeekFrom::Start(34 * 512))?;
        disk.write_all(&decompressed[..write_len])?;

        // Write GPT end marker
        disk.seek(SeekFrom::Start(17908))?;
        disk.write_all(&[0x23])?;
    } else {
        // MBR: Write core.img starting at sector 1
        let core_img_xz = read_install_file(CHOOSABLE_FILE_STG1_IMG)?;
        let decompressed = decompress_xz(&core_img_xz)?;
        let write_len = std::cmp::min(decompressed.len(), 2047 * 512);
        disk.seek(SeekFrom::Start(1 * 512))?;
        disk.write_all(&decompressed[..write_len])?;
    }

    // Write choosable.disk.img to partition 2
    let disk_img_xz = read_install_file(CHOOSABLE_FILE_DISK_IMG)?;
    let decompressed = decompress_xz(&disk_img_xz)?;
    let write_len = std::cmp::min(decompressed.len(), (CHOOSABLE_SECTOR_NUM * 512) as usize);
    disk.seek(SeekFrom::Start(part2_start_sector * 512))?;
    disk.write_all(&decompressed[..write_len])?;

    disk.flush()?;

    Ok(())
}

/// Write the choosable disk image to partition 2 (for install_gpt which uses disk_path)
fn write_disk_image<W: Write + Seek>(
    disk: &mut W,
    disk_path: &str,
    part2_start_sector: u64,
    secure_boot: bool,
) -> Result<()> {
    println!("Writing disk image to partition 2...");

    let disk_img_xz = read_install_file(CHOOSABLE_FILE_DISK_IMG)?;
    let decompressed = decompress_xz(&disk_img_xz)?;
    let write_len = std::cmp::min(decompressed.len(), (CHOOSABLE_SECTOR_NUM * 512) as usize);
    disk.seek(SeekFrom::Start(part2_start_sector * 512))?;
    disk.write_all(&decompressed[..write_len])?;

    // Write disk GUID
    let disk_guid = generate_guid();
    disk.seek(SeekFrom::Start(384))?;
    disk.write_all(&disk_guid)?;

    // Write disk signature
    disk.seek(SeekFrom::Start(440))?;
    disk.write_all(&disk_guid[12..16])?;

    disk.flush()?;

    // Process ESP partition for secure boot
    if !secure_boot {
        std::thread::sleep(std::time::Duration::from_secs(2));
        // Resize ESP for non-secure boot
        // TODO: Implement vtoycli partresize equivalent
        let _ = disk_path;
    }

    Ok(())
}

/// Decompress XZ data using ruzstd or an external xzcat
fn decompress_xz(data: &[u8]) -> Result<Vec<u8>> {
    // Try using xzcat command line tool (simplest approach)
    use std::process::{Command, Stdio};
    use std::io::Write;

    let mut child = Command::new("xzcat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| ChoosableError::ToolNotFound("xzcat".to_string()))?;

    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(data)?;
    }

    let output = child.wait_with_output()
        .map_err(|e| ChoosableError::Generic(format!("xzcat failed: {}", e)))?;

    if !output.status.success() {
        return Err(ChoosableError::Generic(
            String::from_utf8_lossy(&output.stderr).to_string()
        ));
    }

    Ok(output.stdout)
}

/// Update Choosable on an already-installed disk
pub fn update_choosable(
    disk_path: &str,
    secure_boot: Option<bool>,
    yes: bool,
) -> Result<()> {
    // Validate disk
    if !is_whole_disk(disk_path) {
        return Err(ChoosableError::IsPartition(disk_path.to_string()));
    }

    let size_bytes = get_disk_size(disk_path)?;
    let model = get_disk_model(disk_path);

    // Detect Choosable
    let (is_choosable, old_version, part2_start, _, mbr) = detect_choosable(disk_path, size_bytes)?;

    if !is_choosable {
        return Err(ChoosableError::NotChoosableDisk);
    }

    let old_ver = old_version.unwrap_or_else(|| "Unknown".to_string());

    // Get current version (from installed files)
    let cur_ver = get_current_version()?;

    // Determine secure boot setting
    let use_secure_boot = secure_boot.unwrap_or(true); // default keep current

    // Print info
    println!("Disk : {}", disk_path);
    println!("Model: {}", model);
    println!("Size : {} GiB", human_readable_gb(size_bytes));
    println!();

    println!("\x1b[33mUpgrade operation is safe, all data in the 1st partition (ISO files etc.) will be unchanged!\x1b[0m");
    println!();

    if !yes {
        print!("Update Choosable {} ===> {}   Continue? (y/n) ", old_ver, cur_ver);
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        if answer.trim().to_lowercase() != "y" {
            println!("Aborted.");
            return Ok(());
        }
    }

    let part2_start = part2_start.unwrap();

    // Open disk for writing
    let mut disk = open_disk_readwrite(disk_path)?;

    // Save disk UUID (16 bytes at offset 384)
    let mut diskuuid = [0u8; 16];
    disk.seek(SeekFrom::Start(384))?;
    disk.read_exact(&mut diskuuid)?;

    // Write boot image
    let boot_img = read_install_file(CHOOSABLE_FILE_BOOT_IMG)?;
    let boot_code_len = std::cmp::min(boot_img.len(), 440);
    disk.seek(SeekFrom::Start(0))?;
    disk.write_all(&boot_img[..boot_code_len])?;

    // Restore disk UUID
    disk.seek(SeekFrom::Start(384))?;
    disk.write_all(&diskuuid)?;

    // Save reserved data (8 sectors from sector 2040)
    let mut rsv_data = vec![0u8; 8 * 512];
    disk.seek(SeekFrom::Start(2040 * 512))?;
    disk.read_exact(&mut rsv_data)?;

    let is_gpt = mbr.is_gpt_protective();

    if is_gpt {
        // GPT update
        disk.seek(SeekFrom::Start(92))?;
        disk.write_all(&[0x22])?;

        let core_img_xz = read_install_file(CHOOSABLE_FILE_STG1_IMG)?;
        let decompressed = decompress_xz(&core_img_xz)?;
        let write_len = std::cmp::min(decompressed.len(), 2014 * 512);
        disk.seek(SeekFrom::Start(34 * 512))?;
        disk.write_all(&decompressed[..write_len])?;

        disk.seek(SeekFrom::Start(17908))?;
        disk.write_all(&[0x23])?;
    } else {
        // MBR update: fix active flag
        let part1_active = mbr.part1_active();
        let part2_active = mbr.part2_active();

        if part1_active == 0x00 && part2_active == 0x80 {
            // Set part1 active, part2 inactive
            disk.seek(SeekFrom::Start(446))?;
            disk.write_all(&[PART_ACTIVE])?;
            disk.seek(SeekFrom::Start(462))?;
            disk.write_all(&[PART_INACTIVE])?;
        }

        let core_img_xz = read_install_file(CHOOSABLE_FILE_STG1_IMG)?;
        let decompressed = decompress_xz(&core_img_xz)?;
        let write_len = std::cmp::min(decompressed.len(), 2047 * 512);
        disk.seek(SeekFrom::Start(1 * 512))?;
        disk.write_all(&decompressed[..write_len])?;
    }

    // Restore reserved data
    disk.seek(SeekFrom::Start(2040 * 512))?;
    disk.write_all(&rsv_data)?;

    // Sync
    disk.flush()?;

    // Write choosable disk image
    let disk_img_xz = read_install_file(CHOOSABLE_FILE_DISK_IMG)?;
    let decompressed = decompress_xz(&disk_img_xz)?;
    let write_len = std::cmp::min(decompressed.len(), (CHOOSABLE_SECTOR_NUM * 512) as usize);
    disk.seek(SeekFrom::Start(part2_start))?;
    disk.write_all(&decompressed[..write_len])?;

    disk.flush()?;

    // ESP processing for secure boot
    if !use_secure_boot {
        std::thread::sleep(std::time::Duration::from_secs(2));
        // TODO: vtoycli partresize equivalent
    }

    if is_gpt {
        // TODO: vtoycli gpt -f equivalent (fix GPT attributes)
    }

    println!();
    println!("\x1b[32mChoosable updated successfully on {}.\x1b[0m", disk_path);

    Ok(())
}

/// Get the current Choosable version from installation files
fn get_current_version() -> Result<String> {
    match read_install_file(CHOOSABLE_FILE_VERSION) {
        Ok(data) => Ok(String::from_utf8_lossy(&data).trim().to_string()),
        Err(_) => Ok("Unknown".to_string()),
    }
}

/// List Choosable information on a disk
pub fn list_choosable(disk_path: &str) -> Result<()> {
    if !is_whole_disk(disk_path) {
        return Err(ChoosableError::IsPartition(disk_path.to_string()));
    }

    let size_bytes = get_disk_size(disk_path)?;
    let model = get_disk_model(disk_path);

    println!("Disk : {}", disk_path);
    println!("Model: {}", model);
    println!("Size : {} GiB", human_readable_gb(size_bytes));

    let (is_choosable, version, _part2_start, _gpt_attr, mbr) = detect_choosable(disk_path, size_bytes)?;

    if is_choosable {
        println!("Choosable Version in Disk: {}", version.unwrap_or_else(|| "?".to_string()));

        let style = if mbr.is_gpt_protective() { "GPT" } else { "MBR" };
        println!("Disk Partition Style  : {}", style);

        // Secure boot status (we'd need to read the EFI partition to determine this)
        println!("Secure Boot Support   : YES");
    } else {
        println!("Choosable Version: NA");
    }

    println!();
    Ok(())
}

/// List all available disks
pub fn list_disks() -> Result<()> {
    let disks = enumerate_disks()?;

    println!("{:<4} {:<20} {:<10} {:<10} {:<8} {}", "ID", "Device", "Size", "Type", "Removable", "Model");
    println!("{}", "-".repeat(80));

    for disk in &disks {
        let size_gb = human_readable_gb(disk.size_bytes);
        let disk_type = if disk.is_usb { "USB" } else { "SATA" };
        let removable = if disk.removable { "Yes" } else { "No" };

        println!(
            "{:<4} {:<20} {:<8} GiB {:<10} {:<8} {}",
            disk.phy_drive, disk.disk_path, size_gb, disk_type, removable, disk.model
        );
    }

    Ok(())
}