// ═══════════════════════════════════════════════════════════════════════════
//  I/O port helpers
// ═══════════════════════════════════════════════════════════════════════════

pub fn outb(port: u16, val: u8) {
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") val) }
}
pub fn inb(port: u16) -> u8 {
    let v: u8;
    unsafe { core::arch::asm!("in al, dx", out("al") v, in("dx") port) };
    v
}

pub fn outw(port: u16, val: u16) {
    unsafe { core::arch::asm!("out dx, ax", in("dx") port, in("ax") val) }
}
pub fn inw(port: u16) -> u16 {
    let v: u16;
    unsafe { core::arch::asm!("in ax, dx", out("ax") v, in("dx") port) };
    v
}