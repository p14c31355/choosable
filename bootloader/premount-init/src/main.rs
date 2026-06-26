// ── Choosable Premount Init ──────────────────────────────────────────────
//
//  Statically-linked /init.choosable (musl target).
//  Replaces /init early in boot via init=/init.choosable.
//  Uses raw syscalls — no external binaries required.
//  Mounts /proc,/sys,/dev, scans /proc/partitions, loopback-mounts
//  an ISO using choosable.iso_offset= from the kernel cmdline,
//  then exec's the real /init.

use std::fs;
use std::io::Read;

const MS_RDONLY: libc::c_ulong = 1;
const MS_BIND: libc::c_ulong = 0x1000;
const LOOP_SET_FD: i32 = 0x4C00u32 as i32;
const LOOP_CLR_FD: i32 = 0x4C01u32 as i32;
const MAX_OFFSET: u64 = 512 * 4_294_967_296;

// ── Console logging ────────────────────────────────────────────────────
fn console_log(s: &str) {
    if let Ok(mut f) = fs::OpenOptions::new().append(true).open("/dev/console") {
        use std::io::Write;
        let _ = write!(f, "premount: {}\n", s);
    }
}

// ── Cmdline parser ──────────────────────────────────────────────────────
fn read_offset() -> u64 {
    let Ok(mut f) = fs::File::open("/proc/cmdline") else { return 0 };
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() { return 0 }
    let marker = b"choosable.iso_offset=";
    if let Some(pos) = buf.windows(marker.len()).position(|w| w == marker) {
        let s = pos + marker.len();
        let mut e = s;
        while e < buf.len() && buf[e] != b' ' && buf[e] != b'\n' && buf[e] != 0 { e += 1 }
        let mut v: u64 = 0;
        for &b in &buf[s..e] {
            if b < b'0' || b > b'9' { return 0 }
            v = v.saturating_mul(10).saturating_add((b - b'0') as u64);
        }
        if v > 0 && v <= MAX_OFFSET { return v }
    }
    0
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
        console_log("casper/pop detected");
        return true;
    }
    if std::path::Path::new("/cdrom/live").is_dir() {
        console_log("debian-live detected");
        return true;
    }
    if std::path::Path::new("/cdrom/LiveOS").is_dir() {
        console_log("LiveOS (Fedora) detected");
        unsafe {
            do_mkdir("/run/initramfs/live");
            do_mount("/cdrom/LiveOS", "/run/initramfs/live/LiveOS", "", MS_BIND);
        }
        return true;
    }
    if std::path::Path::new("/cdrom/arch").is_dir() {
        console_log("archiso detected");
        unsafe {
            do_mkdir("/run/archiso/bootmnt");
            do_mount("/cdrom", "/run/archiso/bootmnt", "", MS_BIND);
        }
        return true;
    }
    if std::path::Path::new("/cdrom/.alpine-release").exists()
        || std::path::Path::new("/cdrom/apks").is_dir()
    {
        console_log("alpine detected");
        return true;
    }
    console_log("unknown distro, mount anyway");
    true
}

// ── Main logic ──────────────────────────────────────────────────────────

fn main() {
    console_log("starting");

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

        // Create /dev/loop* nodes (loop module creates them via devtmpfs,
        // but we ensure they exist)
        for i in 0u32..8 {
            do_mknod_blk(&format!("/dev/loop{i}"), 7, i);
        }
    }

    // 2. Read ISO offset from cmdline
    let offset = read_offset();
    console_log(&format!("offset={offset}"));

    // 3. Wait for devices
    std::thread::sleep(std::time::Duration::from_secs(3));

    // 4. Scan /proc/partitions
    let Ok(data) = fs::read_to_string("/proc/partitions") else {
        console_log("cannot read /proc/partitions");
        unsafe { do_execve("/init"); }
    };

    let mut found = false;
    for line in data.lines().skip(2) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 { continue }
        let name = cols[3];
        if name.starts_with("loop") || name.starts_with("ram")
            || name.starts_with("dm") || name.starts_with("sr")
        { continue }

        let dev = format!("/dev/{name}");
        let Ok(df) = fs::OpenOptions::new().read(true).write(true).open(&dev) else { continue };
        use std::os::fd::AsRawFd;
        let dfd = df.as_raw_fd();

        // Find free loop device and bind
        let mut loop_i: Option<i32> = None;
        for i in 0i32..8 {
            let lp = format!("/dev/loop{i}");
            if let Ok(lf) = fs::OpenOptions::new().read(true).write(true).open(&lp) {
                let lfd = lf.as_raw_fd();
                if unsafe { libc::ioctl(lfd, LOOP_SET_FD, dfd) } == 0 {
                    loop_i = Some(i);
                    unsafe { libc::close(lfd); }
                    break;
                }
                unsafe { libc::close(lfd); }
            }
        }
        drop(df); // close dev_fd

        let Some(li) = loop_i else { continue };
        let lp = format!("/dev/loop{li}");

        // Mount with offset
        let mount_ok = if offset > 0 {
            let opts = format!("ro,offset={offset}");
            let o = std::ffi::CString::new(opts).unwrap();
            let l = std::ffi::CString::new(lp.as_str()).unwrap();
            let t = std::ffi::CString::new("/cdrom").unwrap();
            let f = std::ffi::CString::new("iso9660").unwrap();
            unsafe { libc::mount(l.as_ptr(), t.as_ptr(), f.as_ptr(), MS_RDONLY, o.as_ptr() as *const libc::c_void) == 0 }
        } else {
            unsafe { do_mount(&lp, "/cdrom", "iso9660", MS_RDONLY) == 0 }
        };

        if !mount_ok {
            // Detach loop
            if let Ok(lf) = fs::OpenOptions::new().read(true).write(true).open(&lp) {
                let lfd = lf.as_raw_fd();
                unsafe { libc::ioctl(lfd, LOOP_CLR_FD, 0) };
            }
            continue;
        }

        if check_distro() {
            found = true;
            break;
        }

        // Not our ISO — clean up
        unsafe { libc::umount2(b"/cdrom\0".as_ptr() as *const i8, libc::MNT_DETACH); }
        if let Ok(lf) = fs::OpenOptions::new().read(true).write(true).open(&lp) {
            let lfd = lf.as_raw_fd();
            unsafe { libc::ioctl(lfd, LOOP_CLR_FD, 0) };
        }
    }

    if found {
        console_log("success, exec /init");
    } else {
        console_log("fail, exec /init (fallback)");
    }

    unsafe { do_execve("/init"); }
}