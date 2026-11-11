//! Internet play between two engines in one process: LAN discovery off, so the only way
//! they meet is the internet layer (a static address, or a DHT), over loopback. The
//! real-internet checks at the bottom are ignored by default (they need the network):
//! `cargo test -p peerparty_core --test internet -- --ignored --nocapture`, with
//! `PEERPARTY_LOOPBACK=0`.

use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use peerparty_core::inet::{self, RoomCode};
use peerparty_core::*;

fn noisy(s: &str) {
    if std::env::var_os("PEERPARTY_TEST_LOG").is_some() {
        eprintln!("{s}");
    }
}

fn base_port(offset: u16) -> u16 {
    40000 + (std::process::id() % 2000) as u16 * 4 + offset
}

fn local_cfg(room: &RoomCode, port: u16, peers: Vec<String>) -> inet::Config {
    let mut c = inet::Config::new(room.clone());
    c.port = port;
    c.peers = peers;
    c.dht = false;
    c.stun = Vec::new();
    c.node_cache = None;
    c
}

fn start(game: &str, name: &str, disc: u16, net: inet::Config) -> Lan {
    Lan::start(Options {
        game: game.into(),
        identity: Identity::ephemeral(name),
        discovery_port: disc,
        extra_targets: Vec::new(),
        log: noisy,
        internet: Some(net),
        public: None,
        lan_discovery: false,
    })
    .expect("start")
}

fn wait(lan: &Lan, seen: &mut Vec<Event>, secs: u64, want: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(i) = seen.iter().position(&want) {
            return seen.remove(i);
        }
        lan.tick();
        seen.extend(lan.poll());
        assert!(Instant::now() < deadline, "timed out; events so far: {seen:#?}; status {:#?}", lan.internet_status());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn receive(lan: &Lan, chan: u8, secs: u64) -> Packet {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(p) = lan.receive(Some(chan)) {
            return p;
        }
        lan.tick();
        lan.poll();
        assert!(Instant::now() < deadline, "no packet on channel {chan}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Opens channel "game" from b to a and returns once both sides have it.
fn open_channel(a: &Lan, ea: &mut Vec<Event>, b: &Lan, eb: &mut Vec<Event>) {
    assert_eq!(b.send(a.me(), "game", 1, b"hello", true, true, true), SendResult::Held);
    wait(a, ea, 10, |e| *e == Event::ChanRequest { peer: b.me(), socket: "game".into() });
    a.accept(b.me(), "game");
    wait(a, ea, 10, |e| matches!(e, Event::ChanOpen { .. }));
    wait(b, eb, 10, |e| matches!(e, Event::ChanOpen { .. }));
    let p = receive(a, 1, 10);
    assert_eq!((p.data.as_slice(), p.peer), (&b"hello"[..], b.me()));
}

#[test]
fn two_copies_meet_over_the_tunnel_lobby_and_talk() {
    let game = format!("test:inet:{}", std::process::id());
    let room = RoomCode::generate();
    let (pa, pb) = (base_port(0), base_port(1));
    let a = start(&game, "a", base_port(2), local_cfg(&room, pa, vec![format!("127.0.0.1:{pb}")]));
    let b = start(&game, "b", base_port(3), local_cfg(&room, pb, Vec::new()));
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    wait(&a, &mut ea, 20, |e| *e == Event::PeerJoined(b.me()));
    wait(&b, &mut eb, 20, |e| *e == Event::PeerJoined(a.me()));
    assert!(a.peer_is_internet(b.me()) || b.peer_is_internet(a.me()));
    let st = a.internet_status().unwrap();
    assert_eq!(st.local_port, pa);
    assert!(st.peers.iter().any(|p| p.connected && p.bridged && p.id == Some(b.me())), "{st:#?}");
    assert_eq!(st.room, room.fingerprint());

    // A lobby, found and joined across the tunnel.
    let mut spec = Lobby::new("eos.lobby");
    spec.bucket = "b1".into();
    let hosted = a.create_lobby(spec).unwrap();
    let mut q = Query::new("eos.lobby");
    q.filters.push(Filter::Bucket { op: Op::Eq, value: "b1".into() });
    let search = b.search(q);
    let Event::SearchDone { results, .. } =
        wait(&b, &mut eb, 10, |e| matches!(e, Event::SearchDone { search: s, .. } if *s == search))
    else {
        unreachable!()
    };
    assert_eq!(results.len(), 1);
    b.join(&results[0]).unwrap();
    assert_eq!(
        wait(&b, &mut eb, 10, |e| matches!(e, Event::JoinDone { .. })),
        Event::JoinDone { lobby: hosted.id.clone(), result: JoinResult::Ok }
    );

    // Reliable data, including a lot of it.
    open_channel(&a, &mut ea, &b, &mut eb);
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
    for chunk in big.chunks(60_000) {
        assert_eq!(b.send(a.me(), "game", 2, chunk, true, true, true), SendResult::Sent);
    }
    let mut got = Vec::new();
    while got.len() < big.len() {
        got.extend(receive(&a, 2, 20).data);
    }
    assert_eq!(got, big);

    // Unreliable data rides the tunnel's datagrams once the mesh's UDP probe answers.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !(a.udp_ready(b.me()) && b.udp_ready(a.me())) {
        assert!(Instant::now() < deadline, "the mesh's udp probe never answered through the tunnel");
        a.tick();
        b.tick();
        std::thread::sleep(Duration::from_millis(50));
    }
    for i in 0..5u8 {
        assert_eq!(a.send(b.me(), "game", 0, &[b'u', i], false, false, true), SendResult::Sent);
    }
    let mut n = 0;
    while n < 5 {
        let p = receive(&b, 0, 5);
        assert!(!p.ordered, "unreliable data came as a datagram");
        n += 1;
    }

    // The address games are told for an internet peer is a stand-in (198.18.0.0/15),
    // never its internet address, and stays the same.
    let ip = b.peer_ip(a.me()).unwrap();
    assert_eq!(ip, inet::redact::stand_in(&a.me().0));
    assert!(matches!(ip, std::net::IpAddr::V4(v) if v.octets()[0] == 198 && (v.octets()[1] & 0xfe) == 18), "{ip}");
    assert_eq!(b.peer_trust(a.me()), Some(Trust::Room));
    // Status and its peers carry no address but tags.
    let st = format!("{:?}", a.internet_status().unwrap());
    assert!(!st.contains("127.0.0.1"), "{st}");

    // A leaves: B hears quickly (the tunnel says goodbye).
    a.shutdown();
    wait(&b, &mut eb, 10, |e| *e == Event::PeerLeft(a.me()));
    b.shutdown();
}

fn on_ipv6(lan: &Lan, peer: PeerId) -> bool {
    lan.internet_status().is_some_and(|st| st.peers.iter().any(|p| p.connected && p.id == Some(peer) && p.ipv6))
}

/// The session starts over IPv4, then moves to the IPv6 address the peer sent (here
/// loopback, `allow_private`) and carries the mesh there.
#[test]
fn a_direct_ipv6_path_is_preferred() {
    let game = format!("test:inet-v6:{}", std::process::id());
    let room = RoomCode::generate();
    let (pa, pb) = (base_port(18), base_port(19));
    let mut ca = local_cfg(&room, pa, vec![format!("127.0.0.1:{pb}")]);
    let mut cb = local_cfg(&room, pb, Vec::new());
    ca.allow_private = true;
    cb.allow_private = true;
    let a = start(&game, "a", base_port(20), ca);
    let b = start(&game, "b", base_port(21), cb);
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    wait(&a, &mut ea, 20, |e| *e == Event::PeerJoined(b.me()));
    wait(&b, &mut eb, 20, |e| *e == Event::PeerJoined(a.me()));
    let deadline = Instant::now() + Duration::from_secs(15);
    while !(on_ipv6(&a, b.me()) && on_ipv6(&b, a.me())) {
        assert!(Instant::now() < deadline, "never moved to IPv6: {:#?} {:#?}", a.internet_status(), b.internet_status());
        a.tick();
        b.tick();
        std::thread::sleep(Duration::from_millis(50));
    }
    open_channel(&a, &mut ea, &b, &mut eb);
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 239) as u8).collect();
    for chunk in big.chunks(50_000) {
        assert_eq!(b.send(a.me(), "game", 2, chunk, true, true, true), SendResult::Sent);
    }
    let mut got = Vec::new();
    while got.len() < big.len() {
        got.extend(receive(&a, 2, 20).data);
    }
    assert_eq!(got, big);
    assert!(on_ipv6(&a, b.me()) && on_ipv6(&b, a.me()), "fell back to IPv4 under load");
    let st = format!("{:?}{:?}", a.internet_status().unwrap(), b.internet_status().unwrap());
    assert!(!st.contains("::1") && !st.contains("127.0.0.1"), "{st}");
    a.shutdown();
    wait(&b, &mut eb, 10, |e| *e == Event::PeerLeft(a.me()));
    b.shutdown();
}

/// An IPv6 address in PEERPARTY_INET_PEERS: the session runs over IPv6 from its
/// handshake on.
#[test]
fn an_ipv6_peer_address_connects_directly() {
    if UdpSocket::bind("[::1]:0").is_err() {
        eprintln!("no IPv6 loopback here: skipped");
        return;
    }
    let game = format!("test:inet-v6peer:{}", std::process::id());
    let room = RoomCode::generate();
    let (pa, pb) = (base_port(32), base_port(33));
    let mut ca = local_cfg(&room, pa, vec![format!("[::1]:{pb}")]);
    let mut cb = local_cfg(&room, pb, Vec::new());
    ca.allow_private = true;
    cb.allow_private = true;
    let a = start(&game, "a", base_port(34), ca);
    let b = start(&game, "b", base_port(35), cb);
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    wait(&a, &mut ea, 20, |e| *e == Event::PeerJoined(b.me()));
    wait(&b, &mut eb, 20, |e| *e == Event::PeerJoined(a.me()));
    assert!(on_ipv6(&a, b.me()) && on_ipv6(&b, a.me()), "{:#?} {:#?}", a.internet_status(), b.internet_status());
    open_channel(&a, &mut ea, &b, &mut eb);
    assert_eq!(b.send(a.me(), "game", 2, b"over six", true, true, true), SendResult::Sent);
    assert_eq!(receive(&a, 2, 10).data, b"over six");
    a.shutdown();
    b.shutdown();
}

/// One side without IPv6 (`PEERPARTY_IPV6=0`): both stay on IPv4 and it all still works.
#[test]
fn ipv6_off_on_one_side_stays_on_ipv4() {
    let game = format!("test:inet-v4only:{}", std::process::id());
    let room = RoomCode::generate();
    let (pa, pb) = (base_port(22), base_port(23));
    let mut ca = local_cfg(&room, pa, vec![format!("127.0.0.1:{pb}")]);
    let mut cb = local_cfg(&room, pb, Vec::new());
    ca.allow_private = true;
    cb.allow_private = true;
    cb.ipv6 = false;
    let a = start(&game, "a", base_port(24), ca);
    let b = start(&game, "b", base_port(25), cb);
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    wait(&a, &mut ea, 20, |e| *e == Event::PeerJoined(b.me()));
    wait(&b, &mut eb, 20, |e| *e == Event::PeerJoined(a.me()));
    open_channel(&a, &mut ea, &b, &mut eb);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        a.tick();
        b.tick();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!on_ipv6(&a, b.me()) && !on_ipv6(&b, a.me()));
    assert_eq!(a.send(b.me(), "game", 2, b"still here", true, true, true), SendResult::Sent);
    assert_eq!(receive(&b, 2, 10).data, b"still here");
    a.shutdown();
    b.shutdown();
}

#[test]
fn a_lossy_path_still_carries_everything() {
    let game = format!("test:inet-loss:{}", std::process::id());
    let room = RoomCode::generate();
    let (pa, pb) = (base_port(4), base_port(5));
    let mut ca = local_cfg(&room, pa, vec![format!("127.0.0.1:{pb}")]);
    let mut cb = local_cfg(&room, pb, vec![format!("127.0.0.1:{pa}")]);
    ca.loss_percent = 10;
    cb.loss_percent = 10;
    let a = start(&game, "a", base_port(6), ca);
    let b = start(&game, "b", base_port(7), cb);
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    wait(&a, &mut ea, 30, |e| *e == Event::PeerJoined(b.me()));
    wait(&b, &mut eb, 30, |e| *e == Event::PeerJoined(a.me()));
    open_channel(&a, &mut ea, &b, &mut eb);
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 241) as u8).collect();
    for chunk in big.chunks(20_000) {
        assert_eq!(b.send(a.me(), "game", 2, chunk, true, true, true), SendResult::Sent);
    }
    let mut got = Vec::new();
    while got.len() < big.len() {
        got.extend(receive(&a, 2, 60).data);
    }
    assert_eq!(got, big);
    let st = b.internet_status().unwrap();
    assert!(st.peers.iter().any(|p| p.resent > 0), "10% loss and nothing resent? {st:#?}");
    a.shutdown();
    b.shutdown();
}

#[test]
fn another_room_never_connects() {
    let game = format!("test:inet-room:{}", std::process::id());
    let (pa, pb) = (base_port(8), base_port(9));
    let a = start(&game, "a", base_port(10), local_cfg(&RoomCode::generate(), pa, vec![format!("127.0.0.1:{pb}")]));
    let b = start(&game, "b", base_port(11), local_cfg(&RoomCode::generate(), pb, vec![format!("127.0.0.1:{pa}")]));
    let until = Instant::now() + Duration::from_secs(4);
    while Instant::now() < until {
        a.tick();
        b.tick();
        for e in a.poll().into_iter().chain(b.poll()) {
            assert!(!matches!(e, Event::PeerJoined(_)), "a different room connected: {e:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let st = a.internet_status().unwrap();
    assert!(st.peers.iter().all(|p| !p.connected), "{st:#?}");
    a.shutdown();
    b.shutdown();
}

static IMPOSTOR_LOG: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn impostor_log(s: &str) {
    noisy(s);
    IMPOSTOR_LOG.lock().unwrap().push(s.to_string());
}

/// Room members are bound to their keys: once B has met A, another copy with A's id
/// but its own key (a member passing as A) is refused, even after A has left.
#[test]
fn a_room_member_cant_pass_as_another() {
    let game = format!("test:inet-impostor:{}", std::process::id());
    let room = RoomCode::generate();
    let (pa, pb, pc) = (base_port(26), base_port(27), base_port(28));
    let b = Lan::start(Options {
        game: game.clone(),
        identity: Identity::ephemeral("b"),
        discovery_port: base_port(29),
        extra_targets: Vec::new(),
        log: impostor_log,
        internet: Some(local_cfg(&room, pb, Vec::new())),
        public: None,
        lan_discovery: false,
    })
    .expect("start");
    let a = start(&game, "a", base_port(30), local_cfg(&room, pa, vec![format!("127.0.0.1:{pb}")]));
    let mut eb = Vec::new();
    wait(&b, &mut eb, 20, |e| *e == Event::PeerJoined(a.me()));
    let a_id = a.me();
    a.shutdown();
    wait(&b, &mut eb, 15, |e| *e == Event::PeerLeft(a_id));

    let mut stolen = Identity::ephemeral("a");
    stolen.id = a_id;
    let c = Lan::start(Options {
        game: game.clone(),
        identity: stolen,
        discovery_port: base_port(31),
        extra_targets: Vec::new(),
        log: noisy,
        internet: Some(local_cfg(&room, pc, vec![format!("127.0.0.1:{pb}")])),
        public: None,
        lan_discovery: false,
    })
    .expect("start");
    let until = Instant::now() + Duration::from_secs(6);
    while Instant::now() < until {
        b.tick();
        c.tick();
        for e in b.poll() {
            assert!(e != Event::PeerJoined(a_id), "a copy with another key got in as A");
        }
        c.poll();
        std::thread::sleep(Duration::from_millis(50));
    }
    let refused = IMPOSTOR_LOG.lock().unwrap().iter().any(|l| l.contains("claims to be") && l.contains("isn't its key's id"));
    assert!(refused, "B never logged the refusal");
    c.shutdown();
    b.shutdown();
}

/// A one-node DHT on loopback: both copies announce to it and find each other there.
#[test]
fn copies_find_each_other_through_a_dht() {
    let node = UdpSocket::bind("127.0.0.1:0").unwrap();
    node.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    let node_addr = node.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let t = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut n = peerparty_core::inet::dht::TestNode::default();
            let mut buf = [0u8; 2048];
            while !stop.load(Ordering::Relaxed) {
                if let Ok((len, from)) = node.recv_from(&mut buf)
                    && let Some(r) = n.answer(from, &buf[..len])
                {
                    let _ = node.send_to(&r, from);
                }
            }
        })
    };
    let game = format!("test:inet-dht:{}", std::process::id());
    let room = RoomCode::generate();
    let cfg = |port: u16| {
        let mut c = local_cfg(&room, port, Vec::new());
        c.dht = true;
        c.bootstrap = vec![node_addr.to_string()];
        c.allow_private = true;
        c
    };
    let a = start(&game, "a", base_port(12), cfg(base_port(13)));
    let b = start(&game, "b", base_port(14), cfg(base_port(15)));
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    // The first lookups run at once, but the first one may finish before the other copy
    // announced: the next round (15 s on) finds it.
    wait(&a, &mut ea, 45, |e| *e == Event::PeerJoined(b.me()));
    wait(&b, &mut eb, 45, |e| *e == Event::PeerJoined(a.me()));
    let st = a.internet_status().unwrap();
    assert!(st.dht_replies > 0, "{st:#?}");
    a.shutdown();
    b.shutdown();
    stop.store(true, Ordering::Relaxed);
    t.join().unwrap();
}

/// The real internet from this PC: STUN, the public DHT, and our own announcement read
/// back from a second socket. Prints what it found.
#[test]
#[ignore = "needs the internet"]
fn real_stun_and_dht() {
    let stun: Vec<String> = inet::stun::SERVERS.iter().map(|s| s.to_string()).collect();
    let boot: Vec<String> = inet::dht::BOOTSTRAP.iter().map(|s| s.to_string()).collect();
    let p = inet::probe(&stun, &boot).expect("probe");
    for l in &p.lines {
        println!("{l}");
    }
    assert!(p.stun.iter().any(|(_, m)| m.is_some()), "no STUN server answered");
    assert!(p.dht_replies > 0, "no DHT node answered");
}

/// Two copies on this PC meeting through the real public DHT (LAN discovery off). Needs
/// `PEERPARTY_LOOPBACK=0`, and the router to loop our own public address back
/// ("hairpinning"): many do, some don't, so a failure here isn't proof the internet path
/// is broken between two homes.
#[test]
#[ignore = "needs the internet"]
fn real_dht_two_copies_on_this_pc() {
    assert_eq!(std::env::var("PEERPARTY_LOOPBACK").as_deref(), Ok("0"), "run with PEERPARTY_LOOPBACK=0");
    let game = format!("test:inet-real:{}", std::process::id());
    let room = RoomCode::generate();
    let cfg = || {
        let mut c = inet::Config::new(room.clone());
        c.node_cache = None;
        c
    };
    fn log(s: &str) {
        println!("{s}");
    }
    let mk = |name: &str, disc: u16| {
        Lan::start(Options {
            game: game.clone(),
            identity: Identity::ephemeral(name),
            discovery_port: disc,
            extra_targets: Vec::new(),
            log,
            internet: Some(cfg()),
            public: None,
            lan_discovery: false,
        })
        .unwrap()
    };
    let a = mk("a", base_port(16));
    let b = mk("b", base_port(17));
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    wait(&a, &mut ea, 120, |e| *e == Event::PeerJoined(b.me()));
    wait(&b, &mut eb, 30, |e| *e == Event::PeerJoined(a.me()));
    println!("{:#?}", a.internet_status());
    a.shutdown();
    b.shutdown();
}