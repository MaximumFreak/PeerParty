use super::*;

const SECRET: [u8; 32] = [42; 32];

/// `l` under its owner's claim for `term`, signed with SECRET.
fn signed(mut l: Lobby, term: u64) -> Lobby {
    l.claim = crate::lobby::Claim::make(&SECRET, &l.id, l.owner, term);
    l
}

fn state(l: &Lobby) -> Msg {
    Msg::State { lobby: l.clone(), secret: SECRET }
}

#[test]
fn a_relay_between_lan_players_isnt_the_internet() {
    let entry = |public, relayed| TunnelEntry {
        tunnel: inet::Tunnel { id: PeerId::default(), public, key: None, name: String::new() },
        down: None,
        closed: None,
        relayed,
    };
    assert!(!crosses_internet(&entry(false, Some(Trust::Lan))));
    assert!(crosses_internet(&entry(false, Some(Trust::Room))));
    assert!(crosses_internet(&entry(true, Some(Trust::Public))));
    assert!(crosses_internet(&entry(false, None)));
    assert!(crosses_internet(&entry(true, None)));
}

#[test]
fn numbered_messages_weigh_what_they_carry() {
    let data = Msg::ChanData { socket: "s".into(), channel: 0, reliable: true, data: vec![0; 1 << 20] };
    let alone = weight(&data);
    assert!(alone > 1 << 20);
    assert_eq!(weight(&Msg::Numbered { n: 1, msg: Box::new(data) }), alone);
}

#[test]
fn public_messages_lose_their_addresses() {
    let w = std::net::Ipv4Addr::new(198, 18, 7, 7);
    let attr = |k: &str, v: &str| Attr { key: k.into(), value: crate::lobby::Value::Str(v.into()), public: true };
    let mut l = Lobby::new("eos.lobby");
    l.attrs = vec![attr("host", "192.168.1.20:7777"), attr("version", "1.0.7.2"), attr("n", "x")];
    l.members.push(Member { id: PeerId::default(), attrs: vec![attr("addr", "[2001:db8::5]:9000")] });
    let mut msgs = vec![
        Msg::Presence { kv: vec![("connect".into(), "+connect 203.0.113.5:27015".into())] },
        Msg::SearchReply { search: 1, lobbies: vec![l.clone()] },
        Msg::State { lobby: l.clone(), secret: [0; 32] },
        Msg::MemberAttrs { lobby: "x".into(), attrs: vec![attr("ip", "10.0.0.9")] },
        Msg::Invite { connect: "+connect 10.0.0.9:27015".into(), lobby: Some(l) },
    ];
    for m in &mut msgs {
        assert!(may_carry_addresses(m));
        scrub_public(m, w);
        let shown = format!("{m:?}");
        for real in ["192.168.1.20", "2001:db8", "203.0.113.5", "10.0.0.9"] {
            assert!(!shown.contains(real), "{shown}");
        }
        assert!(shown.contains("198.18.7.7"), "{shown}");
    }
    let Msg::State { lobby, .. } = &msgs[2] else { unreachable!() };
    assert_eq!(lobby.attrs[0].value, crate::lobby::Value::Str("198.18.7.7:7777".into()));
    assert_eq!(lobby.attrs[1].value, crate::lobby::Value::Str("1.0.7.2".into()), "a version was taken for an address");
    assert_eq!(lobby.members[0].attrs[0].value, crate::lobby::Value::Str("198.18.7.7:9000".into()));
    // A member the owner relays keeps its own stand-in, not the owner's.
    let relayed = PeerId::random();
    let mut l = Lobby::new("eos.lobby");
    l.members.push(Member { id: relayed, attrs: vec![attr("addr", "192.168.1.30:7777")] });
    let mut m = Msg::State { lobby: l, secret: [0; 32] };
    scrub_public(&mut m, w);
    let Msg::State { lobby, .. } = &m else { unreachable!() };
    let want = format!("{}:7777", crate::redact::stand_in(&relayed.0));
    assert_eq!(lobby.members[0].attrs[0].value, crate::lobby::Value::Str(want));
    assert!(!may_carry_addresses(&Msg::ChanData { socket: "s".into(), channel: 0, reliable: true, data: Vec::new() }));
}

#[test]
fn public_lobbies_are_bounded() {
    let attr = |k: &str, v: crate::lobby::Value| Attr { key: k.into(), value: v, public: true };
    let mut l = Lobby::new("eos.lobby");
    l.id = "x".into();
    l.max_members = 1000;
    l.attrs = vec![
        attr("ok", crate::lobby::Value::Str("fine".into())),
        attr("huge", crate::lobby::Value::Str("v".repeat(5000))),
        attr(&"k".repeat(300), crate::lobby::Value::Int(1)),
        attr("nan", crate::lobby::Value::Double(f64::NAN)),
    ];
    l.attrs.extend((0..500).map(|i| attr(&format!("a{i}"), crate::lobby::Value::Bool(true))));
    l.members = vec![Member { id: PeerId([1; 16]), attrs: vec![attr("big", crate::lobby::Value::Str("m".repeat(2000)))] }];
    let mut m = Msg::State { lobby: l.clone(), secret: [0; 32] };
    assert!(cap_public(&mut m));
    let Msg::State { lobby: got, .. } = m else { unreachable!() };
    assert_eq!(got.attrs.len(), PUBLIC_ATTRS);
    assert_eq!(got.attrs[0].key, "ok");
    assert!(got.attrs.iter().all(|a| a.key != "huge" && a.key != "nan" && a.key.len() <= PUBLIC_KEY_BYTES));
    assert!(got.members[0].attrs.is_empty());
    assert_eq!(got.max_members, PUBLIC_MEMBERS as u32);

    let mut crowded = l.clone();
    crowded.members = (0..100).map(|i| Member { id: PeerId([i; 16]), attrs: Vec::new() }).collect();
    assert!(!cap_public(&mut Msg::State { lobby: crowded.clone(), secret: [0; 32] }));
    let mut reply = Msg::SearchReply { search: 1, lobbies: vec![crowded.clone(), l.clone()] };
    assert!(cap_public(&mut reply));
    let Msg::SearchReply { lobbies, .. } = reply else { unreachable!() };
    assert_eq!(lobbies.len(), 1, "the crowded lobby is dropped");
    let mut invite = Msg::Invite { connect: "c".into(), lobby: Some(crowded) };
    assert!(cap_public(&mut invite));
    assert!(matches!(invite, Msg::Invite { lobby: None, .. }));
    assert!(!cap_public(&mut Msg::Invite { connect: "c".repeat(5000), lobby: None }));
    assert!(cap_public(&mut Msg::Invite { connect: "+connect 198.18.0.5:27015".into(), lobby: None }));
    assert!(cap_public(&mut Msg::Invite { connect: "lobby:123456".into(), lobby: None }));
    assert!(!cap_public(&mut Msg::Invite { connect: "+connect 1.2.3.4 +exec evil".into(), lobby: None }));
    assert!(!cap_public(&mut Msg::Invite { connect: "+connect 1.2.3.4;quit".into(), lobby: None }));
    assert!(!cap_public(&mut Msg::Invite { connect: "+connect 1.2.3.4\n+exec x".into(), lobby: None }));
}

/// A snapshot held back for a stranger's quick attribute changes goes out once its
/// time is up; held-back state for a lobby we no longer host is forgotten.
#[test]
fn a_strangers_held_back_snapshot_goes_out_later() {
    let mut l = Lobby::new("eos.lobby");
    l.id = "x".into();
    let mut s = State::default();
    s.lobbies.insert("x".into(), Held::new(l, Role::Hosting));
    let now = Instant::now();
    s.stranger_casts.insert("x".into(), (now, true));
    s.stranger_casts.insert("gone".into(), (now, true));
    s.send_owed_casts(now + STRANGER_CAST_EVERY / 2);
    assert_eq!(s.stranger_casts.get("x"), Some(&(now, true)), "not yet");
    assert!(!s.stranger_casts.contains_key("gone"));
    let later = now + STRANGER_CAST_EVERY;
    s.send_owed_casts(later);
    assert_eq!(s.stranger_casts.get("x"), Some(&(later, false)), "sent");
    s.send_owed_casts(later + Duration::from_secs(11));
    assert!(s.stranger_casts.is_empty(), "quiet for a while: forgotten");
}

/// A stranger's join that waits for a lobby closed meanwhile, or from a player who
/// left, is let go: it doesn't keep one of the JOINS_WAITING places for good.
#[test]
fn waiting_joins_with_nobody_to_answer_go() {
    let mut s = State::default();
    let (me, gone) = (PeerId([1; 16]), PeerId([2; 16]));
    let now = Instant::now();
    let mut l = Lobby::new("eos.lobby");
    l.id = "hosted".into();
    s.lobbies.insert(l.id.clone(), Held::new(l, Role::Hosting));
    for i in 0..JOINS_WAITING {
        s.waiting.push((gone, format!("closed {i}"), now + JOIN_WAIT));
    }
    s.waiting.push((gone, "hosted".into(), now + JOIN_WAIT));
    s.answer_waiting_joins(me, now);
    assert!(s.waiting.is_empty(), "{:?}", s.waiting);
    assert!(s.events.is_empty());
}
/// The heir's first State, sent as soon as it noticed the owner gone, isn't lost on a
/// member that notices a moment later; one from any other member, or while the owner
/// is still here, changes nothing.
#[test]
fn the_heirs_first_state_waits_for_the_owners_loss() {
    let (heir, me, owner, other) = (PeerId([2; 16]), PeerId([6; 16]), PeerId([5; 16]), PeerId([7; 16]));
    let member = |id| Member { id, attrs: Vec::new() };
    let mut l = Lobby::new("eos.lobby");
    l.id = "x".into();
    l.owner = owner;
    l.members = vec![member(owner), member(heir), member(me), member(other)];
    let l = signed(l, 1);
    let mut s = State::default();
    s.lobbies.insert(l.id.clone(), Held::new(l.clone(), Role::Joined));

    let mut by_heir = l.clone();
    by_heir.owner = heir;
    by_heir.members.retain(|m| m.id != owner);
    by_heir.attrs.push(Attr { key: "map".into(), value: crate::lobby::Value::Str("new".into()), public: true });
    let by_heir = signed(by_heir, 2);
    let mut by_other = by_heir.clone();
    by_other.owner = other;
    let by_other = signed(by_other, 2);
    s.handle(me, other, state(&by_other), true);
    s.handle(me, heir, state(&by_heir), true);
    assert_eq!(s.lobbies["x"].lobby, l, "nothing changes while the owner is here");
    assert!(s.events.is_empty());

    s.peer_lost(me, owner);
    assert_eq!(s.lobbies["x"].lobby, by_heir, "the heir's snapshot, attributes and all");
    let events: Vec<_> = s.events.drain(..).collect();
    assert!(events.iter().any(|e| matches!(e, Event::MemberStatus { member, status: MemberStatus::Disconnected, .. } if *member == owner)));
    assert!(events.iter().any(|e| matches!(e, Event::MemberStatus { member, status: MemberStatus::Promoted, .. } if *member == heir)));
    assert!(events.iter().any(|e| matches!(e, Event::LobbyUpdated { .. })), "{events:?}");
}

/// Member attributes sent to an owner that hands the lobby on before reading them
/// aren't lost: they go to the new owner, or apply here when that's us (M-12).
#[test]
fn member_attributes_survive_a_hand_off() {
    let (owner, heir, me) = (PeerId([5; 16]), PeerId([2; 16]), PeerId([6; 16]));
    let ready = vec![Attr { key: "ready".into(), value: crate::lobby::Value::Str("1".into()), public: true }];
    let member = |id| Member { id, attrs: Vec::new() };
    let mut l = Lobby::new("eos.lobby");
    l.id = "x".into();
    l.owner = owner;
    l.host_migration = true;
    l.members = vec![member(owner), member(heir), member(me)];
    let l = signed(l, 1);
    let mut s = State::default();
    s.lobbies.insert(l.id.clone(), Held { wanted: Some(ready.clone()), ..Held::new(l.clone(), Role::Joined) });
    // The old owner names its heir without our attributes: still wanted (sent on).
    let mut handed = l.clone();
    handed.owner = heir;
    handed.members.retain(|m| m.id != owner);
    let handed = signed(handed, 2);
    s.handle(me, owner, state(&handed), true);
    assert_eq!(s.lobbies["x"].lobby.owner, heir);
    assert_eq!(s.lobbies["x"].wanted.as_ref(), Some(&ready));
    // Once a State shows them, nothing more is sent at the next hand-off.
    let mut shown = handed.clone();
    shown.member_mut(me).unwrap().attrs = ready.clone();
    s.handle(me, heir, state(&shown), true);
    let mut again = shown.clone();
    again.owner = owner;
    s.handle(me, heir, state(&signed(again, 3)), true);
    assert_eq!(s.lobbies["x"].wanted, None);

    // We inherit: our attributes apply here.
    let mut s = State::default();
    s.lobbies.insert(l.id.clone(), Held { wanted: Some(ready.clone()), ..Held::new(l.clone(), Role::Joined) });
    let mut to_me = l.clone();
    to_me.owner = me;
    to_me.members.retain(|m| m.id != owner);
    s.handle(me, owner, state(&signed(to_me, 2)), true);
    assert_eq!(s.lobbies["x"].role, Role::Hosting);
    assert_eq!(s.lobbies["x"].lobby.member(me).unwrap().attrs, ready);
    assert!(s.events.iter().any(|e| matches!(e, Event::MemberUpdated { member, .. } if *member == me)));
}

/// A stand-in address goes to the most trusted peer, then the one here longest: a
/// stranger who came first doesn't keep it from a friend or a LAN player.
#[test]
fn peers_stand_by_trust_then_time() {
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let id = |b: u8| PeerId([b; 16]);
    let order = by_standing(
        [
            (Trust::Public, false, at(0), id(1)),
            (Trust::Room, false, at(5), id(2)),
            (Trust::Lan, false, at(9), id(3)),
            (Trust::Room, false, at(1), id(4)),
            (Trust::Public, false, at(3), id(5)),
            (Trust::Lan, false, at(2), id(6)),
        ]
        .into_iter(),
    );
    assert_eq!(order, [id(6), id(3), id(4), id(2), id(1), id(5)]);
}

/// A member asking to join again (its answer was lost) is answered without the
/// others being told it joined twice.
#[test]
fn a_repeated_join_is_quiet() {
    let (me, guest) = (PeerId([1; 16]), PeerId([2; 16]));
    let mut l = Lobby::new("eos.lobby");
    l.id = "x".into();
    l.owner = me;
    l.members = vec![Member { id: me, attrs: Vec::new() }, Member { id: guest, attrs: Vec::new() }];
    let mut s = State::default();
    s.lobbies.insert(l.id.clone(), Held::new(l.clone(), Role::Hosting));
    s.handle(me, guest, Msg::Join { lobby: "x".into(), secret: String::new() }, true);
    assert!(s.events.is_empty(), "{:?}", s.events);
    assert_eq!(s.lobbies["x"].lobby, l);
}

/// The owner's front end decides on the secret a join comes with; a refused joiner
/// isn't added, an accepted one is.
#[test]
fn the_owners_join_check_decides_on_the_secret() {
    fn check(_: &Lobby, _: PeerId, secret: &str) -> Option<JoinResult> {
        (secret != "letmein").then_some(JoinResult::WrongSecret)
    }
    let (me, guest) = (PeerId([1; 16]), PeerId([2; 16]));
    let mut l = Lobby::new("ugs.lobby");
    l.id = "x".into();
    l.owner = me;
    l.permission = crate::lobby::Permission::Public;
    l.members = vec![Member { id: me, attrs: Vec::new() }];
    let mut s = State::default();
    s.join_checks.insert("ugs.lobby".into(), check);
    s.lobbies.insert(l.id.clone(), Held::new(l.clone(), Role::Hosting));
    s.handle(me, guest, Msg::Join { lobby: "x".into(), secret: "guess".into() }, true);
    assert!(s.lobbies["x"].lobby.member(guest).is_none());
    assert!(s.events.is_empty(), "{:?}", s.events);
    s.handle(me, guest, Msg::Join { lobby: "x".into(), secret: "letmein".into() }, true);
    assert!(s.lobbies["x"].lobby.member(guest).is_some());
    // Another namespace's lobbies aren't this check's.
    s.lobbies.get_mut("x").unwrap().lobby.namespace = "eos.lobby".into();
    let other = PeerId([3; 16]);
    s.handle(me, other, Msg::Join { lobby: "x".into(), secret: String::new() }, true);
    assert!(s.lobbies["x"].lobby.member(other).is_some());
}

#[test]
fn a_public_players_invites_are_rationed() {
    let t0 = Instant::now();
    let mut r = Ration::new(PUBLIC_INVITE_BURST, t0);
    let mut take = |now: Instant| r.take(now, PUBLIC_INVITE_BURST, PUBLIC_INVITE_EVERY);
    assert_eq!((0..100).filter(|_| take(t0)).count(), PUBLIC_INVITE_BURST as usize, "a burst, then no more");
    assert!(!take(t0 + PUBLIC_INVITE_EVERY / 2));
    assert!(take(t0 + PUBLIC_INVITE_EVERY * 2), "one more after a wait");
    // A long wait doesn't save up more than the burst.
    let later = t0 + Duration::from_secs(3600);
    assert_eq!((0..100).filter(|_| take(later)).count(), PUBLIC_INVITE_BURST as usize);
}

fn packet(n: u8, len: usize) -> Packet {
    Packet {
        peer: PeerId([n; 16]),
        socket: "s".into(),
        channel: 0,
        data: vec![n; len],
        ordered: true,
    }
}

#[test]
fn inbox_drops_the_oldest_past_its_caps() {
    let mut inbox = Inbox::default();
    for n in 0..5 {
        inbox.push_capped(packet(n, 10), 3, 1000);
    }
    let kept: Vec<u8> = inbox.iter().map(|p| p.data[0]).collect();
    assert_eq!(kept, [2, 3, 4]);
    assert_eq!((inbox.bytes, inbox.dropped), (30, 2));

    let mut inbox = Inbox::default();
    inbox.push_capped(packet(1, 60), 100, 100);
    inbox.push_capped(packet(2, 60), 100, 100);
    assert_eq!(inbox.iter().map(|p| p.data[0]).collect::<Vec<_>>(), [2]);
    // One packet over the byte cap on its own is still kept.
    inbox.push_capped(packet(3, 500), 100, 100);
    assert_eq!(inbox.iter().map(|p| p.data[0]).collect::<Vec<_>>(), [3]);
    assert_eq!(inbox.bytes, 500);

    assert!(inbox.take(None, |_| true).is_some());
    assert_eq!((inbox.bytes, inbox.len, live_queues(&inbox)), (0, 0, 0));
    inbox.push(packet(4, 7));
    inbox.push(packet(5, 9));
    inbox.retain(|p| p.data[0] == 5);
    assert_eq!(inbox.bytes, 9);
    assert_eq!(inbox.per_peer.len(), 1);
    assert_eq!(inbox.per_peer[&PeerId([5; 16])], (1, 9));
}

/// A public player's packets stop at their share; the others' still go in, and the
/// share frees up as the game reads.
#[test]
fn a_strangers_share_of_the_inbox_is_bounded() {
    let mut inbox = Inbox::default();
    for _ in 0..10 {
        inbox.push_shared(packet(1, 10), 4, 1000);
    }
    assert_eq!((inbox.len, inbox.over_share), (4, 6), "count share");
    for _ in 0..3 {
        inbox.push_shared(packet(2, 400), 100, 1000);
    }
    assert_eq!(inbox.per_peer[&PeerId([2; 16])], (2, 800), "byte share");
    // A friend's packets aren't held to a share, and nothing was pushed out for them.
    for _ in 0..50 {
        inbox.push(packet(3, 10));
    }
    assert_eq!(inbox.len, 56);
    assert_eq!(inbox.dropped, 0);
    // Read one: there's room for one more.
    assert_eq!(inbox.take(None, |_| true).unwrap().peer, PeerId([1; 16]), "the oldest");
    inbox.push_shared(packet(1, 10), 4, 1000);
    inbox.push_shared(packet(1, 10), 4, 1000);
    assert_eq!(inbox.per_peer[&PeerId([1; 16])].0, 4);
    assert_eq!(inbox.over_share, 8);
    // A first packet bigger than the byte share is still taken (as a lone one over
    // the inbox's cap is).
    inbox.push_shared(packet(4, 5000), 100, 1000);
    assert_eq!(inbox.per_peer[&PeerId([4; 16])], (1, 5000));
    assert_eq!(inbox.bytes, inbox.iter().map(|p| p.data.len()).sum::<usize>());
}

/// Reads take the oldest packet of the sockets and channel asked for, whatever else
/// waits; the inbox's oldest still goes first when it is full.
#[test]
fn inbox_reads_by_socket_and_channel_in_arrival_order() {
    let p = |socket: &str, channel: u8, n: u8| Packet { peer: PeerId([n; 16]), socket: socket.into(), channel, data: vec![n], ordered: true };
    let mut inbox = Inbox::default();
    // A channel nobody reads fills up first.
    for n in 0..100 {
        inbox.push(p("steam:p2p", 1, n));
    }
    inbox.push(p("steam:p2p", 0, 200));
    inbox.push(p("galaxy:p2p", 0, 201));
    inbox.push(p("steam:p2p", 0, 202));
    let steam = |s: &str| s.starts_with("steam:");
    assert_eq!(inbox.peek_len(Some(0), steam), Some(1));
    assert_eq!(inbox.take(Some(0), steam).unwrap().data, [200]);
    assert_eq!(inbox.take(Some(0), steam).unwrap().data, [202]);
    assert!(inbox.take(Some(0), steam).is_none());
    assert_eq!(inbox.take(None, |_| true).unwrap().data, [0], "any channel: the oldest");
    assert_eq!(inbox.take(Some(0), |_| true).unwrap().data, [201]);
    assert_eq!((inbox.len, live_queues(&inbox)), (99, 1));
    // Full: the oldest anywhere goes, not the newest channel's.
    let mut inbox = Inbox::default();
    inbox.push_capped(p("a", 0, 1), 2, 1000);
    inbox.push_capped(p("b", 0, 2), 2, 1000);
    inbox.push_capped(p("b", 0, 3), 2, 1000);
    assert_eq!(inbox.iter().map(|p| p.data[0]).collect::<Vec<_>>(), [2, 3]);
    assert_eq!(live_queues(&inbox), 1);
}

/// A push wakes a thread waiting for packets (`Lan::wait_for_packet`) at once, not
/// when its wait runs out.
#[test]
fn a_push_wakes_a_waiting_reader() {
    let s = Arc::new(Mutex::new(Inbox::default()));
    let (s2, start) = (s.clone(), Instant::now());
    let waiter = std::thread::spawn(move || {
        let mut g = s2.lock().unwrap();
        let arrived = g.arrived.clone();
        g.waiting += 1;
        while g.len == 0 && start.elapsed() < Duration::from_secs(10) {
            g = arrived.wait_timeout(g, Duration::from_secs(10)).unwrap().0;
        }
        g.waiting -= 1;
        start.elapsed()
    });
    while s.lock().unwrap().waiting == 0 {
        std::thread::yield_now();
    }
    s.lock().unwrap().push(packet(1, 4));
    assert!(waiter.join().unwrap() < Duration::from_secs(5));
}

fn live_queues(inbox: &Inbox) -> usize {
    inbox.queues.values().filter(|q| !q.is_empty()).count()
}

/// A socket's emptied queue stays for its next packet, up to `KEPT_EMPTY_QUEUES` of them;
/// past that an emptied one goes, so new socket names can't pile up empty queues.
#[test]
fn inbox_keeps_a_bounded_number_of_emptied_queues() {
    let p = |socket: String| Packet { peer: PeerId([1; 16]), socket, channel: 0, data: vec![1], ordered: true };
    let mut inbox = Inbox::default();
    inbox.push(p("steam:p2p".into()));
    assert!(inbox.take(None, |_| true).is_some());
    assert_eq!(inbox.queues.len(), 1, "kept for the next packet");
    inbox.push(p("steam:p2p".into()));
    assert_eq!(inbox.queues.len(), 1, "the same queue");
    assert!(inbox.take(None, |_| true).is_some());
    for n in 0..1000 {
        inbox.push(p(format!("s{n}")));
        assert!(inbox.take(None, |_| true).is_some());
    }
    assert!(inbox.queues.len() <= KEPT_EMPTY_QUEUES + 1, "{} queues", inbox.queues.len());
    assert_eq!((inbox.len, inbox.bytes, live_queues(&inbox)), (0, 0, 0));
}

#[test]
fn channel_hold_is_bounded() {
    let mut chan = Chan::new(ChanState::Pending);
    assert!(chan.hold(0, true, &[1; 10], 2, 25));
    assert!(chan.hold(0, true, &[2; 10], 2, 25));
    assert!(!chan.hold(0, true, &[3; 1], 2, 25), "count cap");
    let mut chan = Chan::new(ChanState::Pending);
    assert!(chan.hold(0, true, &[1; 20], 10, 25));
    assert!(!chan.hold(0, true, &[2; 6], 10, 25), "byte cap");
    assert!(chan.hold(0, true, &[2; 5], 10, 25));
    assert_eq!(chan.take_delayed().len(), 2);
    assert!(chan.hold(0, true, &[3; 25], 10, 25), "taking frees the hold");
}

/// A lobby's word comes only under its key (M-09): a snapshot signed with another
/// key, unsigned, or older than what we have changes nothing; the owner's own does,
/// and hands its members the secret for whoever owns it next.
#[test]
fn only_the_lobbys_key_speaks_for_it() {
    let (owner, me) = (PeerId([5; 16]), PeerId([6; 16]));
    let member = |id| Member { id, attrs: Vec::new() };
    let mut l = Lobby::new("eos.lobby");
    l.id = "x".into();
    l.owner = owner;
    l.members = vec![member(owner), member(me)];
    let l = signed(l, 2);
    let mut s = State::default();
    s.lobbies.insert(l.id.clone(), Held::new(l.clone(), Role::Joined));
    let attr = |v: &str| Attr { key: "map".into(), value: crate::lobby::Value::Str(v.into()), public: true };

    let mut forged = l.clone();
    forged.attrs.push(attr("forged"));
    forged.claim = crate::lobby::Claim::make(&[7; 32], "x", owner, 9);
    s.handle(me, owner, Msg::State { lobby: forged, secret: [7; 32] }, true);
    let mut unsigned = l.clone();
    unsigned.attrs.push(attr("unsigned"));
    unsigned.claim = crate::lobby::Claim::default();
    s.handle(me, owner, state(&unsigned), true);
    let mut old = l.clone();
    old.attrs.push(attr("old"));
    s.handle(me, owner, state(&signed(old, 1)), true);
    assert_eq!(s.lobbies["x"].lobby, l, "nothing changed");
    assert_eq!(s.lobbies["x"].secret, None);

    let mut real = l.clone();
    real.attrs.push(attr("real"));
    s.handle(me, owner, state(&real), true);
    assert_eq!(s.lobbies["x"].lobby, real);
    assert_eq!(s.lobbies["x"].secret, Some(SECRET), "a member holds the secret");
    // A secret that isn't the lobby's is never taken.
    let mut s2 = State::default();
    s2.lobbies.insert(l.id.clone(), Held::new(l.clone(), Role::Joined));
    s2.handle(me, owner, Msg::State { lobby: real.clone(), secret: [7; 32] }, true);
    assert_eq!(s2.lobbies["x"].secret, None);

    // Owning it next, we sign it a term on, so the others take our word.
    s.peer_lost(me, owner);
    let held = &s.lobbies["x"];
    assert_eq!(held.role, Role::Hosting);
    assert!(held.lobby.claim.holds("x", me) && held.lobby.claim.term == 3 && held.lobby.claim.key == l.claim.key);
}

/// Joining a lobby whose key we have (a search result): an answer under another key
/// is ignored, even a LAN player's, and the key's own is taken at once, even a public
/// player's. With no key in hand (a bare id) an answer must still be signed.
#[test]
fn a_join_is_answered_under_the_lobbys_key() {
    let (owner, me, faker) = (PeerId([5; 16]), PeerId([6; 16]), PeerId([8; 16]));
    let mut l = Lobby::new("eos.lobby");
    l.id = "x".into();
    l.owner = owner;
    let l = signed(l, 1);
    let mut s = State::default();
    s.lobbies.insert(l.id.clone(), Held::new(l.clone(), Role::Joining));
    s.joins.insert("x".into(), Instant::now() + JOIN_TIMEOUT);
    let fake = crate::lobby::Claim::make(&[7; 32], "x", owner, 5);
    s.handle(me, owner, Msg::JoinReply { lobby: "x".into(), result: JoinResult::Ok, claim: fake }, true);
    assert_eq!(s.lobbies["x"].role, Role::Joining, "another key's answer");
    s.handle(me, owner, Msg::JoinReply { lobby: "x".into(), result: JoinResult::Ok, claim: l.claim }, true);
    assert_eq!(s.lobbies["x"].role, Role::Joined, "taken at once, though no peer vouches for it");
    assert!(s.by_id.is_empty());

    // By id: an unsigned answer is nobody's; a signed one waits for the others asked
    // (or JOIN_CLAIM_WAIT); everyone saying it isn't theirs ends it.
    let mut s = State::default();
    let mut placeholder = Lobby::new("eos.lobby");
    placeholder.id = "y".into();
    s.lobbies.insert("y".into(), Held::new(placeholder, Role::Joining));
    s.joins.insert("y".into(), Instant::now() + JOIN_TIMEOUT);
    s.by_id.insert("y".into(), ById { asked: [faker, owner].into_iter().collect(), ..ById::default() });
    s.handle(me, faker, Msg::JoinReply { lobby: "y".into(), result: JoinResult::Ok, claim: crate::lobby::Claim::default() }, true);
    assert!(s.by_id["y"].answers.is_empty() && s.by_id["y"].until.is_none() && s.lobbies["y"].role == Role::Joining);
    let theirs = crate::lobby::Claim::make(&[7; 32], "y", faker, 1);
    s.handle(me, faker, Msg::JoinReply { lobby: "y".into(), result: JoinResult::Ok, claim: theirs }, true);
    assert!(s.by_id["y"].answers.len() == 1 && s.by_id["y"].until.is_some() && s.lobbies["y"].role == Role::Joining);
    s.handle(me, owner, Msg::JoinReply { lobby: "y".into(), result: JoinResult::NotFound, claim: crate::lobby::Claim::default() }, true);
    // All have answered: settled. The faker isn't among our peers (this test has
    // none), so its answer doesn't count, and nobody's lobby it is.
    assert!(s.by_id.is_empty() && !s.lobbies.contains_key("y"));
    assert_eq!(s.events.back(), Some(&Event::JoinDone { lobby: "y".into(), result: JoinResult::NotFound }));
}

/// A join by id: friends' answers count over strangers', and all that count must be
/// under one key, the latest term's owner taken.
#[test]
fn a_join_by_id_takes_one_key_or_none() {
    let (o, f, x) = (PeerId([1; 16]), PeerId([2; 16]), PeerId([3; 16]));
    let real = |owner: PeerId, term: u64| crate::lobby::Claim::make(&[7; 32], "y", owner, term);
    let fake = crate::lobby::Claim::make(&[9; 32], "y", f, 1);
    let ok = JoinResult::Ok;
    assert_eq!(settle(&[]), Settled::Nobody);
    assert_eq!(settle(&[(o, ok, real(o, 1), false)]), Settled::Take(0));
    assert_eq!(settle(&[(f, ok, fake, false), (o, ok, real(o, 1), false)]), Settled::Disputed, "a LAN player answering for it");
    assert_eq!(settle(&[(x, ok, fake, true), (o, JoinResult::Full, real(o, 1), false)]), Settled::Take(1), "a friend's over a stranger's");
    assert_eq!(settle(&[(x, ok, fake, true), (f, ok, real(f, 1), true)]), Settled::Disputed, "two strangers");
    assert_eq!(settle(&[(x, ok, fake, true)]), Settled::Take(0), "a stranger's own lobby");
    assert_eq!(settle(&[(o, ok, real(o, 2), false), (f, ok, real(f, 3), false)]), Settled::Take(1), "handed on as we asked");
}

/// Search results only count with their owner's signed claim.
#[test]
fn unsigned_search_results_are_dropped() {
    let (owner, me) = (PeerId([5; 16]), PeerId([6; 16]));
    let mut s = State::default();
    s.searches.insert(1, Search { query: Query::new("eos.lobby"), results: Vec::new(), awaiting: [owner].into_iter().collect(), deadline: Instant::now() + Duration::from_secs(5) });
    let mut good = Lobby::new("eos.lobby");
    good.id = "a".into();
    good.owner = owner;
    let mut bad = good.clone();
    bad.id = "b".into();
    let good = signed(good, 1);
    bad.claim = good.claim;
    s.handle(me, owner, Msg::SearchReply { search: 1, lobbies: vec![good.clone(), bad] }, true);
    let Some(Event::SearchDone { results, .. }) = s.events.pop_front() else { panic!("{:?}", s.events) };
    assert_eq!(results, vec![good]);
}

#[test]
fn application_signatures_name_their_player() {
    let (k, other) = (IdentityKey::generate(), IdentityKey::generate());
    let (me, sig) = (PeerId::of_key(&k.public()), k.sign_bytes("app", b"hello"));
    assert!(Lan::verify_bytes(me, &k.public(), "app", b"hello", &sig));
    assert!(!Lan::verify_bytes(PeerId::of_key(&other.public()), &k.public(), "app", b"hello", &sig), "another player's id");
    assert!(!Lan::verify_bytes(me, &k.public(), "app", b"hello!", &sig));
    assert!(!Lan::verify_bytes(me, &k.public(), "other", b"hello", &sig));
}

#[test]
fn voice_channels_are_told_apart_by_name() {
    assert_eq!(ChannelKind::of("rtc:party:1"), ChannelKind::Voice);
    assert_eq!(ChannelKind::of(&ChannelKind::voice_socket("room")), ChannelKind::Voice);
    assert_eq!(ChannelKind::of("steam:1"), ChannelKind::Data);
    assert!(voice_socket("rtc:x") && !voice_socket("party:x"));
}

#[test]
fn channel_traffic_is_counted_and_reported_every_30_s() {
    let mut s = State::default();
    let peer = PeerId::random();
    let key = (peer, "steam:msg".to_string());
    s.chans.insert(key.clone(), Chan::new(ChanState::Open));
    for _ in 0..2 {
        s.handle_chan(peer, Msg::ChanData { socket: "steam:msg".into(), channel: 0, reliable: true, data: vec![1, 2, 3] }, true);
    }
    assert_eq!((s.chans[&key].traffic.got, s.chans[&key].traffic.got_bytes), (2, 6));
    let t0 = Instant::now();
    assert!(s.traffic_report(t0).is_empty(), "the first call only starts the clock");
    assert!(s.traffic_report(t0 + TRAFFIC_EVERY / 2).is_empty());
    let lines = s.traffic_report(t0 + TRAFFIC_EVERY);
    assert_eq!(lines.len(), 1, "{lines:?}");
    for part in ["lan traffic over the last 30.0 s with", "no longer connected", "steam:msg: sent 0 (0 B, the last never), got 2 (6 B", "2 packet(s) (6 B) wait unread"] {
        assert!(lines[0].contains(part), "{part} in {}", lines[0]);
    }
    let t = s.chans[&key].traffic;
    assert_eq!((t.got, t.got_bytes), (0, 0), "counts start again");
    assert!(t.last_got.is_some(), "the times stay");
    // Unread packets keep the peer in the report; once read and quiet, it's left out.
    assert_eq!(s.traffic_report(t0 + TRAFFIC_EVERY * 2).len(), 1);
    while s.inbox.take(None, |_| true).is_some() {}
    assert!(s.traffic_report(t0 + TRAFFIC_EVERY * 3).is_empty());
    assert!(s.link_text(peer, t0, false).contains("steam:msg: sent 0"));
}

#[test]
fn spans_and_sizes_read_well() {
    assert_eq!(ago(Duration::from_millis(15)), "15 ms");
    assert_eq!(ago(Duration::from_millis(2_400)), "2400 ms");
    assert_eq!(ago(Duration::from_millis(12_340)), "12.3 s");
    assert_eq!(ago(Duration::from_secs(600)), "10 min");
    assert_eq!(size_text(512), "512 B");
    assert_eq!(size_text(4_200), "4.1 KB");
    assert_eq!(size_text(3 << 20), "3.0 MB");
}
