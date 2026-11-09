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
