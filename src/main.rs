mod checks;
mod cli;
mod constants;
mod disk;
mod disk_layout;
mod error;
mod installer;
mod gui;

use clap::Parser;
use cli::{Cli, Commands};
use error::Result;

fn main() -> Result<()> {
    // If no arguments given at all, launch GUI
    let args: Vec<String> = std::env::args().collect();
    if args.len() <= 1 {
        return gui::run_gui();
    }

    // Handle --help / --version without requiring a subcommand
    if args.len() == 2 && (args[1] == "--help" || args[1] == "-h" || args[1] == "--version" || args[1] == "-V") {
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
            let secure_boot = !no_secure_boot;

            if non_destructive {
                let fs_type = installer::FilesystemType::from_str(&filesystem)?;
                installer::non_destructive_install(&disk, &label, fs_type, secure_boot, yes)?;
                return Ok(());
            }

            let fs_type = installer::FilesystemType::from_str(&filesystem)?;

            installer::install_choosable(
                &disk,
                gpt,
                secure_boot,
                reserve_space.unwrap_or(0),
                &label,
                fs_type,
                force,
                yes,
            )?;
        }

        Commands::Update {
            disk,
            secure_boot,
            no_secure_boot,
            yes,
        } => {
            let secure_boot = if no_secure_boot {
                Some(false)
            } else if secure_boot {
                Some(true)
            } else {
                None
            };

            installer::update_choosable(&disk, secure_boot, yes)?;
        }

        Commands::List { disk } => {
            installer::list_choosable(&disk)?;
        }

        Commands::ListDisks => {
            installer::list_disks()?;
        }
    }

    Ok(())
}