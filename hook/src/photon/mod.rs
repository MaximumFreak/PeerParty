//! Photon Realtime / PUN on the LAN: PeerParty hosts Photon's name, master and game
//! servers itself, and the game's own Photon client talks to them.
//!
//! On in every game started through PeerParty. Lookups of Photon Cloud's hosts (`ns.photonengine.io`,
//! `ns.exitgames.com`, `*.exitgames.com`, `*.exitgamescloud.com`, `*.photonengine.io`,
//! `*.photonengine.cn`) resolve to one copy of PeerParty on the LAN that runs the
//! servers on Photon's usual UDP ports (5058 name server, 5055 master, 5056 game server,
//! and the 27000-27002 "alternative" ports). The first copy that needs Photon looks
//! for a peer that already hosts (LAN presence `photon=host`, lowest peer id wins);
//! if there is none after a short wait, it hosts. Every other copy's lookups resolve
//! to that peer's LAN address, so all copies meet in the same rooms.
//!
//! Over the internet (a public session or an internet room, with the typed-IP forwarder
//! on) a host that isn't on the LAN is reached at its stand-in (198.18.x): the forwarder
//! carries Photon's UDP, TCP and WebSocket traffic through the encrypted tunnel, and the
//! host gives such clients its own stand-in as the master and game server address. A LAN
//! host still wins over an internet one.
//!
//! Ported from Luxon Server (github.com/niansa/LuxonServer, commit 147e5e5) and its
//! Luxon library (gitlab.com/luxon_project/Luxon, commit 809c9ce), both BSD-3-Clause
//! (licence in LICENSE-Luxon): ENet over UDP, GpBinary v1.6 and v1.8, payload
//! encryption (DH key exchange, or the AuthOnce secret), authentication with tokens,
//! regions, lobbies, matchmaking and rooms, properties, events and the event cache.
//! Checked against the behaviour of Photon's own .NET client (Photon3Unity3D /
//! PhotonRealtime).
//! Photon Voice rides on the same rooms (it is a Realtime client with its own app id).
//! Photon Chat is served too (chat.rs): channels, history, private messages, friends.
//! Photon Bolt rides plain Realtime rooms (its relay is event 150 to one actor), so it
//! needs nothing more. Fusion's Host / Server / Client modes get a stand-in for Photon's
//! Fusion plugin (fusion.rs), and Shared mode a server of its own (shared.rs). An
//! overview is in docs/design.md.
//!
//! `PEERPARTY_PHOTON=0` (or `PEERPARTY_SKIP=photon`) leaves Photon alone.

// The protocol modules are complete codecs and both ends of ENet: the client half,
// the plain value writers and the status getters are used by the tests only.
/// Photon Chat, served by the master server.
pub mod chat;
#[cfg_attr(not(test), allow(dead_code))]
pub mod crypto;
#[cfg_attr(not(test), allow(dead_code))]
pub mod enet;
#[cfg_attr(not(test), allow(dead_code))]
pub mod fusion;
#[cfg_attr(not(test), allow(dead_code))]
pub mod gp16;
#[cfg_attr(not(test), allow(dead_code))]
pub mod gp18;
#[cfg_attr(not(test), allow(dead_code))]
pub mod host;
#[cfg_attr(not(test), allow(dead_code))]
pub mod message;
#[cfg_attr(not(test), allow(dead_code))]
pub mod server;
/// SQL lobby filters.
pub mod sql;
/// Fusion's Shared mode server (actor 0 in a Shared room).
pub mod shared;
#[cfg_attr(not(test), allow(dead_code))]
pub mod value;
/// Photon's WebSocket transport (host.rs serves it).
pub mod ws;
#[cfg(test)]
mod loopback;
#[cfg(test)]
mod tests;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use peerparty_core::PeerId;

use crate::log;

/// The presence key a hosting copy sets.
const PRESENCE: &str = "photon";
/// How long the first Photon lookup waits for the LAN to show an existing host.
const DISCOVERY_WAIT: Duration = Duration::from_secs(3);

/// Whether `host` is one of Photon Cloud's (name server, regional masters and game
/// servers, and the China cloud).
pub fn is_photon_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    ["exitgames.com", "exitgamescloud.com", "photonengine.io", "photonengine.cn"]
        .iter()
        .any(|d| h == *d || h.ends_with(&format!(".{d}")))
}

fn wanted() -> bool {
    crate::net::enabled()
        && !crate::net::skip("photon")
        && std::env::var("PEERPARTY_PHOTON").ok().as_deref() != Some("0")
        && !crate::modes::official("photon")
}

/// Whether this game has looked up Photon (it joins rooms by code).
static USED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn in_use() -> bool {
    USED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The room this PC's own game is in on PeerParty's Photon servers, and its actor
/// number there (None when it's in none). Photon has no invites: the overlay says how
/// players join it instead.
static LOCAL_ROOM: Mutex<Option<(String, i32)>> = Mutex::new(None);

/// The room this PC's game is in on PeerParty's Photon servers, if any.
pub fn local_room() -> Option<String> {
    LOCAL_ROOM.lock().unwrap().as_ref().map(|(r, _)| r.clone())
}

/// A client from this PC (loopback) joined `room` as actor `nr` (server.rs).
pub(crate) fn joined_locally(room: &str, nr: i32) {
    *LOCAL_ROOM.lock().unwrap() = Some((room.to_string(), nr));
}

/// Actor `nr` left `room` (server.rs): if it was this PC's game, it's in no room now.
pub(crate) fn left(room: &str, nr: i32) {
    let mut r = LOCAL_ROOM.lock().unwrap();
    if r.as_ref().is_some_and(|(name, actor)| name == room && *actor == nr) {
        *r = None;
    }
}

/// A room with players on this PC's Photon servers: where its players connect from.
#[derive(Clone)]
struct RoomSnap {
    code: String,
    addrs: Vec<SocketAddr>,
    max: u8,
}

static ROOMS: Mutex<Vec<RoomSnap>> = Mutex::new(Vec::new());

/// The presence key that tells the other copies who is in the room we host
/// (`code|max|peer,peer`), so a player who joined from another PC sees its room too.
const ROOM_PRESENCE: &str = "photon_room";

/// A room the player is in, on whichever PC hosts Photon.
#[derive(Clone, Debug, PartialEq)]
pub struct RoomView {
    pub code: String,
    /// 0 = no limit.
    pub max: u32,
    pub host: PeerId,
    pub members: Vec<PeerId>,
}

/// The player behind a client address on our servers: this PC (loopback), an internet
/// player (through the forwarder), or a LAN peer by its address.
fn peer_at(lan: &peerparty_core::Lan, addr: SocketAddr) -> Option<PeerId> {
    if addr.ip().is_loopback() {
        return Some(lan.me());
    }
    if let Some(p) = crate::ipfwd::forwarded_peer(addr) {
        return Some(p);
    }
    lan.peers().into_iter().map(|(p, _)| p).find(|p| lan.peer_has_ip(*p, addr.ip()))
}

/// Who is in a room of ours now (server.rs). Empty `addrs`: the room is gone.
pub(crate) fn room_changed(code: &str, addrs: Vec<SocketAddr>, max: u8) {
    {
        let mut r = ROOMS.lock().unwrap();
        r.retain(|x| x.code != code);
        if !addrs.is_empty() {
            r.push(RoomSnap { code: code.to_string(), addrs, max });
        }
    }
    publish_room();
}

fn members_of(lan: &peerparty_core::Lan, r: &RoomSnap) -> Vec<PeerId> {
    let mut m: Vec<PeerId> = Vec::new();
    for a in &r.addrs {
        if let Some(p) = peer_at(lan, *a)
            && !m.contains(&p)
        {
            m.push(p);
        }
    }
    m
}

/// The room to show: ours with this PC's game in it, else any of ours.
fn hosted_room(lan: &peerparty_core::Lan) -> Option<(RoomSnap, Vec<PeerId>)> {
    let rooms = ROOMS.lock().unwrap().clone();
    let all: Vec<(RoomSnap, Vec<PeerId>)> = rooms.into_iter().map(|r| { let m = members_of(lan, &r); (r, m) }).collect();
    let me = lan.me();
    all.iter().find(|(_, m)| m.contains(&me)).or_else(|| all.first()).cloned()
}

fn publish_room() {
    let Some(lan) = crate::net::lan_if_started() else { return };
    let text = hosted_room(lan).map(|(r, m)| {
        let ids: Vec<String> = m.iter().map(|p| p.hex()).collect();
        format!("{}|{}|{}", r.code, r.max, ids.join(","))
    });
    crate::net::set_presence("photon_room", text.map(|t| vec![(ROOM_PRESENCE.to_string(), t)]).unwrap_or_default());
}

/// The Photon room this PC's game is in, if it is in one: hosted here, or on the
/// PC that hosts (which says who is in its room). Never starts the LAN.
pub fn room_view() -> Option<RoomView> {
    let lan = crate::net::lan_if_started()?;
    let me = lan.me();
    if let Some((r, members)) = hosted_room(lan)
        && members.contains(&me)
    {
        return Some(RoomView { code: r.code, max: r.max as u32, host: me, members });
    }
    for (peer, _) in lan.peers() {
        let Some((_, v)) = lan.presence(peer).into_iter().find(|(k, _)| k == ROOM_PRESENCE) else { continue };
        let mut it = v.splitn(3, '|');
        let (Some(code), Some(max), Some(ids)) = (it.next(), it.next(), it.next()) else { continue };
        let members: Vec<PeerId> = ids.split(',').filter_map(PeerId::from_hex).collect();
        if members.contains(&me) {
            return Some(RoomView { code: code.to_string(), max: max.parse().unwrap_or(0), host: peer, members });
        }
    }
    None
}

#[derive(Default)]
struct State {
    /// Our own servers, once started.
    host: Option<host::Host>,
    /// Starting them failed; don't retry on every lookup.
    failed: bool,
    /// The first lookup already waited for discovery.
    waited: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    host: None,
    failed: false,
    waited: false,
});

/// The address a lookup of `host` should resolve to instead, as text, or None to
/// leave the lookup alone.
pub fn redirect(host: &str) -> Option<String> {
    if !is_photon_host(host) || !wanted() {
        return None;
    }
    let (ip, whose) = target()?;
    USED.store(true, std::sync::atomic::Ordering::Relaxed);
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    crate::log::throttled(&format!("photon redirect {h} {ip}"), 2, |n| {
        log!("photon {h} -> {ip} ({whose}) n={n}")
    });
    Some(ip.to_string())
}

/// A peer that says it hosts Photon.
#[derive(Clone, Debug)]
struct Candidate {
    id: PeerId,
    name: String,
    /// Its LAN address, when it's on the LAN.
    direct: Option<IpAddr>,
    /// The address games are given for it otherwise (its stand-in, for an internet peer).
    shown: Option<IpAddr>,
}

/// The host to use: a LAN host if there is one, else (only while the typed-IP forwarder
/// runs, which is what makes a stand-in lead somewhere) an internet host at its stand-in;
/// the lowest peer id within each.
fn pick_host(hosts: Vec<Candidate>, forwarding: bool) -> Option<(PeerId, IpAddr, String)> {
    let lan = hosts.iter().filter_map(|c| c.direct.map(|ip| (c.id, ip, c.name.clone()))).min_by_key(|h| h.0);
    lan.or_else(|| {
        if !forwarding {
            return None;
        }
        hosts
            .iter()
            .filter(|c| c.direct.is_none())
            .filter_map(|c| match c.shown {
                Some(IpAddr::V4(v4)) if peerparty_core::fwd::is_stand_in(v4) => Some((c.id, IpAddr::V4(v4), c.name.clone())),
                _ => None,
            })
            .min_by_key(|h| h.0)
    })
}

/// A peer that hosts Photon, as `pick_host` chooses among those that say so.
fn peer_host() -> Option<(PeerId, IpAddr, String)> {
    let lan = crate::net::lan()?;
    let hosts = lan
        .peers()
        .into_iter()
        .filter(|(id, _)| lan.presence(*id).iter().any(|(k, v)| k == PRESENCE && v == "host"))
        .map(|(id, name)| {
            let direct = if lan.peer_trust(id) == Some(peerparty_core::Trust::Lan) { lan.peer_direct_ip(id) } else { None };
            Candidate { id, name, direct, shown: lan.peer_ip(id) }
        })
        .collect();
    pick_host(hosts, crate::ipfwd::active())
}

fn target() -> Option<(IpAddr, String)> {
    let mut st = STATE.lock().unwrap();
    let lan = crate::net::lan();
    if !st.waited {
        st.waited = true;
        if lan.is_some() {
            let until = Instant::now() + DISCOVERY_WAIT;
            while Instant::now() < until && peer_host().is_none() {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    if let Some((id, ip, name)) = peer_host() {
        // A peer with a lower id hosts too (both started at once): its rooms win for
        // new connections. Ours keeps serving whoever is already connected.
        let lower = lan.is_some_and(|l| id < l.me());
        if st.host.is_none() || lower {
            return Some((ip, format!("Photon servers on {name}")));
        }
    }
    if st.host.is_some() {
        return Some((IpAddr::V4(Ipv4Addr::LOCALHOST), "PeerParty's Photon servers".into()));
    }
    if st.failed {
        return None;
    }
    let opts = host::Options::standard(
        IpAddr::V4(peerparty_core::bind_ip()),
        Box::new(advertise),
        Arc::new(|s: &str| log!("{s}")),
    );
    match host::Host::start(opts) {
        Ok(h) => {
            st.host = Some(h);
            crate::net::set_presence(PRESENCE, vec![(PRESENCE.into(), "host".into())]);
            log!("photon servers started; Photon lookups resolve here (LAN peers come to this PC)");
            Some((IpAddr::V4(Ipv4Addr::LOCALHOST), "PeerParty's Photon servers".into()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Another copy on this PC (or a local Photon server) has the ports.
            st.failed = true;
            log!("photon ports already in use on this PC ({e}); using the servers already there");
            Some((IpAddr::V4(Ipv4Addr::LOCALHOST), "Photon servers already on this PC".into()))
        }
        Err(e) => {
            st.failed = true;
            log!("photon servers could not start: {e}; Photon lookups are left alone");
            None
        }
    }
}

/// The address of this PC that a client at `client` reaches.
fn advertise(client: SocketAddr) -> String {
    let me = crate::net::lan().map(|l| l.me());
    advertise_for(client, crate::ipfwd::forwarded_peer(client).and(me))
}

/// `forwarded_for` is our own id when the client is an internet peer coming through the
/// forwarder: it reaches us at our stand-in, the same address on every PC. Otherwise
/// loopback for this PC, else the local address the OS routes to the client on (no
/// packet is sent).
fn advertise_for(client: SocketAddr, forwarded_for: Option<PeerId>) -> String {
    if let Some(me) = forwarded_for {
        return peerparty_core::fwd::Forwarder::address_of(me).to_string();
    }
    let client = client.ip();
    if client.is_loopback() {
        return "127.0.0.1".into();
    }
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|s| {
            s.connect(SocketAddr::new(client, server::PORTS[0][1]))?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

#[cfg(test)]
mod glue_tests {
    use super::*;

    #[test]
    fn photon_hosts_are_recognised() {
        for h in ["ns.photonengine.io", "NS.EXITGAMES.COM", "ns.exitgames.com.", "app-eu.exitgamescloud.com", "ns.photonengine.cn"] {
            assert!(is_photon_host(h), "{h}");
        }
        for h in ["photonengine.io.evil.com", "example.com", "notexitgames.com", "steamcommunity.com"] {
            assert!(!is_photon_host(h), "{h}");
        }
    }

    #[test]
    fn loopback_clients_get_loopback() {
        assert_eq!(advertise((Ipv4Addr::LOCALHOST, 5055).into()), "127.0.0.1");
    }

    #[test]
    fn internet_clients_get_our_stand_in() {
        let me = PeerId::random();
        let shown = advertise_for((Ipv4Addr::LOCALHOST, 50000).into(), Some(me));
        assert_eq!(shown, peerparty_core::fwd::Forwarder::address_of(me).to_string());
        assert!(peerparty_core::fwd::is_stand_in(shown.parse().unwrap()));
        assert_eq!(advertise_for((Ipv4Addr::LOCALHOST, 50000).into(), None), "127.0.0.1");
    }

    #[test]
    fn hosts_on_the_lan_win_then_internet_ones_at_their_stand_in() {
        let mut ids = [PeerId::random(), PeerId::random(), PeerId::random()];
        ids.sort();
        let at = |id| IpAddr::V4(peerparty_core::fwd::Forwarder::address_of(id));
        let lan_host = Candidate { id: ids[2], name: "lan".into(), direct: Some("192.168.1.9".parse().unwrap()), shown: Some("192.168.1.9".parse().unwrap()) };
        let far = |id, name: &str| Candidate { id, name: name.into(), direct: None, shown: Some(at(id)) };
        let hosts = vec![far(ids[0], "far0"), far(ids[1], "far1"), lan_host.clone()];
        assert_eq!(pick_host(hosts.clone(), true).map(|h| h.2), Some("lan".into()));
        let only_far = vec![far(ids[1], "far1"), far(ids[0], "far0")];
        assert_eq!(pick_host(only_far.clone(), true), Some((ids[0], at(ids[0]), "far0".into())));
        assert_eq!(pick_host(only_far, false), None);
        // A peer whose address isn't a stand-in (nothing forwards to it) is never picked.
        let odd = Candidate { id: ids[0], name: "odd".into(), direct: None, shown: Some("203.0.113.5".parse().unwrap()) };
        assert_eq!(pick_host(vec![odd], true), None);
    }
}
