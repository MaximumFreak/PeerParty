//! Internet play without a server of ours: copies that share a room code find each other
//! through the public BitTorrent DHT, punch through their NATs with UDP, and carry the
//! ordinary LAN mesh connection through an encrypted tunnel.
//!
//! One UDP socket does all of it, told apart by the first byte: STUN answers (which public
//! address we have), DHT responses (bencoded dictionaries) and tunnel packets (`MAGIC`).
//!
//! Finding each other: every copy in a room announces itself under the room's DHT topic
//! for this hour and looks up who else did (see `code`), then sends handshake packets to
//! every address it found. Both sides doing that at once is what opens both NATs (UDP
//! hole punching). Addresses in `PEERPARTY_INET_PEERS` (a forwarded port, a VPN) are
//! punched the same way, with or without the DHT.
//!
//! The tunnel: a handshake keyed with the room code (`crypto`) gives a session; inside
//! it, a reliable stream (`stream`) carries the bytes of one mesh TCP connection and
//! datagrams carry the mesh's UDP channel data. The mesh itself doesn't change: the side
//! with the lower peer id opens a loopback listener and asks its mesh to dial it, the
//! other side connects to its own mesh's listener, and each gateway relays. The only
//! rewrite is the peer's Hello, whose UDP port becomes a local relay socket's, so the
//! mesh sends datagrams there and they go out through the tunnel too. A peer the LAN
//! already reaches gets no tunnel.
//!
//! Public sessions (`public`, opt-in): the same, in a room everyone shares, with each
//! player's own signing key in the handshake, a block list, and tighter limits.
//!
//! Internet addresses stay out of logs and status by default (`redact`).

mod bridge;
pub mod code;
mod disc;
mod probe;
mod session;
pub mod dht;
pub mod portmap;
pub mod public;
pub mod stream;
pub mod stun;

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

pub use code::RoomCode;
/// Crate-level since they serve more than the internet layer; kept here too under their
/// old paths (`inet::crypto`, `inet::redact`).
pub use crate::{crypto, redact};
pub use stun::Nat;
pub use probe::{Probe, probe};

use bridge::*;
use disc::*;
use probe::*;
use session::*;

use crate::hello::{HelloRewriter, sniff_hello};
use crate::id::PeerId;
use crypto::{Ephemeral, Keys, Replay};
use dht::Dht;
use stream::{Out, Stream};

/// A peer that connects again from another address while its old session has been
/// quiet this long (two keepalives missed: the old copy is gone, e.g. a game restarted)
/// replaces the old session instead of waiting out DEAD_AFTER. A test may set
/// `Config::replace_quiet` shorter.
const REPLACE_QUIET: Duration = Duration::from_secs(10);
const MAX_SESSIONS: usize = 32;
const TICK: Duration = Duration::from_millis(10);
/// How long `Inet::stop` waits for the layer's threads to end, at most.
const STOP_WAIT: Duration = Duration::from_secs(2);
/// Public sessions: fewer sessions, less buffering, and an inbound handshake that doesn't
/// finish soon is dropped (half-open sessions). One internet address can be a whole
/// household or LAN party behind one router, so the per-address limits are per player:
/// PUBLIC_PER_KEY sessions per verified key, PUBLIC_HALF_OPEN_PER_IP handshakes not yet
/// verified per address, and PUBLIC_PER_IP in all (PUBLIC_MAX_SESSIONS bounds it too).
const PUBLIC_MAX_SESSIONS: usize = 16;
const PUBLIC_PER_IP: usize = 32;
/// Handshakes (a new session or new keys) per internet address: a burst, then per second.
const PUBLIC_INIT_BURST: f32 = 32.0;
const PUBLIC_INIT_RATE: f32 = 8.0;
/// Public sessions: a first INIT from an address we have no session with gets a cookie
/// back (a MAC of that address, the INIT's key and the time, under a key that never
/// leaves this run), and nothing is kept for it. It gets a session when its INIT comes
/// again carrying the cookie, which only a sender that receives packets at that address
/// can do: made-up source addresses can't take the half-open slots, and what one gets
/// back is no bigger than what it sent. A cookie is good for one to two COOKIE_FORs
/// (an INIT goes out every second or faster while unanswered).
const COOKIE_FOR: Duration = Duration::from_secs(10);
/// Cookies sent in all, whatever the addresses: a burst, then per second.
const COOKIE_BURST: f32 = 64.0;
const COOKIE_RATE: f32 = 32.0;
/// How long a blocked player's address is ignored.
const BLOCKED_FOR: Duration = Duration::from_secs(600);
/// How many blocks remember the address they dropped (`Shared::recent_blocks`).
const RECENT_BLOCKS: usize = 64;
/// After a block, how long keys we've never seen are refused from that player's internet
/// address (one coming back with a new key). Known keys and friends still get in.
const NEW_KEY_COOLDOWN: Duration = Duration::from_secs(1800);

/// What the internet layer does, from `PEERPARTY_ROOM` and friends or set by a test.
#[derive(Clone, Debug)]
pub struct Config {
    pub room: RoomCode,
    /// Our UDP port (0: any). A fixed one can be forwarded on the router.
    pub port: u16,
    /// Addresses to punch besides what the DHT finds ("host:port").
    pub peers: Vec<String>,
    /// Use the public DHT.
    pub dht: bool,
    pub bootstrap: Vec<String>,
    /// STUN servers ("host:port"); none skips STUN.
    pub stun: Vec<String>,
    /// Where good DHT nodes are kept between runs.
    pub node_cache: Option<PathBuf>,
    /// Accept DHT nodes on private addresses (a test's local DHT).
    pub allow_private: bool,
    /// Drops this share (percent) of outgoing tunnel packets: tests only.
    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub loss_percent: u8,
    /// Public sessions (`room` is then `RoomCode::public()`).
    pub public: Option<public::PublicCfg>,
    /// Ask the router to forward our UDP port (PCP, NAT-PMP or UPnP), and announce the
    /// forwarded port. Off in loopback mode and with `PEERPARTY_INET_PORTMAP=0`.
    pub portmap: bool,
    /// Prefer a direct IPv6 path to a peer when both sides have one (`PEERPARTY_IPV6=0`: off).
    pub ipv6: bool,
    /// Rooms: the key that signs our handshakes (this player's, filled in by the
    /// engine; a fresh one when `None`). A member's id must be its key's hash, which
    /// stops one from passing as another. Public sessions use `public`'s instead.
    pub room_key: Option<Arc<public::IdentityKey>>,
    /// Quiet time before a restarted player's new handshake replaces the old session.
    /// Production is 10s (`REPLACE_QUIET`); tests set a shorter one.
    #[doc(hidden)]
    pub replace_quiet: Duration,
    /// How soon a DHT lookup is repeated in the first two minutes, while no one is
    /// connected. Production is 15s (`EARLY_LOOKUP_EVERY`): two copies started together
    /// announce at the same moment and find each other only on the next lookup, so
    /// tests on a local DHT set a shorter one.
    #[doc(hidden)]
    pub early_lookup_every: Duration,
}

impl Config {
    pub fn new(room: RoomCode) -> Config {
        Config {
            room,
            port: 0,
            peers: Vec::new(),
            dht: true,
            bootstrap: dht::BOOTSTRAP.iter().map(|s| s.to_string()).collect(),
            stun: stun::SERVERS.iter().map(|s| s.to_string()).collect(),
            node_cache: crate::sys::data_dir().map(|d| d.join("dht-nodes.txt")),
            allow_private: false,
            #[cfg(any(test, feature = "test-util"))]
            loss_percent: 0,
            public: None,
            portmap: !crate::loopback_only(),
            ipv6: true,
            room_key: None,
            replace_quiet: REPLACE_QUIET,
            early_lookup_every: disc::EARLY_LOOKUP_EVERY,
        }
    }

    /// Public sessions: anyone online with the same game who also has them on.
    pub fn public(p: public::PublicCfg) -> Config {
        let mut c = Config::new(RoomCode::public());
        c.node_cache = crate::sys::data_dir().map(|d| d.join("dht-nodes-public.txt"));
        c.public = Some(p);
        c
    }

    /// `PEERPARTY_PUBLIC=1` turns public sessions on (`Ok(None)`: off).
    /// `PEERPARTY_PUBLIC_PORT` picks their UDP port; `PEERPARTY_PUBLIC_NAME` the name
    /// shown; the DHT and STUN settings are shared with rooms.
    pub fn public_from_env() -> Result<Option<Config>, String> {
        if !env_var("PEERPARTY_PUBLIC").is_some_and(|v| env_on(&v)) {
            return Ok(None);
        }
        let mut p = public::PublicCfg::standard();
        // `PEERPARTY_PUBLIC_FRIENDS_ONLY=1`: only keys on the friends list connect.
        p.friends_only = env_var("PEERPARTY_PUBLIC_FRIENDS_ONLY").is_some_and(|v| env_on(&v));
        // The name public players see: chosen for them, else the one chosen in Settings.
        p.name = env_var("PEERPARTY_PUBLIC_NAME").or_else(|| env_var("PEERPARTY_NAME"));
        let mut c = Config::public(p);
        let var = env_var;
        if let Some(p) = var("PEERPARTY_PUBLIC_PORT") {
            c.port = p.parse().map_err(|_| format!("PEERPARTY_PUBLIC_PORT={p} isn't a port"))?;
        }
        c.apply_shared_env();
        Ok(Some(c))
    }

    fn apply_shared_env(&mut self) {
        let (var, list) = (env_var, env_list);
        if var("PEERPARTY_INET_DHT").as_deref() == Some("0") {
            self.dht = false;
        }
        if let Some(b) = var("PEERPARTY_INET_BOOTSTRAP") {
            self.bootstrap = list(b);
        }
        if let Some(s) = var("PEERPARTY_INET_STUN") {
            self.stun = list(s);
        }
        if var("PEERPARTY_INET_PORTMAP").as_deref() == Some("0") {
            self.portmap = false;
        }
        if var("PEERPARTY_IPV6").as_deref() == Some("0") {
            self.ipv6 = false;
        }
    }

    /// From the environment: `PEERPARTY_ROOM` turns internet play on. `Ok(None)` when it's
    /// off, `Err` for a code that doesn't read.
    ///
    /// - `PEERPARTY_INET_PORT`: our UDP port (default: any)
    /// - `PEERPARTY_INET_PEERS`: addresses to punch, comma-separated `host:port`
    /// - `PEERPARTY_INET_DHT=0`: no DHT (only `PEERPARTY_INET_PEERS`)
    /// - `PEERPARTY_INET_BOOTSTRAP`, `PEERPARTY_INET_STUN`: other servers (`0`: none)
    pub fn from_env() -> Result<Option<Config>, String> {
        let (var, list) = (env_var, env_list);
        let Some(code) = var("PEERPARTY_ROOM") else { return Ok(None) };
        if code == "0" || code.eq_ignore_ascii_case("off") {
            return Ok(None);
        }
        let room = RoomCode::parse(&code).ok_or("PEERPARTY_ROOM isn't a valid room code (peerparty --new-room makes one)")?;
        let mut c = Config::new(room);
        if let Some(p) = var("PEERPARTY_INET_PORT") {
            c.port = p.parse().map_err(|_| format!("PEERPARTY_INET_PORT={p} isn't a port"))?;
        }
        if let Some(p) = var("PEERPARTY_INET_PEERS") {
            c.peers = list(p);
        }
        c.apply_shared_env();
        Ok(Some(c))
    }
}

/// A setting from the environment: trimmed, `None` when unset or empty. The internet
/// layer's settings are read here alone (`Config::from_env`, `Config::public_from_env`).
fn env_var(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// A list setting ("a, b; c"); "0" is an empty list.
fn env_list(v: String) -> Vec<String> {
    if v == "0" {
        return Vec::new();
    }
    v.split([',', ';', ' ']).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

/// A switch that's on: 1/on/yes/true.
fn env_on(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "1" | "on" | "yes" | "true")
}

/// What the internet layer needs from the engine.
pub trait Host: Send + Sync + 'static {
    fn me(&self) -> PeerId;
    fn name(&self) -> String;
    fn game(&self) -> String;
    /// Whether the mesh already has this peer (over the LAN, or a tunnel).
    fn knows(&self, id: PeerId) -> bool;
    /// The mesh's TCP listener port on this PC.
    fn mesh_port(&self) -> u16;
    /// Asks the mesh to dial a loopback port that a tunnel to `id` listens on.
    fn dial_local(&self, port: u16, id: PeerId);
    /// A tunnel is about to carry the mesh connection whose far end (as our mesh sees
    /// it) is `mesh`, a loopback address. Called before any of the peer's bytes pass.
    fn tunnel_up(&self, mesh: SocketAddr, t: Tunnel);
    /// The tunnel under that mesh connection ended; `leaving`: its peer left for good
    /// (said so, or we dropped it), so it isn't waited for.
    fn tunnel_down(&self, mesh: SocketAddr, leaving: bool);
    /// The tunnel's bridge has handed our mesh everything that came and closed its end
    /// of that mesh connection: the connection should end now, and is cut if it doesn't.
    fn tunnel_closed(&self, mesh: SocketAddr);
    fn log(&self, line: &str);
}

/// What the engine learns about a mesh connection that runs through a tunnel. No
/// address: the engine (and so the overlay and games) never gets the peer's.
#[derive(Clone, Debug, PartialEq)]
pub struct Tunnel {
    pub id: PeerId,
    /// A public session (else a room).
    pub public: bool,
    /// The player's key fingerprint (public sessions).
    pub key: Option<String>,
    pub name: String,
}

/// One peer as the internet layer sees it.
#[derive(Clone, Debug)]
pub struct PeerStatus {
    /// The peer's address as logs show it (`redact::addr`): a tag unless debugging.
    pub tag: String,
    /// Its key fingerprint (public sessions).
    pub key: Option<String>,
    pub id: Option<PeerId>,
    pub name: String,
    /// The handshake finished.
    pub connected: bool,
    /// The mesh connection runs through this tunnel.
    pub bridged: bool,
    pub rtt: Option<Duration>,
    pub resent: u64,
    /// The tunnel runs over a direct IPv6 path (else IPv4).
    pub ipv6: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    /// The room's fingerprint (never its code).
    pub room: String,
    /// Public sessions (else a room).
    pub public_mode: bool,
    /// Our key's fingerprint (public sessions).
    pub identity: Option<String>,
    /// Players on the block list.
    pub blocked: usize,
    pub local_port: u16,
    /// Whether we know our address on the internet (from STUN or the DHT), as logs
    /// show it: hidden but for the port unless debugging.
    pub public: Option<String>,
    pub nat: Nat,
    /// The router forwarding our port: how, and the external port (never the address).
    pub port_map: Option<String>,
    pub dht_replies: u64,
    pub dht_nodes: usize,
    pub candidates: usize,
    pub peers: Vec<PeerStatus>,
}

struct Shared {
    cfg: Config,
    host: Arc<dyn Host>,
    sock: UdpSocket,
    /// IPv6 (data packets only: sessions start over IPv4), when there is one.
    sock6: Option<UdpSocket>,
    /// Our IPv6 addresses as F_ADDRS6 sends them, and when they were looked up.
    own6: Mutex<(Option<Instant>, Vec<SocketAddr>)>,
    local_port: u16,
    me: PeerInfo,
    room_id: [u8; 8],
    init_key: [u8; 32],
    psk: [u8; 32],
    stop: AtomicBool,
    sessions: Mutex<HashMap<SocketAddr, Session>>,
    /// Signalled (with `sessions`) when a session's stream may have room again: a packet
    /// came (acks), a tick went by, or we're stopping (`bridge`'s readers wait on it).
    drained: Condvar,
    workers: Arc<Workers>,
    naps: Naps,
    disc: Mutex<Disc>,
    ended: Mutex<Vec<(SocketAddr, bool)>>,
    next_gen: Mutex<u64>,
    /// Public sessions.
    pubs: Option<PubState>,
    /// Handshakes per internet address (public sessions).
    limit: Mutex<public::RateLimit>,
    max_sessions: usize,
    /// Addresses of blocked players, ignored for a while (a leaf lock).
    blocked_addrs: Mutex<HashMap<SocketAddr, Instant>>,
    /// Internet addresses of blocked players: no new keys from them for a while (a leaf lock).
    cooldown: Mutex<HashMap<IpAddr, Instant>>,
    /// The addresses players were dropped from when blocked (newest last, at most
    /// RECENT_BLOCKS), so an unblock forgives them too (a leaf lock).
    recent_blocks: Mutex<Vec<(String, SocketAddr)>>,
    /// The port the router forwards to us, and what the last try said (leaf locks).
    mapping: Mutex<Option<portmap::Mapping>>,
    map_note: Mutex<Option<String>>,
    /// No more mappings: stopping, or the game exits.
    stop_mapping: AtomicBool,
    /// Addresses whose handshake was turned away with every session in use, logged once
    /// each (a leaf lock).
    full_logged: Mutex<HashSet<SocketAddr>>,
    /// Rooms' signing key and pins (None for public sessions, which have `pubs`).
    room_sig: Option<RoomSig>,
    /// Our sockets' ports are ours, not the game's: the typed-IP forwarder never delivers
    /// a peer's traffic to them (see `own`).
    _own: Vec<crate::own::Held>,
    /// Public sessions' cookies (COOKIE_FOR): the key, the time they count from, and
    /// how many go out (one bucket for all addresses).
    cookie_key: [u8; 32],
    cookie_t0: Instant,
    cookies: Mutex<public::RateLimit>,
}

/// A room's signing key (see Config::room_key).
struct RoomSig {
    key: Arc<public::IdentityKey>,
}

struct PubState {
    key: Arc<public::IdentityKey>,
    fp: String,
    blocked: Arc<public::Blocklist>,
    pins: Arc<public::Pins>,
    friends: Arc<public::Blocklist>,
    friends_only: bool,
}

/// The internet layer's threads, counted while they run so `Inet::stop` can wait for
/// them: once they have, nothing but the `Inet` holds the sockets, and dropping it closes
/// them (its port is free at once for the next start).
#[derive(Default)]
struct Workers {
    running: Mutex<usize>,
    idle: Condvar,
    handles: Mutex<Vec<thread::JoinHandle<()>>>,
}

/// Counts a worker out when its thread ends (after everything it held is dropped).
struct Done(Arc<Workers>);

impl Drop for Done {
    fn drop(&mut self) {
        let mut n = self.0.running.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
        if *n == 0 {
            self.0.idle.notify_all();
        }
    }
}

impl Workers {
    /// Waits up to `limit` for every worker to end; whether they did.
    fn wait_idle(&self, limit: Duration) -> bool {
        let n = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let (n, _) = self.idle.wait_timeout_while(n, limit, |n| *n > 0).unwrap_or_else(|e| e.into_inner());
        *n == 0
    }
}

/// How the layer's periodic threads sleep: woken at once when it stops.
#[derive(Default)]
struct Naps {
    stopping: Mutex<bool>,
    wake: Condvar,
}

impl Naps {
    /// Sleeps `d`, or until `stop`; whether we're stopping.
    fn nap(&self, d: Duration) -> bool {
        let g = self.stopping.lock().unwrap_or_else(|e| e.into_inner());
        if *g {
            return true;
        }
        *self.wake.wait_timeout(g, d).unwrap_or_else(|e| e.into_inner()).0
    }

    fn stop(&self) {
        *self.stopping.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.notify_all();
    }
}

/// Runs `f` on a thread of its own, counted in `sh.workers` (see `Workers`).
fn spawn(sh: &Arc<Shared>, name: &str, f: impl FnOnce(Arc<Shared>) + Send + 'static) -> io::Result<()> {
    let w = sh.workers.clone();
    *w.running.lock().unwrap() += 1;
    let done = Done(w.clone());
    let sh = sh.clone();
    match thread::Builder::new().name(name.into()).spawn(move || {
        let _done = done;
        f(sh);
    }) {
        Ok(h) => {
            let mut hs = w.handles.lock().unwrap();
            hs.retain(|h| !h.is_finished());
            hs.push(h);
            Ok(())
        }
        // The closure (and its count) went with the failed spawn.
        Err(e) => Err(e),
    }
}

impl Shared {
    fn mode(&self) -> Mode {
        if self.pubs.is_some() { Mode::Public } else { Mode::Room }
    }

    /// Wakes the threads blocked receiving on our sockets (they see `stop`).
    fn wake_receivers(&self) {
        let _ = self.sock.send_to(&[0], (Ipv4Addr::LOCALHOST, self.local_port));
        if let Some(s) = &self.sock6
            && let Ok(a) = s.local_addr()
        {
            let _ = s.send_to(&[0], (Ipv6Addr::LOCALHOST, a.port()));
        }
    }
    fn new_keys_cooling(&self, ip: IpAddr, now: Instant) -> bool {
        let mut c = self.cooldown.lock().unwrap();
        c.retain(|_, until| *until > now);
        c.contains_key(&public::limit_key(ip))
    }

    fn is_blocked_addr(&self, a: SocketAddr, now: Instant) -> bool {
        let mut b = self.blocked_addrs.lock().unwrap();
        b.retain(|_, until| *until > now);
        b.contains_key(&a)
    }

    /// Ignores a blocked player's address a while, remembered under its fingerprint so
    /// unblocking forgives it (whichever way the block found it).
    fn block_addr(&self, a: SocketAddr, fp: Option<&str>, now: Instant) {
        if let Some(fp) = fp.and_then(public::normalize_fingerprint) {
            let mut r = self.recent_blocks.lock().unwrap();
            if r.len() >= RECENT_BLOCKS {
                r.remove(0);
            }
            r.push((fp, a));
        }
        let mut b = self.blocked_addrs.lock().unwrap();
        if b.len() < 4096 {
            b.insert(a, now + BLOCKED_FOR);
        }
        drop(b);
        let mut c = self.cooldown.lock().unwrap();
        if c.len() < 4096 {
            c.insert(public::limit_key(a.ip()), now + NEW_KEY_COOLDOWN);
        }
    }

    fn cx(&self) -> Ctx<'_> {
        Ctx {
            sock: &self.sock,
            sock6: self.sock6.as_ref(),
            allow_private: self.cfg.allow_private,
            room_id: self.room_id,
            init_key: &self.init_key,
            psk: &self.psk,
            me: &self.me,
            #[cfg(any(test, feature = "test-util"))]
            loss: self.cfg.loss_percent,
            signer: Some(match (&self.pubs, &self.room_sig) {
                (Some(p), _) => &*p.key,
                (None, Some(r)) => &*r.key,
                (None, None) => unreachable!("a room always has a key"),
            }),
        }
    }

    fn log(&self, s: &str) {
        self.host.log(&format!("inet {s}"));
    }

    /// Our IPv6 addresses for peers: the one the OS would use toward the internet (when
    /// global), and loopback in tests. Looked up once a minute.
    fn own_v6(&self, now: Instant) -> Vec<SocketAddr> {
        let Some(port) = self.sock6.as_ref().and_then(|s| s.local_addr().ok()).map(|a| a.port()) else {
            return Vec::new();
        };
        let mut c = self.own6.lock().unwrap();
        if c.0.is_some_and(|t| now.duration_since(t) < ADVERT_EVERY) {
            return c.1.clone();
        }
        let mut v = Vec::new();
        if !crate::loopback_only()
            && let Ok(u) = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))
            && u.connect((Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888), 53)).is_ok()
            && let Ok(a) = u.local_addr()
        {
            let a = SocketAddr::new(a.ip(), port);
            if usable_v6(a, false) {
                v.push(a);
            }
        }
        if self.cfg.allow_private {
            v.push(SocketAddr::from((Ipv6Addr::LOCALHOST, port)));
        }
        *c = (Some(now), v.clone());
        v
    }

    fn epoch(&self) -> u64 {
        let mut g = self.next_gen.lock().unwrap();
        *g += 1;
        *g
    }

    /// The cookie for an INIT with `blob` from `from`, in time slot `slot`.
    fn cookie(&self, from: SocketAddr, blob: &[u8; 48], slot: u64) -> [u8; 16] {
        crypto::mac16(&self.cookie_key, &[&cookie_input(from, blob, slot)])
    }

    fn cookie_slot(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.cookie_t0).as_secs() / COOKIE_FOR.as_secs()
    }

    /// Whether `c` is the cookie we gave `from` for `blob`, this time slot or the last.
    fn cookie_ok(&self, from: SocketAddr, blob: &[u8; 48], c: &[u8], now: Instant) -> bool {
        let slot = self.cookie_slot(now);
        [Some(slot), slot.checked_sub(1)].into_iter().flatten().any(|s| crypto::mac16_ok(&self.cookie_key, &[&cookie_input(from, blob, s)], c))
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.psk.zeroize();
        self.init_key.zeroize();
        self.cookie_key.zeroize();
    }
}

/// The running internet layer. Dropping it doesn't stop it: call `stop`.
pub struct Inet {
    sh: Arc<Shared>,
}

/// The internet sockets' receive and send buffers (asked for; 64 KB by default on
/// Windows, a few dozen full packets).
const UDP_BUFFERS: i32 = 1 << 20;

impl Inet {
    pub fn start(cfg: Config, host: Arc<dyn Host>) -> io::Result<Inet> {
        let bind = if crate::loopback_only() { Ipv4Addr::LOCALHOST } else { Ipv4Addr::UNSPECIFIED };
        let sock = UdpSocket::bind((bind, cfg.port))?;
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        // Every session's packets, the DHT's and STUN's share this socket.
        crate::sys::udp_buffers(&sock, UDP_BUFFERS);
        let local_port = sock.local_addr()?.port();
        let sock6 = if cfg.ipv6 {
            let b6 = if crate::loopback_only() { Ipv6Addr::LOCALHOST } else { Ipv6Addr::UNSPECIFIED };
            UdpSocket::bind((b6, local_port))
                .or_else(|_| UdpSocket::bind((b6, 0)))
                .ok()
                .filter(|s| s.set_read_timeout(Some(Duration::from_millis(200))).is_ok())
                .inspect(|s| crate::sys::udp_buffers(s, UDP_BUFFERS))
        } else {
            None
        };
        let now = Instant::now();
        let pubs = cfg.public.as_ref().map(|p| {
            let key = p.key.clone().unwrap_or_else(|| Arc::new(public::IdentityKey::generate()));
            let fp = key.fingerprint();
            PubState {
                key,
                fp,
                blocked: p.blocked.clone(),
                pins: p.pins.clone(),
                friends: p.friends.clone(),
                friends_only: p.friends_only,
            }
        });
        let room_sig = pubs.is_none().then(|| RoomSig {
            key: cfg.room_key.clone().unwrap_or_else(|| Arc::new(public::IdentityKey::generate())),
        });
        let name = match (&pubs, &cfg.public) {
            (Some(ps), Some(p)) => public::our_name(p.name.as_deref(), &ps.fp),
            _ => host.name(),
        };
        let me = PeerInfo { id: host.me(), game: host.game(), name };
        let max_sessions = if pubs.is_some() { PUBLIC_MAX_SESSIONS } else { MAX_SESSIONS };
        let mut dht = cfg.dht.then(|| Dht::new(Vec::new(), load_nodes(&cfg.node_cache)));
        if let Some(d) = &mut dht {
            d.allow_private = cfg.allow_private;
        }
        let mut own = vec![crate::own::udp(local_port)];
        if let Some(p) = sock6.as_ref().and_then(|s| s.local_addr().ok()).map(|a| a.port()) {
            own.push(crate::own::udp(p));
        }
        let sh = Arc::new(Shared {
            _own: own,
            room_id: cfg.room.room_id(),
            init_key: cfg.room.init_key(),
            psk: cfg.room.psk(),
            disc: Mutex::new(Disc {
                dht,
                next_lookup: now,
                started: now,
                stun_servers: Vec::new(),
                stun_tx: HashMap::new(),
                stun_mapped: HashMap::new(),
                next_stun: now,
                cands: HashMap::new(),
                statics: Vec::new(),
                selves: HashSet::new(),
                banned: HashMap::new(),
                given_up: HashMap::new(),
                next_resolve: now,
                next_save: now + Duration::from_secs(60),
            }),
            cfg,
            host,
            sock,
            sock6,
            own6: Mutex::new((None, Vec::new())),
            local_port,
            me,
            stop: AtomicBool::new(false),
            sessions: Mutex::new(HashMap::new()),
            drained: Condvar::new(),
            workers: Arc::new(Workers::default()),
            naps: Naps::default(),
            ended: Mutex::new(Vec::new()),
            next_gen: Mutex::new(0),
            pubs,
            limit: Mutex::new(public::RateLimit::new(PUBLIC_INIT_BURST, PUBLIC_INIT_RATE)),
            max_sessions,
            blocked_addrs: Mutex::new(HashMap::new()),
            cooldown: Mutex::new(HashMap::new()),
            recent_blocks: Mutex::new(Vec::new()),
            mapping: Mutex::new(None),
            map_note: Mutex::new(None),
            stop_mapping: AtomicBool::new(false),
            full_logged: Mutex::new(HashSet::new()),
            room_sig,
            cookie_key: crypto::random_array(),
            cookie_t0: now,
            cookies: Mutex::new(public::RateLimit::new(COOKIE_BURST, COOKIE_RATE)),
        });
        if let Some(p) = &sh.pubs {
            sh.log(&format!("public sessions: we are {} ({})", sh.me.name, p.fp));
        }
        sh.log(&format!(
            "started: room {} udp {local_port}, ipv6 {}, dht {}, {} static peer(s)",
            sh.cfg.room.fingerprint(),
            sh.sock6.as_ref().and_then(|s| s.local_addr().ok()).map_or("off".to_string(), |a| format!("udp {}", a.port())),
            if sh.cfg.dht { "on" } else { "off" },
            sh.cfg.peers.len()
        ));
        if sh.cfg.dht && sh.cfg.early_lookup_every != disc::EARLY_LOOKUP_EVERY {
            sh.log(&format!("dht: lookups every {:?} at first (a test's; {:?} normally)", sh.cfg.early_lookup_every, disc::EARLY_LOOKUP_EVERY));
        }
        let inet = Inet { sh };
        // A thread that doesn't start stops the ones that did (and frees the ports).
        let started = (|| {
            let sh = &inet.sh;
            if sh.sock6.is_some() {
                spawn(sh, "peerparty-inet-recv6", |sh| recv6_loop(&sh))?;
            }
            spawn(sh, "peerparty-inet-recv", |sh| recv_loop(&sh))?;
            spawn(sh, "peerparty-inet-tick", |sh| tick_loop(&sh))?;
            spawn(sh, "peerparty-inet-disc", |sh| disc_loop(&sh))?;
            if sh.cfg.portmap {
                spawn(sh, "peerparty-inet-portmap", |sh| portmap_loop(&sh))?;
            }
            io::Result::Ok(())
        })();
        if let Err(e) = started {
            inet.stop();
            return Err(e);
        }
        Ok(inet)
    }

    pub fn local_port(&self) -> u16 {
        self.sh.local_port
    }

    pub fn status(&self) -> Status {
        let mut st = Status {
            room: self.sh.cfg.room.fingerprint(),
            public_mode: self.sh.pubs.is_some(),
            identity: self.sh.pubs.as_ref().map(|p| p.fp.clone()),
            blocked: self.sh.pubs.as_ref().map_or(0, |p| p.blocked.len()),
            local_port: self.sh.local_port,
            ..Default::default()
        };
        {
            let d = self.sh.disc.lock().unwrap();
            let mapped: Vec<SocketAddr> = d.stun_mapped.values().copied().collect();
            st.nat = Nat::classify(None, &mapped);
            st.public = mapped.first().copied().or(d.dht.as_ref().and_then(|x| x.external)).map(redact::own);
            if let Some(x) = &d.dht {
                st.dht_replies = x.replies;
                st.dht_nodes = x.good_nodes().len();
            }
            st.candidates = d.cands.len();
        }
        st.port_map = self.sh.map_note.lock().unwrap().clone();
        let s = self.sh.sessions.lock().unwrap();
        st.peers = s.values().map(Session::status).collect();
        st.peers.sort_by(|a, b| a.tag.cmp(&b.tag));
        st
    }

    /// Our public key's fingerprint (public sessions).
    pub fn identity(&self) -> Option<String> {
        self.sh.pubs.as_ref().map(|p| p.fp.clone())
    }

    /// Our name as public players see it.
    pub fn name(&self) -> &str {
        &self.sh.me.name
    }

    /// Puts a player on the block list and ends any session with it. Returns whether
    /// it's newly blocked; errors for a bad fingerprint or when not in public mode.
    pub fn block(&self, fp: &str, note: &str) -> io::Result<bool> {
        let Some(p) = &self.sh.pubs else {
            return Err(io::Error::other("not in public sessions"));
        };
        let added = p.blocked.add(fp, note)?;
        self.kick_blocked();
        Ok(added)
    }

    /// Takes a player off the block list, and forgives the addresses it was dropped
    /// from (so it can come back at once). Whether it was on the list.
    pub fn unblock(&self, fp: &str) -> io::Result<bool> {
        let Some(p) = &self.sh.pubs else {
            return Err(io::Error::other("not in public sessions"));
        };
        let removed = p.blocked.remove(fp)?;
        let norm = public::normalize_fingerprint(fp).unwrap_or_default();
        let addrs: Vec<SocketAddr> = {
            let mut r = self.sh.recent_blocks.lock().unwrap();
            let addrs = r.iter().filter(|(f, _)| *f == norm).map(|(_, a)| *a).collect();
            r.retain(|(f, _)| *f != norm);
            addrs
        };
        let now = Instant::now();
        for a in &addrs {
            self.sh.blocked_addrs.lock().unwrap().remove(a);
            self.sh.cooldown.lock().unwrap().remove(&public::limit_key(a.ip()));
            let mut d = self.sh.disc.lock().unwrap();
            d.banned.remove(a);
            // Punched again now, not when its schedule from before the block comes round
            // (a punch's PUNCH_FOR, or a back-off): the player is back at once even if
            // its own side has backed off or given up on us.
            let given_up = d.given_up.remove(a).is_some();
            match d.cands.get_mut(a) {
                Some(c) => {
                    c.next_try = now;
                    c.fails = 0;
                }
                None if given_up => {
                    d.cands.insert(*a, disc::Cand::new(true, now));
                }
                None => {}
            }
        }
        if !addrs.is_empty() {
            self.sh.log(&format!("unblocked {norm}: punching its {} address(es) again now", addrs.len()));
        }
        Ok(removed)
    }

    /// Adds a player to the friends list (friends-only public sessions). Whether it's new.
    pub fn befriend(&self, fp: &str, note: &str) -> io::Result<bool> {
        let Some(p) = &self.sh.pubs else {
            return Err(io::Error::other("not in public sessions"));
        };
        p.friends.add(fp, note)
    }

    /// Ends the sessions of players on the block list (after it changed).
    pub fn kick_blocked(&self) {
        kick_blocked(&self.sh);
    }

    /// Deletes the router's port mapping (the game is exiting). Never blocks on a lock.
    pub fn release_port_map(&self) {
        self.sh.stop_mapping.store(true, Ordering::Relaxed);
        if let Ok(mut m) = self.sh.mapping.try_lock()
            && let Some(m) = m.take()
        {
            m.remove();
        }
    }

    /// Says goodbye to every peer, drops the tunnels and stops the threads, waiting (a
    /// moment at most) for them to end: then dropping this `Inet` closes its sockets.
    pub fn stop(&self) {
        if self.sh.stop.swap(true, Ordering::Relaxed) {
            return;
        }
        self.sh.naps.stop();
        self.release_port_map();
        let now = Instant::now();
        let gone: Vec<Session> = {
            let mut s = self.sh.sessions.lock().unwrap();
            let cx = self.sh.cx();
            for v in s.values_mut().filter(|v| v.confirmed) {
                v.send_close(&cx, now, true);
            }
            s.drain().map(|(_, v)| v).collect()
        };
        for v in gone {
            if let Some(b) = &v.bridge {
                b.shut();
                self.sh.host.tunnel_down(b.mesh, true);
            }
        }
        {
            let d = self.sh.disc.lock().unwrap();
            if let Some(x) = &d.dht {
                save_nodes(&self.sh.cfg.node_cache, &x.good_nodes());
            }
        }
        self.sh.drained.notify_all();
        self.sh.wake_receivers();
        if self.sh.workers.wait_idle(STOP_WAIT) {
            for h in std::mem::take(&mut *self.sh.workers.handles.lock().unwrap()) {
                let _ = h.join();
            }
        } else {
            self.sh.log("stopping: a thread is still busy; not waiting for it");
        }
    }
}

fn recv_loop(sh: &Arc<Shared>) {
    let mut buf = vec![0u8; 65536];
    while !sh.stop.load(Ordering::Relaxed) {
        let Ok((n, from)) = sh.sock.recv_from(&mut buf) else { continue };
        let d = &buf[..n];
        if d.first() == Some(&MAGIC) {
            if d.len() >= HEAD && d[1] == PROTO {
                let acts = {
                    let mut s = sh.sessions.lock().unwrap();
                    on_packet(sh, &mut s, from, d, Instant::now())
                };
                sh.drained.notify_all();
                run(sh, acts);
            }
        } else if stun::is_stun(d) {
            if let Some((txid, mapped)) = stun::parse_response(d) {
                let mut disc = sh.disc.lock().unwrap();
                if let Some(server) = disc.stun_tx.remove(&txid) {
                    if disc.stun_mapped.insert(server, mapped) != Some(mapped) {
                        sh.log(&format!("stun: {} sees us as {}", redact::addr(server), redact::own(mapped)));
                    }
                    disc.selves.insert(mapped);
                }
            }
        } else if dht::is_dht(d) {
            let mut disc = sh.disc.lock().unwrap();
            if let Some(x) = &mut disc.dht {
                let before = x.external;
                x.handle(from, d);
                if let Some(e) = x.external
                    && before != Some(e)
                {
                    disc.selves.insert(e);
                }
            }
        }
    }
}

/// The IPv6 socket: data packets for sessions that began over IPv4 (on_packet matches
/// them by the addresses the peer sent), and handshakes for sessions that run over IPv6
/// from the start (an IPv6 address in PEERPARTY_INET_PEERS).
fn recv6_loop(sh: &Arc<Shared>) {
    let Some(sock) = &sh.sock6 else { return };
    let mut buf = vec![0u8; 65536];
    while !sh.stop.load(Ordering::Relaxed) {
        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
        let d = &buf[..n];
        if d.len() >= HEAD && d[0] == MAGIC && d[1] == PROTO && matches!(d[2], T_INIT | T_CONFIRM | T_DATA | T_COOKIE) {
            let acts = {
                let mut s = sh.sessions.lock().unwrap();
                on_packet(sh, &mut s, from, d, Instant::now())
            };
            sh.drained.notify_all();
            run(sh, acts);
        }
    }
}

fn tick_loop(sh: &Arc<Shared>) {
    while !sh.stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let acts = {
            let mut ss = sh.sessions.lock().unwrap();
            tick_sessions(sh, &mut ss, now)
        };
        run(sh, acts);
        sh.drained.notify_all();
        if sh.naps.nap(TICK) {
            return;
        }
    }
}

fn run(sh: &Arc<Shared>, acts: Vec<Act>) {
    for a in acts {
        match a {
            Act::Bridge(addr, epoch, id) => start_bridge(sh.clone(), addr, epoch, id),
            Act::Gone(bridge, leaving) => {
                if let Some(b) = bridge {
                    // Not cut: the writer still hands our mesh what already came from
                    // the peer, then closes (dropping the bridge ends its queue).
                    b.closed.store(true, Ordering::Relaxed);
                    sh.host.tunnel_down(b.mesh, leaving);
                }
            }
            Act::Log(s) => sh.log(&s),
            Act::Ended(addr, up) => sh.ended.lock().unwrap().push((addr, up)),
            Act::Heard(addr) => {
                let now = Instant::now();
                let mut d = sh.disc.lock().unwrap();
                d.given_up.remove(&addr);
                if d.cands.len() < MAX_CANDS || d.cands.contains_key(&addr) {
                    let c = d.cands.entry(addr).or_insert_with(|| Cand::new(false, now));
                    c.answered = true;
                    c.fails = 0;
                    c.seen = now;
                }
            }
            Act::Ban(addr) => {
                sh.disc.lock().unwrap().banned.insert(addr, Instant::now() + Duration::from_secs(600));
            }
            Act::RememberPin(id, key) => {
                if let Some(p) = &sh.pubs {
                    p.pins.remember(id, &key);
                }
            }
        }
    }
}

/// Builds handshake packets the way a peer (or an attacker who knows the room) would:
/// for tests that throw forged and tampered ones at a running engine.
#[cfg(any(test, feature = "test-util"))]
#[doc(hidden)]
pub mod testing {
    use super::*;
    use crate::wire::Msg;

    pub fn init(room: &RoomCode, eph: &crypto::Ephemeral) -> Vec<u8> {
        init_with(room, &eph.blob)
    }

    /// An INIT carrying any 48 bytes as its key and nonce, properly MACed (anyone can
    /// make these for public sessions).
    pub fn init_with(room: &RoomCode, blob: &[u8; 48]) -> Vec<u8> {
        let mut p = head(T_INIT).to_vec();
        p.extend_from_slice(&room.room_id());
        p.extend_from_slice(blob);
        let tag = crypto::mac16(&room.init_key(), &[&p]);
        p.extend_from_slice(&tag);
        p
    }

    pub fn peer_info(id: PeerId, game: &str, name: &str) -> Vec<u8> {
        PeerInfo { id, game: game.into(), name: name.into() }.encode()
    }

    /// The frame for `data`: a mesh datagram that says `as_peer` sent `bytes` on `socket`.
    pub fn forged_datagram(as_peer: PeerId, socket: &str, bytes: &[u8]) -> Vec<u8> {
        let msg = crate::wire::Msg::ChanData { socket: socket.into(), channel: 0, reliable: false, data: bytes.to_vec() };
        let d = crate::mesh::unreliable_datagram(as_peer, None, &msg);
        let mut f = vec![F_DGRAM];
        f.extend_from_slice(&(d.len() as u16).to_le_bytes());
        f.extend_from_slice(&d);
        f
    }

    /// The INIT to send again, with its cookie, when `d` is a cookie for `eph`.
    pub fn init_answering(room: &RoomCode, eph: &crypto::Ephemeral, d: &[u8]) -> Option<Vec<u8>> {
        Some(init_with_cookie(room, &eph.blob, &cookie_in(room, &eph.blob, d)?))
    }

    /// `init_answering`'s INIT, but for `blob` and with any 16 bytes as the cookie.
    pub fn init_with_cookie(room: &RoomCode, blob: &[u8; 48], cookie: &[u8; 16]) -> Vec<u8> {
        let mut p = head(T_INIT).to_vec();
        p.extend_from_slice(&room.room_id());
        p.extend_from_slice(blob);
        p.extend_from_slice(cookie);
        let tag = crypto::mac16(&room.init_key(), &[&p]);
        p.extend_from_slice(&tag);
        p
    }

    /// The cookie in `d`, if it's one answering an INIT with `blob`.
    pub fn cookie_in(room: &RoomCode, blob: &[u8; 48], d: &[u8]) -> Option<[u8; 16]> {
        if d.len() != COOKIE_LEN || d[..HEAD] != head(T_COOKIE) || d[HEAD..HEAD + 8] != room.room_id() || d[HEAD + 8..HEAD + 56] != blob[..] {
            return None;
        }
        d[HEAD + 56..].try_into().ok()
    }

    /// From a CONFIRM we got: the sender's blob, if it's one for `eph`.
    pub fn confirm_sender(room: &RoomCode, eph: &crypto::Ephemeral, d: &[u8]) -> Option<[u8; 48]> {
        if d.len() < CONFIRM_AD + 16 || d[..HEAD] != head(T_CONFIRM) || d[HEAD..HEAD + 8] != room.room_id() {
            return None;
        }
        if d[HEAD + 56..CONFIRM_AD] != eph.blob[..] {
            return None;
        }
        d[HEAD + 8..HEAD + 56].try_into().ok()
    }

    /// A data packet in the session `eph` / `theirs`, sealed with counter `c`.
    pub fn data(room: &RoomCode, eph: &crypto::Ephemeral, theirs: &[u8; 48], c: u64, plain: &[u8]) -> Option<Vec<u8>> {
        let keys = crypto::Keys::derive(&room.psk(), eph, theirs)?;
        let mut p = head(T_DATA).to_vec();
        p.extend_from_slice(&c.to_le_bytes());
        let sealed = keys.seal(c, &p, plain);
        p.extend_from_slice(&sealed);
        Some(p)
    }

    /// The nonce in the hello the other side's mesh sent first, out of the data packets
    /// it came in (what a test player's own hello must be proved against).
    pub fn hello_nonce(room: &RoomCode, eph: &crypto::Ephemeral, theirs: &[u8; 48], packets: &[Vec<u8>]) -> Option<[u8; 16]> {
        let keys = crypto::Keys::derive(&room.psk(), eph, theirs)?;
        let mut segs = std::collections::BTreeMap::new();
        for d in packets {
            if d.len() < HEAD + 8 || d[..HEAD] != head(T_DATA) {
                continue;
            }
            let c = u64::from_le_bytes(d[HEAD..HEAD + 8].try_into().ok()?);
            let Some(plain) = keys.open(c, &d[..HEAD + 8], &d[HEAD + 8..]) else { continue };
            let mut p = &plain[..];
            while let Some(&f) = p.first() {
                p = &p[1..];
                let skip = match f {
                    F_SEG => {
                        let seq = u32::from_le_bytes(p.get(..4)?.try_into().ok()?);
                        let len = u16::from_le_bytes(p.get(4..6)?.try_into().ok()?) as usize;
                        segs.insert(seq, p.get(6..6 + len)?.to_vec());
                        6 + len
                    }
                    F_ACK => 12,
                    F_DGRAM => 2 + u16::from_le_bytes(p.get(..2)?.try_into().ok()?) as usize,
                    F_PING | F_PONG => 8,
                    F_ADDRS6 => 1 + *p.first()? as usize * 18,
                    _ => break,
                };
                p = p.get(skip..)?;
            }
        }
        let mut stream = Vec::new();
        for (i, (seq, data)) in segs.iter().enumerate() {
            if *seq != i as u32 {
                break;
            }
            stream.extend_from_slice(data);
        }
        let len = u32::from_le_bytes(stream.get(..4)?.try_into().ok()?) as usize;
        match Msg::decode(stream.get(4..4 + len)?) {
            Some(Msg::Hello { nonce, .. }) => Some(nonce),
            _ => None,
        }
    }

    /// A test player's mesh hello, framed: the first thing it puts on the stream (the
    /// side with the lower id is dialed by its own mesh and speaks first; the other
    /// waits for this).
    pub fn mesh_hello(key: &public::IdentityKey, game: &str, nonce: &[u8; 16]) -> Vec<u8> {
        mesh_hello_as(PeerId::of_key(&key.public()), &key.public(), game, nonce)
    }

    /// A framed mesh hello that says whatever it is told to: any id, with any key.
    pub fn mesh_hello_as(id: PeerId, key: &[u8; 32], game: &str, nonce: &[u8; 16]) -> Vec<u8> {
        framed(&Msg::Hello { version: crate::wire::VERSION, game: game.into(), id, name: "Fuzz".into(), udp: 0, key: *key, nonce: *nonce })
    }

    /// A framed mesh proof carrying `sig` (`IdentityKey::sign_hello` makes real ones).
    pub fn mesh_proof(sig: [u8; 64]) -> Vec<u8> {
        framed(&Msg::Proof { sig })
    }

    /// The id and nonce in a mesh hello (one frame's body, without its length).
    pub fn mesh_hello_read(body: &[u8]) -> Option<(PeerId, [u8; 16])> {
        match Msg::decode(body) {
            Some(Msg::Hello { id, nonce, .. }) => Some((id, nonce)),
            _ => None,
        }
    }

    fn framed(m: &Msg) -> Vec<u8> {
        let body = m.encode();
        let mut out = (body.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(&body);
        out
    }

    /// What a hostile public player puts on the mesh stream after its `mesh_hello`: the
    /// proof of it (it is who its key says: that much can't be faked; `owner_nonce` is
    /// `hello_nonce`), then `n` framed messages aimed at `owner`'s lobby `victim` (forged
    /// owner snapshots, closes and kicks, oversized attributes and presence, channel
    /// traffic for sockets nobody opened, a second hello), most mutated while still
    /// decoding, and last a frame that doesn't decode.
    #[allow(clippy::too_many_arguments)]
    pub fn hostile_mesh_stream(key: &public::IdentityKey, game: &str, nonce: &[u8; 16], owner: PeerId, owner_nonce: &[u8; 16], victim: &str, seed: u64, n: usize) -> Vec<u8> {
        use crate::lobby::{Attr, Lobby, Member, Query, Value};
        use crate::wire::{JoinResult, VERSION};
        let me = PeerId::of_key(&key.public());
        let mut x = seed | 1;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let frame = |out: &mut Vec<u8>, body: &[u8]| {
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(body);
        };
        let mut lobby = Lobby::new("eos.lobby");
        lobby.id = victim.into();
        lobby.owner = owner;
        lobby.attrs = vec![Attr { key: "host".into(), value: Value::Str("hijacked".into()), public: true }];
        lobby.members = vec![Member { id: me, attrs: Vec::new() }];
        let big = "A".repeat(20_000);
        let many: Vec<(String, String)> = (0..2000).map(|i| (format!("k{i}"), big[..i % 300].to_string())).collect();
        let seeds = [
            Msg::Hello { version: VERSION, game: game.into(), id: PeerId::random(), name: "again".into(), udp: 1, key: [3; 32], nonce: [4; 16] },
            Msg::Proof { sig: [5; 64] },
            Msg::Search { search: 1, query: Query::new("eos.lobby") },
            Msg::SearchReply { search: 1, lobbies: vec![lobby.clone(); 100] },
            Msg::Join { lobby: victim.into(), secret: String::new() },
            Msg::OwnerCheck { lobby: victim.into(), owner: me },
            Msg::OwnerHere { lobby: victim.into() },
            Msg::JoinReply { lobby: victim.into(), result: JoinResult::Ok, claim: Default::default() },
            Msg::State { lobby: lobby.clone(), secret: [0; 32] },
            Msg::Leave { lobby: victim.into() },
            Msg::MemberAttrs { lobby: victim.into(), attrs: vec![Attr { key: big.clone(), value: Value::Str(big.clone()), public: true }] },
            Msg::Closed { lobby: victim.into() },
            Msg::Kicked { lobby: victim.into() },
            Msg::ChanRequest { socket: "peerparty-ip:t:1".into() },
            Msg::ChanAccept { socket: "nobody".into() },
            Msg::ChanData { socket: "peerparty-ip:u".into(), channel: 255, reliable: true, data: vec![0; 4000] },
            Msg::ChanClose { socket: "nobody".into() },
            Msg::Presence { kv: many },
            Msg::Invite { connect: big.clone(), lobby: Some(lobby.clone()) },
        ]
        .map(|m| m.encode());
        let mut out = Vec::new();
        frame(&mut out, &Msg::Proof { sig: key.sign_hello(game, (me, nonce), (owner, owner_nonce)) }.encode());
        for _ in 0..n {
            let base = &seeds[(next() % seeds.len() as u64) as usize];
            let mut pick = base.clone();
            if next() % 3 != 0 {
                for _ in 0..8 {
                    let mut m = base.clone();
                    for _ in 0..1 + next() % 4 {
                        let i = (next() % m.len() as u64) as usize;
                        m[i] ^= 1 << (next() % 8);
                    }
                    if Msg::decode(&m).is_some() {
                        pick = m;
                        break;
                    }
                }
            }
            frame(&mut out, &pick);
        }
        frame(&mut out, &[0xEE; 40]);
        out
    }

    /// A data packet carrying stream segment `seq`.
    pub fn segment(room: &RoomCode, eph: &crypto::Ephemeral, theirs: &[u8; 48], c: u64, seq: u32, data: &[u8]) -> Option<Vec<u8>> {
        let mut f = vec![F_SEG];
        f.extend_from_slice(&seq.to_le_bytes());
        f.extend_from_slice(&(data.len() as u16).to_le_bytes());
        f.extend_from_slice(data);
        self::data(room, eph, theirs, c, &f)
    }

    pub fn confirm(room: &RoomCode, eph: &crypto::Ephemeral, theirs: &[u8; 48], body: &[u8]) -> Option<Vec<u8>> {
        let keys = crypto::Keys::derive(&room.psk(), eph, theirs)?;
        let mut p = head(T_CONFIRM).to_vec();
        p.extend_from_slice(&room.room_id());
        p.extend_from_slice(&eph.blob);
        p.extend_from_slice(theirs);
        let sealed = keys.seal(crypto::CONFIRM_COUNTER, &p, body);
        p.extend_from_slice(&sealed);
        Some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Msg;

    #[test]
    fn only_usable_ipv6_addresses_are_kept() {
        let v6 = |s: &str| -> SocketAddr { s.parse().unwrap() };
        assert!(usable_v6(v6("[2406:3400::1]:5000"), false));
        assert!(!usable_v6(v6("[2406:3400::1]:0"), false));
        assert!(!usable_v6(v6("[fe80::1]:5000"), true), "link-local");
        assert!(!usable_v6(v6("[2001:0:1::1]:5000"), false), "Teredo");
        assert!(!usable_v6(v6("[2001:db8::1]:5000"), false), "documentation");
        assert!(!usable_v6(v6("[::ffff:1.2.3.4]:5000"), true), "v4-mapped");
        assert!(!usable_v6(v6("[ff02::1]:5000"), true), "multicast");
        assert!(!usable_v6(v6("[::1]:5000"), false));
        assert!(usable_v6(v6("[::1]:5000"), true));
        assert!(!usable_v6(v6("[fd00::1]:5000"), false));
        assert!(usable_v6(v6("[fd00::1]:5000"), true));
        assert!(!usable_v6(v6("1.2.3.4:5000"), true));
    }

    #[test]
    fn a_peers_ipv6_addresses_are_checked_and_capped() {
        let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let sock6 = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).ok();
        let me = PeerInfo { id: PeerId::random(), game: "g".into(), name: "n".into() };
        let cx = |allow_private, with6: bool| Ctx {
            sock: &sock,
            sock6: if with6 { sock6.as_ref().or(Some(&sock)) } else { None },
            allow_private,
            room_id: [0; 8],
            init_key: &[0; 32],
            psk: &[0; 32],
            me: &me,
            loss: 0,
            signer: None,
        };
        let now = Instant::now();
        let v4: SocketAddr = "203.0.113.5:4000".parse().unwrap();
        let addrs: Vec<SocketAddr> = ["[fe80::1]:1", "[2406:3400::1]:2", "[::1]:3", "[2406:3400::2]:4", "[2406:3400::3]:5", "[2406:3400::4]:6", "[2406:3400::5]:7"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        // addrs6_frame caps what we send at MAX_ALT; build a longer one by hand.
        let mut f = vec![F_ADDRS6, addrs.len() as u8];
        for a in &addrs {
            let SocketAddr::V6(v) = a else { unreachable!() };
            f.extend_from_slice(&v.ip().octets());
            f.extend_from_slice(&v.port().to_le_bytes());
        }
        assert_eq!(addrs6_frame(&addrs).len(), 2 + MAX_ALT * 18);

        let mut s = Session::new(v4, 0, now, false, Mode::Room);
        assert!(frames(&mut s, &cx(false, true), &f, now).is_ok());
        assert_eq!(s.alt, addrs[1..].iter().filter(|a| a.port() != 3).take(MAX_ALT).copied().collect::<Vec<_>>());
        assert!(s.probe_until.is_some() && s.path == v4);

        let mut s = Session::new(v4, 0, now, false, Mode::Room);
        assert!(frames(&mut s, &cx(true, true), &f, now).is_ok());
        assert_eq!(s.alt[..2], addrs[1..3]);

        // No IPv6 socket here: ignored.
        let mut s = Session::new(v4, 0, now, false, Mode::Room);
        assert!(frames(&mut s, &cx(true, false), &f, now).is_ok());
        assert!(s.alt.is_empty() && s.probe_until.is_none());

        // Cut short: a bad frame ends the session.
        let mut s = Session::new(v4, 0, now, false, Mode::Room);
        assert!(frames(&mut s, &cx(true, true), &f[..f.len() - 1], now).is_err());
        assert!(frames(&mut s, &cx(true, true), &[F_ADDRS6], now).is_err());

        // The peer stopped listing the address we use: back to its IPv4 one.
        let mut s = Session::new(v4, 0, now, false, Mode::Room);
        s.path = addrs[1];
        assert!(frames(&mut s, &cx(false, true), &addrs6_frame(&addrs[3..4]), now).is_ok());
        assert_eq!((s.path, s.alt.clone()), (v4, vec![addrs[3]]));
    }

    #[test]
    fn candidates_rank_live_and_fresh_addresses_first() {
        let now = Instant::now();
        let later = now + Duration::from_secs(30);
        let mut stale = Cand::new(true, now);
        stale.fails = 2;
        let mut old_hour = Cand::new(true, later);
        old_hour.current = false;
        let mut this_hour_old = Cand::new(true, now);
        this_hour_old.current = true;
        let mut this_hour_new = Cand::new(true, later);
        this_hour_new.current = true;
        let statik = Cand::new(false, now);
        let mut answered = Cand::new(true, now);
        answered.answered = true;
        answered.fails = 1;
        let mut v = [&stale, &old_hour, &this_hour_old, &this_hour_new, &statik, &answered];
        v.sort_by_key(|c| c.rank());
        let order: Vec<*const Cand> = v.iter().map(|c| *c as *const Cand).collect();
        let want: Vec<*const Cand> =
            [&answered, &statik, &this_hour_new, &this_hour_old, &old_hour, &stale].iter().map(|c| *c as *const Cand).collect();
        assert_eq!(order, want);
        // DHT finds that never answered hold a punching slot briefly, less once failed.
        assert_eq!(Cand::new(true, now).punch_for(), PUNCH_DHT_FOR);
        assert_eq!(stale.punch_for(), PUNCH_RETRY_FOR);
        assert_eq!(statik.punch_for(), PUNCH_FOR);
        assert_eq!(answered.punch_for(), PUNCH_FOR);
    }

    #[test]
    fn peer_info_round_trips() {
        let i = PeerInfo { id: PeerId::random(), game: "steam:1".into(), name: "Robin".into() };
        assert_eq!(PeerInfo::decode(&i.encode()), Some(i.clone()));
        let e = i.encode();
        assert_eq!(PeerInfo::decode(&e[..e.len() - 1]), None);
    }

    #[test]
    fn hello_filter_rewrites_only_the_first_hello() {
        let id = PeerId::random();
        let hello = Msg::Hello { version: 2, game: "g".into(), id, name: "n".into(), udp: 5000, key: [6; 32], nonce: [7; 16] }.encode();
        let mut bytes = (hello.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&hello);
        bytes.extend_from_slice(b"rest");
        let mut f = HelloRewriter::default();
        // Fed a byte at a time, it holds back until the whole hello is in.
        let mut out = Vec::new();
        for b in &bytes {
            out.extend(peer_hello(&mut f, &[*b], id, 7777, None).unwrap().unwrap_or_else(|| vec![*b]));
        }
        let len = u32::from_le_bytes(out[..4].try_into().unwrap()) as usize;
        match Msg::decode(&out[4..4 + len]) {
            Some(Msg::Hello { udp, id: got, .. }) => {
                assert_eq!(udp, 7777);
                assert_eq!(got, id);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(&out[4 + len..], b"rest");
        assert_eq!(peer_hello(&mut f, b"more", id, 7777, None).unwrap(), None, "passes as it is");
        // Someone else's hello is refused.
        let mut f = HelloRewriter::default();
        assert!(peer_hello(&mut f, &bytes, PeerId::random(), 7777, None).is_err());
        let mut buf = Vec::new();
        assert_eq!(sniff_hello(&mut buf, &bytes[..3]), None);
        assert_eq!(sniff_hello(&mut buf, &bytes[3..]), Some(5000));
    }

    fn hello_name(out: &[u8]) -> String {
        let len = u32::from_le_bytes(out[..4].try_into().unwrap()) as usize;
        match Msg::decode(&out[4..4 + len]) {
            Some(Msg::Hello { name, .. }) => name,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hello_names_are_cleaned_or_replaced() {
        let id = PeerId::random();
        let frame = |name: &str| {
            let hello = Msg::Hello { version: 2, game: "g".into(), id, name: name.into(), udp: 5000, key: [6; 32], nonce: [7; 16] }.encode();
            let mut b = (hello.len() as u32).to_le_bytes().to_vec();
            b.extend_from_slice(&hello);
            b
        };
        // A room peer's name is cleaned up: no address, no control characters.
        let out = peer_hello(&mut HelloRewriter::default(), &frame("203.0.113.9"), id, 1, None).unwrap().unwrap();
        assert_eq!(hello_name(&out), "Player");
        let out = peer_hello(&mut HelloRewriter::default(), &frame("Ro\u{202e}bin\n"), id, 1, None).unwrap().unwrap();
        assert_eq!(hello_name(&out), "Robin");
        // A public player's is the one from its signed handshake.
        let out = peer_hello(&mut HelloRewriter::default(), &frame("whatever"), id, 1, Some("Signed")).unwrap().unwrap();
        assert_eq!(hello_name(&out), "Signed");
        // Ours, on the way out to a public player.
        let mut udp_seen = 0;
        let out = HelloRewriter::default()
            .edit(&frame("Sam"), &mut |_, n, udp| {
                *n = "Player-abcd".into();
                udp_seen = *udp;
                Ok(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(hello_name(&out), "Player-abcd");
        assert_eq!(udp_seen, 5000);
    }

    #[test]
    fn hello_filter_survives_junk() {
        let id = PeerId::random();
        let mut rng = 0x1234_5678u64;
        for _ in 0..2000 {
            let mut f = HelloRewriter::default();
            let n = (rng % 64) as usize;
            let mut junk = Vec::new();
            for _ in 0..n {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                junk.push(rng as u8);
            }
            let _ = peer_hello(&mut f, &junk, id, 1, None);
            let _ = peer_hello(&mut f, &junk, id, 1, Some("x"));
        }
    }
}