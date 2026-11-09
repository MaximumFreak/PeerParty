//! Games where a player types the host's IP address (no lobby SDK): the forwarder carries
//! their plain UDP and TCP traffic over the mesh, so they work with internet peers too.
//!
//! Each peer has a stand-in address (`inet::redact::stand_in`, in 198.18.0.0/15, never
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
//! system's socket tables), so a peer can't reach anything else on that PC.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use crate::inet::redact::stand_in;
use crate::{Event, Lan, Packet, PeerId, SendResult};

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
const OWNER_CACHE: Duration = Duration::from_secs(5);

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
    last: Mutex<Instant>,
    dead: AtomicBool,
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
    owners: HashMap<(Proto, u16), (Option<Ipv4Addr>, Instant)>,
    peers: Vec<(Ipv4Addr, PeerId)>,
    peers_at: Option<Instant>,
}

/// One per process: carries typed-IP traffic for the game over `lan`.
pub struct Forwarder {
    lan: Lan,
    st: Mutex<St>,
    me: Weak<Forwarder>,
    pumping: AtomicBool,
    /// The hook ticks the engine and hands us its events; `None`: we do it ourselves.
    tick: Option<fn()>,
    next: AtomicU64,
    log: fn(&str),
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
            tick,
            next: AtomicU64::new(1),
            log,
        })
    }

    fn arc(&self) -> Option<Arc<Forwarder>> {
        self.me.upgrade()
    }

    /// The peer behind a stand-in address.
    pub fn peer_at(&self, ip: Ipv4Addr) -> Option<PeerId> {
        if !is_stand_in(ip) {
            return None;
        }
        let mut st = self.st.lock().unwrap();
        let find = |st: &St| st.peers.iter().find(|(a, _)| *a == ip).map(|(_, p)| *p);
        if let Some(p) = find(&st) {
            return Some(p);
        }
        if st.peers_at.is_some_and(|t| t.elapsed() < Duration::from_millis(200)) {
            return None;
        }
        st.peers = self.lan.peers().into_iter().map(|(id, _)| (stand_in(&id.0), id)).collect();
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
        *a.last.lock().unwrap() = Instant::now();
        Some(a.addr)
    }

    /// What our game should see as the sender of a datagram from `from`: the peer's
    /// stand-in and port, when `from` is one of our aliases.
    pub fn udp_from(&self, from: SocketAddrV4) -> Option<SocketAddrV4> {
        let st = self.st.lock().unwrap();
        let a = st.by_addr.get(&from)?;
        Some(SocketAddrV4::new(stand_in(&a.peer.0), a.remote))
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
        thread::Builder::new()
            .name("peerparty-ipfwd-accept".into())
            .spawn(move || {
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
                if let Some(t) = self.find_tcp(*peer, socket) {
                    t.kill();
                }
            }
            Event::PeerLeft(peer) => {
                let (tcps, aliases): (Vec<_>, Vec<_>) = {
                    let st = self.st.lock().unwrap();
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
        if let SocketAddr::V4(v4) = game_sees
            && let Some(from) = self.udp_from(v4)
        {
            return self.peer_at(*from.ip());
        }
        let st = self.st.lock().unwrap();
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
        let count = self.st.lock().unwrap().tcp.keys().filter(|(p, _)| *p == peer).count();
        let target = self.owner(Proto::Tcp, port);
        let (Some(me), Some(target), true) = (self.arc(), target, count < MAX_TCP_PER_PEER) else {
            (self.log)(&format!("ipfwd: refused a TCP stream to port {port} (the game isn't listening there)"));
            self.lan.close(peer, Some(socket));
            return;
        };
        let socket = socket.to_string();
        let _ = thread::Builder::new().name("peerparty-ipfwd-connect".into()).spawn(move || {
            let to = |ip: IpAddr| SocketAddr::new(ip, port);
            let first = to(IpAddr::V4(target));
            let stream = TcpStream::connect_timeout(&first, Duration::from_secs(3))
                .or_else(|_| TcpStream::connect_timeout(&to(IpAddr::V6(Ipv6Addr::LOCALHOST)), Duration::from_secs(3)));
            match stream {
                Ok(s) => me.run_tcp(peer, socket, s, false),
                Err(e) => {
                    (me.log)(&format!("ipfwd: couldn't reach the game on port {port}: {e}"));
                    me.lan.close(peer, Some(&socket));
                }
            }
        });
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
        self.st.lock().unwrap().tcp.remove(&(peer, socket));
    }

    fn alias(&self, peer: PeerId, remote: u16, local: u16) -> Option<Arc<Alias>> {
        let key = (peer, remote, local);
        if let Some(a) = self.st.lock().unwrap().udp.get(&key)
            && !a.dead.load(Ordering::Relaxed)
        {
            return Some(a.clone());
        }
        let ip = self.owner(Proto::Udp, local).unwrap_or(Ipv4Addr::LOCALHOST);
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
        sock.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
        let a = Arc::new(Alias {
            sock,
            addr,
            peer,
            remote,
            local,
            target: SocketAddrV4::new(ip, local),
            last: Mutex::new(Instant::now()),
            dead: AtomicBool::new(false),
        });
        st.udp.insert(key, a.clone());
        st.by_addr.insert(addr, a.clone());
        drop(st);
        let a2 = a.clone();
        let spawned = thread::Builder::new().name("peerparty-ipfwd-udp".into()).spawn(move || {
            let mut buf = vec![0u8; 65536 + 4];
            loop {
                if a2.dead.load(Ordering::Relaxed) || a2.last.lock().unwrap().elapsed() > UDP_IDLE {
                    break;
                }
                let Ok((n, from)) = a2.sock.recv_from(&mut buf[4..]) else { continue };
                // Only our game's socket may use the alias.
                let ours = from.ip() == IpAddr::V4(*a2.target.ip()) || from.ip().is_loopback();
                if from.port() != a2.local || !ours {
                    continue;
                }
                *a2.last.lock().unwrap() = Instant::now();
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

    /// Where our own game listens on `port` (`None`: it doesn't).
    fn owner(&self, proto: Proto, port: u16) -> Option<Ipv4Addr> {
        if let Some((ip, at)) = self.st.lock().unwrap().owners.get(&(proto, port))
            && at.elapsed() < OWNER_CACHE
        {
            return *ip;
        }
        let ip = owned_by_me(proto == Proto::Tcp, port);
        self.st.lock().unwrap().owners.insert((proto, port), (ip, Instant::now()));
        ip
    }

    fn packet(&self, p: Packet) {
        if p.socket == UDP_SOCKET {
            if p.channel != DATA || p.data.len() < 4 {
                return;
            }
            let dst = u16::from_be_bytes([p.data[0], p.data[1]]);
            let src = u16::from_be_bytes([p.data[2], p.data[3]]);
            if self.owner(Proto::Udp, dst).is_none() {
                return;
            }
            if let Some(a) = self.alias(p.peer, src, dst) {
                *a.last.lock().unwrap() = Instant::now();
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
        let Some(me) = self.arc() else { return };
        let _ = thread::Builder::new().name("peerparty-ipfwd".into()).spawn(move || {
            let mut last_tick = Instant::now() - Duration::from_secs(1);
            loop {
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
                // Packets first: they arrived before any close among the events.
                let mut busy = false;
                while let Some(p) = me.lan.receive_where(None, |s| s.starts_with(PREFIX)) {
                    busy = true;
                    me.packet(p);
                }
                for ev in &events {
                    me.event(ev);
                }
                if !busy {
                    thread::sleep(Duration::from_millis(1));
                }
            }
        });
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

/// Where this process listens on `port` (its bound IPv4 address, loopback for "any" or
/// IPv6), or `None` when it has no such socket.
#[cfg(windows)]
fn owned_by_me(tcp: bool, port: u16) -> Option<Ipv4Addr> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, TCP_TABLE_OWNER_PID_LISTENER, UDP_TABLE_OWNER_PID,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    let pid = std::process::id();
    let table = |v6: bool| -> Vec<u32> {
        let af = if v6 { AF_INET6 } else { AF_INET } as u32;
        let mut size = 0u32;
        for _ in 0..4 {
            let mut buf = vec![0u32; size as usize / 4 + 1];
            let rc = unsafe {
                if tcp {
                    GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut size, 0, af, TCP_TABLE_OWNER_PID_LISTENER, 0)
                } else {
                    GetExtendedUdpTable(buf.as_mut_ptr().cast(), &mut size, 0, af, UDP_TABLE_OWNER_PID, 0)
                }
            };
            if rc == 0 {
                return buf;
            }
        }
        Vec::new()
    };
    // Row layouts (u32s): MIB_TCPROW_OWNER_PID starts with its state; the UDP row with
    // the address.
    let (width, addr_at, port_at, pid_at) = if tcp { (6, 1, 2, 5) } else { (3, 0, 1, 2) };
    let v4 = table(false);
    let rows = v4.first().copied().unwrap_or(0) as usize;
    for i in 0..rows {
        let Some(r) = v4.get(1 + i * width..1 + (i + 1) * width) else { break };
        if r[pid_at] == pid && u16::from_be(r[port_at] as u16) == port {
            let ip = Ipv4Addr::from(u32::from_be(r[addr_at]));
            return Some(if ip.is_unspecified() { Ipv4Addr::LOCALHOST } else { ip });
        }
    }
    let (width, port_at, pid_at) = if tcp { (14, 5, 13) } else { (7, 5, 6) };
    let v6 = table(true);
    let rows = v6.first().copied().unwrap_or(0) as usize;
    for i in 0..rows {
        let Some(r) = v6.get(1 + i * width..1 + (i + 1) * width) else { break };
        if r[pid_at] == pid && u16::from_be(r[port_at] as u16) == port {
            return Some(Ipv4Addr::LOCALHOST);
        }
    }
    None
}

#[cfg(not(windows))]
fn owned_by_me(_tcp: bool, _port: u16) -> Option<Ipv4Addr> {
    Some(Ipv4Addr::LOCALHOST)
}
