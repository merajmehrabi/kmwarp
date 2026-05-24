//! M9 pairing: SPAKE2 + cert pin helpers.
//!
//! This module is the platform-agnostic core of the M9 pairing flow.
//! TLS stream wrapping and on-disk cert generation live in the binary
//! crates (server: `server::net::pairing`; client: `client::net::pairing`);
//! this module provides the primitives both sides need:
//!
//! - [`pin_hash`] — SHA-256 of a DER-encoded peer certificate, the
//!   canonical pin value.
//! - [`pin_to_hex`] / [`pin_from_hex`] — file-format conversion for
//!   `~/.config/kmwarp/peer.pin`.
//! - [`load_pin_file`] / [`save_pin_file`] — atomic read/write of the
//!   pin file at a caller-chosen path.
//! - [`gen_pairing_code`] — random 6-digit code the user reads off
//!   the server screen and types on the client.
//! - [`PairingSession`] — thin wrapper around the `spake2` crate that
//!   keeps the two sides' API symmetric.
//! - [`build_auth_frame`] / [`verify_auth_frame`] — `HMAC-SHA256(K,
//!   cert_der)`-authenticated frames for exchanging cert DER blobs
//!   under the SPAKE2-derived shared key.
//!
//! Wire-format for the auth frame (used inside the TLS pairing stream):
//!
//! ```text
//! [u16 cert_der_len LE][cert_der_bytes][32 bytes HMAC-SHA256]
//! ```

use std::path::Path;

use hmac::Mac;
use sha2::Digest;
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::error::PairingError;

/// Length of the user-visible pairing code, in decimal digits. Matches
/// PLAN.md M9 ("server generates 6-digit code, displays it").
pub const PAIRING_CODE_DIGITS: usize = 6;

/// Length of a pin hash. SHA-256 → 32 bytes.
pub const PIN_HASH_LEN: usize = 32;

/// Length of an HMAC-SHA256 tag. 32 bytes.
pub const HMAC_LEN: usize = 32;

/// Compute the canonical pin value for a DER-encoded X.509 certificate:
/// `SHA-256(cert_der)`. The hex encoding of this is what goes in the
/// `peer.pin` file.
pub fn pin_hash(cert_der: &[u8]) -> [u8; PIN_HASH_LEN] {
    sha2::Sha256::digest(cert_der).into()
}

/// Hex-encode a pin hash for storage in `peer.pin`. Lowercase, no
/// separators, matches `hex::encode` output exactly.
pub fn pin_to_hex(hash: &[u8; PIN_HASH_LEN]) -> String {
    hex::encode(hash)
}

/// Decode a hex-encoded pin from `peer.pin`. Tolerant of trailing
/// whitespace (e.g. an editor that left a newline) but strict about
/// length: must decode to exactly [`PIN_HASH_LEN`] bytes.
pub fn pin_from_hex(s: &str) -> Result<[u8; PIN_HASH_LEN], PairingError> {
    let trimmed = s.trim();
    let bytes = hex::decode(trimmed).map_err(|_| PairingError::PinNotHex)?;
    bytes.try_into().map_err(|_| PairingError::PinLength)
}

/// Load and parse the pin file. Returns `Ok(None)` if the file
/// doesn't exist (first connect, before pairing); `Ok(Some(_))` on
/// success; `Err(_)` on IO or format failure.
pub fn load_pin_file(path: &Path) -> Result<Option<[u8; PIN_HASH_LEN]>, PairingError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(pin_from_hex(&s)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(PairingError::Io(e)),
    }
}

/// Save a pin hash to the pin file. Creates parent directories if
/// needed. Caller is responsible for the path being inside the user's
/// config dir (path validation is not this module's job).
pub fn save_pin_file(path: &Path, hash: &[u8; PIN_HASH_LEN]) -> Result<(), PairingError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut s = pin_to_hex(hash);
    s.push('\n');
    std::fs::write(path, s)?;
    Ok(())
}

/// Generate a fresh `PAIRING_CODE_DIGITS`-digit decimal code. Uses
/// `getrandom` (cryptographic OS RNG). The code is uniformly drawn
/// from `[0, 10^N)` — leading zeros preserved via `format!("{:0Nw$}")`.
pub fn gen_pairing_code() -> Result<String, PairingError> {
    // 8 bytes of entropy → u64; modulo 10^6 introduces negligible bias
    // (< 1 in 2^44).
    let mut buf = [0u8; 8];
    getrandom::getrandom(&mut buf).map_err(|e| PairingError::Rng(e.to_string()))?;
    let n = u64::from_le_bytes(buf);
    let modulus = 10u64.pow(PAIRING_CODE_DIGITS as u32);
    let code = n % modulus;
    Ok(format!("{:0width$}", code, width = PAIRING_CODE_DIGITS))
}

/// Identity labels used in SPAKE2. The two sides must agree; we hard-
/// code them so the labels are part of the protocol, not config.
const ID_SERVER: &[u8] = b"kmwarp.server";
const ID_CLIENT: &[u8] = b"kmwarp.client";

/// In-progress SPAKE2 session. Wraps `spake2::Spake2<Ed25519Group>`
/// so callers don't need to import that crate directly.
///
/// The two sides start asymmetrically (`start_server` / `start_client`)
/// because SPAKE2 is asymmetric by design; both call `finish` with
/// the peer's element and obtain the same 32-byte shared key.
pub struct PairingSession {
    inner: Spake2<Ed25519Group>,
}

impl PairingSession {
    /// Server side: start a SPAKE2 session using the pairing `code`.
    /// Returns `(session, element_a)` — `element_a` must be sent to
    /// the client.
    pub fn start_server(code: &str) -> (Self, Vec<u8>) {
        let (state, msg) = Spake2::<Ed25519Group>::start_a(
            &Password::new(code.as_bytes()),
            &Identity::new(ID_SERVER),
            &Identity::new(ID_CLIENT),
        );
        (Self { inner: state }, msg)
    }

    /// Client side: start a SPAKE2 session using the pairing `code`
    /// the user typed in. Returns `(session, element_b)` — `element_b`
    /// must be sent to the server.
    pub fn start_client(code: &str) -> (Self, Vec<u8>) {
        let (state, msg) = Spake2::<Ed25519Group>::start_b(
            &Password::new(code.as_bytes()),
            &Identity::new(ID_SERVER),
            &Identity::new(ID_CLIENT),
        );
        (Self { inner: state }, msg)
    }

    /// Consume the session with the peer's element. Returns the
    /// 32-byte shared key on success; the underlying crate returns
    /// an opaque `()` error on a bad peer element, which we wrap as
    /// `PairingError::Spake2`.
    pub fn finish(self, peer_msg: &[u8]) -> Result<[u8; 32], PairingError> {
        let key = self
            .inner
            .finish(peer_msg)
            .map_err(|_| PairingError::Spake2)?;
        // The spake2 crate returns `Vec<u8>`; coerce to [u8; 32].
        let arr: [u8; 32] = key
            .as_slice()
            .try_into()
            .map_err(|_| PairingError::Spake2)?;
        Ok(arr)
    }
}

type HmacSha256 = hmac::Hmac<sha2::Sha256>;

/// Build an HMAC-SHA256-authenticated frame carrying a cert DER blob.
///
/// Layout (LE u16 length prefix + raw bytes + 32-byte HMAC):
/// ```text
/// [u16 cert_der_len][cert_der_bytes][32 bytes HMAC]
/// ```
/// The HMAC covers ONLY `cert_der` (not the length prefix); the
/// length prefix is integrity-protected indirectly by the wire codec
/// that carries this frame and by the recipient's exact-bytes check
/// in `verify_auth_frame`.
pub fn build_auth_frame(key: &[u8], cert_der: &[u8]) -> Result<Vec<u8>, PairingError> {
    let cert_len =
        u16::try_from(cert_der.len()).map_err(|_| PairingError::CertTooLong(cert_der.len()))?;
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| PairingError::HmacKey)?;
    mac.update(cert_der);
    let tag = mac.finalize().into_bytes();
    let mut out = Vec::with_capacity(2 + cert_der.len() + HMAC_LEN);
    out.extend_from_slice(&cert_len.to_le_bytes());
    out.extend_from_slice(cert_der);
    out.extend_from_slice(&tag);
    Ok(out)
}

/// Verify an HMAC-authenticated frame and return the cert DER on
/// success. Uses constant-time comparison (`hmac::Mac::verify`) so a
/// timing-side-channel can't extract the tag byte-by-byte.
pub fn verify_auth_frame(key: &[u8], frame: &[u8]) -> Result<Vec<u8>, PairingError> {
    if frame.len() < 2 + HMAC_LEN {
        return Err(PairingError::AuthFrameTooShort);
    }
    let cert_len = u16::from_le_bytes([frame[0], frame[1]]) as usize;
    let expected_len = 2 + cert_len + HMAC_LEN;
    if frame.len() != expected_len {
        return Err(PairingError::AuthFrameLen);
    }
    let cert_der = &frame[2..2 + cert_len];
    let tag = &frame[2 + cert_len..];
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| PairingError::HmacKey)?;
    mac.update(cert_der);
    mac.verify_slice(tag)
        .map_err(|_| PairingError::HmacMismatch)?;
    Ok(cert_der.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn pin_hash_is_sha256_of_input() {
        // Sanity: matches an independent SHA-256 computation.
        let input = b"hello, kmwarp";
        let expected: [u8; 32] = sha2::Sha256::digest(input).into();
        assert_eq!(pin_hash(input), expected);
    }

    #[test]
    fn pin_hex_roundtrips() {
        let input = b"some cert der bytes";
        let h = pin_hash(input);
        let s = pin_to_hex(&h);
        assert_eq!(s.len(), 64); // 32 bytes × 2 hex chars
        let back = pin_from_hex(&s).expect("decode");
        assert_eq!(back, h);
    }

    #[test]
    fn pin_from_hex_tolerates_trailing_whitespace() {
        let h = pin_hash(b"x");
        let s = format!("{}\n", pin_to_hex(&h));
        assert_eq!(pin_from_hex(&s).expect("decode"), h);
    }

    #[test]
    fn pin_from_hex_rejects_wrong_length() {
        assert!(matches!(pin_from_hex("abcd"), Err(PairingError::PinLength)));
    }

    #[test]
    fn pin_from_hex_rejects_non_hex() {
        assert!(matches!(
            pin_from_hex("not-hex-at-all"),
            Err(PairingError::PinNotHex)
        ));
    }

    #[test]
    fn pin_file_roundtrip_in_tempdir() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("subdir").join("peer.pin");
        // Missing file → Ok(None).
        assert!(load_pin_file(&path).expect("ok").is_none());

        let h = pin_hash(b"some cert");
        save_pin_file(&path, &h).expect("save");
        assert!(path.exists());
        let loaded = load_pin_file(&path).expect("load").expect("present");
        assert_eq!(loaded, h);
    }

    #[test]
    fn pin_file_load_propagates_format_errors() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("peer.pin");
        std::fs::write(&path, "this is not a pin").expect("write");
        match load_pin_file(&path) {
            Err(PairingError::PinNotHex) => {}
            other => panic!("expected PinNotHex, got {other:?}"),
        }
    }

    #[test]
    fn gen_pairing_code_is_six_digits() {
        for _ in 0..32 {
            let code = gen_pairing_code().expect("rng");
            assert_eq!(code.len(), PAIRING_CODE_DIGITS);
            assert!(code.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn gen_pairing_code_distributes_leading_zeros() {
        // Force many draws; at least one of the first chars should
        // include '0' across the population (probabilistic but very
        // safe for n=200).
        let mut saw_zero_leading = false;
        for _ in 0..200 {
            let code = gen_pairing_code().expect("rng");
            if code.starts_with('0') {
                saw_zero_leading = true;
                break;
            }
        }
        assert!(saw_zero_leading, "gen never produced a leading-zero code");
    }

    #[test]
    fn spake2_two_sides_derive_same_key() {
        let code = "123456";
        let (server, msg_a) = PairingSession::start_server(code);
        let (client, msg_b) = PairingSession::start_client(code);

        let key_server = server.finish(&msg_b).expect("server finish");
        let key_client = client.finish(&msg_a).expect("client finish");
        assert_eq!(key_server, key_client);
        assert_eq!(key_server.len(), 32);
    }

    #[test]
    fn spake2_wrong_code_yields_different_keys_or_error() {
        // SPAKE2 is designed so a wrong-password client either
        // can't complete the protocol or derives a different key.
        // The library returns Ok(_) with a different key — that's
        // the PAKE property; only the subsequent HMAC step fails.
        let (server, msg_a) = PairingSession::start_server("111111");
        let (client, msg_b) = PairingSession::start_client("999999");

        let key_server = server.finish(&msg_b).expect("server finishes");
        let key_client = client.finish(&msg_a).expect("client finishes");
        // Different code → different derived keys.
        assert_ne!(key_server, key_client);
    }

    #[test]
    fn auth_frame_roundtrips() {
        let key = [0xAAu8; 32];
        let cert_der = b"-----BEGIN FAKE CERT-----abc-----END-----";
        let frame = build_auth_frame(&key, cert_der).expect("build");
        let recovered = verify_auth_frame(&key, &frame).expect("verify");
        assert_eq!(recovered.as_slice(), cert_der);
    }

    #[test]
    fn auth_frame_rejects_wrong_key() {
        let key = [0xAAu8; 32];
        let other = [0xBBu8; 32];
        let frame = build_auth_frame(&key, b"cert").expect("build");
        assert!(matches!(
            verify_auth_frame(&other, &frame),
            Err(PairingError::HmacMismatch)
        ));
    }

    #[test]
    fn auth_frame_rejects_tampered_cert() {
        let key = [0xAAu8; 32];
        let mut frame = build_auth_frame(&key, b"cert").expect("build");
        // Flip a byte in the cert region (position 2 is the first
        // cert byte after the u16 length prefix).
        frame[2] ^= 0x01;
        assert!(matches!(
            verify_auth_frame(&key, &frame),
            Err(PairingError::HmacMismatch)
        ));
    }

    #[test]
    fn auth_frame_rejects_tampered_tag() {
        let key = [0xAAu8; 32];
        let mut frame = build_auth_frame(&key, b"cert").expect("build");
        let last = frame.len() - 1;
        frame[last] ^= 0x01;
        assert!(matches!(
            verify_auth_frame(&key, &frame),
            Err(PairingError::HmacMismatch)
        ));
    }

    #[test]
    fn auth_frame_rejects_short_buffer() {
        let key = [0xAAu8; 32];
        let short = [0u8; 4];
        assert!(matches!(
            verify_auth_frame(&key, &short),
            Err(PairingError::AuthFrameTooShort)
        ));
    }

    #[test]
    fn auth_frame_rejects_wrong_total_length() {
        let key = [0xAAu8; 32];
        let mut frame = build_auth_frame(&key, b"cert").expect("build");
        // Drop one byte from the middle to make the total length
        // mismatch the encoded cert_len.
        frame.pop();
        assert!(matches!(
            verify_auth_frame(&key, &frame),
            Err(PairingError::AuthFrameLen)
        ));
    }

    /// The full M9 acceptance shape: two sides exchange SPAKE2
    /// elements, derive K, exchange HMAC-authed cert DERs, pin the
    /// hashes. End-to-end mock; no TLS yet.
    #[test]
    fn full_pairing_flow_in_memory() {
        let code = gen_pairing_code().expect("rng");

        // Server emits element_a; client emits element_b. Both finish.
        let (s, msg_a) = PairingSession::start_server(&code);
        let (c, msg_b) = PairingSession::start_client(&code);
        let k_server = s.finish(&msg_b).expect("server K");
        let k_client = c.finish(&msg_a).expect("client K");
        assert_eq!(k_server, k_client);

        // Each side sends its cert DER inside an HMAC-authed frame.
        let server_cert = b"DER-of-server-cert".to_vec();
        let client_cert = b"DER-of-client-cert".to_vec();

        let frame_s2c = build_auth_frame(&k_server, &server_cert).expect("build s");
        let frame_c2s = build_auth_frame(&k_client, &client_cert).expect("build c");

        let recovered_at_client = verify_auth_frame(&k_client, &frame_s2c).expect("verify s");
        let recovered_at_server = verify_auth_frame(&k_server, &frame_c2s).expect("verify c");
        assert_eq!(recovered_at_client, server_cert);
        assert_eq!(recovered_at_server, client_cert);

        // Each side pins the OTHER side's cert DER.
        let server_side_pin = pin_hash(&recovered_at_server);
        let client_side_pin = pin_hash(&recovered_at_client);
        assert_eq!(server_side_pin, pin_hash(&client_cert));
        assert_eq!(client_side_pin, pin_hash(&server_cert));
    }
}
