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

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

mod relay;

use crate::id::{Identity, PeerId};
use crate::inet;
use crate::link::{Link, Standing};
use crate::lobby::{self, Attr, Lobby, Member, Query};
use crate::mesh::{self, Conn, Mesh, Sink};
use crate::wire::{JoinResult, Msg};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);
const JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Received packets kept for the game at most: a game that stops reading, or a socket
/// nobody reads, would otherwise grow the inbox without limit. The oldest go first.
const INBOX_MAX_PACKETS: usize = 65_536;
/// How long what a peer sent on a channel before closing it stays readable.
const CLOSED_KEEP: Duration = Duration::from_secs(30);
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
/// And at most this many bytes of them, per peer (a count alone lets a peer's big
/// messages pile up to gigabytes while its old connection drains).
const MOVE_HOLD_BYTES: usize = 64 << 20;

/// About what a message takes held in memory: its channel data, else a little. A
/// numbered message weighs what it carries.
fn weight(m: &Msg) -> usize {
    match m {
        Msg::Numbered { msg, .. } => weight(msg),
        Msg::ChanData { socket, data, .. } => data.len() + socket.len() + 64,
        Msg::Presence { kv } => kv.iter().map(|(k, v)| k.len() + v.len() + 48).sum::<usize>() + 64,
        _ => 256,
    }
}
/// How long a peer whose connection dropped keeps its place: it is still a peer, in its
/// lobbies and channels, and what is sent to it waits (see `link`). Back within this, it
/// picks up where it left and the game sees nothing; not back, it leaves. Without this a
/// break between a member and a lobby's owner split the lobby for good: the member made
/// itself or another the owner, the owner dropped the member, and the reconnect that
/// followed was a stranger to both lobbies. A copy that quits says so (`Msg::Bye`) and
/// leaves at once.
const LINK_GRACE: Duration = Duration::from_secs(10);

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

/// Where a channel with a peer stands, for SDKs that report a session's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChanInfo {
    /// Both sides agreed.
    pub open: bool,
    /// We asked and the peer hasn't answered yet.
    pub connecting: bool,
    /// The peer asked and we haven't accepted yet.
    pub requested: bool,
    /// Packets held until it opens, and their bytes.
    pub held_packets: usize,
    pub held_bytes: usize,
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
    /// A snapshot from the member that would inherit the lobby, saying the owner has
    /// gone, received before we noticed it ourselves, and when: applied if we notice it
    /// soon after (`peer_lost`), so the heir's first State isn't lost.
    heir_state: Option<(Lobby, Instant)>,
    /// Our member attributes as we last sent them to the owner, until a State shows
    /// them: if the lobby changes hands first, the old owner may never have had them,
    /// and they go to the new one (M-12).
    wanted: Option<Vec<Attr>>,
    /// The lobby's secret key (see `lobby::Claim`): made with the lobby, held by its
    /// members, so whoever owns it next signs its claim.
    secret: Option<[u8; 32]>,
}

impl Held {
    fn new(lobby: Lobby, role: Role) -> Held {
        Held { lobby, role, heir_state: None, wanted: None, secret: None }
    }

    /// The lobby's claim for a new owner, signed (one term on); unchanged without the
    /// secret.
    fn sign_for(&mut self, owner: PeerId) {
        if let Some(secret) = &self.secret {
            self.lobby.claim = lobby::Claim::make(secret, &self.lobby.id, owner, self.lobby.claim.term + 1);
        }
    }

    /// Whether a snapshot from the lobby's owner (or heir) is the lobby's word: a claim
    /// the lobby's key signed for the owner it names, the key we know (if we know one),
    /// and no older than the claim we have.
    fn speaks_for(&self, new: &Lobby) -> bool {
        let have = &self.lobby.claim;
        new.claim.holds(&new.id, new.owner) && (have.is_none() || (new.claim.key == have.key && new.claim.term >= have.term))
    }

    /// Takes the secret a State came with, when it's this lobby's.
    fn take_secret(&mut self, secret: &[u8; 32]) {
        if *secret != [0; 32] && lobby::lobby_key(secret) == self.lobby.claim.key {
            self.secret = Some(*secret);
        }
    }
}

/// How long an heir's early snapshot is kept for the owner's loss to be noticed here.
const HEIR_STATE_KEEP: Duration = Duration::from_secs(15);

/// How long a member that lost a lobby's owner waits for the other members' word
/// (`Msg::OwnerCheck`) before electing a new owner. A break of just its own link to the
/// owner (the owner and the others still reach each other) would otherwise split the
/// lobby once LINK_GRACE is up: the owner drops the member, and the member makes itself
/// or another the owner of a lobby the others still follow the old owner in. Asked,
/// a member that still reaches the owner passes the question on, and the owner's answer
/// (`Msg::OwnerHere`) comes back: the asker is out of the lobby instead. An owner that is
/// really gone doesn't answer, and the election goes ahead as before, this much later.
const OWNER_CHECK_WAIT: Duration = Duration::from_secs(3);
/// Owner checks passed on for others at once, at most.
const OWNER_CHECKS_RELAYED: usize = 64;

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
///
/// One queue per socket and channel, each packet numbered as it arrives: a read takes
/// the oldest packet of the queues it wants, and the inbox's oldest goes first when it
/// is full, both in a step per queue. One shared queue made every read walk past the
/// packets of channels nobody reads (L-38).
#[derive(Default)]
struct Inbox {
    queues: HashMap<(String, u8), VecDeque<(u64, Packet)>>,
    next: u64,
    len: usize,
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
        self.len += 1;
        let e = self.per_peer.entry(p.peer).or_default();
        e.0 += 1;
        e.1 += p.data.len();
        let n = self.next;
        self.next += 1;
        self.queues.entry((p.socket.clone(), p.channel)).or_default().push_back((n, p));
        while self.len > max_n || (self.bytes > max_bytes && self.len > 1) {
            let Some(old) = self.take(None, |_| true) else { break };
            self.dropped += 1;
            drop(old);
        }
    }

    /// The queue holding the oldest packet on a socket `want` accepts (and `channel`,
    /// if given).
    fn oldest(&self, channel: Option<u8>, want: impl Fn(&str) -> bool) -> Option<&(String, u8)> {
        self.queues
            .iter()
            .filter(|((socket, ch), _)| channel.is_none_or(|c| c == *ch) && want(socket))
            .filter_map(|(k, q)| q.front().map(|(n, _)| (k, *n)))
            .min_by_key(|(_, n)| *n)
            .map(|(k, _)| k)
    }

    /// The size of the packet `take` would hand out.
    fn peek_len(&self, channel: Option<u8>, want: impl Fn(&str) -> bool) -> Option<usize> {
        let k = self.oldest(channel, want)?;
        self.queues[k].front().map(|(_, p)| p.data.len())
    }

    /// The oldest packet on a socket `want` accepts (and `channel`, if given).
    fn take(&mut self, channel: Option<u8>, want: impl Fn(&str) -> bool) -> Option<Packet> {
        let k = self.oldest(channel, want)?.clone();
        let q = self.queues.get_mut(&k)?;
        let (_, p) = q.pop_front()?;
        if q.is_empty() {
            self.queues.remove(&k);
        }
        self.forget(&p);
        Some(p)
    }

    /// Takes a packet that left the inbox off the counts.
    fn forget(&mut self, p: &Packet) {
        self.len -= 1;
        self.bytes -= p.data.len();
        if let Some(e) = self.per_peer.get_mut(&p.peer) {
            e.0 -= 1;
            e.1 -= p.data.len();
            if e.0 == 0 {
                self.per_peer.remove(&p.peer);
            }
        }
    }

    fn retain(&mut self, mut f: impl FnMut(&Packet) -> bool) {
        for q in self.queues.values_mut() {
            q.retain(|(_, p)| f(p));
        }
        self.queues.retain(|_, q| !q.is_empty());
        self.len = 0;
        self.bytes = 0;
        self.per_peer.clear();
        for (_, p) in self.queues.values().flatten() {
            self.len += 1;
            self.bytes += p.data.len();
            let e = self.per_peer.entry(p.peer).or_default();
            e.0 += 1;
            e.1 += p.data.len();
        }
    }

    /// Every packet, oldest first (tests).
    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = &Packet> {
        let mut all: Vec<&(u64, Packet)> = self.queues.values().flatten().collect();
        all.sort_by_key(|(n, _)| *n);
        all.into_iter().map(|(_, p)| p)
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
    /// The presence changes a public player may still send us (PUBLIC_PRESENCE_BURST),
    /// and its newest one held back meanwhile (applied once the ration allows: the last
    /// word is never lost, only the ones in between).
    presences: Ration,
    presence_late: Option<Vec<(String, String)>>,
    /// A public player: connected to us only because public sessions are on (kept up
    /// to date as the peer moves between paths).
    stranger: bool,
    /// When we last invited this peer ourselves.
    invited: Option<Instant>,
    /// How it reaches us (kept here: a dropped connection's tunnel is gone), and whether
    /// through another player (`relay`).
    trust: Trust,
    relayed: bool,
    /// Its session: what we sent and received, for picking up after a dropped connection.
    link: RefCell<Link>,
    /// The connection dropped and the peer may come back until then (LINK_GRACE). Still
    /// set while a new connection waits for the peer's `Msg::Session` (`waiting`).
    gone: Option<Instant>,
    waiting: bool,
    /// Where our side stood in our last `Msg::Session` to it.
    said: Option<Standing>,
    /// What came from it before its `Msg::Session`, for after the decision (and the
    /// connection each came on).
    early: VecDeque<(Arc<Conn>, Msg)>,
    /// `early` is being delivered (`Inner::deliver_early`): what comes meanwhile, on
    /// another connection, queues behind it rather than overtaking it.
    draining: bool,
    /// How many times a drain was cut short by the connection dropping (`Inner::lost`):
    /// a message `deliver_early` took off the queue before that is dropped, not
    /// delivered after what it overtook was discarded (a resume sends it again; no
    /// resume and it belongs to a session that's over).
    drain_cut: u64,
    /// What `held` and `prior` (`held_bytes`) and `early` (`early_bytes`) hold, about
    /// (`weight`), against MOVE_HOLD_BYTES.
    held_bytes: usize,
    early_bytes: usize,
    /// It said goodbye, or we dropped it: no waiting for it to come back.
    bye: bool,
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
    fn new(conn: Arc<Conn>, name: String, shown: IpAddr, trust: Trust, relayed: bool) -> Peer {
        let now = Instant::now();
        Peer {
            conn,
            name,
            shown,
            prior: VecDeque::new(),
            held: Vec::new(),
            moved_off: None,
            joined: now,
            invites: Ration::new(PUBLIC_INVITE_BURST, now),
            presences: Ration::new(PUBLIC_PRESENCE_BURST, now),
            presence_late: None,
            stranger: trust == Trust::Public,
            invited: None,
            trust,
            relayed,
            link: RefCell::new(Link::new(0)),
            gone: None,
            waiting: false,
            said: None,
            early: VecDeque::new(),
            draining: false,
            drain_cut: 0,
            held_bytes: 0,
            early_bytes: 0,
            bye: false,
        }
    }

    /// Sends a message in the peer's session: numbered and kept until the peer has it
    /// (see `link`), and only kept while the connection is down or a new one waits for
    /// the peer's `Msg::Session`.
    fn post(&self, msg: &Msg) {
        if !crate::link::counted(msg) {
            if self.gone.is_none() {
                self.conn.send(msg);
            }
            return;
        }
        if self.gone.is_some() {
            self.link.borrow_mut().sent(msg, true);
            return;
        }
        let n = self.link.borrow().next();
        if self.conn.send_numbered(n, msg) {
            self.link.borrow_mut().sent(msg, false);
        }
    }

    /// Whether it has a live connection (a peer whose connection dropped has none, and
    /// the mesh may dial it again).
    fn connected(&self) -> bool {
        self.gone.is_none() || self.waiting
    }

    fn held_count(&self) -> usize {
        self.held.len() + self.prior.iter().map(|p| p.held.len()).sum::<usize>()
    }
}

/// How good a path is: a better one replaces a worse one without the peer leaving.
/// Peers ordered as `Lan::peers_by_standing` says: most trusted first, then longest here.
fn by_standing(peers: impl Iterator<Item = (Trust, bool, Instant, PeerId)>) -> Vec<PeerId> {
    let mut v: Vec<_> = peers.map(|(t, relayed, joined, id)| (std::cmp::Reverse(rank(t, relayed)), joined, id)).collect();
    v.sort();
    v.into_iter().map(|(_, _, id)| id).collect()
}

/// How good a path is: a better one replaces a worse one without the peer leaving. A
/// connection through another player (`relay`) is below every direct one but a public
/// session, and is trusted as the worse of its two halves.
fn rank(t: Trust, relayed: bool) -> u8 {
    match (t, relayed) {
        (Trust::Lan, false) => 4,
        (Trust::Room, false) => 3,
        (Trust::Lan | Trust::Room, true) => 2,
        (Trust::Public, false) => 1,
        (Trust::Public, true) => 0,
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
    /// Channels the peer closed with packets still unread, and when: what nobody has
    /// read after CLOSED_KEEP goes, unless the channel opened again.
    closed_unread: Vec<((PeerId, String), Instant)>,
    events: VecDeque<Event>,
    my_presence: Vec<(String, String)>,
    presence: HashMap<PeerId, Vec<(String, String)>>,
    /// Strangers' requests to join lobbies that aren't public, and when each is
    /// answered "not allowed" if nothing has vouched for them by then (see JOIN_WAIT).
    waiting: Vec<(PeerId, String, Instant)>,
    /// Joins by id (no key in hand): who was asked and hasn't answered, and the signed
    /// answers so far (see `settle`).
    by_id: HashMap<String, ById>,
    /// Lobbies we hosted and handed on as we left them, to whom, and until when a Join
    /// for one is pointed there (`Msg::JoinElsewhere`).
    handed: HashMap<String, (PeerId, Instant)>,
    /// Lobbies we host whose snapshot a public player's attribute change last went out
    /// for, and when, and whether a later one waits (`STRANGER_CAST_EVERY`).
    stranger_casts: HashMap<String, (Instant, bool)>,
    /// Front ends' checks of a join's secret, by lobby namespace (`Lan::set_join_check`).
    join_checks: HashMap<String, JoinCheck>,
    /// Lobbies whose owner we lost while other members are still here: the owner, and
    /// until when the others may say it is still there (OWNER_CHECK_WAIT).
    owner_checks: HashMap<String, (PeerId, Instant)>,
    /// Owner checks we passed on to a lobby's owner for members that lost it: (lobby,
    /// asker, until when an answer is passed back).
    relayed_checks: Vec<(String, PeerId, Instant)>,
    /// The secret each of our joins goes with, for asking again (another owner, a peer
    /// met meanwhile), by lobby id.
    join_secrets: HashMap<String, String>,
    /// The secrets of strangers' joins waiting in `waiting`.
    waiting_secrets: HashMap<(PeerId, String), String>,
    /// Connections through other players (`relay`).
    relay: relay::Relays,
}

/// A front end's answer to a join of one of its lobbies that came with `secret` from
/// `joiner`: None lets it in (as far as the front end goes), else the answer it gets.
/// It runs under the engine's lock: it must not call the engine.
pub type JoinCheck = fn(lobby: &Lobby, joiner: PeerId, secret: &str) -> Option<JoinResult>;

/// A public player's member attributes, however often it sends them, go out to the
/// lobby at most this often: each change is a snapshot to every member, so a stranger
/// sending them fast would have us send many times as much.
const STRANGER_CAST_EVERY: Duration = Duration::from_millis(250);

/// PeerParty's proof at the end of a sign-in ticket (`Lan::auth_ticket`): magic, key,
/// nonce, signature.
pub const TICKET_MAGIC: &[u8; 4] = b"PPAT";
pub const TICKET_LEN: usize = 4 + 32 + 16 + 64;

/// How long a lobby we handed on as we left it is still pointed to its new owner.
const HANDED_KEEP: Duration = Duration::from_secs(30);

struct Inner {
    me: PeerId,
    name: String,
    slot: u32,
    server: bool,
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
    /// This engine's session (see `link`), and how long a dropped peer keeps its place.
    session: u64,
    grace: Duration,
    /// Copies of our game heard speaking another wire version, their version, and when
    /// last (see `Lan::other_versions`).
    versions: Mutex<HashMap<PeerId, (u16, Instant)>>,
    /// The identity, whose slot lock lasts as long as the engine or until the game
    /// starts exiting (`Lan::release_slot`).
    identity: Mutex<Option<Identity>>,
    /// Peers whose connections are refused (`Lan::sever`, tests only).
    severed: Mutex<HashSet<PeerId>>,
    /// Our mesh's listening port, for relayed connections that reach us (`relay`), and
    /// whether we start and carry them at all (always, but in tests).
    mesh_port: OnceLock<u16>,
    relaying: bool,
    /// Our identity key: a relayed connection's offer and answer are signed with it.
    key: Arc<inet::public::IdentityKey>,
    /// Peers whose dropped connection we don't notice (`Lan::sever_unnoticed`, tests).
    unnoticed: Mutex<HashSet<PeerId>>,
}

/// A copy on another version counts as here while it was heard this recently (beacons
/// and board dials come every second).
const OTHER_VERSION_FOR: Duration = Duration::from_secs(15);

/// A join by id under way: see `settle`.
#[derive(Default)]
struct ById {
    /// Peers asked that haven't answered yet.
    asked: HashSet<PeerId>,
    /// Signed answers: (from, result, claim, whether a public player sent it).
    answers: Vec<(PeerId, JoinResult, lobby::Claim, bool)>,
    /// When it is settled with what came, if not everyone answered first.
    until: Option<Instant>,
}

/// How a join by id comes out.
#[derive(Debug, PartialEq)]
enum Settled {
    /// Joined (or told "full", "not allowed") by this answer.
    Take(usize),
    /// Answered for under more than one key, by players trusted alike: one of them isn't
    /// its owner and nothing says which, so no answer is taken.
    Disputed,
    /// No signed answer.
    Nobody,
}

/// A join by id, with no key in hand (a typed code, a presence string): the answers of
/// LAN and room players count over public players' (a key is free to make, a stranger
/// can't answer for a friend's lobby; its own lobbies it can), and among the answers that
/// count, all must be under one key: someone answering for a lobby that isn't theirs
/// keeps the join from going through, but can't take it over. Under that key, the latest
/// term's owner (the lobby changing hands as we asked).
fn settle(answers: &[(PeerId, JoinResult, lobby::Claim, bool)]) -> Settled {
    let friends = answers.iter().any(|a| !a.3);
    let counted: Vec<usize> = (0..answers.len()).filter(|i| !(friends && answers[*i].3)).collect();
    let Some(&first) = counted.first() else { return Settled::Nobody };
    if counted.iter().any(|i| answers[*i].2.key != answers[first].2.key) {
        return Settled::Disputed;
    }
    let best = counted.iter().copied().fold(first, |b, i| if answers[i].2.term > answers[b].2.term { i } else { b });
    Settled::Take(best)
}

struct TunnelEntry {
    tunnel: inet::Tunnel,
    down: Option<Instant>,
    /// A connection through another player (`relay`), and what it is trusted as.
    relayed: Option<Trust>,
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
/// A join by id asks every peer, and any of them can answer as the owner (`settle`):
/// once all have answered, or this long after the first signed answer, the join is
/// settled with what came (longer than JOIN_WAIT, which an owner may take to answer).
const JOIN_CLAIM_WAIT: Duration = Duration::from_secs(3);
const JOINS_WAITING: usize = 64;
/// How long our invite to a public player stands (see `State::known`).
const INVITE_STANDS: Duration = Duration::from_secs(15 * 60);
const PUBLIC_INVITE_BURST: f32 = 5.0;
const PUBLIC_INVITE_EVERY: Duration = Duration::from_secs(4);
/// A public player's presence changes: a few at once, then one every this long. Each
/// one is an event every front end handles (Steam's persona callbacks and the like).
const PUBLIC_PRESENCE_BURST: f32 = 8.0;
const PUBLIC_PRESENCE_EVERY: Duration = Duration::from_millis(500);

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
    match msg {
        Msg::Numbered { msg, .. } => may_carry_addresses(msg),
        m => matches!(m, Msg::Presence { .. } | Msg::SearchReply { .. } | Msg::State { .. } | Msg::MemberAttrs { .. } | Msg::Invite { .. }),
    }
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
        Msg::State { lobby, .. } => scrub_lobby(lobby, with),
        Msg::Numbered { msg, .. } => scrub_public(msg, with),
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
/// Whether a public player's invite connect string is one command at most: games
/// handle it as they handle their command line (`+connect 10.0.0.5:27015`), so a
/// stranger's must not carry a second command (`+exec ...`), a command separator,
/// quotes or control characters.
fn plain_connect(connect: &str) -> bool {
    let commands = connect.split_whitespace().filter(|w| w.starts_with('+') || w.starts_with('-')).count();
    commands <= 1 && !connect.chars().any(|c| c.is_control() || matches!(c, ';' | '"' | '`' | '|' | '&'))
}

fn cap_public(msg: &mut Msg) -> bool {
    match msg {
        Msg::Presence { kv } => cap_presence(kv),
        Msg::SearchReply { lobbies, .. } => {
            lobbies.retain_mut(cap_lobby);
            lobbies.truncate(PUBLIC_MEMBERS);
        }
        Msg::State { lobby, .. } => return cap_lobby(lobby),
        Msg::MemberAttrs { attrs, .. } => cap_attrs(attrs),
        Msg::Invite { connect, lobby } => {
            if connect.len() > PUBLIC_CONNECT_BYTES || !plain_connect(connect) {
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
        if !conn.addr.ip().is_loopback() {
            return Trust::Lan;
        }
        match self.tunnels.lock().unwrap().get(&conn.addr).map(|e| (e.tunnel.public, e.relayed)) {
            Some((_, Some(t))) => t,
            Some((true, None)) => Trust::Public,
            Some((false, None)) => Trust::Room,
            None => Trust::Lan,
        }
    }

    /// Whether a connection runs through another player (`relay`).
    fn relayed(&self, conn: &Conn) -> bool {
        conn.addr.ip().is_loopback() && self.tunnels.lock().unwrap().get(&conn.addr).is_some_and(|e| e.relayed.is_some())
    }

    /// A peer's live connection is gone (it closed, or the peer showed us through
    /// another player that it lost it, `relay`): it leaves at once if it said goodbye,
    /// else its place is kept a while (LINK_GRACE): what is sent to it waits, and what
    /// its old connections held back is delivered now rather than lost.
    fn lost(&self, s: &mut State, peer: PeerId) {
        let Some(p) = s.peers.get_mut(&peer) else { return };
        if p.bye || self.grace.is_zero() {
            s.peers.remove(&peer);
            (self.log)(&format!("lan peer {peer} left"));
            s.peer_lost(self.me, peer);
            return;
        }
        // Already counted lost (and not since back on a new connection).
        if p.gone.is_some() && !p.waiting {
            return;
        }
        let until = *p.gone.get_or_insert(Instant::now() + self.grace);
        p.waiting = false;
        p.said = None;
        p.early.clear();
        p.early_bytes = 0;
        p.draining = false;
        p.drain_cut += 1;
        for x in p.prior.iter_mut() {
            x.done = true;
        }
        (self.log)(&format!(
            "lan peer {peer} ({}) lost its connection: its place is kept {}s for it to come back",
            p.name,
            until.saturating_duration_since(Instant::now()).as_secs_f32().ceil()
        ));
        s.release(self.me, peer);
        s.unawait(peer);
    }

    /// How good a connection's path is (`rank`).
    fn rank_of(&self, conn: &Conn) -> u8 {
        rank(self.trust(conn), self.relayed(conn))
    }

    /// How a connection reaches us, for the log (no internet address).
    fn how(&self, conn: &Conn) -> String {
        if self.relayed(conn) {
            return "another player".into();
        }
        match self.trust(conn) {
            Trust::Lan => inet::redact::lan(conn.addr),
            t => describe(t).into(),
        }
    }

    /// Our `Msg::Session` to a peer, on its live connection; what it said is kept for
    /// the decision (`session_from`).
    fn tell_session(&self, p: &mut Peer) {
        let st = p.link.borrow().standing();
        let yours = p.link.borrow().session;
        p.said = Some(st);
        p.conn.send(&Msg::Session { mine: self.session, yours, sent: st.sent, recv: st.recv, first: st.first });
    }

    /// A peer whose connection dropped is back on `conn` (or one we didn't see drop
    /// dialed again): it waits for the peer's `Msg::Session`.
    fn come_back(&self, p: &mut Peer, conn: &Arc<Conn>, name: String) {
        let trust = self.trust(conn);
        p.conn = conn.clone();
        p.trust = trust;
        p.relayed = self.relayed(conn);
        p.stranger = trust == Trust::Public;
        p.name = name;
        p.moved_off = None;
        p.gone.get_or_insert(Instant::now() + self.grace);
        p.waiting = true;
        self.tell_session(p);
    }

    /// A peer's `Msg::Session`. From a new peer it's just its session. From one coming
    /// back, it settles whether the two pick up where they left: the same two sessions
    /// on both sides and nothing missing that isn't kept (`link::can_resume`), decided
    /// alike on both sides from what each said. If so, each sends again what the other
    /// hasn't got and the game sees nothing; if not, the peer leaves and joins again.
    fn session_from(&self, mut s: std::sync::MutexGuard<'_, State>, conn: &Arc<Conn>, mine: u64, yours: u64, theirs: Standing) {
        let peer = conn.peer;
        let Some(p) = s.peers.get_mut(&peer) else { return };
        if !p.waiting {
            let known = p.link.borrow().session;
            if known == 0 {
                p.link.borrow_mut().session = mine;
                return;
            }
            // It came back on a connection we took for a duplicate of the live one (it
            // restarted, or its old connection died without us seeing): both start
            // again from a clean connection, and the sessions then settle it.
            (self.log)(&format!("lan peer {peer} ({}) started again on another connection: reconnecting", p.name));
            p.conn.close();
            conn.close();
            return;
        }
        let said = p.said.unwrap_or_else(|| p.link.borrow().standing());
        let same = p.link.borrow().session == mine && yours == self.session;
        let resume = same && crate::link::can_resume(said, theirs);
        p.waiting = false;
        p.gone = None;
        p.said = None;
        // What came early is delivered after the lock is let go, and before anything
        // newer: the peer's other connections' readers queue behind it meanwhile.
        p.draining = true;
        if resume {
            let again = p.link.borrow_mut().replay(theirs.recv);
            for (n, m) in &again {
                p.conn.send_numbered(*n, m);
            }
            (self.log)(&format!("lan peer {peer} ({}) picked up where it left: {} message(s) sent again", p.name, again.len()));
        } else {
            let why = if p.link.borrow().session != mine {
                "it started again"
            } else if !same {
                "it had given up on us"
            } else {
                "more was missed than was kept"
            };
            (self.log)(&format!("lan peer {peer} ({}) is back, but {why}: it left and joined again", p.name));
            *p.link.borrow_mut() = Link::new(mine);
            let c = p.conn.clone();
            s.peer_lost(self.me, peer);
            s.events.push_back(Event::PeerJoined(peer));
            s.greet(&c);
        }
        drop(s);
        self.deliver_early(peer);
    }

    /// What came from a peer before its `Msg::Session`, in the order it came, one at a
    /// time with the lock let go between (delivering takes it). What its connections
    /// bring meanwhile joins the back of the queue (`draining`), so a newer message on
    /// another connection can't overtake an older one held here.
    fn deliver_early(&self, peer: PeerId) {
        loop {
            let (c, m, cut) = {
                let mut s = self.state.lock().unwrap();
                let Some(p) = s.peers.get_mut(&peer) else { return };
                // Lost and back again meanwhile: the next decision delivers what's left.
                if p.waiting || !p.draining {
                    return;
                }
                let Some((c, m)) = p.early.pop_front() else {
                    p.draining = false;
                    p.early_bytes = 0;
                    return;
                };
                p.early_bytes = p.early_bytes.saturating_sub(weight(&m));
                (c, m, p.drain_cut)
            };
            self.receive(&c, m, true, Some(cut));
        }
    }

    /// Too much held back by a move (MOVE_HOLD_MAX): give up on order and deliver.
    fn check_hold(&self, s: &mut State, peer: PeerId) {
        let Some(p) = s.peers.get_mut(&peer) else { return };
        if p.held_count() <= MOVE_HOLD_MAX && p.held_bytes <= MOVE_HOLD_BYTES {
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
    /// LINK_GRACE (zero: a peer whose connection drops leaves at once).
    pub link_grace: Option<Duration>,
    /// Some(false): no connections through other players (`relay`), started or carried.
    pub relay: Option<bool>,
}

impl TestIntervals {
    pub const NONE: Self = Self { handshake: None, lan_upgrade_every: None, replace_quiet: None, link_grace: None, relay: None };
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
            // Nor is a peer we reach only through another player (`relay`): its tunnel
            // takes over as a better path.
            Some(c) if self.inner.relayed(&c) => false,
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
            m.insert(mesh, TunnelEntry { tunnel: t, down: None, relayed: None });
        }
    }
    fn tunnel_down(&self, mesh: SocketAddr, leaving: bool) {
        // Kept until the mesh connection on it is gone (`Sink::disconnected`).
        if let Some(e) = self.inner.tunnels.lock().unwrap().get_mut(&mesh) {
            e.down.get_or_insert_with(Instant::now);
        }
        // Its peer left for good: when the mesh connection ends, so does the peer,
        // without waiting for it to come back (LINK_GRACE).
        if leaving {
            let mut s = self.inner.state.lock().unwrap();
            if let Some(p) = s.peers.values_mut().find(|p| p.conn.addr == mesh) {
                p.bye = true;
            }
        }
    }
    fn log(&self, line: &str) {
        (self.inner.log)(line);
    }
}

static GLOBAL: OnceLock<Lan> = OnceLock::new();

impl Lan {
    /// The process-wide engine, started on first use. Front ends in one process share
    /// it; the first one's game key wins. `server`: this process is a dedicated server,
    /// which takes a server identity slot (`Identity::claim_server`), not a player's.
    pub fn global(game: &str, server: bool, log: fn(&str)) -> io::Result<Lan> {
        if let Some(lan) = GLOBAL.get() {
            return Ok(lan.clone());
        }
        // One starts at a time: two first callers at once would each bind the ports and
        // claim an identity slot, and the one not kept would hold them for good.
        static STARTING: Mutex<()> = Mutex::new(());
        let _starting = STARTING.lock().unwrap_or_else(|e| e.into_inner());
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
            identity: if server {
                Identity::claim_server_for(&Identity::default_dir(), game)
            } else {
                Identity::claim_for(&Identity::default_dir(), game)
            },
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
            server: opts.identity.server,
            game: opts.game.clone(),
            state: Mutex::new(State::default()),
            log: opts.log,
            tunnels: Mutex::new(HashMap::new()),
            public_voice: std::sync::atomic::AtomicBool::new(public_voice_env()),
            drop_logged: Mutex::new(HashSet::new()),
            this: OnceLock::new(),
            session: u64::from_le_bytes(PeerId::random().0[..8].try_into().unwrap_or_default()) | 1,
            grace: test_intervals().link_grace.unwrap_or(LINK_GRACE),
            versions: Mutex::new(HashMap::new()),
            identity: Mutex::new(None),
            severed: Mutex::new(HashSet::new()),
            mesh_port: OnceLock::new(),
            relaying: test_intervals().relay.unwrap_or(true),
            key: opts.identity.key.clone(),
            unnoticed: Mutex::new(HashSet::new()),
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
        let _ = inner.mesh_port.set(port);
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
        // The identity's slot lock lives as long as the engine.
        *inner.identity.lock().unwrap() = Some(opts.identity);
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
        let mut dropped = 0;
        {
            let mut s = self.inner.state.lock().unwrap();
            let internet: Vec<PeerId> = s.peers.iter().filter(|(_, p)| p.trust != Trust::Lan).map(|(id, _)| *id).collect();
            for id in internet {
                let p = s.peers.get_mut(&id).unwrap();
                p.bye = true;
                dropped += 1;
                if p.connected() {
                    p.conn.bye();
                } else {
                    // Its connection had dropped already: nothing will say it ended.
                    s.peers.remove(&id);
                    s.peer_lost(self.inner.me, id);
                }
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
    /// A dedicated server's slots are apart from players' (`server`).
    pub fn slot(&self) -> u32 {
        self.inner.slot
    }

    /// Whether this copy is a dedicated server (its identity is a server slot's).
    pub fn server(&self) -> bool {
        self.inner.server
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
        by_standing(s.peers.iter().map(|(id, p)| (p.trust, p.relayed, p.joined, *id)))
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

    /// Copies of this game heard lately that run another PeerParty, which can't play with
    /// this one: for each, whether its version is newer than ours. The overlay says so,
    /// rather than the player wondering why a friend never shows up.
    pub fn other_versions(&self) -> Vec<bool> {
        let v = self.inner.versions.lock().unwrap();
        v.values()
            .filter(|(_, at)| at.elapsed() < OTHER_VERSION_FOR)
            .map(|(version, _)| *version > crate::wire::VERSION)
            .collect()
    }

    /// Cuts the live connection to a peer, as a network failure would (tests).
    #[doc(hidden)]
    pub fn cut_connection(&self, id: PeerId) {
        if let Some(c) = Sink::conn(&*self.inner, id) {
            c.close();
        }
    }

    /// Cuts the connection to a peer and refuses every new one with it, as a link that
    /// stays broken (tests: the rest of the mesh still connects).
    #[doc(hidden)]
    pub fn sever(&self, id: PeerId) {
        self.inner.severed.lock().unwrap().insert(id);
        self.cut_connection(id);
    }

    /// `sever`, but our side doesn't notice the connection is gone: it stays the
    /// peer's connection, as a link that dies without a word can for a while (tests).
    #[doc(hidden)]
    pub fn sever_unnoticed(&self, id: PeerId) {
        self.inner.unnoticed.lock().unwrap().insert(id);
        self.sever(id);
    }

    /// Lets a peer `sever` cut off connect again (tests).
    #[doc(hidden)]
    pub fn mend(&self, id: PeerId) {
        self.inner.severed.lock().unwrap().remove(&id);
        self.inner.unnoticed.lock().unwrap().remove(&id);
    }

    /// Whether we reach a peer only through another player (see `relay`).
    pub fn peer_is_relayed(&self, id: PeerId) -> bool {
        Sink::conn(&*self.inner, id).is_some_and(|c| self.inner.relayed(&c))
    }

    /// Whether a peer's connection runs through an internet tunnel.
    pub fn peer_is_internet(&self, id: PeerId) -> bool {
        Sink::conn(&*self.inner, id).is_some_and(|c| self.inner.tunnel(&c).is_some())
    }

    /// How a peer reaches us (`None`: not connected).
    pub fn peer_trust(&self, id: PeerId) -> Option<Trust> {
        Sink::conn(&*self.inner, id).map(|c| self.inner.trust(&c))
    }

    /// The proof PeerParty adds to a game's sign-in ticket (Steam's auth tickets): this
    /// player's public key, a fresh number and the key's signature over it
    /// (TICKET_LEN bytes, starting with TICKET_MAGIC). `check_auth_ticket` on another PC
    /// says whose ticket it is: no player can sign in as another.
    pub fn auth_ticket(&self) -> Option<Vec<u8>> {
        let key = self.inner.identity.lock().unwrap().as_ref().map(|i| i.key.clone())?;
        let nonce: [u8; 16] = inet::crypto::random_array();
        // Not bound to the game's key: a dedicated server is often another app id
        // than its game's, and checks its players' tickets all the same.
        let sig = key.sign_ticket("", &nonce);
        let mut t = Vec::with_capacity(TICKET_LEN);
        t.extend_from_slice(TICKET_MAGIC);
        t.extend_from_slice(&key.public());
        t.extend_from_slice(&nonce);
        t.extend_from_slice(&sig);
        Some(t)
    }

    /// Whose a sign-in ticket is: None when it carries no proof of PeerParty's at its
    /// end; else whether the proof holds and is `claimed`'s.
    pub fn check_auth_ticket(&self, ticket: &[u8], claimed: PeerId) -> Option<bool> {
        let t = ticket.get(ticket.len().checked_sub(TICKET_LEN)?..)?;
        if &t[..4] != TICKET_MAGIC {
            return None;
        }
        let key: [u8; 32] = t[4..36].try_into().ok()?;
        let nonce: [u8; 16] = t[36..52].try_into().ok()?;
        let sig: [u8; 64] = t[52..116].try_into().ok()?;
        Some(PeerId::of_key(&key) == claimed && inet::public::ticket_proves(&key, &sig, "", &nonce))
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
        if let Some(p) = self.inner.state.lock().unwrap().peers.get_mut(&id) {
            p.bye = true;
        }
        conn.close();
        (self.inner.log)(&format!("lan blocked public player {id} ({fp})"));
        Ok(fp)
    }

    /// Takes a public player off the block list again (the overlay's Undo), by the
    /// fingerprint `block_peer` gave. Whether it was on the list.
    pub fn unblock(&self, fp: &str) -> Result<bool, String> {
        let Some(p) = self.net.public() else { return Err("public sessions are off".into()) };
        let removed = p.unblock(fp).map_err(|e| e.to_string())?;
        (self.inner.log)(&format!("lan unblocked public player {fp}"));
        Ok(removed)
    }

    /// Lets this copy's identity slot go, for a game that is exiting: started again
    /// straight away, it finds its slot free and is the same player (core's id.rs).
    /// Never blocks.
    pub fn release_slot(&self) {
        if let Ok(mut id) = self.inner.identity.try_lock() {
            id.take();
        }
    }

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

    /// Stops discovery and drops every connection. Peers see us leave, at once: they're
    /// told it's for good.
    pub fn shutdown(&self) {
        self.net.on.store(false, std::sync::atomic::Ordering::Relaxed);
        for i in [self.net.room.lock().unwrap().take(), self.net.public.lock().unwrap().take()].into_iter().flatten() {
            i.stop();
        }
        self.mesh.stop();
        let peers: Vec<Arc<Conn>> = {
            let mut s = self.inner.state.lock().unwrap();
            for p in s.peers.values_mut() {
                p.bye = true;
            }
            s.peers.values().filter(|p| p.connected()).map(|p| p.conn.clone()).collect()
        };
        // Leaving for good: peers drop us at once rather than wait for us (LINK_GRACE).
        for c in peers {
            c.bye();
        }
    }

    /// Times out searches and joins. Call it every frame, before `poll`.
    pub fn tick(&self) {
        let starts = self.tick_locked();
        // Connections through other players, dialed now the state is free.
        for st in starts {
            relay::start(Arc::downgrade(&self.inner), &self.mesh, st);
        }
    }

    fn tick_locked(&self) -> Vec<relay::Start> {
        let now = Instant::now();
        let mut s = self.inner.state.lock().unwrap();
        s.forget_closed_unread(now);
        relay::drain(&self.inner, &mut s);
        let starts = relay::tick(&self.inner, &mut s, now);
        // Peers whose connection dropped and that didn't come back in time (or had more
        // waiting for them than is kept), and acknowledgements of what came quietly.
        let expired: Vec<PeerId> = s
            .peers
            .iter()
            .filter(|(_, p)| p.gone.is_some_and(|t| t <= now || p.link.borrow().overfull()))
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(p) = s.peers.remove(&id) {
                p.conn.close();
                (self.inner.log)(&format!(
                    "lan peer {id} ({}) {}: it left",
                    p.name,
                    if p.link.borrow().overfull() {
                        "had more waiting for it than is kept while it was away".to_string()
                    } else {
                        format!("didn't come back within {}s", self.inner.grace.as_secs())
                    }
                ));
            }
            s.peer_lost(self.inner.me, id);
        }
        for (id, p) in &s.peers {
            if p.gone.is_some() {
                continue;
            }
            if let Some(n) = p.link.borrow_mut().ack_due(now) {
                p.conn.send(&Msg::Ack { recv: n });
            }
            // Messages lost on a connection that was cut while another carried later
            // ones: asked for again, and in the end given up on.
            match p.link.borrow_mut().gap_due(now) {
                Some(crate::link::Gap::Ask(from)) => {
                    (self.inner.log)(&format!("lan peer {id} ({}): messages after {from} missing; asking for them again", p.name));
                    p.conn.send(&Msg::Resend { from });
                }
                Some(crate::link::Gap::GaveUp(lost)) => {
                    (self.inner.log)(&format!("lan peer {id} ({}): {lost} message(s) lost for good", p.name));
                }
                None => {}
            }
        }
        // A public player's presence held back by its ration, once there's room.
        let late: Vec<(PeerId, Vec<(String, String)>)> = s
            .peers
            .iter_mut()
            .filter(|(_, p)| p.presence_late.is_some())
            .filter_map(|(id, p)| p.presences.take(now, PUBLIC_PRESENCE_BURST, PUBLIC_PRESENCE_EVERY).then(|| (*id, p.presence_late.take().unwrap())))
            .collect();
        for (id, kv) in late {
            if s.presence.get(&id) != Some(&kv) {
                s.presence.insert(id, kv);
                s.events.push_back(Event::PresenceChanged(id));
            }
        }
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
        s.handed.retain(|_, (_, until)| *until > now);
        s.relayed_checks.retain(|(_, _, until)| *until > now);
        // Owners lost with nobody saying they are still there: elected away from now.
        let due: Vec<(String, PeerId)> =
            s.owner_checks.iter().filter(|(_, (_, until))| *until <= now).map(|(id, (owner, _))| (id.clone(), *owner)).collect();
        for (id, owner) in due {
            s.owner_checks.remove(&id);
            // Back meanwhile (a new session): its next snapshot says where we stand.
            if s.peers.get(&owner).is_some_and(Peer::connected) {
                continue;
            }
            s.owner_gone(self.inner.me, id, owner);
        }
        s.send_owed_casts(now);
        // Joins by id not everyone answered: settled with what came.
        let due: Vec<String> =
            s.by_id.iter().filter(|(id, b)| b.until.is_some_and(|u| u <= now) || !s.joins.contains_key(*id)).map(|(id, _)| id.clone()).collect();
        for id in due {
            s.settle_by_id(&id);
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
        starts
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
        let secret = lobby::new_lobby_secret();
        lobby.claim = lobby::Claim::make(&secret, &lobby.id, me, 1);
        s.lobbies.insert(lobby.id.clone(), Held { secret: Some(secret), ..Held::new(lobby.clone(), Role::Hosting) });
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
        // Not a peer whose connection dropped: its answer would come after the timeout.
        let awaiting: HashSet<PeerId> = s.peers.iter().filter(|(_, p)| p.connected()).map(|(id, _)| *id).collect();
        let msg = Msg::Search {
            search: id,
            query: query.clone(),
        };
        for p in &awaiting {
            s.peers[p].post(&msg);
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
        self.join_with(target, "")
    }

    /// Joins with a secret the lobby's owner checks (its password, an invitation).
    pub fn join_with(&self, target: &Lobby, secret: &str) -> Result<(), LobbyError> {
        let mut s = self.inner.state.lock().unwrap();
        if s.lobbies.contains_key(&target.id) {
            return Err(LobbyError::AlreadyIn);
        }
        let Some(owner) = s.peers.get(&target.owner) else {
            return Err(LobbyError::OwnerUnknown);
        };
        owner.post(&Msg::Join {
            lobby: target.id.clone(),
            secret: secret.into(),
        });
        s.remember_secret(&target.id, secret);
        s.lobbies.insert(
            target.id.clone(),
            Held::new(target.clone(), Role::Joining),
        );
        s.joins
            .insert(target.id.clone(), Instant::now() + JOIN_TIMEOUT);
        Ok(())
    }

    /// Joins by id alone: every peer is asked and the owner answers.
    pub fn join_by_id(&self, namespace: &str, id: &str) -> Result<(), LobbyError> {
        self.join_by_id_with(namespace, id, "")
    }

    /// `join_by_id` with a secret the owner checks.
    pub fn join_by_id_with(&self, namespace: &str, id: &str, secret: &str) -> Result<(), LobbyError> {
        let mut s = self.inner.state.lock().unwrap();
        if s.lobbies.contains_key(id) {
            return Err(LobbyError::AlreadyIn);
        }
        let msg = Msg::Join { lobby: id.into(), secret: secret.into() };
        s.remember_secret(id, secret);
        let mut asked = HashSet::new();
        for (pid, p) in &s.peers {
            p.post(&msg);
            if p.connected() {
                asked.insert(*pid);
            }
        }
        s.by_id.insert(id.into(), ById { asked, ..ById::default() });
        let mut placeholder = Lobby::new(namespace);
        placeholder.id = id.into();
        s.lobbies.insert(
            id.into(),
            Held::new(placeholder, Role::Joining),
        );
        s.joins.insert(id.into(), Instant::now() + JOIN_TIMEOUT);
        Ok(())
    }

    /// A front end checks the secret joins of its lobbies (namespace `namespace`) come
    /// with, on the owner's PC: what only the owner may decide (a password, an
    /// invitation) isn't left to the joiner's own copy.
    pub fn set_join_check(&self, namespace: &str, check: JoinCheck) {
        self.inner.state.lock().unwrap().join_checks.insert(namespace.into(), check);
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
            Role::Hosting => s.close_hosted(me, held, true),
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
            Role::Hosting => s.close_hosted(me, held, false),
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
        held.wanted = (!owner).then(|| attrs.clone());
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
            held.sign_for(target);
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
            p.post(&msg);
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
            if to.is_none() && p.trust == Trust::Public {
                continue;
            }
            if to.is_none_or(|t| t == *id) {
                p.post(&msg);
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
            && s.peers.get(&peer).is_some_and(|p| p.trust == Trust::Public)
        {
            drop(s);
            self.inner.note_drop(peer, socket, "not sent: voice to a public player, and voice chat with public players is off");
            return SendResult::Dropped;
        }
        // Bigger than one message to this peer can be (`Conn::frame_max`, with room for
        // the socket's name and the framing): the connection would drop it unseen, so
        // the game hears it wasn't sent.
        if let Some(p) = s.peers.get(&peer)
            && data.len() + socket.len() + 64 > p.conn.frame_max()
        {
            let max = p.conn.frame_max();
            drop(s);
            self.inner.note_drop(peer, socket, &format!("not sent: {} bytes in one message, more than the {max} a message to this player can carry", data.len()));
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
        s.peers.get(&peer).is_some_and(|p| p.gone.is_none() && p.conn.udp_ready())
    }

    /// Whether our datagrams to `peer` carry sequence numbers (it runs this version too).
    pub fn udp_sequenced(&self, peer: PeerId) -> bool {
        let s = self.inner.state.lock().unwrap();
        s.peers.get(&peer).is_some_and(|p| p.gone.is_none() && p.conn.udp_sequenced())
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

    /// Our channel with `peer` on `socket`, if there is one.
    pub fn channel_info(&self, peer: PeerId, socket: &str) -> Option<ChanInfo> {
        let s = self.inner.state.lock().unwrap();
        let c = s.chans.get(&(peer, socket.to_string()))?;
        Some(ChanInfo {
            open: c.state == ChanState::Open,
            connecting: c.state == ChanState::Pending,
            requested: c.state == ChanState::Requested,
            held_packets: c.delayed.len(),
            held_bytes: c.delayed_bytes,
        })
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
        s.inbox.peek_len(channel, want)
    }

    pub fn receive_where(&self, channel: Option<u8>, want: impl Fn(&str) -> bool) -> Option<Packet> {
        let mut s = self.inner.state.lock().unwrap();
        s.inbox.take(channel, want)
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
        (s.inbox.len as u64, s.inbox.bytes as u64)
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
                self.waiting_secrets.remove(&(peer, lobby));
                continue;
            }
            if self.known(peer) || until <= now {
                // Kept in `waiting` so the handler answers this time, one way or the other.
                self.waiting.push((peer, lobby.clone(), until));
                let secret = self.waiting_secrets.remove(&(peer, lobby.clone())).unwrap_or_default();
                self.handle(me, peer, Msg::Join { lobby, secret }, true);
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
            p.post(msg);
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
                p.post(&msg);
            }
        }
    }

    fn send_unreliable_to(&self, peer: PeerId, msg: &Msg) {
        if let Some(p) = self.peers.get(&peer)
            && p.gone.is_none()
        {
            p.conn.send_unreliable(msg);
        }
    }

    /// Snapshots a public player's attribute changes held back (`STRANGER_CAST_EVERY`),
    /// once their time comes; lobbies no longer ours, and quiet ones, are forgotten.
    fn send_owed_casts(&mut self, now: Instant) {
        let due: Vec<String> = self
            .stranger_casts
            .iter()
            .filter(|(_, (at, owed))| *owed && now.duration_since(*at) >= STRANGER_CAST_EVERY)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            if let Some(lobby) = self.lobbies.get(&id).filter(|h| h.role == Role::Hosting).map(|h| h.lobby.clone()) {
                self.broadcast(&lobby);
                self.stranger_casts.insert(id, (now, false));
            }
        }
        let lobbies = &self.lobbies;
        self.stranger_casts.retain(|id, (at, owed)| {
            lobbies.get(id).is_some_and(|h| h.role == Role::Hosting) && (*owed || now.duration_since(*at) < Duration::from_secs(10))
        });
    }

    /// The secret a join of ours goes with, kept while it's under way (bounded: a game
    /// that keeps joining keeps a few).
    fn remember_secret(&mut self, id: &str, secret: &str) {
        if secret.is_empty() {
            self.join_secrets.remove(id);
            return;
        }
        if self.join_secrets.len() >= 64 {
            let joins = &self.joins;
            self.join_secrets.retain(|k, _| joins.contains_key(k));
        }
        self.join_secrets.insert(id.into(), secret.into());
    }

    /// The owner's snapshot to every other member.
    /// The lobby's snapshot to its members, with its secret (see `lobby::Claim`).
    fn broadcast(&self, lobby: &Lobby) {
        let secret = self.lobbies.get(&lobby.id).and_then(|h| h.secret);
        self.broadcast_with(lobby, secret);
    }

    fn broadcast_with(&self, lobby: &Lobby, secret: Option<[u8; 32]>) {
        let msg = Msg::State { lobby: lobby.clone(), secret: secret.unwrap_or([0; 32]) };
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
    /// A lobby we're in changed hands: member attributes we sent that its State doesn't
    /// show yet went to the old owner, which may never have read them. They go to the
    /// new owner, or apply here if that's us. Shown ones need nothing.
    fn resend_wanted(&mut self, me: PeerId, id: &str) {
        let Some(held) = self.lobbies.get_mut(id) else { return };
        let Some(want) = held.wanted.take() else { return };
        let Some(mine) = held.lobby.member_mut(me) else { return };
        if mine.attrs == want {
            return;
        }
        if held.role == Role::Hosting {
            mine.attrs = want;
            let lobby = held.lobby.clone();
            self.broadcast(&lobby);
            self.events.push_back(Event::MemberUpdated { lobby: id.into(), member: me });
        } else {
            let owner = held.lobby.owner;
            held.wanted = Some(want.clone());
            self.send_to(owner, &Msg::MemberAttrs { lobby: id.into(), attrs: want });
        }
    }

    fn close_hosted(&mut self, me: PeerId, mut held: Held, migrate: bool) {
        let heir = if migrate && held.lobby.host_migration {
            lobby::elect(&held.lobby.members, me)
        } else {
            None
        };
        held.lobby.members.retain(|m| m.id != me);
        match heir {
            Some(h) => {
                held.lobby.owner = h;
                held.sign_for(h);
                let lobby = held.lobby;
                self.handed.insert(lobby.id.clone(), (h, Instant::now() + HANDED_KEEP));
                self.broadcast_with(&lobby, held.secret);
            }
            None => {
                let msg = Msg::Closed {
                    lobby: held.lobby.id.clone(),
                };
                for m in &held.lobby.members {
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

    /// What peers sent on channels they closed, unread after CLOSED_KEEP: gone, unless
    /// the channel opened again (then it's the new channel's to read).
    fn forget_closed_unread(&mut self, now: Instant) {
        if self.closed_unread.is_empty() {
            return;
        }
        let (old, keep): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.closed_unread).into_iter().partition(|(_, at)| now.duration_since(*at) >= CLOSED_KEEP);
        self.closed_unread = keep;
        for (key, _) in old {
            if !self.chans.contains_key(&key) {
                self.inbox.retain(|p| !(p.peer == key.0 && p.socket == key.1));
            }
        }
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
        // Closing our end drops what we hadn't read. When the peer closes, what it sent
        // before closing stays readable, as on a TCP stream: a game's last message
        // (a disconnect) is often sent just before it leaves.
        if local {
            self.inbox.retain(|p| !(p.peer == key.0 && p.socket == key.1));
        } else {
            self.closed_unread.push((key.clone(), Instant::now()));
        }
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
            Msg::Hello { .. }
            | Msg::Proof { .. }
            | Msg::Moved
            | Msg::Session { .. }
            | Msg::Ack { .. }
            | Msg::Bye
            | Msg::Numbered { .. }
            | Msg::Resend { .. } => {}
            // Bytes of connections through us or to us: the engine's (`relay::drain`).
            m @ (Msg::Relay { .. } | Msg::RelayEnd { .. }) => self.relay.inbox.push((from, m)),
            Msg::Search { search, query } => {
                let max = query.max_results.max(1) as usize;
                // A search by id or by member finds lobbies that aren't public, for a
                // friend following an invite or a friend's game. Not for a stranger:
                // every public player has every other's id.
                let known = self.known(from);
                let lobbies: Vec<Lobby> = self
                    .lobbies
                    .values()
                    .filter(|h| h.role == Role::Hosting && (known || h.lobby.permission.open_to_strangers()) && lobby::matches(&h.lobby, &query))
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
                    // A peer only speaks for lobbies it owns, signed (see `lobby::Claim`).
                    if l.owner == from && l.claim.holds(&l.id, from) && lobby::matches(&l, &s.query) && s.results.len() < max {
                        s.results.push(l);
                    }
                }
                if s.awaiting.is_empty() {
                    self.finish_search(search);
                }
            }
            Msg::Join { lobby: id, secret } => {
                // What the game made the lobby (friends only, invite only) holds for
                // strangers: one gets in when invited, or from a lobby we share.
                let known = self.known(from);
                // Not ours (any more): the asker had an old owner. Pointed to the one we
                // know, so it needn't wait out its join (M-12).
                let elsewhere = match self.lobbies.get(&id) {
                    Some(held) if held.role == Role::Joined => Some(held.lobby.owner),
                    Some(_) => None,
                    None => self.handed.get(&id).map(|h| h.0),
                };
                if let Some(owner) = elsewhere {
                    if owner != from && owner != me && owner != PeerId::default() {
                        self.send_to(from, &Msg::JoinElsewhere { lobby: id, owner });
                    }
                    return;
                }
                // Not ours: said so (unsigned), so a join by id needn't wait for us.
                if self.lobbies.get(&id).is_none_or(|h| h.role != Role::Hosting) {
                    self.send_to(from, &Msg::JoinReply { lobby: id, result: JoinResult::NotFound, claim: lobby::Claim::default() });
                    return;
                }
                let Some(held) = self.lobbies.get_mut(&id) else {
                    return;
                };
                // Asked again by a member (its JoinReply was lost): answered, but the
                // others aren't told it joined twice.
                if held.lobby.member(from).is_some() {
                    let claim = held.lobby.claim;
                    self.send_to(from, &Msg::JoinReply { lobby: id, result: JoinResult::Ok, claim });
                    return;
                }
                let allowed = known || held.lobby.permission.open_to_strangers();
                if !allowed && !self.waiting.iter().any(|(p, l, _)| *p == from && *l == id) && self.waiting.len() < JOINS_WAITING {
                    self.waiting_secrets.insert((from, id.clone()), secret);
                    self.waiting.push((from, id, Instant::now() + JOIN_WAIT));
                    return;
                }
                // The front end's say (a password, an invitation), on this PC alone.
                let checked = self.join_checks.get(&held.lobby.namespace).and_then(|check| check(&held.lobby, from, &secret));
                self.waiting.retain(|(p, l, _)| !(*p == from && *l == id));
                let held = self.lobbies.get_mut(&id).unwrap();
                let result = if !allowed {
                    JoinResult::NotAllowed
                } else if let Some(r) = checked {
                    r
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
                        claim: lobby.claim,
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
            Msg::JoinElsewhere { lobby: id, owner } => {
                // Only from the player we asked, about a join still waiting: asked again
                // there, within the same deadline (a chain of these can't keep it going).
                // To a join by id: a member pointing at the owner, which was asked too.
                if let Some(b) = self.by_id.get_mut(&id) {
                    b.asked.remove(&from);
                    if b.asked.is_empty() {
                        self.settle_by_id(&id);
                    }
                    return;
                }
                let Some(held) = self.lobbies.get_mut(&id) else { return };
                if held.role != Role::Joining || !self.joins.contains_key(&id) || held.lobby.owner != from || owner == me {
                    return;
                }
                let Some(p) = self.peers.get(&owner) else { return };
                held.lobby.owner = owner;
                let secret = self.join_secrets.get(&id).cloned().unwrap_or_default();
                p.post(&Msg::Join { lobby: id, secret });
            }
            Msg::OwnerCheck { lobby: id, owner } => {
                let Some(held) = self.lobbies.get(&id) else { return };
                if held.role == Role::Hosting {
                    // Asked about us by a member that reaches us, for one that doesn't:
                    // we're here.
                    if owner == me && held.lobby.member(from).is_some() {
                        self.send_to(from, &Msg::OwnerHere { lobby: id });
                    }
                    return;
                }
                // A member lost the owner we still reach (as far as we know): the owner
                // answers it through us. An owner that is gone doesn't, and the asker's
                // election goes ahead. The asker may be off our roster already (the
                // owner dropped it as it lost it), so it needn't be on it.
                if held.role != Role::Joined || held.lobby.owner != owner || owner == from {
                    return;
                }
                if !self.peers.get(&owner).is_some_and(Peer::connected) || self.relayed_checks.len() >= OWNER_CHECKS_RELAYED {
                    return;
                }
                if !self.relayed_checks.iter().any(|(l, p, _)| *l == id && *p == from) {
                    self.relayed_checks.push((id.clone(), from, Instant::now() + OWNER_CHECK_WAIT));
                }
                self.send_to(owner, &Msg::OwnerCheck { lobby: id, owner });
            }
            Msg::OwnerHere { lobby: id } => {
                let Some(held) = self.lobbies.get(&id) else { return };
                // The owner's answer to questions we passed on: back to whoever asked.
                if held.role == Role::Joined && held.lobby.owner == from {
                    let now = Instant::now();
                    let askers: Vec<PeerId> =
                        self.relayed_checks.iter().filter(|(l, _, until)| *l == id && *until > now).map(|(_, p, _)| *p).collect();
                    self.relayed_checks.retain(|(l, _, _)| *l != id);
                    for a in askers {
                        self.send_to(a, &Msg::OwnerHere { lobby: id.clone() });
                    }
                    return;
                }
                // Our own question, answered by a member: the owner is still there for
                // the others and only our link to it broke. The owner has dropped us
                // already (its own LINK_GRACE), so we're out rather than in a lobby of
                // our own.
                let Some((owner, _)) = self.owner_checks.get(&id) else { return };
                if held.role != Role::Joined || held.lobby.owner != *owner || from == *owner || held.lobby.member(from).is_none() {
                    return;
                }
                self.owner_checks.remove(&id);
                self.lobbies.remove(&id);
                self.events.push_back(Event::MemberStatus { lobby: id, member: me, status: MemberStatus::Closed });
            }
            Msg::JoinReply { lobby: id, result, claim } => {
                if !self.joins.contains_key(&id) {
                    // Let into a lobby we stopped asking for (we left, gave up waiting,
                    // or joined by id under another answer): the owner is told, or we'd
                    // stay in it as a ghost.
                    if result == JoinResult::Ok && self.lobbies.get(&id).is_none_or(|h| h.lobby.owner != from) {
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
                let signed = claim.holds(&id, from);
                // Asked by id: every peer answers, the owner under the lobby's key, the
                // others that it isn't theirs; settled once all have (`settle`).
                if unknown_owner {
                    let Some(b) = self.by_id.get_mut(&id) else { return };
                    b.asked.remove(&from);
                    if signed && !b.answers.iter().any(|a| a.0 == from) {
                        let stranger = self.peers.get(&from).is_none_or(|p| p.stranger);
                        b.answers.push((from, result, claim, stranger));
                        b.until.get_or_insert(Instant::now() + JOIN_CLAIM_WAIT);
                    }
                    if b.asked.is_empty() {
                        self.settle_by_id(&id);
                    }
                    return;
                }
                // Only the lobby's owner answers, under the lobby's key (see
                // `lobby::Claim`). A lobby we know the key of (from a search, an invite)
                // is answered for by its key's holder alone, at once.
                if !signed {
                    return;
                }
                let pinned = held.lobby.claim;
                if pinned.is_none() || (claim.key == pinned.key && claim.term >= pinned.term) {
                    self.join_answered(id, from, result, claim);
                }
            }
            Msg::State { lobby: new, secret } => {
                let Some(held) = self.lobbies.get_mut(&new.id) else {
                    // Its owner still counts us in a lobby we left: our Leave went to an
                    // owner that handed the lobby on before reading it (M-12).
                    if new.owner == from && new.member(me).is_some() {
                        self.send_to(from, &Msg::Leave { lobby: new.id });
                    }
                    return;
                };
                // The member that inherits the lobby when its owner goes may say so
                // before we notice the owner has gone: kept for when we do (`peer_lost`),
                // never applied while the owner is still here.
                if held.role == Role::Joined
                    && held.lobby.owner != from
                    && held.lobby.host_migration
                    && new.owner == from
                    && new.member(held.lobby.owner).is_none()
                    && lobby::elect(&held.lobby.members, held.lobby.owner) == Some(from)
                    && held.speaks_for(&new)
                {
                    held.heir_state = Some((new, Instant::now()));
                    return;
                }
                // Only the owner we know speaks for the lobby, which also lets it name
                // its successor, and only in the lobby's own name (see `lobby::Claim`).
                if held.role == Role::Hosting || held.lobby.owner != from || !held.speaks_for(&new) {
                    return;
                }
                held.heir_state = None;
                let old_claim = held.lobby.claim;
                let old = std::mem::replace(&mut held.lobby, new.clone());
                if old_claim.is_none() || old_claim.key == new.claim.key {
                    held.take_secret(&secret);
                }
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
                if old.owner != new.owner {
                    self.resend_wanted(me, &new.id);
                }
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
                let stranger = self.peers.get(&from).is_some_and(|p| p.stranger);
                let now = Instant::now();
                match self.stranger_casts.get_mut(&id) {
                    // Sent for a stranger a moment ago: this one waits for `tick`.
                    Some((at, owed)) if stranger && now.duration_since(*at) < STRANGER_CAST_EVERY => *owed = true,
                    _ => {
                        if stranger {
                            self.stranger_casts.insert(id.clone(), (now, false));
                        }
                        self.broadcast(&lobby);
                    }
                }
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
                // A lobby in an invite is only taken from the peer that owns it, or a
                // member, under its owner's signed claim.
                let lobby = lobby.filter(|l| (l.owner == from || l.member(from).is_some()) && l.claim.holds(&l.id, l.owner));
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
    /// Settles a join by id with the answers that came (`settle`). Every other player
    /// that let us in hears we aren't coming (or it keeps a member who never arrives).
    fn settle_by_id(&mut self, id: &str) {
        let Some(b) = self.by_id.remove(id) else { return };
        let asking = self.joins.contains_key(id) && self.lobbies.get(id).is_some_and(|h| h.role == Role::Joining && h.lobby.owner == PeerId::default());
        let answers: Vec<_> = b.answers.into_iter().filter(|a| self.peers.contains_key(&a.0)).collect();
        let settled = if asking { settle(&answers) } else { Settled::Nobody };
        let taken = match settled {
            Settled::Take(i) => Some(answers[i].0),
            _ => None,
        };
        for (from, result, _, _) in &answers {
            if *result == JoinResult::Ok && Some(*from) != taken {
                self.send_to(*from, &Msg::Leave { lobby: id.into() });
            }
        }
        if !asking {
            return;
        }
        match settled {
            Settled::Take(i) => {
                let (from, result, claim, _) = answers[i];
                self.join_answered(id.into(), from, result, claim);
            }
            Settled::Disputed => {
                self.join_answered(id.into(), PeerId::default(), JoinResult::NotFound, lobby::Claim::default());
            }
            // Everyone said it isn't theirs.
            Settled::Nobody if b.asked.is_empty() => {
                self.join_answered(id.into(), PeerId::default(), JoinResult::NotFound, lobby::Claim::default());
            }
            // Nobody yet: the rest may still answer.
            Settled::Nobody => {
                self.by_id.insert(id.into(), ById { asked: b.asked, ..ById::default() });
            }
        }
    }

    fn join_answered(&mut self, id: String, from: PeerId, result: JoinResult, claim: lobby::Claim) {
        self.joins.remove(&id);
        self.join_secrets.remove(&id);
        let Some(held) = self.lobbies.get_mut(&id) else {
            return;
        };
        if result == JoinResult::Ok {
            held.role = Role::Joined;
            held.lobby.owner = from;
            held.lobby.claim = claim;
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
                // Others we still reach may still reach the owner: they have their say
                // first (OWNER_CHECK_WAIT).
                let others: Vec<PeerId> = held
                    .lobby
                    .members
                    .iter()
                    .map(|m| m.id)
                    .filter(|m| *m != me && *m != peer && self.peers.get(m).is_some_and(Peer::connected))
                    .collect();
                if held.lobby.host_migration && !others.is_empty() {
                    for m in others {
                        self.send_to(m, &Msg::OwnerCheck { lobby: id.clone(), owner: peer });
                    }
                    self.owner_checks.insert(id, (peer, Instant::now() + OWNER_CHECK_WAIT));
                    continue;
                }
                self.owner_gone(me, id, peer);
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
        self.peer_gone(peer);
    }

    /// A lobby we joined lost its owner `peer` for good: a new owner is elected (or the
    /// lobby closes for us).
    fn owner_gone(&mut self, me: PeerId, id: String, peer: PeerId) {
        {
            let Some(held) = self.lobbies.get_mut(&id) else {
                return;
            };
            if held.role != Role::Joined || held.lobby.owner != peer {
                return;
            }
            {
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
                        // The heir's snapshot, if it got here first.
                        if let Some((new, at)) = held.heir_state.take()
                            && new.owner == h
                            && at.elapsed() < HEIR_STATE_KEEP
                            && h != me
                        {
                            if new.member(me).is_some() {
                                let old = std::mem::replace(&mut held.lobby, new.clone());
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
                                // Both name `h` as the owner: what else changed.
                                self.diff(&old, &new);
                                self.resend_wanted(me, &id);
                                return;
                            }
                        }
                        if h == me {
                            held.role = Role::Hosting;
                            held.sign_for(me);
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
                        self.resend_wanted(me, &id);
                        self.events.push_back(Event::LobbyUpdated { lobby: id });
                    }
                }
            }
        }
    }

    /// What else goes with a peer that left: its searches, channels, packets, presence.
    fn peer_gone(&mut self, peer: PeerId) {
        self.unawait(peer);
        relay::peer_gone(self, peer);
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

    /// Searches stop waiting for a peer's answer (it's gone, or its connection dropped
    /// and the question with it).
    fn unawait(&mut self, peer: PeerId) {
        let searches: Vec<u64> = self
            .searches
            .iter_mut()
            .filter_map(|(id, s)| (s.awaiting.remove(&peer) && s.awaiting.is_empty()).then_some(*id))
            .collect();
        for id in searches {
            self.finish_search(id);
        }
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
            let w: usize = msgs.iter().map(weight).sum();
            p.held_bytes = p.held_bytes.saturating_sub(w);
            for m in msgs {
                self.handle(me, peer, m, true);
            }
        }
    }

    /// What a newly connected peer needs to know about us straight away.
    fn greet(&self, conn: &Conn) {
        let Some(p) = self.peers.get(&conn.peer) else { return };
        if !self.my_presence.is_empty() {
            p.post(&Msg::Presence {
                kv: self.my_presence.clone(),
            });
        }
        // Joins still waiting go to a peer met meanwhile: a copy that joins as it starts
        // can ask before it has met the lobby's owner. Others ignore it; the owner
        // answers a member twice alike.
        for (id, held) in &self.lobbies {
            if held.role == Role::Joining && self.joins.contains_key(id) {
                let secret = self.join_secrets.get(id).cloned().unwrap_or_default();
                p.post(&Msg::Join { lobby: id.clone(), secret });
            }
        }
        // And channels asked for while this peer wasn't there to hear it (a game goes
        // on sending through a second's break in the connection): asked for again, or
        // they would wait for an answer to a question nobody got.
        for ((peer, socket), chan) in &self.chans {
            if *peer == conn.peer && chan.state == ChanState::Pending {
                p.post(&Msg::ChanRequest { socket: socket.clone() });
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

impl Inner {
    /// A message from a peer's connection (`Sink::message`), or (`drain`, the peer's
    /// `drain_cut` when it was taken) one `deliver_early` takes off its early queue.
    fn receive(&self, conn: &Arc<Conn>, msg: Msg, ordered: bool, drain: Option<u64>) {
        let draining = drain.is_some();
        // A number belongs to the ordered stream; a datagram with one is a lie.
        if !ordered && matches!(msg, Msg::Numbered { .. }) {
            return;
        }
        // The session (see `link`): what is about the session itself, what waits for
        // the peer's `Msg::Session`, and the numbers of what came, which drop what we
        // already have.
        if ordered {
            let mut s = self.state.lock().unwrap();
            let Some(p) = s.peers.get_mut(&conn.peer) else {
                return;
            };
            // Taken off the early queue, then the connection dropped before it was delivered.
            if drain.is_some_and(|cut| cut != p.drain_cut) {
                return;
            }
            if Arc::ptr_eq(&p.conn, conn) || p.prior.iter().any(|x| Arc::ptr_eq(&x.conn, conn)) || conn.retired() {
                match &msg {
                    &Msg::Session { mine, yours, sent, recv, first } => {
                        self.session_from(s, conn, mine, yours, Standing { sent, recv, first });
                        return;
                    }
                    &Msg::Ack { recv } => {
                        if p.gone.is_none() {
                            p.link.borrow_mut().acked(recv);
                        }
                        return;
                    }
                    Msg::Bye => {
                        p.bye = true;
                        return;
                    }
                    &Msg::Resend { from } => {
                        if p.gone.is_none() {
                            let again = p.link.borrow_mut().resend(from, Instant::now());
                            if !again.is_empty() {
                                (self.log)(&format!("lan peer {} ({}) missed messages: {} sent again", conn.peer, p.name, again.len()));
                            }
                            for (n, m) in &again {
                                p.conn.send_numbered(*n, m);
                            }
                        }
                        return;
                    }
                    _ => {}
                }
                if p.waiting || (p.draining && !draining) {
                    if p.early.len() >= MOVE_HOLD_MAX || p.early_bytes > MOVE_HOLD_BYTES {
                        // Never seen: its Session is the first thing it sends.
                        p.conn.close();
                    } else {
                        p.early_bytes += weight(&msg);
                        p.early.push_back((conn.clone(), msg));
                    }
                    return;
                }
                if let Msg::Numbered { n, .. } = &msg {
                    let now = Instant::now();
                    if !p.link.borrow_mut().received(*n, now) {
                        return;
                    }
                    if p.gone.is_none()
                        && let Some(n) = p.link.borrow_mut().ack_due(now)
                    {
                        p.conn.send(&Msg::Ack { recv: n });
                    }
                }
            }
        }
        let mut msg = match msg {
            Msg::Numbered { msg, .. } => *msg,
            m => m,
        };
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
            if let Msg::Presence { kv } = &msg
                && let Some(p) = s.peers.get_mut(&conn.peer)
                && !p.presences.take(Instant::now(), PUBLIC_PRESENCE_BURST, PUBLIC_PRESENCE_EVERY)
            {
                p.presence_late = Some(kv.clone());
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
                p.held_bytes += weight(&msg);
                p.prior[i].held.push(msg);
                self.check_hold(&mut s, conn.peer);
                return;
            }
            Some(_) => {}
            None if current => {
                if ordered && !p.prior.is_empty() {
                    p.held_bytes += weight(&msg);
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
                if !conn.retired() || self.rank_of(conn) != rank(p.trust, p.relayed) {
                    return;
                }
            }
        }
        let (dropped, over) = (s.inbox.dropped, s.inbox.over_share);
        s.handle(self.me, conn.peer, msg, ordered);
        relay::drain(self, &mut s);
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
}

impl Sink for Inner {
    fn connected(&self, conn: &Arc<Conn>, name: String) -> bool {
        if self.severed.lock().unwrap().contains(&conn.peer) && !self.relayed(conn) {
            conn.close();
            return false;
        }
        if self.is_public(conn) {
            conn.scrub_as(inet::redact::stand_in(&self.me.0));
        }
        let mut s = self.state.lock().unwrap();
        let preferred = self.me.min(conn.peer);
        if let Some(existing) = s.peers.get_mut(&conn.peer)
            && existing.gone.is_some()
            && !existing.waiting
        {
            // Back after its connection dropped (LINK_GRACE), by whatever path: whether
            // it picks up where it left is decided when its `Msg::Session` comes.
            (self.log)(&format!("lan peer {} ({}) is back, through {}", conn.peer, existing.name, self.how(conn)));
            self.come_back(existing, conn, name);
            return true;
        }
        if let Some(existing) = s.peers.get(&conn.peer) {
            let old = existing.conn.clone();
            let (was, now) = (existing.trust, self.trust(conn));
            let (rank_was, rank_now) = (rank(was, existing.relayed), self.rank_of(conn));
            if rank_now < rank_was {
                // A worse path than the one we have (a public session for a room or LAN
                // peer, a tunnel for a LAN peer): never takes over. The peer decides the
                // same, so nothing is sent on it.
                (self.log)(&format!("lan peer {} also came through {}: keeping {}", conn.peer, describe(now), describe(was)));
                return false;
            }
            if rank_now > rank_was {
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
                p.trust = now;
                let from = if std::mem::replace(&mut p.relayed, self.relayed(conn)) { "another player" } else { describe(was) };
                (self.log)(&format!("lan peer {} ({}) moved from {from} to {}", conn.peer, p.name, self.how(conn)));
                if done {
                    s.release(self.me, conn.peer);
                } else {
                    self.move_timeout(conn.peer, old);
                }
                return true;
            }
            if replaces_stale(&old, conn) {
                // The peer dialed us again although we're connected: it restarted or
                // lost the old connection without us noticing. The old one is dead. Its
                // `Msg::Session` says which: a restart starts over (it leaves and joins
                // again), a lost connection picks up where it left.
                (self.log)(&format!(
                    "lan peer {} reconnected; dropping the old connection ({}s old)",
                    conn.peer,
                    old.since.elapsed().as_secs()
                ));
                if !self.grace.is_zero() {
                    let p = s.peers.get_mut(&conn.peer).unwrap();
                    self.come_back(p, conn, name);
                    old.close();
                    return true;
                }
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
                // Coming back, it hears from us once the decision is made.
                if !p.waiting {
                    s.greet(conn);
                }
                return true;
            }
        }
        (self.log)(&format!("lan peer {} ({name}) at {}", conn.peer, self.how(conn)));
        let shown = self.shown_ip(conn);
        let mut p = Peer::new(conn.clone(), name, shown, self.trust(conn), self.relayed(conn));
        self.tell_session(&mut p);
        s.peers.insert(conn.peer, p);
        s.greet(conn);
        s.events.push_back(Event::PeerJoined(conn.peer));
        true
    }

    fn message(&self, conn: &Arc<Conn>, msg: Msg, ordered: bool) {
        self.receive(conn, msg, ordered, None);
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
        if self.unnoticed.lock().unwrap().contains(&conn.peer) {
            (self.log)(&format!("lan peer {}: its connection closed, unnoticed (test)", conn.peer));
            return;
        }
        self.lost(&mut s, conn.peer);
    }

    fn knows(&self, peer: PeerId) -> bool {
        self.state.lock().unwrap().peers.get(&peer).is_some_and(Peer::connected)
    }

    fn upgradable(&self, peer: PeerId) -> bool {
        Sink::conn(self, peer).is_some_and(|c| self.tunnel(&c).is_some())
    }

    fn conn(&self, peer: PeerId) -> Option<Arc<Conn>> {
        self.state.lock().unwrap().peers.get(&peer).filter(|p| p.connected()).map(|p| p.conn.clone())
    }

    fn other_version(&self, id: PeerId, version: u16) {
        let now = Instant::now();
        let mut v = self.versions.lock().unwrap();
        if v.len() >= 64 {
            v.retain(|_, (_, at)| now.duration_since(*at) < OTHER_VERSION_FOR);
        }
        let new = v.get(&id).is_none_or(|(_, at)| now.duration_since(*at) >= OTHER_VERSION_FOR);
        if v.len() < 64 || v.contains_key(&id) {
            v.insert(id, (version, now));
        }
        drop(v);
        if new {
            (self.log)(&format!(
                "lan: a copy of this game ({id}) runs {} PeerParty (wire version {version}; this one speaks {}): they can't play together until both run the same version",
                if version > crate::wire::VERSION { "a newer" } else { "an older" },
                crate::wire::VERSION
            ));
        }
    }

    fn log(&self, line: &str) {
        (self.log)(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 32] = [42; 32];

    /// `l` under its owner's claim for `term`, signed with SECRET.
    fn signed(mut l: Lobby, term: u64) -> Lobby {
        l.claim = lobby::Claim::make(&SECRET, &l.id, l.owner, term);
        l
    }

    fn state(l: &Lobby) -> Msg {
        Msg::State { lobby: l.clone(), secret: SECRET }
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
        let attr = |k: &str, v: &str| Attr { key: k.into(), value: lobby::Value::Str(v.into()), public: true };
        let mut l = Lobby::new("eos.lobby");
        l.attrs = vec![attr("host", "192.168.1.20:7777"), attr("version", "1.0.7.2"), attr("n", "x")];
        l.members.push(lobby::Member { id: PeerId::default(), attrs: vec![attr("addr", "[2001:db8::5]:9000")] });
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
        assert_eq!(lobby.attrs[0].value, lobby::Value::Str("198.18.7.7:7777".into()));
        assert_eq!(lobby.attrs[1].value, lobby::Value::Str("1.0.7.2".into()), "a version was taken for an address");
        assert_eq!(lobby.members[0].attrs[0].value, lobby::Value::Str("198.18.7.7:9000".into()));
        // A member the owner relays keeps its own stand-in, not the owner's.
        let relayed = PeerId::random();
        let mut l = Lobby::new("eos.lobby");
        l.members.push(lobby::Member { id: relayed, attrs: vec![attr("addr", "192.168.1.30:7777")] });
        let mut m = Msg::State { lobby: l, secret: [0; 32] };
        scrub_public(&mut m, w);
        let Msg::State { lobby, .. } = &m else { unreachable!() };
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
        by_heir.attrs.push(Attr { key: "map".into(), value: lobby::Value::Str("new".into()), public: true });
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
        let ready = vec![Attr { key: "ready".into(), value: lobby::Value::Str("1".into()), public: true }];
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
        assert_eq!((inbox.bytes, inbox.len, inbox.queues.len()), (0, 0, 0));
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
        assert_eq!((inbox.len, inbox.queues.len()), (99, 1));
        // Full: the oldest anywhere goes, not the newest channel's.
        let mut inbox = Inbox::default();
        inbox.push_capped(p("a", 0, 1), 2, 1000);
        inbox.push_capped(p("b", 0, 2), 2, 1000);
        inbox.push_capped(p("b", 0, 3), 2, 1000);
        assert_eq!(inbox.iter().map(|p| p.data[0]).collect::<Vec<_>>(), [2, 3]);
        assert!(!inbox.queues.contains_key(&("a".to_string(), 0)));
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
        let attr = |v: &str| Attr { key: "map".into(), value: lobby::Value::Str(v.into()), public: true };

        let mut forged = l.clone();
        forged.attrs.push(attr("forged"));
        forged.claim = lobby::Claim::make(&[7; 32], "x", owner, 9);
        s.handle(me, owner, Msg::State { lobby: forged, secret: [7; 32] }, true);
        let mut unsigned = l.clone();
        unsigned.attrs.push(attr("unsigned"));
        unsigned.claim = lobby::Claim::default();
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
        let fake = lobby::Claim::make(&[7; 32], "x", owner, 5);
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
        s.handle(me, faker, Msg::JoinReply { lobby: "y".into(), result: JoinResult::Ok, claim: lobby::Claim::default() }, true);
        assert!(s.by_id["y"].answers.is_empty() && s.by_id["y"].until.is_none() && s.lobbies["y"].role == Role::Joining);
        let theirs = lobby::Claim::make(&[7; 32], "y", faker, 1);
        s.handle(me, faker, Msg::JoinReply { lobby: "y".into(), result: JoinResult::Ok, claim: theirs }, true);
        assert!(s.by_id["y"].answers.len() == 1 && s.by_id["y"].until.is_some() && s.lobbies["y"].role == Role::Joining);
        s.handle(me, owner, Msg::JoinReply { lobby: "y".into(), result: JoinResult::NotFound, claim: lobby::Claim::default() }, true);
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
        let real = |owner: PeerId, term: u64| lobby::Claim::make(&[7; 32], "y", owner, term);
        let fake = lobby::Claim::make(&[9; 32], "y", f, 1);
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
}
