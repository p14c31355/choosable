// ── Choosable Premount Init ──────────────────────────────────────────────
//
//  Statically-linked /init.choosable (musl target).
//  Replaces /init early in boot via init=/init.choosable.
//  Uses raw syscalls — no external binaries required.
//  Mounts /proc,/sys,/dev, reads kernel cmdline for choosable.* params,
//  locates the ISO by PARTUUID (preferred) or partition number + offset,
//  loopback-mounts it, then exec's the real /init.

use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;

const MS_RDONLY: libc::c_ulong = 1;
const MS_BIND: libc::c_ulong = 0x1000;

#[cfg(target_env = "musl")]
type IoctlRequest = libc::c_int;
#[cfg(not(target_env = "musl"))]
type IoctlRequest = libc::c_ulong;

const LOOP_SET_FD: IoctlRequest = 0x4C00;
const LOOP_CLR_FD: IoctlRequest = 0x4C01;
const LOOP_SET_STATUS64: IoctlRequest = 0x4C04;
const MAX_OFFSET: u64 = 512 * 4_294_967_296;
const LO_FLAGS_READ_ONLY: u32 = 1;

#[repr(C)]
struct LoopInfo64 {
    lo_device: u64,
    lo_inode: u64,
    lo_rdevice: u64,
    lo_offset: u64,
    lo_sizelimit: u64,
    lo_number: u32,
    lo_encrypt_type: u32,
    lo_encrypt_key_size: u32,
    lo_flags: u32,
    lo_file_name: [u8; 64],
    lo_crypt_name: [u8; 64],
    lo_encrypt_key: [u8; 32],
    lo_init: [u64; 2],
}

// ── Console logging ────────────────────────────────────────────────────
fn console_log(s: &str) {
    if let Ok(mut f) = fs::OpenOptions::new().append(true).open("/dev/console") {
        use std::io::Write;
        let _ = write!(f, "[choosable] {}\n", s);
    }
}

// ── Kernel cmdline parser ──────────────────────────────────────────────
#[derive(Default)]
struct CmdlineParams {
    iso_offset: Option<u64>,
    part_guid: Option<String>,
    part_num: Option<u32>,
    iso_path: Option<String>,
    iso_size: Option<u64>,
}

fn parse_cmdline() -> CmdlineParams {
    let mut params = CmdlineParams::default();
    let Ok(mut f) = fs::File::open("/proc/cmdline") else {
        console_log("cannot open /proc/cmdline");
        return params;
    };
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        console_log("cannot read /proc/cmdline");
        return params;
    }

    // Parse space-separated key=value pairs
    let mut i = 0;
    while i < buf.len() {
        // Skip leading whitespace
        while i < buf.len() && (buf[i] == b' ' || buf[i] == b'\t' || buf[i] == b'\n') { i += 1; }
        if i >= buf.len() { break; }

        // Find the end of this token
        let token_start = i;
        while i < buf.len() && buf[i] != b' ' && buf[i] != b'\t' && buf[i] != b'\n' { i += 1; }
        let token = &buf[token_start..i];

        // Split on '=' to get key/value
        if let Some(eq_pos) = token.iter().position(|&b| b == b'=') {
            let key = &token[..eq_pos];
            let val = &token[eq_pos + 1..];

            match key {
                b"choosable.iso_offset" => {
                    let mut v: u64 = 0;
                    for &b in val {
                        if b < b'0' || b > b'9' { v = 0; break; }
                        v = v.saturating_mul(10).saturating_add((b - b'0') as u64);
                    }
                    if v > 0 && v <= MAX_OFFSET { params.iso_offset = Some(v); }
                }
                b"choosable.part_guid" => {
                    if let Ok(s) = std::str::from_utf8(val) {
                        let s = s.to_lowercase();
                        if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                            params.part_guid = Some(s);
                        }
                    }
                }
                b"choosable.part_num" => {
                    let mut v: u32 = 0;
                    for &b in val {
                        if b < b'0' || b > b'9' { v = 0; break; }
                        v = v.saturating_mul(10).saturating_add((b - b'0') as u32);
                    }
                    if v > 0 { params.part_num = Some(v); }
                }
                b"choosable.iso_path" => {
                    if let Ok(s) = std::str::from_utf8(val) { params.iso_path = Some(s.to_string()); }
                }
                b"choosable.iso_size" => {
                    let mut v: u64 = 0;
                    for &b in val {
                        if b < b'0' || b > b'9' { v = 0; break; }
                        v = v.saturating_mul(10).saturating_add((b - b'0') as u64);
                    }
                    if v > 0 { params.iso_size = Some(v); }
                }
                _ => {}
            }
        }
    }

    console_log(&format!("cmdline: offset={:?} guid={:?} num={:?} path={:?} size={:?}",
        params.iso_offset, params.part_guid.as_deref(), params.part_num, params.iso_path.as_deref(), params.iso_size));
    params
}

// ── Raw syscall wrappers ────────────────────────────────────────────────
unsafe fn do_mount(src: &str, tgt: &str, fstype: &str, flags: libc::c_ulong) -> i32 {
    let s = std::ffi::CString::new(src).unwrap();
    let t = std::ffi::CString::new(tgt).unwrap();
    let f = std::ffi::CString::new(fstype).unwrap();
    libc::mount(s.as_ptr(), t.as_ptr(), f.as_ptr(), flags, std::ptr::null())
}

unsafe fn do_mkdir(path: &str) {
    let p = std::ffi::CString::new(path).unwrap();
    libc::mkdir(p.as_ptr(), 0o755);
}

unsafe fn do_mknod_blk(path: &str, maj: u32, min: u32) {
    let p = std::ffi::CString::new(path).unwrap();
    libc::mknod(p.as_ptr(), libc::S_IFBLK | 0o600, libc::makedev(maj, min));
}

unsafe fn do_execve(path: &str) -> ! {
    let p = std::ffi::CString::new(path).unwrap();
    let a: [*const libc::c_char; 2] = [p.as_ptr(), std::ptr::null()];
    let e: [*const libc::c_char; 1] = [std::ptr::null()];
    libc::execve(p.as_ptr(), a.as_ptr(), e.as_ptr());
    loop { std::hint::spin_loop() }
}

// ── Distro detection ────────────────────────────────────────────────────
fn check_distro() -> bool {
    if std::path::Path::new("/cdrom/casper").is_dir() {
        console_log("distro: casper/pop");
        return true;
    }
    if std::path::Path::new("/cdrom/live").is_dir() {
        console_log("distro: debian-live");
        return true;
    }
    if std::path::Path::new("/cdrom/LiveOS").is_dir() {
        console_log("distro: LiveOS (Fedora)");
        unsafe {
            do_mkdir("/run/initramfs");
            do_mkdir("/run/initramfs/live");
            do_mkdir("/run/initramfs/live/LiveOS");
            do_mount("/cdrom/LiveOS", "/run/initramfs/live/LiveOS", "", MS_BIND);
        }
        return true;
    }
    if std::path::Path::new("/cdrom/arch").is_dir() {
        console_log("distro: archiso");
        unsafe {
            do_mkdir("/run/archiso");
            do_mkdir("/run/archiso/bootmnt");
            do_mount("/cdrom", "/run/archiso/bootmnt", "", MS_BIND);
        }
        return true;
    }
    if std::path::Path::new("/cdrom/.alpine-release").exists()
        || std::path::Path::new("/cdrom/apks").is_dir()
    {
        console_log("distro: alpine");
        return true;
    }
    console_log("distro: unknown, mount anyway");
    true
}

// ── Partition lookup ────────────────────────────────────────────────────

/// Find target partition by PARTUUID. Falls back to scanning /proc/partitions + /sys.
fn by_partuuid(guid: &str) -> Option<String> {
    // First try /dev/disk/by-partuuid/ (requires udev/sysfs)
    let by_partuuid = format!("/dev/disk/by-partuuid/{}", guid);
    if std::path::Path::new(&by_partuuid).exists() {
        if let Ok(target) = fs::read_link(&by_partuuid) {
            let dev = if target.is_absolute() { target } else {
                std::path::Path::new("/dev/disk/by-partuuid").join(target)
            };
            let dev_str = dev.to_string_lossy().to_string();
            console_log(&format!("PARTUUID match: {} -> {}", by_partuuid, dev_str));
            return Some(dev_str);
        }
        console_log(&format!("PARTUUID path exists but read_link failed: {}", by_partuuid));
        return Some(by_partuuid);
    }

    // Fallback: scan /sys/block to find partition by GUID
    // Each partition has /sys/block/<dev>/<dev>/partition and optional
    // /sys/block/<dev>/<dev>/uuid (GPT partition UUID)
    console_log(&format!("PARTUUID {} not found via udev, scanning sysfs", guid));
    let guid_upper = guid.to_uppercase();
    let Ok(blocks) = fs::read_dir("/sys/block") else { return None };
    for block in blocks {
        let Ok(block) = block else { continue };
        let block_name = block.file_name();
        let Ok(parts) = fs::read_dir(block.path()) else { continue };
        for part in parts {
            let Ok(part) = part else { continue };
            let part_name = part.file_name();
            let part_name_s = part_name.to_string_lossy();
            // Each partition has a "partition" file and optionally "uuid" (GPT partition GUID)
            let uuid_path = part.path().join("uuid");
            if !uuid_path.exists() { continue; }
            if let Ok(uuid) = fs::read_to_string(&uuid_path) {
                let uuid = uuid.trim().to_uppercase();
                if uuid == guid_upper {
                    let dev = format!("/dev/{}", part_name_s);
                    console_log(&format!("PARTUUID match via sysfs: {} -> {}", uuid_path.display(), dev));
                    return Some(dev);
                }
            }
        }
    }
    console_log("PARTUUID not found via sysfs either");
    None
}

/// Fallback: find partition by 1-based number on a specific disk.
/// This tries to match partition N on disks that have partitions (sda1, sda2, etc).
/// It does NOT use a global /proc/partitions index.
fn by_partnum(target_num: u32) -> Option<String> {
    // Scan /sys/block for disks that have partitions
    let Ok(blocks) = fs::read_dir("/sys/block") else { return None };
    for block in blocks {
        let Ok(block) = block else { continue };
        let block_name = block.file_name();
        let block_name_s = block_name.to_string_lossy();

        // Skip loop, ram, dm, sr devices
        if block_name_s.starts_with("loop") || block_name_s.starts_with("ram")
            || block_name_s.starts_with("dm") || block_name_s.starts_with("sr")
        { continue; }

        // Try to find the Nth partition on this disk
        let partition_name = if block_name_s.ends_with(|c: char| c.is_numeric()) {
            // For nvme0n1, mmc0, etc: partition is nvme0n1p1, mmc0p1
            format!("{}p{}", block_name_s, target_num)
        } else {
            // For sda, hda, vda: partition is sda1, hda1, vda1
            format!("{}{}", block_name_s, target_num)
        };

        let dev_path = format!("/dev/{}", partition_name);
        if std::path::Path::new(&dev_path).exists() {
            console_log(&format!("by_partnum: found {} as partition {} on disk {}", dev_path, target_num, block_name_s));
            return Some(dev_path);
        }
    }
    None
}

/// Scan all partitions (legacy fallback).
fn scan_all(offset: u64) {
    let Ok(data) = fs::read_to_string("/proc/partitions") else {
        console_log("cannot read /proc/partitions");
        return;
    };
    for line in data.lines().skip(2) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 { continue; }
        let name = cols[3];
        if name.starts_with("loop") || name.starts_with("ram")
            || name.starts_with("dm") || name.starts_with("sr")
        { continue; }
        let dev = format!("/dev/{}", name);
        if let Some(loop_path) = try_mount_iso(&dev, offset) {
            if check_distro() {
                console_log("success (fallback scan)");
                unsafe { do_execve("/init"); }
            } else {
                unsafe {
                    libc::umount2(b"/cdrom\0".as_ptr() as *const i8, libc::MNT_DETACH);
                }
                if let Ok(lf) = fs::OpenOptions::new().read(true).write(true).open(&loop_path) {
                    let lfd = lf.as_raw_fd();
                    unsafe { libc::ioctl(lfd, LOOP_CLR_FD, 0) };
                }
            }
        }
    }
}

// ── Loop + mount ───────────────────────────────────────────────────────

/// Try to loopback-mount the ISO at the given partition + offset.
/// Returns the loop device path on success, or None.
fn try_mount_iso(dev_path: &str, offset: u64) -> Option<String> {
    let Ok(df) = fs::OpenOptions::new().read(true).write(true).open(dev_path) else { return None };
    let dfd = df.as_raw_fd();

    let mut loop_dev = None;
    for i in 0i32..8 {
        let lp = format!("/dev/loop{}", i);
        if let Ok(lf) = fs::OpenOptions::new().read(true).write(true).open(&lp) {
            let lfd = lf.as_raw_fd();
            if unsafe { libc::ioctl(lfd, LOOP_SET_FD, dfd) } == 0 {
                loop_dev = Some((lp, lf));
                break;
            }
        }
    }
    drop(df);

    let Some((loop_path, lf)) = loop_dev else { return None };
    let lfd = lf.as_raw_fd();

    if offset > 0 {
        let mut info: LoopInfo64 = unsafe { core::mem::zeroed() };
        info.lo_offset = offset;
        info.lo_flags = LO_FLAGS_READ_ONLY;
        if unsafe { libc::ioctl(lfd, LOOP_SET_STATUS64, &info as *const LoopInfo64) } != 0 {
            unsafe { libc::ioctl(lfd, LOOP_CLR_FD, 0) };
            return None;
        }
    }

    let ok = unsafe { do_mount(&loop_path, "/cdrom", "iso9660", MS_RDONLY) == 0 };
    if !ok {
        unsafe { libc::ioctl(lfd, LOOP_CLR_FD, 0) };
        return None;
    }

    console_log(&format!("mounted {} on /cdrom (offset={})", loop_path, offset));
    Some(loop_path)
}

// ── Main logic ──────────────────────────────────────────────────────────

fn main() {
    console_log("premount started");

    // 1. Mount essential filesystems
    unsafe {
        do_mkdir("/proc");
        do_mount("proc", "/proc", "proc", 0);
        do_mkdir("/sys");
        do_mount("sysfs", "/sys", "sysfs", 0);
        do_mkdir("/dev");
        do_mount("devtmpfs", "/dev", "devtmpfs", 0);
        do_mkdir("/tmp");
        do_mkdir("/cdrom");
        do_mkdir("/run");
        for i in 0u32..8 {
            do_mknod_blk(&format!("/dev/loop{}", i), 7, i);
        }
    }

    // 2. Parse kernel cmdline
    let params = parse_cmdline();
    let offset = params.iso_offset.unwrap_or(0);

    // 3. Wait for devices
    std::thread::sleep(std::time::Duration::from_secs(3));

    // 4. Target partition via PARTUUID (preferred) or partition number
    let target: Option<String> = params.part_guid.as_ref().and_then(|g| by_partuuid(g))
        .or_else(|| params.part_num.and_then(|n| by_partnum(n)));

    let mounted = match target {
        Some(ref dev) => {
            console_log(&format!("target: {}", dev));
            if let Some(loop_path) = try_mount_iso(dev, offset) {
                if check_distro() {
                    true
                } else {
                    unsafe {
                        libc::umount2(b"/cdrom\0".as_ptr() as *const i8, libc::MNT_DETACH);
                    }
                    if let Ok(lf) = fs::OpenOptions::new().read(true).write(true).open(&loop_path) {
                        let lfd = lf.as_raw_fd();
                        unsafe { libc::ioctl(lfd, LOOP_CLR_FD, 0) };
                    }
                    console_log("target mount/check failed, falling back to scan_all");
                    scan_all(offset);
                    false
                }
            } else {
                console_log("target mount failed, falling back to scan_all");
                scan_all(offset);
                false
            }
        }
        None => {
            console_log("no PARTUUID/partnum, scanning all partitions");
            scan_all(offset);
            false
        }
    };

    if mounted {
        console_log("success, exec /init");
    } else {
        console_log("fail, exec /init (fallback)");
    }
    unsafe { do_execve("/init"); }
}
