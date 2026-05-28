#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// UEFI entry point — NEVER returns.
#[no_mangle]
extern "efiapi" fn efi_main(
    _image_handle: *mut core::ffi::c_void,
    system_table: *mut SystemTable,
) -> ! {
    let st = unsafe { &mut *system_table };
    let bs = unsafe { &mut *st.boot_services };

    if !st.con_out.is_null() {
        let con = unsafe { &mut *(st.con_out as *mut SimpleTextOutput) };
        con.output_string(b"\r\n========================================\r\n\0");
        con.output_string(b"    Choosable UEFI Bootloader v0.4      \r\n\0");
        con.output_string(b"========================================\r\n\0");
    }

    // ── Use LocateProtocol to find the SimpleFileSystem instance ──
    //    LocateProtocol searches the entire handle database —
    //    no need to go through LoadedImageProtocol → DeviceHandle.
    let mut fs_iface: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = unsafe {
        (bs.locate_protocol)(
            &SIMPLE_FILE_SYSTEM_GUID,
            core::ptr::null_mut(),   // Registration (optional)
            &mut fs_iface,
        )
    };

    if status != 0 || fs_iface.is_null() {
        print_hex(st, b"ERROR: LocateProtocol(SFS) returned 0x", status as u64);
        print_raw(st, b"\r\nBoot failed -- halting.\r\n\0");
        halt_or_reboot(st);
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
    let mut buf = [0u8; 18]; // "0x" + 16 hex digits
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        let shift = (15 - i) * 4;
        buf[2 + i] = hex[((val >> shift) & 0xF) as usize];
    }
    print_raw(st, &buf);
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

// ═══════════════════════════════════════════════════════════════════
//  UEFI Type Definitions
//
//  CRITICAL: These structs are laid out to match the UEFI spec
//  (x86_64 ABI).  Every pointer-sized field is explicitly listed
//  with `_padNN` placeholders so offsets never drift.
// ═══════════════════════════════════════════════════════════════════

#[repr(C)]
struct SystemTable {
    hdr:                  TableHeader,            // 0x00
    firmware_vendor:      *const u16,             // 0x18
    firmware_revision:    u32,                    // 0x20
    _pad_24:              u32,                    // 0x24 (align to 8)
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
    // Offset 0x00
    _reset:         unsafe extern "efiapi" fn(*mut Self, bool) -> usize,
    // Offset 0x08
    output_string:  unsafe extern "efiapi" fn(*mut Self, *const u16) -> usize,
    // Offset 0x10
    _test_string:   *mut core::ffi::c_void,
    // Offset 0x18
    _query_mode:    *mut core::ffi::c_void,
    // Offset 0x20
    _set_mode:      *mut core::ffi::c_void,
    // Offset 0x28
    _set_attribute: *mut core::ffi::c_void,
    // Offset 0x30
    _clear_screen:  unsafe extern "efiapi" fn(*mut Self) -> usize,
    // Offset 0x38
    _set_cursor:    *mut core::ffi::c_void,
    // Offset 0x40
    _enable_cursor: *mut core::ffi::c_void,
    // Offset 0x48
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

/// BootServices table — offsets must EXACTLY match UEFI spec 4.4.
/// Every field is listed in order; unused fields use `_padNN`.
#[repr(C)]
struct BootServices {
    // ── 0x00 ─────────────────────────────────────────────────────
    hdr: TableHeader,  // 24 bytes → 0x00..0x18

    // ── 0x18 ─────────────────────────────────────────────────────
    _pad18: *mut core::ffi::c_void,  // RaiseTPL
    _pad20: *mut core::ffi::c_void,  // RestoreTPL
    _pad28: *mut core::ffi::c_void,  // AllocatePages
    _pad30: *mut core::ffi::c_void,  // FreePages
    _pad38: *mut core::ffi::c_void,  // GetMemoryMap
    _pad40: *mut core::ffi::c_void,  // AllocatePool
    _pad48: *mut core::ffi::c_void,  // FreePool
    _pad50: *mut core::ffi::c_void,  // CreateEvent
    _pad58: *mut core::ffi::c_void,  // SetTimer
    _pad60: *mut core::ffi::c_void,  // WaitForEvent
    _pad68: *mut core::ffi::c_void,  // SignalEvent
    _pad70: *mut core::ffi::c_void,  // CloseEvent
    _pad78: *mut core::ffi::c_void,  // CheckEvent
    _pad80: *mut core::ffi::c_void,  // InstallProtocolInterface
    _pad88: *mut core::ffi::c_void,  // ReinstallProtocolInterface
    _pad90: *mut core::ffi::c_void,  // UninstallProtocolInterface

    // ── 0x98: HandleProtocol ────────────────────────────────────
    handle_protocol: unsafe extern "efiapi" fn(
        *mut core::ffi::c_void,      // Handle
        *const Guid,                  // Protocol
        *mut *mut core::ffi::c_void,  // Interface
    ) -> usize,

    // ── 0xA0..0xF0 ──────────────────────────────────────────────
    _pada0: *mut core::ffi::c_void,  // Reserved
    _pada8: *mut core::ffi::c_void,  // RegisterProtocolNotify
    _padb0: *mut core::ffi::c_void,  // LocateHandle
    _padb8: *mut core::ffi::c_void,  // LocateDevicePath
    _padc0: *mut core::ffi::c_void,  // InstallConfigurationTable
    _padc8: *mut core::ffi::c_void,  // LoadImage
    _padd0: *mut core::ffi::c_void,  // StartImage
    _padd8: *mut core::ffi::c_void,  // Exit
    _pade0: *mut core::ffi::c_void,  // UnloadImage
    _pade8: *mut core::ffi::c_void,  // ExitBootServices
    _padf0: *mut core::ffi::c_void,  // GetNextMonotonicCount

    // ── 0xF8: Stall ─────────────────────────────────────────────
    stall: unsafe extern "efiapi" fn(usize) -> usize,

    // ── 0x100..0x138 ────────────────────────────────────────────
    _pad100: *mut core::ffi::c_void, // SetWatchdogTimer
    _pad108: *mut core::ffi::c_void, // ConnectController
    _pad110: *mut core::ffi::c_void, // DisconnectController
    _pad118: *mut core::ffi::c_void, // OpenProtocol
    _pad120: *mut core::ffi::c_void, // CloseProtocol
    _pad128: *mut core::ffi::c_void, // OpenProtocolInformation
    _pad130: *mut core::ffi::c_void, // ProtocolsPerHandle
    _pad138: *mut core::ffi::c_void, // LocateHandleBuffer

    // ── 0x140: LocateProtocol ───────────────────────────────────
    locate_protocol: unsafe extern "efiapi" fn(
        *const Guid,                  // Protocol
        *mut core::ffi::c_void,       // Registration (can be NULL)
        *mut *mut core::ffi::c_void,  // Interface
    ) -> usize,

    // ── 0x148+ ──────────────────────────────────────────────────
    _pad148: *mut core::ffi::c_void, // InstallMultipleProtocols
    _pad150: *mut core::ffi::c_void, // UninstallMultipleProtocols
    _pad158: *mut core::ffi::c_void, // CalculateCrc32
    _pad160: *mut core::ffi::c_void, // CopyMem
    _pad168: *mut core::ffi::c_void, // SetMem
    _pad170: *mut core::ffi::c_void, // CreateEventEx
}

#[repr(C)]
struct RuntimeServices {
    hdr: TableHeader,
    _pad18: *mut core::ffi::c_void, // GetTime
    _pad20: *mut core::ffi::c_void, // SetTime
    _pad28: *mut core::ffi::c_void, // GetWakeupTime
    _pad30: *mut core::ffi::c_void, // SetWakeupTime
    _pad38: *mut core::ffi::c_void, // SetVirtualAddressMap
    _pad40: *mut core::ffi::c_void, // ConvertPointer
    _pad48: *mut core::ffi::c_void, // GetVariable
    _pad50: *mut core::ffi::c_void, // GetNextVariableName
    _pad58: *mut core::ffi::c_void, // SetVariable
    _pad60: *mut core::ffi::c_void, // GetNextHighMonoCount
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

const SIMPLE_FILE_SYSTEM_GUID: Guid = Guid {
    data1: 0x0964e5b2,
    data2: 0x6459,
    data3: 0x11d2,
    data4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};

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