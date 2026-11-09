//! The internet switch (`Lan::set_internet`, the overlay's Internet page): off stops the
//! room and public sessions and drops the players they brought while LAN players stay;
//! on brings them back. Over loopback (PEERPARTY_LOOPBACK=1), LAN discovery off for the
//! room tests, so the tunnel is the only way two copies meet.

use std::time::{Duration, Instant};

use peerparty_core::inet::{self, RoomCode};
use peerparty_core::*;

fn noisy(s: &str) {
    if std::env::var_os("PEERPARTY_TEST_LOG").is_some() {
        eprintln!("{s}");
    }
}

fn base_port(offset: u16) -> u16 {
    // Clear of the other test files' ranges (20000.., 30000.., 40000.., 47600.., 50000..64000).
    // 10000..: 16 ports for each of 600 process ids.
    10000 + (std::process::id() % 600) as u16 * 16 + offset
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

fn start(game: &str, name: &str, disc: u16, room: Option<inet::Config>, lan: bool, internet_on: bool) -> Lan {
    Lan::start_with(
        Options {
            game: game.into(),
            identity: Identity::ephemeral(name),
            discovery_port: disc,
            extra_targets: Vec::new(),
            log: noisy,
            internet: room,
            public: None,
            lan_discovery: lan,
        },
        internet_on,
    )
    .expect("start")
}

fn pump_until(a: &Lan, ea: &mut Vec<Event>, b: &Lan, eb: &mut Vec<Event>, secs: u64, what: &str, done: impl Fn(&Lan, &Lan, &[Event], &[Event]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done(a, b, ea, eb) {
        a.tick();
        b.tick();
        ea.extend(a.poll());
        eb.extend(b.poll());
        assert!(Instant::now() < deadline, "timed out waiting for {what}; a {ea:#?}; b {eb:#?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn has(e: &[Event], want: Event) -> bool {
    e.contains(&want)
}

#[test]
fn switching_internet_off_drops_room_players_and_on_brings_them_back() {
    let game = format!("test:switch:{}", std::process::id());
    let room = RoomCode::generate();
    let (pa, pb) = (base_port(0), base_port(1));
    let a = start(&game, "a", base_port(2), Some(room_cfg(&room, pa, vec![format!("127.0.0.1:{pb}")])), false, true);
    let b = start(&game, "b", base_port(3), Some(room_cfg(&room, pb, Vec::new())), false, true);
    assert!(a.internet_configured() && a.internet_on());
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    pump_until(&a, &mut ea, &b, &mut eb, 20, "the room to connect", |a, b, ea, eb| {
        has(ea, Event::PeerJoined(b.me())) && has(eb, Event::PeerJoined(a.me()))
    });
    assert!(a.peer_is_internet(b.me()));

    // Off: the room stops and b leaves, on both sides.
    assert!(a.set_internet(false));
    assert!(!a.set_internet(false), "already off");
    assert!(!a.internet_on());
    assert!(a.internet_status().is_none(), "no room running while off");
    assert!(a.internet_configured(), "still set up");
    pump_until(&a, &mut ea, &b, &mut eb, 20, "the room players to leave", |a, b, ea, eb| {
        has(ea, Event::PeerLeft(b.me())) && has(eb, Event::PeerLeft(a.me()))
    });
    assert!(a.peers().is_empty(), "{:?}", a.peers());

    // On: the room starts again on the same port and they meet again.
    ea.clear();
    eb.clear();
    assert!(a.set_internet(true));
    assert!(a.internet_on());
    assert_eq!(a.internet_status().map(|s| s.local_port), Some(pa), "same port as before");
    pump_until(&a, &mut ea, &b, &mut eb, 30, "the room to reconnect", |a, b, ea, eb| {
        has(ea, Event::PeerJoined(b.me())) && has(eb, Event::PeerJoined(a.me()))
    });
    assert!(a.peer_is_internet(b.me()));
    a.shutdown();
    b.shutdown();
}

#[test]
fn switching_internet_off_keeps_lan_players() {
    let game = format!("test:switch-lan:{}", std::process::id());
    let room = RoomCode::generate();
    let disc = base_port(6);
    // Both on the LAN (the same-PC board, over loopback) and in a room with no way to
    // reach each other over it (no static peers, no DHT): they meet on the LAN.
    let a = start(&game, "a", disc, Some(room_cfg(&room, base_port(4), Vec::new())), true, true);
    let b = start(&game, "b", disc, Some(room_cfg(&room, base_port(5), Vec::new())), true, true);
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    pump_until(&a, &mut ea, &b, &mut eb, 20, "the LAN to connect", |a, b, ea, eb| {
        has(ea, Event::PeerJoined(b.me())) && has(eb, Event::PeerJoined(a.me()))
    });
    assert!(!a.peer_is_internet(b.me()));
    assert!(a.set_internet(false));
    assert!(b.set_internet(false));
    // A few seconds on, nobody left.
    let until = Instant::now() + Duration::from_secs(3);
    pump_until(&a, &mut ea, &b, &mut eb, 10, "a few seconds", |_, _, _, _| Instant::now() >= until);
    assert!(!has(&ea, Event::PeerLeft(b.me())) && !has(&eb, Event::PeerLeft(a.me())), "a {ea:#?}; b {eb:#?}");
    assert_eq!(a.peers().len(), 1);
    assert!(a.internet_status().is_none() && b.internet_status().is_none());
    a.shutdown();
    b.shutdown();
}

#[test]
fn starts_switched_off_and_switches_on_later() {
    let game = format!("test:switch-paused:{}", std::process::id());
    let room = RoomCode::generate();
    let (pa, pb) = (base_port(7), base_port(8));
    let a = start(&game, "a", base_port(9), Some(room_cfg(&room, pa, vec![format!("127.0.0.1:{pb}")])), false, false);
    let b = start(&game, "b", base_port(10), Some(room_cfg(&room, pb, Vec::new())), false, true);
    assert!(a.internet_configured() && !a.internet_on());
    assert!(a.internet_status().is_none(), "nothing running while switched off");
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    let until = Instant::now() + Duration::from_secs(2);
    pump_until(&a, &mut ea, &b, &mut eb, 10, "a couple of seconds", |_, _, _, _| Instant::now() >= until);
    assert!(a.peers().is_empty() && b.peers().is_empty(), "met while switched off");
    assert!(a.set_internet(true));
    pump_until(&a, &mut ea, &b, &mut eb, 20, "the room to connect", |a, b, ea, eb| {
        has(ea, Event::PeerJoined(b.me())) && has(eb, Event::PeerJoined(a.me()))
    });
    a.shutdown();
    b.shutdown();
}

#[test]
fn nothing_to_switch_without_a_room_or_public_sessions() {
    let game = format!("test:switch-none:{}", std::process::id());
    let a = start(&game, "a", base_port(11), None, false, true);
    assert!(!a.internet_configured());
    assert!(a.set_internet(false) && a.set_internet(true));
    assert!(a.internet_status().is_none() && a.public_status().is_none());
    a.shutdown();
}
