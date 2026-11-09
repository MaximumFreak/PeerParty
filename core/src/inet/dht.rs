//! A small client for BitTorrent's public "mainline" DHT (BEP 5): millions of nodes that
//! store, for any 20-byte info-hash, the addresses that announced themselves under it.
//! Copies in the same room announce and look each other up under a topic only the room
//! code gives (see `code::RoomCode::topic`), so nobody needs a server of ours.
//!
//! We're a read-only node (BEP 43): we ask, announce and answer nothing, so nodes don't
//! put us in their routing tables or send us queries. A response's `ip` (BEP 42) tells
//! us our public address as a bonus. `implied_port` makes the nodes record the address
//! our packets come from, which is the NAT mapping the other side punches to.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

pub const BOOTSTRAP: &[&str] = &[
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "dht.libtorrent.org:25401",
    "router.utorrent.com:6881",
];

/// A bencoded value.
#[derive(Clone, Debug, PartialEq)]
pub enum B {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<B>),
    Dict(BTreeMap<Vec<u8>, B>),
}

impl B {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            B::Int(i) => out.extend_from_slice(format!("i{i}e").as_bytes()),
            B::Bytes(b) => {
                out.extend_from_slice(format!("{}:", b.len()).as_bytes());
                out.extend_from_slice(b);
            }
            B::List(l) => {
                out.push(b'l');
                for v in l {
                    v.encode(out);
                }
                out.push(b'e');
            }
            B::Dict(d) => {
                out.push(b'd');
                for (k, v) in d {
                    B::Bytes(k.clone()).encode(out);
                    v.encode(out);
                }
                out.push(b'e');
            }
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.encode(&mut v);
        v
    }

    /// One whole value: trailing bytes, more than 32 levels or a malformed field are
    /// `None`.
    pub fn decode(d: &[u8]) -> Option<B> {
        let (v, rest) = parse(d, 0)?;
        rest.is_empty().then_some(v)
    }

    pub fn get(&self, key: &str) -> Option<&B> {
        match self {
            B::Dict(d) => d.get(key.as_bytes()),
            _ => None,
        }
    }

    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            B::Bytes(b) => Some(b),
            _ => None,
        }
    }
}

pub fn dict(pairs: Vec<(&str, B)>) -> B {
    B::Dict(pairs.into_iter().map(|(k, v)| (k.as_bytes().to_vec(), v)).collect())
}

fn bytes(b: &[u8]) -> B {
    B::Bytes(b.to_vec())
}

fn parse(d: &[u8], depth: usize) -> Option<(B, &[u8])> {
    if depth > 32 {
        return None;
    }
    match *d.first()? {
        b'i' => {
            let end = d.iter().position(|&c| c == b'e')?;
            let s = std::str::from_utf8(&d[1..end]).ok()?;
            Some((B::Int(s.parse().ok()?), &d[end + 1..]))
        }
        b'l' => {
            let mut rest = &d[1..];
            let mut l = Vec::new();
            loop {
                if rest.first() == Some(&b'e') {
                    return Some((B::List(l), &rest[1..]));
                }
                let (v, r) = parse(rest, depth + 1)?;
                l.push(v);
                rest = r;
            }
        }
        b'd' => {
            let mut rest = &d[1..];
            let mut m = BTreeMap::new();
            loop {
                if rest.first() == Some(&b'e') {
                    return Some((B::Dict(m), &rest[1..]));
                }
                let (B::Bytes(k), r) = parse(rest, depth + 1)? else { return None };
                let (v, r) = parse(r, depth + 1)?;
                m.insert(k, v);
                rest = r;
            }
        }
        b'0'..=b'9' => {
            let colon = d.iter().position(|&c| c == b':')?;
            let len: usize = std::str::from_utf8(&d[..colon]).ok()?.parse().ok()?;
            // A huge length must not wrap around (found by the fuzz tests).
            let end = colon.checked_add(1)?.checked_add(len)?;
            let body = d.get(colon + 1..end)?;
            Some((B::Bytes(body.to_vec()), &d[end..]))
        }
        _ => None,
    }
}

/// A compact IPv4 address and port (BEP 5's 6-byte form).
pub fn compact(addr: &[u8]) -> Option<SocketAddr> {
    let a: [u8; 6] = addr.try_into().ok()?;
    let port = u16::from_be_bytes([a[4], a[5]]);
    let ip = Ipv4Addr::new(a[0], a[1], a[2], a[3]);
    (port != 0 && !ip.is_unspecified()).then(|| SocketAddr::V4(SocketAddrV4::new(ip, port)))
}

pub fn to_compact(addr: SocketAddrV4) -> [u8; 6] {
    let mut c = [0u8; 6];
    c[..4].copy_from_slice(&addr.ip().octets());
    c[4..].copy_from_slice(&addr.port().to_be_bytes());
    c
}

/// Whether a DHT packet: a bencoded dictionary.
pub fn is_dht(d: &[u8]) -> bool {
    d.first() == Some(&b'd') && d.last() == Some(&b'e')
}

const ALPHA: usize = 6;
const K: usize = 8;
const MAX_CANDS: usize = 128;
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_NODES: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ask {
    New,
    Asked(Instant),
    Replied,
    Failed,
}

struct Cand {
    addr: SocketAddr,
    dist: [u8; 20],
    ask: Ask,
    token: Option<Vec<u8>>,
}

struct Lookup {
    id: u64,
    target: [u8; 20],
    announce: Option<u16>,
    started: Instant,
    cands: Vec<Cand>,
    values: usize,
}

impl Lookup {
    fn add(&mut self, addr: SocketAddr, node: Option<[u8; 20]>) {
        if self.cands.iter().any(|c| c.addr == addr) {
            return;
        }
        let dist = match node {
            Some(n) => std::array::from_fn(|i| n[i] ^ self.target[i]),
            None => [0xFF; 20],
        };
        let at = self.cands.partition_point(|c| c.dist <= dist);
        if at >= MAX_CANDS {
            return;
        }
        self.cands.insert(at, Cand { addr, dist, ask: Ask::New, token: None });
        self.cands.truncate(MAX_CANDS);
    }
}

struct Pending {
    to: SocketAddr,
    sent: Instant,
    lookup: u64,
    announce: bool,
}

/// How a lookup went, for the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    pub id: u64,
    pub target: [u8; 20],
    pub replied: usize,
    pub values: usize,
    pub announced: usize,
}

/// An `announce_peer` due: to, topic, port, token, lookup id.
type Announce = (SocketAddr, [u8; 20], u16, Vec<u8>, u64);

pub struct Dht {
    id: [u8; 20],
    seeds: Vec<SocketAddr>,
    nodes: Vec<([u8; 20], SocketAddr)>,
    next_txn: u16,
    next_lookup: u64,
    pending: HashMap<[u8; 2], Pending>,
    lookups: Vec<Lookup>,
    out: Vec<(SocketAddr, Vec<u8>)>,
    found: Vec<([u8; 20], SocketAddr)>,
    /// Our address as the nodes see it (BEP 42's `ip`).
    pub external: Option<SocketAddr>,
    /// Responses so far, from any node.
    pub replies: u64,
    /// Accept nodes on private or loopback addresses (the tests' local network).
    pub allow_private: bool,
    /// The port the router forwards to us (`portmap`): announced as it is, instead of
    /// the nodes taking the one our packets come from.
    pub mapped_port: Option<u16>,
}

fn public(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => {
            !(ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_broadcast()
                || ip.is_multicast() || ip.is_unspecified() || ip.octets()[0] == 0)
        }
        IpAddr::V6(_) => false,
    }
}

impl Dht {
    pub fn new(seeds: Vec<SocketAddr>, cached: Vec<([u8; 20], SocketAddr)>) -> Dht {
        Dht {
            id: super::crypto::random_array(),
            seeds,
            nodes: cached,
            next_txn: u16::from_le_bytes(super::crypto::random_array()),
            next_lookup: 1,
            pending: HashMap::new(),
            lookups: Vec::new(),
            out: Vec::new(),
            found: Vec::new(),
            external: None,
            replies: 0,
            allow_private: false,
            mapped_port: None,
        }
    }

    pub fn set_seeds(&mut self, seeds: Vec<SocketAddr>) {
        self.seeds = seeds;
    }

    /// Nodes that answered us, closest-first to nothing in particular: for the cache.
    pub fn good_nodes(&self) -> Vec<([u8; 20], SocketAddr)> {
        self.nodes.clone()
    }

    pub fn busy(&self) -> bool {
        !self.lookups.is_empty()
    }

    /// Looks up who announced `target`, then announces us under it when `announce` (our
    /// port; the nodes use the one our packets come from).
    pub fn lookup(&mut self, target: [u8; 20], announce: Option<u16>, now: Instant) -> u64 {
        let id = self.next_lookup;
        self.next_lookup += 1;
        let mut l = Lookup { id, target, announce, started: now, cands: Vec::new(), values: 0 };
        for (n, a) in &self.nodes {
            l.add(*a, Some(*n));
        }
        for s in &self.seeds {
            l.add(*s, None);
        }
        self.lookups.push(l);
        id
    }

    fn txn(&mut self) -> [u8; 2] {
        self.next_txn = self.next_txn.wrapping_add(1);
        self.next_txn.to_be_bytes()
    }

    fn query(&mut self, to: SocketAddr, q: &str, args: Vec<(&str, B)>, lookup: u64, now: Instant) {
        let t = self.txn();
        let mut a = vec![("id", bytes(&self.id))];
        a.extend(args);
        let msg = dict(vec![
            ("a", dict(a)),
            ("q", bytes(q.as_bytes())),
            ("ro", B::Int(1)),
            ("t", bytes(&t)),
            ("y", bytes(b"q")),
        ]);
        self.pending.insert(t, Pending { to, sent: now, lookup, announce: q == "announce_peer" });
        self.out.push((to, msg.to_vec()));
    }

    /// Sends what's due and finishes lookups; returns the ones that finished.
    pub fn tick(&mut self, now: Instant) -> Vec<Done> {
        self.pending.retain(|_, p| now.duration_since(p.sent) < QUERY_TIMEOUT * 2);
        let mut done = Vec::new();
        let mut sends: Vec<(SocketAddr, [u8; 20], u64)> = Vec::new();
        let mut announces: Vec<Announce> = Vec::new();
        self.lookups.retain_mut(|l| {
            for c in &mut l.cands {
                if let Ask::Asked(at) = c.ask
                    && now.duration_since(at) >= QUERY_TIMEOUT
                {
                    c.ask = Ask::Failed;
                }
            }
            let live: Vec<usize> = (0..l.cands.len()).filter(|&i| l.cands[i].ask != Ask::Failed).collect();
            let window = &live[..live.len().min(K * 2)];
            let mut outstanding = window.iter().filter(|&&i| matches!(l.cands[i].ask, Ask::Asked(_))).count();
            for &i in window {
                if outstanding >= ALPHA {
                    break;
                }
                if l.cands[i].ask == Ask::New {
                    l.cands[i].ask = Ask::Asked(now);
                    sends.push((l.cands[i].addr, l.target, l.id));
                    outstanding += 1;
                }
            }
            let closest = &live[..live.len().min(K)];
            let settled = outstanding == 0
                && closest.iter().all(|&i| l.cands[i].ask == Ask::Replied);
            if !(settled || now.duration_since(l.started) >= LOOKUP_TIMEOUT || live.is_empty()) {
                return true;
            }
            let replied: Vec<&Cand> = l.cands.iter().filter(|c| c.ask == Ask::Replied).collect();
            let mut announced = 0;
            if let Some(port) = l.announce {
                for c in replied.iter().filter(|c| c.token.is_some()).take(K) {
                    announces.push((c.addr, l.target, port, c.token.clone().unwrap(), l.id));
                    announced += 1;
                }
            }
            done.push(Done { id: l.id, target: l.target, replied: replied.len(), values: l.values, announced });
            false
        });
        for (to, target, lookup) in sends {
            self.query(to, "get_peers", vec![("info_hash", bytes(&target))], lookup, now);
        }
        for (to, target, port, token, lookup) in announces {
            let (implied, port) = match self.mapped_port {
                Some(p) => (0, p),
                None => (1, port),
            };
            self.query(
                to,
                "announce_peer",
                vec![
                    ("implied_port", B::Int(implied)),
                    ("info_hash", bytes(&target)),
                    ("port", B::Int(port as i64)),
                    ("token", B::Bytes(token)),
                ],
                lookup,
                now,
            );
        }
        done
    }

    /// Handles a datagram from the DHT. `false` when it isn't a response of ours.
    pub fn handle(&mut self, from: SocketAddr, d: &[u8]) -> bool {
        let Some(msg) = B::decode(d) else { return false };
        let Some(t) = msg.get("t").and_then(B::bytes).and_then(|t| <[u8; 2]>::try_from(t).ok()) else {
            return false;
        };
        let Some(p) = self.pending.get(&t) else { return false };
        if p.to != from {
            return false;
        }
        let p = self.pending.remove(&t).unwrap();
        let y = msg.get("y").and_then(B::bytes);
        let Some(lookup) = self.lookups.iter_mut().find(|l| l.id == p.lookup) else {
            return true;
        };
        let cand = lookup.cands.iter_mut().find(|c| c.addr == from);
        if y != Some(b"r") {
            if let Some(c) = cand {
                c.ask = Ask::Failed;
            }
            return true;
        }
        self.replies += 1;
        if let Some(ip) = msg.get("ip").and_then(B::bytes).and_then(compact) {
            self.external = Some(ip);
        }
        if p.announce {
            return true;
        }
        let Some(r) = msg.get("r") else { return true };
        let node = r.get("id").and_then(B::bytes).and_then(|i| <[u8; 20]>::try_from(i).ok());
        if let Some(c) = cand {
            c.ask = Ask::Replied;
            c.token = r.get("token").and_then(B::bytes).map(<[u8]>::to_vec);
            if let Some(n) = node {
                c.dist = std::array::from_fn(|i| n[i] ^ lookup.target[i]);
            }
        }
        lookup.cands.sort_by_key(|a| a.dist);
        if let Some(n) = node
            && !self.nodes.iter().any(|(_, a)| *a == from)
        {
            if self.nodes.len() >= MAX_NODES {
                self.nodes.remove(0);
            }
            self.nodes.push((n, from));
        }
        if let Some(nodes) = r.get("nodes").and_then(B::bytes) {
            for c in nodes.as_chunks::<26>().0 {
                let (Ok(id), Some(addr)) = (<[u8; 20]>::try_from(&c[..20]), compact(&c[20..])) else {
                    continue;
                };
                if self.allow_private || public(&addr) {
                    lookup.add(addr, Some(id));
                }
            }
        }
        if let Some(B::List(values)) = r.get("values") {
            for v in values {
                if let Some(addr) = v.bytes().and_then(compact) {
                    lookup.values += 1;
                    self.found.push((lookup.target, addr));
                }
            }
        }
        true
    }

    pub fn drain_out(&mut self) -> Vec<(SocketAddr, Vec<u8>)> {
        std::mem::take(&mut self.out)
    }

    /// Addresses announced under a topic we looked up.
    pub fn drain_found(&mut self) -> Vec<([u8; 20], SocketAddr)> {
        std::mem::take(&mut self.found)
    }
}

/// A tiny DHT node for tests: answers `get_peers` with every node it knows and the
/// peers announced so far, and records `announce_peer` (with `implied_port`). One node
/// is the whole DHT, so lookups settle after one round.
pub struct TestNode {
    pub id: [u8; 20],
    peers: HashMap<[u8; 20], Vec<SocketAddr>>,
}

impl Default for TestNode {
    fn default() -> Self {
        TestNode { id: super::crypto::random_array(), peers: HashMap::new() }
    }
}

impl TestNode {
    /// The reply to one datagram, if it's a query we answer.
    pub fn answer(&mut self, from: SocketAddr, d: &[u8]) -> Option<Vec<u8>> {
        let msg = B::decode(d)?;
        if msg.get("y").and_then(B::bytes) != Some(b"q") {
            return None;
        }
        let t = msg.get("t")?.clone();
        let a = msg.get("a")?;
        let hash: [u8; 20] = a.get("info_hash").and_then(B::bytes)?.try_into().ok()?;
        let SocketAddr::V4(from4) = from else { return None };
        let mut r = vec![("id", bytes(&self.id))];
        match msg.get("q").and_then(B::bytes)? {
            b"get_peers" => {
                r.push(("token", bytes(b"tok")));
                let values: Vec<B> = self
                    .peers
                    .get(&hash)
                    .map(|v| {
                        v.iter()
                            .filter_map(|p| match p {
                                SocketAddr::V4(p) => Some(bytes(&to_compact(*p))),
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if !values.is_empty() {
                    r.push(("values", B::List(values)));
                }
            }
            b"announce_peer" => {
                if a.get("token").and_then(B::bytes) != Some(b"tok") {
                    return None;
                }
                let port = match a.get("implied_port") {
                    Some(B::Int(1)) => from.port(),
                    _ => match a.get("port") {
                        Some(B::Int(p)) => *p as u16,
                        _ => return None,
                    },
                };
                let p = SocketAddr::V4(SocketAddrV4::new(*from4.ip(), port));
                let list = self.peers.entry(hash).or_default();
                if !list.contains(&p) {
                    list.push(p);
                }
            }
            _ => return None,
        }
        Some(
            dict(vec![
                ("ip", bytes(&to_compact(from4))),
                ("r", dict(r)),
                ("t", t),
                ("y", bytes(b"r")),
            ])
            .to_vec(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bencode_round_trips_and_rejects_junk() {
        let v = dict(vec![
            ("a", B::List(vec![B::Int(-3), bytes(b"xy")])),
            ("b", dict(vec![("c", B::Int(0))])),
        ]);
        let e = v.to_vec();
        assert_eq!(e, b"d1:ali-3e2:xye1:bd1:ci0eee");
        assert_eq!(B::decode(&e), Some(v));
        for junk in [&b""[..], b"d", b"i12", b"5:abc", b"d1:ai1ee1", b"di1ei2ee", b"x"] {
            assert_eq!(B::decode(junk), None, "{junk:?}");
        }
        let deep = format!("{}{}", "l".repeat(100), "e".repeat(100));
        assert_eq!(B::decode(deep.as_bytes()), None);
    }

    #[test]
    fn compact_addresses() {
        let a: SocketAddrV4 = "203.0.113.5:6881".parse().unwrap();
        assert_eq!(compact(&to_compact(a)), Some(SocketAddr::V4(a)));
        assert_eq!(compact(&[1, 2, 3, 4, 0, 0]), None);
        assert_eq!(compact(&[1, 2, 3]), None);
        assert!(public(&"8.8.8.8:1".parse().unwrap()));
        assert!(!public(&"192.168.1.2:1".parse().unwrap()));
        assert!(!public(&"127.0.0.1:1".parse().unwrap()));
    }

    /// Two clients announce and find each other through one test node, all in memory.
    #[test]
    fn two_clients_find_each_other_through_a_node() {
        let node_addr: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let addrs: [SocketAddr; 2] = ["10.0.0.2:5000".parse().unwrap(), "10.0.0.3:6000".parse().unwrap()];
        let mut node = TestNode::default();
        let mut c: Vec<Dht> = (0..2).map(|_| Dht::new(vec![node_addr], Vec::new())).collect();
        let topic = [9u8; 20];
        let now = Instant::now();
        // Round 1: both announce. Round 2: both look again and see each other.
        for round in 0..2 {
            for (i, d) in c.iter_mut().enumerate() {
                d.allow_private = true;
                d.lookup(topic, Some(1), now);
                for _ in 0..4 {
                    d.tick(now);
                    for (to, q) in d.drain_out() {
                        assert_eq!(to, node_addr);
                        if let Some(r) = node.answer(addrs[i], &q) {
                            assert!(d.handle(node_addr, &r));
                        }
                    }
                }
                assert!(!d.busy(), "round {round}: the lookup settled");
                assert_eq!(d.external, Some(addrs[i]));
            }
        }
        let f0: Vec<SocketAddr> = c[0].drain_found().into_iter().map(|(_, a)| a).collect();
        let f1: Vec<SocketAddr> = c[1].drain_found().into_iter().map(|(_, a)| a).collect();
        assert!(f0.contains(&addrs[1]), "{f0:?}");
        assert!(f1.contains(&addrs[0]), "{f1:?}");
        // A reply from the wrong address, or to no query, is ignored.
        let mut d = Dht::new(vec![node_addr], Vec::new());
        d.lookup(topic, None, now);
        d.tick(now);
        let (_, q) = d.drain_out().remove(0);
        let r = node.answer(addrs[0], &q).unwrap();
        assert!(!d.handle(addrs[1], &r));
        assert!(d.handle(node_addr, &r));
        assert!(!d.handle(node_addr, &r), "only once");
    }

    #[test]
    fn unanswered_lookups_give_up() {
        let mut d = Dht::new(vec!["10.0.0.1:1".parse().unwrap()], Vec::new());
        let t0 = Instant::now();
        d.lookup([1; 20], Some(1), t0);
        assert!(d.tick(t0).is_empty());
        assert_eq!(d.drain_out().len(), 1);
        let done = d.tick(t0 + QUERY_TIMEOUT);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].replied, 0);
        assert!(!d.busy());
    }
}