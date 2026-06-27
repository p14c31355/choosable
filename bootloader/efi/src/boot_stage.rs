// ═══════════════════════════════════════════════════════════════════════════
//  Boot Stage — trait and concrete stage implementations
// ═══════════════════════════════════════════════════════════════════════════

use core::ffi::c_void;

use crate::boot_context::BootContext;
use crate::disk;
use crate::fs;
use crate::fs::{PayloadType, PAYLOAD_SLOT_COUNT};
use crate::iso;
use crate::output::{banner, die, halt_or_reboot, print_hex, print_raw};
use crate::protocol::{BlockIoProtocol, BootServices, SystemTable, BLOCK_IO_PROTOCOL_GUID, EFI_SUCCESS};

pub enum StageResult {
    Continue,
    Halt,
}

pub trait BootStage {
    fn name(&self) -> &'static str;
    fn execute(&mut self, ctx: &mut BootContext) -> StageResult;
}

macro_rules! die_then_halt {
    ($ctx:expr, $msg:expr) => {
        die(unsafe { &mut *$ctx.system_table }, $msg)
    };
}

// ── Helper: extract *mut SystemTable from raw pointer ───────────────
fn st_from_ctx(ctx: &BootContext) -> *mut SystemTable {
    ctx.system_table
}

// ── Helper: extract *mut BootServices from raw pointer ──────────────
fn bs_from_ctx(ctx: &BootContext) -> *mut BootServices {
    unsafe { (*ctx.system_table).boot_services }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Stage 1: DiscoverDisk
// ═══════════════════════════════════════════════════════════════════════════

pub struct DiscoverDiskStage;

impl BootStage for DiscoverDiskStage {
    fn name(&self) -> &'static str { "DiscoverDisk" }

    fn execute(&mut self, ctx: &mut BootContext) -> StageResult {
        let image_handle = ctx.image_handle;

        // Banner — use raw pointer to avoid borrow issues
        banner(unsafe { &mut *st_from_ctx(ctx) });

        let disk_handle = match disk::find_disk_handle(unsafe { &mut *bs_from_ctx(ctx) }, image_handle) {
            Some(h) => h,
            None => die_then_halt!(ctx, b"ERROR: No disk device found.\r\n\0"),
        };

        let mut bio: *mut BlockIoProtocol = core::ptr::null_mut();
        {
            let bs = unsafe { &mut *bs_from_ctx(ctx) };
            if unsafe {
                (bs.handle_protocol)(
                    disk_handle,
                    &BLOCK_IO_PROTOCOL_GUID,
                    &mut bio as *mut _ as _,
                )
            } != EFI_SUCCESS
                || bio.is_null()
            {
                die_then_halt!(ctx, b"ERROR: No Block I/O on disk.\r\n\0");
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
    fn name(&self) -> &'static str { "DiscoverPartition" }

    fn execute(&mut self, ctx: &mut BootContext) -> StageResult {
        let bio_ptr = ctx.block_io.expect("block_io must be set");
        let bio_ref = unsafe { &*bio_ptr };
        let mid = ctx.media_id;

        // Read MBR
        let mut mbr: [u8; 512] = [0; 512];
        if !disk::read_sector(bio_ref, bio_ptr, mid, 0, &mut mbr) {
            die_then_halt!(ctx, b"ERROR: Cannot read MBR.\r\n\0");
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
            let st = unsafe { &mut *st_from_ctx(ctx) };
            print_raw(st, b"GPT detected, searching for data partition...\r\n\0");
            if let Some((lba, guid)) = disk::find_gpt_data_partition(st, bio_ref, bio_ptr, mid) {
                part1_lba = lba;
                ctx.partition_guid = guid;
                // Count preceding partitions for partition number
                ctx.partition_number = disk::count_gpt_partition_number(bio_ref, bio_ptr, mid, &guid) as u32;
            }
        }
        if part1_lba == 0 {
            die_then_halt!(ctx, b"ERROR: No partition 1 found.\r\n\0");
        }

        // Read partition 1 VBR
        let mut vbr: [u8; 512] = [0; 512];
        if !disk::read_sector(bio_ref, bio_ptr, mid, part1_lba, &mut vbr) {
            die_then_halt!(ctx, b"ERROR: Cannot read partition 1.\r\n\0");
        }

        let fs = if &vbr[3..11] == b"EXFAT   " {
            fs::FsType::Exfat
        } else if &vbr[3..11] == b"NTFS    " {
            fs::FsType::Ntfs
        } else if &vbr[0x52..0x5A] == b"FAT32   " {
            fs::FsType::Fat32
        } else {
            let st = unsafe { &mut *st_from_ctx(ctx) };
            print_raw(st, b"Unknown filesystem on partition 1.\r\n\0");
            print_hex(st, b"  First 16 bytes: ", u64::from_le_bytes(vbr[0..8].try_into().unwrap()));
            print_hex(st, b"  ", u64::from_le_bytes(vbr[8..16].try_into().unwrap()));
            print_raw(st, b"\r\n\0");
            halt_or_reboot(st);
            loop { unsafe { core::arch::asm!("hlt") } }
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
    fn name(&self) -> &'static str { "MountFilesystem" }

    fn execute(&mut self, ctx: &mut BootContext) -> StageResult {
        let bio_ptr = ctx.block_io.expect("block_io must be set");
        let bio_ref = unsafe { &*bio_ptr };
        let mid = ctx.media_id;
        let part1_lba = ctx.partition_start_lba;
        let fs = ctx.fs_type.expect("fs_type must be set by DiscoverPartition");

        let mut vbr: [u8; 512] = [0; 512];
        if !disk::read_sector(bio_ref, bio_ptr, mid, part1_lba, &mut vbr) {
            die_then_halt!(ctx, b"ERROR: Cannot re-read partition 1 VBR.\r\n\0");
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
                    die_then_halt!(ctx, b"ERROR: Invalid SectorsPerClusterShift.\r\n\0");
                }
                let cluster_bytes = (1u32 << spc_shift) * 512;
                let fat_off = u32::from_le_bytes([vbr[80], vbr[81], vbr[82], vbr[83]]) as u64;
                let fat_len = u32::from_le_bytes([vbr[84], vbr[85], vbr[86], vbr[87]]) as u64;
                let heap_off = u32::from_le_bytes([vbr[88], vbr[89], vbr[90], vbr[91]]) as u64;
                let root_cluster = u32::from_le_bytes([vbr[96], vbr[97], vbr[98], vbr[99]]);

                fs_ctx.spc = cluster_bytes / 512;
                fs_ctx.fat_start = part1_lba + fat_off;
                fs_ctx.fat_len = fat_len;
                fs_ctx.heap_start = part1_lba + heap_off;
                fs_ctx.root_cluster = root_cluster;

                print_raw(unsafe { &mut *st_from_ctx(ctx) }, b"exFAT detected. Scanning...\r\n\0");
            }
            fs::FsType::Fat32 => {
                let spc = vbr[13] as u32;
                if spc == 0 {
                    die_then_halt!(ctx, b"ERROR: Invalid sectors per cluster.\r\n\0");
                }
                let reserved = u16::from_le_bytes([vbr[14], vbr[15]]) as u64;
                let num_fats = vbr[16] as u64;
                let fat_sectors = u32::from_le_bytes([vbr[36], vbr[37], vbr[38], vbr[39]]) as u64;
                let root_cluster = u32::from_le_bytes([vbr[44], vbr[45], vbr[46], vbr[47]]);

                let fat_start = part1_lba + reserved;
                let data_start = fat_start + num_fats * fat_sectors;

                fs_ctx.spc = spc;
                fs_ctx.fat_start = fat_start;
                fs_ctx.fat_len = fat_sectors;
                fs_ctx.heap_start = data_start;
                fs_ctx.root_cluster = root_cluster;

                print_raw(unsafe { &mut *st_from_ctx(ctx) }, b"FAT32 detected. Scanning...\r\n\0");
            }
            fs::FsType::Ntfs => {
                let spc = vbr[13] as u32;
                if spc == 0 {
                    die_then_halt!(ctx, b"ERROR: Invalid sectors per cluster.\r\n\0");
                }
                let cluster_bytes = spc as u64 * 512;
                let mft_lcn = i64::from_le_bytes(vbr[0x30..0x38].try_into().unwrap());
                let mft_start_lba = part1_lba + (mft_lcn as u64) * spc as u64;
                let cpmr_raw = vbr[0x40] as i8;
                let mft_record_size: u64 = if cpmr_raw > 0 {
                    cpmr_raw as u64 * cluster_bytes
                } else if cpmr_raw >= -12 {
                    1u64 << (-cpmr_raw)
                } else {
                    0
                };
                if mft_record_size == 0 || mft_record_size > 4096 {
                    die_then_halt!(ctx, b"ERROR: Invalid MFT record size.\r\n\0");
                }

                fs_ctx.spc = spc;
                fs_ctx.sectors_per_cluster = spc;
                fs_ctx.bytes_per_cluster = cluster_bytes;
                fs_ctx.mft_start_lba = mft_start_lba;
                fs_ctx.mft_record_size = mft_record_size;
                fs_ctx.heap_start = part1_lba;

                print_raw(unsafe { &mut *st_from_ctx(ctx) }, b"NTFS detected. Scanning...\r\n\0");
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
    fn name(&self) -> &'static str { "DiscoverPayload" }

    fn execute(&mut self, ctx: &mut BootContext) -> StageResult {
        let bio_ptr = ctx.block_io.expect("block_io must be set");
        let bio_ref = unsafe { &*bio_ptr };
        let mid = ctx.media_id;

        let fs_ctx_ref = ctx.fs_ctx.as_ref().expect("fs_ctx must be set");

        fs::scan_payloads(bio_ref, bio_ptr, mid, fs_ctx_ref, &mut ctx.payloads, &mut ctx.payload_count);

        if ctx.payload_count == 0 {
            let st = unsafe { &mut *st_from_ctx(ctx) };
            print_raw(st, b"\r\nNo bootable payloads found on partition 1.\r\n\0");
            halt_or_reboot(st);
            loop { unsafe { core::arch::asm!("hlt") } }
        }

        StageResult::Continue
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Stage 5: SelectPayload
// ═══════════════════════════════════════════════════════════════════════════

pub struct SelectPayloadStage;

impl BootStage for SelectPayloadStage {
    fn name(&self) -> &'static str { "SelectPayload" }

    fn execute(&mut self, ctx: &mut BootContext) -> StageResult {
        let image_handle = ctx.image_handle;
        let disk_handle = ctx.disk_handle.expect("disk_handle must be set");
        let bio_ptr = ctx.block_io.expect("block_io must be set");
        let bio_ref = unsafe { &*bio_ptr };
        let mid = ctx.media_id;
        let fs_ctx_ref = ctx.fs_ctx.as_ref().expect("fs_ctx must be set");
        let payload_count = ctx.payload_count;
        let st = unsafe { &mut *ctx.system_table };

        iso::show_payload_menu(
            st,
            image_handle,
            disk_handle,
            &ctx.payloads,
            payload_count,
            fs_ctx_ref,
            bio_ref,
            bio_ptr,
            mid,
            &ctx.partition_guid,
            ctx.partition_number,
        );
        loop { unsafe { core::arch::asm!("hlt") } }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Stage 6: ExecuteBoot
// ═══════════════════════════════════════════════════════════════════════════

pub struct ExecuteBootStage {
    pub selected_index: usize,
}

impl BootStage for ExecuteBootStage {
    fn name(&self) -> &'static str { "ExecuteBoot" }

    fn execute(&mut self, ctx: &mut BootContext) -> StageResult {
        if ctx.payload_count == 0 || self.selected_index >= ctx.payload_count || self.selected_index >= PAYLOAD_SLOT_COUNT {
            let st = unsafe { &mut *st_from_ctx(ctx) };
            print_raw(st, b"ERROR: Invalid payload selection index.\r\n\0");
            halt_or_reboot(st);
            loop { unsafe { core::arch::asm!("hlt") } }
        }

        ctx.selected_index = Some(self.selected_index);

        let image_handle = ctx.image_handle;
        let disk_handle = ctx.disk_handle.expect("disk_handle must be set");
        let partition_start_lba = ctx.partition_start_lba;
        let bio_ptr = ctx.block_io.expect("block_io must be set");
        let bio_ref = unsafe { &*bio_ptr };
        let mid = ctx.media_id;
        let selected_index = self.selected_index;
        let st = unsafe { &mut *ctx.system_table };

        iso::boot_payload_by_type(
            st,
            image_handle,
            disk_handle,
            partition_start_lba,
            &ctx.payloads,
            selected_index,
            bio_ref,
            bio_ptr,
            mid,
            &ctx.partition_guid,
            ctx.partition_number,
        );
        loop { unsafe { core::arch::asm!("hlt") } }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Boot Pipeline
// ═══════════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════════
//  NetworkPayloadLocatorStage — downloads payload via HTTP/TFTP/PXE
// ═══════════════════════════════════════════════════════════════════════════

/// Stage that retrieves a boot payload from a network source.
///
/// This stage replaces `DiscoverPayloadStage` when network booting.
/// It downloads an ISO/IMG/EFI payload into memory and populates the
/// `ctx.payloads` array so the pipeline can continue to `SelectPayloadStage`.
///
/// For PXE: the DHCP-offered filename is used.
/// For HTTP(S): the URL is provided at stage construction.
pub struct NetworkPayloadLocatorStage {
    pub url: Option<&'static [u8]>,
}

impl BootStage for NetworkPayloadLocatorStage {
    fn name(&self) -> &'static str { "NetworkPayloadLocator" }

    fn execute(&mut self, ctx: &mut BootContext) -> StageResult {
        let st = unsafe { &mut *ctx.system_table };
        let bs = unsafe { &mut *st.boot_services };

        print_raw(st, b"[NetworkPayloadLocator] Network boot not yet implemented.\r\n\0");
        print_raw(st, b"  URL config: \0");
        if let Some(url) = self.url {
            print_raw(st, url);
        } else {
            print_raw(st, b"(DHCP / PXE)\0");
        }
        print_raw(st, b"\r\n\0");

        // TODO: Implement network download via UEFI HTTP/SIMPLE_NETWORK protocols.
        // For now, fall through with empty payloads so the pipeline continues
        // to the next stage or halts gracefully.
        if ctx.payload_count == 0 {
            print_raw(st, b"[NetworkPayloadLocator] No network payload loaded.\r\n\0");
            halt_or_reboot(st);
        }

        StageResult::Continue
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Boot Pipeline and Builder
// ═══════════════════════════════════════════════════════════════════════════

pub struct BootPipeline;

impl BootPipeline {
    pub fn run(ctx: &mut BootContext, stages: &mut [&mut dyn BootStage]) {
        for stage in stages.iter_mut() {
            match stage.execute(ctx) {
                StageResult::Continue => {}
                StageResult::Halt => return,
            }
        }
    }
}

// Pipeline assembly is done inline in `main.rs` to avoid lifetime issues
// with returning references to local stage objects.
//
// Example custom pipeline (in main.rs):
//   let mut stage4 = NetworkPayloadLocatorStage { url: None };
//   let stages = &mut [&mut stage1, &mut stage2, &mut stage3,
//                      &mut stage4, &mut stage5] as &mut [&mut dyn BootStage];