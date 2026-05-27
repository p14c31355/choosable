use std::io::{Read, Seek, SeekFrom, Write};
use crate::{checks, constants::*, disk::*, disk_layout::*};
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

// ─── CRC32 helper (replaces `crc32fast` in-line) ────────────────────────

pub fn crc32_checksum(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

/// Recalculate GPT header and partition table CRCs in-place
pub fn finalize_gpt_crcs(gpt: &mut GptInfo) {
    let part_table_bytes = unsafe {
        std::slice::from_raw_parts(gpt.partitions.as_ptr() as *const u8, 128 * 128)
    };
    gpt.header.part_table_crc32 = crc32_checksum(part_table_bytes);
    gpt.header.header_crc32 = 0;
    let header_bytes = unsafe {
        std::slice::from_raw_parts(&gpt.header as *const GptHeader as *const u8, 92)
    };
    gpt.header.header_crc32 = crc32_checksum(header_bytes);
}

/// Compute backup GPT header (swap primary/backup LBA, recalc CRC)
pub fn make_backup_gpt_header(primary: &GptInfo) -> GptHeader {
    let mut backup = primary.header.clone();
    // Direct field access on packed struct — Rust handles unaligned access automatically
    let efi_start = backup.efi_start_lba;
    let efi_backup = backup.efi_backup_lba;
    backup.efi_start_lba = efi_backup;
    backup.efi_backup_lba = efi_start;
    backup.part_table_start_lba = efi_start + 1 - 33;
    backup.header_crc32 = 0;
    let header_bytes = unsafe {
        std::slice::from_raw_parts(&backup as *const GptHeader as *const u8, 92)
    };
    backup.header_crc32 = crc32_checksum(header_bytes);
    backup
}

// ─── Non-destructive install ────────────────────────────────────────────

pub fn non_destructive_install(disk_path: &str, label: &str, fs_type: FilesystemType, secure_boot: bool, yes: bool) -> Result<()> {
    if !is_whole_disk(disk_path) {
        return Err(ChoosableError::IsPartition(disk_path.to_string()));
    }

    let size_bytes = get_disk_size(disk_path)?;
    let disk_size_gb = human_readable_gb(size_bytes);
    let model = get_disk_model(disk_path);

    // Pre-flight checks (Ventoy: check_umount_disk, check_swap)
    checks::check_umount_disk(disk_path)?;
    checks::check_swap(disk_path)?;

    let (is_choosable, _version, _, _, _) = detect_choosable(disk_path, size_bytes)?;
    if is_choosable {
        println!("Disk already contains Choosable. Non-destructive installation not needed.");
        return Ok(());
    }

    let part1 = get_partition_name(disk_path, 1);
    if !std::path::Path::new(&part1).exists() {
        return Err(ChoosableError::Generic("Partition 1 not found on disk".to_string()));
    }

    let part1_sectors = get_partition_size_sectors(&part1)?;
    let part1_mb = (part1_sectors * SECTOR_SIZE) / SIZE_1MB;

    let mbr = read_mbr(disk_path)?;
    let is_gpt = read_mbr_is_gpt(&mbr);

    println!("Disk : {}", disk_path);
    println!("Model: {}", model);
    println!("Size : {} GiB", disk_size_gb);
    if is_gpt { println!("Style: GPT"); } else { println!("Style: MBR"); }
    println!();

    eprintln!("\x1b[33mAttention:\x1b[0m");
    eprintln!("\x1b[33mChoosable will try non-destructive installation on {} if possible.\x1b[0m", disk_path);
    eprintln!();

    if !yes {
        print!("Continue? (y/n) ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        if answer.trim().to_lowercase() != "y" { println!("Aborted."); return Ok(()); }
    }

    let disk_sectors = size_bytes / SECTOR_SIZE;
    let min_required = CHOOSABLE_PART1_START_SECTOR + (CHOOSABLE_EFI_PART_SIZE / SECTOR_SIZE);
    if disk_sectors <= min_required {
        return Err(ChoosableError::DiskTooSmall { required: min_required * SECTOR_SIZE, available: size_bytes });
    }

    let part1_start = get_partition_start_sector(&part1)?;
    if part1_start != CHOOSABLE_PART1_START_SECTOR {
        return Err(ChoosableError::Generic("Partition 1 does not start at 1 MiB".to_string()));
    }

    let part1_end = part1_start + part1_sectors;
    let next_part_start = find_next_partition_start(disk_path, &mbr, is_gpt, size_bytes)?;
    let free_space = next_part_start.saturating_sub(part1_end * SECTOR_SIZE);

    let part2_start_sector = if free_space >= CHOOSABLE_EFI_PART_SIZE {
        align_to_4k(part1_end)
    } else {
        let efi_part_size_mb = CHOOSABLE_EFI_PART_SIZE / SIZE_1MB;
        if part1_mb <= efi_part_size_mb {
            return Err(ChoosableError::Generic(format!(
                "Partition 1 is too small ({} MiB) to be shrunk by {} MiB", part1_mb, efi_part_size_mb
            )));
        }
        let new_part1_mb = part1_mb - efi_part_size_mb;
        println!("We need to shrink partition 1 from {} MiB to {} MiB...", part1_mb, new_part1_mb);

        let fs_type_str = detect_partition_fs(&part1)?;
        match fs_type_str.as_str() {
            "ntfs" => {
                run_cmd("ntfsfix", &["-b", "-d", &part1])?;
                run_cmd("ntfsresize", &["-f", "--size", &format!("{}M", new_part1_mb), &part1])?;
            }
            "ext4" | "ext3" | "ext2" => {
                run_cmd("e2fsck", &["-f", &part1])?;
                run_cmd("resize2fs", &[&part1, &format!("{}M", new_part1_mb)])?;
            }
            other => {
                return Err(ChoosableError::UnsupportedFilesystem(format!(
                    "Cannot shrink filesystem type: {}", other
                )));
            }
        }

        align_to_4k(part1_end - (CHOOSABLE_EFI_PART_SIZE / SECTOR_SIZE))
    };

    println!("Writing partition table with new CZBLEFI partition...");
    update_partition_table(disk_path, &mbr, is_gpt, size_bytes, part2_start_sector)?;

    let mut disk_file = open_disk_readwrite(disk_path)?;

    // Format EFI partition (FAT16 "CZBLEFI")
    format_efi_partition(disk_path, 2)?;

    write_boot_images(&mut disk_file, is_gpt, part2_start_sector)?;

    let guid = generate_guid();
    disk_file.seek(SeekFrom::Start(384))?; disk_file.write_all(&guid)?;
    disk_file.seek(SeekFrom::Start(440))?; disk_file.write_all(&guid[12..16])?;
    disk_file.flush()?;

    write_disk_image_raw(&mut disk_file, part2_start_sector)?;
    disk_file.flush()?;

    // Fix GPT attributes for GPT scheme
    if is_gpt {
        fix_gpt_attributes(disk_path)?;
    }

    if !secure_boot {
        std::thread::sleep(std::time::Duration::from_secs(1));
        process_secure_boot_esp(disk_path, part2_start_sector, false)?;
    }

    notify_kernel(disk_path);

    println!();
    println!("\x1b[32mChoosable non-destructive installation on {} successfully finished.\x1b[0m", disk_path);
    Ok(())
}

fn detect_partition_fs(partition: &str) -> Result<String> {
    let output = std::process::Command::new("blkid")
        .args(&["-o", "value", "-s", "TYPE", partition])
        .output()
        .map_err(|_| ChoosableError::ToolNotFound("blkid".to_string()))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_lowercase())
    } else {
        Err(ChoosableError::UnsupportedFilesystem("Cannot detect filesystem".to_string()))
    }
}

fn run_cmd(cmd: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(cmd).args(args).status()
        .map_err(|e| ChoosableError::ToolNotFound(format!("{}: {}", cmd, e)))?;
    if !status.success() {
        return Err(ChoosableError::Generic(format!("{} failed with code {:?}", cmd, status.code())));
    }
    Ok(())
}

/// 4KB alignment helper
fn align_to_4k(sector: u64) -> u64 {
    let m = sector % 8;
    if m > 0 { sector - m } else { sector }
}

fn find_next_partition_start(disk_path: &str, mbr: &Mbr, is_gpt: bool, disk_size_bytes: u64) -> Result<u64> {
    if is_gpt {
        let mut file = std::fs::File::open(disk_path)?;
        let gpt = GptInfo::read_from_disk(&mut file)?;
        let mut next = gpt.header.part_area_end_lba + 1;
        for i in 1..128 {
            if gpt.partitions[i].unique_part_guid != [0u8; 16] && gpt.partitions[i].start_lba < next {
                next = gpt.partitions[i].start_lba;
            }
        }
        Ok(next * SECTOR_SIZE)
    } else {
        let mut next = disk_size_bytes;
        for i in 1..4 {
            if mbr.partitions[i].sector_count > 0 {
                let start = mbr.partitions[i].start_lba as u64 * SECTOR_SIZE;
                if start < next { next = start; }
            }
        }
        Ok(next)
    }
}

fn update_partition_table(disk_path: &str, mbr: &Mbr, is_gpt: bool, _disk_size_bytes: u64, part2_start_sector: u64) -> Result<()> {
    if is_gpt {
        let mut disk = open_disk_readwrite(disk_path)?;
        update_gpt_partition_table_f(&mut disk, part2_start_sector)?;
        disk.flush()?;
    } else {
        let mut disk = open_disk_readwrite(disk_path)?;
        update_mbr_partition_table_f(&mut disk, mbr, part2_start_sector)?;
        disk.flush()?;
    }
    Ok(())
}

fn update_mbr_partition_table_f(disk: &mut std::fs::File, mbr: &Mbr, part2_start_sector: u64) -> Result<()> {
    let mut new_mbr = mbr.clone();
    let efi_sectors = (CHOOSABLE_EFI_PART_SIZE / SECTOR_SIZE) as u32;
    let disk_size = disk.seek(SeekFrom::End(0))?;
    disk.seek(SeekFrom::Start(0))?;

    let slot = {
        let mut s = None;
        for i in 1..4 {
            if new_mbr.partitions[i].sector_count == 0 { s = Some(i); break; }
        }
        match s {
            Some(slot) => slot,
            None => {
                for j in (1..=2).rev() { new_mbr.partitions[j + 1] = new_mbr.partitions[j]; }
                1
            }
        }
    };

    let part1_count = part2_start_sector as u32 - CHOOSABLE_PART1_START_SECTOR as u32;
    let mut part0 = new_mbr.partitions[0];
    fill_mbr_chs_entry(&mut part0, disk_size, CHOOSABLE_PART1_START_SECTOR as u32, part1_count);
    new_mbr.partitions[0] = part0;
    new_mbr.partitions[0].active = PART_ACTIVE;
    new_mbr.partitions[0].fs_flag = 0x07;

    let mut part_slot = new_mbr.partitions[slot];
    fill_mbr_chs_entry(&mut part_slot, disk_size, part2_start_sector as u32, efi_sectors);
    new_mbr.partitions[slot] = part_slot;
    new_mbr.partitions[slot].active = PART_INACTIVE;
    new_mbr.partitions[slot].fs_flag = PART_TYPE_EFI_SYSTEM;

    disk.seek(SeekFrom::Start(0))?;
    new_mbr.write(disk)?;
    Ok(())
}

fn fill_mbr_chs_entry(entry: &mut PartitionTableEntry, _disk_size_bytes: u64, start_sector: u32, sector_count: u32) {
    let nsector: u32 = 63u32;

    entry.start_lba = start_sector;
    entry.sector_count = sector_count;

    let cylinder = start_sector / 255 / nsector;
    let head = (start_sector / nsector) % 255;
    let sector = (start_sector % nsector) + 1;

    entry.start_head = head as u8;
    entry.start_sector_cylinder = ((cylinder as u16 & 0x3FF) << 6) | ((sector as u16) & 0x3F);

    let end_lba = start_sector + sector_count - 1;
    let ecylinder = end_lba / 255 / nsector;
    let ehead = (end_lba / nsector) % 255;
    let esector = (end_lba % nsector) + 1;

    entry.end_head = ehead as u8;
    entry.end_sector_cylinder = ((ecylinder as u16 & 0x3FF) << 6) | ((esector as u16) & 0x3F);
}

fn update_gpt_partition_table_f(disk: &mut std::fs::File, part2_start_sector: u64) -> Result<()> {
    disk.seek(SeekFrom::Start(0))?;
    let mut gpt = GptInfo::read_from_disk(disk)?;
    let efi_sectors = CHOOSABLE_EFI_PART_SIZE / SECTOR_SIZE;

    let slot = {
        let mut s = None;
        for i in 1..128 {
            if gpt.partitions[i].unique_part_guid == [0u8; 16] { s = Some(i); break; }
        }
        match s {
            Some(slot) => slot,
            None => {
                for j in (1..127).rev() { gpt.partitions[j + 1] = gpt.partitions[j]; }
                1
            }
        }
    };

    gpt.partitions[0].end_lba = part2_start_sector - 1;

    gpt.partitions[slot].part_type_guid = GPT_TYPE_EFI_SYSTEM;
    gpt.partitions[slot].unique_part_guid = generate_guid();
    gpt.partitions[slot].start_lba = part2_start_sector;
    gpt.partitions[slot].end_lba = part2_start_sector + efi_sectors - 1;
    gpt.partitions[slot].attributes = GPT_ATTR_CZBLEFI;
    let mut name = [0u16; 36];
    name[0] = 'C' as u16; name[1] = 'Z' as u16; name[2] = 'B' as u16;
    name[3] = 'L' as u16; name[4] = 'E' as u16; name[5] = 'F' as u16; name[6] = 'I' as u16;
    gpt.partitions[slot].name = name;

    finalize_gpt_crcs(&mut gpt);
    gpt.write_to_disk(disk)?;
    Ok(())
}

// ─── EFI partition format (FAT16 "CZBLEFI") ─────────────────────────────

/// Format the EFI partition as FAT16 with label "CZBLEFI"
fn format_efi_partition(disk_path: &str, part_num: u32) -> Result<()> {
    let part = get_partition_name(disk_path, part_num);
    println!("Formatting EFI partition {} as FAT16...", part);

    // Ventoy uses: mkfs.vfat -F 16 -n CZBLEFI -s 1 <partition>
    for _ in 0..10 {
        // Unmount if mounted
        check_umount(&part);

        let status = std::process::Command::new("mkfs.vfat")
            .args(&["-F", "16", "-n", "CZBLEFI", "-s", "1", &part])
            .status();

        match status {
            Ok(s) if s.success() => {
                println!("EFI partition formatted successfully.");
                return Ok(());
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ChoosableError::ToolNotFound("mkfs.vfat".to_string()));
            }
            _ => {
                println!("mkfs.vfat failed, retrying...");
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
    }

    Err(ChoosableError::FormatFailed)
}

/// Zero-clear 32 sectors at EFI partition start (Ventoy: dd if=/dev/zero ...)
fn zero_efi_partition(disk_path: &str, part2_start_sector: u64) -> Result<()> {
    let mut disk = std::fs::OpenOptions::new().write(true).open(disk_path)?;
    let zero_buf = vec![0u8; 32 * 512];
    disk.seek(SeekFrom::Start(part2_start_sector * 512))?;
    disk.write_all(&zero_buf)?;
    disk.flush()?;
    Ok(())
}

/// Unmount a partition if mounted
fn check_umount(partition: &str) {
    // Read /proc/mounts and unmount
    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        for line in mounts.lines() {
            if line.starts_with(partition) {
                if let Some(mount_point) = line.split_whitespace().nth(1) {
                    let _ = std::process::Command::new("umount")
                        .arg(mount_point)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
        }
    }
}

// ─── GPT attribute fix (czblgpt -f equivalent) ──────────────────────────

/// Fix GPT attributes for CZBLEFI partition on already-installed disk
pub fn fix_gpt_attributes(disk_path: &str) -> Result<()> {
    let mut disk = open_disk_readwrite(disk_path)?;

    // Get disk size
    let disk_size = disk.seek(SeekFrom::End(0))?;
    disk.seek(SeekFrom::Start(0))?;

    let mut gpt = GptInfo::read_from_disk(&mut disk)?;

    let entry = &mut gpt.partitions[1];
    let is_czblefi = entry.name[0] == 'C' as u16 && entry.name[1] == 'Z' as u16 && entry.name[2] == 'B' as u16 && entry.name[3] == 'L' as u16;
    let current_attr = entry.attributes;

    if is_czblefi && current_attr != GPT_ATTR_CZBLEFI {
        println!("Fixing GPT attributes for CZBLEFI partition...");
        entry.attributes = GPT_ATTR_CZBLEFI;

        // Recalculate CRCs and write
        finalize_gpt_crcs(&mut gpt);
        gpt.write_to_disk(&mut disk)?;

        // Also write backup GPT
        let backup_header = make_backup_gpt_header(&gpt);
        let backup_offset = gpt.header.efi_backup_lba * SECTOR_SIZE;
        disk.seek(SeekFrom::Start(backup_offset))?;
        backup_header.write(&mut disk)?;

        // Write backup partition table
        let backup_pt_offset = (gpt.header.efi_backup_lba - 32) * SECTOR_SIZE;
        disk.seek(SeekFrom::Start(backup_pt_offset))?;
        for entry in &gpt.partitions {
            let ptr = entry as *const GptPartitionEntry as *const u8;
            let bytes = unsafe { std::slice::from_raw_parts(ptr, 128) };
            disk.write_all(bytes)?;
        }

        disk.flush()?;
        println!("GPT attributes fixed.");
    }

    Ok(())
}

// ─── Kernel notification ────────────────────────────────────────────────

fn notify_kernel(disk_path: &str) {
    let _ = std::process::Command::new("partprobe").arg(disk_path)
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    let _ = std::process::Command::new("udevadm").args(&["trigger", "--name-match", disk_path])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    let _ = std::process::Command::new("partx").args(&["-u", disk_path])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
}

// ─── Secure Boot ESP processing ─────────────────────────────────────────

pub fn process_secure_boot_esp(disk_path: &str, part2_start_byte: u64, enable_secure_boot: bool) -> Result<()> {
    use std::io::Cursor;

    if enable_secure_boot { return Ok(()); }

    let mut file = std::fs::OpenOptions::new().read(true).write(true).open(disk_path)?;
    let partition_size = CHOOSABLE_EFI_PART_SIZE;
    let mut buf = vec![0u8; partition_size as usize];
    file.seek(SeekFrom::Start(part2_start_byte))?;
    file.read_exact(&mut buf)?;

    let rw_buf = {
        let mut rw = buf.clone();
        {
            let cursor = Cursor::new(&mut rw[..]);
            let fs = match fatfs::FileSystem::new(cursor, fatfs::FsOptions::new()) {
                Ok(fs) => fs, Err(_) => return Ok(()),
            };
            let root = fs.root_dir();

            let has_sb = if let Ok(efi) = root.open_dir("EFI") {
                if let Ok(boot) = efi.open_dir("BOOT") {
                    boot.iter().any(|e| {
                        if let Ok(entry) = e { entry.file_name() == "grubx64_real.efi" } else { false }
                    })
                } else { false }
            } else { false };

            if has_sb {
                println!("Disabling Secure Boot (renaming EFI files)...");
                if let Ok(efi) = root.open_dir("EFI") {
                    if let Ok(boot) = efi.open_dir("BOOT") {
                        let _ = boot.rename("grubx64_real.efi", &boot, "BOOTX64.EFI");
                        let _ = boot.rename("grubia32_real.efi", &boot, "BOOTIA32.EFI");
                        let _ = boot.remove("grubx64.efi");
                        let _ = boot.remove("MokManager.efi");
                        let _ = boot.remove("mmx64.efi");
                        let _ = boot.remove("grubia32.efi");
                        let _ = boot.remove("mmia32.efi");
                    }
                }
                let _ = root.remove("ENROLL_THIS_KEY_IN_MOKMANAGER.cer");
            }
        }
        rw
    };

    file.seek(SeekFrom::Start(part2_start_byte))?;
    file.write_all(&rw_buf)?;
    file.flush()?;

    notify_kernel(disk_path);
    Ok(())
}

// ─── Standard install ───────────────────────────────────────────────────

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
    if !is_whole_disk(disk_path) {
        return Err(ChoosableError::IsPartition(disk_path.to_string()));
    }

    let size_bytes = get_disk_size(disk_path)?;
    let disk_size_gb = human_readable_gb(size_bytes);
    let model = get_disk_model(disk_path);

    if is_4k_native(disk_path) { return Err(ChoosableError::FourKNativeSector); }
    if !use_gpt && size_bytes > 2 * SIZE_1TB { return Err(ChoosableError::MbrOverflow); }

    let required_sectors = CHOOSABLE_PART1_START_SECTOR + (CHOOSABLE_PART_SIZE_MB * 2048);
    if size_bytes < required_sectors * SECTOR_SIZE {
        return Err(ChoosableError::DiskTooSmall { required: required_sectors * SECTOR_SIZE, available: size_bytes });
    }

    if reserve_space_mb > 0 {
        let reserve_sectors = (reserve_space_mb + CHOOSABLE_PART_SIZE_MB) * 2048;
        if size_bytes / SECTOR_SIZE <= reserve_sectors {
            return Err(ChoosableError::Generic(format!("Cannot reserve {} MiB on disk", reserve_space_mb)));
        }
    }

    // Pre-flight checks (Ventoy: check_umount_disk, check_swap, check_tool_work_ok)
    checks::check_umount_disk(disk_path)?;
    checks::check_swap(disk_path)?;
    checks::check_tool_work_ok()?;

    let (is_choosable, version, _, _, _) = detect_choosable(disk_path, size_bytes)?;
    if is_choosable && !force {
        return Err(ChoosableError::AlreadyInstalled(version.unwrap_or_else(|| "?".to_string())));
    }

    println!("Disk : {}", disk_path);
    println!("Model: {}", model);
    println!("Size : {} GiB", disk_size_gb);
    if use_gpt { println!("Style: GPT"); } else { println!("Style: MBR"); }
    if reserve_space_mb > 0 { println!("You will reserve {} MiB disk space", reserve_space_mb); }
    println!();

    eprintln!("\x1b[33mAttention:\x1b[0m");
    eprintln!("\x1b[33mYou will install Choosable to {}.\x1b[0m", disk_path);
    eprintln!("\x1b[33mAll the data on the disk {} will be lost!!!\x1b[0m", disk_path);
    eprintln!();

    if !yes {
        print!("Continue? (y/n) ");
        std::io::stdout().flush().ok();
        let mut answer = String::new(); std::io::stdin().read_line(&mut answer).ok();
        if answer.trim().to_lowercase() != "y" { println!("Aborted."); return Ok(()); }

        eprintln!();
        eprintln!("\x1b[33mAll the data on the disk {} will be lost!!!\x1b[0m", disk_path);
        print!("Double-check. Continue? (y/n) ");
        std::io::stdout().flush().ok();
        let mut answer = String::new(); std::io::stdin().read_line(&mut answer).ok();
        if answer.trim().to_lowercase() != "y" { println!("Aborted."); return Ok(()); }
    }

    let mut disk = open_disk_readwrite(disk_path)?;

    println!("Cleaning disk...");
    let zero_buf = vec![0u8; 64 * 512];
    disk.seek(SeekFrom::Start(0))?; disk.write_all(&zero_buf)?; disk.flush()?;

    println!("Creating partition table...");
    if use_gpt {
        install_gpt_f(&mut disk, disk_path, size_bytes, reserve_space_mb, label, fs_type, secure_boot)?;
    } else {
        install_mbr_f(&mut disk, disk_path, size_bytes, reserve_space_mb, label, fs_type, secure_boot)?;
    }

    notify_kernel(disk_path);

    println!();
    println!("\x1b[32mChoosable installed successfully to {}.\x1b[0m", disk_path);
    Ok(())
}

fn install_mbr_f(disk: &mut std::fs::File, disk_path: &str, disk_size_bytes: u64, reserve_space_mb: u64, label: &str, fs_type: FilesystemType, secure_boot: bool) -> Result<()> {
    let total_sectors = disk_size_bytes / SECTOR_SIZE;
    let efi_part_sectors = CHOOSABLE_EFI_PART_SIZE / SECTOR_SIZE;
    let part1_start = CHOOSABLE_PART1_START_SECTOR;

    let mut part2_start = if reserve_space_mb > 0 { total_sectors - efi_part_sectors - (reserve_space_mb * 2048) } else { total_sectors - efi_part_sectors };
    part2_start = align_to_4k(part2_start);
    let part1_sectors = part2_start - part1_start;

    let mut mbr = Mbr::new_empty();
    let mut part0 = mbr.partitions[0];
    fill_mbr_chs_entry(&mut part0, disk_size_bytes, part1_start as u32, part1_sectors as u32);
    mbr.partitions[0] = part0;
    mbr.partitions[0].active = PART_ACTIVE;
    mbr.partitions[0].fs_flag = 0x07;
    let mut part1 = mbr.partitions[1];
    fill_mbr_chs_entry(&mut part1, disk_size_bytes, part2_start as u32, efi_part_sectors as u32);
    mbr.partitions[1] = part1;
    mbr.partitions[1].fs_flag = PART_TYPE_EFI_SYSTEM;

    disk.seek(SeekFrom::Start(0))?; mbr.write(disk)?; disk.flush()?;

    // Remove old partition nodes and wait for new ones to appear
    checks::remove_partition_nodes(disk_path);
    checks::wait_for_partitions(disk_path)?;

    // Zero-clear EFI partition area (32 sectors) then format
    zero_efi_partition(disk_path, part2_start)?;

    // Format EFI partition
    format_efi_partition(disk_path, 2)?;

    let part1 = get_partition_name(disk_path, 1);
    format_partition(&part1, label, fs_type)?;
    write_boot_images(disk, false, part2_start)?;

    let guid = generate_guid();
    disk.seek(SeekFrom::Start(384))?; disk.write_all(&guid)?;
    disk.seek(SeekFrom::Start(440))?; disk.write_all(&guid[12..16])?;
    disk.flush()?;

    write_disk_image_raw(disk, part2_start)?;
    disk.flush()?;

    if !secure_boot {
        std::thread::sleep(std::time::Duration::from_secs(1));
        process_secure_boot_esp(disk_path, part2_start * SECTOR_SIZE, false)?;
    }
    Ok(())
}

fn install_gpt_f(disk: &mut std::fs::File, disk_path: &str, disk_size_bytes: u64, reserve_space_mb: u64, label: &str, fs_type: FilesystemType, secure_boot: bool) -> Result<()> {
    let efi_part_sectors = CHOOSABLE_EFI_PART_SIZE / SECTOR_SIZE;
    let total_sectors = disk_size_bytes / SECTOR_SIZE;
    let part2_end = total_sectors - 34;
    let mut part2_start = if reserve_space_mb > 0 { part2_end - efi_part_sectors - (reserve_space_mb * 2048) } else { part2_end - efi_part_sectors };
    part2_start = align_to_4k(part2_start);
    let part1_end = part2_start - 1;

    let disk_guid = generate_guid();
    let mut gpt_info = GptInfo::new_choosable(disk_size_bytes, disk_guid);
    gpt_info.partitions[0].start_lba = CHOOSABLE_PART1_START_SECTOR;
    gpt_info.partitions[0].end_lba = part1_end;
    gpt_info.partitions[0].attributes = 0;
    gpt_info.partitions[1].start_lba = part2_start;
    gpt_info.partitions[1].end_lba = part2_start + efi_part_sectors - 1;
    gpt_info.partitions[1].attributes = GPT_ATTR_CZBLEFI;

    // Finalize CRCs before writing
    finalize_gpt_crcs(&mut gpt_info);

    gpt_info.write_to_disk(disk)?;
    disk.flush()?;

    // Remove old partition nodes and wait for new ones
    checks::remove_partition_nodes(disk_path);
    checks::wait_for_partitions(disk_path)?;

    // Zero-clear EFI partition area (32 sectors) then format
    zero_efi_partition(disk_path, part2_start)?;

    // Format EFI partition
    format_efi_partition(disk_path, 2)?;

    let part1 = get_partition_name(disk_path, 1);
    format_partition(&part1, label, fs_type)?;
    write_boot_images(disk, true, part2_start)?;

    let guid = gpt_info.header.disk_guid;
    disk.seek(SeekFrom::Start(384))?; disk.write_all(&guid)?;
    disk.seek(SeekFrom::Start(440))?; disk.write_all(&guid[12..16])?;
    disk.flush()?;

    write_disk_image_raw(disk, part2_start)?;
    disk.flush()?;

    // Fix GPT attributes (ventoy equivalent: czblcli gpt -f)
    fix_gpt_attributes(disk_path)?;

    if !secure_boot {
        std::thread::sleep(std::time::Duration::from_secs(1));
        process_secure_boot_esp(disk_path, part2_start * SECTOR_SIZE, false)?;
    }
    Ok(())
}

// ─── Helper utilities ────────────────────────────────────────────────────

fn format_partition(partition: &str, label: &str, fs_type: FilesystemType) -> Result<()> {
    let (cmd, args): (&str, Vec<&str>) = match fs_type {
        FilesystemType::ExFat => {
            let part_sectors = get_partition_size_sectors(partition)?;
            let part_size_gb = part_sectors * SECTOR_SIZE / SIZE_1GB;
            let cluster_sectors = if part_size_gb > 32 { "256" } else { "64" };
            ("mkexfatfs", vec!["-n", label, "-s", cluster_sectors, partition])
        }
        FilesystemType::Fat32 => ("mkfs.vfat", vec!["-n", label, "-F", "32", partition]),
        FilesystemType::Ntfs => ("mkfs.ntfs", vec!["-f", "-L", label, partition]),
    };

    println!("Formatting {} as {}...", partition, fs_type.as_str());
    let status = std::process::Command::new(cmd).args(&args).status()
        .map_err(|e| ChoosableError::ToolNotFound(format!("{}: {}", cmd, e)))?;
    if !status.success() {
        let status = std::process::Command::new(cmd).args(&args).status().map_err(|_| ChoosableError::FormatFailed)?;
        if !status.success() { return Err(ChoosableError::FormatFailed); }
    }
    Ok(())
}

fn write_boot_images(disk: &mut std::fs::File, is_gpt: bool, _part2_start_sector: u64) -> Result<()> {
    println!("Writing boot images...");
    let boot_img = read_install_file(CHOOSABLE_FILE_BOOT_IMG)?;
    let boot_code_len = std::cmp::min(boot_img.len(), 446);
    disk.seek(SeekFrom::Start(0))?;
    disk.write_all(&boot_img[..boot_code_len])?;

    if is_gpt {
        disk.seek(SeekFrom::Start(92))?; disk.write_all(&[0x22])?;
        let core = decompress_xz(&read_install_file(CHOOSABLE_FILE_STG1_IMG)?)?;
        let len = std::cmp::min(core.len(), 2014 * 512);
        disk.seek(SeekFrom::Start(34 * 512))?; disk.write_all(&core[..len])?;
        disk.seek(SeekFrom::Start(17908))?; disk.write_all(&[0x23])?;
    } else {
        let core = decompress_xz(&read_install_file(CHOOSABLE_FILE_STG1_IMG)?)?;
        let len = std::cmp::min(core.len(), 2047 * 512);
        disk.seek(SeekFrom::Start(1 * 512))?; disk.write_all(&core[..len])?;
    }

    disk.flush()?;
    Ok(())
}

fn write_disk_image_raw(disk: &mut std::fs::File, part2_start_sector: u64) -> Result<()> {
    let disk_img_xz = read_install_file(CHOOSABLE_FILE_DISK_IMG)?;
    let decompressed = decompress_xz(&disk_img_xz)?;
    let len = std::cmp::min(decompressed.len(), (CHOOSABLE_SECTOR_NUM * 512) as usize);
    disk.seek(SeekFrom::Start(part2_start_sector * 512))?;
    disk.write_all(&decompressed[..len])?;
    disk.flush()?;
    Ok(())
}

fn decompress_xz(data: &[u8]) -> Result<Vec<u8>> {
    use std::process::{Command, Stdio};
    use std::io::Write;
    let mut child = Command::new("xzcat")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn()
        .map_err(|_| ChoosableError::ToolNotFound("xzcat".to_string()))?;

    std::thread::scope(|s| {
        if let Some(mut stdin) = child.stdin.take() {
            s.spawn(move || {
                let _ = stdin.write_all(data);
            });
        }
    });

    let output = child.wait_with_output().map_err(|e| ChoosableError::Generic(format!("xzcat: {}", e)))?;
    if !output.status.success() {
        return Err(ChoosableError::Generic(String::from_utf8_lossy(&output.stderr).to_string()));
    }
    Ok(output.stdout)
}

// ─── Update ──────────────────────────────────────────────────────────────

pub fn update_choosable(disk_path: &str, secure_boot: Option<bool>, yes: bool) -> Result<()> {
    if !is_whole_disk(disk_path) { return Err(ChoosableError::IsPartition(disk_path.to_string())); }

    let size_bytes = get_disk_size(disk_path)?;
    let model = get_disk_model(disk_path);
    let (is_choosable, old_version, part2_start, _, mbr) = detect_choosable(disk_path, size_bytes)?;

    if !is_choosable { return Err(ChoosableError::NotChoosableDisk); }

    // Pre-flight: unmount
    checks::check_umount_disk(disk_path)?;

    let old_ver = old_version.unwrap_or_else(|| "Unknown".to_string());
    let cur_ver = get_current_version()?;

    // Auto-detect secure boot status from disk if not specified (VentoyWorker.sh L556-562)
    let use_secure_boot = match secure_boot {
        Some(sb) => sb,
        None => {
            if let Some(p2) = part2_start {
                check_choosable_secure_boot(disk_path, p2)
            } else {
                true // default
            }
        }
    };

    println!("Disk : {}", disk_path);
    println!("Model: {}", model);
    println!("Size : {} GiB", human_readable_gb(size_bytes));
    println!();
    println!("\x1b[33mUpgrade operation is safe, all data in the 1st partition (ISO files etc.) will be unchanged!\x1b[0m");
    println!();

    if !yes {
        print!("Update Choosable {} ===> {}   Continue? (y/n) ", old_ver, cur_ver);
        std::io::stdout().flush().ok();
        let mut answer = String::new(); std::io::stdin().read_line(&mut answer).ok();
        if answer.trim().to_lowercase() != "y" { println!("Aborted."); return Ok(()); }
    }

    let part2_start = part2_start.unwrap();
    let mut disk = open_disk_readwrite(disk_path)?;

    let mut diskuuid = [0u8; 16];
    disk.seek(SeekFrom::Start(384))?; disk.read_exact(&mut diskuuid)?;

    let boot_img = read_install_file(CHOOSABLE_FILE_BOOT_IMG)?;
    let boot_code_len = std::cmp::min(boot_img.len(), 440);
    disk.seek(SeekFrom::Start(0))?; disk.write_all(&boot_img[..boot_code_len])?;
    disk.seek(SeekFrom::Start(384))?; disk.write_all(&diskuuid)?;

    let mut rsv_data = vec![0u8; 8 * 512];
    disk.seek(SeekFrom::Start(2040 * 512))?; disk.read_exact(&mut rsv_data)?;

    let is_gpt = mbr.is_gpt_protective();

    if is_gpt {
        disk.seek(SeekFrom::Start(92))?; disk.write_all(&[0x22])?;
        let core = decompress_xz(&read_install_file(CHOOSABLE_FILE_STG1_IMG)?)?;
        let len = std::cmp::min(core.len(), 2014 * 512);
        disk.seek(SeekFrom::Start(34 * 512))?; disk.write_all(&core[..len])?;
        disk.seek(SeekFrom::Start(17908))?; disk.write_all(&[0x23])?;
    } else {
        if mbr.part1_active() == 0x00 && mbr.part2_active() == 0x80 {
            disk.seek(SeekFrom::Start(446))?; disk.write_all(&[PART_ACTIVE])?;
            disk.seek(SeekFrom::Start(462))?; disk.write_all(&[PART_INACTIVE])?;
        }
        let core = decompress_xz(&read_install_file(CHOOSABLE_FILE_STG1_IMG)?)?;
        let len = std::cmp::min(core.len(), 2047 * 512);
        disk.seek(SeekFrom::Start(1 * 512))?; disk.write_all(&core[..len])?;
    }

    disk.seek(SeekFrom::Start(2040 * 512))?; disk.write_all(&rsv_data)?;
    disk.flush()?;

    let disk_img = decompress_xz(&read_install_file(CHOOSABLE_FILE_DISK_IMG)?)?;
    let len = std::cmp::min(disk_img.len(), (CHOOSABLE_SECTOR_NUM * 512) as usize);
    disk.seek(SeekFrom::Start(part2_start))?; disk.write_all(&disk_img[..len])?;
    disk.flush()?;

    if !use_secure_boot {
        std::thread::sleep(std::time::Duration::from_secs(1));
        process_secure_boot_esp(disk_path, part2_start, false)?;
    }

    // Fix GPT attributes on update (ventoy: czblcli gpt -f)
    if is_gpt {
        fix_gpt_attributes(disk_path)?;
    }

    notify_kernel(disk_path);

    println!();
    println!("\x1b[32mChoosable updated successfully on {}.\x1b[0m", disk_path);
    Ok(())
}

fn get_current_version() -> Result<String> {
    match read_install_file(CHOOSABLE_FILE_VERSION) {
        Ok(data) => Ok(String::from_utf8_lossy(&data).trim().to_string()),
        Err(_) => Ok("Unknown".to_string()),
    }
}

// ─── List ────────────────────────────────────────────────────────────────

pub fn list_choosable(disk_path: &str) -> Result<()> {
    if !is_whole_disk(disk_path) { return Err(ChoosableError::IsPartition(disk_path.to_string())); }
    let size_bytes = get_disk_size(disk_path)?;
    let model = get_disk_model(disk_path);

    println!("Disk : {}", disk_path);
    println!("Model: {}", model);
    println!("Size : {} GiB", human_readable_gb(size_bytes));

    let (is_choosable, version, part2_start, _gpt_attr, mbr) = detect_choosable(disk_path, size_bytes)?;

    if is_choosable {
        println!("Choosable Version in Disk: {}", version.unwrap_or_else(|| "?".to_string()));
        let style = if mbr.is_gpt_protective() { "GPT" } else { "MBR" };
        println!("Disk Partition Style  : {}", style);

        if let Some(p2) = part2_start {
            println!("Secure Boot Support   : {}", if check_choosable_secure_boot(disk_path, p2) { "YES" } else { "NO" });
        } else {
            println!("Secure Boot Support   : ?");
        }
    } else {
        println!("Choosable Version: NA");
    }
    println!();
    Ok(())
}

fn check_choosable_secure_boot(disk_path: &str, part2_start_byte: u64) -> bool {
    use std::io::Cursor;
    let file = match std::fs::OpenOptions::new().read(true).open(disk_path) {
        Ok(f) => f, Err(_) => return false,
    };
    let partition_size = CHOOSABLE_EFI_PART_SIZE;
    let mut buf = vec![0u8; partition_size as usize];
    let mut file = file;
    if file.seek(SeekFrom::Start(part2_start_byte)).is_err() { return false; }
    if file.read_exact(&mut buf).is_err() { return false; }

    let cursor = Cursor::new(buf);
    let fs = match fatfs::FileSystem::new(cursor, fatfs::FsOptions::new()) {
        Ok(fs) => fs, Err(_) => return false,
    };
    let root = fs.root_dir();
    if let Ok(efi) = root.open_dir("EFI") {
        if let Ok(boot) = efi.open_dir("BOOT") {
            return boot.iter().any(|e| {
                if let Ok(entry) = e { entry.file_name() == "grubx64_real.efi" } else { false }
            });
        }
    }
    false
}

pub fn list_disks() -> Result<()> {
    let disks = enumerate_disks()?;
    println!("{:<4} {:<20} {:<10} {:<10} {:<8} {}", "ID", "Device", "Size", "Type", "Removable", "Model");
    println!("{}", "-".repeat(80));
    for disk in &disks {
        let size_gb = human_readable_gb(disk.size_bytes);
        let disk_type = if disk.is_usb { "USB" } else { "SATA" };
        let removable = if disk.removable { "Yes" } else { "No" };
        println!("{:<4} {:<20} {:<8} GiB {:<10} {:<8} {}", disk.phy_drive, disk.disk_path, size_gb, disk_type, removable, disk.model);
    }
    Ok(())
}