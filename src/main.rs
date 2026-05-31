mod checks;
mod cli;
mod constants;
mod disk;
mod disk_layout;
mod error;
mod gui;
mod installer;

use clap::Parser;
use cli::{Cli, Commands};
use error::Result;

fn main() -> Result<()> {
    // If no arguments given at all, launch GUI
    let args: Vec<String> = std::env::args().collect();
    if args.len() <= 1 {
        // Ensure XDG_RUNTIME_DIR for Wayland under sudo
        if std::env::var("XDG_RUNTIME_DIR").is_err() {
            let uid = std::env::var("SUDO_UID").ok().or_else(|| {
                std::fs::read_to_string("/proc/self/status")
                    .ok()
                    .and_then(|status| {
                        status
                            .lines()
                            .find(|l| l.starts_with("Uid:"))
                            .and_then(|l| l.split_whitespace().nth(1))
                            .map(String::from)
                    })
            });
            if let Some(uid) = uid {
                unsafe {
                    std::env::set_var("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
                }
            }
        }

        return match gui::run_gui() {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("GUI failed: {e}");
                eprintln!("Use 'choosable install <disk>' for CLI mode.");
                Ok(())
            }
        };
    }

    // Handle --help / --version without requiring a subcommand
    if args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h" | "--version" | "-V") {
        Cli::parse();
        return Ok(());
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp(None)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Install {
            disk,
            force,
            gpt,
            secure_boot,
            no_secure_boot,
            reserve_space,
            label,
            non_destructive,
            filesystem,
            yes,
        } => {
            let secure_boot = secure_boot || !no_secure_boot;
            let fs_type = installer::FilesystemType::from_str(&filesystem)?;

            if non_destructive {
                return installer::non_destructive_install(&disk, &label, fs_type, secure_boot, yes);
            }

            installer::install_choosable(
                &disk, gpt, secure_boot, reserve_space.unwrap_or(0), &label, fs_type, force, yes,
            )?;
        }

        Commands::Update {
            disk,
            secure_boot,
            no_secure_boot,
            yes,
        } => {
            let secure_boot = no_secure_boot
                .then_some(false)
                .or_else(|| secure_boot.then_some(true));

            installer::update_choosable(&disk, secure_boot, yes)?;
        }

        Commands::List { disk } => installer::list_choosable(&disk)?,

        Commands::ListDisks => installer::list_disks()?,
    }

    Ok(())
}