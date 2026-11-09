//! Public sessions: several copies on one PC, each in public mode with its own key, meet
//! through a loopback DHT node and see each other's lobbies, with no address anywhere a
//! game or the UI would show it; forged handshakes and blocked players get nowhere.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use peerparty_core::inet::{self, RoomCode, crypto, public, redact};
use peerparty_core::*;

static LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn log(s: &str) {
    if std::env::var_os("PEERPARTY_TEST_LOG").is_some() {
        eprintln!("{s}");
    }
    let mut l = LOGS.lock().unwrap();
    if l.len() < 200_000 {
        l.push(s.to_string());
    }
}

fn base_port(offset: u16) -> u16 {
    // Clear of the other test files' ranges (40000.. and 20000..).
    30000 + (std::process::id() % 1000) as u16 * 8 + offset
}

struct Node {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    t: Option<std::thread::JoinHandle<()>>,
}

impl Node {
    fn start() -> Node {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let addr = sock.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let t = std::thread::spawn(move || {
            let mut n = inet::dht::TestNode::default();
            let mut buf = [0u8; 2048];
            while !s2.load(Ordering::Relaxed) {
                if let Ok((len, from)) = sock.recv_from(&mut buf)
                    && let Some(r) = n.answer(from, &buf[..len])
                {
                    let _ = sock.send_to(&r, from);
                }
            }
        });
        Node { addr, stop, t: Some(t) }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.t.take() {
            let _ = t.join();
        }
    }
}

fn public_cfg(name: &str, port: u16, node: Option<SocketAddr>, peers: Vec<String>) -> inet::Config {
    let mut c = inet::Config::public(public::PublicCfg::ephemeral(name));
    c.port = port;
    c.peers = peers;
    c.dht = node.is_some();
    c.bootstrap = node.map(|n| vec![n.to_string()]).unwrap_or_default();
    c.stun = Vec::new();
    c.node_cache = None;
    c.allow_private = true;
    c
}

fn start(game: &str, lan_name: &str, disc: u16, public: inet::Config) -> Lan {
    Lan::start(Options {
        game: game.into(),
        identity: Identity::ephemeral(lan_name),
        discovery_port: disc,
        extra_targets: Vec::new(),
        log,
        internet: None,
        public: Some(public),
        lan_discovery: false,
    })
    .expect("start")
}

fn pump(lans: &[&Lan], seen: &mut [Vec<Event>]) {
    for (l, s) in lans.iter().zip(seen.iter_mut()) {
        l.tick();
        s.extend(l.poll());
    }
}

fn wait_all(lans: &[&Lan], seen: &mut [Vec<Event>], secs: u64, done: impl Fn(&[Vec<Event>]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done(seen) {
        pump(lans, seen);
        assert!(
            Instant::now() < deadline,
            "timed out; events {seen:#?}; status {:#?}",
            lans.iter().map(|l| l.public_status()).collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn joined(seen: &[Event], who: PeerId) -> bool {
    seen.contains(&Event::PeerJoined(who))
}

fn is_stand_in(ip: std::net::IpAddr) -> bool {
    matches!(ip, std::net::IpAddr::V4(v) if v.octets()[0] == 198 && (v.octets()[1] & 0xfe) == 18)
}

#[test]
fn three_public_players_find_each_other_and_their_lobbies() {
    let node = Node::start();
    let game = format!("test:public:{}", std::process::id());
    let ports = [base_port(0), base_port(1), base_port(2)];
    // LAN-side names hold an address and junk: public peers must never see them.
    let a = start(&game, "10.1.2.3", base_port(3), public_cfg("Alice", ports[0], Some(node.addr), Vec::new()));
    let b = start(&game, "bob\u{202e}evil", base_port(4), public_cfg("Bob", ports[1], Some(node.addr), Vec::new()));
    let c = start(&game, "carol", base_port(5), public_cfg("Carol", ports[2], Some(node.addr), Vec::new()));
    let lans = [&a, &b, &c];
    let mut seen = vec![Vec::new(), Vec::new(), Vec::new()];
    let ids = [a.me(), b.me(), c.me()];
    wait_all(&lans, &mut seen, 60, |s| {
        (0..3).all(|i| (0..3).filter(|&j| j != i).all(|j| joined(&s[i], ids[j])))
    });

    // Who's who: the signed public names, each peer a public one with a key, at a stand-in.
    let names = ["Alice", "Bob", "Carol"];
    for (i, l) in lans.iter().enumerate() {
        for j in (0..3).filter(|&j| j != i) {
            assert_eq!(l.peer_name(ids[j]).as_deref(), Some(names[j]), "{i} sees {j}");
            assert_eq!(l.peer_trust(ids[j]), Some(Trust::Public));
            assert!(l.peer_key(ids[j]).is_some());
            let ip = l.peer_ip(ids[j]).unwrap();
            assert!(is_stand_in(ip), "{ip}");
        }
        let st = l.public_status().unwrap();
        assert!(st.public_mode);
        assert!(st.dht_replies > 0, "{st:#?}");
        assert_eq!(st.peers.iter().filter(|p| p.connected).count(), 2, "{st:#?}");
    }

    // A hosts; B and C find it in a search, and join.
    let mut spec = Lobby::new("eos.lobby");
    spec.bucket = "pub".into();
    let hosted = a.create_lobby(spec).unwrap();
    for (k, l) in [(1usize, &b), (2, &c)] {
        let mut q = Query::new("eos.lobby");
        q.filters.push(Filter::Bucket { op: Op::Eq, value: "pub".into() });
        let search = l.search(q);
        let deadline = Instant::now() + Duration::from_secs(15);
        let results = loop {
            pump(&lans, &mut seen);
            if let Some(Event::SearchDone { results, .. }) =
                seen[k].iter().find(|e| matches!(e, Event::SearchDone { search: s, .. } if *s == search))
            {
                break results.clone();
            }
            assert!(Instant::now() < deadline, "no search result on {k}");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(results.len(), 1, "{results:#?}");
        assert_eq!(results[0].id, hosted.id);
        // Nothing in the lobby a game sees holds an address.
        let shown = format!("{:?}", results[0]);
        assert!(!redact::has_address(&shown) && !shown.contains("127.0.0.1"), "{shown}");
        l.join(&results[0]).unwrap();
    }
    wait_all(&lans, &mut seen, 15, |s| {
        [1usize, 2].iter().all(|&k| {
            s[k].iter().any(|e| *e == Event::JoinDone { lobby: hosted.id.clone(), result: JoinResult::Ok })
        })
    });

    // Nothing UI-facing holds an address: names, status, logs.
    for l in lans {
        for (_, n) in l.peers() {
            assert!(!redact::has_address(&n), "{n}");
        }
        let st = format!("{:?}", l.public_status().unwrap());
        assert!(!st.contains("127.0.0.1"), "{st}");
    }
    let logs = LOGS.lock().unwrap().join("\n");
    for p in ports {
        assert!(!logs.contains(&format!("127.0.0.1:{p}")), "a public address in the log");
    }
    assert!(logs.contains("connected to public player"), "no connect line logged");
    for l in lans {
        l.shutdown();
    }
}

#[test]
fn a_blocked_player_is_dropped_and_stays_out() {
    let game = format!("test:public-block:{}", std::process::id());
    let (pa, pb, pc) = (base_port(6), base_port(7), base_port(8));
    let peers = |me: u16| [pa, pb, pc].iter().filter(|&&p| p != me).map(|p| format!("127.0.0.1:{p}")).collect();
    let a = start(&game, "a", base_port(9), public_cfg("Ann", pa, None, peers(pa)));
    let b = start(&game, "b", base_port(10), public_cfg("Ben", pb, None, peers(pb)));
    let c = start(&game, "c", base_port(11), public_cfg("Cat", pc, None, peers(pc)));
    let lans = [&a, &b, &c];
    let mut seen = vec![Vec::new(), Vec::new(), Vec::new()];
    let (ida, idb, idc) = (a.me(), b.me(), c.me());
    wait_all(&lans, &mut seen, 30, |s| joined(&s[0], idb) && joined(&s[0], idc) && joined(&s[1], ida));

    let fp = a.block_peer(idb).expect("block");
    assert!(!fp.is_empty());
    wait_all(&lans, &mut seen, 15, |s| s[0].contains(&Event::PeerLeft(idb)));
    assert_eq!(a.public_status().unwrap().blocked, 1);
    // B keeps trying (it has A as a static peer); A never lets it back.
    for s in seen.iter_mut() {
        s.clear();
    }
    let until = Instant::now() + Duration::from_secs(8);
    while Instant::now() < until {
        pump(&lans, &mut seen);
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(!joined(&seen[0], idb), "a blocked player came back: {:#?}", seen[0]);
    assert!(a.peers().iter().any(|(id, _)| *id == idc), "C should still be there");
    assert!(!a.peers().iter().any(|(id, _)| *id == idb));
    // A LAN-side lan_name is never what a public peer sees.
    assert_eq!(c.peer_name(ida).as_deref(), Some("Ann"));
    // Not a public player: no blocking room or LAN peers this way.
    assert!(a.block_peer(PeerId([7; 16])).is_err());
    for l in lans {
        l.shutdown();
    }
}

/// Someone who knows the (public) room code can start handshakes, but without a valid
/// signed advert over this very handshake they never become a peer: no advert, a
/// tampered one, one signed for another handshake, or one claiming another's key.
#[test]
fn forged_and_tampered_adverts_never_connect() {
    let game = format!("test:public-forge:{}", std::process::id());
    let pa = base_port(12);
    let a = start(&game, "a", base_port(13), public_cfg("Ann", pa, None, Vec::new()));
    let target: SocketAddr = format!("127.0.0.1:{pa}").parse().unwrap();
    let room = RoomCode::public();
    let room_id = room.room_id();
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let key = public::IdentityKey::generate();
    let other = public::IdentityKey::generate();
    let me = PeerId([0x42; 16]);
    let info = inet::testing::peer_info(me, &game, "Mallory");

    // One handshake per forgery: returns A's blob for this attempt.
    let handshake = |eph: &crypto::Ephemeral| -> [u8; 48] {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 2048];
        loop {
            sock.send_to(&inet::testing::init(&room, eph), target).unwrap();
            a.tick();
            if let Ok((n, _)) = sock.recv_from(&mut buf)
                && let Some(b) = inet::testing::confirm_sender(&room, eph, &buf[..n])
            {
                return b;
            }
            assert!(Instant::now() < deadline, "no CONFIRM from the engine");
        }
    };
    type Forge<'a> = Box<dyn Fn(&crypto::Ephemeral, &[u8; 48]) -> Vec<u8> + 'a>;
    let mut attempts: Vec<(&str, Forge)> = Vec::new();
    attempts.push(("no advert", Box::new(|_, _| info.clone())));
    attempts.push((
        "tampered",
        Box::new(|e, theirs| {
            let mut v = public::advert(&key, &room_id, &e.blob, theirs, &info);
            let n = v.len();
            v[n - 70] ^= 1;
            v
        }),
    ));
    attempts.push((
        "tampered info",
        Box::new(|e, theirs| {
            let mut v = public::advert(&key, &room_id, &e.blob, theirs, &info);
            if let Some(p) = v.windows(7).position(|w| w == b"Mallory") {
                v[p] = b'X';
            }
            v
        }),
    ));
    attempts.push((
        "another handshake's",
        Box::new(|e, _| public::advert(&key, &room_id, &e.blob, &[9; 48], &info)),
    ));
    attempts.push((
        "swapped key",
        Box::new(|e, theirs| {
            let good = public::advert(&key, &room_id, &e.blob, theirs, &info);
            let bad = public::advert(&other, &room_id, &e.blob, theirs, &info);
            // Signed by `other`, but claiming `key`: [len][info][key 32][sig 64].
            let n = bad.len();
            let mut v = bad.clone();
            v[n - 96..n - 64].copy_from_slice(&good[n - 96..n - 64]);
            v
        }),
    ));
    for (what, body) in &attempts {
        let eph = crypto::Ephemeral::new();
        let theirs = handshake(&eph);
        let p = inet::testing::confirm(&room, &eph, &theirs, &body(&eph, &theirs)).unwrap();
        sock.send_to(&p, target).unwrap();
        let until = Instant::now() + Duration::from_millis(600);
        while Instant::now() < until {
            a.tick();
            for e in a.poll() {
                assert!(!matches!(e, Event::PeerJoined(_)), "{what}: a forged advert connected");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let st = a.public_status().unwrap();
        assert!(st.peers.iter().all(|p| !p.connected), "{what}: {st:#?}");
    }

    // The control: a correct advert does get through the handshake (it's a test peer with
    // no mesh behind it, so it only shows as connected in the status).
    let eph = crypto::Ephemeral::new();
    let theirs = handshake(&eph);
    let good = public::advert(&key, &room_id, &eph.blob, &theirs, &info);
    sock.send_to(&inet::testing::confirm(&room, &eph, &theirs, &good).unwrap(), target).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        a.tick();
        a.poll();
        let st = a.public_status().unwrap();
        if st.peers.iter().any(|p| p.connected && p.key.as_deref() == Some(&public::fingerprint(&key.public()))) {
            break;
        }
        assert!(Instant::now() < deadline, "a valid advert didn't connect: {st:#?}");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Junk at the port: ignored, the engine keeps going.
    let mut x = 0x1234_5678u32;
    for _ in 0..3000 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let n = (x % 300) as usize;
        let mut d: Vec<u8> = (0..n).map(|i| (x.rotate_left(i as u32 % 32) as u8) ^ i as u8).collect();
        if d.len() > 3 {
            d[0] = 0xC7;
            d[1] = 1;
            d[2] = (x % 4) as u8;
        }
        let _ = sock.send_to(&d, target);
    }
    std::thread::sleep(Duration::from_millis(300));
    a.tick();
    a.poll();
    let st = a.public_status().unwrap();
    assert!(st.peers.len() <= 16, "{st:#?}");
    a.shutdown();
}
/// A raw test player (no mesh behind it): handshakes with `a` at `target` from a fresh
/// socket with `key`, and says whether `a` lists it as connected within `secs`.
struct Raw {
    /// The peer id it says it is.
    id: PeerId,
    sock: UdpSocket,
    eph: crypto::Ephemeral,
    theirs: [u8; 48],
    connected: bool,
}

fn raw_connect(a: &Lan, target: SocketAddr, game: &str, key: &public::IdentityKey, name: &str, secs: u64) -> Raw {
    raw_connect_as(a, target, game, key, PeerId(crypto::random_array()), name, secs)
}

/// `raw_connect` with a given peer id (the same player started again).
fn raw_connect_as(a: &Lan, target: SocketAddr, game: &str, key: &public::IdentityKey, id: PeerId, name: &str, secs: u64) -> Raw {
    let room = RoomCode::public();
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    let eph = crypto::Ephemeral::new();
    let mut buf = [0u8; 2048];
    let deadline = Instant::now() + Duration::from_secs(secs);
    let theirs = loop {
        sock.send_to(&inet::testing::init(&room, &eph), target).unwrap();
        a.tick();
        a.poll();
        if let Ok((n, _)) = sock.recv_from(&mut buf)
            && let Some(b) = inet::testing::confirm_sender(&room, &eph, &buf[..n])
        {
            break b;
        }
        if Instant::now() >= deadline {
            return Raw { id, sock, eph, theirs: [0; 48], connected: false };
        }
    };
    let info = inet::testing::peer_info(id, game, name);
    let body = public::advert(key, &RoomCode::public().room_id(), &eph.blob, &theirs, &info);
    sock.send_to(&inet::testing::confirm(&room, &eph, &theirs, &body).unwrap(), target).unwrap();
    let fp = public::fingerprint(&key.public());
    while Instant::now() < deadline {
        a.tick();
        a.poll();
        if a.public_status().unwrap().peers.iter().any(|p| p.connected && p.key.as_deref() == Some(&fp)) {
            return Raw { id, sock, eph, theirs, connected: true };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Raw { id, sock, eph, theirs, connected: false }
}

/// The first handshake packet, which anyone can send to a public player: mutated INITs,
/// properly MACed ones carrying junk keys (zeros, all ones, low-order points,
/// random), and noise behind a valid header. None may crash or
/// wedge the engine: a real player connects afterwards.
#[test]
fn junk_first_handshake_packets_get_nowhere() {
    let game = format!("test:public-initfuzz:{}", std::process::id());
    let pa = base_port(40);
    let a = start(&game, "a", base_port(41), public_cfg("Ann", pa, None, Vec::new()));
    let target: SocketAddr = format!("127.0.0.1:{pa}").parse().unwrap();
    let room = RoomCode::public();
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let valid = inet::testing::init(&room, &crypto::Ephemeral::new());
    // Low-order Curve25519 points (their DH output is all zeros) and other odd keys.
    let mut odd: Vec<[u8; 48]> = vec![[0; 48], [0xFF; 48]];
    let mut one = [0u8; 48];
    one[0] = 1;
    odd.push(one);
    let mut order8 = [0u8; 48];
    order8[..32].copy_from_slice(&[
        0xe0, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1, 0x9f, 0xc4, 0x6a, 0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1,
        0xfd, 0x86, 0x62, 0x05, 0x16, 0x5f, 0x49, 0xb8, 0x00,
    ]);
    odd.push(order8);
    for i in 0..3000 {
        let p = match i % 4 {
            0 => {
                let mut m = valid.clone();
                for _ in 0..1 + next() % 4 {
                    let j = (next() % m.len() as u64) as usize;
                    m[j] ^= 1 << (next() % 8);
                }
                if next() % 5 == 0 {
                    m.truncate((next() % m.len() as u64) as usize);
                } else if next() % 5 == 0 {
                    m.extend((0..next() % 64).map(|_| next() as u8));
                }
                m
            }
            1 => inet::testing::init_with(&room, &odd[(next() % odd.len() as u64) as usize]),
            2 => {
                let mut blob = [0u8; 48];
                blob.iter_mut().for_each(|b| *b = next() as u8);
                inet::testing::init_with(&room, &blob)
            }
            _ => {
                let mut m = valid[..3].to_vec();
                m.extend((0..next() % 200).map(|_| next() as u8));
                m
            }
        };
        let _ = sock.send_to(&p, target);
        if i % 100 == 0 {
            a.tick();
            a.poll();
        }
    }
    // Whatever it made of them, a real player still gets in (after the rate limit for
    // this address has refilled).
    std::thread::sleep(Duration::from_secs(3));
    let key = public::IdentityKey::generate();
    assert!(raw_connect(&a, target, &game, &key, "Real", 15).connected, "a real player couldn't connect after the junk");
    a.shutdown();
}

/// A blocked player who comes back with a fresh key from the same internet address is
/// refused for a while; a friend from that address still gets in.
#[test]
fn a_blocked_player_with_a_new_key_is_held_off() {
    let game = format!("test:public-evade:{}", std::process::id());
    let pa = base_port(14);
    let mut cfg = public_cfg("Ann", pa, None, Vec::new());
    let p = cfg.public.clone().unwrap();
    cfg.public = Some(p.clone());
    let a = start(&game, "a", base_port(15), cfg);
    let target: SocketAddr = format!("127.0.0.1:{pa}").parse().unwrap();
    let (k1, k2, k3) = (public::IdentityKey::generate(), public::IdentityKey::generate(), public::IdentityKey::generate());
    p.blocked.add(&k1.fingerprint(), "griefer").unwrap();
    assert!(!raw_connect(&a, target, &game, &k1, "Grief", 2).connected, "a blocked key connected");
    assert!(!raw_connect(&a, target, &game, &k2, "Grief2", 2).connected, "a new key right after a block connected");
    p.friends.add(&k3.fingerprint(), "pal").unwrap();
    assert!(raw_connect(&a, target, &game, &k3, "Pal", 5).connected, "a friend was held off");
    let logs = LOGS.lock().unwrap().join("\n");
    assert!(logs.contains("from the address of a player blocked lately"), "no cooldown line");
    a.shutdown();
}

#[test]
fn friends_only_lets_only_friends_in() {
    let game = format!("test:public-friends:{}", std::process::id());
    let pa = base_port(16);
    let mut cfg = public_cfg("Ann", pa, None, Vec::new());
    let mut p = cfg.public.clone().unwrap();
    p.friends_only = true;
    cfg.public = Some(p.clone());
    let a = start(&game, "a", base_port(17), cfg);
    let target: SocketAddr = format!("127.0.0.1:{pa}").parse().unwrap();
    let (stranger, pal) = (public::IdentityKey::generate(), public::IdentityKey::generate());
    p.friends.add(&pal.fingerprint(), "pal").unwrap();
    assert!(!raw_connect(&a, target, &game, &stranger, "Stranger", 2).connected, "a stranger got into friends-only");
    assert!(raw_connect(&a, target, &game, &pal, "Pal", 5).connected, "a friend was refused");
    a.shutdown();
}

/// Inside a live, verified public session the peer sends junk frames (sealed properly,
/// so they get past the crypto): random types, lengths, sequence numbers, acks,
/// datagrams. The engine ends or keeps the session, never falls over, and still takes
/// new players afterwards.
#[test]
fn malformed_frames_in_a_live_public_session() {
    let game = format!("test:public-fuzz:{}", std::process::id());
    let pa = base_port(18);
    let a = start(&game, "a", base_port(19), public_cfg("Ann", pa, None, Vec::new()));
    let target: SocketAddr = format!("127.0.0.1:{pa}").parse().unwrap();
    let room = RoomCode::public();
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for round in 0..8 {
        let key = public::IdentityKey::generate();
        let r = raw_connect(&a, target, &game, &key, &format!("Fuzz{round}"), 5);
        assert!(r.connected, "round {round}: no session to fuzz");
        for c in 0..300u64 {
            let n = (next() % 1400) as usize;
            let mut plain: Vec<u8> = (0..n).map(|_| next() as u8).collect();
            // Mostly plausible frame types, with lengths that lie.
            if let Some(f) = plain.first_mut() {
                *f = (next() % 8) as u8;
            }
            if plain.len() > 7 && next() % 2 == 0 {
                let lie = (next() % 70_000) as u16;
                plain[5..7].copy_from_slice(&lie.to_le_bytes());
            }
            let counter = if next() % 10 == 0 { u64::MAX - (next() % 3) } else { c + 1 };
            if let Some(p) = inet::testing::data(&room, &r.eph, &r.theirs, counter, &plain) {
                let _ = r.sock.send_to(&p, target);
            }
            if c % 50 == 0 {
                a.tick();
                a.poll();
            }
        }
        a.tick();
        a.poll();
    }
    std::thread::sleep(Duration::from_millis(200));
    a.tick();
    a.poll();
    // Still standing, and still lets a well-behaved player in.
    let ok = raw_connect(&a, target, &game, &public::IdentityKey::generate(), "Fine", 5);
    assert!(ok.connected, "{:#?}", a.public_status());
    a.shutdown();
}

/// A verified public player that turns hostile once on the mesh: after its hello it
/// streams lobby messages aimed at someone else's lobby (forged owner snapshots, closes,
/// kicks, huge attributes and presence, channel traffic for sockets nobody opened, a
/// second hello), mostly mutated but still decoding, then a frame that doesn't decode.
/// The lobby keeps its owner and data, the engine keeps going, and takes new players.
#[test]
fn hostile_mesh_messages_in_a_live_public_session() {
    let game = format!("test:public-hostile:{}", std::process::id());
    let pa = base_port(24);
    let a = start(&game, "a", base_port(25), public_cfg("Ann", pa, None, Vec::new()));
    let target: SocketAddr = format!("127.0.0.1:{pa}").parse().unwrap();
    let room = RoomCode::public();
    let mut spec = Lobby::new("eos.lobby");
    spec.attrs = vec![Attr { key: "host".into(), value: Value::Str("mine".into()), public: true }];
    let victim = a.create_lobby(spec).unwrap();
    let mut joined_any = false;
    for round in 0..4u64 {
        let r = raw_connect(&a, target, &game, &public::IdentityKey::generate(), &format!("Hostile{round}"), 5);
        assert!(r.connected, "round {round}: no session");
        let stream = inet::testing::hostile_mesh_stream(r.id, &game, a.me(), &victim.id, 0xC0FFEE + round, 400);
        for (seq, chunk) in stream.chunks(1100).enumerate() {
            let p = inet::testing::segment(&room, &r.eph, &r.theirs, seq as u64 + 1, seq as u32, chunk).unwrap();
            let _ = r.sock.send_to(&p, target);
            if seq % 16 == 15 {
                a.tick();
                joined_any |= a.poll().contains(&Event::PeerJoined(r.id));
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        let line = format!("dropped a malformed frame from {}", r.id);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !LOGS.lock().unwrap().iter().any(|l| l.contains(&line)) {
            a.tick();
            joined_any |= a.poll().contains(&Event::PeerJoined(r.id));
            assert!(Instant::now() < deadline, "round {round}: the stream never reached its end");
            std::thread::sleep(Duration::from_millis(20));
        }
        let l = a.lobby(&victim.id).expect("the lobby is gone");
        assert_eq!(l.owner, a.me(), "round {round}: the owner changed");
        assert_eq!(l.attrs.iter().find(|x| x.key == "host").map(|x| x.value.clone()), Some(Value::Str("mine".into())));
        assert!(!l.members.iter().any(|m| m.id == r.id), "round {round}: a stranger became a member");
    }
    assert!(joined_any, "the hostile players never reached the mesh");
    let ok = raw_connect(&a, target, &game, &public::IdentityKey::generate(), "Fine", 5);
    assert!(ok.connected, "{:#?}", a.public_status());
    a.shutdown();
}

/// What a game typing an address reaches over a public session: each API hands out the
/// host's stand-in (EOS host address, Steam server list and join-by-IP, Unity Relay
/// join, Photon all use `game_address`), and TCP and UDP to it reach the host's game,
/// which can tell which player a forwarded connection is from.
#[test]
fn typed_ip_games_reach_a_public_player() {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
    use peerparty_core::fwd::Forwarder;
    let node = Node::start();
    let game = format!("test:public-typed:{}", std::process::id());
    let a = start(&game, "a", base_port(26), public_cfg("Ann", base_port(27), Some(node.addr), Vec::new()));
    let b = start(&game, "b", base_port(28), public_cfg("Ben", base_port(29), Some(node.addr), Vec::new()));
    let lans = [&a, &b];
    let mut seen = vec![Vec::new(), Vec::new()];
    let (ia, ib) = (a.me(), b.me());
    wait_all(&lans, &mut seen, 60, |s| joined(&s[0], ib) && joined(&s[1], ia));
    let (sa, sb) = (Forwarder::address_of(ia), Forwarder::address_of(ib));
    let at = std::net::IpAddr::V4(sa);
    assert_eq!(b.peer_direct_ip(ia), None);
    assert_eq!(b.game_address(ia), Some(at));
    assert_eq!(b.peer_ip(ia), Some(at));
    assert!(b.peer_has_ip(ia, at));
    assert!(!b.peer_has_ip(ia, std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)));

    let fa = Forwarder::new(a.clone(), None, log);
    let fb = Forwarder::new(b.clone(), None, log);
    fa.ensure_pump();
    fb.ensure_pump();

    // TCP: A's game server; B's game connects to A's stand-in.
    let server = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let sport = server.local_addr().unwrap().port();
    let fa2 = fa.clone();
    let echo = std::thread::spawn(move || {
        let (mut s, from) = server.accept().unwrap();
        let mut buf = [0u8; 5];
        s.read_exact(&mut buf).unwrap();
        let who = fa2.forwarded_peer(from);
        s.write_all(&buf).unwrap();
        who
    });
    let to = fb.tcp_dest(SocketAddrV4::new(sa, sport)).expect("a stand-in");
    let mut c = TcpStream::connect(to).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    c.write_all(b"hello").unwrap();
    let mut back = [0u8; 5];
    c.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"hello");
    assert_eq!(echo.join().unwrap(), Some(ib), "the server couldn't tell who connected");

    // UDP: B's game sends to A's stand-in; A's game sees B at its stand-in and answers.
    let ga = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let gb = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    ga.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    gb.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    let (pp, pb) = (ga.local_addr().unwrap().port(), gb.local_addr().unwrap().port());
    let mut buf = [0u8; 2048];
    let deadline = Instant::now() + Duration::from_secs(15);
    let alias = loop {
        let via = fb.udp_dest(SocketAddrV4::new(sa, pp), pb).unwrap();
        gb.send_to(b"ping", via).unwrap();
        if let Ok((n, from)) = ga.recv_from(&mut buf) {
            assert_eq!(&buf[..n], b"ping");
            break from;
        }
        assert!(Instant::now() < deadline, "the datagram never arrived");
    };
    let SocketAddr::V4(alias4) = alias else { panic!("not v4") };
    assert_eq!(fa.udp_from(alias4), Some(SocketAddrV4::new(sb, pb)));
    assert_eq!(fa.forwarded_peer(alias), Some(ib));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        ga.send_to(b"pong", alias).unwrap();
        if let Ok((n, _)) = gb.recv_from(&mut buf) {
            assert_eq!(&buf[..n], b"pong");
            break;
        }
        assert!(Instant::now() < deadline, "the reply never arrived");
    }
    let logs = LOGS.lock().unwrap().join("\n");
    for p in [base_port(27), base_port(29)] {
        assert!(!logs.contains(&format!("127.0.0.1:{p}")), "a public address in the log");
    }
    for l in lans {
        l.shutdown();
    }
}

/// What a game puts in lobby and member attributes, presence and invites reaches a public
/// player with every real address (LAN or internet, v4 or v6) swapped for the sender's
/// stand-in, ports kept, versions left alone; the stand-in is the one the receiver's
/// games reach the sender at.
#[test]
fn public_lobby_data_never_carries_an_address() {
    let node = Node::start();
    let game = format!("test:public-scrub:{}", std::process::id());
    let a = start(&game, "a", base_port(20), public_cfg("Ann", base_port(21), Some(node.addr), Vec::new()));
    let b = start(&game, "b", base_port(22), public_cfg("Ben", base_port(23), Some(node.addr), Vec::new()));
    let lans = [&a, &b];
    let mut seen = vec![Vec::new(), Vec::new()];
    let (ia, ib) = (a.me(), b.me());
    wait_all(&lans, &mut seen, 60, |s| joined(&s[0], ib) && joined(&s[1], ia));
    let a_at = b.peer_ip(ia).unwrap().to_string();
    assert!(is_stand_in(b.peer_ip(ia).unwrap()));

    a.set_presence(vec![
        ("connect".into(), "+connect 192.168.1.20:27015".into()),
        ("status".into(), "In a match on 1.0.7.2".into()),
    ]);
    let attr = |k: &str, v: &str| Attr { key: k.into(), value: Value::Str(v.into()), public: true };
    let mut spec = Lobby::new("eos.lobby");
    spec.bucket = "scrub".into();
    spec.attrs = vec![attr("host", "203.0.113.5:7777"), attr("lan", "10.0.0.4"), attr("v6", "[2001:db8::7]:7777"), attr("version", "1.0.7.2")];
    let hosted = a.create_lobby(spec).unwrap();
    a.invite(Some(ib), "+connect 10.0.0.4:27015", Some(&hosted));

    let mut q = Query::new("eos.lobby");
    q.filters.push(Filter::Bucket { op: Op::Eq, value: "scrub".into() });
    let search = b.search(q);
    wait_all(&lans, &mut seen, 15, |s| {
        s[1].iter().any(|e| matches!(e, Event::SearchDone { search: x, .. } if *x == search))
            && s[1].iter().any(|e| matches!(e, Event::Invite { .. }))
            && !b.presence(ia).is_empty()
    });
    let results = seen[1]
        .iter()
        .find_map(|e| match e {
            Event::SearchDone { search: x, results } if *x == search => Some(results.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(results.len(), 1, "{results:#?}");
    let get = |k: &str| results[0].attrs.iter().find(|x| x.key == k).map(|x| x.value.clone());
    assert_eq!(get("host"), Some(Value::Str(format!("{a_at}:7777"))));
    assert_eq!(get("lan"), Some(Value::Str(a_at.clone())));
    assert_eq!(get("v6"), Some(Value::Str(format!("{a_at}:7777"))));
    assert_eq!(get("version"), Some(Value::Str("1.0.7.2".into())));
    let pres = b.presence(ia);
    assert!(pres.contains(&("connect".into(), format!("+connect {a_at}:27015"))), "{pres:?}");
    assert!(pres.contains(&("status".into(), "In a match on 1.0.7.2".into())), "{pres:?}");
    let inv = seen[1].iter().find(|e| matches!(e, Event::Invite { .. })).unwrap();
    let Event::Invite { connect, lobby, .. } = inv else { unreachable!() };
    assert_eq!(connect, &format!("+connect {a_at}:27015"));
    let shown = format!("{lobby:?}");
    assert!(!shown.contains("203.0.113.5") && !shown.contains("10.0.0.4") && !shown.contains("2001:db8"), "{shown}");
    // The host's own copy is untouched: only what crosses to strangers changes.
    assert_eq!(a.lobby(&hosted.id).unwrap().attrs[0].value, Value::Str("203.0.113.5:7777".into()));
    for l in lans {
        l.shutdown();
    }
}

/// Two public players on this PC meeting through the real public DHT under a random game
/// name (so no stranger shares the topic). Needs `PEERPARTY_LOOPBACK=0`. Private addresses
/// are allowed so the copy on this PC is reached when the router doesn't hairpin.
#[test]
#[ignore = "needs the internet"]
fn real_dht_two_public_players_on_this_pc() {
    assert_eq!(std::env::var("PEERPARTY_LOOPBACK").as_deref(), Ok("0"), "run with PEERPARTY_LOOPBACK=0");
    let game = format!("test:public-real:{}:{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    let cfg = |name: &str| {
        let mut c = inet::Config::public(public::PublicCfg::ephemeral(name));
        c.node_cache = None;
        c.allow_private = true;
        c
    };
    let a = start(&game, "a", base_port(6), cfg("RealA"));
    let b = start(&game, "b", base_port(7), cfg("RealB"));
    let lans = [&a, &b];
    let mut seen = vec![Vec::new(), Vec::new()];
    let (ia, ib) = (a.me(), b.me());
    wait_all(&lans, &mut seen, 150, |s| joined(&s[0], ib) && joined(&s[1], ia));
    assert_eq!(a.peer_trust(ib), Some(Trust::Public));
    assert!(is_stand_in(a.peer_ip(ib).unwrap()));
    let st = format!("{:?}", a.public_status().unwrap());
    assert!(!redact::has_address(&st), "{st}");
    println!("{st}");
    for l in LOGS.lock().unwrap().iter().filter(|l| l.contains("connected to public player")) {
        println!("{l}");
    }
    a.shutdown();
    b.shutdown();
}

/// Pumps every copy, accepting any channel asked for, until a packet on `socket`
/// reaches `to`; its data.
fn arrives(lans: &[&Lan], to: usize, socket: &str, secs: u64) -> Option<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        for l in lans {
            l.tick();
            for e in l.poll() {
                if let Event::ChanRequest { peer, socket } = e {
                    l.accept(peer, &socket);
                }
            }
        }
        if let Some(p) = lans[to].receive_where(None, |s| s == socket) {
            return Some(p.data);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

/// PlayFab Party game data (`party:<guid>`) is game traffic: it flows with public
/// players both ways, whatever the voice setting. Voice (`rtc:`, PlayFab Party / Xbox
/// chat included) flows by default, and not at all once voice with public players is
/// off (`PEERPARTY_PUBLIC_VOICE=0`, the launcher's setting).
#[test]
fn public_players_get_party_game_data_and_voice_unless_voice_is_off() {
    let game = format!("test:public-party:{}", std::process::id());
    let (pa, pb) = (base_port(30), base_port(31));
    let a = start(&game, "a", base_port(32), public_cfg("Ann", pa, None, vec![format!("127.0.0.1:{pb}")]));
    let b = start(&game, "b", base_port(33), public_cfg("Ben", pb, None, vec![format!("127.0.0.1:{pa}")]));
    let lans = [&a, &b];
    let mut seen = vec![Vec::new(), Vec::new()];
    let (ida, idb) = (a.me(), b.me());
    wait_all(&lans, &mut seen, 30, |s| joined(&s[0], idb) && joined(&s[1], ida));
    assert_eq!(a.peer_trust(idb), Some(Trust::Public));
    assert!(a.public_voice() && b.public_voice(), "voice with public players is on by default");

    // Party game data, both ways (reliable and not).
    let game_sock = "party:6a1f2c3d-0000-4000-8000-000000000001";
    assert_ne!(a.send(idb, game_sock, 0, b"\x01host says hi", true, true, true), SendResult::Dropped);
    assert_eq!(arrives(&lans, 1, game_sock, 10).as_deref(), Some(&b"\x01host says hi"[..]));
    assert_ne!(b.send(ida, game_sock, 0, b"\x01client says hi", true, true, true), SendResult::Dropped);
    assert_eq!(arrives(&lans, 0, game_sock, 10).as_deref(), Some(&b"\x01client says hi"[..]));
    assert_eq!(b.send(ida, game_sock, 0, b"\x00datagram", false, true, true), SendResult::Sent);
    assert_eq!(arrives(&lans, 0, game_sock, 10).as_deref(), Some(&b"\x00datagram"[..]));

    // Voice (PlayFab Party chat's room), by default.
    let voice_sock = "rtc:party:6a1f2c3d-0000-4000-8000-000000000001";
    assert_ne!(a.send(idb, voice_sock, 1, b"audio", true, true, true), SendResult::Dropped);
    assert_eq!(arrives(&lans, 1, voice_sock, 10).as_deref(), Some(&b"audio"[..]));

    // Voice off at B: B sends none and takes none, on open channels or new ones.
    b.set_public_voice(false);
    assert_eq!(b.send(ida, voice_sock, 1, b"mine", true, true, true), SendResult::Dropped);
    assert_eq!(b.send(ida, "rtc:lobby:peerparty-voice", 1, b"mine", true, true, true), SendResult::Dropped);
    assert_eq!(a.send(idb, voice_sock, 1, b"more audio", true, true, true), SendResult::Sent);
    let _ = a.send(idb, "rtc:other-room", 1, b"new room", true, true, true);
    assert_eq!(arrives(&lans, 1, voice_sock, 3), None, "voice got through with voice off");
    assert_eq!(arrives(&lans, 1, "rtc:other-room", 1), None, "voice got through with voice off");
    // ... while game data still flows both ways.
    assert_eq!(a.send(idb, game_sock, 0, b"\x01still here", true, true, true), SendResult::Sent);
    assert_eq!(arrives(&lans, 1, game_sock, 10).as_deref(), Some(&b"\x01still here"[..]));
    assert_eq!(b.send(ida, game_sock, 0, b"\x01me too", true, true, true), SendResult::Sent);
    assert_eq!(arrives(&lans, 0, game_sock, 10).as_deref(), Some(&b"\x01me too"[..]));
    // Each drop is logged, once per player and socket.
    let logs = LOGS.lock().unwrap();
    let dropped = |sock: &str| logs.iter().filter(|l| l.contains(sock) && l.contains("voice chat with public players is off")).count();
    assert!(dropped(voice_sock) >= 1 && dropped(voice_sock) <= 2, "{:?}", logs.iter().filter(|l| l.contains("rtc:")).collect::<Vec<_>>());
    drop(logs);
    for l in lans {
        l.shutdown();
    }
}

/// Stale addresses (a full set of them that never answer, like old DHT finds) can't fill
/// the session table and lock out a live player: B punches 30 dead addresses, then A
/// punches B and the two meet within seconds. Before, B's 16 sessions were all taken by
/// the dead punches for 40 s at a time and A's handshake was dropped without a word.
#[test]
fn many_dead_addresses_dont_lock_out_a_live_player() {
    let game = format!("test:public:stale:{}", std::process::id());
    // Bound but silent, so no "port unreachable" ends the punches early.
    let dead: Vec<UdpSocket> = (0..30).map(|_| UdpSocket::bind("127.0.0.1:0").unwrap()).collect();
    let dead_addrs: Vec<String> = dead.iter().map(|s| s.local_addr().unwrap().to_string()).collect();
    let (pa, pb) = (base_port(34), base_port(35));
    let b = start(&game, "b", base_port(36), public_cfg("Bob", pb, None, dead_addrs));
    let mut seen = vec![Vec::new(), Vec::new()];
    // Let B start punching the dead addresses first.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        pump(&[&b], &mut seen[1..]);
        std::thread::sleep(Duration::from_millis(20));
    }
    let a = start(&game, "a", base_port(37), public_cfg("Alice", pa, None, vec![format!("127.0.0.1:{pb}")]));
    let (ia, ib) = (a.me(), b.me());
    wait_all(&[&a, &b], &mut seen, 12, |s| joined(&s[0], ib) && joined(&s[1], ia));
    let st = b.public_status().unwrap();
    assert!(st.peers.len() <= 16, "{st:#?}");
    assert!(st.peers.iter().any(|p| p.connected), "{st:#?}");
    let logs = LOGS.lock().unwrap();
    assert!(!logs.iter().any(|l| l.contains("sessions are in use")), "a handshake was turned away");
    drop(dead);
}

/// A player whose game restarts (same key and id, new port) while its old session has
/// gone quiet replaces that session instead of waiting out the 30 s timeout; a second
/// copy while the old session is still fresh is turned away.
#[test]
fn a_restarted_player_replaces_its_quiet_old_session() {
    // Production waits 10s of silence. The same rule, on a shorter interval.
    let quiet = Duration::from_millis(500);
    let _timing = TestTiming::install(TestIntervals { replace_quiet: Some(quiet), ..TestIntervals::NONE });
    let game = format!("test:public:restart:{}", std::process::id());
    let pa = base_port(38);
    let a = start(&game, "a", base_port(39), public_cfg("Ann", pa, None, Vec::new()));
    let target: SocketAddr = format!("127.0.0.1:{pa}").parse().unwrap();
    let key = public::IdentityKey::generate();
    let id = PeerId(crypto::random_array());
    let has = |s: &str| LOGS.lock().unwrap().iter().any(|l| l.contains(s));
    let wait_log = |s: &str, secs: u64| {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !has(s) && Instant::now() < deadline {
            a.tick();
            a.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
        has(s)
    };
    let first = raw_connect_as(&a, target, &game, &key, id, "Relaunch", 5);
    assert!(first.connected, "{:#?}", a.public_status());
    // At once: the old session is fresh, so the new copy is turned away.
    let _second = raw_connect_as(&a, target, &game, &key, id, "Relaunch", 1);
    assert!(wait_log("is Relaunch again (already connected", 5), "a second copy wasn't turned away");
    assert!(!has("is Relaunch again; its old session"), "a fresh session was replaced");
    // The first copy says nothing more (its game is gone); once it has been quiet past
    // the limit, a third copy takes its place.
    let t0 = Instant::now();
    while t0.elapsed() < quiet + Duration::from_millis(200) {
        a.tick();
        a.poll();
        std::thread::sleep(Duration::from_millis(20));
    }
    let _third = raw_connect_as(&a, target, &game, &key, id, "Relaunch", 2);
    assert!(wait_log("is Relaunch again; its old session", 5), "the quiet old session wasn't replaced");
    let fp = public::fingerprint(&key.public());
    let st = a.public_status().unwrap();
    let n = st.peers.iter().filter(|p| p.connected && p.key.as_deref() == Some(&fp)).count();
    assert_eq!(n, 1, "{st:#?}");
    a.shutdown();
}
