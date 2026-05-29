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
    pub bs_18: *mut c_void,
    pub bs_20: *mut c_void,
    pub allocate_pages:
        unsafe extern "efiapi" fn(AllocateType, MemoryType, usize, *mut u64) -> usize,
    pub bs_30: *mut c_void,
    pub bs_38: *mut c_void,
    pub bs_40: *mut c_void,
    pub free_pool: unsafe extern "efiapi" fn(*mut c_void) -> usize,
    pub bs_50: *mut c_void,
    pub bs_58: *mut c_void,
    pub bs_60: *mut c_void,
    pub bs_68: *mut c_void,
    pub bs_70: *mut c_void,
    pub bs_78: *mut c_void,
    pub bs_80: *mut c_void,
    pub bs_88: *mut c_void,
    pub bs_90: *mut c_void,
    pub bs_98: *mut c_void,
    pub handle_protocol: unsafe extern "efiapi" fn(
        *mut c_void,
        *const Guid,
        *mut *mut c_void,
    ) -> usize,
    pub bs_a0: *mut c_void,
    pub bs_a8: *mut c_void,
    pub bs_b0: *mut c_void,
    pub bs_b8: *mut c_void,
    pub bs_c0: *mut c_void,
    pub bs_c8: *mut c_void,
    pub bs_d0: *mut c_void,
    pub bs_d8: *mut c_void,
    pub bs_e0: *mut c_void,
    pub bs_e8: *mut c_void,
    pub bs_f0: *mut c_void,
    pub stall: unsafe extern "efiapi" fn(usize) -> usize,
    pub bs_100: *mut c_void,
    pub bs_108: *mut c_void,
    pub bs_110: *mut c_void,
    pub bs_118: *mut c_void,
    pub bs_120: *mut c_void,
    pub bs_128: *mut c_void,
    pub bs_130: *mut c_void,
    pub locate_handle_buffer: unsafe extern "efiapi" fn(
        LocateSearchType,
        *const Guid,
        *mut c_void,
        *mut usize,
        *mut *mut *mut c_void,
    ) -> usize,
    pub bs_140: *mut c_void,
    pub bs_148: *mut c_void,
    pub bs_150: *mut c_void,
    pub bs_158: *mut c_void,
    pub bs_160: *mut c_void,
    pub bs_168: *mut c_void,
    pub bs_170: *mut c_void,
    pub bs_178: *mut c_void,
    pub get_memory_map: unsafe extern "efiapi" fn(
        *mut usize,
        *mut MemoryDescriptor,
        *mut u64,
        *mut u64,
        *mut u32,
    ) -> usize,
    pub bs_188: *mut c_void,
    pub exit_boot_services: unsafe extern "efiapi" fn(*mut c_void, u64) -> usize,
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

pub const BLOCK_IO_PROTOCOL_GUID: Guid = Guid {
    d1: 0x964e5b21,
    d2: 0x6459,
    d3: 0x11d2,
    d4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};