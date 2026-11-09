//! A peer found on a better path moves to it without leaving: a room or public tunnel
//! gives way to the LAN, a public session to a room, with nothing lost, duplicated or
//! reordered, and no leave or join for the game. Over loopback (PEERPARTY_LOOPBACK=1):
//! "the LAN" is the same-PC board, which the tests turn on once the tunnel is up.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use peerparty_core::inet::{self, RoomCode, public};
use peerparty_core::*;

fn noisy(s: &str) {
    if std::env::var_os("PEERPARTY_TEST_LOG").is_some() {
        eprintln!("{s}");
    }
}

fn base_port(offset: u16) -> u16 {
    // Clear of the other test files' ranges (20000.., 30000.., 40000.., 47600..).
    50000 + (std::process::id() % 700) as u16 * 20 + offset
}

fn room_cfg(room: &RoomCode, port: u16, peers: Vec<String>) -> inet::Config {
    let mut c = inet::Config::new(room.clone());
    c.port = port;
    c.peers = peers;
    c.dht = false;
    c.stun = Vec::new();
    c.node_cache = None;
    c
}

fn public_cfg(name: &str, port: u16, peers: Vec<String>) -> inet::Config {
    let mut c = inet::Config::public(public::PublicCfg::ephemeral(name));
    c.port = port;
    c.peers = peers;
    c.dht = false;
    c.stun = Vec::new();
    c.node_cache = None;
    c.allow_private = true;
    c
}

fn start(game: &str, name: &str, disc: u16, room: Option<inet::Config>, public: Option<inet::Config>, lan: bool) -> Lan {
    Lan::start(Options {
        game: game.into(),
        identity: Identity::ephemeral(name),
        discovery_port: disc,
        extra_targets: Vec::new(),
        log: noisy,
        internet: room,
        public,
        lan_discovery: lan,
    })
    .expect("start")
}

/// Ticks both and keeps their events until `done`.
fn pump_until(a: &Lan, ea: &mut Vec<Event>, b: &Lan, eb: &mut Vec<Event>, secs: u64, what: &str, done: impl Fn(&Lan, &Lan) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done(a, b) {
        a.tick();
        b.tick();
        ea.extend(a.poll());
        eb.extend(b.poll());
        assert!(Instant::now() < deadline, "timed out waiting for {what}; a {ea:#?}; b {eb:#?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn joined(e: &[Event], who: PeerId) -> usize {
    e.iter().filter(|x| **x == Event::PeerJoined(who)).count()
}

fn left(e: &[Event], who: PeerId) -> usize {
    e.iter().filter(|x| **x == Event::PeerLeft(who)).count()
}

fn wait_event(lan: &Lan, seen: &mut Vec<Event>, secs: u64, want: impl Fn(&Event) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !seen.iter().any(&want) {
        lan.tick();
        seen.extend(lan.poll());
        assert!(Instant::now() < deadline, "timed out; events so far: {seen:#?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Opens channel "game" from b to a (both ways usable afterwards).
fn open_channel(a: &Lan, ea: &mut Vec<Event>, b: &Lan, eb: &mut Vec<Event>) {
    assert_eq!(b.send(a.me(), "game", 1, b"hello", true, true, true), SendResult::Held);
    wait_event(a, ea, 10, |e| *e == Event::ChanRequest { peer: b.me(), socket: "game".into() });
    a.accept(b.me(), "game");
    wait_event(a, ea, 10, |e| matches!(e, Event::ChanOpen { .. }));
    wait_event(b, eb, 10, |e| matches!(e, Event::ChanOpen { .. }));
    let p = a.receive(Some(1)).or_else(|| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            a.tick();
            if let Some(p) = a.receive(Some(1)) {
                return Some(p);
            }
            assert!(Instant::now() < deadline, "no hello");
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    assert_eq!(p.unwrap().data, b"hello");
}

/// Numbered reliable messages from `from` to `to` on channel 2, and what has arrived.
struct Stream {
    sent: u32,
    got: Vec<u32>,
}

impl Stream {
    fn new() -> Stream {
        Stream { sent: 0, got: Vec::new() }
    }
    fn send(&mut self, from: &Lan, to: &Lan, n: u32) {
        for _ in 0..n {
            let mut m = self.sent.to_le_bytes().to_vec();
            m.resize(2000, (self.sent % 251) as u8);
            assert_eq!(from.send(to.me(), "game", 2, &m, true, true, true), SendResult::Sent, "message {}", self.sent);
            self.sent += 1;
        }
    }
    fn drain(&mut self, to: &Lan) {
        while let Some(p) = to.receive(Some(2)) {
            assert_eq!(p.data.len(), 2000);
            let n = u32::from_le_bytes(p.data[..4].try_into().unwrap());
            assert!(p.data[4..].iter().all(|&x| x == (n % 251) as u8), "message {n} corrupted");
            self.got.push(n);
        }
    }
    /// Everything arrived exactly once, in order.
    fn check(&self) {
        let want: Vec<u32> = (0..self.sent).collect();
        if self.got != want {
            let dups = self.got.len() as i64 - self.got.iter().collect::<std::collections::HashSet<_>>().len() as i64;
            panic!("sent {}, got {} ({} duplicates, first out of place at {:?})", self.sent, self.got.len(), dups, self.got.iter().zip(&want).position(|(a, b)| a != b));
        }
    }
}

/// Keeps both streams going while ticking, until `done`.
#[allow(clippy::too_many_arguments)]
fn stream_until(
    a: &Lan,
    ea: &mut Vec<Event>,
    b: &Lan,
    eb: &mut Vec<Event>,
    ab: &mut Stream,
    ba: &mut Stream,
    secs: u64,
    what: &str,
    done: impl Fn(&Lan, &Lan) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done(a, b) {
        ab.send(a, b, 5);
        ba.send(b, a, 5);
        for _ in 0..3 {
            a.tick();
            b.tick();
            ea.extend(a.poll());
            eb.extend(b.poll());
            ab.drain(b);
            ba.drain(a);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}; a {ea:#?}; b {eb:#?}");
    }
}

fn finish_streams(a: &Lan, ea: &mut Vec<Event>, b: &Lan, eb: &mut Vec<Event>, ab: &mut Stream, ba: &mut Stream) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while ab.got.len() < ab.sent as usize || ba.got.len() < ba.sent as usize {
        a.tick();
        b.tick();
        ea.extend(a.poll());
        eb.extend(b.poll());
        ab.drain(b);
        ba.drain(a);
        assert!(Instant::now() < deadline, "a->b got {}/{}, b->a got {}/{}", ab.got.len(), ab.sent, ba.got.len(), ba.sent);
        std::thread::sleep(Duration::from_millis(5));
    }
    // Nothing extra trickles in later (no duplicates from the old path).
    for _ in 0..50 {
        a.tick();
        b.tick();
        ab.drain(b);
        ba.drain(a);
        std::thread::sleep(Duration::from_millis(20));
    }
    ab.check();
    ba.check();
}

fn lobby_shared(a: &Lan, ea: &mut Vec<Event>, b: &Lan, eb: &mut Vec<Event>) -> String {
    let mut spec = Lobby::new("eos.lobby");
    spec.bucket = "m1".into();
    let hosted = a.create_lobby(spec).unwrap();
    let mut q = Query::new("eos.lobby");
    q.filters.push(Filter::Bucket { op: Op::Eq, value: "m1".into() });
    let search = b.search(q);
    wait_event(b, eb, 10, |e| matches!(e, Event::SearchDone { search: s, .. } if *s == search));
    let Some(Event::SearchDone { results, .. }) = eb.iter().find(|e| matches!(e, Event::SearchDone { search: s, .. } if *s == search)).cloned() else {
        unreachable!()
    };
    assert_eq!(results.len(), 1);
    b.join(&results[0]).unwrap();
    wait_event(b, eb, 10, |e| *e == Event::JoinDone { lobby: hosted.id.clone(), result: JoinResult::Ok });
    let _ = ea;
    hosted.id
}

/// No leave of either peer or of the lobby, and one join each.
fn no_leave(a: &Lan, ea: &[Event], b: &Lan, eb: &[Event]) {
    assert_eq!((joined(ea, b.me()), left(ea, b.me())), (1, 0), "a's events {ea:#?}");
    assert_eq!((joined(eb, a.me()), left(eb, a.me())), (1, 0), "b's events {eb:#?}");
    for e in ea.iter().chain(eb) {
        let s = format!("{e:?}");
        assert!(!s.contains("Left") && !s.contains("Kicked") && !s.contains("Closed"), "{s}");
    }
}

fn trust(want: Trust) -> impl Fn(&Lan, &Lan) -> bool {
    move |a: &Lan, b: &Lan| a.peer_trust(b.me()) == Some(want) && b.peer_trust(a.me()) == Some(want)
}

fn bridged(l: &Lan) -> usize {
    l.internet_status().map_or(0, |s| s.peers.iter().filter(|p| p.bridged).count())
        + l.public_status().map_or(0, |s| s.peers.iter().filter(|p| p.bridged).count())
}

#[test]
fn a_room_peer_moves_to_the_lan_without_leaving() {
    let game = format!("test:move-lan:{}", std::process::id());
    let room = RoomCode::generate();
    let (pa, pb, disc) = (base_port(0), base_port(1), base_port(2));
    let a = start(&game, "a", disc, Some(room_cfg(&room, pa, vec![format!("127.0.0.1:{pb}")])), None, false);
    let b = start(&game, "b", disc, Some(room_cfg(&room, pb, Vec::new())), None, false);
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    pump_until(&a, &mut ea, &b, &mut eb, 20, "the room tunnel", trust(Trust::Room));
    let stand_in = b.peer_ip(a.me()).unwrap();
    assert_eq!(stand_in, inet::redact::stand_in(&a.me().0));
    lobby_shared(&a, &mut ea, &b, &mut eb);
    open_channel(&a, &mut ea, &b, &mut eb);
    let (mut ab, mut ba) = (Stream::new(), Stream::new());
    stream_until(&a, &mut ea, &b, &mut eb, &mut ab, &mut ba, 5, "some data", |_, _| ab_sent_some());
    a.set_lan_discovery(true);
    b.set_lan_discovery(true);
    stream_until(&a, &mut ea, &b, &mut eb, &mut ab, &mut ba, 40, "the move to the LAN", trust(Trust::Lan));
    // Keep going a while on the new path, then everything must be there, once, in order.
    let until = Instant::now() + Duration::from_secs(2);
    stream_until(&a, &mut ea, &b, &mut eb, &mut ab, &mut ba, 10, "more data", |_, _| Instant::now() >= until);
    finish_streams(&a, &mut ea, &b, &mut eb, &mut ab, &mut ba);
    assert!(ab.sent > 100 && ba.sent > 100);
    no_leave(&a, &ea, &b, &eb);
    // Games keep the address they were told; the LAN address is there for those that
    // open their own sockets.
    assert_eq!(b.peer_ip(a.me()), Some(stand_in));
    assert_eq!(b.peer_direct_ip(a.me()), Some(std::net::Ipv4Addr::LOCALHOST.into()));
    assert!(b.peer_has_ip(a.me(), stand_in) && b.peer_has_ip(a.me(), std::net::Ipv4Addr::LOCALHOST.into()));
    // The tunnel stops carrying the mesh.
    pump_until(&a, &mut ea, &b, &mut eb, 30, "the tunnel to wind down", |a, b| bridged(a) == 0 && bridged(b) == 0);
    no_leave(&a, &ea, &b, &eb);
    a.shutdown();
    b.shutdown();
}

fn ab_sent_some() -> bool {
    // Five rounds of stream_until (25 messages each way) before the switch.
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    N.fetch_add(1, Ordering::Relaxed) % 5 == 4
}

#[test]
fn lan_first_needs_no_tunnel_and_the_tunnel_takes_over_when_the_lan_goes() {
    let game = format!("test:move-first:{}", std::process::id());
    let room = RoomCode::generate();
    let (pa, pb, disc) = (base_port(3), base_port(4), base_port(5));
    let a = start(&game, "a", disc, Some(room_cfg(&room, pa, vec![format!("127.0.0.1:{pb}")])), None, true);
    let b = start(&game, "b", disc, Some(room_cfg(&room, pb, Vec::new())), None, true);
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    pump_until(&a, &mut ea, &b, &mut eb, 30, "the LAN", trust(Trust::Lan));
    // The room session comes up but never carries the mesh.
    let until = Instant::now() + Duration::from_secs(8);
    pump_until(&a, &mut ea, &b, &mut eb, 15, "a few seconds", |_, _| Instant::now() >= until);
    assert!(a.internet_status().unwrap().peers.iter().any(|p| p.connected), "{:#?}", a.internet_status());
    assert_eq!((bridged(&a), bridged(&b)), (0, 0));
    assert_eq!((joined(&ea, b.me()), joined(&eb, a.me())), (1, 1));
    // The LAN goes away: the tunnel takes over (a leave and a join: the peer was gone).
    a.set_lan_discovery(false);
    b.set_lan_discovery(false);
    a.cut_connection(b.me());
    let deadline = Instant::now() + Duration::from_secs(30);
    while joined(&ea, b.me()) < 2 || joined(&eb, a.me()) < 2 {
        a.tick();
        b.tick();
        ea.extend(a.poll());
        eb.extend(b.poll());
        assert!(Instant::now() < deadline, "the tunnel never took over: a {ea:#?}; b {eb:#?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(trust(Trust::Room)(&a, &b));
    assert_eq!((joined(&ea, b.me()), left(&ea, b.me())), (2, 1), "{ea:#?}");
    assert_eq!((joined(&eb, a.me()), left(&eb, a.me())), (2, 1), "{eb:#?}");
    open_channel(&a, &mut ea, &b, &mut eb);
    a.shutdown();
    b.shutdown();
}

/// A UDP forwarder between a and b's room sockets that passes nothing until opened (a
/// room path that appears later).
struct Gate {
    addr: SocketAddr,
    open: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Gate {
    fn new(to: SocketAddr) -> Gate {
        let near = UdpSocket::bind("127.0.0.1:0").unwrap();
        let far = UdpSocket::bind("127.0.0.1:0").unwrap();
        near.set_read_timeout(Some(Duration::from_millis(2))).unwrap();
        far.set_read_timeout(Some(Duration::from_millis(2))).unwrap();
        let addr = near.local_addr().unwrap();
        let (open, stop) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (o, s) = (open.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            let mut back: Option<SocketAddr> = None;
            while !s.load(Ordering::Relaxed) {
                if let Ok((n, from)) = near.recv_from(&mut buf) {
                    back = Some(from);
                    if o.load(Ordering::Relaxed) {
                        let _ = far.send_to(&buf[..n], to);
                    }
                }
                if let Ok((n, _)) = far.recv_from(&mut buf)
                    && o.load(Ordering::Relaxed)
                    && let Some(b) = back
                {
                    let _ = near.send_to(&buf[..n], b);
                }
            }
        });
        Gate { addr, open, stop }
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[test]
fn a_public_player_moves_to_a_room_without_leaving() {
    let game = format!("test:move-room:{}", std::process::id());
    let room = RoomCode::generate();
    let (ra, rb, qa, qb, disc_a, disc_b) = (base_port(6), base_port(7), base_port(8), base_port(9), base_port(10), base_port(11));
    let gate = Gate::new(format!("127.0.0.1:{rb}").parse().unwrap());
    let a = start(&game, "a", disc_a, Some(room_cfg(&room, ra, vec![gate.addr.to_string()])), Some(public_cfg("pa", qa, vec![format!("127.0.0.1:{qb}")])), false);
    let b = start(&game, "b", disc_b, Some(room_cfg(&room, rb, Vec::new())), Some(public_cfg("pb", qb, Vec::new())), false);
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    pump_until(&a, &mut ea, &b, &mut eb, 30, "the public session", trust(Trust::Public));
    let stand_in = b.peer_ip(a.me()).unwrap();
    lobby_shared(&a, &mut ea, &b, &mut eb);
    open_channel(&a, &mut ea, &b, &mut eb);
    let (mut ab, mut ba) = (Stream::new(), Stream::new());
    let until = Instant::now() + Duration::from_millis(500);
    stream_until(&a, &mut ea, &b, &mut eb, &mut ab, &mut ba, 5, "some data", |_, _| Instant::now() >= until);
    gate.open.store(true, Ordering::Relaxed);
    stream_until(&a, &mut ea, &b, &mut eb, &mut ab, &mut ba, 40, "the move to the room", trust(Trust::Room));
    let until = Instant::now() + Duration::from_secs(2);
    stream_until(&a, &mut ea, &b, &mut eb, &mut ab, &mut ba, 10, "more data", |_, _| Instant::now() >= until);
    finish_streams(&a, &mut ea, &b, &mut eb, &mut ab, &mut ba);
    no_leave(&a, &ea, &b, &eb);
    assert_eq!(b.peer_ip(a.me()), Some(stand_in));
    assert_eq!(b.peer_direct_ip(a.me()), None, "a room tunnel is no direct path");
    a.shutdown();
    b.shutdown();
}

#[test]
fn a_public_player_on_the_lan_moves_there_and_is_trusted() {
    let game = format!("test:move-pub-lan:{}", std::process::id());
    let (qa, qb, disc) = (base_port(12), base_port(13), base_port(14));
    let a = start(&game, "a", disc, None, Some(public_cfg("pa", qa, vec![format!("127.0.0.1:{qb}")])), false);
    let b = start(&game, "b", disc, None, Some(public_cfg("pb", qb, Vec::new())), false);
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    pump_until(&a, &mut ea, &b, &mut eb, 30, "the public session", trust(Trust::Public));
    lobby_shared(&a, &mut ea, &b, &mut eb);
    open_channel(&a, &mut ea, &b, &mut eb);
    let (mut ab, mut ba) = (Stream::new(), Stream::new());
    let until = Instant::now() + Duration::from_millis(500);
    stream_until(&a, &mut ea, &b, &mut eb, &mut ab, &mut ba, 5, "some data", |_, _| Instant::now() >= until);
    a.set_lan_discovery(true);
    b.set_lan_discovery(true);
    stream_until(&a, &mut ea, &b, &mut eb, &mut ab, &mut ba, 40, "the move to the LAN", trust(Trust::Lan));
    finish_streams(&a, &mut ea, &b, &mut eb, &mut ab, &mut ba);
    no_leave(&a, &ea, &b, &eb);
    // Trusted now: a LAN peer, without the public limits (presence caps, the voice opt-out).
    assert!(!a.peer_is_internet(b.me()) && !b.peer_is_internet(a.me()));
    a.shutdown();
    b.shutdown();
}

#[test]
fn a_blocked_lan_is_tried_at_most_every_15_seconds() {
    // Production dials a blocked LAN at most every 15s. The same cap, on a shorter
    // interval: a few tries across a burst of beacons, not one dial per beacon.
    let every = Duration::from_millis(300);
    let _timing = TestTiming::install(TestIntervals { lan_upgrade_every: Some(every), ..TestIntervals::NONE });
    let game = format!("test:move-blocked:{}", std::process::id());
    let room = RoomCode::generate();
    // Separate discovery ports: no board between them; only forged beacons.
    let (pa, pb, da, db) = (base_port(15), base_port(16), base_port(17), base_port(18));
    let a = start(&game, "a", da, Some(room_cfg(&room, pa, vec![format!("127.0.0.1:{pb}")])), None, true);
    let b = start(&game, "b", db, Some(room_cfg(&room, pb, Vec::new())), None, true);
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    pump_until(&a, &mut ea, &b, &mut eb, 20, "the room tunnel", trust(Trust::Room));
    // The lower id dials: it hears the other's beacons, but its port leads nowhere.
    let (low, high, low_disc) = if a.me() < b.me() { (&a, &b, da) } else { (&b, &a, db) };
    let dead = TcpPortClosed::new();
    let beacon = test_beacon(&game, high.me(), dead.port);
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let before = low.lan_upgrade_tries();
    let mut beacons = 0u64;
    let end = Instant::now() + every * 3;
    while Instant::now() < end {
        beacons += 1;
        let _ = sock.send_to(&beacon, ("127.0.0.1", low_disc));
        a.tick();
        b.tick();
        ea.extend(a.poll());
        eb.extend(b.poll());
        std::thread::sleep(Duration::from_millis(50));
    }
    let tries = low.lan_upgrade_tries() - before;
    assert!(tries >= 2, "{tries} tries in {:?}, wanted a retry", every * 3);
    assert!(tries < beacons / 2, "{tries} tries for {beacons} beacons");
    assert_eq!(high.lan_upgrade_tries(), 0, "the higher id never dials a known peer");
    // Still on the tunnel, nobody left.
    assert!(trust(Trust::Room)(&a, &b));
    no_leave(&a, &ea, &b, &eb);
    a.shutdown();
    b.shutdown();
}

/// A loopback TCP port with nothing listening.
struct TcpPortClosed {
    port: u16,
}

impl TcpPortClosed {
    fn new() -> TcpPortClosed {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        TcpPortClosed { port }
    }
}
#[test]
fn more_than_four_players_behind_one_address_all_connect() {
    // One router, six players: the hub sees them all at 127.0.0.1, each with its own key.
    let game = format!("test:same-ip:{}", std::process::id());
    let hub_port = base_port(19);
    let hub = start(&game, "hub", hub_port + 1000, None, Some(public_cfg("hub", hub_port, Vec::new())), false);
    let players: Vec<Lan> = (0..6u16)
        .map(|i| {
            let p = 64000 + (std::process::id() % 70) as u16 * 15 + i * 2;
            start(&game, &format!("p{i}"), p + 1, None, Some(public_cfg(&format!("p{i}"), p, vec![format!("127.0.0.1:{hub_port}")])), false)
        })
        .collect();
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(40);
    while !players.iter().all(|p| joined(&seen, p.me()) == 1) {
        hub.tick();
        seen.extend(hub.poll());
        for p in &players {
            p.tick();
            p.poll();
        }
        assert!(Instant::now() < deadline, "hub saw {seen:#?}; status {:#?}", hub.public_status());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(players.iter().all(|p| hub.peer_trust(p.me()) == Some(Trust::Public)));
    for p in &players {
        p.shutdown();
    }
    hub.shutdown();
}