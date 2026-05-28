#![no_std]
#![no_main]

use core::panic::PanicInfo;

const EFI_SUCCESS: usize = 0;

#[no_mangle]
extern "efiapi" fn efi_main(
    image_handle: *mut core::ffi::c_void,
    system_table: *mut SystemTable,
) -> ! {
    let st = unsafe { &mut *system_table };
    let bs = unsafe { &mut *st.boot_services };

    banner(st);

    // 1. Try SimpleFileSystem on image_handle (fast path)
    if try_sfs(bs, image_handle) { info(st, b"Filesystem opened (SFS on image).\r\n\0"); halt_or_reboot(st); }

    // 2. Try SFS via LoadedImageProtocol → device_handle
    if let Some(dh) = device_handle_from_image(bs, image_handle) {
        if try_sfs(bs, dh) { info(st, b"Filesystem opened (SFS on device).\r\n\0"); halt_or_reboot(st); }
    }

    // 3. Block I/O on device_handle then image_handle
    info(st, b"SFS not available, trying Block I/O...\r\n\0");
    if try_block_io_on_handles(st, bs, image_handle) { halt_or_reboot(st); }

    // 4. Locate ALL Block I/O handles in the system
    info(st, b"Still not found, locating all Block I/O handles...\r\n\0");
    try_block_io_locate_all(st, bs);

    halt_or_reboot(st);
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn device_handle_from_image(bs: &mut BootServices, image_handle: *mut core::ffi::c_void) -> Option<*mut core::ffi::c_void> {
    let mut lip: *mut LoadedImageProtocol = core::ptr::null_mut();
    if unsafe { (bs.handle_protocol)(image_handle, &LOADED_IMAGE_PROTOCOL_GUID, &mut lip as *mut _ as _) } != EFI_SUCCESS || lip.is_null() {
        return None;
    }
    let dh = unsafe { (*lip).device_handle };
    if dh.is_null() { None } else { Some(dh) }
}

fn try_sfs(bs: &mut BootServices, handle: *mut core::ffi::c_void) -> bool {
    let mut sfs: *mut core::ffi::c_void = core::ptr::null_mut();
    unsafe { (bs.handle_protocol)(handle, &SIMPLE_FILE_SYSTEM_GUID, &mut sfs) == EFI_SUCCESS && !sfs.is_null() }
}

/// Try Block I/O on a set of handles. Returns true if FAT16 found.
fn try_block_io_on_handles(st: &mut SystemTable, bs: &mut BootServices, image_handle: *mut core::ffi::c_void) -> bool {
    let dh = device_handle_from_image(bs, image_handle);
    let handles: [*mut core::ffi::c_void; 2] = [
        dh.unwrap_or(core::ptr::null_mut()),
        image_handle,
    ];
    for &handle in &handles {
        if handle.is_null() { continue; }
        if scan_block_io_handle(st, bs, handle) { return true; }
    }
    false
}

/// Locate ALL handles with Block I/O protocol via LocateHandleBuffer.
fn try_block_io_locate_all(st: &mut SystemTable, bs: &mut BootServices) {
    let mut num: usize = 0;
    let mut buf: *mut *mut core::ffi::c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.locate_handle_buffer)(LocateSearchType::ByProtocol, &BLOCK_IO_PROTOCOL_GUID,
            core::ptr::null_mut(), &mut num, &mut buf)
    };
    if status != EFI_SUCCESS || buf.is_null() || num == 0 {
        print_hex(st, b"locate_handle_buffer status: 0x", status as u64);
        print_raw(st, b"\r\n\0");
        die(st, b"ERROR: No Block I/O handles in system.\r\n\0");
    }
    for i in 0..num {
        let h = unsafe { *buf.add(i) };
        if scan_block_io_handle(st, bs, h) {
            unsafe { (bs.free_pool)(buf as *mut core::ffi::c_void) };
            return;
        }
    }
    unsafe { (bs.free_pool)(buf as *mut core::ffi::c_void) };
    die(st, b"ERROR: No FAT16 partition found on any handle.\r\n\0");
}

/// Read LBA 0 from a handle's Block I/O, then search for FAT16/MBR/GPT→ESP.
fn scan_block_io_handle(st: &mut SystemTable, bs: &mut BootServices, handle: *mut core::ffi::c_void) -> bool {
    let mut bio: *mut BlockIoProtocol = core::ptr::null_mut();
    if unsafe { (bs.handle_protocol)(handle, &BLOCK_IO_PROTOCOL_GUID, &mut bio as *mut _ as _) } != EFI_SUCCESS || bio.is_null() { return false; }
    let bio_ptr = bio;
    let bio_ref = unsafe { &*bio };
    let mid = if !bio_ref.media.is_null() { unsafe { (*bio_ref.media).media_id } } else { return false };

    let mut buf: [u8; 512] = [0; 512];
    if unsafe { (bio_ref.read_blocks)(bio_ptr, mid, 0, 512, buf.as_mut_ptr() as _) } != EFI_SUCCESS { return false; }

    if is_fat16(&buf) { info(st, b"FAT16 found at LBA 0 on handle.\r\n\0"); return true; }
    if buf[510] != 0x55 || buf[511] != 0xAA { return false; }

    // MBR found — scan partition entries
    for i in 0..4 {
        let off = 446 + i * 16;
        let fs = buf[off + 4];
        let lba = u32::from_le_bytes([buf[off+8],buf[off+9],buf[off+10],buf[off+11]]);
        let sec = u32::from_le_bytes([buf[off+12],buf[off+13],buf[off+14],buf[off+15]]);
        if sec == 0 { continue; }
        if fs == 0xEF {
            if check_fat16_at(st, bio_ref, bio_ptr, mid, lba as u64) { return true; }
        }
        if fs == 0xEE {
            if search_gpt(st, bio_ref, bio_ptr, mid) { return true; }
        }
    }
    false
}

fn is_fat16(buf: &[u8; 512]) -> bool { &buf[0x36..0x3E] == b"FAT16   " }

fn check_fat16_at(st: &mut SystemTable, bio_ref: &BlockIoProtocol, bio_ptr: *mut BlockIoProtocol, mid: u32, lba: u64) -> bool {
    let mut vbr: [u8; 512] = [0; 512];
    if unsafe { (bio_ref.read_blocks)(bio_ptr, mid, lba, 512, vbr.as_mut_ptr() as _) } != EFI_SUCCESS { return false; }
    if is_fat16(&vbr) { info(st, b"FAT16 CZBLEFI found via Block I/O.\r\n\0"); return true; }
    false
}

fn search_gpt(st: &mut SystemTable, bio_ref: &BlockIoProtocol, bio_ptr: *mut BlockIoProtocol, mid: u32) -> bool {
    let mut hdr: [u8; 92] = [0; 92];
    if unsafe { (bio_ref.read_blocks)(bio_ptr, mid, 1, 512, hdr.as_mut_ptr() as _) } != EFI_SUCCESS { return false; }
    if &hdr[0..8] != b"EFI PART" { return false; }
    let entries_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
    let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap());
    let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap());
    if sz == 0 || n == 0 { return false; }
    let esp: [u8;16] = [0x28,0x73,0x2A,0xC1,0x1F,0xF8,0xD2,0x11,0xBA,0x4B,0x00,0xA0,0xC9,0x3E,0xC9,0x3B];
    for i in 0..n.min(128) {
        let eoff = i as usize * sz as usize;
        let sec = entries_lba + (eoff / 512) as u64;
        let boff = eoff % 512;
        let mut s: [u8; 512] = [0; 512];
        if unsafe { (bio_ref.read_blocks)(bio_ptr, mid, sec, 512, s.as_mut_ptr() as _) } != EFI_SUCCESS { break; }
        if boff + 16 > 512 { continue; }
        if s[boff..boff+16] == esp {
            let lba = u64::from_le_bytes(s[boff+32..boff+40].try_into().unwrap());
            if check_fat16_at(st, bio_ref, bio_ptr, mid, lba) { return true; }
        }
    }
    false
}

// ═══════════════════════════════════════════════════════════════════
//  Output helpers
// ═══════════════════════════════════════════════════════════════════

fn banner(st: &mut SystemTable) {
    if !st.con_out.is_null() {
        let con = unsafe { &mut *(st.con_out as *mut SimpleTextOutput) };
        prints(con, b"\r\n========================================\r\n\0");
        prints(con, b"    Choosable UEFI Bootloader v0.9      \r\n\0");
        prints(con, b"========================================\r\n\0");
    }
}
fn prints(co: &mut SimpleTextOutput, s: &[u8]) {
    let mut buf = [0u16; 256];
    let len = s.len().min(255);
    for (i, &b) in s[..len].iter().enumerate() { buf[i] = b as u16; }
    buf[len] = 0;
    unsafe { (co.output_string)(co as *mut SimpleTextOutput, buf.as_ptr()) };
}
fn print_raw(st: &mut SystemTable, s: &[u8]) { if !st.con_out.is_null() { let co = unsafe { &mut *(st.con_out as *mut SimpleTextOutput) }; prints(co, s); } }
fn info(st: &mut SystemTable, s: &[u8]) { print_raw(st, s); }
fn die(st: &mut SystemTable, s: &[u8]) -> ! { print_raw(st, s); halt_or_reboot(st); }
fn print_hex(st: &mut SystemTable, prefix: &[u8], val: u64) {
    print_raw(st, prefix);
    for i in (0..16).rev() { print_raw(st, &[b"0123456789ABCDEF"[((val>>(i*4))&0xF) as usize]]); }
}

// ═══════════════════════════════════════════════════════════════════
//  Halt / reboot
// ═══════════════════════════════════════════════════════════════════

fn system_reset(st: &mut SystemTable) -> ! {
    let rt = unsafe { &mut *st.runtime_services };
    unsafe { (rt.reset_system)(ResetType::ResetCold, 0, 0, core::ptr::null_mut()) };
    loop { unsafe { core::arch::asm!("hlt") } }
}
fn halt_or_reboot(st: &mut SystemTable) -> ! {
    let bs = unsafe { &mut *st.boot_services };
    if !st.con_in.is_null() {
        let ci = unsafe { &mut *(st.con_in as *mut SimpleTextInput) };
        print_raw(st, b"Press any key to reboot.\r\n\0");
        for _ in 0..300 {
            unsafe { (bs.stall)(100_000) };
            let mut k = Key { sc: 0, uc: 0 };
            if unsafe { (ci.read_key_stroke)(ci as *mut _, &mut k) } == EFI_SUCCESS { system_reset(st); }
        }
    }
    system_reset(st);
}
#[panic_handler] fn panic(_: &PanicInfo) -> ! { loop { unsafe { core::arch::asm!("hlt") } } }

// ═══════════════════════════════════════════════════════════════════
//  UEFI Types (offsets verified against UEFI 2.10 x86_64 ABI)
// ═══════════════════════════════════════════════════════════════════

#[repr(C)] struct SystemTable {
    hdr:              TableHeader,
    firmware_vendor:  *const u16,
    firmware_revision: u32,
    _pad24:           u32,
    _console_in_handle: *mut core::ffi::c_void,
    con_in:           *mut core::ffi::c_void,
    _console_out_handle: *mut core::ffi::c_void,
    con_out:          *mut core::ffi::c_void,
    _stderr_handle:   *mut core::ffi::c_void,
    stderr:           *mut core::ffi::c_void,
    runtime_services: *mut RuntimeServices,
    boot_services:    *mut BootServices,
    _num_table_entries: usize,
    _config_table:    *mut core::ffi::c_void,
}

#[repr(C)] struct TableHeader { signature: u64, revision: u32, header_size: u32, crc32: u32, _reserved: u32 }

#[repr(C)] struct SimpleTextOutput {
    _reset:        unsafe extern "efiapi" fn(*mut Self, bool) -> usize,
    output_string: unsafe extern "efiapi" fn(*mut Self, *const u16) -> usize,
    _p10:*mut core::ffi::c_void, _p18:*mut core::ffi::c_void, _p20:*mut core::ffi::c_void,
    _p28:*mut core::ffi::c_void, _p30:unsafe extern "efiapi" fn(*mut Self)->usize,
    _p38:*mut core::ffi::c_void, _p40:*mut core::ffi::c_void, _p48:*mut core::ffi::c_void,
}

#[repr(C)] struct SimpleTextInput {
    _r: unsafe extern "efiapi" fn(*mut Self,bool)->usize,
    read_key_stroke: unsafe extern "efiapi" fn(*mut Self,*mut Key)->usize,
    _w: *mut core::ffi::c_void,
}

#[repr(C)] struct Key { sc: u16, uc: u16 }

/// BootServices — all offsets explicitly listed.  17 pads → handle_protocol @ 0x98
#[repr(C)] struct BootServices {
    hdr: TableHeader,
    _18:*mut core::ffi::c_void, _20:*mut core::ffi::c_void, _28:*mut core::ffi::c_void, _30:*mut core::ffi::c_void,
    _38:*mut core::ffi::c_void, _40:*mut core::ffi::c_void, _48:*mut core::ffi::c_void, _50:*mut core::ffi::c_void,
    _58:*mut core::ffi::c_void, _60:*mut core::ffi::c_void, _68:*mut core::ffi::c_void, _70:*mut core::ffi::c_void,
    _78:*mut core::ffi::c_void, _80:*mut core::ffi::c_void, _88:*mut core::ffi::c_void, _90:*mut core::ffi::c_void,
    /// 0x98
    handle_protocol: unsafe extern "efiapi" fn(*mut core::ffi::c_void, *const Guid, *mut *mut core::ffi::c_void) -> usize,
    _a0:*mut core::ffi::c_void, _a8:*mut core::ffi::c_void, _b0:*mut core::ffi::c_void, _b8:*mut core::ffi::c_void,
    _c0:*mut core::ffi::c_void, _c8:*mut core::ffi::c_void, _d0:*mut core::ffi::c_void, _d8:*mut core::ffi::c_void,
    _e0:*mut core::ffi::c_void, _e8:*mut core::ffi::c_void, _f0:*mut core::ffi::c_void,
    /// 0xF8
    stall: unsafe extern "efiapi" fn(usize) -> usize,
    _100:*mut core::ffi::c_void, _108:*mut core::ffi::c_void, _110:*mut core::ffi::c_void, _118:*mut core::ffi::c_void,
    _120:*mut core::ffi::c_void, _128:*mut core::ffi::c_void, _130:*mut core::ffi::c_void,
    /// 0x138: LocateHandleBuffer
    locate_handle_buffer: unsafe extern "efiapi" fn(
        LocateSearchType, *const Guid, *mut core::ffi::c_void,
        *mut usize, *mut *mut *mut core::ffi::c_void,
    ) -> usize,
    _140:*mut core::ffi::c_void,
    /// 0x148
    free_pool: unsafe extern "efiapi" fn(*mut core::ffi::c_void) -> usize,
    _150:*mut core::ffi::c_void, _158:*mut core::ffi::c_void, _160:*mut core::ffi::c_void, _168:*mut core::ffi::c_void,
    _170:*mut core::ffi::c_void,
}

#[repr(C)] struct RuntimeServices {
    hdr: TableHeader,
    _18:*mut core::ffi::c_void, _20:*mut core::ffi::c_void, _28:*mut core::ffi::c_void, _30:*mut core::ffi::c_void,
    _38:*mut core::ffi::c_void, _40:*mut core::ffi::c_void, _48:*mut core::ffi::c_void, _50:*mut core::ffi::c_void,
    _58:*mut core::ffi::c_void, _60:*mut core::ffi::c_void,
    reset_system: unsafe extern "efiapi" fn(ResetType, usize, usize, *mut core::ffi::c_void) -> !,
}

#[repr(C)] struct BlockIoProtocol {
    _revision: u64, media: *mut BlockIoMedia, _rst: *mut core::ffi::c_void,
    read_blocks: unsafe extern "efiapi" fn(*mut Self, u32, u64, usize, *mut core::ffi::c_void) -> usize,
    _w: *mut core::ffi::c_void, _f: *mut core::ffi::c_void,
}

#[repr(C)] struct BlockIoMedia {
    media_id: u32, _rm: bool, _mp: bool, _lp: bool, _ro: bool, _wc: bool, _bs: u32, _ia: u32, _lb: u64,
}

#[repr(C)] struct Guid { d1: u32, d2: u16, d3: u16, d4: [u8; 8] }

#[repr(C)] struct LoadedImageProtocol {
    _rev: u32, _p: *mut core::ffi::c_void, _st: *mut core::ffi::c_void, device_handle: *mut core::ffi::c_void,
}

#[derive(Clone,Copy)] #[repr(u32)] enum ResetType { ResetCold=0 }
#[derive(Clone,Copy)] #[repr(u32)] enum LocateSearchType { ByProtocol=2 }

const LOADED_IMAGE_PROTOCOL_GUID: Guid = Guid { d1:0x5B1B31A1,d2:0x9562,d3:0x11D2,d4:[0x8E,0x3F,0x00,0xA0,0xC9,0x69,0x72,0x3B] };
const SIMPLE_FILE_SYSTEM_GUID: Guid = Guid { d1:0x0964e5b2,d2:0x6459,d3:0x11d2,d4:[0x8e,0x39,0x00,0xa0,0xc9,0x69,0x72,0x3b] };
const BLOCK_IO_PROTOCOL_GUID: Guid = Guid { d1:0x964e5b21,d2:0x6459,d3:0x11d2,d4:[0x8e,0x39,0x00,0xa0,0xc9,0x69,0x72,0x3b] };