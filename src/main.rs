use cps_common::errors::CpsiError::PackageNotFound;
mod cli;
mod database;
mod dependency;
mod package;
mod repository;
mod signature;
mod util;

use clap::Parser;
use clap::Subcommand;

use crate::SubCommands::Install;
use crate::SubCommands::Update;

#[derive(Parser, Debug)]
struct Cli {
    #[command(subcommand)]
    subcommand: SubCommands,
}

#[derive(Subcommand, Debug)]
enum SubCommands {
    /// Install packages.
    Install {
        /// Packages to install.
        packages: Vec<String>,
    },
    /// Update package repositories.
    Update {
        /// Repository prefix to update.
        prefix: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.subcommand {
        Install { packages } => {
            if let Err(e) = cli::install::install(&packages) {
                if let PackageNotFound(pkg) = e {
                    eprintln!("{}: Package Not Found", pkg);
                }
            }
        }
        Update { prefix } => {
            _ = prefix;
            if let Err(e) = cli::update::update().await {
                eprintln!("{}", e.to_string());
            }
        }
    }
}
