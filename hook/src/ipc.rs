//! The connection to the peerparty.exe that started the game, which stays running beside
//! it and draws the overlay (launcher/src/host). Each message is one line of JSON, typed
//! in protocol/src/ipc.rs (`HookMsg` up, `HostMsg` down). The launcher names where it is
//! in `PEERPARTY_IPC` or `PEERPARTY_PIPE` (an `IpcEndpoint`):
//!
//! - Two one-way named pipes, so a read waiting on one never holds up a write on the
//!   other: `<name>-up` carries our messages to the launcher, `<name>-down` its commands
//!   to us. The pipes' server must be the launcher whose pid is in the name.
//! - Or a TCP connection on loopback, opened with the launcher's token: where a named
//!   pipe can't reach (a launcher outside Wine, the game inside it).
//!
//! Up: `state` (who we are, LAN players, the lobby we're in, invites waiting for the
//! player, settings), sent whenever it changes and every few seconds; `open` (the game
//! asked for an overlay page: its invite dialog, its friends list); `note` (a `Notice`
//! with its code, which the launcher says in the player's language); `local_saves`.
//!
//! Down: `invite` (one player or everyone), `accept` / `decline` (a waiting invite),
//! `join` (ask a player to invite us), `set` (a setting changed), `overlay` (shown or
//! hidden: games hear it as their SDK's overlay event, and controllers stop reaching
//! the game while it shows), and the rest of `HostMsg`.
//!
//! Without either variable (a game started some other way) none of this runs, and the
//! game calls that would open the overlay do what they did before it existed.

use std::io::{BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use peerparty_core::PeerId;
use peerparty_protocol::env;
use peerparty_protocol::ipc::{HookMsg, HostMsg, MAX_HOST_LINE, Notice, msg_type, read_line_capped};
use peerparty_protocol::transport::{IpcEndpoint, IpcStreams, TcpIpcClient, launcher_pid};

use crate::util::json::Json;
use crate::log;

static START: Once = Once::new();
static OUT: Mutex<Option<Sender<String>>> = Mutex::new(None);
static CONNECTED: AtomicBool = AtomicBool::new(false);

/// How often the state is checked for changes.
const STATE_EVERY: Duration = Duration::from_millis(250);
/// The state goes out at least this often, so a launcher that missed one catches up.
const HEARTBEAT: Duration = Duration::from_secs(5);
/// How long the launcher gets to be there when the game starts.
const FIND_FOR: Duration = Duration::from_secs(10);
/// One TCP connection attempt's wait.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Where the launcher is: `PEERPARTY_IPC`, else `PEERPARTY_PIPE` (either may hold any
/// `IpcEndpoint`).
fn endpoint_var() -> Option<String> {
    crate::config::HookConfig::global().ipc.clone()
}

/// Whether the launcher that started the game has the overlay on (it named where it is).
pub fn enabled() -> bool {
    endpoint_var().is_some()
}

/// Whether a launcher is listening (the overlay can be shown).
pub fn connected() -> bool {
    CONNECTED.load(Ordering::Relaxed)
}

/// Starts the connection's threads if the launcher named where it is.
pub fn start() {
    let Some(spec) = endpoint_var() else { return };
    START.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("peerparty-ipc".into())
            .spawn(move || run(&spec));
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
    *crate::sys::lock(&OUT) = None;
}

static HANDED_OVER: AtomicBool = AtomicBool::new(false);

/// Players blocked from the overlay in this run, with their fingerprints (for Undo).
static BLOCKED: Mutex<Vec<(PeerId, String)>> = Mutex::new(Vec::new());

/// Whether this process has let the launcher go to a program it started.
pub fn handed_over() -> bool {
    HANDED_OVER.load(Ordering::Relaxed)
}

/// Sends one message to the launcher (dropped when there's none).
pub fn send(msg: HookMsg) {
    if let Some(tx) = crate::sys::lock(&OUT).as_ref() {
        let _ = tx.send(msg.to_line());
    }
}

/// A short message for the player about something they just did (an invite that went,
/// or couldn't): the overlay shows it, or a notification while the overlay is closed.
pub fn note_notice(notice: Notice) {
    if connected() {
        send(HookMsg::Note(notice));
    }
}

/// A game first wrote to one of PeerParty's stand-ins for a service's cloud saves or
/// player data (`service`: "EOS", "PlayFab", ...): said once per service, in the log
/// and as a notice in the overlay (in the player's language), that it stays on this PC
/// in `path`, out of reach of the real service and of other PCs.
pub fn local_store(service: &'static str, path: &std::path::Path) {
    static SAID: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    {
        let mut said = crate::sys::lock(&SAID);
        if said.contains(&service) {
            return;
        }
        said.push(service);
    }
    let folder = path.parent().unwrap_or(path).display().to_string();
    log!("saves: {service}'s cloud saves and player data are kept on this PC, in {folder}: the real {service} service and other PCs don't see them");
    if connected() {
        send(HookMsg::LocalSaves { service: service.into(), path: folder });
    }
}

/// Asks the launcher to show an overlay page. False when there's no launcher to ask.
pub fn open(page: &str, peer: Option<PeerId>) -> bool {
    if !connected() {
        return false;
    }
    log!("overlay: the game asked for the {page} page");
    send(HookMsg::Open { page: page.into(), peer: peer.map(|p| p.hex()) });
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

/// Our end of the connection to write to. A TCP one keeps its port listed as ours
/// (`own::tcp`) while either end of it lives, and says it's done when the writer ends
/// (the read end is a second handle on the same socket, so dropping this alone wouldn't).
enum Up {
    Pipe(std::fs::File),
    Tcp { stream: std::net::TcpStream, _held: Arc<peerparty_core::own::Held> },
}

impl Write for Up {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Up::Pipe(f) => f.write(buf),
            Up::Tcp { stream, .. } => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Up::Pipe(f) => f.flush(),
            Up::Tcp { stream, .. } => stream.flush(),
        }
    }
}

impl Drop for Up {
    fn drop(&mut self) {
        if let Up::Tcp { stream, .. } = self {
            let _ = stream.shutdown(std::net::Shutdown::Write);
        }
    }
}

fn run(spec: &str) {
    match IpcEndpoint::parse(spec) {
        Some(IpcEndpoint::WinPipe { name }) => run_pipes(&name),
        Some(IpcEndpoint::TcpToken { addr, token }) => run_tcp(addr, &token),
        None => log!("ipc: the launcher's endpoint isn't one PeerParty makes; the overlay is off"),
    }
}

fn run_pipes(base: &str) {
    let deadline = Instant::now() + FIND_FOR;
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
    // The pipe's server must be the launcher the name was made by (its pid is in it):
    // nothing else can stand in for it, even with an instance of the same name.
    if let Some(want) = launcher_pid(base) {
        use std::os::windows::io::AsRawHandle;
        let mut got = 0u32;
        let ok = unsafe { windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId(down.as_raw_handle() as _, &mut got) } != 0;
        if !ok || got != want {
            log!("ipc: the pipe {base} is served by process {got}, not the launcher ({want}); the overlay is off");
            return;
        }
    }
    log!("ipc: connected to the launcher at {base}");
    serve(Up::Pipe(up), down);
}

fn run_tcp(addr: std::net::SocketAddr, token: &str) {
    let deadline = Instant::now() + FIND_FOR;
    let IpcStreams { read, write } = loop {
        if HANDED_OVER.load(Ordering::Relaxed) {
            return;
        }
        match TcpIpcClient::connect(addr, token, CONNECT_TIMEOUT) {
            Ok(s) => break s,
            // The launcher refused the token: trying again won't change that.
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                log!("ipc: the launcher on port {} refused us ({e}); the overlay is off", addr.port());
                return;
            }
            Err(e) if Instant::now() > deadline => {
                log!("ipc: no launcher on port {} ({e}); the overlay is off", addr.port());
                return;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let Ok(local) = read.local_addr() else { return };
    // Our socket is ours, not the game's: the forwarder never hands a peer to its port.
    let held = Arc::new(peerparty_core::own::tcp(local.port()));
    log!("ipc: connected to the launcher on port {}", addr.port());
    serve(Up::Tcp { stream: write, _held: held.clone() }, read);
    drop(held);
}

/// Talks to a connected launcher until it goes away (or this process hands it over).
fn serve(up: Up, down: impl Read) {
    let (tx, rx) = mpsc::channel::<String>();
    *crate::sys::lock(&OUT) = Some(tx);
    CONNECTED.store(true, Ordering::Relaxed);
    let _ = std::thread::Builder::new()
        .name("peerparty-ipc-out".into())
        .spawn(move || writer(up, rx));
    let mut reader = BufReader::new(down);
    let mut line = Vec::new();
    while read_line_capped(&mut reader, &mut line, MAX_HOST_LINE) {
        if HANDED_OVER.load(Ordering::Relaxed) {
            return;
        }
        if let Some(msg) = std::str::from_utf8(&line).ok().and_then(Json::parse) {
            match HostMsg::from_json(&msg) {
                Some(cmd) => command(cmd),
                None => log!("ipc: unknown command {:?}", msg_type(&msg).unwrap_or_default()),
            }
        }
    }
    if HANDED_OVER.load(Ordering::Relaxed) {
        return;
    }
    log!("ipc: the launcher went away");
    CONNECTED.store(false, Ordering::Relaxed);
    *crate::sys::lock(&OUT) = None;
    crate::overlay::set_visible(false);
    crate::overlay::pads_released();
    crate::window::restore_from_compositor();
}

fn writer(mut up: Up, rx: Receiver<String>) {
    let mut last_state = String::new();
    let mut last_sent = Instant::now() - HEARTBEAT;
    loop {
        match rx.recv_timeout(STATE_EVERY) {
            Ok(mut msg) => {
                // One pipe write a line, its newline with it.
                msg.push('\n');
                if up.write_all(msg.as_bytes()).is_err() {
                    return;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        let mut state = std::panic::catch_unwind(state).unwrap_or_default();
        if state.is_empty() {
            continue;
        }
        state.push('\n');
        if state == last_state && last_sent.elapsed() < HEARTBEAT {
            continue;
        }
        if up.write_all(state.as_bytes()).is_err() {
            return;
        }
        last_state = state;
        last_sent = Instant::now();
    }
}

fn peer_of(hex: &str) -> Option<PeerId> {
    PeerId::from_hex(hex)
}

fn command(cmd: HostMsg) {
    match cmd {
        HostMsg::Invite { to } => {
            let to = to.as_deref().and_then(peer_of);
            std::thread::spawn(move || crate::invite::to(to));
        }
        HostMsg::Accept { id } => {
            crate::invite::accept(id);
        }
        HostMsg::Paste => {
            // Type the clipboard's room code into the game (the overlay is closing).
            crate::invite::type_clipboard();
        }
        HostMsg::Copy { text } => {
            if let Some(text) = crate::invite::typing::clean_code(&text) {
                crate::invite::typing::set_clipboard(&text);
            }
        }
        HostMsg::Decline { id } => {
            crate::invite::decline(id);
        }
        HostMsg::Join { peer } => {
            if let Some(p) = peer_of(&peer)
                && !crate::steam::gs::join_server(p)
            {
                crate::invite::request_join(p);
            }
        }
        HostMsg::Block { peer } => {
            if let Some(p) = peer_of(&peer)
                && let Some(lan) = crate::net::lan_if_started()
            {
                match lan.block_peer(p) {
                    Ok(fp) => {
                        log!("ipc: blocked public player {p} ({fp})");
                        let mut b = crate::sys::lock(&BLOCKED);
                        b.retain(|(q, _)| *q != p);
                        if b.len() >= 16 {
                            b.remove(0);
                        }
                        b.push((p, fp));
                    }
                    Err(e) => log!("ipc: can't block {p}: {e}"),
                }
            }
        }
        // The overlay's voice switches (voice.rs): the microphone, and one player.
        HostMsg::Mic { on } => {
            crate::voice::set_mic(on);
            log!("ipc: microphone {}", if on { "on" } else { "off" });
        }
        HostMsg::Mute { peer, on } => {
            if let Some(p) = peer_of(&peer) {
                crate::voice::set_player_muted(p, on);
                log!("ipc: {p}'s voice {}", if on { "muted" } else { "heard again" });
            }
        }
        // The overlay's Undo, for a player blocked from it in this run.
        HostMsg::Unblock { peer } => {
            if let Some(p) = peer_of(&peer)
                && let Some(lan) = crate::net::lan_if_started()
            {
                let fp = crate::sys::lock(&BLOCKED).iter().find(|(q, _)| *q == p).map(|(_, f)| f.clone());
                match fp.map(|fp| lan.unblock(&fp)) {
                    Some(Ok(_)) => log!("ipc: unblocked public player {p}"),
                    Some(Err(e)) => log!("ipc: can't unblock {p}: {e}"),
                    None => log!("ipc: can't unblock {p}: not blocked in this run"),
                }
            }
        }
        HostMsg::Set { key, value } => {
            if key == "internet" {
                set_internet(&value);
            } else {
                crate::config::settings::set(&key, &value);
            }
        }
        HostMsg::Overlay { visible, why, pads_held } => {
            if let Some(why) = why.filter(|_| visible && !crate::overlay::visible()) {
                log!("overlay: opened by {why}");
            }
            crate::overlay::set_visible_holding(visible, pads_held);
        }
        // The buttons down when the overlay closed are up.
        HostMsg::PadsHeld { on } => {
            if !on {
                crate::overlay::pads_released();
            }
        }
        HostMsg::Note { text } => {
            // A line from the overlay host for the game's log.
            log!("{}", text.chars().take(300).collect::<String>());
        }
        HostMsg::Frame { name } => crate::overlay::frame_ready(&name),
        HostMsg::Instance(spec) => {
            let has_pane = spec.pane.is_some();
            crate::window::update_spec(spec);
            if has_pane {
                crate::video::ensure_viewport_hooks();
            }
        }
        HostMsg::InputShm { name } => {
            crate::input::attach_shm(&name);
        }
        HostMsg::Capture { enabled, ring, event, pace, pixels } => {
            crate::video::set_capture(enabled, ring.as_deref(), event.as_deref(), pace.as_deref(), pixels);
        }
        HostMsg::DevicesChanged => {
            // What the hook knows of devices first, so the game's own look finds it fresh.
            crate::input::devices_changed();
            crate::window::notify_devices_changed();
        }
    }
}

/// The overlay's internet switch: the room and public sessions on or off, live. Off
/// keeps LAN players; it takes a moment (sessions say goodbye), so not on the pipe's thread.
fn set_internet(value: &str) {
    let on = env::parse_bool(value) != Some(false);
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
    let mut fields: Vec<(&str, Json)> = Vec::new();
    fields.push(("pid", Json::u64(u64::from(std::process::id()))));
    fields.push(("lan", Json::Bool(crate::net::enabled())));
    let sdks: Vec<Json> = crate::net::fronts().iter().map(|f| Json::str(f.name())).collect();
    fields.push(("sdks", Json::Arr(sdks)));
    // Whether the game joins by room code (Photon): the overlay offers to copy and paste codes.
    fields.push(("codes", Json::Bool(crate::photon::in_use())));
    fields.push(("can_invite", Json::Bool(crate::invite::can_invite())));
    // Why there's nothing to invite to: its English, and its code and arguments for the
    // launcher to say in the player's language.
    let why_not = crate::invite::why_not();
    fields.push(("invite_note", Json::str(why_not.as_ref().map(|b| b.english()).unwrap_or_default())));
    fields.push(("invite_block", why_not.as_ref().map_or(Json::Null, |b| b.to_json())));
    // Layers the player turned off that this game needs: the overlay says what each costs.
    let off: Vec<Json> = crate::net::off_layers().into_iter().map(Json::str).collect();
    fields.push(("layers_off", Json::Arr(off)));
    let foreign: Vec<Json> = foreign_overlays().into_iter().map(Json::str).collect();
    fields.push(("foreign", Json::Arr(foreign)));
    fields.push(("present", Json::str(crate::video::api())));
    // Whether the overlay should draw inside the game's frames from the start (asked
    // to, or `auto` in Gamescope), and what the game runs under when it isn't Windows.
    fields.push(("prefer_ingame", Json::Bool(crate::video::prefer_ingame())));
    fields.push(("wine", crate::sys::wine::to_json()));
    fields.push((
        "settings",
        Json::obj(vec![
            ("auto_accept", Json::Bool(crate::config::settings::auto_accept())),
        ]),
    ));
    let lan = crate::net::lan_if_started();
    // The overlay's internet switch: whether it's on, and whether there's anything
    // (a room code or public sessions) for it to switch.
    fields.push(("internet_on", Json::Bool(lan.map_or_else(peerparty_core::internet_wanted, |l| l.internet_on()))));
    fields.push(("internet_configured", Json::Bool(lan.map_or_else(crate::net::ipfwd::internet_configured, |l| l.internet_configured()))));
    let name_of = |p: PeerId| lan.and_then(|l| l.peer_name(p)).unwrap_or_else(|| format!("Player {}", &p.hex()[..6]));
    if let Some(l) = lan {
        // Which of this PC's player slots this copy holds (0: the first player).
        let slot = if l.server() || l.slot() == u32::MAX { 0 } else { l.slot() };
        fields.push(("me", Json::obj(vec![("id", Json::str(l.me().hex())), ("name", Json::str(l.name())), ("slot", Json::u64(u64::from(slot)))])));
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
                ("max", Json::u64(u64::from(lobby.max_members))),
                ("members", Json::Arr(members)),
            ]),
        ));
    }
    let peers: Vec<Json> = lan
        .map(|l| l.peers())
        .unwrap_or_default()
        .into_iter()
        .map(|(p, name)| {
            // In our lobby, or with us in a session whose lobbies don't say so
            // (demonware/mm.rs `together`).
            let with_me = here.as_ref().is_some_and(|h| h.member(p).is_some()) || crate::invite::in_our_session(p);
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
            // A dedicated server goes by the name it advertises, if it has one.
            let server = crate::net::is_server_peer(p);
            let this_pc = on_this_pc(p);
            let name = presence
                .iter()
                .find(|(k, v)| server && k == crate::net::SERVER_NAME_KEY && !v.trim().is_empty())
                .map_or(name, |(_, v)| v.clone());
            Json::obj(vec![
                ("id", Json::str(p.hex())),
                ("name", Json::str(name)),
                ("server", Json::Bool(server)),
                ("this_pc", Json::Bool(this_pc)),
                ("with_me", Json::Bool(with_me)),
                ("status", Json::str(status)),
                ("ip", Json::str(ip)),
                ("via", Json::str(via)),
                ("fp", Json::str(fp)),
                ("same_build", Json::Bool(crate::net::same_build(p))),
                ("muted", Json::Bool(crate::voice::player_muted(p))),
            ])
        })
        .collect();
    fields.push(("peers", Json::Arr(peers)));
    fields.push(("voice", Json::obj(vec![("in_use", Json::Bool(crate::voice::in_use())), ("mic", Json::Bool(crate::voice::mic_on()))])));
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
                        ("rtt_ms", Json::int(p.rtt.map_or(-1, |r| i64::try_from(r.as_millis()).unwrap_or(i64::MAX)))),
                        ("bridged", Json::Bool(p.bridged)),
                        ("resent", Json::u64(p.resent)),
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
                ("dht_nodes", Json::u64(st.dht_nodes as u64)),
                ("found", Json::u64(st.candidates as u64)),
                ("blocked", Json::u64(st.blocked as u64)),
                ("peers", Json::Arr(peers)),
            ]));
        }
    }
    fields.push(("internet", Json::Arr(internet)));
    // Players on our network we hear but can't connect to (their firewall, most often):
    // the overlay says so, and points at the fix on their PC.
    // Only while the engine runs: the overlay checks the firewall then.
    if let Some(l) = lan {
        fields.push(("lan_unreachable", Json::u64(l.unreachable().len() as u64)));
        // Players of this game on another PeerParty: they can't connect, and the overlay
        // says why (and who should update).
        let other = l.other_versions();
        fields.push(("lan_other_version", Json::u64(other.len() as u64)));
        fields.push(("lan_newer_version", Json::Bool(other.iter().any(|newer| *newer))));
    }
    let pending: Vec<Json> = crate::invite::pending()
        .into_iter()
        .map(|p| {
            Json::obj(vec![
                // Exact: an id past 2^53 would come back as another through an f64.
                ("id", Json::u64(p.id)),
                ("from", Json::str(p.from.hex())),
                ("name", Json::str(name_of(p.from))),
                ("kind", Json::str(p.kind())),
                ("code", Json::str(p.code().unwrap_or_default())),
            ])
        })
        .collect();
    fields.push(("pending", Json::Arr(pending)));
    HookMsg::State(Json::obj(fields)).to_line()
}

/// Whether a peer is another copy on this PC (its id is one of this PC's identity
/// slots'): the player's own second copy or server, not someone else. Looked up once
/// per peer: a copy's key file is written before it meets anyone.
pub(crate) fn on_this_pc(peer: peerparty_core::PeerId) -> bool {
    static SEEN: std::sync::Mutex<Vec<(peerparty_core::PeerId, bool)>> = std::sync::Mutex::new(Vec::new());
    let mut seen = crate::sys::lock(&SEEN);
    if let Some((_, here)) = seen.iter().find(|(p, _)| *p == peer) {
        return *here;
    }
    let dir = peerparty_core::Identity::default_dir();
    let here = peerparty_core::Identity::ids_on_this_pc(&dir).contains(&peer);
    if seen.len() > 256 {
        seen.clear();
    }
    seen.push((peer, here));
    here
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launcher_pid_comes_from_the_pipe_name() {
        assert_eq!(launcher_pid(r"\\.\pipe\peerparty-4242-0011aabbccddeeff"), Some(4242));
        assert_eq!(launcher_pid(r"\\.\pipe\something-else"), None);
        let mut r = std::io::Cursor::new(b"{\"t\":\"note\"}\nxxxxxxxxxx\n".to_vec());
        let mut line = Vec::new();
        assert!(read_line_capped(&mut r, &mut line, 16));
        assert_eq!(line, b"{\"t\":\"note\"}");
        assert!(!read_line_capped(&mut r, &mut line, 4));
    }

    /// Over TCP: the hook proves the token, its socket's port is ours (the forwarder
    /// won't deliver to it) until both of its ends are gone, and the writer's end says
    /// so to the launcher.
    #[test]
    fn a_tcp_connection_is_held_as_ours_while_it_lives() {
        use peerparty_protocol::transport::TcpIpcServer;
        use std::io::BufRead;
        let token = "00112233445566778899aabbccddeeff";
        let server = TcpIpcServer::bind(token).unwrap();
        let addr = server.local_addr().unwrap();
        let launcher = std::thread::spawn(move || {
            let IpcStreams { read, .. } = server.accept().unwrap();
            let mut r = BufReader::new(read);
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            assert_eq!(HookMsg::from_line(line.trim_end()), Some(HookMsg::Close));
            // The hook's writer ended: the launcher reads the end.
            line.clear();
            assert_eq!(r.read_line(&mut line).unwrap(), 0);
        });
        let IpcStreams { read, write } = TcpIpcClient::connect(addr, token, CONNECT_TIMEOUT).unwrap();
        let port = read.local_addr().unwrap().port();
        let held = Arc::new(peerparty_core::own::tcp(port));
        let mut up = Up::Tcp { stream: write, _held: held.clone() };
        assert!(peerparty_core::own::is_held(true, port));
        up.write_all(format!("{}\n", HookMsg::Close.to_line()).as_bytes()).unwrap();
        drop(held);
        assert!(peerparty_core::own::is_held(true, port), "the writer's end still lives");
        drop(up);
        assert!(!peerparty_core::own::is_held(true, port));
        launcher.join().unwrap();
        drop(read);
    }

    /// What the launcher sends comes in typed, ids exact.
    #[test]
    fn commands_are_typed() {
        assert_eq!(HostMsg::from_line(r#"{"t":"accept","id":18446744073709551615}"#), Some(HostMsg::Accept { id: u64::MAX }));
        assert_eq!(HostMsg::from_line(r#"{"t":"mute","peer":"00aa","on":true}"#), Some(HostMsg::Mute { peer: "00aa".into(), on: true }));
        assert_eq!(peer_of("not hex"), None);
    }
}
