use crate::{
    database::{InstalledDatabase, InstalledPackage},
    repository::parquet::Repository,
    util::constants,
};
use cps_common::{
    dependency::{ComparisonOperator, Dependency},
    errors::CpsiError,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct RemoveContext {
    pub database_dir: PathBuf,
    pub root: PathBuf,
    pub repositories_dir: PathBuf,
    pub repositories_config_dir: PathBuf,
    pub keys_dir: PathBuf,
    verify_repository_signatures: bool,
}

impl RemoveContext {
    pub fn new(
        database_dir: impl Into<PathBuf>,
        root: impl Into<PathBuf>,
        repositories_dir: impl Into<PathBuf>,
        repositories_config_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            database_dir: database_dir.into(),
            root: root.into(),
            repositories_dir: repositories_dir.into(),
            repositories_config_dir: repositories_config_dir.into(),
            keys_dir: PathBuf::new(),
            verify_repository_signatures: false,
        }
    }

    pub fn with_verified_repositories(
        database_dir: impl Into<PathBuf>,
        root: impl Into<PathBuf>,
        repositories_dir: impl Into<PathBuf>,
        repositories_config_dir: impl Into<PathBuf>,
        keys_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            database_dir: database_dir.into(),
            root: root.into(),
            repositories_dir: repositories_dir.into(),
            repositories_config_dir: repositories_config_dir.into(),
            keys_dir: keys_dir.into(),
            verify_repository_signatures: true,
        }
    }
}

impl Default for RemoveContext {
    fn default() -> Self {
        Self {
            database_dir: PathBuf::from(constants::INSTALLED_DATABASE_DIRECTORY),
            root: PathBuf::from("/"),
            repositories_dir: PathBuf::from(constants::REPOSITORIES_DIRECTORY),
            repositories_config_dir: PathBuf::from(constants::REPOSITORIES_CONFIG_DIRECTORY),
            keys_dir: PathBuf::from(constants::PUBLIC_KEYS_DIRECTORY),
            verify_repository_signatures: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoveOutcome {
    pub removed_packages: Vec<String>,
    pub auto_removed_packages: Vec<String>,
    pub removed_files: Vec<PathBuf>,
}

/// Remove installed packages using CPSI's system paths.
pub fn remove(
    package_names: &[String],
    auto_remove: bool,
    noconfirm: bool,
) -> Result<(), CpsiError> {
    let outcome = remove_with_context(
        package_names,
        auto_remove,
        noconfirm,
        &RemoveContext::default(),
    )?;

    for package in &outcome.removed_packages {
        println!("removed {package}");
    }
    if !outcome.auto_removed_packages.is_empty() {
        println!("auto-removed: {}", outcome.auto_removed_packages.join(", "));
    }
    Ok(())
}

/// Remove packages using caller-provided database, root, and repository paths.
pub fn remove_with_context(
    package_names: &[String],
    auto_remove: bool,
    noconfirm: bool,
    context: &RemoveContext,
) -> Result<RemoveOutcome, CpsiError> {
    let mut database = InstalledDatabase::load_from(&context.database_dir)?;
    let repository = load_optional_repository(context)?;
    remove_from_sources(
        package_names,
        auto_remove,
        noconfirm,
        &mut database,
        repository.as_ref(),
        &context.root,
    )
}

/// Core removal operation for callers that already loaded package metadata.
pub fn remove_from_sources(
    package_names: &[String],
    auto_remove: bool,
    noconfirm: bool,
    database: &mut InstalledDatabase,
    repository: Option<&Repository>,
    root: &Path,
) -> Result<RemoveOutcome, CpsiError> {
    if package_names.is_empty() {
        return Err(CpsiError::PackageNotInstalled(String::new()));
    }

    let explicitly_removed = package_names.iter().cloned().collect::<BTreeSet<_>>();
    for package_name in &explicitly_removed {
        if database.get_package(package_name).is_none() {
            return Err(CpsiError::PackageNotInstalled(package_name.clone()));
        }
    }

    let installed = database
        .packages()
        .iter()
        .map(|package| (package.name.clone(), package))
        .collect::<BTreeMap<_, _>>();
    let dependency_closure = if auto_remove {
        dependency_closure(&explicitly_removed, &installed, repository)
    } else {
        BTreeSet::new()
    };

    let mut auto_removed = if auto_remove {
        auto_removal_candidates(
            &explicitly_removed,
            &dependency_closure,
            &installed,
            repository,
        )
    } else {
        BTreeSet::new()
    };
    let mut removal_plan = explicitly_removed.clone();
    removal_plan.extend(auto_removed.iter().cloned());

    // Auto-removal must never break a package that remains installed. Retain
    // any automatic candidate implicated by an unresolved dependency and
    // repeat until the plan is stable.
    loop {
        let broken = broken_dependencies(&removal_plan, &installed, repository);
        let implicated_auto = broken
            .iter()
            .flat_map(|dependency| dependency.removed_providers.iter())
            .filter(|provider| auto_removed.contains(*provider))
            .cloned()
            .collect::<BTreeSet<_>>();
        if implicated_auto.is_empty() {
            break;
        }
        for package in implicated_auto {
            auto_removed.remove(&package);
            removal_plan.remove(&package);
        }
    }

    let broken = broken_dependencies(&removal_plan, &installed, repository);
    if !broken.is_empty() {
        for dependency in &broken {
            eprintln!(
                "warning: removing {} leaves '{}' without dependency {}",
                dependency.removed_providers.join(", "),
                dependency.dependent,
                dependency.dependency
            );
        }
        if !noconfirm {
            let required_packages = broken
                .iter()
                .flat_map(|dependency| dependency.removed_providers.iter().cloned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ");
            let dependents = broken
                .iter()
                .map(|dependency| dependency.dependent.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ");
            return Err(CpsiError::PackageRequired(required_packages, dependents));
        }
    }

    let removed_files = remove_owned_files(database, &removal_plan, root)?;
    for package_name in &removal_plan {
        database.remove_package(package_name)?;
    }
    database.save()?;

    Ok(RemoveOutcome {
        removed_packages: removal_plan.into_iter().collect(),
        auto_removed_packages: auto_removed.into_iter().collect(),
        removed_files,
    })
}

#[derive(Debug)]
struct BrokenDependency {
    dependent: String,
    dependency: Dependency,
    removed_providers: Vec<String>,
}

fn dependency_closure(
    roots: &BTreeSet<String>,
    installed: &BTreeMap<String, &InstalledPackage>,
    repository: Option<&Repository>,
) -> BTreeSet<String> {
    let mut closure = BTreeSet::new();
    let mut queue = roots.iter().cloned().collect::<VecDeque<_>>();

    while let Some(package_name) = queue.pop_front() {
        let Some(metadata) = repository.and_then(|repo| repo.find_package(&package_name)) else {
            continue;
        };

        for dependency in &metadata.dependencies {
            let providers =
                installed_providers(dependency, installed, repository, &BTreeSet::new());
            let selected = if providers
                .iter()
                .any(|provider| provider == &dependency.name)
            {
                vec![dependency.name.clone()]
            } else if providers.len() == 1 {
                providers
            } else {
                // Multiple virtual providers are ambiguous. Keeping all of
                // them is safer than guessing which one was explicitly chosen.
                Vec::new()
            };

            for provider in selected {
                if !roots.contains(&provider) && closure.insert(provider.clone()) {
                    queue.push_back(provider);
                }
            }
        }
    }

    closure
}

fn auto_removal_candidates(
    explicitly_removed: &BTreeSet<String>,
    closure: &BTreeSet<String>,
    installed: &BTreeMap<String, &InstalledPackage>,
    repository: Option<&Repository>,
) -> BTreeSet<String> {
    if closure.is_empty() {
        return BTreeSet::new();
    }

    // Packages outside the dependency closure are remaining roots. Retain any
    // closure package needed to satisfy their dependency graph. When multiple
    // virtual providers are possible, retain all candidates conservatively.
    let mut retained = installed
        .keys()
        .filter(|name| !explicitly_removed.contains(*name) && !closure.contains(*name))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut queue = retained.iter().cloned().collect::<VecDeque<_>>();

    while let Some(package_name) = queue.pop_front() {
        let Some(metadata) = repository.and_then(|repo| repo.find_package(&package_name)) else {
            continue;
        };

        for dependency in &metadata.dependencies {
            let excluded_from_retained = installed
                .keys()
                .filter(|name| !retained.contains(*name))
                .cloned()
                .collect::<BTreeSet<_>>();
            if !installed_providers(dependency, installed, repository, &excluded_from_retained)
                .is_empty()
            {
                continue;
            }

            let excluded = installed
                .keys()
                .filter(|name| {
                    explicitly_removed.contains(*name)
                        || retained.contains(*name)
                        || !closure.contains(*name)
                })
                .cloned()
                .collect::<BTreeSet<_>>();
            let candidates = installed_providers(dependency, installed, repository, &excluded)
                .into_iter()
                .filter(|provider| closure.contains(provider))
                .collect::<Vec<_>>();
            let preferred = if candidates
                .iter()
                .any(|provider| provider == &dependency.name)
            {
                vec![dependency.name.clone()]
            } else {
                candidates
            };

            for provider in preferred {
                if retained.insert(provider.clone()) {
                    queue.push_back(provider);
                }
            }
        }
    }

    closure.difference(&retained).cloned().collect()
}

fn broken_dependencies(
    removal_plan: &BTreeSet<String>,
    installed: &BTreeMap<String, &InstalledPackage>,
    repository: Option<&Repository>,
) -> Vec<BrokenDependency> {
    let mut broken = Vec::new();
    let no_exclusions = BTreeSet::new();

    for package_name in installed
        .keys()
        .filter(|name| !removal_plan.contains(*name))
    {
        let Some(metadata) = repository.and_then(|repo| repo.find_package(package_name)) else {
            continue;
        };

        for dependency in &metadata.dependencies {
            let before = installed_providers(dependency, installed, repository, &no_exclusions);
            if before.is_empty() {
                // Do not attribute a dependency that was already unsatisfied to
                // the requested removal.
                continue;
            }
            let after = installed_providers(dependency, installed, repository, removal_plan);
            if after.is_empty() {
                broken.push(BrokenDependency {
                    dependent: package_name.clone(),
                    dependency: dependency.clone(),
                    removed_providers: before
                        .into_iter()
                        .filter(|provider| removal_plan.contains(provider))
                        .collect(),
                });
            }
        }
    }

    broken
}

fn installed_providers(
    dependency: &Dependency,
    installed: &BTreeMap<String, &InstalledPackage>,
    repository: Option<&Repository>,
    excluded: &BTreeSet<String>,
) -> Vec<String> {
    let mut providers = BTreeSet::new();

    for (name, installed_package) in installed {
        if excluded.contains(name) || !installed_version_satisfies(installed_package, dependency) {
            continue;
        }

        let directly_matches = name == &dependency.name;
        let provides = repository
            .and_then(|repo| repo.find_package(name))
            .is_some_and(|package| {
                package
                    .provides
                    .iter()
                    .any(|provided| provided == &dependency.name)
            });
        if directly_matches || provides {
            providers.insert(name.clone());
        }
    }

    providers.into_iter().collect()
}

fn installed_version_satisfies(package: &InstalledPackage, dependency: &Dependency) -> bool {
    let Some(required) = dependency.version.as_ref() else {
        return dependency.operator.is_none();
    };

    match dependency.operator.unwrap_or(ComparisonOperator::Gte) {
        ComparisonOperator::Eq => &package.version == required,
        ComparisonOperator::Gt => &package.version > required,
        ComparisonOperator::Gte => &package.version >= required,
        ComparisonOperator::Lt => &package.version < required,
        ComparisonOperator::Lte => &package.version <= required,
    }
}

fn remove_owned_files(
    database: &InstalledDatabase,
    removal_plan: &BTreeSet<String>,
    root: &Path,
) -> Result<Vec<PathBuf>, CpsiError> {
    let canonical_root = fs::canonicalize(root).map_err(|error| {
        CpsiError::Database(format!(
            "unable to resolve removal root {}: {error}",
            root.display()
        ))
    })?;
    let mut owned_paths = removal_plan
        .iter()
        .flat_map(|package| database.files_for_package(package))
        .collect::<Vec<_>>();
    owned_paths.sort_by(|left, right| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| left.cmp(right))
    });
    owned_paths.dedup();

    let mut removed_files = Vec::new();
    let mut parent_directories = BTreeSet::new();
    for owned_path in owned_paths {
        let destination = destination_under_root(root, &owned_path)?;
        let Some(parent) = destination.parent().map(Path::to_path_buf) else {
            return Err(CpsiError::Database(format!(
                "owned path has no parent: {}",
                owned_path.display()
            )));
        };

        match fs::canonicalize(&parent) {
            Ok(canonical_parent) if canonical_parent.starts_with(&canonical_root) => {}
            Ok(canonical_parent) => {
                return Err(CpsiError::Database(format!(
                    "owned path escapes removal root through a symbolic link: {} -> {}",
                    destination.display(),
                    canonical_parent.display()
                )));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(CpsiError::Io(error)),
        }

        match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir(&destination)?,
            Ok(_) => fs::remove_file(&destination)?,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(CpsiError::Io(error)),
        }
        removed_files.push(destination);
        parent_directories.insert(parent);
    }

    let mut parent_directories = parent_directories.into_iter().collect::<Vec<_>>();
    parent_directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in parent_directories {
        prune_empty_parents(&directory, root)?;
    }

    Ok(removed_files)
}

fn destination_under_root(root: &Path, owned_path: &Path) -> Result<PathBuf, CpsiError> {
    if !owned_path.is_absolute() {
        return Err(CpsiError::Database(format!(
            "owned path is not absolute: {}",
            owned_path.display()
        )));
    }

    let mut relative = PathBuf::new();
    for component in owned_path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(component) => relative.push(component),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(CpsiError::Database(format!(
                    "owned path escapes removal root: {}",
                    owned_path.display()
                )));
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(CpsiError::Database(
            "the removal root cannot be owned by a package".to_string(),
        ));
    }

    // Production manifests use logical paths such as `/usr/bin/tool`. Some
    // path-injected install flows historically stored the already-prefixed
    // destination (`/tmp/root/usr/bin/tool`) instead. Accept both without
    // joining the test root twice.
    if owned_path.starts_with(root) {
        if owned_path == root {
            return Err(CpsiError::Database(
                "the removal root cannot be owned by a package".to_string(),
            ));
        }
        Ok(owned_path.to_path_buf())
    } else {
        Ok(root.join(relative))
    }
}

fn prune_empty_parents(start: &Path, root: &Path) -> Result<(), CpsiError> {
    let mut current = start.to_path_buf();
    while current != root && current.starts_with(root) {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => break,
            Ok(metadata) if !metadata.file_type().is_dir() => break,
            Ok(_) => match fs::remove_dir(&current) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
                    ) =>
                {
                    break;
                }
                Err(error) => return Err(CpsiError::Io(error)),
            },
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(CpsiError::Io(error)),
        }

        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }
    Ok(())
}

fn load_optional_repository(context: &RemoveContext) -> Result<Option<Repository>, CpsiError> {
    let result = if context.verify_repository_signatures {
        Repository::load_registered_from(
            &context.repositories_config_dir,
            &context.keys_dir,
            &context.repositories_dir,
        )
    } else {
        Repository::load_from(&context.repositories_dir)
    };

    match result {
        Ok(repository) => Ok(Some(repository)),
        Err(CpsiError::NoRepositories) => Ok(None),
        Err(CpsiError::Io(error)) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cps_common::{architecture::Architecture, package::Package, version::Version};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join("opencode").join(format!(
                "cpsi-remove-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn package(name: &str, dependencies: &[&str], provides: &[&str]) -> Package {
        Package {
            name: name.to_string(),
            version: Version::from("1.0.0"),
            release: 1,
            arch: vec![Architecture::host().unwrap()],
            dependencies: dependencies
                .iter()
                .map(|dependency| dependency.parse().unwrap())
                .collect(),
            description: String::new(),
            provides: provides.iter().map(|value| value.to_string()).collect(),
            license: String::new(),
            package_size: 0,
            installed_size: 0,
            repository: "test".to_string(),
        }
    }

    fn database_with_packages(directory: &Path, packages: &[Package]) -> InstalledDatabase {
        let mut database = InstalledDatabase::load_from(directory).unwrap();
        for package in packages {
            database.add_package(package).unwrap();
        }
        database.save().unwrap();
        database
    }

    #[test]
    fn removes_owned_files_and_empty_parents_without_removing_root() {
        let temp = TestDirectory::new("files");
        let database_dir = temp.0.join("database");
        let root = temp.0.join("root");
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::create_dir_all(root.join("etc/demo")).unwrap();
        fs::write(root.join("usr/bin/demo"), b"demo").unwrap();
        fs::write(root.join("etc/demo/config"), b"config").unwrap();
        let demo = package("demo", &[], &[]);
        let mut database = database_with_packages(&database_dir, &[demo]);
        database
            .add_files(
                "demo",
                &[PathBuf::from("/usr/bin/demo"), root.join("etc/demo/config")],
            )
            .unwrap();
        database.save().unwrap();

        let outcome = remove_from_sources(
            &["demo".to_string()],
            false,
            false,
            &mut database,
            None,
            &root,
        )
        .unwrap();

        assert_eq!(outcome.removed_packages, ["demo"]);
        assert!(!root.join("usr/bin/demo").exists());
        assert!(!root.join("usr/bin").exists());
        assert!(!root.join("etc/demo/config").exists());
        assert!(root.is_dir());
        assert!(
            InstalledDatabase::load_from(&database_dir)
                .unwrap()
                .packages()
                .is_empty()
        );
    }

    #[test]
    fn rejects_uninstalled_package_before_filesystem_changes() {
        let temp = TestDirectory::new("missing");
        let root = temp.0.join("root");
        fs::create_dir_all(&root).unwrap();
        let mut database = InstalledDatabase::load_from(temp.0.join("database")).unwrap();

        assert!(matches!(
            remove_from_sources(
                &["missing".to_string()],
                false,
                false,
                &mut database,
                None,
                &root
            ),
            Err(CpsiError::PackageNotInstalled(name)) if name == "missing"
        ));
    }

    #[test]
    fn provides_are_considered_for_reverse_dependencies() {
        let temp = TestDirectory::new("provides");
        let root = temp.0.join("root");
        fs::create_dir_all(&root).unwrap();
        let provider = package("libssl", &[], &["ssl"]);
        let consumer = package("browser", &["ssl"], &[]);
        let repository =
            Repository::from_packages(vec![provider.clone(), consumer.clone()]).unwrap();
        let mut database = database_with_packages(&temp.0.join("database"), &[provider, consumer]);

        assert!(matches!(
            remove_from_sources(
                &["libssl".to_string()],
                false,
                false,
                &mut database,
                Some(&repository),
                &root
            ),
            Err(CpsiError::PackageRequired(_, dependents)) if dependents == "browser"
        ));
        assert!(database.get_package("libssl").is_some());

        remove_from_sources(
            &["libssl".to_string()],
            false,
            true,
            &mut database,
            Some(&repository),
            &root,
        )
        .unwrap();
        assert!(database.get_package("libssl").is_none());
    }

    #[test]
    fn auto_remove_removes_only_unneeded_dependency_closure() {
        let temp = TestDirectory::new("auto");
        let root = temp.0.join("root");
        fs::create_dir_all(&root).unwrap();
        let app = package("app", &["middle"], &[]);
        let middle = package("middle", &["leaf"], &[]);
        let leaf = package("leaf", &[], &[]);
        let unrelated = package("unrelated", &[], &[]);
        let packages = vec![app, middle, leaf, unrelated];
        let repository = Repository::from_packages(packages.clone()).unwrap();
        let mut database = database_with_packages(&temp.0.join("database"), &packages);

        let outcome = remove_from_sources(
            &["app".to_string()],
            true,
            false,
            &mut database,
            Some(&repository),
            &root,
        )
        .unwrap();

        assert_eq!(outcome.removed_packages, ["app", "leaf", "middle"]);
        assert_eq!(outcome.auto_removed_packages, ["leaf", "middle"]);
        assert!(database.get_package("unrelated").is_some());
    }

    #[test]
    fn auto_remove_retains_dependency_used_by_remaining_package() {
        let temp = TestDirectory::new("shared");
        let root = temp.0.join("root");
        fs::create_dir_all(&root).unwrap();
        let app = package("app", &["shared"], &[]);
        let shared = package("shared", &[], &[]);
        let other = package("other", &["shared"], &[]);
        let packages = vec![app, shared, other];
        let repository = Repository::from_packages(packages.clone()).unwrap();
        let mut database = database_with_packages(&temp.0.join("database"), &packages);

        let outcome = remove_from_sources(
            &["app".to_string()],
            true,
            false,
            &mut database,
            Some(&repository),
            &root,
        )
        .unwrap();

        assert_eq!(outcome.removed_packages, ["app"]);
        assert!(database.get_package("shared").is_some());
        assert!(database.get_package("other").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_parent_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temp = TestDirectory::new("symlink-escape");
        let root = temp.0.join("root");
        let outside = temp.0.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(outside.join("bin")).unwrap();
        fs::write(outside.join("bin/demo"), b"outside").unwrap();
        symlink(&outside, root.join("usr")).unwrap();
        let demo = package("demo", &[], &[]);
        let mut database = database_with_packages(&temp.0.join("database"), &[demo]);
        database
            .add_files("demo", &[PathBuf::from("/usr/bin/demo")])
            .unwrap();

        assert!(matches!(
            remove_from_sources(
                &["demo".to_string()],
                false,
                false,
                &mut database,
                None,
                &root
            ),
            Err(CpsiError::Database(_))
        ));
        assert!(outside.join("bin/demo").is_file());
        assert!(database.get_package("demo").is_some());
    }
}
