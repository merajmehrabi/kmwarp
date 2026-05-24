//! Binary-side TLS plumbing (client).
//!
//! Thin wrapper over `kmwarp_core::tls`, mirror of `server::tls`.
//!
//! What lives here:
//! - `init_crypto_provider`: install the rustls process-default crypto
//!   provider exactly once.
//! - `build_client_config`: assemble a `rustls::ClientConfig` from our
//!   cert bundle and the optional pinned server cert. Pairing mode
//!   (`pinned == None`) uses a `PermissiveServerVerifier`.
//! - `pinned_server_name` / `default_config_dir` / `pin_path`: helpers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kmwarp_core::tls::PinnedCertVerifier;
use kmwarp_core::TlsError;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::ClientConfig;
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tracing::warn;

pub const PIN_FILENAME: &str = "peer.pin";

pub fn init_crypto_provider() {
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

pub fn pin_path(config_dir: &Path) -> PathBuf {
    config_dir.join(PIN_FILENAME)
}

/// A dummy `ServerName` we always pass to `TlsConnector::connect`. Our
/// custom verifier ignores the SNI — pinning is on `SHA-256(cert_der)`,
/// not the hostname.
pub fn pinned_server_name() -> ServerName<'static> {
    ServerName::try_from("kmwarp").expect("static literal is a valid DNS name")
}

/// Build a client-side rustls config: our cert+key for client-auth
/// presentation, plus a custom server-cert verifier (pin or pairing mode).
pub fn build_client_config(
    cert_der: &[u8],
    private_key_der: &[u8],
    pinned: Option<[u8; 32]>,
) -> Result<Arc<ClientConfig>, TlsError> {
    let cert_chain = vec![CertificateDer::from(cert_der.to_vec())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key_der.to_vec()));

    let verifier: Arc<dyn ServerCertVerifier> = match pinned {
        Some(pin) => {
            let algs = aws_lc_rs::default_provider().signature_verification_algorithms;
            // `PinnedCertVerifier` impls both Server and Client verifier
            // traits, and the binary's into_arc_server() / Arc::new wrap
            // is what the rustls API actually wants here.
            Arc::new(PinnedCertVerifier::new(pin, algs))
        }
        None => Arc::new(PermissiveServerVerifier::new()),
    };

    let cfg = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(cert_chain, key)?;

    Ok(Arc::new(cfg))
}

/// `ServerCertVerifier` that accepts any cert. Used only during the M9
/// first-launch pairing window.
#[derive(Debug)]
struct PermissiveServerVerifier {
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl PermissiveServerVerifier {
    fn new() -> Self {
        Self {
            algs: aws_lc_rs::default_provider().signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for PermissiveServerVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        warn!("pairing mode: accepting any server cert; pin will be set after handshake");
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}
