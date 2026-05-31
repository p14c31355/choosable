// ═══════════════════════════════════════════════════════════════════════════
//  Console output helpers
// ═══════════════════════════════════════════════════════════════════════════

use crate::protocol::{Key, ResetType, RuntimeServices, SimpleTextInput, SimpleTextOutput, SystemTable, EFI_SUCCESS};

pub fn prints(co: &mut SimpleTextOutput, s: &[u8]) {
    let mut buf = [0u16; 256];
    let len = s.len().min(255);
    for (i, &b) in s[..len].iter().enumerate() {
        buf[i] = b as u16;
    }
    buf[len] = 0;
    unsafe { (co.output_string)(co as *mut SimpleTextOutput, buf.as_ptr()) };
}

pub fn print_raw(st: &mut SystemTable, s: &[u8]) {
    if !st.con_out.is_null() {
        prints(
            unsafe { &mut *(st.con_out as *mut SimpleTextOutput) },
            s,
        );
    }
}

pub fn die(st: &mut SystemTable, s: &[u8]) -> ! {
    print_raw(st, s);
    halt_or_reboot(st);
}

pub fn print_hex(st: &mut SystemTable, prefix: &[u8], val: u64) {
    print_raw(st, prefix);
    for i in (0..16).rev() {
        print_raw(
            st,
            &[b"0123456789ABCDEF"[((val >> (i * 4)) & 0xF) as usize]],
        );
    }
}

pub fn banner(st: &mut SystemTable) {
    if !st.con_out.is_null() {
        let con = unsafe { &mut *(st.con_out as *mut SimpleTextOutput) };
        prints(con, b"\r\n========================================\r\n\0");
        prints(con, b"        Choosable UEFI Bootloader       \r\n\0");
        prints(con, b"========================================\r\n\0");
    }
}

pub fn format_u64_buf(v: u64) -> ([u8; 20], usize) {
    let mut buf = [0u8; 20];
    if v == 0 {
        buf[0] = b'0';
        return (buf, 1);
    }
    let mut pos = 20;
    let mut n = v;
    while n > 0 && pos > 0 {
        pos -= 1;
        buf[pos] = (n % 10) as u8 + b'0';
        n /= 10;
    }
    (buf, 20 - pos)
}

pub fn wait_for_key(st: &mut SystemTable) {
    let bs = unsafe { &mut *st.boot_services };
    if !st.con_in.is_null() {
        let ci = unsafe { &mut *(st.con_in as *mut SimpleTextInput) };
        loop {
            let mut k = Key { sc: 0, uc: 0 };
            if unsafe { (ci.read_key_stroke)(ci as *mut _, &mut k) } == EFI_SUCCESS {
                break;
            }
            unsafe { (bs.stall)(10_000) };
        }
    }
}

fn system_reset(st: &mut SystemTable) -> ! {
    let rt = unsafe { &mut *st.runtime_services };
    unsafe { (rt.reset_system)(ResetType::ResetCold, 0, 0, core::ptr::null_mut()) };
    loop {
        unsafe { core::arch::asm!("hlt") }
    }
}

pub fn print_dec(st: &mut SystemTable, v: u64) {
    let (buf, len) = format_u64_buf(v);
    print_raw(st, &buf[20 - len..]);
}

pub fn halt_or_reboot(st: &mut SystemTable) -> ! {
    let bs = unsafe { &mut *st.boot_services };
    if !st.con_in.is_null() {
        let ci = unsafe { &mut *(st.con_in as *mut SimpleTextInput) };
        print_raw(st, b"Press any key to reboot.\r\n\0");
        for _ in 0..300 {
            unsafe { (bs.stall)(100_000) };
            let mut k = Key { sc: 0, uc: 0 };
            if unsafe { (ci.read_key_stroke)(ci as *mut _, &mut k) } == EFI_SUCCESS {
                system_reset(st);
            }
        }
    }
    system_reset(st);
}