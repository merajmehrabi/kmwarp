//! `rustls` cert verifier that checks against a stored SHA-256 pin.
//!
//! Used by both `rustls::ClientConfig` (verifying the server cert)
//! and `rustls::ServerConfig` (verifying the client cert via mutual
//! auth). Ignores `ServerName`, intermediates, and trust anchors —
//! pinning is the only authentication signal.
//!
//! Signature verification (the "does this peer hold the matching
//! private key" check) is delegated to
//! `rustls::crypto::verify_tls12_signature` /
//! `verify_tls13_signature` using algorithms from the caller's
//! installed crypto provider:
//!
//! ```ignore
//! let provider = rustls::crypto::aws_lc_rs::default_provider();
//! let verifier = PinnedCertVerifier::new(
//!     pin,
//!     provider.signature_verification_algorithms,
//! );
//! ```
//!
//! Pin comparison uses `subtle::ConstantTimeEq`.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error as RustlsError,
    SignatureScheme,
};
use subtle::ConstantTimeEq;

use crate::tls::pin::{pin_hash_of, PinHash};

/// Pin-based [`ServerCertVerifier`] + [`ClientCertVerifier`].
#[derive(Debug)]
pub struct PinnedCertVerifier {
    expected_pin: PinHash,
    supported_algs: WebPkiSupportedAlgorithms,
}

impl PinnedCertVerifier {
    pub fn new(expected_pin: PinHash, supported_algs: WebPkiSupportedAlgorithms) -> Self {
        Self {
            expected_pin,
            supported_algs,
        }
    }

    /// Shared cert-pin check.
    fn check_pin(&self, end_entity: &CertificateDer<'_>) -> Result<(), RustlsError> {
        let got = pin_hash_of(end_entity.as_ref());
        let matches: bool = got.ct_eq(&self.expected_pin).into();
        if matches {
            Ok(())
        } else {
            Err(RustlsError::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    /// Wrap in `Arc<dyn ServerCertVerifier>` for
    /// `ClientConfig::dangerous().with_custom_certificate_verifier(_)`.
    pub fn into_arc_server(self) -> Arc<dyn ServerCertVerifier> {
        Arc::new(self)
    }
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        self.check_pin(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls12_signature(message, cert, dss, &self.supported_algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(message, cert, dss, &self.supported_algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_algs.supported_schemes()
    }
}

impl ClientCertVerifier for PinnedCertVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, RustlsError> {
        self.check_pin(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls12_signature(message, cert, dss, &self.supported_algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(message, cert, dss, &self.supported_algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_algs.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::cert::generate_self_signed;

    fn provider_algs() -> WebPkiSupportedAlgorithms {
        rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms
    }

    fn dummy_server_name() -> ServerName<'static> {
        ServerName::try_from("kmwarp").expect("valid name")
    }

    #[test]
    fn pinned_verifier_accepts_matching_cert() {
        let bundle = generate_self_signed().expect("generate");
        let pin = pin_hash_of(&bundle.cert_der);
        let v = PinnedCertVerifier::new(pin, provider_algs());

        let end_entity = CertificateDer::from(bundle.cert_der.clone());
        let result = v.verify_server_cert(
            &end_entity,
            &[],
            &dummy_server_name(),
            &[],
            UnixTime::now(),
        );
        assert!(result.is_ok(), "matching pin should verify");
    }

    #[test]
    fn pinned_verifier_rejects_mismatched_cert() {
        let cert_a = generate_self_signed().expect("gen a");
        let cert_b = generate_self_signed().expect("gen b");
        let pin_of_a = pin_hash_of(&cert_a.cert_der);
        let v = PinnedCertVerifier::new(pin_of_a, provider_algs());

        let end_entity = CertificateDer::from(cert_b.cert_der.clone());
        let result = v.verify_server_cert(
            &end_entity,
            &[],
            &dummy_server_name(),
            &[],
            UnixTime::now(),
        );
        assert!(matches!(
            result,
            Err(RustlsError::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure
            ))
        ));
    }

    #[test]
    fn pinned_verifier_client_path_accepts_matching() {
        let bundle = generate_self_signed().expect("generate");
        let pin = pin_hash_of(&bundle.cert_der);
        let v = PinnedCertVerifier::new(pin, provider_algs());

        let end_entity = CertificateDer::from(bundle.cert_der.clone());
        let result = <PinnedCertVerifier as ClientCertVerifier>::verify_client_cert(
            &v,
            &end_entity,
            &[],
            UnixTime::now(),
        );
        assert!(result.is_ok());
    }
}
