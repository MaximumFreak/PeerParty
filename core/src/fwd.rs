//! Games where a player types the host's IP address (no lobby SDK): the forwarder carries
//! their plain UDP and TCP traffic over the mesh, so they work with internet peers too.
//!
//! Each peer has a stand-in address (`redact::stand_in`, in 198.18.0.0/15, never
//! routed and never its real address). The hook rewrites the game's socket calls:
//!
//! - UDP: a `sendto` to stand-in:P goes to a local "alias" socket instead
//!   (`udp_dest`), which carries the datagram to the peer with a `[dst, src]` port
//!   header. The peer hands it to its own game from a matching alias, and replies take
//!   the same path back. `recvfrom` shows the alias as stand-in:P (`udp_from`).
//! - TCP: a `connect` to stand-in:P connects to a one-shot local listener instead
//!   (`tcp_dest`). Its stream travels as a reliable channel `peerparty-ip:t:P:n`; the
//!   peer connects to its game's port P on loopback and pumps the bytes back, with a
//!   1 MiB window so one stream can't fill the connection's send queue.
//!
//! The peer only delivers to ports its own game process has open (checked against the
//! system's socket tables), so a peer can't reach anything else on that PC. PeerParty's
//! own sockets in that process (the mesh's listener, the tunnels' ends, a local stand-in
//! service: `own`) are not the game's and are never delivered to: a peer that reached the
//! mesh this way would arrive from loopback and be taken for a player on this PC.
//!
//! A port the game keeps to this PC (a socket bound to 127.0.0.1 or `::1` only: a debug
//! console, an admin page, a local control port) isn't for strangers: public players are
//! never delivered to one. Friends (LAN and room players) still are, as they would be
//! if PeerParty weren't in between and the game listened everywhere: they're people the
//! player chose to play with. (Tests bind everything to loopback, so `loopback_only()`
//! mode lifts this unless a test turns it on, `keep_loopback_private`.) TCP goes to the
//! IPv4 address the game's socket has; `[::1]` is tried only when the game has an IPv6
//! socket on that port, so a stream never lands in another program's socket.
//!
//! Stand-ins are 17 bits of a hash of the peer's id, so two peers can have the same one
//! (and a stranger can pick an id to make it so). An address then means the peer we
//! trust most, and of those the one here longest (`Lan::peers_by_standing`): a public
//! player can't take over the address a friend or a LAN player already answers at.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use crate::redact::stand_in;
use crate::{Event, Lan, Packet, PeerId, SendResult, Trust};

/// Every socket name the forwarder uses starts with this.
pub const PREFIX: &str = "peerparty-ip:";
const UDP_SOCKET: &str = "peerparty-ip:u";
const TCP_PREFIX: &str = "peerparty-ip:t:";
const DATA: u8 = 0;
const CONTROL: u8 = 1;
const EOF: u8 = 0;
const ACK: u8 = 1;
const OPEN: u8 = 2;
/// Everything the peer sent us is written to our game (after its `EOF`): the peer may
/// close the channel once it has this and is done itself, since closing drops anything
/// still queued.
const DONE: u8 = 3;
/// Bytes a stream may have in flight before it waits for the peer's acks.
const WINDOW: u64 = 1 << 20;
const ACK_EVERY: u64 = 64 << 10;
const CHUNK: usize = 16 << 10;
const UDP_IDLE: Duration = Duration::from_secs(120);
const MAX_UDP_PER_PEER: usize = 64;
const MAX_TCP_PER_PEER: usize = 32;
const OPEN_WAIT: Duration = Duration::from_secs(10);
/// How long a port found in the socket tables counts as still open.
const OWNER_CACHE: Duration = Duration::from_secs(5);
/// How long the list of peers behind stand-in addresses is used before it's read again.
const PEERS_FRESH: Duration = Duration::from_secs(2);
/// A port that isn't in the tables we read is looked for again (the game may have just
/// opened it) only once they're this old: a peer naming one port after another can't
/// make us read the system's socket tables for each. Short, because until then a socket
/// the game has only just opened gets nothing (answers to what the game itself sent
/// don't wait for this, see `packet`).
const OWNER_REFRESH: Duration = Duration::from_millis(50);
/// Packets handled before the pump looks at the engine's events again.
const PUMP_BATCH: usize = 512;

/// Whether `ip` is in the stand-in range (198.18.0.0/15).
pub fn is_stand_in(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 198 && o[1] & 0xfe == 18
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Proto {
    Udp,
    Tcp,
}

struct Alias {
    sock: UdpSocket,
    addr: SocketAddrV4,
    peer: PeerId,
    /// The peer's port: the header's `src` going out, its `dst` coming in.
    remote: u16,
    /// Our game's port.
    local: u16,
    /// Where our game listens (loopback, or the address it bound to).
    target: SocketAddrV4,
    /// The peer's stand-in, worked out once: `udp_from` is asked for every datagram the
    /// game reads, and the stand-in is an HMAC.
    shown: Ipv4Addr,
    born: Instant,
    /// When a datagram last went through, in ms after `born` (`touch`), as both
    /// directions note it per datagram.
    last_ms: AtomicU64,
    dead: AtomicBool,
    /// The alias's own port is ours, not the game's.
    _own: crate::own::Held,
}

struct Flow {
    sent: u64,
    acked: u64,
    open: bool,
    closed: bool,
    reading: bool,
    writing: bool,
    /// The peer sent `DONE`.
    peer_done: bool,
}

struct Tcp {
    peer: PeerId,
    socket: String,
    stream: TcpStream,
    out: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    flow: Mutex<Flow>,
    cv: Condvar,
}

#[derive(Default)]
struct St {
    udp: HashMap<(PeerId, u16, u16), Arc<Alias>>,
    by_addr: HashMap<SocketAddrV4, Arc<Alias>>,
    tcp: HashMap<(PeerId, String), Arc<Tcp>>,
    /// Streams a peer asked for that are still connecting to the game: they count toward
    /// its limit, and one closed (or whose peer left) meanwhile is dropped once connected.
    connecting: HashSet<(PeerId, String)>,
    /// The ports this process has open, and when the socket tables were read.
    owned: Option<(Instant, Owned)>,
    peers: Vec<(Ipv4Addr, PeerId)>,
    peers_at: Option<Instant>,
}

/// One per process: carries typed-IP traffic for the game over `lan`.
pub struct Forwarder {
    lan: Lan,
    st: Mutex<St>,
    me: Weak<Forwarder>,
    pumping: AtomicBool,
    /// The pump thread's (`ensure_pump`): it holds the forwarder only by `me`, a moment
    /// at a time, so dropping the last handle drops the forwarder (and its `Lan`), and
    /// the drop tells the pump to stop.
    pump: Arc<Pump>,
    /// The hook ticks the engine and hands us its events; `None`: we do it ourselves.
    tick: Option<fn()>,
    next: AtomicU64,
    /// Streams a peer asked for that were refused: their log lines are thinned out.
    refused: AtomicU64,
    /// Public players are kept off ports the game bound to loopback only (see the
    /// module docs). Off in `loopback_only()` mode, where every socket is on loopback.
    loopback_private: AtomicBool,
    /// Some UDP alias is on a LAN address (a game socket bound to one gets its peers'
    /// datagrams from there): senders that aren't loopback are looked up too.
    lan_aliases: AtomicBool,
    log: fn(&str),
}

#[derive(Default)]
struct Pump {
    /// Set when the forwarder drops.
    stop: AtomicBool,
    /// Set by the pump thread as it ends.
    done: AtomicBool,
}

/// Why a peer's traffic for a port isn't handed to our game.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refused {
    /// The port is one of PeerParty's own sockets.
    Own,
    /// Our game has nothing there.
    NotOpen,
    /// The game keeps it to this PC, and the peer is a public player.
    LoopbackOnly,
}

impl Refused {
    fn why(self) -> &'static str {
        match self {
            Refused::Own => "that port is peerparty's own, not the game's",
            Refused::NotOpen => "the game isn't listening there",
            Refused::LoopbackOnly => "the game keeps that port to this PC (bound to loopback), and public players don't get those",
        }
    }
}

impl Forwarder {
    /// `tick` runs every few ms on the pump thread; it should tick the engine and pass
    /// prefixed channel events and `PeerLeft` to `event`. With `None` the forwarder ticks
    /// and polls `lan` itself (it then sees every event; only for a process with no
    /// other user of `lan`).
    pub fn new(lan: Lan, tick: Option<fn()>, log: fn(&str)) -> Arc<Forwarder> {
        Arc::new_cyclic(|me| Forwarder {
            lan,
            st: Mutex::new(St::default()),
            me: me.clone(),
            pumping: AtomicBool::new(false),
            pump: Arc::new(Pump::default()),
            tick,
            next: AtomicU64::new(1),
            refused: AtomicU64::new(0),
            loopback_private: AtomicBool::new(!crate::loopback_only()),
            lan_aliases: AtomicBool::new(false),
            log,
        })
    }

    /// Whether a datagram's sender can be one of our aliases: a loopback one always
    /// can, another one only once an alias sits on a LAN address.
    pub fn may_be_alias(&self, from: Ipv4Addr) -> bool {
        from.is_loopback() || self.lan_aliases.load(Ordering::Relaxed)
    }

    /// Whether public players are kept off ports the game bound to loopback only. On
    /// unless in `loopback_only()` mode; tests turn it on to check it.
    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub fn keep_loopback_private(&self, on: bool) {
        self.loopback_private.store(on, Ordering::Relaxed);
    }

    fn arc(&self) -> Option<Arc<Forwarder>> {
        self.me.upgrade()
    }

    /// Logs a stream that was refused: the first few, then every power of two, since a
    /// peer can ask for as many as it likes.
    fn log_refused(&self, line: &str) {
        let n = self.refused.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 3 || n.is_power_of_two() {
            (self.log)(&format!("{line} ({n} refused so far)"));
        }
    }

    /// The peer behind a stand-in address.
    pub fn peer_at(&self, ip: Ipv4Addr) -> Option<PeerId> {
        if !is_stand_in(ip) {
            return None;
        }
        let mut st = self.st.lock().unwrap();
        let find = |st: &St| st.peers.iter().find(|(a, _)| *a == ip).map(|(_, p)| *p);
        // A hit is kept for PEERS_FRESH (a peer that left, or one more trusted at the
        // same stand-in, shows within that); a miss looks again after 200 ms at most.
        let age = st.peers_at.map(|t| t.elapsed());
        if let Some(p) = find(&st).filter(|_| age.is_some_and(|a| a < PEERS_FRESH)) {
            return Some(p);
        }
        if age.is_some_and(|a| a < Duration::from_millis(200)) {
            return find(&st);
        }
        // Best first: `find` takes the first peer at an address (see the module docs).
        st.peers = self.lan.peers_by_standing().into_iter().map(|id| (stand_in(&id.0), id)).collect();
        st.peers_at = Some(Instant::now());
        find(&st)
    }

    /// The stand-in address games see for `peer`.
    pub fn address_of(peer: PeerId) -> Ipv4Addr {
        stand_in(&peer.0)
    }

    /// Where a UDP datagram from our game's port `local` to `dest` should really go,
    /// when `dest` is a peer's stand-in.
    pub fn udp_dest(&self, dest: SocketAddrV4, local: u16) -> Option<SocketAddrV4> {
        let peer = self.peer_at(*dest.ip())?;
        self.ensure_pump();
        let a = self.alias(peer, dest.port(), local)?;
        a.touch();
        Some(a.addr)
    }

    /// What our game should see as the sender of a datagram from `from`: the peer's
    /// stand-in and port, when `from` is one of our aliases.
    pub fn udp_from(&self, from: SocketAddrV4) -> Option<SocketAddrV4> {
        let st = self.st.lock().unwrap();
        let a = st.by_addr.get(&from)?;
        Some(SocketAddrV4::new(a.shown, a.remote))
    }

    /// Where a TCP `connect` to `dest` should really go, when `dest` is a peer's stand-in.
    pub fn tcp_dest(&self, dest: SocketAddrV4) -> Option<SocketAddrV4> {
        let peer = self.peer_at(*dest.ip())?;
        let me = self.arc()?;
        self.ensure_pump();
        let count = self.st.lock().unwrap().tcp.keys().filter(|(p, _)| *p == peer).count();
        if count >= MAX_TCP_PER_PEER {
            (self.log)("ipfwd: too many TCP streams to one peer; refusing another");
            return None;
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).ok()?;
        let addr = match listener.local_addr().ok()? {
            SocketAddr::V4(a) => a,
            _ => return None,
        };
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let socket = format!("{TCP_PREFIX}{}:{n}", dest.port());
        let own = crate::own::tcp(addr.port());
        thread::Builder::new()
            .name("peerparty-ipfwd-accept".into())
            .spawn(move || {
                let _own = own;
                if listener.set_nonblocking(true).is_err() {
                    return;
                }
                let deadline = Instant::now() + OPEN_WAIT;
                while Instant::now() < deadline {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            me.run_tcp(peer, socket, stream, true);
                            return;
                        }
                        Err(_) => thread::sleep(Duration::from_millis(5)),
                    }
                }
            })
            .ok()?;
        Some(addr)
    }

    /// A channel event from the engine (others are ignored).
    pub fn event(&self, ev: &Event) {
        match ev {
            Event::ChanRequest { peer, socket } if socket.starts_with(PREFIX) => self.request(*peer, socket),
            Event::ChanOpen { peer, socket } => {
                if let Some(t) = self.find_tcp(*peer, socket) {
                    t.flow.lock().unwrap().open = true;
                    t.cv.notify_all();
                }
            }
            Event::ChanClosed { peer, socket, .. } => {
                self.st.lock().unwrap().connecting.remove(&(*peer, socket.clone()));
                if let Some(t) = self.find_tcp(*peer, socket) {
                    t.kill();
                }
            }
            Event::PeerLeft(peer) => {
                let (tcps, aliases): (Vec<_>, Vec<_>) = {
                    let mut st = self.st.lock().unwrap();
                    st.connecting.retain(|(p, _)| p != peer);
                    (
                        st.tcp.values().filter(|t| t.peer == *peer).cloned().collect(),
                        st.udp.values().filter(|a| a.peer == *peer).cloned().collect(),
                    )
                };
                tcps.iter().for_each(|t| t.kill());
                aliases.iter().for_each(|a| a.dead.store(true, Ordering::Relaxed));
            }
            _ => {}
        }
    }

    /// Whether an event concerns the forwarder (for a caller that filters).
    pub fn wants(ev: &Event) -> bool {
        match ev {
            Event::ChanRequest { socket, .. } | Event::ChanOpen { socket, .. } | Event::ChanClosed { socket, .. } => {
                socket.starts_with(PREFIX)
            }
            Event::PeerLeft(_) => true,
            _ => false,
        }
    }

    /// The internet peer behind a connection the game accepted from `game_sees` (the
    /// address its socket reports for the other end): streams we open to the game come
    /// from loopback, so a game server can't tell them from a local client otherwise.
    /// Also answers for datagrams from one of our UDP aliases.
    pub fn forwarded_peer(&self, game_sees: SocketAddr) -> Option<PeerId> {
        let st = self.st.lock().unwrap();
        // The alias knows its peer: not through its stand-in address, which two peers
        // can share and which `peer_at` reads from a list that may not have it yet.
        if let SocketAddr::V4(v4) = game_sees
            && let Some(a) = st.by_addr.get(&v4)
        {
            return Some(a.peer);
        }
        st.tcp.values().find(|t| t.stream.local_addr().ok() == Some(game_sees)).map(|t| t.peer)
    }

    /// Streams and aliases alive (tests, the overlay).
    pub fn counts(&self) -> (usize, usize) {
        let st = self.st.lock().unwrap();
        (st.tcp.len(), st.udp.len())
    }

    fn find_tcp(&self, peer: PeerId, socket: &str) -> Option<Arc<Tcp>> {
        if !socket.starts_with(TCP_PREFIX) {
            return None;
        }
        self.st.lock().unwrap().tcp.get(&(peer, socket.to_string())).cloned()
    }

    fn request(&self, peer: PeerId, socket: &str) {
        if socket == UDP_SOCKET {
            self.lan.accept(peer, socket);
            return;
        }
        let port = socket
            .strip_prefix(TCP_PREFIX)
            .and_then(|r| r.split(':').next())
            .and_then(|p| p.parse::<u16>().ok());
        let Some(port) = port else {
            self.lan.close(peer, Some(socket));
            return;
        };
        let key = (peer, socket.to_string());
        let count = {
            let st = self.st.lock().unwrap();
            if st.tcp.contains_key(&key) || st.connecting.contains(&key) {
                // Asked again while it's open or connecting: the one there stands.
                return;
            }
            st.tcp.keys().chain(st.connecting.iter()).filter(|(p, _)| *p == peer).count()
        };
        let target = match self.for_peer(peer, Proto::Tcp, port) {
            Ok(t) if count < MAX_TCP_PER_PEER => Ok(t),
            Ok(_) => Err("too many streams from that peer"),
            Err(r) => Err(r.why()),
        };
        let (Some(me), Ok((target, strict))) = (self.arc(), target) else {
            self.log_refused(&format!("ipfwd: refused a TCP stream to port {port}: {}", target.err().unwrap_or("shutting down")));
            self.lan.close(peer, Some(socket));
            return;
        };
        let socket = socket.to_string();
        self.st.lock().unwrap().connecting.insert(key.clone());
        let gone = key.clone();
        let spawned = thread::Builder::new().name("peerparty-ipfwd-connect".into()).spawn(move || {
            let mut stream = Err(std::io::Error::from(std::io::ErrorKind::NotFound));
            for ip in target.connect_to(strict) {
                stream = TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs(3));
                if stream.is_ok() {
                    break;
                }
            }
            // Closed, or the peer gone, while connecting: nobody wants it now.
            if !me.st.lock().unwrap().connecting.remove(&key) {
                return;
            }
            match stream {
                Ok(s) => me.run_tcp(peer, socket, s, false),
                Err(e) => {
                    me.log_refused(&format!("ipfwd: couldn't reach the game on port {port}: {e}"));
                    me.lan.close(peer, Some(&socket));
                }
            }
        });
        if spawned.is_err() {
            self.st.lock().unwrap().connecting.remove(&gone);
            self.lan.close(peer, Some(&gone.1));
        }
    }

    /// Runs one stream until both directions finish (on the calling thread).
    fn run_tcp(&self, peer: PeerId, socket: String, stream: TcpStream, opener: bool) {
        let _ = stream.set_nodelay(true);
        let (Ok(read_half), Ok(write_half)) = (stream.try_clone(), stream.try_clone()) else {
            self.lan.close(peer, Some(&socket));
            return;
        };
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let t = Arc::new(Tcp {
            peer,
            socket: socket.clone(),
            stream,
            out: Mutex::new(Some(tx)),
            flow: Mutex::new(Flow { sent: 0, acked: 0, open: !opener, closed: false, reading: true, writing: true, peer_done: false }),
            cv: Condvar::new(),
        });
        self.st.lock().unwrap().tcp.insert((peer, socket.clone()), t.clone());
        if opener {
            self.lan.send(peer, &socket, CONTROL, &[OPEN], true, true, true);
        } else {
            self.lan.accept(peer, &socket);
        }
        // Peer → game.
        let writer = {
            let (t, lan) = (t.clone(), self.lan.clone());
            thread::Builder::new().name("peerparty-ipfwd-write".into()).spawn(move || {
                let mut w = write_half;
                let (mut done, mut acked) = (0u64, 0u64);
                while let Ok(buf) = rx.recv() {
                    if buf.is_empty() {
                        let _ = w.shutdown(Shutdown::Write);
                        lan.send(t.peer, &t.socket, CONTROL, &[DONE], true, true, false);
                        break;
                    }
                    if w.write_all(&buf).is_err() {
                        t.kill();
                        break;
                    }
                    done += buf.len() as u64;
                    if done - acked >= ACK_EVERY {
                        acked = done;
                        let mut m = vec![ACK];
                        m.extend_from_slice(&done.to_be_bytes());
                        lan.send(t.peer, &t.socket, CONTROL, &m, true, true, false);
                    }
                }
                t.flow.lock().unwrap().writing = false;
                t.cv.notify_all();
            })
        };
        // Game → peer.
        {
            let mut f = t.flow.lock().unwrap();
            let deadline = Instant::now() + OPEN_WAIT;
            while !f.open && !f.closed && Instant::now() < deadline {
                f = t.cv.wait_timeout(f, Duration::from_millis(100)).unwrap().0;
            }
            if !f.open {
                f.closed = true;
            }
        }
        let mut r = read_half;
        let mut buf = vec![0u8; CHUNK];
        loop {
            {
                let mut f = t.flow.lock().unwrap();
                while !f.closed && f.sent - f.acked >= WINDOW {
                    f = t.cv.wait_timeout(f, Duration::from_millis(500)).unwrap().0;
                }
                if f.closed {
                    break;
                }
            }
            let n = match r.read(&mut buf) {
                Ok(0) | Err(_) => {
                    self.lan.send(peer, &socket, CONTROL, &[EOF], true, true, false);
                    break;
                }
                Ok(n) => n,
            };
            match self.lan.send(peer, &socket, DATA, &buf[..n], true, true, false) {
                SendResult::Sent | SendResult::Held => t.flow.lock().unwrap().sent += n as u64,
                _ => {
                    t.kill();
                    break;
                }
            }
        }
        {
            let mut f = t.flow.lock().unwrap();
            f.reading = false;
            while !f.closed && (f.writing || !f.peer_done) {
                f = t.cv.wait_timeout(f, Duration::from_millis(500)).unwrap().0;
            }
            if !f.closed {
                drop(f);
                self.lan.close(peer, Some(&socket));
            }
        }
        t.kill();
        if let Ok(h) = writer {
            let _ = h.join();
        }
        self.lan.close(peer, Some(&socket));
        let mut st = self.st.lock().unwrap();
        let key = (peer, socket);
        // Only this stream's entry: a later one under the same name isn't ours to drop.
        if st.tcp.get(&key).is_some_and(|x| Arc::ptr_eq(x, &t)) {
            st.tcp.remove(&key);
        }
    }

    fn alias(&self, peer: PeerId, remote: u16, local: u16) -> Option<Arc<Alias>> {
        let key = (peer, remote, local);
        if let Some(a) = self.st.lock().unwrap().udp.get(&key)
            && !a.dead.load(Ordering::Relaxed)
        {
            return Some(a.clone());
        }
        let ip = self.owner(Proto::Udp, local).map_or(Ipv4Addr::LOCALHOST, |p| p.ip);
        let me = self.arc()?;
        let mut st = self.st.lock().unwrap();
        if let Some(a) = st.udp.get(&key)
            && !a.dead.load(Ordering::Relaxed)
        {
            return Some(a.clone());
        }
        if st.udp.keys().filter(|(p, _, _)| *p == peer).count() >= MAX_UDP_PER_PEER {
            return None;
        }
        let sock = UdpSocket::bind((ip, 0)).ok()?;
        let SocketAddr::V4(addr) = sock.local_addr().ok()? else { return None };
        if !ip.is_loopback() {
            self.lan_aliases.store(true, Ordering::Relaxed);
        }
        sock.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
        let a = Arc::new(Alias {
            sock,
            addr,
            peer,
            remote,
            local,
            target: SocketAddrV4::new(ip, local),
            shown: stand_in(&peer.0),
            born: Instant::now(),
            last_ms: AtomicU64::new(0),
            dead: AtomicBool::new(false),
            _own: crate::own::udp(addr.port()),
        });
        st.udp.insert(key, a.clone());
        st.by_addr.insert(addr, a.clone());
        drop(st);
        let a2 = a.clone();
        let spawned = thread::Builder::new().name("peerparty-ipfwd-udp".into()).spawn(move || {
            let mut buf = vec![0u8; 65536 + 4];
            loop {
                if a2.dead.load(Ordering::Relaxed) || a2.idle() > UDP_IDLE {
                    break;
                }
                let Ok((n, from)) = a2.sock.recv_from(&mut buf[4..]) else { continue };
                // Only our game's socket may use the alias.
                let ours = from.ip() == IpAddr::V4(*a2.target.ip()) || from.ip().is_loopback();
                if from.port() != a2.local || !ours {
                    continue;
                }
                a2.touch();
                buf[0..2].copy_from_slice(&a2.remote.to_be_bytes());
                buf[2..4].copy_from_slice(&a2.local.to_be_bytes());
                me.lan.send(a2.peer, UDP_SOCKET, DATA, &buf[..n + 4], false, true, true);
            }
            a2.dead.store(true, Ordering::Relaxed);
            let mut st = me.st.lock().unwrap();
            if st.udp.get(&key).is_some_and(|x| Arc::ptr_eq(x, &a2)) {
                st.udp.remove(&key);
            }
            if st.by_addr.get(&a2.addr).is_some_and(|x| Arc::ptr_eq(x, &a2)) {
                st.by_addr.remove(&a2.addr);
            }
        });
        spawned.ok()?;
        Some(a)
    }

    /// Where our game has `port` open, if `peer` may be handed to it there: not one of
    /// PeerParty's own ports, and for a public player not one the game keeps to this PC.
    /// Also says whether the peer is kept off what the game keeps to this PC (for
    /// `Port::connect_to`).
    fn for_peer(&self, peer: PeerId, proto: Proto, port: u16) -> Result<(Port, bool), Refused> {
        if crate::own::is_held(proto == Proto::Tcp, port) {
            return Err(Refused::Own);
        }
        let found = self.owner(proto, port).ok_or(Refused::NotOpen)?;
        // A peer that's gone counts as a stranger.
        let strict = self.loopback_private.load(Ordering::Relaxed) && self.lan.peer_trust(peer).is_none_or(|t| t == Trust::Public);
        let kept = match proto {
            Proto::Udp => found.kept_local(),
            Proto::Tcp => found.connect_to(true).is_empty(),
        };
        if strict && kept {
            return Err(Refused::LoopbackOnly);
        }
        Ok((found, strict))
    }

    /// Where our own game has `port` open (`None`: it doesn't, or the port is one of
    /// PeerParty's own).
    fn owner(&self, proto: Proto, port: u16) -> Option<Port> {
        if crate::own::is_held(proto == Proto::Tcp, port) {
            return None;
        }
        {
            let st = self.st.lock().unwrap();
            if let Some((at, owned)) = &st.owned {
                let found = owned.get(proto, port);
                if (found.is_some() && at.elapsed() < OWNER_CACHE) || at.elapsed() < OWNER_REFRESH {
                    return found;
                }
            }
        }
        let owned = Owned::read();
        let found = owned.get(proto, port);
        self.st.lock().unwrap().owned = Some((Instant::now(), owned));
        found
    }

    fn packet(&self, p: Packet) {
        if p.socket == UDP_SOCKET {
            if p.channel != DATA || p.data.len() < 4 {
                return;
            }
            let dst = u16::from_be_bytes([p.data[0], p.data[1]]);
            let src = u16::from_be_bytes([p.data[2], p.data[3]]);
            // An answer to what our game sent this peer from that port: the game has
            // the port, as it just used it. Only a port a peer is first to name is
            // looked up. (Looked up always, a socket opened since the tables were last
            // read lost the answer to its first datagram.)
            let answering = self.st.lock().unwrap().udp.get(&(p.peer, src, dst)).is_some_and(|a| !a.dead.load(Ordering::Relaxed));
            if !answering
                && let Err(r) = self.for_peer(p.peer, Proto::Udp, dst)
            {
                if r == Refused::LoopbackOnly {
                    self.log_refused(&format!("ipfwd: dropped datagrams for port {dst}: {}", r.why()));
                }
                return;
            }
            if let Some(a) = self.alias(p.peer, src, dst) {
                a.touch();
                let _ = a.sock.send_to(&p.data[4..], a.target);
            }
            return;
        }
        let Some(t) = self.find_tcp(p.peer, &p.socket) else { return };
        match (p.channel, p.data.first()) {
            (DATA, _) => {
                if let Some(tx) = t.out.lock().unwrap().as_ref()
                    && !p.data.is_empty()
                {
                    let _ = tx.send(p.data);
                }
            }
            (CONTROL, Some(&EOF)) => {
                if let Some(tx) = t.out.lock().unwrap().take() {
                    let _ = tx.send(Vec::new());
                }
            }
            (CONTROL, Some(&DONE)) => {
                t.flow.lock().unwrap().peer_done = true;
                t.cv.notify_all();
            }
            (CONTROL, Some(&ACK)) if p.data.len() == 9 => {
                let n = u64::from_be_bytes(p.data[1..9].try_into().unwrap());
                let mut f = t.flow.lock().unwrap();
                if n <= f.sent && n > f.acked {
                    f.acked = n;
                }
                drop(f);
                t.cv.notify_all();
            }
            _ => {}
        }
    }

    /// Starts the thread that takes our packets off the engine (once).
    pub fn ensure_pump(&self) {
        if self.pumping.swap(true, Ordering::AcqRel) {
            return;
        }
        let (weak, pump) = (self.me.clone(), self.pump.clone());
        let _ = thread::Builder::new().name("peerparty-ipfwd".into()).spawn(move || {
            let mut last_tick = Instant::now() - Duration::from_secs(1);
            // One pass at a time with the forwarder in hand, let go before the sleep: a
            // forwarder nobody else holds is dropped, and the pump ends.
            while !pump.stop.load(Ordering::Acquire) {
                let Some(me) = weak.upgrade() else { break };
                let mut events = Vec::new();
                if last_tick.elapsed() >= Duration::from_millis(5) {
                    last_tick = Instant::now();
                    match me.tick {
                        Some(f) => f(),
                        None => {
                            me.lan.tick();
                            events = me.lan.poll();
                        }
                    }
                }
                // Packets first: they arrived before any close among the events. A batch
                // at a time, so a peer that never stops sending can't keep the engine's
                // tick and events waiting.
                let mut busy = false;
                for _ in 0..PUMP_BATCH {
                    let Some(p) = me.lan.receive_where(None, |s| s.starts_with(PREFIX)) else { break };
                    busy = true;
                    me.packet(p);
                }
                for ev in &events {
                    me.event(ev);
                }
                let lan = me.lan.clone();
                drop(me);
                // Woken by the next packet for us rather than polling for it: a sleep
                // of 1 ms is 15.6 on the default timer, and each forwarded datagram
                // waited for it. At most until the next tick is due.
                if !busy {
                    let due = Duration::from_millis(5).saturating_sub(last_tick.elapsed()).max(Duration::from_millis(1));
                    lan.wait_for_packet(due, |s| s.starts_with(PREFIX));
                }
            }
            pump.done.store(true, Ordering::Release);
        });
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        // Not joined: the last handle may be the pump's own, dropped on its thread.
        self.pump.stop.store(true, Ordering::Release);
    }
}

impl Alias {
    fn touch(&self) {
        self.last_ms.store(self.born.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// How long since a datagram last went through.
    fn idle(&self) -> Duration {
        self.born.elapsed().saturating_sub(Duration::from_millis(self.last_ms.load(Ordering::Relaxed)))
    }
}

impl Tcp {
    fn kill(&self) {
        self.flow.lock().unwrap().closed = true;
        self.out.lock().unwrap().take();
        let _ = self.stream.shutdown(Shutdown::Both);
        self.cv.notify_all();
    }
}

/// One port of our game's, from the socket tables (every socket of this process on it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Port {
    /// Where IPv4 traffic for it goes: the first IPv4 socket's own address, loopback
    /// for "any" (or when only IPv6 sockets have the port).
    ip: Ipv4Addr,
    /// An IPv4 socket on it is bound to loopback only (127.x): the game keeps the port
    /// to this PC. Any one is enough: a socket bound to 127.0.0.1 gets loopback traffic
    /// before one bound to every address.
    loopback4: bool,
    /// The same for its IPv6 sockets (`::1`, or IPv4-mapped loopback).
    loopback6: bool,
    /// An IPv4 socket has it.
    v4: bool,
    /// An IPv6 socket has it.
    v6: bool,
}

impl Port {
    /// Whether the socket a datagram for it reaches (the IPv4 one if there is one) is
    /// kept to this PC.
    fn kept_local(&self) -> bool {
        if self.v4 { self.loopback4 } else { self.loopback6 }
    }

    /// Where to connect, in order: the IPv4 socket's address, then `[::1]` if an IPv6
    /// socket of the game's has the port (never another program's). For a public
    /// player, neither one the game keeps to this PC.
    fn connect_to(&self, public: bool) -> Vec<IpAddr> {
        let mut to = Vec::new();
        if self.v4 && !(public && self.loopback4) {
            to.push(IpAddr::V4(self.ip));
        }
        if self.v6 && !(public && self.loopback6) {
            to.push(IpAddr::V6(Ipv6Addr::LOCALHOST));
        }
        to
    }
}

/// One row of the socket tables that's ours: its port, its IPv4 address (`None` for an
/// IPv6 socket) and whether it's bound to loopback only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Row {
    port: u16,
    v4: Option<Ipv4Addr>,
    loopback: bool,
}

impl Row {
    /// The row of a socket at `addr`.
    fn of(addr: SocketAddr) -> Row {
        match addr {
            SocketAddr::V4(a) => Row { port: a.port(), v4: Some(*a.ip()), loopback: a.ip().is_loopback() },
            SocketAddr::V6(a) => {
                let ip = a.ip();
                let loopback = ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v| v.is_loopback());
                Row { port: a.port(), v4: None, loopback }
            }
        }
    }
}

/// The ports this process has open, from the system's socket tables: TCP listeners and
/// UDP sockets.
#[derive(Default)]
struct Owned {
    ports: HashMap<(Proto, u16), Port>,
    /// The tables can't be read here: every port counts as open, on loopback, and none
    /// as kept to this PC.
    all: bool,
}

impl Owned {
    fn get(&self, proto: Proto, port: u16) -> Option<Port> {
        if self.all {
            return Some(Port { ip: Ipv4Addr::LOCALHOST, loopback4: false, loopback6: false, v4: true, v6: true });
        }
        self.ports.get(&(proto, port)).copied()
    }

    fn add(&mut self, proto: Proto, rows: &[Row]) {
        for r in rows {
            let p = self.ports.entry((proto, r.port)).or_insert(Port {
                ip: Ipv4Addr::LOCALHOST,
                loopback4: false,
                loopback6: false,
                v4: false,
                v6: false,
            });
            match r.v4 {
                Some(ip) => {
                    // The first IPv4 row gives the address, as it did when one port was
                    // looked up at a time.
                    if !p.v4 {
                        p.ip = if ip.is_unspecified() { Ipv4Addr::LOCALHOST } else { ip };
                    }
                    p.v4 = true;
                    p.loopback4 |= r.loopback;
                }
                None => {
                    p.v6 = true;
                    p.loopback6 |= r.loopback;
                }
            }
        }
    }

    /// This process's TCP listeners and UDP sockets (IPv4 rows first).
    fn read() -> Owned {
        let pid = std::process::id();
        let (Some(tcp), Some(udp)) = (crate::sys::sockets_of(pid, true), crate::sys::sockets_of(pid, false)) else {
            return Owned { ports: HashMap::new(), all: true };
        };
        let mut owned = Owned::default();
        owned.add(Proto::Tcp, &tcp.into_iter().map(Row::of).collect::<Vec<_>>());
        owned.add(Proto::Udp, &udp.into_iter().map(Row::of).collect::<Vec<_>>());
        owned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dropping the last handle to a forwarder whose pump runs drops it (and the `Lan`
    /// it holds), and the pump thread ends.
    #[test]
    fn a_dropped_forwarder_stops_its_pump() {
        fn quiet(_: &str) {}
        let lan = Lan::start(crate::Options {
            game: format!("fwdtest:drop:{}", std::process::id()),
            identity: crate::Identity::ephemeral("a"),
            discovery_port: crate::test_ports::claim(1),
            extra_targets: Vec::new(),
            log: quiet,
            internet: None,
            public: None,
            lan_discovery: false,
            public_voice: None,
        })
        .expect("start");
        let f = Forwarder::new(lan.clone(), None, quiet);
        f.ensure_pump();
        let (weak, pump) = (Arc::downgrade(&f), f.pump.clone());
        // The pump is running: it has taken the forwarder in hand at least once.
        std::thread::sleep(Duration::from_millis(50));
        assert!(!pump.done.load(Ordering::Acquire));
        drop(f);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pump.done.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "the pump still runs");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(pump.stop.load(Ordering::Acquire), "dropped, which tells the pump to stop");
        assert_eq!(weak.strong_count(), 0, "nothing holds the forwarder");
        lan.shutdown();
    }

    /// The socket tables name this process's own listeners and sockets.
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn reads_this_processes_open_ports() {
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let (t, u) = (tcp.local_addr().unwrap().port(), udp.local_addr().unwrap().port());
        let owned = Owned::read();
        let local = Some(Port { ip: Ipv4Addr::LOCALHOST, loopback4: true, loopback6: false, v4: true, v6: false });
        assert_eq!(owned.get(Proto::Tcp, t), local);
        assert_eq!(owned.get(Proto::Udp, u), local);
        assert!(!owned.all);
    }

    /// Which of the game's ports are kept to this PC: a socket on loopback alone makes
    /// it so for its address family; "any", a LAN address and IPv6 "any" don't, and
    /// `[::1]` is only tried when the game has an IPv6 socket there. (Which rows are this
    /// process's is `sys`'s: its tests.)
    #[test]
    fn loopback_only_ports_are_told_from_the_tables() {
        let any6 = Ipv6Addr::UNSPECIFIED;
        let mapped = Ipv4Addr::LOCALHOST.to_ipv6_mapped();
        let v4 = |ip: [u8; 4], port: u16| SocketAddr::from((ip, port));
        let v6 = |ip: Ipv6Addr, port: u16| SocketAddr::from((ip, port));
        for tcp in [true, false] {
            let proto = if tcp { Proto::Tcp } else { Proto::Udp };
            let socks = [
                v4([127, 0, 0, 1], 1000),
                v4([0, 0, 0, 0], 1001),
                v4([10, 1, 2, 3], 1002),
                v4([0, 0, 0, 0], 1005),
                v4([127, 0, 0, 1], 1005),
                v4([127, 9, 9, 9], 1007),
                v6(Ipv6Addr::LOCALHOST, 1001),
                v6(any6, 1004),
                v6(mapped, 1006),
                v6(any6, 1002),
            ];
            let mut owned = Owned::default();
            owned.add(proto, &socks.map(Row::of));
            let get = |port| owned.get(proto, port);
            let lo = Ipv4Addr::LOCALHOST;
            let (v4lo, v6lo) = (IpAddr::V4(lo), IpAddr::V6(Ipv6Addr::LOCALHOST));
            let port = |ip, loopback4, loopback6, v4, v6| Some(Port { ip, loopback4, loopback6, v4, v6 });
            assert_eq!(get(1000), port(lo, true, false, true, false));
            assert_eq!(get(1001), port(lo, false, true, true, true), "IPv4 any, IPv6 loopback");
            assert_eq!(get(1002), port(Ipv4Addr::new(10, 1, 2, 3), false, false, true, true));
            assert_eq!(get(1003), None, "not the game's");
            assert_eq!(get(1004), port(lo, false, false, false, true));
            assert_eq!(get(1005), port(lo, true, false, true, false), "loopback beside any");
            assert_eq!(get(1006).map(|p| p.kept_local()), Some(true), "IPv4-mapped loopback");
            assert_eq!(get(1007).map(|p| p.kept_local()), Some(true), "all of 127/8");
            let p = |n| get(n).unwrap();
            assert!(p(1000).kept_local() && !p(1001).kept_local() && !p(1002).kept_local() && !p(1004).kept_local());
            // A friend gets everything the game has; a stranger only what isn't kept.
            assert_eq!(p(1000).connect_to(false), vec![v4lo], "no [::1] the game doesn't have");
            assert_eq!(p(1000).connect_to(true), Vec::<IpAddr>::new());
            assert_eq!(p(1001).connect_to(false), vec![v4lo, v6lo]);
            assert_eq!(p(1001).connect_to(true), vec![v4lo], "not on to the IPv6 loopback socket");
            assert_eq!(p(1004).connect_to(true), vec![v6lo]);
            assert_eq!(p(1005).connect_to(true), Vec::<IpAddr>::new());
        }
    }
}
