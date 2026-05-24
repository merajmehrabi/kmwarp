//! Self-signed cert generation + on-disk persistence for M9.
//!
//! Each device generates its own self-signed cert at first launch
//! (CN=`kmwarp`, ECDSA P-256, 10-year validity). The cert + matching
//! PKCS#8 private key live at `~/.config/kmwarp/{cert,key}.der`; the
//! key file is `0600` on Unix.

use std::io::Write;
use std::path::{Path, PathBuf};

use time::{Duration, OffsetDateTime};

use crate::error::TlsError;

/// X.509 cert DER + matching PKCS#8 private key DER.
#[derive(Clone)]
pub struct CertBundle {
    pub cert_der: Vec<u8>,
    pub private_key_der: Vec<u8>,
}

impl std::fmt::Debug for CertBundle {
    /// Elides the private key bytes from Debug output.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertBundle")
            .field("cert_der_len", &self.cert_der.len())
            .field("private_key_der_len", &self.private_key_der.len())
            .finish_non_exhaustive()
    }
}

/// Generate a fresh self-signed cert + key pair valid for 10 years.
pub fn generate_self_signed() -> Result<CertBundle, TlsError> {
    let mut params = rcgen::CertificateParams::new(vec!["kmwarp".to_string()])?;
    params.not_before = OffsetDateTime::now_utc();
    params.not_after = OffsetDateTime::now_utc() + Duration::days(3650);
    let key_pair = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;
    Ok(CertBundle {
        cert_der: cert.der().to_vec(),
        private_key_der: key_pair.serialize_der(),
    })
}

/// Load a previously generated bundle from disk.
pub fn load_from_disk(cert_path: &Path, key_path: &Path) -> Result<CertBundle, TlsError> {
    let cert_der = std::fs::read(cert_path)?;
    let private_key_der = std::fs::read(key_path)?;
    if cert_der.is_empty() {
        return Err(TlsError::BadCertFormat("cert file is empty".to_string()));
    }
    if private_key_der.is_empty() {
        return Err(TlsError::BadCertFormat("key file is empty".to_string()));
    }
    Ok(CertBundle {
        cert_der,
        private_key_der,
    })
}

/// Persist a bundle to disk. Creates parent dirs; `0600` perms on
/// the key file (Unix).
pub fn save_to_disk(
    bundle: &CertBundle,
    cert_path: &Path,
    key_path: &Path,
) -> Result<(), TlsError> {
    if let Some(parent) = cert_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = key_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(cert_path, &bundle.cert_der)?;
    write_key_locked_down(key_path, &bundle.private_key_der)?;
    Ok(())
}

/// Default cert/key paths inside a caller-provided config directory.
pub fn default_paths_in(config_dir: &Path) -> (PathBuf, PathBuf) {
    (config_dir.join("cert.der"), config_dir.join("key.der"))
}

#[cfg(unix)]
fn write_key_locked_down(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_key_locked_down(path: &Path, data: &[u8]) -> std::io::Result<()> {
    // Windows: key file inherits the parent dir's ACL. v1
    // ~/.config/kmwarp is per-user under %APPDATA%; adequate for M9.
    // M10 packaging can revisit explicit ACL hardening.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn generate_self_signed_produces_nonempty_der() {
        let b = generate_self_signed().expect("generate");
        assert!(!b.cert_der.is_empty());
        assert!(!b.private_key_der.is_empty());
        assert_eq!(b.cert_der[0], 0x30); // SEQUENCE
        assert_eq!(b.private_key_der[0], 0x30);
    }

    #[test]
    fn cert_generation_produces_loadable_pkcs8() {
        let dir = tempdir().expect("tempdir");
        let cert_path = dir.path().join("cert.der");
        let key_path = dir.path().join("key.der");
        let b = generate_self_signed().expect("generate");
        save_to_disk(&b, &cert_path, &key_path).expect("save");
        let loaded = load_from_disk(&cert_path, &key_path).expect("load");
        assert_eq!(loaded.cert_der, b.cert_der);
        assert_eq!(loaded.private_key_der, b.private_key_der);
    }

    #[test]
    fn load_propagates_empty_cert_error() {
        let dir = tempdir().expect("tempdir");
        let cert_path = dir.path().join("cert.der");
        let key_path = dir.path().join("key.der");
        std::fs::write(&cert_path, b"").expect("write empty");
        std::fs::write(&key_path, b"x").expect("write key");
        assert!(matches!(
            load_from_disk(&cert_path, &key_path),
            Err(TlsError::BadCertFormat(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().expect("tempdir");
        let cert_path = dir.path().join("cert.der");
        let key_path = dir.path().join("key.der");
        let b = generate_self_signed().expect("generate");
        save_to_disk(&b, &cert_path, &key_path).expect("save");
        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
