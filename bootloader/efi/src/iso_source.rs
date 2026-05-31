// ═══════════════════════════════════════════════════════════════════════════
//  IsoSource — data supply abstraction
// ═══════════════════════════════════════════════════════════════════════════
//
//  Abstracts "where to read ISO data from" behind a uniform interface.
//
//  Long-term goal:
//    Stop asking Linux to find the original ISO (iso-scan/filename=).
//    Instead, Choosable understands ISO → squashfs → casper and serves
//    the necessary files directly through the virtual CD-ROM.
//
//  IsoSource is the first step:
//    - BlockIoIsoSource: reads from an ISO file on a real disk.
//    - Future: HttpIsoSource, RamIsoSource — same trait, different backends.
//
//  IsoSource vs IsoLocator distinction:
//    - IsoLocator: answers "WHERE is the ISO?" (location metadata).
//    - IsoSource:   answers "HOW to read the ISO?" (I/O abstraction).

use core::ffi::c_void;
use crate::protocol::{BlockIoProtocol, EFI_SUCCESS};

/// Data source for ISO content.
///
/// Provides random-access reads at arbitrary offsets.  The virtual CD-ROM
/// and ISO9660 parser operate against this trait, so they never need to
/// know whether the backing store is a local disk, HTTP server, or RAM.
pub trait IsoSource: Sync {
    /// Read from the given byte offset into `buf`.
    /// Returns the number of bytes actually read (may be less than `buf.len()`).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> usize;

    /// Total size of the ISO in bytes.
    fn size(&self) -> u64;

    /// Read one 2048-byte ISO logical sector.
    /// `sector` is 0-based in ISO 2048-byte units.
    fn read_iso_sector(&self, sector: u32, buf: &mut [u8; 2048]) -> bool {
        let offset = sector as u64 * 2048;
        if offset + 2048 > self.size() {
            return false;
        }
        self.read_at(offset, buf) >= 2048
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  BlockIoIsoSource — read an ISO file via UEFI Block I/O Protocol
// ═══════════════════════════════════════════════════════════════════════════

/// Reads ISO content directly from a real disk using UEFI Block I/O.
///
/// This is Choosable's primary use case: a `.iso` file on a USB partition
/// is presented as a virtual CD-ROM.
pub struct BlockIoIsoSource {
    /// Real disk BlockIoProtocol pointer
    bio_ptr: *mut BlockIoProtocol,
    /// Real media ID
    media_id: u32,
    /// ISO file start LBA (512-byte sectors)
    iso_lba: u64,
    /// ISO file total size in bytes
    iso_size: u64,
}

impl BlockIoIsoSource {
    /// Create a new BlockIoIsoSource.
    pub fn new(
        bio_ptr: *mut BlockIoProtocol,
        media_id: u32,
        iso_lba: u64,
        iso_size: u64,
    ) -> Self {
        BlockIoIsoSource {
            bio_ptr,
            media_id,
            iso_lba,
            iso_size,
        }
    }

    /// Returns the real BlockIoProtocol pointer (compatibility).
    pub fn bio_ptr(&self) -> *mut BlockIoProtocol {
        self.bio_ptr
    }

    /// Returns the real media ID (compatibility).
    pub fn media_id(&self) -> u32 {
        self.media_id
    }

    /// Returns the ISO start LBA (compatibility).
    pub fn iso_lba(&self) -> u64 {
        self.iso_lba
    }
}

impl IsoSource for BlockIoIsoSource {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> usize {
        if offset >= self.iso_size || buf.is_empty() {
            return 0;
        }

        let bio_ref = unsafe { &*self.bio_ptr };

        // Convert ISO-internal byte offset to disk LBA (512-byte sectors).
        let disk_lba = self.iso_lba + (offset / 512);
        let byte_offset_in_sector = (offset % 512) as usize;

        // Handle unaligned reads by first reading the partial sector.
        if byte_offset_in_sector != 0 {
            let mut sector = [0u8; 512];
            let status = unsafe {
                (bio_ref.read_blocks)(
                    self.bio_ptr,
                    self.media_id,
                    disk_lba,
                    512,
                    sector.as_mut_ptr() as *mut c_void,
                )
            };
            if status != EFI_SUCCESS {
                return 0;
            }
            let from_sector = byte_offset_in_sector.min(512);
            let to_copy = buf.len().min(512 - from_sector);
            buf[..to_copy]
                .copy_from_slice(&sector[from_sector..from_sector + to_copy]);

            // If more data is needed, read the remainder aligned.
            if to_copy < buf.len() {
                let remaining = &mut buf[to_copy..];
                let next_offset = offset + to_copy as u64;
                let next_lba = self.iso_lba + (next_offset / 512);
                let status2 = unsafe {
                    (bio_ref.read_blocks)(
                        self.bio_ptr,
                        self.media_id,
                        next_lba,
                        remaining.len(),
                        remaining.as_mut_ptr() as *mut c_void,
                    )
                };
                if status2 != EFI_SUCCESS {
                    return to_copy;
                }
                return buf.len();
            }
            return to_copy;
        }

        // Aligned read — straight pass-through to Block I/O.
        let read_len = buf.len().min((self.iso_size - offset) as usize);
        let status = unsafe {
            (bio_ref.read_blocks)(
                self.bio_ptr,
                self.media_id,
                disk_lba,
                read_len,
                buf.as_mut_ptr() as *mut c_void,
            )
        };
        if status != EFI_SUCCESS {
            return 0;
        }
        read_len
    }

    fn size(&self) -> u64 {
        self.iso_size
    }
}

unsafe impl Sync for BlockIoIsoSource {}