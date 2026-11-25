//! How far a peer is trusted (`Trust`), how good its path is (`rank`), and what a public
//! player may send and is sent: rations, caps on what reaches the game (`cap_public`), and
//! addresses replaced with stand-ins on the way in and out (`scrub_public`).

use super::*;

/// A token bucket: `burst` at once, then one more every `every`.
pub(super) struct Ration {
    pub(super) tokens: f32,
    pub(super) at: Instant,
}

impl Ration {
    pub(super) fn new(burst: f32, now: Instant) -> Ration {
        Ration { tokens: burst, at: now }
    }

    /// Takes one if there's one to take.
    pub(super) fn take(&mut self, now: Instant, burst: f32, every: Duration) -> bool {
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

/// How good a path is: a better one replaces a worse one without the peer leaving.
/// Peers ordered as `Lan::peers_by_standing` says: most trusted first, then longest here.
pub(super) fn by_standing(peers: impl Iterator<Item = (Trust, bool, Instant, PeerId)>) -> Vec<PeerId> {
    let mut v: Vec<_> = peers.map(|(t, relayed, joined, id)| (std::cmp::Reverse(rank(t, relayed)), joined, id)).collect();
    v.sort();
    v.into_iter().map(|(_, _, id)| id).collect()
}

/// How good a path is: a better one replaces a worse one without the peer leaving. A
/// connection through another player (`relay`) is below every direct one but a public
/// session, and is trusted as the worse of its two halves.
pub(super) fn rank(t: Trust, relayed: bool) -> u8 {
    match (t, relayed) {
        (Trust::Lan, false) => 4,
        (Trust::Room, false) => 3,
        (Trust::Lan | Trust::Room, true) => 2,
        (Trust::Public, false) => 1,
        (Trust::Public, true) => 0,
    }
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
pub(super) fn voice_socket(socket: &str) -> bool {
    ChannelKind::of(socket) == ChannelKind::Voice
}

/// What a channel carries, as far as the engine treats channels apart: voice (and party
/// chat) or anything else. Voice is what public players don't get unless voice with them
/// is turned on (`Lan::set_public_voice`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    Data,
    Voice,
}

impl ChannelKind {
    /// The socket-name prefix that marks a voice channel (voice.rs's rooms).
    pub const VOICE_PREFIX: &'static str = "rtc:";

    /// What the channel on `socket` carries.
    pub fn of(socket: &str) -> ChannelKind {
        if socket.starts_with(Self::VOICE_PREFIX) { ChannelKind::Voice } else { ChannelKind::Data }
    }

    /// A voice channel's socket name for `room`.
    pub fn voice_socket(room: &str) -> String {
        format!("{}{room}", Self::VOICE_PREFIX)
    }
}

pub(super) fn msg_socket(m: &Msg) -> Option<&str> {
    match m {
        Msg::ChanRequest { socket } | Msg::ChanAccept { socket } | Msg::ChanClose { socket } => Some(socket),
        Msg::ChanData { socket, .. } => Some(socket),
        _ => None,
    }
}

/// What a public player's presence may hold.
pub(super) const PUBLIC_PRESENCE_KEYS: usize = 32;
pub(super) const PUBLIC_PRESENCE_BYTES: usize = 4096;

pub(super) fn cap_presence(kv: &mut Vec<(String, String)>) {
    kv.truncate(PUBLIC_PRESENCE_KEYS);
    let mut total = 0;
    kv.retain(|(k, v)| {
        total += k.len() + v.len();
        total <= PUBLIC_PRESENCE_BYTES
    });
}

/// What a public player's lobbies may hold (EOS's own limits are lower: 64 attributes,
/// 1000-character values), so a stranger can't hand the game oversized or endless data.
pub(super) const PUBLIC_ATTRS: usize = 100;
pub(super) const PUBLIC_KEY_BYTES: usize = 256;
pub(super) const PUBLIC_VALUE_BYTES: usize = 1000;
pub(super) const PUBLIC_MEMBERS: usize = 64;
pub(super) const PUBLIC_ID_BYTES: usize = 256;
pub(super) const PUBLIC_CONNECT_BYTES: usize = 1024;
/// Channels a public player may have with us at once (each stays until it's closed or
/// the player leaves, and each new one is an event for the game), and how long a
/// channel's name may be. The names are PeerParty's own ("steam:...", "party:<guid>",
/// "peerparty-ip:t:<port>:<n>"): a few dozen bytes, a handful of channels.
pub(super) const PUBLIC_CHANNELS: usize = 256;
pub(super) const PUBLIC_SOCKET_BYTES: usize = 256;
/// Invites (and requests to join, and room codes) a public player may send: this many at
/// once, then one every `PUBLIC_INVITE_EVERY`. Each one waits in the overlay, tells the
/// game and is logged, so a stranger doesn't get to send them without end.
/// A public player asking to join a lobby that isn't public may be a moment ahead of
/// the news that puts them in a lobby with us (it comes from that lobby's owner, by
/// another path). Their request waits this long for it, and at most this many wait.
pub(super) const JOIN_WAIT: Duration = Duration::from_secs(2);
/// A join by id asks every peer, and any of them can answer as the owner (`settle`):
/// once all have answered, or this long after the first signed answer, the join is
/// settled with what came (longer than JOIN_WAIT, which an owner may take to answer).
pub(super) const JOIN_CLAIM_WAIT: Duration = Duration::from_secs(3);
pub(super) const JOINS_WAITING: usize = 64;
/// How long our invite to a public player stands (see `State::known`).
pub(super) const INVITE_STANDS: Duration = Duration::from_secs(15 * 60);
pub(super) const PUBLIC_INVITE_BURST: f32 = 5.0;
pub(super) const PUBLIC_INVITE_EVERY: Duration = Duration::from_secs(4);
/// A public player's presence changes: a few at once, then one every this long. Each
/// one is an event every front end handles (Steam's persona callbacks and the like).
pub(super) const PUBLIC_PRESENCE_BURST: f32 = 8.0;
pub(super) const PUBLIC_PRESENCE_EVERY: Duration = Duration::from_millis(500);

/// Drops attributes that are too big and keeps at most `PUBLIC_ATTRS`.
pub(super) fn cap_attrs(attrs: &mut Vec<Attr>) {
    attrs.retain(|a| {
        a.key.len() <= PUBLIC_KEY_BYTES
            && match &a.value {
                crate::lobby::Value::Str(s) => s.len() <= PUBLIC_VALUE_BYTES,
                crate::lobby::Value::Double(d) => d.is_finite(),
                _ => true,
            }
    });
    attrs.truncate(PUBLIC_ATTRS);
}

/// A public player's lobby made safe to hand the game, or `false` to drop it (names too
/// long to be real, or more members than a lobby can hold).
pub(super) fn cap_lobby(l: &mut Lobby) -> bool {
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
pub(super) fn may_carry_addresses(msg: &Msg) -> bool {
    match msg {
        Msg::Numbered { msg, .. } => may_carry_addresses(msg),
        m => matches!(m, Msg::Presence { .. } | Msg::SearchReply { .. } | Msg::State { .. } | Msg::MemberAttrs { .. } | Msg::Invite { .. }),
    }
}

pub(super) fn scrub_attrs(attrs: &mut [Attr], with: std::net::Ipv4Addr) {
    for a in attrs {
        match &mut a.value {
            crate::lobby::Value::Str(s) => {
                if let Some(clean) = crate::redact::scrub(&a.key, s, with) {
                    *s = clean;
                }
            }
            // An address held as a number, under a key that names one.
            crate::lobby::Value::Int(n) => {
                if let Some(clean) = crate::redact::scrub_int(&a.key, *n, with) {
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
pub(super) fn scrub_lobby(l: &mut Lobby, with: std::net::Ipv4Addr) {
    scrub_attrs(&mut l.attrs, with);
    for m in &mut l.members {
        let own = if m.id == PeerId::default() { with } else { crate::redact::stand_in(&m.id.0) };
        scrub_attrs(&mut m.attrs, own);
    }
}

/// Replaces the IP addresses in what a public session carries to or from the game
/// (lobby, member and presence attributes, invite connect strings) with `with`: our own
/// stand-in on the way out, so no game ever tells a stranger our real or LAN address,
/// and the sender's on the way in, so a game that connects to an address it read goes
/// through the tunnel (`fwd`) and never straight to an address a stranger chose.
/// Stand-ins already in the data are kept (a lobby lists its other members' own).
/// The outgoing policy for a public player's connection (`Conn::scrub_as`): a scrubbed
/// copy of what may carry addresses.
pub(super) fn scrub_outbound(msg: &Msg, with: std::net::Ipv4Addr) -> Option<Msg> {
    may_carry_addresses(msg).then(|| {
        let mut m = msg.clone();
        scrub_public(&mut m, with);
        m
    })
}

pub(super) fn scrub_public(msg: &mut Msg, with: std::net::Ipv4Addr) {
    match msg {
        Msg::Presence { kv } => {
            for (k, v) in kv {
                if let Some(clean) = crate::redact::scrub(k, v, with) {
                    *v = clean;
                }
            }
        }
        Msg::SearchReply { lobbies, .. } => lobbies.iter_mut().for_each(|l| scrub_lobby(l, with)),
        Msg::State { lobby, .. } => scrub_lobby(lobby, with),
        Msg::Numbered { msg, .. } => scrub_public(msg, with),
        Msg::MemberAttrs { attrs, .. } => scrub_attrs(attrs, with),
        Msg::Invite { connect, lobby } => {
            if let Some(clean) = crate::redact::scrub("connect", connect, with) {
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
pub(super) fn plain_connect(connect: &str) -> bool {
    let commands = connect.split_whitespace().filter(|w| w.starts_with('+') || w.starts_with('-')).count();
    commands <= 1 && !connect.chars().any(|c| c.is_control() || matches!(c, ';' | '"' | '`' | '|' | '&'))
}

pub(super) fn cap_public(msg: &mut Msg) -> bool {
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
    /// What a public player sends made safe to take (see `cap_public`, `scrub_public`),
    /// or `Err` to drop it, with what to log (`note_drop`) if anything: voice and party
    /// chat unless that's turned on, channel names too long to be ours, and what
    /// `cap_public` refuses. Takes no lock of the state's (`receive` holds none).
    pub(super) fn bound_public(&self, conn: &Conn, msg: &mut Msg) -> Result<(), Option<(String, &'static str)>> {
        if !self.public_voice.load(std::sync::atomic::Ordering::Relaxed)
            && let Some(socket) = msg_socket(msg).filter(|s| voice_socket(s))
        {
            return Err(Some((socket.to_string(), "voice from a public player ignored: voice chat with public players is off")));
        }
        if msg_socket(msg).is_some_and(|s| s.len() > PUBLIC_SOCKET_BYTES) {
            return Err(Some(("a channel".to_string(), "ignored: its name is too long to be one of ours")));
        }
        if !cap_public(msg) {
            return Err(None);
        }
        // The stand-in is an HMAC: worked out only for what can carry an address, not
        // for every packet of game data.
        if may_carry_addresses(msg) {
            scrub_public(msg, crate::redact::stand_in(&conn.peer.0));
        }
        Ok(())
    }
}

/// Whether a new connection from a peer replaces our old one to it outright (see
/// RECONNECT_AFTER). Only from the same address: someone else claiming the id gets no
/// further than the tie-break.
pub(super) fn describe(t: Trust) -> &'static str {
    match t {
        Trust::Lan => "the LAN",
        Trust::Room => "a room tunnel",
        Trust::Public => "a public session",
    }
}

