//! Two Photon clients against the real servers over loopback UDP: name server ->
//! master -> game server, one client on GpBinary v1.6 with Diffie-Hellman payload
//! encryption, the other on v1.8 with AuthOnce (token in an HTTP-style Init, secret
//! from EncryptionData). The client side sends what Photon's client libraries send on
//! the wire (checked against Photon's own client in tests/photon).

use super::crypto::{Aes256, Dh};
use super::enet::{self, Peer, State};
use super::host::{Host, Options};
use super::message::Proto;
use super::server::{ev, key, op};
use super::value::*;
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const APP: &str = "11111111-2222-3333-4444-555555555555";

struct Client {
    name: &'static str,
    sock: UdpSocket,
    to: SocketAddr,
    peer: Peer,
    proto: Proto,
    aes: Option<Aes256>,
    start: Instant,
    inbox: VecDeque<(Msg, bool)>,
}

impl Client {
    fn now(&self) -> i64 {
        self.start.elapsed().as_millis() as i64
    }

    fn connect(name: &'static str, to: SocketAddr, proto: Proto, hello: Option<Vec<u8>>) -> Client {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let challenge = u32::from_le_bytes(super::crypto::random_bytes(4).try_into().unwrap());
        let mut c = Client {
            name,
            sock,
            to,
            peer: Peer::connect(challenge, 2, 0),
            proto,
            aes: None,
            start: Instant::now(),
            inbox: VecDeque::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while c.peer.state != State::Connected {
            assert!(Instant::now() < deadline, "{name}: ENet connect to {to} timed out");
            c.pump();
        }
        let init = hello.unwrap_or_else(|| {
            let (major, minor) = if proto == Proto::V16 { (1, 6) } else { (1, 8) };
            let i = Init { proto_major: major, proto_minor: minor, sdk: 15, version: [4, 1, 8, 0], ipv6: false, app_id: APP.into() };
            proto.encode(&Msg::Init(i), None).unwrap()
        });
        c.peer.send(0, &init, true, false);
        let r = c.wait(|m| matches!(m, Msg::InitResponse));
        assert!(matches!(r, Msg::InitResponse));
        c
    }

    fn pump(&mut self) {
        let now = self.now();
        for d in self.peer.flush(now) {
            self.sock.send_to(&d, self.to).unwrap();
        }
        let mut buf = [0u8; 4096];
        if let Ok((n, _)) = self.sock.recv_from(&mut buf) {
            let (hdr, cmds) = enet::parse(&buf[..n]).unwrap();
            self.peer.handle(&hdr, cmds, self.now());
            while let Some(d) = self.peer.dispatch() {
                let m = self.proto.decode(&d.payload, self.aes.as_ref()).unwrap();
                self.inbox.push_back(m);
            }
        }
        let now = self.now();
        for d in self.peer.flush(now) {
            self.sock.send_to(&d, self.to).unwrap();
        }
    }

    fn wait(&mut self, want: impl Fn(&Msg) -> bool) -> Msg {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(i) = self.inbox.iter().position(|(m, _)| want(m)) {
                return self.inbox.remove(i).unwrap().0;
            }
            assert!(Instant::now() < deadline, "{}: timed out; inbox {:?}", self.name, self.inbox);
            self.pump();
        }
    }

    fn request(&mut self, code: u8, params: Params) -> Response {
        let bytes = self.proto.encode(&Msg::Request(Request { code, params }), self.aes.as_ref()).unwrap();
        self.peer.send(0, &bytes, true, false);
        match self.wait(|m| matches!(m, Msg::Response(r) if r.code == code)) {
            Msg::Response(r) => r,
            _ => unreachable!(),
        }
    }

    fn ok(&mut self, code: u8, params: Params) -> Response {
        let r = self.request(code, params);
        assert_eq!(r.ret, 0, "{}: op {code} failed: {:?}", self.name, r.debug);
        r
    }

    fn event(&mut self, code: u8) -> EventMsg {
        match self.wait(|m| matches!(m, Msg::Event(e) if e.code == code)) {
            Msg::Event(e) => e,
            _ => unreachable!(),
        }
    }

    fn raise(&mut self, code: u8, data: Value, cache: u8, reliable: bool) {
        let mut p = vec![(key::CODE, Value::Byte(code)), (key::DATA, data)];
        if cache != 0 {
            p.push((key::CACHE, Value::Byte(cache)));
        }
        let bytes = self.proto.encode(&Msg::Request(Request { code: op::RAISE_EVENT, params: p }), None).unwrap();
        self.peer.send(0, &bytes, reliable, false);
        self.pump();
    }

    /// RaiseEvent to chosen actors only (RaiseEventOptions.TargetActors).
    fn raise_to(&mut self, code: u8, data: Value, targets: &[i32], reliable: bool) {
        let p = vec![(key::CODE, Value::Byte(code)), (key::DATA, data), (key::ACTOR_LIST, Value::Ints(targets.to_vec()))];
        let bytes = self.proto.encode(&Msg::Request(Request { code: op::RAISE_EVENT, params: p }), None).unwrap();
        self.peer.send(0, &bytes, reliable, false);
        self.pump();
    }

    fn leave(mut self) {
        let now = self.now();
        for d in self.peer.disconnect(now) {
            self.sock.send_to(&d, self.to).unwrap();
        }
    }

    /// Diffie-Hellman key exchange (internal operation 0), as `EstablishEncryption`.
    fn establish_encryption(&mut self) {
        let dh = Dh::new();
        let req = Request { code: 0, params: vec![(1, Value::Bytes(dh.public.clone()))] };
        let bytes = self.proto.encode(&Msg::InternalRequest(req), None).unwrap();
        self.peer.send(0, &bytes, true, false);
        let Msg::InternalResponse(r) = self.wait(|m| matches!(m, Msg::InternalResponse(r) if r.code == 0)) else { unreachable!() };
        assert_eq!(r.ret, 0);
        let key = dh.derive(param(&r.params, 1).unwrap().as_bytes().unwrap()).unwrap();
        self.aes = Some(Aes256::new(&key));
    }
}

fn str_param(r: &Response, k: u8) -> String {
    param(&r.params, k).and_then(Value::as_str).unwrap_or_else(|| panic!("missing {k} in {r:?}")).to_owned()
}

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn http_init(proto: Proto, token: &str) -> Vec<u8> {
    let p = if proto == Proto::V16 { "GpBinaryV16" } else { "GpBinaryV18" };
    let mut v = format!("POST /?init&app={APP}&clientversion=4.1.8.0&protocol={p}&sid=1 HTTP/1.1\r\nContent-Length: 0\r\n\r\n").into_bytes();
    proto.write_value(&mut v, &Value::Str(token.into())).unwrap();
    v
}

fn start() -> (Host, Arc<Mutex<Vec<String>>>) {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let l2 = logs.clone();
    let host = Host::start(Options {
        bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        ports: [[0; 3]; 2],
        tcp_ports: [0; 3],
        ws_ports: [0; 3],
        advertise: Box::new(|_| "127.0.0.1".into()),
        log: Arc::new(move |s: &str| l2.lock().unwrap().push(s.to_owned())),
    })
    .unwrap();
    (host, logs)
}

#[test]
fn two_clients_play_through_name_master_and_game_servers() {
    let (host, logs) = start();
    let ns = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host.ports[0][0]);

    // Region ping echo on the master port.
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut ping = [0x7Du8; 13];
    ping[12] = 3;
    s.send_to(&ping, SocketAddr::new(ns.ip(), host.ports[0][1])).unwrap();
    let mut buf = [0u8; 32];
    let (n, _) = s.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], &ping);

    // --- A: v1.6, DH encryption, classic Authenticate.
    let mut a = Client::connect("A", ns, Proto::V16, None);
    let regions = a.ok(op::GET_REGIONS, vec![(key::APPLICATION_ID, Value::Str(APP.into()))]);
    let codes = param(&regions.params, key::REGION).unwrap().as_strs().unwrap();
    assert!(codes.contains(&"eu".to_string()));
    a.establish_encryption();
    let auth = a.ok(
        op::AUTHENTICATE,
        vec![
            (key::APP_VERSION, Value::Str("1.0_1.0".into())),
            (key::APPLICATION_ID, Value::Str(APP.into())),
            (key::REGION, Value::Str("eu".into())),
            (key::USER_ID, Value::Str("alice".into())),
        ],
    );
    assert_eq!(str_param(&auth, key::USER_ID), "alice");
    let master = addr(&str_param(&auth, key::ADDRESS));
    assert_eq!(master.port(), host.ports[0][1]);
    let token = str_param(&auth, key::TOKEN);
    a.leave();

    let mut a = Client::connect("A", master, Proto::V16, None);
    a.ok(op::AUTHENTICATE, vec![(key::TOKEN, Value::Str(token))]);
    a.ok(op::JOIN_LOBBY, vec![]);
    let list = a.event(ev::GAME_LIST);
    assert_eq!(param(&list.params, key::GAME_LIST), Some(&Value::Hashtable(vec![])));
    let created = a.ok(op::CREATE_GAME, vec![(key::GAME_ID, Value::Str("room1".into()))]);
    let gs = addr(&str_param(&created, key::ADDRESS));
    assert_eq!(gs.port(), host.ports[0][2]);
    let token = str_param(&created, key::TOKEN);
    a.leave();

    let mut a = Client::connect("A", gs, Proto::V16, None);
    a.ok(op::AUTHENTICATE, vec![(key::TOKEN, Value::Str(token))]);
    let room_props = vec![
        (Value::Byte(253), Value::Bool(true)),
        (Value::Byte(254), Value::Bool(true)),
        (Value::Byte(255), Value::Byte(4)),
        (Value::Byte(250), Value::Strs(vec!["map".into()])),
        (Value::Str("map".into()), Value::Str("forest".into())),
    ];
    let joined = a.ok(
        op::CREATE_GAME,
        vec![
            (key::GAME_ID, Value::Str("room1".into())),
            (key::PLAYER_PROPERTIES, Value::Hashtable(vec![(Value::Byte(255), Value::Str("Alice".into()))])),
            (key::BROADCAST, Value::Bool(true)),
            (key::GAME_PROPERTIES, Value::Hashtable(room_props)),
            (key::CLEANUP_CACHE_ON_LEAVE, Value::Bool(true)),
            (key::CHECK_USER_ON_JOIN, Value::Bool(true)),
            (key::GAME_FLAGS, Value::Int(1 | 2 | 0x20)),
        ],
    );
    assert_eq!(param(&joined.params, key::ACTOR_NR), Some(&Value::Int(1)));
    let gp = param(&joined.params, key::GAME_PROPERTIES).unwrap().as_table().unwrap().clone();
    assert_eq!(table_get(&gp, &Value::Str("map".into())), Some(&Value::Str("forest".into())));
    assert_eq!(table_get(&gp, &Value::Byte(255)), Some(&Value::Byte(4)));
    a.event(ev::JOIN);
    // A cached event for whoever joins later.
    a.raise(7, Value::Str("cached hello".into()), 4, true);

    // --- B: v1.8, AuthOnce with the token in an HTTP init and payload encryption.
    let mut b = Client::connect("B", ns, Proto::V18, None);
    let auth = b.ok(
        op::AUTHENTICATE_ONCE,
        vec![
            (key::APP_VERSION, Value::Str("1.0_1.0".into())),
            (key::APPLICATION_ID, Value::Str(APP.into())),
            (key::REGION, Value::Str("eu".into())),
            (195, Value::Byte(1)),
            (193, Value::Byte(0)),
        ],
    );
    let Some(Value::Dict(_, enc)) = param(&auth.params, key::ENCRYPTION_DATA) else { panic!("no EncryptionData: {auth:?}") };
    let secret: [u8; 32] = enc.iter().find(|(k, _)| *k == Value::Byte(1)).unwrap().1.as_bytes().unwrap().try_into().unwrap();
    let master = addr(&str_param(&auth, key::ADDRESS));
    let token = str_param(&auth, key::TOKEN);
    b.leave();

    let mut b = Client::connect("B", master, Proto::V18, Some(http_init(Proto::V18, &token)));
    let Msg::Response(r) = b.wait(|m| matches!(m, Msg::Response(r) if r.code == op::AUTHENTICATE_ONCE)) else { unreachable!() };
    assert_eq!(r.ret, 0, "{r:?}");
    b.aes = Some(Aes256::new(&secret));
    b.ok(op::JOIN_LOBBY, vec![]);
    let list = b.event(ev::GAME_LIST);
    let rooms = param(&list.params, key::GAME_LIST).unwrap().as_table().unwrap().clone();
    let room1 = table_get(&rooms, &Value::Str("room1".into())).expect("room1 listed").as_table().unwrap().clone();
    assert_eq!(table_get(&room1, &Value::Byte(252)), Some(&Value::Byte(1)));
    assert_eq!(table_get(&room1, &Value::Str("map".into())), Some(&Value::Str("forest".into())));
    let found = b.ok(
        op::JOIN_RANDOM_GAME,
        vec![(key::GAME_PROPERTIES, Value::Hashtable(vec![(Value::Str("map".into()), Value::Str("forest".into()))]))],
    );
    assert_eq!(str_param(&found, key::GAME_ID), "room1");
    let gs = addr(&str_param(&found, key::ADDRESS));
    let token = str_param(&found, key::TOKEN);
    b.leave();

    let mut b = Client::connect("B", gs, Proto::V18, Some(http_init(Proto::V18, &token)));
    let Msg::Response(r) = b.wait(|m| matches!(m, Msg::Response(r) if r.code == op::AUTHENTICATE_ONCE)) else { unreachable!() };
    assert_eq!(r.ret, 0, "{r:?}");
    b.aes = Some(Aes256::new(&secret));
    let joined = b.ok(
        op::JOIN_GAME,
        vec![
            (key::GAME_ID, Value::Str("room1".into())),
            (key::PLAYER_PROPERTIES, Value::Hashtable(vec![(Value::Byte(255), Value::Str("Bob".into()))])),
            (key::BROADCAST, Value::Bool(true)),
        ],
    );
    assert_eq!(param(&joined.params, key::ACTOR_NR), Some(&Value::Int(2)));
    assert_eq!(param(&joined.params, key::ACTOR_LIST), Some(&Value::Ints(vec![1, 2])));
    let others = param(&joined.params, key::PLAYER_PROPERTIES).unwrap().as_table().unwrap().clone();
    let alice = table_get(&others, &Value::Int(1)).unwrap().as_table().unwrap();
    assert_eq!(table_get(alice, &Value::Byte(255)), Some(&Value::Str("Alice".into())));
    // B gets the cached event from A; A sees B join.
    let cached = b.event(7);
    assert_eq!(param(&cached.params, key::ACTOR_NR), Some(&Value::Int(1)));
    assert_eq!(param(&cached.params, key::DATA), Some(&Value::Str("cached hello".into())));
    let join = a.event(ev::JOIN);
    assert_eq!(param(&join.params, key::ACTOR_NR), Some(&Value::Int(2)));

    // RaiseEvent both ways, reliable and not.
    b.raise(1, Value::Floats(vec![1.0, 2.5]), 0, true);
    let e = a.event(1);
    assert_eq!(param(&e.params, key::ACTOR_NR), Some(&Value::Int(2)));
    assert_eq!(param(&e.params, key::DATA), Some(&Value::Floats(vec![1.0, 2.5])));
    a.raise(2, Value::Custom(b'V', vec![0; 12]), 0, false);
    let e = b.event(2);
    assert_eq!(param(&e.params, key::DATA), Some(&Value::Custom(b'V', vec![0; 12])));

    // Room property change reaches everyone (BroadcastPropsChangeToAll).
    a.ok(
        op::SET_PROPERTIES,
        vec![
            (key::PROPERTIES, Value::Hashtable(vec![(Value::Str("round".into()), Value::Int(2))])),
            (key::BROADCAST, Value::Bool(true)),
        ],
    );
    for c in [&mut a, &mut b] {
        let e = c.event(ev::PROPERTIES_CHANGED);
        assert_eq!(param(&e.params, key::TARGET_ACTOR), Some(&Value::Int(0)));
    }

    // A (master client) leaves: B gets Leave with the new master.
    let r = a.ok(op::LEAVE, vec![]);
    assert_eq!(r.ret, 0);
    a.leave();
    let left = b.event(ev::LEAVE);
    assert_eq!(param(&left.params, key::ACTOR_NR), Some(&Value::Int(1)));
    assert_eq!(param(&left.params, key::MASTER_CLIENT_ID), Some(&Value::Int(2)));
    b.leave();

    std::thread::sleep(Duration::from_millis(300));
    let logs = logs.lock().unwrap().join("\n");
    assert!(logs.contains("photon: serving on 127.0.0.1"), "{logs}");
    assert!(logs.contains("created room 'room1' as actor 1"), "{logs}");
    assert!(logs.contains("joined room 'room1' as actor 2"), "{logs}");
    assert!(logs.contains("encryption established"), "{logs}");
    host.stop();
}

/// A Photon Chat client the way ChatClient connects: name server sign-in, then the
/// front end it names (Init app "chat") with the token.
fn chat_client(name: &'static str, ip: IpAddr, ns_port: u16, user: &str) -> Client {
    let mut n = Client::connect(name, SocketAddr::new(ip, ns_port), Proto::V18, None);
    let auth = n.ok(
        op::AUTHENTICATE,
        vec![(key::APPLICATION_ID, Value::Str(APP.into())), (key::APP_VERSION, Value::Str("1".into())), (key::USER_ID, Value::Str(user.into()))],
    );
    let (front, token) = (addr(&str_param(&auth, key::ADDRESS)), str_param(&auth, key::TOKEN));
    n.leave();
    let init = Init { proto_major: 1, proto_minor: 8, sdk: 15, version: [4, 1, 8, 0], ipv6: false, app_id: "chat".into() };
    let hello = Proto::V18.encode(&Msg::Init(init), None).unwrap();
    let mut c = Client::connect(name, front, Proto::V18, Some(hello));
    c.ok(op::AUTHENTICATE, vec![(key::TOKEN, Value::Str(token))]);
    c
}

/// Two chat clients: a channel message, a private one, and a friend's status.
#[test]
fn photon_chat_channels_and_private_messages() {
    use super::chat::{ev as cev, key as ckey, op as cop};
    let (host, logs) = start();
    let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let mut a = chat_client("ann", ip, host.ports[0][0], "ann");
    let mut b = chat_client("bob", ip, host.ports[0][0], "bob");
    for c in [&mut a, &mut b] {
        c.ok(cop::SUBSCRIBE, vec![(ckey::CHANNELS, Value::Strs(vec!["global".into()]))]);
        let e = c.event(cev::SUBSCRIBE);
        assert_eq!(param(&e.params, ckey::SUBSCRIBE_RESULTS), Some(&Value::Bools(vec![true])));
    }
    a.ok(cop::PUBLISH, vec![(ckey::CHANNEL, Value::Str("global".into())), (ckey::MESSAGE, Value::Str("hello".into()))]);
    let e = b.event(cev::CHAT_MESSAGES);
    assert_eq!(param(&e.params, ckey::MESSAGES), Some(&Value::Objects(vec![Value::Str("hello".into())])));
    assert_eq!(param(&e.params, ckey::SENDERS), Some(&Value::Strs(vec!["ann".into()])));
    b.ok(cop::SEND_PRIVATE, vec![(ckey::USER_ID, Value::Str("ann".into())), (ckey::MESSAGE, Value::Str("hi ann".into()))]);
    let e = a.event(cev::PRIVATE_MESSAGE);
    assert_eq!(param(&e.params, ckey::SENDER), Some(&Value::Str("bob".into())));
    a.ok(cop::ADD_FRIENDS, vec![(ckey::FRIENDS, Value::Strs(vec!["bob".into()]))]);
    let e = a.event(cev::STATUS_UPDATE);
    assert_eq!(param(&e.params, ckey::STATUS), Some(&Value::Int(super::chat::ONLINE)));
    let logs = logs.lock().unwrap().join("\n");
    assert!(logs.contains("Photon Chat"), "{logs}");
    host.stop();
}

#[test]
fn bad_tokens_are_refused_cleanly() {
    let (host, logs) = start();
    let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
    // Unknown token on the master.
    let mut d = Client::connect("tok", SocketAddr::new(ip, host.ports[0][1]), Proto::V16, None);
    let r = d.request(op::AUTHENTICATE, vec![(key::TOKEN, Value::Str("nope".into()))]);
    assert_eq!(r.ret, 32753);
    // Game server without an invitation.
    let mut n = Client::connect("ns", SocketAddr::new(ip, host.ports[0][0]), Proto::V16, None);
    let auth = n.ok(op::AUTHENTICATE, vec![(key::APPLICATION_ID, Value::Str(APP.into())), (key::APP_VERSION, Value::Str("1".into()))]);
    let token = str_param(&auth, key::TOKEN);
    let mut g = Client::connect("gs", SocketAddr::new(ip, host.ports[0][2]), Proto::V16, None);
    let r = g.request(op::AUTHENTICATE, vec![(key::TOKEN, Value::Str(token))]);
    assert_eq!(r.ret, 32761);
    // Alternative ports serve too.
    let mut alt = Client::connect("alt", SocketAddr::new(ip, host.ports[1][0]), Proto::V18, None);
    let auth = alt.ok(op::AUTHENTICATE, vec![(key::APPLICATION_ID, Value::Str(APP.into())), (key::APP_VERSION, Value::Str("1".into()))]);
    assert_eq!(addr(&str_param(&auth, key::ADDRESS)).port(), host.ports[1][1]);
    // Unsupported op: -2 with a message.
    let r = alt.request(199, vec![]);
    assert_eq!(r.ret, -2);
    let logs = logs.lock().unwrap().join("\n");
    assert!(logs.contains("unsupported operation 199"), "{logs}");
    host.stop();
}

fn to_game(name: &'static str, ns: SocketAddr, user: &str, proto: Proto, op_code: u8, room: &str, watcher: Option<&mut Option<Client>>) -> Client {
    let mut c = Client::connect(name, ns, proto, None);
    let auth = c.ok(
        op::AUTHENTICATE,
        vec![
            (key::APP_VERSION, Value::Str("v".into())),
            (key::APPLICATION_ID, Value::Str(APP.into())),
            (key::USER_ID, Value::Str(user.into())),
        ],
    );
    let (master, token) = (addr(&str_param(&auth, key::ADDRESS)), str_param(&auth, key::TOKEN));
    c.leave();
    let mut c = Client::connect(name, master, proto, None);
    c.ok(op::AUTHENTICATE, vec![(key::TOKEN, Value::Str(token))]);
    let r = c.ok(op_code, vec![(key::GAME_ID, Value::Str(room.into()))]);
    let (gs, token) = (addr(&str_param(&r, key::ADDRESS)), str_param(&r, key::TOKEN));
    if let Some(w) = watcher {
        *w = Some(c);
    } else {
        c.leave();
    }
    let mut c = Client::connect(name, gs, proto, None);
    c.ok(op::AUTHENTICATE, vec![(key::TOKEN, Value::Str(token))]);
    let mut p = vec![(key::GAME_ID, Value::Str(room.into()))];
    if op_code == op::CREATE_GAME {
        // Two players, except the Bolt room (three, to check a bystander).
        let max = if room == "bolt" { 3 } else { 2 };
        // Photon Realtime 4.1.6 (Fusion's) sends MaxPlayers as an int, older PUN a byte.
        let max = if room.starts_with("fusion") { Value::Int(max) } else { Value::Byte(max as u8) };
        p.push((key::GAME_PROPERTIES, Value::Hashtable(vec![(Value::Byte(255), max)])));
        p.push((key::EMPTY_ROOM_TTL, Value::Int(0)));
        if room == "fusion" {
            p.push((key::PLUGINS, Value::Strs(vec!["FusionPlugin".into()])));
        }
    }
    c.ok(op_code, p);
    c
}

#[test]
fn lobby_updates_room_expiry_cache_removal_and_groups() {
    let (host, _logs) = start();
    let ns = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host.ports[0][0]);
    // A watcher in the default lobby.
    let mut w = Client::connect("W", ns, Proto::V18, None);
    let auth = w.ok(op::AUTHENTICATE, vec![(key::APP_VERSION, Value::Str("v".into())), (key::APPLICATION_ID, Value::Str(APP.into()))]);
    let (master, token) = (addr(&str_param(&auth, key::ADDRESS)), str_param(&auth, key::TOKEN));
    w.leave();
    let mut w = Client::connect("W", master, Proto::V18, None);
    w.ok(op::AUTHENTICATE, vec![(key::TOKEN, Value::Str(token))]);
    w.ok(op::JOIN_LOBBY, vec![]);
    w.event(ev::GAME_LIST);

    let mut a = to_game("A", ns, "a", Proto::V16, op::CREATE_GAME, "duo", None);
    let upd = w.event(ev::GAME_LIST_UPDATE);
    let list = param(&upd.params, key::GAME_LIST).unwrap().as_table().unwrap().clone();
    assert!(table_get(&list, &Value::Str("duo".into())).is_some(), "{upd:?}");

    // Cache two events, remove one by code, then B joins and only sees the other.
    a.raise(10, Value::Int(1), 4, true);
    a.raise(11, Value::Int(2), 4, true);
    a.raise(10, Value::Null, 6, true);
    std::thread::sleep(Duration::from_millis(100));
    let mut b = to_game("B", ns, "b", Proto::V18, op::JOIN_GAME, "duo", None);
    let e = b.event(11);
    assert_eq!(param(&e.params, key::DATA), Some(&Value::Int(2)));
    std::thread::sleep(Duration::from_millis(200));
    b.pump();
    assert!(!b.inbox.iter().any(|(m, _)| matches!(m, Msg::Event(e) if e.code == 10)), "{:?}", b.inbox);

    // The room is full (max 2): a third player is refused on the master.
    let mut c = Client::connect("C", ns, Proto::V16, None);
    let auth = c.ok(op::AUTHENTICATE, vec![(key::APP_VERSION, Value::Str("v".into())), (key::APPLICATION_ID, Value::Str(APP.into()))]);
    let (master, token) = (addr(&str_param(&auth, key::ADDRESS)), str_param(&auth, key::TOKEN));
    c.leave();
    let mut c = Client::connect("C", master, Proto::V16, None);
    c.ok(op::AUTHENTICATE, vec![(key::TOKEN, Value::Str(token))]);
    assert_eq!(c.request(op::JOIN_GAME, vec![(key::GAME_ID, Value::Str("duo".into()))]).ret, 32765);
    assert_eq!(c.request(op::JOIN_RANDOM_GAME, vec![]).ret, 32760);
    assert_eq!(c.request(op::CREATE_GAME, vec![(key::GAME_ID, Value::Str("duo".into()))]).ret, 32766);
    let friends = c.ok(op::FIND_FRIENDS, vec![(1, Value::Strs(vec!["a".into(), "zed".into()]))]);
    assert_eq!(param(&friends.params, 1), Some(&Value::Bools(vec![true, false])));
    assert_eq!(param(&friends.params, 2), Some(&Value::Strs(vec!["duo".into(), String::new()])));

    // Interest groups: B listens to group 5 only; A sends to group 5 and group 6.
    b.ok(op::CHANGE_GROUPS, vec![(key::ADD, Value::Bytes(vec![5]))]);
    for (g, v) in [(6u8, 60), (5, 50)] {
        let p = vec![(key::CODE, Value::Byte(20)), (key::DATA, Value::Int(v)), (key::GROUP, Value::Byte(g))];
        let bytes = a.proto.encode(&Msg::Request(Request { code: op::RAISE_EVENT, params: p }), None).unwrap();
        a.peer.send(0, &bytes, true, false);
        a.pump();
    }
    let e = b.event(20);
    assert_eq!(param(&e.params, key::DATA), Some(&Value::Int(50)));

    // Both leave by disconnecting: the room empties (EmptyRoomTtl 0) and the watcher
    // gets it removed from the list.
    a.leave();
    b.leave();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < deadline, "room removal never reached the lobby");
        let upd = w.event(ev::GAME_LIST_UPDATE);
        let list = param(&upd.params, key::GAME_LIST).unwrap().as_table().unwrap().clone();
        if let Some(Value::Hashtable(v)) = table_get(&list, &Value::Str("duo".into())) {
            if table_get(v, &Value::Byte(251)) == Some(&Value::Bool(true)) {
                break;
            }
        }
    }
    host.stop();
}

/// Photon Bolt over Photon (udpkit.platform.photon, as in Clone Drone): the joiner says
/// hello to the master client (event 82, a string), punch info goes as event 80, and
/// with punching failed (no STUN on a LAN without internet) all Bolt traffic is relayed
/// as unreliable event 150 carrying a byte[] to one target actor, the receiver reading
/// the sender from parameter 254. Nobody else in the room may see it.
#[test]
fn bolt_relay_events_reach_only_their_target() {
    let (host, _logs) = start();
    let ns = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host.ports[0][0]);
    let mut a = to_game("A", ns, "a", Proto::V18, op::CREATE_GAME, "bolt", None);
    a.event(ev::JOIN);
    let mut b = to_game("B", ns, "b", Proto::V18, op::JOIN_GAME, "bolt", None);
    a.event(ev::JOIN);
    let mut c = to_game("C", ns, "c", Proto::V18, op::JOIN_GAME, "bolt", None);
    a.event(ev::JOIN);
    b.event(ev::JOIN);
    b.raise_to(82, Value::Str(String::new()), &[1], true);
    let hello = a.event(82);
    assert_eq!(param(&hello.params, key::ACTOR_NR), Some(&Value::Int(2)));
    b.raise_to(80, Value::Str("punch info".into()), &[1], true);
    assert_eq!(param(&a.event(80).params, key::DATA), Some(&Value::Str("punch info".into())));
    // Bolt packets both ways, unreliable.
    let packet: Vec<u8> = (0..=255u8).cycle().take(1100).collect();
    for i in 0..20u8 {
        let mut p = packet.clone();
        p[0] = i;
        a.raise_to(150, Value::Bytes(p), &[2], false);
    }
    let e = b.event(150);
    assert_eq!(param(&e.params, key::ACTOR_NR), Some(&Value::Int(1)));
    assert_eq!(param(&e.params, key::DATA).and_then(Value::as_bytes).map(|d| d.len()), Some(1100));
    b.raise_to(150, Value::Bytes(vec![9; 40]), &[1], false);
    let e = a.event(150);
    assert_eq!(param(&e.params, key::ACTOR_NR), Some(&Value::Int(2)));
    assert_eq!(param(&e.params, key::DATA), Some(&Value::Bytes(vec![9; 40])));
    // C saw none of it.
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        c.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!c.inbox.iter().any(|(m, _)| matches!(m, Msg::Event(e) if e.code == 150 || e.code == 82)), "C got another player's Bolt traffic");
    host.stop();
}

/// Photon Fusion in Host mode: the room asks for the Fusion plugin, and PeerParty's
/// answers as actor 0 (fusion.rs): Join confirmations, Start naming the host, the
/// client's and host's addresses passed across; then game data relays as event 101.
#[test]
fn fusion_host_and_client_meet_through_the_plugin() {
    use super::fusion::{self, Body, Message, Version};
    let v = Version { protocol: 6, fusion: (2, 0, 6) };
    let send = |c: &mut Client, body: Body| c.raise_to(100, Value::Bytes(fusion::encode(&Message::new(v, body))), &[0], true);
    let recv = |c: &mut Client| -> Vec<Body> {
        let e = c.event(100);
        assert_eq!(param(&e.params, key::ACTOR_NR), None, "plugin events have no sender");
        fusion::decode(param(&e.params, key::DATA).and_then(Value::as_bytes).unwrap()).into_iter().map(|m| m.body).collect()
    };
    let (host, logs) = start();
    let ns = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host.ports[0][0]);
    let mut a = to_game("A", ns, "a", Proto::V18, op::CREATE_GAME, "fusion", None);
    send(&mut a, Body::Join { kind: 1, game_mode: fusion::CLIENT_SERVER, peer_mode: fusion::PEER_SERVER, requests: 0, unique_id: None });
    assert!(matches!(recv(&mut a)[..], [Body::Join { kind: 2, .. }]));
    assert!(matches!(recv(&mut a)[..], [Body::Start { remote_server: 1, .. }]));
    send(&mut a, Body::ReflexiveInfo { actor: 1, public: [0; 3], private: [7, 0, 0], nat: 2, unique_id: None });
    let mut b = to_game("B", ns, "b", Proto::V18, op::JOIN_GAME, "fusion", None);
    send(&mut b, Body::Join { kind: 1, game_mode: fusion::CLIENT_SERVER, peer_mode: fusion::PEER_CLIENT, requests: 0, unique_id: None });
    assert!(matches!(recv(&mut b)[..], [Body::Join { kind: 2, .. }]));
    assert!(matches!(recv(&mut b)[..], [Body::Start { remote_server: 1, .. }]));
    send(&mut b, Body::ReflexiveInfo { actor: 2, public: [0; 3], private: [8, 0, 0], nat: 2, unique_id: None });
    assert!(matches!(recv(&mut a)[..], [Body::ReflexiveInfo { actor: 2, private: [8, 0, 0], .. }]));
    assert!(matches!(recv(&mut b)[..], [Body::ReflexiveInfo { actor: 1, private: [7, 0, 0], .. }]));
    // Relayed game data.
    b.raise_to(101, Value::Bytes(vec![1, 2, 3]), &[1], false);
    let e = a.event(101);
    assert_eq!(param(&e.params, key::ACTOR_NR), Some(&Value::Int(2)));
    assert_eq!(param(&e.params, key::DATA), Some(&Value::Bytes(vec![1, 2, 3])));
    let logs = logs.lock().unwrap().join("
");
    assert!(logs.contains("Fusion host joined as actor 1"), "{logs}");
    host.stop();
}

/// Fusion Shared mode end to end: two peers join, connect to the plugin's server through
/// the room (event 101 to actor 0) and get its state packets every tick.
#[test]
fn fusion_shared_peers_connect_to_the_plugin_server() {
    use super::fusion::{self, Body, Message, Version};
    let v = Version { protocol: 6, fusion: (2, 0, 6) };
    let send = |c: &mut Client, body: Body| c.raise_to(100, Value::Bytes(fusion::encode(&Message::new(v, body))), &[0], true);
    let recv = |c: &mut Client| -> Vec<Body> {
        let e = c.event(100);
        fusion::decode(param(&e.params, key::DATA).and_then(Value::as_bytes).unwrap()).into_iter().map(|m| m.body).collect()
    };
    let relay = |c: &mut Client| -> Vec<u8> {
        let e = c.event(101);
        assert_eq!(param(&e.params, key::ACTOR_NR), Some(&Value::Int(0)), "the server's packets come from actor 0");
        param(&e.params, key::DATA).and_then(Value::as_bytes).unwrap().to_vec()
    };
    let connect = |id: u8| {
        let mut b = vec![0u8; 152];
        b[0] = 1;
        b[1] = 1;
        b[8] = id;
        Value::Bytes(b)
    };
    let (host, logs) = start();
    let ns = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host.ports[0][0]);
    let mut a = to_game("A", ns, "a", Proto::V18, op::CREATE_GAME, "fusion", None);
    send(&mut a, Body::Join { kind: 1, game_mode: fusion::SHARED, peer_mode: fusion::PEER_CLIENT, requests: 0, unique_id: None });
    assert!(matches!(recv(&mut a)[..], [Body::Join { kind: 2, .. }]));
    assert!(matches!(recv(&mut a)[..], [Body::Start { requests: fusion::start_requests::CONNECT_TO_SHARED, .. }]));
    let mut b = to_game("B", ns, "b", Proto::V18, op::JOIN_GAME, "fusion", None);
    send(&mut b, Body::Join { kind: 1, game_mode: fusion::SHARED, peer_mode: fusion::PEER_CLIENT, requests: 0, unique_id: None });
    assert!(matches!(recv(&mut b)[..], [Body::Join { kind: 2, .. }]));
    assert!(matches!(recv(&mut b)[..], [Body::Start { .. }]));
    a.raise_to(101, connect(0xa1), &[0], false);
    let accepted = relay(&mut a);
    assert_eq!((accepted[0], accepted[1], accepted[16]), (1, 2, 0xa1));
    b.raise_to(101, connect(0xb2), &[0], false);
    let accepted = relay(&mut b);
    assert_eq!((accepted[0], accepted[1], accepted[16]), (1, 2, 0xb2));
    assert_ne!(accepted[24], 0);
    // State packets (notify data) follow on the server's own clock.
    let state = relay(&mut a);
    assert_eq!(state[0], 3, "a notify data packet");
    let logs = logs.lock().unwrap().join("
");
    assert!(logs.contains("Fusion Shared server up"), "{logs}");
    assert!(logs.contains("Fusion Shared player 1 is actor 2"), "{logs}");
    // Fusion's client sizes its player list from the room's MaxPlayers, sent as an int:
    // a room left at "any" made a Fusion client throw IndexOutOfRange on the Accepted and
    // shut down.
    assert!(logs.contains("created room 'fusion' as actor 1 (1/2 players)"), "{logs}");
    host.stop();
}

/// Photon's TCP transport, framed as Photon's client frames it.
struct TcpClient {
    s: std::net::TcpStream,
    proto: Proto,
}

impl TcpClient {
    fn connect(port: u16, proto: Proto) -> TcpClient {
        let s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut c = TcpClient { s, proto };
        let i = Init { proto_major: 1, proto_minor: 8, sdk: 15, version: [4, 1, 8, 0], ipv6: false, app_id: APP.into() };
        let init = proto.encode(&Msg::Init(i), None).unwrap();
        c.frame(&init);
        assert!(matches!(c.read(), Msg::InitResponse));
        c
    }

    fn frame(&mut self, msg: &[u8]) {
        use std::io::Write;
        let mut f = vec![0xFB];
        f.extend_from_slice(&((msg.len() + 7) as u32).to_be_bytes());
        f.extend_from_slice(&[0, 1]);
        f.extend_from_slice(msg);
        self.s.write_all(&f).unwrap();
    }

    fn read(&mut self) -> Msg {
        use std::io::Read;
        let mut h = [0u8; 7];
        self.s.read_exact(&mut h).unwrap();
        assert_eq!(h[0], 0xFB);
        let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
        let mut m = vec![0u8; len - 7];
        self.s.read_exact(&mut m).unwrap();
        self.proto.decode(&m, None).unwrap().0
    }

    fn request(&mut self, code: u8, params: Params) -> Response {
        let bytes = self.proto.encode(&Msg::Request(Request { code, params }), None).unwrap();
        self.frame(&bytes);
        loop {
            if let Msg::Response(r) = self.read() {
                if r.code == code {
                    return r;
                }
            }
        }
    }
}

#[test]
fn tcp_clients_sign_in_and_are_redirected_over_tcp() {
    use std::io::{Read, Write};
    let (host, logs) = start();
    assert!(host.tcp_ports.iter().all(|p| *p != 0));
    let mut n = TcpClient::connect(host.tcp_ports[0], Proto::V18);
    // Ping: 5 bytes out, 9 back with our time in the last four.
    n.s.write_all(&[0xF0, 0, 0, 0x30, 0x39]).unwrap();
    let mut pong = [0u8; 9];
    n.s.read_exact(&mut pong).unwrap();
    assert_eq!((pong[0], &pong[5..]), (0xF0, &[0u8, 0, 0x30, 0x39][..]));
    let auth = n.request(op::AUTHENTICATE, vec![(key::APPLICATION_ID, Value::Str(APP.into())), (key::APP_VERSION, Value::Str("1".into()))]);
    assert_eq!(auth.ret, 0, "{:?}", auth.debug);
    let master = addr(&str_param(&auth, key::ADDRESS));
    assert_eq!(master.port(), host.tcp_ports[1], "a TCP client is sent to the master's TCP port");
    let token = str_param(&auth, key::TOKEN);
    let mut m = TcpClient::connect(master.port(), Proto::V18);
    let r = m.request(op::AUTHENTICATE, vec![(key::TOKEN, Value::Str(token))]);
    assert_eq!(r.ret, 0);
    let r = m.request(op::CREATE_GAME, vec![(key::GAME_ID, Value::Str("tcproom".into()))]);
    assert_eq!(r.ret, 0, "{:?}", r.debug);
    assert_eq!(addr(&str_param(&r, key::ADDRESS)).port(), host.tcp_ports[2]);
    let logs = logs.lock().unwrap().join("\n");
    assert!(logs.contains("TCP"), "{logs}");
    host.stop();
}
