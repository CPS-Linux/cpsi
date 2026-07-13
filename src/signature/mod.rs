use crate::{repository::validate_repository_name, util::constants};
use cps_common::errors::CpsiError;
use minisign_verify::{PublicKey, Signature};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const PACKAGE_SIGNATURE_SUFFIX: &str = ".minisig";
const PARQUET_SIGNATURE_SUFFIX: &str = ".minisign";
const VERIFY_BUFFER_SIZE: usize = 64 * 1024;

/// Verify a package file using the adjacent `<file>.minisig` signature.
pub fn verify_file(path: &Path, public_key: &str) -> Result<(), CpsiError> {
    let sig_path = append_suffix(path, PACKAGE_SIGNATURE_SUFFIX);
    verify_file_with_sig(path, &sig_path, public_key)
}

/// Verify `path` using an explicitly supplied Minisign signature file.
pub fn verify_file_with_sig(
    path: &Path,
    sig_path: &Path,
    public_key: &str,
) -> Result<(), CpsiError> {
    let public_key = parse_public_key(public_key)?;
    let signature = Signature::from_file(sig_path).map_err(|error| {
        signature_error(
            sig_path,
            format!("unable to read or decode signature: {error}"),
        )
    })?;

    let mut verifier = public_key
        .verify_stream(&signature)
        .map_err(|error| signature_error(sig_path, error.to_string()))?;
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; VERIFY_BUFFER_SIZE];

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        verifier.update(&buffer[..bytes_read]);
    }

    verifier
        .finalize()
        .map_err(|error| signature_error(path, error.to_string()))
}

/// Verify a repository index using `<path>.minisign`.
pub fn verify_packages_parquet(path: &Path, public_key: &str) -> Result<(), CpsiError> {
    let sig_path = append_suffix(path, PARQUET_SIGNATURE_SUFFIX);
    verify_file_with_sig(path, &sig_path, public_key)
}

/// Validate a Base64-encoded Minisign public key.
pub fn validate_public_key(public_key: &str) -> Result<(), CpsiError> {
    parse_public_key(public_key).map(|_| ())
}

/// Save a repository public key under `/etc/cpsi/keys/`.
pub fn save_public_key(repo_name: &str, public_key: &str) -> Result<(), CpsiError> {
    save_public_key_to(
        repo_name,
        public_key,
        Path::new(constants::PUBLIC_KEYS_DIRECTORY),
    )
}

/// Save a repository public key to an explicitly supplied key directory.
///
/// This is public so callers and tests can use an alternate CPSI root without
/// writing to the host's `/etc`.
pub fn save_public_key_to(
    repo_name: &str,
    public_key: &str,
    keys_dir: &Path,
) -> Result<(), CpsiError> {
    validate_repository_name(repo_name)?;
    validate_public_key(public_key)?;
    fs::create_dir_all(keys_dir)?;

    let key_path = public_key_path(keys_dir, repo_name)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&key_path)?;
    file.write_all(public_key.trim().as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;

    Ok(())
}

/// Load a repository public key from `/etc/cpsi/keys/`.
pub fn load_public_key(repo_name: &str) -> Result<String, CpsiError> {
    load_public_key_from(repo_name, Path::new(constants::PUBLIC_KEYS_DIRECTORY))
}

/// Load a repository public key from an explicitly supplied key directory.
pub fn load_public_key_from(repo_name: &str, keys_dir: &Path) -> Result<String, CpsiError> {
    let key_path = public_key_path(keys_dir, repo_name)?;
    let public_key = fs::read_to_string(key_path)?;
    validate_public_key(&public_key)?;
    Ok(public_key.trim().to_string())
}

/// Return the on-disk path used for a repository public key.
pub fn public_key_path(keys_dir: &Path, repo_name: &str) -> Result<PathBuf, CpsiError> {
    validate_repository_name(repo_name)?;
    Ok(keys_dir.join(format!("{repo_name}.pub")))
}

fn parse_public_key(public_key: &str) -> Result<PublicKey, CpsiError> {
    PublicKey::from_base64(public_key.trim()).map_err(|error| {
        CpsiError::SignatureVerificationFailed(format!("invalid public key: {error}"))
    })
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut path_with_suffix = OsString::from(path.as_os_str());
    path_with_suffix.push(suffix);
    PathBuf::from(path_with_suffix)
}

fn signature_error(path: &Path, message: impl AsRef<str>) -> CpsiError {
    CpsiError::SignatureVerificationFailed(format!("{}: {}", path.display(), message.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use minisign::KeyPair;
    use std::{
        io::Cursor,
        sync::atomic::{AtomicU64, Ordering},
    };

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_directory(label: &str) -> PathBuf {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "cpsi-signature-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        path
    }

    fn write_signed_file(path: &Path, signature_path: &Path, data: &[u8]) -> String {
        let KeyPair { pk, sk } = KeyPair::generate_unencrypted_keypair().unwrap();
        let signature = minisign::sign(Some(&pk), &sk, Cursor::new(data), None, None).unwrap();
        fs::write(path, data).unwrap();
        fs::write(signature_path, signature.into_string()).unwrap();
        pk.to_base64()
    }

    #[test]
    fn verifies_file_with_default_minisig_suffix() {
        let dir = temp_directory("package");
        let package_path = dir.join("sample.clos");
        let public_key =
            write_signed_file(&package_path, &dir.join("sample.clos.minisig"), b"test");

        assert!(verify_file(&package_path, &public_key).is_ok());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn verifies_packages_parquet_with_minisign_suffix() {
        let dir = temp_directory("parquet");
        let parquet_path = dir.join("Packages.parquet");
        let public_key = write_signed_file(
            &parquet_path,
            &dir.join("Packages.parquet.minisign"),
            b"test",
        );

        assert!(verify_packages_parquet(&parquet_path, &public_key).is_ok());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_tampered_file() {
        let dir = temp_directory("tampered");
        let package_path = dir.join("sample.clos");
        let signature_path = dir.join("sample.clos.minisig");
        let public_key = write_signed_file(&package_path, &signature_path, b"test");
        fs::write(&package_path, b"Test").unwrap();

        assert!(matches!(
            verify_file_with_sig(&package_path, &signature_path, &public_key),
            Err(CpsiError::SignatureVerificationFailed(_))
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saves_and_loads_public_key_from_custom_directory() {
        let dir = temp_directory("keys");
        let KeyPair { pk, .. } = KeyPair::generate_unencrypted_keypair().unwrap();
        let public_key = pk.to_base64();
        save_public_key_to("core", &public_key, &dir).unwrap();

        assert_eq!(load_public_key_from("core", &dir).unwrap(), public_key);
        assert!(dir.join("core.pub").is_file());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_unsafe_key_name_and_invalid_key() {
        let dir = temp_directory("invalid-key");
        let KeyPair { pk, .. } = KeyPair::generate_unencrypted_keypair().unwrap();
        assert!(save_public_key_to("../outside", &pk.to_base64(), &dir).is_err());
        assert!(matches!(
            save_public_key_to("core", "not-base64", &dir),
            Err(CpsiError::SignatureVerificationFailed(_))
        ));
        fs::remove_dir_all(dir).unwrap();
    }
}
