//! TLS plumbing for the server (M9).
//!
//! This module owns three concerns:
//!
//! 1. **Self-signed cert generation + persistence.** Each binary gets its
//!    own cert at first launch, kept in the user config dir. Saved with
//!    `0600` mode on Unix to keep the private key inaccessible to other
//!    local users.
//! 2. **Custom `ClientCertVerifier`** that runs in one of two modes:
//!    - **Pin mode** (subsequent connects): rejects unless the client cert's
//!      `SHA-256(DER)` matches the pinned value from `peer.pin`. Constant-
//!      time compare via `subtle::ConstantTimeEq` so a timing-side-channel
//!      can't extract the pin byte-by-byte.
//!    - **Pairing mode** (first connect, no `peer.pin` on disk): accepts
//!      any cert. The M9 pairing flow runs inside the TLS stream
//!      immediately after the handshake and writes the pin afterwards.
//! 3. **`build_server_config`** wires our cert+key as the server identity
//!    and installs the verifier as the mutual-TLS authentication step.
//!
//! Crypto provider: we use `rustls::crypto::ring::default_provider()`.
//! It must be installed once per process via `install_default()` — we do
//! that in [`init_crypto_provider`] which is idempotent (returns early on
//! double-call) so test harnesses don't trip over it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{ServerConfig, WebPkiClientVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tracing::{info, warn};

/// Filename inside the config dir for the persisted cert (DER).
pub const CERT_FILENAME: &str = "cert.der";

/// Filename inside the config dir for the persisted private key (PKCS#8 DER).
pub const KEY_FILENAME: &str = "key.der";

/// Filename inside the config dir for the pinned peer cert hash.
pub const PIN_FILENAME: &str = "peer.pin";

/// Validity window for a freshly-generated self-signed cert. 10 years; we
/// re-key only on explicit user action (deleting `cert.der` + `key.der`),
/// not on calendar rotation.
const CERT_VALIDITY_DAYS: i64 = 365 * 10;

/// Anything that can go wrong wiring TLS / certs on the server.
#[derive(Debug, Error)]
pub enum TlsError {
    #[error("TLS IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("cert generation failed: {0}")]
    Rcgen(#[from] rcgen::Error),

    #[error("on-disk cert / key has bad format: {0}")]
    BadCertFormat(String),

    #[error("rustls error: {0}")]
    Rustls(#[from] rustls::Error),

    #[error("rustls verifier builder error: {0}")]
    VerifierBuilder(String),
}

/// Owned cert + private-key DER blobs as understood by `rustls`.
#[derive(Clone)]
pub struct CertBundle {
    pub cert_der: Vec<u8>,
    pub private_key_der: Vec<u8>,
}

/// Install the default rustls crypto provider (aws-lc-rs in rustls 0.23
/// stock build). Safe to call from multiple entry points — only the
/// first call wins; subsequent calls return early.
pub fn init_crypto_provider() {
    // `install_default` returns an `Err(_)` if a provider is already set,
    // which is exactly the "second call" case we want to swallow.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Load an existing cert+key from disk, or generate a fresh self-signed
/// pair on first launch and persist them.
pub fn load_or_generate_certs(cert_path: &Path, key_path: &Path) -> Result<CertBundle, TlsError> {
    if cert_path.exists() && key_path.exists() {
        let cert_der = fs::read(cert_path)?;
        let key_der = fs::read(key_path)?;
        return Ok(CertBundle {
            cert_der,
            private_key_der: key_der,
        });
    }
    info!(
        ?cert_path,
        ?key_path,
        "no cert / key on disk; generating self-signed pair"
    );
    let bundle = generate_self_signed()?;
    persist(&bundle, cert_path, key_path)?;
    Ok(bundle)
}

/// Generate a fresh self-signed cert + key pair.
///
/// The cert has a single CN of `"kmwarp"` (the server name in our pin-
/// verifier is irrelevant; we authenticate via SHA-256 of the cert DER,
/// not the cert chain). Valid for [`CERT_VALIDITY_DAYS`] days from now.
fn generate_self_signed() -> Result<CertBundle, TlsError> {
    let mut params = CertificateParams::new(vec!["kmwarp".to_string()])?;
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + time::Duration::days(CERT_VALIDITY_DAYS);
    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;
    Ok(CertBundle {
        cert_der: cert.der().to_vec(),
        private_key_der: key_pair.serialize_der(),
    })
}

/// Write the cert + key to disk with hardened permissions on Unix.
fn persist(bundle: &CertBundle, cert_path: &Path, key_path: &Path) -> Result<(), TlsError> {
    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_with_mode(cert_path, &bundle.cert_der, 0o600)?;
    write_with_mode(key_path, &bundle.private_key_der, 0o600)?;
    Ok(())
}

/// Atomic-ish write with `0o600` perms on Unix; on Windows the mode is
/// ignored and we rely on the user-profile ACL inherited from the parent
/// dir (M10 packaging will tighten this).
fn write_with_mode(path: &Path, data: &[u8], _mode: u32) -> Result<(), TlsError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(_mode)
            .open(path)?;
        use std::io::Write;
        f.write_all(data)?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, data)?;
    }
    Ok(())
}

/// Build a server-side rustls config: our cert+key as the server's
/// identity, plus a custom client-cert verifier (pin-mode or pairing-mode
/// based on `pinned`).
pub fn build_server_config(
    bundle: &CertBundle,
    pinned: Option<[u8; 32]>,
) -> Result<Arc<ServerConfig>, TlsError> {
    let cert_chain = vec![CertificateDer::from(bundle.cert_der.clone())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(bundle.private_key_der.clone()));

    let verifier = Arc::new(PinnedClientVerifier { pinned });

    let cfg = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert_chain, key)?;

    Ok(Arc::new(cfg))
}

/// Custom `ClientCertVerifier` that fails closed against a pinned hash.
///
/// `pinned == None` means "pairing mode" — we trust any cert and let the
/// in-stream pairing flow exchange the real pins. Otherwise we recompute
/// `SHA-256(end_entity_cert_der)` and constant-time-compare against the
/// stored pin.
#[derive(Debug)]
struct PinnedClientVerifier {
    pinned: Option<[u8; 32]>,
}

impl ClientCertVerifier for PinnedClientVerifier {
    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let Some(expected) = self.pinned.as_ref() else {
            warn!("pairing mode: accepting any client cert; pin will be set after handshake");
            return Ok(ClientCertVerified::assertion());
        };
        let actual = sha2_of(end_entity.as_ref());
        if bool::from(actual.ct_eq(expected)) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    /// We never present a CA list — the client is expected to send its
    /// self-signed cert directly, and we authenticate it by hash.
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    /// Permissive: we don't actually verify the signature against a CA —
    /// the pin check above is the only authentication step. Delegate to
    /// the webpki default validator for the on-wire signature shape only.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        WebPkiClientVerifier::builder(default_root_store())
            .build()
            .map_err(|e| rustls::Error::General(e.to_string()))?
            .verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        WebPkiClientVerifier::builder(default_root_store())
            .build()
            .map_err(|e| rustls::Error::General(e.to_string()))?
            .verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        // The schemes rustls' default crypto provider supports for self-
        // signed ECDSA P-256 / Ed25519 certs (which rcgen 0.13 generates).
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
        ]
    }

    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }
}

/// Hash a single byte slice with SHA-256.
fn sha2_of(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

/// An empty root store. Our pin check is the real authentication; this
/// shim is just here so the WebPKI signature-shape validator has
/// something to bind against.
fn default_root_store() -> Arc<rustls::RootCertStore> {
    Arc::new(rustls::RootCertStore::empty())
}

/// Default config-dir paths for the M9 cert / key / pin files. Resolves
/// via the same logic as `Config::default_config_path` (so the cert and
/// pin live alongside `config.toml`).
pub struct M9Paths {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub pin: PathBuf,
}

impl M9Paths {
    /// Resolve under the OS-conventional config dir. `None` if the
    /// platform doesn't expose a home directory.
    #[allow(clippy::should_implement_trait)]
    pub fn default() -> Option<Self> {
        let dir = kmwarp_core::config::Config::default_config_path()?
            .parent()?
            .to_path_buf();
        Some(Self {
            cert: dir.join(CERT_FILENAME),
            key: dir.join(KEY_FILENAME),
            pin: dir.join(PIN_FILENAME),
        })
    }
}
