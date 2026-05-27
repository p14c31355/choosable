#![no_std]
#![no_main]

use core::panic::PanicInfo;

const VGA_BUF: *mut u8 = 0xB8000 as *mut u8;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Clear screen
    for i in 0..(80 * 25) {
        unsafe {
            *VGA_BUF.add(i * 2) = b' ';
            *VGA_BUF.add(i * 2 + 1) = 0x07;
        }
    }

    print_at(0, 0, b"  Choosable Kernel v0.1  ", 0x1F);
    print_at(2, 0, b"==========================", 0x17);
    print_at(4, 0, b"64-bit long mode active!", 0x0A);
    print_at(6, 0, b"Scanning for ISO files...", 0x07);

    // Halt
    loop {
        unsafe { core::arch::asm!("hlt") }
    }
}

fn print_at(row: usize, col: usize, s: &[u8], attr: u8) {
    let mut offset = (row * 80 + col) * 2;
    for &ch in s {
        if ch == 0 { break; }
        unsafe {
            *VGA_BUF.add(offset) = ch;
            *VGA_BUF.add(offset + 1) = attr;
        }
        offset += 2;
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop { unsafe { core::arch::asm!("hlt") } }
}