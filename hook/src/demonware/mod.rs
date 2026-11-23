//! A local stand-in for Activision's Demonware online service, for games that link its
//! `bd*` library (statically: there is no DLL to hook, only the hosts it reaches).
//!
//! In a game started through PeerParty, lookups of `*.demonware.net` resolve to this
//! process's loopback address (127.80.x.y, the PlayFab service's: playfab/mod.rs), where
//! PeerParty answers:
//!
//! - auth.rs: the JSON sign-in (`<title>-pc-auth3.prod.demonware.net`, HTTPS on 443,
//!   through the PlayFab service's HTTPS server and certificate). It hands out a ticket
//!   and session key of its own; the game's Steam encrypted app ticket comes from
//!   steamlan.rs, which answers it while this service is in use.
//! - lobby.rs: the lobby service (`<title>-pc-lobby.prod.demonware.net`, TCP 3074): its
//!   handshake, the keys it derives from the session key, and the encrypted task calls,
//!   each logged by service and task.
//! - mm.rs: matchmaking sessions, shared with the other copies through LAN presence.
//! - stun.rs: Demonware's own address and NAT discovery (`stun.*.demonware.net`, UDP
//!   3074), which reports this PC's LAN address.
//! - schannel.rs: a game whose HTTPS is SChannel with automatic certificate checks
//!   (libcurl's) connects to PeerParty's server for Demonware hosts with a manual-check
//!   credential instead; nothing else changes.
//!
//! References: the wire formats as an open-source community client's Demonware
//! service documents them (read for the protocol only, no code taken), checked against
//! what a game's own client sends and expects.
//!
//! On for every game started through PeerParty that looks up a Demonware host.
//! `PEERPARTY_DEMONWARE=0` or `PEERPARTY_SKIP=demonware` turns it off; the mode
//! "demonware" set to official leaves the real servers.

pub mod auth;
pub mod crypto;
pub mod lobby;
pub mod mm;
pub mod schannel;
pub mod stun;

use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, UdpSocket};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::log;

pub const HOST_SUFFIX: &str = ".demonware.net";

/// Demonware's lobby and STUN port (TCP and UDP).
pub const PORT: u16 = 3074;

/// Whether `host` is one of Demonware's (port and case aside).
pub fn is_host(host: &str) -> bool {
    let h = host.split(':').next().unwrap_or_default().trim_end_matches('.').to_ascii_lowercase();
    h.ends_with(HOST_SUFFIX)
}

/// Demonware's sign-in hosts on the local HTTPS server.
pub static HTTPS: crate::https::HttpsService = crate::https::HttpsService {
    name: "demonware",
    cert_names: || vec!["*.prod.demonware.net", "*.demonware.net"],
    is_host,
    handle: auth::handle,
    upgrade: None,
    reads_root_early: || false,
};

/// Lookups of Demonware's hosts.
pub static DNS: crate::net::dns::DnsService = crate::net::dns::DnsService { layer: "demonware", is_host, redirect };

/// Whether the stand-in is on for this game (see the module comment).
pub fn wanted() -> bool {
    crate::net::enabled()
        && !crate::net::skip("demonware")
        && crate::config::HookConfig::global().demonware != Some(false)
        && !crate::config::modes::official("demonware")
}

static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether the game has reached for Demonware and been sent here: then its Steam
/// encrypted app ticket is PeerParty's too (steamlan.rs), for auth.rs to read.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

#[cfg(test)]
pub fn set_active() {
    ACTIVE.store(true, Ordering::Relaxed);
}

/// The address a lookup of `host` resolves to instead, or None to leave it alone.
pub fn redirect(host: &str) -> Option<String> {
    if !is_host(host) || !wanted() {
        return None;
    }
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    let ip = crate::https::server()?;
    start(ip);
    ACTIVE.store(true, Ordering::Relaxed);
    crate::log::throttled(&format!("demonware redirect {h}"), 2, |n| {
        log!("demonware {h} -> {ip} (PeerParty's Demonware service) n={n}")
    });
    Some(ip.to_string())
}

/// Starts the lobby (TCP) and STUN (UDP) servers on `ip` (once).
fn start(ip: Ipv4Addr) {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        match TcpListener::bind(SocketAddrV4::new(ip, PORT)) {
            Ok(listener) => {
                log!("demonware lobby service on {ip}:{PORT} (tcp)");
                let _ = std::thread::Builder::new().name("peerparty-demonware-lobby".into()).spawn(move || {
                    for conn in listener.incoming().flatten() {
                        let _ = std::thread::Builder::new()
                            .name("peerparty-demonware-conn".into())
                            .spawn(move || lobby::serve(conn));
                    }
                });
            }
            Err(e) => log!("demonware lobby service could not listen on {ip}:{PORT}: {e}"),
        }
        match UdpSocket::bind(SocketAddrV4::new(ip, PORT)) {
            Ok(sock) => {
                log!("demonware stun service on {ip}:{PORT} (udp)");
                let _ = std::thread::Builder::new()
                    .name("peerparty-demonware-stun".into())
                    .spawn(move || stun::serve(sock));
            }
            Err(e) => log!("demonware stun service could not listen on {ip}:{PORT}: {e}"),
        }
    });
}

/// This PC's address on the LAN (the one its route out uses), for STUN's answers.
pub fn lan_address() -> Ipv4Addr {
    static ADDR: OnceLock<Ipv4Addr> = OnceLock::new();
    *ADDR.get_or_init(|| {
        // Connecting a UDP socket sends nothing; it only picks the route and source.
        let found = UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| s.connect("192.0.2.1:9").map(|_| s))
            .and_then(|s| s.local_addr())
            .ok()
            .and_then(|a| match a.ip() {
                std::net::IpAddr::V4(v4) if !v4.is_unspecified() => Some(v4),
                _ => None,
            });
        found.unwrap_or(Ipv4Addr::LOCALHOST)
    })
}

/// Bytes as hex, for the log.
pub(crate) fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demonware_hosts_are_recognised() {
        assert!(is_host("title-pc-auth3.prod.demonware.net"));
        assert!(is_host("stun.us.demonware.net:3074"));
        assert!(is_host("TITLE-PC-LOBBY.PROD.DEMONWARE.NET."));
        assert!(!is_host("demonware.net.example.com"));
        assert!(!is_host("example.com"));
    }
}
