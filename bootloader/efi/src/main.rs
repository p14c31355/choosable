#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// EFI_UNSUPPORTED = 0x8000_0000_0000_0003 (bit 63 set → error)
/// EFI_NOT_FOUND   = 0x8000_0000_0000_000E
const EFI_SUCCESS: usize = 0;

/// UEFI entry point — NEVER returns.
#[no_mangle]
extern "efiapi" fn efi_main(
    image_handle: *mut core::ffi::c_void,
    system_table: *mut SystemTable,
) -> ! {
    let st = unsafe { &mut *system_table };
    let bs = unsafe { &mut *st.boot_services };

    if !st.con_out.is_null() {
        let con = unsafe { &mut *(st.con_out as *mut SimpleTextOutput) };
        con.output_string(b"\r\n========================================\r\n\0");
        con.output_string(b"    Choosable UEFI Bootloader v0.5      \r\n\0");
        con.output_string(b"========================================\r\n\0");
    }

    // Step 1: Get LoadedImageProtocol on image_handle.
    let mut loaded_image: *mut LoadedImageProtocol = core::ptr::null_mut();
    let status = unsafe {
        (bs.handle_protocol)(
            image_handle,
            &LOADED_IMAGE_PROTOCOL_GUID,
            &mut loaded_image as *mut _ as *mut *mut core::ffi::c_void,
        )
    };
    if status != EFI_SUCCESS || loaded_image.is_null() {
        print_hex(st, b"ERROR: LoadedImageProtocol failed: 0x", status as u64);
        print_raw(st, b"\r\n");
        halt_or_reboot(st);
    }

    // Step 2: Get the device handle from LoadedImage.
    let device_handle = unsafe { (*loaded_image).device_handle };
    if device_handle.is_null() {
        print_raw(st, b"ERROR: No device handle in LoadedImage.\r\n\0");
        halt_or_reboot(st);
    }

    // Step 3: Get SimpleFileSystem on device_handle.
    let mut fs: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.handle_protocol)(device_handle, &SIMPLE_FILE_SYSTEM_GUID, &mut fs)
    };
    if status != EFI_SUCCESS || fs.is_null() {
        print_hex(st, b"ERROR: SFS on device failed: 0x", status as u64);
        print_raw(st, b"\r\n");

        // Fallback: try handle_protocol on image_handle directly
        // (some firmware binds SFS to the image handle)
        fs = core::ptr::null_mut();
        let status = unsafe {
            (bs.handle_protocol)(image_handle, &SIMPLE_FILE_SYSTEM_GUID, &mut fs)
        };
        if status == EFI_SUCCESS && !fs.is_null() {
            print_raw(st, b"  (found SFS on image handle as fallback)\r\n\0");
        } else {
            print_hex(st, b"  Fallback also failed: 0x", status as u64);
            print_raw(st, b"\r\n");
            halt_or_reboot(st);
        }
    }

    print_raw(st, b"Filesystem opened successfully!\r\n\0");
    print_raw(st, b"Boot menu coming in a future update.\r\n\0");

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
            let status = unsafe { (con_in.read_key_stroke)(con_in as *mut SimpleTextInput, &mut key) };
            if status == EFI_SUCCESS { system_reset(st); }
        }
    }
    system_reset(st);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop { unsafe { core::arch::asm!("hlt") } }
}

// ═══════════════════════════════════════════════════════════════════
//  UEFI Type Definitions (offsets verified against UEFI 2.10 spec)
// ═══════════════════════════════════════════════════════════════════

#[repr(C)]
struct SystemTable {
    hdr:                  TableHeader,            // 0x00
    firmware_vendor:      *const u16,             // 0x18
    firmware_revision:    u32,                    // 0x20
    _pad24:               u32,                    // 0x24 (align to 8)
    _console_in_handle:   *mut core::ffi::c_void, // 0x28
    con_in:               *mut core::ffi::c_void, // 0x30
    _console_out_handle:  *mut core::ffi::c_void, // 0x38
    con_out:              *mut core::ffi::c_void, // 0x40
    _stderr_handle:       *mut core::ffi::c_void, // 0x48
    stderr:               *mut core::ffi::c_void, // 0x50
    runtime_services:     *mut RuntimeServices,   // 0x58
    boot_services:        *mut BootServices,      // 0x60
    _num_table_entries:   usize,                  // 0x68
    _config_table:        *mut core::ffi::c_void, // 0x70
}

#[repr(C)]
struct TableHeader {
    signature:   u64,  // 0x00
    revision:    u32,  // 0x08
    header_size: u32,  // 0x0C
    crc32:       u32,  // 0x10
    _reserved:   u32,  // 0x14
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

/// BootServices — every field explicitly listed with offsets.
#[repr(C)]
struct BootServices {
    // 0x00
    hdr: TableHeader,
    // 0x18
    _18: *mut core::ffi::c_void, // RaiseTPL
    _20: *mut core::ffi::c_void, // RestoreTPL
    _28: *mut core::ffi::c_void, // AllocatePages
    _30: *mut core::ffi::c_void, // FreePages
    _38: *mut core::ffi::c_void, // GetMemoryMap
    _40: *mut core::ffi::c_void, // AllocatePool
    _48: *mut core::ffi::c_void, // FreePool
    _50: *mut core::ffi::c_void, // CreateEvent
    _58: *mut core::ffi::c_void, // SetTimer
    _60: *mut core::ffi::c_void, // WaitForEvent
    _68: *mut core::ffi::c_void, // SignalEvent
    _70: *mut core::ffi::c_void, // CloseEvent
    _78: *mut core::ffi::c_void, // CheckEvent
    _80: *mut core::ffi::c_void, // InstallProtocolInterface
    _88: *mut core::ffi::c_void, // ReinstallProtocolInterface
    _90: *mut core::ffi::c_void, // UninstallProtocolInterface
    // 0x98
    handle_protocol: unsafe extern "efiapi" fn(
        *mut core::ffi::c_void,
        *const Guid,
        *mut *mut core::ffi::c_void,
    ) -> usize,
    // 0xA0
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
    // 0xF8
    stall: unsafe extern "efiapi" fn(usize) -> usize,
    // 0x100
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
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[derive(Clone, Copy)]
#[repr(u32)]
enum ResetType { ResetCold = 0 }

// ─── GUIDs ──────────────────────────────────────────────────────────────

const LOADED_IMAGE_PROTOCOL_GUID: Guid = Guid {
    data1: 0x5B1B31A1,
    data2: 0x9562,
    data3: 0x11D2,
    data4: [0x8E, 0x3F, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B],
};

const SIMPLE_FILE_SYSTEM_GUID: Guid = Guid {
    data1: 0x0964e5b2,
    data2: 0x6459,
    data3: 0x11d2,
    data4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};

// ─── EFI_LOADED_IMAGE_PROTOCOL ──────────────────────────────────────────

#[repr(C)]
struct LoadedImageProtocol {
    _revision:      u32,
    _parent_handle: *mut core::ffi::c_void,
    _system_table:  *mut core::ffi::c_void,
    device_handle:  *mut core::ffi::c_void,
}

// ─── SimpleTextOutput helpers ───────────────────────────────────────────

impl SimpleTextOutput {
    fn output_string(&mut self, s: &[u8]) {
        let mut buf = [0u16; 256];
        let len = s.len().min(255);
        for (i, &b) in s[..len].iter().enumerate() {
            buf[i] = b as u16;
        }
        buf[len] = 0;
        unsafe { (self.output_string)(self as *mut Self, buf.as_ptr()) };
    }
}