//! LAN invites, for games with an invite button and for games without one.
//!
//! An invite carries where to go: the game's Steam rich presence `connect` string (what
//! a Steam friend joining us would get) or its EOS presence join info, and/or the EOS
//! lobby or session or Steam lobby we're in. The receiving
//! copy accepts it through whichever SDK the game listens on (see net.rs).
//!
//! Without an in-game invite button, any of these invites every other player of this game
//! to where we are:
//! - the overlay's "Invite everyone" button (a command over ipc.rs).
//! - `peerparty --invite` (for a keyboard hotkey, e.g. from AutoHotkey), which sets the
//!   named event `Local\peerparty.invite`.
//! - only when turned on (`PEERPARTY_INVITE_BUTTONS`, or the setting): the Xbox (Guide)
//!   button and/or View + Menu held together for a second, on any XInput controller. By
//!   default the Guide button opens the overlay instead.
//!
//! Invites that arrive are accepted at once only when the player turned that on
//! (`PEERPARTY_INVITES=auto`, or the setting; see settings.rs). Otherwise each front end
//! tells the game one arrived, where its SDK has such an event, and the invite waits here
//! until the player accepts or declines it in the overlay. Accepting delivers it again,
//! and the front ends then send the SDK's "accepted" event (`decision` says which).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use peerparty_core::{Event, Lobby, PeerId};

use crate::log;
use crate::net;

pub const EVENT_NAME: &str = "Local\\peerparty.invite";

/// The lobby we're in, EOS or Steam.
fn here() -> Option<Lobby> {
    crate::eos::current_lobby().or_else(crate::steammm::current_lobby)
}

/// Sends an invite with an explicit connect string (the game's own invite button).
/// The lobby we're in rides along. Returns how many peers it reached.
pub fn send(to: Option<PeerId>, connect: &str) -> usize {
    deliver(to, connect, here())
}

/// Invites to a Steam lobby (the game's "invite to lobby"), with our `connect` presence
/// for games that join through that instead.
pub fn send_lobby(to: Option<PeerId>, lobby: Lobby) -> usize {
    deliver(to, &my_connect(), Some(lobby))
}

/// Where a friend joining us through presence should go: Steam's `connect`, else
/// EOS's join info, else the PlayFab lobby we're in.
fn my_connect() -> String {
    let steam = crate::steamlan::my_connect();
    if !steam.is_empty() {
        return steam;
    }
    let eos = crate::eos::join_info();
    if !eos.is_empty() {
        return eos;
    }
    if let Some(c) = crate::pfmp::current_connect() {
        return c;
    }
    if let Some(g) = crate::galaxy::my_connect() {
        return g;
    }
    // A Photon room has no invite to give: the code to type goes instead.
    crate::photon::room_view().map(|r| format!("{CODE_PREFIX}{}", r.code)).unwrap_or_default()
}

/// What an invite to a game that joins by room code (Photon) carries: the code, for the
/// other player to type into their game (the overlay does it for them).
pub const CODE_PREFIX: &str = "peerparty:code:";

/// The Photon room we're in, as a lobby for the overlay (its code is the lobby's id).
fn room_lobby() -> Option<Lobby> {
    let r = crate::photon::room_view()?;
    let mut l = Lobby::new("photon.room");
    l.id = r.code;
    l.owner = r.host;
    l.max_members = r.max;
    l.members = r.members.into_iter().map(|id| peerparty_core::Member { id, attrs: Vec::new() }).collect();
    Some(l)
}

/// The lobby we're in, through any SDK, for the overlay. Never starts the LAN (the
/// overlay asks from every process it talks to, a game's starter program too).
pub fn current_lobby() -> Option<Lobby> {
    let lan = net::lan_if_started()?;
    if let Some(l) = here() {
        return Some(l);
    }
    [
        "eos.session",
        crate::pfmp::NS,
        crate::galaxy::NS,
        "party.network",
        "ugs.lobby",
    ]
    .iter()
    .find_map(|ns| lan.my_lobbies(ns).into_iter().next())
    .or_else(room_lobby)
}

/// The connect string of a "please invite me" message (the overlay's Join button). A
/// copy that gets one invites the sender to where it is.
pub const JOIN_REQUEST: &str = "peerparty:join-request";

/// Players we asked to invite us, and when: their invite is accepted without asking.
static JOINING: Mutex<Vec<(PeerId, Instant)>> = Mutex::new(Vec::new());
const JOIN_WAIT: Duration = Duration::from_secs(20);

/// The overlay's Join button: asks `peer` to invite us to where it is.
pub fn request_join(peer: PeerId) {
    let Some(lan) = net::lan() else { return };
    {
        let mut j = JOINING.lock().unwrap();
        j.retain(|(p, t)| *p != peer && t.elapsed() < JOIN_WAIT);
        j.push((peer, Instant::now()));
    }
    let n = lan.invite(Some(peer), JOIN_REQUEST, None);
    log!("join: asked {peer} to invite us ({})", if n > 0 { "sent" } else { "not reachable" });
}

/// A player asked us to invite them (net.rs, instead of passing it to the game).
pub fn join_requested(from: PeerId) {
    log!("join: {from} asked to be invited");
    // Off the game's tick: inviting reads the front ends' lobbies under their locks.
    std::thread::spawn(move || {
        let connect = my_connect();
        let lobby = here();
        if connect.is_empty() && lobby.is_none() {
            let why = why_not_with(&net::fronts());
            log!("join: nothing to invite {from} to yet: {why}");
            crate::ipc::note("A player asked to join you", &format!("There's nothing to invite them to. {why}"));
            return;
        }
        deliver(Some(from), &connect, lobby);
    });
}

/// Whether we asked this player to invite us a moment ago (peeks; `decision` takes it).
fn joining(from: PeerId) -> bool {
    JOINING.lock().unwrap().iter().any(|(p, t)| *p == from && t.elapsed() < JOIN_WAIT)
}

/// Whether there's anywhere to invite to yet (never starts the LAN; see current_lobby).
pub fn can_invite() -> bool {
    net::lan_if_started().is_some() && (!my_connect().is_empty() || here().is_some())
}

/// What the overlay says when the game hosts a Photon room: it's matched (and
/// translated) there, with the room's name after it.
pub const PHOTON_NO_INVITES: &str = "This game has no invites: players on your network join your session from the game's own menu";

/// The online services an invite can travel through (my_connect / here read them).
const CARRIERS: &[net::Front] = &[net::Front::Eos, net::Front::Steam, net::Front::PfLobby, net::Front::Galaxy];

/// Why nothing can be invited to, for the overlay; None when inviting works now.
pub fn why_not() -> Option<String> {
    if can_invite() {
        return None;
    }
    let fronts = net::fronts();
    Some(why_not_with(&fronts))
}

/// `why_not` for these active front ends: a game whose only online services are ones
/// PeerParty can't hand an invite to (Xbox's invites and PlayFab Party only, say) is
/// told so, instead of being asked to host first.
fn why_not_with(fronts: &[net::Front]) -> String {
    why_not_for(fronts, &crate::modes::official_in_use(), crate::photon::local_room())
}

/// `why_not_with`, given the services left on their official servers this run: a game
/// that hosts its rooms there (Photon's cloud, say) has nothing PeerParty can invite to,
/// however many rooms it made, so it's told to use the game's own codes. A game hosting
/// a Photon room on PeerParty's own servers (`photon_room`) is hosting: Photon has no
/// invites, so it's told how the others join it instead of to host first.
fn why_not_for(fronts: &[net::Front], official: &[&str], photon_room: Option<String>) -> String {
    if !official.is_empty() {
        return format!(
            "This game's online play goes through {}'s own servers, which peerparty can't invite through: use the game's own invites or room codes instead",
            official.join(" and ")
        );
    }
    if let Some(room) = photon_room {
        return format!("{PHOTON_NO_INVITES} (room {room})");
    }
    if !fronts.is_empty() && !fronts.iter().any(|f| CARRIERS.contains(f)) {
        let names: Vec<&str> = fronts.iter().map(|f| f.label()).collect();
        return format!("Not supported for this game yet: peerparty can't hand it an invite through {}", names.join(" or "));
    }
    "Host or join a game first".into()
}

/// Invites everyone on this network to wherever we are: our presence `connect` and/or
/// lobby. Room and public players are only ever invited one at a time, by name.
pub fn everyone() -> usize {
    to(None)
}

/// Invites one player (or everyone) to wherever we are, with a chime: an error chime
/// when we aren't anywhere yet.
pub fn to(target: Option<PeerId>) -> usize {
    let (connect, lobby) = crate::overlay::dialog_target().unwrap_or_else(|| (my_connect(), here()));
    if connect.is_empty() && lobby.is_none() {
        let why = why_not_with(&net::fronts());
        log!("invite: nothing to invite to: {why}");
        crate::ipc::note("Can't invite", &why);
        beep(false);
        return 0;
    }
    let n = deliver(target, &connect, lobby);
    if n > 0 {
        crate::ipc::note("Invite sent", &format!("To {n} player{}", if n == 1 { "" } else { "s" }));
    } else {
        crate::ipc::note("Nobody to invite", "No other player of this game is running on your network, or they're all with you already");
    }
    beep(n > 0);
    n
}

/// Invites each target that isn't already in the lobby. A game asked to join the
/// lobby it's in may leave and rejoin it, which drops a connection
/// that was working. Everyone (`to` None) means the players on this network only: a
/// room's friends and public players get an invite only when picked by name.
fn deliver(to: Option<PeerId>, connect: &str, lobby: Option<Lobby>) -> usize {
    let Some(lan) = net::lan() else { return 0 };
    let targets: Vec<PeerId> = lan
        .peers()
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| match to {
            Some(t) => t == *p,
            None => !matches!(lan.peer_trust(*p), Some(peerparty_core::Trust::Room | peerparty_core::Trust::Public)),
        })
        .filter(|p| lobby.as_ref().is_none_or(|l| l.member(*p).is_none()))
        .filter(|p| !connect.starts_with(CODE_PREFIX) || room_lobby().is_none_or(|r| r.member(*p).is_none()))
        .collect();
    let mut n = 0;
    for p in &targets {
        n += lan.invite(Some(*p), connect, lobby.as_ref());
    }
    log!(
        "invite sent to {n} player(s) not already in the lobby: connect={connect:?} lobby={:?}",
        lobby.as_ref().map(|l| l.id.as_str())
    );
    n
}

// ---- invites that arrive ----

/// How a front end handles an invite it is delivering to the game.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Accept it at once: "received" (where the SDK has it), then "accepted".
    Auto,
    /// Only tell the game it arrived; the player answers in the overlay.
    Ask,
    /// The player accepted it in the overlay: "accepted" only (the game already heard
    /// it arrive).
    Forced,
}

impl Decision {
    /// Whether the game hears "received" (it hasn't yet).
    pub fn received(self) -> bool {
        self != Decision::Forced
    }

    /// Whether the game hears "accepted".
    pub fn accept(self) -> bool {
        self != Decision::Ask
    }
}

/// An invite waiting for the player.
#[derive(Clone, Debug)]
pub struct Pending {
    pub id: u64,
    pub from: PeerId,
    pub connect: String,
    pub lobby: Option<Lobby>,
    pub at: Instant,
}

impl Pending {
    /// What the overlay says about it, after the sender's name.
    pub fn kind(&self) -> &'static str {
        if self.connect.starts_with(CODE_PREFIX) {
            "sent you a room code"
        } else if crate::eos::is_request_to_join(&self.connect) {
            "asks to join you"
        } else {
            "invited you"
        }
    }
}

type Key = (PeerId, String, Option<String>);

static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
static FORCED: Mutex<Vec<Key>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// Bumped whenever the pending list changes, so ipc.rs knows to send it.
pub static CHANGES: AtomicU64 = AtomicU64::new(0);

/// Unanswered invites are dropped after this long.
const PENDING_FOR: Duration = Duration::from_secs(600);

fn key(from: PeerId, connect: &str, lobby: Option<&Lobby>) -> Key {
    (from, connect.to_string(), lobby.map(|l| l.id.clone()))
}

/// What to do with this invite (front ends call it once per delivery).
pub fn decision(from: PeerId, connect: &str, lobby: Option<&Lobby>) -> Decision {
    let k = key(from, connect, lobby);
    {
        let mut j = JOINING.lock().unwrap();
        if let Some(i) = j.iter().position(|(p, t)| *p == from && t.elapsed() < JOIN_WAIT) {
            j.remove(i);
            log!("join: {from} invited us as asked; accepted");
            return Decision::Forced;
        }
    }
    {
        let mut forced = FORCED.lock().unwrap();
        if let Some(i) = forced.iter().position(|f| *f == k) {
            forced.remove(i);
            return Decision::Forced;
        }
    }
    if auto_for(from) { Decision::Auto } else { Decision::Ask }
}

/// Invites accepted without asking: when the player said so, and never from a public
/// player (a stranger), whose invites always wait for the player.
fn auto_for(from: PeerId) -> bool {
    crate::settings::auto_accept()
        && crate::net::lan_if_started().and_then(|l| l.peer_trust(from)) != Some(peerparty_core::Trust::Public)
}

/// A fresh invite from the LAN (net.rs, before it reaches the front ends). Unless
/// invites are accepted at once, it waits for the player.
pub fn arrived(ev: &Event) {
    let Event::Invite { from, connect, lobby } = ev else { return };
    if auto_for(*from) || !crate::eos::needs_answer(connect) || joining(*from) {
        return;
    }
    let mut p = PENDING.lock().unwrap();
    p.retain(|x| x.at.elapsed() < PENDING_FOR);
    let k = key(*from, connect, lobby.as_ref());
    p.retain(|x| key(x.from, &x.connect, x.lobby.as_ref()) != k);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    log!("invite {id} from {from} waits for the player (auto-accept is off)");
    p.push(Pending { id, from: *from, connect: connect.clone(), lobby: lobby.clone(), at: Instant::now() });
    CHANGES.fetch_add(1, Ordering::Relaxed);
}

impl Pending {
    /// The room code this carries, for an invite to a game that joins by code.
    pub fn code(&self) -> Option<&str> {
        self.connect.strip_prefix(CODE_PREFIX)
    }
}

/// A room code from another player (net.rs, instead of passing it to the game): it
/// waits in the overlay until the player has it typed into their game.
pub fn code_arrived(from: PeerId, code: &str) {
    let Some(code) = crate::typing::clean_code(code) else { return };
    let connect = format!("{CODE_PREFIX}{code}");
    let mut p = PENDING.lock().unwrap();
    p.retain(|x| x.at.elapsed() < PENDING_FOR && !(x.from == from && x.connect == connect));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    log!("room code {code} from {from} waits for the player (pending {id})");
    p.push(Pending { id, from, connect, lobby: None, at: Instant::now() });
    CHANGES.fetch_add(1, Ordering::Relaxed);
}

/// The overlay's "type what's on the clipboard": a code copied from a chat, say.
pub fn type_clipboard() {
    match crate::typing::clipboard_text().and_then(|t| crate::typing::clean_code(&t)) {
        Some(code) => crate::typing::type_code(code),
        None => {
            log!("type: nothing on the clipboard that looks like a room code");
            crate::ipc::note("No code to paste", "Copy the room code first, then try again");
        }
    }
}

/// Invites waiting for the player, oldest first.
pub fn pending() -> Vec<Pending> {
    let mut p = PENDING.lock().unwrap();
    p.retain(|x| x.at.elapsed() < PENDING_FOR);
    p.clone()
}

fn take_pending(id: u64) -> Option<Pending> {
    let mut p = PENDING.lock().unwrap();
    let i = p.iter().position(|x| x.id == id)?;
    CHANGES.fetch_add(1, Ordering::Relaxed);
    Some(p.remove(i))
}

/// The player accepted an invite in the overlay: it goes to the front ends again, and
/// this time they accept it.
pub fn accept(id: u64) -> bool {
    let Some(inv) = take_pending(id) else { return false };
    if let Some(code) = inv.code() {
        log!("room code {code} from {} accepted by the player: typing it into the game", inv.from);
        crate::typing::type_code(code.to_string());
        return true;
    }
    log!("invite {id} from {} accepted by the player", inv.from);
    FORCED.lock().unwrap().push(key(inv.from, &inv.connect, inv.lobby.as_ref()));
    net::redeliver(Event::Invite { from: inv.from, connect: inv.connect, lobby: inv.lobby });
    true
}

/// The player declined: a request to join hears no, anything else is just dropped.
pub fn decline(id: u64) -> bool {
    let Some(inv) = take_pending(id) else { return false };
    log!("invite {id} from {} declined by the player", inv.from);
    if crate::eos::is_request_to_join(&inv.connect) {
        crate::eos::reject_request_to_join(inv.from);
    }
    true
}

// ---- triggers ----

/// Triggers act in the game in front: a controller and `--invite` reach every copy.
fn in_front() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
    pid == std::process::id()
}

fn beep(ok: bool) {
    use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONASTERISK, MB_ICONHAND};
    unsafe { MessageBeep(if ok { MB_ICONASTERISK } else { MB_ICONHAND }) };
}

static START: Once = Once::new();

/// Starts the trigger thread (once).
pub fn start() {
    if !net::enabled() {
        return;
    }
    START.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("peerparty-invite".into())
            .spawn(run);
    });
}

const GUIDE: u16 = 0x0400;
const BACK: u16 = 0x0020;
const START_BUTTON: u16 = 0x0010;
const COMBO_HOLD: Duration = Duration::from_secs(1);

/// `XInputGetStateEx`'s state: `XINPUT_STATE` and a DWORD of padding (20 bytes; a 16-byte
/// `XINPUT_STATE` buffer is overrun).
const STATE_LEN: usize = 20;

type GetStateFn = unsafe extern "system" fn(u32, *mut [u8; STATE_LEN]) -> u32;

/// `XInputGetStateEx` (ordinal 100) reports the Guide button; plain `XInputGetState`
/// doesn't, so it's the fallback for the View + Menu combo only.
fn xinput() -> Option<GetStateFn> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    for dll in ["xinput1_4.dll", "xinput1_3.dll"] {
        let wide: Vec<u16> = dll.encode_utf16().chain(Some(0)).collect();
        let m = unsafe { LoadLibraryW(wide.as_ptr()) };
        if m.is_null() {
            continue;
        }
        let f = unsafe { GetProcAddress(m, 100 as *const u8) }
            .or_else(|| unsafe { GetProcAddress(m, c"XInputGetState".as_ptr().cast()) });
        if let Some(f) = f {
            return Some(unsafe { std::mem::transmute::<_, GetStateFn>(f) });
        }
    }
    None
}

/// Waits on the `--invite` event. Controllers are read only while invite buttons are
/// turned on; with them off (the default) the thread sleeps in the wait.
fn run() {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    let name: Vec<u16> = EVENT_NAME.encode_utf16().chain(Some(0)).collect();
    // Manual reset: `peerparty --invite` sets it, then clears it a moment later.
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
    let mut get_state: Option<Option<GetStateFn>> = None;
    log!(
        "invite triggers: `peerparty --invite`, the overlay{}",
        match crate::settings::invite_buttons() {
            0 => "",
            b if b == crate::settings::GUIDE => ", the Xbox button",
            b if b == crate::settings::COMBO => ", View+Menu",
            _ => ", the Xbox button, View+Menu",
        }
    );
    let mut was_set = false;
    let mut guide_down = [false; 4];
    let mut combo_since: [Option<Instant>; 4] = [None; 4];
    let mut combo_fired = [false; 4];
    loop {
        let buttons = crate::settings::invite_buttons();
        let wait = if buttons == 0 && !was_set { 1000 } else { 50 };
        if event.is_null() {
            std::thread::sleep(Duration::from_millis(u64::from(wait)));
        } else {
            let set = unsafe { WaitForSingleObject(event, wait) } == WAIT_OBJECT_0;
            if set && !was_set && in_front() {
                log!("invite: triggered by peerparty --invite");
                everyone();
            }
            if set {
                // Still set: don't spin on it until the sender clears it.
                std::thread::sleep(Duration::from_millis(50));
            }
            was_set = set;
        }
        if buttons == 0 || !in_front() {
            guide_down = [false; 4];
            combo_since = [None; 4];
            continue;
        }
        let Some(get_state) = *get_state.get_or_insert_with(xinput) else { continue };
        for pad in 0..4u32 {
            let mut state = [0u8; STATE_LEN];
            if unsafe { get_state(pad, &mut state) } != 0 {
                continue;
            }
            let pressed = u16::from_le_bytes([state[4], state[5]]);
            let i = pad as usize;
            let guide = pressed & GUIDE != 0;
            if guide && !guide_down[i] && buttons & crate::settings::GUIDE != 0 {
                log!("invite: triggered by the Xbox button on pad {pad}");
                everyone();
            }
            guide_down[i] = guide;
            let combo = buttons & crate::settings::COMBO != 0
                && pressed & (BACK | START_BUTTON) == (BACK | START_BUTTON);
            match (combo, combo_since[i]) {
                (true, None) => combo_since[i] = Some(Instant::now()),
                (true, Some(t)) if !combo_fired[i] && t.elapsed() >= COMBO_HOLD => {
                    combo_fired[i] = true;
                    log!("invite: triggered by View+Menu on pad {pad}");
                    everyone();
                }
                (false, _) => {
                    combo_since[i] = None;
                    combo_fired[i] = false;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With auto-accept off an invite waits; accepting it makes the next delivery
    /// "accepted only"; declining drops it; a join we asked for is accepted at once.
    #[test]
    fn invites_wait_for_the_player() {
        let from = PeerId([7; 16]);
        let ev = Event::Invite { from, connect: "+connect_lobby 1".into(), lobby: None };
        crate::settings::set("auto_accept", "ask");
        assert_eq!(decision(from, "+connect_lobby 1", None), Decision::Ask);
        arrived(&ev);
        let p = pending();
        let mine: Vec<&Pending> = p.iter().filter(|x| x.from == from).collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].kind(), "invited you");
        // The same invite again replaces it rather than piling up.
        arrived(&ev);
        assert_eq!(pending().iter().filter(|x| x.from == from).count(), 1);
        let id = pending().iter().find(|x| x.from == from).unwrap().id;
        assert!(accept(id));
        assert!(pending().iter().all(|x| x.from != from));
        assert_eq!(decision(from, "+connect_lobby 1", None), Decision::Forced);
        // Used up: the next delivery is an ordinary one again.
        assert_eq!(decision(from, "+connect_lobby 1", None), Decision::Ask);
        arrived(&ev);
        let id = pending().iter().find(|x| x.from == from).unwrap().id;
        assert!(decline(id));
        assert!(!decline(id));
        // Auto-accept on: nothing waits.
        crate::settings::set("auto_accept", "auto");
        arrived(&ev);
        assert!(pending().iter().all(|x| x.from != from));
        assert_eq!(decision(from, "+connect_lobby 1", None), Decision::Auto);
        crate::settings::set("auto_accept", "ask");
        // Asked to be invited: that player's invite is accepted without asking.
        let host = PeerId([9; 16]);
        JOINING.lock().unwrap().push((host, Instant::now()));
        let from_host = Event::Invite { from: host, connect: "x".into(), lobby: None };
        arrived(&from_host);
        assert!(pending().iter().all(|x| x.from != host));
        assert_eq!(decision(host, "x", None), Decision::Forced);
    }

    #[test]
    fn says_why_there_is_nothing_to_invite_to() {
        use crate::net::Front;
        // PlayFab Party only, whose invites PeerParty can't carry.
        let party = why_not_for(&[Front::Party], &[], None);
        assert!(party.starts_with("Not supported for this game yet"), "{party}");
        assert!(party.contains("PlayFab Party"), "{party}");
        // A game with a lobby service just hasn't hosted yet, nor has one that hasn't
        // started any online service.
        assert_eq!(why_not_for(&[Front::Party, Front::Eos], &[], None), "Host or join a game first");
        assert_eq!(why_not_for(&[], &[], None), "Host or join a game first");
        // Rooms on Photon's own cloud (a Steam game whose multiplayer is Photon's):
        // no room the player makes is one PeerParty can invite to.
        let photon = why_not_for(&[Front::Steam], &["Photon"], None);
        assert!(photon.contains("Photon's own servers") && photon.contains("room codes"), "{photon}");
        // A room on PeerParty's own Photon servers: the game has hosted, and its players
        // join from its own menu (Photon has no invites), not "host first".
        let room = why_not_for(&[Front::Steam], &[], Some("EUCSH".into()));
        assert_eq!(room, format!("{PHOTON_NO_INVITES} (room EUCSH)"));
    }
}
