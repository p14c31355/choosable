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
        std::slice::from_raw_parts(
            gpt.partitions.as_ptr() as *const u8,
            std::mem::size_of_val(&gpt.partitions),
        )
    };
    gpt.header.part_table_crc32 = crc32_checksum(part_table_bytes);
    gpt.header.header_crc32 = 0;
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            &gpt.header as *const GptHeader as *const u8,
            gpt.header.header_size as usize,
        )
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
    backup.part_table_start_lba = efi_backup - 32;
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
    let is_gpt = mbr.is_gpt_protective();

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

    // Stop udev while we modify the partition table, format partitions,
    // and install the bootloader.  If udev fires in the middle it will
    // see a half-initialised state and produce "No object for D-bus
    // interface" errors.
    let _ = std::process::Command::new("udevadm")
        .args(&["control", "--stop-exec-queue"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();

    println!("Writing partition table with new CZBLEFI partition...");
    update_partition_table(disk_path, &mbr, is_gpt, size_bytes, part2_start_sector)?;

    let mut disk_file = open_disk_readwrite(disk_path)?;

    write_boot_images(&mut disk_file, is_gpt, part2_start_sector)?;

    let guid = generate_guid();
    disk_file.seek(SeekFrom::Start(384))?; disk_file.write_all(&guid)?;
    disk_file.seek(SeekFrom::Start(440))?; disk_file.write_all(&guid[12..16])?;
    disk_file.flush()?;
    drop(disk_file);

    let _ = std::process::Command::new("sync")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();

    // Remove stale partition nodes and create new ones while udev is still
    // stopped.  Doing remove+add with a live udev causes udisks2 to see a
    // transient "no partition" state and produce "No object for D-bus
    // interface" errors.
    checks::remove_partition_nodes(disk_path);
    let _ = std::process::Command::new("partx").args(&["-a", disk_path])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    std::thread::sleep(std::time::Duration::from_millis(500));
    checks::wait_for_partitions(disk_path)?;

    // Format EFI partition and write bootloader while udev is still
    // stopped.  If udev is running, mkfs.vfat will fire change events
    // that cause udisks2 to see half-initialised filesystems.
    format_efi_partition(disk_path, 2)?;

    write_efi_bootloader(disk_path, part2_start_sector * SECTOR_SIZE)?;

    // Fix GPT attributes for GPT scheme
    if is_gpt {
        fix_gpt_attributes(disk_path)?;
    }

    if !secure_boot {
        // Safe while udev is stopped — operates on the already-formatted
        // FAT filesystem.
        process_secure_boot_esp(disk_path, part2_start_sector * SECTOR_SIZE, false)?;
    }

    // All disk writes are complete.  Re-read kernel partition table,
    // then resume udev so that udisks2 sees the final consistent state.
    let _ = std::process::Command::new("sync")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    notify_kernel_before_udev(disk_path);

    // Resume udev event processing now.
    let _ = std::process::Command::new("udevadm")
        .args(&["control", "--start-exec-queue"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();

    // Trigger change events and let udisks2 pick up the final state.
    notify_kernel_after_udev(disk_path);

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
/// 4KB alignment helper (aligns up to prevent partition overlap)
fn align_to_4k(sector: u64) -> u64 {
    let m = sector % 8;
    if m > 0 { sector + (8 - m) } else { sector }
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
        s.ok_or_else(|| ChoosableError::Generic("No free partition slot available in MBR".to_string()))?
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

    let (cylinder, head, sector) = if start_sector >= 16450560 {
        (1023, 254, 63)
    } else {
        let cylinder = start_sector / 255 / nsector;
        let head = (start_sector / nsector) % 255;
        let sector = (start_sector % nsector) + 1;
        (cylinder, head, sector)
    };

    entry.start_head = head as u8;
    entry.start_sector_cylinder = (((cylinder & 0xFF) as u16) << 8) | (((cylinder & 0x300) >> 2) as u16) | ((sector & 0x3F) as u16);

    let end_lba = start_sector + sector_count.saturating_sub(1);
    let (ecylinder, ehead, esector) = if end_lba >= 16450560 {
        (1023, 254, 63)
    } else {
        let ecylinder = end_lba / 255 / nsector;
        let ehead = (end_lba / nsector) % 255;
        let esector = (end_lba % nsector) + 1;
        (ecylinder, ehead, esector)
    };

    entry.end_head = ehead as u8;
    entry.end_sector_cylinder = (((ecylinder & 0xFF) as u16) << 8) | (((ecylinder & 0x300) >> 2) as u16) | ((esector & 0x3F) as u16);
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
        s.ok_or_else(|| ChoosableError::Generic("No free partition slot available in GPT".to_string()))?
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
                        .arg(partition)
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
        let part_ptr = std::ptr::addr_of!(gpt.partitions) as *const u8;
        let part_bytes = unsafe { std::slice::from_raw_parts(part_ptr, 128 * 128) };
        disk.write_all(part_bytes)?;

        disk.flush()?;
        println!("GPT attributes fixed.");
    }

    Ok(())
}

// ─── Kernel notification ────────────────────────────────────────────────

/// Kernel-side notification only (sync + partx -u).
/// Called BEFORE udev is resumed — no trigger/change events are fired.
fn notify_kernel_before_udev(disk_path: &str) {
    let _ = std::process::Command::new("sync")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();

    // Make sure kernel partition nodes are up-to-date
    let _ = std::process::Command::new("partx").args(&["-u", disk_path])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
}

/// Udev-side notification (trigger + settle).
/// Called AFTER udev is resumed so that udisks2 can pick up the final state.
fn notify_kernel_after_udev(disk_path: &str) {
    // Trigger a "change" event on the parent disk so that udisks2
    // re-reads the partition table and creates D-Bus objects for every
    // partition at once.  Per-partition triggers are NOT used because
    // they race with desktop auto-mount.
    let _ = std::process::Command::new("udevadm")
        .args(&["trigger", "--action=change", "--name-match", disk_path])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();

    // Wait for udev + udisks2 to finish processing
    std::thread::sleep(std::time::Duration::from_secs(3));
    let _ = std::process::Command::new("udevadm").arg("settle")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
}

/// Full kernel notification: sync, trigger, settle, partx -u.
/// Used by code paths that don't split udev stop/start.
fn notify_kernel(disk_path: &str) {
    notify_kernel_before_udev(disk_path);
    notify_kernel_after_udev(disk_path);
}

// ─── Secure Boot ESP processing ─────────────────────────────────────────

pub fn process_secure_boot_esp(disk_path: &str, _part2_start_byte: u64, enable_secure_boot: bool) -> Result<()> {
    if enable_secure_boot { return Ok(()); }

    // Open the EFI partition device directly to avoid stale page-cache data
    // from the raw disk fd (same reason as write_efi_bootloader).
    let part2 = get_partition_name(disk_path, 2);

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&part2)
        .map_err(|e| ChoosableError::Generic(format!("Cannot open {}: {}", part2, e)))?;

    let fs = match fatfs::FileSystem::new(file, fatfs::FsOptions::new()) {
        Ok(fs) => fs,
        Err(_) => return Ok(()),
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

    // NOTE: Do NOT call notify_kernel/notify_kernel_after_udev here.
    // udev exec-queue may be stopped by the caller (install/update/
    // non-destructive paths all use --stop-exec-queue).  Calling
    // udevadm settle while udev is stopped will deadlock.
    //
    // The caller is responsible for re-reading the partition table,
    // resuming udev, and triggering change events after all disk
    // operations are complete.
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

    checks::check_umount_disk(disk_path)?;
    checks::check_swap(disk_path)?;
    checks::check_tool_work_ok(fs_type)?;

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

    // Stop udev while we rewrite the entire disk layout, create partitions,
    // format them and install the bootloader.  If udev fires in the middle
    // of this process it will see half-initialised filesystems and produce
    // "No object for D-bus interface" errors.
    let _ = std::process::Command::new("udevadm")
        .args(&["control", "--stop-exec-queue"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();

    let mut disk = open_disk_readwrite(disk_path)?;

    println!("Cleaning disk...");
    let zero_buf = vec![0u8; 64 * 512];
    disk.seek(SeekFrom::Start(0))?; disk.write_all(&zero_buf)?; disk.flush()?;

    println!("Writing partition table and boot images...");
    let part2_start_sector = if use_gpt {
        write_gpt_f(&mut disk, disk_path, size_bytes, reserve_space_mb, secure_boot)?
    } else {
        write_mbr_f(&mut disk, disk_path, size_bytes, reserve_space_mb, secure_boot)?
    };

    drop(disk);
    let _ = std::process::Command::new("sync")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();

    // Remove stale partition nodes and create new ones while udev is still
    // stopped.  Doing remove+add with a live udev causes udisks2 to see a
    // transient "no partition" state and produce "No object for D-bus
    // interface" errors that survive even a later udevadm trigger.
    checks::remove_partition_nodes(disk_path);
    let _ = std::process::Command::new("partx").args(&["-a", disk_path])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    std::thread::sleep(std::time::Duration::from_millis(500));
    checks::wait_for_partitions(disk_path)?;

    // Format partitions and write EFI bootloader while udev is still
    // stopped.  If udev is running, mkfs.* will fire change events that
    // cause udisks2 to see half-initialised filesystems and produce
    // "No object for D-bus interface" errors.
    let part1 = get_partition_name(disk_path, 1);
    format_partition(&part1, label, fs_type)?;
    format_efi_partition(disk_path, 2)?;

    write_efi_bootloader(disk_path, part2_start_sector)?;

    if !secure_boot {
        // process_secure_boot_esp operates on the already-formatted FAT
        // filesystem — safe while udev is stopped.
        process_secure_boot_esp(disk_path, part2_start_sector, false)?;
    }

    // All disk writes are complete.  Re-read kernel partition table,
    // then resume udev so that udisks2 sees the final consistent state.
    let _ = std::process::Command::new("sync")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    notify_kernel_before_udev(disk_path);

    // Resume udev event processing now.
    let _ = std::process::Command::new("udevadm")
        .args(&["control", "--start-exec-queue"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();

    // Trigger change events and let udisks2 pick up the final state.
    notify_kernel_after_udev(disk_path);

    println!();
    println!("\x1b[32mChoosable installed successfully to {}.\x1b[0m", disk_path);
    Ok(())
}

/// Write MBR partition table, boot code, GUID, and Stage 2 — raw disk only.
/// Returns part2 start offset in bytes.
fn write_mbr_f(disk: &mut std::fs::File, _disk_path: &str, disk_size_bytes: u64, reserve_space_mb: u64, _secure_boot: bool) -> Result<u64> {
    let total_sectors = disk_size_bytes / SECTOR_SIZE;
    let efi_part_sectors = CHOOSABLE_EFI_PART_SIZE / SECTOR_SIZE;
    let part1_start = CHOOSABLE_PART1_START_SECTOR;

    let mut part2_start = if reserve_space_mb > 0 { total_sectors - efi_part_sectors - (reserve_space_mb * 2048) } else { total_sectors - efi_part_sectors };
    part2_start = part2_start - (part2_start % 8);
    let part1_sectors = part2_start - part1_start;

    // ── Phase 1: All raw-disk writes BEFORE partitions exist ──────────
    // Writing to the raw disk while active partition devices exist causes
    // the kernel to fire spurious partition-change events that confuse
    // udev/udisks2, producing "No object for D-bus interface".

    // Build MBR with boot code embedded, then write once.
    let boot_img = bootloader::BOOT_IMG;
    let mut mbr_bytes = [0u8; 512];
    let boot_code_len = std::cmp::min(boot_img.len(), 440);
    mbr_bytes[..boot_code_len].copy_from_slice(&boot_img[..boot_code_len]);

    // GUID at offset 384 (disk signature) and 440 (NT serial-like)
    let guid = generate_guid();
    mbr_bytes[384..400].copy_from_slice(&guid);
    mbr_bytes[440..444].copy_from_slice(&guid[12..16]);

    // MBR signature
    mbr_bytes[510] = MBR_SIGNATURE_55;
    mbr_bytes[511] = MBR_SIGNATURE_AA;

    // Partition table entries at offset 446–509
    let mut mbr = Mbr {
        boot_code: [0u8; 446],
        partitions: [PartitionTableEntry::empty(); 4],
        signature_55: MBR_SIGNATURE_55,
        signature_aa: MBR_SIGNATURE_AA,
    };
    let mut part0 = mbr.partitions[0];
    fill_mbr_chs_entry(&mut part0, disk_size_bytes, part1_start as u32, part1_sectors as u32);
    mbr.partitions[0] = part0;
    mbr.partitions[0].active = PART_ACTIVE;
    mbr.partitions[0].fs_flag = 0x07;
    let mut part1 = mbr.partitions[1];
    fill_mbr_chs_entry(&mut part1, disk_size_bytes, part2_start as u32, efi_part_sectors as u32);
    mbr.partitions[1] = part1;
    mbr.partitions[1].fs_flag = PART_TYPE_EFI_SYSTEM;

    // Serialize only the partition table (not boot_code/signature) into bytes[446..510]
    let partitions_ptr = std::ptr::addr_of!(mbr.partitions) as *const u8;
    let partitions_bytes = unsafe { std::slice::from_raw_parts(partitions_ptr, 64) };
    mbr_bytes[446..510].copy_from_slice(partitions_bytes);

    // Write the complete MBR sector in one shot
    disk.seek(SeekFrom::Start(0))?;
    disk.write_all(&mbr_bytes)?;
    disk.flush()?;

    // Write Stage 2 (core.img) into the post-MBR gap (sectors 1–2047)
    let core = bootloader::STAGE2_BIN;
    let core_len = std::cmp::min(core.len(), 2047 * 512);
    disk.seek(SeekFrom::Start(SECTOR_SIZE))?;
    disk.write_all(&core[..core_len])?;
    disk.flush()?;

    Ok(part2_start * SECTOR_SIZE)
}

/// Write GPT partition table, boot code, GUID, and Stage 2 — raw disk only.
/// Returns part2 start offset in bytes.
fn write_gpt_f(disk: &mut std::fs::File, _disk_path: &str, disk_size_bytes: u64, reserve_space_mb: u64, _secure_boot: bool) -> Result<u64> {
    let efi_part_sectors = CHOOSABLE_EFI_PART_SIZE / SECTOR_SIZE;
    let total_sectors = disk_size_bytes / SECTOR_SIZE;
    let part2_end = total_sectors - 34;
    let mut part2_start = if reserve_space_mb > 0 { part2_end - efi_part_sectors - (reserve_space_mb * 2048) } else { part2_end - efi_part_sectors };
    part2_start = part2_start - (part2_start % 8);
    let part1_end = part2_start - 1;

    let disk_guid = generate_guid();
    let mut gpt_info = GptInfo::new_choosable(disk_size_bytes, disk_guid);
    gpt_info.partitions[0].start_lba = CHOOSABLE_PART1_START_SECTOR;
    gpt_info.partitions[0].end_lba = part1_end;
    gpt_info.partitions[0].attributes = 0;
    gpt_info.partitions[1].start_lba = part2_start;
    gpt_info.partitions[1].end_lba = part2_start + efi_part_sectors - 1;
    gpt_info.partitions[1].attributes = GPT_ATTR_CZBLEFI;

    // ── Phase 1: All raw-disk writes BEFORE partitions exist ──────────
    // Write boot code (440 bytes) + GPT protective MBR
    let boot_img = bootloader::BOOT_IMG;
    let boot_code_len = std::cmp::min(boot_img.len(), 440);
    disk.seek(SeekFrom::Start(0))?;
    disk.write_all(&boot_img[..boot_code_len])?;

    // Write Stage 2 (core.img) into GPT gap (LBA 34–2047)
    disk.seek(SeekFrom::Start(92))?; disk.write_all(&[0x22])?;
    let core = bootloader::STAGE2_BIN;
    let core_len = std::cmp::min(core.len(), 2014 * 512);
    disk.seek(SeekFrom::Start(34 * SECTOR_SIZE))?;
    disk.write_all(&core[..core_len])?;
    disk.seek(SeekFrom::Start(17908))?; disk.write_all(&[0x23])?;

    // Write GUID at offset 384/440
    disk.seek(SeekFrom::Start(384))?; disk.write_all(&disk_guid)?;
    disk.seek(SeekFrom::Start(440))?; disk.write_all(&disk_guid[12..16])?;

    // Finalize CRCs
    finalize_gpt_crcs(&mut gpt_info);

    // Write full GPT (protective MBR + GPT header + partition table).
    // This overwrites the boot code we wrote above (protective MBR has
    // boot_code = [0u8; 446]), so re-write boot code afterwards.
    gpt_info.write_to_disk(disk)?;
    disk.flush()?;

    // Re-write boot code (bytes 0–440) — gpt_info.write_to_disk zeroed it
    disk.seek(SeekFrom::Start(0))?;
    disk.write_all(&boot_img[..boot_code_len])?;
    disk.flush()?;

    Ok(part2_start * SECTOR_SIZE)
}

// ─── Helper utilities ────────────────────────────────────────────────────

fn format_partition(partition: &str, label: &str, fs_type: FilesystemType) -> Result<()> {
    println!("Formatting {} as {}...", partition, fs_type.as_str());

    match fs_type {
        FilesystemType::ExFat => {
            let part_sectors = get_partition_size_sectors(partition)?;
            let part_size_gb = part_sectors * SECTOR_SIZE / SIZE_1GB;
            let cluster_sectors = if part_size_gb > 32 { "256" } else { "64" };

            // Try mkexfatfs first, then mkfs.exfat (including full path) as fallback
            for cmd in &["mkexfatfs", "mkfs.exfat", "/usr/sbin/mkfs.exfat"] {
                let args: Vec<&str> = if *cmd == "mkexfatfs" {
                    vec!["-n", label, "-s", cluster_sectors, partition]
                } else {
                    vec!["-n", label, partition]
                };
                match std::process::Command::new(cmd).args(&args).status() {
                    Ok(s) if s.success() => return Ok(()),
                    Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    _ => continue,
                }
            }
            Err(ChoosableError::FormatFailed)
        }
        FilesystemType::Fat32 => {
            let status = std::process::Command::new("mkfs.vfat")
                .args(&["-n", label, "-F", "32", partition])
                .status()
                .map_err(|e| ChoosableError::ToolNotFound(format!("mkfs.vfat: {}", e)))?;
            if !status.success() { return Err(ChoosableError::FormatFailed); }
            Ok(())
        }
        FilesystemType::Ntfs => {
            let status = std::process::Command::new("mkfs.ntfs")
                .args(&["-f", "-L", label, partition])
                .status()
                .map_err(|e| ChoosableError::ToolNotFound(format!("mkfs.ntfs: {}", e)))?;
            if !status.success() { return Err(ChoosableError::FormatFailed); }
            Ok(())
        }
    }
}

fn write_boot_images(disk: &mut std::fs::File, is_gpt: bool, _part2_start_sector: u64) -> Result<()> {
    println!("Writing boot images...");
    let boot_img = bootloader::BOOT_IMG;
    let boot_code_len = std::cmp::min(boot_img.len(), 440);
    disk.seek(SeekFrom::Start(0))?;
    disk.write_all(&boot_img[..boot_code_len])?;

    let core = bootloader::STAGE2_BIN;
    if is_gpt {
        disk.seek(SeekFrom::Start(92))?; disk.write_all(&[0x22])?;
        let len = std::cmp::min(core.len(), 2014 * 512);
        disk.seek(SeekFrom::Start(34 * 512))?; disk.write_all(&core[..len])?;
        disk.seek(SeekFrom::Start(17908))?; disk.write_all(&[0x23])?;
    } else {
        let len = std::cmp::min(core.len(), 2047 * 512);
        disk.seek(SeekFrom::Start(1 * 512))?; disk.write_all(&core[..len])?;
    }

    disk.flush()?;
    Ok(())
}

fn write_efi_bootloader(disk_path: &str, _part2_start_byte: u64) -> Result<()> {
    // Open the EFI partition device directly (/dev/sdb2 etc.).
    // Opening the raw disk and using PartitionSlice can return stale
    // page-cache data when mkfs.vfat wrote through the partition device.
    let part2 = get_partition_name(disk_path, 2);

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&part2)
        .map_err(|e| ChoosableError::Generic(format!("Cannot open {}: {}", part2, e)))?;

    let fs = fatfs::FileSystem::new(file, fatfs::FsOptions::new())
        .map_err(|e| ChoosableError::Generic(format!("Failed to open EFI FAT on {}: {}", part2, e)))?;

    let root = fs.root_dir();

    // Ensure EFI/BOOT directory exists
    let efi_dir = root.open_dir("EFI")
        .or_else(|_| root.create_dir("EFI"))
        .map_err(|e| ChoosableError::Generic(format!("Failed to create EFI dir: {}", e)))?;

    let boot_dir = efi_dir.open_dir("BOOT")
        .or_else(|_| efi_dir.create_dir("BOOT"))
        .map_err(|e| ChoosableError::Generic(format!("Failed to create BOOT dir: {}", e)))?;

    // Write BOOTX64.EFI
    let efi_bin = bootloader::EFI_BIN;
    let mut file = boot_dir.create_file("BOOTX64.EFI")
        .map_err(|e| ChoosableError::Generic(format!("Failed to create BOOTX64.EFI: {}", e)))?;

    file.write_all(efi_bin)
        .map_err(|e| ChoosableError::Generic(format!("Failed to write EFI binary: {}", e)))?;

    file.flush()
        .map_err(|e| ChoosableError::Generic(format!("Failed to flush EFI file: {}", e)))?;

    Ok(())
}

fn decompress_xz(data: &[u8]) -> Result<Vec<u8>> {
    use std::process::{Command, Stdio};
    use std::io::Write;
    let mut child = Command::new("xzcat")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn()
        .map_err(|_| ChoosableError::ToolNotFound("xzcat".to_string()))?;

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut stdin = child.stdin.take().unwrap();

    let (decoded, err_bytes) = std::thread::scope(|s| {
        s.spawn(move || {
            let _ = stdin.write_all(data);
        });

        let err_thread = s.spawn(move || {
            let mut err = Vec::new();
            let _ = stderr.read_to_end(&mut err);
            err
        });

        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);

        let err = err_thread.join().unwrap_or_default();
        (out, err)
    });

    let status = child.wait().map_err(|e| ChoosableError::Generic(format!("xzcat: {}", e)))?;
    if !status.success() {
        return Err(ChoosableError::Generic(String::from_utf8_lossy(&err_bytes).to_string()));
    }
    Ok(decoded)
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

    // Stop udev while we write boot code, stage2, EFI bootloader and fix
    // GPT attributes.  If udev fires in the middle it will see a
    // half-updated state and produce "No object for D-bus interface" errors.
    let _ = std::process::Command::new("udevadm")
        .args(&["control", "--stop-exec-queue"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();

    let part2_start = part2_start.unwrap();
    let mut disk = open_disk_readwrite(disk_path)?;

    let mut diskuuid = [0u8; 16];
    disk.seek(SeekFrom::Start(384))?; disk.read_exact(&mut diskuuid)?;

    let boot_img = bootloader::BOOT_IMG;
    let boot_code_len = std::cmp::min(boot_img.len(), 440);
    disk.seek(SeekFrom::Start(0))?; disk.write_all(&boot_img[..boot_code_len])?;
    disk.seek(SeekFrom::Start(384))?; disk.write_all(&diskuuid)?;

    let mut rsv_data = vec![0u8; 8 * 512];
    disk.seek(SeekFrom::Start(2040 * 512))?; disk.read_exact(&mut rsv_data)?;

    let is_gpt = mbr.is_gpt_protective();

    if is_gpt {
        disk.seek(SeekFrom::Start(92))?; disk.write_all(&[0x22])?;
        let core = bootloader::STAGE2_BIN;
        let len = std::cmp::min(core.len(), 2014 * 512);
        disk.seek(SeekFrom::Start(34 * 512))?; disk.write_all(&core[..len])?;
        disk.seek(SeekFrom::Start(17908))?; disk.write_all(&[0x23])?;
    } else {
        if mbr.part1_active() == 0x00 && mbr.part2_active() == 0x80 {
            disk.seek(SeekFrom::Start(446))?; disk.write_all(&[PART_ACTIVE])?;
            disk.seek(SeekFrom::Start(462))?; disk.write_all(&[PART_INACTIVE])?;
        }
        let core = bootloader::STAGE2_BIN;
        let len = std::cmp::min(core.len(), 2047 * 512);
        disk.seek(SeekFrom::Start(1 * 512))?; disk.write_all(&core[..len])?;
    }

    disk.seek(SeekFrom::Start(2040 * 512))?; disk.write_all(&rsv_data)?;
    disk.flush()?;
    drop(disk);

    let _ = std::process::Command::new("sync")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();

    write_efi_bootloader(disk_path, part2_start)?;

    if !use_secure_boot {
        // Safe while udev is stopped — operates on the already-formatted
        // FAT filesystem.
        process_secure_boot_esp(disk_path, part2_start, false)?;
    }

    if is_gpt {
        fix_gpt_attributes(disk_path)?;
    }

    // All disk writes are complete.  Re-read kernel partition table,
    // then resume udev so that udisks2 sees the final consistent state.
    let _ = std::process::Command::new("sync")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    notify_kernel_before_udev(disk_path);

    // Resume udev event processing now.
    let _ = std::process::Command::new("udevadm")
        .args(&["control", "--start-exec-queue"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();

    // Trigger change events and let udisks2 pick up the final state.
    notify_kernel_after_udev(disk_path);

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
    let mut file = match std::fs::OpenOptions::new().read(true).open(disk_path) {
        Ok(f) => f, Err(_) => return false,
    };
    if file.seek(SeekFrom::Start(part2_start_byte)).is_err() { return false; }

    let slice = PartitionSlice::new(file, part2_start_byte, CHOOSABLE_EFI_PART_SIZE);

    let fs = match fatfs::FileSystem::new(slice, fatfs::FsOptions::new()) {
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