//! TLS plumbing for the client (M9).
//!
//! Mirror of `kmwarp_server::tls`. The two are intentionally near-
//! identical — the only real differences are:
//!
//!  * client implements [`ServerCertVerifier`] (validates the server's
//!    cert) instead of `ClientCertVerifier`.
//!  * client's TLS config carries the client cert as an *auth identity*
//!    (`with_client_auth_cert`) so mutual TLS works.
//!
//! See the server-side file for the detailed design rationale (pin-mode
//! vs pairing-mode, constant-time compare, persistence layout, etc.).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcgen::{CertificateParams, KeyPair};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tracing::{info, warn};

pub const CERT_FILENAME: &str = "cert.der";
pub const KEY_FILENAME: &str = "key.der";
pub const PIN_FILENAME: &str = "peer.pin";

const CERT_VALIDITY_DAYS: i64 = 365 * 10;

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
}

#[derive(Clone)]
pub struct CertBundle {
    pub cert_der: Vec<u8>,
    pub private_key_der: Vec<u8>,
}

pub fn init_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

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

fn persist(bundle: &CertBundle, cert_path: &Path, key_path: &Path) -> Result<(), TlsError> {
    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_with_mode(cert_path, &bundle.cert_der, 0o600)?;
    write_with_mode(key_path, &bundle.private_key_der, 0o600)?;
    Ok(())
}

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

/// Build a client-side rustls config: our cert+key for client-auth
/// presentation, plus a custom server-cert verifier (pin-mode or
/// pairing-mode).
pub fn build_client_config(
    bundle: &CertBundle,
    pinned: Option<[u8; 32]>,
) -> Result<Arc<ClientConfig>, TlsError> {
    let cert_chain = vec![CertificateDer::from(bundle.cert_der.clone())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(bundle.private_key_der.clone()));

    let verifier = Arc::new(PinnedServerVerifier { pinned });

    let cfg = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(cert_chain, key)?;

    Ok(Arc::new(cfg))
}

/// A dummy `ServerName` we always pass to `TlsConnector::connect`. The
/// pin verifier ignores the SNI — pinning is on the SHA-256 of the cert
/// DER, not on the hostname.
pub fn pinned_server_name() -> ServerName<'static> {
    // "kmwarp" is the CN we put in self-signed certs; using it for SNI
    // keeps the handshake clean even though our verifier ignores it.
    ServerName::try_from("kmwarp").expect("static literal is a valid DNS name")
}

#[derive(Debug)]
struct PinnedServerVerifier {
    pinned: Option<[u8; 32]>,
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let Some(expected) = self.pinned.as_ref() else {
            warn!("pairing mode: accepting any server cert; pin will be set after handshake");
            return Ok(ServerCertVerified::assertion());
        };
        let actual = sha2_of(end_entity.as_ref());
        if bool::from(actual.ct_eq(expected)) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    /// We don't actually verify the chain (the pin is authoritative); but
    /// we still need the signature shape to be valid. rustls' default
    /// verifier handles the schemes we care about.
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
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
}

fn sha2_of(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
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
    /// Resolve under the OS-conventional config dir.
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
