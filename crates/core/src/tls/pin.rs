//! SHA-256 cert pin storage.
//!
//! After a successful M9 pair, each side computes `SHA-256(peer_cert_der)`
//! and stores the result hex-encoded in `peer.pin`. Subsequent connects
//! recompute the hash from the offered cert and compare; any mismatch
//! refuses the connection.

use std::path::{Path, PathBuf};

use sha2::Digest;

use crate::error::TlsError;

/// SHA-256 of a DER-encoded X.509 cert. Stored as the pin.
pub type PinHash = [u8; 32];

/// Length of a pin in bytes (SHA-256 → 32 bytes, 64 hex chars).
pub const PIN_HASH_LEN: usize = 32;

/// Compute the canonical pin value for a DER-encoded X.509 cert.
pub fn pin_hash_of(cert_der: &[u8]) -> PinHash {
    sha2::Sha256::digest(cert_der).into()
}

/// On-disk pin storage at a caller-chosen path.
#[derive(Debug, Clone)]
pub struct PinStore {
    path: PathBuf,
}

impl PinStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the stored pin. `Ok(None)` on missing file (first
    /// connect); `Err(TlsError::PinFileCorrupt)` if malformed.
    pub fn load(&self) -> Result<Option<PinHash>, TlsError> {
        let s = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(TlsError::Io(e)),
        };
        let trimmed = s.trim();
        let bytes = hex::decode(trimmed)
            .map_err(|_| TlsError::PinFileCorrupt("not valid hex".to_string()))?;
        let arr: PinHash = bytes.try_into().map_err(|v: Vec<u8>| {
            TlsError::PinFileCorrupt(format!("decoded to {} bytes, expected 32", v.len()))
        })?;
        Ok(Some(arr))
    }

    /// Atomically write the pin: tmpfile + rename. Unix `0600` perms
    /// on the tmpfile so even momentarily it's user-only.
    pub fn store(&self, pin: &PinHash) -> Result<(), TlsError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("pin.tmp");
        let mut contents = hex::encode(pin);
        contents.push('\n');
        write_locked_down(&tmp, contents.as_bytes())?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Delete the pin file. Missing file is a success.
    pub fn forget(&self) -> Result<(), TlsError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(TlsError::Io(e)),
        }
    }
}

#[cfg(unix)]
fn write_locked_down(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
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
fn write_locked_down(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
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
    fn pin_hash_is_stable() {
        let h = pin_hash_of(b"abc");
        assert_eq!(
            hex::encode(h),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn pin_store_round_trip() {
        let dir = tempdir().expect("tempdir");
        let store = PinStore::new(dir.path().join("subdir").join("peer.pin"));
        assert!(store.load().expect("ok").is_none());
        let pin = pin_hash_of(b"some cert der");
        store.store(&pin).expect("store");
        assert_eq!(store.load().expect("load").expect("present"), pin);
    }

    #[test]
    fn pin_store_load_propagates_corrupt_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("peer.pin");
        std::fs::write(&path, "this is not a pin").expect("write");
        let store = PinStore::new(&path);
        assert!(matches!(store.load(), Err(TlsError::PinFileCorrupt(_))));
    }

    #[test]
    fn pin_store_rejects_wrong_length_hex() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("peer.pin");
        std::fs::write(&path, "abcd1234\n").expect("write");
        let store = PinStore::new(&path);
        match store.load() {
            Err(TlsError::PinFileCorrupt(msg)) => assert!(msg.contains("32")),
            other => panic!("expected PinFileCorrupt, got {other:?}"),
        }
    }

    #[test]
    fn pin_store_forget_removes_file() {
        let dir = tempdir().expect("tempdir");
        let store = PinStore::new(dir.path().join("peer.pin"));
        store.store(&pin_hash_of(b"x")).expect("store");
        store.forget().expect("forget");
        assert!(!store.path().exists());
        store.forget().expect("forget on absent");
    }

    #[cfg(unix)]
    #[test]
    fn pin_file_is_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().expect("tempdir");
        let store = PinStore::new(dir.path().join("peer.pin"));
        store.store(&pin_hash_of(b"x")).expect("store");
        let mode = std::fs::metadata(store.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
