//! Error types for the platform-agnostic core.
//!
//! Each error type is scoped to a single concern so callers can pattern-match
//! on the variants they need without paying for unrelated context. The crate
//! intentionally avoids `anyhow` — binary `main`s above us are the only place
//! that aggregates errors.

use thiserror::Error;

/// Errors that can occur while encoding or decoding wire frames.
///
/// `decode_frame` returns `Ok(None)` when the buffer simply needs more bytes;
/// the variants below are reserved for genuine protocol violations.
#[derive(Debug, Error)]
pub enum WireError {
    /// The payload claimed more bytes than were available while decoding a
    /// known sub-field. Distinct from "need more bytes from the socket",
    /// which is signaled by `Ok(None)` from `decode_frame`.
    #[error("payload ended before all fields could be read")]
    ShortBuffer,

    /// The header carried a `msg_type` byte that the current protocol version
    /// does not recognize.
    #[error("unknown wire message type: 0x{0:02X}")]
    UnknownMsgType(u8),

    /// A payload exceeded the `u16` length field; either the caller tried to
    /// encode something too large, or the peer sent a malformed frame.
    #[error("payload too long: {len} bytes (max {max})")]
    PayloadTooLong { len: u16, max: u16 },

    /// A utf-8 field on the wire was not valid utf-8.
    #[error("invalid utf-8 in wire payload: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    /// A payload was syntactically well-formed but logically invalid (e.g. an
    /// inner length field that overruns the outer payload).
    #[error("invalid wire payload: {0}")]
    InvalidPayload(&'static str),
}

/// Errors emitted by the edge state machine.
///
/// Intentionally empty in M1 — variants are added as the state machine grows
/// in M6 and M7. Keeping the type in place from day one means `core::Result`
/// aliases and external signatures don't need to churn later.
#[derive(Debug, Error)]
pub enum StateError {}

/// Errors from `core::tls` — cert generation, pin storage, and the
/// `PinnedCertVerifier` integration with rustls.
#[derive(Debug, Error)]
pub enum TlsError {
    /// Filesystem error reading or writing cert/key/pin files.
    #[error("TLS IO error: {0}")]
    Io(#[from] std::io::Error),

    /// `rcgen` cert generation failed.
    #[error("cert generation failed: {0}")]
    Rcgen(#[from] rcgen::Error),

    /// On-disk cert/key bytes didn't parse.
    #[error("on-disk cert/key has bad format: {0}")]
    BadCertFormat(String),

    /// Pin file contents were not what we expected.
    #[error("pin file corrupt: {0}")]
    PinFileCorrupt(String),

    /// Rustls library error.
    #[error("rustls error: {0}")]
    Rustls(#[from] rustls::Error),
}

/// Errors from `core::pairing` — SPAKE2 handshake + cert HMAC.
#[derive(Debug, Error)]
pub enum PairingError {
    /// SPAKE2 protocol failure.
    #[error("SPAKE2 protocol failed")]
    Spake,

    /// HMAC over the peer's cert DER didn't match the expected tag.
    /// The M9 fail-shut path: wrong pairing code → different derived
    /// keys → HMAC mismatch → reject.
    #[error("HMAC verification of peer cert failed")]
    HmacVerifyFailed,

    /// Pairing code is not exactly 6 decimal digits.
    #[error("pairing code must be exactly 6 decimal digits")]
    CodeMustBe6Digits,

    /// `getrandom` failure during `generate_code`.
    #[error("RNG failure: {0}")]
    Rng(String),
}

/// Errors from `~/.config/kmwarp/config.toml` parsing and loading.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Filesystem error reading the config file.
    #[error("config IO error: {0}")]
    Io(#[from] std::io::Error),

    /// TOML parse failure. Carries the upstream error so the binary's
    /// `fn main()` can surface its rich span info verbatim.
    #[error("config parse error: {0}")]
    Parse(#[from] toml::de::Error),

    /// `directories` couldn't resolve a home directory on this
    /// platform — rare, but happens in sandboxed contexts without
    /// `$HOME`.
    #[error("could not resolve OS-conventional config directory")]
    MissingDir,
}
