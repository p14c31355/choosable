use crate::disk::{get_partition_name, is_whole_disk};
use crate::error::{ChoosableError, Result};
use crate::installer::FilesystemType;
use std::path::Path;

/// Returns true if `dev` is a partition of `disk_path`.
/// Handles NVMe (/dev/nvme0n1 → /dev/nvme0n1p1) and standard (/dev/sda → /dev/sda1) naming.
fn is_partition_of(dev: &str, disk_path: &str) -> bool {
    if let Some(suffix) = dev.strip_prefix(disk_path) {
        if suffix.is_empty() {
            return false; // exact match handled by caller
        }
        let disk_ends_with_digit = disk_path
            .chars()
            .last()
            .map_or(false, |c| c.is_ascii_digit());
        if disk_ends_with_digit {
            // NVMe / MMC naming: partition suffix must be 'p' followed by digit(s)
            if let Some(part_suffix) = suffix.strip_prefix('p') {
                part_suffix
                    .chars()
                    .next()
                    .map_or(false, |c| c.is_ascii_digit())
            } else {
                false
            }
        } else {
            // Standard naming: partition suffix must start with a digit
            suffix.chars().next().map_or(false, |c| c.is_ascii_digit())
        }
    } else {
        false
    }
}

/// Check if disk or any of its partitions is used as swap
pub fn check_swap(disk_path: &str) -> Result<()> {
    // Check with swapon
    if let Ok(output) = std::process::Command::new("swapon")
        .arg("--show")
        .arg("--noheadings")
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if let Some(dev) = line.split_whitespace().next() {
                let is_sub_dev = dev == disk_path || is_partition_of(dev, disk_path);
                if is_sub_dev {
                    return Err(ChoosableError::Generic(format!(
                        "{} is used as swap, please swapoff it first!",
                        disk_path
                    )));
                }
            }
        }
    }

    Ok(())
}

/// Unmount all partitions belonging to a disk
pub fn check_umount_disk(disk_path: &str) -> Result<()> {
    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        for line in mounts.lines() {
            if let Some(dev) = line.split_whitespace().next() {
                let is_sub_dev = dev == disk_path || is_partition_of(dev, disk_path);
                if is_sub_dev {
                    if let Some(mount_point) = line.split_whitespace().nth(1) {
                        println!("Unmounting {} (was mounted at {})...", dev, mount_point);
                        let _ = std::process::Command::new("umount")
                            .arg(dev)
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status();
                    }
                }
            }
        }
    }

    // Verify no mounts remain
    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        for line in mounts.lines() {
            if let Some(dev) = line.split_whitespace().next() {
                let is_sub_dev = dev == disk_path || is_partition_of(dev, disk_path);
                if is_sub_dev {
                    return Err(ChoosableError::Generic(format!(
                        "{} is still mounted, please unmount it first!",
                        disk_path
                    )));
                }
            }
        }
    }

    Ok(())
}

/// Check that required tools work for the given filesystem type
pub fn check_tool_work_ok(fs_type: FilesystemType) -> Result<()> {
    // Check hexdump
    let hexdump = std::process::Command::new("hexdump")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match hexdump {
        Ok(s) if s.success() => {}
        _ => {
            return Err(ChoosableError::ToolNotFound("hexdump".to_string()));
        }
    }

    // Check filesystem-specific formatting tool
    match fs_type {
        FilesystemType::ExFat => {
            // Try mkexfatfs first, then mkfs.exfat as fallback
            let tools = [
                ("mkexfatfs", true),   // uses -V
                ("mkfs.exfat", false), // -V exits with code 1, just check existence
                ("/usr/sbin/mkfs.exfat", false),
            ];
            let mut found = false;
            for (tool_name, use_version_flag) in &tools {
                let mut cmd = std::process::Command::new(tool_name);
                cmd.stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                if *use_version_flag {
                    cmd.arg("-V");
                }
                match cmd.status() {
                    Ok(_) => {
                        // mkfs.exfat -V exits with code 1 on success,
                        // so just check that the command exists and runs
                        found = true;
                        break;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    _ => continue,
                }
            }
            if !found {
                return Err(ChoosableError::ToolNotFound(
                    "mkexfatfs or mkfs.exfat is required for exFAT formatting".to_string(),
                ));
            }
        }
        FilesystemType::Fat32 => {
            match std::process::Command::new("mkfs.vfat")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
            {
                Ok(_) => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(ChoosableError::ToolNotFound("mkfs.vfat".to_string()));
                }
                Err(e) => {
                    return Err(ChoosableError::Generic(format!(
                        "Failed to run mkfs.vfat: {}",
                        e
                    )));
                }
            }
        }
        FilesystemType::Ntfs => {
            let status = std::process::Command::new("mkfs.ntfs")
                .arg("-V")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map_err(|_| ChoosableError::ToolNotFound("mkfs.ntfs".to_string()))?;
            if !status.success() {
                return Err(ChoosableError::ToolNotFound(
                    "mkfs.ntfs does not work on this system".to_string(),
                ));
            }
        }
    }

    // Check xzcat
    let status = std::process::Command::new("xzcat")
        .arg("-V")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match status {
        Ok(s) if s.success() => {}
        _ => {
            return Err(ChoosableError::ToolNotFound("xzcat".to_string()));
        }
    }

    Ok(())
}

/// Wait for partition devices to appear (poll sysfs until device nodes exist)
pub fn wait_for_partitions(disk_path: &str) -> Result<()> {
    let part1 = get_partition_name(disk_path, 1);
    let part2 = get_partition_name(disk_path, 2);

    for i in 0..10 {
        if Path::new(&part1).exists() && Path::new(&part2).exists() {
            return Ok(());
        }
        println!(
            "Waiting for partitions {} and {} ... (attempt {})",
            part1,
            part2,
            i + 1
        );
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Try to probe partitions
        if i == 2 {
            let _ = std::process::Command::new("partprobe")
                .arg(disk_path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    // Use partx to add partitions.  `partx -a` works with the kernel directly,
    // so it is safe even while udev exec-queue is stopped.
    // Do NOT call `udevadm settle` here — the caller may have stopped udev.
    println!("Adding partitions for {} via partx...", disk_path);
    let _ = std::process::Command::new("partx")
        .args(&["-a", disk_path])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Poll for device nodes — partx adds them synchronously, so they should
    // appear almost immediately.
    std::thread::sleep(std::time::Duration::from_millis(500));

    if Path::new(&part1).exists() && Path::new(&part2).exists() {
        Ok(())
    } else {
        Err(ChoosableError::Generic(format!(
            "Partitions {} / {} do not exist after waiting",
            part1, part2
        )))
    }
}

/// Read major:minor from /sys/class/block/{dev}/dev
fn read_dev_major_minor(part_path: &str) -> Option<(u32, u32)> {
    let name = part_path.strip_prefix("/dev/").unwrap_or(part_path);
    let dev_file = format!("/sys/class/block/{}/dev", name);
    if let Ok(contents) = std::fs::read_to_string(&dev_file) {
        let parts: Vec<&str> = contents.trim().split(':').collect();
        if parts.len() == 2 {
            let major = parts[0].parse::<u32>().ok()?;
            let minor = parts[1].parse::<u32>().ok()?;
            return Some((major, minor));
        }
    }
    None
}

/// Delete existing partition device nodes using partx.
///
/// `rm -f /dev/sdX1` bypasses udev and causes "No object for D-bus interface"
/// errors in udisks2.  `partx -d` properly tells the kernel to remove partition
/// nodes.
///
/// NOTE: This function must NOT call `udevadm settle` — it is called while
/// udev exec-queue is stopped (`--stop-exec-queue`).  `partx -d` works
/// directly with the kernel and does not require udev.
pub fn remove_partition_nodes(disk_path: &str) {
    let part1 = get_partition_name(disk_path, 1);
    let part2 = get_partition_name(disk_path, 2);

    if Path::new(&part1).exists() || Path::new(&part2).exists() {
        println!(
            "Removing existing partition nodes for {} via partx...",
            disk_path
        );
        let _ = std::process::Command::new("partx")
            .args(&["-d", disk_path])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        // Brief sleep to let the kernel finish removing nodes
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}
