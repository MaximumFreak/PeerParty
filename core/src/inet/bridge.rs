//! Bridging a session into our mesh: a loopback TCP connection the mesh takes as any
//! other (the lower id's side has its mesh dial a listener of ours, the other dials its
//! own mesh), whose first frame (the Hello) is rewritten on the way (`hello`), and a
//! loopback UDP relay for the mesh's datagrams.

use super::*;

/// The peer's side of a tunnel's mesh connection: its Hello must be from `expect`; its
/// UDP port becomes our relay's, and its name the cleaned-up one (for a public player,
/// the one its signed handshake gave).
pub(super) fn peer_hello(filter: &mut HelloRewriter, data: &[u8], expect: PeerId, relay: u16, name: Option<&str>) -> Result<Vec<u8>, String> {
    filter.edit(data, &mut |id, n, udp| {
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

pub(super) struct Bridge {
    pub(super) tcp: TcpStream,
    /// The far end of the mesh connection as our mesh sees it (`Host::tunnel_up`).
    pub(super) mesh: SocketAddr,
    pub(super) to_tcp: mpsc::Sender<Vec<u8>>,
    /// Bytes in `to_tcp` our mesh hasn't taken yet (capped at mesh::SEND_QUEUE_MAX).
    pub(super) to_tcp_bytes: Arc<AtomicUsize>,
    pub(super) relay: Arc<UdpSocket>,
    pub(super) relay_port: u16,
    pub(super) _relay_own: crate::own::Held,
    /// Our mesh's data socket port (0 until its Hello passes).
    pub(super) mesh_udp: Arc<AtomicU16>,
    pub(super) closed: Arc<AtomicBool>,
    /// Our mesh stopped writing (it finished with the connection, see FINISH_FOR).
    pub(super) eof: Arc<AtomicBool>,
}

impl Bridge {
    pub(super) fn shut(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = self.tcp.shutdown(std::net::Shutdown::Both);
    }
}

/// Connects a confirmed session to our mesh: the lower id's side has its mesh dial a
/// loopback listener, the other side dials its own mesh.
pub(super) fn start_bridge(sh: Arc<Shared>, addr: SocketAddr, epoch: u64, id: PeerId) {
    let _ = spawn(&sh.clone(), "peerparty-inet-bridge", move |sh| {
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
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline && !sh.stop.load(Ordering::Relaxed) => {
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

pub(super) fn attach(sh: &Arc<Shared>, addr: SocketAddr, epoch: u64, tcp: TcpStream) -> io::Result<()> {
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
        Tunnel { id: info.id, public: s.mode == Mode::Public, key: s.fp.clone(), name: info.name.clone() }
    };
    let id = tunnel.id;
    sh.host.tunnel_up(mesh, tunnel);
    {
        let mut ss = sh.sessions.lock().unwrap();
        let Some(s) = ss.get_mut(&addr).filter(|s| s.epoch == epoch) else {
            drop(ss);
            let _ = tcp.shutdown(std::net::Shutdown::Both);
            sh.host.tunnel_down(mesh, false);
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
        let (closed, tag) = (closed.clone(), tag.clone());
        spawn(sh, "peerparty-inet-w", move |sh| {
            let mut failed = false;
            for chunk in rx {
                let ok = writer.write_all(&chunk).is_ok();
                to_tcp_bytes.fetch_sub(chunk.len(), Ordering::Relaxed);
                if !ok {
                    failed = true;
                    break;
                }
            }
            closed.store(true, Ordering::Relaxed);
            // What ends our mesh's connection on this tunnel (and so the peer, or its
            // grace). Refused ("not connected") when the mesh's end closed first, as is
            // usual; but now and then on Windows also without that end hearing anything,
            // and the engine then cuts that connection from its own side
            // (`Host::tunnel_closed`).
            let shut = writer.shutdown(std::net::Shutdown::Both);
            sh.log(&format!(
                "{tag}: the tunnel's mesh connection is closed ({}){}",
                if failed { "our mesh stopped taking data" } else { "the session ended" },
                match &shut {
                    Ok(()) => String::new(),
                    Err(e) => format!("; our end was already shut ({})", e.kind()),
                }
            ));
            sh.host.tunnel_closed(mesh);
        })?;
    }
    {
        let (closed, mesh_udp, eof) = (closed.clone(), mesh_udp.clone(), eof.clone());
        spawn(sh, "peerparty-inet-r", move |sh| {
            let mut buf = vec![0u8; 16 * 1024];
            let mut hello = Some(Vec::new());
            // Public sessions: our Hello carries our public name, not the LAN one.
            let mut ours = (sh.mode() == Mode::Public).then(HelloRewriter::default);
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
                // Room in the session's stream: woken as acks come (`Shared::drained`),
                // and looked at again now and then regardless.
                let mut ss = sh.sessions.lock().unwrap();
                loop {
                    if closed.load(Ordering::Relaxed) || sh.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let Some(s) = ss.get_mut(&addr).filter(|s| s.epoch == epoch) else { return };
                    if s.stream.backlog() < stream::MAX_QUEUED {
                        s.stream.write(&out);
                        break;
                    }
                    ss = sh.drained.wait_timeout(ss, Duration::from_millis(50)).unwrap().0;
                }
            }
            closed.store(true, Ordering::Relaxed);
        })?;
    }
    {
        let closed = closed.clone();
        spawn(sh, "peerparty-inet-u", move |sh| {
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
