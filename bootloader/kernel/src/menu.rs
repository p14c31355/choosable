// ═══════════════════════════════════════════════════════════════════════════
//  BIOS boot menu
// ═══════════════════════════════════════════════════════════════════════════

use crate::fs::{scan_filesystem, DirEntry, FsCtx};
use crate::iso::boot_iso;
use crate::kbd::{kbd_wait_key, scancode_to_ascii};
use crate::vga::{vga_clear, vga_print};

const MENU_PAGE_SIZE: usize = 16;

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

/// Convert the decimal number entered by the user to a zero-based index.
/// Keeping this separate makes the 10+ item behavior testable without VGA or
/// keyboard hardware.
fn parse_selection(digits: &[u8], len: usize, count: usize) -> Option<usize> {
    if len == 0 || len > digits.len() {
        return None;
    }
    let mut value = 0usize;
    for &digit in &digits[..len] {
        if !(b'0'..=b'9').contains(&digit) {
            return None;
        }
        value = value.saturating_mul(10).saturating_add((digit - b'0') as usize);
    }
    if value > 0 && value <= count {
        Some(value - 1)
    } else {
        None
    }
}

fn render_menu(files: &[DirEntry], count: usize, page_start: usize) {
    vga_clear(0x07);
    vga_print(0, 8, b"=== Choosable ISO Boot Menu ===", 0x1F);

    let page_end = (page_start + MENU_PAGE_SIZE).min(count);
    for (row, index) in (page_start..page_end).enumerate() {
        let row = 3 + row;
        let mut num_buf = [0u8; 20];
        let num_str = format_u64((index + 1) as u64, &mut num_buf);
        vga_print(row, 1, num_str, 0x0A);
        vga_print(row, 1 + num_str.len(), b". ", 0x07);
        let name_len = files[index].name_len.min(43);
        vga_print(row, 4, &files[index].name[..name_len], 0x07);

        let size_mb = files[index].file_size / (1024 * 1024);
        let mut size_buf = [0u8; 20];
        let size_str = format_u64(size_mb, &mut size_buf);
        vga_print(row, 50, b"(", 0x07);
        vga_print(row, 51, size_str, 0x07);
        vga_print(row, 51 + size_str.len(), b" MiB)", 0x07);
    }

    let prompt_row = 21;
    vga_print(prompt_row, 1, b"Enter number, n/p page, r refresh: ", 0x07);
}

pub fn show_menu(files: &[DirEntry], count: usize, ctx: &FsCtx) -> ! {
    if count == 0 {
        vga_clear(0x07);
        vga_print(4, 10, b"No ISO files found.", 0x0C);
        vga_print(6, 8, b"Press any key to halt...", 0x07);
        kbd_wait_key();
        loop { unsafe { core::arch::asm!("hlt") } }
    }

    // The scanner owns the caller's array, so refresh into a local array and
    // keep the menu iterative.  This avoids growing the real-mode stack every
    // time the user presses r or changes page.
    let mut current_files = [DirEntry::zero(); 64];
    current_files[..count.min(64)].copy_from_slice(&files[..count.min(64)]);
    let mut current_count = count.min(64);
    let mut page_start = 0usize;
    let mut digits = [0u8; 2];
    let mut digit_len = 0usize;

    loop {
        render_menu(&current_files, current_count, page_start);
        if digit_len > 0 {
            vga_print(22, 1, b"Selection: ", 0x07);
            vga_print(22, 11, &digits[..digit_len], 0x0A);
        }

        let ch = scancode_to_ascii(kbd_wait_key());
        match ch {
            Some(b'r') | Some(b'R') => {
                scan_filesystem(ctx, &mut current_files, &mut current_count);
                page_start = 0;
                digit_len = 0;
            }
            Some(b'n') | Some(b'N') => {
                if page_start + MENU_PAGE_SIZE < current_count {
                    page_start += MENU_PAGE_SIZE;
                }
                digit_len = 0;
            }
            Some(b'p') | Some(b'P') => {
                page_start = page_start.saturating_sub(MENU_PAGE_SIZE);
                digit_len = 0;
            }
            Some(8) => {
                digit_len = digit_len.saturating_sub(1);
            }
            Some(d @ b'0'..=b'9') if digit_len < digits.len() => {
                digits[digit_len] = d;
                digit_len += 1;
            }
            Some(b'\n') => {
                if let Some(index) = parse_selection(&digits, digit_len, current_count) {
                    boot_iso(&current_files[index], ctx);
                }
                digit_len = 0;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_selection;

    #[test]
    fn parses_two_digit_selection() {
        assert_eq!(parse_selection(b"12", 2, 16), Some(11));
    }

    #[test]
    fn rejects_zero_and_out_of_range() {
        assert_eq!(parse_selection(b"0", 1, 16), None);
        assert_eq!(parse_selection(b"17", 2, 16), None);
    }
}
