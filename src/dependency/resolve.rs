use std::collections::HashSet;

use crate::repository::parquet::Repository;

use cps_common::{
    dependency::{ComparisonOperator, Dependency},
    errors::CpsiError,
    package::Package,
};

/// Resolve package dependencies in install order.
///
/// Dependencies are emitted before the package that requires them. Packages
/// already added to the result are de-duplicated by package name.
pub fn resolve<'a>(
    targets: &[&'a Package],
    repository: &'a Repository,
) -> Result<Vec<&'a Package>, CpsiError> {
    let mut resolver = Resolver {
        repository,
        visiting: HashSet::new(),
        visited: HashSet::new(),
        resolved: Vec::new(),
    };

    for target in targets {
        resolver.visit(target)?;
    }

    Ok(resolver.resolved)
}

/// Resolve package names against the repository, then resolve their
/// dependencies in install order.
pub fn resolve_names<I, S>(
    package_names: I,
    repository: &Repository,
) -> Result<Vec<&Package>, CpsiError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut targets = Vec::new();

    for name in package_names {
        let name = name.as_ref();
        let package = repository
            .find_package(name)
            .ok_or_else(|| CpsiError::PackageNotFound(name.to_string()))?;
        targets.push(package);
    }

    resolve(&targets, repository)
}

struct Resolver<'a> {
    repository: &'a Repository,
    visiting: HashSet<String>,
    visited: HashSet<String>,
    resolved: Vec<&'a Package>,
}

impl<'a> Resolver<'a> {
    fn visit(&mut self, package: &'a Package) -> Result<(), CpsiError> {
        if self.visited.contains(&package.name) {
            return Ok(());
        }

        if !self.visiting.insert(package.name.clone()) {
            return Err(CpsiError::DependencyCycleDetected);
        }

        for dependency in &package.dependencies {
            let dependency_package = self.resolve_dependency(dependency)?;
            self.visit(dependency_package)?;
        }

        self.visiting.remove(&package.name);
        self.visited.insert(package.name.clone());
        self.resolved.push(package);

        Ok(())
    }

    fn resolve_dependency(&self, dependency: &Dependency) -> Result<&'a Package, CpsiError> {
        if let Some(package) = self.repository.find_package(&dependency.name)
            && crate::repository::parquet::supports_architecture(
                package,
                self.repository.architecture(),
            ) && dependency_is_satisfied(package, dependency)
        {
            return Ok(package);
        }

        let candidates: Vec<_> = self
            .repository
            .packages()
            .filter(|package| {
                crate::repository::parquet::supports_architecture(
                    package,
                    self.repository.architecture(),
                )
            })
            .filter(|package| package.provides.iter().any(|name| name == &dependency.name))
            .filter(|package| dependency_is_satisfied(package, dependency))
            .collect();

        match candidates.as_slice() {
            [package] => Ok(*package),
            [] => Err(CpsiError::UnsatisfiedDependency(format_dependency(
                dependency,
            ))),
            _ => {
                let mut provider_names: Vec<_> = candidates
                    .iter()
                    .map(|package| package.name.as_str())
                    .collect();
                provider_names.sort_unstable();

                Err(CpsiError::AmbiguousProvider(
                    dependency.name.clone(),
                    provider_names.join(", "),
                ))
            }
        }
    }
}

pub fn dependency_is_satisfied(package: &Package, dependency: &Dependency) -> bool {
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

fn format_dependency(dependency: &Dependency) -> String {
    dependency.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cps_common::{architecture::Architecture, version::Version};

    fn package(name: &str, version: &str) -> Package {
        Package {
            name: name.to_string(),
            version: Version::from(version),
            release: 1,
            arch: vec![Architecture::X86_64],
            dependencies: Vec::new(),
            description: String::new(),
            provides: Vec::new(),
            license: String::new(),
            package_size: 0,
            installed_size: 0,
            repository: "test".to_string(),
        }
    }

    #[test]
    fn supports_every_comparison_operator_at_boundaries() {
        let package = package("lib", "2.0.0");

        for (operator, version, expected) in [
            (ComparisonOperator::Eq, "2.0.0", true),
            (ComparisonOperator::Eq, "2.0.1", false),
            (ComparisonOperator::Gt, "1.9.9", true),
            (ComparisonOperator::Gt, "2.0.0", false),
            (ComparisonOperator::Gte, "2.0.0", true),
            (ComparisonOperator::Gte, "2.0.1", false),
            (ComparisonOperator::Lt, "2.0.1", true),
            (ComparisonOperator::Lt, "2.0.0", false),
            (ComparisonOperator::Lte, "2.0.0", true),
            (ComparisonOperator::Lte, "1.9.9", false),
        ] {
            let dependency = Dependency {
                name: "lib".to_string(),
                version: Some(Version::from(version)),
                operator: Some(operator),
            };
            assert_eq!(
                dependency_is_satisfied(&package, &dependency),
                expected,
                "{operator}{version}"
            );
        }
    }

    #[test]
    fn resolves_dependencies_before_targets() {
        let dependency = Dependency {
            name: "lib".to_string(),
            version: Some(Version::from("1.0.0")),
            operator: Some(ComparisonOperator::Gte),
        };
        let lib = package("lib", "1.0.0");
        let mut app = package("app", "1.0.0");
        app.dependencies.push(dependency);
        let repository =
            Repository::from_packages_for_arch(vec![app, lib], Architecture::X86_64).unwrap();

        let resolved = resolve_names(["app"], &repository).unwrap();
        assert_eq!(
            resolved
                .iter()
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            ["lib", "app"]
        );
    }

    #[test]
    fn resolves_single_provider() {
        let mut provider = package("busybox", "1.0.0");
        provider.provides.push("shell".to_string());
        let mut app = package("app", "1.0.0");
        app.dependencies.push(Dependency {
            name: "shell".to_string(),
            version: None,
            operator: None,
        });
        let repository =
            Repository::from_packages_for_arch(vec![app, provider], Architecture::X86_64).unwrap();

        let resolved = resolve_names(["app"], &repository).unwrap();
        assert_eq!(resolved[0].name, "busybox");
    }
}
