//! The typed-IP forwarder: two engines in one process, a "game" on each side talking
//! plain TCP and UDP to the other's stand-in address.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use peerparty_core::fwd::{Forwarder, is_stand_in};
use peerparty_core::*;

fn quiet(_: &str) {}

fn start(game: &str, port: u16, name: &str) -> Lan {
    Lan::start(Options {
        game: game.into(),
        identity: Identity::ephemeral(name),
        discovery_port: port,
        extra_targets: Vec::new(),
        log: quiet,
        internet: None,
        public: None,
        lan_discovery: true,
    })
    .expect("start")
}

fn meet(a: &Lan, b: &Lan) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut ma, mut mb) = (false, false);
    while !(ma && mb) {
        a.tick();
        b.tick();
        ma |= a.poll().contains(&Event::PeerJoined(b.me()));
        mb |= b.poll().contains(&Event::PeerJoined(a.me()));
        assert!(Instant::now() < deadline, "the engines didn't meet");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn v4(a: SocketAddr) -> SocketAddrV4 {
    match a {
        SocketAddr::V4(a) => a,
        _ => panic!("not v4"),
    }
}

#[test]
fn typed_ip_tcp_and_udp_cross_the_mesh() {
    let port = 47300 + (std::process::id() % 250) as u16;
    let game = format!("fwdtest:{}", std::process::id());
    let a = start(&game, port, "a");
    let b = start(&game, port, "b");
    meet(&a, &b);
    let fa = Forwarder::new(a.clone(), None, quiet);
    let fb = Forwarder::new(b.clone(), None, quiet);
    // Every game socket here is on loopback only, which public players don't get to
    // (tests/public.rs): LAN players do.
    fa.keep_loopback_private(true);
    fb.keep_loopback_private(true);
    assert_eq!(a.peer_trust(b.me()), Some(Trust::Lan));
    fa.ensure_pump();
    fb.ensure_pump();
    let (sa, sb) = (Forwarder::address_of(a.me()), Forwarder::address_of(b.me()));
    assert!(is_stand_in(sa) && is_stand_in(sb) && sa != sb);
    assert_eq!(fa.peer_at(sb), Some(b.me()));
    assert_eq!(fa.peer_at(Ipv4Addr::new(198, 18, 0, 0)).filter(|p| *p != b.me()), None);
    assert_eq!(fa.peer_at(Ipv4Addr::new(10, 0, 0, 1)), None);

    // TCP: B's game listens; A's game connects to B's stand-in. 3 MiB each way exceeds
    // the 1 MiB window, so flow control has to work.
    let server = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let sport = server.local_addr().unwrap().port();
    let echo = std::thread::spawn(move || {
        let (mut s, _) = server.accept().unwrap();
        let mut buf = vec![0u8; 65536];
        let mut total = 0usize;
        loop {
            let n = s.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
            s.write_all(&buf[..n]).unwrap();
        }
        total
    });
    let to = fa.tcp_dest(SocketAddrV4::new(sb, sport)).expect("a stand-in");
    assert!(to.ip().is_loopback());
    let mut c = TcpStream::connect(to).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let data: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i * 7 + i / 251) as u8).collect();
    let mut w = c.try_clone().unwrap();
    let sent = data.clone();
    let writer = std::thread::spawn(move || {
        w.write_all(&sent).unwrap();
        w.shutdown(std::net::Shutdown::Write).unwrap();
    });
    let mut back = Vec::new();
    c.read_to_end(&mut back).unwrap();
    writer.join().unwrap();
    assert_eq!(echo.join().unwrap(), data.len());
    assert!(back == data, "the echo came back different ({} of {} bytes)", back.len(), data.len());

    // Nobody listens on this port at B: the stream is refused and closes.
    let unused = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let dead_port = unused.local_addr().unwrap().port();
    drop(unused);
    let to = fa.tcp_dest(SocketAddrV4::new(sb, dead_port)).unwrap();
    let mut c = TcpStream::connect(to).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let mut buf = [0u8; 16];
    assert!(matches!(c.read(&mut buf), Ok(0) | Err(_)), "a refused stream stayed open");

    // B's engine listens in B's process too, but its listener isn't B's game: a peer the
    // forwarder connected there would arrive from loopback, like a player on B's own PC,
    // and be trusted as one. Refused like a port nobody listens on (the engine would
    // have sent its hello at once).
    assert!(peerparty_core::own::is_held(true, b.tcp_port()));
    let to = fa.tcp_dest(SocketAddrV4::new(sb, b.tcp_port())).unwrap();
    let mut c = TcpStream::connect(to).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    assert!(matches!(c.read(&mut buf), Ok(0) | Err(_)), "the forwarder reached the engine's own listener");
    assert_eq!(b.peers().len(), 1, "nobody new joined B through the forwarder");

    // UDP: A's game sends to B's stand-in; B's game sees A's stand-in and answers it.
    let ga = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let gb = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    ga.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    gb.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    let (pa, pb) = (ga.local_addr().unwrap().port(), gb.local_addr().unwrap().port());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buf = [0u8; 2048];
    let from = loop {
        let via = fa.udp_dest(SocketAddrV4::new(sb, pb), pa).unwrap();
        ga.send_to(b"ping", via).unwrap();
        if let Ok((n, from)) = gb.recv_from(&mut buf) {
            assert_eq!(&buf[..n], b"ping");
            break v4(from);
        }
        assert!(Instant::now() < deadline, "the datagram never arrived");
    };
    assert_eq!(fb.udp_from(from), Some(SocketAddrV4::new(sa, pa)));
    let reply_via = fb.udp_dest(SocketAddrV4::new(sa, pa), pb).unwrap();
    assert_eq!(reply_via, from, "replies should leave by the same alias");
    let deadline = Instant::now() + Duration::from_secs(10);
    let from = loop {
        gb.send_to(b"pong", reply_via).unwrap();
        if let Ok((n, from)) = ga.recv_from(&mut buf) {
            assert_eq!(&buf[..n], b"pong");
            break v4(from);
        }
        assert!(Instant::now() < deadline, "the reply never arrived");
    };
    assert_eq!(fa.udp_from(from), Some(SocketAddrV4::new(sb, pb)));

    // A datagram for a port B's game doesn't have is dropped, not delivered anywhere.
    assert!(fa.udp_from(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9)).is_none());
}
