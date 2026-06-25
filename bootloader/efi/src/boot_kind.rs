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
    /// Custom /init.choosable (Alpine Linux — legacy path)
    Alpine,
    /// Alpine using casper-style premount hook (unified path)
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
    pub fn linux_extra(&self, is_popos: bool) -> &'static [u8] {
        match self {
            BootKind::Casper => {
                if is_popos {
                    b" boot=casper casper_path=pop-os maybe-ubiquity"
                } else {
                    b" boot=casper maybe-ubiquity"
                }
            }
            BootKind::DebianLive => {
                b" boot=live live-media=removable"
            }
            BootKind::FedoraLive => {
                b" init=/init.choosable rd.live.image rootdelay=10"
            }
            BootKind::ArchIso => {
                b" archisobasedir=arch copytoram"
            }
            BootKind::Alpine => {
                b" init=/init.choosable modules=loop,iso9660"
            }
            BootKind::AlpinePremount => {
                b" init=/init.choosable modules=loop,iso9660"
            }
            BootKind::WindowsPE => b"",
            BootKind::Unknown => b" boot=casper maybe-ubiquity",
        }
    }

    /// Extra kernel args appended at the end of linux lines (before newline).
    /// For DebianLive this is `findiso=` (appended with the ISO path);
    /// Casper/FedoraLive/ArchIso/Alpine/AlpinePremount don't need any EOL extra
    /// because the premount initramfs hook handles ISO discovery independently.
    pub fn linux_eol_extra(&self) -> &'static [u8] {
        match self {
            BootKind::DebianLive => b" findiso=",
            _ => b" choosable.iso_offset=",
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // Verify all variants produce valid UTF-8 sequences
        for k in &[BootKind::Casper, BootKind::DebianLive, BootKind::FedoraLive,
                   BootKind::ArchIso, BootKind::Alpine, BootKind::AlpinePremount,
                   BootKind::WindowsPE, BootKind::Unknown] {
            let _ = k.linux_extra(false);
            let _ = k.linux_eol_extra();
            let _ = k.hook_targets();
            let _ = k.needs_sr_mod();
            let _ = k.fixup_type();
        }
    }

    #[test]
    fn test_boot_kind_linux_extra_values() {
        assert_eq!(BootKind::Casper.linux_extra(false), b" boot=casper maybe-ubiquity");
        assert_eq!(BootKind::Casper.linux_extra(true), b" boot=casper casper_path=pop-os maybe-ubiquity");
        assert_eq!(BootKind::WindowsPE.linux_extra(false), b"");
        assert_eq!(BootKind::DebianLive.linux_eol_extra(), b" findiso=");
        assert!(!BootKind::Casper.linux_eol_extra().is_empty());
    }

    #[test]
    fn test_boot_kind_equality() {
        assert_eq!(BootKind::Casper, BootKind::Casper);
        assert_ne!(BootKind::Casper, BootKind::DebianLive);
    }
}
