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
    DiscoverPayloadStage, ExecuteBootStage, MountFilesystemStage, SelectPayloadStage,
};

#[no_mangle]
extern "efiapi" fn efi_main(
    image_handle: *mut c_void,
    system_table: *mut protocol::SystemTable,
) -> ! {
    // ── Banner ──────────────────────────────────────────────────────
    output::banner(unsafe { &mut *system_table });

    // ── Initialize boot context ─────────────────────────────────────
    let mut ctx = BootContext::new(image_handle, system_table);

    // ── Assemble pipeline ───────────────────────────────────────────
    let mut stage1 = DiscoverDiskStage;
    let mut stage2 = DiscoverPartitionStage;
    let mut stage3 = MountFilesystemStage;
    let mut stage4 = DiscoverPayloadStage;
    let mut stage5 = SelectPayloadStage;

    // Note: SelectPayloadStage calls iso::show_menu which internally
    // handles user interaction and chainloads.  It never returns.
    // ExecuteBootStage is used when auto-booting without a menu.
    let stages: &mut [&mut dyn BootStage] = &mut [
        &mut stage1,
        &mut stage2,
        &mut stage3,
        &mut stage4,
        &mut stage5,
    ];

    BootPipeline::run(&mut ctx, stages);

    // If we ever get here (shouldn't, because SelectPayload diverges):
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