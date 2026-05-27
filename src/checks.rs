use crate::disk::{get_partition_name, is_whole_disk};
use crate::error::{ChoosableError, Result};
use std::path::Path;

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
                if dev.starts_with(disk_path) {
                    return Err(ChoosableError::Generic(format!(
                        "{} is used as swap, please swapoff it first!", disk_path
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
            if line.starts_with(disk_path) {
                if let Some(mount_point) = line.split_whitespace().nth(1) {
                    println!("Unmounting {} (was mounted at {})...", disk_path, mount_point);
                    let _ = std::process::Command::new("umount")
                        .arg(mount_point)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
        }
    }

    // Verify no mounts remain
    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        if mounts.lines().any(|l| l.starts_with(disk_path)) {
            return Err(ChoosableError::Generic(format!(
                "{} is still mounted, please unmount it first!", disk_path
            )));
        }
    }

    Ok(())
}

/// Check that required tools work
pub fn check_tool_work_ok() -> Result<()> {
    // Check hexdump
    let child = std::process::Command::new("echo")
        .arg("1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|_| ChoosableError::ToolNotFound("echo".to_string()))?;

    let hexdump = std::process::Command::new("hexdump")
        .stdin(child.stdout.unwrap())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match hexdump {
        Ok(s) if s.success() => {}
        _ => {
            return Err(ChoosableError::ToolNotFound(
                "hexdump".to_string()
            ));
        }
    }

    // Check mkexfatfs
    let status = std::process::Command::new("mkexfatfs")
        .arg("-V")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|_| ChoosableError::ToolNotFound("mkexfatfs".to_string()))?;

    if !status.success() {
        return Err(ChoosableError::ToolNotFound(
            "mkexfatfs does not work on this system".to_string()
        ));
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
            return Err(ChoosableError::ToolNotFound(
                "xzcat".to_string()
            ));
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
        println!("Waiting for partitions {} and {} ... (attempt {})", part1, part2, i + 1);
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

    // Create device nodes if they still don't exist (mknod fallback)
    if !Path::new(&part1).exists() {
        if let Some(major_minor) = read_dev_major_minor(&part1) {
            println!("Creating device node {} with mknod...", part1);
            let _ = std::process::Command::new("mknod")
                .args(&["-m", "0660", &part1, "b", &major_minor.0.to_string(), &major_minor.1.to_string()])
                .status();
        }
    }

    if !Path::new(&part2).exists() {
        if let Some(major_minor) = read_dev_major_minor(&part2) {
            println!("Creating device node {} with mknod...", part2);
            let _ = std::process::Command::new("mknod")
                .args(&["-m", "0660", &part2, "b", &major_minor.0.to_string(), &major_minor.1.to_string()])
                .status();
        }
    }

    if Path::new(&part1).exists() && Path::new(&part2).exists() {
        Ok(())
    } else {
        Err(ChoosableError::Generic(format!(
            "Partitions {} / {} do not exist after waiting", part1, part2
        )))
    }
}

/// Read major:minor from /sys/class/block/{dev}/dev
fn read_dev_major_minor(part_path: &str) -> Option<(u32, u32)> {
    let name = part_path.trim_start_matches("/dev/");
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

/// Delete existing partition device nodes (equivalent to `rm -f $PART1 $PART2`)
pub fn remove_partition_nodes(disk_path: &str) {
    let part1 = get_partition_name(disk_path, 1);
    let part2 = get_partition_name(disk_path, 2);

    if Path::new(&part1).exists() {
        println!("Removing existing partition node {}", part1);
        let _ = std::fs::remove_file(&part1);
    }
    if Path::new(&part2).exists() {
        println!("Removing existing partition node {}", part2);
        let _ = std::fs::remove_file(&part2);
    }
}