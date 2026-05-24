//! M9 pairing handshake: SPAKE2 + cert HMAC.
//!
//! The full first-launch flow lives in the binary crates' `net/pairing.rs`
//! (server displays a 6-digit code; client prompts for it; both sides
//! exchange SPAKE2 elements over the unencrypted TCP socket; both
//! derive a 32-byte shared key; both sides exchange their cert DER
//! authenticated by `HMAC-SHA256(K, cert_der)`; both pin the other's
//! cert hash). This module provides the pure crypto primitives that
//! flow drives.
//!
//! API surface:
//! - [`generate_code`] — random 6-digit decimal pairing code.
//! - [`ServerPairing`] / [`ClientPairing`] — symmetric wrappers around
//!   `spake2::Spake2<Ed25519Group>` so callers don't import that
//!   crate directly.
//! - [`cert_hmac`] / [`cert_hmac_verify`] — `HMAC-SHA256(K, cert_der)`
//!   with constant-time verify.
//!
//! Wire transport of SPAKE2 elements + auth frames is handled by the
//! `Message::Pair*` variants in `core::wire`.

use hmac::Mac;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use subtle::ConstantTimeEq;

use crate::error::PairingError;

/// User-visible pairing code length, in decimal digits.
pub const PAIRING_CODE_DIGITS: usize = 6;

/// SPAKE2-derived shared key length (32 bytes).
pub const SHARED_KEY_LEN: usize = 32;

/// Length of an HMAC-SHA256 tag.
pub const HMAC_LEN: usize = 32;

/// Shared key the two sides derive after exchanging SPAKE2 elements.
pub type SharedKey = [u8; SHARED_KEY_LEN];

/// Raw bytes of a single SPAKE2 element (`msg_a` or `msg_b`) carried
/// in the `PairSpakeA` / `PairSpakeB` wire messages.
pub type SpakeMessage = Vec<u8>;

/// Identity labels used in SPAKE2. Hardcoded so they're part of the
/// protocol, not config. Must be the SAME bytes on both sides.
const ID_SERVER: &[u8] = b"kmwarp-server";
const ID_CLIENT: &[u8] = b"kmwarp-client";

/// Generate a fresh `PAIRING_CODE_DIGITS`-digit decimal pairing code.
/// Uses `getrandom` (cryptographic OS RNG). Leading zeros preserved.
pub fn generate_code() -> Result<String, PairingError> {
    // 8 bytes of entropy → u64; modulo 10^6 introduces negligible bias
    // (< 1 in 2^44).
    let mut buf = [0u8; 8];
    getrandom::getrandom(&mut buf).map_err(|e| PairingError::Rng(e.to_string()))?;
    let n = u64::from_le_bytes(buf);
    let modulus = 10u64.pow(PAIRING_CODE_DIGITS as u32);
    let code = n % modulus;
    Ok(format!("{:0width$}", code, width = PAIRING_CODE_DIGITS))
}

/// Validate that `code` is exactly [`PAIRING_CODE_DIGITS`] ASCII
/// decimal digits. Returns `Err(CodeMustBe6Digits)` otherwise.
fn check_code(code: &str) -> Result<(), PairingError> {
    if code.len() != PAIRING_CODE_DIGITS || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err(PairingError::CodeMustBe6Digits);
    }
    Ok(())
}

/// Server side of the SPAKE2 handshake.
///
/// Pattern:
/// ```ignore
/// let code = generate_code()?;
/// // Display `code` to the user.
/// let (pairing, msg_a) = ServerPairing::start(&code)?;
/// // Send PairSpakeA { msg: msg_a } to the client.
/// // Receive PairSpakeB { msg: msg_b } from the client.
/// let shared = pairing.finish(&msg_b)?;
/// ```
pub struct ServerPairing {
    inner: Spake2<Ed25519Group>,
}

impl ServerPairing {
    /// Start a server-side session. Returns the SPAKE2 element A to
    /// send to the client.
    pub fn start(code: &str) -> Result<(Self, SpakeMessage), PairingError> {
        check_code(code)?;
        let (state, msg) = Spake2::<Ed25519Group>::start_a(
            &Password::new(code.as_bytes()),
            &Identity::new(ID_SERVER),
            &Identity::new(ID_CLIENT),
        );
        Ok((Self { inner: state }, msg))
    }

    /// Consume the session with the peer's element B and derive the
    /// shared key.
    pub fn finish(self, peer_msg: &[u8]) -> Result<SharedKey, PairingError> {
        let key = self.inner.finish(peer_msg).map_err(|_| PairingError::Spake)?;
        key.as_slice()
            .try_into()
            .map_err(|_| PairingError::Spake)
    }
}

/// Client side of the SPAKE2 handshake. Mirror of [`ServerPairing`].
pub struct ClientPairing {
    inner: Spake2<Ed25519Group>,
}

impl ClientPairing {
    /// Start a client-side session. Returns SPAKE2 element B.
    pub fn start(code: &str) -> Result<(Self, SpakeMessage), PairingError> {
        check_code(code)?;
        let (state, msg) = Spake2::<Ed25519Group>::start_b(
            &Password::new(code.as_bytes()),
            &Identity::new(ID_SERVER),
            &Identity::new(ID_CLIENT),
        );
        Ok((Self { inner: state }, msg))
    }

    /// Consume the session with the peer's element A and derive the
    /// shared key.
    pub fn finish(self, peer_msg: &[u8]) -> Result<SharedKey, PairingError> {
        let key = self.inner.finish(peer_msg).map_err(|_| PairingError::Spake)?;
        key.as_slice()
            .try_into()
            .map_err(|_| PairingError::Spake)
    }
}

type HmacSha256 = hmac::Hmac<sha2::Sha256>;

/// Compute `HMAC-SHA256(shared, cert_der)`. The result is the tag
/// carried in `Message::PairCertExchange.hmac`.
pub fn cert_hmac(shared: &SharedKey, cert_der: &[u8]) -> [u8; HMAC_LEN] {
    // `Hmac::new_from_slice` only fails for some backends with empty
    // keys; `shared` is always 32 bytes so this is infallible in
    // practice. We unwrap by `expect` to keep the helper signature
    // simple — see the comment for the static argument.
    let mut mac =
        HmacSha256::new_from_slice(shared).expect("SHARED_KEY_LEN=32 is always a valid HMAC key");
    mac.update(cert_der);
    mac.finalize().into_bytes().into()
}

/// Constant-time HMAC comparison. Returns `true` iff the two tags
/// match. Use this — not `==` — to avoid leaking a timing side-channel
/// that would let an attacker recover the tag byte-by-byte.
pub fn cert_hmac_verify(expected: &[u8; HMAC_LEN], actual: &[u8; HMAC_LEN]) -> bool {
    expected.ct_eq(actual).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_code_is_6_digits() {
        for _ in 0..100 {
            let code = generate_code().expect("rng");
            assert_eq!(code.len(), PAIRING_CODE_DIGITS);
            assert!(code.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn generate_code_is_6_digits_zero_padded() {
        // Force many draws; at least one should start with '0'.
        let mut saw_zero = false;
        for _ in 0..200 {
            if generate_code().expect("rng").starts_with('0') {
                saw_zero = true;
                break;
            }
        }
        assert!(saw_zero, "200 draws should include at least one leading-zero code");
    }

    #[test]
    fn check_code_rejects_wrong_length() {
        assert!(matches!(
            check_code("12345"),
            Err(PairingError::CodeMustBe6Digits)
        ));
        assert!(matches!(
            check_code("1234567"),
            Err(PairingError::CodeMustBe6Digits)
        ));
    }

    #[test]
    fn check_code_rejects_non_digits() {
        assert!(matches!(
            check_code("12345a"),
            Err(PairingError::CodeMustBe6Digits)
        ));
        assert!(matches!(
            check_code("abcdef"),
            Err(PairingError::CodeMustBe6Digits)
        ));
    }

    #[test]
    fn spake2_roundtrip_completes() {
        let code = "123456";
        let (server, msg_a) = ServerPairing::start(code).expect("server start");
        let (client, msg_b) = ClientPairing::start(code).expect("client start");

        let k_server = server.finish(&msg_b).expect("server finish");
        let k_client = client.finish(&msg_a).expect("client finish");
        assert_eq!(k_server, k_client, "both sides derive same key");
        assert_eq!(k_server.len(), SHARED_KEY_LEN);
        // Sanity: not the all-zeros key.
        assert!(k_server.iter().any(|&b| b != 0));
    }

    #[test]
    fn spake2_with_wrong_code_diverges() {
        let (server, msg_a) = ServerPairing::start("123456").expect("server");
        let (client, msg_b) = ClientPairing::start("654321").expect("client");

        // SPAKE2 always finishes; the mismatch is caught at HMAC time.
        let k_server = server.finish(&msg_b).expect("server finishes");
        let k_client = client.finish(&msg_a).expect("client finishes");
        assert_ne!(
            k_server, k_client,
            "wrong code → different keys (HMAC step catches it)"
        );
    }

    #[test]
    fn cert_hmac_roundtrip() {
        let k = [0xAAu8; 32];
        let cert = b"-----BEGIN FAKE CERT-----abc-----END-----";
        let tag1 = cert_hmac(&k, cert);
        let tag2 = cert_hmac(&k, cert);
        // Deterministic for the same key + input.
        assert_eq!(tag1, tag2);
        assert!(cert_hmac_verify(&tag1, &tag2));
    }

    #[test]
    fn cert_hmac_verify_constant_time() {
        let k = [0xAAu8; 32];
        let cert = b"cert bytes";
        let mut tag = cert_hmac(&k, cert);
        let original = tag;
        // Flip one bit anywhere → verify fails.
        tag[0] ^= 0x01;
        assert!(!cert_hmac_verify(&original, &tag));
        // Restore + flip a different position → still fails.
        tag[0] ^= 0x01;
        tag[HMAC_LEN - 1] ^= 0x80;
        assert!(!cert_hmac_verify(&original, &tag));
    }

    #[test]
    fn cert_hmac_different_keys_yield_different_tags() {
        let k1 = [0xAAu8; 32];
        let k2 = [0xBBu8; 32];
        let cert = b"cert bytes";
        let t1 = cert_hmac(&k1, cert);
        let t2 = cert_hmac(&k2, cert);
        assert_ne!(t1, t2);
        assert!(!cert_hmac_verify(&t1, &t2));
    }
}
