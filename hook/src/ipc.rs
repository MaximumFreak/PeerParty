//! The pipe to the peerparty.exe that started the game, which stays running beside it
//! and draws the overlay (launcher/src/host). Two one-way pipes, so a read waiting on
//! one never holds up a write on the other: `<PEERPARTY_PIPE>-up` carries our messages
//! to the launcher, `<PEERPARTY_PIPE>-down` its commands to us. Each message is one line
//! of JSON.
//!
//! Up: `state` (who we are, LAN players, the lobby we're in, invites waiting for the
//! player, settings), sent whenever it changes and every few seconds; `open` (the game
//! asked for an overlay page: its invite dialog, its friends list); `log`.
//!
//! Down: `invite` (one player or everyone), `accept` / `decline` (a waiting invite),
//! `join` (ask a player to invite us), `set` (a setting changed), `overlay` (shown or
//! hidden: games hear it as their SDK's overlay event, and controllers stop reaching
//! the game while it shows).
//!
//! Without `PEERPARTY_PIPE` (a game started some other way) none of this runs, and the
//! game calls that would open the overlay do what they did before it existed.

use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use peerparty_core::PeerId;

use crate::json::Json;
use crate::log;

static START: Once = Once::new();
static OUT: Mutex<Option<Sender<String>>> = Mutex::new(None);
static CONNECTED: AtomicBool = AtomicBool::new(false);

/// How often the state is checked for changes.
const STATE_EVERY: Duration = Duration::from_millis(250);
/// The state goes out at least this often, so a launcher that missed one catches up.
const HEARTBEAT: Duration = Duration::from_secs(5);

/// Whether the launcher that started the game has the overlay on (it named a pipe).
pub fn enabled() -> bool {
    std::env::var_os("PEERPARTY_PIPE").is_some()
}

/// Whether a launcher is listening (the overlay can be shown).
pub fn connected() -> bool {
    CONNECTED.load(Ordering::Relaxed)
}

/// Starts the pipe threads if the launcher set `PEERPARTY_PIPE`.
pub fn start() {
    let Ok(base) = std::env::var("PEERPARTY_PIPE") else { return };
    START.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("peerparty-ipc".into())
            .spawn(move || run(base));
    });
}

/// This process started the game it was launching (child.rs): let the launcher go, so
/// the game's own copy of the hook takes the overlay. Our writer ends when its sender
/// goes, closing our end; the launcher talks to the newest connection.
pub fn hand_over() {
    HANDED_OVER.store(true, Ordering::Relaxed);
    if CONNECTED.swap(false, Ordering::Relaxed) {
        log!("ipc: handing the launcher over to the game this process started");
    }
    *OUT.lock().unwrap() = None;
}

static HANDED_OVER: AtomicBool = AtomicBool::new(false);

/// Whether this process has let the launcher go to a program it started.
pub fn handed_over() -> bool {
    HANDED_OVER.load(Ordering::Relaxed)
}

/// Sends one message to the launcher (dropped when there's none).
pub fn send(msg: Json) {
    if let Some(tx) = OUT.lock().unwrap().as_ref() {
        let _ = tx.send(msg.to_string());
    }
}

/// Asks the launcher to show an overlay page. False when there's no launcher to ask.
/// A short message for the player about something they just did (an invite that went,
/// or couldn't): the overlay shows it, or a notification while the overlay is closed.
pub fn note(title: &str, detail: &str) {
    if connected() {
        send(Json::obj(vec![("t", Json::str("note")), ("title", Json::str(title)), ("detail", Json::str(detail))]));
    }
}

pub fn open(page: &str, peer: Option<PeerId>) -> bool {
    if !connected() {
        return false;
    }
    log!("overlay: the game asked for the {page} page");
    send(Json::obj(vec![
        ("t", Json::str("open")),
        ("page", Json::str(page)),
        ("peer", peer.map(|p| Json::str(p.hex())).unwrap_or(Json::Null)),
    ]));
    true
}

fn open_pipe(name: &str, write: bool) -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_FLAG_WRITE_THROUGH keeps small writes from waiting in a buffer.
    std::fs::OpenOptions::new()
        .read(!write)
        .write(write)
        .custom_flags(0x8000_0000)
        .open(name)
        .ok()
}

fn run(base: String) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let (up, down) = loop {
        if HANDED_OVER.load(Ordering::Relaxed) {
            return;
        }
        let up = open_pipe(&format!("{base}-up"), true);
        let down = open_pipe(&format!("{base}-down"), false);
        if let (Some(u), Some(d)) = (up, down) {
            break (u, d);
        }
        if Instant::now() > deadline {
            log!("ipc: no launcher at {base}; the overlay is off");
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    log!("ipc: connected to the launcher at {base}");
    let (tx, rx) = mpsc::channel::<String>();
    *OUT.lock().unwrap() = Some(tx);
    CONNECTED.store(true, Ordering::Relaxed);
    let _ = std::thread::Builder::new()
        .name("peerparty-ipc-out".into())
        .spawn(move || writer(up, rx));
    let reader = BufReader::new(down);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if HANDED_OVER.load(Ordering::Relaxed) {
            return;
        }
        if let Some(msg) = Json::parse(&line) {
            command(&msg);
        }
    }
    if HANDED_OVER.load(Ordering::Relaxed) {
        return;
    }
    log!("ipc: the launcher went away");
    CONNECTED.store(false, Ordering::Relaxed);
    *OUT.lock().unwrap() = None;
    crate::overlay::set_visible(false);
    crate::overlay::pads_released();
}

fn writer(mut up: std::fs::File, rx: Receiver<String>) {
    let mut last_state = String::new();
    let mut last_sent = Instant::now() - HEARTBEAT;
    loop {
        match rx.recv_timeout(STATE_EVERY) {
            Ok(msg) => {
                if up.write_all(msg.as_bytes()).and_then(|_| up.write_all(b"\n")).is_err() {
                    return;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        let state = std::panic::catch_unwind(state).unwrap_or_default();
        if state.is_empty() || (state == last_state && last_sent.elapsed() < HEARTBEAT) {
            continue;
        }
        if up.write_all(state.as_bytes()).and_then(|_| up.write_all(b"\n")).is_err() {
            return;
        }
        last_state = state;
        last_sent = Instant::now();
    }
}

fn peer_arg(msg: &Json, key: &str) -> Option<PeerId> {
    msg.get(key).as_str().and_then(PeerId::from_hex)
}

fn command(msg: &Json) {
    let t = msg.get("t").as_str().unwrap_or_default();
    match t {
        "invite" => {
            let to = peer_arg(msg, "to");
            std::thread::spawn(move || crate::invite::to(to));
        }
        "accept" => {
            if let Some(id) = msg.get("id").as_f64() {
                crate::invite::accept(id as u64);
            }
        }
        "paste" => {
            // Type the clipboard's room code into the game (the overlay is closing).
            crate::invite::type_clipboard();
        }
        "copy" => {
            if let Some(text) = msg.get("text").as_str().and_then(crate::typing::clean_code) {
                crate::typing::set_clipboard(&text);
            }
        }
        "decline" => {
            if let Some(id) = msg.get("id").as_f64() {
                crate::invite::decline(id as u64);
            }
        }
        "join" => {
            if let Some(p) = peer_arg(msg, "peer") {
                crate::invite::request_join(p);
            }
        }
        "block" => {
            if let Some(p) = peer_arg(msg, "peer")
                && let Some(lan) = crate::net::lan_if_started()
            {
                match lan.block_peer(p) {
                    Ok(fp) => log!("ipc: blocked public player {p} ({fp})"),
                    Err(e) => log!("ipc: can't block {p}: {e}"),
                }
            }
        }
        "set" => {
            let key = msg.get("key").as_str().unwrap_or_default();
            let value = msg.get("value").as_str().unwrap_or_default();
            if key == "internet" {
                set_internet(value);
            } else {
                crate::settings::set(key, value);
            }
        }
        "overlay" => {
            let visible = matches!(msg.get("visible"), Json::Bool(true));
            if let Some(why) = msg.get("why").as_str().filter(|_| visible && !crate::overlay::visible()) {
                log!("overlay: opened by {why}");
            }
            crate::overlay::set_visible_holding(visible, matches!(msg.get("pads_held"), Json::Bool(true)));
        }
        // The buttons down when the overlay closed are up.
        "pads_held" => {
            if !matches!(msg.get("on"), Json::Bool(true)) {
                crate::overlay::pads_released();
            }
        }
        "note" => {
            // A line from the overlay host for the game's log.
            if let Some(text) = msg.get("text").as_str() {
                log!("{}", text.chars().take(300).collect::<String>());
            }
        }
        "frame" => {
            if let Some(name) = msg.get("name").as_str() {
                crate::overlay::frame_ready(name);
            }
        }
        other => log!("ipc: unknown command {other:?}"),
    }
}

/// The overlay's internet switch: the room and public sessions on or off, live. Off
/// keeps LAN players; it takes a moment (sessions say goodbye), so not on the pipe's thread.
fn set_internet(value: &str) {
    let on = !matches!(value.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no");
    let Some(lan) = crate::net::lan_if_started() else {
        // Not started yet: it starts that way.
        peerparty_core::choose_internet(on);
        log!("ipc: internet play will start {}", if on { "on" } else { "off" });
        return;
    };
    peerparty_core::choose_internet(on);
    let _ = std::thread::Builder::new().name("peerparty-internet".into()).spawn(move || {
        if lan.set_internet(on) {
            log!("ipc: internet play switched {}", if on { "on" } else { "off" });
        }
    });
}

/// Other stores' overlays loaded in the game: their hotkeys stay theirs.
fn foreign_overlays() -> Vec<&'static str> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    let loaded = |n: &str| {
        let w: Vec<u16> = n.encode_utf16().chain(Some(0)).collect();
        !unsafe { GetModuleHandleW(w.as_ptr()) }.is_null()
    };
    let mut out = Vec::new();
    if loaded("GameOverlayRenderer64.dll") || loaded("GameOverlayRenderer.dll") {
        out.push("steam");
    }
    if loaded("EOSOVH-Win64-Shipping.dll") || loaded("EOSOVH-Win32-Shipping.dll") {
        out.push("epic");
    }
    if loaded("igo64.dll") || loaded("igo32.dll") {
        out.push("ea");
    }
    out
}

/// What the overlay shows, as one line of JSON.
fn state() -> String {
    let mut fields: Vec<(&str, Json)> = vec![("t", Json::str("state"))];
    fields.push(("pid", Json::Num(f64::from(std::process::id()))));
    fields.push(("lan", Json::Bool(crate::net::enabled())));
    let sdks: Vec<Json> = crate::net::fronts().iter().map(|f| Json::str(format!("{f:?}"))).collect();
    fields.push(("sdks", Json::Arr(sdks)));
    // Whether the game joins by room code (Photon): the overlay offers to copy and paste codes.
    fields.push(("codes", Json::Bool(crate::photon::in_use())));
    fields.push(("can_invite", Json::Bool(crate::invite::can_invite())));
    fields.push(("invite_note", Json::str(crate::invite::why_not().unwrap_or_default())));
    let foreign: Vec<Json> = foreign_overlays().into_iter().map(Json::str).collect();
    fields.push(("foreign", Json::Arr(foreign)));
    fields.push(("present", Json::str(crate::present::api())));
    fields.push((
        "settings",
        Json::obj(vec![
            ("auto_accept", Json::Bool(crate::settings::auto_accept())),
            ("invite_buttons", Json::Num(f64::from(crate::settings::invite_buttons()))),
        ]),
    ));
    let lan = crate::net::lan_if_started();
    // The overlay's internet switch: whether it's on, and whether there's anything
    // (a room code or public sessions) for it to switch.
    fields.push(("internet_on", Json::Bool(lan.map_or_else(peerparty_core::internet_wanted, |l| l.internet_on()))));
    fields.push(("internet_configured", Json::Bool(lan.map_or_else(crate::ipfwd::internet_configured, |l| l.internet_configured()))));
    let name_of = |p: PeerId| lan.and_then(|l| l.peer_name(p)).unwrap_or_else(|| format!("Player {}", &p.hex()[..6]));
    if let Some(l) = lan {
        fields.push(("me", Json::obj(vec![("id", Json::str(l.me().hex())), ("name", Json::str(l.name()))])));
    }
    let here = crate::invite::current_lobby();
    if let Some(lobby) = &here {
        let members: Vec<Json> = lobby
            .members
            .iter()
            .map(|m| {
                Json::obj(vec![
                    ("id", Json::str(m.id.hex())),
                    ("name", Json::str(if lan.is_some_and(|l| l.me() == m.id) { lan.map(|l| l.name().to_string()).unwrap_or_default() } else { name_of(m.id) })),
                    ("owner", Json::Bool(m.id == lobby.owner)),
                ])
            })
            .collect();
        fields.push((
            "lobby",
            Json::obj(vec![
                ("id", Json::str(lobby.id.clone())),
                ("kind", Json::str(lobby.namespace.clone())),
                ("max", Json::Num(f64::from(lobby.max_members))),
                ("members", Json::Arr(members)),
            ]),
        ));
    }
    let peers: Vec<Json> = lan
        .map(|l| l.peers())
        .unwrap_or_default()
        .into_iter()
        .map(|(p, name)| {
            let with_me = here.as_ref().is_some_and(|h| h.member(p).is_some());
            let presence = lan.map(|l| l.presence(p)).unwrap_or_default();
            let status = presence
                .iter()
                .find(|(k, _)| k == "steam_display" || k == "status")
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            // The overlay may show a LAN peer's private address, nothing else: internet
            // peers (rooms, public players) never have theirs on screen.
            let trust = lan.and_then(|l| l.peer_trust(p));
            let ip = lan
                .filter(|_| trust == Some(peerparty_core::Trust::Lan))
                .and_then(|l| l.peer_direct_ip(p))
                .filter(|ip| peerparty_core::inet::redact::is_local(*ip))
                .map(|ip| ip.to_string())
                .unwrap_or_default();
            let via = match trust {
                Some(peerparty_core::Trust::Public) => "public",
                Some(peerparty_core::Trust::Room) => "room",
                _ => "lan",
            };
            let fp = lan.and_then(|l| l.peer_key(p)).unwrap_or_default();
            Json::obj(vec![
                ("id", Json::str(p.hex())),
                ("name", Json::str(name)),
                ("with_me", Json::Bool(with_me)),
                ("status", Json::str(status)),
                ("ip", Json::str(ip)),
                ("via", Json::str(via)),
                ("fp", Json::str(fp)),
                ("same_build", Json::Bool(crate::net::same_build(p))),
            ])
        })
        .collect();
    fields.push(("peers", Json::Arr(peers)));
    // The overlay's Internet page: how internet play is doing, with no addresses (the
    // status keeps them out already; ports and counts only).
    let mut internet = Vec::new();
    if let Some(l) = lan {
        for (mode, st) in [("room", l.internet_status()), ("public", l.public_status())] {
            let Some(st) = st else { continue };
            let peers: Vec<Json> = st
                .peers
                .iter()
                .filter(|p| p.connected)
                .map(|p| {
                    Json::obj(vec![
                        ("name", Json::str(p.id.map(name_of).unwrap_or_else(|| p.name.clone()))),
                        ("rtt_ms", Json::Num(p.rtt.map_or(-1.0, |r| r.as_millis() as f64))),
                        ("bridged", Json::Bool(p.bridged)),
                        ("resent", Json::Num(p.resent as f64)),
                        ("ipv6", Json::Bool(p.ipv6)),
                        // What to type in a game that asks for an IP (ipfwd.rs): a
                        // stand-in, never the player's real address.
                        (
                            "address",
                            Json::str(p.id.map(|id| peerparty_core::fwd::Forwarder::address_of(id).to_string()).unwrap_or_default()),
                        ),
                    ])
                })
                .collect();
            internet.push(Json::obj(vec![
                ("mode", Json::str(mode)),
                ("online", Json::Bool(st.public.is_some())),
                ("nat", Json::str(st.nat.describe())),
                ("port_map", Json::str(st.port_map.clone().unwrap_or_default())),
                ("dht_nodes", Json::Num(st.dht_nodes as f64)),
                ("found", Json::Num(st.candidates as f64)),
                ("blocked", Json::Num(st.blocked as f64)),
                ("peers", Json::Arr(peers)),
            ]));
        }
    }
    fields.push(("internet", Json::Arr(internet)));
    // Players on our network we hear but can't connect to (their firewall, most often):
    // the overlay says so, and points at the fix on their PC.
    // Only while the engine runs: the overlay checks the firewall then.
    if let Some(l) = lan {
        fields.push(("lan_unreachable", Json::Num(l.unreachable().len() as f64)));
    }
    let pending: Vec<Json> = crate::invite::pending()
        .into_iter()
        .map(|p| {
            Json::obj(vec![
                ("id", Json::Num(p.id as f64)),
                ("from", Json::str(p.from.hex())),
                ("name", Json::str(name_of(p.from))),
                ("kind", Json::str(p.kind())),
                ("code", Json::str(p.code().unwrap_or_default())),
            ])
        })
        .collect();
    fields.push(("pending", Json::Arr(pending)));
    Json::obj(fields).to_string()
}
