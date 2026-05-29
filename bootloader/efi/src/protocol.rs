// ═══════════════════════════════════════════════════════════════════════════
//  UEFI Protocol types (minimal hand-rolled)
// ═══════════════════════════════════════════════════════════════════════════

use core::ffi::c_void;

pub const EFI_SUCCESS: usize = 0;
pub const EFI_BUFFER_TOO_SMALL: usize = 0x8000000000000005usize;

#[repr(C)]
pub struct TableHeader {
    pub sig: u64,
    pub rev: u32,
    pub hsz: u32,
    pub crc32: u32,
    pub reserved: u32,
}

#[repr(C)]
pub struct SystemTable {
    pub hdr: TableHeader,
    pub fw: *const u16,
    pub rev: u32,
    pub pad_st0: u32,
    pub cih: *mut c_void,
    pub con_in: *mut c_void,
    pub coh: *mut c_void,
    pub con_out: *mut c_void,
    pub seh: *mut c_void,
    pub stderr: *mut c_void,
    pub runtime_services: *mut RuntimeServices,
    pub boot_services: *mut BootServices,
    pub nte: usize,
    pub ct: *mut c_void,
}

#[repr(C)]
pub struct BootServices {
    pub hdr: TableHeader,
    // ── 0x018–0x020 ──
    pub raise_tpl: *mut c_void,         // 0x018
    pub restore_tpl: *mut c_void,       // 0x020
    // ── 0x028 ──
    pub allocate_pages:
        unsafe extern "efiapi" fn(AllocateType, MemoryType, usize, *mut u64) -> usize,
    // ── 0x030 ──
    pub free_pages: *mut c_void,        // 0x030
    // ── 0x038 ──
    pub get_memory_map: unsafe extern "efiapi" fn(
        *mut usize,
        *mut MemoryDescriptor,
        *mut u64,
        *mut u64,
        *mut u32,
    ) -> usize,
    // ── 0x040 ──
    pub allocate_pool: unsafe extern "efiapi" fn(MemoryType, usize, *mut *mut c_void) -> usize,
    // ── 0x048 ──
    pub free_pool: unsafe extern "efiapi" fn(*mut c_void) -> usize,
    // ── 0x050–0x078 ──
    pub create_event: *mut c_void,      // 0x050
    pub set_timer: *mut c_void,         // 0x058
    pub wait_for_event: *mut c_void,    // 0x060
    pub signal_event: *mut c_void,      // 0x068
    pub close_event: *mut c_void,       // 0x070
    pub check_event: *mut c_void,       // 0x078
    // ── 0x080–0x098 ──
    pub install_protocol_interface: *mut c_void,       // 0x080
    pub reinstall_protocol_interface: *mut c_void,     // 0x088
    pub uninstall_protocol_interface: *mut c_void,     // 0x090
    pub handle_protocol: unsafe extern "efiapi" fn(    // 0x098
        *mut c_void,
        *const Guid,
        *mut *mut c_void,
    ) -> usize,
    // ── 0x0A0 ──
    pub reserved: *mut c_void,          // 0x0A0 (must point to NULL)
    // ── 0x0A8–0x0B0 ──
    pub register_protocol_notify: *mut c_void,  // 0x0A8
    pub locate_handle: *mut c_void,              // 0x0B0
    // ── 0x0B8–0x0E8 ──
    pub locate_device_path: *mut c_void,        // 0x0B8
    pub install_configuration_table: *mut c_void, // 0x0C0
    pub load_image: unsafe extern "efiapi" fn(  // 0x0C8
        bool,                       // BootPolicy
        *mut c_void,                // ParentImageHandle
        *mut crate::protocol::DevicePathProtocol, // DevicePath
        *const u8,                  // SourceBuffer
        u64,                        // SourceSize
        *mut *mut c_void,           // ImageHandle
    ) -> usize,
    pub start_image: unsafe extern "efiapi" fn(
        *mut c_void,                // ImageHandle
        *mut u64,                   // ExitDataSize
        *mut *mut u16,              // ExitData
    ) -> usize,
    pub exit: *mut c_void,                       // 0x0D8
    pub unload_image: *mut c_void,               // 0x0E0
    pub exit_boot_services: unsafe extern "efiapi" fn(*mut c_void, u64) -> usize, // 0x0E8
    // ── 0x0F0–0x0F8 ──
    pub get_next_monotonic_count: *mut c_void,  // 0x0F0
    pub stall: unsafe extern "efiapi" fn(usize) -> usize,  // 0x0F8
    // ── 0x100–0x138 ──
    pub set_watchdog_timer: *mut c_void,        // 0x100
    pub connect_controller: *mut c_void,         // 0x108
    pub disconnect_controller: *mut c_void,      // 0x110
    pub open_protocol: *mut c_void,              // 0x118
    pub close_protocol: *mut c_void,             // 0x120
    pub open_protocol_information: *mut c_void,  // 0x128
    pub protocols_per_handle: *mut c_void,       // 0x130
    pub locate_handle_buffer: unsafe extern "efiapi" fn(    // 0x138
        LocateSearchType,
        *const Guid,
        *mut c_void,
        *mut usize,
        *mut *mut *mut c_void,
    ) -> usize,
    // ── 0x140–0x168 ──
    pub locate_protocol: *mut c_void,                      // 0x140
    pub install_multiple_protocol_interfaces: *mut c_void, // 0x148
    pub uninstall_multiple_protocol_interfaces: *mut c_void, // 0x150
    pub calculate_crc32: *mut c_void,                      // 0x158
    pub copy_mem: *mut c_void,                             // 0x160
    pub set_mem: *mut c_void,                              // 0x168
    pub create_event_ex: *mut c_void,                      // 0x170
}

#[repr(C)]
pub struct RuntimeServices {
    pub hdr: TableHeader,
    pub rs_18: *mut c_void,
    pub rs_20: *mut c_void,
    pub rs_28: *mut c_void,
    pub rs_30: *mut c_void,
    pub rs_38: *mut c_void,
    pub rs_40: *mut c_void,
    pub rs_48: *mut c_void,
    pub rs_50: *mut c_void,
    pub rs_58: *mut c_void,
    pub rs_60: *mut c_void,
    pub reset_system:
        unsafe extern "efiapi" fn(ResetType, usize, usize, *mut c_void) -> !,
}

#[repr(C)]
pub struct SimpleTextOutput {
    pub sto_reset: unsafe extern "efiapi" fn(*mut Self, bool) -> usize,
    pub output_string: unsafe extern "efiapi" fn(*mut Self, *const u16) -> usize,
    pub sto_p10: *mut c_void,
    pub sto_p18: *mut c_void,
    pub sto_p20: *mut c_void,
    pub sto_p28: *mut c_void,
    pub sto_clear_screen: unsafe extern "efiapi" fn(*mut Self) -> usize,
    pub sto_p38: *mut c_void,
    pub sto_p40: *mut c_void,
    pub sto_p48: *mut c_void,
}

#[repr(C)]
pub struct SimpleTextInput {
    pub sti_reset: unsafe extern "efiapi" fn(*mut Self, bool) -> usize,
    pub read_key_stroke: unsafe extern "efiapi" fn(*mut Self, *mut Key) -> usize,
    pub sti_wait: *mut c_void,
}

#[repr(C)]
pub struct Key {
    pub sc: u16,
    pub uc: u16,
}

#[repr(C)]
pub struct BlockIoProtocol {
    pub bio_rev: u64,
    pub media: *mut BlockIoMedia,
    pub bio_rst: *mut c_void,
    pub read_blocks:
        unsafe extern "efiapi" fn(*mut Self, u32, u64, usize, *mut c_void) -> usize,
    pub bio_w: *mut c_void,
    pub bio_f: *mut c_void,
}

#[repr(C)]
pub struct BlockIoMedia {
    pub mid: u32,
    pub bim_rm: u8,
    pub bim_mp: u8,
    pub bim_lp: u8,
    pub bim_ro: u8,
    pub bim_wc: u8,
    pub bim_bs: u32,
    pub bim_ia: u32,
    pub bim_lb: u64,
}

#[repr(C)]
pub struct Guid {
    pub d1: u32,
    pub d2: u16,
    pub d3: u16,
    pub d4: [u8; 8],
}

#[repr(C)]
pub struct MemoryDescriptor {
    pub ty: u32,
    pub pad: u32,
    pub phys_start: u64,
    pub virt_start: u64,
    pub num_pages: u64,
    pub attr: u64,
}

#[derive(Clone, Copy)]
#[repr(u32)]
pub enum ResetType {
    ResetCold = 0,
    ResetWarm = 1,
}

#[derive(Clone, Copy)]
#[repr(u32)]
pub enum LocateSearchType {
    ByProtocol = 2,
}

#[derive(Clone, Copy)]
#[repr(u32)]
pub enum AllocateType {
    AllocateAnyPages = 0,
    AllocateAddress = 2,
}

#[derive(Clone, Copy)]
#[repr(u32)]
pub enum MemoryType {
    EfiLoaderData = 1,
}

/// Opaque device path type — only needed as a pointer in LoadImage.
#[repr(C)]
pub struct DevicePathProtocol {
    pub ty: u8,
    pub sub_type: u8,
    pub length: [u8; 2],
}

pub const BLOCK_IO_PROTOCOL_GUID: Guid = Guid {
    d1: 0x964e5b21,
    d2: 0x6459,
    d3: 0x11d2,
    d4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};