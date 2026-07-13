pub mod add;
pub mod parquet;
pub mod sync;

use cps_common::errors::CpsiError;
use std::io;

/// Validate a repository name before using it as a file name.
///
/// Repository names originate in remote `repository.json` files, so accepting
/// path separators or special path components here would allow those files to
/// escape CPSI's configuration and cache directories.
pub fn validate_repository_name(name: &str) -> Result<(), CpsiError> {
    let valid = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));

    if valid {
        Ok(())
    } else {
        Err(CpsiError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid repository name: {name}"),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::validate_repository_name;

    #[test]
    fn validates_safe_repository_names() {
        for name in ["core", "cps-main", "cps.testing_1"] {
            assert!(validate_repository_name(name).is_ok());
        }
    }

    #[test]
    fn rejects_repository_path_traversal() {
        for name in ["", ".", "..", "../outside", "nested/repo", r"nested\repo"] {
            assert!(validate_repository_name(name).is_err());
        }
    }
}
