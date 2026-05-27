use clap::{Parser, Subcommand};
use crate::constants::*;

/// choosable - A pure Rust implementation of Choosable
/// Create bootable USB drives for ISO/WIM/IMG/VHD(x)/EFI files
#[derive(Parser, Debug)]
#[command(name = "choosable", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Install Choosable to a disk
    Install {
        /// Disk device (e.g., /dev/sdb)
        disk: String,

        /// Force install even if Choosable is already installed
        #[arg(short = 'I', long)]
        force: bool,

        /// Use GPT partition style (default: MBR)
        #[arg(short = 'g', long)]
        gpt: bool,

        /// Enable secure boot support (default: enabled)
        #[arg(short = 's', long)]
        secure_boot: bool,

        /// Disable secure boot support
        #[arg(short = 'S', long)]
        no_secure_boot: bool,

        /// Reserve space at bottom of disk (in MiB)
        #[arg(short = 'r', long)]
        reserve_space: Option<u64>,

        /// Label for the first partition (default: "Choosable")
        #[arg(short = 'L', long, default_value = DEFAULT_CHOOSABLE_LABEL)]
        label: String,

        /// Try non-destructive installation
        #[arg(short = 'n', long)]
        non_destructive: bool,

        /// Filesystem type for partition 1: exfat, ntfs, fat32
        #[arg(long = "fs", default_value = "exfat")]
        filesystem: String,

        /// Skip prompt for confirmation
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Update Choosable on an already-installed disk
    Update {
        /// Disk device (e.g., /dev/sdb)
        disk: String,

        /// Enable secure boot support
        #[arg(short = 's', long)]
        secure_boot: bool,

        /// Disable secure boot support
        #[arg(short = 'S', long)]
        no_secure_boot: bool,

        /// Skip prompt for confirmation
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// List Choosable information on a disk
    List {
        /// Disk device (e.g., /dev/sdb)
        disk: String,
    },

    /// List all available disks
    ListDisks,
}