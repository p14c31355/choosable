// ═══════════════════════════════════════════════════════════════════════════
//  VGA text mode output
// ═══════════════════════════════════════════════════════════════════════════

pub const VGA: *mut u8 = 0xB8000 as *mut u8;
pub const VGA_COLS: usize = 80;
pub const VGA_ROWS: usize = 25;

pub fn vga_clear(attr: u8) {
    for i in 0..(VGA_COLS * VGA_ROWS) {
        unsafe {
            *VGA.add(i * 2) = b' ';
            *VGA.add(i * 2 + 1) = attr;
        }
    }
}

pub fn vga_print(row: usize, col: usize, s: &[u8], attr: u8) {
    if row >= VGA_ROWS || col >= VGA_COLS {
        return;
    }
    let base = (row * VGA_COLS + col) * 2;
    let buf_end = VGA_COLS * VGA_ROWS * 2;
    let mut off = base;
    for &ch in s {
        if ch == 0 {
            break;
        }
        if off >= buf_end {
            break;
        }
        unsafe {
            *VGA.add(off) = ch;
            *VGA.add(off + 1) = attr;
        }
        off += 2;
    }
}

pub fn vga_print_byte(val: u8, row: usize, col: usize, attr: u8) {
    if val < 10 {
        vga_print(row, col, &[(val + b'0')], attr);
    } else {
        vga_print(row, col, &[(val - 10 + b'A')], attr);
    }
}

pub fn vga_print_u32(v: u32, row: usize, col: usize, attr: u8) {
    for i in 0..8 {
        let b = (v >> (28 - i * 4)) as u8 & 0xF;
        vga_print_byte(b, row, col + i, attr);
    }
}