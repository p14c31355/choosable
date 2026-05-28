#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// UEFI entry point
#[no_mangle]
extern "efiapi" fn efi_main(
    image_handle: *mut core::ffi::c_void,
    system_table: *mut SystemTable,
) -> usize {
    let st = unsafe { &mut *system_table };
    let con = unsafe { &mut *(st.con_out as *mut SimpleTextOutput) };

    // Print banner
    con.output_string(b"\r\nChoosable UEFI Bootloader\r\n\0");
    con.output_string(b"Scanning for ISO files...\r\n\0");

    // Scan filesystem and show menu
    show_boot_menu(st, image_handle, con);

    0
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop { unsafe { core::arch::asm!("hlt") } }
}

// ─── UEFI Type Definitions (manual, no uefi crate) ─────────────────────

#[repr(C)]
struct SystemTable {
    hdr: TableHeader,
    firmware_vendor: *const u16,
    firmware_revision: u32,
    _console_in_handle: *mut core::ffi::c_void,
    con_in: *mut core::ffi::c_void,  // SimpleTextInput *
    _console_out_handle: *mut core::ffi::c_void,
    con_out: *mut core::ffi::c_void,  // SimpleTextOutput *
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
struct BootServices {
    hdr: TableHeader,
    _raise_tpl: *mut core::ffi::c_void,
    _restore_tpl: *mut core::ffi::c_void,
    allocate_pages: unsafe extern "efiapi" fn(AllocateType, MemoryType, usize, *mut u64) -> usize,
    free_pages: *mut core::ffi::c_void,
    get_memory_map: unsafe extern "efiapi" fn(*mut usize, *mut MemoryDescriptor, *mut usize, *mut usize, *mut u32) -> usize,
    allocate_pool: unsafe extern "efiapi" fn(MemoryType, usize, *mut *mut core::ffi::c_void) -> usize,
    free_pool: *mut core::ffi::c_void,
    create_event: *mut core::ffi::c_void,
    set_timer: *mut core::ffi::c_void,
    wait_for_event: *mut core::ffi::c_void,
    signal_event: *mut core::ffi::c_void,
    close_event: *mut core::ffi::c_void,
    check_event: *mut core::ffi::c_void,
    install_protocol_interface: *mut core::ffi::c_void,
    reinstall_protocol_interface: *mut core::ffi::c_void,
    uninstall_protocol_interface: *mut core::ffi::c_void,
    handle_protocol: unsafe extern "efiapi" fn(*mut core::ffi::c_void, *const Guid, *mut *mut core::ffi::c_void) -> usize,
    _reserved: *mut core::ffi::c_void,
    register_protocol_notify: *mut core::ffi::c_void,
    locate_handle: *mut core::ffi::c_void,
    locate_device_path: *mut core::ffi::c_void,
    install_configuration_table: *mut core::ffi::c_void,
    image_load: *mut core::ffi::c_void,
    image_start: *mut core::ffi::c_void,
    exit: *mut core::ffi::c_void,
    image_unload: *mut core::ffi::c_void,
    exit_boot_services: unsafe extern "efiapi" fn(*mut core::ffi::c_void, usize) -> usize,
    get_next_monotonic_count: *mut core::ffi::c_void,
    stall: unsafe extern "efiapi" fn(usize) -> usize,
    set_watchdog_timer: *mut core::ffi::c_void,
    connect_controller: *mut core::ffi::c_void,
    disconnect_controller: *mut core::ffi::c_void,
    open_protocol: *mut core::ffi::c_void,
    close_protocol: *mut core::ffi::c_void,
    open_protocol_information: *mut core::ffi::c_void,
    protocols_per_handle: *mut core::ffi::c_void,
    locate_handle_buffer: *mut core::ffi::c_void,
    locate_protocol: unsafe extern "efiapi" fn(*const Guid, *mut core::ffi::c_void, *mut *mut core::ffi::c_void) -> usize,
    install_multiple_protocol_interfaces: *mut core::ffi::c_void,
    uninstall_multiple_protocol_interfaces: *mut core::ffi::c_void,
    calculate_crc32: *mut core::ffi::c_void,
    copy_mem: *mut core::ffi::c_void,
    set_mem: *mut core::ffi::c_void,
    create_event_ex: *mut core::ffi::c_void,
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
    _reset_system: *mut core::ffi::c_void,
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
enum AllocateType {
    AllocateAnyPages = 0,
    AllocateMaxAddress = 1,
    AllocateAddress = 2,
}

#[derive(Clone, Copy)]
#[repr(u32)]
enum MemoryType {
    EfiLoaderData = 1,
    EfiLoaderCode = 4,
    EfiBootServicesData = 2,
    EfiBootServicesCode = 3,
    EfiRuntimeServicesData = 5,
    EfiRuntimeServicesCode = 6,
    EfiConventionalMemory = 7,
}

// ─── UEFI Protocol GUIDs ────────────────────────────────────────────────

/// EFI_LOADED_IMAGE_PROTOCOL_GUID
/// {5B1B31A1-9562-11D2-8E3F-00A0C969723B}
const LOADED_IMAGE_PROTOCOL_GUID: Guid = Guid {
    data1: 0x5B1B31A1,
    data2: 0x9562,
    data3: 0x11D2,
    data4: [0x8E, 0x3F, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B],
};

/// EFI_SIMPLE_FILE_SYSTEM_PROTOCOL_GUID
/// {0964E5B2-6459-11D2-8E39-00A0C969723B}
const SIMPLE_FILE_SYSTEM_GUID: Guid = Guid {
    data1: 0x0964e5b2,
    data2: 0x6459,
    data3: 0x11d2,
    data4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};

// ─── EFI_LOADED_IMAGE_PROTOCOL ──────────────────────────────────────────

/// Minimal definition — we only need DeviceHandle (offset 0x18 → 5th pointer-sized field).
#[repr(C)]
struct LoadedImageProtocol {
    _revision: u32,
    _parent_handle: *mut core::ffi::c_void,
    _system_table: *mut core::ffi::c_void,
    // device_handle: fourth pointer-sized field
    device_handle: *mut core::ffi::c_void,
}

// ─── SimpleTextOutput helpers ───────────────────────────────────────────

impl SimpleTextOutput {
    fn output_string(&mut self, s: &[u8]) {
        // UEFI strings are UTF-16. Convert ASCII to wide chars on the fly.
        let mut buf = [0u16; 256];
        let len = s.len().min(255);
        for (i, &b) in s[..len].iter().enumerate() {
            buf[i] = b as u16;
        }
        buf[len] = 0;
        unsafe { (self.output_string)(self as *mut Self, buf.as_ptr()) };
    }
}

// ─── Boot Menu ──────────────────────────────────────────────────────────

fn show_boot_menu(
    st: &mut SystemTable,
    image_handle: *mut core::ffi::c_void,
    con: &mut SimpleTextOutput,
) {
    let bs = unsafe { &mut *st.boot_services };

    // Step 1: Get LoadedImageProtocol on our image handle to find the
    //         device handle that our EFI binary was loaded from.
    let mut loaded_image: *mut LoadedImageProtocol = core::ptr::null_mut();
    let status = unsafe {
        (bs.handle_protocol)(
            image_handle,
            &LOADED_IMAGE_PROTOCOL_GUID,
            &mut loaded_image as *mut _ as *mut *mut core::ffi::c_void,
        )
    };
    if status != 0 || loaded_image.is_null() {
        con.output_string(b"Failed to get LoadedImageProtocol.\r\n\0");
        return;
    }

    let device_handle = unsafe { (*loaded_image).device_handle };
    if device_handle.is_null() {
        con.output_string(b"LoadedImage has no device handle.\r\n\0");
        return;
    }

    // Step 2: Get SimpleFileSystem protocol on the device handle.
    //         The image_handle itself does NOT have SFS installed —
    //         only the device handle (the partition block device) does.
    let mut fs: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.handle_protocol)(device_handle, &SIMPLE_FILE_SYSTEM_GUID, &mut fs)
    };
    if status != 0 || fs.is_null() {
        con.output_string(b"Failed to open file system.\r\n\0");
        return;
    }

    con.output_string(b"Filesystem opened. Boot menu coming soon.\r\n\0");
}