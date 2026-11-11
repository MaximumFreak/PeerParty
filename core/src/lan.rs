//! The engine: peers, lobbies and channels, driven by messages from the mesh and calls
//! from a front end. A front end calls `poll` from the game's own tick (EOS_Platform_Tick,
//! SteamAPI_RunCallbacks) and turns the events into its SDK's callbacks, so game code
//! only ever runs on the game's thread.
//!
//! Lobbies are owner-authoritative: the owner applies every change and sends the whole
//! lobby to every member, who diff it against what they had to raise join / leave /
//! update / promote events. A lost owner hands the lobby to the lowest remaining id when
//! host migration is on, otherwise the lobby closes.
//!
//! Channels are keyed by (peer, socket name) and open once both sides agree: one side
//! asks (by sending or accepting), the other accepts (or asks too). Data on a channel
//! the game hasn't agreed to is a request, not data.

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

use crate::id::{Identity, PeerId};
use crate::inet;
use crate::lobby::{self, Attr, Lobby, Member, Permission, Query};
use crate::mesh::{self, Conn, Mesh, Sink};
use crate::wire::{JoinResult, Msg};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);
const JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Received packets kept for the game at most: a game that stops reading, or a socket
/// nobody reads, would otherwise grow the inbox without limit. The oldest go first.
const INBOX_MAX_PACKETS: usize = 65_536;
const INBOX_MAX_BYTES: usize = 64 << 20;
/// What one public player's packets may take of the inbox: past it, its new ones are
/// dropped, so a stranger sending faster than the game reads can't push friends'
/// packets out (the inbox drops its oldest when full, whoever sent them).
const PUBLIC_INBOX_PACKETS: usize = 8192;
const PUBLIC_INBOX_BYTES: usize = 8 << 20;
/// Packets held per channel for a peer that hasn't agreed yet, at most; past that a send
/// is dropped (and says so).
const DELAYED_MAX_PACKETS: usize = 4096;
const DELAYED_MAX_BYTES: usize = 16 << 20;
/// A new connection from a peer we're connected to, from the same address, replaces the
/// old one outright once the old one is this old: the peer restarted or lost the old
/// connection without us noticing. Both sides dialing at once (the tie-break case) only
/// happens within a dial's timeouts of the first connection, well under this.
const RECONNECT_AFTER: Duration = Duration::from_secs(15);
/// After moving a peer to a better connection we keep the old one open until the peer
/// says it moved too (`Msg::Moved`), so nothing either side queued on it is lost. A peer
/// that doesn't (an older copy) gets this long before the old connection is retired.
const MOVE_WAIT: Duration = Duration::from_secs(10);
/// Ordered messages held at most while a move finishes (see `Peer::held`); past that
/// they're delivered as they come and order across the two connections isn't kept.
const MOVE_HOLD_MAX: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberStatus {
    Joined,
    Left,
    Disconnected,
    Kicked,
    Promoted,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    ByLocal,
    ByPeer,
    /// The peer's connection to us went away.
    Lost,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    PeerJoined(PeerId),
    PeerLeft(PeerId),
    SearchDone {
        search: u64,
        results: Vec<Lobby>,
    },
    JoinDone {
        lobby: String,
        result: JoinResult,
    },
    LobbyUpdated {
        lobby: String,
    },
    MemberStatus {
        lobby: String,
        member: PeerId,
        status: MemberStatus,
    },
    MemberUpdated {
        lobby: String,
        member: PeerId,
    },
    ChanRequest {
        peer: PeerId,
        socket: String,
    },
    ChanOpen {
        peer: PeerId,
        socket: String,
    },
    ChanClosed {
        peer: PeerId,
        socket: String,
        reason: CloseReason,
    },
    /// A peer's presence (rich presence) changed; read it with `Lan::presence`.
    PresenceChanged(PeerId),
    /// A peer asked us to join it.
    Invite {
        from: PeerId,
        connect: String,
        lobby: Option<Lobby>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LobbyError {
    NotFound,
    AlreadyIn,
    /// The lobby's owner isn't a peer we're connected to.
    OwnerUnknown,
    NotOwner,
    NotMember,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub peer: PeerId,
    pub socket: String,
    pub channel: u8,
    pub data: Vec<u8>,
    /// Arrived over the connection's ordered, reliable stream (TCP), rather than as a
    /// datagram that may be lost or reordered.
    pub ordered: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendResult {
    Sent,
    /// Held until the peer agrees (the caller allowed delayed delivery).
    Held,
    /// Dropped: the channel isn't open and the caller didn't allow holding it.
    Dropped,
    /// No channel, and the caller declined to open one.
    NoChannel,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Hosting,
    Joining,
    Joined,
}

struct Held {
    lobby: Lobby,
    role: Role,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChanState {
    /// We asked; waiting for the peer.
    Pending,
    /// The peer asked; waiting for the game.
    Requested,
    Open,
}

struct Chan {
    state: ChanState,
    delayed: Vec<(u8, bool, Vec<u8>)>,
    delayed_bytes: usize,
}

impl Chan {
    fn new(state: ChanState) -> Chan {
        Chan {
            state,
            delayed: Vec::new(),
            delayed_bytes: 0,
        }
    }

    /// Holds a packet until the channel opens; false when the hold is full.
    fn hold(&mut self, channel: u8, reliable: bool, data: &[u8], max_n: usize, max_bytes: usize) -> bool {
        if self.delayed.len() >= max_n || self.delayed_bytes + data.len() > max_bytes {
            return false;
        }
        self.delayed_bytes += data.len();
        self.delayed.push((channel, reliable, data.to_vec()));
        true
    }

    fn take_delayed(&mut self) -> Vec<(u8, bool, Vec<u8>)> {
        self.delayed_bytes = 0;
        std::mem::take(&mut self.delayed)
    }
}

/// Received channel data waiting for the game, bounded (see INBOX_MAX_PACKETS).
#[derive(Default)]
struct Inbox {
    q: VecDeque<Packet>,
    bytes: usize,
    /// Packets and bytes in `q` by sender.
    per_peer: HashMap<PeerId, (usize, usize)>,
    /// Packets dropped to make room, ever.
    dropped: u64,
    /// Public players' packets dropped for going past their share, ever.
    over_share: u64,
}

impl Inbox {
    fn push(&mut self, p: Packet) {
        self.push_capped(p, INBOX_MAX_PACKETS, INBOX_MAX_BYTES);
    }

    /// A public player's packet: dropped if its packets already hold their share.
    fn push_stranger(&mut self, p: Packet) {
        self.push_shared(p, PUBLIC_INBOX_PACKETS, PUBLIC_INBOX_BYTES);
    }

    fn push_shared(&mut self, p: Packet, max_n: usize, max_bytes: usize) {
        let (n, bytes) = self.per_peer.get(&p.peer).copied().unwrap_or_default();
        if n + 1 > max_n || (n > 0 && bytes + p.data.len() > max_bytes) {
            self.over_share += 1;
            return;
        }
        self.push(p);
    }

    fn push_capped(&mut self, p: Packet, max_n: usize, max_bytes: usize) {
        self.bytes += p.data.len();
        let e = self.per_peer.entry(p.peer).or_default();
        e.0 += 1;
        e.1 += p.data.len();
        self.q.push_back(p);
        while self.q.len() > max_n || (self.bytes > max_bytes && self.q.len() > 1) {
            let Some(old) = self.q.pop_front() else { break };
            self.forget(&old);
            self.dropped += 1;
        }
    }

    /// Takes a packet that left `q` off the counts.
    fn forget(&mut self, p: &Packet) {
        self.bytes -= p.data.len();
        if let Some(e) = self.per_peer.get_mut(&p.peer) {
            e.0 -= 1;
            e.1 -= p.data.len();
            if e.0 == 0 {
                self.per_peer.remove(&p.peer);
            }
        }
    }

    fn remove(&mut self, i: usize) -> Option<Packet> {
        let p = self.q.remove(i)?;
        self.forget(&p);
        Some(p)
    }

    fn retain(&mut self, f: impl FnMut(&Packet) -> bool) {
        self.q.retain(f);
        self.bytes = 0;
        self.per_peer.clear();
        for p in &self.q {
            self.bytes += p.data.len();
            let e = self.per_peer.entry(p.peer).or_default();
            e.0 += 1;
            e.1 += p.data.len();
        }
    }

    fn iter(&self) -> impl Iterator<Item = &Packet> {
        self.q.iter()
    }
}

struct Search {
    query: Query,
    awaiting: HashSet<PeerId>,
    results: Vec<Lobby>,
    deadline: Instant,
}

struct Peer {
    conn: Arc<Conn>,
    name: String,
    /// The address games are told (`Lan::peer_ip`), fixed when the peer joins so it
    /// stays the same when the peer moves to a better path.
    shown: IpAddr,
    /// Connections this one replaced (a better path came up) that may still carry the
    /// peer's last messages, oldest first. Each ends with the peer's `Msg::Moved`.
    prior: VecDeque<Prior>,
    /// Ordered messages that came on `conn` while `prior` still drains: delivered once it
    /// has, so the move keeps their order.
    held: Vec<Msg>,
    /// The peer sent `Msg::Moved` on this connection, which is still our live one: it
    /// moved to a new one we haven't seen yet.
    moved_off: Option<Arc<Conn>>,
    /// When the peer joined (it keeps this through a move to a better path).
    joined: Instant,
    /// The invites a public player may still send us (see PUBLIC_INVITE_BURST).
    invites: Ration,
    /// A public player: connected to us only because public sessions are on (kept up
    /// to date as the peer moves between paths).
    stranger: bool,
    /// When we last invited this peer ourselves.
    invited: Option<Instant>,
}

/// A token bucket: `burst` at once, then one more every `every`.
struct Ration {
    tokens: f32,
    at: Instant,
}

impl Ration {
    fn new(burst: f32, now: Instant) -> Ration {
        Ration { tokens: burst, at: now }
    }

    /// Takes one if there's one to take.
    fn take(&mut self, now: Instant, burst: f32, every: Duration) -> bool {
        self.tokens = (self.tokens + now.saturating_duration_since(self.at).as_secs_f32() / every.as_secs_f32()).min(burst);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

struct Prior {
    conn: Arc<Conn>,
    /// Messages that came on it while an older one still drains.
    held: Vec<Msg>,
    /// The peer sent its last message on it (or it closed).
    done: bool,
}

impl Peer {
    fn new(conn: Arc<Conn>, name: String, shown: IpAddr, stranger: bool) -> Peer {
        let now = Instant::now();
        Peer { conn, name, shown, prior: VecDeque::new(), held: Vec::new(), moved_off: None, joined: now, invites: Ration::new(PUBLIC_INVITE_BURST, now), stranger, invited: None }
    }

    fn held_count(&self) -> usize {
        self.held.len() + self.prior.iter().map(|p| p.held.len()).sum::<usize>()
    }
}

/// How good a path is: a better one replaces a worse one without the peer leaving.
fn rank(t: Trust) -> u8 {
    match t {
        Trust::Lan => 2,
        Trust::Room => 1,
        Trust::Public => 0,
    }
}

#[derive(Default)]
struct State {
    peers: HashMap<PeerId, Peer>,
    lobbies: HashMap<String, Held>,
    searches: HashMap<u64, Search>,
    joins: HashMap<String, Instant>,
    next_search: u64,
    chans: HashMap<(PeerId, String), Chan>,
    inbox: Inbox,
    events: VecDeque<Event>,
    my_presence: Vec<(String, String)>,
    presence: HashMap<PeerId, Vec<(String, String)>>,
    /// Strangers' requests to join lobbies that aren't public, and when each is
    /// answered "not allowed" if nothing has vouched for them by then (see JOIN_WAIT).
    waiting: Vec<(PeerId, String, Instant)>,
    /// Joins by id a public player answered, held until JOIN_CLAIM_WAIT is up: the
    /// first such answer for each lobby, and when it counts.
    claims: HashMap<String, (PeerId, JoinResult, Instant)>,
}

struct Inner {
    me: PeerId,
    name: String,
    slot: u32,
    game: String,
    state: Mutex<State>,
    log: fn(&str),
    /// Mesh connections that run through an internet tunnel, by their far end's
    /// (loopback) address: who's there and how far we trust them. No internet address.
    /// An entry outlives its tunnel (`down`) until the mesh connection on it is gone, so
    /// what's still draining off it keeps its trust.
    tunnels: Mutex<HashMap<SocketAddr, TunnelEntry>>,
    /// Voice and party chat with public players: off unless `PEERPARTY_PUBLIC_VOICE=1`
    /// (the launcher's "Voice chat with strangers" setting) or `set_public_voice`.
    public_voice: std::sync::atomic::AtomicBool,
    /// (peer, socket) pairs whose dropped traffic has been logged: each once.
    drop_logged: Mutex<HashSet<(PeerId, String)>>,
    /// Ourselves, for timers.
    this: OnceLock<Weak<Inner>>,
}

struct TunnelEntry {
    tunnel: inet::Tunnel,
    down: Option<Instant>,
}

/// How a peer reaches us, and so how far it's trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    /// On this LAN or PC.
    Lan,
    /// Through a room code (people we gave the code to).
    Room,
    /// A stranger from public sessions: lobbies, game traffic and (when it's turned
    /// on) voice; no file, overlay or IP extras.
    Public,
}

/// Voice sockets: what public players don't get unless voice with them is turned on
/// (`PEERPARTY_PUBLIC_VOICE=1`). That's voice.rs's rooms, `rtc:<room>`: EOS RTC rooms,
/// EOS lobby voice (`rtc:<lobby>:peerparty-voice`) and PlayFab Party / Xbox party chat
/// (`rtc:party:<guid>`, voice and chat text). Never game data: PlayFab Party's endpoint
/// messages ride `party:<guid>`, and Steam (`steam:`), EOS P2P, Photon, UGS, GOG and
/// the IP forwarder (`peerparty-ip:`) have sockets of their own.
fn voice_socket(socket: &str) -> bool {
    socket.starts_with("rtc:")
}

/// `PEERPARTY_PUBLIC_VOICE`: voice with public players is off unless it's set, and to
/// something other than 0/off/false/no. A microphone isn't opened to strangers by a
/// variable that's missing.
fn public_voice_env() -> bool {
    std::env::var("PEERPARTY_PUBLIC_VOICE")
        .is_ok_and(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "off" | "false" | "no"))
}

fn msg_socket(m: &Msg) -> Option<&str> {
    match m {
        Msg::ChanRequest { socket } | Msg::ChanAccept { socket } | Msg::ChanClose { socket } => Some(socket),
        Msg::ChanData { socket, .. } => Some(socket),
        _ => None,
    }
}

/// What a public player's presence may hold.
const PUBLIC_PRESENCE_KEYS: usize = 32;
const PUBLIC_PRESENCE_BYTES: usize = 4096;

fn cap_presence(kv: &mut Vec<(String, String)>) {
    kv.truncate(PUBLIC_PRESENCE_KEYS);
    let mut total = 0;
    kv.retain(|(k, v)| {
        total += k.len() + v.len();
        total <= PUBLIC_PRESENCE_BYTES
    });
}

/// What a public player's lobbies may hold (EOS's own limits are lower: 64 attributes,
/// 1000-character values), so a stranger can't hand the game oversized or endless data.
const PUBLIC_ATTRS: usize = 100;
const PUBLIC_KEY_BYTES: usize = 256;
const PUBLIC_VALUE_BYTES: usize = 1000;
const PUBLIC_MEMBERS: usize = 64;
const PUBLIC_ID_BYTES: usize = 256;
const PUBLIC_CONNECT_BYTES: usize = 1024;
/// Channels a public player may have with us at once (each stays until it's closed or
/// the player leaves, and each new one is an event for the game), and how long a
/// channel's name may be. The names are PeerParty's own ("steam:...", "party:<guid>",
/// "peerparty-ip:t:<port>:<n>"): a few dozen bytes, a handful of channels.
const PUBLIC_CHANNELS: usize = 256;
const PUBLIC_SOCKET_BYTES: usize = 256;
/// Invites (and requests to join, and room codes) a public player may send: this many at
/// once, then one every `PUBLIC_INVITE_EVERY`. Each one waits in the overlay, tells the
/// game and is logged, so a stranger doesn't get to send them without end.
/// A public player asking to join a lobby that isn't public may be a moment ahead of
/// the news that puts them in a lobby with us (it comes from that lobby's owner, by
/// another path). Their request waits this long for it, and at most this many wait.
const JOIN_WAIT: Duration = Duration::from_secs(2);
/// A join by id asks every peer, and any of them can answer as the owner. A public
/// player's answer is held this long, and counts only if no LAN or room player answers
/// meanwhile: a stranger can't answer for a friend's lobby (its own lobbies it can).
const JOIN_CLAIM_WAIT: Duration = Duration::from_secs(1);
const JOINS_WAITING: usize = 64;
/// How long our invite to a public player stands (see `State::known`).
const INVITE_STANDS: Duration = Duration::from_secs(15 * 60);
const PUBLIC_INVITE_BURST: f32 = 5.0;
const PUBLIC_INVITE_EVERY: Duration = Duration::from_secs(4);

/// Drops attributes that are too big and keeps at most `PUBLIC_ATTRS`.
fn cap_attrs(attrs: &mut Vec<Attr>) {
    attrs.retain(|a| {
        a.key.len() <= PUBLIC_KEY_BYTES
            && match &a.value {
                lobby::Value::Str(s) => s.len() <= PUBLIC_VALUE_BYTES,
                lobby::Value::Double(d) => d.is_finite(),
                _ => true,
            }
    });
    attrs.truncate(PUBLIC_ATTRS);
}

/// A public player's lobby made safe to hand the game, or `false` to drop it (names too
/// long to be real, or more members than a lobby can hold).
fn cap_lobby(l: &mut Lobby) -> bool {
    if l.id.len() > PUBLIC_ID_BYTES
        || l.namespace.len() > PUBLIC_ID_BYTES
        || l.bucket.len() > PUBLIC_ID_BYTES
        || l.members.len() > PUBLIC_MEMBERS
    {
        return false;
    }
    l.max_members = l.max_members.min(PUBLIC_MEMBERS as u32);
    cap_attrs(&mut l.attrs);
    for m in &mut l.members {
        cap_attrs(&mut m.attrs);
    }
    true
}

/// Messages that can carry addresses a game put in (or a stranger did): lobby, member
/// and presence attributes, and invites.
pub(crate) fn may_carry_addresses(msg: &Msg) -> bool {
    matches!(
        msg,
        Msg::Presence { .. } | Msg::SearchReply { .. } | Msg::State { .. } | Msg::MemberAttrs { .. } | Msg::Invite { .. }
    )
}

fn scrub_attrs(attrs: &mut [Attr], with: std::net::Ipv4Addr) {
    for a in attrs {
        match &mut a.value {
            lobby::Value::Str(s) => {
                if let Some(clean) = inet::redact::scrub(&a.key, s, with) {
                    *s = clean;
                }
            }
            // An address held as a number, under a key that names one.
            lobby::Value::Int(n) => {
                if let Some(clean) = inet::redact::scrub_int(&a.key, *n, with) {
                    *n = clean;
                }
            }
            _ => {}
        }
    }
}

/// A lobby's own attributes take `with` (the owner's or sender's stand-in); each
/// member's take that member's own stand-in, so a member the owner relays (one on its
/// LAN or in its room) isn't shown at the owner's address.
fn scrub_lobby(l: &mut Lobby, with: std::net::Ipv4Addr) {
    scrub_attrs(&mut l.attrs, with);
    for m in &mut l.members {
        let own = if m.id == PeerId::default() { with } else { inet::redact::stand_in(&m.id.0) };
        scrub_attrs(&mut m.attrs, own);
    }
}

/// Replaces the IP addresses in what a public session carries to or from the game
/// (lobby, member and presence attributes, invite connect strings) with `with`: our own
/// stand-in on the way out, so no game ever tells a stranger our real or LAN address,
/// and the sender's on the way in, so a game that connects to an address it read goes
/// through the tunnel (`fwd`) and never straight to an address a stranger chose.
/// Stand-ins already in the data are kept (a lobby lists its other members' own).
pub(crate) fn scrub_public(msg: &mut Msg, with: std::net::Ipv4Addr) {
    match msg {
        Msg::Presence { kv } => {
            for (k, v) in kv {
                if let Some(clean) = inet::redact::scrub(k, v, with) {
                    *v = clean;
                }
            }
        }
        Msg::SearchReply { lobbies, .. } => lobbies.iter_mut().for_each(|l| scrub_lobby(l, with)),
        Msg::State { lobby } => scrub_lobby(lobby, with),
        Msg::MemberAttrs { attrs, .. } => scrub_attrs(attrs, with),
        Msg::Invite { connect, lobby } => {
            if let Some(clean) = inet::redact::scrub("connect", connect, with) {
                *connect = clean;
            }
            if let Some(l) = lobby {
                scrub_lobby(l, with);
            }
        }
        _ => {}
    }
}

/// Bounds what a public player sends that reaches the game: presence, lobbies, invites.
/// `false`: drop the message.
fn cap_public(msg: &mut Msg) -> bool {
    match msg {
        Msg::Presence { kv } => cap_presence(kv),
        Msg::SearchReply { lobbies, .. } => {
            lobbies.retain_mut(cap_lobby);
            lobbies.truncate(PUBLIC_MEMBERS);
        }
        Msg::State { lobby } => return cap_lobby(lobby),
        Msg::MemberAttrs { attrs, .. } => cap_attrs(attrs),
        Msg::Invite { connect, lobby } => {
            if connect.len() > PUBLIC_CONNECT_BYTES {
                return false;
            }
            if lobby.as_mut().is_some_and(|l| !cap_lobby(l)) {
                *lobby = None;
            }
        }
        _ => {}
    }
    true
}

impl Inner {
    /// Logs traffic on a channel that was dropped, once per peer and socket.
    fn note_drop(&self, peer: PeerId, socket: &str, why: &str) {
        let mut d = self.drop_logged.lock().unwrap();
        if d.len() >= 4096 || !d.insert((peer, format!("{socket} {why}"))) {
            return;
        }
        drop(d);
        (self.log)(&format!("lan {socket} with {peer}: {why} (logged once per player and socket)"));
    }

    fn tunnel(&self, conn: &Conn) -> Option<inet::Tunnel> {
        if !conn.addr.ip().is_loopback() {
            return None;
        }
        self.tunnels.lock().unwrap().get(&conn.addr).map(|e| e.tunnel.clone())
    }

    /// The address games are told a peer on this connection is at (see `Lan::peer_ip`).
    fn shown_ip(&self, conn: &Conn) -> IpAddr {
        if self.tunnel(conn).is_some() {
            return inet::redact::stand_in(&conn.peer.0).into();
        }
        conn.addr.ip()
    }

    /// Retires a moved-off connection the peer never finished (see MOVE_WAIT).
    fn move_timeout(&self, peer: PeerId, old: Arc<Conn>) {
        let Some(this) = self.this.get().cloned() else { return };
        let _ = thread::Builder::new().name("peerparty-move".into()).spawn(move || {
            thread::sleep(MOVE_WAIT);
            let Some(inner) = this.upgrade() else { return };
            let mut s = inner.state.lock().unwrap();
            let Some(p) = s.peers.get_mut(&peer) else { return };
            let Some(prior) = p.prior.iter_mut().find(|x| Arc::ptr_eq(&x.conn, &old)) else { return };
            if prior.done {
                return;
            }
            prior.done = true;
            prior.conn.retire();
            (inner.log)(&format!("lan peer {peer} didn't finish its old connection in {}s: retired it", MOVE_WAIT.as_secs()));
            s.release(inner.me, peer);
        });
    }

    fn is_public(&self, conn: &Conn) -> bool {
        self.tunnel(conn).is_some_and(|t| t.public)
    }

    fn trust(&self, conn: &Conn) -> Trust {
        match self.tunnel(conn) {
            Some(t) if t.public => Trust::Public,
            Some(_) => Trust::Room,
            None => Trust::Lan,
        }
    }

    /// How a connection reaches us, for the log (no internet address).
    fn how(&self, conn: &Conn) -> String {
        match self.trust(conn) {
            Trust::Lan => inet::redact::lan(conn.addr),
            t => describe(t).into(),
        }
    }

    /// Too much held back by a move (MOVE_HOLD_MAX): give up on order and deliver.
    fn check_hold(&self, s: &mut State, peer: PeerId) {
        let Some(p) = s.peers.get_mut(&peer) else { return };
        if p.held_count() <= MOVE_HOLD_MAX {
            return;
        }
        (self.log)(&format!("lan peer {peer}: too much waiting on its old connection; delivering out of order"));
        for x in p.prior.iter_mut() {
            x.done = true;
        }
        s.release(self.me, peer);
    }
}

/// A handle to the LAN engine. Cheap to clone.
#[derive(Clone)]
pub struct Lan {
    inner: Arc<Inner>,
    mesh: Arc<Mesh>,
    port: u16,
    net: Arc<NetSide>,
}

/// The internet side: a room and / or public sessions. The player can switch it off and
/// on while the game runs (the overlay's Internet page): off stops both and drops the
/// players they brought, and LAN players stay connected; on starts them again from the
/// same settings.
struct NetSide {
    /// What each starts from (keys filled in), kept for switching back on.
    room_cfg: Option<inet::Config>,
    public_cfg: Option<inet::Config>,
    room: Mutex<Option<Arc<inet::Inet>>>,
    public: Mutex<Option<Arc<inet::Inet>>>,
    on: std::sync::atomic::AtomicBool,
    /// One switch at a time.
    switching: Mutex<()>,
}

impl NetSide {
    fn room(&self) -> Option<Arc<inet::Inet>> {
        self.room.lock().unwrap().clone()
    }

    fn public(&self) -> Option<Arc<inet::Inet>> {
        self.public.lock().unwrap().clone()
    }
}

/// `PEERPARTY_INTERNET`: internet play starts switched on unless it's 0/off/false/no
/// (the launcher's "Internet play" switch). A room code or public sessions still has to
/// be set for there to be anything to switch.
fn internet_env() -> bool {
    std::env::var("PEERPARTY_INTERNET")
        .map_or(true, |v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"))
}

/// The switch flipped before the engine started (0: not flipped, 1: on, 2: off).
static INTERNET_CHOICE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Whether `Lan::global` starts with internet play on: the switch if it was flipped
/// before the engine started (`choose_internet`), otherwise `PEERPARTY_INTERNET`.
pub fn internet_wanted() -> bool {
    match INTERNET_CHOICE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => internet_env(),
    }
}

/// Flips the internet switch for an engine `Lan::global` hasn't started yet (a running
/// one takes `set_internet`).
pub fn choose_internet(on: bool) {
    INTERNET_CHOICE.store(if on { 1 } else { 2 }, std::sync::atomic::Ordering::Relaxed);
}

/// Intervals a test shortens. Unset fields keep the production values (5s handshake,
/// 15s between LAN upgrades, 10s before a quiet session is replaced).
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct TestIntervals {
    pub handshake: Option<Duration>,
    pub lan_upgrade_every: Option<Duration>,
    pub replace_quiet: Option<Duration>,
}

impl TestIntervals {
    pub const NONE: Self = Self { handshake: None, lan_upgrade_every: None, replace_quiet: None };
}

impl Default for TestIntervals {
    fn default() -> Self { Self::NONE }
}

thread_local! {
    static TEST_INTERVALS: Cell<TestIntervals> = const { Cell::new(TestIntervals::NONE) };
}

/// Engines `Lan::start` creates on this thread use `intervals` until the guard drops.
/// Other threads, and this thread after the drop, keep the production intervals.
#[doc(hidden)]
pub struct TestTiming {
    prev: TestIntervals,
}

impl TestTiming {
    pub fn install(intervals: TestIntervals) -> Self {
        Self { prev: TEST_INTERVALS.with(|c| c.replace(intervals)) }
    }
}

impl Drop for TestTiming {
    fn drop(&mut self) {
        let prev = self.prev;
        TEST_INTERVALS.with(|c| c.set(prev));
    }
}

fn test_intervals() -> TestIntervals {
    TEST_INTERVALS.with(|c| c.get())
}

fn apply_test_intervals(cfg: &mut inet::Config, intervals: TestIntervals) {
    if let Some(q) = intervals.replace_quiet {
        cfg.replace_quiet = q;
    }
}

pub struct Options {
    /// Peers only meet peers with the same game key ("eos:<product id>").
    pub game: String,
    pub identity: Identity,
    pub discovery_port: u16,
    pub extra_targets: Vec<SocketAddr>,
    pub log: fn(&str),
    /// Internet play (a room code): off with `None`.
    pub internet: Option<inet::Config>,
    /// Public sessions (`inet::Config::public`): off with `None`.
    pub public: Option<inet::Config>,
    /// LAN beacons and the same-PC board (on, normally).
    pub lan_discovery: bool,
}

/// The engine as the internet layer sees it.
struct InetHost {
    inner: Arc<Inner>,
    mesh: Arc<Mesh>,
    /// The public sessions instance (else the room one).
    public: bool,
}

impl inet::Host for InetHost {
    fn me(&self) -> PeerId {
        self.inner.me
    }
    fn name(&self) -> String {
        self.inner.name.clone()
    }
    fn game(&self) -> String {
        self.inner.game.clone()
    }
    fn knows(&self, id: PeerId) -> bool {
        match Sink::conn(&*self.inner, id) {
            None => false,
            // A room peer we only know through public sessions still gets its room
            // tunnel (which then takes over: see `connected`).
            Some(c) => self.public || !self.inner.is_public(&c),
        }
    }

    fn mesh_port(&self) -> u16 {
        self.mesh.port
    }
    fn dial_local(&self, port: u16, id: PeerId) {
        self.mesh.dial_local(port, id);
    }
    fn tunnel_up(&self, mesh: SocketAddr, t: inet::Tunnel) {
        let mut m = self.inner.tunnels.lock().unwrap();
        if m.len() >= 1024 {
            // Down long enough that whatever ran on them is long cut (RETIRE_GRACE).
            m.retain(|_, e| e.down.is_none_or(|d| d.elapsed() < Duration::from_secs(60)));
        }
        if m.len() < 1024 {
            m.insert(mesh, TunnelEntry { tunnel: t, down: None });
        }
    }
    fn tunnel_down(&self, mesh: SocketAddr) {
        // Kept until the mesh connection on it is gone (`Sink::disconnected`).
        if let Some(e) = self.inner.tunnels.lock().unwrap().get_mut(&mesh) {
            e.down.get_or_insert_with(Instant::now);
        }
    }
    fn log(&self, line: &str) {
        (self.inner.log)(line);
    }
}

static GLOBAL: OnceLock<Lan> = OnceLock::new();

impl Lan {
    /// The process-wide engine, started on first use. Front ends in one process share
    /// it; the first one's game key wins.
    pub fn global(game: &str, log: fn(&str)) -> io::Result<Lan> {
        if let Some(lan) = GLOBAL.get() {
            return Ok(lan.clone());
        }
        let extra = std::env::var("PEERPARTY_PEERS")
            .unwrap_or_default()
            .split([',', ';', ' '])
            .filter_map(|s| {
                let s = s.trim();
                s.parse::<SocketAddr>()
                    .ok()
                    .or_else(|| format!("{s}:{}", mesh::DISCOVERY_PORT).parse().ok())
            })
            .collect();
        let lan = Lan::start_with(Options {
            game: game.into(),
            identity: Identity::claim(&Identity::default_dir()),
            discovery_port: mesh::DISCOVERY_PORT,
            extra_targets: extra,
            log,
            internet: match inet::Config::from_env() {
                Ok(c) => c,
                Err(e) => {
                    log(&format!("inet off: {e}"));
                    None
                }
            },
            public: match inet::Config::public_from_env() {
                Ok(c) => c,
                Err(e) => {
                    log(&format!("public sessions off: {e}"));
                    None
                }
            },
            lan_discovery: std::env::var("PEERPARTY_LAN_DISCOVERY").map_or(true, |v| v.trim() != "0"),
        }, internet_wanted())?;
        Ok(GLOBAL.get_or_init(|| lan).clone())
    }

    pub fn start(opts: Options) -> io::Result<Lan> {
        Lan::start_with(opts, true)
    }

    /// `start`, with internet play (the room and public sessions in `opts`) switched on
    /// or not to begin with; `set_internet` switches it later.
    pub fn start_with(opts: Options, internet_on: bool) -> io::Result<Lan> {
        let inner = Arc::new(Inner {
            me: opts.identity.id,
            name: opts.identity.name.clone(),
            slot: opts.identity.slot,
            game: opts.game.clone(),
            state: Mutex::new(State::default()),
            log: opts.log,
            tunnels: Mutex::new(HashMap::new()),
            public_voice: std::sync::atomic::AtomicBool::new(public_voice_env()),
            drop_logged: Mutex::new(HashSet::new()),
            this: OnceLock::new(),
        });
        let _ = inner.this.set(Arc::downgrade(&inner));
        let key = opts.identity.key.clone();
        let intervals = test_intervals();
        let mesh = mesh::start(
            mesh::Config {
                game: opts.game,
                me: opts.identity.id,
                key: key.clone(),
                name: opts.identity.name.clone(),
                discovery_port: opts.discovery_port,
                extra_targets: opts.extra_targets,
                lan_discovery: std::sync::atomic::AtomicBool::new(opts.lan_discovery),
                handshake_timeout: intervals.handshake.unwrap_or(mesh::HANDSHAKE_TIMEOUT),
                lan_upgrade_every: intervals.lan_upgrade_every.unwrap_or(mesh::LAN_UPGRADE_EVERY),
            },
            inner.clone(),
        )?;
        let port = mesh.port;
        let mesh = Arc::new(mesh);
        if !opts.lan_discovery {
            (inner.log)("lan discovery off: only internet peers");
        }
        // Room and public handshakes are signed with this player's one key, the key
        // the id is the hash of: whatever key a config came with, the id wouldn't be its.
        let room_cfg = opts.internet.map(|mut cfg| {
            cfg.room_key = Some(key.clone());
            apply_test_intervals(&mut cfg, intervals);
            cfg
        });
        let public_cfg = opts.public.map(|mut cfg| {
            cfg.public.get_or_insert_with(inet::public::PublicCfg::standard).key = Some(key.clone());
            apply_test_intervals(&mut cfg, intervals);
            cfg
        });
        let net = Arc::new(NetSide {
            room_cfg,
            public_cfg,
            room: Mutex::new(None),
            public: Mutex::new(None),
            on: std::sync::atomic::AtomicBool::new(false),
            switching: Mutex::new(()),
        });
        // The identity's slot lock must live as long as the engine.
        std::mem::forget(opts.identity);
        (inner.log)(&format!(
            "lan started id={} name={} game={} tcp={port}",
            inner.me, inner.name, inner.game
        ));
        let lan = Lan { inner, mesh, port, net };
        if internet_on {
            lan.internet_up(false);
        } else if lan.internet_configured() {
            (lan.inner.log)("internet play is switched off: local network only until it's switched on");
        }
        Ok(lan)
    }

    /// Starts the room and public sessions (whichever are set up). `retry`: their port
    /// may still be held by the instance just stopped, so a fixed port is retried for a
    /// few seconds before falling back to any port.
    fn internet_up(&self, retry: bool) {
        let net = &self.net;
        net.on.store(true, std::sync::atomic::Ordering::Relaxed);
        let start = |cfg: &inet::Config, public: bool| -> Option<Arc<inet::Inet>> {
            let what = if public { "public sessions" } else { "inet" };
            let mut cfg = cfg.clone();
            let mut tries = if retry && cfg.port != 0 { 20 } else { 0 };
            loop {
                let host = Arc::new(InetHost { inner: self.inner.clone(), mesh: self.mesh.clone(), public });
                match inet::Inet::start(cfg.clone(), host) {
                    Ok(i) => return Some(Arc::new(i)),
                    Err(e) if tries > 0 && e.kind() == io::ErrorKind::AddrInUse => {
                        tries -= 1;
                        thread::sleep(Duration::from_millis(150));
                        if tries == 0 {
                            (self.inner.log)(&format!("{what}: udp {} still in use; taking any port", cfg.port));
                            cfg.port = 0;
                        }
                    }
                    Err(e) => {
                        (self.inner.log)(&format!("{what} couldn't start: {e}"));
                        return None;
                    }
                }
            }
        };
        if let Some(cfg) = &net.room_cfg {
            let i = start(cfg, false);
            *net.room.lock().unwrap() = i;
        }
        if let Some(cfg) = &net.public_cfg {
            let i = start(cfg, true);
            *net.public.lock().unwrap() = i;
        }
    }

    /// Whether a room code or public sessions is set up (so `set_internet` has anything
    /// to switch).
    pub fn internet_configured(&self) -> bool {
        self.net.room_cfg.is_some() || self.net.public_cfg.is_some()
    }

    /// Whether internet play is switched on (see `set_internet`).
    pub fn internet_on(&self) -> bool {
        self.net.on.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Switches internet play (the room and public sessions) on or off while running.
    /// Off: both stop, their port mappings go, and the players they brought leave; LAN
    /// players stay connected (a player reachable both ways moves to the LAN). On: both
    /// start again from the same settings and find their players again. Returns whether
    /// anything changed.
    pub fn set_internet(&self, on: bool) -> bool {
        let _one = self.net.switching.lock().unwrap();
        if on == self.internet_on() {
            return false;
        }
        if on {
            self.internet_up(true);
            (self.inner.log)(&format!(
                "internet play switched on{}",
                if self.internet_configured() { "" } else { " (no room code or public sessions set up)" }
            ));
            return true;
        }
        self.net.on.store(false, std::sync::atomic::Ordering::Relaxed);
        let room = self.net.room.lock().unwrap().take();
        let public = self.net.public.lock().unwrap().take();
        for i in [room, public].into_iter().flatten() {
            i.stop();
        }
        // Whatever still runs over a tunnel goes now (the bridges shut as the sessions
        // ended; this doesn't wait for the mesh to notice). LAN connections stay.
        let conns: Vec<Arc<Conn>> = {
            let s = self.inner.state.lock().unwrap();
            s.peers.values().map(|p| p.conn.clone()).collect()
        };
        let mut dropped = 0;
        for c in conns {
            if self.inner.tunnel(&c).is_some() {
                c.close();
                dropped += 1;
            }
        }
        (self.inner.log)(&format!("internet play switched off: local network only ({dropped} internet player(s) disconnected)"));
        true
    }

    pub fn me(&self) -> PeerId {
        self.inner.me
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Which of this PC's identity slots we took: 0 for the first copy running here.
    pub fn slot(&self) -> u32 {
        self.inner.slot
    }

    pub fn tcp_port(&self) -> u16 {
        self.port
    }

    pub fn peers(&self) -> Vec<(PeerId, String)> {
        let s = self.inner.state.lock().unwrap();
        s.peers
            .iter()
            .map(|(id, p)| (*id, p.name.clone()))
            .collect()
    }

    /// Every peer, the one that speaks for an address first: the most trusted (the LAN,
    /// then a room, then public sessions), and of those the one that has been here
    /// longest. Two peers can share a stand-in address (`inet::redact::stand_in`); a
    /// newcomer, or a stranger, then doesn't take it from the peer that had it.
    pub fn peers_by_standing(&self) -> Vec<PeerId> {
        let s = self.inner.state.lock().unwrap();
        let mut v: Vec<(std::cmp::Reverse<u8>, Instant, PeerId)> =
            s.peers.iter().map(|(id, p)| (std::cmp::Reverse(rank(self.inner.trust(&p.conn))), p.joined, *id)).collect();
        v.sort();
        v.into_iter().map(|(_, _, id)| id).collect()
    }

    pub fn peer_name(&self, id: PeerId) -> Option<String> {
        if id == self.inner.me {
            return Some(self.inner.name.clone());
        }
        let s = self.inner.state.lock().unwrap();
        s.peers.get(&id).map(|p| p.name.clone())
    }

    /// The address games are told a peer is at: its LAN IP, or for a peer that came
    /// through an internet tunnel, a stable stand-in from a reserved range (never its
    /// internet address, which games couldn't reach directly anyway and might show).
    /// Fixed for as long as the peer stays: a peer that moves to the LAN keeps its
    /// stand-in here (games key players by it); `peer_direct_ip` has the LAN address.
    pub fn peer_ip(&self, id: PeerId) -> Option<IpAddr> {
        self.inner.state.lock().unwrap().peers.get(&id).map(|p| p.shown)
    }

    /// The peer's LAN address when its live connection is on the LAN (what a game can
    /// open its own socket to); `None` through an internet tunnel or when not connected.
    pub fn peer_direct_ip(&self, id: PeerId) -> Option<IpAddr> {
        let conn = Sink::conn(&*self.inner, id)?;
        (self.inner.tunnel(&conn).is_none()).then(|| conn.addr.ip())
    }

    /// The address to give a game for reaching this peer's servers: its LAN address when
    /// it's on the LAN, else its stand-in (the typed-IP forwarder carries traffic sent
    /// there through the tunnel). What EOS host addresses, Steam server lists, Unity
    /// Relay join codes and Photon all hand out.
    pub fn game_address(&self, id: PeerId) -> Option<IpAddr> {
        self.peer_direct_ip(id).or_else(|| self.peer_ip(id))
    }

    /// Whether a game-side address is this peer's: its `peer_ip` or `peer_direct_ip`.
    pub fn peer_has_ip(&self, id: PeerId, ip: IpAddr) -> bool {
        self.peer_ip(id) == Some(ip) || self.peer_direct_ip(id) == Some(ip)
    }

    /// Turns voice and party chat with public players on or off (it starts as
    /// `PEERPARTY_PUBLIC_VOICE` says: on unless that's 0). Room and LAN players always
    /// have voice.
    pub fn set_public_voice(&self, on: bool) {
        self.inner.public_voice.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether voice and party chat with public players is on.
    pub fn public_voice(&self) -> bool {
        self.inner.public_voice.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Turns LAN discovery (beacons and the same-PC board) on or off while running.
    pub fn set_lan_discovery(&self, on: bool) {
        self.mesh.set_lan_discovery(on);
        (self.inner.log)(&format!("lan discovery {}", if on { "on" } else { "off" }));
    }

    /// How many times we tried the LAN path to a peer we reach through a tunnel.
    #[doc(hidden)]
    pub fn lan_upgrade_tries(&self) -> u64 {
        self.mesh.upgrades.tries.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Players on our network whose beacons we hear but whose LAN port never answers:
    /// three or more dials in a row timed out, the last in the past minute, and we have
    /// no connection to them any other way. Most often their firewall drops PeerParty's
    /// connections (ours can't stop our own dials out).
    pub fn unreachable(&self) -> Vec<PeerId> {
        self.mesh
            .upgrades
            .timed_out(3, Duration::from_secs(60))
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| !Sink::knows(&*self.inner, *p))
            .collect()
    }

    /// Cuts the live connection to a peer, as a network failure would (tests).
    #[doc(hidden)]
    pub fn cut_connection(&self, id: PeerId) {
        if let Some(c) = Sink::conn(&*self.inner, id) {
            c.close();
        }
    }

    /// Whether a peer's connection runs through an internet tunnel.
    pub fn peer_is_internet(&self, id: PeerId) -> bool {
        Sink::conn(&*self.inner, id).is_some_and(|c| self.inner.tunnel(&c).is_some())
    }

    /// How a peer reaches us (`None`: not connected).
    pub fn peer_trust(&self, id: PeerId) -> Option<Trust> {
        Sink::conn(&*self.inner, id).map(|c| self.inner.trust(&c))
    }

    /// A public player's key fingerprint.
    pub fn peer_key(&self, id: PeerId) -> Option<String> {
        Sink::conn(&*self.inner, id).and_then(|c| self.inner.tunnel(&c)).and_then(|t| t.key)
    }

    /// The internet layer's state, when it's on.
    pub fn internet_status(&self) -> Option<inet::Status> {
        self.net.room().map(|i| i.status())
    }

    /// Public sessions' state, when they're on.
    pub fn public_status(&self) -> Option<inet::Status> {
        self.net.public().map(|i| i.status())
    }

    /// Blocks a public player: on the block list (by key, so it stays blocked), and
    /// disconnected. Returns its fingerprint.
    pub fn block_peer(&self, id: PeerId) -> Result<String, String> {
        let Some(p) = self.net.public() else { return Err("public sessions are off".into()) };
        let Some(conn) = Sink::conn(&*self.inner, id) else { return Err(format!("{id} isn't connected")) };
        let Some(t) = self.inner.tunnel(&conn).filter(|t| t.public) else {
            return Err(format!("{id} isn't a public player"));
        };
        let fp = t.key.clone().ok_or("no key")?;
        p.block(&fp, &t.name).map_err(|e| e.to_string())?;
        conn.close();
        (self.inner.log)(&format!("lan blocked public player {id} ({fp})"));
        Ok(fp)
    }

    /// Stops discovery and drops every connection. Peers see us leave.
    /// Deletes the routers' port mappings: for a game that's exiting.
    pub fn release_port_maps(&self) {
        // try_lock: never blocks a game that's exiting (a switch in progress keeps its own).
        for m in [&self.net.room, &self.net.public] {
            if let Ok(g) = m.try_lock()
                && let Some(i) = g.as_ref()
            {
                i.release_port_map();
            }
        }
    }

    pub fn shutdown(&self) {
        self.net.on.store(false, std::sync::atomic::Ordering::Relaxed);
        for i in [self.net.room.lock().unwrap().take(), self.net.public.lock().unwrap().take()].into_iter().flatten() {
            i.stop();
        }
        self.mesh.stop();
        let peers: Vec<Arc<Conn>> = {
            let s = self.inner.state.lock().unwrap();
            s.peers.values().map(|p| p.conn.clone()).collect()
        };
        for c in peers {
            c.close();
        }
    }

    /// Times out searches and joins. Call it every frame, before `poll`.
    pub fn tick(&self) {
        let now = Instant::now();
        let mut s = self.inner.state.lock().unwrap();
        let expired: Vec<u64> = s
            .searches
            .iter()
            .filter(|(_, q)| q.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            s.finish_search(id);
        }
        s.answer_waiting_joins(self.inner.me, now);
        // Public players' answers to joins by id that no LAN or room player beat.
        let due: Vec<(String, PeerId, JoinResult)> =
            s.claims.iter().filter(|(_, c)| c.2 <= now).map(|(id, c)| (id.clone(), c.0, c.1)).collect();
        for (id, from, result) in due {
            s.claims.remove(&id);
            let asking = s.joins.contains_key(&id)
                && s.lobbies.get(&id).is_some_and(|h| h.role == Role::Joining && h.lobby.owner == PeerId::default());
            if asking && s.peers.contains_key(&from) {
                s.join_answered(id, from, result);
            }
        }
        let late: Vec<String> = s
            .joins
            .iter()
            .filter(|(_, d)| **d <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in late {
            s.joins.remove(&id);
            // The owner may have let us in and its answer be lost or late: it hears
            // that we aren't coming, or it keeps a member who never arrived.
            if let Some(held) = s.lobbies.remove(&id) {
                s.tell_left(&held.lobby);
            }
            s.events.push_back(Event::JoinDone {
                lobby: id,
                result: JoinResult::TimedOut,
            });
        }
    }

    pub fn poll(&self) -> Vec<Event> {
        let mut s = self.inner.state.lock().unwrap();
        s.events.drain(..).collect()
    }

    // ---- lobbies ----

    /// Creates a lobby we own. An empty id gets a fresh one.
    pub fn create_lobby(&self, mut lobby: Lobby) -> Result<Lobby, LobbyError> {
        let me = self.inner.me;
        let mut s = self.inner.state.lock().unwrap();
        if lobby.id.is_empty() {
            lobby.id = lobby::new_lobby_id();
        }
        if s.lobbies.contains_key(&lobby.id) {
            return Err(LobbyError::AlreadyIn);
        }
        lobby.owner = me;
        let mine = lobby
            .member(me)
            .map(|m| m.attrs.clone())
            .unwrap_or_default();
        lobby.members = vec![Member {
            id: me,
            attrs: mine,
        }];
        s.lobbies.insert(
            lobby.id.clone(),
            Held {
                lobby: lobby.clone(),
                role: Role::Hosting,
            },
        );
        Ok(lobby)
    }

    /// Our current copy of a lobby we're in.
    pub fn lobby(&self, id: &str) -> Option<Lobby> {
        let s = self.inner.state.lock().unwrap();
        s.lobbies
            .get(id)
            .filter(|h| h.role != Role::Joining)
            .map(|h| h.lobby.clone())
    }

    pub fn is_owner(&self, id: &str) -> bool {
        let s = self.inner.state.lock().unwrap();
        s.lobbies.get(id).is_some_and(|h| h.role == Role::Hosting)
    }

    /// Starts a search; `Event::SearchDone` carries the results. Lobbies we host count.
    pub fn search(&self, query: Query) -> u64 {
        let mut s = self.inner.state.lock().unwrap();
        s.next_search += 1;
        let id = s.next_search;
        let max = query.max_results.max(1) as usize;
        let mut results: Vec<Lobby> = s
            .lobbies
            .values()
            .filter(|h| h.role == Role::Hosting && lobby::matches(&h.lobby, &query))
            .map(|h| h.lobby.public_view())
            .take(max)
            .collect();
        results.truncate(max);
        // Everyone is asked, also for one player's lobbies: a lobby is answered for by
        // its owner, and the lobby a player is in is as often someone else's as theirs
        // ("join my friend's game" found nothing when the friend had joined a host).
        let awaiting: HashSet<PeerId> = s.peers.keys().copied().collect();
        let msg = Msg::Search {
            search: id,
            query: query.clone(),
        };
        for p in &awaiting {
            s.peers[p].conn.send(&msg);
        }
        s.searches.insert(
            id,
            Search {
                query,
                awaiting,
                results,
                deadline: Instant::now() + SEARCH_TIMEOUT,
            },
        );
        if s.searches[&id].awaiting.is_empty() {
            s.finish_search(id);
        }
        id
    }

    /// Joins a lobby a search returned. `Event::JoinDone` says how it went.
    pub fn join(&self, target: &Lobby) -> Result<(), LobbyError> {
        let mut s = self.inner.state.lock().unwrap();
        if s.lobbies.contains_key(&target.id) {
            return Err(LobbyError::AlreadyIn);
        }
        let Some(owner) = s.peers.get(&target.owner) else {
            return Err(LobbyError::OwnerUnknown);
        };
        owner.conn.send(&Msg::Join {
            lobby: target.id.clone(),
        });
        s.lobbies.insert(
            target.id.clone(),
            Held {
                lobby: target.clone(),
                role: Role::Joining,
            },
        );
        s.joins
            .insert(target.id.clone(), Instant::now() + JOIN_TIMEOUT);
        Ok(())
    }

    /// Joins by id alone: every peer is asked and the owner answers.
    pub fn join_by_id(&self, namespace: &str, id: &str) -> Result<(), LobbyError> {
        let mut s = self.inner.state.lock().unwrap();
        if s.lobbies.contains_key(id) {
            return Err(LobbyError::AlreadyIn);
        }
        let msg = Msg::Join { lobby: id.into() };
        for p in s.peers.values() {
            p.conn.send(&msg);
        }
        s.claims.remove(id);
        let mut placeholder = Lobby::new(namespace);
        placeholder.id = id.into();
        s.lobbies.insert(
            id.into(),
            Held {
                lobby: placeholder,
                role: Role::Joining,
            },
        );
        s.joins.insert(id.into(), Instant::now() + JOIN_TIMEOUT);
        Ok(())
    }

    /// Leaves a lobby. The owner leaving hands it on, or closes it.
    pub fn leave(&self, id: &str) -> Result<(), LobbyError> {
        let me = self.inner.me;
        let mut s = self.inner.state.lock().unwrap();
        let Some(held) = s.lobbies.remove(id) else {
            return Err(LobbyError::NotFound);
        };
        s.joins.remove(id);
        match held.role {
            Role::Hosting => s.close_hosted(me, held.lobby, true),
            _ => s.tell_left(&held.lobby),
        }
        Ok(())
    }

    /// Ends a lobby we own for everyone. For a lobby we only joined it's a leave.
    pub fn destroy(&self, id: &str) -> Result<(), LobbyError> {
        let me = self.inner.me;
        let mut s = self.inner.state.lock().unwrap();
        let Some(held) = s.lobbies.remove(id) else {
            return Err(LobbyError::NotFound);
        };
        match held.role {
            Role::Hosting => s.close_hosted(me, held.lobby, false),
            _ => s.send_to(held.lobby.owner, &Msg::Leave { lobby: id.into() }),
        }
        Ok(())
    }

    /// Applies an edit. The owner may change the lobby's settings and attributes (taken
    /// from `edited`); anyone may change their own member attributes.
    pub fn update(
        &self,
        id: &str,
        edited: Option<&Lobby>,
        member_set: &[Attr],
        member_remove: &[String],
    ) -> Result<(), LobbyError> {
        let me = self.inner.me;
        let mut s = self.inner.state.lock().unwrap();
        let Some(held) = s.lobbies.get_mut(id) else {
            return Err(LobbyError::NotFound);
        };
        let owner = held.role == Role::Hosting;
        if owner {
            if let Some(e) = edited {
                let l = &mut held.lobby;
                l.bucket = e.bucket.clone();
                l.max_members = e.max_members;
                l.permission = e.permission;
                l.allow_invites = e.allow_invites;
                l.attrs = e.attrs.clone();
            }
        }
        let Some(mine) = held.lobby.member_mut(me) else {
            return Err(LobbyError::NotMember);
        };
        for k in member_remove {
            lobby::remove_attr(&mut mine.attrs, k);
        }
        for a in member_set {
            lobby::set_attr(&mut mine.attrs, a.clone());
        }
        let attrs = mine.attrs.clone();
        let lobby = held.lobby.clone();
        if owner {
            s.broadcast(&lobby);
        } else {
            s.send_to(
                lobby.owner,
                &Msg::MemberAttrs {
                    lobby: id.into(),
                    attrs,
                },
            );
        }
        Ok(())
    }

    pub fn kick(&self, id: &str, target: PeerId) -> Result<(), LobbyError> {
        let mut s = self.inner.state.lock().unwrap();
        let held = s.lobbies.get_mut(id).ok_or(LobbyError::NotFound)?;
        if held.role != Role::Hosting {
            return Err(LobbyError::NotOwner);
        }
        if held.lobby.member(target).is_none() || target == self.inner.me {
            return Err(LobbyError::NotMember);
        }
        held.lobby.members.retain(|m| m.id != target);
        let lobby = held.lobby.clone();
        // Our invite doesn't outlast being shown the door.
        if let Some(p) = s.peers.get_mut(&target) {
            p.invited = None;
        }
        s.send_to(target, &Msg::Kicked { lobby: id.into() });
        s.broadcast(&lobby);
        s.events.push_back(Event::MemberStatus {
            lobby: id.into(),
            member: target,
            status: MemberStatus::Kicked,
        });
        Ok(())
    }

    pub fn promote(&self, id: &str, target: PeerId) -> Result<(), LobbyError> {
        let mut s = self.inner.state.lock().unwrap();
        let held = s.lobbies.get_mut(id).ok_or(LobbyError::NotFound)?;
        if held.role != Role::Hosting {
            return Err(LobbyError::NotOwner);
        }
        if held.lobby.member(target).is_none() {
            return Err(LobbyError::NotMember);
        }
        if target != self.inner.me {
            held.lobby.owner = target;
            held.role = Role::Joined;
        }
        let lobby = held.lobby.clone();
        s.broadcast(&lobby);
        s.events.push_back(Event::MemberStatus {
            lobby: id.into(),
            member: target,
            status: MemberStatus::Promoted,
        });
        Ok(())
    }

    /// Lobbies in `namespace` we're in (hosting or joined), most useful for "invite
    /// everyone to where I am".
    pub fn my_lobbies(&self, namespace: &str) -> Vec<Lobby> {
        let s = self.inner.state.lock().unwrap();
        s.lobbies
            .values()
            .filter(|h| h.role != Role::Joining && h.lobby.namespace == namespace)
            .map(|h| h.lobby.clone())
            .collect()
    }

    // ---- presence and invites ----

    /// Replaces our presence (rich presence) and tells every peer.
    pub fn set_presence(&self, kv: Vec<(String, String)>) {
        let mut s = self.inner.state.lock().unwrap();
        if s.my_presence == kv {
            return;
        }
        s.my_presence = kv.clone();
        let msg = Msg::Presence { kv };
        for p in s.peers.values() {
            p.conn.send(&msg);
        }
    }

    pub fn my_presence(&self) -> Vec<(String, String)> {
        self.inner.state.lock().unwrap().my_presence.clone()
    }

    /// A peer's last presence.
    pub fn presence(&self, peer: PeerId) -> Vec<(String, String)> {
        let s = self.inner.state.lock().unwrap();
        s.presence.get(&peer).cloned().unwrap_or_default()
    }

    /// Asks `to` (or every peer) to join us. Returns how many peers it went to.
    pub fn invite(&self, to: Option<PeerId>, connect: &str, lobby: Option<&Lobby>) -> usize {
        let mut s = self.inner.state.lock().unwrap();
        let msg = Msg::Invite {
            connect: connect.into(),
            lobby: lobby.map(|l| l.public_view()),
        };
        let mut n = 0;
        for (id, p) in &s.peers {
            // "Invite everyone" means everyone we know, not strangers.
            if to.is_none() && self.inner.is_public(&p.conn) {
                continue;
            }
            if to.is_none_or(|t| t == *id) {
                p.conn.send(&msg);
                n += 1;
            }
        }
        // An invite to one player is the player's word that this one may come in.
        if let Some(p) = to.and_then(|t| s.peers.get_mut(&t)) {
            p.invited = Some(Instant::now());
        }
        n
    }

    /// Whether `peer` may be let into what isn't open to everyone (a voice room, say):
    /// always for a LAN or room player; for a public player only once we invited them
    /// or while they are in a lobby with us.
    pub fn knows(&self, peer: PeerId) -> bool {
        self.inner.state.lock().unwrap().known(peer)
    }

    // ---- channels ----

    #[allow(clippy::too_many_arguments)]
    pub fn send(
        &self,
        peer: PeerId,
        socket: &str,
        channel: u8,
        data: &[u8],
        reliable: bool,
        allow_delayed: bool,
        auto_accept: bool,
    ) -> SendResult {
        let mut s = self.inner.state.lock().unwrap();
        if voice_socket(socket)
            && !self.public_voice()
            && s.peers.get(&peer).is_some_and(|p| self.inner.is_public(&p.conn))
        {
            drop(s);
            self.inner.note_drop(peer, socket, "not sent: voice to a public player, and voice chat with public players is off");
            return SendResult::Dropped;
        }
        let key = (peer, socket.to_string());
        if !s.chans.contains_key(&key) {
            if !auto_accept {
                return SendResult::NoChannel;
            }
            s.chans.insert(
                key.clone(),
                Chan::new(ChanState::Pending),
            );
            s.send_to(
                peer,
                &Msg::ChanRequest {
                    socket: socket.into(),
                },
            );
        }
        let chan = s.chans.get_mut(&key).unwrap();
        if chan.state != ChanState::Open {
            if !allow_delayed
                || !chan.hold(channel, reliable, data, DELAYED_MAX_PACKETS, DELAYED_MAX_BYTES)
            {
                let why = if allow_delayed {
                    "not sent: the channel isn't open yet and its hold buffer is full"
                } else {
                    "not sent: the channel isn't open yet"
                };
                drop(s);
                self.inner.note_drop(peer, socket, why);
                return SendResult::Dropped;
            }
            return SendResult::Held;
        }
        let msg = Msg::ChanData {
            socket: socket.into(),
            channel,
            reliable,
            data: data.to_vec(),
        };
        if reliable {
            s.send_to(peer, &msg);
        } else {
            s.send_unreliable_to(peer, &msg);
        }
        SendResult::Sent
    }

    /// Whether unreliable data to `peer` travels over UDP yet.
    pub fn udp_ready(&self, peer: PeerId) -> bool {
        let s = self.inner.state.lock().unwrap();
        s.peers.get(&peer).is_some_and(|p| p.conn.udp_ready())
    }

    /// Whether our datagrams to `peer` carry sequence numbers (it runs this version too).
    pub fn udp_sequenced(&self, peer: PeerId) -> bool {
        let s = self.inner.state.lock().unwrap();
        s.peers.get(&peer).is_some_and(|p| p.conn.udp_sequenced())
    }

    pub fn accept(&self, peer: PeerId, socket: &str) {
        let mut s = self.inner.state.lock().unwrap();
        let key = (peer, socket.to_string());
        match s.chans.get(&key).map(|c| c.state) {
            Some(ChanState::Open) => {}
            Some(ChanState::Requested) => {
                s.send_to(
                    peer,
                    &Msg::ChanAccept {
                        socket: socket.into(),
                    },
                );
                s.open_chan(&key);
            }
            Some(ChanState::Pending) => s.send_to(
                peer,
                &Msg::ChanRequest {
                    socket: socket.into(),
                },
            ),
            None => {
                s.chans.insert(
                    key,
                    Chan::new(ChanState::Pending),
                );
                s.send_to(
                    peer,
                    &Msg::ChanRequest {
                        socket: socket.into(),
                    },
                );
            }
        }
    }

    /// Closes our channels with `peer` on `socket`, or on every socket.
    pub fn close(&self, peer: PeerId, socket: Option<&str>) {
        let mut s = self.inner.state.lock().unwrap();
        let keys: Vec<_> = s
            .chans
            .keys()
            .filter(|(p, k)| *p == peer && socket.is_none_or(|x| x == k))
            .cloned()
            .collect();
        for key in keys {
            s.close_chan(key, true);
        }
    }

    /// Closes every channel on `socket`.
    pub fn close_socket(&self, socket: &str) {
        let mut s = self.inner.state.lock().unwrap();
        let keys: Vec<_> = s
            .chans
            .keys()
            .filter(|(_, k)| k == socket)
            .cloned()
            .collect();
        for key in keys {
            s.close_chan(key, true);
        }
    }

    pub fn next_packet_size(&self, channel: Option<u8>) -> Option<usize> {
        self.next_packet_size_where(channel, |_| true)
    }

    pub fn receive(&self, channel: Option<u8>) -> Option<Packet> {
        self.receive_where(channel, |_| true)
    }

    /// The next packet on a socket `want` accepts. Front ends keep their packets apart
    /// by socket name (Steam's are "steam:...").
    pub fn next_packet_size_where(
        &self,
        channel: Option<u8>,
        want: impl Fn(&str) -> bool,
    ) -> Option<usize> {
        let s = self.inner.state.lock().unwrap();
        s.inbox
            .iter()
            .find(|p| channel.is_none_or(|c| c == p.channel) && want(&p.socket))
            .map(|p| p.data.len())
    }

    pub fn receive_where(&self, channel: Option<u8>, want: impl Fn(&str) -> bool) -> Option<Packet> {
        let mut s = self.inner.state.lock().unwrap();
        let i = s
            .inbox
            .iter()
            .position(|p| channel.is_none_or(|c| c == p.channel) && want(&p.socket))?;
        s.inbox.remove(i)
    }

    /// Drops queued packets, both received and held for sending.
    pub fn clear(&self, peer: Option<PeerId>, socket: Option<&str>) {
        let mut s = self.inner.state.lock().unwrap();
        let hit =
            |p: &PeerId, k: &str| peer.is_none_or(|x| x == *p) && socket.is_none_or(|x| x == k);
        s.inbox.retain(|p| !hit(&p.peer, &p.socket));
        for ((p, k), c) in s.chans.iter_mut() {
            if hit(p, k) {
                c.take_delayed();
            }
        }
    }

    /// (packets, bytes) waiting to be received.
    pub fn inbox_size(&self) -> (u64, u64) {
        let s = self.inner.state.lock().unwrap();
        (s.inbox.q.len() as u64, s.inbox.bytes as u64)
    }
}

impl State {
    /// Strangers' joins that waited (JOIN_WAIT): let in if something vouches for them by
    /// now, refused once their wait is up. One for a lobby we no longer host, or from a
    /// player who has gone, has nobody to answer: it goes, or it would hold one of the
    /// JOINS_WAITING places for good.
    fn answer_waiting_joins(&mut self, me: PeerId, now: Instant) {
        for (peer, lobby, until) in std::mem::take(&mut self.waiting) {
            if !self.peers.contains_key(&peer) || !self.lobbies.get(&lobby).is_some_and(|h| h.role == Role::Hosting) {
                continue;
            }
            if self.known(peer) || until <= now {
                // Kept in `waiting` so the handler answers this time, one way or the other.
                self.waiting.push((peer, lobby.clone(), until));
                self.handle(me, peer, Msg::Join { lobby }, true);
            } else {
                self.waiting.push((peer, lobby, until));
            }
        }
    }
    /// Whether a peer may be let into what isn't open to everyone: a lobby that isn't
    /// public, a voice room. Anyone on the LAN or in the room may, as before: they are
    /// who the player chose to play with. A public player (anyone at all who runs the
    /// game) only once we invited them, or while they are in a lobby with us.
    fn known(&self, peer: PeerId) -> bool {
        let Some(p) = self.peers.get(&peer) else { return false };
        !p.stranger
            || p.invited.is_some_and(|t| t.elapsed() < INVITE_STANDS)
            || self.lobbies.values().any(|h| h.role != Role::Joining && h.lobby.member(peer).is_some())
    }

    fn send_to(&self, peer: PeerId, msg: &Msg) {
        if let Some(p) = self.peers.get(&peer) {
            p.conn.send(msg);
        }
    }

    /// We are out of `lobby` (one we joined, or were still asking to join): its owner
    /// hears it, or everyone does when we asked by id and don't know yet whose it is.
    /// Anyone but the owner of a lobby we're in ignores it.
    fn tell_left(&self, lobby: &Lobby) {
        let msg = Msg::Leave { lobby: lobby.id.clone() };
        if lobby.owner != PeerId::default() {
            self.send_to(lobby.owner, &msg);
        } else {
            for p in self.peers.values() {
                p.conn.send(&msg);
            }
        }
    }

    fn send_unreliable_to(&self, peer: PeerId, msg: &Msg) {
        if let Some(p) = self.peers.get(&peer) {
            p.conn.send_unreliable(msg);
        }
    }

    /// The owner's snapshot to every other member.
    fn broadcast(&self, lobby: &Lobby) {
        let msg = Msg::State {
            lobby: lobby.clone(),
        };
        for m in &lobby.members {
            self.send_to(m.id, &msg);
        }
    }

    fn finish_search(&mut self, id: u64) {
        if let Some(search) = self.searches.remove(&id) {
            self.events.push_back(Event::SearchDone {
                search: id,
                results: search.results,
            });
        }
    }

    /// We owned `lobby` and are leaving it: hand it to an heir, or close it.
    fn close_hosted(&mut self, me: PeerId, mut lobby: Lobby, migrate: bool) {
        let heir = if migrate && lobby.host_migration {
            lobby::elect(&lobby.members, me)
        } else {
            None
        };
        lobby.members.retain(|m| m.id != me);
        match heir {
            Some(h) => {
                lobby.owner = h;
                self.broadcast(&lobby);
            }
            None => {
                let msg = Msg::Closed {
                    lobby: lobby.id.clone(),
                };
                for m in &lobby.members {
                    self.send_to(m.id, &msg);
                }
            }
        }
    }

    fn open_chan(&mut self, key: &(PeerId, String)) {
        let Some(chan) = self.chans.get_mut(key) else {
            return;
        };
        chan.state = ChanState::Open;
        let delayed = chan.take_delayed();
        for (channel, reliable, data) in delayed {
            self.send_to(
                key.0,
                &Msg::ChanData {
                    socket: key.1.clone(),
                    channel,
                    reliable,
                    data,
                },
            );
        }
        self.events.push_back(Event::ChanOpen {
            peer: key.0,
            socket: key.1.clone(),
        });
    }

    fn close_chan(&mut self, key: (PeerId, String), local: bool) {
        if self.chans.remove(&key).is_none() {
            return;
        }
        if local {
            self.send_to(
                key.0,
                &Msg::ChanClose {
                    socket: key.1.clone(),
                },
            );
        }
        self.inbox
            .retain(|p| !(p.peer == key.0 && p.socket == key.1));
        self.events.push_back(Event::ChanClosed {
            peer: key.0,
            socket: key.1,
            reason: if local {
                CloseReason::ByLocal
            } else {
                CloseReason::ByPeer
            },
        });
    }

    /// `ordered`: the message came off the TCP stream rather than as a datagram.
    fn handle(&mut self, me: PeerId, from: PeerId, msg: Msg, ordered: bool) {
        match msg {
            Msg::Hello { .. } | Msg::Proof { .. } | Msg::Moved => {}
            Msg::Search { search, query } => {
                let max = query.max_results.max(1) as usize;
                // A search by id or by member finds lobbies that aren't public, for a
                // friend following an invite or a friend's game. Not for a stranger:
                // every public player has every other's id.
                let known = self.known(from);
                let lobbies: Vec<Lobby> = self
                    .lobbies
                    .values()
                    .filter(|h| h.role == Role::Hosting && (known || h.lobby.permission == Permission::Public) && lobby::matches(&h.lobby, &query))
                    .map(|h| h.lobby.public_view())
                    .take(max)
                    .collect();
                self.send_to(from, &Msg::SearchReply { search, lobbies });
            }
            Msg::SearchReply { search, lobbies } => {
                let Some(s) = self.searches.get_mut(&search) else {
                    return;
                };
                if !s.awaiting.remove(&from) {
                    return;
                }
                let max = s.query.max_results.max(1) as usize;
                for l in lobbies {
                    // A peer only speaks for lobbies it owns.
                    if l.owner == from && lobby::matches(&l, &s.query) && s.results.len() < max {
                        s.results.push(l);
                    }
                }
                if s.awaiting.is_empty() {
                    self.finish_search(search);
                }
            }
            Msg::Join { lobby: id } => {
                // What the game made the lobby (friends only, invite only) holds for
                // strangers: one gets in when invited, or from a lobby we share.
                let known = self.known(from);
                let Some(held) = self.lobbies.get_mut(&id) else {
                    return;
                };
                if held.role != Role::Hosting {
                    return;
                }
                let allowed = known || held.lobby.permission == Permission::Public;
                if !allowed && held.lobby.member(from).is_none() && !self.waiting.iter().any(|(p, l, _)| *p == from && *l == id) && self.waiting.len() < JOINS_WAITING {
                    self.waiting.push((from, id, Instant::now() + JOIN_WAIT));
                    return;
                }
                self.waiting.retain(|(p, l, _)| !(*p == from && *l == id));
                let held = self.lobbies.get_mut(&id).unwrap();
                let result = if held.lobby.member(from).is_some() {
                    JoinResult::Ok
                } else if !allowed {
                    JoinResult::NotAllowed
                } else if held.lobby.open_slots() == 0 {
                    JoinResult::Full
                } else {
                    held.lobby.members.push(Member {
                        id: from,
                        attrs: Vec::new(),
                    });
                    JoinResult::Ok
                };
                let lobby = held.lobby.clone();
                self.send_to(
                    from,
                    &Msg::JoinReply {
                        lobby: id.clone(),
                        result,
                    },
                );
                if result == JoinResult::Ok {
                    self.broadcast(&lobby);
                    self.events.push_back(Event::MemberStatus {
                        lobby: id,
                        member: from,
                        status: MemberStatus::Joined,
                    });
                }
            }
            Msg::JoinReply { lobby: id, result } => {
                if !self.joins.contains_key(&id) {
                    // Let into a lobby we stopped asking for (we left, or gave up
                    // waiting): the owner is told, or we'd stay in it as a ghost.
                    if result == JoinResult::Ok && !self.lobbies.contains_key(&id) {
                        self.send_to(from, &Msg::Leave { lobby: id });
                    }
                    return;
                }
                let Some(held) = self.lobbies.get(&id) else {
                    return;
                };
                let unknown_owner = held.lobby.owner == PeerId::default();
                if held.role != Role::Joining || !(unknown_owner || held.lobby.owner == from) {
                    return;
                }
                // Asked by id, only the owner answers (a peer that doesn't host the
                // lobby says nothing), so the first answer is the owner's, "full" and
                // "not allowed" too. Unless someone else answers as the owner: a public
                // player's answer waits for JOIN_CLAIM_WAIT (`tick`), and a LAN or room
                // player's beats it.
                if unknown_owner && self.peers.get(&from).is_none_or(|p| p.stranger) {
                    match self.claims.get(&id) {
                        None => {
                            self.claims.insert(id, (from, result, Instant::now() + JOIN_CLAIM_WAIT));
                        }
                        // Let in by a second stranger answering for it: it hears we
                        // aren't coming.
                        Some(c) if c.0 != from && result == JoinResult::Ok => self.send_to(from, &Msg::Leave { lobby: id }),
                        Some(_) => {}
                    }
                    return;
                }
                self.join_answered(id, from, result);
            }
            Msg::State { lobby: new } => {
                let Some(held) = self.lobbies.get_mut(&new.id) else {
                    return;
                };
                // Only the owner we know speaks for the lobby, which also lets it name
                // its successor.
                if held.role == Role::Hosting || held.lobby.owner != from {
                    return;
                }
                let old = std::mem::replace(&mut held.lobby, new.clone());
                if held.role == Role::Joined && new.owner == me {
                    held.role = Role::Hosting;
                }
                if new.member(me).is_none() && held.role == Role::Joined {
                    // We were dropped from the roster without a kick or close.
                    self.lobbies.remove(&new.id);
                    self.events.push_back(Event::MemberStatus {
                        lobby: new.id,
                        member: me,
                        status: MemberStatus::Left,
                    });
                    return;
                }
                self.diff(&old, &new);
            }
            Msg::Leave { lobby: id } => {
                let Some(held) = self.lobbies.get_mut(&id) else {
                    return;
                };
                if held.role != Role::Hosting || held.lobby.member(from).is_none() {
                    return;
                }
                held.lobby.members.retain(|m| m.id != from);
                let lobby = held.lobby.clone();
                self.broadcast(&lobby);
                self.events.push_back(Event::MemberStatus {
                    lobby: id,
                    member: from,
                    status: MemberStatus::Left,
                });
            }
            Msg::MemberAttrs { lobby: id, attrs } => {
                let Some(held) = self.lobbies.get_mut(&id) else {
                    return;
                };
                if held.role != Role::Hosting {
                    return;
                }
                let Some(m) = held.lobby.member_mut(from) else {
                    return;
                };
                m.attrs = attrs;
                let lobby = held.lobby.clone();
                self.broadcast(&lobby);
                self.events.push_back(Event::MemberUpdated {
                    lobby: id,
                    member: from,
                });
            }
            Msg::Closed { lobby: id } => self.removed(me, from, id, MemberStatus::Closed),
            Msg::Kicked { lobby: id } => self.removed(me, from, id, MemberStatus::Kicked),
            Msg::ChanRequest { socket } => {
                let key = (from, socket.clone());
                match self.chans.get(&key).map(|c| c.state) {
                    None => {
                        self.chans.insert(
                            key,
                            Chan::new(ChanState::Requested),
                        );
                        self.events
                            .push_back(Event::ChanRequest { peer: from, socket });
                    }
                    // Both asked at once: both already agree.
                    Some(ChanState::Pending) => {
                        self.send_to(from, &Msg::ChanAccept { socket });
                        self.open_chan(&key);
                    }
                    Some(ChanState::Open) => self.send_to(from, &Msg::ChanAccept { socket }),
                    Some(ChanState::Requested) => {}
                }
            }
            Msg::ChanAccept { socket } => {
                let key = (from, socket);
                if self
                    .chans
                    .get(&key)
                    .is_some_and(|c| c.state == ChanState::Pending)
                {
                    self.open_chan(&key);
                }
            }
            Msg::ChanData {
                socket,
                channel,
                data,
                ..
            } => {
                let key = (from, socket.clone());
                match self.chans.get(&key).map(|c| c.state) {
                    Some(ChanState::Open) => {
                        let p = Packet { peer: from, socket, channel, data, ordered };
                        if self.peers.get(&from).is_some_and(|p| p.stranger) {
                            self.inbox.push_stranger(p);
                        } else {
                            self.inbox.push(p);
                        }
                    }
                    None => {
                        self.chans.insert(
                            key,
                            Chan::new(ChanState::Requested),
                        );
                        self.events
                            .push_back(Event::ChanRequest { peer: from, socket });
                    }
                    _ => {}
                }
            }
            Msg::ChanClose { socket } => self.close_chan((from, socket), false),
            Msg::Presence { kv } => {
                if self.presence.get(&from) != Some(&kv) {
                    self.presence.insert(from, kv);
                    self.events.push_back(Event::PresenceChanged(from));
                }
            }
            Msg::Invite { connect, lobby } => {
                // A lobby in an invite is only taken from the peer that owns it.
                let lobby = lobby.filter(|l| l.owner == from || l.member(from).is_some());
                self.events.push_back(Event::Invite {
                    from,
                    connect,
                    lobby,
                });
            }
        }
    }

    /// The answer to a join that counts: from the owner we asked, or (asked by id) the
    /// peer taken for the owner.
    fn join_answered(&mut self, id: String, from: PeerId, result: JoinResult) {
        // A stranger's answer held for it, now beaten: if it let us in, it hears we
        // aren't coming (or it keeps a member who never arrives).
        if let Some((other, held_result, _)) = self.claims.remove(&id)
            && other != from
            && held_result == JoinResult::Ok
        {
            self.send_to(other, &Msg::Leave { lobby: id.clone() });
        }
        self.joins.remove(&id);
        let Some(held) = self.lobbies.get_mut(&id) else {
            return;
        };
        if result == JoinResult::Ok {
            held.role = Role::Joined;
            held.lobby.owner = from;
        } else {
            self.lobbies.remove(&id);
        }
        self.events.push_back(Event::JoinDone { lobby: id, result });
    }

    /// The owner closed the lobby or kicked us out of it.
    fn removed(&mut self, me: PeerId, from: PeerId, id: String, status: MemberStatus) {
        let ours = self
            .lobbies
            .get(&id)
            .is_some_and(|h| h.role != Role::Hosting && h.lobby.owner == from);
        if ours {
            self.lobbies.remove(&id);
            self.joins.remove(&id);
            self.events.push_back(Event::MemberStatus {
                lobby: id,
                member: me,
                status,
            });
        }
    }

    /// Events for the difference between two snapshots of a lobby.
    fn diff(&mut self, old: &Lobby, new: &Lobby) {
        for m in &new.members {
            match old.member(m.id) {
                None => self.events.push_back(Event::MemberStatus {
                    lobby: new.id.clone(),
                    member: m.id,
                    status: MemberStatus::Joined,
                }),
                Some(was) if was.attrs != m.attrs => self.events.push_back(Event::MemberUpdated {
                    lobby: new.id.clone(),
                    member: m.id,
                }),
                _ => {}
            }
        }
        for m in &old.members {
            if new.member(m.id).is_none() && m.id != old.owner {
                self.events.push_back(Event::MemberStatus {
                    lobby: new.id.clone(),
                    member: m.id,
                    status: MemberStatus::Left,
                });
            }
        }
        if new.owner != old.owner && old.owner != PeerId::default() {
            if new.member(old.owner).is_none() {
                self.events.push_back(Event::MemberStatus {
                    lobby: new.id.clone(),
                    member: old.owner,
                    status: MemberStatus::Left,
                });
            }
            self.events.push_back(Event::MemberStatus {
                lobby: new.id.clone(),
                member: new.owner,
                status: MemberStatus::Promoted,
            });
        }
        self.events.push_back(Event::LobbyUpdated {
            lobby: new.id.clone(),
        });
    }

    fn peer_lost(&mut self, me: PeerId, peer: PeerId) {
        let ids: Vec<String> = self.lobbies.keys().cloned().collect();
        for id in ids {
            let Some(held) = self.lobbies.get_mut(&id) else {
                continue;
            };
            if held.role != Role::Hosting && held.lobby.owner == peer {
                if held.role == Role::Joining {
                    self.lobbies.remove(&id);
                    self.joins.remove(&id);
                    self.events.push_back(Event::JoinDone {
                        lobby: id,
                        result: JoinResult::NotFound,
                    });
                    continue;
                }
                held.lobby.members.retain(|m| m.id != peer);
                let heir = if held.lobby.host_migration {
                    lobby::elect(&held.lobby.members, peer)
                } else {
                    None
                };
                match heir {
                    None => {
                        self.lobbies.remove(&id);
                        self.events.push_back(Event::MemberStatus {
                            lobby: id,
                            member: me,
                            status: MemberStatus::Closed,
                        });
                    }
                    Some(h) => {
                        held.lobby.owner = h;
                        if h == me {
                            held.role = Role::Hosting;
                            let lobby = held.lobby.clone();
                            self.broadcast(&lobby);
                        }
                        self.events.push_back(Event::MemberStatus {
                            lobby: id.clone(),
                            member: peer,
                            status: MemberStatus::Disconnected,
                        });
                        self.events.push_back(Event::MemberStatus {
                            lobby: id.clone(),
                            member: h,
                            status: MemberStatus::Promoted,
                        });
                        self.events.push_back(Event::LobbyUpdated { lobby: id });
                    }
                }
            } else if held.role == Role::Hosting && held.lobby.member(peer).is_some() {
                held.lobby.members.retain(|m| m.id != peer);
                let lobby = held.lobby.clone();
                self.broadcast(&lobby);
                self.events.push_back(Event::MemberStatus {
                    lobby: id,
                    member: peer,
                    status: MemberStatus::Disconnected,
                });
            }
        }
        let searches: Vec<u64> = self
            .searches
            .iter_mut()
            .filter_map(|(id, s)| {
                (s.awaiting.remove(&peer) && s.awaiting.is_empty()).then_some(*id)
            })
            .collect();
        for id in searches {
            self.finish_search(id);
        }
        let chans: Vec<_> = self
            .chans
            .keys()
            .filter(|(p, _)| *p == peer)
            .cloned()
            .collect();
        for key in chans {
            self.chans.remove(&key);
            self.events.push_back(Event::ChanClosed {
                peer,
                socket: key.1,
                reason: CloseReason::Lost,
            });
        }
        self.inbox.retain(|p| p.peer != peer);
        if self.presence.remove(&peer).is_some() {
            self.events.push_back(Event::PresenceChanged(peer));
        }
        self.events.push_back(Event::PeerLeft(peer));
    }

    /// Delivers what a peer's move held back, as far as its old connections have
    /// finished, oldest first (see `Peer::prior`).
    fn release(&mut self, me: PeerId, peer: PeerId) {
        loop {
            let Some(p) = self.peers.get_mut(&peer) else { return };
            if !p.prior.front().is_some_and(|f| f.done) {
                return;
            }
            p.prior.pop_front();
            let msgs = match p.prior.front_mut() {
                Some(next) => std::mem::take(&mut next.held),
                None => std::mem::take(&mut p.held),
            };
            for m in msgs {
                self.handle(me, peer, m, true);
            }
        }
    }

    /// What a newly connected peer needs to know about us straight away.
    fn greet(&self, conn: &Conn) {
        if !self.my_presence.is_empty() {
            conn.send(&Msg::Presence {
                kv: self.my_presence.clone(),
            });
        }
        // Joins still waiting go to a peer met meanwhile: a copy that joins as it starts
        // can ask before it has met the lobby's owner. Others ignore it; the owner
        // answers a member twice alike.
        for (id, held) in &self.lobbies {
            if held.role == Role::Joining && self.joins.contains_key(id) {
                conn.send(&Msg::Join { lobby: id.clone() });
            }
        }
        // And channels asked for while this peer wasn't there to hear it (a game goes
        // on sending through a second's break in the connection): asked for again, or
        // they would wait for an answer to a question nobody got.
        for ((peer, socket), chan) in &self.chans {
            if *peer == conn.peer && chan.state == ChanState::Pending {
                conn.send(&Msg::ChanRequest { socket: socket.clone() });
            }
        }
    }
}

/// Whether a new connection from a peer replaces our old one to it outright (see
/// RECONNECT_AFTER). Only from the same address: someone else claiming the id gets no
/// further than the tie-break.
fn describe(t: Trust) -> &'static str {
    match t {
        Trust::Lan => "the LAN",
        Trust::Room => "a room tunnel",
        Trust::Public => "a public session",
    }
}

fn replaces_stale(old: &Conn, new: &Conn) -> bool {
    !old.retired() && old.addr.ip() == new.addr.ip() && old.since.elapsed() >= RECONNECT_AFTER
}

impl Sink for Inner {
    fn connected(&self, conn: &Arc<Conn>, name: String) -> bool {
        if self.is_public(conn) {
            conn.scrub_as(inet::redact::stand_in(&self.me.0));
        }
        let mut s = self.state.lock().unwrap();
        let preferred = self.me.min(conn.peer);
        if let Some(existing) = s.peers.get(&conn.peer) {
            let old = existing.conn.clone();
            let (was, now) = (self.trust(&old), self.trust(conn));
            if rank(now) < rank(was) {
                // A worse path than the one we have (a public session for a room or LAN
                // peer, a tunnel for a LAN peer): never takes over. The peer decides the
                // same, so nothing is sent on it.
                (self.log)(&format!("lan peer {} also came through {}: keeping {}", conn.peer, describe(now), describe(was)));
                return false;
            }
            if rank(now) > rank(was) {
                // A better path came up (the LAN for a tunnelled peer, a room for a public
                // one): move to it without the peer leaving. Everything we send from now
                // on goes on the new connection; the old one ends with `Moved` and stays
                // open until the peer's `Moved` comes back, so neither side loses what
                // was queued on it, and what comes on the new one meanwhile waits for the
                // old one to finish, so order is kept.
                let p = s.peers.get_mut(&conn.peer).unwrap();
                old.send(&Msg::Moved);
                let done = p.moved_off.take().is_some_and(|c| Arc::ptr_eq(&c, &old));
                if done {
                    old.retire();
                }
                p.prior.push_back(Prior { conn: old.clone(), held: Vec::new(), done });
                p.conn = conn.clone();
                p.stranger = now == Trust::Public;
                (self.log)(&format!("lan peer {} ({}) moved from {} to {}", conn.peer, p.name, describe(was), self.how(conn)));
                if done {
                    s.release(self.me, conn.peer);
                } else {
                    self.move_timeout(conn.peer, old);
                }
                return true;
            }
            if replaces_stale(&old, conn) {
                // The peer dialed us again although we're connected: it restarted or
                // lost the old connection without us noticing. The old one is dead; the
                // peer starts over, so we do too (it leaves and joins again).
                (self.log)(&format!(
                    "lan peer {} reconnected; dropping the old connection ({}s old)",
                    conn.peer,
                    old.since.elapsed().as_secs()
                ));
                s.peers.remove(&conn.peer);
                old.close();
                s.peer_lost(self.me, conn.peer);
            } else {
                // Both sides dialed: keep the connection the lower id opened, so both
                // ends keep the same one. The other is retired, not cut: what was
                // already queued on it is still delivered.
                if old.initiator == preferred || conn.initiator != preferred {
                    return false;
                }
                old.retire();
                let p = s.peers.get_mut(&conn.peer).unwrap();
                p.conn = conn.clone();
                if p.moved_off.as_ref().is_some_and(|c| Arc::ptr_eq(c, &old)) {
                    p.moved_off = None;
                }
                s.greet(conn);
                return true;
            }
        }
        (self.log)(&format!("lan peer {} ({name}) at {}", conn.peer, self.how(conn)));
        let shown = self.shown_ip(conn);
        let stranger = self.is_public(conn);
        s.peers.insert(conn.peer, Peer::new(conn.clone(), name, shown, stranger));
        s.greet(conn);
        s.events.push_back(Event::PeerJoined(conn.peer));
        true
    }

    fn message(&self, conn: &Arc<Conn>, mut msg: Msg, ordered: bool) {
        let public = self.is_public(conn);
        if public {
            // Strangers: voice and party chat unless that's turned off, and bounded presence.
            if !self.public_voice.load(std::sync::atomic::Ordering::Relaxed)
                && let Some(socket) = msg_socket(&msg).filter(|s| voice_socket(s))
            {
                self.note_drop(conn.peer, socket, "voice from a public player ignored: voice chat with public players is off");
                return;
            }
            if msg_socket(&msg).is_some_and(|s| s.len() > PUBLIC_SOCKET_BYTES) {
                self.note_drop(conn.peer, "a channel", "ignored: its name is too long to be one of ours");
                return;
            }
            if !cap_public(&mut msg) {
                return;
            }
            scrub_public(&mut msg, inet::redact::stand_in(&conn.peer.0));
        }
        let mut s = self.state.lock().unwrap();
        if public {
            // A channel the stranger doesn't have yet, past what one player may have.
            if let Msg::ChanRequest { socket } | Msg::ChanData { socket, .. } = &msg
                && !s.chans.contains_key(&(conn.peer, socket.clone()))
                && s.chans.keys().filter(|(p, _)| *p == conn.peer).count() >= PUBLIC_CHANNELS
            {
                drop(s);
                self.note_drop(conn.peer, "channels", "a new one ignored: this public player has as many open as one may have");
                return;
            }
            if let Msg::Invite { .. } = &msg
                && let Some(p) = s.peers.get_mut(&conn.peer)
                && !p.invites.take(Instant::now(), PUBLIC_INVITE_BURST, PUBLIC_INVITE_EVERY)
            {
                drop(s);
                self.note_drop(conn.peer, "invites", "ignored: this public player sends more than a player would");
                return;
            }
        }
        let Some(p) = s.peers.get_mut(&conn.peer) else {
            return;
        };
        let current = Arc::ptr_eq(&p.conn, conn);
        let prior = p.prior.iter().position(|x| Arc::ptr_eq(&x.conn, conn));
        if let Msg::Moved = msg {
            if let Some(i) = prior {
                // The peer moved too: the old connection is finished both ways.
                p.prior[i].done = true;
                p.prior[i].conn.retire();
                s.release(self.me, conn.peer);
            } else if current {
                // The peer moved to a connection we haven't got yet (see `connected`).
                p.moved_off = Some(conn.clone());
            }
            return;
        }
        match prior {
            Some(i) if ordered && i > 0 => {
                p.prior[i].held.push(msg);
                self.check_hold(&mut s, conn.peer);
                return;
            }
            Some(_) => {}
            None if current => {
                if ordered && !p.prior.is_empty() {
                    p.held.push(msg);
                    self.check_hold(&mut s, conn.peer);
                    return;
                }
            }
            None => {
                // A duplicate retired by the tie-break while the peer is still
                // connected: what the peer sent before it learned which one wins still
                // counts (across the two TCP streams order isn't kept for the brief
                // moment both carry data). Never from a worse path than the live one.
                if !conn.retired() || rank(self.trust(conn)) != rank(self.trust(&p.conn)) {
                    return;
                }
            }
        }
        let (dropped, over) = (s.inbox.dropped, s.inbox.over_share);
        s.handle(self.me, conn.peer, msg, ordered);
        let now = s.inbox.dropped;
        if now != dropped && (dropped == 0 || now.ilog2() != dropped.ilog2()) {
            (self.log)(&format!(
                "lan inbox full ({INBOX_MAX_PACKETS} packets / {} MiB): dropped the oldest; {now} so far",
                INBOX_MAX_BYTES >> 20
            ));
        }
        let now = s.inbox.over_share;
        if now != over && (over == 0 || now.ilog2() != over.ilog2()) {
            (self.log)(&format!(
                "lan dropped packets from public player {}: its share of the inbox ({PUBLIC_INBOX_PACKETS} packets / {} MiB) is full; {now} so far",
                conn.peer,
                PUBLIC_INBOX_BYTES >> 20
            ));
        }
    }

    fn disconnected(&self, conn: &Arc<Conn>) {
        let mut s = self.state.lock().unwrap();
        {
            let mut t = self.tunnels.lock().unwrap();
            if t.get(&conn.addr).is_some_and(|e| e.down.is_some()) {
                t.remove(&conn.addr);
            }
        }
        let Some(p) = s.peers.get_mut(&conn.peer) else {
            return;
        };
        if let Some(i) = p.prior.iter().position(|x| Arc::ptr_eq(&x.conn, conn)) {
            // An old connection closed: whatever it carried has arrived.
            p.prior[i].done = true;
            s.release(self.me, conn.peer);
            return;
        }
        if !Arc::ptr_eq(&p.conn, conn) {
            return;
        }
        s.peers.remove(&conn.peer);
        (self.log)(&format!("lan peer {} left", conn.peer));
        s.peer_lost(self.me, conn.peer);
    }

    fn knows(&self, peer: PeerId) -> bool {
        self.state.lock().unwrap().peers.contains_key(&peer)
    }

    fn upgradable(&self, peer: PeerId) -> bool {
        Sink::conn(self, peer).is_some_and(|c| self.tunnel(&c).is_some())
    }

    fn conn(&self, peer: PeerId) -> Option<Arc<Conn>> {
        self.state.lock().unwrap().peers.get(&peer).map(|p| p.conn.clone())
    }

    fn log(&self, line: &str) {
        (self.log)(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_messages_lose_their_addresses() {
        let w = std::net::Ipv4Addr::new(198, 18, 7, 7);
        let attr = |k: &str, v: &str| Attr { key: k.into(), value: lobby::Value::Str(v.into()), public: true };
        let mut l = Lobby::new("eos.lobby");
        l.attrs = vec![attr("host", "192.168.1.20:7777"), attr("version", "1.0.7.2"), attr("n", "x")];
        l.members.push(lobby::Member { id: PeerId::default(), attrs: vec![attr("addr", "[2001:db8::5]:9000")] });
        let mut msgs = vec![
            Msg::Presence { kv: vec![("connect".into(), "+connect 203.0.113.5:27015".into())] },
            Msg::SearchReply { search: 1, lobbies: vec![l.clone()] },
            Msg::State { lobby: l.clone() },
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
        let Msg::State { lobby } = &msgs[2] else { unreachable!() };
        assert_eq!(lobby.attrs[0].value, lobby::Value::Str("198.18.7.7:7777".into()));
        assert_eq!(lobby.attrs[1].value, lobby::Value::Str("1.0.7.2".into()), "a version was taken for an address");
        assert_eq!(lobby.members[0].attrs[0].value, lobby::Value::Str("198.18.7.7:9000".into()));
        // A member the owner relays keeps its own stand-in, not the owner's.
        let relayed = PeerId::random();
        let mut l = Lobby::new("eos.lobby");
        l.members.push(lobby::Member { id: relayed, attrs: vec![attr("addr", "192.168.1.30:7777")] });
        let mut m = Msg::State { lobby: l };
        scrub_public(&mut m, w);
        let Msg::State { lobby } = &m else { unreachable!() };
        let want = format!("{}:7777", inet::redact::stand_in(&relayed.0));
        assert_eq!(lobby.members[0].attrs[0].value, lobby::Value::Str(want));
        assert!(!may_carry_addresses(&Msg::ChanData { socket: "s".into(), channel: 0, reliable: true, data: Vec::new() }));
    }

    #[test]
    fn public_lobbies_are_bounded() {
        let attr = |k: &str, v: lobby::Value| Attr { key: k.into(), value: v, public: true };
        let mut l = Lobby::new("eos.lobby");
        l.id = "x".into();
        l.max_members = 1000;
        l.attrs = vec![
            attr("ok", lobby::Value::Str("fine".into())),
            attr("huge", lobby::Value::Str("v".repeat(5000))),
            attr(&"k".repeat(300), lobby::Value::Int(1)),
            attr("nan", lobby::Value::Double(f64::NAN)),
        ];
        l.attrs.extend((0..500).map(|i| attr(&format!("a{i}"), lobby::Value::Bool(true))));
        l.members = vec![Member { id: PeerId([1; 16]), attrs: vec![attr("big", lobby::Value::Str("m".repeat(2000)))] }];
        let mut m = Msg::State { lobby: l.clone() };
        assert!(cap_public(&mut m));
        let Msg::State { lobby: got } = m else { unreachable!() };
        assert_eq!(got.attrs.len(), PUBLIC_ATTRS);
        assert_eq!(got.attrs[0].key, "ok");
        assert!(got.attrs.iter().all(|a| a.key != "huge" && a.key != "nan" && a.key.len() <= PUBLIC_KEY_BYTES));
        assert!(got.members[0].attrs.is_empty());
        assert_eq!(got.max_members, PUBLIC_MEMBERS as u32);

        let mut crowded = l.clone();
        crowded.members = (0..100).map(|i| Member { id: PeerId([i; 16]), attrs: Vec::new() }).collect();
        assert!(!cap_public(&mut Msg::State { lobby: crowded.clone() }));
        let mut reply = Msg::SearchReply { search: 1, lobbies: vec![crowded.clone(), l.clone()] };
        assert!(cap_public(&mut reply));
        let Msg::SearchReply { lobbies, .. } = reply else { unreachable!() };
        assert_eq!(lobbies.len(), 1, "the crowded lobby is dropped");
        let mut invite = Msg::Invite { connect: "c".into(), lobby: Some(crowded) };
        assert!(cap_public(&mut invite));
        assert!(matches!(invite, Msg::Invite { lobby: None, .. }));
        assert!(!cap_public(&mut Msg::Invite { connect: "c".repeat(5000), lobby: None }));
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
        s.lobbies.insert(l.id.clone(), Held { lobby: l, role: Role::Hosting });
        for i in 0..JOINS_WAITING {
            s.waiting.push((gone, format!("closed {i}"), now + JOIN_WAIT));
        }
        s.waiting.push((gone, "hosted".into(), now + JOIN_WAIT));
        s.answer_waiting_joins(me, now);
        assert!(s.waiting.is_empty(), "{:?}", s.waiting);
        assert!(s.events.is_empty());
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

        assert!(inbox.remove(0).is_some());
        assert_eq!(inbox.bytes, 0);
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
        assert_eq!((inbox.q.len(), inbox.over_share), (4, 6), "count share");
        for _ in 0..3 {
            inbox.push_shared(packet(2, 400), 100, 1000);
        }
        assert_eq!(inbox.per_peer[&PeerId([2; 16])], (2, 800), "byte share");
        // A friend's packets aren't held to a share, and nothing was pushed out for them.
        for _ in 0..50 {
            inbox.push(packet(3, 10));
        }
        assert_eq!(inbox.q.len(), 56);
        assert_eq!(inbox.dropped, 0);
        // Read one: there's room for one more.
        let i = inbox.iter().position(|p| p.peer == PeerId([1; 16])).unwrap();
        inbox.remove(i).unwrap();
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
}
