// ═══════════════════════════════════════════════════════════════════════════
//  Simple keyboard input
// ═══════════════════════════════════════════════════════════════════════════

use crate::io::inb;

pub fn kbd_poll() -> Option<u8> {
    if inb(0x64) & 1 == 0 {
        return None;
    }
    Some(inb(0x60))
}

pub fn kbd_wait_key() -> u8 {
    loop {
        if let Some(sc) = kbd_poll() {

            return sc;
        }
    }
}

pub fn scancode_to_ascii(sc: u8) -> Option<u8> {
    match sc {
        0x02..=0x0A => Some(b'1' + (sc - 0x02)),
        0x0B => Some(b'0'),
        0x13 => Some(b'r'),
        0x1F => Some(b'R'),
        0x1C => Some(b'\n'),
        _ => None,
    }
}