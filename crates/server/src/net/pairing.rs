//! Server-side M9 pairing flow.
//!
//! Runs once, inside the TLS-wrapped [`Connection`], between the TLS
//! handshake completing and the normal `Hello`/`HelloAck`. Only fires
//! when no `peer.pin` exists yet (first launch / `KMWARP_REPAIR=1`).
//!
//! Protocol shape:
//!
//! ```text
//!   server                                          client
//!     │ ── PairSpakeA { msg = element A } ──────► │
//!     │ ◄─ PairSpakeB { msg = element B } ─────── │
//!     │                                            │
//!     │  both sides ::finish() → 32-byte shared K  │
//!     │                                            │
//!     │ ── PairCertExchange { our cert + HMAC } ─► │
//!     │ ◄─ PairCertExchange { peer cert + HMAC } ──│
//!     │                                            │
//!     │  each side verifies HMAC; pins             │
//!     │  SHA-256(other cert)                       │
//!     │                                            │
//!     │ ── PairAccepted ──────────────────────────►│
//!     │ ◄─ PairAccepted ─────────────────────────  │
//! ```
//!
//! `PairRejected { reason_code }` is sent (and the flow aborts) on any
//! SPAKE2 / HMAC failure. The pin file is only written on the success
//! path — a half-completed pairing leaves disk clean.
//!
//! All wire transport uses the standard `Message::Pair*` variants (added
//! to core in `40082a8`); the binary-side `Connection::read_raw` /
//! `write_raw` helpers are NOT used here. Wire framing stays uniform.

use kmwarp_core::pairing::{cert_hmac, cert_hmac_verify, generate_code, ServerPairing};
use kmwarp_core::tls::{pin_hash_of, PinStore};
use kmwarp_core::wire::Message;
use kmwarp_core::{PairingError, TlsError};
use thiserror::Error;
use tracing::{debug, error, info, warn};

use crate::error::ServerError;
use crate::net::Connection;

/// Reason codes for `Message::PairRejected`. Matches the per-variant
/// docstring on the wire enum.
mod reject_code {
    pub const SPAKE2_FAILURE: u8 = 1;
    pub const HMAC_MISMATCH: u8 = 2;
    pub const PROTOCOL_VIOLATION: u8 = 5;
}

/// Anything that can go wrong inside the server's pairing flow.
#[derive(Debug, Error)]
pub enum ServerPairingError {
    #[error("server pairing IO: {0}")]
    Connection(#[from] ServerError),

    #[error("server pairing crypto: {0}")]
    Pairing(#[from] PairingError),

    #[error("could not save peer pin: {0}")]
    PinStore(#[from] TlsError),

    #[error("client sent unexpected frame during pairing: {0}")]
    UnexpectedFrame(&'static str),

    #[error("client rejected pairing (reason code {0})")]
    Rejected(u8),

    #[error("could not write pairing code to stdout: {0}")]
    StdoutWrite(#[from] std::io::Error),
}

/// Run the server-side pairing flow.
///
/// On success the peer's cert hash is written to `pin_store` (atomic
/// `tmpfile + rename`). On any failure we send `PairRejected
/// { reason_code }` so the client logs a clear refusal, then return
/// `Err`.
pub async fn run_server_pairing_flow(
    conn: &mut Connection,
    cert_der: &[u8],
    pin_store: &PinStore,
) -> Result<(), ServerPairingError> {
    info!("entering server pairing mode (no peer.pin on disk yet)");

    // 1. Generate the 6-digit code and present it to the operator.
    let code = generate_code()?;
    display_code(&code)?;

    // 2. Start SPAKE2 server side and send element A.
    let (session, msg_a) = ServerPairing::start(&code)?;
    conn.write_frame(&Message::PairSpakeA { msg: msg_a })
        .await?;
    debug!("sent PairSpakeA");

    // 3. Receive client's element B.
    let msg_b = match conn.read_frame().await? {
        Message::PairSpakeB { msg } => msg,
        Message::PairRejected { reason_code } => {
            return Err(ServerPairingError::Rejected(reason_code))
        }
        other => {
            warn!(?other, "expected PairSpakeB; refusing");
            send_reject(conn, reject_code::PROTOCOL_VIOLATION).await;
            return Err(ServerPairingError::UnexpectedFrame("expected PairSpakeB"));
        }
    };
    debug!(b_len = msg_b.len(), "received PairSpakeB");

    // 4. Finish SPAKE2 to derive the shared key K.
    let shared_key = match session.finish(&msg_b) {
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

    // 6. Receive client's PairCertExchange and verify HMAC.
    let (peer_cert_der, peer_hmac) = match conn.read_frame().await? {
        Message::PairCertExchange { cert_der, hmac } => (cert_der, hmac),
        Message::PairRejected { reason_code } => {
            return Err(ServerPairingError::Rejected(reason_code))
        }
        other => {
            warn!(?other, "expected PairCertExchange; refusing");
            send_reject(conn, reject_code::PROTOCOL_VIOLATION).await;
            return Err(ServerPairingError::UnexpectedFrame(
                "expected PairCertExchange",
            ));
        }
    };
    let expected = cert_hmac(&shared_key, &peer_cert_der);
    if !cert_hmac_verify(&expected, &peer_hmac) {
        warn!("HMAC over client cert failed; refusing");
        send_reject(conn, reject_code::HMAC_MISMATCH).await;
        return Err(ServerPairingError::Pairing(PairingError::HmacVerifyFailed));
    }
    info!(
        peer_cert_len = peer_cert_der.len(),
        "client cert verified via SPAKE2-derived HMAC"
    );

    // 7. Pin the client's cert hash.
    let pin = pin_hash_of(&peer_cert_der);
    pin_store.store(&pin)?;
    info!(
        path = %pin_store.path().display(),
        pin = %hex::encode(pin),
        "saved peer pin"
    );

    // 8. Send PairAccepted and wait for the client's ack.
    conn.write_frame(&Message::PairAccepted).await?;
    match conn.read_frame().await? {
        Message::PairAccepted => {
            info!("pairing complete; entering normal handshake");
            Ok(())
        }
        Message::PairRejected { reason_code } => {
            warn!(reason_code, "client rejected pairing after cert exchange");
            Err(ServerPairingError::Rejected(reason_code))
        }
        other => {
            warn!(?other, "expected PairAccepted; treating as failure");
            Err(ServerPairingError::UnexpectedFrame("expected PairAccepted"))
        }
    }
}

/// Print the pairing code prominently. Goes to STDOUT (not via
/// `tracing`) so it lands above any log noise the operator may have
/// enabled.
fn display_code(code: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    writeln!(out)?;
    writeln!(out, "┌─────────────────────────────────────┐")?;
    writeln!(out, "│  kmwarp pairing code: {code:>6}        │")?;
    writeln!(out, "│  enter on client to pair this peer  │")?;
    writeln!(out, "└─────────────────────────────────────┘")?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

/// Best-effort `PairRejected`; ignore send failures because we're
/// already in an error path.
async fn send_reject(conn: &mut Connection, reason_code: u8) {
    if let Err(e) = conn
        .write_frame(&Message::PairRejected { reason_code })
        .await
    {
        error!(error = %e, reason_code, "failed to send PairRejected; closing");
    }
}
