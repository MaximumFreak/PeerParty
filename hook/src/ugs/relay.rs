//! Unity Relay: the allocations API (relay-allocations.services.api.unity.com/v1) and
//! a Relay protocol server in the host's copy.
//!
//! On Unity's service an allocation names a relay server; Unity Transport then binds to
//! it over UDP and relays every packet through it. Here the allocating copy (the host)
//! is the relay: `allocate` starts a UDP socket in its PeerParty and hands out its own
//! address; `joincode` puts the code on the LAN (a lobby in namespace `ugs.relay`
//! holding the relay's port and the host's connection data); `join` finds the code's
//! host on the LAN and hands the joiner an allocation that points at the host's LAN
//! address. Packets go game -> host's PeerParty -> game, one hop on the LAN.
//!
//! The protocol (com.unity.transport 2.x, Runtime/Relay: RelayMessageHeader, Bind,
//! ConnectRequest, Accepted, Relay, Ping, Disconnect, Error; RelayLayer for the client's
//! side) is a 4-byte header (signature 0x72DA little-endian, version 0, type) then:
//! Bind = accept mode, nonce, length 255, connection data (255), HMAC (32);
//! ConnectRequest = from allocation id (16), length 255, the host's connection data;
//! Accepted / Disconnect = from id, to id; Relay = from id, to id, length (big-endian),
//! payload; Ping = from id, sequence; Error = allocation id, code. A client is
//! connected when it gets BindReceived, and to the host when it gets Accepted (from the
//! host's id); the host learns a client from its first Relay packet.
//!
//! Connection data is opaque to clients, so PeerParty writes its own into it: "LBRL",
//! a version, host or joiner, the allocation id and the host's connection limit. The
//! relay reads ids from there. Packets pass only between a joiner and the host it
//! connected to. Allocations list `udp` and `dtls` endpoints, both with `secure: false`:
//! Unity Transport's RelayServerData(allocation, type) and the SDK's
//! AllocationUtils.ToRelayServerData take `isSecure` from the endpoint, so games asking
//! for "dtls" (the Multiplayer SDK's default) run the plain protocol.
//!
//! Keys: every allocation has a random 64-byte key, as Unity's do, and the relay knows
//! only the keys it handed out. The host's copy makes its own allocation; a joiner's
//! copy asks the code's host, over the core's channel `ugs-relay` (`ask_host`), for an
//! allocation on that relay, and the host's copy makes it and answers that player only
//! (one it plays with: `Lan::knows`). So nobody else on the network learns a joiner's
//! key, and allocation ids, which are in every packet, prove nothing. A Bind must carry
//! the HMAC Unity Transport computes with the key (RelayServerData.ComputeBindHMAC:
//! HMAC-SHA256 over the Bind's first 263 bytes, accept mode zeroed): one without it, or
//! for an allocation the relay didn't make, is dropped. An allocation stays with the
//! address that bound it: another address may take it over only with a Bind the relay
//! hasn't seen (a new nonce), or once the old address has said nothing for
//! `REBIND_AFTER`, so a copied Bind moves nothing.
//!
//! A game that makes the relay secure itself (RelayServerData's explicit constructor or
//! UnityTransport.SetRelayServerData with isSecure true) speaks DTLS to it: DTLS 1.2,
//! the allocation id as PSK identity and the allocation's `key` as the PSK (dtls.rs).
//! The same socket answers it: a datagram that starts like a DTLS record opens a DTLS
//! session for its address, the Relay messages inside go through the same `Server`,
//! and whatever goes back to that address is sealed in its session. Plain and DTLS
//! clients can share a host. WebSocket relays (`wss`, WebGL) aren't offered.
//!
//! Design reference (MIT): github.com/abdelfattahradwan/blitz-relay has a host-owned room
//! per code, gone with its host, and a forwarding table per room. It doesn't speak
//! Unity's relay protocol; no code was taken.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use peerparty_core::{Attr, Filter, Lobby, Op, Query, Value};

use crate::json::Json;
use crate::log;
use crate::net;

use super::dtls;
use super::{base64, error_body, random_bytes, random_text};

pub const NS: &str = "ugs.relay";
const MAGIC: &[u8; 4] = b"LBRL";
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
/// Bound allocations that send nothing for this long are dropped (clients ping).
const IDLE: Duration = Duration::from_secs(120);
/// An allocation belongs to the address that bound it, until that address has said
/// nothing for this long, or a Bind with a nonce the relay hasn't seen comes from
/// elsewhere (a client that came back on another port binds again).
const REBIND_AFTER: Duration = Duration::from_secs(20);
/// A joiner's allocation that is never bound is forgotten after this long.
const UNBOUND_KEY_LIFE: Duration = Duration::from_secs(600);
/// The core channel joiners' copies ask a code's host on, and its two directions.
pub const SOCKET: &str = "ugs-relay";
const CH_ASK: u8 = 1;
const CH_ANSWER: u8 = 2;
/// Allocations bound and DTLS sessions open at once: far more than a game's players,
/// and an end to what datagrams from made-up addresses can have the relay keep.
const MAX_BOUND: usize = 1024;
const MAX_TLS: usize = 256;

pub const BIND: u8 = 0;
pub const BIND_RECEIVED: u8 = 1;
pub const PING: u8 = 2;
pub const CONNECT_REQUEST: u8 = 3;
pub const ACCEPTED: u8 = 6;
pub const REJECTED: u8 = 8;
pub const DISCONNECT: u8 = 9;
pub const RELAY: u8 = 10;
pub const ERROR: u8 = 12;

/// RelayMessageError codes.
pub const ERR_CLIENT_MISMATCH: u8 = 3;
pub const ERR_NOT_FOUND: u8 = 4;
pub const ERR_NOT_CONNECTED: u8 = 5;
pub const ERR_SELF_CONNECT: u8 = 6;

pub type AllocId = [u8; 16];

pub fn header(kind: u8) -> [u8; 4] {
    let sig = 29402u16.to_le_bytes();
    [sig[0], sig[1], 0, kind]
}

/// PeerParty's connection data for an allocation (255 bytes).
pub fn connection_data(id: &AllocId, host: bool, max: u16) -> Vec<u8> {
    let mut d = vec![0u8; 255];
    d[..4].copy_from_slice(MAGIC);
    d[4] = 1;
    d[5] = if host { 0 } else { 1 };
    d[6..22].copy_from_slice(id);
    d[22..24].copy_from_slice(&max.to_le_bytes());
    d
}

/// (allocation id, is host, connection limit) from PeerParty's connection data.
pub fn parse_connection_data(d: &[u8]) -> Option<(AllocId, bool, u16)> {
    if d.len() < 24 || &d[..4] != MAGIC || d[4] != 1 {
        return None;
    }
    Some((d[6..22].try_into().ok()?, d[5] == 0, u16::from_le_bytes([d[22], d[23]])))
}

/// A new allocation's key (64 bytes, as Unity's RelayHMACKey).
pub fn new_key() -> Vec<u8> {
    random_bytes(64)
}

/// A Bind's HMAC as Unity Transport computes it (RelayServerData.ComputeBindHMAC): over
/// the message's first 263 bytes (header, accept mode as 0, nonce, length, connection
/// data) with the allocation's key.
pub fn bind_hmac(key: &[u8], bind: &[u8]) -> Option<[u8; 32]> {
    let mut m = bind.get(..263)?.to_vec();
    m[4] = 0;
    Some(crate::photon::crypto::hmac_sha256(key, &m))
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn id_at(pkt: &[u8], at: usize) -> Option<AllocId> {
    pkt.get(at..at + 16)?.try_into().ok()
}

fn error(id: &AllocId, code: u8) -> Vec<u8> {
    let mut p = header(ERROR).to_vec();
    p.extend_from_slice(id);
    p.push(code);
    p
}

fn pair(kind: u8, from: &AllocId, to: &AllocId) -> Vec<u8> {
    let mut p = header(kind).to_vec();
    p.extend_from_slice(from);
    p.extend_from_slice(to);
    p
}

/// The relay itself, without sockets: packets in, packets out.
#[derive(Default)]
pub struct Server {
    /// Every allocation this relay made: its key, and when (a joiner's that is never
    /// bound goes after `UNBOUND_KEY_LIFE`).
    keys: HashMap<AllocId, (Vec<u8>, Instant)>,
    /// Where each bound allocation is, when it last spoke, and the nonce of the Bind
    /// that put it there.
    bound: HashMap<AllocId, (SocketAddr, Instant, u16)>,
    /// Host allocations made on this copy, and their connection limits.
    hosts: HashMap<AllocId, u16>,
    /// Joiner -> host, once connected.
    links: HashMap<AllocId, AllocId>,
    /// DTLS sessions, by client address.
    tls: HashMap<SocketAddr, dtls::Session>,
    /// The secret DTLS cookies are made with (made on first use).
    cookie_secret: Vec<u8>,
}

impl Server {
    pub fn add_host(&mut self, id: AllocId, max: u16, key: Vec<u8>, now: Instant) {
        self.hosts.insert(id, max);
        self.keys.insert(id, (key, now));
    }

    /// A joiner's allocation on this relay. False when the relay holds too many.
    pub fn add_client(&mut self, id: AllocId, key: Vec<u8>, now: Instant) -> bool {
        if self.keys.len() >= MAX_BOUND && !self.keys.contains_key(&id) {
            return false;
        }
        self.keys.insert(id, (key, now));
        true
    }

    pub fn clients_of(&self, host: &AllocId) -> usize {
        self.links.values().filter(|h| *h == host).count()
    }

    /// Forgets allocations and DTLS sessions that went quiet.
    pub fn expire(&mut self, now: Instant) {
        self.tls.retain(|_, s| now.duration_since(s.last_seen) < IDLE);
        let before = self.bound.len();
        let gone: Vec<AllocId> = self.bound.iter().filter(|(_, (_, at, _))| now.duration_since(*at) >= IDLE).map(|(id, _)| *id).collect();
        for id in &gone {
            self.bound.remove(id);
            // A joiner gone quiet is gone: its game makes a new allocation to come back.
            if !self.hosts.contains_key(id) {
                self.keys.remove(id);
            }
        }
        let (bound, hosts) = (&self.bound, &self.hosts);
        self.keys.retain(|id, (_, made)| hosts.contains_key(id) || bound.contains_key(id) || now.duration_since(*made) < UNBOUND_KEY_LIFE);
        if self.bound.len() != before {
            let bound = &self.bound;
            self.links.retain(|c, _| bound.contains_key(c));
        }
    }

    /// Whether `from` is the address `id` is bound from (which counts as life).
    fn known(&mut self, id: &AllocId, from: SocketAddr, now: Instant) -> bool {
        match self.bound.get_mut(id) {
            Some(b) if b.0 == from => {
                b.1 = now;
                true
            }
            _ => false,
        }
    }

    /// Whether the two are a joiner and the host it connected to: the only pairs
    /// packets pass between. (Any bound allocation could be sent to before, so a Bind
    /// from a made-up address aimed the relay at it.)
    fn linked(&self, a: &AllocId, b: &AllocId) -> bool {
        self.links.get(a) == Some(b) || self.links.get(b) == Some(a)
    }

    /// One datagram from `from`, plain or DTLS: what to send where.
    pub fn datagram(&mut self, from: SocketAddr, pkt: &[u8], now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        let mut out = vec![];
        let msgs = if dtls::is_dtls(pkt) {
            if !self.tls.contains_key(&from) {
                if self.tls.len() >= MAX_TLS {
                    return out;
                }
                // Nothing is kept for an address until it has echoed its cookie, so
                // datagrams from made-up addresses use up nothing.
                if self.cookie_secret.is_empty() {
                    self.cookie_secret = random_bytes(32);
                }
                match dtls::check_hello(&self.cookie_secret, from.to_string().as_bytes(), pkt) {
                    dtls::Hello::Verify(hvr) => return vec![(from, hvr)],
                    dtls::Hello::Other => return out,
                    dtls::Hello::Verified => {}
                }
            }
            let s = self.tls.entry(from).or_insert_with(|| {
                log!("ugs relay: DTLS handshake from {from}");
                dtls::Session::new(now)
            });
            let keys = &self.keys;
            let r = s.recv(pkt, now, &|identity: &[u8]| {
                let id: AllocId = identity.try_into().ok()?;
                keys.get(&id).map(|(k, _)| k.clone())
            });
            if r.opened {
                log!("ugs relay: DTLS session with {from} open (allocation {})", hex(&s.identity));
            }
            if let Some(why) = &r.problem {
                log!("ugs relay: DTLS with {from}: {why}");
            }
            if r.closed {
                self.tls.remove(&from);
            }
            out.extend(r.send.into_iter().map(|p| (from, p)));
            r.data
        } else {
            vec![pkt.to_vec()]
        };
        for m in msgs {
            for (to, p) in self.handle(from, &m, now) {
                match self.tls.get_mut(&to) {
                    // A DTLS client still in its handshake gets nothing in the clear.
                    Some(s) => out.extend(s.seal(&p).map(|p| (to, p))),
                    None => out.push((to, p)),
                }
            }
        }
        out
    }

    /// One Relay message from `from`: what to send where.
    pub fn handle(&mut self, from: SocketAddr, pkt: &[u8], now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        if pkt.len() < 4 || pkt[..3] != header(0)[..3] {
            return vec![];
        }
        match pkt[3] {
            BIND => {
                let (Some(data), Some(mac)) = (pkt.get(8..8 + 255), pkt.get(263..295)) else { return vec![] };
                let nonce = u16::from_le_bytes([pkt[5], pkt[6]]);
                let Some((id, _, _)) = parse_connection_data(data) else {
                    return vec![(from, error(&[0; 16], ERR_NOT_FOUND))];
                };
                // Not an allocation this relay made, or not signed with its key: the
                // sender doesn't have the key, so nothing is said back.
                let Some((key, _)) = self.keys.get(&id) else {
                    crate::log::throttled("ugs relay unknown bind", 3, |n| log!("ugs relay: Bind from {from} for an allocation this relay didn't make, dropped n={n}"));
                    return vec![];
                };
                if !bind_hmac(key, pkt).is_some_and(|h| same(&h, mac)) {
                    crate::log::throttled("ugs relay bad hmac", 3, |n| log!("ugs relay: Bind from {from} for {} without its key's HMAC, dropped n={n}", hex(&id)));
                    return vec![];
                }
                match self.bound.get(&id) {
                    // Someone else's, still there, and this Bind is one already seen.
                    Some((at, seen, last)) if *at != from && now.duration_since(*seen) < REBIND_AFTER && *last == nonce => return vec![],
                    Some((at, _, _)) if *at != from => log!("ugs relay: allocation {} bound again, from {from}", hex(&id)),
                    Some(_) => {}
                    None if self.bound.len() >= MAX_BOUND => return vec![],
                    None => log!("ugs relay: allocation {} bound from {from} (Bind HMAC checks out)", hex(&id)),
                }
                self.bound.insert(id, (from, now, nonce));
                vec![(from, header(BIND_RECEIVED).to_vec())]
            }
            PING => {
                let Some(id) = id_at(pkt, 4) else { return vec![] };
                if self.known(&id, from, now) {
                    vec![(from, pkt.to_vec())]
                } else {
                    vec![(from, error(&id, ERR_CLIENT_MISMATCH))]
                }
            }
            CONNECT_REQUEST => {
                let (Some(client), Some(data)) = (id_at(pkt, 4), pkt.get(21..21 + 255)) else { return vec![] };
                if !self.known(&client, from, now) {
                    return vec![(from, error(&client, ERR_CLIENT_MISMATCH))];
                }
                let Some((host, true, _)) = parse_connection_data(data) else {
                    return vec![(from, error(&client, ERR_NOT_FOUND))];
                };
                if host == client {
                    return vec![(from, error(&client, ERR_SELF_CONNECT))];
                }
                let Some(&max) = self.hosts.get(&host) else {
                    return vec![(from, error(&client, ERR_NOT_FOUND))];
                };
                if self.links.get(&client) != Some(&host) && self.clients_of(&host) >= max as usize {
                    log!("ugs relay: host {} is full ({max})", hex(&host));
                    return vec![(from, pair(REJECTED, &host, &client))];
                }
                if self.links.insert(client, host).is_none() {
                    log!("ugs relay: {} connected to host {}", hex(&client), hex(&host));
                }
                vec![(from, pair(ACCEPTED, &host, &client))]
            }
            RELAY => {
                let (Some(src), Some(dst)) = (id_at(pkt, 4), id_at(pkt, 20)) else { return vec![] };
                let Some(len) = pkt.get(36..38).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize) else { return vec![] };
                if pkt.len() != 38 + len {
                    return vec![];
                }
                if !self.known(&src, from, now) {
                    return vec![(from, error(&src, ERR_CLIENT_MISMATCH))];
                }
                match self.bound.get(&dst) {
                    Some((to, _, _)) if self.linked(&src, &dst) => vec![(*to, pkt.to_vec())],
                    _ => vec![(from, error(&src, ERR_NOT_CONNECTED))],
                }
            }
            DISCONNECT => {
                let (Some(src), Some(dst)) = (id_at(pkt, 4), id_at(pkt, 20)) else { return vec![] };
                if !self.known(&src, from, now) || !self.linked(&src, &dst) {
                    return vec![];
                }
                if self.links.get(&src) == Some(&dst) {
                    self.links.remove(&src);
                }
                if self.links.get(&dst) == Some(&src) {
                    self.links.remove(&dst);
                }
                match self.bound.get(&dst) {
                    Some((to, _, _)) => vec![(*to, pkt.to_vec())],
                    None => vec![],
                }
            }
            _ => vec![],
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A GUID's text for 16 bytes, in RFC 4122 byte order.
pub fn guid(id: &AllocId) -> String {
    let h = hex(id);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

// ---- the socket ----

struct Running {
    port: u16,
    server: Mutex<Server>,
}

static RELAY_SERVER: OnceLock<Option<&'static Running>> = OnceLock::new();

/// This copy's relay (started on first use): its UDP port.
fn relay() -> Option<&'static Running> {
    *RELAY_SERVER.get_or_init(|| {
        let sock = match UdpSocket::bind((peerparty_core::bind_ip(), 0)) {
            Ok(s) => s,
            Err(e) => {
                log!("ugs relay could not open a UDP socket: {e}");
                return None;
            }
        };
        let port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
        let running: &'static Running = Box::leak(Box::new(Running { port, server: Mutex::new(Server::default()) }));
        let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            loop {
                let (n, from) = match sock.recv_from(&mut buf) {
                    Ok(r) => r,
                    // Windows reports an ICMP "port unreachable" from a client that went
                    // away as an error on the next receive.
                    Err(_) => continue,
                };
                let out = running.server.lock().unwrap().datagram(from, &buf[..n], Instant::now());
                for (to, pkt) in out {
                    let _ = sock.send_to(&pkt, to);
                }
            }
        });
        log!("ugs relay server on UDP port {port}");
        Some(running)
    })
}

/// Housekeeping from the event pump.
pub fn tick() {
    if let Some(Some(r)) = RELAY_SERVER.get() {
        r.server.lock().unwrap().expire(Instant::now());
    }
}

// ---- allocations ----

struct Alloc {
    id: AllocId,
    data: Vec<u8>,
    code: Option<String>,
}

/// This copy's requests to codes' hosts for an allocation: None while waiting, then the
/// answer (None again inside: refused).
type Answer = Option<Option<(AllocId, Vec<u8>)>>;
static ANSWERS: LazyLock<(Mutex<HashMap<u64, Answer>>, std::sync::Condvar)> = LazyLock::new(Default::default);

static ALLOCS: LazyLock<Mutex<HashMap<String, Alloc>>> = LazyLock::new(Mutex::default);

type Reply = (u16, String);

fn meta(status: u16) -> Json {
    Json::obj(vec![("requestId", Json::str(guid(&random_bytes(16).try_into().unwrap()))), ("status", Json::Num(status as f64))])
}

fn err(status: u16, code: u32, title: &str, detail: &str) -> Reply {
    (status, error_body(status, code, title, detail))
}

fn endpoints(host: &str, port: u16) -> Json {
    Json::Arr(
        ["udp", "dtls"]
            .iter()
            .map(|t| {
                Json::obj(vec![
                    ("connectionType", Json::str(*t)),
                    ("network", Json::str("udp")),
                    ("reliable", Json::Bool(false)),
                    ("secure", Json::Bool(false)),
                    ("host", Json::str(host)),
                    ("port", Json::Num(port as f64)),
                ])
            })
            .collect(),
    )
}

fn allocation(id: &AllocId, key: &[u8], data: &[u8], host: &str, port: u16, region: &str, host_data: Option<&[u8]>) -> Json {
    let mut a = vec![
        ("allocationId", Json::str(guid(id))),
        ("serverEndpoints", endpoints(host, port)),
        ("relayServer", Json::obj(vec![("ipV4", Json::str(host)), ("port", Json::Num(port as f64))])),
        ("key", Json::str(base64(key))),
        ("connectionData", Json::str(base64(data))),
        ("allocationIdBytes", Json::str(base64(id))),
        ("region", Json::str(region)),
    ];
    if let Some(h) = host_data {
        a.push(("hostConnectionData", Json::str(base64(h))));
    }
    Json::obj(a)
}

fn allocate(body: &Json) -> Reply {
    let max = body.get("maxConnections").as_f64().unwrap_or(0.0);
    if !(1.0..=100.0).contains(&max) {
        return err(400, 15001, "Invalid Request", "maxConnections must be 1 to 100");
    }
    let Some(r) = relay() else {
        return err(503, 15010, "No Suitable Relay", "PeerParty couldn't open its relay socket");
    };
    let id: AllocId = random_bytes(16).try_into().unwrap();
    let key = new_key();
    let data = connection_data(&id, true, max as u16);
    r.server.lock().unwrap().add_host(id, max as u16, key.clone(), Instant::now());
    let region = body.get("region").as_str().unwrap_or("lan").to_string();
    ALLOCS.lock().unwrap().insert(guid(&id), Alloc { id, data: data.clone(), code: None });
    log!("ugs relay: allocation {} for {max} connection(s), relay on port {}", guid(&id), r.port);
    let out = Json::obj(vec![
        ("meta", meta(201)),
        ("data", Json::obj(vec![("allocation", allocation(&id, &key, &data, "127.0.0.1", r.port, &region, None))])),
    ]);
    (201, out.to_string())
}

fn join_code(body: &Json) -> Reply {
    let Some(lan) = net::lan() else { return err(503, 15010, "No Suitable Relay", "no LAN") };
    let key = body.get("allocationId").as_str().unwrap_or_default().to_ascii_lowercase();
    let mut allocs = ALLOCS.lock().unwrap();
    let Some(a) = allocs.get_mut(&key) else {
        return err(404, 15008, "Allocation Not Found", "no such allocation on this copy");
    };
    let code = match &a.code {
        Some(c) => c.clone(),
        None => {
            let Some(r) = relay() else { return err(503, 15010, "No Suitable Relay", "no relay") };
            let code = random_text(6, CODE_ALPHABET);
            let mut l = Lobby::new(NS);
            l.id = format!("relay:{code}");
            l.bucket = code.clone();
            l.max_members = 1;
            l.host_migration = false;
            for (k, v) in [
                ("port", Value::Int(r.port as i64)),
                ("data", Value::Str(base64(&a.data))),
                ("alloc", Value::Str(guid(&a.id))),
            ] {
                l.attrs.push(Attr { key: k.into(), value: v, public: true });
            }
            if let Err(e) = lan.create_lobby(l) {
                return err(500, 15000, "Error", &format!("couldn't publish the code: {e:?}"));
            }
            log!("ugs relay: join code {code} for allocation {key}");
            a.code = Some(code.clone());
            code
        }
    };
    let out = Json::obj(vec![
        ("meta", meta(200)),
        ("links", Json::obj(vec![])),
        ("data", Json::obj(vec![("joinCode", Json::str(code))])),
    ]);
    (200, out.to_string())
}

fn join(body: &Json) -> Reply {
    let Some(lan) = net::lan() else { return err(503, 15010, "No Suitable Relay", "no LAN") };
    let code = body.get("joinCode").as_str().unwrap_or_default().trim().to_ascii_uppercase();
    if code.is_empty() {
        return err(400, 15001, "Invalid Request", "a join code is needed");
    }
    // Our own code (a host joining itself, or two games in one process).
    let own = lan.lobby(&format!("relay:{code}"));
    let found = own.or_else(|| {
        let mut q = Query::new(NS);
        q.filters.push(Filter::Bucket { op: Op::Eq, value: code.clone() });
        super::search(q, Duration::from_secs(2)).into_iter().find(|l| l.namespace == NS)
    });
    let Some(l) = found else {
        log!("ugs relay: join code {code} isn't on the LAN");
        return err(404, 15009, "Join Code Not Found", "no copy on the LAN has this join code");
    };
    let ip: IpAddr = if l.owner == lan.me() {
        Ipv4Addr::LOCALHOST.into()
    } else {
        match lan.game_address(l.owner) {
            Some(ip) => ip,
            None => return err(404, 15009, "Join Code Not Found", "the code's host left"),
        }
    };
    let attr = |k: &str| l.attr(k).map(|a| a.value.text()).unwrap_or_default();
    let port: u16 = attr("port").parse().unwrap_or(0);
    let Some(host_data) = super::base64_decode(&attr("data")).filter(|d| d.len() == 255) else {
        return err(404, 15009, "Join Code Not Found", "the code's host data is unreadable");
    };
    // The allocation is the host's relay's to make: only it and this player learn the key.
    let made = if l.owner == lan.me() { make_client(&code) } else { ask_host(lan, l.owner, &code) };
    let Some((id, key)) = made else {
        log!("ugs relay: the host of code {code} gave no allocation");
        return err(404, 15009, "Join Code Not Found", "the code's host didn't answer");
    };
    let data = connection_data(&id, false, 0);
    log!("ugs relay: joined code {code}: relay {ip}:{port}, allocation {} (made by the host's relay)", guid(&id));
    let out = Json::obj(vec![
        ("meta", meta(200)),
        (
            "data",
            Json::obj(vec![("allocation", allocation(&id, &key, &data, &ip.to_string(), port, "lan", Some(&host_data)))]),
        ),
    ]);
    (200, out.to_string())
}

/// A joiner's allocation on this copy's relay, for one of this copy's join codes.
fn make_client(code: &str) -> Option<(AllocId, Vec<u8>)> {
    if !ALLOCS.lock().unwrap().values().any(|a| a.code.as_deref() == Some(code)) {
        return None;
    }
    let r = relay()?;
    let id: AllocId = random_bytes(16).try_into().unwrap();
    let key = new_key();
    r.server.lock().unwrap().add_client(id, key.clone(), Instant::now()).then_some((id, key))
}

/// Asks the host of a join code for an allocation on its relay (the core's channel to
/// that player, which only the two of them read). None if it refuses or doesn't answer.
fn ask_host(lan: &peerparty_core::Lan, host: peerparty_core::PeerId, code: &str) -> Option<(AllocId, Vec<u8>)> {
    super::start_pump();
    let ask: u64 = u64::from_le_bytes(random_bytes(8).try_into().unwrap());
    let mut m = ask.to_le_bytes().to_vec();
    m.extend_from_slice(code.as_bytes());
    let (lock, cv) = &*ANSWERS;
    lock.lock().unwrap().insert(ask, None);
    lan.send(host, SOCKET, CH_ASK, &m, true, true, true);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut w = lock.lock().unwrap();
    loop {
        if w.get(&ask).is_some_and(|a| a.is_some()) {
            return w.remove(&ask).flatten().flatten();
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            w.remove(&ask);
            return None;
        }
        w = cv.wait_timeout(w, left).unwrap().0;
    }
}

/// From the UGS pump: answers players asking this copy's relay for an allocation, and
/// takes the answers to this copy's own asking.
pub fn serve(lan: &peerparty_core::Lan) {
    while let Some(p) = lan.receive_where(Some(CH_ASK), |s| s == SOCKET) {
        let Some(ask) = p.data.get(..8) else { continue };
        let code = String::from_utf8_lossy(&p.data[8..]).to_string();
        let mut reply = ask.to_vec();
        // Only someone this copy plays with: a LAN or room player, or a public one in a
        // lobby with us.
        if lan.knows(p.peer) {
            if let Some((id, key)) = make_client(&code) {
                log!("ugs relay: allocation {} for {} on code {code}", guid(&id), p.peer);
                reply.extend_from_slice(&id);
                reply.extend_from_slice(&key);
            }
        }
        lan.send(p.peer, SOCKET, CH_ANSWER, &reply, true, true, true);
    }
    while let Some(p) = lan.receive_where(Some(CH_ANSWER), |s| s == SOCKET) {
        let Some(ask) = p.data.get(..8).map(|b| u64::from_le_bytes(b.try_into().unwrap())) else { continue };
        let made = (p.data.len() == 8 + 16 + 64).then(|| (p.data[8..24].try_into().unwrap(), p.data[24..].to_vec()));
        let (lock, cv) = &*ANSWERS;
        // Only an answer someone here is waiting for.
        if let Some(slot @ None) = lock.lock().unwrap().get_mut(&ask) {
            *slot = Some(made);
            cv.notify_all();
        }
    }
}

fn regions() -> Reply {
    let out = Json::obj(vec![
        ("meta", meta(200)),
        (
            "data",
            Json::obj(vec![(
                "regions",
                Json::Arr(vec![Json::obj(vec![("id", Json::str("lan")), ("description", Json::str("PeerParty LAN relay"))])]),
            )]),
        ),
    ]);
    (200, out.to_string())
}

/// relay-allocations' routes.
pub fn handle(method: &str, path: &str, body: &Json) -> Reply {
    match (method, path.trim_end_matches('/')) {
        ("POST", "/v1/allocate") => allocate(body),
        ("POST", "/v1/joincode") => join_code(body),
        ("POST", "/v1/join") => join(body),
        ("GET", "/v1/regions") => regions(),
        _ => err(404, 15404, "Not Found", "PeerParty doesn't answer this Relay call"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(p: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], p))
    }

    /// A test allocation's key.
    fn key(id: &AllocId) -> Vec<u8> {
        (0..64u8).map(|i| i ^ id[0]).collect()
    }

    /// A Bind as Unity Transport writes it (RelayMessageBind.Write), signed with `key`.
    fn bind_signed(id: &AllocId, host: bool, nonce: u16, key: &[u8]) -> Vec<u8> {
        let mut p = header(BIND).to_vec();
        p.push(0);
        p.extend_from_slice(&nonce.to_le_bytes());
        p.push(255);
        p.extend_from_slice(&connection_data(id, host, 2));
        let mac = bind_hmac(key, &p).unwrap();
        p.extend_from_slice(&mac);
        assert_eq!(p.len(), 295, "RelayMessageBind.Length");
        p
    }

    fn bind(id: &AllocId, host: bool) -> Vec<u8> {
        bind_signed(id, host, 0, &key(id))
    }

    /// A relay that made `host` (a host allocation) and `clients`.
    fn server(host: AllocId, max: u16, clients: &[AllocId], now: Instant) -> Server {
        let mut s = Server::default();
        s.add_host(host, max, key(&host), now);
        for c in clients {
            assert!(s.add_client(*c, key(c), now));
        }
        s
    }

    fn connect(client: &AllocId, host: &AllocId) -> Vec<u8> {
        let mut p = header(CONNECT_REQUEST).to_vec();
        p.extend_from_slice(client);
        p.push(255);
        p.extend_from_slice(&connection_data(host, true, 2));
        assert_eq!(p.len(), 276, "RelayMessageConnectRequest.k_Length");
        p
    }

    fn relay_msg(from: &AllocId, to: &AllocId, data: &[u8]) -> Vec<u8> {
        let mut p = header(RELAY).to_vec();
        p.extend_from_slice(from);
        p.extend_from_slice(to);
        p.extend_from_slice(&(data.len() as u16).to_be_bytes());
        p.extend_from_slice(data);
        p
    }

    #[test]
    fn header_is_unity_transports() {
        // RelayMessageHeader: ushort 29402 (little-endian on the wire), version 0, type.
        assert_eq!(header(RELAY), [0xDA, 0x72, 0x00, 0x0A]);
        let id = [9u8; 16];
        let d = connection_data(&id, true, 7);
        assert_eq!(d.len(), 255);
        assert_eq!(parse_connection_data(&d), Some((id, true, 7)));
        assert_eq!(parse_connection_data(&[0u8; 255]), None);
        assert_eq!(guid(&[0xab; 16]).len(), 36);
    }

    #[test]
    fn bind_connect_relay_and_disconnect() {
        let now = Instant::now();
        let (host, a, b, c) = ([1u8; 16], [2u8; 16], [3u8; 16], [4u8; 16]);
        let mut s = server(host, 2, &[a, b, c], now);
        // Bind: BindReceived back to the sender.
        for (id, port, is_host) in [(host, 1, true), (a, 2, false), (b, 3, false), (c, 4, false)] {
            let out = s.handle(addr(port), &bind(&id, is_host), now);
            assert_eq!(out, vec![(addr(port), header(BIND_RECEIVED).to_vec())]);
        }
        // Connect: Accepted from the host to the client.
        let out = s.handle(addr(2), &connect(&a, &host), now);
        assert_eq!(out, vec![(addr(2), pair(ACCEPTED, &host, &a))]);
        assert_eq!(out[0].1.len(), 36, "RelayMessageAccepted.k_Length");
        assert_eq!(s.handle(addr(3), &connect(&b, &host), now)[0].1[3], ACCEPTED);
        // A third client is over the host's limit of 2.
        assert_eq!(s.handle(addr(4), &connect(&c, &host), now)[0].1[3], REJECTED);
        // Asking again while connected is still accepted.
        assert_eq!(s.handle(addr(2), &connect(&a, &host), now)[0].1[3], ACCEPTED);
        // Relay both ways, verbatim.
        let up = relay_msg(&a, &host, b"hello");
        assert_eq!(s.handle(addr(2), &up, now), vec![(addr(1), up.clone())]);
        let down = relay_msg(&host, &a, &[0u8; 1200]);
        assert_eq!(s.handle(addr(1), &down, now), vec![(addr(2), down.clone())]);
        // A wrong length is dropped; an unbound sender is told to bind again.
        let mut bad = up.clone();
        bad.push(0);
        assert!(s.handle(addr(2), &bad, now).is_empty());
        let stranger = relay_msg(&[7u8; 16], &host, b"x");
        assert_eq!(s.handle(addr(9), &stranger, now)[0].1, error(&[7u8; 16], ERR_CLIENT_MISMATCH));
        // Ping is echoed (the client counts any packet as life).
        let mut ping = header(PING).to_vec();
        ping.extend_from_slice(&a);
        ping.extend_from_slice(&1u16.to_le_bytes());
        assert_eq!(ping.len(), 22, "RelayMessagePing.k_Length");
        assert_eq!(s.handle(addr(2), &ping, now), vec![(addr(2), ping.clone())]);
        // Disconnect goes to the other side and frees a slot.
        let bye = pair(DISCONNECT, &a, &host);
        assert_eq!(s.handle(addr(2), &bye, now), vec![(addr(1), bye.clone())]);
        assert_eq!(s.handle(addr(4), &connect(&c, &host), now)[0].1[3], ACCEPTED);
        // Rebinding from a new port moves the allocation, once its old port has gone
        // quiet, or at once with a new Bind (the tests below are about the rest).
        let fresh = bind_signed(&c, false, 1, &key(&c));
        assert_eq!(s.handle(addr(40), &fresh, now)[0].1, header(BIND_RECEIVED));
        let later = now + REBIND_AFTER;
        assert_eq!(s.handle(addr(20), &bind(&b, false), later)[0].1, header(BIND_RECEIVED));
        let down = relay_msg(&host, &b, b"to its new port");
        s.handle(addr(1), &bind(&host, true), later);
        assert_eq!(s.handle(addr(1), &down, later), vec![(addr(20), down)]);
    }

    /// Allocation ids aren't secret (they are in every packet, and in lobby data), and
    /// nothing in a packet proves who sent it but where it came from. Each of these took
    /// over a connected player's traffic, or aimed the relay at an address of choice.
    #[test]
    fn an_allocation_stays_with_the_address_that_bound_it() {
        let now = Instant::now();
        let (host, a, x) = ([1u8; 16], [2u8; 16], [9u8; 16]);
        let mut s = server(host, 4, &[a, x], now);
        s.handle(addr(1), &bind(&host, true), now);
        s.handle(addr(2), &bind(&a, false), now);
        assert_eq!(s.handle(addr(2), &connect(&a, &host), now)[0].1[3], ACCEPTED);
        let (thief, soon) = (addr(66), now + Duration::from_secs(1));
        // A ping, a connect request, a packet and a disconnect, each naming the
        // joiner's allocation from somewhere else; then a Bind for it.
        let mut ping = header(PING).to_vec();
        ping.extend_from_slice(&a);
        ping.extend_from_slice(&1u16.to_le_bytes());
        for forged in [ping, connect(&a, &host), relay_msg(&a, &host, b"x"), pair(DISCONNECT, &a, &host)] {
            let out = s.handle(thief, &forged, soon);
            assert!(out.iter().all(|(to, p)| *to == thief && p.get(3) != Some(&RELAY)), "{out:?}");
        }
        // A Bind copied off the network (the same nonce) moves nothing; one the thief
        // makes without the key, or with another allocation's, isn't even answered.
        assert!(s.handle(thief, &bind(&a, false), soon).is_empty());
        assert!(s.handle(thief, &bind_signed(&a, false, 7, &[0u8; 64]), soon).is_empty());
        assert!(s.handle(thief, &bind_signed(&a, false, 7, &key(&x)), soon).is_empty());
        let mut flipped = bind_signed(&a, false, 7, &key(&a));
        flipped[20] ^= 1;
        assert!(s.handle(thief, &flipped, soon).is_empty());
        // The joiner's traffic still goes to the joiner, and the joiner is still connected.
        let down = relay_msg(&host, &a, b"for the joiner");
        assert_eq!(s.handle(addr(1), &down, soon), vec![(addr(2), down)]);
        assert_eq!(s.clients_of(&host), 1);

        // An allocation bound from an address of the sender's choosing isn't somewhere
        // packets can be sent: only a joiner and its host pass them.
        s.handle(addr(77), &bind(&x, false), soon);
        let aimed = relay_msg(&a, &x, &[0u8; 1000]);
        assert_eq!(s.handle(addr(2), &aimed, soon), vec![(addr(2), error(&a, ERR_NOT_CONNECTED))]);

        // Allocations the relay didn't make can't be bound at all, and what it keeps
        // has an end.
        for i in 0..(MAX_BOUND as u32 + 50) {
            let mut id = [0xEEu8; 16];
            id[..4].copy_from_slice(&i.to_le_bytes());
            assert!(s.handle(addr(1000 + (i % 60_000) as u16), &bind(&id, false), soon).is_empty());
            s.add_client(id, key(&id), soon);
        }
        assert_eq!(s.keys.len(), MAX_BOUND);
        assert!(s.bound.len() <= MAX_BOUND);
    }

    /// Unity Transport's HMAC (RelayServerData.ComputeBindHMAC) for a known key, nonce
    /// and connection data, worked by hand from its definition: HMAC-SHA256 over
    /// DA 72 00 00 | 00 | nonce (little-endian) | FF | 255 bytes of connection data.
    #[test]
    fn the_bind_hmac_is_unity_transports() {
        let key: Vec<u8> = (0..64).collect();
        let data = connection_data(&[0x42; 16], false, 0);
        let mut m = vec![0xDA, 0x72, 0x00, 0x00, 0x00, 0x34, 0x12, 0xFF];
        m.extend_from_slice(&data);
        let want = crate::photon::crypto::hmac_sha256(&key, &m);
        let mut pkt = header(BIND).to_vec();
        pkt.push(1); // accept mode: not part of the HMAC
        pkt.extend_from_slice(&0x1234u16.to_le_bytes());
        pkt.push(255);
        pkt.extend_from_slice(&data);
        assert_eq!(bind_hmac(&key, &pkt), Some(want));
        assert_eq!(bind_hmac(&key, &pkt[..100]), None);
    }

    #[test]
    fn joiners_allocations_are_made_by_the_hosts_relay_and_never_bound_ones_expire() {
        let now = Instant::now();
        let (host, a) = ([1u8; 16], [2u8; 16]);
        let mut s = server(host, 4, &[a], now);
        s.expire(now + UNBOUND_KEY_LIFE + Duration::from_secs(1));
        assert!(s.keys.contains_key(&host) && !s.keys.contains_key(&a));
        assert!(s.handle(addr(2), &bind(&a, false), now).is_empty());
        // A bound joiner keeps its key while it talks; once it has gone quiet it's gone.
        assert!(s.add_client(a, key(&a), now));
        assert_eq!(s.handle(addr(2), &bind(&a, false), now)[0].1, header(BIND_RECEIVED));
        s.expire(now + IDLE - Duration::from_secs(1));
        assert!(s.keys.contains_key(&a));
        s.expire(now + IDLE + Duration::from_secs(1));
        assert!(!s.keys.contains_key(&a) && s.keys.contains_key(&host));
    }

    /// A DTLS client's handshake with the relay, done in place.
    fn dtls_bind(s: &mut Server, port: u16, id: &AllocId, host: bool, now: Instant) -> dtls::client::Client {
        let mut c = dtls::client::Client::new(id, &key(id), true);
        let out = s.datagram(addr(port), &c.hello(), now);
        assert!(!s.tls.contains_key(&addr(port)), "no session before the cookie");
        let (again, _) = c.recv(&out[0].1);
        let out = s.datagram(addr(port), &again.unwrap(), now);
        let (f3, _) = c.recv(&out[0].1);
        let out = s.datagram(addr(port), &f3.unwrap(), now);
        c.recv(&out[0].1);
        assert!(c.open);
        let out = s.datagram(addr(port), &c.seal(&bind(id, host)), now);
        assert_eq!(out.len(), 1);
        assert_eq!(c.recv(&out[0].1).1, vec![header(BIND_RECEIVED).to_vec()]);
        c
    }

    #[test]
    fn dtls_and_plain_clients_share_a_host() {
        let now = Instant::now();
        let (host, a, b) = ([1u8; 16], [2u8; 16], [3u8; 16]);
        let mut s = server(host, 4, &[a, b], now);
        let mut h = dtls_bind(&mut s, 1, &host, true, now);
        let mut ca = dtls_bind(&mut s, 2, &a, false, now);
        s.datagram(addr(3), &bind(&b, false), now);
        assert_eq!(s.datagram(addr(3), &connect(&b, &host), now)[0].1[3], ACCEPTED);
        // The DTLS joiner connects and talks to the DTLS host, sealed both ways.
        let out = s.datagram(addr(2), &ca.seal(&connect(&a, &host)), now);
        assert_eq!(ca.recv(&out[0].1).1, vec![pair(ACCEPTED, &host, &a)]);
        let up = relay_msg(&a, &host, b"hello");
        let out = s.datagram(addr(2), &ca.seal(&up), now);
        assert_eq!(out[0].0, addr(1));
        assert_eq!(h.recv(&out[0].1).1, vec![up]);
        // The plain joiner reaches the DTLS host too, and hears back in the clear.
        let up = relay_msg(&b, &host, b"plain");
        let out = s.datagram(addr(3), &up, now);
        assert_eq!(h.recv(&out[0].1).1, vec![up]);
        let down = relay_msg(&host, &b, b"back");
        let out = s.datagram(addr(1), &h.seal(&down), now);
        assert_eq!(out, vec![(addr(3), down)]);
        // A wrong key ends the session without a word in the clear.
        let mut bad = dtls::client::Client::new(&[9u8; 16], &[0u8; 64], true);
        let out = s.datagram(addr(9), &bad.hello(), now);
        let (again, _) = bad.recv(&out[0].1);
        let out = s.datagram(addr(9), &again.unwrap(), now);
        let (f3, _) = bad.recv(&out[0].1);
        s.datagram(addr(9), &f3.unwrap(), now);
        assert!(!s.tls.contains_key(&addr(9)));
        s.expire(now + IDLE + Duration::from_secs(1));
        assert!(s.tls.is_empty());
    }

    #[test]
    fn unknown_hosts_and_self_connects_are_errors() {
        let now = Instant::now();
        let mut s = Server::default();
        let (host, a) = ([1u8; 16], [2u8; 16]);
        s.add_client(a, key(&a), now);
        s.handle(addr(2), &bind(&a, false), now);
        assert_eq!(s.handle(addr(2), &connect(&a, &host), now)[0].1, error(&a, ERR_NOT_FOUND));
        s.add_host(a, 4, key(&a), now);
        let mut p = header(CONNECT_REQUEST).to_vec();
        p.extend_from_slice(&a);
        p.push(255);
        p.extend_from_slice(&connection_data(&a, true, 4));
        assert_eq!(s.handle(addr(2), &p, now)[0].1, error(&a, ERR_SELF_CONNECT));
        // Garbage and foreign headers are ignored.
        assert!(s.handle(addr(2), b"\x00\x01", now).is_empty());
        assert!(s.handle(addr(2), &[0x13, 0x37, 0, 0, 1, 2, 3], now).is_empty());
    }

    #[test]
    fn idle_allocations_expire() {
        let t0 = Instant::now();
        let mut s = server([1; 16], 1, &[[2; 16]], t0);
        s.handle(addr(1), &bind(&[1; 16], true), t0);
        s.handle(addr(2), &bind(&[2; 16], false), t0);
        s.handle(addr(2), &connect(&[2; 16], &[1; 16]), t0);
        s.expire(t0 + IDLE + Duration::from_secs(1));
        assert_eq!(s.clients_of(&[1; 16]), 0);
        let r = relay_msg(&[2; 16], &[1; 16], b"x");
        assert_eq!(s.handle(addr(2), &r, t0)[0].1[3], ERROR);
    }

    #[test]
    fn allocations_parse_as_the_sdk_models_expect() {
        let id = [5u8; 16];
        let k = new_key();
        let a = allocation(&id, &k, &connection_data(&id, false, 0), "10.0.0.2", 4000, "lan", Some(&connection_data(&[6; 16], true, 2)));
        assert_eq!(super::super::base64_decode(a.get("allocationIdBytes").as_str().unwrap()).unwrap(), id.to_vec());
        assert_eq!(super::super::base64_decode(a.get("key").as_str().unwrap()).unwrap(), k, "RelayHMACKey, 64 bytes");
        assert_eq!(k.len(), 64);
        assert_ne!(k, new_key(), "random per allocation");
        assert_eq!(super::super::base64_decode(a.get("connectionData").as_str().unwrap()).unwrap().len(), 255);
        let eps = a.get("serverEndpoints").as_arr();
        assert!(eps.iter().any(|e| e.get("connectionType").as_str() == Some("dtls") && e.get("secure") == &Json::Bool(false)));
        assert_eq!(handle("GET", "/v1/regions", &Json::Null).0, 200);
        assert_eq!(handle("POST", "/v1/allocate", &Json::obj(vec![("maxConnections", Json::Num(0.0))])).0, 400);
    }
}
