// ═══════════════════════════════════════════════════════════════════════════
//  Boot kind detection — identifies distro family from ISO directory structure
// ═══════════════════════════════════════════════════════════════════════════
//
//  Instead of matching ISO filenames (ubuntu, arch, alpine, …), this module
//  inspects the actual contents of the ISO filesystem to determine the boot
//  protocol the distro expects.  This makes detection robust across different
//  ISO versions, derivatives, and renamed files.
//
//  Usage:
//    1. Call `scan_iso_structure()` while you have access to the real
//       Block I/O protocol.
//    2. Use the returned `BootDescriptor.boot_kind` to select kernel
//       cmdline arguments, premount hooks, and initrd fixup.
//    3. The old name-based `strategy.rs` dispatch is replaced by this.

// ═══════════════════════════════════════════════════════════════════════════
//  FixupType — identifies which initrd builder to use
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FixupType {
    /// initramfs-tools casper-premount hooks (Ubuntu, Mint, Pop)
    Casper,
    /// initramfs-tools live-premount hooks (Debian Live)
    LiveBoot,
    /// dracut premount hook (Fedora, RHEL, CentOS)
    Dracut,
    /// archiso copytoram hook (Arch Linux)
    Arch,
    /// No initrd fixup needed (Windows PE)
    WindowsPE,
    /// Custom /init.choosable (Alpine Linux — lightweight, no initramfs-tools)
    Alpine,
    /// Alpine using generic premount (unified path)
    AlpinePremount,
}

// ═══════════════════════════════════════════════════════════════════════════
//  HookTargetSet — controls which initramfs hooks to install
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug)]
pub struct HookTargetSet {
    pub live: bool,
    pub live_premount: bool,
    pub casper_premount: bool,
    pub casper_bottom: bool,
}

// ═══════════════════════════════════════════════════════════════════════════
//  BootKind — distro family identified from ISO directory structure
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BootKind {
    /// /casper/ directory — uses casper-premount hooks (Ubuntu, Mint, Pop!_OS)
    Casper,
    /// /live/ directory — uses live-premount hooks (Debian Live)
    DebianLive,
    /// /LiveOS/ directory — uses dracut + rd.live.image (Fedora, RHEL, CentOS)
    FedoraLive,
    /// /arch/ directory — uses archiso initramfs (Arch Linux)
    ArchIso,
    /// /.alpine-release or /apks/ — Alpine Linux (custom init or premount)
    Alpine,
    /// Alpine using unified casper-style premount (preferred for Alpine)
    AlpinePremount,
    /// /sources/boot.wim — Windows PE
    WindowsPE,
    /// No known marker found — use Casper defaults as fallback
    Unknown,
}

impl BootKind {
    /// Kernel cmdline arguments to inject after the second argument on linux lines.
    /// `is_popos` is true when the ISO filename contains "pop" or "pop-os".
    /// `iso_location` provides partition GUID, number, offset, path, and size for precise ISO location.
    /// Writes into `buf` and returns a slice of the used portion, or None if buffer is insufficient.
    pub fn linux_extra<'a>(&self, is_popos: bool, iso_location: Option<&crate::locator::IsoLocation>, buf: &'a mut [u8]) -> Option<&'a [u8]> {
        let mut pos = 0usize;
        let initial_cap = buf.len();

        match self {
            BootKind::Casper => {
                if is_popos {
                    let chunk = b" boot=casper casper_path=pop-os maybe-ubiquity init=/init.choosable";
                    if pos + chunk.len() > initial_cap { return None; }
                    pos += copy_at(buf, pos, chunk);
                } else {
                    let chunk = b" boot=casper maybe-ubiquity init=/init.choosable";
                    if pos + chunk.len() > initial_cap { return None; }
                    pos += copy_at(buf, pos, chunk);
                }
            }
            BootKind::DebianLive => {
                let chunk = b" boot=live live-media=removable init=/init.choosable";
                if pos + chunk.len() > initial_cap { return None; }
                pos += copy_at(buf, pos, chunk);
            }
            BootKind::FedoraLive => {
                let chunk = b" rd.live.image root=live:CDLABEL=CHOOSABLE rd.live.dir=/LiveOS rootdelay=10 init=/init.choosable";
                if pos + chunk.len() > initial_cap { return None; }
                pos += copy_at(buf, pos, chunk);
            }
            BootKind::ArchIso => {
                if let Some(loc) = iso_location {
                    let chunk = b" archisodevice=/dev/disk/by-partuuid/";
                    if pos + chunk.len() + 36 > initial_cap { return None; }
                    pos += copy_at(buf, pos, chunk);
                    pos += guid_to_partuuid_bytes(&loc.partition_guid, &mut buf[pos..]);
                    let chunk2 = b" archisobasedir=arch copytoram init=/init.choosable";
                    if pos + chunk2.len() > initial_cap { return None; }
                    pos += copy_at(buf, pos, chunk2);
                } else {
                    let chunk = b" archisodevice=LABEL=CHOOSABLE archisobasedir=arch copytoram init=/init.choosable";
                    if pos + chunk.len() > initial_cap { return None; }
                    pos += copy_at(buf, pos, chunk);
                }
            }
            BootKind::Alpine | BootKind::AlpinePremount => {
                let chunk = b" init=/init.choosable modules=loop,iso9660";
                if pos + chunk.len() > initial_cap { return None; }
                pos += copy_at(buf, pos, chunk);
            }
            BootKind::WindowsPE => {}
            BootKind::Unknown => {
                let chunk = b" boot=casper maybe-ubiquity init=/init.choosable";
                if pos + chunk.len() > initial_cap { return None; }
                pos += copy_at(buf, pos, chunk);
            }
        }

        // Append choosable.* parameters if IsoLocation is available
        if let Some(loc) = iso_location {
            let new_pos = append_choosable_params_slice(buf, pos, loc);
            if new_pos == pos && loc.partition_number > 0 {
                // append_choosable_params_slice returned early due to capacity
                return None;
            }
            pos = new_pos;
        }

        Some(&buf[..pos])
    }

    /// Extra kernel args appended at the end of linux lines (before newline).
    /// DebianLive uses `findiso=` (native live-boot ISO scanner).
    /// Casper uses `iso-scan/filename=` so casper-premount can locate the ISO.
    /// Other boot kinds use premount initrd for ISO discovery — no EOL needed.
    pub fn linux_eol_extra(&self) -> &'static [u8] {
        match self {
            BootKind::Casper => b" iso-scan/filename=",
            BootKind::DebianLive => b" findiso=",
            _ => b"",
        }
    }

    /// Which initramfs-tools hook directories the premount CPIO should populate.
    pub fn hook_targets(&self) -> HookTargetSet {
        match self {
            BootKind::Casper => HookTargetSet {
                live: false,
                live_premount: false,
                casper_premount: true,
                casper_bottom: true,
            },
            BootKind::DebianLive => HookTargetSet {
                live: true,
                live_premount: true,
                casper_premount: false,
                casper_bottom: false,
            },
            BootKind::FedoraLive => HookTargetSet {
                live: true,
                live_premount: true,
                casper_premount: true,
                casper_bottom: true,
            },
            BootKind::ArchIso => HookTargetSet {
                live: false,
                live_premount: false,
                casper_premount: true,
                casper_bottom: false,
            },
            BootKind::AlpinePremount => HookTargetSet {
                live: false,
                live_premount: false,
                casper_premount: false,
                casper_bottom: false,
            },
            BootKind::Alpine | BootKind::WindowsPE | BootKind::Unknown => {
                HookTargetSet {
                    live: false,
                    live_premount: false,
                    casper_premount: false,
                    casper_bottom: false,
                }
            }
        }
    }

    /// Whether the premount script should load `sr_mod` (needed for real CD-ROM).
    pub fn needs_sr_mod(&self) -> bool {
        matches!(self, BootKind::Casper | BootKind::Unknown)
    }

    /// The `FixupType` corresponding to this boot kind, used to select the
    /// `EarlyBootFixup` implementation.
    pub fn fixup_type(&self) -> FixupType {
        match self {
            BootKind::Casper => FixupType::Casper,
            BootKind::DebianLive => FixupType::LiveBoot,
            BootKind::FedoraLive => FixupType::Dracut,
            BootKind::ArchIso => FixupType::Arch,
            BootKind::Alpine => FixupType::Alpine,
            BootKind::AlpinePremount => FixupType::Alpine,
            BootKind::WindowsPE => FixupType::WindowsPE,
            BootKind::Unknown => FixupType::Casper,
        }
    }
}

/// Copy a byte slice into buf at position pos, returning the number of bytes copied.
fn copy_at(buf: &mut [u8], pos: usize, src: &[u8]) -> usize {
    let avail = buf.len().saturating_sub(pos);
    let n = src.len().min(avail);
    buf[pos..pos + n].copy_from_slice(&src[..n]);
    n
}

/// Write a GUID as PARTUUID string (hex with dashes, lowercase) into buf.
/// Returns the number of bytes written (always 36 for a valid GUID).
fn guid_to_partuuid_bytes(guid: &crate::protocol::Guid, buf: &mut [u8]) -> usize {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let g = |v: u32, i: usize| -> u8 { HEX[((v >> (i as u32 * 4)) & 0xF) as usize] };

    // PARTUUID format: time_low-time_mid-time_hi_version-clock_seq-node
    // d1 (u32) -> 8 hex digits
    // d2 (u16) -> 4 hex digits
    // d3 (u16) -> 4 hex digits
    // d4[0..1] -> 2 hex digits
    // d4[2..7] -> 12 hex digits
    // Total: 8+1+4+1+4+1+2+1+12 = 32 hex + 4 dashes = 36 chars

    let mut pos = 0usize;
    // d1: 8 hex digits, MSB first
    for i in (0..8).rev() { if pos < buf.len() { buf[pos] = g(guid.d1, i); pos += 1; } }
    if pos < buf.len() { buf[pos] = b'-'; pos += 1; }
    // d2: 4 hex digits, MSB first
    for i in (0..4).rev() { if pos < buf.len() { buf[pos] = g(guid.d2 as u32, i); pos += 1; } }
    if pos < buf.len() { buf[pos] = b'-'; pos += 1; }
    // d3: 4 hex digits, MSB first
    for i in (0..4).rev() { if pos < buf.len() { buf[pos] = g(guid.d3 as u32, i); pos += 1; } }
    if pos < buf.len() { buf[pos] = b'-'; pos += 1; }
    // d4[0..1]: 2 hex digits (stored as bytes, no byte swap)
    for j in 0..2 {
        let b = guid.d4[j];
        if pos < buf.len() { buf[pos] = HEX[(b >> 4) as usize]; pos += 1; }
        if pos < buf.len() { buf[pos] = HEX[(b & 0xF) as usize]; pos += 1; }
    }
    if pos < buf.len() { buf[pos] = b'-'; pos += 1; }
    // d4[2..7]: 12 hex digits
    for j in 2..8 {
        let b = guid.d4[j];
        if pos < buf.len() { buf[pos] = HEX[(b >> 4) as usize]; pos += 1; }
        if pos < buf.len() { buf[pos] = HEX[(b & 0xF) as usize]; pos += 1; }
    }

    pos
}

/// Append choosable.* kernel parameters from IsoLocation into buf at position pos.
/// Returns the new position after writing, or the original pos if capacity is insufficient.
fn append_choosable_params_slice(buf: &mut [u8], mut pos: usize, loc: &crate::locator::IsoLocation) -> usize {
    let start_pos = pos;
    let buf_len = buf.len();

    // " choosable.part_guid="
    let chunk = b" choosable.part_guid=";
    if pos + chunk.len() + 36 > buf_len { return start_pos; }
    pos += copy_at(buf, pos, chunk);
    pos += guid_to_partuuid_bytes(&loc.partition_guid, &mut buf[pos..]);

    // " choosable.part_num=<decimal>"
    let chunk = b" choosable.part_num=";
    if pos + chunk.len() + 10 > buf_len { return start_pos; }
    pos += copy_at(buf, pos, chunk);
    pos += write_u64_decimal(buf, pos, loc.partition_number as u64);

    // " choosable.iso_offset=<decimal>"
    let chunk = b" choosable.iso_offset=";
    if pos + chunk.len() + 20 > buf_len { return start_pos; }
    pos += copy_at(buf, pos, chunk);
    pos += write_u64_decimal(buf, pos, loc.offset_bytes());

    // " choosable.iso_path=<path>"
    let path = loc.path_without_leading_slash();
    if !path.is_empty() {
        let chunk = b" choosable.iso_path=";
        if pos + chunk.len() + path.len() > buf_len { return start_pos; }
        pos += copy_at(buf, pos, chunk);
        pos += copy_at(buf, pos, path);
    }

    // " choosable.iso_size=<decimal>"
    let chunk = b" choosable.iso_size=";
    if pos + chunk.len() + 20 > buf_len { return start_pos; }
    pos += copy_at(buf, pos, chunk);
    pos += write_u64_decimal(buf, pos, loc.file_size);

    pos
}

/// Write a u64 decimal string into buf at position pos. Returns bytes written.
fn write_u64_decimal(buf: &mut [u8], mut pos: usize, mut v: u64) -> usize {
    let start = pos;
    // Write digits in reverse, then reverse them
    let mut digits = [0u8; 20];
    let mut dpos = 0usize;
    if v == 0 {
        if pos < buf.len() { buf[pos] = b'0'; return 1; }
        return 0;
    }
    while v > 0 && dpos < 20 {
        digits[dpos] = b'0' + (v % 10) as u8;
        v /= 10;
        dpos += 1;
    }
    for i in (0..dpos).rev() {
        if pos < buf.len() { buf[pos] = digits[i]; pos += 1; }
    }
    pos - start
}

// ═══════════════════════════════════════════════════════════════════════════
//  BootloaderType — what kind of boot config the ISO ships
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BootloaderType {
    /// Standard GRUB (linux/initrd lines) — the most common.
    Grub,
    /// No bootloader config found — chainload EFI binary directly.
    None_,
}

// ═══════════════════════════════════════════════════════════════════════════
//  BootDescriptor — result of scanning an ISO
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug)]
pub struct BootDescriptor {
    pub boot_kind: BootKind,
    pub bootloader: BootloaderType,
}

// ═══════════════════════════════════════════════════════════════════════════
//  Helper: case-insensitive name comparison for ISO9660 directory records
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) fn name_matches(iso_name: &[u8], pattern: &[u8]) -> bool {
    iso_name.len() == pattern.len()
        && iso_name.iter().zip(pattern.iter()).all(|(&a, &b)| (a | 0x20) == (b | 0x20))
}

/// Convert a GUID to PARTUUID string format for testing only.
/// Returns a fixed 36-byte string (xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx).
fn guid_to_partuuid(guid: &crate::protocol::Guid) -> [u8; 36] {
    let mut out = [0u8; 36];
    let written = guid_to_partuuid_bytes(guid, &mut out);
    let _ = written;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Guid;

    #[test]
    fn test_guid_to_partuuid() {
        let guid = Guid { d1: 0x12345678, d2: 0x9abc, d3: 0xdef0, d4: [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0] };
        let partuuid = guid_to_partuuid(&guid);
        assert_eq!(&partuuid[..], b"12345678-9abc-def0-1234-56789abcdef0");
    }

    #[test]
    fn test_linux_extra_with_iso_location() {
        use crate::locator::IsoLocation;
        let guid = Guid { d1: 0x11111111, d2: 0x2222, d3: 0x3333, d4: [0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb] };
        let file_path_str = b"/boot/ubuntu.iso";
        let loc = IsoLocation {
            partition_guid: guid,
            partition_number: 2,
            file_path: {
                let mut fp = [0u8; 256];
                let plen = file_path_str.len();
                fp[..plen].copy_from_slice(&file_path_str[..]);
                fp
            },
            file_path_len: file_path_str.len(),
            file_size: 2_000_000_000,
            part1_lba: 2048,
            iso_lba: 100_000,
        };
        let mut buf = [0u8; 512];
        let extra = BootKind::Casper.linux_extra(false, Some(&loc), &mut buf).unwrap();
        let s = core::str::from_utf8(extra).unwrap();
        assert!(s.contains("choosable.part_guid=11111111-2222-3333-4455-66778899aabb"));
        assert!(s.contains("choosable.part_num=2"));
        assert!(s.contains("choosable.iso_offset="));
        assert!(s.contains("choosable.iso_path=boot/ubuntu.iso"));
        assert!(s.contains("choosable.iso_size=2000000000"));
    }

    #[test]
    fn test_name_matches() {
        assert!(name_matches(b"CASPER", b"casper"));
        assert!(name_matches(b"casper", b"CASPER"));
        assert!(name_matches(b"LiveOS", b"liveos"));
        assert!(name_matches(b"BOOTX64.EFI", b"bootx64.efi"));
    }

    #[test]
    fn test_name_matches_fails() {
        assert!(!name_matches(b"CASPER", b"caspe"));
        assert!(!name_matches(b"live", b"lives"));
        assert!(!name_matches(b"BOOT", b"BOOTX"));
    }

    #[test]
    fn test_boot_kind_linux_extra_no_panic() {
        let mut buf = [0u8; 512];
        for k in &[BootKind::Casper, BootKind::DebianLive, BootKind::FedoraLive,
                   BootKind::ArchIso, BootKind::Alpine, BootKind::AlpinePremount,
                   BootKind::WindowsPE, BootKind::Unknown] {
            let _ = k.linux_extra(false, None, &mut buf);
            let _ = k.linux_eol_extra();
            let _ = k.hook_targets();
            let _ = k.needs_sr_mod();
            let _ = k.fixup_type();
        }
    }

    #[test]
    fn test_boot_kind_linux_extra_values() {
        let mut buf = [0u8; 512];
        assert_eq!(BootKind::Casper.linux_extra(false, None, &mut buf).unwrap(), b" boot=casper maybe-ubiquity init=/init.choosable");
        let mut buf2 = [0u8; 512];
        assert_eq!(BootKind::Casper.linux_extra(true, None, &mut buf2).unwrap(), b" boot=casper casper_path=pop-os maybe-ubiquity init=/init.choosable");
        let mut buf3 = [0u8; 512];
        assert_eq!(BootKind::WindowsPE.linux_extra(false, None, &mut buf3).unwrap(), b"");
        assert_eq!(BootKind::DebianLive.linux_eol_extra(), b" findiso=");
        assert_eq!(BootKind::Casper.linux_eol_extra(), b" iso-scan/filename=");
    }

    #[test]
    fn test_boot_kind_linux_extra_insufficient_buffer() {
        use crate::locator::IsoLocation;
        let guid = Guid { d1: 0x11111111, d2: 0x2222, d3: 0x3333, d4: [0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb] };
        let long_path = {
            let mut s = [0u8; 256];
            for i in 0..250 {
                s[i] = b'x';
            }
            s
        };
        let loc = IsoLocation {
            partition_guid: guid,
            partition_number: 2,
            file_path: long_path,
            file_path_len: 250,
            file_size: 2_000_000_000,
            part1_lba: 2048,
            iso_lba: 100_000,
        };
        let mut small_buf = [0u8; 50];
        assert_eq!(BootKind::Casper.linux_extra(false, Some(&loc), &mut small_buf), None);
    }

    #[test]
    fn test_boot_kind_equality() {
        assert_eq!(BootKind::Casper, BootKind::Casper);
        assert_ne!(BootKind::Casper, BootKind::DebianLive);
    }
}
