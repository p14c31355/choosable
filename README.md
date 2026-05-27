# Choosable

**A pure Rust tool for creating bootable USB drives with support for ISO, WIM, IMG, VHD(x), and EFI files.**

Just copy your image files to the USB drive and boot from them — no need to reformat every time. You can store multiple images at once and select from a boot menu.

## Features

- **One-time setup**: Install Choosable once, then just copy ISO/WIM/IMG/VHD(x)/EFI files
- **Multiple images**: Store many bootable images on a single drive
- **MBR and GPT**: Both partition styles supported
- **Non-destructive installation**: Install without losing existing data (NTFS/ext2/3/4)
- **Secure Boot support**: Toggle with `-s`/`-S` flags
- **Multiple filesystems**: exFAT (default), NTFS, FAT32
- **Reserved space**: Reserve unused space at the end of the drive
- **Safe updates**: Update Choosable without touching your files
- **FAT16 EFI partition**: Properly formatted VTOYEFI-equivalent partition
- **CRC32-verified GPT**: All GPT headers written with correct checksums
- **4K-aligned partitions**: Ensures optimal alignment for modern drives

## Installation

### From Source

```bash
git clone https://github.com/p14c31355/choosable.git
cd choosable
cargo build --release
```

The binary will be at `target/release/choosable`.

## CLI Usage

### Install

```bash
# Install to /dev/sdX with MBR (default)
sudo choosable install /dev/sdX

# Install with GPT partition style
sudo choosable install -g /dev/sdX

# Force install even if already installed
sudo choosable install -I /dev/sdX

# Non-destructive installation
sudo choosable install -n /dev/sdX

# Disable Secure Boot
sudo choosable install -S /dev/sdX

# Custom filesystem and label
sudo choosable install --fs ntfs -L "MyDrive" /dev/sdX

# Reserve 4 GiB at the end of the disk
sudo choosable install -r 4096 /dev/sdX

# Skip confirmation prompts
sudo choosable install -y /dev/sdX
```

### Update

```bash
# Update Choosable on an existing installation (safe, preserves files)
sudo choosable update /dev/sdX
```

### List

```bash
# View Choosable version and secure boot status on a disk
sudo choosable list /dev/sdX

# List all available disks
sudo choosable list-disks
```

*Note: root/sudo is required because Choosable writes directly to block devices.*

## GUI

Run Choosable without any arguments to launch the graphical interface:

```bash
choosable
```

The GUI lets you:
- Browse and select disks
- Configure all options (GPT, Secure Boot, Force, Non-destructive, filesystem type, label, reserved space)
- One-click Install, Update, or view disk info

## Supported Filesystems for Partition 1

| Filesystem | CLI Flag |
|------------|----------|
| exFAT | `--fs exfat` (default) |
| NTFS | `--fs ntfs` |
| FAT32 | `--fs fat32` |

## Disk Layout

```
┌──────────────────────────────────────────┐
│  Sector 0: MBR (Master Boot Record)      │
├──────────────────────────────────────────┤
│  Sector 1–2047: Choosable core image     │
├──────────────────────────────────────────┤
│  Sector 2048 – (end – 32 MiB):          │
│    Partition 1: Data (exFAT/NTFS/FAT32)  │
│      (your ISO/VHD/IMG files go here)    │
├──────────────────────────────────────────┤
│  Last 32 MiB: Partition 2 (EFI, FAT16)  │
│    Bootloader & Secure Boot support      │
└──────────────────────────────────────────┘
```

## How It Works

Choosable writes a bootable MBR (or GPT protective MBR) to the disk, creates two partitions:
1. A large data partition (exFAT by default) for your bootable image files
2. A small (32 MiB) FAT16 EFI partition containing the bootloader

The bootloader at sector 0 scans the data partition for ISO/WIM/IMG/VHD(x)/EFI files and presents a menu at boot time.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](docs/LICENSE-APACHE))
- MIT license ([LICENSE-MIT](docs/LICENSE-MIT))

at your option.