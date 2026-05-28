#![no_std]
#![no_main]

use core::panic::PanicInfo;

const OPEN_PROTOCOL_GET_PROTOCOL: u32 = 2;

/// UEFI entry point — NEVER returns.
#[no_mangle]
extern "efiapi" fn efi_main(
    image_handle: *mut core::ffi::c_void,
    system_table: *mut SystemTable,
) -> ! {
    // Save image_handle for open_protocol fallback
    unsafe { IMAGE_HANDLE = image_handle; }

    let st = unsafe { &mut *system_table };
    let bs = unsafe { &mut *st.boot_services };

    if !st.con_out.is_null() {
        let con = unsafe { &mut *(st.con_out as *mut SimpleTextOutput) };
        con.output_string(b"\r\n========================================\r\n\0");
        con.output_string(b"    Choosable UEFI Bootloader v0.1      \r\n\0");
        con.output_string(b"========================================\r\n\0");
        con.output_string(b"\r\nScanning for bootable images...\r\n\0");
    }

    // Step 1: Get LoadedImageProtocol on our image handle.
    let loaded_image = unsafe { get_protocol::<LoadedImageProtocol>(bs, image_handle, &LOADED_IMAGE_PROTOCOL_GUID) };
    let loaded_image = match loaded_image {
        Some(lip) => lip,
        None => {
            print_error(st, b"ERROR: Failed to get LoadedImageProtocol.\r\n\0");
            print_error(st, b"Boot failed -- halting.\r\n\0");
            halt_or_reboot(st);
        }
    };

    let device_handle = unsafe { (*loaded_image).device_handle };
    if device_handle.is_null() {
        print_error(st, b"ERROR: LoadedImage has no device handle.\r\n\0");
        print_error(st, b"Boot failed -- halting.\r\n\0");
        halt_or_reboot(st);
    }

    // Step 2: Get SimpleFileSystem protocol on the device handle.
    //
    // Some firmware (notably NEC OEM) fails handle_protocol for SFS.
    // Try handle_protocol first, then open_protocol as fallback.
    let fs: *mut core::ffi::c_void = unsafe { get_protocol_raw(bs, device_handle, &SIMPLE_FILE_SYSTEM_GUID) };
    if fs.is_null() {
        print_error(st, b"ERROR: Failed to open filesystem.\r\n\0");
        print_error(st, b"Boot failed -- halting.\r\n\0");
        halt_or_reboot(st);
    }

    print_info(st, b"Filesystem opened successfully!\r\n\0");
    print_info(st, b"Boot menu coming in a future update.\r\n\0");

    halt_or_reboot(st);
}

// ─── Protocol acquisition (two-stage fallback) ──────────────────────────

/// Get a typed protocol interface.
/// Tries handle_protocol first, then open_protocol as fallback.
unsafe fn get_protocol<T>(bs: &mut BootServices, handle: *mut core::ffi::c_void, guid: &Guid) -> Option<*mut T> {
    let ptr = get_protocol_raw(bs, handle, guid);
    if ptr.is_null() { None } else { Some(ptr as *mut T) }
}

/// Get a raw protocol interface pointer (handle_protocol → open_protocol fallback).
unsafe fn get_protocol_raw(
    bs: &mut BootServices,
    handle: *mut core::ffi::c_void,
    guid: &Guid,
) -> *mut core::ffi::c_void {
    // 1. Try handle_protocol (UEFI 2.0+ fast path)
    let mut iface: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = (bs.handle_protocol)(handle, guid, &mut iface);
    if status == 0 && !iface.is_null() {
        return iface;
    }

    // 2. Fallback: open_protocol with GET_PROTOCOL attribute
    //    (works on buggy firmware where handle_protocol is broken for SFS)
    iface = core::ptr::null_mut();
    let agent_handle: *mut core::ffi::c_void = core::ptr::null_mut();
    let controller_handle: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = (bs.open_protocol)(
        handle,
        guid,
        &mut iface,
        IMAGE_HANDLE,
        controller_handle,
        OPEN_PROTOCOL_GET_PROTOCOL,
    );
    if status == 0 && !iface.is_null() {
        return iface;
    }

    core::ptr::null_mut()
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

// We need image_handle at file scope for open_protocol fallback.
static mut IMAGE_HANDLE: *mut core::ffi::c_void = core::ptr::null_mut();

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
    _free_pool: *mut core::ffi::c_void,
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
        *mut core::ffi::c_void,   // Handle
        *const Guid,               // Protocol
        *mut *mut core::ffi::c_void, // Interface
    ) -> usize,
    _reserved: *mut core::ffi::c_void,
    _register_protocol_notify: *mut core::ffi::c_void,
    _locate_handle: *mut core::ffi::c_void,
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
        *mut core::ffi::c_void,   // Handle
        *const Guid,               // Protocol
        *mut *mut core::ffi::c_void, // Interface
        *mut core::ffi::c_void,   // AgentHandle
        *mut core::ffi::c_void,   // ControllerHandle
        u32,                       // Attributes
    ) -> usize,
    close_protocol: unsafe extern "efiapi" fn(
        *mut core::ffi::c_void,
        *const Guid,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
    ) -> usize,
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
    ResetWarm = 1,
    ResetShutdown = 2,
    ResetPlatformSpecific = 3,
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