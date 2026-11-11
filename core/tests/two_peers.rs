//! Two engines in one process, sharing a discovery port the way two copies of a game on
//! one PC do.

use std::time::{Duration, Instant};

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

/// Ticks and polls `lan` until `want` matches an event, keeping the rest in `seen`.
fn wait(lan: &Lan, seen: &mut Vec<Event>, want: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(i) = seen.iter().position(&want) {
            return seen.remove(i);
        }
        lan.tick();
        seen.extend(lan.poll());
        assert!(
            Instant::now() < deadline,
            "timed out; events so far: {seen:#?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn two_copies_meet_lobby_and_talk() {
    // Production waits 5s for a Hello. An idle connection must outlive that, because the
    // timeout is cleared once the Hello is done. A shorter one proves the same thing.
    let handshake = Duration::from_millis(250);
    let _timing = TestTiming::install(TestIntervals { handshake: Some(handshake), ..TestIntervals::NONE });
    let port = 47600 + (std::process::id() % 300) as u16;
    let game = format!("test:{}", std::process::id());
    let a = start(&game, port, "a");
    let b = start(&game, port, "b");
    let (mut ea, mut eb) = (Vec::new(), Vec::new());

    wait(&a, &mut ea, |e| *e == Event::PeerJoined(b.me()));
    wait(&b, &mut eb, |e| *e == Event::PeerJoined(a.me()));

    std::thread::sleep(handshake * 2);
    a.tick();
    b.tick();
    assert!(
        !a.poll()
            .into_iter()
            .chain(b.poll())
            .any(|e| matches!(e, Event::PeerLeft(_))),
        "an idle connection dropped"
    );

    // A hosts.
    let mut spec = Lobby::new("eos.lobby");
    spec.bucket = "b1".into();
    spec.attrs.push(Attr {
        key: "MODE".into(),
        value: Value::Int(3),
        public: true,
    });
    let hosted = a.create_lobby(spec).unwrap();
    assert_eq!(hosted.owner, a.me());

    // B finds it by bucket and attribute.
    let mut q = Query::new("eos.lobby");
    q.filters.push(Filter::Bucket {
        op: Op::Eq,
        value: "b1".into(),
    });
    q.filters.push(Filter::Attr {
        key: "mode".into(),
        op: Op::Eq,
        value: Value::Int(3),
    });
    let search = b.search(q);
    let Event::SearchDone { results, .. } = wait(
        &b,
        &mut eb,
        |e| matches!(e, Event::SearchDone { search: s, .. } if *s == search),
    ) else {
        unreachable!()
    };
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, hosted.id);

    // B joins; A sees it.
    b.join(&results[0]).unwrap();
    assert_eq!(
        wait(&b, &mut eb, |e| matches!(e, Event::JoinDone { .. })),
        Event::JoinDone {
            lobby: hosted.id.clone(),
            result: JoinResult::Ok
        }
    );
    wait(&a, &mut ea, |e| {
        *e == Event::MemberStatus {
            lobby: hosted.id.clone(),
            member: b.me(),
            status: MemberStatus::Joined,
        }
    });
    wait(&b, &mut eb, |e| matches!(e, Event::LobbyUpdated { .. }));
    assert_eq!(b.lobby(&hosted.id).unwrap().members.len(), 2);

    // B sets its own member attribute; the owner applies it and everyone hears.
    let ready = Attr {
        key: "READY".into(),
        value: Value::Bool(true),
        public: true,
    };
    b.update(&hosted.id, None, &[ready.clone()], &[]).unwrap();
    wait(&a, &mut ea, |e| {
        *e == Event::MemberUpdated {
            lobby: hosted.id.clone(),
            member: b.me(),
        }
    });
    let a_view = a.lobby(&hosted.id).unwrap();
    assert_eq!(a_view.member(b.me()).unwrap().attrs, vec![ready]);

    // A changes the lobby; B gets an update.
    let mut edited = a.lobby(&hosted.id).unwrap();
    edited.max_members = 8;
    a.update(&hosted.id, Some(&edited), &[], &[]).unwrap();
    eb.clear();
    wait(&b, &mut eb, |e| matches!(e, Event::LobbyUpdated { .. }));
    assert_eq!(b.lobby(&hosted.id).unwrap().max_members, 8);

    // Presence reaches the other side, and so does an invite.
    let kv = vec![("connect".to_string(), hosted.id.clone())];
    a.set_presence(kv.clone());
    wait(&b, &mut eb, |e| *e == Event::PresenceChanged(a.me()));
    assert_eq!(b.presence(a.me()), kv);
    let mine = a.my_lobbies("eos.lobby");
    assert_eq!(mine.len(), 1);
    assert_eq!(a.invite(Some(b.me()), &hosted.id, mine.first()), 1);
    let Event::Invite {
        from,
        connect,
        lobby,
    } = wait(&b, &mut eb, |e| matches!(e, Event::Invite { .. }))
    else {
        unreachable!()
    };
    assert_eq!((from, connect.as_str()), (a.me(), hosted.id.as_str()));
    assert_eq!(lobby.map(|l| l.id), Some(hosted.id.clone()));

    // B opens a channel by sending; the packet waits until A accepts.
    assert_eq!(
        b.send(a.me(), "game", 1, b"hello", true, true, true),
        SendResult::Held
    );
    wait(&a, &mut ea, |e| {
        *e == Event::ChanRequest {
            peer: b.me(),
            socket: "game".into(),
        }
    });
    a.accept(b.me(), "game");
    wait(&a, &mut ea, |e| matches!(e, Event::ChanOpen { .. }));
    wait(&b, &mut eb, |e| matches!(e, Event::ChanOpen { .. }));
    let deadline = Instant::now() + Duration::from_secs(5);
    let packet = loop {
        if let Some(p) = a.receive(Some(1)) {
            break p;
        }
        assert!(Instant::now() < deadline, "no packet");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(packet.data, b"hello");
    assert_eq!(packet.peer, b.me());
    assert!(packet.ordered, "reliable data comes off the TCP stream");
    // By now the UDP probe has answered (it ran during the idle wait above), so
    // unreliable data goes as datagrams; it still arrives, in order on a quiet LAN.
    assert!(a.udp_ready(b.me()) && b.udp_ready(a.me()), "udp probe");
    // Both run this version, so their datagrams are sequenced.
    assert!(
        a.udp_sequenced(b.me()) && b.udp_sequenced(a.me()),
        "sequenced datagrams negotiated"
    );
    for i in 0..5u8 {
        assert_eq!(
            a.send(b.me(), "game", 0, &[b'u', i], false, false, true),
            SendResult::Sent
        );
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = Vec::new();
    while got.len() < 5 {
        if let Some(p) = b.receive(Some(0)) {
            assert_eq!(p.peer, a.me());
            assert!(!p.ordered, "unreliable data came as a datagram");
            got.push(p.data[1]);
        }
        assert!(Instant::now() < deadline, "unreliable packets lost: {got:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(got.len(), 5);

    // A vanishes: B's channel closes and B inherits the lobby.
    let a_port = a.tcp_port();
    a.shutdown();
    // Shutting down closes the listener too (accept no longer blocks it open).
    let deadline = Instant::now() + Duration::from_secs(3);
    while std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], a_port)),
        Duration::from_millis(500),
    )
    .is_ok()
    {
        assert!(Instant::now() < deadline, "listener still open after shutdown");
        std::thread::sleep(Duration::from_millis(50));
    }
    wait(&b, &mut eb, |e| {
        matches!(
            e,
            Event::ChanClosed {
                reason: CloseReason::Lost,
                ..
            }
        )
    });
    wait(&b, &mut eb, |e| {
        *e == Event::MemberStatus {
            lobby: hosted.id.clone(),
            member: b.me(),
            status: MemberStatus::Promoted,
        }
    });
    assert!(b.is_owner(&hosted.id));
    b.shutdown();
}

/// Whatever connects to the engine's port and waits is told nothing: the engine's
/// hello (this player's name and id) is the answer to a copy of the game that said
/// hello first, not a greeting.
#[test]
fn a_silent_caller_learns_nothing() {
    use std::io::Read;
    let port = 47950 + (std::process::id() % 300) as u16;
    let a = start(&format!("test:silent:{}", std::process::id()), port, "a");
    let mut s = std::net::TcpStream::connect(("127.0.0.1", a.tcp_port())).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(600))).unwrap();
    let mut buf = [0u8; 256];
    // Nothing comes: the read times out, or the engine hangs up; no bytes either way.
    assert!(matches!(s.read(&mut buf), Ok(0) | Err(_)), "the engine spoke first");
    a.shutdown();
}

/// An id is its key's hash, and a connection is a player's only once that key has signed
/// for it. Before, whoever could reach the engine's port could say it was any player
/// whose id it knew: a friend, and (the LAN counting for more than the internet) in
/// place of that friend's own connection.
#[test]
fn an_id_cant_be_claimed_without_its_key() {
    use peerparty_core::inet::public::IdentityKey;
    use peerparty_core::inet::testing::{mesh_hello_as, mesh_hello_read, mesh_proof};
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let port = 48250 + (std::process::id() % 300) as u16;
    let game = format!("test:claim:{}", std::process::id());
    let a = start(&game, port, "a");
    let ben = Identity::ephemeral("ben");
    let (ben_id, ben_key) = (ben.id, ben.key.public());
    let b = Lan::start(Options { game: game.clone(), identity: ben, discovery_port: port, extra_targets: Vec::new(), log: quiet, internet: None, public: None, lan_discovery: true }).expect("start");
    let (mut ea, mut eb) = (Vec::new(), Vec::new());
    wait(&a, &mut ea, |e| *e == Event::PeerJoined(ben_id));
    wait(&b, &mut eb, |e| *e == Event::PeerJoined(a.me()));

    // One handshake with A's engine as `id`, showing `key` and signing with `signer`:
    // whether A let the connection in (it says nothing more and hangs up if not). A
    // connection let in may stay quiet, so one still open after `quiet` counts as let
    // in: a refusal is waited for longer than a busy PC takes to check a proof.
    let claim = |id: PeerId, key: [u8; 32], signer: &IdentityKey, quiet: Duration| -> bool {
        let mut s = TcpStream::connect(("127.0.0.1", a.tcp_port())).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mine = [0x5A; 16];
        s.write_all(&mesh_hello_as(id, &key, &game, &mine)).unwrap();
        let mut len = [0u8; 4];
        s.read_exact(&mut len).expect("the engine's hello");
        let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
        s.read_exact(&mut body).unwrap();
        let (a_id, a_nonce) = mesh_hello_read(&body).expect("a hello");
        assert_eq!(a_id, a.me());
        s.write_all(&mesh_proof(signer.sign_hello(&game, (id, &mine), (a_id, &a_nonce)))).unwrap();
        // The engine's own proof comes either way; after it, a connection that was let
        // in stays open and quiet, and one that wasn't is closed.
        s.read_exact(&mut len).expect("the engine's proof");
        let mut proof = vec![0u8; u32::from_le_bytes(len) as usize];
        s.read_exact(&mut proof).unwrap();
        s.set_read_timeout(Some(quiet)).unwrap();
        let mut rest = [0u8; 64];
        match s.read(&mut rest) {
            Ok(0) => false,
            Ok(_) => true,
            // Hung up on, with a reset rather than an end.
            Err(e) if !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => false,
            Err(_) => true,
        }
    };

    let eve = IdentityKey::generate();
    // Ben's id with Eve's own key: the id isn't that key's.
    assert!(!claim(ben_id, eve.public(), &eve, Duration::from_secs(10)), "an id was taken with another key");
    // Ben's id and Ben's (public) key, signed by Eve.
    assert!(!claim(ben_id, ben_key, &eve, Duration::from_secs(10)), "an id was taken without its key");
    // Ben is still Ben: nothing left, nothing joined, and they still talk.
    a.tick();
    ea.extend(a.poll());
    assert!(!ea.iter().any(|e| matches!(e, Event::PeerLeft(_) | Event::PeerJoined(_))), "{ea:?}");
    assert_eq!(a.peers().len(), 1);
    assert_ne!(b.send(a.me(), "game:x", 0, b"still ben", true, true, true), SendResult::Dropped);
    let deadline = Instant::now() + Duration::from_secs(10);
    let got = loop {
        for l in [&a, &b] {
            l.tick();
            for e in l.poll() {
                if let Event::ChanRequest { peer, socket } = e {
                    l.accept(peer, &socket);
                }
            }
        }
        if let Some(p) = a.receive_where(None, |s| s == "game:x") {
            break p;
        }
        assert!(Instant::now() < deadline, "Ben's message never came");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!((got.peer, &got.data[..]), (ben_id, &b"still ben"[..]));

    // The same steps with a key of one's own get in: the refusals above were for the
    // claim, not for how the test shakes hands.
    assert!(claim(PeerId::of_key(&eve.public()), eve.public(), &eve, Duration::from_millis(1500)), "an honest hello was refused");
    wait(&a, &mut Vec::new(), |e| *e == Event::PeerJoined(PeerId::of_key(&eve.public())));
    a.shutdown();
    b.shutdown();
}

/// Three small things that went wrong between two copies: a search for the lobby a
/// player is in found it only if that player owned it; a join given up on left its
/// asker in the owner's lobby for good; and a channel first used while the other copy
/// was briefly gone never opened once it was back.
#[test]
fn member_searches_abandoned_joins_and_channels_asked_for_early() {
    let port = 48560 + (std::process::id() % 300) as u16;
    let game = format!("test:small:{}", std::process::id());
    let a = start(&game, port, "a");
    let b = start(&game, port, "b");
    let c = start(&game, port, "c");
    let (mut ea, mut eb, mut ec) = (Vec::new(), Vec::new(), Vec::new());
    for (l, e) in [(&a, &mut ea), (&b, &mut eb), (&c, &mut ec)] {
        for other in [&a, &b, &c] {
            if other.me() != l.me() {
                wait(l, e, |ev| *ev == Event::PeerJoined(other.me()));
            }
        }
    }
    let pump_all = || {
        for l in [&a, &b, &c] {
            l.tick();
        }
    };

    // A hosts, B joins. C looks for "the lobby B is in": it is A's, and it is found.
    let mut spec = Lobby::new("test.lobby");
    spec.max_members = 2;
    let lobby = a.create_lobby(spec).unwrap();
    b.join(&lobby).unwrap();
    wait(&b, &mut eb, |e| matches!(e, Event::JoinDone { result: JoinResult::Ok, .. }));
    let mut q = Query::new("test.lobby");
    q.member = Some(b.me());
    let search = c.search(q);
    let found = loop {
        pump_all();
        if let Event::SearchDone { results, .. } = wait(&c, &mut ec, |e| matches!(e, Event::SearchDone { search: s, .. } if *s == search)) {
            break results;
        }
    };
    assert_eq!(found.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), [lobby.id.as_str()]);

    // B leaves; C asks to join by id and changes its mind at once. A must not keep C.
    b.leave(&lobby.id).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while a.lobby(&lobby.id).unwrap().members.len() != 1 {
        pump_all();
        a.poll();
        assert!(Instant::now() < until, "B never left");
        std::thread::sleep(Duration::from_millis(10));
    }
    c.join_by_id("test.lobby", &lobby.id).unwrap();
    c.leave(&lobby.id).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        pump_all();
        a.poll();
        c.poll();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(a.lobby(&lobby.id).unwrap().members.len(), 1, "a join given up on left a ghost member");

    // The connection between A and B breaks for a moment, and A's game sends through it.
    a.cut_connection(b.me());
    wait(&a, &mut ea, |e| *e == Event::PeerLeft(b.me()));
    assert_eq!(a.send(b.me(), "game:early", 0, b"sent while away", true, true, true), SendResult::Held);
    let until = Instant::now() + Duration::from_secs(20);
    let got = loop {
        for (l, e) in [(&a, &mut ea), (&b, &mut eb)] {
            l.tick();
            for ev in l.poll() {
                if let Event::ChanRequest { peer, socket } = &ev {
                    l.accept(*peer, socket);
                }
                e.push(ev);
            }
        }
        if let Some(p) = b.receive_where(None, |s| s == "game:early") {
            break p.data;
        }
        assert!(Instant::now() < until, "what was sent during the break never arrived\n  a: {ea:?}\n  b: {eb:?}");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(got, b"sent while away");
    for l in [&a, &b, &c] {
        l.shutdown();
    }
}
