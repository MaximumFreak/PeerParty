//! Discovery and connections.
//!
//! Every copy broadcasts a small UDP beacon once a second on a fixed port, bound with
//! SO_REUSEADDR so every copy on one PC hears every other. A beacon from an unknown peer
//! of the same game makes us dial its TCP port. Each connection starts with a Hello in
//! both directions, then carries length-prefixed `Msg` frames. Each connection has a
//! reader thread and a writer thread fed by a queue, so the game thread never blocks on
//! the network. When both sides dial at once, the engine keeps one (see `Sink`).
//!
//! Copies on the same PC also meet through a peer board, with no network at all (a PC
//! that's offline, with no adapter up, has nowhere to send a broadcast): each copy keeps
//! a file `%LOCALAPPDATA%\peerparty\peers\<id>.peer` naming its game and TCP port, open
//! without delete sharing for as long as it runs, and dials the others' over loopback.
//! A file that can be deleted belongs to a copy that's gone.
//!
//! Unreliable channel data goes over UDP instead, once the peer has answered a UDP
//! ping on the data socket its Hello named: a firewall that lets the TCP connection
//! through but not UDP leaves it on TCP rather than losing it. Datagrams start with
//! "PP", the wire version and the sender's id; only channel data travels in them.
//!
//! Datagrams are only taken from the IP address the peer's TCP connection comes from.
//! Two copies of this version also agree on sequenced datagrams: each side puts a
//! capability byte and a random per-connection epoch in its ping and pong bodies (older
//! copies send and ignore empty bodies), and once a pong echoes our epoch, our channel
//! data goes as `DGRAM_SEQ` (epoch + sequence number + message). The receiver drops
//! sequenced datagrams with the wrong epoch (a previous connection's, or a forger's who
//! never saw a ping) and duplicates or ones more than 64 behind the newest; datagrams
//! reordered within that window are still delivered. This is not cryptographic
//! authentication: anyone on the LAN can watch pings and claim a peer's id over TCP.
//! The bridge trusts its LAN, as the games' own LAN modes do.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawSocket, FromRawSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::id::PeerId;
use crate::wire::{Beacon, MAX_FRAME, Msg, VERSION};

pub const DISCOVERY_PORT: u16 = 47590;
const BEACON_EVERY: Duration = Duration::from_secs(1);
/// How long a dial and the Hello exchange may take. Tests start an engine with a
/// shorter one (`TestTiming`); an idle connection must outlive it, because the timeout
/// is cleared once the Hello is done.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Bytes queued for one peer that hasn't taken them yet. A peer that stops reading (a
/// hung game, a broken path) is cut at this point instead of growing our memory without
/// end: it reconnects and starts again. Four of the largest frames.
pub(crate) const SEND_QUEUE_MAX: usize = 4 * MAX_FRAME;
/// TCP keepalive: probe a connection idle this long, then every KEEPALIVE_EVERY. Windows
/// sends 10 probes, so a peer whose PC vanished (power, Wi-Fi) is noticed in about 20 s
/// instead of never (a reader waiting on a dead TCP connection otherwise waits forever).
const KEEPALIVE_IDLE: Duration = Duration::from_secs(10);
const KEEPALIVE_EVERY: Duration = Duration::from_secs(1);
/// Biggest datagram we send: under a typical 1500-byte MTU with room for headers.
const MAX_DATAGRAM: usize = 1200;
const DGRAM_PING: u8 = 0;
const DGRAM_PONG: u8 = 1;
const DGRAM_MSG: u8 = 2;
/// Channel data with the sender's epoch and a sequence number; only sent to a peer that
/// echoed our epoch with `CAP_SEQ` (older copies ignore unknown kinds).
const DGRAM_SEQ: u8 = 3;
/// Capability bits in a ping or pong body.
const CAP_SEQ: u8 = 1;
/// Sequence numbers this far behind the newest are too old to deliver.
const SEQ_WINDOW: u32 = 64;
const PINGS: u32 = 10;
/// How long a connection that lost a tie-break (see `Conn::retire`) gets to deliver what
/// was already queued on it before it is cut.
const RETIRE_GRACE: Duration = Duration::from_secs(10);
/// How often we try the LAN path to a peer we already reach through an internet tunnel
/// (its beacon or board entry says it's here too): a LAN that blocks the connection
/// then costs one failed dial per peer this often, not one a second.
/// How often a peer we already reach through a tunnel is dialed on the LAN. Tests start
/// an engine with a shorter one (`TestTiming`).
pub(crate) const LAN_UPGRADE_EVERY: Duration = Duration::from_secs(15);

pub(crate) trait Sink: Send + Sync + 'static {
    /// A handshaken connection. Returns false when we already have a better one: the
    /// new one is then retired, not cut, so what the peer already sent on it arrives.
    fn connected(&self, conn: &Arc<Conn>, name: String) -> bool;
    /// `ordered` is true for a frame off the TCP stream, false for a datagram.
    fn message(&self, conn: &Arc<Conn>, msg: Msg, ordered: bool);
    fn disconnected(&self, conn: &Arc<Conn>);
    fn knows(&self, peer: PeerId) -> bool;
    /// Known, but only through an internet tunnel: a LAN connection would replace it.
    fn upgradable(&self, peer: PeerId) -> bool;
    /// The live connection to a peer.
    fn conn(&self, peer: PeerId) -> Option<Arc<Conn>>;
    fn log(&self, line: &str);
}

pub struct Conn {
    pub peer: PeerId,
    /// The side that dialed. Used to pick one connection when both dial at once.
    pub initiator: PeerId,
    pub addr: SocketAddr,
    tx: Mutex<Option<Sender<Vec<u8>>>>,
    stream: TcpStream,
    local: PeerId,
    data: Arc<UdpSocket>,
    /// Where the peer's data socket is: from its Hello, then from its pings.
    udp: Mutex<Option<SocketAddr>>,
    /// The peer answered a ping: datagrams reach it.
    udp_ok: AtomicBool,
    pings: AtomicU32,
    /// Lost a tie-break to another connection to the same peer (see `retire`).
    retired: AtomicBool,
    /// When the handshake finished.
    pub(crate) since: Instant,
    /// Random per connection: stamps our sequenced datagrams.
    epoch: u32,
    seq: AtomicU32,
    /// The peer's epoch and which of its sequenced datagrams came (see `PeerSeq`).
    seq_in: Mutex<PeerSeq>,
    /// The peer echoed our epoch with CAP_SEQ: it takes sequenced datagrams from us.
    peer_seq: AtomicBool,
    /// Datagrams dropped as duplicates, too old, or with the wrong epoch.
    pub(crate) dgram_drops: AtomicU64,
    /// Bytes queued and not yet written (see SEND_QUEUE_MAX).
    queued: Arc<AtomicUsize>,
    /// Cut because the peer stopped reading.
    overflowed: AtomicBool,
    /// A public stranger's connection: addresses in what we send it (lobby, member and
    /// presence attributes, invites) become this, our stand-in (see `lan::scrub_public`).
    scrub: std::sync::OnceLock<Ipv4Addr>,
}

impl Conn {
    /// From now on, addresses in lobby data, presence and invites sent on this
    /// connection are replaced with `with` (set for public sessions).
    pub fn scrub_as(&self, with: Ipv4Addr) {
        let _ = self.scrub.set(with);
    }

    /// Queues a message. Never blocks; a dead connection drops it.
    pub fn send(&self, msg: &Msg) {
        let frame = match self.scrub.get() {
            Some(with) if crate::lan::may_carry_addresses(msg) => {
                let mut m = msg.clone();
                crate::lan::scrub_public(&mut m, *with);
                m.encode()
            }
            _ => msg.encode(),
        };
        // The peer closes the connection on a frame over MAX_FRAME (`read_frame`), losing
        // everything queued behind it; drop just this one instead.
        if frame.len() > MAX_FRAME {
            return;
        }
        let guard = self.tx.lock().unwrap();
        let Some(tx) = guard.as_ref() else { return };
        let len = frame.len();
        if self.queued.fetch_add(len, Ordering::Relaxed) + len > SEND_QUEUE_MAX {
            self.queued.fetch_sub(len, Ordering::Relaxed);
            drop(guard);
            if !self.overflowed.swap(true, Ordering::Relaxed) {
                self.close();
            }
            return;
        }
        if tx.send(frame).is_err() {
            self.queued.fetch_sub(len, Ordering::Relaxed);
        }
    }


    /// Sends a message that may be lost: as a datagram when UDP reaches the peer and it
    /// fits, else like `send`.
    pub fn send_unreliable(&self, msg: &Msg) {
        if self.udp_ok.load(Ordering::Relaxed) {
            let seq = self.peer_seq.load(Ordering::Relaxed);
            let mut d = unreliable_datagram(self.local, seq.then_some(self.epoch), msg);
            if d.len() <= MAX_DATAGRAM {
                let to = *self.udp.lock().unwrap();
                if let Some(to) = to {
                    if seq {
                        set_seq(&mut d, self.seq.fetch_add(1, Ordering::Relaxed));
                    }
                    if self.data.send_to(&d, to).is_ok() {
                        return;
                    }
                }
            }
        }
        self.send(msg);
    }

    /// Whether our datagrams to this peer carry sequence numbers.
    pub fn udp_sequenced(&self) -> bool {
        self.peer_seq.load(Ordering::Relaxed)
    }

    /// Whether unreliable data to this peer goes over UDP.
    pub fn udp_ready(&self) -> bool {
        self.udp_ok.load(Ordering::Relaxed)
    }

    fn ping(&self) {
        let to = *self.udp.lock().unwrap();
        if let Some(to) = to {
            let body = caps_body(self.epoch, None);
            let _ = self.data.send_to(&datagram(DGRAM_PING, self.local, &body), to);
        }
    }

    pub fn close(&self) {
        self.tx.lock().unwrap().take();
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// Takes a connection that lost a tie-break out of service gently: nothing new is
    /// queued, what is already queued still goes out, then our side stops writing. The
    /// reader keeps delivering what the peer had sent on it until the peer closes it too.
    /// RETIRE_GRACE later it is cut regardless, for a peer that never closes it.
    pub fn retire(self: &Arc<Self>) {
        if self.retired.swap(true, Ordering::Relaxed) {
            return;
        }
        self.tx.lock().unwrap().take();
        let conn = self.clone();
        let _ = thread::Builder::new()
            .name("peerparty-retire".into())
            .spawn(move || {
                thread::sleep(RETIRE_GRACE);
                let _ = conn.stream.shutdown(std::net::Shutdown::Both);
            });
    }

    pub fn retired(&self) -> bool {
        self.retired.load(Ordering::Relaxed)
    }
}

/// Which recent sequence numbers arrived: drops duplicates and datagrams more than
/// SEQ_WINDOW behind the newest, delivers reordered ones inside the window.
#[derive(Default)]
pub(crate) struct SeqWindow {
    started: bool,
    newest: u32,
    /// Bit i: newest - i arrived.
    seen: u64,
}

impl SeqWindow {
    /// Whether `seq` is new; records it.
    pub(crate) fn accept(&mut self, seq: u32) -> bool {
        if !self.started {
            self.started = true;
            self.newest = seq;
            self.seen = 1;
            return true;
        }
        let ahead = seq.wrapping_sub(self.newest) as i32;
        if ahead > 0 {
            let ahead = ahead as u32;
            self.seen = if ahead >= SEQ_WINDOW { 0 } else { self.seen << ahead };
            self.seen |= 1;
            self.newest = seq;
            return true;
        }
        let behind = ahead.unsigned_abs();
        if behind >= SEQ_WINDOW {
            return false;
        }
        let bit = 1u64 << behind;
        if self.seen & bit != 0 {
            return false;
        }
        self.seen |= bit;
        true
    }
}

/// What came of the peer's sequenced datagrams on one connection: its epoch (from its
/// pings and pongs) and the window of sequence numbers seen in that epoch.
#[derive(Default)]
pub(crate) struct PeerSeq {
    epoch: Option<u32>,
    window: SeqWindow,
}

impl PeerSeq {
    /// The peer's epoch, from a ping or pong. A different one is a new sequence (the
    /// peer's side of the connection changed, say after a tie-break), so the window
    /// starts over: kept, the old sequence's newest number would make the new one's
    /// first datagrams look stale and drop them.
    pub(crate) fn set_epoch(&mut self, epoch: u32) {
        if self.epoch != Some(epoch) {
            self.epoch = Some(epoch);
            self.window = SeqWindow::default();
        }
    }

    /// Whether a sequenced datagram is from the peer's current epoch and new; records it.
    pub(crate) fn accept(&mut self, epoch: u32, seq: u32) -> bool {
        self.epoch == Some(epoch) && self.window.accept(seq)
    }

    pub(crate) fn epoch(&self) -> Option<u32> {
        self.epoch
    }
}

/// Whether a datagram from an address other than the peer's TCP connection's may count
/// (a PC with several addresses answers from whichever its routes pick): only when it
/// carries an epoch that went to, or came from, the peer over UDP. A pong must echo our
/// epoch (it saw our ping); a ping or sequenced datagram must carry the peer's epoch we
/// already know. Someone off the path knows neither; unsequenced datagrams (older
/// copies) carry no epoch and stay dropped.
fn foreign_ok(kind: u8, body: &[u8], ours: u32, theirs: Option<u32>) -> bool {
    match kind {
        DGRAM_PONG => parse_caps(body).is_some_and(|(_, _, echo)| echo == Some(ours)),
        DGRAM_PING => theirs.is_some() && parse_caps(body).is_some_and(|(_, e, _)| Some(e) == theirs),
        DGRAM_SEQ => theirs.is_some() && parse_seq(body).is_some_and(|(e, _, _)| Some(e) == theirs),
        _ => false,
    }
}

const SEQ_HEADER: usize = 4 + 4;

/// A sequenced datagram's body, built the plain way (`unreliable_datagram` builds the
/// whole datagram in one buffer); tests check one against the other.
#[cfg(test)]
fn seq_body(epoch: u32, seq: u32, msg: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(SEQ_HEADER + msg.len());
    b.extend_from_slice(&epoch.to_le_bytes());
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(msg);
    b
}

/// (epoch, seq, message) of a sequenced datagram's body.
fn parse_seq(body: &[u8]) -> Option<(u32, u32, &[u8])> {
    if body.len() < SEQ_HEADER {
        return None;
    }
    let epoch = u32::from_le_bytes(body[..4].try_into().ok()?);
    let seq = u32::from_le_bytes(body[4..8].try_into().ok()?);
    Some((epoch, seq, &body[SEQ_HEADER..]))
}

/// A ping body (`echo` None) or a pong body (`echo` = the epoch of the ping it answers):
/// our capabilities and epoch.
fn caps_body(epoch: u32, echo: Option<u32>) -> Vec<u8> {
    let mut b = vec![CAP_SEQ];
    b.extend_from_slice(&epoch.to_le_bytes());
    if let Some(e) = echo {
        b.extend_from_slice(&e.to_le_bytes());
    }
    b
}

/// (capabilities, epoch, echoed epoch) from a ping or pong body; None for an older
/// copy's empty body.
fn parse_caps(body: &[u8]) -> Option<(u8, u32, Option<u32>)> {
    if body.len() < 5 {
        return None;
    }
    let epoch = u32::from_le_bytes(body[1..5].try_into().ok()?);
    let echo = body
        .get(5..9)
        .and_then(|e| e.try_into().ok())
        .map(u32::from_le_bytes);
    Some((body[0], epoch, echo))
}

pub(crate) struct Config {
    pub game: String,
    pub me: PeerId,
    pub name: String,
    pub discovery_port: u16,
    /// Extra beacon targets for networks broadcast doesn't cross (VPNs, other subnets).
    pub extra_targets: Vec<SocketAddr>,
    /// Beacons and the same-PC board. Off, the mesh only connects where it's told to
    /// (`Mesh::dial_local`, from the internet tunnel) and to whoever dials it.
    pub lan_discovery: AtomicBool,
    pub handshake_timeout: Duration,
    pub lan_upgrade_every: Duration,
}

impl Config {
    fn lan(&self) -> bool {
        self.lan_discovery.load(Ordering::Relaxed)
    }
}

/// When we last tried the LAN path to peers we reach through a tunnel (see
/// LAN_UPGRADE_EVERY), and how many tries so far.
#[derive(Default)]
pub(crate) struct Upgrades {
    last: Mutex<HashMap<PeerId, Instant>>,
    pub(crate) tries: AtomicU64,
    /// LAN dials that timed out in a row, by peer, and when the last one did: a peer
    /// whose beacons reach us but whose port never answers is behind a firewall that
    /// drops connections (see `Lan::unreachable`).
    pub(crate) timeouts: Mutex<HashMap<PeerId, (u32, Instant)>>,
}

impl Upgrades {
    /// Peers whose last `n` or more LAN dials timed out, the last within `within`.
    pub(crate) fn timed_out(&self, n: u32, within: Duration) -> Vec<(PeerId, u32)> {
        let mut t = self.timeouts.lock().unwrap();
        t.retain(|_, (_, at)| at.elapsed() < within);
        let mut out: Vec<(PeerId, u32)> = t.iter().filter(|(_, (k, _))| *k >= n).map(|(p, (k, _))| (*p, *k)).collect();
        out.sort();
        out
    }

    fn dialed(&self, id: PeerId, timed_out: bool) {
        let mut t = self.timeouts.lock().unwrap();
        if !timed_out {
            t.remove(&id);
            return;
        }
        if t.len() >= 1024 && !t.contains_key(&id) {
            return;
        }
        let e = t.entry(id).or_insert((0, Instant::now()));
        e.0 = e.0.saturating_add(1);
        e.1 = Instant::now();
    }
}

/// Whether to dial a peer a beacon or the board told us about: one we don't know, or one
/// we only reach through an internet tunnel (from the lower id's side only, so the two
/// don't dial each other at once, and at most every LAN_UPGRADE_EVERY).
fn wanted(id: PeerId, cfg: &Config, sink: &Arc<dyn Sink>, upgrades: &Upgrades) -> bool {
    if !sink.knows(id) {
        return true;
    }
    if cfg.me > id || !sink.upgradable(id) {
        return false;
    }
    let now = Instant::now();
    let mut last = upgrades.last.lock().unwrap();
    if last.get(&id).is_some_and(|t| now.duration_since(*t) < cfg.lan_upgrade_every) {
        return false;
    }
    last.retain(|_, t| now.duration_since(*t) < cfg.lan_upgrade_every);
    if last.len() >= 4096 {
        return false;
    }
    last.insert(id, now);
    let n = upgrades.tries.fetch_add(1, Ordering::Relaxed) + 1;
    sink.log(&format!("lan {id} is on our LAN too: trying the LAN path ({n} tries so far)"));
    true
}

pub(crate) struct Mesh {
    pub port: u16,
    stop: Arc<AtomicBool>,
    cfg: Arc<Config>,
    sink: Arc<dyn Sink>,
    dialing: Arc<Mutex<HashSet<PeerId>>>,
    data: Arc<UdpSocket>,
    pub(crate) upgrades: Arc<Upgrades>,
}

const DGRAM_HEADER: usize = 2 + 2 + 1 + 16;

/// A datagram's header, in a buffer with room for `body` bytes after it.
fn datagram_head(kind: u8, from: PeerId, body: usize) -> Vec<u8> {
    let mut d = Vec::with_capacity(DGRAM_HEADER + body);
    d.extend_from_slice(b"PP");
    d.extend_from_slice(&VERSION.to_le_bytes());
    d.push(kind);
    d.extend_from_slice(&from.0);
    d
}

fn datagram(kind: u8, from: PeerId, body: &[u8]) -> Vec<u8> {
    let mut d = datagram_head(kind, from, body.len());
    d.extend_from_slice(body);
    d
}

/// A channel message as one datagram, built in one buffer: sequenced (with our `epoch`
/// and a sequence number of 0 for `set_seq` to fill in once it is sent) or plain.
fn unreliable_datagram(from: PeerId, epoch: Option<u32>, msg: &Msg) -> Vec<u8> {
    let kind = if epoch.is_some() { DGRAM_SEQ } else { DGRAM_MSG };
    let mut d = datagram_head(kind, from, SEQ_HEADER + 256);
    if let Some(epoch) = epoch {
        d.extend_from_slice(&epoch.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes());
    }
    msg.encode_onto(d)
}

/// Writes a sequenced datagram's sequence number.
fn set_seq(d: &mut [u8], seq: u32) {
    if let Some(s) = d.get_mut(DGRAM_HEADER + 4..DGRAM_HEADER + SEQ_HEADER) {
        s.copy_from_slice(&seq.to_le_bytes());
    }
}

/// (kind, sender, body) of one of our datagrams.
fn parse_datagram(d: &[u8]) -> Option<(u8, PeerId, &[u8])> {
    if d.len() < DGRAM_HEADER || &d[..2] != b"PP" || d[2..4] != VERSION.to_le_bytes() {
        return None;
    }
    let id = PeerId(d[5..21].try_into().ok()?);
    Some((d[4], id, &d[DGRAM_HEADER..]))
}

impl Mesh {
    /// Dials a peer at a port on this PC (an internet tunnel's end), as for a beacon.
    pub fn dial_local(&self, port: u16, id: PeerId) {
        dial(Ipv4Addr::LOCALHOST.into(), port, id, &self.cfg, &self.sink, &self.dialing, &self.data, &self.upgrades);
    }

    /// Turns LAN beacons and the same-PC board on or off while running.
    pub fn set_lan_discovery(&self, on: bool) {
        self.cfg.lan_discovery.store(on, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        if self.stop.swap(true, Ordering::Relaxed) {
            return;
        }
        // The accept thread is blocked in accept(): a connection of our own wakes it to
        // see the flag, and it closes the listener.
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)),
            Duration::from_secs(1),
        );
    }
}

pub(crate) fn start(cfg: Config, sink: Arc<dyn Sink>) -> io::Result<Mesh> {
    let listener = TcpListener::bind((crate::bind_ip(), 0))?;
    let port = listener.local_addr()?.port();
    let udp = discovery_socket(cfg.discovery_port)?;
    let data = Arc::new(UdpSocket::bind((crate::bind_ip(), 0))?);
    data.set_read_timeout(Some(Duration::from_millis(500)))?;
    let stop = Arc::new(AtomicBool::new(false));
    let cfg = Arc::new(cfg);
    let dialing = Arc::new(Mutex::new(HashSet::new()));
    let upgrades = Arc::new(Upgrades::default());

    {
        let (data, sink, stop) = (data.clone(), sink.clone(), stop.clone());
        thread::Builder::new()
            .name("peerparty-udp".into())
            .spawn(move || {
                let mut buf = vec![0u8; 65536];
                let mut foreign: u64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    let Ok((n, from)) = data.recv_from(&mut buf) else {
                        continue;
                    };
                    let Some((kind, peer, body)) = parse_datagram(&buf[..n]) else {
                        continue;
                    };
                    let Some(conn) = sink.conn(peer) else { continue };
                    // Only from where the peer's TCP connection comes from, or vouched
                    // for by an epoch (see foreign_ok): a datagram claiming its id from
                    // elsewhere could otherwise redirect our traffic (a ping) or inject
                    // channel data.
                    let elsewhere = from.ip() != conn.addr.ip();
                    let theirs = conn.seq_in.lock().unwrap().epoch();
                    if elsewhere && !foreign_ok(kind, body, conn.epoch, theirs) {
                        foreign += 1;
                        if foreign <= 3 || foreign.is_power_of_two() {
                            sink.log(&format!(
                                "lan ignored a datagram for {peer} from {} (connected from {}; {foreign} so far)",
                                crate::inet::redact::lan(from),
                                crate::inet::redact::lan(conn.addr)
                            ));
                        }
                        continue;
                    }
                    match kind {
                        DGRAM_PING => {
                            // A ping from another of the peer's addresses gets its pong
                            // there, but our traffic keeps going where it went.
                            if !elsewhere {
                                *conn.udp.lock().unwrap() = Some(from);
                            }
                            let echo = parse_caps(body).map(|(_, epoch, _)| {
                                conn.seq_in.lock().unwrap().set_epoch(epoch);
                                epoch
                            });
                            // An older copy pings with an empty body and gets an empty pong
                            // back, as it always did.
                            let pong = match echo {
                                Some(e) => caps_body(conn.epoch, Some(e)),
                                None => Vec::new(),
                            };
                            let _ = data.send_to(&datagram(DGRAM_PONG, conn.local, &pong), from);
                        }
                        DGRAM_PONG => {
                            let caps = parse_caps(body);
                            if caps.is_some_and(|(_, _, echo)| echo != Some(conn.epoch)) {
                                // An answer to another connection's ping (one the
                                // tie-break or a move just replaced): it says nothing
                                // about this one, and taking it would leave this one
                                // unsequenced for good, as the pings stop once UDP works.
                                continue;
                            }
                            if !conn.udp_ok.swap(true, Ordering::Relaxed) {
                                sink.log(&format!("lan udp to {peer} works ({})", crate::inet::redact::lan(from)));
                            }
                            if let Some((caps, epoch, echo)) = caps {
                                conn.seq_in.lock().unwrap().set_epoch(epoch);
                                if caps & CAP_SEQ != 0
                                    && echo == Some(conn.epoch)
                                    && !conn.peer_seq.swap(true, Ordering::Relaxed)
                                {
                                    sink.log(&format!("lan udp to {peer} is sequenced"));
                                }
                            }
                        }
                        DGRAM_MSG => {
                            if let Some(msg @ Msg::ChanData { .. }) = Msg::decode(body) {
                                sink.message(&conn, msg, false);
                            }
                        }
                        DGRAM_SEQ => {
                            let Some((epoch, seq, body)) = parse_seq(body) else {
                                continue;
                            };
                            let fresh = conn.seq_in.lock().unwrap().accept(epoch, seq);
                            if !fresh {
                                let n = conn.dgram_drops.fetch_add(1, Ordering::Relaxed) + 1;
                                if n <= 3 || n.is_power_of_two() {
                                    sink.log(&format!(
                                        "lan dropped a stale, duplicate or foreign datagram from {peer} (seq {seq}; {n} so far)"
                                    ));
                                }
                                continue;
                            }
                            if let Some(msg @ Msg::ChanData { .. }) = Msg::decode(body) {
                                sink.message(&conn, msg, false);
                            }
                        }
                        _ => {}
                    }
                }
            })?;
    }

    {
        let (cfg, sink, stop, data) = (cfg.clone(), sink.clone(), stop.clone(), data.clone());
        thread::Builder::new()
            .name("peerparty-accept".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let (cfg, sink) = (cfg.clone(), sink.clone());
                    let data = data.clone();
                    thread::spawn(move || {
                        if let Err(e) = handshake(stream, &cfg, &sink, &data, None) {
                            sink.log(&format!("lan accept failed: {e}"));
                        }
                    });
                }
            })?;
    }

    {
        let udp = udp.try_clone()?;
        let (cfg, stop) = (cfg.clone(), stop.clone());
        let beacon = Beacon {
            game: cfg.game.clone(),
            id: cfg.me,
            port,
        }
        .encode();
        let (sink, dialing, data, upgrades) = (sink.clone(), dialing.clone(), data.clone(), upgrades.clone());
        thread::Builder::new()
            .name("peerparty-beacon".into())
            .spawn(move || {
                let broadcast = SocketAddrV4::new(Ipv4Addr::BROADCAST, cfg.discovery_port);
                let mut board: Option<Board> = None;
                let mut board_tried: Option<Instant> = None;
                while !stop.load(Ordering::Relaxed) {
                    // LAN discovery can be turned on and off while we run.
                    let lan = cfg.lan();
                    if !lan {
                        board = None;
                        board_tried = None;
                    } else if board.is_none() && board_tried.is_none_or(|t| t.elapsed() >= Duration::from_secs(30)) {
                        board_tried = Some(Instant::now());
                        board = Board::open(&cfg, port);
                    }
                    if lan && !crate::loopback_only() {
                        let _ = udp.send_to(&beacon, broadcast);
                    }
                    if lan {
                        for t in &cfg.extra_targets {
                            let _ = udp.send_to(&beacon, t);
                        }
                    }
                    if let Some(board) = &board {
                        for (id, tcp) in board.others(&cfg) {
                            if wanted(id, &cfg, &sink, &upgrades) {
                                dial(Ipv4Addr::LOCALHOST.into(), tcp, id, &cfg, &sink, &dialing, &data, &upgrades);
                            }
                        }
                    }
                    thread::sleep(BEACON_EVERY);
                }
                drop(board);
            })?;
    }

    let data_for_dials = data.clone();
    {
        let data = data_for_dials;
        let (cfg, sink, stop, dialing, upgrades) = (cfg.clone(), sink.clone(), stop.clone(), dialing.clone(), upgrades.clone());
        thread::Builder::new()
            .name("peerparty-discovery".into())
            .spawn(move || {
                let mut buf = [0u8; 1500];
                while !stop.load(Ordering::Relaxed) {
                    let Ok((n, from)) = udp.recv_from(&mut buf) else {
                        continue;
                    };
                    let Some(b) = Beacon::decode(&buf[..n]) else {
                        continue;
                    };
                    if !cfg.lan() || b.id == cfg.me || b.game != cfg.game || !wanted(b.id, &cfg, &sink, &upgrades) {
                        continue;
                    }
                    dial(from.ip(), b.port, b.id, &cfg, &sink, &dialing, &data, &upgrades);
                }
            })?;
    }

    Ok(Mesh { port, stop, cfg, sink, dialing, data, upgrades })
}

/// Dials a peer we heard about, unless a dial to it is already under way.
#[allow(clippy::too_many_arguments)]
fn dial(
    ip: IpAddr,
    port: u16,
    id: PeerId,
    cfg: &Arc<Config>,
    sink: &Arc<dyn Sink>,
    dialing: &Arc<Mutex<HashSet<PeerId>>>,
    data: &Arc<UdpSocket>,
    upgrades: &Arc<Upgrades>,
) {
    if !dialing.lock().unwrap().insert(id) {
        return;
    }
    let addr = SocketAddr::new(ip, port);
    let (cfg, sink, dialing, data, upgrades) = (cfg.clone(), sink.clone(), dialing.clone(), data.clone(), upgrades.clone());
    thread::spawn(move || {
        let connected = TcpStream::connect_timeout(&addr, cfg.handshake_timeout);
        // A connect that times out (rather than being refused) is what a firewall that
        // drops connections looks like; this PC's own loopback never has one.
        let timed_out = matches!(&connected, Err(e) if e.kind() == io::ErrorKind::TimedOut) && !ip.is_loopback();
        if connected.is_ok() || timed_out {
            upgrades.dialed(id, timed_out);
        }
        let result = connected.and_then(|s| handshake(s, &cfg, &sink, &data, Some(id)));
        if let Err(e) = result {
            sink.log(&format!("lan dial {} failed: {e}", crate::inet::redact::lan(addr)));
        }
        dialing.lock().unwrap().remove(&id);
    });
}

/// The same-PC peer board (see the module docs).
struct Board {
    dir: PathBuf,
    path: PathBuf,
    file: Option<std::fs::File>,
}

impl Board {
    fn dir() -> PathBuf {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("peerparty")
            .join("peers")
    }

    fn open(cfg: &Config, tcp: u16) -> Option<Board> {
        let dir = Self::dir();
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{}.peer", cfg.me.hex()));
        // FILE_SHARE_READ only: others may read it, nobody may delete it while we run.
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .share_mode(1)
            .open(&path)
            .ok()?;
        writeln!(file, "{}
{}
{}", cfg.game, cfg.discovery_port, tcp).ok()?;
        file.flush().ok()?;
        Some(Board {
            dir,
            path,
            file: Some(file),
        })
    }

    /// (id, TCP port) of every other live copy of our game on this PC.
    fn others(&self, cfg: &Config) -> Vec<(PeerId, u16)> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in entries.flatten() {
            let path = e.path();
            if path == self.path || path.extension().is_none_or(|x| x != "peer") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(PeerId::from_hex)
            else {
                continue;
            };
            // Deletable means its copy is gone.
            if std::fs::remove_file(&path).is_ok() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut lines = text.lines();
            let (game, disc, tcp) = (lines.next(), lines.next(), lines.next());
            if game != Some(cfg.game.as_str())
                || disc.and_then(|d| d.parse::<u16>().ok()) != Some(cfg.discovery_port)
            {
                continue;
            }
            if let Some(tcp) = tcp.and_then(|t| t.parse::<u16>().ok()) {
                out.push((id, tcp));
            }
        }
        out
    }
}

impl Drop for Board {
    fn drop(&mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Hello both ways, then hand the connection to the engine and start its threads.
/// `expect` is the id the beacon promised when we are the dialer.
fn handshake(
    stream: TcpStream,
    cfg: &Config,
    sink: &Arc<dyn Sink>,
    data: &Arc<UdpSocket>,
    expect: Option<PeerId>,
) -> io::Result<()> {
    let addr = stream.peer_addr()?;
    stream.set_nodelay(true)?;
    if let Err(e) = keepalive(&stream, KEEPALIVE_IDLE, KEEPALIVE_EVERY) {
        sink.log(&format!("lan: no TCP keepalive on a connection: {e}"));
    }
    stream.set_read_timeout(Some(cfg.handshake_timeout))?;
    let hello = Msg::Hello {
        version: VERSION,
        game: cfg.game.clone(),
        id: cfg.me,
        name: cfg.name.clone(),
        udp: data.local_addr().map_or(0, |a| a.port()),
    };
    let mut writer = stream.try_clone()?;
    write_frame(&mut writer, &hello.encode())?;
    let mut reader = stream.try_clone()?;
    let frame = read_frame(&mut reader)?;
    let Some(Msg::Hello {
        version,
        game,
        id,
        name,
        udp,
    }) = Msg::decode(&frame)
    else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "no hello"));
    };
    if version != VERSION || game != cfg.game || id == cfg.me || expect.is_some_and(|e| e != id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "wrong peer or game",
        ));
    }
    // A cloned handle keeps its own receive timeout; the reader is the one that matters.
    reader.set_read_timeout(None)?;
    stream.set_read_timeout(None)?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let epoch = u32::from_le_bytes(PeerId::random().0[..4].try_into().unwrap_or_default());
    let conn = Arc::new(Conn {
        peer: id,
        initiator: if expect.is_some() { cfg.me } else { id },
        addr,
        tx: Mutex::new(Some(tx)),
        stream,
        local: cfg.me,
        data: data.clone(),
        udp: Mutex::new((udp != 0).then(|| SocketAddr::new(addr.ip(), udp))),
        udp_ok: AtomicBool::new(false),
        pings: AtomicU32::new(0),
        retired: AtomicBool::new(false),
        since: Instant::now(),
        epoch,
        seq: AtomicU32::new(0),
        seq_in: Mutex::new(PeerSeq::default()),
        peer_seq: AtomicBool::new(false),
        dgram_drops: AtomicU64::new(0),
        queued: Arc::new(AtomicUsize::new(0)),
        overflowed: AtomicBool::new(false),
        scrub: std::sync::OnceLock::new(),
    });
    let queued = conn.queued.clone();
    // A duplicate that lost the tie-break still runs its threads, retired: the peer may
    // have queued data on it before it learned which connection wins, and cutting it
    // would lose that.
    let keep = sink.connected(&conn, name);
    if !keep {
        conn.retire();
    }

    // Find out whether datagrams get through, a few times over the first seconds.
    if keep && udp != 0 {
        let conn = conn.clone();
        thread::Builder::new()
            .name("peerparty-ping".into())
            .spawn(move || {
                while !conn.udp_ok.load(Ordering::Relaxed)
                    && conn.pings.fetch_add(1, Ordering::Relaxed) < PINGS
                    && conn.tx.lock().unwrap().is_some()
                {
                    conn.ping();
                    thread::sleep(Duration::from_millis(500));
                }
            })?;
    }

    thread::Builder::new()
        .name("peerparty-send".into())
        .spawn(move || {
            let mut failed = false;
            while let Ok(frame) = rx.recv() {
                let ok = write_frame(&mut writer, &frame).is_ok();
                queued.fetch_sub(frame.len(), Ordering::Relaxed);
                if !ok {
                    failed = true;
                    break;
                }
            }
            // The queue closed: a retired connection has sent everything queued and only
            // stops writing, so its reader still gets what the peer sends; a closed one
            // is already shut both ways.
            let how = if failed {
                std::net::Shutdown::Both
            } else {
                std::net::Shutdown::Write
            };
            let _ = writer.shutdown(how);
        })?;
    let sink = sink.clone();
    thread::Builder::new()
        .name("peerparty-recv".into())
        .spawn(move || {
            let mut malformed: u64 = 0;
            while let Ok(frame) = read_frame(&mut reader) {
                match Msg::decode(&frame) {
                    Some(msg) => sink.message(&conn, msg, true),
                    None => {
                        // Throttled: a peer on another wire format would flood the log.
                        malformed += 1;
                        if malformed <= 3 || malformed.is_power_of_two() {
                            sink.log(&format!(
                                "lan dropped a malformed frame from {} ({} bytes, first byte {:#04x}; {malformed} so far)",
                                conn.peer,
                                frame.len(),
                                frame.first().copied().unwrap_or(0)
                            ));
                        }
                    }
                }
            }
            // A retired connection's writer may still be draining its queue: leave it to
            // finish (RETIRE_GRACE cuts it in the end).
            if !conn.retired() {
                conn.close();
            }
            if conn.overflowed.load(Ordering::Relaxed) {
                sink.log(&format!(
                    "lan cut {}: it stopped taking data ({} MiB queued)",
                    conn.peer,
                    SEND_QUEUE_MAX >> 20
                ));
            }
            sink.disconnected(&conn);
        })?;
    Ok(())
}

/// Writes the length, then the body, without copying the body (frames run to
/// MAX_FRAME): one vectored write carries both when it can, so a small frame still goes
/// out as one segment.
fn write_frame(w: &mut impl Write, body: &[u8]) -> io::Result<()> {
    let len = (body.len() as u32).to_le_bytes();
    let mut head: &[u8] = &len;
    let mut rest = body;
    while !head.is_empty() {
        match w.write_vectored(&[io::IoSlice::new(head), io::IoSlice::new(rest)]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) if n < head.len() => head = &head[n..],
            Ok(n) => {
                rest = &rest[n - head.len()..];
                head = &[];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    w.write_all(rest)
}

fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(body)
}

/// Turns on TCP keepalive with our timings (SIO_KEEPALIVE_VALS: Windows' default waits
/// two hours before the first probe).
pub(crate) fn keepalive(stream: &TcpStream, idle: Duration, every: Duration) -> io::Result<()> {
    use windows_sys::Win32::Networking::WinSock::{SIO_KEEPALIVE_VALS, SOCKET, WSAIoctl};
    #[repr(C)]
    struct Vals {
        onoff: u32,
        time_ms: u32,
        interval_ms: u32,
    }
    let v = Vals { onoff: 1, time_ms: idle.as_millis() as u32, interval_ms: every.as_millis() as u32 };
    let mut ret = 0u32;
    let r = unsafe {
        WSAIoctl(
            stream.as_raw_socket() as SOCKET,
            SIO_KEEPALIVE_VALS,
            &v as *const Vals as *const core::ffi::c_void,
            size_of::<Vals>() as u32,
            std::ptr::null_mut(),
            0,
            &mut ret,
            std::ptr::null_mut(),
            None,
        )
    };
    if r != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// A UDP socket on the discovery port that every copy on this machine can share.
/// std can't set SO_REUSEADDR before bind, so this builds the socket with winsock.
fn discovery_socket(port: u16) -> io::Result<UdpSocket> {
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, IN_ADDR, INVALID_SOCKET, IPPROTO_UDP, SO_BROADCAST, SO_REUSEADDR, SOCK_DGRAM,
        SOCKADDR, SOCKADDR_IN, SOL_SOCKET, WSADATA, WSAStartup, bind, closesocket, setsockopt,
        socket,
    };
    unsafe {
        let mut data: WSADATA = std::mem::zeroed();
        // Returns the error itself (WSAGetLastError isn't usable before it succeeds).
        let ret = WSAStartup(0x0202, &mut data);
        if ret != 0 {
            return Err(io::Error::from_raw_os_error(ret));
        }
        let s = socket(AF_INET as i32, SOCK_DGRAM, IPPROTO_UDP);
        if s == INVALID_SOCKET {
            return Err(io::Error::last_os_error());
        }
        let on: i32 = 1;
        for opt in [SO_REUSEADDR, SO_BROADCAST] {
            if setsockopt(s, SOL_SOCKET, opt, &on as *const i32 as *const u8, 4) != 0 {
                let e = io::Error::last_os_error();
                closesocket(s);
                return Err(e);
            }
        }
        let mut addr: SOCKADDR_IN = std::mem::zeroed();
        addr.sin_family = AF_INET;
        addr.sin_port = port.to_be();
        addr.sin_addr = IN_ADDR {
            S_un: std::mem::zeroed(),
        };
        addr.sin_addr.S_un.S_addr = u32::from(crate::bind_ip()).to_be();
        if bind(
            s,
            &addr as *const SOCKADDR_IN as *const SOCKADDR,
            std::mem::size_of::<SOCKADDR_IN>() as i32,
        ) != 0
        {
            let e = io::Error::last_os_error();
            closesocket(s);
            return Err(e);
        }
        let udp = UdpSocket::from_raw_socket(s as std::os::windows::io::RawSocket);
        udp.set_read_timeout(Some(Duration::from_millis(500)))?;
        Ok(udp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lan_dial_timeouts_in_a_row_mark_a_peer_unreachable_until_one_connects() {
        let u = Upgrades::default();
        let (a, b) = (PeerId([1; 16]), PeerId([2; 16]));
        for _ in 0..3 {
            u.dialed(a, true);
        }
        u.dialed(b, true);
        assert_eq!(u.timed_out(3, Duration::from_secs(60)), vec![(a, 3)]);
        // One that gets through resets the run.
        u.dialed(a, false);
        assert!(u.timed_out(1, Duration::from_secs(60)).iter().all(|(p, _)| *p != a));
        // And old ones age out.
        assert!(u.timed_out(1, Duration::ZERO).is_empty());
    }

    #[test]
    fn datagrams_from_another_address_need_an_epoch() {
        let (ours, theirs) = (0x1111_1111, 0x2222_2222);
        // A pong that echoes our ping counts; one that doesn't, or has no echo, doesn't.
        assert!(foreign_ok(DGRAM_PONG, &caps_body(theirs, Some(ours)), ours, None));
        assert!(!foreign_ok(DGRAM_PONG, &caps_body(theirs, Some(7)), ours, None));
        assert!(!foreign_ok(DGRAM_PONG, &caps_body(theirs, None), ours, None));
        // Pings and sequenced data only once the peer's epoch is known, and only with it.
        assert!(!foreign_ok(DGRAM_PING, &caps_body(theirs, None), ours, None));
        assert!(foreign_ok(DGRAM_PING, &caps_body(theirs, None), ours, Some(theirs)));
        assert!(!foreign_ok(DGRAM_PING, &caps_body(9, None), ours, Some(theirs)));
        assert!(foreign_ok(DGRAM_SEQ, &seq_body(theirs, 1, b"x"), ours, Some(theirs)));
        assert!(!foreign_ok(DGRAM_SEQ, &seq_body(9, 1, b"x"), ours, Some(theirs)));
        // Unsequenced data carries nothing to check.
        assert!(!foreign_ok(DGRAM_MSG, b"anything", ours, Some(theirs)));
    }

    /// A loopback TCP pair: (ours, the peer's end).
    fn tcp_pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (b, _) = l.accept().unwrap();
        (a, b)
    }

    #[test]
    fn keepalive_is_accepted() {
        let (a, _b) = tcp_pair();
        keepalive(&a, KEEPALIVE_IDLE, KEEPALIVE_EVERY).unwrap();
    }

    /// A peer that stops taking data is cut once SEND_QUEUE_MAX is queued, not kept
    /// growing our memory.
    #[test]
    fn a_peer_that_stops_reading_is_cut() {
        let (a, mut b) = tcp_pair();
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let conn = Conn {
            peer: PeerId::random(),
            initiator: PeerId::random(),
            addr: a.peer_addr().unwrap(),
            tx: Mutex::new(Some(tx)),
            stream: a,
            local: PeerId::random(),
            data: Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap()),
            udp: Mutex::new(None),
            udp_ok: AtomicBool::new(false),
            pings: AtomicU32::new(0),
            retired: AtomicBool::new(false),
            since: Instant::now(),
            epoch: 1,
            seq: AtomicU32::new(0),
            seq_in: Mutex::new(PeerSeq::default()),
            peer_seq: AtomicBool::new(false),
            dgram_drops: AtomicU64::new(0),
            queued: Arc::new(AtomicUsize::new(0)),
            overflowed: AtomicBool::new(false),
            scrub: std::sync::OnceLock::new(),
        };
        let msg = Msg::ChanData { socket: "s".into(), channel: 0, reliable: true, data: vec![7; 1 << 20] };
        let mut sent = 0;
        while !conn.overflowed.load(Ordering::Relaxed) {
            conn.send(&msg);
            sent += 1;
            assert!(sent < 200, "never cut");
        }
        assert!(conn.queued.load(Ordering::Relaxed) <= SEND_QUEUE_MAX);
        assert!(conn.tx.lock().unwrap().is_none(), "the queue is closed");
        assert_eq!(rx.try_iter().count(), sent - 1, "the frame that didn't fit wasn't queued");
        // The connection is shut: the peer sees the end.
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(b.read(&mut buf).unwrap_or(0), 0);
        conn.send(&msg); // a cut connection ignores more
    }

    #[test]
    fn seq_window_drops_duplicates_and_stale() {
        let mut w = SeqWindow::default();
        assert!(w.accept(10));
        assert!(!w.accept(10), "duplicate");
        assert!(w.accept(12));
        assert!(w.accept(11), "reordered inside the window");
        assert!(!w.accept(11));
        assert!(w.accept(12 + 63));
        assert!(!w.accept(12), "63 behind and already seen");
        assert!(!w.accept(11), "64 behind is too old");
        assert!(w.accept(13), "62 behind is still in the window");
        assert!(w.accept(1000), "a jump forward");
        assert!(!w.accept(999 - 64));
        assert!(w.accept(999));
    }

    #[test]
    fn seq_window_wraps() {
        let mut w = SeqWindow::default();
        assert!(w.accept(u32::MAX - 1));
        assert!(w.accept(u32::MAX));
        assert!(w.accept(0));
        assert!(w.accept(1));
        assert!(!w.accept(u32::MAX));
        assert!(w.accept(u32::MAX - 2), "behind across the wrap, in the window");
    }

    #[test]
    fn a_new_peer_epoch_starts_the_window_over() {
        let mut s = PeerSeq::default();
        assert!(!s.accept(1, 0), "no epoch yet");
        s.set_epoch(1);
        for n in 0..1000 {
            assert!(s.accept(1, n));
        }
        assert!(!s.accept(2, 1000), "another epoch's datagram");
        // The same epoch again (every ping says it) keeps the window.
        s.set_epoch(1);
        assert!(!s.accept(1, 999), "still a duplicate");
        // A new epoch's sequence starts at 0, far behind the old one's newest.
        s.set_epoch(2);
        assert!(s.accept(2, 0));
        assert!(s.accept(2, 1));
        assert!(!s.accept(2, 0));
        assert!(!s.accept(1, 1000), "the old epoch's are foreign now");
    }

    #[test]
    fn unreliable_datagrams_parse_back() {
        let msg = Msg::ChanData {
            socket: "party:g".into(),
            channel: 3,
            reliable: false,
            data: vec![1, 2, 3, 4],
        };
        let me = PeerId([7; 16]);
        let mut d = unreliable_datagram(me, Some(0xA1B2_C3D4), &msg);
        set_seq(&mut d, 42);
        let (kind, from, body) = parse_datagram(&d).unwrap();
        assert_eq!((kind, from), (DGRAM_SEQ, me));
        let (epoch, seq, inner) = parse_seq(body).unwrap();
        assert_eq!((epoch, seq), (0xA1B2_C3D4, 42));
        assert_eq!(Msg::decode(inner), Some(msg.clone()));
        assert_eq!(d, datagram(DGRAM_SEQ, me, &seq_body(0xA1B2_C3D4, 42, &msg.encode())));

        let d = unreliable_datagram(me, None, &msg);
        assert_eq!(d, datagram(DGRAM_MSG, me, &msg.encode()));
    }

    /// Takes at most `max` bytes per write, like a socket with a full buffer.
    struct Trickle {
        out: Vec<u8>,
        max: usize,
    }

    impl Write for Trickle {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            let n = b.len().min(self.max);
            self.out.extend_from_slice(&b[..n]);
            Ok(n)
        }
        fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
            let mut n = 0;
            for b in bufs {
                let take = b.len().min(self.max - n);
                self.out.extend_from_slice(&b[..take]);
                n += take;
                if n == self.max {
                    break;
                }
            }
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn frames_survive_short_writes() {
        let body: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        for max in [1, 2, 3, 4, 5, 7, 1000, 2000] {
            let mut w = Trickle { out: Vec::new(), max };
            write_frame(&mut w, &body).unwrap();
            write_frame(&mut w, &[]).unwrap();
            let mut r = &w.out[..];
            assert_eq!(read_frame(&mut r).unwrap(), body, "max {max}");
            assert_eq!(read_frame(&mut r).unwrap(), Vec::<u8>::new());
            assert!(r.is_empty());
        }
    }

    #[test]
    fn caps_and_seq_bodies_round_trip() {
        assert_eq!(parse_caps(&[]), None, "an older copy's empty body");
        assert_eq!(parse_caps(&caps_body(7, None)), Some((CAP_SEQ, 7, None)));
        assert_eq!(
            parse_caps(&caps_body(7, Some(9))),
            Some((CAP_SEQ, 7, Some(9)))
        );
        let b = seq_body(5, 6, b"msg");
        assert_eq!(parse_seq(&b), Some((5, 6, &b"msg"[..])));
        assert_eq!(parse_seq(&b[..7]), None);
        let d = datagram(DGRAM_SEQ, PeerId([3; 16]), &b);
        let (kind, from, body) = parse_datagram(&d).unwrap();
        assert_eq!((kind, from), (DGRAM_SEQ, PeerId([3; 16])));
        assert_eq!(body, &b[..]);
    }
}
