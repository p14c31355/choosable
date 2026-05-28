#![no_std]
#![no_main]

use core::panic::PanicInfo;

const OPEN_PROTOCOL_GET_PROTOCOL: u32 = 2;

/// UEFI entry point — NEVER returns.
#[no_mangle]
extern "efiapi" fn efi_main(
    image_handle_arg: *mut core::ffi::c_void,
    system_table: *mut SystemTable,
) -> ! {
    // Save image_handle for open_protocol fallback
    unsafe { OUR_IMAGE_HANDLE = image_handle_arg; }

    let st = unsafe { &mut *system_table };
    let bs = unsafe { &mut *st.boot_services };

    if !st.con_out.is_null() {
        let con = unsafe { &mut *(st.con_out as *mut SimpleTextOutput) };
        con.output_string(b"\r\n========================================\r\n\0");
        con.output_string(b"    Choosable UEFI Bootloader v0.2      \r\n\0");
        con.output_string(b"========================================\r\n\0");
        con.output_string(b"\r\nScanning for bootable images...\r\n\0");
    }

    // Step 1: Get LoadedImageProtocol.
    let loaded_image = unsafe { get_protocol::<LoadedImageProtocol>(bs, image_handle_arg, &LOADED_IMAGE_PROTOCOL_GUID) };
    let loaded_image = match loaded_image {
        Some(lip) => lip,
        None => {
            print_error(st, b"ERROR: Failed to get LoadedImageProtocol.\r\n\0");
            halt_or_reboot(st);
        }
    };

    let device_handle = unsafe { (*loaded_image).device_handle };
    if device_handle.is_null() {
        print_error(st, b"ERROR: LoadedImage has no device handle.\r\n\0");
        halt_or_reboot(st);
    }

    // Step 2: Get SimpleFileSystem protocol — three-stage fallback.
    //         1. handle_protocol on device_handle (fast path)
    //         2. open_protocol on device_handle (for buggy firmware)
    //         3. locate_handle_buffer + handle_protocol (global search)
    let fs = unsafe { acquire_simple_file_system(st, bs, device_handle) };
    if fs.is_null() {
        print_error(st, b"ERROR: Failed to open filesystem.\r\n\0");
        halt_or_reboot(st);
    }

    print_info(st, b"Filesystem opened successfully!\r\n\0");
    print_info(st, b"Boot menu coming in a future update.\r\n\0");

    halt_or_reboot(st);
}

// ─── Three-stage SFS acquisition ────────────────────────────────────────

unsafe fn acquire_simple_file_system(
    st: &mut SystemTable,
    bs: &mut BootServices,
    device_handle: *mut core::ffi::c_void,
) -> *mut core::ffi::c_void {
    let guid = &SIMPLE_FILE_SYSTEM_GUID;

    // Stage 1: handle_protocol on the device handle (UEFI 2.0+ fast path)
    let mut iface: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = (bs.handle_protocol)(device_handle, guid, &mut iface);
    if status == 0 && !iface.is_null() {
        return iface;
    }
    print_hex32(st, b"  handle_protocol status: 0x", status);
    print_raw(st, b"\r\n\0");

    // Stage 2: open_protocol with GET_PROTOCOL on device_handle
    iface = core::ptr::null_mut();
    let status = (bs.open_protocol)(
        device_handle, guid, &mut iface,
        OUR_IMAGE_HANDLE, core::ptr::null_mut(),
        OPEN_PROTOCOL_GET_PROTOCOL,
    );
    if status == 0 && !iface.is_null() {
        return iface;
    }
    print_hex32(st, b"  open_protocol  status: 0x", status);
    print_raw(st, b"\r\n\0");

    // Stage 3: locate_handle_buffer to find ANY handle with SFS,
    //          then handle_protocol on the first one found.
    //          (Works on firmware where the device_handle has no
    //           explicit SFS binding but the partition does.)
    let mut num_handles: usize = 0;
    let mut handle_buf: *mut *mut core::ffi::c_void = core::ptr::null_mut();
    let status = (bs.locate_handle_buffer)(
        LocateSearchType::ByProtocol,
        guid,
        core::ptr::null_mut(),  // SearchKey (not used for ByProtocol)
        &mut num_handles,
        &mut handle_buf,
    );
    print_hex32(st, b"  locate_handle   status: 0x", status);
    print_raw(st, b"\r\n\0");

    if status == 0 && !handle_buf.is_null() && num_handles > 0 {
        // Try each handle — the first one that gives us SFS wins.
        for i in 0..num_handles {
            let h = unsafe { *handle_buf.add(i) };
            iface = core::ptr::null_mut();
            let s = (bs.handle_protocol)(h, guid, &mut iface);
            if s == 0 && !iface.is_null() {
                // Found one! Free the buffer and return.
                (bs.free_pool)(handle_buf as *mut core::ffi::c_void);
                return iface;
            }
        }
        (bs.free_pool)(handle_buf as *mut core::ffi::c_void);
    }

    core::ptr::null_mut()
}

// ─── Protocol acquisition (used for LoadedImageProtocol only) ───────────

unsafe fn get_protocol<T>(
    bs: &mut BootServices,
    handle: *mut core::ffi::c_void,
    guid: &Guid,
) -> Option<*mut T> {
    let mut iface: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = (bs.handle_protocol)(handle, guid, &mut iface);
    if status == 0 && !iface.is_null() {
        return Some(iface as *mut T);
    }
    // Fallback: open_protocol
    iface = core::ptr::null_mut();
    let status = (bs.open_protocol)(
        handle, guid, &mut iface,
        OUR_IMAGE_HANDLE, core::ptr::null_mut(),
        OPEN_PROTOCOL_GET_PROTOCOL,
    );
    if status == 0 && !iface.is_null() {
        return Some(iface as *mut T);
    }
    None
}

// ─── Output helpers ─────────────────────────────────────────────────────

fn print_raw(st: &mut SystemTable, s: &[u8]) {
    if !st.con_out.is_null() {
        let con = unsafe { &mut *(st.con_out as *mut SimpleTextOutput) };
        con.output_string(s);
    }
}

fn print_error(st: &mut SystemTable, s: &[u8]) { print_raw(st, s); }
fn print_info(st: &mut SystemTable, s: &[u8]) { print_raw(st, s); }

fn print_hex32(st: &mut SystemTable, prefix: &[u8], val: usize) {
    print_raw(st, prefix);
    let hex = b"0123456789ABCDEF";
    let mut buf = [0u8; 10];
    buf[8] = hex[(val >> 4) & 0xF];
    buf[9] = hex[val & 0xF];
    buf[6] = hex[(val >> 12) & 0xF];
    buf[7] = hex[(val >> 8) & 0xF];
    buf[4] = hex[(val >> 20) & 0xF];
    buf[5] = hex[(val >> 16) & 0xF];
    buf[2] = hex[(val >> 28) & 0xF];
    buf[3] = hex[(val >> 24) & 0xF];
    // Strip leading zeros but keep at least "0x0"
    let start = if val >= 0x1000_0000 { 0 }
        else if val >= 0x100_0000 { 2 }
        else if val >= 0x10_0000 { 4 }
        else if val >= 0x10000 { 5 }
        else { 6 };
    buf[start] = b'0';
    buf[start + 1] = b'x';
    print_raw(st, &buf[start..10]);
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
        print_info(st, b"Press any key to reboot.\r\n\0");
        for _ in 0..300 {
            unsafe { (bs.stall)(100_000) };
            let mut key = Key { scan_code: 0, unicode_char: 0 };
            let status = unsafe { (con_in.read_key_stroke)(con_in as *mut SimpleTextInput, &mut key) };
            if status == 0 {
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

// ─── UEFI Type Definitions ──────────────────────────────────────────────

static mut OUR_IMAGE_HANDLE: *mut core::ffi::c_void = core::ptr::null_mut();

#[repr(C)]
struct SystemTable {
    hdr: TableHeader,
    firmware_vendor: *const u16,
    firmware_revision: u32,
    _pad: u32,
    _console_in_handle: *mut core::ffi::c_void,
    con_in: *mut core::ffi::c_void,
    _console_out_handle: *mut core::ffi::c_void,
    con_out: *mut core::ffi::c_void,
    _stderr_handle: *mut core::ffi::c_void,
    stderr: *mut core::ffi::c_void,
    runtime_services: *mut RuntimeServices,
    boot_services: *mut BootServices,
    _num_table_entries: usize,
    _config_table: *mut core::ffi::c_void,
}

#[repr(C)]
struct TableHeader {
    signature: u64,
    revision: u32,
    header_size: u32,
    crc32: u32,
    _reserved: u32,
}

#[repr(C)]
struct SimpleTextOutput {
    _reset: unsafe extern "efiapi" fn(*mut Self, bool) -> usize,
    output_string: unsafe extern "efiapi" fn(*mut Self, *const u16) -> usize,
    _test_string: *mut core::ffi::c_void,
    _query_mode: *mut core::ffi::c_void,
    _set_mode: *mut core::ffi::c_void,
    _set_attribute: *mut core::ffi::c_void,
    _clear_screen: unsafe extern "efiapi" fn(*mut Self) -> usize,
    _set_cursor: *mut core::ffi::c_void,
    _enable_cursor: *mut core::ffi::c_void,
    mode: *mut SimpleTextOutputMode,
}

#[repr(C)]
struct SimpleTextOutputMode {
    _max_mode: i32,
    _mode: i32,
    _attribute: i32,
    _cursor_column: i32,
    _cursor_row: i32,
    _cursor_visible: bool,
}

#[repr(C)]
struct SimpleTextInput {
    _reset: unsafe extern "efiapi" fn(*mut Self, bool) -> usize,
    read_key_stroke: unsafe extern "efiapi" fn(*mut Self, *mut Key) -> usize,
    _wait_for_key: *mut core::ffi::c_void,
}

#[repr(C)]
struct Key {
    scan_code: u16,
    unicode_char: u16,
}

#[repr(C)]
struct BootServices {
    hdr: TableHeader,
    _raise_tpl: *mut core::ffi::c_void,
    _restore_tpl: *mut core::ffi::c_void,
    _allocate_pages: *mut core::ffi::c_void,
    _free_pages: *mut core::ffi::c_void,
    _get_memory_map: *mut core::ffi::c_void,
    _allocate_pool: *mut core::ffi::c_void,
    free_pool: unsafe extern "efiapi" fn(*mut core::ffi::c_void) -> usize,
    _create_event: *mut core::ffi::c_void,
    _set_timer: *mut core::ffi::c_void,
    _wait_for_event: *mut core::ffi::c_void,
    _signal_event: *mut core::ffi::c_void,
    _close_event: *mut core::ffi::c_void,
    _check_event: *mut core::ffi::c_void,
    _install_protocol_interface: *mut core::ffi::c_void,
    _reinstall_protocol_interface: *mut core::ffi::c_void,
    _uninstall_protocol_interface: *mut core::ffi::c_void,
    handle_protocol: unsafe extern "efiapi" fn(
        *mut core::ffi::c_void,
        *const Guid,
        *mut *mut core::ffi::c_void,
    ) -> usize,
    _reserved: *mut core::ffi::c_void,
    _register_protocol_notify: *mut core::ffi::c_void,
    locate_handle_buffer: unsafe extern "efiapi" fn(
        LocateSearchType,
        *const Guid,
        *mut core::ffi::c_void,
        *mut usize,
        *mut *mut *mut core::ffi::c_void,
    ) -> usize,
    _locate_device_path: *mut core::ffi::c_void,
    _install_configuration_table: *mut core::ffi::c_void,
    _image_load: *mut core::ffi::c_void,
    _image_start: *mut core::ffi::c_void,
    _exit: *mut core::ffi::c_void,
    _image_unload: *mut core::ffi::c_void,
    _exit_boot_services: *mut core::ffi::c_void,
    _get_next_monotonic_count: *mut core::ffi::c_void,
    stall: unsafe extern "efiapi" fn(usize) -> usize,
    _set_watchdog_timer: *mut core::ffi::c_void,
    _connect_controller: *mut core::ffi::c_void,
    _disconnect_controller: *mut core::ffi::c_void,
    open_protocol: unsafe extern "efiapi" fn(
        *mut core::ffi::c_void,
        *const Guid,
        *mut *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        u32,
    ) -> usize,
    _close_protocol: *mut core::ffi::c_void,
    _open_protocol_information: *mut core::ffi::c_void,
    _protocols_per_handle: *mut core::ffi::c_void,
    _locate_handle_buffer: *mut core::ffi::c_void,
    _locate_protocol: *mut core::ffi::c_void,
    _install_multiple_protocol_interfaces: *mut core::ffi::c_void,
    _uninstall_multiple_protocol_interfaces: *mut core::ffi::c_void,
    _calculate_crc32: *mut core::ffi::c_void,
    _copy_mem: *mut core::ffi::c_void,
    _set_mem: *mut core::ffi::c_void,
    _create_event_ex: *mut core::ffi::c_void,
}

#[repr(C)]
struct RuntimeServices {
    hdr: TableHeader,
    _get_time: *mut core::ffi::c_void,
    _set_time: *mut core::ffi::c_void,
    _get_wakeup_time: *mut core::ffi::c_void,
    _set_wakeup_time: *mut core::ffi::c_void,
    _set_virtual_address_map: *mut core::ffi::c_void,
    _convert_pointer: *mut core::ffi::c_void,
    _get_variable: *mut core::ffi::c_void,
    _get_next_variable_name: *mut core::ffi::c_void,
    _set_variable: *mut core::ffi::c_void,
    _get_next_high_monotonic_count: *mut core::ffi::c_void,
    reset_system: unsafe extern "efiapi" fn(ResetType, usize, usize, *mut core::ffi::c_void) -> !,
    _update_capsule: *mut core::ffi::c_void,
    _query_capsule_capabilities: *mut core::ffi::c_void,
    _query_variable_info: *mut core::ffi::c_void,
}

#[repr(C)]
struct MemoryDescriptor {
    _type_: u32,
    _pad: u32,
    _physical_start: u64,
    _virtual_start: u64,
    _number_of_pages: u64,
    _attribute: u64,
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
enum ResetType {
    ResetCold = 0,
}

#[derive(Clone, Copy)]
#[repr(u32)]
enum LocateSearchType {
    ByProtocol = 2,
}

// ─── UEFI Protocol GUIDs ────────────────────────────────────────────────

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
    _revision: u32,
    _parent_handle: *mut core::ffi::c_void,
    _system_table: *mut core::ffi::c_void,
    device_handle: *mut core::ffi::c_void,
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