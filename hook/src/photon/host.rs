//! Runs [`Server`] on real UDP sockets: one socket per server role and port set,
//! a reader thread per socket and one thread that owns the server state.
//!
//! Luxon Server (BSD-3-Clause, see LICENSE-Luxon) runs each server on its own UDP
//! port with ENet; this keeps the same ports (5058 name, 5055 master, 5056 game, and
//! the 27000-27002 "alternative" set) in one process. It also echoes the 13-byte
//! region pings the client's `PingMono` sends to each region's master port.
//!
//! Photon's TCP transport (`ConnectionProtocol.Tcp`; name server 4533, master 4530,
//! game 4531) is served by turning each TCP connection into an ENet client of the
//! server's own, inside the server thread: frames from the client go out on that peer,
//! and what the server delivers to it goes back as frames. The server sees one more
//! ENet connection (from the TCP client's address, on socket index 6-8) and needs no
//! TCP code. Framing, as Photon's client puts it on the wire: the client sends
//! `0xFB, length (4, big-endian, header included), channel, delivery mode` then the
//! message, and pings as 5 bytes `0xF0, its time`; the server answers pings with 9
//! bytes `0xF0, server time, the client's time` and frames messages the same way.
//!
//! Photon's WebSocket transport (name server 9093 and 80, master 9090, game 9091) rides
//! the same adapters: each WebSocket message is a Photon message; ws.rs does the
//! handshake, makes the Init a WebSocket client doesn't send, and answers its pings.

use super::enet::{self, Peer, State};
use super::server::{is_region_ping, is_tcp, is_ws, socket_role, Config, Out, Server, PORTS, TCP_BASE, TCP_PORTS, WS_BASE, WS_NAME_PORT_OLD, WS_PORTS};
use super::ws;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub struct Host {
    stop: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
    /// The ports bound, by socket index (0 where that socket couldn't be bound).
    pub ports: [[u16; 3]; 2],
    /// The TCP ports listened on, by role (0 where none).
    pub tcp_ports: [u16; 3],
}

/// What the reader threads hand the server thread.
enum Input {
    Udp(usize, SocketAddr, Vec<u8>),
    TcpOpen(usize, SocketAddr, TcpStream),
    /// A WebSocket client after its handshake; true when PeerParty made its Init (the
    /// server's Init response is then not for the client).
    WsOpen(usize, SocketAddr, TcpStream, bool),
    /// Bytes to write to a client as they are (WebSocket frames: ping answers, pongs).
    Write(usize, SocketAddr, Vec<u8>),
    /// A framed message: channel, reliable, the message.
    TcpFrame(usize, SocketAddr, u8, bool, Vec<u8>),
    /// A ping with the client's time.
    TcpPing(usize, SocketAddr, [u8; 4]),
    TcpClosed(usize, SocketAddr),
}

/// A TCP client's ENet stand-in.
struct Adapter {
    peer: Peer,
    stream: TcpStream,
    /// Frames that came before the peer was connected.
    queued: Vec<(u8, bool, Vec<u8>)>,
    /// A WebSocket client: messages go out as WebSocket frames.
    ws: bool,
    /// The server's Init response is dropped (the client sent no Init).
    drop_init_response: bool,
}

impl Adapter {
    fn new(peer: Peer, stream: TcpStream, ws: bool, drop_init_response: bool) -> Adapter {
        Adapter { peer, stream, queued: Vec::new(), ws, drop_init_response }
    }

    fn write(&mut self, channel: u8, reliable: bool, msg: &[u8]) {
        if self.ws {
            if self.drop_init_response && msg.len() >= 2 && msg[0] == 0xF3 && msg[1] & 0x7F == 1 {
                self.drop_init_response = false;
                return;
            }
            let _ = self.stream.write_all(&ws::frame(ws::OP_BINARY, msg));
            return;
        }
        let mut f = Vec::with_capacity(msg.len() + 7);
        f.push(0xFB);
        f.extend_from_slice(&((msg.len() + 7) as u32).to_be_bytes());
        f.push(channel);
        f.push(u8::from(reliable));
        f.extend_from_slice(msg);
        let _ = self.stream.write_all(&f);
    }
}

/// Reads one TCP client's frames and pings until it closes.
fn read_tcp(sock: usize, from: SocketAddr, mut s: TcpStream, tx: mpsc::Sender<Input>) {
    let mut first = [0u8; 1];
    loop {
        if s.read_exact(&mut first).is_err() {
            break;
        }
        match first[0] {
            0xF0 => {
                let mut t = [0u8; 4];
                if s.read_exact(&mut t).is_err() || tx.send(Input::TcpPing(sock, from, t)).is_err() {
                    break;
                }
            }
            0xFB => {
                let mut h = [0u8; 6];
                if s.read_exact(&mut h).is_err() {
                    break;
                }
                let len = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize;
                if !(7..=1 << 20).contains(&len) {
                    break;
                }
                let mut msg = vec![0u8; len - 7];
                if s.read_exact(&mut msg).is_err() {
                    break;
                }
                // Delivery modes 1 and 3 are reliable.
                if tx.send(Input::TcpFrame(sock, from, h[4], h[5] & 1 == 1, msg)).is_err() {
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = tx.send(Input::TcpClosed(sock, from));
}

/// Does a WebSocket client's handshake, then reads its messages until it closes.
fn read_ws(sock: usize, from: SocketAddr, mut s: TcpStream, tx: mpsc::Sender<Input>, start: Instant) {
    let Ok(hs) = ws::accept(&mut s) else { return };
    let Ok(w) = s.try_clone() else { return };
    if tx.send(Input::WsOpen(sock, from, w, hs.init.is_some())).is_err() {
        return;
    }
    if let Some(init) = hs.init {
        if tx.send(Input::TcpFrame(sock, from, 0, true, init)).is_err() {
            return;
        }
    }
    let mut msg: Vec<u8> = Vec::new();
    while let Ok((fin, op, payload)) = ws::read_frame(&mut s) {
        match op {
            ws::OP_CLOSE => {
                let _ = tx.send(Input::Write(sock, from, ws::frame(ws::OP_CLOSE, &payload[..payload.len().min(2)])));
                break;
            }
            ws::OP_PING => {
                let _ = tx.send(Input::Write(sock, from, ws::frame(ws::OP_PONG, &payload)));
                continue;
            }
            ws::OP_PONG => continue,
            _ => {}
        }
        msg.extend_from_slice(&payload);
        if msg.len() > 1 << 20 {
            break;
        }
        if !fin {
            continue;
        }
        let whole = std::mem::take(&mut msg);
        let out = match ws::sort(whole, hs.proto, start.elapsed().as_millis() as i32) {
            ws::Message::ToServer(m) => Input::TcpFrame(sock, from, 0, true, m),
            ws::Message::Ping(answer) => Input::Write(sock, from, ws::frame(ws::OP_BINARY, &answer)),
        };
        if tx.send(out).is_err() {
            break;
        }
    }
    let _ = tx.send(Input::TcpClosed(sock, from));
}

/// Hands the server's datagrams to UDP sockets or to TCP adapters, and the adapters'
/// answers back to the server, until nothing is left to move.
fn route(server: &mut Server, adapters: &mut HashMap<(usize, SocketAddr), Adapter>, senders: &[Option<UdpSocket>], outs: Vec<Out>, now: i64) {
    let mut work: VecDeque<Out> = outs.into();
    while let Some(o) = work.pop_front() {
        if !is_tcp(o.sock) {
            if let Some(Some(s)) = senders.get(o.sock) {
                let _ = s.send_to(&o.data, o.to);
            }
            continue;
        }
        let Some(a) = adapters.get_mut(&(o.sock, o.to)) else { continue };
        if let Ok((hdr, cmds)) = enet::parse(&o.data) {
            a.peer.handle(&hdr, cmds, now);
        }
        while let Some(d) = a.peer.dispatch() {
            a.write(d.channel, d.reliable, &d.payload);
        }
        if a.peer.state == State::Connected {
            for (ch, rel, m) in std::mem::take(&mut a.queued) {
                a.peer.send(ch, &m, rel, false);
            }
        }
        for dg in a.peer.flush(now) {
            work.extend(server.on_datagram(o.sock, o.to, &dg, now));
        }
    }
}

/// Datagrams an adapter produced, into the server and on to wherever its answers go.
fn feed(server: &mut Server, adapters: &mut HashMap<(usize, SocketAddr), Adapter>, senders: &[Option<UdpSocket>], key: (usize, SocketAddr), dgs: Vec<Vec<u8>>, now: i64) {
    for dg in dgs {
        let outs = server.on_datagram(key.0, key.1, &dg, now);
        route(server, adapters, senders, outs, now);
    }
}

pub struct Options {
    pub bind: IpAddr,
    /// Ports to bind, by socket index; 0 picks a free port (tests).
    pub ports: [[u16; 3]; 2],
    /// TCP ports to listen on, by role; 0 picks a free port (tests).
    pub tcp_ports: [u16; 3],
    /// WebSocket ports to listen on, by role; 0 picks a free port (tests).
    pub ws_ports: [u16; 3],
    pub advertise: Box<dyn Fn(std::net::SocketAddr) -> String + Send>,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
}

impl Options {
    pub fn standard(bind: IpAddr, advertise: Box<dyn Fn(std::net::SocketAddr) -> String + Send>, log: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        Options { bind, ports: PORTS, tcp_ports: TCP_PORTS, ws_ports: WS_PORTS, advertise, log }
    }
}

impl Host {
    /// Binds the sockets and starts serving. The primary port set must bind; the
    /// alternative set is best effort.
    pub fn start(opt: Options) -> io::Result<Host> {
        let mut sockets: Vec<Option<UdpSocket>> = Vec::new();
        let mut ports = [[0u16; 3]; 2];
        for i in 0..6 {
            let want = opt.ports[i / 3][i % 3];
            match UdpSocket::bind(SocketAddr::new(opt.bind, want)) {
                Ok(s) => {
                    ports[i / 3][i % 3] = s.local_addr()?.port();
                    sockets.push(Some(s));
                }
                Err(e) if i < 3 => {
                    return Err(io::Error::new(e.kind(), format!("UDP port {want}: {e}")));
                }
                Err(e) => {
                    (opt.log)(&format!("photon: alternative UDP port {want} unavailable ({e}); skipping it"));
                    sockets.push(None);
                }
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(AtomicUsize::new(0));
        // The server's clock, shared with the WebSocket readers that answer pings.
        let start = Instant::now();
        let (tx, rx) = mpsc::channel::<Input>();
        let mut senders: Vec<Option<UdpSocket>> = Vec::new();
        let mut threads = Vec::new();
        for (i, s) in sockets.into_iter().enumerate() {
            let Some(s) = s else {
                senders.push(None);
                continue;
            };
            s.set_read_timeout(Some(Duration::from_millis(200)))?;
            senders.push(Some(s.try_clone()?));
            let (tx, stop) = (tx.clone(), stop.clone());
            threads.push(thread::Builder::new().name(format!("pp-photon-rx{i}")).spawn(move || {
                let mut buf = vec![0u8; 65536];
                while !stop.load(Ordering::Relaxed) {
                    match s.recv_from(&mut buf) {
                        Ok((n, from)) => {
                            let data = &buf[..n];
                            if is_region_ping(data) {
                                let _ = s.send_to(data, from);
                            } else if tx.send(Input::Udp(i, from, data.to_vec())).is_err() {
                                break;
                            }
                        }
                        // Timeouts, and Windows' WSAECONNRESET after an ICMP
                        // port-unreachable, are both just "nothing to read".
                        Err(_) => {}
                    }
                }
            })?);
        }
        // TCP: best effort (a port taken by something else only loses that transport).
        let mut tcp_ports = [0u16; 3];
        for (r, want) in opt.tcp_ports.into_iter().enumerate() {
            let l = match TcpListener::bind(SocketAddr::new(opt.bind, want)) {
                Ok(l) => l,
                Err(e) => {
                    (opt.log)(&format!("photon: TCP port {want} unavailable ({e}); Photon's TCP transport is off for that server"));
                    continue;
                }
            };
            tcp_ports[r] = l.local_addr()?.port();
            l.set_nonblocking(true)?;
            let (tx, stop) = (tx.clone(), stop.clone());
            threads.push(thread::Builder::new().name(format!("pp-photon-tcp{r}")).spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match l.accept() {
                        Ok((s, from)) => {
                            let _ = s.set_nonblocking(false);
                            let _ = s.set_nodelay(true);
                            let Ok(w) = s.try_clone() else { continue };
                            if tx.send(Input::TcpOpen(TCP_BASE + r, from, w)).is_err() {
                                break;
                            }
                            let tx = tx.clone();
                            let _ = thread::Builder::new().name("pp-photon-tcp-rx".into()).spawn(move || read_tcp(TCP_BASE + r, from, s, tx));
                        }
                        Err(_) => thread::sleep(Duration::from_millis(50)),
                    }
                }
            })?);
        }
        // WebSocket: best effort too. Older Realtime clients dial the name server on 80.
        let mut ws_ports = [0u16; 3];
        let mut ws_listen: Vec<(usize, u16)> = opt.ws_ports.iter().copied().enumerate().collect();
        if opt.ws_ports == WS_PORTS {
            ws_listen.push((0, WS_NAME_PORT_OLD));
        }
        for (r, want) in ws_listen {
            let l = match TcpListener::bind(SocketAddr::new(opt.bind, want)) {
                Ok(l) => l,
                Err(e) => {
                    (opt.log)(&format!("photon: WebSocket port {want} unavailable ({e})"));
                    continue;
                }
            };
            if ws_ports[r] == 0 {
                ws_ports[r] = l.local_addr()?.port();
            }
            l.set_nonblocking(true)?;
            let (tx, stop) = (tx.clone(), stop.clone());
            threads.push(thread::Builder::new().name(format!("pp-photon-ws{r}")).spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match l.accept() {
                        Ok((s, from)) => {
                            let _ = s.set_nonblocking(false);
                            let _ = s.set_nodelay(true);
                            let tx = tx.clone();
                            let _ = thread::Builder::new().name("pp-photon-ws-rx".into()).spawn(move || read_ws(WS_BASE + r, from, s, tx, start));
                        }
                        Err(_) => thread::sleep(Duration::from_millis(50)),
                    }
                }
            })?);
        }
        drop(tx);
        let log = opt.log.clone();
        let mut server = Server::new(Config {
            ports,
            tcp_ports,
            ws_ports,
            advertise: opt.advertise,
            log: Box::new(move |s| log(s)),
        });
        let (stop2, conns2, log) = (stop.clone(), connections.clone(), opt.log.clone());
        threads.push(thread::Builder::new().name("pp-photon".into()).spawn(move || {
            let now = || start.elapsed().as_millis() as i64;
            let mut adapters: HashMap<(usize, SocketAddr), Adapter> = HashMap::new();
            let mut next = 0i64;
            while !stop2.load(Ordering::Relaxed) {
                let wait = (next - now()).clamp(1, 200) as u64;
                let t = now();
                match rx.recv_timeout(Duration::from_millis(wait)) {
                    Ok(Input::Udp(sock, from, data)) => {
                        let outs = server.on_datagram(sock, from, &data, t);
                        route(&mut server, &mut adapters, &senders, outs, t);
                    }
                    Ok(Input::TcpOpen(sock, from, stream)) => {
                        let challenge = u32::from_le_bytes(super::crypto::random_bytes(4).try_into().unwrap());
                        let mut a = Adapter::new(Peer::connect(challenge, 32, t), stream, false, false);
                        let dgs = a.peer.flush(t);
                        adapters.insert((sock, from), a);
                        feed(&mut server, &mut adapters, &senders, (sock, from), dgs, t);
                    }
                    Ok(Input::WsOpen(sock, from, stream, made_init)) => {
                        let challenge = u32::from_le_bytes(super::crypto::random_bytes(4).try_into().unwrap());
                        let mut a = Adapter::new(Peer::connect(challenge, 32, t), stream, is_ws(sock), made_init);
                        let dgs = a.peer.flush(t);
                        adapters.insert((sock, from), a);
                        feed(&mut server, &mut adapters, &senders, (sock, from), dgs, t);
                    }
                    Ok(Input::Write(sock, from, bytes)) => {
                        if let Some(a) = adapters.get_mut(&(sock, from)) {
                            let _ = a.stream.write_all(&bytes);
                        }
                    }
                    Ok(Input::TcpFrame(sock, from, channel, reliable, msg)) => {
                        let dgs = match adapters.get_mut(&(sock, from)) {
                            Some(a) => {
                                if a.peer.state == State::Connected {
                                    a.peer.send(channel, &msg, reliable, false);
                                } else {
                                    a.queued.push((channel, reliable, msg));
                                }
                                a.peer.flush(t)
                            }
                            None => Vec::new(),
                        };
                        feed(&mut server, &mut adapters, &senders, (sock, from), dgs, t);
                    }
                    Ok(Input::TcpPing(sock, from, client_time)) => {
                        if let Some(a) = adapters.get_mut(&(sock, from)) {
                            let mut r = [0u8; 9];
                            r[0] = 0xF0;
                            r[1..5].copy_from_slice(&(t as i32).to_be_bytes());
                            r[5..9].copy_from_slice(&client_time);
                            let _ = a.stream.write_all(&r);
                        }
                    }
                    Ok(Input::TcpClosed(sock, from)) => {
                        if let Some(mut a) = adapters.remove(&(sock, from)) {
                            let _ = a.stream.shutdown(std::net::Shutdown::Both);
                            let dgs = a.peer.disconnect(t);
                            feed(&mut server, &mut adapters, &senders, (sock, from), dgs, t);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                let t = now();
                if t >= next {
                    let outs = server.tick(t);
                    route(&mut server, &mut adapters, &senders, outs, t);
                    // The adapters' own acks and pings.
                    let keys: Vec<(usize, SocketAddr)> = adapters.keys().copied().collect();
                    for k in keys {
                        let dgs = adapters.get_mut(&k).map(|a| a.peer.flush(t)).unwrap_or_default();
                        feed(&mut server, &mut adapters, &senders, k, dgs, t);
                    }
                    next = server.next_tick(t);
                }
                conns2.store(server.connection_count(), Ordering::Relaxed);
            }
            log("photon: servers stopped");
        })?);
        let mut roles: Vec<String> = (0..6)
            .filter(|i| ports[i / 3][i % 3] != 0)
            .map(|i| format!("{} {}", socket_role(i).0.label(), ports[i / 3][i % 3]))
            .collect();
        roles.extend((0..3).filter(|r| tcp_ports[*r] != 0).map(|r| format!("{} TCP {}", socket_role(r).0.label(), tcp_ports[r])));
        roles.extend((0..3).filter(|r| ws_ports[*r] != 0).map(|r| format!("{} WebSocket {}", socket_role(r).0.label(), ws_ports[r])));
        (opt.log)(&format!("photon: serving on {} ({})", opt.bind, roles.join(", ")));
        Ok(Host { stop, threads, ports, tcp_ports })
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
