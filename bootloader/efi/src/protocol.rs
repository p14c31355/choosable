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
    pub install_protocol_interface: unsafe extern "efiapi" fn(  // 0x080
        *mut *mut c_void,          // Handle
        *const Guid,               // Protocol GUID
        usize,                     // InterfaceType (0 = EFI_NATIVE_INTERFACE)
        *mut c_void,               // Interface
    ) -> usize,
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
    pub set_watchdog_timer:
        unsafe extern "efiapi" fn(usize, u64, usize, *const u16) -> usize,  // 0x100
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

/// EFI_LOADED_IMAGE_PROTOCOL — the protocol installed on every loaded image.
#[repr(C)]
pub struct LoadedImageProtocol {
    pub revision: u32,
    pub parent_handle: *mut c_void,
    pub system_table: *mut SystemTable,
    // ── device location ──
    pub device_handle: *mut c_void,
    pub file_path: *mut c_void,    // DevicePathProtocol *
    pub _reserved: *mut c_void,
    // ── image load options ──
    pub load_options_size: u32,
    pub load_options: *mut c_void,
    // ── image location ──
    pub image_base: *mut c_void,
    pub image_size: u64,
    pub image_code_type: u32,
    pub image_data_type: u32,
    pub unload: *mut c_void,
}

pub const LOADED_IMAGE_PROTOCOL_GUID: Guid = Guid {
    d1: 0x5B1B31A1,
    d2: 0x9562,
    d3: 0x11d2,
    d4: [0x8E, 0x3F, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B],
};

/// Virtual Block I/O context — wraps ISO file data for CD-ROM emulation.
#[repr(C)]
pub struct VirtualBlockIo {
    pub protocol: BlockIoProtocol,
    pub media: BlockIoMedia,
    /// ISO file start LBA (absolute disk sector)
    pub iso_lba: u64,
    /// Underlying real Block I/O protocol pointer
    pub real_bio_ptr: *mut BlockIoProtocol,
    /// Underlying media ID
    pub real_media_id: u32,
}

pub const DEVICE_PATH_PROTOCOL_GUID: Guid = Guid {
    d1: 0x09576e91,
    d2: 0x6d3f,
    d3: 0x11d2,
    d4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};

/// ── EFI_SIMPLE_FILE_SYSTEM_PROTOCOL ───────────────────────────────────

pub const SIMPLE_FILE_SYSTEM_PROTOCOL_GUID: Guid = Guid {
    d1: 0x0964e5b22,
    d2: 0x6459,
    d3: 0x11d2,
    d4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};

pub const FILE_INFO_GUID: Guid = Guid {
    d1: 0x09576e92,
    d2: 0x6d3f,
    d3: 0x11d2,
    d4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};

#[repr(C)]
pub struct SimpleFileSystemProtocol {
    pub revision: u64,
    pub open_volume:
        unsafe extern "efiapi" fn(*mut SimpleFileSystemProtocol, *mut *mut FileProtocol) -> usize,
}

#[repr(C)]
pub struct FileProtocol {
    pub revision: u64,
    pub open: unsafe extern "efiapi" fn(
        *mut FileProtocol,
        *mut *mut FileProtocol,
        *const u16, // FileName (UCS-2)
        u64,         // OpenMode
        u64,         // Attributes
    ) -> usize,
    pub close: unsafe extern "efiapi" fn(*mut FileProtocol) -> usize,
    pub delete: unsafe extern "efiapi" fn(*mut FileProtocol) -> usize,
    pub read: unsafe extern "efiapi" fn(
        *mut FileProtocol,
        *mut usize,  // BufferSize
        *mut c_void, // Buffer
    ) -> usize,
    pub write: unsafe extern "efiapi" fn(
        *mut FileProtocol,
        *mut usize,
        *mut c_void,
    ) -> usize,
    pub get_position: unsafe extern "efiapi" fn(*mut FileProtocol, *mut u64) -> usize,
    pub set_position: unsafe extern "efiapi" fn(*mut FileProtocol, u64) -> usize,
    pub get_info: unsafe extern "efiapi" fn(
        *mut FileProtocol,
        *const Guid,  // InformationType
        *mut usize,   // BufferSize
        *mut c_void,  // Buffer
    ) -> usize,
    pub set_info: unsafe extern "efiapi" fn(
        *mut FileProtocol,
        *const Guid,
        usize,
        *mut c_void,
    ) -> usize,
    pub flush: unsafe extern "efiapi" fn(*mut FileProtocol) -> usize,
}

#[repr(C, packed)]
pub struct EfiFileInfo {
    pub size: u64,
    pub file_size: u64,
    pub physical_size: u64,
    pub create_time: EfiTime,
    pub last_access_time: EfiTime,
    pub modification_time: EfiTime,
    pub attribute: u64,
    // followed by: FileName (UCS-2 null-terminated)
}

#[repr(C, packed)]
pub struct EfiTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub _pad1: u8,
    pub nanosecond: u32,
    pub timezone: i16,
    pub daylight: u8,
    pub _pad2: u8,
}

/// Additional EFI_STATUS codes used by file protocols
pub const EFI_NOT_FOUND: usize            = 0x80000000_0000000Eusize;
pub const EFI_INVALID_PARAMETER: usize    = 0x80000000_00000002usize;
pub const EFI_UNSUPPORTED: usize          = 0x80000000_00000003usize;
pub const EFI_BAD_BUFFER_SIZE: usize      = 0x80000000_00000004usize;
pub const EFI_DEVICE_ERROR: usize         = 0x80000000_00000007usize;
pub const EFI_NO_MEDIA: usize             = 0x80000000_00000014usize;
pub const EFI_WRITE_PROTECTED: usize      = 0x80000000_00000011usize;

pub const BLOCK_IO_PROTOCOL_GUID: Guid = Guid {
    d1: 0x964e5b21,
    d2: 0x6459,
    d3: 0x11d2,
    d4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};