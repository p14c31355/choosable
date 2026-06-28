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

use crate::fs::{FsCtx, FsType, PayloadEntry, PAYLOAD_SLOT_COUNT};
use crate::locator::IsoLocation;
use crate::protocol::{BlockIoProtocol, Guid, SystemTable};

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
    /// GPT partition GUID (unique partition identifier, not the type GUID)
    pub partition_guid: Guid,
    /// 1-based partition number (for fallback if no GPT)
    pub partition_number: u32,
    /// false = GPT, true = MBR (affects PARTUUID format)
    pub is_mbr: bool,
    pub fs_type: Option<FsType>,

    // ── Filesystem ────────────────────────────────────────────────────
    pub fs_ctx: Option<FsCtx>,

    // ── Payload ───────────────────────────────────────────────────────
    pub payloads: [PayloadEntry; PAYLOAD_SLOT_COUNT],
    pub payload_count: usize,
    pub selected_index: Option<usize>,

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
            partition_guid: Guid { d1: 0, d2: 0, d3: 0, d4: [0; 8] },
            partition_number: 0,
            is_mbr: false,
            fs_type: None,
            fs_ctx: None,
            payloads: [PayloadEntry {
                name: [0; 256],
                name_len: 0,
                file_start_lba: 0,
                file_size: 0,
                payload_type: crate::fs::PayloadType::Iso,
            }; PAYLOAD_SLOT_COUNT],
            payload_count: 0,
            selected_index: None,
            iso_location: None,
        }
    }

    /// Returns the currently selected payload entry, or None.
    pub fn selected_payload(&self) -> Option<&PayloadEntry> {
        self.selected_index.and_then(|idx| {
            if idx < self.payload_count { Some(&self.payloads[idx]) } else { None }
        })
    }

    /// Returns a mutable reference to the currently selected payload, or None.
    pub fn selected_payload_mut(&mut self) -> Option<&mut PayloadEntry> {
        self.selected_index.and_then(|idx| {
            if idx < self.payload_count { Some(&mut self.payloads[idx]) } else { None }
        })
    }
}