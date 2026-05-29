// ═══════════════════════════════════════════════════════════════════════════
//  Disk handle discovery + GPT partition search
// ═══════════════════════════════════════════════════════════════════════════

use core::ffi::c_void;

use crate::output::{die, print_hex, print_raw};
use crate::protocol::{
    BlockIoProtocol, BootServices, LocateSearchType, SimpleTextOutput, SystemTable,
    BLOCK_IO_PROTOCOL_GUID, EFI_SUCCESS,
};

/// Find the disk handle (BIOS Logical Partition = 0 means physical disk)
pub fn find_disk_handle(
    bs: &mut BootServices,
    _image_handle: *mut c_void,
) -> Option<*mut c_void> {
    let mut num: usize = 0;
    let mut buf: *mut *mut c_void = core::ptr::null_mut();
    if unsafe {
        (bs.locate_handle_buffer)(
            LocateSearchType::ByProtocol,
            &BLOCK_IO_PROTOCOL_GUID,
            core::ptr::null_mut(),
            &mut num,
            &mut buf,
        )
    } != EFI_SUCCESS
        || buf.is_null()
        || num == 0
    {
        return None;
    }

    let handles = unsafe { core::slice::from_raw_parts(buf, num) };
    let mut result: Option<*mut c_void> = None;

    for &h in handles {
        let mut bio: *mut BlockIoProtocol = core::ptr::null_mut();
        if unsafe {
            (bs.handle_protocol)(h, &BLOCK_IO_PROTOCOL_GUID, &mut bio as *mut _ as _)
        } != EFI_SUCCESS
            || bio.is_null()
        {
            continue;
        }
        if unsafe { (*bio).media }.is_null() {
            continue;
        }
        let media = unsafe { &*((*bio).media) };
        if media.bim_lp == 0 {
            result = Some(h);
            break;
        }
    }

    unsafe { (bs.free_pool)(buf as *mut c_void) };
    result
}

/// Given BlockIoProtocol, read an LBA sector into buf
pub fn read_sector(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    lba: u64,
    buf: &mut [u8; 512],
) -> bool {
    unsafe {
        (bio_ref.read_blocks)(bio_ptr, mid, lba, 512, buf.as_mut_ptr() as _)
            == EFI_SUCCESS
    }
}

/// Find a GPT Basic Data partition from the GPT header
pub fn find_gpt_data_partition(
    st: &mut SystemTable,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
) -> u64 {
    let mut hdr_sec: [u8; 512] = [0; 512];
    if !read_sector(bio_ref, bio_ptr, mid, 1, &mut hdr_sec) {
        return 0;
    }
    if &hdr_sec[0..8] != b"EFI PART" {
        return 0;
    }
    let entries_lba = u64::from_le_bytes(hdr_sec[72..80].try_into().unwrap());
    let n = u32::from_le_bytes(hdr_sec[80..84].try_into().unwrap());
    let sz = u32::from_le_bytes(hdr_sec[84..88].try_into().unwrap());
    if sz == 0 || n == 0 {
        return 0;
    }

    let basic_data_guid: [u8; 16] = [
        0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99,
        0xC7,
    ];

    let mut sec: [u8; 512] = [0; 512];
    let mut current_lba: u64 = 0;
    let mut loaded = false;
    for i in 0..n.min(128) {
        let eoff = i as usize * sz as usize;
        let lba = entries_lba + (eoff / 512) as u64;
        let boff = eoff % 512;
        if boff + 40 > 512 {
            continue;
        }
        if !loaded || lba != current_lba {
            if !read_sector(bio_ref, bio_ptr, mid, lba, &mut sec) {
                break;
            }
            current_lba = lba;
            loaded = true;
        }
        if sec[boff..boff + 16] == basic_data_guid {
            let start_lba =
                u64::from_le_bytes(sec[boff + 32..boff + 40].try_into().unwrap());
            print_raw(st, b"Found GPT Basic Data at LBA ");
            print_hex(st, b"0x", start_lba);
            print_raw(st, b"\r\n\0");
            return start_lba;
        }
    }
    0
}