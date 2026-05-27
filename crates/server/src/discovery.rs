//! mDNS-SD service advertisement (v1.1).
//!
//! Registers `_kmwarp._tcp.local.` on every local IPv4 interface so a
//! `kmwarp-client` on the same LAN can find us without the operator
//! pasting `KMWARP_CONNECT=<ip>:<port>` on the Windows side.
//!
//! ## Failure mode
//!
//! mDNS is a convenience, not a hard requirement — the manual
//! `KMWARP_CONNECT` override path still works end-to-end. So this
//! module returns a `Result` but the caller (`app::run_server`) logs
//! and continues on `Err`; the listener keeps running, just unannounced.
//!
//! ## Lifetime
//!
//! [`ServiceAdvertisement`] holds the `ServiceDaemon` retain and the
//! registered `fullname`. Dropping it unregisters cleanly and shuts
//! down the daemon thread; `run_server` keeps it alive for the
//! process lifetime by storing it in a function-local variable that
//! never falls out of scope (the accept loop is `loop {}` forever).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use mdns_sd::{ServiceDaemon, ServiceInfo};
use thiserror::Error;
use tracing::{debug, info, warn};

/// Service type registered with mDNS. The trailing dot is mandatory
/// per RFC 6763; mdns-sd will reject a string without it.
pub const SERVICE_TYPE: &str = "_kmwarp._tcp.local.";

/// Wire protocol version advertised in the TXT record. Bumped when
/// the framing or message catalogue changes incompatibly. Clients
/// MAY refuse to connect to a peer whose `proto` differs from their
/// own; v1.1 ignores this and connects optimistically.
pub const PROTO_VERSION: &str = "1";

/// Errors raised while bringing up the mDNS advertisement.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// `ServiceDaemon::new` failed (likely a port 5353 conflict or
    /// permissions issue inside a tightly-sandboxed environment).
    #[error("failed to create mDNS daemon: {0}")]
    Daemon(String),

    /// `ServiceInfo::new` rejected the parameters (malformed type,
    /// empty IP list, etc.).
    #[error("failed to build mDNS service info: {0}")]
    Info(String),

    /// `ServiceDaemon::register` rejected the service info.
    #[error("failed to register mDNS service: {0}")]
    Register(String),
}

/// Live registration handle. Drop = unregister + daemon shutdown.
///
/// Held by `app::run_server` in a function-local variable so the
/// daemon survives every accept iteration.
pub struct ServiceAdvertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl ServiceAdvertisement {
    /// The full instance name (e.g. `kmwarp-server-foo._kmwarp._tcp.local.`)
    /// the daemon registered. Logged at startup so the operator can
    /// confirm what the LAN will see.
    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

impl Drop for ServiceAdvertisement {
    fn drop(&mut self) {
        // Best-effort: unregister cleanly and shut down the daemon
        // thread. The unregister send returns a Receiver we don't
        // wait on — process is exiting anyway.
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// Register `_kmwarp._tcp.local.` advertising `bind`.
///
/// * `instance_name` becomes the user-visible service name (truncated
///   to 63 chars by the daemon if longer); the operator's `KMWARP_PEER_NAME`
///   feeds in here so multiple Macs on one LAN don't collide.
/// * `bind` is the listener's already-resolved local address. The
///   port is what the client will dial. The IP component is mostly
///   ignored — we always advertise on every detected IPv4 interface
///   below — unless the operator deliberately bound `127.0.0.1`, in
///   which case we honor that and skip auto-detection.
///
/// Returns `Err` only on hard daemon/setup failures. The caller
/// (`run_server`) should warn-and-continue on `Err` — see module
/// docs for the rationale.
pub fn register(
    instance_name: &str,
    bind: SocketAddr,
) -> Result<ServiceAdvertisement, DiscoveryError> {
    let daemon = ServiceDaemon::new().map_err(|e| DiscoveryError::Daemon(e.to_string()))?;

    // Build the address list. If the operator deliberately bound to
    // a single non-wildcard address, honor it — they probably know
    // something we don't (e.g. a multi-homed box where only one
    // interface should be reachable). Otherwise enumerate all
    // non-loopback IPv4 interfaces.
    let addrs: Vec<Ipv4Addr> = match bind.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_loopback() => vec![ip],
        IpAddr::V4(ip) if ip.is_loopback() => vec![ip],
        _ => detect_local_ipv4_addrs(),
    };
    if addrs.is_empty() {
        warn!("mDNS: no local IPv4 addresses detected; skipping registration");
        return Err(DiscoveryError::Info(
            "no local IPv4 addresses available".into(),
        ));
    }

    // RFC 6763 TXT records — kept tiny (each pair is one DNS string
    // in the mDNS packet). `version` lets future clients spot
    // protocol breaks; `proto` is a parking spot for future
    // transport variants (QUIC, etc.).
    let mut txt = HashMap::new();
    txt.insert("version".to_string(), PROTO_VERSION.to_string());
    txt.insert("proto".to_string(), "tcp".to_string());

    // Host name passed to ServiceInfo::new must end in `.local.`;
    // base it on the instance name with non-DNS-safe chars stripped
    // so two Macs with the same KMWARP_PEER_NAME don't collide on
    // the host name component either.
    let host_name = format!("{}.local.", sanitize_dns_label(instance_name));

    let addr_strs: Vec<String> = addrs.iter().map(|ip| ip.to_string()).collect();
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        instance_name,
        &host_name,
        &addr_strs.join(",")[..],
        bind.port(),
        Some(txt),
    )
    .map_err(|e| DiscoveryError::Info(e.to_string()))?;

    let fullname = info.get_fullname().to_string();

    daemon
        .register(info)
        .map_err(|e| DiscoveryError::Register(e.to_string()))?;

    info!(
        service_type = SERVICE_TYPE,
        fullname = %fullname,
        host = %host_name,
        port = bind.port(),
        addrs = ?addrs,
        "registered mDNS service"
    );

    Ok(ServiceAdvertisement { daemon, fullname })
}

/// Strip a DNS label down to `[A-Za-z0-9-]`. mdns-sd will reject host
/// names with `.` or spaces; the operator's `KMWARP_PEER_NAME` could
/// be anything (defaults to `kmwarp-server`).
fn sanitize_dns_label(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Empty / all-stripped — fall back to a static name so we never
    // register a zero-length label.
    if cleaned.is_empty() {
        "kmwarp-host".to_string()
    } else {
        cleaned
    }
}

/// Enumerate non-loopback IPv4 interface addresses. We don't pull in
/// `pnet`/`if-addrs`/`netdev` — for v1.1 a quick `UdpSocket::connect`
/// trick is enough: ask the kernel which IP it would use to reach a
/// public-ish address, and use that. mDNS only needs one address per
/// interface and one interface is the common case; multi-homed
/// support is best-effort and clients will see whichever IPs the
/// daemon's own multicast announcements pick up too.
fn detect_local_ipv4_addrs() -> Vec<Ipv4Addr> {
    use std::net::UdpSocket;

    let mut out = Vec::new();
    // 198.51.100.1 is RFC 5737 TEST-NET-2 — guaranteed unroutable, so
    // we never accidentally send anything. We just want to ask the
    // kernel which interface it would prefer.
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("198.51.100.1:1").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                if let IpAddr::V4(v4) = addr.ip() {
                    if !v4.is_unspecified() && !v4.is_loopback() {
                        out.push(v4);
                    }
                }
            }
        }
    }
    if out.is_empty() {
        debug!("detect_local_ipv4_addrs: no non-loopback IPv4 detected via probe");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_dns_label_keeps_alphanumeric_and_dash() {
        assert_eq!(sanitize_dns_label("kmwarp-server"), "kmwarp-server");
        assert_eq!(sanitize_dns_label("MyMac01"), "MyMac01");
    }

    #[test]
    fn sanitize_dns_label_replaces_unsafe_chars() {
        assert_eq!(sanitize_dns_label("My Mac.local"), "My-Mac-local");
        assert_eq!(sanitize_dns_label("kmwarp_server"), "kmwarp-server");
    }

    #[test]
    fn sanitize_dns_label_falls_back_when_empty() {
        assert_eq!(sanitize_dns_label(""), "kmwarp-host");
        assert_eq!(sanitize_dns_label("..."), "---");
    }

    #[test]
    fn service_type_ends_in_local_dot() {
        assert!(SERVICE_TYPE.ends_with("._tcp.local."));
    }
}
