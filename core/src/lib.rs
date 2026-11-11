//! PeerParty's shared LAN core. SDK front ends (EOS now; Steamworks and PlayFab later)
//! translate their APIs onto these primitives:
//!
//! - identity: a stable per-slot `PeerId` and a display name
//! - discovery and connections: UDP beacons, then one TCP connection per peer pair
//! - lobbies: owner-authoritative, with attributes, member attributes and search
//! - channels: per (peer, socket name) connections carrying packets
//! - internet play (opt-in): room codes, the public DHT, NAT punching and an encrypted
//!   tunnel that carries the mesh (`inet`)
//!
//! Nothing here knows about any SDK.

#[cfg(test)]
mod fuzz_tests;
pub mod fwd;
mod id;
pub mod inet;
mod lan;
mod lobby;
mod mesh;
pub mod own;
pub mod release;
mod wire;

pub use id::{Identity, PeerId};
pub use lan::{CloseReason, Event, Lan, LobbyError, MemberStatus, Options, Packet, SendResult, TestIntervals, TestTiming, Trust, choose_internet, internet_wanted};
pub use lobby::{
    Attr, Filter, Lobby, Member, Op, Permission, Query, Value, find_attr, remove_attr, set_attr,
};
pub use mesh::DISCOVERY_PORT;
pub use wire::JoinResult;

/// `PEERPARTY_LOOPBACK=1`: sockets bind to 127.0.0.1 and no beacons are broadcast, so
/// nothing listens on the network (the tests: no Windows Firewall prompts). Copies on one
/// PC still meet through the peer board.
pub fn loopback_only() -> bool {
    std::env::var("PEERPARTY_LOOPBACK").is_ok_and(|v| v == "1")
}

/// A discovery beacon as a copy of `game` with `id` listening on TCP `port` sends it
/// (tests: a peer that answers beacons but can't be reached).
#[doc(hidden)]
pub fn test_beacon(game: &str, id: PeerId, port: u16) -> Vec<u8> {
    wire::Beacon { game: game.into(), id, port }.encode()
}

/// The address PeerParty's sockets bind to: every interface, or loopback only.
pub fn bind_ip() -> std::net::Ipv4Addr {
    if loopback_only() { std::net::Ipv4Addr::LOCALHOST } else { std::net::Ipv4Addr::UNSPECIFIED }
}
