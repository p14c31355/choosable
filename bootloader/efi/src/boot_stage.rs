// ═══════════════════════════════════════════════════════════════════════════
//  Boot Stage — trait and concrete stage implementations
// ═══════════════════════════════════════════════════════════════════════════
//
//  Each stage is a finite-state-machine transition:
//    DiscoverDisk       → finds the physical disk + Block I/O
//    DiscoverPartition  → reads MBR/GPT, finds partition + filesystem type
//    MountFilesystem    → parses BPB, fills FsCtx
//    DiscoverPayload    → scans the filesystem for ISO/WIM/VHD files
//    SelectPayload      → user interaction (menu / auto-select)
//    PrepareBoot        → applies strategy patches, builds premount cpio
//    ExecuteBoot        → chainloads the EFI bootloader from the payload
//
//  The pipeline runs stages sequentially via `BootPipeline::run()`.

use crate::boot_context::BootContext;
use crate::disk;
use crate::fs;
use crate::iso;
use crate::output::{banner, die, halt_or_reboot, print_hex, print_raw};
use crate::protocol::{BlockIoProtocol, SystemTable, BLOCK_IO_PROTOCOL_GUID, EFI_SUCCESS};

/// Result of executing a stage.
/// - `Continue` means advance to the next stage.
/// - `Halt` means stop (after printing an error or completing chainload).
pub enum StageResult {
    Continue,
    Halt,
}

/// A single step in the boot pipeline.
pub trait BootStage {
    /// Human-readable name for debug/log output.
    fn name(&self) -> &'static str;

    /// Execute this stage.  The stage reads from `ctx` and writes its
    /// discoveries back into `ctx`.
    ///
    /// Returning `StageResult::Halt` stops the pipeline immediately.
    fn execute(&mut self, ctx: &mut BootContext) -> StageResult;
}

// ═══════════════════════════════════════════════════════════════════════════
//  Stage 1: DiscoverDisk
// ═══════════════════════════════════════════════════════════════════════════

pub struct DiscoverDiskStage;

impl BootStage for DiscoverDiskStage {
    fn name(&self) -> &'static str {
        "DiscoverDisk"
    }

    fn execute(&mut self, ctx: &mut BootContext) -> StageResult {
        // Extract raw values before borrowing ctx mutably for st_mut().
        let image_handle = ctx.image_handle;
        let st_ptr: *mut SystemTable = ctx.system_table;

        // Banner uses st_mut, but we can pass st_ptr to it.
        // Actually, banner takes &mut SystemTable, so we derive it:
        let st = unsafe { &mut *st_ptr };
        banner(st);

        // bs() borrows ctx mutably; image_handle is a copy so it's fine.
        let disk_handle = match disk::find_disk_handle(ctx.bs(), image_handle) {
            Some(h) => h,
            None => {
                die(ctx.st_mut(), b"ERROR: No disk device found.\r\n\0");
                // die() diverges, so control never reaches the next line.
                // But the compiler sees it as a normal function, so we need
                // an explicit diverging path.
                loop {
                    unsafe { core::arch::asm!("hlt") }
                }
            }
        };

        let mut bio: *mut BlockIoProtocol = core::ptr::null_mut();
        {
            let bs = ctx.bs();
            if unsafe {
                (bs.handle_protocol)(
                    disk_handle,
                    &BLOCK_IO_PROTOCOL_GUID,
                    &mut bio as *mut _ as _,
                )
            } != EFI_SUCCESS
                || bio.is_null()
            {
                die(ctx.st_mut(), b"ERROR: No Block I/O on disk.\r\n\0");
                loop {
                    unsafe { core::arch::asm!("hlt") }
                }
            }
        }

        let mid = if !unsafe { &*bio }.media.is_null() {
            unsafe { (*(*bio).media).mid }
        } else {
            0
        };

        ctx.disk_handle = Some(disk_handle);
        ctx.block_io = Some(bio);
        ctx.media_id = mid;

        StageResult::Continue
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Stage 2: DiscoverPartition
// ═══════════════════════════════════════════════════════════════════════════

pub struct DiscoverPartitionStage;

impl BootStage for DiscoverPartitionStage {
    fn name(&self) -> &'static str {
        "DiscoverPartition"
    }

    fn execute(&mut self, ctx: &mut BootContext) -> StageResult {
        // Extract raw copies before any borrowing.
        let bio_ptr = ctx.bio_ptr_raw();
        let bio_ref = unsafe { BootContext::bio_ref_from_raw(bio_ptr) };
        let mid = ctx.media_id;

        // Read MBR
        let mut mbr: [u8; 512] = [0; 512];
        if !disk::read_sector(bio_ref, bio_ptr, mid, 0, &mut mbr) {
            die(ctx.st_mut(), b"ERROR: Cannot read MBR.\r\n\0");
            loop {
                unsafe { core::arch::asm!("hlt") }
            }
        }

        let mut part1_lba: u64 = 0;
        let mut is_gpt = false;
        for i in 0..4 {
            let off = 446 + i * 16;
            let fs_type = mbr[off + 4];
            let lba =
                u32::from_le_bytes([mbr[off + 8], mbr[off + 9], mbr[off + 10], mbr[off + 11]]);
            let sec = u32::from_le_bytes([
                mbr[off + 12],
                mbr[off + 13],
                mbr[off + 14],
                mbr[off + 15],
            ]);
            if fs_type == 0xEE && sec > 0 {
                is_gpt = true;
            }
            if sec == 0 || fs_type == 0xEE {
                continue;
            }
            part1_lba = lba as u64;
            break;
        }

        if part1_lba == 0 && is_gpt {
            // Extract st before the call that borrows ctx mutably.
            // find_gpt_data_partition needs &mut SystemTable + raw bio values.
            // We pass only raw copies so no borrow conflict.
            let st = ctx.st_mut();
            print_raw(st, b"GPT detected, searching for data partition...\r\n\0");
            part1_lba = disk::find_gpt_data_partition(st, bio_ref, bio_ptr, mid);
        }
        if part1_lba == 0 {
            die(ctx.st_mut(), b"ERROR: No partition 1 found.\r\n\0");
            loop {
                unsafe { core::arch::asm!("hlt") }
            }
        }

        // Read partition 1 VBR
        let mut vbr: [u8; 512] = [0; 512];
        if !disk::read_sector(bio_ref, bio_ptr, mid, part1_lba, &mut vbr) {
            die(ctx.st_mut(), b"ERROR: Cannot read partition 1.\r\n\0");
            loop {
                unsafe { core::arch::asm!("hlt") }
            }
        }

        // Detect filesystem type
        let fs = if &vbr[3..11] == b"EXFAT   " {
            fs::FsType::Exfat
        } else if &vbr[3..11] == b"NTFS    " {
            fs::FsType::Ntfs
        } else if &vbr[0x52..0x5A] == b"FAT32   " {
            fs::FsType::Fat32
        } else {
            // Fallback: check FAT32 at 0x52
            if &vbr[0x52..0x5A] == b"FAT32   " {
                fs::FsType::Fat32
            } else {
                print_raw(
                    ctx.st_mut(),
                    b"Unknown filesystem on partition 1.\r\n\0",
                );
                print_hex(
                    ctx.st_mut(),
                    b"  First 16 bytes: ",
                    u64::from_le_bytes(vbr[0..8].try_into().unwrap()),
                );
                print_hex(
                    ctx.st_mut(),
                    b"  ",
                    u64::from_le_bytes(vbr[8..16].try_into().unwrap()),
                );
                print_raw(ctx.st_mut(), b"\r\n\0");
                halt_or_reboot(ctx.st_mut());
                loop {
                    unsafe { core::arch::asm!("hlt") }
                }
            }
        };

        ctx.partition_start_lba = part1_lba;
        ctx.fs_type = Some(fs);

        StageResult::Continue
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Stage 3: MountFilesystem
// ═══════════════════════════════════════════════════════════════════════════

pub struct MountFilesystemStage;

impl BootStage for MountFilesystemStage {
    fn name(&self) -> &'static str {
        "MountFilesystem"
    }

    fn execute(&mut self, ctx: &mut BootContext) -> StageResult {
        // Extract raw copies first.
        let bio_ptr = ctx.bio_ptr_raw();
        let bio_ref = unsafe { BootContext::bio_ref_from_raw(bio_ptr) };
        let mid = ctx.media_id;
        let part1_lba = ctx.partition_start_lba;
        let fs = ctx.fs_type.expect("fs_type must be set by DiscoverPartition");

        // Read VBR again to parse BPB
        let mut vbr: [u8; 512] = [0; 512];
        if !disk::read_sector(bio_ref, bio_ptr, mid, part1_lba, &mut vbr) {
            die(ctx.st_mut(), b"ERROR: Cannot re-read partition 1 VBR.\r\n\0");
            loop {
                unsafe { core::arch::asm!("hlt") }
            }
        }

        let mut fs_ctx = fs::FsCtx {
            fs,
            part1_lba,
            spc: 0,
            fat_start: 0,
            fat_len: 0,
            heap_start: 0,
            root_cluster: 0,
            mft_start_lba: 0,
            sectors_per_cluster: 0,
            bytes_per_cluster: 0,
            mft_record_size: 0,
        };

        match fs {
            fs::FsType::Exfat => {
                let spc_shift = vbr[109] as u32;
                if spc_shift > 16 {
                    die(ctx.st_mut(), b"ERROR: Invalid SectorsPerClusterShift.\r\n\0");
                    loop {
                        unsafe { core::arch::asm!("hlt") }
                    }
                }
                let cluster_bytes = (1u32 << spc_shift) * 512;
                let fat_off =
                    u32::from_le_bytes([vbr[80], vbr[81], vbr[82], vbr[83]]) as u64;
                let fat_len =
                    u32::from_le_bytes([vbr[84], vbr[85], vbr[86], vbr[87]]) as u64;
                let heap_off =
                    u32::from_le_bytes([vbr[88], vbr[89], vbr[90], vbr[91]]) as u64;
                let root_cluster =
                    u32::from_le_bytes([vbr[96], vbr[97], vbr[98], vbr[99]]);

                fs_ctx.spc = cluster_bytes / 512;
                fs_ctx.fat_start = part1_lba + fat_off;
                fs_ctx.fat_len = fat_len;
                fs_ctx.heap_start = part1_lba + heap_off;
                fs_ctx.root_cluster = root_cluster;

                print_raw(ctx.st_mut(), b"exFAT detected. Scanning...\r\n\0");
            }
            fs::FsType::Fat32 => {
                let spc = vbr[13] as u32;
                if spc == 0 {
                    die(ctx.st_mut(), b"ERROR: Invalid sectors per cluster.\r\n\0");
                    loop {
                        unsafe { core::arch::asm!("hlt") }
                    }
                }
                let reserved = u16::from_le_bytes([vbr[14], vbr[15]]) as u64;
                let num_fats = vbr[16] as u64;
                let fat_sectors =
                    u32::from_le_bytes([vbr[36], vbr[37], vbr[38], vbr[39]]) as u64;
                let root_cluster =
                    u32::from_le_bytes([vbr[44], vbr[45], vbr[46], vbr[47]]);

                let fat_start = part1_lba + reserved;
                let data_start = fat_start + num_fats * fat_sectors;

                fs_ctx.spc = spc;
                fs_ctx.fat_start = fat_start;
                fs_ctx.fat_len = fat_sectors;
                fs_ctx.heap_start = data_start;
                fs_ctx.root_cluster = root_cluster;

                print_raw(ctx.st_mut(), b"FAT32 detected. Scanning...\r\n\0");
            }
            fs::FsType::Ntfs => {
                let spc = vbr[13] as u32;
                if spc == 0 {
                    die(ctx.st_mut(), b"ERROR: Invalid sectors per cluster.\r\n\0");
                    loop {
                        unsafe { core::arch::asm!("hlt") }
                    }
                }
                let cluster_bytes = spc as u64 * 512;
                let mft_lcn =
                    i64::from_le_bytes(vbr[0x30..0x38].try_into().unwrap());
                let mft_start_lba =
                    part1_lba + (mft_lcn as u64) * spc as u64;
                let cpmr_raw = vbr[0x40] as i8;
                let mft_record_size: u64 = if cpmr_raw > 0 {
                    cpmr_raw as u64 * cluster_bytes
                } else if cpmr_raw >= -12 {
                    1u64 << (-cpmr_raw)
                } else {
                    0
                };
                if mft_record_size == 0 || mft_record_size > 4096 {
                    die(ctx.st_mut(), b"ERROR: Invalid MFT record size.\r\n\0");
                    loop {
                        unsafe { core::arch::asm!("hlt") }
                    }
                }

                fs_ctx.spc = spc;
                fs_ctx.sectors_per_cluster = spc;
                fs_ctx.bytes_per_cluster = cluster_bytes;
                fs_ctx.mft_start_lba = mft_start_lba;
                fs_ctx.mft_record_size = mft_record_size;
                fs_ctx.heap_start = part1_lba;

                print_raw(ctx.st_mut(), b"NTFS detected. Scanning...\r\n\0");
            }
        }

        ctx.fs_ctx = Some(fs_ctx);

        StageResult::Continue
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Stage 4: DiscoverPayload
// ═══════════════════════════════════════════════════════════════════════════

pub struct DiscoverPayloadStage;

impl BootStage for DiscoverPayloadStage {
    fn name(&self) -> &'static str {
        "DiscoverPayload"
    }

    fn execute(&mut self, ctx: &mut BootContext) -> StageResult {
        // Extract raw copies first so st_mut() / bio_ref don't conflict.
        let bio_ptr = ctx.bio_ptr_raw();
        let bio_ref = unsafe { BootContext::bio_ref_from_raw(bio_ptr) };
        let mid = ctx.media_id;

        // Take fs_ctx out of the Option temporarily to avoid conflicting borrows.
        // scan_directory takes &FsCtx, so we can create a reference after
        // extracting the raw pointer.
        let fs_ctx_ptr: *const fs::FsCtx = match &ctx.fs_ctx {
            Some(fc) => fc as *const fs::FsCtx,
            None => {
                // shouldn't happen — MountFilesystem must have run
                panic!("fs_ctx not set before DiscoverPayload");
            }
        };
        let fs_ctx_ref = unsafe { &*fs_ctx_ptr };

        let mut iso_count: usize = 0;
        let mut iso_files: [fs::IsoEntry; 64] = unsafe { core::mem::zeroed() };
        fs::scan_directory(bio_ref, bio_ptr, mid, fs_ctx_ref, &mut iso_files, &mut iso_count);

        ctx.iso_files = iso_files;
        ctx.iso_count = iso_count;

        if iso_count == 0 {
            print_raw(ctx.st_mut(), b"\r\nNo ISO files found on partition 1.\r\n\0");
            halt_or_reboot(ctx.st_mut());
            loop {
                unsafe { core::arch::asm!("hlt") }
            }
        }

        StageResult::Continue
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Stage 5: SelectPayload
// ═══════════════════════════════════════════════════════════════════════════

pub struct SelectPayloadStage;

impl BootStage for SelectPayloadStage {
    fn name(&self) -> &'static str {
        "SelectPayload"
    }

    fn execute(&mut self, ctx: &mut BootContext) -> StageResult {
        // Extract raw copies / pointers before calling st_mut().
        let image_handle = ctx.image_handle;
        let disk_handle = ctx.disk_handle.expect("disk_handle must be set");
        let bio_ptr = ctx.bio_ptr_raw();
        let bio_ref = unsafe { BootContext::bio_ref_from_raw(bio_ptr) };
        let mid = ctx.media_id;

        // Get a raw pointer to fs_ctx.
        let fs_ctx_ptr: *const fs::FsCtx = match &ctx.fs_ctx {
            Some(fc) => fc as *const fs::FsCtx,
            None => panic!("fs_ctx not set before SelectPayload"),
        };
        let fs_ctx_ref = unsafe { &*fs_ctx_ptr };

        // Use raw pointer to iso_files to avoid borrowing ctx immutably
        // while also calling ctx.st_mut().
        let iso_files_ptr: *const fs::IsoEntry = ctx.iso_files.as_ptr();
        let iso_count = ctx.iso_count;
        let st = ctx.st_mut(); // mutable borrow of ctx

        iso::show_menu(
            st,
            image_handle,
            disk_handle,
            unsafe { &*(iso_files_ptr as *const [fs::IsoEntry; 64]) },
            iso_count,
            fs_ctx_ref,
            bio_ref,
            bio_ptr,
            mid,
        );
        // show_menu never returns, but the compiler doesn't know that.
        // Since it diverges, we just tell the pipeline to halt.
        // This line is unreachable but satisfies the type system.
        loop {
            unsafe { core::arch::asm!("hlt") }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Stage 7: ExecuteBoot
// ═══════════════════════════════════════════════════════════════════════════
//
//  (Stage 6 is "PrepareBoot", applied internally by `iso::boot_iso`)

pub struct ExecuteBootStage {
    pub selected_index: usize,
}

impl BootStage for ExecuteBootStage {
    fn name(&self) -> &'static str {
        "ExecuteBoot"
    }

    fn execute(&mut self, ctx: &mut BootContext) -> StageResult {
        // Set selected_index before any mutable borrow of ctx.
        ctx.selected_index = self.selected_index;

        // Extract raw copies / pointers before calling st_mut().
        let image_handle = ctx.image_handle;
        let disk_handle = ctx.disk_handle.expect("disk_handle must be set");
        let partition_start_lba = ctx.partition_start_lba;
        let bio_ptr = ctx.bio_ptr_raw();
        let bio_ref = unsafe { BootContext::bio_ref_from_raw(bio_ptr) };
        let mid = ctx.media_id;

        // Use raw pointer to iso_files to avoid borrowing ctx immutably
        // while also calling ctx.st_mut().
        let iso_files_ptr: *const fs::IsoEntry = ctx.iso_files.as_ptr();
        let selected_index = self.selected_index;
        let st = ctx.st_mut();

        iso::boot_iso(
            st,
            image_handle,
            disk_handle,
            partition_start_lba,
            unsafe { &*(iso_files_ptr as *const [fs::IsoEntry; 64]) },
            selected_index,
            bio_ref,
            bio_ptr,
            mid,
        );
        // boot_iso chainloads and never returns.
        loop {
            unsafe { core::arch::asm!("hlt") }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Boot Pipeline — runs stages in sequence
// ═══════════════════════════════════════════════════════════════════════════

pub struct BootPipeline;

impl BootPipeline {
    /// Run all boot stages sequentially over `ctx`.
    ///
    /// Each stage may return `StageResult::Halt` to stop the pipeline.
    /// Stages that never return (chainload) implicitly halt.
    pub fn run(ctx: &mut BootContext, stages: &mut [&mut dyn BootStage]) {
        for stage in stages.iter_mut() {
            match stage.execute(ctx) {
                StageResult::Continue => {}
                StageResult::Halt => return,
            }
        }
    }
}