//! M9 TLS plumbing: cert generation, pin storage, custom verifier.
//!
//! Three submodules, all platform-agnostic but feature-heavy on deps:
//!
//! - [`cert`] — self-signed cert generation via `rcgen` + on-disk
//!   persistence (with `0600` perms on Unix for the key file).
//! - [`pin`] — SHA-256 cert pin computation + atomic on-disk storage.
//! - [`verifier`] — [`rustls`] custom verifier that checks against a
//!   stored pin; ignores `ServerName` and trust anchors entirely.
//!
//! See `core::pairing` for the SPAKE2 first-launch handshake that
//! populates the pin.

pub mod cert;
pub mod pin;
pub mod verifier;

pub use cert::{generate_self_signed, load_from_disk, save_to_disk, CertBundle};
pub use pin::{pin_hash_of, PinHash, PinStore, PIN_HASH_LEN};
pub use verifier::PinnedCertVerifier;
