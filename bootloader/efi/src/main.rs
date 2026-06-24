#![no_std]
#![no_main]

mod boot_context;
mod boot_stage;
mod disk;
mod fs;
mod iso;
mod iso_fs;
mod locator;
mod output;
mod premount;
mod protocol;
mod strategy;
mod virtual_blockio;

use core::ffi::c_void;
use core::panic::PanicInfo;

use boot_context::BootContext;
use boot_stage::{
    BootPipeline, BootStage, DiscoverDiskStage, DiscoverPartitionStage,
    DiscoverPayloadStage, ExecuteBootStage, MountFilesystemStage,
    NetworkPayloadLocatorStage, SelectPayloadStage,
};

#[no_mangle]
extern "efiapi" fn efi_main(
    image_handle: *mut c_void,
    system_table: *mut protocol::SystemTable,
) -> ! {
    let mut ctx = BootContext::new(image_handle, system_table);

    // ── Customize pipeline here ─────────────────────────────────────
    //
    // Default local-boot pipeline:
    //   DiscoverDisk → DiscoverPartition → MountFilesystem
    //   → DiscoverPayload → SelectPayload
    //
    // Replace DiscoverPayloadStage with NetworkPayloadLocatorStage
    // for PXE/HTTP boot:
    //   let mut stage4 = NetworkPayloadLocatorStage { url: None };
    //
    // Replace SelectPayloadStage with ExecuteBootStage { index: N }
    // for unattended boot.
    //
    // Additional stages (e.g. SecureBootEnforceStage) can be inserted
    // anywhere in the pipeline.

    let mut stage1 = DiscoverDiskStage;
    let mut stage2 = DiscoverPartitionStage;
    let mut stage3 = MountFilesystemStage;
    let mut stage4 = DiscoverPayloadStage;
    let mut stage5 = SelectPayloadStage;

    let stages: &mut [&mut dyn BootStage] = &mut [
        &mut stage1,
        &mut stage2,
        &mut stage3,
        &mut stage4,
        &mut stage5,
    ];

    BootPipeline::run(&mut ctx, stages);

    loop {
        unsafe { core::arch::asm!("hlt") }
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("hlt") }
    }
}