// ═══════════════════════════════════════════════════════════════════════════
//  Disk handle discovery + GPT partition search
// ═══════════════════════════════════════════════════════════════════════════

use core::ffi::c_void;

use crate::output::{die, print_dec, print_hex, print_raw};
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

/// Find a GPT Basic Data partition from the GPT header.
/// Returns (start_lba, partition_guid, partition_number) on success, or None.
pub fn find_gpt_data_partition(
    st: &mut SystemTable,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
) -> Option<(u64, crate::protocol::Guid, u32)> {
    let basic_data_guid: [u8; 16] = [
        0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99,
        0xC7,
    ];

    read_gpt_entries_with_index(bio_ref, bio_ptr, mid, &mut |entry_index, boff, sec| {
        if sec[boff..boff + 16] == basic_data_guid {
            let start_lba = u64::from_le_bytes(sec[boff + 32..boff + 40].try_into().unwrap());
            let part_guid = read_partition_guid(sec, boff);
            print_raw(st, b"Found GPT Basic Data at LBA ");
            print_hex(st, b"0x", start_lba);
            print_raw(st, b"\r\n\0");
            Some((start_lba, part_guid, entry_index + 1))
        } else {
            None
        }
    })
}

/// Find a GPT partition by its start LBA (matches the already-known part1_lba).
/// Returns (partition_guid, partition_number) on success.
/// The partition_number is the real GPT entry index (1-based), not a compacted count.
pub fn find_partition_by_lba(
    st: &mut SystemTable,
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    target_lba: u64,
) -> Option<(crate::protocol::Guid, u32)> {
    read_gpt_entries_with_index(bio_ref, bio_ptr, mid, &mut |entry_index, boff, sec| {
        let start_lba = u64::from_le_bytes(sec[boff + 32..boff + 40].try_into().unwrap());
        if start_lba == target_lba {
            let part_guid = read_partition_guid(sec, boff);
            let part_number = entry_index + 1;
            print_raw(st, b"Found partition by LBA ");
            print_hex(st, b"0x", start_lba);
            print_raw(st, b" number=");
            print_dec(st, part_number as u64);
            print_raw(st, b"\r\n\0");
            Some((part_guid, part_number))
        } else {
            None
        }
    })
}

/// Iterate over GPT partition entries with the raw entry index (0-based).
/// Returns the first `Some` result from `f`, or `None` if none match.
fn read_gpt_entries_with_index<T>(
    bio_ref: &BlockIoProtocol,
    bio_ptr: *mut BlockIoProtocol,
    mid: u32,
    f: &mut impl FnMut(u32, usize, &[u8; 512]) -> Option<T>,
) -> Option<T> {
    // Read GPT header at LBA 1
    let mut hdr_sec: [u8; 512] = [0; 512];
    if !read_sector(bio_ref, bio_ptr, mid, 1, &mut hdr_sec) { return None; }
    if &hdr_sec[0..8] != b"EFI PART" { return None; }
    let entries_lba = u64::from_le_bytes(hdr_sec[72..80].try_into().unwrap());
    let n = u32::from_le_bytes(hdr_sec[80..84].try_into().unwrap());
    let sz = u32::from_le_bytes(hdr_sec[84..88].try_into().unwrap());
    if sz == 0 || n == 0 { return None; }

    let mut sec: [u8; 512] = [0; 512];
    let mut current_lba: u64 = 0;
    let mut loaded = false;
    for i in 0..n.min(128) {
        let eoff = i as usize * sz as usize;
        let lba = entries_lba + (eoff / 512) as u64;
        let boff = eoff % 512;
        if boff + 40 > 512 { continue; }
        if !loaded || lba != current_lba {
            if !read_sector(bio_ref, bio_ptr, mid, lba, &mut sec) { break; }
            current_lba = lba;
            loaded = true;
        }
        // Check if this is a valid partition entry (non-zero type GUID)
        let mut is_zero = true;
        for j in 0..16 {
            if sec[boff + j] != 0 { is_zero = false; break; }
        }
        if is_zero { continue; }

        if let Some(result) = f(i, boff, &sec) {
            return Some(result);
        }
    }
    None
}

/// Read MBR disk signature and convert to GUID for PARTUUID matching.
/// Linux MBR PARTUUID convention:
///   - 4-byte disk signature at offset 0x1B8 (bytes 440-443)
///   - Partition number at offset 0x1BC (typically 2 hex digits)
///   - Formatted as: <sig_hex_le>-<partnum_hex> e.g. "aabbccdd-01"
/// For GUID representation, map the 4-byte signature into d1 and partition into d4[4..8].
pub fn read_mbr_guid(mbr: &[u8; 512]) -> crate::protocol::Guid {
    let disk_sig = u32::from_le_bytes(mbr[440..444].try_into().unwrap());
    let part_num = mbr[446]; // First partition entry starts at 446, partition number implicit as 1
    crate::protocol::Guid {
        d1: disk_sig,
        d2: 0,
        d3: 0,
        d4: {
            let mut d4 = [0u8; 8];
            d4[4] = part_num;
            d4
        },
    }
}

/// Read the unique partition GUID from a GPT entry at byte offset boff in sec.
fn read_partition_guid(sec: &[u8; 512], boff: usize) -> crate::protocol::Guid {
    crate::protocol::Guid {
        d1: u32::from_le_bytes(sec[boff + 16..boff + 20].try_into().unwrap()),
        d2: u16::from_le_bytes(sec[boff + 20..boff + 22].try_into().unwrap()),
        d3: u16::from_le_bytes(sec[boff + 22..boff + 24].try_into().unwrap()),
        d4: {
            let mut d4 = [0u8; 8];
            d4.copy_from_slice(&sec[boff + 24..boff + 32]);
            d4
        },
    }
}


