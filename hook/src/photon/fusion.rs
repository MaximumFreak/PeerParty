//! Photon Fusion's cloud plugin ("FusionPlugin"), the part Host / Server / Client games
//! need. A Fusion room asks Photon Cloud for the plugin; the plugin is actor 0 in the
//! room and speaks Fusion's protocol messages in event 100 (Fusion.Protocol, read from
//! the traffic of Fusion.Sockets / Fusion.Runtime 2.x clients):
//!
//! - every peer sends `Join` (request: game mode, Server or Client, flags); the plugin
//!   confirms it with a unique id and asks for the host's network config and every
//!   peer's addresses (`ReflexiveInfo`),
//! - `Start` tells the host to start and each client which actor is the host,
//! - each side's `ReflexiveInfo` (public and private address, from STUN) goes to the
//!   other, so a client connects straight to the host's LAN address, or else relays
//!   through the room (event 101, which the game server forwards like any event),
//! - the host's host-migration snapshots are confirmed (they're not kept: a host that
//!   leaves ends the session, as Fusion does without migration).
//!
//! Shared mode: there the plugin itself is the game's server. Every Shared peer is
//! confirmed (the first one asked for its network config, for the tick rate) and told
//! to `Start` with `ConnectToShared`; its Fusion connection then goes to actor 0 through
//! the room (event 101), where shared.rs serves it.
//!
//! The messages are a little-endian bit stream (Fusion's BitStream): values are written
//! low bit first at any bit offset, a string is a "null" bit then a 16-bit length and
//! UTF-8, a byte array a "present" bit then a 16-bit length and the bytes.

use std::collections::HashMap;

/// Event code of protocol messages; data packets are 101 and need nothing from us.
pub const PROTOCOL_EVENT: u8 = 100;
pub const PLUGIN_NAME: &str = "FusionPlugin";

#[derive(Default)]
pub struct BitWriter {
    pub data: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    /// Bits written so far.
    pub fn bit_len(&self) -> usize {
        self.bits
    }
    pub fn bits(&mut self, value: u64, n: usize) {
        for i in 0..n {
            if self.bits % 8 == 0 {
                self.data.push(0);
            }
            if value >> i & 1 != 0 {
                *self.data.last_mut().unwrap() |= 1 << (self.bits % 8);
            }
            self.bits += 1;
        }
    }
    pub fn u8(&mut self, v: u8) {
        self.bits(v as u64, 8);
    }
    pub fn u16(&mut self, v: u16) {
        self.bits(v as u64, 16);
    }
    pub fn u32(&mut self, v: u32) {
        self.bits(v as u64, 32);
    }
    pub fn u64(&mut self, v: u64) {
        self.bits(v, 64);
    }
    pub fn bool(&mut self, v: bool) {
        self.bits(v as u64, 1);
    }
    pub fn bytes(&mut self, b: &[u8]) {
        for x in b {
            self.u8(*x);
        }
    }
    /// `Serialize(ref byte[])`: present bit, 16-bit length, bytes.
    pub fn array(&mut self, b: Option<&[u8]>) {
        self.bool(b.is_some());
        if let Some(b) = b {
            self.u16(b.len() as u16);
            self.bytes(b);
        }
    }
    /// `Serialize(ref string)`: a *null* bit, 16-bit length, UTF-8.
    pub fn string(&mut self, s: Option<&str>) {
        self.bool(s.is_none());
        if let Some(s) = s {
            self.u16(s.len() as u16);
            self.bytes(s.as_bytes());
        }
    }
}

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }
    pub fn left(&self) -> usize {
        self.data.len() * 8 - self.pos.min(self.data.len() * 8)
    }
    /// The bit position.
    pub fn pos(&self) -> usize {
        self.pos
    }
    pub fn seek(&mut self, pos: usize) {
        self.pos = pos;
    }
    pub fn bits(&mut self, n: usize) -> Option<u64> {
        if self.left() < n {
            return None;
        }
        let mut v = 0u64;
        for i in 0..n {
            let p = self.pos + i;
            if self.data[p / 8] >> (p % 8) & 1 != 0 {
                v |= 1 << i;
            }
        }
        self.pos += n;
        Some(v)
    }
    pub fn u8(&mut self) -> Option<u8> {
        self.bits(8).map(|v| v as u8)
    }
    pub fn u16(&mut self) -> Option<u16> {
        self.bits(16).map(|v| v as u16)
    }
    pub fn u32(&mut self) -> Option<u32> {
        self.bits(32).map(|v| v as u32)
    }
    pub fn u64(&mut self) -> Option<u64> {
        self.bits(64)
    }
    pub fn bool(&mut self) -> Option<bool> {
        self.bits(1).map(|v| v != 0)
    }
    pub fn bytes(&mut self, n: usize) -> Option<Vec<u8>> {
        (0..n).map(|_| self.u8()).collect()
    }
    pub fn array(&mut self) -> Option<Option<Vec<u8>>> {
        if !self.bool()? {
            return Some(None);
        }
        let n = self.u16()? as usize;
        self.bytes(n).map(Some)
    }
    pub fn string(&mut self) -> Option<Option<String>> {
        if self.bool()? {
            return Some(None);
        }
        let n = self.u16()? as usize;
        self.bytes(n).map(|b| Some(String::from_utf8_lossy(&b).into_owned()))
    }
}

/// The header every message starts with: protocol version (1..=6) and the Fusion
/// serialization version (major, minor, build) from version 2 on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Version {
    pub protocol: u8,
    pub fusion: (u32, u32, u32),
}

/// Message type ids (ProtocolSerializer).
pub mod kind {
    pub const JOIN: u8 = 1;
    pub const NETWORK_CONFIG: u8 = 2;
    pub const REFLEXIVE_INFO: u8 = 3;
    pub const DISCONNECT: u8 = 4;
    pub const START: u8 = 5;
    pub const SNAPSHOT: u8 = 6;
    pub const HOST_MIGRATION: u8 = 7;
}

pub mod join_requests {
    pub const NETWORK_CONFIG: u32 = 2;
    pub const REFLEXIVE_INFO: u32 = 4;
    pub const DISABLE_NAT_PUNCH: u32 = 8;
}

pub mod start_requests {
    pub const CONNECT_TO_SHARED: u32 = 2;
    pub const WAIT_FOR_REFLEXIVE_INFO: u32 = 4;
}

/// Event code of Fusion's relayed connection packets.
pub const RELAY_EVENT: u8 = 101;

/// Game modes and peer modes in Join.
pub const CLIENT_SERVER: u8 = 1;
pub const SHARED: u8 = 2;
pub const PEER_SERVER: u8 = 1;
pub const PEER_CLIENT: u8 = 2;
/// DisconnectReason.
pub const INCOMPATIBLE_CONFIGURATION: u8 = 5;
pub const SERVER_ALREADY_IN_ROOM: u8 = 6;

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Join { kind: u8, game_mode: u8, peer_mode: u8, requests: u32, unique_id: Option<Vec<u8>> },
    NetworkConfig { kind: u8, config: Option<String> },
    ReflexiveInfo { actor: i32, public: [u64; 3], private: [u64; 3], nat: u8, unique_id: Option<Vec<u8>> },
    Disconnect { reason: u8 },
    Start { remote_server: i32, requests: u32 },
    Snapshot { tick: i32, network_id: u32, kind: u8, size: i32, crc: u64, data: Option<Vec<u8>> },
    HostMigration { peer_mode: u8 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub version: Version,
    pub body: Body,
    pub custom: Option<String>,
}

impl Message {
    pub fn new(version: Version, body: Body) -> Message {
        Message { version, body, custom: Some(String::new()) }
    }

    fn type_id(&self) -> u8 {
        match self.body {
            Body::Join { .. } => kind::JOIN,
            Body::NetworkConfig { .. } => kind::NETWORK_CONFIG,
            Body::ReflexiveInfo { .. } => kind::REFLEXIVE_INFO,
            Body::Disconnect { .. } => kind::DISCONNECT,
            Body::Start { .. } => kind::START,
            Body::Snapshot { .. } => kind::SNAPSHOT,
            Body::HostMigration { .. } => kind::HOST_MIGRATION,
        }
    }

    pub fn write(&self, w: &mut BitWriter) {
        let v = self.version.protocol.min(6);
        w.u8(self.type_id());
        w.u8(v);
        if v >= 2 {
            let (a, b, c) = self.version.fusion;
            w.u32(a);
            w.u32(b);
            w.u32(c);
        }
        match &self.body {
            Body::Join { kind, game_mode, peer_mode, requests, unique_id } => {
                w.u8(*kind);
                w.u8(*game_mode);
                w.u8(*peer_mode);
                w.u32(*requests);
                if v >= 6 {
                    w.array(unique_id.as_deref());
                }
            }
            Body::NetworkConfig { kind, config } => {
                w.u8(*kind);
                w.string(config.as_deref());
            }
            Body::ReflexiveInfo { actor, public, private, nat, unique_id } => {
                w.u32(*actor as u32);
                for b in public.iter().chain(private) {
                    w.u64(*b);
                }
                if v >= 4 {
                    w.u8(*nat);
                }
                if v >= 6 {
                    w.array(unique_id.as_deref());
                }
            }
            Body::Disconnect { reason } => {
                if v >= 2 {
                    w.u8(*reason);
                } else {
                    w.string(Some(DISCONNECT_NAMES.get(*reason as usize).copied().unwrap_or("Error")));
                }
            }
            Body::Start { remote_server, requests } => {
                w.u32(*remote_server as u32);
                w.u32(*requests);
            }
            Body::Snapshot { tick, network_id, kind, size, crc, data } => {
                w.u32(*tick as u32);
                w.u32(*network_id);
                w.u8(*kind);
                w.u32(*size as u32);
                w.u64(*crc);
                w.u32(data.as_ref().map_or(0, |d| d.len()) as u32);
                w.array(data.as_deref());
            }
            Body::HostMigration { peer_mode } => w.u8(*peer_mode),
        }
        if v >= 3 {
            w.string(self.custom.as_deref());
        }
    }

    fn read(r: &mut BitReader) -> Option<Message> {
        let id = r.u8()?;
        let protocol = r.u8()?.min(6);
        let fusion = if protocol >= 2 { (r.u32()?, r.u32()?, r.u32()?) } else { (0, 0, 0) };
        let v = protocol;
        let body = match id {
            kind::JOIN => Body::Join {
                kind: r.u8()?,
                game_mode: r.u8()?,
                peer_mode: r.u8()?,
                requests: r.u32()?,
                unique_id: if v >= 6 { r.array()? } else { None },
            },
            kind::NETWORK_CONFIG => Body::NetworkConfig { kind: r.u8()?, config: r.string()? },
            kind::REFLEXIVE_INFO => {
                let actor = r.u32()? as i32;
                let public = [r.u64()?, r.u64()?, r.u64()?];
                let private = [r.u64()?, r.u64()?, r.u64()?];
                let nat = if v >= 4 { r.u8()? } else { 4 };
                let unique_id = if v >= 6 { r.array()? } else { None };
                Body::ReflexiveInfo { actor, public, private, nat, unique_id }
            }
            kind::DISCONNECT => {
                if v >= 2 {
                    Body::Disconnect { reason: r.u8()? }
                } else {
                    let s = r.string()?.unwrap_or_default();
                    Body::Disconnect { reason: DISCONNECT_NAMES.iter().position(|n| *n == s).unwrap_or(7) as u8 }
                }
            }
            kind::START => Body::Start { remote_server: r.u32()? as i32, requests: r.u32()? },
            kind::SNAPSHOT => {
                let tick = r.u32()? as i32;
                let network_id = r.u32()?;
                let kind = r.u8()?;
                let size = r.u32()? as i32;
                let crc = r.u64()?;
                let _len = r.u32()?;
                let data = r.array()?;
                Body::Snapshot { tick, network_id, kind, size, crc, data }
            }
            kind::HOST_MIGRATION => Body::HostMigration { peer_mode: r.u8()? },
            _ => return None,
        };
        let custom = if v >= 3 { r.string()? } else { Some(String::new()) };
        Some(Message { version: Version { protocol, fusion }, body, custom })
    }
}

const DISCONNECT_NAMES: [&str; 8] =
    ["None", "ServerLogic", "InvalidEventCode", "InvalidJoinMsgType", "InvalidJoinGameMode", "IncompatibleConfiguration", "ServerAlreadyInRoom", "Error"];

/// All messages in one event's data (Fusion packs several back to back).
pub fn decode(data: &[u8]) -> Vec<Message> {
    let mut r = BitReader::new(data);
    let mut out = Vec::new();
    while r.left() >= 8 {
        match Message::read(&mut r) {
            Some(m) => out.push(m),
            None => break,
        }
    }
    out
}

pub fn encode(m: &Message) -> Vec<u8> {
    let mut w = BitWriter::default();
    m.write(&mut w);
    w.data
}

struct Peer {
    server: bool,
    unique_id: [u8; 8],
    version: Version,
    started: bool,
    reflexive: Option<Body>,
    /// This client still waits for the host's addresses.
    wants_host_info: bool,
}

/// The plugin of one room.
#[derive(Default)]
pub struct Plugin {
    peers: HashMap<i32, Peer>,
    host: Option<i32>,
    /// The host's network config, as it sent it (Shared mode: the first peer's).
    config: Option<String>,
    /// Shared mode: PeerParty's Fusion server, from the first relayed packet on.
    shared: Option<super::shared::Server>,
    shared_mode: bool,
}

/// What the plugin sends: (actor, message bytes), and lines for the log.
pub type Out = Vec<(i32, Vec<u8>)>;

impl Plugin {
    /// A protocol event from `sender` addressed to the plugin.
    pub fn handle(&mut self, sender: i32, data: &[u8], unique: impl Fn() -> [u8; 8], log: &mut Vec<String>) -> Out {
        let mut out = Out::new();
        for m in decode(data) {
            self.message(sender, m, &unique, &mut out, log);
        }
        out
    }

    fn message(&mut self, sender: i32, m: Message, unique: &impl Fn() -> [u8; 8], out: &mut Out, log: &mut Vec<String>) {
        let v = m.version;
        match m.body {
            Body::Join { kind: 1, game_mode, peer_mode, requests, .. } => {
                if game_mode == SHARED && self.host.is_none() {
                    self.shared_mode = true;
                    let id = unique();
                    // The first peer's config gives the session's tick rate.
                    let first = self.config.is_none() && !self.peers.values().any(|p| p.server);
                    let asks = if first { join_requests::NETWORK_CONFIG } else { 0 };
                    self.peers.insert(
                        sender,
                        Peer { server: first, unique_id: id, version: v, started: true, reflexive: None, wants_host_info: false },
                    );
                    log.push(format!("photon: Fusion Shared peer joined as actor {sender}"));
                    let confirm = Body::Join {
                        kind: 2,
                        game_mode,
                        peer_mode,
                        requests: asks | (requests & join_requests::DISABLE_NAT_PUNCH),
                        unique_id: Some(id.to_vec()),
                    };
                    out.push((sender, encode(&Message::new(v, confirm))));
                    let start = Body::Start { remote_server: 0, requests: start_requests::CONNECT_TO_SHARED };
                    out.push((sender, encode(&Message::new(v, start))));
                    return;
                }
                if game_mode != CLIENT_SERVER || self.shared_mode {
                    log.push(format!("photon: Fusion room mixes Host and Shared modes; refusing actor {sender}"));
                    out.push((sender, encode(&Message::new(v, Body::Disconnect { reason: INCOMPATIBLE_CONFIGURATION }))));
                    return;
                }
                let server = peer_mode == PEER_SERVER;
                if server && self.host.is_some_and(|h| h != sender) {
                    log.push(format!("photon: Fusion room already has a host (actor {}); refusing actor {sender}", self.host.unwrap()));
                    out.push((sender, encode(&Message::new(v, Body::Disconnect { reason: SERVER_ALREADY_IN_ROOM }))));
                    return;
                }
                let id = unique();
                let mut asks = join_requests::REFLEXIVE_INFO;
                if server {
                    asks |= join_requests::NETWORK_CONFIG;
                    self.host = Some(sender);
                }
                self.peers.insert(
                    sender,
                    Peer { server, unique_id: id, version: v, started: false, reflexive: None, wants_host_info: false },
                );
                log.push(format!("photon: Fusion {} joined as actor {sender}", if server { "host" } else { "client" }));
                let confirm = Body::Join { kind: 2, game_mode, peer_mode, requests: asks | (requests & join_requests::DISABLE_NAT_PUNCH), unique_id: Some(id.to_vec()) };
                out.push((sender, encode(&Message::new(v, confirm))));
                if server {
                    self.start(sender, out);
                    // Clients that joined before the host start now.
                    let waiting: Vec<i32> = self.peers.iter().filter(|(_, p)| !p.server && !p.started).map(|(a, _)| *a).collect();
                    for a in waiting {
                        self.start(a, out);
                    }
                } else if self.host.is_some() {
                    self.start(sender, out);
                }
            }
            Body::NetworkConfig { config, .. } => {
                let shared_first = self.shared_mode && self.peers.get(&sender).is_some_and(|p| p.server);
                if self.host == Some(sender) || (shared_first && self.config.is_none()) {
                    self.config = config;
                }
            }
            Body::ReflexiveInfo { public, private, nat, .. } => {
                let Some(p) = self.peers.get_mut(&sender) else { return };
                let info = Body::ReflexiveInfo { actor: sender, public, private, nat, unique_id: Some(p.unique_id.to_vec()) };
                p.reflexive = Some(info.clone());
                if p.server {
                    // Clients waiting for the host's addresses get them now.
                    let waiting: Vec<i32> = self.peers.iter().filter(|(_, c)| c.wants_host_info).map(|(a, _)| *a).collect();
                    for a in waiting {
                        self.send_host_info(a, out);
                    }
                } else {
                    p.wants_host_info = true;
                    if let Some(h) = self.host {
                        let hv = self.peers[&h].version;
                        out.push((h, encode(&Message::new(hv, info))));
                    }
                    self.send_host_info(sender, out);
                }
            }
            Body::Snapshot { tick, network_id, kind: 1, .. } if self.host == Some(sender) => {
                let confirm = Body::Snapshot { tick, network_id, kind: 2, size: 0, crc: 0, data: None };
                out.push((sender, encode(&Message::new(v, confirm))));
            }
            _ => {}
        }
    }

    fn start(&mut self, actor: i32, out: &mut Out) {
        let Some(host) = self.host else { return };
        let Some(p) = self.peers.get_mut(&actor) else { return };
        p.started = true;
        let requests = if p.server { 0 } else { start_requests::WAIT_FOR_REFLEXIVE_INFO };
        out.push((actor, encode(&Message::new(p.version, Body::Start { remote_server: host, requests }))));
    }

    /// The host's addresses to a client that sent its own.
    fn send_host_info(&mut self, client: i32, out: &mut Out) {
        let Some(info) = self.host.and_then(|h| self.peers.get(&h)).and_then(|h| h.reflexive.clone()) else { return };
        let Some(c) = self.peers.get_mut(&client) else { return };
        c.wants_host_info = false;
        out.push((client, encode(&Message::new(c.version, info))));
    }

    /// Shared mode: a relayed Fusion packet from `actor` to the plugin. `players` is the
    /// room's player limit (0 = none), `master` its master client.
    pub fn relay(&mut self, actor: i32, data: &[u8], now: i64, players: usize, master: i32, log: &mut Vec<String>) -> Out {
        if !self.shared_mode {
            return Out::new();
        }
        if self.shared.is_none() {
            let (tick_rate, default_players) = config_numbers(self.config.as_deref());
            let players = if players > 0 { players } else { default_players };
            log.push(format!("photon: Fusion Shared server up ({tick_rate} ticks/s, {players} players)"));
            self.shared = Some(super::shared::Server::new(tick_rate, players));
        }
        let s = self.shared.as_mut().unwrap();
        let out = s.packet(actor, data, now, master);
        log.append(&mut s.log);
        out
    }

    /// Shared mode timers: the server tick.
    pub fn tick(&mut self, now: i64, log: &mut Vec<String>) -> Out {
        let Some(s) = self.shared.as_mut() else { return Out::new() };
        let out = s.tick(now);
        log.append(&mut s.log);
        out
    }

    pub fn next_tick(&self, now: i64) -> Option<i64> {
        self.shared.as_ref().filter(|s| s.players() > 0).map(|s| s.next_tick(now))
    }

    /// An actor left the room.
    pub fn left(&mut self, actor: i32) {
        if let Some(s) = self.shared.as_mut() {
            s.left(actor);
        }
        self.peers.remove(&actor);
        if self.host == Some(actor) {
            self.host = None;
            self.config = None;
        }
    }
}

/// TickRate and DefaultPlayers from a serialized NetworkProjectConfig (its Simulation
/// section), or Fusion's defaults (60, 10).
fn config_numbers(config: Option<&str>) -> (u32, usize) {
    let number = |key: &str| -> Option<f64> {
        let c = config?;
        let at = c.find(&format!("\"{key}\""))? + key.len() + 2;
        let rest = c[at..].trim_start().strip_prefix(':')?.trim_start();
        let end = rest.find(|ch: char| !(ch.is_ascii_digit() || ch == '.' || ch == '-')).unwrap_or(rest.len());
        rest[..end].parse().ok()
    };
    let tick_rate = number("TickRate").filter(|t| *t >= 1.0).map_or(60, |t| t as u32);
    let players = number("DefaultPlayers").filter(|p| *p >= 1.0).map_or(10, |p| p as usize);
    (tick_rate, players)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: Version = Version { protocol: 6, fusion: (2, 0, 6) };

    fn join(peer_mode: u8, game_mode: u8) -> Vec<u8> {
        encode(&Message::new(V, Body::Join { kind: 1, game_mode, peer_mode, requests: 0, unique_id: None }))
    }

    fn reflexive(actor: i32, private: u64) -> Vec<u8> {
        encode(&Message::new(V, Body::ReflexiveInfo { actor, public: [0; 3], private: [private, 0, 0], nat: 2, unique_id: None }))
    }

    fn bodies(out: &Out, to: i32) -> Vec<Body> {
        out.iter().filter(|(a, _)| *a == to).flat_map(|(_, d)| decode(d)).map(|m| m.body).collect()
    }

    #[test]
    fn bit_stream_matches_fusions_layout() {
        // A bool then a byte straddles a byte boundary, low bit first.
        let mut w = BitWriter::default();
        w.bool(true);
        w.u8(0xFF);
        w.string(Some("ab"));
        assert_eq!(w.data[0], 0xFF);
        assert_eq!(w.data[1] & 1, 1);
        let mut r = BitReader::new(&w.data);
        assert_eq!(r.bool(), Some(true));
        assert_eq!(r.u8(), Some(0xFF));
        assert_eq!(r.string(), Some(Some("ab".into())));
        for m in [
            Message::new(V, Body::Join { kind: 2, game_mode: 1, peer_mode: 2, requests: 6, unique_id: Some(vec![1, 2, 3, 4, 5, 6, 7, 8]) }),
            Message::new(V, Body::Start { remote_server: 3, requests: 4 }),
            Message::new(V, Body::Snapshot { tick: 99, network_id: 7, kind: 2, size: 0, crc: 0, data: None }),
            Message::new(V, Body::NetworkConfig { kind: 2, config: Some("{\"x\":1}".into()) }),
            Message::new(Version { protocol: 1, fusion: (0, 0, 0) }, Body::Disconnect { reason: 5 }),
        ] {
            assert_eq!(decode(&encode(&m)), vec![if m.version.protocol < 3 { Message { custom: Some(String::new()), ..m.clone() } } else { m.clone() }]);
        }
    }

    #[test]
    fn host_and_client_are_introduced() {
        let mut p = Plugin::default();
        let mut log = Vec::new();
        let uid = || [9u8; 8];
        // The client comes first and waits for the host.
        let out = p.handle(2, &join(PEER_CLIENT, CLIENT_SERVER), uid, &mut log);
        assert!(matches!(bodies(&out, 2)[..], [Body::Join { kind: 2, .. }]));
        let out = p.handle(1, &join(PEER_SERVER, CLIENT_SERVER), uid, &mut log);
        let host = bodies(&out, 1);
        assert!(matches!(host[0], Body::Join { kind: 2, requests, .. } if requests & join_requests::NETWORK_CONFIG != 0));
        assert!(matches!(host[1], Body::Start { remote_server: 1, .. }));
        assert!(matches!(bodies(&out, 2)[..], [Body::Start { remote_server: 1, requests: start_requests::WAIT_FOR_REFLEXIVE_INFO }]));
        // Addresses: the client's reaches the host; the host's reaches the client once known.
        let out = p.handle(2, &reflexive(2, 0xC0A8), uid, &mut log);
        assert!(matches!(bodies(&out, 1)[..], [Body::ReflexiveInfo { actor: 2, ref unique_id, .. }] if unique_id.as_deref() == Some(&[9u8; 8][..])));
        assert!(bodies(&out, 2).is_empty());
        let out = p.handle(1, &reflexive(1, 0xC0A9), uid, &mut log);
        assert!(matches!(bodies(&out, 2)[..], [Body::ReflexiveInfo { actor: 1, private: [0xC0A9, 0, 0], .. }]));
        // Host-migration snapshots are confirmed.
        let snap = encode(&Message::new(V, Body::Snapshot { tick: 50, network_id: 3, kind: 1, size: 64, crc: 1, data: Some(vec![1; 10]) }));
        let out = p.handle(1, &snap, uid, &mut log);
        assert!(matches!(bodies(&out, 1)[..], [Body::Snapshot { tick: 50, kind: 2, .. }]));
    }

    #[test]
    fn shared_peers_connect_to_the_plugin() {
        let mut p = Plugin::default();
        let mut log = Vec::new();
        let out = p.handle(1, &join(PEER_CLIENT, SHARED), || [1; 8], &mut log);
        assert!(matches!(
            bodies(&out, 1)[..],
            [Body::Join { kind: 2, requests: join_requests::NETWORK_CONFIG, .. }, Body::Start { requests: start_requests::CONNECT_TO_SHARED, .. }]
        ));
        let out = p.handle(2, &join(PEER_CLIENT, SHARED), || [2; 8], &mut log);
        assert!(matches!(bodies(&out, 2)[..], [Body::Join { kind: 2, requests: 0, .. }, Body::Start { .. }]));
        let config = r#"{"Simulation":{"InputDataWordCount":0,"TickRate":30,"DefaultPlayers":4}}"#;
        p.handle(1, &encode(&Message::new(V, Body::NetworkConfig { kind: 2, config: Some(config.into()) })), || [1; 8], &mut log);
        assert_eq!(config_numbers(p.config.as_deref()), (30, 4));
        // A Host mode peer can't join a Shared room.
        let out = p.handle(3, &join(PEER_SERVER, CLIENT_SERVER), || [3; 8], &mut log);
        assert!(matches!(bodies(&out, 3)[..], [Body::Disconnect { reason: INCOMPATIBLE_CONFIGURATION }]));
    }

    #[test]
    fn second_hosts_are_refused() {
        let mut p = Plugin::default();
        let mut log = Vec::new();
        p.handle(1, &join(PEER_SERVER, CLIENT_SERVER), || [1; 8], &mut log);
        let out = p.handle(2, &join(PEER_SERVER, CLIENT_SERVER), || [2; 8], &mut log);
        assert!(matches!(bodies(&out, 2)[..], [Body::Disconnect { reason: SERVER_ALREADY_IN_ROOM }]));
    }
}
