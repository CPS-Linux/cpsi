use clap::{Parser, Subcommand};
use cps_common::errors::CpsiError;
use cpsi::cli;
use std::process::ExitCode;

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
        #[arg(required = true, num_args = 1..)]
        packages: Vec<String>,
    },
    /// Remove installed packages.
    Remove {
        /// Packages to remove.
        #[arg(required = true, num_args = 1..)]
        packages: Vec<String>,
        /// Also remove packages that become orphaned.
        #[arg(long)]
        auto_remove: bool,
        /// Skip confirmation prompts.
        #[arg(short = 'y', long = "noconfirm")]
        noconfirm: bool,
    },
    /// Upgrade installed packages, or only the named packages when provided.
    Upgrade {
        /// Packages to upgrade. If omitted, upgrade every installed package.
        packages: Vec<String>,
    },
    /// Show package metadata.
    Info {
        /// Package to inspect.
        package: String,
    },
    /// Update package repositories.
    Update {
        /// Repository prefix to update.
        prefix: Option<String>,
    },
    Repo {
        #[command(subcommand)]
        repo: RepoSubcommand,
    },
}

#[derive(Subcommand, Debug)]
enum RepoSubcommand {
    /// Add a package repository.
    Add {
        url: String,
        /// Skip repository signature verification. Intended for development.
        #[arg(long)]
        insecure: bool,
    },
    /// List configured repositories.
    List,
    /// Remove a configured repository.
    Remove { name: String },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match dispatch(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(command_line: Cli) -> Result<(), CpsiError> {
    match command_line.subcommand {
        SubCommands::Install { packages } => {
            run_blocking(move || cli::install::install(&packages)).await
        }
        SubCommands::Remove {
            packages,
            auto_remove,
            noconfirm,
        } => run_blocking(move || cli::remove::remove(&packages, auto_remove, noconfirm)).await,
        SubCommands::Upgrade { packages } => {
            run_blocking(move || cli::upgrade::upgrade(&packages)).await
        }
        SubCommands::Info { package } => run_blocking(move || cli::info::info(&package)).await,
        SubCommands::Update { prefix } => cli::update::update_with_prefix(prefix.as_deref()).await,
        SubCommands::Repo { repo } => match repo {
            RepoSubcommand::Add { url, insecure } => {
                run_blocking(move || cli::repo::add_repository_with_options(url, insecure)).await
            }
            RepoSubcommand::List => run_blocking(cli::repo::list_repositories).await,
            RepoSubcommand::Remove { name } => {
                run_blocking(move || cli::repo::remove_repository(&name)).await
            }
        },
    }
}

async fn run_blocking<F>(operation: F) -> Result<(), CpsiError>
where
    F: FnOnce() -> Result<(), CpsiError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| {
            CpsiError::Io(std::io::Error::other(format!(
                "command worker failed: {error}"
            )))
        })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn parses_remove_packages_and_flags() {
        let cli = Cli::try_parse_from(["cpsi", "remove", "alpha", "beta", "--auto-remove", "-y"])
            .unwrap();

        assert!(matches!(
            cli.subcommand,
            SubCommands::Remove {
                packages,
                auto_remove: true,
                noconfirm: true,
            } if packages == ["alpha", "beta"]
        ));
    }

    #[test]
    fn remove_requires_at_least_one_package() {
        let error = Cli::try_parse_from(["cpsi", "remove", "--auto-remove"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);

        let long_flag = Cli::try_parse_from(["cpsi", "remove", "alpha", "--noconfirm"]).unwrap();
        assert!(matches!(
            long_flag.subcommand,
            SubCommands::Remove {
                packages,
                auto_remove: false,
                noconfirm: true,
            } if packages == ["alpha"]
        ));
    }

    #[test]
    fn parses_upgrade_with_optional_package_selection() {
        let all = Cli::try_parse_from(["cpsi", "upgrade"]).unwrap();
        assert!(matches!(
            all.subcommand,
            SubCommands::Upgrade { packages } if packages.is_empty()
        ));

        let selected = Cli::try_parse_from(["cpsi", "upgrade", "alpha", "beta"]).unwrap();
        assert!(matches!(
            selected.subcommand,
            SubCommands::Upgrade { packages } if packages == ["alpha", "beta"]
        ));
    }

    #[test]
    fn parses_info_command() {
        let cli = Cli::try_parse_from(["cpsi", "info", "alpha"]).unwrap();
        assert!(matches!(
            cli.subcommand,
            SubCommands::Info { package } if package == "alpha"
        ));
    }

    #[test]
    fn parses_existing_install_and_update_commands() {
        let install = Cli::try_parse_from(["cpsi", "install", "alpha", "beta"]).unwrap();
        assert!(matches!(
            install.subcommand,
            SubCommands::Install { packages } if packages == ["alpha", "beta"]
        ));

        let update = Cli::try_parse_from(["cpsi", "update", "core"]).unwrap();
        assert!(matches!(
            update.subcommand,
            SubCommands::Update { prefix } if prefix.as_deref() == Some("core")
        ));

        let update_all = Cli::try_parse_from(["cpsi", "update"]).unwrap();
        assert!(matches!(
            update_all.subcommand,
            SubCommands::Update { prefix: None }
        ));

        let error = Cli::try_parse_from(["cpsi", "update", "core", "extra"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn install_requires_at_least_one_package() {
        let error = Cli::try_parse_from(["cpsi", "install"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn parses_repository_subcommands_and_insecure_flag() {
        let add = Cli::try_parse_from([
            "cpsi",
            "repo",
            "add",
            "https://example.test/repository",
            "--insecure",
        ])
        .unwrap();
        assert!(matches!(
            add.subcommand,
            SubCommands::Repo {
                repo: RepoSubcommand::Add { url, insecure: true }
            } if url == "https://example.test/repository"
        ));

        let trusted =
            Cli::try_parse_from(["cpsi", "repo", "add", "https://example.test/repository"])
                .unwrap();
        assert!(matches!(
            trusted.subcommand,
            SubCommands::Repo {
                repo: RepoSubcommand::Add {
                    insecure: false,
                    ..
                }
            }
        ));

        let list = Cli::try_parse_from(["cpsi", "repo", "list"]).unwrap();
        assert!(matches!(
            list.subcommand,
            SubCommands::Repo {
                repo: RepoSubcommand::List
            }
        ));

        let remove = Cli::try_parse_from(["cpsi", "repo", "remove", "core"]).unwrap();
        assert!(matches!(
            remove.subcommand,
            SubCommands::Repo {
                repo: RepoSubcommand::Remove { name }
            } if name == "core"
        ));
    }
}
