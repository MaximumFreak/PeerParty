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

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

mod channel;
mod lobby;
mod policy;
mod relay;
#[cfg(test)]
mod tests;

use channel::*;
pub use lobby::JoinCheck;
use lobby::*;
pub use policy::{ChannelKind, Trust};
use policy::*;

use crate::id::{Identity, PeerId};
use crate::identity_key::{self, IdentityKey};
use crate::inet;
use crate::link::{Link, Standing};
use crate::lobby::{Attr, Lobby, Member, Query};
use crate::mesh::{self, Conn, Mesh, Sink};
use crate::wire::{JoinResult, Msg};

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
/// How long a tunnel's mesh connection may outlive its bridge closing its end (what came
/// is still being read meanwhile) before we cut it ourselves: the close doesn't always
/// reach our end, and a peer that left would stay listed.
const TUNNEL_END_WAIT: Duration = Duration::from_secs(2);

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
    link: Link,
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
    /// Numbered messages that came past a gap in the peer's numbers (see `link`), by
    /// number, with the connection each came on: delivered once the gap fills or is
    /// given up on (`Inner::release_gap`), so an older one still on another connection
    /// can't land after them (both sides dialing at once: for a moment both
    /// connections carry data). Kept through a lost connection (they're counted, so a
    /// resume won't send them again); dropped with the session when it starts over.
    gap: std::collections::BTreeMap<u64, (Arc<Conn>, Msg)>,
    gap_bytes: usize,
    /// It said goodbye, or we dropped it: no waiting for it to come back.
    bye: bool,
}

struct Prior {
    conn: Arc<Conn>,
    /// Messages that came on it while an older one still drains.
    held: Vec<Msg>,
    /// The peer sent its last message on it (or it closed).
    done: bool,
    /// When it is retired if the peer never finishes it (MOVE_WAIT; `Lan::tick`).
    retire_at: Instant,
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
            link: Link::new(0),
            gone: None,
            waiting: false,
            said: None,
            early: VecDeque::new(),
            draining: false,
            drain_cut: 0,
            held_bytes: 0,
            early_bytes: 0,
            gap: std::collections::BTreeMap::new(),
            gap_bytes: 0,
            bye: false,
        }
    }

    /// Sends a message in the peer's session: numbered and kept until the peer has it
    /// (see `link`), and only kept while the connection is down or a new one waits for
    /// the peer's `Msg::Session`.
    fn post(&mut self, msg: &Msg) {
        if !msg.is_counted() {
            if self.gone.is_none() {
                self.conn.send(msg);
            }
            return;
        }
        if self.gone.is_some() {
            self.link.sent(msg, true);
            return;
        }
        let n = self.link.next();
        if self.conn.send_numbered(n, msg) {
            self.link.sent(msg, false);
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
    /// Owner checks that ran out with the owner back in a new session, asked of the
    /// owner itself: one that restarted has no lobby and doesn't answer.
    owners_asked: HashSet<String>,
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
    /// When the last traffic report covered up to (`traffic_report`).
    traffic_at: Option<Instant>,
}

/// PeerParty's proof at the end of a sign-in ticket (`Lan::auth_ticket`): magic, key,
/// nonce, signature.
pub const TICKET_MAGIC: &[u8; 4] = b"PPAT";
pub const TICKET_LEN: usize = 4 + 32 + 16 + 64;

struct Inner {
    me: PeerId,
    name: String,
    slot: u32,
    server: bool,
    game: String,
    state: Mutex<State>,
    /// Signalled (with `state`) when something a thread waiting on the state may want
    /// came: a relayed connection keyed, the engine's tick (`relay::pump`).
    wake: Condvar,
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
    /// Ourselves, for the threads of connections through other players (`relay`).
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
    #[cfg(any(test, feature = "test-util"))]
    severed: Mutex<HashSet<PeerId>>,
    /// Our mesh's listening port, for relayed connections that reach us (`relay`), and
    /// whether we start and carry them at all (always, but in tests).
    mesh_port: OnceLock<u16>,
    relaying: bool,
    /// Our identity key: a relayed connection's offer and answer are signed with it.
    key: Arc<IdentityKey>,
    /// Peers whose dropped connection we don't notice (`Lan::sever_unnoticed`, tests).
    #[cfg(any(test, feature = "test-util"))]
    unnoticed: Mutex<HashSet<PeerId>>,
}

/// A copy on another version counts as here while it was heard this recently (beacons
/// and board dials come every second).
const OTHER_VERSION_FOR: Duration = Duration::from_secs(15);

struct TunnelEntry {
    tunnel: inet::Tunnel,
    down: Option<Instant>,
    /// When the bridge closed its end of the mesh connection (`Host::tunnel_closed`):
    /// one still open TUNNEL_END_WAIT later is cut (`Lan::tick`).
    closed: Option<Instant>,
    /// A connection through another player (`relay`), and what it is trusted as.
    relayed: Option<Trust>,
}

/// Whether a tunnel's connection crosses the internet: every room or public tunnel, and a
/// relay unless it runs between LAN players.
fn crosses_internet(e: &TunnelEntry) -> bool {
    e.relayed != Some(Trust::Lan)
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
        if conn.addr.ip().is_loopback() && self.tunnels.lock().unwrap().contains_key(&conn.addr) {
            return crate::redact::stand_in(&conn.peer.0).into();
        }
        conn.addr.ip()
    }

    /// Asked for every message that comes in: looked up without copying the tunnel.
    fn is_public(&self, conn: &Conn) -> bool {
        conn.addr.ip().is_loopback() && self.tunnels.lock().unwrap().get(&conn.addr).is_some_and(|e| e.tunnel.public)
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

    /// Whether a connection crosses the internet (see `Lan::peer_is_internet`): through a
    /// tunnel, unless that is a relay between LAN players. Such a relay still has a
    /// loopback end and a stand-in (`shown_ip`), and no direct address
    /// (`Lan::peer_direct_ip`): the LAN address the game could open a socket to is one
    /// this PC can't reach.
    fn internet(&self, conn: &Conn) -> bool {
        conn.addr.ip().is_loopback() && self.tunnels.lock().unwrap().get(&conn.addr).is_some_and(crosses_internet)
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
            Trust::Lan => crate::redact::lan(conn.addr),
            t => describe(t).into(),
        }
    }

    /// Our `Msg::Session` to a peer, on its live connection; what it said is kept for
    /// the decision (`session_from`).
    fn tell_session(&self, p: &mut Peer) {
        let st = p.link.standing();
        let yours = p.link.session;
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
            let known = p.link.session;
            if known == 0 {
                p.link.session = mine;
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
        let said = p.said.unwrap_or_else(|| p.link.standing());
        let same = p.link.session == mine && yours == self.session;
        let resume = same && crate::link::can_resume(said, theirs);
        p.waiting = false;
        p.gone = None;
        p.said = None;
        // What came early is delivered after the lock is let go, and before anything
        // newer: the peer's other connections' readers queue behind it meanwhile.
        p.draining = true;
        if resume {
            let again = p.link.replay(theirs.recv);
            for (n, m) in &again {
                p.conn.send_numbered(*n, m);
            }
            (self.log)(&format!("lan peer {peer} ({}) picked up where it left: {} message(s) sent again", p.name, again.len()));
        } else {
            let why = if p.link.session != mine {
                "it started again"
            } else if !same {
                "it had given up on us"
            } else {
                "more was missed than was kept"
            };
            (self.log)(&format!("lan peer {peer} ({}) is back, but {why}: it left and joined again", p.name));
            p.link = Link::new(mine);
            p.gap.clear();
            p.gap_bytes = 0;
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

/// What the engine reads from the environment, in one place (`Options::from_env`, and
/// `internet_wanted` / `Lan::start` for what isn't set otherwise). The launcher's settings
/// arrive this way.
struct Env;

impl Env {
    fn var(k: &str) -> Option<String> {
        std::env::var(k).ok().map(|v| v.trim().to_string())
    }

    /// `PEERPARTY_INTERNET`: internet play starts switched on unless it's 0/off/false/no
    /// (the launcher's "Internet play" switch). A room code or public sessions still has
    /// to be set for there to be anything to switch.
    fn internet() -> bool {
        Self::var("PEERPARTY_INTERNET").is_none_or(|v| !matches!(v.to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"))
    }

    /// `PEERPARTY_PUBLIC_VOICE`: voice with public players is off unless it's set, and to
    /// something other than 0/off/false/no. A microphone isn't opened to strangers by a
    /// variable that's missing.
    fn public_voice() -> bool {
        Self::var("PEERPARTY_PUBLIC_VOICE").is_some_and(|v| !matches!(v.to_ascii_lowercase().as_str(), "" | "0" | "off" | "false" | "no"))
    }

    /// `PEERPARTY_LAN_DISCOVERY=0`: no beacons or same-PC board.
    fn lan_discovery() -> bool {
        Self::var("PEERPARTY_LAN_DISCOVERY").is_none_or(|v| v != "0")
    }

    /// `PEERPARTY_PEERS`: extra beacon targets ("host[:port]", comma-separated).
    fn peers() -> Vec<SocketAddr> {
        Self::var("PEERPARTY_PEERS")
            .unwrap_or_default()
            .split([',', ';', ' '])
            .filter_map(|s| {
                let s = s.trim();
                s.parse::<SocketAddr>().ok().or_else(|| format!("{s}:{}", mesh::DISCOVERY_PORT).parse().ok())
            })
            .collect()
    }
}

/// Tries, and the wait between them, for a fixed internet port still held as internet
/// play is switched on again (see `Lan::internet_up`).
const RETRY_PORT: (u32, Duration) = (5, Duration::from_millis(50));

/// The switch flipped before the engine started (0: not flipped, 1: on, 2: off).
static INTERNET_CHOICE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Whether `Lan::global` starts with internet play on: the switch if it was flipped
/// before the engine started (`choose_internet`), otherwise `PEERPARTY_INTERNET`.
pub fn internet_wanted() -> bool {
    match INTERNET_CHOICE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => Env::internet(),
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

#[cfg(any(test, feature = "test-util"))]
thread_local! {
    static TEST_INTERVALS: std::cell::Cell<TestIntervals> = const { std::cell::Cell::new(TestIntervals::NONE) };
}

/// Engines `Lan::start` creates on this thread use `intervals` until the guard drops.
/// Other threads, and this thread after the drop, keep the production intervals.
#[cfg(any(test, feature = "test-util"))]
#[doc(hidden)]
pub struct TestTiming {
    prev: TestIntervals,
}

#[cfg(any(test, feature = "test-util"))]
impl TestTiming {
    pub fn install(intervals: TestIntervals) -> Self {
        Self { prev: TEST_INTERVALS.with(|c| c.replace(intervals)) }
    }
}

#[cfg(any(test, feature = "test-util"))]
impl Drop for TestTiming {
    fn drop(&mut self) {
        let prev = self.prev;
        TEST_INTERVALS.with(|c| c.set(prev));
    }
}

#[cfg(any(test, feature = "test-util"))]
fn test_intervals() -> TestIntervals {
    TEST_INTERVALS.with(|c| c.get())
}

/// Release builds: always the production intervals.
#[cfg(not(any(test, feature = "test-util")))]
fn test_intervals() -> TestIntervals {
    TestIntervals::NONE
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
    /// Voice and party chat with public players to begin with (`Lan::set_public_voice`
    /// changes it); `None`: as `PEERPARTY_PUBLIC_VOICE` says (off unless it's set).
    pub public_voice: Option<bool>,
}

impl Options {
    /// Everything from the environment (`PEERPARTY_PEERS`, `PEERPARTY_ROOM` and the other
    /// internet settings, `PEERPARTY_PUBLIC*`, `PEERPARTY_LAN_DISCOVERY`,
    /// `PEERPARTY_PUBLIC_VOICE`), as `Lan::global` starts with. A room code or public
    /// setting that doesn't read is logged and left off.
    pub fn from_env(game: &str, identity: Identity, log: fn(&str)) -> Options {
        Options {
            game: game.into(),
            identity,
            discovery_port: mesh::DISCOVERY_PORT,
            extra_targets: Env::peers(),
            log,
            internet: inet::Config::from_env().unwrap_or_else(|e| {
                log(&format!("inet off: {e}"));
                None
            }),
            public: inet::Config::public_from_env().unwrap_or_else(|e| {
                log(&format!("public sessions off: {e}"));
                None
            }),
            lan_discovery: Env::lan_discovery(),
            public_voice: Some(Env::public_voice()),
        }
    }
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
            m.insert(mesh, TunnelEntry { tunnel: t, down: None, closed: None, relayed: None });
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
            let who = s.peers.iter_mut().find(|(_, p)| p.conn.addr == mesh).map(|(id, p)| {
                p.bye = true;
                *id
            });
            drop(s);
            match who {
                Some(id) => (self.inner.log)(&format!("lan peer {id} left through its tunnel: gone once that connection ends")),
                None => (self.inner.log)("lan: a tunnel's peer left, but no peer is on that tunnel's connection now"),
            }
        }
    }
    fn tunnel_closed(&self, mesh: SocketAddr) {
        // Gone already (`Sink::disconnected`) when the connection ended as it should.
        if let Some(e) = self.inner.tunnels.lock().unwrap().get_mut(&mesh) {
            e.closed.get_or_insert_with(Instant::now);
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
        let identity = if server {
            Identity::claim_server_for(&Identity::default_dir(), game)
        } else {
            Identity::claim_for(&Identity::default_dir(), game)
        };
        let lan = Lan::start_with(Options::from_env(game, identity, log), internet_wanted())?;
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
            wake: Condvar::new(),
            log: opts.log,
            tunnels: Mutex::new(HashMap::new()),
            public_voice: std::sync::atomic::AtomicBool::new(opts.public_voice.unwrap_or_else(Env::public_voice)),
            drop_logged: Mutex::new(HashSet::new()),
            this: OnceLock::new(),
            session: u64::from_le_bytes(PeerId::random().0[..8].try_into().unwrap_or_default()) | 1,
            grace: test_intervals().link_grace.unwrap_or(LINK_GRACE),
            versions: Mutex::new(HashMap::new()),
            identity: Mutex::new(None),
            #[cfg(any(test, feature = "test-util"))]
            severed: Mutex::new(HashSet::new()),
            mesh_port: OnceLock::new(),
            relaying: test_intervals().relay.unwrap_or(true),
            key: opts.identity.key.clone(),
            #[cfg(any(test, feature = "test-util"))]
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

    /// Starts the room and public sessions (whichever are set up). `retry`: switching on
    /// again. A stopped instance waits for its threads (`Inet::stop`) and frees its port
    /// as it is dropped, so the fixed port is normally free at once; it is retried
    /// briefly (RETRY_PORT) in case something still held the old instance a moment
    /// longer (a status read), then any port is taken.
    fn internet_up(&self, retry: bool) {
        let net = &self.net;
        net.on.store(true, std::sync::atomic::Ordering::Relaxed);
        let start = |cfg: &inet::Config, public: bool| -> Option<Arc<inet::Inet>> {
            let what = if public { "public sessions" } else { "inet" };
            let mut cfg = cfg.clone();
            let mut tries = if retry && cfg.port != 0 { RETRY_PORT.0 } else { 0 };
            loop {
                let host = Arc::new(InetHost { inner: self.inner.clone(), mesh: self.mesh.clone(), public });
                match inet::Inet::start(cfg.clone(), host) {
                    Ok(i) => return Some(Arc::new(i)),
                    Err(e) if tries > 0 && e.kind() == io::ErrorKind::AddrInUse => {
                        tries -= 1;
                        thread::sleep(RETRY_PORT.1);
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
    /// longest. Two peers can share a stand-in address (`crate::redact::stand_in`); a
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
    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub fn sever(&self, id: PeerId) {
        self.inner.severed.lock().unwrap().insert(id);
        self.cut_connection(id);
    }

    /// `sever`, but our side doesn't notice the connection is gone: it stays the
    /// peer's connection, as a link that dies without a word can for a while (tests).
    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub fn sever_unnoticed(&self, id: PeerId) {
        self.inner.unnoticed.lock().unwrap().insert(id);
        self.sever(id);
    }

    /// Lets a peer `sever` cut off connect again (tests).
    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub fn mend(&self, id: PeerId) {
        self.inner.severed.lock().unwrap().remove(&id);
        self.inner.unnoticed.lock().unwrap().remove(&id);
    }

    /// Whether we reach a peer only through another player (see `relay`).
    pub fn peer_is_relayed(&self, id: PeerId) -> bool {
        Sink::conn(&*self.inner, id).is_some_and(|c| self.inner.relayed(&c))
    }

    /// Whether a peer's connection runs through an internet tunnel: a room or public
    /// session, or another player who is one. A LAN player reached through another LAN
    /// player (`relay`) is still on the LAN.
    pub fn peer_is_internet(&self, id: PeerId) -> bool {
        Sink::conn(&*self.inner, id).is_some_and(|c| self.inner.internet(&c))
    }

    /// How a peer reaches us (`None`: not connected).
    pub fn peer_trust(&self, id: PeerId) -> Option<Trust> {
        Sink::conn(&*self.inner, id).map(|c| self.inner.trust(&c))
    }

    /// This player's signature over `msg` for `context` (a front end's own name for what
    /// it signs), with the public key it checks against, the key our id is the hash of.
    /// Nothing PeerParty itself signs can be made this way (see `IdentityKey::sign_bytes`).
    pub fn sign_bytes(&self, context: &str, msg: &[u8]) -> ([u8; identity_key::KEY_LEN], [u8; identity_key::SIG_LEN]) {
        (self.inner.key.public(), self.inner.key.sign_bytes(context, msg))
    }

    /// Whether `sig` is `sign_bytes(context, msg)` by the player `claimed`: `key` must be
    /// the key its id is the hash of, and the signature that key's.
    pub fn verify_bytes(claimed: PeerId, key: &[u8; identity_key::KEY_LEN], context: &str, msg: &[u8], sig: &[u8; identity_key::SIG_LEN]) -> bool {
        PeerId::of_key(key) == claimed && identity_key::verify_bytes(key, context, msg, sig)
    }

    /// The proof PeerParty adds to a game's sign-in ticket (Steam's auth tickets): this
    /// player's public key, a fresh number and the key's signature over it
    /// (TICKET_LEN bytes, starting with TICKET_MAGIC). `check_auth_ticket` on another PC
    /// says whose ticket it is: no player can sign in as another.
    pub fn auth_ticket(&self) -> Option<Vec<u8>> {
        let key = self.inner.identity.lock().unwrap().as_ref().map(|i| i.key.clone())?;
        let nonce: [u8; 16] = crate::crypto::random_array();
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
        Some(PeerId::of_key(&key) == claimed && identity_key::ticket_proves(&key, &sig, "", &nonce))
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
        let (starts, gave_up) = self.tick_locked();
        // What waited on a gap given up on goes now, in order.
        for peer in gave_up {
            self.inner.release_gap(peer);
        }
        self.inner.wake.notify_all();
        // Connections through other players, dialed now the state is free.
        for st in starts {
            relay::start(Arc::downgrade(&self.inner), &self.mesh, st);
        }
    }

    /// The engine's tick under the lock: the connections through other players to dial,
    /// and the peers whose gap in their messages was given up on (`Inner::release_gap`).
    fn tick_locked(&self) -> (Vec<relay::Start>, Vec<PeerId>) {
        let now = Instant::now();
        let mut gave_up = Vec::new();
        let mut s = self.inner.state.lock().unwrap();
        s.forget_closed_unread(now);
        for line in s.traffic_report(now) {
            (self.inner.log)(&line);
        }
        relay::drain(&self.inner, &mut s);
        let starts = relay::tick(&self.inner, &mut s, now);
        // Connections a peer moved off and never finished (MOVE_WAIT): retired, and what
        // they held back is delivered.
        let unfinished: Vec<PeerId> = s
            .peers
            .iter_mut()
            .filter_map(|(id, p)| {
                let mut any = false;
                for x in p.prior.iter_mut().filter(|x| !x.done && x.retire_at <= now) {
                    x.done = true;
                    x.conn.retire();
                    any = true;
                }
                any.then_some(*id)
            })
            .collect();
        for id in unfinished {
            (self.inner.log)(&format!("lan peer {id} didn't finish its old connection in {}s: retired it", MOVE_WAIT.as_secs()));
            s.release(self.inner.me, id);
        }
        // Peers whose connection dropped and that didn't come back in time (or had more
        // waiting for them than is kept), and acknowledgements of what came quietly.
        let expired: Vec<PeerId> = s
            .peers
            .iter()
            .filter(|(_, p)| p.gone.is_some_and(|t| t <= now || p.link.overfull()))
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(p) = s.peers.remove(&id) {
                p.conn.close();
                (self.inner.log)(&format!(
                    "lan peer {id} ({}) {}: it left",
                    p.name,
                    if p.link.overfull() {
                        "had more waiting for it than is kept while it was away".to_string()
                    } else {
                        format!("didn't come back within {}s", self.inner.grace.as_secs())
                    }
                ));
            }
            s.peer_lost(self.inner.me, id);
        }
        for (id, p) in &mut s.peers {
            if p.gone.is_some() {
                continue;
            }
            if let Some(n) = p.link.ack_due(now) {
                p.conn.send(&Msg::Ack { recv: n });
            }
            // Messages lost on a connection that was cut while another carried later
            // ones: asked for again, and in the end given up on.
            match p.link.gap_due(now) {
                Some(crate::link::Gap::Ask(from)) => {
                    (self.inner.log)(&format!("lan peer {id} ({}): messages after {from} missing; asking for them again", p.name));
                    p.conn.send(&Msg::Resend { from });
                }
                Some(crate::link::Gap::GaveUp(lost)) => {
                    (self.inner.log)(&format!("lan peer {id} ({}): {lost} message(s) lost for good", p.name));
                    gave_up.push(*id);
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
            // Back meanwhile (a new session): asked whether we're still in its lobby,
            // once. One that restarted in between hasn't got it any more, and says
            // nothing: then it's gone as far as the lobby goes.
            if s.peers.get(&owner).is_some_and(Peer::connected) && s.owners_asked.insert(id.clone()) {
                s.send_to(owner, &Msg::OwnerCheck { lobby: id.clone(), owner });
                s.owner_checks.insert(id, (owner, now + OWNER_CHECK_WAIT));
                continue;
            }
            s.owners_asked.remove(&id);
            s.owner_gone(self.inner.me, id, owner);
        }
        s.send_owed_casts(now);
        // Joins by id not everyone answered: settled with what came.
        let due: Vec<String> =
            s.by_id.iter().filter(|(id, b)| b.until.is_some_and(|u| u <= now) || !s.joins.contains_key(*id)).map(|(id, _)| id.clone()).collect();
        for id in due {
            if let Some((from, state)) = s.settle_by_id(&id) {
                s.handle(self.inner.me, from, state, true);
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
        self.cut_closed_tunnels(&s, now);
        (starts, gave_up)
    }

    /// Mesh connections whose tunnel's bridge closed its end TUNNEL_END_WAIT ago and
    /// that are still open: our end never heard. Cut from our side, which ends them as
    /// the bridge's close should have (the peer leaves, or gets its grace).
    fn cut_closed_tunnels(&self, s: &State, now: Instant) {
        let due: Vec<SocketAddr> = {
            let mut t = self.inner.tunnels.lock().unwrap();
            t.iter_mut()
                .filter(|(_, e)| e.closed.is_some_and(|c| now.duration_since(c) >= TUNNEL_END_WAIT))
                .map(|(a, e)| {
                    e.closed = None;
                    *a
                })
                .collect()
        };
        for mesh in due {
            for (id, p) in &s.peers {
                for c in std::iter::once(&p.conn).chain(p.prior.iter().map(|x| &x.conn)).filter(|c| c.addr == mesh) {
                    (self.inner.log)(&format!(
                        "lan peer {id} ({}): its tunnel closed {}s ago and the connection on it didn't end: cutting it",
                        p.name,
                        TUNNEL_END_WAIT.as_secs()
                    ));
                    c.close();
                }
            }
        }
    }

    pub fn poll(&self) -> Vec<Event> {
        let mut s = self.inner.state.lock().unwrap();
        s.events.drain(..).collect()
    }

    // ---- presence and invites ----

    /// Replaces our presence (rich presence) and tells every peer.
    pub fn set_presence(&self, kv: Vec<(String, String)>) {
        let mut s = self.inner.state.lock().unwrap();
        if s.my_presence == kv {
            return;
        }
        s.my_presence = kv.clone();
        s.post_all(&Msg::Presence { kv });
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
        for (id, p) in &mut s.peers {
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
}

fn replaces_stale(old: &Conn, new: &Conn) -> bool {
    !old.retired() && old.addr.ip() == new.addr.ip() && old.since.elapsed() >= RECONNECT_AFTER
}

impl Inner {
    /// A message from a peer's connection (`Sink::message`), or (`drain`, the peer's
    /// `drain_cut` when it was taken) one `deliver_early` takes off its early queue;
    /// then what that let out of the peer's gap (`release_gap`).
    fn receive(&self, conn: &Arc<Conn>, msg: Msg, ordered: bool, drain: Option<u64>) {
        self.receive_one(conn, msg, ordered, drain);
        if ordered {
            self.release_gap(conn.peer);
        }
    }

    /// The peer's messages held past a gap (`Peer::gap`) that it has now reached (the
    /// gap filled, or was given up on), in order, the lock let go between each
    /// (delivering takes it). One whose connection is gone comes as on the live one:
    /// it's counted, so nothing would send it again.
    fn release_gap(&self, peer: PeerId) {
        let mut n = 0u64;
        loop {
            let (c, m) = {
                let mut s = self.state.lock().unwrap();
                let Some(p) = s.peers.get_mut(&peer) else { return };
                let recv = p.link.recv();
                if p.gap.first_key_value().is_none_or(|(&k, _)| k > recv) {
                    if n > 0 {
                        (self.log)(&format!("lan peer {peer} ({}): the {n} message(s) held for a gap delivered, in order", p.name));
                    }
                    return;
                }
                let (_, (c, m)) = p.gap.pop_first().unwrap();
                p.gap_bytes = p.gap_bytes.saturating_sub(weight(&m));
                let usable = Arc::ptr_eq(&p.conn, &c) || p.prior.iter().any(|x| Arc::ptr_eq(&x.conn, &c)) || c.retired();
                (if usable { c } else { p.conn.clone() }, m)
            };
            n += 1;
            let m = match m {
                Msg::Numbered { msg, .. } => *msg,
                m => m,
            };
            self.receive_one(&c, m, true, None);
        }
    }

    fn receive_one(&self, conn: &Arc<Conn>, msg: Msg, ordered: bool, drain: Option<u64>) {
        let draining = drain.is_some();
        // A number belongs to the ordered stream; a datagram with one is a lie.
        if !ordered && matches!(msg, Msg::Numbered { .. }) {
            return;
        }
        // What a public player sends is bounded and scrubbed first, outside the lock;
        // whether it's then dropped is decided below, under the same lock that counts
        // its number (see `refused`).
        let mut msg = msg;
        let public = self.is_public(conn);
        let refused = if public {
            let body = match &mut msg {
                Msg::Numbered { msg, .. } => &mut **msg,
                m => m,
            };
            self.bound_public(conn, body)
        } else {
            Ok(())
        };
        // One lock from here to delivery: the session (see `link`), what is about the
        // session itself, what waits for the peer's `Msg::Session`, and the numbers of
        // what came, which drop what we already have. A number counted (and
        // acknowledged) is a message delivered, held, or dropped on purpose, never one
        // lost because the peer went, or started waiting, between the two.
        let mut s = self.state.lock().unwrap();
        if ordered {
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
                            p.link.acked(recv);
                        }
                        return;
                    }
                    Msg::Bye => {
                        p.bye = true;
                        return;
                    }
                    &Msg::Resend { from } => {
                        if p.gone.is_none() {
                            let again = p.link.resend(from, Instant::now());
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
                    let (n, now) = (*n, Instant::now());
                    if !p.link.received(n, now) {
                        return;
                    }
                    if p.gone.is_none()
                        && let Some(n) = p.link.ack_due(now)
                    {
                        p.conn.send(&Msg::Ack { recv: n });
                    }
                    if n > p.link.recv() {
                        // Past a gap: held until what's before it comes (`release_gap`).
                        if p.gap.is_empty() {
                            (self.log)(&format!(
                                "lan peer {} ({}): message {n} came before {}: holding it until that comes",
                                conn.peer,
                                p.name,
                                p.link.recv() + 1
                            ));
                        }
                        p.gap_bytes += weight(&msg);
                        p.gap.insert(n, (conn.clone(), msg));
                        if p.gap.len() > MOVE_HOLD_MAX || p.gap_bytes > MOVE_HOLD_BYTES {
                            (self.log)(&format!("lan peer {}: too much waiting on a gap in its messages; delivering without it", conn.peer));
                            p.link.give_up_gap();
                        }
                        return;
                    }
                }
            }
        }
        let msg = match msg {
            Msg::Numbered { msg, .. } => *msg,
            m => m,
        };
        if let Err(why) = refused {
            drop(s);
            if let Some((socket, why)) = why {
                self.note_drop(conn.peer, &socket, why);
            }
            return;
        }
        if public {
            // A channel the stranger doesn't have yet, past what one player may have.
            if let Msg::ChanRequest { socket } | Msg::ChanData { socket, .. } = &msg
                && !s.chans.contains_key(by(&(conn.peer, socket.as_str())))
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
        #[cfg(any(test, feature = "test-util"))]
        if self.severed.lock().unwrap().contains(&conn.peer) && !self.relayed(conn) {
            conn.close();
            return false;
        }
        if self.is_public(conn) {
            conn.scrub_as(crate::redact::stand_in(&self.me.0), scrub_outbound);
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
                p.prior.push_back(Prior { conn: old.clone(), held: Vec::new(), done, retire_at: Instant::now() + MOVE_WAIT });
                p.conn = conn.clone();
                p.stranger = now == Trust::Public;
                p.trust = now;
                let from = if std::mem::replace(&mut p.relayed, self.relayed(conn)) { "another player" } else { describe(was) };
                (self.log)(&format!("lan peer {} ({}) moved from {from} to {}", conn.peer, p.name, self.how(conn)));
                if done {
                    s.release(self.me, conn.peer);
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
            // Neither its live connection nor an old one: nothing changes for the peer.
            (self.log)(&format!("lan peer {}: a connection closed that isn't its live one (that's {})", conn.peer, self.how(&p.conn)));
            return;
        }
        #[cfg(any(test, feature = "test-util"))]
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

impl State {
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

    /// Posts `msg` to every peer (see `Peer::post`).
    fn post_all(&mut self, msg: &Msg) {
        for p in self.peers.values_mut() {
            p.post(msg);
        }
    }

    fn send_to(&mut self, peer: PeerId, msg: &Msg) {
        if let Some(p) = self.peers.get_mut(&peer) {
            p.post(msg);
        }
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
            m @ (Msg::ChanRequest { .. } | Msg::ChanAccept { .. } | Msg::ChanData { .. } | Msg::ChanClose { .. }) => self.handle_chan(from, m, ordered),
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
            m => self.handle_lobby(me, from, m),
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
    fn greet(&mut self, conn: &Conn) {
        let Some(p) = self.peers.get_mut(&conn.peer) else { return };
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

