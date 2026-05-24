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
