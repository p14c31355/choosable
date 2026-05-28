#![no_std]
#![no_main]

use core::panic::PanicInfo;

const EFI_SUCCESS: usize = 0;

/// UEFI entry point — NEVER returns.
#[no_mangle]
extern "efiapi" fn efi_main(
    image_handle: *mut core::ffi::c_void,
    system_table: *mut SystemTable,
) -> ! {
    let st = unsafe { &mut *system_table };
    let bs = unsafe { &mut *st.boot_services };

    // Print banner
    if !st.con_out.is_null() {
        let con = unsafe { &mut *(st.con_out as *mut SimpleTextOutput) };
        con.output_string(b"\r\n========================================\r\n\0");
        con.output_string(b"    Choosable UEFI Bootloader v0.6      \r\n\0");
        con.output_string(b"========================================\r\n\0");
    }

    // Step 1: Get LoadedImageProtocol
    let mut loaded_image: *mut LoadedImageProtocol = core::ptr::null_mut();
    let status = unsafe {
        (bs.handle_protocol)(
            image_handle,
            &LOADED_IMAGE_PROTOCOL_GUID,
            &mut loaded_image as *mut _ as *mut *mut core::ffi::c_void,
        )
    };
    if status != EFI_SUCCESS || loaded_image.is_null() {
        print_raw(st, b"ERROR: No LoadedImageProtocol\r\n\0");
        halt_or_reboot(st);
    }

    let device_handle = unsafe { (*loaded_image).device_handle };
    if device_handle.is_null() {
        print_raw(st, b"ERROR: No device handle\r\n\0");
        halt_or_reboot(st);
    }

    // Step 2: Try SimpleFileSystem first (fast path on most firmware)
    let mut sfs_iface: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.handle_protocol)(device_handle, &SIMPLE_FILE_SYSTEM_GUID, &mut sfs_iface)
    };
    if status == EFI_SUCCESS && !sfs_iface.is_null() {
        print_raw(st, b"Filesystem opened (SFS).\r\n\0");
        halt_or_reboot(st);
    }

    // Step 3: Fallback — Block I/O Protocol + minimal FAT16 reader
    print_raw(st, b"SFS not supported, trying Block I/O...\r\n\0");

    let mut bio: *mut BlockIoProtocol = core::ptr::null_mut();
    let status = unsafe {
        (bs.handle_protocol)(
            device_handle,
            &BLOCK_IO_PROTOCOL_GUID,
            &mut bio as *mut _ as *mut *mut core::ffi::c_void,
        )
    };
    if status != EFI_SUCCESS || bio.is_null() {
        print_raw(st, b"ERROR: No Block I/O protocol.\r\n\0");
        halt_or_reboot(st);
    }

    let bio = unsafe { &*bio };
    if !bio.media.is_null() {
        let media = unsafe { &*bio.media };
        if !media.logical_partition {
            // Whole disk — we need partition 2 (offset 32 MiB from end)
            // But we don't know disk size here.  For now, just read sector 0.
            print_raw(st, b"Whole-disk Block I/O detected.\r\n\0");
        }
    }

    // Read a few sectors from the start of the partition
    // (Block I/O reads from partition start if logical_partition=true)
    let mut buf: [u8; 512] = [0; 512];
    let status = unsafe { (bio.read_blocks)(bio as *const BlockIoProtocol as *mut BlockIoProtocol, 0, 0, 1, buf.as_mut_ptr() as *mut core::ffi::c_void) };
    if status != EFI_SUCCESS {
        print_raw(st, b"ERROR: Block I/O read failed.\r\n\0");
        halt_or_reboot(st);
    }

    // Check for FAT16 signature (bytes 0x36-0x3D = "FAT16   ")
    if &buf[0x36..0x3E] == b"FAT16   " {
        print_raw(st, b"FAT16 partition found via Block I/O!\r\n\0");
        print_raw(st, b"Boot menu coming in a future update.\r\n\0");
    } else {
        print_hex(st, b"  First bytes: ", u64::from_le_bytes(buf[0..8].try_into().unwrap()));
        print_raw(st, b"\r\n\0");
    }

    halt_or_reboot(st);
}

// ─── Output helpers ─────────────────────────────────────────────────────

fn print_raw(st: &mut SystemTable, s: &[u8]) {
    if !st.con_out.is_null() {
        let con = unsafe { &mut *(st.con_out as *mut SimpleTextOutput) };
        con.output_string(s);
    }
}

fn print_hex(st: &mut SystemTable, prefix: &[u8], val: u64) {
    print_raw(st, prefix);
    let hex = b"0123456789ABCDEF";
    for i in (0..16).rev() {
        let shift = i * 4;
        let buf = [hex[((val >> shift) & 0xF) as usize]];
        print_raw(st, &buf);
    }
}

// ─── Halt / reboot ──────────────────────────────────────────────────────

fn system_reset(st: &mut SystemTable) -> ! {
    let rt = unsafe { &mut *st.runtime_services };
    unsafe { (rt.reset_system)(ResetType::ResetCold, 0, 0, core::ptr::null_mut()); }
    loop { unsafe { core::arch::asm!("hlt") } }
}

fn halt_or_reboot(st: &mut SystemTable) -> ! {
    let bs = unsafe { &mut *st.boot_services };
    if !st.con_in.is_null() {
        let con_in = unsafe { &mut *(st.con_in as *mut SimpleTextInput) };
        print_raw(st, b"Press any key to reboot.\r\n\0");
        for _ in 0..300 {
            unsafe { (bs.stall)(100_000) };
            let mut key = Key { scan_code: 0, unicode_char: 0 };
            if unsafe { (con_in.read_key_stroke)(con_in as *mut SimpleTextInput, &mut key) } == EFI_SUCCESS {
                system_reset(st);
            }
        }
    }
    system_reset(st);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop { unsafe { core::arch::asm!("hlt") } }
}

// ═══════════════════════════════════════════════════════════════════
//  UEFI Types
// ═══════════════════════════════════════════════════════════════════

#[repr(C)]
struct SystemTable {
    hdr:                  TableHeader,
    firmware_vendor:      *const u16,
    firmware_revision:    u32,
    _pad24:               u32,
    _console_in_handle:   *mut core::ffi::c_void,
    con_in:               *mut core::ffi::c_void,
    _console_out_handle:  *mut core::ffi::c_void,
    con_out:              *mut core::ffi::c_void,
    _stderr_handle:       *mut core::ffi::c_void,
    stderr:               *mut core::ffi::c_void,
    runtime_services:     *mut RuntimeServices,
    boot_services:        *mut BootServices,
    _num_table_entries:   usize,
    _config_table:        *mut core::ffi::c_void,
}

#[repr(C)]
struct TableHeader {
    signature:   u64,
    revision:    u32,
    header_size: u32,
    crc32:       u32,
    _reserved:   u32,
}

#[repr(C)]
struct SimpleTextOutput {
    _reset:         unsafe extern "efiapi" fn(*mut Self, bool) -> usize,
    output_string:  unsafe extern "efiapi" fn(*mut Self, *const u16) -> usize,
    _test_string:   *mut core::ffi::c_void,
    _query_mode:    *mut core::ffi::c_void,
    _set_mode:      *mut core::ffi::c_void,
    _set_attribute: *mut core::ffi::c_void,
    _clear_screen:  unsafe extern "efiapi" fn(*mut Self) -> usize,
    _set_cursor:    *mut core::ffi::c_void,
    _enable_cursor: *mut core::ffi::c_void,
    mode:           *mut SimpleTextOutputMode,
}

#[repr(C)]
struct SimpleTextOutputMode {
    _max_mode:        i32,
    _mode:            i32,
    _attribute:       i32,
    _cursor_column:   i32,
    _cursor_row:      i32,
    _cursor_visible:  bool,
}

#[repr(C)]
struct SimpleTextInput {
    _reset:          unsafe extern "efiapi" fn(*mut Self, bool) -> usize,
    read_key_stroke: unsafe extern "efiapi" fn(*mut Self, *mut Key) -> usize,
    _wait_for_key:   *mut core::ffi::c_void,
}

#[repr(C)]
struct Key {
    scan_code:    u16,
    unicode_char: u16,
}

#[repr(C)]
struct BootServices {
    hdr: TableHeader,
    _18: *mut core::ffi::c_void,
    _20: *mut core::ffi::c_void,
    _28: *mut core::ffi::c_void,
    _30: *mut core::ffi::c_void,
    _38: *mut core::ffi::c_void,
    _40: *mut core::ffi::c_void,
    _48: *mut core::ffi::c_void,
    _50: *mut core::ffi::c_void,
    _58: *mut core::ffi::c_void,
    _60: *mut core::ffi::c_void,
    _68: *mut core::ffi::c_void,
    _70: *mut core::ffi::c_void,
    _78: *mut core::ffi::c_void,
    _80: *mut core::ffi::c_void,
    _88: *mut core::ffi::c_void,
    _90: *mut core::ffi::c_void,
    handle_protocol: unsafe extern "efiapi" fn(
        *mut core::ffi::c_void,
        *const Guid,
        *mut *mut core::ffi::c_void,
    ) -> usize,
    _a0: *mut core::ffi::c_void,
    _a8: *mut core::ffi::c_void,
    _b0: *mut core::ffi::c_void,
    _b8: *mut core::ffi::c_void,
    _c0: *mut core::ffi::c_void,
    _c8: *mut core::ffi::c_void,
    _d0: *mut core::ffi::c_void,
    _d8: *mut core::ffi::c_void,
    _e0: *mut core::ffi::c_void,
    _e8: *mut core::ffi::c_void,
    _f0: *mut core::ffi::c_void,
    stall: unsafe extern "efiapi" fn(usize) -> usize,
    _100: *mut core::ffi::c_void,
    _108: *mut core::ffi::c_void,
    _110: *mut core::ffi::c_void,
    _118: *mut core::ffi::c_void,
    _120: *mut core::ffi::c_void,
    _128: *mut core::ffi::c_void,
    _130: *mut core::ffi::c_void,
    _138: *mut core::ffi::c_void,
    _140: *mut core::ffi::c_void,
    _148: *mut core::ffi::c_void,
    _150: *mut core::ffi::c_void,
    _158: *mut core::ffi::c_void,
    _160: *mut core::ffi::c_void,
    _168: *mut core::ffi::c_void,
    _170: *mut core::ffi::c_void,
}

#[repr(C)]
struct RuntimeServices {
    hdr: TableHeader,
    _18: *mut core::ffi::c_void,
    _20: *mut core::ffi::c_void,
    _28: *mut core::ffi::c_void,
    _30: *mut core::ffi::c_void,
    _38: *mut core::ffi::c_void,
    _40: *mut core::ffi::c_void,
    _48: *mut core::ffi::c_void,
    _50: *mut core::ffi::c_void,
    _58: *mut core::ffi::c_void,
    _60: *mut core::ffi::c_void,
    reset_system: unsafe extern "efiapi" fn(ResetType, usize, usize, *mut core::ffi::c_void) -> !,
}

#[repr(C)]
struct BlockIoProtocol {
    _revision:    u64,
    media:        *mut BlockIoMedia,
    _reset:       *mut core::ffi::c_void,
    read_blocks:  unsafe extern "efiapi" fn(
        *mut BlockIoProtocol,
        u32,       // MediaId
        u64,       // LBA
        usize,     // BufferSize
        *mut core::ffi::c_void, // Buffer
    ) -> usize,
    _write_blocks: *mut core::ffi::c_void,
    _flush_blocks: *mut core::ffi::c_void,
}

#[repr(C)]
struct BlockIoMedia {
    _media_id:          u32,
    _removable_media:   bool,
    _media_present:     bool,
    logical_partition:  bool,
    _read_only:         bool,
    _write_caching:     bool,
    _block_size:        u32,
    _io_align:          u32,
    _last_block:        u64,
}

#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[derive(Clone, Copy)]
#[repr(u32)]
enum ResetType { ResetCold = 0 }

#[repr(C)]
struct LoadedImageProtocol {
    _revision:      u32,
    _parent_handle: *mut core::ffi::c_void,
    _system_table:  *mut core::ffi::c_void,
    device_handle:  *mut core::ffi::c_void,
}

// ─── GUIDs ──────────────────────────────────────────────────────────────

const LOADED_IMAGE_PROTOCOL_GUID: Guid = Guid {
    data1: 0x5B1B31A1, data2: 0x9562, data3: 0x11D2,
    data4: [0x8E, 0x3F, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B],
};
const SIMPLE_FILE_SYSTEM_GUID: Guid = Guid {
    data1: 0x0964e5b2, data2: 0x6459, data3: 0x11d2,
    data4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};
const BLOCK_IO_PROTOCOL_GUID: Guid = Guid {
    data1: 0x964e5b21, data2: 0x6459, data3: 0x11d2,
    data4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};

// ─── SimpleTextOutput helpers ───────────────────────────────────────────

impl SimpleTextOutput {
    fn output_string(&mut self, s: &[u8]) {
        let mut buf = [0u16; 256];
        let len = s.len().min(255);
        for (i, &b) in s[..len].iter().enumerate() { buf[i] = b as u16; }
        buf[len] = 0;
        unsafe { (self.output_string)(self as *mut Self, buf.as_ptr()) };
    }
}