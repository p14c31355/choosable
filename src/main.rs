mod cli;
mod constants;
mod disk;
mod disk_layout;
mod error;
mod installer;

use clap::Parser;
use cli::{Cli, Commands};
use error::Result;

fn main() -> Result<()> {
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
            let secure_boot = if no_secure_boot { false } else { secure_boot || !no_secure_boot };

            if non_destructive {
                println!("Non-destructive installation is not yet implemented.");
                println!("Please use standard installation mode.");
                return Ok(());
            }

            let fs_type = installer::FilesystemType::from_str(&filesystem)?;

            installer::install_ventoy(
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

            installer::update_ventoy(&disk, secure_boot, yes)?;
        }

        Commands::List { disk } => {
            installer::list_ventoy(&disk)?;
        }

        Commands::ListDisks => {
            installer::list_disks()?;
        }
    }

    Ok(())
}