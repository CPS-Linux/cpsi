use super::install::{InstallContext, candidate_is_newer, install_with_context_allow_upgrade};
use crate::{database::InstalledDatabase, repository::parquet::Repository};
use cps_common::errors::CpsiError;

/// Upgrade installed packages using the system CPSI directories.
///
/// With no names, every installed package that has a newer repository version
/// is selected. With explicit names, only those installed packages are passed
/// to the shared install pipeline.
pub fn upgrade(package_names: &[String]) -> Result<(), CpsiError> {
    upgrade_with_context(package_names, &InstallContext::system())
}

/// Upgrade installed packages using caller-provided filesystem locations.
pub fn upgrade_with_context(
    package_names: &[String],
    context: &InstallContext,
) -> Result<(), CpsiError> {
    let database = InstalledDatabase::load_from(&context.database_dir)?;

    if package_names.is_empty() && database.packages().is_empty() {
        println!("Nothing to upgrade");
        return Ok(());
    }

    let selected = if package_names.is_empty() {
        select_all_outdated(&database, context)?
    } else {
        for name in package_names {
            if database.get_package(name).is_none() {
                return Err(CpsiError::PackageNotFound(name.clone()));
            }
        }
        package_names.to_vec()
    };

    if selected.is_empty() {
        println!("Nothing to upgrade");
        return Ok(());
    }

    install_with_context_allow_upgrade(&selected, context)
}

fn select_all_outdated(
    database: &InstalledDatabase,
    context: &InstallContext,
) -> Result<Vec<String>, CpsiError> {
    let repository = Repository::load_registered_from(
        &context.config_dir,
        &context.keys_dir,
        &context.repositories_dir,
    )?;
    let mut selected = database
        .packages()
        .iter()
        .filter_map(|installed| {
            let candidate = repository.find_package(&installed.name)?;
            candidate_is_newer(installed, candidate).then(|| installed.name.clone())
        })
        .collect::<Vec<_>>();
    selected.sort();
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::InstalledPackage;
    use cps_common::{
        architecture::Architecture, dependency::Dependency, package::Package, version::Version,
    };

    fn package(name: &str, version: &str, release: u32) -> Package {
        Package {
            name: name.to_string(),
            version: Version::from(version),
            release,
            arch: vec![Architecture::X86_64],
            dependencies: Vec::<Dependency>::new(),
            description: String::new(),
            provides: Vec::new(),
            license: String::new(),
            package_size: 0,
            installed_size: 0,
            repository: "test".to_string(),
        }
    }

    fn installed(name: &str, version: &str, release: u32) -> InstalledPackage {
        InstalledPackage {
            name: name.to_string(),
            version: Version::from(version),
            release,
            arch: vec![Architecture::X86_64],
            install_time: 0,
        }
    }

    #[test]
    fn newer_selection_uses_version_then_release() {
        assert!(candidate_is_newer(
            &installed("demo", "1.0.0", 1),
            &package("demo", "1.0.0", 2)
        ));
        assert!(candidate_is_newer(
            &installed("demo", "1.9.9", 20),
            &package("demo", "2.0.0", 1)
        ));
        assert!(!candidate_is_newer(
            &installed("demo", "2.0.0", 1),
            &package("demo", "2.0.0", 1)
        ));
    }
}
