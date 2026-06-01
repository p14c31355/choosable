// ═══════════════════════════════════════════════════════════════════════════
//  BootContext — centralized, immutable boot state
// ═══════════════════════════════════════════════════════════════════════════
//
//  Replaces the scattered `st`, `bs`, `bio`, `mid`, `part1_lba`, `iso_lba`
//  variables with a single struct.  Every stage receives `&mut BootContext`
//  and fills in the fields it discovers.
//
//  Fields are Option-wrapped so the pipeline can validate that each stage
//  has completed its work before the next stage runs.

use core::ffi::c_void;

use crate::fs::{FsCtx, FsType, IsoEntry};
use crate::locator::IsoLocation;
use crate::protocol::{BlockIoProtocol, SystemTable};

/// Complete state of the boot process.
///
/// Stages populate fields incrementally:
///   DiskDiscovery     → disk_handle, block_io
///   PartitionDiscovery → partition_start_lba, fs_type
///   FilesystemMount   → fs_ctx
///   PayloadDiscovery  → iso_files, iso_count
///   BootPreparation   → (strategy applies patches)
///   BootExecution     → chainload / transfer control
pub struct BootContext {
    /// UEFI image handle (passed from efi_main)
    pub image_handle: *mut c_void,

    /// UEFI System Table
    pub system_table: *mut SystemTable,

    // ── Disk ──────────────────────────────────────────────────────────
    /// Handle of the physical disk device
    pub disk_handle: Option<*mut c_void>,

    /// Block I/O protocol on the disk
    pub block_io: Option<*mut BlockIoProtocol>,

    /// Block I/O media ID
    pub media_id: u32,

    // ── Partition ─────────────────────────────────────────────────────
    /// Start LBA of the data partition (0 = not found)
    pub partition_start_lba: u64,

    /// Detected filesystem type on the partition
    pub fs_type: Option<FsType>,

    // ── Filesystem ────────────────────────────────────────────────────
    /// Parsed filesystem context (BPB, FAT, MFT info, etc.)
    pub fs_ctx: Option<FsCtx>,

    // ── Payload ───────────────────────────────────────────────────────
    /// Discovered ISO files (or other boot payloads)
    pub iso_files: [IsoEntry; 64],

    /// Number of valid entries in `iso_files`
    pub iso_count: usize,

    /// Currently selected payload index
    pub selected_index: usize,

    // ── Locator ───────────────────────────────────────────────────────
    /// Physical location of the selected payload
    pub iso_location: Option<IsoLocation>,
}

impl BootContext {
    /// Create a new context with only the mandatory fields set.
    pub fn new(image_handle: *mut c_void, system_table: *mut SystemTable) -> Self {
        BootContext {
            image_handle,
            system_table,
            disk_handle: None,
            block_io: None,
            media_id: 0,
            partition_start_lba: 0,
            fs_type: None,
            fs_ctx: None,
            iso_files: unsafe { core::mem::zeroed() },
            iso_count: 0,
            selected_index: 0,
            iso_location: None,
        }
    }

    /// Returns a mutable reference to the system table.
    ///
    /// CAUTION: Do NOT hold other references into `self` while this borrow is
    /// active.  Extract raw values first, then call `st_mut()`.
    pub fn st_mut(&mut self) -> &mut SystemTable {
        unsafe { &mut *self.system_table }
    }

    /// Returns a mutable reference to boot services.
    ///
    /// CAUTION: same borrowing constraints as `st_mut()`.
    pub fn bs(&mut self) -> &mut crate::protocol::BootServices {
        unsafe { &mut *self.st_mut().boot_services }
    }

    /// Returns the raw Block I/O protocol pointer.
    /// Does NOT borrow `self`, so it can coexist with `st_mut()`.
    pub fn bio_ptr_raw(&self) -> *mut BlockIoProtocol {
        self.block_io.expect("Block I/O not discovered")
    }

    /// Returns a reference to the Block I/O protocol from the raw pointer.
    /// SAFETY: caller must ensure the pointer is valid.
    pub unsafe fn bio_ref_from_raw(ptr: *mut BlockIoProtocol) -> &'static BlockIoProtocol {
        &*ptr
    }

    /// Returns the selected ISO entry (panics if none selected).
    pub fn selected_iso(&self) -> &IsoEntry {
        &self.iso_files[self.selected_index]
    }
}