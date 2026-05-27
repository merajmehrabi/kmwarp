//! Client-side M9 pairing flow.
//!
//! Mirror of `kmwarp_server::net::pairing`. The client:
//!
//! 1. Awaits the operator-supplied 6-digit code (see [`CodeProvider`]).
//! 2. Receives `PairSpakeA`.
//! 3. Sends `PairSpakeB`.
//! 4. Finishes SPAKE2 → derives shared key K.
//! 5. Sends our cert under HMAC(K) via `PairCertExchange`.
//! 6. Receives the server's `PairCertExchange` and verifies HMAC.
//! 7. Pins SHA-256(server cert).
//! 8. Sends `PairAccepted` and awaits the server's `PairAccepted`.
//!
//! Crypto primitives come from `kmwarp_core::pairing`; pin storage from
//! `kmwarp_core::tls::PinStore`.
//!
//! ## Code provider contract (v1.1)
//!
//! v1.0 hard-coded an `stdin` read for step 1. v1.1 inverts the call:
//! the operator-facing input surface is *injected* by the caller as a
//! [`CodeProvider`]. Two production providers ship today:
//!
//! * [`stdin_code_provider`] — reads a line from stdin (CLI / headless
//!   path; KMWARP_HEADLESS=1 services).
//! * `crate::platform::windows::pairing_dialog::dialog_code_provider`
//!   — pops a native Win32 input dialog (Windows tray path).
//!
//! Implementations are free to:
//!   * run on any task (the future returned by the provider is `Send`
//!     and may be awaited from a tokio worker);
//!   * spawn helper threads (Windows dialogs in particular need a
//!     dedicated GUI thread because the tokio runtime owns the tray's
//!     main thread and can't block on `DialogBoxW`);
//!   * fail fast with a descriptive `anyhow::Error` if the operator
//!     cancels / times out — the pairing flow surfaces the message
//!     to logs and aborts the attempt; the connect-loop will
//!     reconnect and the provider will be re-invoked.
//!
//! The provider is consumed exactly once per pairing attempt
//! (`FnOnce`); a fresh closure is built per attempt by the caller in
//! `app::run_client` so any builder-style setup (HWND for the dialog,
//! etc.) happens per-attempt and not at startup.

use std::future::Future;
use std::pin::Pin;

use kmwarp_core::pairing::{cert_hmac, cert_hmac_verify, ClientPairing};
use kmwarp_core::tls::{pin_hash_of, PinStore};
use kmwarp_core::wire::Message;
use kmwarp_core::{PairingError, TlsError};
use thiserror::Error;
use tokio::io::AsyncBufReadExt;
use tracing::{debug, error, info, warn};

use crate::error::ClientError;
use crate::net::Connection;

mod reject_code {
    pub const SPAKE2_FAILURE: u8 = 1;
    pub const HMAC_MISMATCH: u8 = 2;
    pub const PROTOCOL_VIOLATION: u8 = 5;
}

/// A pinned, boxed future that resolves to the operator-typed pairing
/// code. `Send` because it crosses task boundaries inside the pairing
/// flow; `'static` because the boxed closure that produces it lives
/// independently of the call site.
pub type CodeFuture = Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>>;

/// One-shot async producer of the 6-digit pairing code. See module
/// docs for the contract.
///
/// Construct one via [`stdin_code_provider`] (headless) or via the
/// platform-specific dialog factory (Windows tray path).
pub type CodeProvider = Box<dyn FnOnce() -> CodeFuture + Send>;

/// Builder of fresh [`CodeProvider`]s, one per pairing attempt.
///
/// The pairing flow consumes a [`CodeProvider`] via `FnOnce`, but a
/// connect-loop iteration that *fails* mid-pairing has to retry on
/// the next reconnect — which means a fresh provider. The factory
/// lives at `run_client` scope and is invoked once per attempt by
/// `run_one_session`.
///
/// Standard factories:
///   * `Box::new(|| stdin_code_provider())` — headless / terminal.
///   * On Windows tray path:
///     `Box::new(move || dialog_code_provider(hwnd_clone))`.
pub type CodeProviderFactory = Box<dyn Fn() -> CodeProvider + Send + Sync>;

/// Default provider: prompt the user on stdin for a 6-digit code,
/// tolerant of trailing whitespace and a leading "code:" prefix.
///
/// Used by every entry path that doesn't have a richer input surface
/// available: KMWARP_HEADLESS=1, the Windows service helper path,
/// any non-Windows / non-tray client build, and the integration
/// tests (via [`fixed_code_provider`]).
pub fn stdin_code_provider() -> CodeProvider {
    Box::new(|| Box::pin(async move { read_code_from_stdin().await }))
}

/// Test / scripted provider: returns the supplied code verbatim, no
/// I/O. Useful for integration tests that drive the pairing flow
/// without a real operator.
#[cfg(test)]
pub fn fixed_code_provider(code: impl Into<String>) -> CodeProvider {
    let code = code.into();
    Box::new(move || Box::pin(async move { Ok(code) }))
}

#[derive(Debug, Error)]
pub enum ClientPairingError {
    #[error("client pairing IO: {0}")]
    Connection(#[from] ClientError),

    #[error("client pairing crypto: {0}")]
    Pairing(#[from] PairingError),

    #[error("could not save peer pin: {0}")]
    PinStore(#[from] TlsError),

    #[error("server sent unexpected frame during pairing: {0}")]
    UnexpectedFrame(&'static str),

    #[error("server rejected pairing (reason code {0})")]
    Rejected(u8),

    /// The injected [`CodeProvider`] returned an error before yielding
    /// a code — operator cancelled the dialog, stdin closed, etc.
    /// Carries the provider's `anyhow::Error` formatted as a string
    /// so this enum stays cheap to clone / serialize.
    #[error("could not obtain pairing code from operator: {0}")]
    CodeProvider(String),
}

/// Run the client-side pairing flow.
///
/// `code_provider` is consumed exactly once; its returned future is
/// awaited inline. On success the server's cert hash is written to
/// `pin_store`.
pub async fn run_client_pairing_flow(
    conn: &mut Connection,
    cert_der: &[u8],
    pin_store: &PinStore,
    code_provider: CodeProvider,
) -> Result<(), ClientPairingError> {
    info!("entering client pairing mode (no peer.pin on disk yet)");

    // 1. Pull the code from whatever input surface the caller wired
    //    up. The provider future may take seconds (user typing into a
    //    dialog) or be instant (a fixed test provider); the pairing
    //    flow doesn't care.
    let code = (code_provider)()
        .await
        .map_err(|e| ClientPairingError::CodeProvider(format!("{e:#}")))?;

    // 2. Receive PairSpakeA from server.
    let msg_a = match conn.read_frame().await? {
        Message::PairSpakeA { msg } => msg,
        Message::PairRejected { reason_code } => {
            return Err(ClientPairingError::Rejected(reason_code))
        }
        other => {
            warn!(?other, "expected PairSpakeA; refusing");
            send_reject(conn, reject_code::PROTOCOL_VIOLATION).await;
            return Err(ClientPairingError::UnexpectedFrame("expected PairSpakeA"));
        }
    };
    debug!(a_len = msg_a.len(), "received PairSpakeA");

    // 3. Start SPAKE2 client and send PairSpakeB.
    let (session, msg_b) = ClientPairing::start(&code)?;
    conn.write_frame(&Message::PairSpakeB { msg: msg_b })
        .await?;
    debug!("sent PairSpakeB");

    // 4. Finish SPAKE2.
    let shared_key = match session.finish(&msg_a) {
        Ok(k) => k,
        Err(e) => {
            warn!(error = ?e, "SPAKE2 finish failed; refusing");
            send_reject(conn, reject_code::SPAKE2_FAILURE).await;
            return Err(e.into());
        }
    };
    debug!("SPAKE2 shared key derived");

    // 5. Send our cert under HMAC(K).
    let hmac = cert_hmac(&shared_key, cert_der);
    conn.write_frame(&Message::PairCertExchange {
        cert_der: cert_der.to_vec(),
        hmac,
    })
    .await?;
    debug!("sent PairCertExchange (our cert)");

    // 6. Receive server's PairCertExchange and verify HMAC.
    let (peer_cert_der, peer_hmac) = match conn.read_frame().await? {
        Message::PairCertExchange { cert_der, hmac } => (cert_der, hmac),
        Message::PairRejected { reason_code } => {
            return Err(ClientPairingError::Rejected(reason_code))
        }
        other => {
            warn!(?other, "expected PairCertExchange; refusing");
            send_reject(conn, reject_code::PROTOCOL_VIOLATION).await;
            return Err(ClientPairingError::UnexpectedFrame(
                "expected PairCertExchange",
            ));
        }
    };
    let expected = cert_hmac(&shared_key, &peer_cert_der);
    if !cert_hmac_verify(&expected, &peer_hmac) {
        warn!("HMAC over server cert failed; refusing");
        send_reject(conn, reject_code::HMAC_MISMATCH).await;
        return Err(ClientPairingError::Pairing(PairingError::HmacVerifyFailed));
    }
    info!(
        peer_cert_len = peer_cert_der.len(),
        "server cert verified via SPAKE2-derived HMAC"
    );

    // 7. Pin the server's cert hash.
    let pin = pin_hash_of(&peer_cert_der);
    pin_store.store(&pin)?;
    info!(
        path = %pin_store.path().display(),
        pin = %hex::encode(pin),
        "saved peer pin"
    );

    // 8. Send PairAccepted and await server's ack.
    conn.write_frame(&Message::PairAccepted).await?;
    match conn.read_frame().await? {
        Message::PairAccepted => {
            info!("pairing complete; entering normal handshake");
            Ok(())
        }
        Message::PairRejected { reason_code } => {
            warn!(reason_code, "server rejected pairing after cert exchange");
            Err(ClientPairingError::Rejected(reason_code))
        }
        other => {
            warn!(?other, "expected PairAccepted; treating as failure");
            Err(ClientPairingError::UnexpectedFrame("expected PairAccepted"))
        }
    }
}

/// Internal: do the stdin read for [`stdin_code_provider`]. Tolerant
/// of trailing whitespace and a leading "code:" prefix. Defers
/// length / digit validation to [`ClientPairing::start`] (single
/// source of truth: `PairingError::CodeMustBe6Digits`).
async fn read_code_from_stdin() -> anyhow::Result<String> {
    use std::io::Write;
    {
        let mut out = std::io::stderr().lock();
        write!(out, "Enter pairing code (6 digits): ").ok();
        out.flush().ok();
    }
    let stdin = tokio::io::stdin();
    let mut reader = tokio::io::BufReader::new(stdin);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let trimmed: String = line
        .trim()
        .trim_start_matches("code:")
        .trim()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    Ok(trimmed)
}

async fn send_reject(conn: &mut Connection, reason_code: u8) {
    if let Err(e) = conn
        .write_frame(&Message::PairRejected { reason_code })
        .await
    {
        error!(error = %e, reason_code, "failed to send PairRejected; closing");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixed_provider_returns_code() {
        let provider = fixed_code_provider("123456");
        let code = provider().await.expect("fixed provider never fails");
        assert_eq!(code, "123456");
    }

    #[tokio::test]
    async fn stdin_provider_is_constructible() {
        // We can't drive stdin in a unit test, but constructing the
        // provider shouldn't allocate I/O or block — proving the
        // FnOnce shape is well-formed.
        let _provider: CodeProvider = stdin_code_provider();
    }
}
