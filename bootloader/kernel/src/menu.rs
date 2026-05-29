// ═══════════════════════════════════════════════════════════════════════════
//  Boot menu UI
// ═══════════════════════════════════════════════════════════════════════════

use crate::fs::{scan_filesystem, DirEntry, FsCtx};
use crate::iso::boot_iso;
use crate::kbd::{kbd_wait_key, scancode_to_ascii};
use crate::vga::{vga_clear, vga_print};

fn format_u64(mut v: u64, buf: &mut [u8; 20]) -> &[u8] {
    let mut pos = buf.len();
    if v == 0 {
        pos -= 1;
        buf[pos] = b'0';
    }
    while v > 0 && pos > 0 {
        pos -= 1;
        buf[pos] = (v % 10) as u8 + b'0';
        v /= 10;
    }
    &buf[pos..]
}

pub fn show_menu(files: &[DirEntry], count: usize, ctx: &FsCtx) -> ! {
    vga_clear(0x07);
    vga_print(0, 8, b"=== Choosable ISO Boot Menu ===", 0x1F);

    if count == 0 {
        vga_print(4, 10, b"No ISO files found.", 0x0C);
        vga_print(6, 8, b"Press any key to halt...", 0x07);
        kbd_wait_key();
        loop {
            unsafe { core::arch::asm!("hlt") }
        }
    }

    for i in 0..count {
        let row = 3 + i;
        if row > 22 {
            break;
        }
        let mut num_buf = [0u8; 20];
        let num_str = format_u64((i + 1) as u64, &mut num_buf);
        vga_print(row, 1, num_str, 0x0A);
        vga_print(row, 1 + num_str.len(), b". ", 0x07);
        if files[i].name_len > 0 {
            vga_print(row, 4, &files[i].name[..files[i].name_len], 0x07);
        }
        let size_mb = files[i].file_size / (1024 * 1024);
        let mut size_buf = [0u8; 20];
        let size_str = format_u64(size_mb, &mut size_buf);
        vga_print(row, 50, b"(", 0x07);
        vga_print(row, 51, size_str, 0x07);
        vga_print(row, 51 + size_str.len(), b" MiB)", 0x07);
    }

    let prompt_row = 3 + count + 1;
    vga_print(prompt_row, 1, b"Enter number (or 'r' to refresh): ", 0x07);

    loop {
        let sc = kbd_wait_key();
        let ch = scancode_to_ascii(sc);
        match ch {
            Some(b'r') | Some(b'R') => {
                let mut new_files: [DirEntry; 64] =
                    unsafe { core::mem::zeroed() };
                let mut new_count: usize = 0;
                scan_filesystem(ctx, &mut new_files, &mut new_count);
                show_menu(&new_files, new_count, ctx);
            }
            Some(b'\n') => continue,
            Some(d) if d.is_ascii_digit() => {
                let idx = if d == b'0' { 9 } else { (d - b'1') as usize };
                if idx < count {
                    boot_iso(&files[idx], ctx);
                }
            }
            _ => {}
        }
    }
}