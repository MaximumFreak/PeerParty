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

pub mod code;
pub mod crypto;
pub mod dht;
pub mod portmap;
pub mod public;
pub mod redact;
pub mod stream;
pub mod stun;

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

pub use code::RoomCode;
pub use stun::Nat;

use crate::id::PeerId;
use crate::wire::Msg;
use crypto::{Ephemeral, Keys, Replay};
use dht::Dht;
use stream::{Out, Stream};

/// The first byte of every tunnel packet. STUN's is below 0x40, the DHT's is 'd'.
const MAGIC: u8 = 0xC7;
/// The tunnel's own version, apart from the mesh's.
const PROTO: u8 = 1;
const T_INIT: u8 = 1;
const T_CONFIRM: u8 = 2;
const T_DATA: u8 = 3;
/// Public sessions' answer to a first INIT: a cookie, not a session (see COOKIE_FOR).
const T_COOKIE: u8 = 4;
const HEAD: usize = 3;
const INIT_LEN: usize = HEAD + 8 + 48 + 16;
/// An INIT carrying the cookie it was answered with, before its MAC.
const INIT_COOKIE_LEN: usize = INIT_LEN + 16;
/// [head][room id][the INIT's blob][cookie]: no bigger than the INIT it answers.
const COOKIE_LEN: usize = HEAD + 8 + 48 + 16;
const CONFIRM_AD: usize = HEAD + 8 + 48 + 48;

const F_SEG: u8 = 1;
const F_ACK: u8 = 2;
const F_DGRAM: u8 = 3;
const F_PING: u8 = 4;
const F_PONG: u8 = 5;
const F_CLOSE: u8 = 6;
/// Our IPv6 addresses (`[n]`, then n × 16-byte address and port, little-endian), sent
/// inside the session in a packet of its own (older peers skip a frame they don't know
/// and the rest of its packet). The peer then tries them: direct IPv6 needs no NAT.
const F_ADDRS6: u8 = 7;

/// IPv6 dual stack: the session starts over IPv4 (the DHT and STUN are IPv4) and moves
/// to an IPv6 address the peer sent once a packet from there decrypts.
const MAX_ALT: usize = 4;
const PROBE_FOR: Duration = Duration::from_secs(20);
const PROBE_EVERY: Duration = Duration::from_secs(1);
/// Back to IPv4 when the IPv6 path has been quiet this long (the peer pings every
/// KEEPALIVE when idle).
const PATH_QUIET: Duration = Duration::from_secs(12);
const ADVERT_EVERY: Duration = Duration::from_secs(60);

/// Plaintext bytes in one data packet, for the stream (datagrams may be bigger).
const MAX_PLAIN: usize = 1200;
const INIT_EVERY: Duration = Duration::from_millis(250);
const INIT_FAST_FOR: Duration = Duration::from_secs(5);
const INIT_SLOW: Duration = Duration::from_secs(1);
/// How long we punch an address that doesn't answer.
const PUNCH_FOR: Duration = Duration::from_secs(40);
/// A DHT find that has never answered is punched for less: the DHT keeps handing out
/// addresses of copies that are long gone (earlier runs, other ports), and those
/// shouldn't hold a punching slot for long.
const PUNCH_DHT_FOR: Duration = Duration::from_secs(20);
/// ... and less again once it has failed.
const PUNCH_RETRY_FOR: Duration = Duration::from_secs(10);
/// At most this share (in quarters) of the session table punches addresses that haven't
/// answered, so stale finds can't fill it and crowd out a real peer's handshake: 12 of
/// 16 public sessions, 24 of 32 in a room. Addresses that answered this run don't
/// count, and a handshake arriving at a full table replaces the oldest such punch.
const PUNCHING_QUARTERS: usize = 3;
/// A DHT find that never answered goes after this many failed tries, and is ignored
/// for GIVE_UP_FOR if a lookup returns it again (a real peer at that address that
/// punches us still gets in: an inbound handshake doesn't need a candidate).
const CAND_MAX_FAILS: u32 = 3;
/// An address that answered earlier in the run (a peer that since left) goes after this
/// many failed tries in a row, backing off in between.
const ANSWERED_MAX_FAILS: u32 = 8;
const GIVE_UP_FOR: Duration = Duration::from_secs(1800);
/// A DHT find that never answered and no lookup has returned for this long goes.
const CAND_STALE: Duration = Duration::from_secs(600);
/// At most this many candidate addresses are kept.
const MAX_CANDS: usize = 256;
const KEEPALIVE: Duration = Duration::from_secs(5);
const DEAD_AFTER: Duration = Duration::from_secs(30);
/// A peer that connects again from another address while its old session has been
/// quiet this long (two keepalives missed: the old copy is gone, e.g. a game restarted)
/// replaces the old session instead of waiting out DEAD_AFTER. A test may set
/// `Config::replace_quiet` shorter.
const REPLACE_QUIET: Duration = Duration::from_secs(10);
/// Our mesh finished its side of a tunnelled connection (it moved to a better path):
/// the session stays until the peer has everything we sent, this long at most.
const FINISH_FOR: Duration = Duration::from_secs(10);
const MAX_SESSIONS: usize = 32;
const MAX_EARLY: usize = 8 << 20;
const TICK: Duration = Duration::from_millis(10);
const SLOT_SECS: u64 = 3600;
/// Public sessions: fewer sessions, less buffering, and an inbound handshake that doesn't
/// finish soon is dropped (half-open sessions). One internet address can be a whole
/// household or LAN party behind one router, so the per-address limits are per player:
/// PUBLIC_PER_KEY sessions per verified key, PUBLIC_HALF_OPEN_PER_IP handshakes not yet
/// verified per address, and PUBLIC_PER_IP in all (PUBLIC_MAX_SESSIONS bounds it too).
const PUBLIC_MAX_SESSIONS: usize = 16;
const PUBLIC_PER_IP: usize = 32;
const PUBLIC_PER_KEY: usize = 4;
const PUBLIC_HALF_OPEN_PER_IP: usize = 8;
/// ... and in all, whatever addresses they come from (a UDP packet's source is whatever
/// its sender wrote there): half the session table. The other half stays free for
/// finished sessions and for our own punches to players the DHT found, so a flood of
/// first packets from made-up addresses can't keep us from meeting anyone. A player we
/// punch as well gets in through that punch.
const PUBLIC_HALF_OPEN_TOTAL: usize = PUBLIC_MAX_SESSIONS / 2;
const PUBLIC_MAX_EARLY: usize = 1 << 20;
const PUBLIC_HALF_OPEN: Duration = Duration::from_secs(10);
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
            node_cache: std::env::var_os("LOCALAPPDATA")
                .map(|d| PathBuf::from(d).join("peerparty").join("dht-nodes.txt")),
            allow_private: false,
            loss_percent: 0,
            public: None,
            portmap: !crate::loopback_only(),
            ipv6: true,
            room_key: None,
            replace_quiet: REPLACE_QUIET,
        }
    }

    /// Public sessions: anyone online with the same game who also has them on.
    pub fn public(p: public::PublicCfg) -> Config {
        let mut c = Config::new(RoomCode::public());
        c.node_cache = std::env::var_os("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("peerparty").join("dht-nodes-public.txt"));
        c.public = Some(p);
        c
    }

    /// `PEERPARTY_PUBLIC=1` turns public sessions on (`Ok(None)`: off).
    /// `PEERPARTY_PUBLIC_PORT` picks their UDP port; `PEERPARTY_PUBLIC_NAME` the name
    /// shown; the DHT and STUN settings are shared with rooms.
    pub fn public_from_env() -> Result<Option<Config>, String> {
        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let on = var("PEERPARTY_PUBLIC").is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "on" | "yes" | "true"));
        if !on {
            return Ok(None);
        }
        let mut c = Config::public(public::PublicCfg::standard());
        if let Some(p) = var("PEERPARTY_PUBLIC_PORT") {
            c.port = p.parse().map_err(|_| format!("PEERPARTY_PUBLIC_PORT={p} isn't a port"))?;
        }
        c.apply_shared_env();
        Ok(Some(c))
    }

    fn apply_shared_env(&mut self) {
        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let list = |v: String| -> Vec<String> {
            if v == "0" {
                return Vec::new();
            }
            v.split([',', ';', ' ']).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
        };
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
        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let Some(code) = var("PEERPARTY_ROOM") else { return Ok(None) };
        if code == "0" || code.eq_ignore_ascii_case("off") {
            return Ok(None);
        }
        let room = RoomCode::parse(&code).ok_or("PEERPARTY_ROOM isn't a valid room code (peerparty --new-room makes one)")?;
        let mut c = Config::new(room);
        let list = |v: String| -> Vec<String> {
            if v == "0" {
                return Vec::new();
            }
            v.split([',', ';', ' ']).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
        };
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
    fn tunnel_down(&self, mesh: SocketAddr);
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

#[derive(Clone, Debug, PartialEq)]
struct PeerInfo {
    id: PeerId,
    game: String,
    name: String,
}

impl PeerInfo {
    fn encode(&self) -> Vec<u8> {
        let mut b = self.id.0.to_vec();
        for s in [&self.game, &self.name] {
            let s = &s.as_bytes()[..s.len().min(1024)];
            b.extend_from_slice(&(s.len() as u16).to_le_bytes());
            b.extend_from_slice(s);
        }
        b
    }

    fn decode(b: &[u8]) -> Option<PeerInfo> {
        let id = PeerId(b.get(..16)?.try_into().ok()?);
        let mut rest = &b[16..];
        let mut strs = Vec::new();
        for _ in 0..2 {
            let len = u16::from_le_bytes(rest.get(..2)?.try_into().ok()?) as usize;
            strs.push(String::from_utf8_lossy(rest.get(2..2 + len)?).into_owned());
            rest = &rest[2 + len..];
        }
        let name = strs.pop()?;
        let game = strs.pop()?;
        Some(PeerInfo { id, game, name })
    }
}

/// Rewrites the first frame a mesh sends (its Hello); everything after passes as is.
/// From the peer: its UDP port becomes our relay's, and its name the cleaned-up one
/// (for a public player, the one its signed handshake gave). From our mesh to a public
/// player: our name becomes our public one.
#[derive(Default)]
struct HelloFilter {
    done: bool,
    buf: Vec<u8>,
}

type HelloEdit<'a> = dyn FnMut(PeerId, &mut String, &mut u16) -> Result<(), String> + 'a;

impl HelloFilter {
    /// The peer's side: the Hello must be from `expect`.
    fn feed(&mut self, data: &[u8], expect: PeerId, relay: u16, name: Option<&str>) -> Result<Vec<u8>, String> {
        self.edit(data, &mut |id, n, udp| {
            if id != expect {
                return Err(format!("the tunnel's hello is from {id}, not {expect}"));
            }
            *n = match name {
                Some(name) => name.to_string(),
                None => public::sanitize_name(n),
            };
            if *udp != 0 {
                *udp = relay;
            }
            Ok(())
        })
    }

    fn edit(&mut self, data: &[u8], f: &mut HelloEdit) -> Result<Vec<u8>, String> {
        if self.done {
            return Ok(data.to_vec());
        }
        self.buf.extend_from_slice(data);
        let Some(len) = self.buf.get(..4).map(|l| u32::from_le_bytes(l.try_into().unwrap()) as usize) else {
            return Ok(Vec::new());
        };
        if len > 64 * 1024 {
            return Err("the peer's first message is too big for a hello".into());
        }
        if self.buf.len() < 4 + len {
            return Ok(Vec::new());
        }
        let Some(Msg::Hello { version, game, id, mut name, mut udp, key, nonce }) = Msg::decode(&self.buf[4..4 + len]) else {
            return Err("the first message isn't a hello".into());
        };
        f(id, &mut name, &mut udp)?;
        // The name and port are ours to change: the proof that follows signs neither.
        let hello = Msg::Hello { version, game, id, name, udp, key, nonce }.encode();
        let mut out = (hello.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(&hello);
        out.extend_from_slice(&self.buf[4 + len..]);
        self.buf = Vec::new();
        self.done = true;
        Ok(out)
    }
}

/// Learns our own mesh's UDP port from the Hello it sends first.
fn sniff_hello(buf: &mut Vec<u8>, data: &[u8]) -> Option<u16> {
    buf.extend_from_slice(data);
    let len = u32::from_le_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    match Msg::decode(buf.get(4..4 + len)?) {
        Some(Msg::Hello { udp, .. }) => Some(udp),
        _ => Some(0),
    }
}

struct Bridge {
    tcp: TcpStream,
    /// The far end of the mesh connection as our mesh sees it (`Host::tunnel_up`).
    mesh: SocketAddr,
    to_tcp: mpsc::Sender<Vec<u8>>,
    /// Bytes in `to_tcp` our mesh hasn't taken yet (capped at mesh::SEND_QUEUE_MAX).
    to_tcp_bytes: Arc<AtomicUsize>,
    relay: Arc<UdpSocket>,
    relay_port: u16,
    _relay_own: crate::own::Held,
    /// Our mesh's data socket port (0 until its Hello passes).
    mesh_udp: Arc<AtomicU16>,
    closed: Arc<AtomicBool>,
    /// Our mesh stopped writing (it finished with the connection, see FINISH_FOR).
    eof: Arc<AtomicBool>,
}

impl Bridge {
    fn shut(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = self.tcp.shutdown(std::net::Shutdown::Both);
    }
}

struct Session {
    addr: SocketAddr,
    epoch: u64,
    eph: Ephemeral,
    peer: Option<[u8; 48]>,
    keys: Option<Keys>,
    confirmed: bool,
    info: Option<PeerInfo>,
    counter: u64,
    replay: Replay,
    stream: Stream,
    created: Instant,
    last_rx: Instant,
    last_tx: Instant,
    last_init: Option<Instant>,
    last_confirm: Option<Instant>,
    filter: HelloFilter,
    early: Vec<u8>,
    bridge: Option<Bridge>,
    bridging: bool,
    next_bridge: Instant,
    /// When our mesh finished writing to the bridge (see FINISH_FOR).
    finishing: Option<Instant>,
    rtt: Option<Duration>,
    /// The peer started it (it sent the first INIT we saw).
    inbound: bool,
    /// The cookie the peer answered our INIT with (public sessions), sent in each INIT
    /// after it.
    cookie: Option<[u8; 16]>,
    /// Public sessions: the player's verified key and its fingerprint.
    key: Option<[u8; public::KEY_LEN]>,
    fp: Option<String>,
    /// Where data goes: `addr`, or one of `alt` once a packet from it decrypted.
    path: SocketAddr,
    /// The last packet that came in on `path`.
    path_rx: Instant,
    /// The peer's IPv6 addresses (F_ADDRS6), checked.
    alt: Vec<SocketAddr>,
    probe_until: Option<Instant>,
    next_probe: Instant,
    next_advert: Instant,
    /// Keeps the IPv4 path (and its NAT mapping) open while on IPv6.
    v4_ping: Instant,
    /// How long it punches before giving up, while unanswered.
    punch_for: Duration,
}

/// What a tick or a packet leaves to do once the sessions are unlocked.
enum Act {
    Bridge(SocketAddr, u64, PeerId),
    Gone(Option<Bridge>),
    Log(String),
    /// A session ended: whether it had ever connected.
    Ended(SocketAddr, bool),
    /// A handshake with this address finished: it's live (see Cand::answered).
    Heard(SocketAddr),
    Ban(SocketAddr),
}

struct Ctx<'a> {
    sock: &'a UdpSocket,
    sock6: Option<&'a UdpSocket>,
    allow_private: bool,
    room_id: [u8; 8],
    /// Borrowed, not copied: key material lives in one place (Shared), wiped on drop.
    init_key: &'a [u8; 32],
    psk: &'a [u8; 32],
    me: &'a PeerInfo,
    loss: u8,
    /// Our key, which signs our CONFIRMs (the public one, or the room's).
    signer: Option<&'a public::IdentityKey>,
}

impl Ctx<'_> {
    fn send(&self, to: SocketAddr, d: &[u8]) {
        if self.loss > 0 && crypto::random_array::<1>()[0] as u32 * 100 / 256 < self.loss as u32 {
            return;
        }
        let sock = match (to.is_ipv6(), self.sock6) {
            (false, _) => self.sock,
            (true, Some(s)) => s,
            (true, None) => return,
        };
        let _ = sock.send_to(d, to);
    }
}

/// An IPv6 address worth trying: global unicast (no Teredo or documentation range), or
/// loopback and unique-local in tests.
fn usable_v6(a: SocketAddr, allow_private: bool) -> bool {
    let SocketAddr::V6(v) = a else { return false };
    if v.port() == 0 {
        return false;
    }
    let ip = *v.ip();
    let s = ip.segments();
    let global = s[0] & 0xe000 == 0x2000 && !(s[0] == 0x2001 && (s[1] == 0 || s[1] == 0xdb8));
    global || (allow_private && (ip.is_loopback() || s[0] & 0xfe00 == 0xfc00))
}

fn addrs6_frame(own: &[SocketAddr]) -> Vec<u8> {
    let own: Vec<&SocketAddr> = own.iter().filter(|a| a.is_ipv6()).take(MAX_ALT).collect();
    let mut f = vec![F_ADDRS6, own.len() as u8];
    for a in own {
        if let SocketAddr::V6(v) = a {
            f.extend_from_slice(&v.ip().octets());
            f.extend_from_slice(&v.port().to_le_bytes());
        }
    }
    f
}

fn ping_frame(s: &Session, now: Instant) -> Vec<u8> {
    let mut ping = vec![F_PING];
    ping.extend_from_slice(&(now.duration_since(s.created).as_micros() as u64).to_le_bytes());
    ping
}

fn head(t: u8) -> [u8; HEAD] {
    [MAGIC, PROTO, t]
}

impl Session {
    fn new(addr: SocketAddr, epoch: u64, now: Instant, inbound: bool) -> Session {
        Session {
            addr,
            epoch,
            eph: Ephemeral::new(),
            peer: None,
            keys: None,
            confirmed: false,
            info: None,
            counter: 0,
            replay: Replay::default(),
            stream: Stream::default(),
            created: now,
            last_rx: now,
            last_tx: now,
            last_init: None,
            last_confirm: None,
            filter: HelloFilter::default(),
            early: Vec::new(),
            bridge: None,
            bridging: false,
            next_bridge: now,
            finishing: None,
            rtt: None,
            inbound,
            cookie: None,
            key: None,
            fp: None,
            path: addr,
            path_rx: now,
            alt: Vec::new(),
            probe_until: None,
            next_probe: now,
            next_advert: now,
            v4_ping: now,
            punch_for: PUNCH_FOR,
        }
    }

    fn send_init(&mut self, cx: &Ctx, now: Instant) {
        let mut p = head(T_INIT).to_vec();
        p.extend_from_slice(&cx.room_id);
        p.extend_from_slice(&self.eph.blob);
        if let Some(c) = &self.cookie {
            p.extend_from_slice(c);
        }
        let tag = crypto::mac16(cx.init_key, &[&p]);
        p.extend_from_slice(&tag);
        cx.send(self.addr, &p);
        self.last_init = Some(now);
    }

    fn send_confirm(&mut self, cx: &Ctx, now: Instant) {
        let (Some(peer), Some(keys)) = (&self.peer, &self.keys) else { return };
        let mut p = head(T_CONFIRM).to_vec();
        p.extend_from_slice(&cx.room_id);
        p.extend_from_slice(&self.eph.blob);
        p.extend_from_slice(peer);
        let body = match cx.signer {
            Some(k) => public::advert(k, &cx.room_id, &self.eph.blob, peer, &cx.me.encode()),
            None => cx.me.encode(),
        };
        let sealed = keys.seal(crypto::CONFIRM_COUNTER, &p, &body);
        p.extend_from_slice(&sealed);
        cx.send(self.addr, &p);
        self.last_confirm = Some(now);
    }

    fn set_peer(&mut self, cx: &Ctx, blob: [u8; 48]) -> bool {
        match Keys::derive(cx.psk, &self.eph, &blob) {
            Some(k) => {
                self.peer = Some(blob);
                self.keys = Some(k);
                true
            }
            None => false,
        }
    }

    fn send_plain(&mut self, cx: &Ctx, plain: &[u8], now: Instant) {
        self.send_plain_to(cx, self.path, plain, now);
    }

    fn send_plain_to(&mut self, cx: &Ctx, to: SocketAddr, plain: &[u8], now: Instant) {
        let Some(keys) = &self.keys else { return };
        let c = self.counter;
        self.counter += 1;
        let mut p = head(T_DATA).to_vec();
        p.extend_from_slice(&c.to_le_bytes());
        let sealed = keys.seal(c, &p, plain);
        p.extend_from_slice(&sealed);
        cx.send(to, &p);
        self.last_tx = now;
    }

    /// Sends the stream's segments and acknowledgements, packed into packets.
    fn flush(&mut self, cx: &Ctx, now: Instant) {
        let outs = self.stream.poll(now);
        let mut plain = Vec::with_capacity(MAX_PLAIN);
        for o in outs {
            let mut f = Vec::new();
            match o {
                Out::Seg { seq, data } => {
                    f.push(F_SEG);
                    f.extend_from_slice(&seq.to_le_bytes());
                    f.extend_from_slice(&(data.len() as u16).to_le_bytes());
                    f.extend_from_slice(&data);
                }
                Out::Ack { next, sack } => {
                    f.push(F_ACK);
                    f.extend_from_slice(&next.to_le_bytes());
                    f.extend_from_slice(&sack.to_le_bytes());
                }
            }
            if plain.len() + f.len() > MAX_PLAIN && !plain.is_empty() {
                self.send_plain(cx, &plain, now);
                plain.clear();
            }
            plain.extend_from_slice(&f);
        }
        if !plain.is_empty() {
            self.send_plain(cx, &plain, now);
        }
    }

    fn send_dgram(&mut self, cx: &Ctx, data: &[u8], now: Instant) {
        let mut plain = Vec::with_capacity(data.len() + 3);
        plain.push(F_DGRAM);
        plain.extend_from_slice(&(data.len().min(u16::MAX as usize) as u16).to_le_bytes());
        plain.extend_from_slice(&data[..data.len().min(u16::MAX as usize)]);
        self.send_plain(cx, &plain, now);
    }

    fn send_close(&mut self, cx: &Ctx, now: Instant) {
        for _ in 0..3 {
            self.send_plain(cx, &[F_CLOSE], now);
        }
    }

    /// In-order stream bytes from the peer, for our mesh (or kept until the bridge is up).
    fn deliver(&mut self, data: Vec<u8>) -> Result<(), String> {
        if data.is_empty() {
            return Ok(());
        }
        let (Some(b), Some(info)) = (&self.bridge, &self.info) else {
            let max = if self.key.is_some() { PUBLIC_MAX_EARLY } else { MAX_EARLY };
            if self.early.len() + data.len() > max {
                return Err("too much data before the tunnel's mesh connection was up".into());
            }
            self.early.extend_from_slice(&data);
            return Ok(());
        };
        let name = self.key.is_some().then_some(info.name.as_str());
        let out = self.filter.feed(&data, info.id, b.relay_port, name)?;
        if !out.is_empty() {
            let len = out.len();
            if b.to_tcp_bytes.fetch_add(len, Ordering::Relaxed) + len > crate::mesh::SEND_QUEUE_MAX {
                b.to_tcp_bytes.fetch_sub(len, Ordering::Relaxed);
                return Err("our mesh stopped taking the peer's data (send queue full)".into());
            }
            if b.to_tcp.send(out).is_err() {
                b.to_tcp_bytes.fetch_sub(len, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn status(&self) -> PeerStatus {
        PeerStatus {
            tag: redact::addr(self.addr),
            key: self.fp.clone(),
            id: self.info.as_ref().map(|i| i.id),
            name: self.info.as_ref().map(|i| i.name.clone()).unwrap_or_default(),
            connected: self.confirmed,
            bridged: self.bridge.is_some(),
            rtt: self.rtt.or(self.stream.rtt()),
            resent: self.stream.resent,
            ipv6: self.path.is_ipv6(),
        }
    }
}

struct Cand {
    from_dht: bool,
    next_try: Instant,
    fails: u32,
    /// When a DHT lookup last returned it (or it was added).
    seen: Instant,
    /// A lookup of this hour's topic returned it (the last hour's holds older
    /// announcements).
    current: bool,
    /// A handshake with it has finished this run: a live address, punched first and
    /// kept.
    answered: bool,
}

impl Cand {
    fn new(from_dht: bool, now: Instant) -> Cand {
        Cand { from_dht, next_try: now, fails: 0, seen: now, current: false, answered: false }
    }

    /// How long a punch to it lasts.
    fn punch_for(&self) -> Duration {
        if self.answered || !self.from_dht {
            PUNCH_FOR
        } else if self.fails == 0 {
            PUNCH_DHT_FOR
        } else {
            PUNCH_RETRY_FOR
        }
    }

    /// Punch order: addresses that answered, then PEERPARTY_INET_PEERS, then this
    /// hour's finds, fewest failures and most recently returned first.
    fn rank(&self) -> (bool, bool, bool, u32, std::cmp::Reverse<Instant>) {
        (!self.answered, self.from_dht, !self.current, self.fails, std::cmp::Reverse(self.seen))
    }
}

struct Disc {
    dht: Option<Dht>,
    next_lookup: Instant,
    started: Instant,
    stun_servers: Vec<SocketAddr>,
    stun_tx: HashMap<[u8; 12], SocketAddr>,
    stun_mapped: HashMap<SocketAddr, SocketAddr>,
    next_stun: Instant,
    cands: HashMap<SocketAddr, Cand>,
    statics: Vec<SocketAddr>,
    selves: HashSet<SocketAddr>,
    banned: HashMap<SocketAddr, Instant>,
    /// DHT finds given up on (CAND_MAX_FAILS), until then.
    given_up: HashMap<SocketAddr, Instant>,
    next_resolve: Instant,
    next_save: Instant,
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

impl Shared {
    fn new_keys_cooling(&self, ip: IpAddr, now: Instant) -> bool {
        let mut c = self.cooldown.lock().unwrap();
        c.retain(|_, until| *until > now);
        c.contains_key(&ip)
    }

    fn is_blocked_addr(&self, a: SocketAddr, now: Instant) -> bool {
        let mut b = self.blocked_addrs.lock().unwrap();
        b.retain(|_, until| *until > now);
        b.contains_key(&a)
    }

    fn block_addr(&self, a: SocketAddr, now: Instant) {
        let mut b = self.blocked_addrs.lock().unwrap();
        if b.len() < 4096 {
            b.insert(a, now + BLOCKED_FOR);
        }
        drop(b);
        let mut c = self.cooldown.lock().unwrap();
        if c.len() < 4096 {
            c.insert(a.ip(), now + NEW_KEY_COOLDOWN);
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

/// What a cookie is the MAC of: the address (IPv4 as IPv4-mapped), port, blob and slot.
fn cookie_input(from: SocketAddr, blob: &[u8; 48], slot: u64) -> [u8; 16 + 2 + 48 + 8] {
    let ip = match from.ip() {
        IpAddr::V4(v) => v.to_ipv6_mapped().octets(),
        IpAddr::V6(v) => v.octets(),
    };
    let mut b = [0u8; 16 + 2 + 48 + 8];
    b[..16].copy_from_slice(&ip);
    b[16..18].copy_from_slice(&from.port().to_le_bytes());
    b[18..66].copy_from_slice(blob);
    b[66..].copy_from_slice(&slot.to_le_bytes());
    b
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

fn resolve(names: &[String]) -> Vec<SocketAddr> {
    resolve_with(names, |a| a.is_ipv4())
}

/// `PEERPARTY_INET_PEERS`: IPv4, and IPv6 ("[2001:db8::5]:4000") when we have an IPv6
/// socket and the address is one worth trying.
fn resolve_peers(names: &[String], v6: bool, allow_private: bool) -> Vec<SocketAddr> {
    resolve_with(names, |a| a.is_ipv4() || (v6 && usable_v6(*a, allow_private)))
}

fn resolve_with(names: &[String], keep: impl Fn(&SocketAddr) -> bool) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for n in names {
        if let Ok(addrs) = n.to_socket_addrs() {
            out.extend(addrs.filter(&keep));
        }
    }
    out.dedup();
    out
}

fn load_nodes(path: &Option<PathBuf>) -> Vec<([u8; 20], SocketAddr)> {
    let Some(p) = path else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(p) else { return Vec::new() };
    text.lines()
        .filter_map(|l| {
            let (id, addr) = l.trim().split_once(' ')?;
            if id.len() != 40 || !id.is_ascii() {
                return None;
            }
            let mut b = [0u8; 20];
            for (i, x) in b.iter_mut().enumerate() {
                *x = u8::from_str_radix(&id[i * 2..i * 2 + 2], 16).ok()?;
            }
            Some((b, addr.parse().ok()?))
        })
        .take(200)
        .collect()
}

fn save_nodes(path: &Option<PathBuf>, nodes: &[([u8; 20], SocketAddr)]) {
    let Some(p) = path else { return };
    if nodes.is_empty() {
        return;
    }
    let text: String = nodes
        .iter()
        .rev()
        .take(200)
        .map(|(id, a)| format!("{} {a}\n", id.iter().map(|b| format!("{b:02x}")).collect::<String>()))
        .collect();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, text);
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Inet {
    pub fn start(cfg: Config, host: Arc<dyn Host>) -> io::Result<Inet> {
        let bind = if crate::loopback_only() { Ipv4Addr::LOCALHOST } else { Ipv4Addr::UNSPECIFIED };
        let sock = UdpSocket::bind((bind, cfg.port))?;
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        let local_port = sock.local_addr()?.port();
        let sock6 = if cfg.ipv6 {
            let b6 = if crate::loopback_only() { Ipv6Addr::LOCALHOST } else { Ipv6Addr::UNSPECIFIED };
            UdpSocket::bind((b6, local_port))
                .or_else(|_| UdpSocket::bind((b6, 0)))
                .ok()
                .filter(|s| s.set_read_timeout(Some(Duration::from_millis(200))).is_ok())
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
            ended: Mutex::new(Vec::new()),
            next_gen: Mutex::new(0),
            pubs,
            limit: Mutex::new(public::RateLimit::new(PUBLIC_INIT_BURST, PUBLIC_INIT_RATE)),
            max_sessions,
            blocked_addrs: Mutex::new(HashMap::new()),
            cooldown: Mutex::new(HashMap::new()),
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
        if sh.sock6.is_some() {
            let sh = sh.clone();
            thread::Builder::new().name("peerparty-inet-recv6".into()).spawn(move || recv6_loop(&sh))?;
        }
        {
            let sh = sh.clone();
            thread::Builder::new().name("peerparty-inet-recv".into()).spawn(move || recv_loop(&sh))?;
        }
        {
            let sh = sh.clone();
            thread::Builder::new().name("peerparty-inet-tick".into()).spawn(move || tick_loop(&sh))?;
        }
        {
            let sh = sh.clone();
            thread::Builder::new().name("peerparty-inet-disc".into()).spawn(move || disc_loop(&sh))?;
        }
        if sh.cfg.portmap {
            let sh = sh.clone();
            thread::Builder::new().name("peerparty-inet-portmap".into()).spawn(move || portmap_loop(&sh))?;
        }
        Ok(Inet { sh })
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

    /// Says goodbye to every peer, drops the tunnels and stops the threads.
    pub fn stop(&self) {
        if self.sh.stop.swap(true, Ordering::Relaxed) {
            return;
        }
        self.release_port_map();
        let now = Instant::now();
        let gone: Vec<Session> = {
            let mut s = self.sh.sessions.lock().unwrap();
            let cx = self.sh.cx();
            for v in s.values_mut().filter(|v| v.confirmed) {
                v.send_close(&cx, now);
            }
            s.drain().map(|(_, v)| v).collect()
        };
        for v in gone {
            if let Some(b) = &v.bridge {
                b.shut();
                self.sh.host.tunnel_down(b.mesh);
            }
        }
        let d = self.sh.disc.lock().unwrap();
        if let Some(x) = &d.dht {
            save_nodes(&self.sh.cfg.node_cache, &x.good_nodes());
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
            run(sh, acts);
        }
    }
}

fn on_packet(sh: &Arc<Shared>, ss: &mut HashMap<SocketAddr, Session>, from: SocketAddr, d: &[u8], now: Instant) -> Vec<Act> {
    let cx = sh.cx();
    let mut acts = Vec::new();
    let tag = || redact::addr(from);
    match d[2] {
        T_INIT => {
            let n = d.len();
            if !(n == INIT_LEN || n == INIT_COOKIE_LEN) || d[HEAD..HEAD + 8] != cx.room_id {
                return acts;
            }
            if !crypto::mac16_ok(cx.init_key, &[&d[..n - 16]], &d[n - 16..]) {
                return acts;
            }
            let blob: [u8; 48] = d[HEAD + 8..HEAD + 56].try_into().unwrap();
            if sh.pubs.is_some() {
                // Anyone can send these: limit what a stranger can make us do.
                if sh.is_blocked_addr(from, now) {
                    return acts;
                }
                // Nothing is kept for an address until it shows it gets our packets
                // (COOKIE_FOR): it's sent a cookie, and comes back with it.
                if !ss.contains_key(&from) && !(n == INIT_COOKIE_LEN && sh.cookie_ok(from, &blob, &d[INIT_LEN - 16..INIT_LEN], now)) {
                    if sh.cookies.lock().unwrap().allow(IpAddr::V4(Ipv4Addr::UNSPECIFIED), now) {
                        let mut p = head(T_COOKIE).to_vec();
                        p.extend_from_slice(&cx.room_id);
                        p.extend_from_slice(&blob);
                        p.extend_from_slice(&sh.cookie(from, &blob, sh.cookie_slot(now)));
                        cx.send(from, &p);
                    }
                    return acts;
                }
                let fresh = ss.get(&from).is_none_or(|s| s.peer != Some(blob));
                if fresh {
                    if !ss.contains_key(&from) {
                        // Half-open counts only handshakes it started: our own punches to
                        // its other ports (an earlier run's, found on the DHT) mustn't
                        // lock its live port out.
                        let same = || ss.values().filter(|s| s.addr.ip() == from.ip());
                        if same().count() >= PUBLIC_PER_IP
                            || same().filter(|s| s.inbound && !s.confirmed).count() >= PUBLIC_HALF_OPEN_PER_IP
                            || ss.values().filter(|s| s.inbound && !s.confirmed).count() >= PUBLIC_HALF_OPEN_TOTAL
                        {
                            return acts;
                        }
                    }
                    if !sh.limit.lock().unwrap().allow(from.ip(), now) {
                        return acts;
                    }
                }
            }
            if !ss.contains_key(&from) {
                if ss.len() >= sh.max_sessions {
                    // A handshake from a live address beats a punch to one that has
                    // never answered (often a stale DHT find): drop the oldest of those.
                    let stale = ss
                        .values()
                        .filter(|s| !s.inbound && !s.confirmed && s.peer.is_none())
                        .min_by_key(|s| s.created)
                        .map(|s| s.addr);
                    match stale {
                        Some(old) => {
                            ss.remove(&old);
                            acts.push(Act::Ended(old, false));
                        }
                        None => {
                            let mut logged = sh.full_logged.lock().unwrap();
                            if logged.len() < 1024 && logged.insert(from) {
                                acts.push(Act::Log(format!(
                                    "{} is punching through to us, but all {} sessions are in use: ignored",
                                    tag(),
                                    sh.max_sessions
                                )));
                            }
                            return acts;
                        }
                    }
                }
                acts.push(Act::Log(format!("{} is punching through to us", tag())));
                ss.insert(from, Session::new(from, sh.epoch(), now, true));
            }
            let s = ss.get_mut(&from).unwrap();
            if s.peer != Some(blob) {
                if s.confirmed {
                    // A first packet proves nothing about who sent it (in public
                    // sessions anyone can make one, and a source address can be made
                    // up), so it doesn't end a session that is alive: only one that has
                    // missed two keepalives, as when the game at the other end was
                    // started again. At 2 s, an idle session could be reset at will.
                    if now.duration_since(s.last_rx) < sh.cfg.replace_quiet {
                        return acts;
                    }
                    // The peer started over (it lost our session): so do we.
                    let old = ss.remove(&from).unwrap();
                    acts.push(Act::Log(format!("{} reconnected", tag())));
                    acts.push(Act::Gone(old.bridge));
                    ss.insert(from, Session::new(from, sh.epoch(), now, true));
                }
                let s = ss.get_mut(&from).unwrap();
                if !s.set_peer(&cx, blob) {
                    return acts;
                }
            }
            let s = ss.get_mut(&from).unwrap();
            if s.last_confirm.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100)) {
                s.send_confirm(&cx, now);
            }
            if !s.confirmed && s.last_init.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100)) {
                s.send_init(&cx, now);
            }
        }
        T_COOKIE => {
            // Our INIT was answered with a cookie: it goes in our INITs from now on.
            if d.len() != COOKIE_LEN || d[HEAD..HEAD + 8] != cx.room_id {
                return acts;
            }
            let Some(s) = ss.get_mut(&from) else { return acts };
            if s.confirmed || d[HEAD + 8..HEAD + 56] != s.eph.blob[..] {
                return acts;
            }
            s.cookie = Some(d[HEAD + 56..COOKIE_LEN].try_into().unwrap());
            if s.last_init.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100)) {
                s.send_init(&cx, now);
            }
        }
        T_CONFIRM => {
            if d.len() < CONFIRM_AD + 16 || d[HEAD..HEAD + 8] != cx.room_id {
                return acts;
            }
            let Some(s) = ss.get_mut(&from) else { return acts };
            let sender: [u8; 48] = d[HEAD + 8..HEAD + 56].try_into().unwrap();
            if d[HEAD + 56..CONFIRM_AD] != s.eph.blob[..] || s.confirmed {
                return acts;
            }
            if s.peer != Some(sender) && !s.set_peer(&cx, sender) {
                return acts;
            }
            let Some(plain) = s.keys.as_ref().and_then(|k| k.open(crypto::CONFIRM_COUNTER, &d[..CONFIRM_AD], &d[CONFIRM_AD..])) else {
                return acts;
            };
            // The body is signed with the player's own key, over both sides' fresh
            // keys, so it can't be replayed or moved to another session.
            let Some((body, signed_by)) = public::open_advert(&plain, &cx.room_id, &sender, &s.eph.blob) else {
                if sh.pubs.is_none() {
                    // In a room only an older PeerParty sends these: the code is right.
                    ss.remove(&from);
                    acts.push(Act::Ban(from));
                    acts.push(Act::Log(format!(
                        "{} has the room code but sent an unsigned handshake: an older peerparty? Everyone in the room needs this version",
                        tag()
                    )));
                }
                return acts;
            };
            let Some(mut info) = PeerInfo::decode(body) else { return acts };
            info.name = public::sanitize_name(&info.name);
            if info.id == cx.me.id || cx.signer.is_some_and(|k| k.public() == signed_by) {
                ss.remove(&from);
                acts.push(Act::Ban(from));
                acts.push(Act::Log(format!("{} is ourselves (our own announcement)", tag())));
                return acts;
            }
            // A player's id is its key's hash, in rooms and public sessions alike: the
            // signature just checked is that key's, so nobody passes as another, and
            // nobody can be first to an id that isn't theirs.
            if info.id != PeerId::of_key(&signed_by) {
                ss.remove(&from);
                acts.push(Act::Ban(from));
                acts.push(Act::Log(format!(
                    "{} ({}) claims to be {}, which isn't its key's id ({}): refused",
                    tag(),
                    info.name,
                    info.id,
                    public::fingerprint(&signed_by)
                )));
                return acts;
            }
            // Only public players are told apart by key elsewhere (status, blocking).
            let key = sh.pubs.as_ref().map(|_| signed_by);
            let fp = key.as_ref().map(public::fingerprint);
            if info.game != cx.me.game {
                ss.remove(&from);
                acts.push(Act::Ban(from));
                acts.push(Act::Log(format!("{} ({}) is in this room but plays {}, not {}", tag(), info.name, info.game, cx.me.game)));
                return acts;
            }
            if let (Some(p), Some(k), Some(fp)) = (&sh.pubs, &key, &fp) {
                if p.blocked.is_blocked(fp) {
                    ss.remove(&from);
                    sh.block_addr(from, now);
                    acts.push(Act::Ban(from));
                    acts.push(Act::Log(format!("{} is {fp}, on the block list: ignored", tag())));
                    return acts;
                }
                let friend = p.friends.is_blocked(fp);
                if p.friends_only && !friend {
                    ss.remove(&from);
                    acts.push(Act::Ban(from));
                    acts.push(Act::Log(format!("{} is {fp}, not on the friends list (friends-only): refused", tag())));
                    return acts;
                }
                if !friend && !p.pins.knows_key(k) && sh.new_keys_cooling(from.ip(), now) {
                    ss.remove(&from);
                    acts.push(Act::Ban(from));
                    acts.push(Act::Log(format!(
                        "{} is a new key ({fp}) from the address of a player blocked lately: refused for now",
                        tag()
                    )));
                    return acts;
                }
                p.pins.remember(info.id, k);
                if ss.values().filter(|o| o.confirmed && o.fp.as_deref() == Some(fp.as_str())).count() >= PUBLIC_PER_KEY {
                    ss.remove(&from);
                    acts.push(Act::Log(format!("{} is {fp} again: {PUBLIC_PER_KEY} sessions with that key already, refused", tag())));
                    acts.push(Act::Ended(from, false));
                    return acts;
                }
            }
            if let Some(other) = ss.values().find(|o| o.addr != from && o.confirmed && o.info.as_ref().is_some_and(|i| i.id == info.id)) {
                let at = redact::addr(other.addr);
                let old = other.addr;
                let quiet = now.duration_since(other.last_rx);
                if quiet < sh.cfg.replace_quiet {
                    // Already up at another of its addresses, and that one is alive: keep it.
                    ss.remove(&from);
                    acts.push(Act::Log(format!("{} is {} again (already connected at {at})", tag(), info.name)));
                    acts.push(Act::Ended(from, true));
                    return acts;
                }
                // The old session went quiet: the peer started over (a restarted game).
                let gone = ss.remove(&old).unwrap();
                acts.push(Act::Log(format!(
                    "{} is {} again; its old session at {at} has been quiet for {}s: replaced",
                    tag(),
                    info.name,
                    quiet.as_secs()
                )));
                acts.push(Act::Gone(gone.bridge));
                acts.push(Act::Ended(old, true));
            }
            let s = ss.get_mut(&from).unwrap();
            s.confirmed = true;
            s.last_rx = now;
            // Only now, with the handshake finished: an init alone proves nothing (a
            // public one can come from anyone, from any source address).
            acts.push(Act::Heard(from));
            match &fp {
                Some(fp) => acts.push(Act::Log(format!("connected to public player {} ({}, key {fp}) at {}", info.name, info.id, tag()))),
                None => acts.push(Act::Log(format!("connected to {} ({}) at {}", info.name, info.id, tag()))),
            }
            s.info = Some(info);
            s.key = key;
            s.fp = fp;
            s.send_confirm(&cx, now);
        }
        T_DATA => {
            if d.len() < HEAD + 8 + 16 {
                return acts;
            }
            // From the session's own address, or from an IPv6 address its peer sent us.
            let key = if ss.contains_key(&from) {
                from
            } else if from.is_ipv6() {
                match ss.iter().find(|(_, s)| s.confirmed && s.alt.contains(&from)) {
                    Some((k, _)) => *k,
                    None => return acts,
                }
            } else {
                return acts;
            };
            let s = ss.get_mut(&key).unwrap();
            if !s.confirmed {
                return acts;
            }
            let c = u64::from_le_bytes(d[HEAD..HEAD + 8].try_into().unwrap());
            if c == crypto::CONFIRM_COUNTER || !s.replay.fresh(c) {
                return acts;
            }
            let Some(plain) = s.keys.as_ref().and_then(|k| k.open(c, &d[..HEAD + 8], &d[HEAD + 8..])) else {
                return acts;
            };
            s.replay.commit(c);
            s.last_rx = now;
            if from == s.path {
                s.path_rx = now;
            } else if from != s.addr && !s.path.is_ipv6() {
                // It decrypted, so it's the peer: prefer the direct IPv6 path.
                s.path = from;
                s.path_rx = now;
                s.probe_until = None;
                acts.push(Act::Log(format!("{}: direct over IPv6 now ({})", redact::addr(key), tag())));
            }
            if let Err(e) = frames(s, &cx, &plain, now) {
                let old = ss.remove(&key).unwrap();
                if !e.is_empty() {
                    acts.push(Act::Log(format!("{}: {e}", redact::addr(key))));
                }
                acts.push(Act::Gone(old.bridge));
                acts.push(Act::Ended(key, true));
            }
        }
        _ => {}
    }
    acts
}

/// Handles a data packet's frames. `Err` ends the session (an empty one: a goodbye).
fn frames(s: &mut Session, cx: &Ctx, mut p: &[u8], now: Instant) -> Result<(), String> {
    while let Some(&f) = p.first() {
        p = &p[1..];
        match f {
            F_SEG => {
                let (Some(seq), Some(len)) = (p.get(..4), p.get(4..6)) else { return Err("bad segment".into()) };
                let seq = u32::from_le_bytes(seq.try_into().unwrap());
                let len = u16::from_le_bytes(len.try_into().unwrap()) as usize;
                if len > stream::MAX_SEG_IN {
                    return Err("a segment bigger than any peerparty sends".into());
                }
                let data = p.get(6..6 + len).ok_or("bad segment")?.to_vec();
                p = &p[6 + len..];
                let ready = s.stream.on_segment(seq, data);
                s.deliver(ready)?;
            }
            F_ACK => {
                let a = p.get(..12).ok_or("bad ack")?;
                let next = u32::from_le_bytes(a[..4].try_into().unwrap());
                let sack = u64::from_le_bytes(a[4..].try_into().unwrap());
                p = &p[12..];
                s.stream.on_ack(next, sack, now);
            }
            F_DGRAM => {
                let len = u16::from_le_bytes(p.get(..2).ok_or("bad datagram")?.try_into().unwrap()) as usize;
                let data = p.get(2..2 + len).ok_or("bad datagram")?;
                p = &p[2 + len..];
                // A mesh datagram names its sender, and our mesh takes it at its word
                // from this address (every tunnel ends at this one). Here is where it is
                // known who sent it: the player this session was proved to be, and no
                // other.
                if s.info.as_ref().is_none_or(|i| !crate::mesh::datagram_is_from(data, i.id)) {
                    continue;
                }
                if let Some(b) = &s.bridge {
                    let port = b.mesh_udp.load(Ordering::Relaxed);
                    if port != 0 {
                        let _ = b.relay.send_to(data, (Ipv4Addr::LOCALHOST, port));
                    }
                }
            }
            F_PING => {
                let t = p.get(..8).ok_or("bad ping")?.to_vec();
                p = &p[8..];
                let mut pong = vec![F_PONG];
                pong.extend_from_slice(&t);
                s.send_plain(cx, &pong, now);
            }
            F_PONG => {
                let t = u64::from_le_bytes(p.get(..8).ok_or("bad pong")?.try_into().unwrap());
                p = &p[8..];
                let sent = Duration::from_micros(t);
                s.rtt = now.duration_since(s.created).checked_sub(sent);
            }
            F_ADDRS6 => {
                let n = *p.first().ok_or("bad addresses")? as usize;
                let body = p.get(1..1 + n * 18).ok_or("bad addresses")?;
                p = &p[1 + n * 18..];
                if cx.sock6.is_some() {
                    s.alt = body
                        .as_chunks::<18>()
                        .0
                        .iter()
                        .map(|c| SocketAddr::from((Ipv6Addr::from(<[u8; 16]>::try_from(&c[..16]).unwrap()), u16::from_le_bytes([c[16], c[17]]))))
                        .filter(|a| usable_v6(*a, cx.allow_private))
                        .take(MAX_ALT)
                        .collect();
                    if s.path != s.addr && !s.alt.contains(&s.path) {
                        s.path = s.addr;
                    }
                    if s.path == s.addr && !s.alt.is_empty() {
                        s.probe_until = Some(now + PROBE_FOR);
                        s.next_probe = now;
                    }
                }
            }
            F_CLOSE => return Err(String::new()),
            _ => return Ok(()), // a newer frame: skip the rest
        }
    }
    Ok(())
}

fn tick_loop(sh: &Arc<Shared>) {
    while !sh.stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let acts = {
            let mut ss = sh.sessions.lock().unwrap();
            tick_sessions(sh, &mut ss, now)
        };
        run(sh, acts);
        thread::sleep(TICK);
    }
}

fn tick_sessions(sh: &Arc<Shared>, ss: &mut HashMap<SocketAddr, Session>, now: Instant) -> Vec<Act> {
    let cx = sh.cx();
    let mut acts = Vec::new();
    let mut remove = Vec::new();
    let own6 = if ss.values().any(|s| s.confirmed) { sh.own_v6(now) } else { Vec::new() };
    for s in ss.values_mut() {
        if !s.confirmed {
            let half_open = sh.pubs.is_some() && s.inbound && now.duration_since(s.created) >= PUBLIC_HALF_OPEN;
            if half_open || now.duration_since(s.created) >= s.punch_for {
                // One-way failures show in the log: they reached us, we never got
                // their confirmation (our packets aren't reaching them), or the other
                // way round.
                let why = if s.inbound {
                    format!(
                        "{} was punching through to us, but the handshake never finished in {}s: our replies aren't reaching them (a one-way path)",
                        redact::addr(s.addr),
                        now.duration_since(s.created).as_secs()
                    )
                } else if s.peer.is_some() {
                    format!(
                        "{} answered our punch, but the handshake never finished in {}s",
                        redact::addr(s.addr),
                        now.duration_since(s.created).as_secs()
                    )
                } else {
                    String::new()
                };
                remove.push((s.addr, false, why));
            } else {
                // Fast at first, then once a second: the DHT hands out some junk
                // addresses, and a real peer that starts punching later gets an
                // answer at once anyway (an INIT is answered within 100 ms).
                let every = if now.duration_since(s.created) < INIT_FAST_FOR { INIT_EVERY } else { INIT_SLOW };
                if s.last_init.is_none_or(|t| now.duration_since(t) >= every) {
                    s.send_init(&cx, now);
                }
            }
            continue;
        }
        if now.duration_since(s.last_rx) >= DEAD_AFTER {
            remove.push((s.addr, true, format!("{} went quiet for {}s: dropped", redact::addr(s.addr), DEAD_AFTER.as_secs())));
            continue;
        }
        if s.bridge.as_ref().is_some_and(|b| b.closed.load(Ordering::Relaxed)) {
            s.send_close(&cx, now);
            remove.push((s.addr, true, format!("{}: the mesh connection closed", redact::addr(s.addr))));
            continue;
        }
        if s.bridge.as_ref().is_some_and(|b| b.eof.load(Ordering::Relaxed)) {
            // Our mesh is done with this connection: close once the peer has had all of
            // it (a goodbye straight away could drop the tail still in flight).
            let since = *s.finishing.get_or_insert(now);
            if s.stream.idle() || now.duration_since(since) >= FINISH_FOR {
                s.send_close(&cx, now);
                remove.push((s.addr, true, format!("{}: the mesh connection finished", redact::addr(s.addr))));
                continue;
            }
        }
        s.flush(&cx, now);
        if now.duration_since(s.last_tx) >= KEEPALIVE {
            let ping = ping_frame(s, now);
            s.send_plain(&cx, &ping, now);
        }
        if cx.sock6.is_some() {
            if now >= s.next_advert && !own6.is_empty() {
                s.send_plain(&cx, &addrs6_frame(&own6), now);
                s.next_advert = now + ADVERT_EVERY;
            }
            if s.path != s.addr {
                if now.duration_since(s.path_rx) >= PATH_QUIET {
                    acts.push(Act::Log(format!("{}: the IPv6 path went quiet: back to IPv4", redact::addr(s.addr))));
                    s.path = s.addr;
                } else if now.duration_since(s.v4_ping) >= KEEPALIVE {
                    let ping = ping_frame(s, now);
                    s.send_plain_to(&cx, s.addr, &ping, now);
                    s.v4_ping = now;
                }
            } else if s.probe_until.is_some_and(|t| now < t) && now >= s.next_probe {
                let ping = ping_frame(s, now);
                for a in s.alt.clone() {
                    s.send_plain_to(&cx, a, &ping, now);
                }
                s.next_probe = now + PROBE_EVERY;
            }
        }
        if let Some(info) = &s.info
            && s.bridge.is_none()
            && !s.bridging
            && now >= s.next_bridge
        {
            if sh.host.knows(info.id) {
                s.next_bridge = now + Duration::from_secs(5);
            } else {
                s.bridging = true;
                acts.push(Act::Bridge(s.addr, s.epoch, info.id));
            }
        }
    }
    for (addr, was_up, why) in remove {
        let s = ss.remove(&addr).unwrap();
        if !why.is_empty() {
            acts.push(Act::Log(why));
        }
        acts.push(Act::Gone(s.bridge));
        acts.push(Act::Ended(addr, was_up));
    }
    acts
}

fn run(sh: &Arc<Shared>, acts: Vec<Act>) {
    for a in acts {
        match a {
            Act::Bridge(addr, epoch, id) => start_bridge(sh.clone(), addr, epoch, id),
            Act::Gone(bridge) => {
                if let Some(b) = bridge {
                    // Not cut: the writer still hands our mesh what already came from
                    // the peer, then closes (dropping the bridge ends its queue).
                    b.closed.store(true, Ordering::Relaxed);
                    sh.host.tunnel_down(b.mesh);
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
        }
    }
}

/// Connects a confirmed session to our mesh: the lower id's side has its mesh dial a
/// loopback listener, the other side dials its own mesh.
fn start_bridge(sh: Arc<Shared>, addr: SocketAddr, epoch: u64, id: PeerId) {
    let _ = thread::Builder::new().name("peerparty-inet-bridge".into()).spawn(move || {
        let tcp = (|| -> io::Result<TcpStream> {
            if sh.me.id < id {
                let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
                l.set_nonblocking(true)?;
                // Only our own mesh may come to this listener (see `own`).
                let _own = crate::own::tcp(l.local_addr()?.port());
                sh.host.dial_local(l.local_addr()?.port(), id);
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    match l.accept() {
                        Ok((s, _)) => {
                            s.set_nonblocking(false)?;
                            return Ok(s);
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            return Err(io::Error::new(io::ErrorKind::TimedOut, "our mesh didn't dial the tunnel"));
                        }
                        Err(e) => return Err(e),
                    }
                }
            } else {
                TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, sh.host.mesh_port())), Duration::from_secs(5))
            }
        })();
        let tcp = match tcp {
            Ok(t) => t,
            Err(e) => {
                sh.log(&format!("{}: couldn't connect the tunnel to our mesh: {e}", redact::addr(addr)));
                if let Some(s) = sh.sessions.lock().unwrap().get_mut(&addr).filter(|s| s.epoch == epoch) {
                    s.bridging = false;
                    s.next_bridge = Instant::now() + Duration::from_secs(10);
                }
                return;
            }
        };
        if let Err(e) = attach(&sh, addr, epoch, tcp) {
            sh.log(&format!("{}: tunnel bridge failed: {e}", redact::addr(addr)));
        }
    });
}

fn attach(sh: &Arc<Shared>, addr: SocketAddr, epoch: u64, tcp: TcpStream) -> io::Result<()> {
    let _ = tcp.set_nodelay(true);
    let relay = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?);
    relay.set_read_timeout(Some(Duration::from_millis(200)))?;
    let relay_port = relay.local_addr()?.port();
    // The relay takes our mesh's datagrams for this peer, and nobody else's (see `own`):
    // held for as long as the bridge or its thread has the socket.
    let (relay_own, relay_own_thread) = (crate::own::udp(relay_port), crate::own::udp(relay_port));
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let to_tcp_bytes = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicBool::new(false));
    let eof = Arc::new(AtomicBool::new(false));
    let mesh_udp = Arc::new(AtomicU16::new(0));
    let mut writer = tcp.try_clone()?;
    let mut reader = tcp.try_clone()?;
    let mesh = tcp.local_addr()?;
    let tag = redact::addr(addr);
    // Tell the engine what this connection is before any of the peer's bytes reach it.
    let tunnel = {
        let ss = sh.sessions.lock().unwrap();
        let Some(s) = ss.get(&addr).filter(|s| s.epoch == epoch) else {
            let _ = tcp.shutdown(std::net::Shutdown::Both);
            return Ok(());
        };
        let Some(info) = &s.info else { return Ok(()) };
        Tunnel { id: info.id, public: s.key.is_some(), key: s.fp.clone(), name: info.name.clone() }
    };
    let id = tunnel.id;
    sh.host.tunnel_up(mesh, tunnel);
    {
        let mut ss = sh.sessions.lock().unwrap();
        let Some(s) = ss.get_mut(&addr).filter(|s| s.epoch == epoch) else {
            drop(ss);
            let _ = tcp.shutdown(std::net::Shutdown::Both);
            sh.host.tunnel_down(mesh);
            return Ok(());
        };
        s.bridge = Some(Bridge {
            tcp,
            mesh,
            to_tcp: tx,
            to_tcp_bytes: to_tcp_bytes.clone(),
            relay: relay.clone(),
            relay_port,
            _relay_own: relay_own,
            mesh_udp: mesh_udp.clone(),
            closed: closed.clone(),
            eof: eof.clone(),
        });
        s.bridging = false;
        let early = std::mem::take(&mut s.early);
        if let Err(e) = s.deliver(early) {
            sh.log(&format!("{tag}: {e}"));
            closed.store(true, Ordering::Relaxed);
        }
    }
    sh.log(&format!("{tag}: the mesh connection to {id} now runs through the tunnel"));
    {
        let closed = closed.clone();
        thread::Builder::new().name("peerparty-inet-w".into()).spawn(move || {
            for chunk in rx {
                let ok = writer.write_all(&chunk).is_ok();
                to_tcp_bytes.fetch_sub(chunk.len(), Ordering::Relaxed);
                if !ok {
                    break;
                }
            }
            closed.store(true, Ordering::Relaxed);
            let _ = writer.shutdown(std::net::Shutdown::Both);
        })?;
    }
    {
        let (sh, closed, mesh_udp, eof) = (sh.clone(), closed.clone(), mesh_udp.clone(), eof.clone());
        thread::Builder::new().name("peerparty-inet-r".into()).spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            let mut hello = Some(Vec::new());
            // Public sessions: our Hello carries our public name, not the LAN one.
            let mut ours = sh.pubs.is_some().then(HelloFilter::default);
            loop {
                let n = match reader.read(&mut buf) {
                    // Our mesh finished writing: what it wrote is queued; the peer's
                    // side still flows to it until the session closes (FINISH_FOR).
                    Ok(0) if !closed.load(Ordering::Relaxed) => {
                        eof.store(true, Ordering::Relaxed);
                        return;
                    }
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let out: Vec<u8> = match &mut ours {
                    Some(filter) => {
                        let name = sh.me.name.clone();
                        let r = filter.edit(&buf[..n], &mut |_, n, udp| {
                            *n = name.clone();
                            mesh_udp.store(*udp, Ordering::Relaxed);
                            Ok(())
                        });
                        match r {
                            Ok(o) => o,
                            Err(_) => break,
                        }
                    }
                    None => {
                        if let Some(h) = &mut hello
                            && let Some(port) = sniff_hello(h, &buf[..n])
                        {
                            mesh_udp.store(port, Ordering::Relaxed);
                            hello = None;
                        }
                        buf[..n].to_vec()
                    }
                };
                if out.is_empty() {
                    continue;
                }
                loop {
                    if closed.load(Ordering::Relaxed) || sh.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    {
                        let mut ss = sh.sessions.lock().unwrap();
                        let Some(s) = ss.get_mut(&addr).filter(|s| s.epoch == epoch) else { return };
                        if s.stream.backlog() < stream::MAX_QUEUED {
                            s.stream.write(&out);
                            break;
                        }
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
            closed.store(true, Ordering::Relaxed);
        })?;
    }
    {
        let (sh, closed) = (sh.clone(), closed.clone());
        thread::Builder::new().name("peerparty-inet-u".into()).spawn(move || {
            let _own = relay_own_thread;
            let mut buf = vec![0u8; 65536];
            while !closed.load(Ordering::Relaxed) && !sh.stop.load(Ordering::Relaxed) {
                let Ok((n, from)) = relay.recv_from(&mut buf) else { continue };
                let port = mesh_udp.load(Ordering::Relaxed);
                if !from.ip().is_loopback() || port == 0 || from.port() != port {
                    continue;
                }
                let mut ss = sh.sessions.lock().unwrap();
                let Some(s) = ss.get_mut(&addr).filter(|s| s.epoch == epoch) else { return };
                s.send_dgram(&sh.cx(), &buf[..n], Instant::now());
            }
        })?;
    }
    Ok(())
}

/// Starts a session to `addr` unless there is one: it punches until answered, for
/// `punch_for` at most.
fn punch(sh: &Arc<Shared>, addr: SocketAddr, punch_for: Duration) -> bool {
    let mut ss = sh.sessions.lock().unwrap();
    if ss.contains_key(&addr) || ss.len() >= sh.max_sessions {
        return false;
    }
    if sh.pubs.is_some() && ss.values().filter(|s| s.addr.ip() == addr.ip()).count() >= PUBLIC_PER_IP {
        return false;
    }
    let now = Instant::now();
    if sh.is_blocked_addr(addr, now) {
        return false;
    }
    let mut s = Session::new(addr, sh.epoch(), now, false);
    s.punch_for = punch_for;
    s.send_init(&sh.cx(), now);
    ss.insert(addr, s);
    true
}

/// This PC's LAN address (the one its default route uses), or None offline or in
/// loopback mode. Sends nothing: connecting a UDP socket only picks a route.
fn lan_ipv4() -> Option<std::net::Ipv4Addr> {
    if crate::loopback_only() {
        return None;
    }
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

/// Keeps a port mapping on the router: makes one, renews it at half its lease, and
/// tries again later (a minute, doubling to half an hour) when the router has none.
fn portmap_loop(sh: &Arc<Shared>) {
    let mut next = Instant::now();
    let mut fails = 0u32;
    while !sh.stop_mapping.load(Ordering::Relaxed) {
        if Instant::now() >= next {
            let cur = sh.mapping.lock().unwrap().clone();
            let renewing = cur.is_some();
            let res = match cur {
                Some(mut m) => m.renew().map(|_| m),
                None => match lan_ipv4() {
                    Some(ip) => portmap::map(ip, portmap::default_gateway(), sh.local_port),
                    None => Err("no LAN address".into()),
                },
            };
            match res {
                Ok(m) => {
                    fails = 0;
                    next = m.renew_at();
                    set_mapping(sh, Some(m), None);
                }
                Err(e) => {
                    fails += 1;
                    next = Instant::now() + Duration::from_secs((60u64 << fails.min(5)).min(1800));
                    let note = if renewing { format!("lost: {e}") } else { format!("none ({e})") };
                    set_mapping(sh, None, Some(note));
                }
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Records a new mapping (or none) and points the DHT announce at it.
fn set_mapping(sh: &Arc<Shared>, m: Option<portmap::Mapping>, why: Option<String>) {
    let note = match &m {
        Some(m) => match m.external() {
            Some(e) => format!("{}: external port {}", m.method.name(), e.port()),
            None => format!("{}: mapped, but the router is behind another NAT (not announced)", m.method.name()),
        },
        None => format!("none ({})", why.as_deref().unwrap_or("?")),
    };
    let ext = m.as_ref().and_then(portmap::Mapping::external);
    {
        let mut cur = sh.mapping.lock().unwrap();
        if sh.stop_mapping.load(Ordering::Relaxed) {
            if let Some(m) = m {
                m.remove();
            }
            return;
        }
        *cur = m;
    }
    let mut n = sh.map_note.lock().unwrap();
    if n.as_deref() != Some(note.as_str()) {
        sh.log(&format!("port mapping: {}", why.map(|w| w.to_string()).unwrap_or(note.clone())));
        *n = Some(note);
        drop(n);
        let mut d = sh.disc.lock().unwrap();
        if let Some(e) = ext {
            d.selves.insert(e);
        }
        let port = ext.map(|e| e.port());
        if let Some(x) = &mut d.dht
            && x.mapped_port != port
        {
            x.mapped_port = port;
            d.next_lookup = Instant::now();
        }
    }
}

fn disc_loop(sh: &Arc<Shared>) {
    while !sh.stop.load(Ordering::Relaxed) {
        disc_tick(sh, Instant::now());
        thread::sleep(Duration::from_millis(100));
    }
}

/// Ends the sessions of players on the block list, and ignores their addresses a while.
fn kick_blocked(sh: &Arc<Shared>) {
    let Some(p) = &sh.pubs else { return };
    let now = Instant::now();
    let mut acts = Vec::new();
    {
        let mut ss = sh.sessions.lock().unwrap();
        let cx = sh.cx();
        let gone: Vec<SocketAddr> =
            ss.values().filter(|s| s.fp.as_deref().is_some_and(|f| p.blocked.is_blocked(f))).map(|s| s.addr).collect();
        for a in gone {
            let mut s = ss.remove(&a).unwrap();
            s.send_close(&cx, now);
            sh.block_addr(a, now);
            acts.push(Act::Log(format!("{} ({}) is on the block list: dropped", redact::addr(a), s.fp.as_deref().unwrap_or("?"))));
            acts.push(Act::Gone(s.bridge.take()));
            acts.push(Act::Ban(a));
        }
    }
    run(sh, acts);
}

fn disc_tick(sh: &Arc<Shared>, now: Instant) {
    if let Some(p) = &sh.pubs
        && p.blocked.refresh()
    {
        kick_blocked(sh);
    }
    // Names first, without the lock: DNS can take a while.
    let resolve_due = sh.disc.lock().unwrap().next_resolve <= now;
    if resolve_due {
        let seeds = if sh.cfg.dht { resolve(&sh.cfg.bootstrap) } else { Vec::new() };
        let stuns = resolve(&sh.cfg.stun);
        let statics = resolve_peers(&sh.cfg.peers, sh.sock6.is_some(), sh.cfg.allow_private);
        let mut d = sh.disc.lock().unwrap();
        let ok = (seeds.is_empty() == sh.cfg.bootstrap.is_empty() || !sh.cfg.dht)
            && stuns.is_empty() == sh.cfg.stun.is_empty()
            && statics.len() >= sh.cfg.peers.len().min(1);
        d.next_resolve = now + if ok { Duration::from_secs(600) } else { Duration::from_secs(30) };
        if !ok {
            sh.log("couldn't look up some names (offline?): trying again in 30 s");
        }
        if let Some(x) = &mut d.dht
            && !seeds.is_empty()
        {
            x.set_seeds(seeds);
        }
        if !stuns.is_empty() {
            d.stun_servers = stuns;
        }
        for a in &statics {
            if !d.statics.contains(a) {
                d.statics.push(*a);
            }
        }
    }
    let ended: Vec<(SocketAddr, bool)> = std::mem::take(&mut *sh.ended.lock().unwrap());
    let mut d = sh.disc.lock().unwrap();
    let d = &mut *d;
    let local = sh.sock.local_addr().ok();
    for (addr, up) in ended {
        let statik = d.statics.contains(&addr);
        let mut give_up = false;
        if let Some(c) = d.cands.get_mut(&addr) {
            if up {
                c.answered = true;
                c.fails = 0;
                c.next_try = now + Duration::from_secs(2);
            } else {
                c.fails += 1;
                // Addresses that answered this run and PEERPARTY_INET_PEERS are retried
                // soon; DHT finds that never answered back off, then go.
                let wait = if statik || (c.answered && c.fails <= 2) { 5 } else { (15u64 << c.fails.min(4)).min(300) };
                c.next_try = now + Duration::from_secs(wait);
                give_up = !statik
                    && if c.answered { c.fails >= ANSWERED_MAX_FAILS } else { c.from_dht && c.fails >= CAND_MAX_FAILS };
            }
        }
        if give_up {
            d.cands.remove(&addr);
            d.given_up.insert(addr, now + GIVE_UP_FOR);
            sh.log(&format!("giving up on {}: no answer after several tries", redact::addr(addr)));
        }
    }
    // STUN: which public address we have, now and every five minutes.
    if d.next_stun <= now && !d.stun_servers.is_empty() {
        d.next_stun = now + Duration::from_secs(300);
        d.stun_tx.clear();
        for s in d.stun_servers.clone() {
            let txid = crypto::random_array::<12>();
            d.stun_tx.insert(txid, s);
            let _ = sh.sock.send_to(&stun::request(&txid), s);
        }
    }
    // DHT: announce and look up this hour's topic (and the last hour's, early on).
    let connected = sh.sessions.lock().unwrap().values().any(|s| s.confirmed);
    if let Some(x) = &mut d.dht {
        for done in x.tick(now) {
            sh.log(&format!(
                "dht lookup {}: {} node(s) answered, {} address(es) found, announced to {}",
                done.id, done.replied, done.values, done.announced
            ));
        }
        if d.next_lookup <= now && !x.busy() {
            let slot = unix_now() / SLOT_SECS;
            let game = &sh.me.game;
            x.lookup(sh.cfg.room.topic(game, slot), Some(sh.local_port), now);
            if unix_now() % SLOT_SECS < 900 {
                x.lookup(sh.cfg.room.topic(game, slot - 1), None, now);
            }
            let early = now.duration_since(d.started) < Duration::from_secs(120);
            d.next_lookup = now + Duration::from_secs(if connected { 120 } else if early { 15 } else { 30 });
        }
        for (to, q) in x.drain_out() {
            let _ = sh.sock.send_to(&q, to);
        }
        // A find at our own public IP is another copy behind our router. Routers that
        // don't loop their own address back ("hairpinning") drop that path, so also try
        // the same port at our own LAN address: that reaches another copy on this PC
        // when the router kept the port. (Another PC on this LAN is found by LAN
        // discovery anyway.)
        let lan_ip = lan_ipv4();
        let this_hour = sh.cfg.room.topic(&sh.me.game, unix_now() / SLOT_SECS);
        for (topic, addr) in x.drain_found() {
            let ours = d.selves.iter().any(|s| s.ip() == addr.ip()) && !d.selves.contains(&addr);
            let mut add = vec![addr];
            if ours && let Some(ip) = lan_ip {
                let alt = SocketAddr::new(ip.into(), addr.port());
                if alt.port() != sh.local_port {
                    add.push(alt);
                }
            }
            for a in add {
                if d.given_up.get(&a).is_some_and(|t| *t > now) {
                    continue;
                }
                if d.cands.len() >= MAX_CANDS && !d.cands.contains_key(&a) {
                    continue;
                }
                let c = d.cands.entry(a).or_insert_with(|| Cand::new(true, now));
                c.seen = now;
                c.current |= topic == this_hour;
            }
        }
        if let Some(e) = x.external {
            d.selves.insert(e);
        }
    }
    for a in d.statics.clone() {
        d.cands.entry(a).or_insert_with(|| Cand::new(false, now));
    }
    d.banned.retain(|_, until| *until > now);
    d.given_up.retain(|_, until| *until > now);
    if d.next_save <= now {
        d.next_save = now + Duration::from_secs(300);
        if let Some(x) = &d.dht {
            save_nodes(&sh.cfg.node_cache, &x.good_nodes());
        }
    }
    // Punch what's due, best first: a capped number of addresses that haven't
    // answered at a time, so a peer's real address isn't starved by stale ones.
    let mut due: Vec<(SocketAddr, bool)> = {
        let mut v: Vec<(&SocketAddr, &Cand)> = d
            .cands
            .iter()
            .filter(|(a, c)| {
                c.next_try <= now && !d.selves.contains(a) && !d.banned.contains_key(a) && Some(**a) != local
            })
            .collect();
        v.sort_by_key(|(_, c)| c.rank());
        v.into_iter().map(|(a, c)| (*a, c.answered)).collect()
    };
    let mut room = {
        let ss = sh.sessions.lock().unwrap();
        let cap = sh.max_sessions * PUNCHING_QUARTERS / 4;
        cap.saturating_sub(ss.values().filter(|s| !s.inbound && !s.confirmed && s.peer.is_none()).count())
    };
    due.retain(|(_, answered)| {
        if *answered {
            return true;
        }
        let ok = room > 0;
        room = room.saturating_sub(1);
        ok
    });
    for (a, _) in due {
        let c = d.cands.get_mut(&a).unwrap();
        let punch_for = c.punch_for();
        c.next_try = now + punch_for + Duration::from_secs(1);
        if punch(sh, a, punch_for) {
            sh.log(&format!(
                "punching to {} ({})",
                redact::addr(a),
                if c.answered {
                    "it reached us before"
                } else if c.from_dht {
                    "found on the DHT"
                } else {
                    "PEERPARTY_INET_PEERS"
                }
            ));
        }
    }
    // DHT finds that never answered and no lookup returns any more go.
    d.cands.retain(|_, c| !c.from_dht || c.answered || now.duration_since(c.seen) < CAND_STALE);
}

/// What `probe` found out about this PC's internet connection.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub local_port: u16,
    pub stun: Vec<(String, Option<SocketAddr>)>,
    pub nat: Nat,
    pub dht_seeds: usize,
    pub dht_replies: u64,
    pub dht_external: Option<SocketAddr>,
    pub announced_to: usize,
    /// A second socket looked the test topic up and found the first socket's address.
    pub found_own_announce: bool,
    /// The router forwarded (and then stopped forwarding) a test port.
    pub port_mapped: bool,
    pub lines: Vec<String>,
}

/// Checks the real internet path, with no peer needed: STUN against every server, a DHT
/// bootstrap, an announce under a random topic, and a second socket looking that topic
/// up. Takes up to about a minute.
pub fn probe(stun_servers: &[String], bootstrap: &[String]) -> io::Result<Probe> {
    let mut p = Probe::default();
    let a = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    let b = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    for s in [&a, &b] {
        s.set_read_timeout(Some(Duration::from_millis(100)))?;
    }
    p.local_port = a.local_addr()?.port();
    let mut buf = vec![0u8; 65536];
    // STUN.
    let mut tx = HashMap::new();
    for name in stun_servers {
        let addrs = resolve(std::slice::from_ref(name));
        match addrs.first() {
            Some(&s) => {
                let txid = crypto::random_array::<12>();
                tx.insert(txid, name.clone());
                let _ = a.send_to(&stun::request(&txid), s);
            }
            None => p.lines.push(format!("stun {name}: no address (DNS)")),
        }
    }
    let mut got: HashMap<String, SocketAddr> = HashMap::new();
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until && got.len() < tx.len() {
        if let Ok((n, _)) = a.recv_from(&mut buf)
            && let Some((id, mapped)) = stun::parse_response(&buf[..n])
            && let Some(name) = tx.get(&id)
        {
            got.insert(name.clone(), mapped);
        }
    }
    for name in stun_servers {
        let m = got.get(name).copied();
        p.lines.push(match m {
            Some(m) => format!("stun {name}: we are {}", if redact::debug() { m.to_string() } else { redact::own(m) }),
            None => format!("stun {name}: no answer"),
        });
        p.stun.push((name.clone(), m));
    }
    let mapped: Vec<SocketAddr> = p.stun.iter().filter_map(|(_, m)| *m).collect();
    p.nat = Nat::classify(None, &mapped);
    p.lines.push(format!("nat: {}", p.nat.describe()));
    // Port mapping: ask the router to forward the test port, then delete it.
    let pm = match lan_ipv4() {
        Some(ip) => portmap::map(ip, portmap::default_gateway(), p.local_port),
        None => Err("no LAN address".into()),
    };
    p.lines.push(match pm {
        Ok(m) => {
            p.port_mapped = m.external().is_some();
            let line = match m.external() {
                Some(e) => format!("port mapping: {} forwards external port {} to us", m.method.name(), e.port()),
                None => format!("port mapping: {} works, but this router is behind another NAT (carrier NAT?)", m.method.name()),
            };
            m.remove();
            line
        }
        Err(e) => format!("port mapping: none ({e})"),
    });
    // DHT: announce from a, then look up from b.
    let seeds = resolve(bootstrap);
    p.dht_seeds = seeds.len();
    p.lines.push(format!("dht: {} bootstrap address(es)", seeds.len()));
    let topic: [u8; 20] = crypto::random_array();
    let run = |sock: &UdpSocket, announce: bool, buf: &mut [u8]| -> (Dht, Vec<dht::Done>) {
        let mut d = Dht::new(seeds.clone(), Vec::new());
        let t0 = Instant::now();
        d.lookup(topic, announce.then_some(1), t0);
        let mut done = Vec::new();
        while Instant::now() < t0 + Duration::from_secs(40) {
            done.extend(d.tick(Instant::now()));
            for (to, q) in d.drain_out() {
                let _ = sock.send_to(&q, to);
            }
            if !d.busy() && done.iter().all(|x: &dht::Done| x.announced == 0 || announce) {
                // Give announces a moment to be answered.
                let until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < until {
                    if let Ok((n, from)) = sock.recv_from(buf) {
                        d.handle(from, &buf[..n]);
                    }
                }
                break;
            }
            if let Ok((n, from)) = sock.recv_from(buf) {
                d.handle(from, &buf[..n]);
            }
        }
        (d, done)
    };
    let (da, done_a) = run(&a, true, &mut buf);
    p.dht_replies = da.replies;
    p.dht_external = da.external;
    p.announced_to = done_a.iter().map(|d| d.announced).sum();
    p.lines.push(format!(
        "dht: {} replies, {} nodes, external address {}, announced to {} node(s)",
        da.replies,
        da.good_nodes().len(),
        da.external.map(|e| if redact::debug() { e.to_string() } else { redact::own(e) }).unwrap_or("unknown".into()),
        p.announced_to
    ));
    let (mut db, _) = run(&b, false, &mut buf);
    let found: Vec<SocketAddr> = db.drain_found().into_iter().map(|(_, a)| a).collect();
    let want_port = da.external.map(|e| e.port()).or(mapped.first().map(|m| m.port()));
    p.found_own_announce = found.iter().any(|f| Some(f.port()) == want_port);
    p.lines.push(format!(
        "dht: a second socket found {} address(es) under the test topic{}",
        found.len(),
        if p.found_own_announce { ", including the first socket's" } else { "" }
    ));
    Ok(p)
}

/// Builds handshake packets the way a peer (or an attacker who knows the room) would:
/// for tests that throw forged and tampered ones at a running engine.
#[doc(hidden)]
pub mod testing {
    use super::*;

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
            Msg::Join { lobby: victim.into() },
            Msg::JoinReply { lobby: victim.into(), result: JoinResult::Ok },
            Msg::State { lobby: lobby.clone() },
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

        let mut s = Session::new(v4, 0, now, false);
        assert!(frames(&mut s, &cx(false, true), &f, now).is_ok());
        assert_eq!(s.alt, addrs[1..].iter().filter(|a| a.port() != 3).take(MAX_ALT).copied().collect::<Vec<_>>());
        assert!(s.probe_until.is_some() && s.path == v4);

        let mut s = Session::new(v4, 0, now, false);
        assert!(frames(&mut s, &cx(true, true), &f, now).is_ok());
        assert_eq!(s.alt[..2], addrs[1..3]);

        // No IPv6 socket here: ignored.
        let mut s = Session::new(v4, 0, now, false);
        assert!(frames(&mut s, &cx(true, false), &f, now).is_ok());
        assert!(s.alt.is_empty() && s.probe_until.is_none());

        // Cut short: a bad frame ends the session.
        let mut s = Session::new(v4, 0, now, false);
        assert!(frames(&mut s, &cx(true, true), &f[..f.len() - 1], now).is_err());
        assert!(frames(&mut s, &cx(true, true), &[F_ADDRS6], now).is_err());

        // The peer stopped listing the address we use: back to its IPv4 one.
        let mut s = Session::new(v4, 0, now, false);
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
        let mut f = HelloFilter::default();
        // Fed a byte at a time, it holds back until the whole hello is in.
        let mut out = Vec::new();
        for b in &bytes {
            out.extend(f.feed(&[*b], id, 7777, None).unwrap());
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
        assert_eq!(f.feed(b"more", id, 7777, None).unwrap(), b"more");
        // Someone else's hello is refused.
        let mut f = HelloFilter::default();
        assert!(f.feed(&bytes, PeerId::random(), 7777, None).is_err());
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
        let out = HelloFilter::default().feed(&frame("203.0.113.9"), id, 1, None).unwrap();
        assert_eq!(hello_name(&out), "Player");
        let out = HelloFilter::default().feed(&frame("Ro\u{202e}bin\n"), id, 1, None).unwrap();
        assert_eq!(hello_name(&out), "Robin");
        // A public player's is the one from its signed handshake.
        let out = HelloFilter::default().feed(&frame("whatever"), id, 1, Some("Signed")).unwrap();
        assert_eq!(hello_name(&out), "Signed");
        // Ours, on the way out to a public player.
        let mut udp_seen = 0;
        let out = HelloFilter::default()
            .edit(&frame("Sam"), &mut |_, n, udp| {
                *n = "Player-abcd".into();
                udp_seen = *udp;
                Ok(())
            })
            .unwrap();
        assert_eq!(hello_name(&out), "Player-abcd");
        assert_eq!(udp_seen, 5000);
    }

    #[test]
    fn hello_filter_survives_junk() {
        let id = PeerId::random();
        let mut rng = 0x1234_5678u64;
        for _ in 0..2000 {
            let mut f = HelloFilter::default();
            let n = (rng % 64) as usize;
            let mut junk = Vec::new();
            for _ in 0..n {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                junk.push(rng as u8);
            }
            let _ = f.feed(&junk, id, 1, None);
            let _ = f.feed(&junk, id, 1, Some("x"));
        }
    }
}