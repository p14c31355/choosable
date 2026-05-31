use crate::disk::get_partition_name;
use crate::error::{ChoosableError, Result};
use crate::installer::FilesystemType;
use std::path::Path;

/// Returns true if `dev` is a partition of `disk_path`.
/// Handles NVMe (/dev/nvme0n1 → /dev/nvme0n1p1) and standard (/dev/sda → /dev/sda1) naming.
pub(crate) fn is_partition_of(dev: &str, disk_path: &str) -> bool {
    dev.strip_prefix(disk_path)
        .map_or(false, |suffix| {
            if suffix.is_empty() {
                return false;
            }
            disk_path
                .chars()
                .last()
                .map_or(false, |c| c.is_ascii_digit())
                .then(|| suffix.strip_prefix('p').map_or(false, |s| s.starts_with(|c: char| c.is_ascii_digit())))
                .unwrap_or_else(|| suffix.starts_with(|c: char| c.is_ascii_digit()))
        })
}

/// Parse /proc/mounts and return an iterator of (device, mount_point)
fn parse_mounts() -> impl Iterator<Item = (String, String)> {
    std::fs::read_to_string("/proc/mounts")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let dev = parts.next()?.to_owned();
            let mount = parts.next()?.to_owned();
            Some((dev, mount))
        })
        .collect::<Vec<_>>()
        .into_iter()
}

/// Check if disk or any of its partitions is used as swap
pub fn check_swap(disk_path: &str) -> Result<()> {
    if let Ok(output) = std::process::Command::new("swapon")
        .arg("--show")
        .arg("--noheadings")
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.lines().any(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|dev| dev == disk_path || is_partition_of(dev, disk_path))
        }) {
            return Err(ChoosableError::Generic(format!(
                "{} is used as swap, please swapoff it first!",
                disk_path
            )));
        }
    }
    Ok(())
}

/// Unmount all partitions belonging to a disk
pub fn check_umount_disk(disk_path: &str) -> Result<()> {
    for (dev, mount_point) in parse_mounts() {
        if dev == disk_path || is_partition_of(&dev, disk_path) {
            println!("Unmounting {} (was mounted at {})...", dev, mount_point);
            let _ = std::process::Command::new("umount")
                .arg(&dev)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    // Verify no mounts remain
    if parse_mounts().any(|(dev, _)| dev == disk_path || is_partition_of(&dev, disk_path)) {
        return Err(ChoosableError::Generic(format!(
            "{} is still mounted, please unmount it first!",
            disk_path
        )));
    }
    Ok(())
}

/// Check that required tools work for the given filesystem type
pub fn check_tool_work_ok(fs_type: FilesystemType) -> Result<()> {
    check_tool("hexdump", None)?;

    let tool_candidates: &[(&[&str], &str)] = match fs_type {
        FilesystemType::ExFat => &[
            (&["mkexfatfs", "-V"], "mkexfatfs or mkfs.exfat is required for exFAT formatting"),
            (&["mkfs.exfat"], "mkexfatfs or mkfs.exfat is required for exFAT formatting"),
            (&["/usr/sbin/mkfs.exfat"], "mkexfatfs or mkfs.exfat is required for exFAT formatting"),
        ],
        FilesystemType::Fat32 => &[(&["mkfs.vfat"], "mkfs.vfat")],
        FilesystemType::Ntfs => &[(&["mkfs.ntfs", "-V"], "mkfs.ntfs does not work on this system")],
    };
    if !tool_candidates.iter().any(|(args, _)| tool_exists(args)) {
        return Err(ChoosableError::ToolNotFound(tool_candidates[0].1.to_string()));
    }

    check_tool("xzcat", Some("-V"))?;
    Ok(())
}

fn tool_exists(args: &[&str]) -> bool {
    let (cmd, rest) = args.split_first().unwrap();
    std::process::Command::new(cmd)
        .args(rest)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn check_tool(cmd: &str, arg: Option<&str>) -> Result<()> {
    let mut c = std::process::Command::new(cmd);
    c.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(a) = arg {
        c.arg(a);
    }
    c.status()
        .map(|s| s.success())
        .unwrap_or(false)
        .then_some(())
        .ok_or_else(|| ChoosableError::ToolNotFound(cmd.to_string()))
}

/// Wait for partition devices to appear (poll sysfs until device nodes exist)
pub fn wait_for_partitions(disk_path: &str) -> Result<()> {
    let part1 = get_partition_name(disk_path, 1);
    let part2 = get_partition_name(disk_path, 2);

    for i in 0..10 {
        if Path::new(&part1).exists() && Path::new(&part2).exists() {
            return Ok(());
        }
        println!("Waiting for partitions {} and {} ... (attempt {})", part1, part2, i + 1);
        std::thread::sleep(std::time::Duration::from_secs(1));

        if i == 2 {
            let _ = std::process::Command::new("partprobe")
                .arg(disk_path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    println!("Adding partitions for {} via partx...", disk_path);
    let _ = std::process::Command::new("partx")
        .args(&["-a", disk_path])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

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

/// Delete existing partition device nodes using partx.
pub fn remove_partition_nodes(disk_path: &str) {
    let part1 = get_partition_name(disk_path, 1);
    let part2 = get_partition_name(disk_path, 2);

    if Path::new(&part1).exists() || Path::new(&part2).exists() {
        println!("Removing existing partition nodes for {} via partx...", disk_path);
        let _ = std::process::Command::new("partx")
            .args(&["-d", disk_path])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_partition_of_standard() {
        assert!(is_partition_of("/dev/sda1", "/dev/sda"));
        assert!(is_partition_of("/dev/sda12", "/dev/sda"));
        assert!(!is_partition_of("/dev/sda", "/dev/sda"));
        assert!(!is_partition_of("/dev/sdb1", "/dev/sda"));
    }

    #[test]
    fn test_is_partition_of_nvme() {
        assert!(is_partition_of("/dev/nvme0n1p1", "/dev/nvme0n1"));
        assert!(is_partition_of("/dev/nvme0n1p12", "/dev/nvme0n1"));
        assert!(!is_partition_of("/dev/nvme0n1", "/dev/nvme0n1"));
        assert!(!is_partition_of("/dev/nvme0n1p1", "/dev/nvme1n1"));
    }
}