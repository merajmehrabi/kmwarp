//! Binary-side TLS plumbing.
//!
//! Thin wrapper over `kmwarp_core::tls`. The core crate owns:
//! - cert generation + on-disk persistence (`core::tls::cert::*`)
//! - pin storage (`core::tls::pin::PinStore`)
//! - the `PinnedCertVerifier` that ties into `rustls`.
//!
//! What lives here:
//! - `init_crypto_provider`: install the rustls process-default crypto
//!   provider exactly once (aws-lc-rs in rustls 0.23 stock).
//! - `build_server_config`: assemble a `rustls::ServerConfig` from the
//!   server's cert bundle and the optional pinned client cert. In
//!   pairing mode (`pinned == None`) we use a `Permissive*Verifier` that
//!   trusts any client cert; the actual auth happens inside the pairing
//!   flow that runs immediately after the TLS handshake.
//! - `default_config_dir` / `pin_path`: small path helpers that route
//!   under the same OS-conventional config dir as `Config`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kmwarp_core::tls::PinnedCertVerifier;
use kmwarp_core::TlsError;
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::ServerConfig;
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use tracing::warn;

/// Filename inside the config dir for the pinned peer cert hash.
pub const PIN_FILENAME: &str = "peer.pin";

/// Install the process-default rustls crypto provider exactly once.
/// Idempotent — a second call is a no-op so test harnesses don't trip.
pub fn init_crypto_provider() {
    // `install_default` returns `Err(_)` on second call; swallow it.
    let _ = aws_lc_rs::default_provider().install_default();
}

/// Resolve the config directory. Honors the `KMWARP_CONFIG_DIR` env
/// override (used for localhost smoke tests where the server and
/// client must NOT share certs) before falling back to the parent of
/// `Config::default_config_path()`.
pub fn default_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("KMWARP_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    kmwarp_core::config::Config::default_config_path()?
        .parent()
        .map(|p| p.to_path_buf())
}

/// `peer.pin` path inside `config_dir`.
pub fn pin_path(config_dir: &Path) -> PathBuf {
    config_dir.join(PIN_FILENAME)
}

/// Build a server-side rustls config: our cert+key as the server's
/// identity, plus a custom client-cert verifier.
///
/// `pinned == Some(hash)` → pin-mode (subsequent connect): only accept a
/// client cert whose SHA-256 matches `hash`.
///
/// `pinned == None` → pairing mode (first connect): accept any client
/// cert. The in-stream `run_server_pairing_flow` exchanges and pins
/// hashes immediately after the TLS handshake.
pub fn build_server_config(
    cert_der: &[u8],
    private_key_der: &[u8],
    pinned: Option<[u8; 32]>,
) -> Result<Arc<ServerConfig>, TlsError> {
    let cert_chain = vec![CertificateDer::from(cert_der.to_vec())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key_der.to_vec()));

    let verifier: Arc<dyn ClientCertVerifier> = match pinned {
        Some(pin) => {
            let algs = aws_lc_rs::default_provider().signature_verification_algorithms;
            Arc::new(PinnedCertVerifier::new(pin, algs))
        }
        None => Arc::new(PermissiveClientVerifier::new()),
    };

    let cfg = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert_chain, key)?;

    Ok(Arc::new(cfg))
}

/// `ClientCertVerifier` that accepts any cert. Used only during the M9
/// first-launch pairing window; the pairing flow exchanges + pins the
/// real cert immediately afterwards. Outside that window the
/// `PinnedCertVerifier` is in charge.
#[derive(Debug)]
struct PermissiveClientVerifier {
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl PermissiveClientVerifier {
    fn new() -> Self {
        Self {
            algs: aws_lc_rs::default_provider().signature_verification_algorithms,
        }
    }
}

impl ClientCertVerifier for PermissiveClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        warn!("pairing mode: accepting any client cert; pin will be set after handshake");
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}
