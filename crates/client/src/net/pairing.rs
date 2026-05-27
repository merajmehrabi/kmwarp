//! Client-side M9 pairing flow.
//!
//! Mirror of `kmwarp_server::net::pairing`. The client:
//!
//! 1. Prompts the operator on stdin for the 6-digit code displayed by
//!    the server.
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

    #[error("could not read pairing code from stdin: {0}")]
    StdinRead(#[from] std::io::Error),
}

/// Run the client-side pairing flow.
///
/// On success the server's cert hash is written to `pin_store`.
///
/// `on_code` is invoked synchronously once the 6-digit code has been
/// read from stdin. It exists so non-stdout UI surfaces (the Windows
/// tray, primarily) can publish the code to the operator without
/// scraping logs. Pass `None` when no extra publication is desired
/// (headless runs, tests).
pub async fn run_client_pairing_flow(
    conn: &mut Connection,
    cert_der: &[u8],
    pin_store: &PinStore,
    on_code: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> Result<(), ClientPairingError> {
    info!("entering client pairing mode (no peer.pin on disk yet)");

    // 1. Prompt for the code.
    let code = prompt_for_code().await?;
    if let Some(cb) = on_code {
        cb(&code);
    }

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

/// Read a 6-digit pairing code from stdin. Tolerant of trailing
/// whitespace and a leading "code:" prefix.
async fn prompt_for_code() -> Result<String, ClientPairingError> {
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
    // Defer length/digit validation to ClientPairing::start (it returns
    // PairingError::CodeMustBe6Digits), so we only have one source of
    // truth for the rule.
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
