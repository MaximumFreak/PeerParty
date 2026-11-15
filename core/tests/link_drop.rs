//! A dropped connection doesn't make a player leave: back within the grace, it picks up
//! where it left, its lobby whole and what was sent meanwhile delivered once and in
//! order. Not back in time, it leaves; quitting, it leaves at once. Three engines in one
//! process, meeting on the same-PC board and a shared discovery port.

use std::time::{Duration, Instant};

use peerparty_core::*;

fn quiet(s: &str) {
    if std::env::var_os("PEERPARTY_TEST_LOG").is_some() {
        eprintln!("{s}");
    }
}

fn port(offset: u16) -> u16 {
    // Clear of the other test files' discovery ports (47300.. and up, 10000..39999).
    // 46000..46899; the version test has 45600..45899.
    46000 + (std::process::id() % 300) as u16 * 3 + offset
}

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

/// Ticks every engine and keeps its events, until `done` or `secs` pass.
fn pump(lans: &[&Lan], seen: &mut [Vec<Event>], secs: u64, what: &str, done: impl Fn(&[Vec<Event>]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done(seen) {
        tick(lans, seen);
        assert!(Instant::now() < deadline, "timed out waiting for {what}: {seen:#?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One frame of every engine: timers (acknowledgements, resend requests) and events.
fn tick(lans: &[&Lan], seen: &mut [Vec<Event>]) {
    for (l, e) in lans.iter().zip(seen.iter_mut()) {
        l.tick();
        for ev in l.poll() {
            if let Event::ChanRequest { peer, socket } = &ev {
                l.accept(*peer, socket);
            }
            e.push(ev);
        }
    }
}

fn has(e: &[Event], want: &Event) -> bool {
    e.contains(want)
}

fn left(e: &[Event]) -> Vec<&Event> {
    e.iter()
        .filter(|x| {
            matches!(
                x,
                Event::PeerLeft(_)
                    | Event::ChanClosed { .. }
                    | Event::MemberStatus { status: MemberStatus::Left | MemberStatus::Disconnected | MemberStatus::Closed | MemberStatus::Promoted, .. }
            )
        })
        .collect()
}

/// Three players meet, A hosts a lobby B and C are in, and B has a channel to A.
fn three_in_a_lobby(tag: &str) -> (Lan, Lan, Lan, Vec<Vec<Event>>, String) {
    let game = format!("test:link-{tag}:{}", std::process::id());
    let disc = port(match tag {
        "resume" => 0,
        "expire" => 1,
        "sever" => 300,
        "ownergone" => 301,
        "relay" => 302,
        "unnoticed-lower" => 303,
        "unnoticed-higher" => 304,
        _ => 2,
    });
    let (a, b, c) = (start(&game, disc, "a"), start(&game, disc, "b"), start(&game, disc, "c"));
    let mut seen = vec![Vec::new(), Vec::new(), Vec::new()];
    let ids = [a.me(), b.me(), c.me()];
    pump(&[&a, &b, &c], &mut seen, 20, "the three to meet", |s| {
        (0..3).all(|i| (0..3).filter(|j| *j != i).all(|j| has(&s[i], &Event::PeerJoined(ids[j]))))
    });
    let mut spec = Lobby::new("test.lobby");
    spec.max_members = 4;
    let lobby = a.create_lobby(spec).unwrap();
    b.join(&lobby).unwrap();
    c.join(&lobby).unwrap();
    let done = Event::JoinDone { lobby: lobby.id.clone(), result: JoinResult::Ok };
    pump(&[&a, &b, &c], &mut seen, 10, "the joins", |s| has(&s[1], &done) && has(&s[2], &done));
    pump(&[&a, &b, &c], &mut seen, 10, "everyone to see three members", |_| {
        [&a, &b, &c].iter().all(|l| l.lobby(&lobby.id).is_some_and(|x| x.members.len() == 3))
    });
    assert_eq!(b.send(a.me(), "game", 0, b"open", true, true, true), SendResult::Held);
    let open = Event::ChanOpen { peer: a.me(), socket: "game".into() };
    pump(&[&a, &b, &c], &mut seen, 10, "the channel", |s| has(&s[1], &open));
    let got = receive_all(&a, &b, &c, &mut seen, 1);
    assert_eq!(got, vec![b"open".to_vec()]);
    (a, b, c, seen, lobby.id)
}

/// What came to A on "game" from B, until `n` messages have. Ticks as it waits: a gap
/// left by a retired connection is only asked for again from `tick`.
fn receive_all(a: &Lan, b: &Lan, c: &Lan, seen: &mut [Vec<Event>], n: usize) -> Vec<Vec<u8>> {
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while got.len() < n {
        tick(&[a, b, c], seen);
        while let Some(p) = a.receive_where(None, |s| s == "game") {
            assert_eq!(p.peer, b.me());
            got.push(p.data);
        }
        assert!(Instant::now() < deadline, "only {} of {n} arrived", got.len());
        std::thread::sleep(Duration::from_millis(10));
    }
    got
}

#[test]
fn a_dropped_connection_picks_up_where_it_left() {
    let (a, b, c, mut seen, lobby) = three_in_a_lobby("resume");
    for s in seen.iter_mut() {
        s.clear();
    }
    // Messages before, during and after the break, each a number.
    let mut sent = 0u32;
    let mut send = |n: u32| {
        for _ in 0..n {
            assert_ne!(b.send(a.me(), "game", 0, &sent.to_le_bytes(), true, true, true), SendResult::Dropped);
            sent += 1;
        }
    };
    send(50);
    b.cut_connection(a.me());
    send(50);
    // The owner changes the lobby meanwhile; B hears it once it's back.
    let mut edited = a.lobby(&lobby).unwrap();
    edited.attrs.push(Attr { key: "map".into(), value: Value::Str("two".into()), public: true });
    a.update(&lobby, Some(&edited), &[], &[]).unwrap();
    pump(&[&a, &b, &c], &mut seen, 20, "A and B to reconnect", |_| a.peer_trust(b.me()).is_some() && b.peer_trust(a.me()).is_some());
    send(50);
    let got = receive_all(&a, &b, &c, &mut seen, 150);
    let numbers: Vec<u32> = got.iter().map(|d| u32::from_le_bytes(d[..4].try_into().unwrap())).collect();
    assert_eq!(numbers, (0..150).collect::<Vec<_>>(), "every message once, in order");
    pump(&[&a, &b, &c], &mut seen, 10, "B to see the change", |_| {
        b.lobby(&lobby).is_some_and(|l| l.attr("map").is_some_and(|x| x.value == Value::Str("two".into())))
    });
    // A while longer: nothing late.
    let until = Instant::now() + Duration::from_secs(2);
    pump(&[&a, &b, &c], &mut seen, 10, "a moment", |_| Instant::now() >= until);
    for (name, e) in ["a", "b", "c"].iter().zip(&seen) {
        assert!(left(e).is_empty(), "{name} saw someone leave: {e:#?}");
    }
    for l in [&a, &b, &c] {
        let x = l.lobby(&lobby).expect("still in the lobby");
        assert_eq!(x.owner, a.me());
        assert_eq!(x.members.len(), 3);
    }
    assert!(a.receive_where(None, |s| s == "game").is_none(), "a duplicate");
    for l in [&a, &b, &c] {
        l.shutdown();
    }
}

#[test]
fn a_player_not_back_in_time_leaves_and_the_lobby_stays_one() {
    let _timing = TestTiming::install(TestIntervals { link_grace: Some(Duration::from_secs(2)), ..TestIntervals::NONE });
    let (a, b, c, mut seen, lobby) = three_in_a_lobby("expire");
    for s in seen.iter_mut() {
        s.clear();
    }
    // B's game quits without a word (its process is gone): nothing comes back. Severed,
    // not just cut: a cut B is still alive and on the same-PC board, and may dial back.
    b.set_lan_discovery(false);
    a.set_lan_discovery(false);
    c.set_lan_discovery(false);
    b.sever(a.me());
    b.sever(c.me());
    let t = Instant::now();
    pump(&[&a, &c], &mut seen[..2], 15, "B to leave", |s| has(&s[0], &Event::PeerLeft(b.me())) && has(&s[1], &Event::PeerLeft(b.me())));
    assert!(t.elapsed() >= Duration::from_millis(1500), "left before its grace was up");
    pump(&[&a, &c], &mut seen[..2], 5, "the lobby to lose B", |_| {
        [&a, &c].iter().all(|l| l.lobby(&lobby).is_some_and(|x| x.members.len() == 2 && x.owner == a.me()))
    });
    for l in [&a, &b, &c] {
        l.shutdown();
    }
}

/// A lasting break of one link, B to the owner A, while both still reach C: their
/// connection comes back through C, the session picks up where it left, and the game
/// sees nothing: B stays in the lobby, and what it sends A meanwhile arrives once and in
/// order. When the direct link mends, it takes over again.
#[test]
fn a_member_cut_off_from_the_owner_alone_stays_through_another_member() {
    let _timing = TestTiming::install(TestIntervals {
        link_grace: Some(Duration::from_secs(4)),
        lan_upgrade_every: Some(Duration::from_secs(1)),
        ..TestIntervals::NONE
    });
    let (a, b, c, mut seen, lobby) = three_in_a_lobby("relay");
    for s in seen.iter_mut() {
        s.clear();
    }
    a.sever(b.me());
    b.sever(a.me());
    // Sent while the link is down and while it comes back through C.
    let mut sent = Vec::new();
    let mut got = Vec::new();
    let take = |got: &mut Vec<Vec<u8>>| {
        while let Some(p) = a.receive_where(None, |s| s == "game") {
            assert_eq!(p.peer, b.me());
            got.push(p.data);
        }
    };
    for i in 0..60u32 {
        let msg = format!("m{i}").into_bytes();
        b.send(a.me(), "game", 0, &msg, true, true, true);
        sent.push(msg);
        let until = Instant::now() + Duration::from_millis(50);
        pump(&[&a, &b, &c], &mut seen, 1, "a moment", |_| Instant::now() >= until);
        take(&mut got);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while got.len() < sent.len() && Instant::now() < deadline {
        let until = Instant::now() + Duration::from_millis(50);
        pump(&[&a, &b, &c], &mut seen, 1, "a moment", |_| Instant::now() >= until);
        take(&mut got);
    }
    assert_eq!(got, sent, "B's messages, once each and in order");
    assert!(a.peer_is_relayed(b.me()) && b.peer_is_relayed(a.me()), "A and B connected again, through C");
    // Past the grace: nobody left anything.
    let until = Instant::now() + Duration::from_secs(5);
    pump(&[&a, &b, &c], &mut seen, 10, "a moment", |_| Instant::now() >= until);
    for (name, e) in ["a", "b", "c"].iter().zip(&seen) {
        assert!(left(e).is_empty(), "{name} saw someone leave: {:#?}", left(e));
    }
    for l in [&a, &b, &c] {
        let x = l.lobby(&lobby).expect("still in the lobby");
        assert_eq!((x.owner, x.members.len()), (a.me(), 3));
    }
    assert_eq!(b.send(a.me(), "game", 0, b"after", true, true, true), SendResult::Sent);
    let mut last = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while last.is_empty() && Instant::now() < deadline {
        let until = Instant::now() + Duration::from_millis(50);
        pump(&[&a, &b, &c], &mut seen, 1, "a moment", |_| Instant::now() >= until);
        take(&mut last);
    }
    assert_eq!(last, vec![b"after".to_vec()]);
    // The link mends: the direct path takes over, still without anyone leaving.
    a.mend(b.me());
    b.mend(a.me());
    pump(&[&a, &b, &c], &mut seen, 15, "the direct path back", |_| !a.peer_is_relayed(b.me()) && !b.peer_is_relayed(a.me()));
    assert_eq!(b.send(a.me(), "game", 0, b"direct", true, true, true), SendResult::Sent);
    let mut last = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while last.is_empty() && Instant::now() < deadline {
        let until = Instant::now() + Duration::from_millis(50);
        pump(&[&a, &b, &c], &mut seen, 1, "a moment", |_| Instant::now() >= until);
        take(&mut last);
    }
    assert_eq!(last, vec![b"direct".to_vec()]);
    for (name, e) in ["a", "b", "c"].iter().zip(&seen) {
        assert!(left(e).is_empty(), "{name} saw someone leave: {:#?}", left(e));
    }
    for l in [&a, &b, &c] {
        l.shutdown();
    }
}

/// The link between B and the owner A dies and only one of them notices: the other's
/// connection still looks open. The side that noticed reaches the other through C, and
/// its offer shows the other that its connection is gone too, so the two come back
/// through C before either's grace is up, whichever of the two (lower or higher id)
/// noticed.
fn one_side_notices(lower_notices: bool) {
    let _timing = TestTiming::install(TestIntervals {
        link_grace: Some(Duration::from_secs(4)),
        lan_upgrade_every: Some(Duration::from_secs(1)),
        ..TestIntervals::NONE
    });
    let (a, b, c, mut seen, lobby) = three_in_a_lobby(if lower_notices { "unnoticed-lower" } else { "unnoticed-higher" });
    for s in seen.iter_mut() {
        s.clear();
    }
    let (lo, hi) = if a.me() < b.me() { (&a, &b) } else { (&b, &a) };
    let (notices, unaware) = if lower_notices { (lo, hi) } else { (hi, lo) };
    unaware.sever_unnoticed(notices.me());
    notices.sever(unaware.me());
    let mut sent = Vec::new();
    let mut got = Vec::new();
    let take = |got: &mut Vec<Vec<u8>>| {
        while let Some(p) = a.receive_where(None, |s| s == "game") {
            got.push(p.data);
        }
    };
    for i in 0..30u32 {
        let msg = format!("m{i}").into_bytes();
        b.send(a.me(), "game", 0, &msg, true, true, true);
        sent.push(msg);
        let until = Instant::now() + Duration::from_millis(50);
        pump(&[&a, &b, &c], &mut seen, 1, "a moment", |_| Instant::now() >= until);
        take(&mut got);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while got.len() < sent.len() && Instant::now() < deadline {
        let until = Instant::now() + Duration::from_millis(50);
        pump(&[&a, &b, &c], &mut seen, 1, "a moment", |_| Instant::now() >= until);
        take(&mut got);
    }
    assert_eq!(got, sent, "B's messages, once each and in order");
    assert!(a.peer_is_relayed(b.me()) && b.peer_is_relayed(a.me()), "A and B connected again, through C");
    let until = Instant::now() + Duration::from_secs(5);
    pump(&[&a, &b, &c], &mut seen, 10, "a moment", |_| Instant::now() >= until);
    for (name, e) in ["a", "b", "c"].iter().zip(&seen) {
        assert!(left(e).is_empty(), "{name} saw someone leave: {:#?}", left(e));
    }
    for l in [&a, &b, &c] {
        let x = l.lobby(&lobby).expect("still in the lobby");
        assert_eq!((x.owner, x.members.len()), (a.me(), 3));
    }
    for l in [&a, &b, &c] {
        l.shutdown();
    }
}

#[test]
fn a_link_only_the_lower_id_sees_drop_comes_back_through_another_member() {
    one_side_notices(true);
}

#[test]
fn a_link_only_the_higher_id_sees_drop_comes_back_through_another_member() {
    one_side_notices(false);
}

/// A lasting break of one link, B to the owner A, while both still reach C, and nobody
/// carries a connection between them: once the grace is up A drops B, and B, told
/// through C that A is still there, is out of the lobby rather than making itself (or
/// C) the owner of a second one.
#[test]
fn a_member_cut_off_from_the_owner_alone_leaves_and_the_lobby_stays_one() {
    let _timing = TestTiming::install(TestIntervals { link_grace: Some(Duration::from_secs(2)), relay: Some(false), ..TestIntervals::NONE });
    let (a, b, c, mut seen, lobby) = three_in_a_lobby("sever");
    for s in seen.iter_mut() {
        s.clear();
    }
    a.sever(b.me());
    b.sever(a.me());
    let closed = Event::MemberStatus { lobby: lobby.clone(), member: b.me(), status: MemberStatus::Closed };
    pump(&[&a, &b, &c], &mut seen, 20, "B to be out of the lobby", |s| has(&s[1], &closed));
    pump(&[&a, &b, &c], &mut seen, 5, "A and C to see two members", |_| {
        [&a, &c].iter().all(|l| l.lobby(&lobby).is_some_and(|x| x.members.len() == 2 && x.owner == a.me()))
    });
    // A while longer: no second owner turns up anywhere.
    let until = Instant::now() + Duration::from_secs(5);
    pump(&[&a, &b, &c], &mut seen, 10, "a moment", |_| Instant::now() >= until);
    assert!(b.lobby(&lobby).is_none(), "B kept a lobby of its own: {:?}", b.lobby(&lobby));
    for (name, l) in [("a", &a), ("c", &c)] {
        let x = l.lobby(&lobby).expect("still in the lobby");
        assert_eq!(x.owner, a.me(), "{name}'s owner");
        assert_eq!(x.members.len(), 2, "{name}'s members");
    }
    let promoted = |e: &[Event]| e.iter().any(|x| matches!(x, Event::MemberStatus { status: MemberStatus::Promoted, .. }));
    assert!(!seen.iter().any(|e| promoted(e)), "someone was made owner: {seen:#?}");
    // B and C still play together outside the lobby.
    assert!(b.peer_trust(c.me()).is_some() && c.peer_trust(b.me()).is_some());
    for l in [&a, &b, &c] {
        l.shutdown();
    }
}

/// The owner really gone (its PC off the network): the members ask each other, nobody
/// still has it, and one of them takes over as before.
#[test]
fn an_owner_gone_for_everyone_is_still_replaced() {
    let _timing = TestTiming::install(TestIntervals { link_grace: Some(Duration::from_secs(2)), ..TestIntervals::NONE });
    let (a, b, c, mut seen, lobby) = three_in_a_lobby("ownergone");
    for s in seen.iter_mut() {
        s.clear();
    }
    a.set_lan_discovery(false);
    for (l, other) in [(&a, b.me()), (&a, c.me()), (&b, a.me()), (&c, a.me())] {
        l.sever(other);
    }
    pump(&[&b, &c], &mut seen[1..], 25, "a new owner", |_| {
        let (x, y) = (b.lobby(&lobby), c.lobby(&lobby));
        x.zip(y).is_some_and(|(x, y)| x.owner == y.owner && x.owner != a.me() && x.members.len() == 2 && y.members.len() == 2)
    });
    for l in [&a, &b, &c] {
        l.shutdown();
    }
}

#[test]
fn a_player_who_quits_leaves_at_once() {
    let (a, b, c, mut seen, _) = three_in_a_lobby("bye");
    for s in seen.iter_mut() {
        s.clear();
    }
    let t = Instant::now();
    b.shutdown();
    pump(&[&a, &c], &mut seen[..2], 8, "B to leave", |s| has(&s[0], &Event::PeerLeft(b.me())) && has(&s[1], &Event::PeerLeft(b.me())));
    assert!(t.elapsed() < Duration::from_secs(5), "waited for a player who said goodbye");
    a.shutdown();
    c.shutdown();
}

/// A copy of the game on another PeerParty can't connect: it is still seen, and said to
/// be older or newer, so the overlay can tell the player why a friend never shows up.
#[test]
fn a_copy_on_another_version_is_seen_not_joined() {
    let game = format!("test:link-version:{}", std::process::id());
    // Its own discovery port (crafted beacons go there), below the others'.
    let disc = 45600 + (std::process::id() % 300) as u16;
    let a = start(&game, disc, "a");
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let them = PeerId::random();
    let mut older = test_beacon(&game, them, 9);
    let ours = u16::from_le_bytes([older[4], older[5]]);
    older[4..6].copy_from_slice(&(ours - 1).to_le_bytes());
    let mut newer = test_beacon(&game, PeerId::random(), 9);
    newer[4..6].copy_from_slice(&(ours + 1).to_le_bytes());
    let elsewhere = {
        let mut b = test_beacon("test:another-game", PeerId::random(), 9);
        b[4..6].copy_from_slice(&(ours + 1).to_le_bytes());
        b
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        for b in [&older, &newer, &elsewhere] {
            let _ = sock.send_to(b, ("127.0.0.1", disc));
        }
        a.tick();
        let mut v = a.other_versions();
        v.sort();
        if v == [false, true] {
            break;
        }
        assert!(Instant::now() < deadline, "heard {v:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(a.peers().is_empty(), "nobody joined");
    a.shutdown();
}
