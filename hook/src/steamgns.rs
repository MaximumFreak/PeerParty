//! Steam's newer networking on the LAN: ISteamNetworkingSockets (connections to a
//! player's virtual port or to an IP:port, listen sockets, poll groups, socket pairs),
//! ISteamNetworkingMessages (connectionless messages by SteamID and channel) and the
//! relay / ping parts of ISteamNetworkingUtils, following Valve's Steamworks
//! documentation and headers for what each call returns and which callbacks follow.
//!
//! A connection is one LAN channel: socket "steam:gns:p2p:<virtual port>:<nonce>" or
//! "steam:gns:ip:<port>:<nonce>", data on channel 0 (reliable or not, as sent) and a
//! close reason on channel 255. Connecting asks for the channel; the other side sees a
//! Connecting connection on its listen socket until the game accepts it. IP listen
//! sockets are announced in our LAN presence, so `ConnectByIPAddress` finds the LAN
//! player listening on that port (preferring the one at that IP). Messages use socket
//! "steam:msg" with the game's channel number.
//!
//! Status changes reach the game the way it asked: a function set through
//! `k_ESteamNetworkingConfig_Callback_ConnectionStatusChanged` (globally, on the listen
//! socket or on the connection), called from the game's Steam dispatch or
//! `RunCallbacks`, else Steam's `SteamNetConnectionStatusChangedCallback_t`. The relay
//! network and authentication always report available.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Instant;

use peerparty_core::{CloseReason, Event, Lan, PeerId};

use crate::log;
use crate::net;
use crate::steamlan::{bytes, my_steam_id, peer_of, steam_id};
use crate::steamobj::{call_real, vmethod};

const PREFIX: &str = "steam:gns:";
const MSG_SOCKET: &str = "steam:msg";
const DATA: u8 = 0;
const CONTROL: u8 = 255;
const KEY_IP_PORTS: &str = "peerparty:gns:ip";
/// The ones of those a game server listens on (also in KEY_IP_PORTS, for older peers):
/// a connection to one is to the game server's SteamID, not the player's.
const KEY_GS_IP_PORTS: &str = "peerparty:gns:gsip";

const STATUS_CHANGED: i32 = 1221;
const AUTH_STATUS: i32 = 1222;
const SESSION_REQUEST: i32 = 1251;
const SESSION_FAILED: i32 = 1252;
const RELAY_STATUS: i32 = 1281;

// ESteamNetworkingConfigValue callbacks
const CB_STATUS: i32 = 201;
const CB_AUTH: i32 = 202;
const CB_RELAY: i32 = 203;
const CB_SESSION_REQUEST: i32 = 204;
const CB_SESSION_FAILED: i32 = 205;
const CONFIG_PTR: i32 = 5;
const SCOPE_GLOBAL: i32 = 1;
const SCOPE_LISTEN: i32 = 3;
const SCOPE_CONNECTION: i32 = 4;

// ESteamNetworkingConnectionState
const NONE: i32 = 0;
const CONNECTING: i32 = 1;
const CONNECTED: i32 = 3;
const CLOSED_BY_PEER: i32 = 4;
const PROBLEM: i32 = 5;

// EResult
const OK: i32 = 1;
const NO_CONNECTION: i32 = 3;
const INVALID_PARAM: i32 = 8;
const INVALID_STATE: i32 = 11;
const LIMIT_EXCEEDED: i32 = 25;

const AVAILABLE: i32 = 100;
const SEND_RELIABLE: i32 = 8;
const MAX_MESSAGE: u32 = 512 * 1024;
const END_APP_GENERIC: i32 = 1000;
const END_REMOTE_TIMEOUT: i32 = 4001;
const END_NO_CONNECTION: i32 = 5010;
const IDENTITY_STEAMID: i32 = 16;

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Identity {
    kind: i32,
    size: i32,
    data: [u8; 128],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct IpAddr {
    ipv6: [u8; 16],
    port: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ConnInfo {
    identity: Identity,
    user_data: i64,
    listen: u32,
    addr: IpAddr,
    pad: u16,
    pop_remote: u32,
    pop_relay: u32,
    state: i32,
    end_reason: i32,
    end_debug: [u8; 128],
    description: [u8; 128],
    flags: i32,
    reserved: [u32; 63],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StatusChanged {
    conn: u32,
    info: ConnInfo,
    old: i32,
}

#[repr(C)]
pub struct RealTimeStatus {
    state: i32,
    ping: i32,
    quality_local: f32,
    quality_remote: f32,
    out_packets: f32,
    out_bytes: f32,
    in_packets: f32,
    in_bytes: f32,
    send_rate: i32,
    pending_unreliable: i32,
    pending_reliable: i32,
    sent_unacked: i32,
    queue_time: i64,
    max_jitter: i32,
    reserved: [u32; 15],
}

/// Steam's SteamNetworkingMessage_t.
#[repr(C)]
pub struct Message {
    data: *mut u8,
    size: i32,
    conn: u32,
    identity: Identity,
    conn_user_data: i64,
    time: i64,
    number: i64,
    free_data: Option<unsafe extern "C" fn(*mut Message)>,
    release: Option<unsafe extern "C" fn(*mut Message)>,
    channel: i32,
    flags: i32,
    user_data: i64,
    lane: u16,
    pad: u16,
}

/// SteamNetworkingConfigValue_t. The value is a union with an int64 in it, so it's 8
/// bytes on x86 too; a pointer sits in its low half.
#[repr(C)]
struct ConfigValue {
    value: i32,
    kind: i32,
    data: u64,
}

struct Conn {
    peer: Option<PeerId>,
    socket: String,
    listen: u32,
    state: i32,
    end_reason: i32,
    end_debug: String,
    user_data: i64,
    name: String,
    group: u32,
    status_cb: usize,
    sent: i64,
    received: i64,
    addr: IpAddr,
    /// The other end of a socket pair, and what it sent us.
    pair: Option<u32>,
    local: VecDeque<Vec<u8>>,
    /// Made through a game server's interface: its status changes go to the game
    /// server's dispatch, and it is the game server's SteamID at our end.
    server: bool,
    /// The SteamID at the other end when that is not the LAN player's own (a game
    /// server they host), else 0.
    remote_id: u64,
    /// A ConnectByIPAddress made before anyone listening there was found: who it's
    /// for (the IP, if one was given, and the port) and when it gives up.
    waiting: Option<(Option<std::net::Ipv4Addr>, u16, Instant)>,
}

struct Listen {
    p2p_port: Option<i32>,
    ip_port: Option<u16>,
    status_cb: usize,
    /// A game server's: what connects to it is the game server's too.
    server: bool,
}

#[derive(Default)]
struct State {
    conns: HashMap<u32, Conn>,
    listens: HashMap<u32, Listen>,
    groups: HashSet<u32>,
    next: u32,
    /// Callback functions set globally (ESteamNetworkingConfigValue -> function).
    global: HashMap<i32, usize>,
    /// Status changes waiting for the game's dispatch.
    /// (callback, change, for the game server's dispatch)
    pending: VecDeque<(usize, StatusChanged, bool)>,
    /// Messages sessions the game has heard about, by peer.
    sessions: HashSet<PeerId>,
}

static S: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);
static ANNOUNCED: AtomicBool = AtomicBool::new(false);
/// A game server's sockets interface is in use; SERVER_ANNOUNCED once it has been told
/// the relay network and authentication are ready.
static SERVER_USED: AtomicBool = AtomicBool::new(false);
static SERVER_ANNOUNCED: AtomicBool = AtomicBool::new(false);

/// Our own SteamID on a connection: the game server's for a game server's (once it
/// has logged on), else the player's.
fn local_id(server: bool) -> u64 {
    let id = if server { crate::steamgs::my_server_id() } else { 0 };
    if id != 0 { id } else { my_steam_id() }
}

/// The SteamID at the other end of a connection.
fn remote_of(c: &Conn) -> u64 {
    match c.peer {
        _ if c.remote_id != 0 => c.remote_id,
        Some(p) => steam_id(p),
        None => local_id(c.server),
    }
}

/// Whether `this` is a game server's interface (and if so, note one is in use).
fn server_side(this: usize) -> bool {
    let server = crate::steamobj::is_server(this);
    if server {
        SERVER_USED.store(true, Ordering::Relaxed);
    }
    server
}

fn lan() -> Option<&'static Lan> {
    net::lan()
}

fn new_handle(s: &mut State) -> u32 {
    s.next += 1;
    0x5050_0000 | (s.next & 0xffff)
}

fn now_us() -> i64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as i64 + 1_000_000
}

fn steam_identity(id: u64) -> Identity {
    let mut data = [0u8; 128];
    data[..8].copy_from_slice(&id.to_le_bytes());
    Identity {
        kind: IDENTITY_STEAMID,
        size: 8,
        data,
    }
}

unsafe fn identity_steam_id(p: *const Identity) -> Option<u64> {
    if p.is_null() {
        return None;
    }
    let i = unsafe { std::ptr::read_unaligned(p) };
    (i.kind == IDENTITY_STEAMID).then(|| u64::from_le_bytes(i.data[..8].try_into().unwrap()))
}

unsafe fn peer_of_identity(p: *const Identity) -> Option<PeerId> {
    peer_of(unsafe { identity_steam_id(p) }?)
}

fn ip_addr(ip: Option<std::net::IpAddr>, port: u16) -> IpAddr {
    let mut a = IpAddr {
        port,
        ..Default::default()
    };
    match ip {
        Some(std::net::IpAddr::V4(v4)) => {
            a.ipv6[10] = 0xff;
            a.ipv6[11] = 0xff;
            a.ipv6[12..].copy_from_slice(&v4.octets());
        }
        Some(std::net::IpAddr::V6(v6)) => a.ipv6 = v6.octets(),
        None => {}
    }
    a
}

fn ipv4_of(a: &IpAddr) -> Option<std::net::Ipv4Addr> {
    let b = a.ipv6;
    (b[..10].iter().all(|x| *x == 0) && b[10] == 0xff && b[11] == 0xff)
        .then(|| std::net::Ipv4Addr::new(b[12], b[13], b[14], b[15]))
}

fn text_into(dst: &mut [u8], s: &str) {
    let n = s.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
    dst[n] = 0;
}

/// A ptr-valued config callback in an options array.
unsafe fn option_callback(n: i32, opts: *const ConfigValue, which: i32) -> usize {
    if opts.is_null() || n <= 0 {
        return 0;
    }
    unsafe { std::slice::from_raw_parts(opts, n as usize) }
        .iter()
        .find(|o| o.value == which && o.kind == CONFIG_PTR)
        .map_or(0, |o| o.data as usize)
}

fn info_of(c: &Conn) -> ConnInfo {
    let remote = remote_of(c);
    let mut info = ConnInfo {
        identity: steam_identity(remote),
        user_data: c.user_data,
        listen: c.listen,
        addr: c.addr,
        pad: 0,
        pop_remote: 0,
        pop_relay: 0,
        state: c.state,
        end_reason: c.end_reason,
        end_debug: [0; 128],
        description: [0; 128],
        flags: 0,
        reserved: [0; 63],
    };
    text_into(&mut info.end_debug, &c.end_debug);
    let desc = if c.name.is_empty() { format!("LAN {}", c.socket) } else { c.name.clone() };
    text_into(&mut info.description, &desc);
    info
}

/// Moves a connection to `state` and queues the status change for the game.
fn set_state(s: &mut State, h: u32, state: i32) {
    let global = s.global.get(&CB_STATUS).copied().unwrap_or(0);
    let Some(c) = s.conns.get_mut(&h) else { return };
    let old = c.state;
    if old == state {
        return;
    }
    c.state = state;
    let cb = if c.status_cb != 0 { c.status_cb } else { global };
    let ev = StatusChanged {
        conn: h,
        info: info_of(c),
        old,
    };
    let server = c.server;
    s.pending.push_back((cb, ev, server));
}

/// Delivers queued status changes. Runs on the game's thread, with no lock held while
/// the game's code runs (it may call straight back in).
fn flush() {
    loop {
        let next = S.lock().unwrap().pending.pop_front();
        let Some((cb, mut ev, server)) = next else { break };
        log!(
            "steam-lan gns connection {:#x} state {} -> {}",
            ev.conn,
            ev.old,
            ev.info.state
        );
        if cb != 0 {
            let f: unsafe extern "C" fn(*mut StatusChanged) = unsafe { std::mem::transmute(cb) };
            unsafe { f(&mut ev) };
        } else {
            crate::steam::inject_to(server, STATUS_CHANGED, bytes(&ev));
        }
    }
}

fn new_conn(peer: Option<PeerId>, socket: String, listen: u32, status_cb: usize, addr: IpAddr) -> Conn {
    Conn {
        peer,
        socket,
        listen,
        state: NONE,
        end_reason: 0,
        end_debug: String::new(),
        user_data: -1,
        name: String::new(),
        group: 0,
        status_cb,
        sent: 0,
        received: 0,
        addr,
        pair: None,
        local: VecDeque::new(),
        server: false,
        remote_id: 0,
        waiting: None,
    }
}

fn nonce() -> String {
    PeerId::random().hex()[..8].to_string()
}

/// Starts an outgoing connection on `socket` to `peer` (or a failed one without).
/// `server`: made through a game server's interface; `remote_id`: the SteamID there
/// when not the player's (0).
/// `wait_for`: an IP connection with nobody listening there yet waits for them (see
/// IP_WAIT) instead of failing at once.
fn connect(peer: Option<PeerId>, socket: String, cb: usize, addr: IpAddr, server: bool, remote_id: u64, wait_for: Option<(Option<std::net::Ipv4Addr>, u16)>) -> u32 {
    let mut s = S.lock().unwrap();
    let h = new_handle(&mut s);
    let mut c = new_conn(peer, socket.clone(), 0, cb, addr);
    c.server = server;
    c.remote_id = remote_id;
    s.conns.insert(h, c);
    set_state(&mut s, h, CONNECTING);
    match (peer, lan(), wait_for) {
        (Some(p), Some(lan), _) => {
            drop(s);
            lan.accept(p, &socket);
            log!("steam-lan gns connecting {h:#x} to {p} on {socket}");
        }
        (None, Some(_), Some((ip, port))) => {
            // A join right as the game starts (a "join IP" before the host's
            // presence arrived): tick() looks again until IP_WAIT is up.
            s.conns.get_mut(&h).unwrap().waiting = Some((ip, port, Instant::now() + IP_WAIT));
            log!("steam-lan gns connect {h:#x} on {socket}: nobody listening on port {port} yet, waiting up to {}s", IP_WAIT.as_secs());
        }
        _ => {
            if let Some(c) = s.conns.get_mut(&h) {
                c.end_reason = END_NO_CONNECTION;
                c.end_debug = "No LAN player there".into();
            }
            set_state(&mut s, h, PROBLEM);
            log!("steam-lan gns connect {h:#x} on {socket}: nobody on the LAN there");
        }
    }
    h
}

/// How long a ConnectByIPAddress waits for someone listening at that port to show up.
const IP_WAIT: std::time::Duration = std::time::Duration::from_secs(8);

/// IP connections waiting for a listener: connect the ones whose listener has shown up,
/// fail the ones whose time is up.
fn retry_waiting() {
    let Some(lan) = lan() else { return };
    let waiting: Vec<(u32, Option<std::net::Ipv4Addr>, u16, Instant)> = {
        let s = S.lock().unwrap();
        s.conns.iter().filter_map(|(h, c)| c.waiting.map(|(ip, port, until)| (*h, ip, port, until))).collect()
    };
    if waiting.is_empty() {
        return;
    }
    let now = Instant::now();
    for (h, ip, port, until) in waiting {
        let peer = find_ip_listener(ip, port);
        let remote = peer.map_or(0, |p| ip_listener_server(p, port));
        let mut s = S.lock().unwrap();
        let Some(c) = s.conns.get_mut(&h) else { continue };
        if c.waiting.is_none() || c.state != CONNECTING {
            c.waiting = None;
            continue;
        }
        match peer {
            Some(p) => {
                c.waiting = None;
                c.peer = Some(p);
                c.remote_id = remote;
                let socket = c.socket.clone();
                drop(s);
                lan.accept(p, &socket);
                log!("steam-lan gns connecting {h:#x} to {p} on {socket} (found after waiting)");
            }
            None if now >= until => {
                c.waiting = None;
                c.end_reason = END_NO_CONNECTION;
                c.end_debug = "No LAN player there".into();
                let socket = c.socket.clone();
                set_state(&mut s, h, PROBLEM);
                log!("steam-lan gns connect {h:#x} on {socket}: nobody on the LAN there after {}s", IP_WAIT.as_secs());
            }
            None => {}
        }
    }
}

/// Our IP listen ports, in our LAN presence for ConnectByIPAddress.
fn announce_ports(s: &State) {
    net::set_presence("gns", ports_presence(s.listens.values()));
}

fn ports_presence<'a>(listens: impl Iterator<Item = &'a Listen>) -> Vec<(String, String)> {
    let (mut all, mut gs) = (Vec::new(), Vec::new());
    for l in listens {
        if let Some(p) = l.ip_port {
            all.push(p.to_string());
            if l.server {
                gs.push(p.to_string());
            }
        }
    }
    let mut kv = Vec::new();
    if !all.is_empty() {
        kv.push((KEY_IP_PORTS.to_string(), all.join(",")));
    }
    if !gs.is_empty() {
        kv.push((KEY_GS_IP_PORTS.to_string(), gs.join(",")));
    }
    kv
}

/// The LAN player listening on IP port `port` (see `choose_listener`).
fn find_ip_listener(ip: Option<std::net::Ipv4Addr>, port: u16) -> Option<PeerId> {
    let lan = lan()?;
    let listening: Vec<Listener> = lan
        .peers()
        .into_iter()
        .filter_map(|(p, _)| {
            let presence = lan.presence(p);
            let ports = presence.iter().find(|(k, _)| k == KEY_IP_PORTS).map(|(_, v)| v.as_str())?;
            let exact = ports.split(',').any(|x| x == port.to_string());
            (exact || lists_port(ports, port)).then(|| Listener {
                peer: p,
                exact,
                at_ip: ip.is_some_and(|ip| lan.peer_has_ip(p, std::net::IpAddr::V4(ip))),
            })
        })
        .collect();
    choose_listener(&listening)
}

/// A player listening on the port asked for: `exact` when they announced that port
/// (else they announced "0", a socket on a port of the system's choosing); `at_ip`
/// when the address asked for is theirs.
#[derive(Clone, Copy, Debug)]
struct Listener {
    peer: PeerId,
    exact: bool,
    at_ip: bool,
}

/// Who an IP connection goes to: the one at the address (on that very port first),
/// else the only one on that port, else the only one on a port of the system's
/// choosing. Several and nothing to tell them apart: nobody, rather than a guess that
/// joins the wrong game (the connection waits, and fails if it stays that way).
fn choose_listener(c: &[Listener]) -> Option<PeerId> {
    if let Some(l) = c.iter().filter(|l| l.at_ip).max_by_key(|l| l.exact) {
        return Some(l.peer);
    }
    let exact: Vec<&Listener> = c.iter().filter(|l| l.exact).collect();
    let set: Vec<&Listener> = if exact.is_empty() { c.iter().collect() } else { exact };
    match set.as_slice() {
        [one] => Some(one.peer),
        _ => None,
    }
}

fn lists_port(ports: &str, port: u16) -> bool {
    ports.split(',').any(|x| x == port.to_string() || x == "0")
}

/// The game server SteamID behind `peer`'s IP listen port `port`, if a game server of
/// theirs listens there, else 0 (the player's own).
fn ip_listener_server(peer: PeerId, port: u16) -> u64 {
    let Some(lan) = lan() else { return 0 };
    let gs = lan.presence(peer).iter().any(|(k, v)| k == KEY_GS_IP_PORTS && lists_port(v, port));
    if gs { crate::steamgs::server_id_of(peer).unwrap_or(0) } else { 0 }
}

// ---- messages handed to the game ----

unsafe extern "C" fn release_message(m: *mut Message) {
    if !m.is_null() {
        drop(unsafe { Box::from_raw(m as *mut Owned) });
    }
}

#[repr(C)]
struct Owned {
    msg: Message,
    data: Vec<u8>,
}

fn hand_out(data: Vec<u8>, conn: u32, peer: u64, channel: i32, number: i64, user: i64) -> *mut Message {
    let mut o = Box::new(Owned {
        msg: Message {
            data: std::ptr::null_mut(),
            size: data.len() as i32,
            conn,
            identity: steam_identity(peer),
            conn_user_data: user,
            time: now_us(),
            number,
            free_data: None,
            release: Some(release_message),
            channel,
            flags: 0,
            user_data: 0,
            lane: 0,
            pad: 0,
        },
        data,
    });
    o.msg.data = o.data.as_mut_ptr();
    Box::into_raw(o) as *mut Message
}

/// Takes up to `max` data messages for the connections `sockets` picks.
fn receive(out: *mut *mut Message, max: i32, pick: impl Fn(&State, u32) -> bool) -> i32 {
    if out.is_null() || max <= 0 {
        return 0;
    }
    let Some(lan) = lan() else { return 0 };
    let mut n = 0;
    // Socket pairs first: their messages never touch the LAN.
    {
        let mut s = S.lock().unwrap();
        let handles: Vec<u32> = s.conns.keys().copied().filter(|h| pick(&s, *h)).collect();
        for h in handles {
            while n < max {
                let Some(c) = s.conns.get_mut(&h) else { break };
                let Some(data) = c.local.pop_front() else { break };
                c.received += 1;
                let (num, user, server) = (c.received, c.user_data, c.server);
                unsafe { *out.add(n as usize) = hand_out(data, h, local_id(server), 0, num, user) };
                n += 1;
            }
        }
    }
    while n < max {
        let sockets: HashMap<String, u32> = {
            let s = S.lock().unwrap();
            s.conns
                .iter()
                .filter(|(h, c)| c.peer.is_some() && pick(&s, **h))
                .map(|(h, c)| (c.socket.clone(), *h))
                .collect()
        };
        if sockets.is_empty() {
            break;
        }
        let Some(mut p) = lan.receive_where(Some(DATA), |sock| sockets.contains_key(sock)) else {
            break;
        };
        crate::steamlan::ids_in(p.peer, &mut p.data);
        let h = sockets[&p.socket];
        let (num, user, remote) = {
            let mut s = S.lock().unwrap();
            let Some(c) = s.conns.get_mut(&h) else { continue };
            c.received += 1;
            (c.received, c.user_data, c.remote_id)
        };
        let from = if remote != 0 { remote } else { steam_id(p.peer) };
        unsafe { *out.add(n as usize) = hand_out(p.data, h, from, 0, num, user) };
        n += 1;
    }
    n
}

/// Sends one message on a connection: (result, message number).
fn send(h: u32, data: &[u8], flags: i32) -> (i32, i64) {
    if data.len() as u32 > MAX_MESSAGE {
        return (INVALID_PARAM, 0);
    }
    let mut s = S.lock().unwrap();
    let Some(c) = s.conns.get_mut(&h) else {
        return (INVALID_PARAM, 0);
    };
    if c.state != CONNECTED && c.state != CONNECTING {
        return (NO_CONNECTION, 0);
    }
    c.sent += 1;
    let number = c.sent;
    if let Some(other) = c.pair {
        if let Some(o) = s.conns.get_mut(&other) {
            o.local.push_back(data.to_vec());
        }
        return (OK, number);
    }
    let (peer, socket) = (c.peer, c.socket.clone());
    drop(s);
    match (peer, lan()) {
        (Some(p), Some(lan)) => {
            let data = crate::steamlan::ids_out(p, data);
            lan.send(p, &socket, DATA, &data, flags & SEND_RELIABLE != 0, true, false);
            (OK, number)
        }
        _ => (NO_CONNECTION, 0),
    }
}

// ---- ISteamNetworkingSockets ----

pub fn sockets(version: &str) -> Vec<(&'static str, usize)> {
    // 001 predates identities and virtual ports; its slots stay Steam's.
    if version == "SteamNetworkingSockets001" {
        return Vec::new();
    }
    vec![
        ("CreateListenSocketIP", create_listen_ip as *const () as usize),
        ("CreateListenSocketIP~1", create_listen_ip_1 as *const () as usize),
        ("ConnectByIPAddress", connect_ip as *const () as usize),
        ("ConnectByIPAddress~1", connect_ip_1 as *const () as usize),
        ("CreateListenSocketP2P", create_listen_p2p as *const () as usize),
        ("CreateListenSocketP2P~1", create_listen_p2p_1 as *const () as usize),
        ("ConnectP2P", connect_p2p as *const () as usize),
        ("ConnectP2P~2", connect_p2p_2 as *const () as usize),
        ("AcceptConnection", accept_connection as *const () as usize),
        ("CloseConnection", close_connection as *const () as usize),
        ("CloseListenSocket", close_listen as *const () as usize),
        ("SetConnectionUserData", set_user_data as *const () as usize),
        ("GetConnectionUserData", get_user_data as *const () as usize),
        ("SetConnectionName", set_name as *const () as usize),
        ("GetConnectionName", get_name as *const () as usize),
        ("SendMessageToConnection", send_message as *const () as usize),
        ("SendMessageToConnection~4", send_message_4 as *const () as usize),
        ("SendMessages", send_messages as *const () as usize),
        ("SendMessages~3", send_messages_3 as *const () as usize),
        ("FlushMessagesOnConnection", flush_messages as *const () as usize),
        ("ReceiveMessagesOnConnection", receive_on_connection as *const () as usize),
        ("ReceiveMessagesOnListenSocket~3", receive_on_listen as *const () as usize),
        ("GetConnectionInfo", get_connection_info as *const () as usize),
        ("GetConnectionRealTimeStatus", get_realtime_status as *const () as usize),
        ("GetQuickConnectionStatus~2", get_quick_status as *const () as usize),
        ("GetDetailedConnectionStatus", get_detailed_status as *const () as usize),
        ("GetListenSocketAddress", get_listen_address as *const () as usize),
        ("CreateSocketPair", create_socket_pair as *const () as usize),
        ("ConfigureConnectionLanes", configure_lanes as *const () as usize),
        ("GetIdentity", get_identity as *const () as usize),
        ("InitAuthentication", init_authentication as *const () as usize),
        ("GetAuthenticationStatus", get_auth_status as *const () as usize),
        ("CreatePollGroup", create_poll_group as *const () as usize),
        ("DestroyPollGroup", destroy_poll_group as *const () as usize),
        ("SetConnectionPollGroup", set_poll_group as *const () as usize),
        ("ReceiveMessagesOnPollGroup", receive_on_poll_group as *const () as usize),
        ("RunCallbacks", run_callbacks as *const () as usize),
    ]
}

fn listen(p2p_port: Option<i32>, ip_port: Option<u16>, cb: usize, server: bool) -> u32 {
    let mut s = S.lock().unwrap();
    let h = new_handle(&mut s);
    s.listens.insert(
        h,
        Listen {
            p2p_port,
            ip_port,
            status_cb: cb,
            server,
        },
    );
    if ip_port.is_some() {
        announce_ports(&s);
    }
    log!(
        "steam-lan gns listen {h:#x} p2p={p2p_port:?} ip={ip_port:?}{}",
        if server { " (game server)" } else { "" }
    );
    h
}

vmethod! {
    fn create_listen_ip(this: usize, addr: *const IpAddr, n: i32, opts: *const ConfigValue) -> u32 {
        let port = if addr.is_null() { 0 } else { unsafe { std::ptr::read_unaligned(addr) }.port };
        listen(None, Some(port), unsafe { option_callback(n, opts, CB_STATUS) }, server_side(this))
    }
}

vmethod! {
    fn create_listen_ip_1(this: usize, addr: *const IpAddr) -> u32 {
        unsafe { create_listen_ip(this, addr, 0, std::ptr::null()) }
    }
}

vmethod! {
    fn connect_ip(this: usize, addr: *const IpAddr, n: i32, opts: *const ConfigValue) -> u32 {
        let a = if addr.is_null() { IpAddr::default() } else { unsafe { std::ptr::read_unaligned(addr) } };
        let ip = ipv4_of(&a);
        let peer = find_ip_listener(ip.filter(|ip| !ip.is_loopback() && !ip.is_unspecified()), a.port);
        let port = a.port;
        let remote = peer.map_or(0, |p| ip_listener_server(p, port));
        let cb = unsafe { option_callback(n, opts, CB_STATUS) };
        let wait = Some((ip.filter(|ip| !ip.is_loopback() && !ip.is_unspecified()), port));
        connect(peer, format!("{PREFIX}ip:{port}:{}", nonce()), cb, a, server_side(this), remote, wait)
    }
}

vmethod! {
    fn connect_ip_1(this: usize, addr: *const IpAddr) -> u32 {
        unsafe { connect_ip(this, addr, 0, std::ptr::null()) }
    }
}

vmethod! {
    fn create_listen_p2p(this: usize, port: i32, n: i32, opts: *const ConfigValue) -> u32 {
        listen(Some(port), None, unsafe { option_callback(n, opts, CB_STATUS) }, server_side(this))
    }
}

vmethod! {
    fn create_listen_p2p_1(this: usize, port: i32) -> u32 {
        unsafe { create_listen_p2p(this, port, 0, std::ptr::null()) }
    }
}

vmethod! {
    fn connect_p2p(this: usize, who: *const Identity, port: i32, n: i32, opts: *const ConfigValue) -> u32 {
        let peer = unsafe { peer_of_identity(who) };
        let addr = ip_addr(peer.and_then(|p| lan()?.peer_ip(p)), 0);
        // A game server's SteamID: it, not its host player, is who answers.
        let remote = unsafe { identity_steam_id(who) }.filter(|id| crate::steamgs::is_server(*id)).unwrap_or(0);
        let cb = unsafe { option_callback(n, opts, CB_STATUS) };
        connect(peer, format!("{PREFIX}p2p:{port}:{}", nonce()), cb, addr, server_side(this), remote, None)
    }
}

vmethod! {
    fn connect_p2p_2(this: usize, who: *const Identity, port: i32) -> u32 {
        unsafe { connect_p2p(this, who, port, 0, std::ptr::null()) }
    }
}

vmethod! {
    fn accept_connection(_this: usize, h: u32) -> i32 {
        let s = S.lock().unwrap();
        let Some(c) = s.conns.get(&h) else { return INVALID_PARAM };
        if c.listen == 0 || c.state != CONNECTING {
            return INVALID_STATE;
        }
        let (peer, socket) = (c.peer, c.socket.clone());
        drop(s);
        match (peer, lan()) {
            (Some(p), Some(lan)) => {
                lan.accept(p, &socket);
                log!("steam-lan gns accepted {h:#x}");
                OK
            }
            _ => NO_CONNECTION,
        }
    }
}

/// Ends a connection from our side: the peer hears why, the handle is gone.
fn close_conn(s: &mut State, h: u32, reason: i32, debug: &str) -> bool {
    let Some(c) = s.conns.remove(&h) else { return false };
    if let Some(other) = c.pair {
        if s.conns.contains_key(&other) {
            if let Some(o) = s.conns.get_mut(&other) {
                o.end_reason = reason;
                o.end_debug = debug.into();
            }
            set_state(s, other, CLOSED_BY_PEER);
        }
        return true;
    }
    if let (Some(p), Some(lan)) = (c.peer, lan()) {
        if c.state == CONNECTED || c.state == CONNECTING {
            let mut why = reason.to_le_bytes().to_vec();
            why.extend_from_slice(debug.as_bytes());
            lan.send(p, &c.socket, CONTROL, &why, true, false, false);
        }
        lan.close(p, Some(&c.socket));
        lan.clear(Some(p), Some(&c.socket));
    }
    true
}

vmethod! {
    fn close_connection(_this: usize, h: u32, reason: i32, debug: *const c_char, _linger: bool) -> bool {
        let debug = crate::read::cstr(debug as usize).unwrap_or_default();
        let closed = close_conn(&mut S.lock().unwrap(), h, reason, &debug);
        log!("steam-lan gns close {h:#x} reason={reason} {debug:?} -> {closed}");
        closed
    }
}

vmethod! {
    fn close_listen(_this: usize, h: u32) -> bool {
        let mut s = S.lock().unwrap();
        if s.listens.remove(&h).is_none() {
            return false;
        }
        // Its accepted connections close with it.
        let mine: Vec<u32> = s.conns.iter().filter(|(_, c)| c.listen == h).map(|(k, _)| *k).collect();
        for c in mine {
            close_conn(&mut s, c, END_APP_GENERIC, "Listen socket closed");
        }
        announce_ports(&s);
        true
    }
}

vmethod! {
    fn set_user_data(_this: usize, h: u32, data: i64) -> bool {
        match S.lock().unwrap().conns.get_mut(&h) {
            Some(c) => {
                c.user_data = data;
                true
            }
            None => false,
        }
    }
}

vmethod! {
    fn get_user_data(_this: usize, h: u32) -> i64 {
        S.lock().unwrap().conns.get(&h).map_or(-1, |c| c.user_data)
    }
}

vmethod! {
    fn set_name(_this: usize, h: u32, name: *const c_char) {
        if let Some(c) = S.lock().unwrap().conns.get_mut(&h) {
            c.name = crate::read::cstr(name as usize).unwrap_or_default();
        }
    }
}

vmethod! {
    fn get_name(_this: usize, h: u32, buf: *mut c_char, len: i32) -> bool {
        let Some(name) = S.lock().unwrap().conns.get(&h).map(|c| c.name.clone()) else { return false };
        if !buf.is_null() && len > 0 {
            let dst = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, len as usize) };
            text_into(dst, &name);
        }
        true
    }
}

vmethod! {
    fn send_message(_this: usize, h: u32, data: *const u8, len: u32, flags: i32, out: *mut i64) -> i32 {
        let body = if data.is_null() || len == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(data, len as usize) } };
        let (r, number) = send(h, body, flags);
        if !out.is_null() {
            unsafe { *out = number };
        }
        r
    }
}

vmethod! {
    fn send_message_4(this: usize, h: u32, data: *const u8, len: u32, flags: i32) -> i32 {
        unsafe { send_message(this, h, data, len, flags, std::ptr::null_mut()) }
    }
}

/// SendMessages takes ownership of the messages: each is released once sent, and a
/// failed one too unless the game asked to keep failures.
fn send_many(n: i32, msgs: *const *mut Message, out: *mut i64, release_failed: bool) {
    if msgs.is_null() || n <= 0 {
        return;
    }
    for i in 0..n as usize {
        let m = unsafe { *msgs.add(i) };
        if m.is_null() {
            continue;
        }
        let (conn, data, size, flags) = unsafe { ((*m).conn, (*m).data, (*m).size, (*m).flags) };
        let body = if data.is_null() || size <= 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(data, size as usize) } };
        let (r, number) = send(conn, body, flags);
        if !out.is_null() {
            unsafe { *out.add(i) = if r == OK { number } else { -(r as i64) } };
        }
        if r == OK || release_failed {
            if let Some(release) = unsafe { (*m).release } {
                unsafe { release(m) };
            }
        }
    }
}

vmethod! {
    fn send_messages(_this: usize, n: i32, msgs: *const *mut Message, out: *mut i64, delete_failed: bool) {
        send_many(n, msgs, out, delete_failed);
    }
}

vmethod! {
    fn send_messages_3(_this: usize, n: i32, msgs: *const *mut Message, out: *mut i64) {
        send_many(n, msgs, out, true);
    }
}

vmethod! {
    fn flush_messages(_this: usize, h: u32) -> i32 {
        if S.lock().unwrap().conns.contains_key(&h) { OK } else { INVALID_PARAM }
    }
}

vmethod! {
    fn receive_on_connection(_this: usize, h: u32, out: *mut *mut Message, max: i32) -> i32 {
        if !S.lock().unwrap().conns.contains_key(&h) {
            return -1;
        }
        receive(out, max, |_, c| c == h)
    }
}

vmethod! {
    fn receive_on_listen(_this: usize, l: u32, out: *mut *mut Message, max: i32) -> i32 {
        if !S.lock().unwrap().listens.contains_key(&l) {
            return -1;
        }
        receive(out, max, |s, c| s.conns.get(&c).is_some_and(|c| c.listen == l))
    }
}

vmethod! {
    fn receive_on_poll_group(_this: usize, g: u32, out: *mut *mut Message, max: i32) -> i32 {
        if !S.lock().unwrap().groups.contains(&g) {
            return -1;
        }
        receive(out, max, |s, c| s.conns.get(&c).is_some_and(|c| c.group == g))
    }
}

vmethod! {
    fn get_connection_info(_this: usize, h: u32, out: *mut ConnInfo) -> bool {
        let Some(info) = S.lock().unwrap().conns.get(&h).map(info_of) else { return false };
        if !out.is_null() {
            unsafe { out.write(info) };
        }
        true
    }
}

fn status_of(state: i32) -> RealTimeStatus {
    RealTimeStatus {
        state,
        ping: 1,
        quality_local: 1.0,
        quality_remote: 1.0,
        out_packets: 0.0,
        out_bytes: 0.0,
        in_packets: 0.0,
        in_bytes: 0.0,
        send_rate: 100_000_000,
        pending_unreliable: 0,
        pending_reliable: 0,
        sent_unacked: 0,
        queue_time: 0,
        max_jitter: 0,
        reserved: [0; 15],
    }
}

vmethod! {
    fn get_realtime_status(_this: usize, h: u32, out: *mut RealTimeStatus, lanes: i32, lane_out: *mut u8) -> i32 {
        let Some(state) = S.lock().unwrap().conns.get(&h).map(|c| c.state) else { return INVALID_PARAM };
        if !out.is_null() {
            unsafe { out.write(status_of(state)) };
        }
        // SteamNetConnectionRealTimeLaneStatus_t: 64 bytes each, all quiet.
        if !lane_out.is_null() && lanes > 0 {
            unsafe { std::ptr::write_bytes(lane_out, 0, lanes as usize * 64) };
        }
        OK
    }
}

vmethod! {
    /// The older SteamNetworkingQuickConnectionStatus shares the real-time status's
    /// fields up to the queue time; only that much is written.
    fn get_quick_status(_this: usize, h: u32, out: *mut u8) -> bool {
        let Some(state) = S.lock().unwrap().conns.get(&h).map(|c| c.state) else { return false };
        if !out.is_null() {
            let st = status_of(state);
            unsafe { std::ptr::copy_nonoverlapping(&st as *const RealTimeStatus as *const u8, out, 56) };
        }
        true
    }
}

vmethod! {
    fn get_detailed_status(_this: usize, h: u32, buf: *mut c_char, len: i32) -> i32 {
        let Some(c) = S.lock().unwrap().conns.get(&h).map(|c| format!("PeerParty LAN connection on {} (state {})", c.socket, c.state)) else {
            return -1;
        };
        if buf.is_null() || len <= c.len() as i32 {
            return c.len() as i32 + 1;
        }
        let dst = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, len as usize) };
        text_into(dst, &c);
        0
    }
}

vmethod! {
    fn get_listen_address(_this: usize, h: u32, out: *mut IpAddr) -> bool {
        let Some(port) = S.lock().unwrap().listens.get(&h).and_then(|l| l.ip_port) else { return false };
        if !out.is_null() {
            unsafe { std::ptr::write_unaligned(out, ip_addr(None, port)) };
        }
        true
    }
}

vmethod! {
    fn create_socket_pair(this: usize, a: *mut u32, b: *mut u32, _loopback: bool, _ia: *const Identity, _ib: *const Identity) -> bool {
        if a.is_null() || b.is_null() {
            return false;
        }
        let server = server_side(this);
        let mut s = S.lock().unwrap();
        let (ha, hb) = (new_handle(&mut s), new_handle(&mut s));
        for (h, other) in [(ha, hb), (hb, ha)] {
            let mut c = new_conn(None, format!("{PREFIX}pair:{h:x}"), 0, 0, IpAddr::default());
            c.pair = Some(other);
            c.state = CONNECTED;
            c.server = server;
            s.conns.insert(h, c);
        }
        unsafe {
            *a = ha;
            *b = hb;
        }
        true
    }
}

vmethod! { fn configure_lanes(_this: usize, h: u32, _n: i32, _p: *const i32, _w: *const u16) -> i32 { if S.lock().unwrap().conns.contains_key(&h) { OK } else { INVALID_PARAM } } }

vmethod! {
    fn get_identity(this: usize, out: *mut Identity) -> bool {
        let id = local_id(crate::steamobj::is_server(this));
        if !out.is_null() {
            unsafe { std::ptr::write_unaligned(out, steam_identity(id)) };
        }
        id != 0
    }
}

vmethod! { fn init_authentication(_this: usize) -> i32 { AVAILABLE } }

vmethod! {
    fn get_auth_status(_this: usize, out: *mut u8) -> i32 {
        // SteamNetAuthenticationStatus_t: availability, then a 256-byte message.
        if !out.is_null() {
            unsafe {
                std::ptr::write_bytes(out, 0, 260);
                *(out as *mut i32) = AVAILABLE;
            }
        }
        AVAILABLE
    }
}

vmethod! {
    fn create_poll_group(_this: usize) -> u32 {
        let mut s = S.lock().unwrap();
        let g = new_handle(&mut s);
        s.groups.insert(g);
        g
    }
}

vmethod! {
    fn destroy_poll_group(_this: usize, g: u32) -> bool {
        let mut s = S.lock().unwrap();
        if !s.groups.remove(&g) {
            return false;
        }
        for c in s.conns.values_mut().filter(|c| c.group == g) {
            c.group = 0;
        }
        true
    }
}

vmethod! {
    fn set_poll_group(_this: usize, h: u32, g: u32) -> bool {
        let mut s = S.lock().unwrap();
        if g != 0 && !s.groups.contains(&g) {
            return false;
        }
        match s.conns.get_mut(&h) {
            Some(c) => {
                c.group = g;
                true
            }
            None => false,
        }
    }
}

vmethod! { fn run_callbacks(_this: usize) { let _ = std::panic::catch_unwind(crate::steamlan::pump); } }

// ---- ISteamNetworkingMessages ----

pub fn messages() -> Vec<(&'static str, usize)> {
    vec![
        ("SendMessageToUser", send_to_user as *const () as usize),
        ("ReceiveMessagesOnChannel", receive_on_channel as *const () as usize),
        ("AcceptSessionWithUser", accept_session as *const () as usize),
        ("CloseSessionWithUser", close_session as *const () as usize),
        ("CloseChannelWithUser", close_channel as *const () as usize),
        ("GetSessionConnectionInfo", session_info as *const () as usize),
    ]
}

vmethod! {
    fn send_to_user(_this: usize, who: *const Identity, data: *const u8, len: u32, flags: i32, channel: i32) -> i32 {
        let (Some(peer), Some(lan)) = (unsafe { peer_of_identity(who) }, lan()) else { return NO_CONNECTION };
        if len > MAX_MESSAGE {
            return LIMIT_EXCEEDED;
        }
        let body = if data.is_null() || len == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(data, len as usize) } };
        S.lock().unwrap().sessions.insert(peer);
        let body = crate::steamlan::ids_out(peer, body);
        lan.send(peer, MSG_SOCKET, channel as u8, &body, flags & SEND_RELIABLE != 0, true, true);
        OK
    }
}

vmethod! {
    fn receive_on_channel(_this: usize, channel: i32, out: *mut *mut Message, max: i32) -> i32 {
        let Some(lan) = lan() else { return 0 };
        if out.is_null() || max <= 0 {
            return 0;
        }
        let mut n = 0;
        while n < max {
            let Some(mut p) = lan.receive_where(Some(channel as u8), |s| s == MSG_SOCKET) else { break };
            crate::steamlan::ids_in(p.peer, &mut p.data);
            unsafe { *out.add(n as usize) = hand_out(p.data, 0, steam_id(p.peer), channel, 0, 0) };
            n += 1;
        }
        n
    }
}

vmethod! {
    fn accept_session(_this: usize, who: *const Identity) -> bool {
        match (unsafe { peer_of_identity(who) }, lan()) {
            (Some(p), Some(lan)) => {
                S.lock().unwrap().sessions.insert(p);
                lan.accept(p, MSG_SOCKET);
                true
            }
            _ => false,
        }
    }
}

vmethod! {
    fn close_session(_this: usize, who: *const Identity) -> bool {
        match (unsafe { peer_of_identity(who) }, lan()) {
            (Some(p), Some(lan)) => {
                S.lock().unwrap().sessions.remove(&p);
                lan.close(p, Some(MSG_SOCKET));
                true
            }
            _ => false,
        }
    }
}

vmethod! { fn close_channel(_this: usize, who: *const Identity, _channel: i32) -> bool { unsafe { peer_of_identity(who) }.is_some() } }

fn session_conn_info(p: PeerId, state: i32) -> ConnInfo {
    let mut c = new_conn(Some(p), MSG_SOCKET.into(), 0, 0, ip_addr(lan().and_then(|l| l.peer_ip(p)), 0));
    c.state = state;
    info_of(&c)
}

vmethod! {
    fn session_info(_this: usize, who: *const Identity, info: *mut ConnInfo, status: *mut RealTimeStatus) -> i32 {
        let Some(p) = (unsafe { peer_of_identity(who) }) else { return NONE };
        let state = if S.lock().unwrap().sessions.contains(&p) { CONNECTED } else { NONE };
        unsafe {
            if !info.is_null() {
                info.write(session_conn_info(p, state));
            }
            if !status.is_null() {
                status.write(status_of(state));
            }
        }
        state
    }
}

// ---- ISteamNetworkingUtils ----

pub fn utils() -> Vec<(&'static str, usize)> {
    vec![
        ("GetRelayNetworkStatus", relay_status as *const () as usize),
        ("GetLocalPingLocation", local_ping_location as *const () as usize),
        ("EstimatePingTimeBetweenTwoLocations", ping_between as *const () as usize),
        ("EstimatePingTimeFromLocalHost", ping_from_here as *const () as usize),
        ("ConvertPingLocationToString", ping_location_string as *const () as usize),
        ("ParsePingLocationString", parse_ping_location as *const () as usize),
        ("CheckPingDataUpToDate", ping_up_to_date as *const () as usize),
        ("GetPingToDataCenter", ping_to_data_center as *const () as usize),
        ("GetDirectPingToPOP", ping_to_pop as *const () as usize),
        ("GetLocalTimestamp", local_timestamp as *const () as usize),
        ("SetConfigValue", set_config_value as *const () as usize),
    ]
}

/// SteamRelayNetworkStatus_t, all available.
fn relay_bytes() -> Vec<u8> {
    let mut b = vec![0u8; 16 + 256];
    b[0..4].copy_from_slice(&AVAILABLE.to_le_bytes());
    b[8..12].copy_from_slice(&AVAILABLE.to_le_bytes());
    b[12..16].copy_from_slice(&AVAILABLE.to_le_bytes());
    b
}

vmethod! {
    fn relay_status(_this: usize, out: *mut u8) -> i32 {
        if !out.is_null() {
            let b = relay_bytes();
            unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), out, b.len()) };
        }
        AVAILABLE
    }
}

vmethod! {
    fn local_ping_location(_this: usize, out: *mut u8) -> f32 {
        if !out.is_null() {
            unsafe { std::ptr::write_bytes(out, 0, 512) };
        }
        1.0
    }
}

vmethod! { fn ping_between(_this: usize, _a: *const u8, _b: *const u8) -> i32 { 1 } }
vmethod! { fn ping_from_here(_this: usize, _a: *const u8) -> i32 { 1 } }

vmethod! {
    fn ping_location_string(_this: usize, _loc: *const u8, buf: *mut c_char, len: i32) {
        if !buf.is_null() && len > 0 {
            let dst = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, len as usize) };
            text_into(dst, "lan");
        }
    }
}

vmethod! {
    fn parse_ping_location(_this: usize, _s: *const c_char, out: *mut u8) -> bool {
        if !out.is_null() {
            unsafe { std::ptr::write_bytes(out, 0, 512) };
        }
        true
    }
}

vmethod! { fn ping_up_to_date(_this: usize, _age: f32) -> bool { true } }

vmethod! {
    fn ping_to_data_center(_this: usize, _pop: u32, via: *mut u32) -> i32 {
        if !via.is_null() {
            unsafe { *via = 0 };
        }
        1
    }
}

vmethod! { fn ping_to_pop(_this: usize, _pop: u32) -> i32 { 1 } }
vmethod! { fn local_timestamp(_this: usize) -> i64 { now_us() } }

vmethod! {
    /// Callback functions for our objects (or global ones) are kept here; everything
    /// else, and the global ones too, still goes to Steam.
    fn set_config_value(this: usize, value: i32, scope: i32, obj: isize, kind: i32, arg: *const u8) -> bool {
        if (CB_STATUS..=CB_SESSION_FAILED).contains(&value) && kind == CONFIG_PTR {
            let f = if arg.is_null() { 0 } else { unsafe { *(arg as *const usize) } };
            let mut s = S.lock().unwrap();
            let h = obj as u32;
            match scope {
                SCOPE_GLOBAL => {
                    s.global.insert(value, f);
                }
                SCOPE_LISTEN if s.listens.contains_key(&h) => {
                    if value == CB_STATUS {
                        s.listens.get_mut(&h).unwrap().status_cb = f;
                    }
                    return true;
                }
                SCOPE_CONNECTION if s.conns.contains_key(&h) => {
                    if value == CB_STATUS {
                        s.conns.get_mut(&h).unwrap().status_cb = f;
                    }
                    return true;
                }
                _ => {}
            }
        }
        call_real!(this, "SetConfigValue", fn(i32, i32, isize, i32, *const u8) -> bool, true, value, scope, obj, kind, arg)
    }
}

// ---- LAN events ----

/// The game-side callback for `which`, set globally, or 0 for Steam's callback.
fn global_cb(which: i32) -> usize {
    S.lock().unwrap().global.get(&which).copied().unwrap_or(0)
}

fn post(which: i32, id: i32, data: Vec<u8>) {
    post_to(false, which, id, data);
}

/// `post`, for the game server's dispatch when `server`.
fn post_to(server: bool, which: i32, id: i32, mut data: Vec<u8>) {
    let f = global_cb(which);
    if f != 0 {
        let f: unsafe extern "C" fn(*mut u8) = unsafe { std::mem::transmute(f) };
        unsafe { f(data.as_mut_ptr()) };
    } else {
        crate::steam::inject_to(server, id, data);
    }
}

fn announce_ready(server: bool) {
    post_to(server, CB_RELAY, RELAY_STATUS, relay_bytes());
    let mut auth = vec![0u8; 260];
    auth[..4].copy_from_slice(&AVAILABLE.to_le_bytes());
    post_to(server, CB_AUTH, AUTH_STATUS, auth);
}

/// Runs from the game's Steam dispatch: close reasons that arrived, then status
/// changes waiting for the game.
pub fn tick() {
    if !ANNOUNCED.load(Ordering::Relaxed)
        && (crate::steamobj::seen("SteamNetworkingSockets").is_some()
            || crate::steamobj::seen("SteamNetworkingUtils").is_some())
    {
        ANNOUNCED.store(true, Ordering::Relaxed);
        announce_ready(false);
    }
    // A dedicated server never fetches the game's interfaces: tell its own dispatch.
    if SERVER_USED.load(Ordering::Relaxed) && !SERVER_ANNOUNCED.swap(true, Ordering::Relaxed) {
        announce_ready(true);
    }
    if let Some(lan) = lan() {
        while let Some(p) = lan.receive_where(Some(CONTROL), |s| s.starts_with(PREFIX)) {
            let mut s = S.lock().unwrap();
            let h = s.conns.iter().find(|(_, c)| c.peer == Some(p.peer) && c.socket == p.socket).map(|(h, _)| *h);
            if let (Some(h), true) = (h, p.data.len() >= 4) {
                let c = s.conns.get_mut(&h).unwrap();
                c.end_reason = i32::from_le_bytes(p.data[..4].try_into().unwrap());
                c.end_debug = String::from_utf8_lossy(&p.data[4..]).into_owned();
            }
        }
    }
    retry_waiting();
    flush();
}

pub fn on_event(ev: &Event) {
    match ev {
        Event::ChanRequest { peer, socket } if socket.starts_with(PREFIX) => incoming(*peer, socket),
        Event::ChanOpen { peer, socket } if socket.starts_with(PREFIX) => {
            let mut s = S.lock().unwrap();
            if let Some(h) = find(&s, *peer, socket) {
                set_state(&mut s, h, CONNECTED);
            }
        }
        Event::ChanClosed { peer, socket, reason } if socket.starts_with(PREFIX) => {
            let mut s = S.lock().unwrap();
            let Some(h) = find(&s, *peer, socket) else { return };
            let c = s.conns.get_mut(&h).unwrap();
            let (state, end) = match reason {
                CloseReason::Lost => (PROBLEM, END_REMOTE_TIMEOUT),
                _ if c.state == CONNECTING && c.listen == 0 => (CLOSED_BY_PEER, END_NO_CONNECTION),
                _ => (CLOSED_BY_PEER, END_APP_GENERIC),
            };
            if c.end_reason == 0 {
                c.end_reason = end;
            }
            set_state(&mut s, h, state);
        }
        Event::ChanRequest { peer, socket } if socket == MSG_SOCKET => {
            log!("steam-lan messages session request from {peer}");
            post(CB_SESSION_REQUEST, SESSION_REQUEST, bytes(&steam_identity(steam_id(*peer))));
        }
        Event::ChanClosed {
            peer,
            socket,
            reason: CloseReason::Lost,
        } if socket == MSG_SOCKET => {
            S.lock().unwrap().sessions.remove(peer);
            let mut info = session_conn_info(*peer, PROBLEM);
            info.end_reason = END_REMOTE_TIMEOUT;
            post(CB_SESSION_FAILED, SESSION_FAILED, bytes(&info));
        }
        _ => {}
    }
}

fn find(s: &State, peer: PeerId, socket: &str) -> Option<u32> {
    s.conns
        .iter()
        .find(|(_, c)| c.peer == Some(peer) && c.socket == socket)
        .map(|(h, _)| *h)
}

/// A LAN player connecting to one of our listen sockets: a Connecting connection
/// until the game accepts it, or turned away if nothing listens there.
fn incoming(peer: PeerId, socket: &str) {
    let Some(lan) = lan() else { return };
    let mut parts = socket[PREFIX.len()..].split(':');
    let (kind, port) = (parts.next().unwrap_or(""), parts.next().and_then(|p| p.parse::<i64>().ok()));
    let mut s = S.lock().unwrap();
    let listen = s
        .listens
        .iter()
        .find(|(_, l)| match (kind, port) {
            ("p2p", Some(p)) => l.p2p_port == Some(p as i32),
            ("ip", Some(p)) => l.ip_port.is_some_and(|lp| lp == p as u16 || lp == 0),
            _ => false,
        })
        .map(|(h, l)| (*h, l.status_cb, l.server));
    let Some((listen, cb, server)) = listen else {
        drop(s);
        log!("steam-lan gns {peer} asked for {socket}; nothing listens there");
        lan.close(peer, Some(socket));
        return;
    };
    let cb = if cb != 0 { cb } else { s.global.get(&CB_STATUS).copied().unwrap_or(0) };
    let addr = ip_addr(lan.peer_ip(peer), if kind == "ip" { port.unwrap_or(0) as u16 } else { 0 });
    let h = new_handle(&mut s);
    let mut c = new_conn(Some(peer), socket.into(), listen, cb, addr);
    c.server = server;
    s.conns.insert(h, c);
    set_state(&mut s, h, CONNECTING);
    log!(
        "steam-lan gns incoming {h:#x} from {peer} on {socket}{}",
        if server { " (game server)" } else { "" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ip_connection_goes_to_the_listener_it_names_or_to_nobody() {
        let (a, b, c) = (PeerId([1; 16]), PeerId([2; 16]), PeerId([3; 16]));
        let l = |peer, exact, at_ip| Listener { peer, exact, at_ip };
        assert_eq!(choose_listener(&[]), None);
        assert_eq!(choose_listener(&[l(a, true, false)]), Some(a), "the only one");
        assert_eq!(choose_listener(&[l(a, true, false), l(b, true, true)]), Some(b), "the one at the address");
        assert_eq!(choose_listener(&[l(a, false, true), l(b, true, true)]), Some(b), "on that very port first");
        assert_eq!(choose_listener(&[l(a, true, false), l(b, true, false)]), None, "two, nothing to tell them apart");
        assert_eq!(choose_listener(&[l(a, false, false), l(b, true, false)]), Some(b), "an announced port beats a chosen one");
        assert_eq!(choose_listener(&[l(a, false, false)]), Some(a), "a port of the system's choosing, alone");
        assert_eq!(choose_listener(&[l(a, false, false), l(c, false, false)]), None);
    }

    #[test]
    fn structs_have_steams_layout() {
        assert_eq!(size_of::<Identity>(), 136);
        assert_eq!(size_of::<IpAddr>(), 18);
        assert_eq!(size_of::<ConnInfo>(), 696);
        assert_eq!(std::mem::offset_of!(ConnInfo, listen), 144);
        assert_eq!(std::mem::offset_of!(ConnInfo, state), 176);
        assert_eq!(std::mem::offset_of!(ConnInfo, end_debug), 184);
        assert_eq!(std::mem::offset_of!(StatusChanged, info), 8);
        assert_eq!(size_of::<StatusChanged>(), 712);
        assert_eq!(size_of::<RealTimeStatus>(), 120);
        assert_eq!(std::mem::offset_of!(RealTimeStatus, queue_time), 48);
        assert_eq!(size_of::<ConfigValue>(), 16);
        let p = size_of::<usize>();
        assert_eq!(std::mem::offset_of!(Message, identity), p + 8);
        assert_eq!(std::mem::offset_of!(Message, conn_user_data), 152);
        assert_eq!(std::mem::offset_of!(Message, free_data), 176);
        assert_eq!(std::mem::offset_of!(Message, channel), 176 + 2 * p);
    }

    #[test]
    fn ipv4_round_trips_through_steams_mapped_form() {
        let a = ip_addr(Some("192.168.1.69".parse().unwrap()), 27015);
        assert_eq!(ipv4_of(&a), Some("192.168.1.69".parse().unwrap()));
        assert_eq!({ a.port }, 27015);
    }

    #[test]
    fn socket_pairs_carry_messages_between_their_ends() {
        let (mut a, mut b) = (0u32, 0u32);
        assert!(unsafe { create_socket_pair(0, &mut a, &mut b, true, std::ptr::null(), std::ptr::null()) });
        assert_eq!(send(a, b"ping", SEND_RELIABLE).0, OK);
        let mut out = [std::ptr::null_mut::<Message>(); 4];
        // Without a LAN, only the pair's own queue is read.
        let n = {
            let mut s = S.lock().unwrap();
            let c = s.conns.get_mut(&b).unwrap();
            let data = c.local.pop_front().unwrap();
            out[0] = hand_out(data, b, 0, 0, 1, 0);
            1
        };
        assert_eq!(n, 1);
        let m = unsafe { &*out[0] };
        assert_eq!(unsafe { std::slice::from_raw_parts(m.data, m.size as usize) }, b"ping");
        unsafe { (m.release.unwrap())(out[0]) };
    }

    /// A connection in the table, as `connect` or `incoming` would make it.
    fn add_conn(server: bool, remote_id: u64) -> u32 {
        add_conn_to(&mut S.lock().unwrap(), server, remote_id)
    }

    /// `add_conn` under a lock the caller holds.
    fn add_conn_to(s: &mut State, server: bool, remote_id: u64) -> u32 {
        let h = new_handle(s);
        let mut c = new_conn(None, format!("{PREFIX}test:{h:x}"), 0, 0, IpAddr::default());
        c.server = server;
        c.remote_id = remote_id;
        s.conns.insert(h, c);
        set_state(s, h, CONNECTING);
        h
    }

    fn injected_for(h: u32) -> Vec<bool> {
        crate::steam::injected()
            .into_iter()
            .filter(|(id, _, data)| *id == STATUS_CHANGED && data[..4] == h.to_le_bytes())
            .map(|(_, server, _)| server)
            .collect()
    }

    #[test]
    fn a_game_servers_status_changes_go_to_its_own_dispatch() {
        // A Steamworks.NET dedicated server reads them on the game server's
        // pipe only: queued for the game's, it never accepted anyone.
        // Looked at under the lock they were queued under: tests run at once, and any
        // other's `flush` delivers these too (the outcome below is the same either way).
        let (srv, game) = {
            let mut s = S.lock().unwrap();
            let (srv, game) = (add_conn_to(&mut s, true, 0), add_conn_to(&mut s, false, 0));
            assert!(s.pending.iter().any(|(_, ev, server)| ev.conn == srv && *server));
            assert!(s.pending.iter().any(|(_, ev, server)| ev.conn == game && !*server));
            (srv, game)
        };
        flush();
        // Classic dispatch in tests: the game server's registered callbacks get it.
        assert_eq!(injected_for(srv), vec![true]);
        assert_eq!(injected_for(game), vec![false]);
    }

    #[test]
    fn a_connection_to_a_game_server_shows_the_servers_steamid() {
        let gs = 0x0140_0000_0000_1234u64;
        let h = add_conn(false, gs);
        let info = info_of(&S.lock().unwrap().conns[&h]);
        assert_eq!(unsafe { identity_steam_id(&info.identity) }, Some(gs));
        flush();
    }

    #[test]
    fn a_game_servers_ip_ports_are_announced_apart() {
        let l = |ip_port: Option<u16>, server| Listen { p2p_port: None, ip_port, status_cb: 0, server };
        let ls = [l(Some(2456), true), l(Some(7777), false), l(None, true)];
        assert_eq!(
            ports_presence(ls.iter()),
            vec![
                (KEY_IP_PORTS.to_string(), "2456,7777".to_string()),
                (KEY_GS_IP_PORTS.to_string(), "2456".to_string()),
            ]
        );
        // No game server: exactly what older copies announce.
        assert_eq!(ports_presence(ls[1..2].iter()), vec![(KEY_IP_PORTS.to_string(), "7777".to_string())]);
        assert!(ports_presence([].iter()).is_empty());
        assert!(lists_port("2456,7777", 7777) && lists_port("0", 2456) && !lists_port("24560", 2456));
    }

    #[test]
    fn unknown_objects_arent_game_server_interfaces() {
        assert!(!crate::steamobj::is_server(0));
        assert!(!crate::steamobj::is_server(0x1234));
    }
}
