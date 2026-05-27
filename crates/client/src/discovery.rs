//! mDNS-SD service discovery (v1.1).
//!
//! Browses `_kmwarp._tcp.local.` to find a kmwarp-server on the LAN
//! when the operator hasn't pinned a target via `KMWARP_CONNECT`.
//!
//! ## Design
//!
//! Synchronous blocking discovery with a hard deadline. The connect
//! path in `app::run_client` is fundamentally serial anyway (resolve
//! address → connect with backoff → handshake) so there's no value
//! in making the browse async; a `std::thread::spawn`'d browse + a
//! `recv_timeout` keeps the call shape inside `app::run_client`'s
//! existing sync prelude.
//!
//! ## First-resolved wins
//!
//! The first `ServiceResolved` event with a routable IPv4 address +
//! non-zero port wins. Subsequent peers are ignored — v1 is single-
//! peer per spec. Multi-server discovery (a list-and-pick UX) is
//! deferred to whenever the menu bar grows a "Connect to…" submenu.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use thiserror::Error;
use tracing::{debug, info, warn};

/// Service type browsed for. Must match the server's
/// `discovery::SERVICE_TYPE`. Kept as a separate const here (not
/// shared via core) because client/server are deployed independently
/// and a typo on either side would silently fail to discover —
/// having two textual occurrences makes that grep-able.
pub const SERVICE_TYPE: &str = "_kmwarp._tcp.local.";

/// How long to wait for the first usable `ServiceResolved` event
/// before giving up. 10 s is the team spec; long enough that a slow
/// router won't lose the announce, short enough that a missing
/// server fails fast and the operator sees a clear error.
pub const DEFAULT_BROWSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors raised while browsing for a server.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// `ServiceDaemon::new` failed (port 5353 conflict, etc.).
    #[error("failed to create mDNS daemon: {0}")]
    Daemon(String),

    /// `ServiceDaemon::browse` rejected the service type.
    #[error("failed to start mDNS browse for {SERVICE_TYPE}: {0}")]
    Browse(String),

    /// Browse ran for the full timeout window without finding a
    /// resolvable server. This is the path the operator hits when
    /// the Mac side isn't running.
    #[error(
        "no kmwarp-server found on the LAN after {timeout_ms}ms — \
         is the Mac side running? (override with KMWARP_CONNECT=<ip>:<port>)"
    )]
    NotFound { timeout_ms: u64 },
}

/// Block for up to `timeout` waiting for the first
/// `ServiceResolved` event carrying at least one IPv4 address and a
/// non-zero port. Returns the resolved `SocketAddr` (first IPv4 of
/// the first server to respond) on success.
///
/// Best-effort daemon cleanup on the way out — if `shutdown` errors
/// we don't care, the process is about to either connect-and-loop
/// or exit.
pub fn discover_server(timeout: Duration) -> Result<SocketAddr, DiscoveryError> {
    let daemon = ServiceDaemon::new().map_err(|e| DiscoveryError::Daemon(e.to_string()))?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .map_err(|e| DiscoveryError::Browse(e.to_string()))?;

    info!(
        service_type = SERVICE_TYPE,
        timeout_ms = timeout.as_millis() as u64,
        "browsing mDNS for kmwarp-server"
    );

    let deadline = Instant::now() + timeout;
    let mut result: Result<SocketAddr, DiscoveryError> = Err(DiscoveryError::NotFound {
        timeout_ms: timeout.as_millis() as u64,
    });

    loop {
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(d) => d,
            None => break,
        };
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let port = info.get_port();
                if port == 0 {
                    debug!(name = info.get_fullname(), "ignoring resolved service with port 0");
                    continue;
                }
                // get_addresses returns an enriched IpAddr set in
                // 0.13+; pick the first non-loopback IPv4. Routable
                // IPv6 support is deferred — v1 is LAN-scoped and
                // pin store / TLS verifier are built around IPv4.
                let ipv4 = info
                    .get_addresses()
                    .iter()
                    .filter_map(|ip| match ip {
                        std::net::IpAddr::V4(v4) if !v4.is_loopback() => Some(*v4),
                        _ => None,
                    })
                    .next();
                let Some(ipv4) = ipv4 else {
                    debug!(
                        name = info.get_fullname(),
                        addrs = ?info.get_addresses(),
                        "resolved kmwarp-server but no routable IPv4 in address set; waiting"
                    );
                    continue;
                };
                let addr = SocketAddr::from((ipv4, port));
                info!(
                    name = info.get_fullname(),
                    addr = %addr,
                    "discovered kmwarp-server via mDNS"
                );
                result = Ok(addr);
                break;
            }
            Ok(ev) => {
                // ServiceFound / ServiceRemoved / SearchStarted / etc.
                // Useful for diagnostics but not a connection-ready
                // event — keep waiting for ServiceResolved.
                debug!(?ev, "mDNS event while browsing");
            }
            Err(_) => break, // timeout / closed channel
        }
    }

    // Best-effort daemon teardown; failure is ignorable.
    let _ = daemon.shutdown();

    if result.is_err() {
        warn!(
            timeout_ms = timeout.as_millis() as u64,
            "mDNS browse timed out without resolving a kmwarp-server"
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_type_matches_server_format() {
        assert!(SERVICE_TYPE.ends_with("._tcp.local."));
        assert!(SERVICE_TYPE.starts_with("_kmwarp."));
    }

    #[test]
    fn not_found_error_mentions_override() {
        let err = DiscoveryError::NotFound { timeout_ms: 1000 };
        let s = err.to_string();
        assert!(s.contains("KMWARP_CONNECT"));
        assert!(s.contains("1000"));
    }
}
