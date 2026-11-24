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
//! - the launcher's "Invite local players" button, for the game that was in front.
//!
//! A game that never publishes where to join (no Steam rich presence `connect`, no SDK
//! lobby PeerParty can hand over) but sends invites of its own (its friends list, through
//! Steam's InviteUserToGame) shows PeerParty what an invite looks like: when that invite
//! names the lobby the game is in (a Unity lobby's id, say), it's kept for this game
//! (`%LOCALAPPDATA%\peerparty\invite-templates`) with the lobby id as a placeholder, and
//! from then on PeerParty's own invites and Join requests send it with the current
//! lobby's id. Until the game has sent one, the overlay says so.
//!
//! Invites that arrive are accepted at once only when the player turned that on
//! (`PEERPARTY_INVITES=auto`, or the setting; see settings.rs). Otherwise each front end
//! tells the game one arrived, where its SDK has such an event, and the invite waits here
//! until the player accepts or declines it in the overlay. Accepting delivers it again,
//! and the front ends then send the SDK's "accepted" event (`decision` says which).

pub(crate) mod lobbychat;
pub(crate) mod typing;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use peerparty_core::{Event, Lobby, PeerId};
use peerparty_protocol::ipc::{InviteBlock, Notice};

use crate::log;
use crate::net::{self, Front};

pub const EVENT_NAME: &str = "Local\\peerparty.invite";
/// Set by the launcher's own "Invite" button: for the game that was last in front (the
/// launcher is in front when its button is pressed, so no game is).
pub const EVENT_LAST: &str = "Local\\peerparty.invite.last";
/// Set by the copy that acts on either, so whoever asked can say whether a game did.
pub const EVENT_ACK: &str = "Local\\peerparty.invite.ack";
/// Four bytes shared by every hooked game: the process id of the one last in front.
const FRONT_MAP: &str = "Local\\peerparty.front";

/// Where the last game in front writes its process id (null if it couldn't be shared).
fn front_slot() -> *mut u32 {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE};
    let name: Vec<u16> = FRONT_MAP.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let map = CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, 4, name.as_ptr());
        if map.is_null() {
            return std::ptr::null_mut();
        }
        MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, 4).Value.cast()
    }
}

/// An SDK whose lobbies and presence carry invites (the table: hub/mod.rs `INVITES`,
/// in the order their presence is offered).
pub struct InviteCarrier {
    pub front: Front,
    /// The LAN namespaces its lobbies are kept under.
    pub namespaces: &'static [&'static str],
    /// The lobby the game is in through this SDK, by the SDK's own account of it.
    pub current_lobby: fn() -> Option<Lobby>,
    /// Where a friend joining us through this SDK's presence should go.
    pub my_connect: fn() -> Option<String>,
    /// Whether `peer` plays with us in a session whose lobbies don't say so.
    pub in_our_session: fn(PeerId) -> bool,
    /// How strongly this SDK claims an invite that arrived, by its connect string and
    /// its lobby's namespace (`taker` picks one SDK).
    pub accept_invite: fn(&str, Option<&str>) -> Claim,
    /// The join requests its SDK sends as invites (EOS's custom invites), if it has any.
    pub join_requests: Option<JoinRequests>,
    /// Why the game may have nothing to invite to, for the log (when the game uses it).
    pub diagnosis: fn() -> Option<String>,
}

/// An SDK's "please let me join you", which travels as an invite.
pub struct JoinRequests {
    /// Whether a connect string is a request to join us.
    pub is_request: fn(&str) -> bool,
    /// Whether it's the answer to our own request: a reply, nothing to answer.
    pub is_reply: fn(&str) -> bool,
    /// The player said no to `from`'s request.
    pub decline: fn(PeerId),
}

/// The carrier whose join request `connect` is.
fn join_request(connect: &str) -> Option<&'static JoinRequests> {
    carriers().iter().filter_map(|c| c.join_requests.as_ref()).find(|j| (j.is_request)(connect))
}

/// Whether `connect` is a reply to a join request of ours (it waits for no answer).
fn is_reply(connect: &str) -> bool {
    carriers().iter().filter_map(|c| c.join_requests.as_ref()).any(|j| (j.is_reply)(connect))
}

/// How strongly a carrier claims an invite, weakest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Claim {
    No,
    /// It can take it when no one else will.
    Fallback,
    /// It takes it (the first such carrier in the table wins).
    Takes,
    /// It's this SDK's alone, when the game uses that SDK.
    SoleWhenUsed,
    /// It's this SDK's alone, whatever the game uses (no other SDK can read it).
    Sole,
}

fn carriers() -> &'static [&'static InviteCarrier] {
    crate::hub::INVITES
}

/// The front end that accepts an invite: one only, or the game would join twice. The
/// strongest claim among the SDKs the game uses (`fronts`) wins, the first in the table
/// on a tie; a `Sole` claim wins whether or not the game uses that SDK.
pub fn taker(connect: &str, ns: Option<&str>, fronts: &[Front]) -> Option<Front> {
    let mut best: Option<(Claim, Front)> = None;
    for c in carriers() {
        let claim = (c.accept_invite)(connect, ns);
        if claim == Claim::No || (claim != Claim::Sole && !fronts.contains(&c.front)) {
            continue;
        }
        if best.is_none_or(|(b, _)| claim > b) {
            best = Some((claim, c.front));
        }
    }
    best.map(|(_, f)| f)
}

/// The lobbies the carriers keep on the LAN that we're in.
pub fn my_carrier_lobbies(lan: &peerparty_core::Lan) -> Vec<Lobby> {
    carriers().iter().flat_map(|c| c.namespaces.iter()).flat_map(|ns| lan.my_lobbies(ns)).collect()
}

/// Whether `connect` is where one of the carriers' presence already points.
pub fn is_my_connect(connect: &str) -> bool {
    !connect.is_empty() && carriers().iter().any(|c| (c.my_connect)().is_some_and(|m| m == connect))
}

/// Where a friend joining us through `front`'s presence should go (empty: nowhere).
pub fn connect_of(front: Front) -> String {
    carriers().iter().find(|c| c.front == front).and_then(|c| (c.my_connect)()).unwrap_or_default()
}

/// The lobby we're in, by an SDK's own account of it (EOS's, Steam's).
fn here() -> Option<Lobby> {
    carriers().iter().find_map(|c| (c.current_lobby)())
}

/// Sends an invite with an explicit connect string (the game's own invite button).
/// The lobby we're in rides along. Returns how many peers it reached.
pub fn send(to: Option<PeerId>, connect: &str) -> usize {
    learn(connect);
    deliver(to, connect, here())
}

// ---- invites learned from the game ----

/// Where the lobby id goes in a learned invite.
const LOBBY_MARK: &str = "{peerparty:lobby}";

/// This game's learned invite (None: none yet), read once.
static LEARNED: Mutex<Option<Option<String>>> = Mutex::new(None);

fn learned_path() -> Option<std::path::PathBuf> {
    let name = crate::util::local_store::file_name(&net::game_key())?;
    Some(crate::util::local_store::sdk_dir("invite-templates")?.join(format!("{name}.txt")))
}

fn learned() -> Option<String> {
    let mut l = crate::sys::lock(&LEARNED);
    l.get_or_insert_with(|| {
        let t = std::fs::read_to_string(learned_path()?).ok()?;
        let t = t.trim().to_string();
        t.contains(LOBBY_MARK).then_some(t)
    })
    .clone()
}

/// The template of an invite that names `lobby_id`: the id swapped for the mark.
fn template_of(connect: &str, lobby_id: &str) -> Option<String> {
    let c = connect.trim();
    if c.is_empty() || lobby_id.len() < 4 || c.starts_with(CODE_PREFIX) || c == JOIN_REQUEST || !c.contains(lobby_id) {
        return None;
    }
    Some(c.replace(lobby_id, LOBBY_MARK))
}

/// A learned invite for the lobby `lobby_id`.
fn fill(template: &str, lobby_id: &str) -> String {
    template.replace(LOBBY_MARK, lobby_id)
}

/// Keeps the game's own invite as this game's template when it names the lobby we're in.
fn learn(connect: &str) {
    let Some(lobby) = current_lobby() else { return };
    let Some(t) = template_of(connect, &lobby.id) else { return };
    if learned().as_deref() == Some(t.as_str()) {
        return;
    }
    let saved = learned_path().is_some_and(|p| {
        p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && crate::saves::write_whole(&p, t.as_bytes()).is_ok()
    });
    *crate::sys::lock(&LEARNED) = Some(Some(t));
    log!(
        "invite: learned this game's invite from its own (the lobby id in it){}; peerparty's Invite and Join use it from now on",
        if saved { ", kept for next time" } else { ", for this run only (couldn't save it)" }
    );
}

/// The game's own invite, learned earlier, for the lobby we're in now.
fn learned_connect() -> Option<String> {
    let t = learned()?;
    Some(fill(&t, &current_lobby()?.id))
}

/// Invites to a Steam lobby (the game's "invite to lobby"), with our `connect` presence
/// for games that join through that instead.
pub fn send_lobby(to: Option<PeerId>, lobby: Lobby) -> usize {
    deliver(to, &my_connect(), Some(lobby))
}

/// Where a friend joining us through presence should go: the first carrier's that has
/// one (Steam's `connect`, EOS's join info, the PlayFab lobby we're in, Galaxy's).
fn my_connect() -> String {
    if let Some(c) = carriers().iter().find_map(|c| (c.my_connect)().filter(|c| !c.is_empty())) {
        return c;
    }
    if let Some(c) = learned_connect() {
        return c;
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
    carriers()
        .iter()
        .flat_map(|c| c.namespaces.iter())
        .chain(crate::hub::OTHER_LOBBIES)
        .find_map(|ns| lan.my_lobbies(ns).into_iter().next())
        .or_else(room_lobby)
}

/// The connect string of a "please invite me" message (the overlay's Join button). A
/// copy that gets one invites the sender to where it is.
pub const JOIN_REQUEST: &str = "peerparty:join-request";

/// Players we asked to invite us, and when: their invite is accepted without asking.
static JOINING: Mutex<Vec<(PeerId, Instant)>> = Mutex::new(Vec::new());
const JOIN_WAIT: Duration = Duration::from_secs(20);

/// The answers to a Join request that sends no invite, back to the player who asked:
/// they're with us already, or we aren't anywhere to invite them to.
pub const JOIN_TOGETHER: &str = "peerparty:join-together";
pub const JOIN_NOWHERE: &str = "peerparty:join-nowhere";
/// The first PeerParty that understands them.
const ANSWERS_SINCE: (u32, u32, u32) = (0, 4, 7);

/// Whether `peer` plays with us in a session whose lobbies don't say so (a carrier's
/// `in_our_session`, as Demonware's over Steam): such a game makes a fresh lobby for
/// each player who joins.
pub fn in_our_session(peer: PeerId) -> bool {
    carriers().iter().any(|c| (c.in_our_session)(peer))
}

/// Whether `peer` is with us already: in the lobby we're in, through any SDK, or in our
/// game's session.
fn with_us(peer: PeerId) -> bool {
    in_our_session(peer) || here().or_else(current_lobby).is_some_and(|l| l.member(peer).is_some())
}

/// A peer's name for the overlay's notes.
fn name_of(peer: PeerId) -> String {
    net::lan_if_started().and_then(|l| l.peer_name(peer)).filter(|n| !n.is_empty()).unwrap_or_else(|| "That player".into())
}

/// The overlay's Join button: asks `peer` to invite us to where it is. A player we're
/// with already is told so instead of asked.
pub fn request_join(peer: PeerId) {
    let Some(lan) = net::lan() else { return };
    if with_us(peer) {
        log!("join: {peer} is with us already; not asking for an invite");
        join_answered(peer, JOIN_TOGETHER);
        beep(false);
        return;
    }
    {
        let mut j = crate::sys::lock(&JOINING);
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
        // Older copies would show the answer as an invite to nowhere.
        let answer = |what: &str| {
            if let Some(lan) = net::lan_if_started()
                && net::peer_at_least(lan, from, ANSWERS_SINCE)
            {
                lan.invite(Some(from), what, None);
            }
        };
        if with_us(from) {
            log!("join: {from} is with us already (our lobby or our game's session); telling them so");
            answer(JOIN_TOGETHER);
            return;
        }
        if connect.is_empty() && lobby.is_none() {
            let why = why_not_with(&net::fronts());
            log!("join: nothing to invite {from} to yet: {}", why.english());
            crate::ipc::note_notice(Notice::join_asked(why));
            answer(JOIN_NOWHERE);
            return;
        }
        deliver(Some(from), &connect, lobby);
    });
}

/// A player we asked to invite us answered without an invite (`JOIN_TOGETHER` or
/// `JOIN_NOWHERE`): the overlay says why nothing came.
pub fn join_answered(from: PeerId, what: &str) {
    crate::sys::lock(&JOINING).retain(|(p, _)| *p != from);
    let name = name_of(from);
    if what == JOIN_TOGETHER {
        log!("join: {from} says we're with them already");
        crate::ipc::note_notice(Notice::already_together(&name));
    } else {
        log!("join: {from} isn't anywhere we can join yet");
        crate::ipc::note_notice(Notice::cant_join(&name));
    }
}

/// Whether we asked this player to invite us a moment ago (peeks; `decision` takes it).
fn joining(from: PeerId) -> bool {
    crate::sys::lock(&JOINING).iter().any(|(p, t)| *p == from && t.elapsed() < JOIN_WAIT)
}

/// Whether there's anywhere to invite to yet (never starts the LAN; see current_lobby).
pub fn can_invite() -> bool {
    net::lan_if_started().is_some() && (!my_connect().is_empty() || here().is_some())
}

/// Why nothing can be invited to, for the overlay; None when inviting works now.
pub fn why_not() -> Option<InviteBlock> {
    if can_invite() {
        return None;
    }
    let fronts = net::fronts();
    Some(why_not_with(&fronts))
}

/// `why_not` for these active front ends: a game whose only online services are ones
/// PeerParty can't hand an invite to (Xbox's invites and PlayFab Party only, say) is
/// told so, instead of being asked to host first.
fn why_not_with(fronts: &[net::Front]) -> InviteBlock {
    why_not_for(fronts, &crate::config::modes::official_in_use(), &net::services_off(), crate::photon::local_room(), current_lobby().is_some())
}

/// `why_not_with`, given the services left on their official servers this run: a game
/// that hosts its rooms there (Photon's cloud, say) has nothing PeerParty can invite to,
/// however many rooms it made, so it's told to use the game's own codes. A game hosting
/// a Photon room on PeerParty's own servers (`photon_room`) is hosting: Photon has no
/// invites, so it's told how the others join it instead of to host first. A game whose
/// services were turned off for it (`off`, PEERPARTY_SKIP) and that plays through none
/// PeerParty answers is told which, not to host first: its lobbies never reach
/// PeerParty. A game in a lobby PeerParty knows (`in_lobby`) but with no way to hand it
/// over is told how PeerParty learns one (`LearnFirst`). The layers that count as off
/// are the ones lobbies travel through (catalog.rs `carries_lobbies`: voice and sign-in
/// layers don't), and the front ends that carry an invite are the catalog's too
/// (`Front::carries_invites`).
fn why_not_for(fronts: &[net::Front], official: &[&str], off: &[&str], photon_room: Option<String>, in_lobby: bool) -> InviteBlock {
    let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if !official.is_empty() {
        return InviteBlock::Official(names(official));
    }
    if let Some(room) = photon_room {
        return InviteBlock::PhotonRoom(room);
    }
    let carried = fronts.iter().any(|f| f.carries_invites());
    let off: Vec<&str> = off.iter().copied().filter(|l| peerparty_protocol::catalog::carries_lobbies(l)).collect();
    if !off.is_empty() && !carried {
        return InviteBlock::LayersOff(names(&off));
    }
    if !fronts.is_empty() && !carried {
        let labels: Vec<&str> = fronts.iter().map(|f| f.label()).collect();
        return InviteBlock::Unsupported(names(&labels));
    }
    if in_lobby {
        return InviteBlock::LearnFirst;
    }
    InviteBlock::HostFirst
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
        let fronts = net::fronts();
        let why = why_not_with(&fronts);
        log!("invite: nothing to invite to: {}", why.english());
        for d in carriers().iter().filter(|c| fronts.contains(&c.front)).filter_map(|c| (c.diagnosis)()) {
            log!("invite: {d}");
        }
        crate::ipc::note_notice(Notice::cant_invite(why));
        beep(false);
        return 0;
    }
    let reached = deliver_to(target, &connect, lobby);
    let n = reached.len();
    if n > 0 {
        let names: Vec<String> = reached.iter().map(|p| name_of(*p)).collect();
        crate::ipc::note_notice(Notice::invite_sent(&names));
    } else {
        crate::ipc::note_notice(Notice::nobody_to_invite());
    }
    beep(n > 0);
    n
}

/// Invites each target that isn't already in the lobby. A game asked to join the
/// lobby it's in may leave and rejoin it, which drops a connection
/// that was working. Everyone (`to` None) means the players on this network only: a
/// room's friends and public players get an invite only when picked by name.
fn deliver(to: Option<PeerId>, connect: &str, lobby: Option<Lobby>) -> usize {
    deliver_to(to, connect, lobby).len()
}

/// `deliver`, saying who it reached.
fn deliver_to(to: Option<PeerId>, connect: &str, lobby: Option<Lobby>) -> Vec<PeerId> {
    let Some(lan) = net::lan() else { return Vec::new() };
    let inside = if lobby.is_none() { current_lobby() } else { None };
    let targets: Vec<PeerId> = lan
        .peers()
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| match to {
            Some(t) => t == *p,
            None => !matches!(lan.peer_trust(*p), Some(peerparty_core::Trust::Room | peerparty_core::Trust::Public)),
        })
        // Whoever is in the lobby we're in, through any SDK, is there already.
        .filter(|p| lobby.as_ref().or(inside.as_ref()).is_none_or(|l| l.member(*p).is_none()))
        .filter(|p| !connect.starts_with(CODE_PREFIX) || room_lobby().is_none_or(|r| r.member(*p).is_none()))
        .collect();
    let reached: Vec<PeerId> = targets.into_iter().filter(|p| lan.invite(Some(*p), connect, lobby.as_ref()) > 0).collect();
    log!(
        "invite sent to {} player(s) not already in the lobby: connect={connect:?} lobby={:?}",
        reached.len(),
        lobby.as_ref().map(|l| l.id.as_str())
    );
    reached
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
        } else if join_request(&self.connect).is_some() {
            "asks to join you"
        } else {
            "invited you"
        }
    }
}

type Key = (PeerId, String, Option<String>);

static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
/// Invites the player accepted, and when a front end was first told so. A game often
/// runs two SDKs' front ends, each handed the same invite and each asking `decision`:
/// all of them hear "accepted" for `FORCED_FOR` from the first, not only whichever
/// asked first.
static FORCED: Mutex<Vec<(Key, Option<Instant>)>> = Mutex::new(Vec::new());
const FORCED_FOR: Duration = Duration::from_secs(2);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// Bumped whenever the pending list changes, so ipc.rs knows to send it.
pub static CHANGES: AtomicU64 = AtomicU64::new(0);

/// Unanswered invites are dropped after this long.
const PENDING_FOR: Duration = Duration::from_secs(600);
/// Invites waiting at most, from one player and in all: a new one takes the place of
/// that player's oldest (or the oldest of all). Nobody sorts through more than a few,
/// and each is a row in the overlay and in every state update sent to it.
const PENDING_PER_PLAYER: usize = 3;
const PENDING_MAX: usize = 32;

/// Adds an invite to the waiting ones, within the limits.
fn push_pending(p: &mut Vec<Pending>, new: Pending) {
    while p.iter().filter(|x| x.from == new.from).count() >= PENDING_PER_PLAYER {
        let Some(i) = p.iter().position(|x| x.from == new.from) else { break };
        p.remove(i);
    }
    while p.len() >= PENDING_MAX {
        p.remove(0);
    }
    p.push(new);
}

fn key(from: PeerId, connect: &str, lobby: Option<&Lobby>) -> Key {
    (from, connect.to_string(), lobby.map(|l| l.id.clone()))
}

/// What to do with this invite (front ends call it once per delivery).
pub fn decision(from: PeerId, connect: &str, lobby: Option<&Lobby>) -> Decision {
    let k = key(from, connect, lobby);
    {
        let mut j = crate::sys::lock(&JOINING);
        if let Some(i) = j.iter().position(|(p, t)| *p == from && t.elapsed() < JOIN_WAIT) {
            j.remove(i);
            log!("join: {from} invited us as asked; accepted");
            crate::sys::lock(&FORCED).push((k, Some(Instant::now())));
            return Decision::Forced;
        }
    }
    {
        let mut forced = crate::sys::lock(&FORCED);
        forced.retain(|(_, told)| told.is_none_or(|t| t.elapsed() < FORCED_FOR));
        if let Some((_, told)) = forced.iter_mut().find(|(f, _)| *f == k) {
            told.get_or_insert_with(Instant::now);
            return Decision::Forced;
        }
    }
    if auto_for(from) { Decision::Auto } else { Decision::Ask }
}

/// Invites accepted without asking: when the player said so, or when there's no
/// overlay to ask in, and never from a public player (a stranger), whose invites
/// always wait for the player.
fn auto_for(from: PeerId) -> bool {
    let trust = crate::net::lan_if_started().and_then(|l| l.peer_trust(from));
    auto_rule(crate::config::settings::auto_accept(), overlay_on(), trust)
}

/// `auto_for` from what it depends on. With the overlay off (the game started without
/// the launcher's pipe) an invite that waited would wait until it expired, since
/// nothing can show it: LAN and room players' invites go through as if auto-accept
/// were on. A stranger's still wait, and so go nowhere, which is the safe side.
fn auto_rule(auto_accept: bool, overlay: bool, trust: Option<peerparty_core::Trust>) -> bool {
    trust != Some(peerparty_core::Trust::Public) && (auto_accept || !overlay)
}

/// Whether the overlay can show waiting invites. Tests run without the launcher's pipe
/// and test the waiting itself, so they count as having one.
fn overlay_on() -> bool {
    cfg!(test) || crate::ipc::enabled()
}

/// A fresh invite from the LAN (net.rs, before it reaches the front ends). Unless
/// invites are accepted at once, it waits for the player.
pub fn arrived(ev: &Event) {
    let Event::Invite { from, connect, lobby } = ev else { return };
    if auto_for(*from) || is_reply(connect) || joining(*from) {
        return;
    }
    let mut p = crate::sys::lock(&PENDING);
    p.retain(|x| x.at.elapsed() < PENDING_FOR);
    let k = key(*from, connect, lobby.as_ref());
    p.retain(|x| key(x.from, &x.connect, x.lobby.as_ref()) != k);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    log!("invite {id} from {from} waits for the player (auto-accept is off)");
    push_pending(&mut p, Pending { id, from: *from, connect: connect.clone(), lobby: lobby.clone(), at: Instant::now() });
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
    let Some(code) = crate::invite::typing::clean_code(code) else { return };
    let connect = format!("{CODE_PREFIX}{code}");
    let mut p = crate::sys::lock(&PENDING);
    p.retain(|x| x.at.elapsed() < PENDING_FOR && !(x.from == from && x.connect == connect));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    log!("room code {code} from {from} waits for the player (pending {id})");
    push_pending(&mut p, Pending { id, from, connect, lobby: None, at: Instant::now() });
    CHANGES.fetch_add(1, Ordering::Relaxed);
}

/// The overlay's "type what's on the clipboard": a code copied from a chat, say.
pub fn type_clipboard() {
    match crate::invite::typing::clipboard_text().and_then(|t| crate::invite::typing::clean_code(&t)) {
        Some(code) => crate::invite::typing::type_code(code),
        None => {
            log!("type: nothing on the clipboard that looks like a room code");
            crate::ipc::note_notice(Notice::no_code_to_paste());
        }
    }
}

/// Invites waiting for the player, oldest first.
pub fn pending() -> Vec<Pending> {
    let mut p = crate::sys::lock(&PENDING);
    p.retain(|x| x.at.elapsed() < PENDING_FOR);
    p.clone()
}

fn take_pending(id: u64) -> Option<Pending> {
    let mut p = crate::sys::lock(&PENDING);
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
        crate::invite::typing::type_code(code.to_string());
        return true;
    }
    log!("invite {id} from {} accepted by the player", inv.from);
    crate::sys::lock(&FORCED).push((key(inv.from, &inv.connect, inv.lobby.as_ref()), None));
    net::redeliver(Event::Invite { from: inv.from, connect: inv.connect, lobby: inv.lobby });
    true
}

/// The player declined: a request to join hears no, anything else is just dropped.
pub fn decline(id: u64) -> bool {
    let Some(inv) = take_pending(id) else { return false };
    log!("invite {id} from {} declined by the player", inv.from);
    if let Some(j) = join_request(&inv.connect) {
        (j.decline)(inv.from);
    }
    true
}

// ---- triggers ----

/// Triggers act in the game in front: `--invite` reaches every copy.
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

/// Waits on the `--invite` event and the launcher's button.
fn run() {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject};
    // Manual reset: `peerparty --invite` sets it, then clears it a moment later.
    let open = |name: &str| {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) }
    };
    let (event, last, ack) = (open(EVENT_NAME), open(EVENT_LAST), open(EVENT_ACK));
    let front = front_slot();
    let me = std::process::id();
    let mut last_was_set = false;
    log!("invite triggers: `peerparty --invite`, the launcher's button, the overlay");
    let mut was_set = false;
    loop {
        let wait = if !was_set && !last_was_set { 1000 } else { 50 };
        if event.is_null() {
            std::thread::sleep(Duration::from_millis(u64::from(wait)));
        } else {
            // Sleeps until either is set, or the time is up.
            if last.is_null() {
                unsafe { WaitForSingleObject(event, wait) };
            } else {
                unsafe { WaitForMultipleObjects(2, [event, last].as_ptr(), 0, wait) };
            }
            let set = unsafe { WaitForSingleObject(event, 0) } == WAIT_OBJECT_0;
            let in_front = in_front();
            if in_front && !front.is_null() {
                unsafe { front.write_volatile(me) };
            }
            // The launcher's button: its window is in front, so the game meant is the
            // one that was before it.
            let last_set = !last.is_null() && unsafe { WaitForSingleObject(last, 0) } == WAIT_OBJECT_0;
            let mine = in_front || (!front.is_null() && unsafe { front.read_volatile() } == me);
            if (set && !was_set && in_front) || (last_set && !last_was_set && mine) {
                log!("invite: triggered by {}", if set { "peerparty --invite" } else { "the launcher" });
                if !ack.is_null() {
                    unsafe { SetEvent(ack) };
                }
                everyone();
            }
            if set || last_set {
                // Still set: don't spin on it until the sender clears it.
                std::thread::sleep(Duration::from_millis(50));
            }
            was_set = set;
            last_was_set = last_set;
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
        crate::config::settings::set("auto_accept", "ask");
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
        // A second front end handed the same delivery hears the same.
        assert_eq!(decision(from, "+connect_lobby 1", None), Decision::Forced);
        // Used up a moment later: the next delivery is an ordinary one again.
        for (_, told) in crate::sys::lock(&FORCED).iter_mut().filter(|(k, _)| k.0 == from) {
            *told = Instant::now().checked_sub(FORCED_FOR);
        }
        assert_eq!(decision(from, "+connect_lobby 1", None), Decision::Ask);
        arrived(&ev);
        let id = pending().iter().find(|x| x.from == from).unwrap().id;
        assert!(decline(id));
        assert!(!decline(id));
        // Auto-accept on: nothing waits.
        crate::config::settings::set("auto_accept", "auto");
        arrived(&ev);
        assert!(pending().iter().all(|x| x.from != from));
        assert_eq!(decision(from, "+connect_lobby 1", None), Decision::Auto);
        crate::config::settings::set("auto_accept", "ask");
        // Asked to be invited: that player's invite is accepted without asking.
        let host = PeerId([9; 16]);
        crate::sys::lock(&JOINING).push((host, Instant::now()));
        let from_host = Event::Invite { from: host, connect: "x".into(), lobby: None };
        arrived(&from_host);
        assert!(pending().iter().all(|x| x.from != host));
        assert_eq!(decision(host, "x", None), Decision::Forced);
    }

    /// With the overlay off nothing can answer a waiting invite, so a LAN or room
    /// player's goes through; a stranger's never does without the player.
    #[test]
    fn without_the_overlay_friends_invites_go_through() {
        use peerparty_core::Trust;
        for trust in [None, Some(Trust::Lan), Some(Trust::Room)] {
            assert!(auto_rule(false, false, trust), "{trust:?}, overlay off");
            assert!(!auto_rule(false, true, trust), "{trust:?}, overlay on, asking");
            assert!(auto_rule(true, true, trust), "{trust:?}, auto-accept on");
        }
        for (auto, overlay) in [(false, false), (false, true), (true, false), (true, true)] {
            assert!(!auto_rule(auto, overlay, Some(Trust::Public)), "public, {auto} {overlay}");
        }
    }

    #[test]
    fn waiting_invites_are_bounded() {
        let inv = |from: u8, n: u64| Pending { id: n, from: PeerId([from; 16]), connect: format!("c{n}"), lobby: None, at: Instant::now() };
        let mut p = Vec::new();
        for n in 0..10 {
            push_pending(&mut p, inv(1, n));
        }
        // One player's newest few are kept.
        assert_eq!(p.iter().map(|x| x.id).collect::<Vec<_>>(), [7, 8, 9]);
        // Many players: the oldest of all make room.
        for from in 2..100u8 {
            push_pending(&mut p, inv(from, 1000 + u64::from(from)));
        }
        assert_eq!(p.len(), PENDING_MAX);
        assert_eq!(p.last().unwrap().id, 1099);
        assert!(p.iter().all(|x| x.from != PeerId([1; 16])), "the oldest went first");
    }

    /// The game's own invite names its lobby: kept with the id as a placeholder, and
    /// filled with whichever lobby the game is in later.
    #[test]
    fn a_games_own_invite_is_learned_with_its_lobby_id_as_a_placeholder() {
        let own = "gameId:ioKDkLB2F3qPEMW3GfcPqy|isPasswordRequired:False|isPrivate:False|isCrossPlatform:False|maxPlayers:4|currentPlayers:1";
        let t = template_of(own, "ioKDkLB2F3qPEMW3GfcPqy").unwrap();
        assert_eq!(t, "gameId:{peerparty:lobby}|isPasswordRequired:False|isPrivate:False|isCrossPlatform:False|maxPlayers:4|currentPlayers:1");
        assert_eq!(fill(&t, "Zz9NewLobby"), own.replace("ioKDkLB2F3qPEMW3GfcPqy", "Zz9NewLobby"));
        // Nothing to learn from an invite that doesn't name the lobby, from PeerParty's
        // own kinds, or from an id too short to be one.
        assert_eq!(template_of("+connect 10.0.0.2", "ioKDkLB2F3qPEMW3GfcPqy"), None);
        assert_eq!(template_of(JOIN_REQUEST, "join"), None);
        assert_eq!(template_of(&format!("{CODE_PREFIX}ABCD"), "ABCD"), None);
        assert_eq!(template_of("gameId:1|x", "1"), None);
    }

    #[test]
    fn says_why_there_is_nothing_to_invite_to() {
        use crate::net::Front;
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // PlayFab Party only, whose invites PeerParty can't carry.
        let party = why_not_for(&[Front::Party], &[], &[], None, false);
        assert_eq!(party, InviteBlock::Unsupported(names(&["PlayFab Party"])));
        assert!(party.english().contains("PlayFab Party"), "{party:?}");
        // A game with a lobby service just hasn't hosted yet, nor has one that hasn't
        // started any online service.
        assert_eq!(why_not_for(&[Front::Party, Front::Eos], &[], &[], None, false), InviteBlock::HostFirst);
        assert_eq!(why_not_for(&[], &[], &[], None, false), InviteBlock::HostFirst);
        assert_eq!(InviteBlock::HostFirst.english(), "Host or join a game first");
        // In a lobby (a Unity lobby, say) PeerParty can't hand over until the game has
        // shown it an invite of its own.
        assert_eq!(why_not_for(&[Front::Steam, Front::Ugs], &[], &[], None, true), InviteBlock::LearnFirst);
        // Rooms on Photon's own cloud (a Steam game whose multiplayer is Photon's):
        // no room the player makes is one PeerParty can invite to.
        let photon = why_not_for(&[Front::Steam], &["Photon"], &[], None, false);
        assert_eq!(photon, InviteBlock::Official(names(&["Photon"])));
        let said = photon.english();
        assert!(said.contains("Photon's own servers") && said.contains("room codes"), "{said}");
        // A room on PeerParty's own Photon servers: the game has hosted, and its players
        // join from its own menu (Photon has no invites), not "host first".
        let room = why_not_for(&[Front::Steam], &[], &[], Some("EUCSH".into()), false);
        assert_eq!(room, InviteBlock::PhotonRoom("EUCSH".into()));
        assert!(room.english().ends_with("(room EUCSH)"));
    }

    /// A Steam game whose Steam layer the player turned off makes its lobbies where
    /// PeerParty never sees them: it's told the layer is off, not to host first.
    #[test]
    fn a_turned_off_service_is_named() {
        use crate::net::Front;
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(why_not_for(&[], &[], &["steam"], None, false), InviteBlock::LayersOff(names(&["steam"])));
        assert_eq!(why_not_for(&[Front::Party], &[], &["eos", "galaxy"], None, false), InviteBlock::LayersOff(names(&["eos", "galaxy"])));
        // Voice off doesn't stop invites.
        assert_eq!(why_not_for(&[], &[], &["vivox"], None, false), InviteBlock::HostFirst);
        // Another service PeerParty answers carries the invite: hosting there is what's missing.
        assert_eq!(why_not_for(&[Front::Eos], &[], &["steam"], None, false), InviteBlock::HostFirst);
    }
}
