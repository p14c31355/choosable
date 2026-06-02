// ═══════════════════════════════════════════════════════════════════════════
//  BootContext — centralized, immutable boot state
// ═══════════════════════════════════════════════════════════════════════════
//
//  Replaces the scattered `st`, `bs`, `bio`, `mid`, `part1_lba`, `iso_lba`
//  variables with a single struct.  Every stage receives `&mut BootContext`
//  and fills in the fields it discovers.
//
//  IMPORTANT: This struct intentionally does NOT provide `st_mut()` or `bs()`
//  methods that borrow `&mut self`.  Instead, stages extract the raw
//  `system_table` pointer (`*mut SystemTable`) and derive `&mut SystemTable`
//  / `&mut BootServices` locally via `unsafe`.  This avoids Stacked Borrows
//  violations when a stage needs to read `iso_files` (or other fields) while
//  also holding a mutable reference to the system table.

use core::ffi::c_void;

use crate::fs::{FsCtx, FsType, IsoEntry};
use crate::locator::IsoLocation;
use crate::protocol::{BlockIoProtocol, SystemTable};

/// Complete state of the boot process.
pub struct BootContext {
    /// UEFI image handle (passed from efi_main)
    pub image_handle: *mut c_void,

    /// UEFI System Table (raw pointer – stages derive &mut SystemTable locally)
    pub system_table: *mut SystemTable,

    // ── Disk ──────────────────────────────────────────────────────────
    pub disk_handle: Option<*mut c_void>,
    pub block_io: Option<*mut BlockIoProtocol>,
    pub media_id: u32,

    // ── Partition ─────────────────────────────────────────────────────
    pub partition_start_lba: u64,
    pub fs_type: Option<FsType>,

    // ── Filesystem ────────────────────────────────────────────────────
    pub fs_ctx: Option<FsCtx>,

    // ── Payload ───────────────────────────────────────────────────────
    pub iso_files: [IsoEntry; 64],
    pub iso_count: usize,
    pub selected_index: usize,

    // ── Locator ───────────────────────────────────────────────────────
    pub iso_location: Option<IsoLocation>,
}

impl BootContext {
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

    /// Returns the selected ISO entry (panics if none selected).
    pub fn selected_iso(&self) -> &IsoEntry {
        &self.iso_files[self.selected_index]
    }
}