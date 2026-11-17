//! Messages between copies, and their byte format. Little-endian, length-prefixed
//! strings and lists. Everything a peer sends is untrusted: decoding stops at the first
//! malformed field and the message is dropped.

use crate::id::PeerId;
use crate::lobby::{Attr, Claim, Filter, Lobby, Member, Op, Permission, Query, Value};

/// 3: the hello carries the sender's key and is followed by `Proof`; a peer id is its
/// key's hash. 4: a session survives a dropped connection (`Session`, `Ack`, `Bye`).
/// 5: `JoinElsewhere`. 6: a join carries a secret its owner checks; a member that lost
/// only its own link to a lobby's owner asks the others (`OwnerCheck`, `OwnerHere`).
/// 7: unlisted lobbies. 8: session messages carry their numbers (`Numbered`, `Resend`);
/// a connection can run through another player (`Relay`, `RelayEnd`); lobbies carry
/// their owner's signed claim (`Claim`), `State` the lobby's secret key for its members
/// and `JoinReply` the claim.
pub const VERSION: u16 = 8;
/// Larger frames are a broken or hostile peer.
pub const MAX_FRAME: usize = 1 << 24;
/// Room set aside for a list before its elements are read, at most. A count only has to
/// be no more than the bytes left in the frame (`R::count`), and an element in memory is
/// many times its smallest form on the wire: a frame that only claims millions of
/// lobbies must not get gigabytes set aside for them (a 32-bit game has no such room,
/// and running out ends the process). The list still grows as far as real elements come.
const PREALLOC: usize = 256;
/// What a decoded message may take in memory, at most: this many times its frame (plus
/// a little). Each list element is charged ELEMENT_COST on top of its bytes: a frame of
/// tiny attributes or members costs many times its size once decoded, and a peer's
/// frame mustn't become a hundred megabytes of lobbies.
const DECODE_FACTOR: usize = 8;
const ELEMENT_COST: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinResult {
    Ok,
    Full,
    NotFound,
    NotAllowed,
    TimedOut,
    /// The owner's front end refused the secret the join came with (a lobby's password).
    WrongSecret,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    Hello {
        version: u16,
        game: String,
        id: PeerId,
        name: String,
        /// Our data socket's UDP port, for unreliable channel data (0: none).
        udp: u16,
        /// The sender's public key, which `id` is the hash of (`PeerId::of_key`).
        key: [u8; 32],
        /// Made up for this connection: what the other side's `Proof` signs.
        nonce: [u8; 16],
    },
    /// Sent once each way, right after the hellos: the sender's signature
    /// (`IdentityKey::sign_hello`) over both hellos' ids and nonces, which shows it
    /// holds the key its id comes from. Nothing else is taken from a connection before
    /// it.
    Proof {
        sig: [u8; 64],
    },
    Search {
        search: u64,
        query: Query,
    },
    SearchReply {
        search: u64,
        lobbies: Vec<Lobby>,
    },
    Join {
        lobby: String,
        /// What the joiner was given to get in (a lobby's password, an invitation): the
        /// owner's front end checks it (`Lan::set_join_check`). Empty for none.
        secret: String,
    },
    JoinReply {
        lobby: String,
        result: JoinResult,
        /// The answering owner's claim to the lobby (see `Claim`).
        claim: Claim,
    },
    /// The owner's full snapshot, sent to every member after any change, with the
    /// lobby's secret key (see `Claim`; zeros for none).
    State {
        lobby: Lobby,
        secret: [u8; 32],
    },
    Leave {
        lobby: String,
    },
    /// A member's own attributes, sent to the owner.
    MemberAttrs {
        lobby: String,
        attrs: Vec<Attr>,
    },
    Closed {
        lobby: String,
    },
    Kicked {
        lobby: String,
    },
    ChanRequest {
        socket: String,
    },
    ChanAccept {
        socket: String,
    },
    ChanData {
        socket: String,
        channel: u8,
        reliable: bool,
        data: Vec<u8>,
    },
    ChanClose {
        socket: String,
    },
    /// The sender's whole presence (rich presence) set, sent on change and on connect.
    Presence {
        kv: Vec<(String, String)>,
    },
    /// "Join me": an SDK connect string (Steam rich presence `connect`) and/or the
    /// lobby to join, as the sender sees it.
    Invite {
        connect: String,
        lobby: Option<Lobby>,
    },
    /// The last message on a connection the sender moved off (to a better path: LAN
    /// over a room tunnel, a room over public sessions): everything after it comes on
    /// the new connection.
    Moved,
    /// The first message on a connection to a peer we haven't heard this from since it
    /// connected or came back (see `link`): our process's session, the one we hold for
    /// the peer (0: none), and where our side of it stands.
    Session {
        mine: u64,
        yours: u64,
        sent: u64,
        recv: u64,
        first: u64,
    },
    /// How many of the peer's messages we have (see `link`): it needn't keep them.
    Ack {
        recv: u64,
    },
    /// The sender is leaving for good (not a dropped connection): no need to wait for
    /// it to come back.
    Bye,
    /// The answer to a Join sent to a player that doesn't own the lobby (any more): ask
    /// `owner`, which does as far as the sender knows.
    JoinElsewhere {
        lobby: String,
        owner: PeerId,
    },
    /// From a member whose connection to a lobby's owner is gone, to the lobby's other
    /// members: is `owner` still there for you? A member that still reaches it passes
    /// the question on to it; the owner answers with `OwnerHere`.
    OwnerCheck {
        lobby: String,
        owner: PeerId,
    },
    /// The owner still runs the lobby (from the owner to the member that passed the
    /// question on, and from that member to the one that asked): only the asker's own
    /// link to it broke, so the asker is out of the lobby rather than electing a new
    /// owner of its own.
    OwnerHere {
        lobby: String,
    },
    /// A counted message of the sender's session and its number there (see `link`):
    /// every message that counts goes in one of these.
    Numbered {
        n: u64,
        msg: Box<Msg>,
    },
    /// Send again everything you keep past `from`: what came from the sender has a gap
    /// there (see `link`).
    Resend {
        from: u64,
    },
    /// Bytes of a connection between `from` and `to` that runs through the player in
    /// between (see `lan::relay`): from an end to the player in between, and on from
    /// it to the other end. `stream` tells the connections apart.
    Relay {
        stream: u64,
        from: PeerId,
        to: PeerId,
        data: Vec<u8>,
    },
    /// That connection is over (or refused), the same way.
    RelayEnd {
        stream: u64,
        from: PeerId,
        to: PeerId,
    },
}

#[derive(Default)]
pub struct W(pub Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    fn id(&mut self, id: &PeerId) {
        self.0.extend_from_slice(&id.0);
    }
    fn value(&mut self, v: &Value) {
        match v {
            Value::Bool(b) => {
                self.u8(0);
                self.u8(*b as u8);
            }
            Value::Int(i) => {
                self.u8(1);
                self.u64(*i as u64);
            }
            Value::Double(d) => {
                self.u8(2);
                self.u64(d.to_bits());
            }
            Value::Str(s) => {
                self.u8(3);
                self.str(s);
            }
        }
    }
    fn attrs(&mut self, attrs: &[Attr]) {
        self.u32(attrs.len() as u32);
        for a in attrs {
            self.str(&a.key);
            self.value(&a.value);
            self.u8(a.public as u8);
        }
    }
    fn lobby(&mut self, l: &Lobby) {
        self.str(&l.id);
        self.str(&l.namespace);
        self.id(&l.owner);
        self.str(&l.bucket);
        self.u32(l.max_members);
        self.u8(l.permission as u8);
        self.u8(l.allow_invites as u8 | (l.host_migration as u8) << 1 | (l.join_by_id as u8) << 2);
        self.attrs(&l.attrs);
        self.u32(l.members.len() as u32);
        for m in &l.members {
            self.id(&m.id);
            self.attrs(&m.attrs);
        }
        self.claim(&l.claim);
    }
    fn claim(&mut self, c: &Claim) {
        self.0.extend_from_slice(&c.key);
        self.u64(c.term);
        self.0.extend_from_slice(&c.sig);
    }
    fn query(&mut self, q: &Query) {
        self.str(&q.namespace);
        self.str(q.lobby_id.as_deref().unwrap_or(""));
        self.u8(q.member.is_some() as u8);
        self.id(&q.member.unwrap_or_default());
        self.u32(q.max_results);
        self.u32(q.filters.len() as u32);
        for f in &q.filters {
            match f {
                Filter::Attr { key, op, value } => {
                    self.u8(0);
                    self.str(key);
                    self.u8(*op as u8);
                    self.value(value);
                }
                Filter::Bucket { op, value } => {
                    self.u8(1);
                    self.u8(*op as u8);
                    self.str(value);
                }
                Filter::MinMembers(n) => {
                    self.u8(2);
                    self.u64(*n as u64);
                }
                Filter::MinSlots(n) => {
                    self.u8(3);
                    self.u64(*n as u64);
                }
            }
        }
    }
}

pub struct R<'a> {
    b: &'a [u8],
    p: usize,
    /// What list elements may still cost (DECODE_FACTOR).
    budget: usize,
}

impl<'a> R<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        R { b, p: 0, budget: b.len().saturating_mul(DECODE_FACTOR) + 4096 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.p.checked_add(n)?;
        let s = self.b.get(self.p..end)?;
        self.p = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        Some(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?).ok()
    }
    fn id(&mut self) -> Option<PeerId> {
        Some(PeerId(self.take(16)?.try_into().ok()?))
    }
    fn count(&mut self) -> Option<usize> {
        let n = self.u32()? as usize;
        // Every element is at least a byte; a count beyond what's left is a lie.
        if n > self.b.len() - self.p {
            return None;
        }
        // And every element costs something in memory, over the decode's budget.
        self.budget = self.budget.checked_sub(n.saturating_mul(ELEMENT_COST))?;
        Some(n)
    }
    fn value(&mut self) -> Option<Value> {
        Some(match self.u8()? {
            0 => Value::Bool(self.u8()? != 0),
            1 => Value::Int(self.u64()? as i64),
            2 => Value::Double(f64::from_bits(self.u64()?)),
            3 => Value::Str(self.str()?),
            _ => return None,
        })
    }
    fn attrs(&mut self) -> Option<Vec<Attr>> {
        let n = self.count()?;
        let mut out = Vec::with_capacity(n.min(PREALLOC));
        for _ in 0..n {
            out.push(Attr {
                key: self.str()?,
                value: self.value()?,
                public: self.u8()? != 0,
            });
        }
        Some(out)
    }
    fn lobby(&mut self) -> Option<Lobby> {
        let id = self.str()?;
        let namespace = self.str()?;
        let owner = self.id()?;
        let bucket = self.str()?;
        let max_members = self.u32()?;
        let permission = Permission::from_u8(self.u8()?);
        let flags = self.u8()?;
        let attrs = self.attrs()?;
        let n = self.count()?;
        let mut members = Vec::with_capacity(n.min(PREALLOC));
        for _ in 0..n {
            members.push(Member {
                id: self.id()?,
                attrs: self.attrs()?,
            });
        }
        let claim = self.claim()?;
        Some(Lobby {
            id,
            namespace,
            owner,
            bucket,
            max_members,
            permission,
            allow_invites: flags & 1 != 0,
            host_migration: flags & 2 != 0,
            join_by_id: flags & 4 != 0,
            attrs,
            members,
            claim,
        })
    }
    fn claim(&mut self) -> Option<Claim> {
        Some(Claim { key: self.take(32)?.try_into().ok()?, term: self.u64()?, sig: self.take(64)?.try_into().ok()? })
    }
    fn op(&mut self) -> Option<Op> {
        Some(match self.u8()? {
            0 => Op::Eq,
            1 => Op::Ne,
            2 => Op::Gt,
            3 => Op::Ge,
            4 => Op::Lt,
            5 => Op::Le,
            6 => Op::Distance,
            7 => Op::AnyOf,
            8 => Op::NotAnyOf,
            9 => Op::Contains,
            _ => return None,
        })
    }
    fn query(&mut self) -> Option<Query> {
        let namespace = self.str()?;
        let lobby_id = Some(self.str()?).filter(|s| !s.is_empty());
        let has_member = self.u8()? != 0;
        let member = self.id()?;
        let max_results = self.u32()?;
        let n = self.count()?;
        let mut filters = Vec::with_capacity(n.min(PREALLOC));
        for _ in 0..n {
            filters.push(match self.u8()? {
                0 => Filter::Attr {
                    key: self.str()?,
                    op: self.op()?,
                    value: self.value()?,
                },
                1 => Filter::Bucket {
                    op: self.op()?,
                    value: self.str()?,
                },
                2 => Filter::MinMembers(self.u64()? as i64),
                3 => Filter::MinSlots(self.u64()? as i64),
                _ => return None,
            });
        }
        Some(Query {
            namespace,
            lobby_id,
            member: has_member.then_some(member),
            filters,
            max_results,
        })
    }
}

fn join_result(v: u8) -> Option<JoinResult> {
    Some(match v {
        0 => JoinResult::Ok,
        1 => JoinResult::Full,
        2 => JoinResult::NotFound,
        3 => JoinResult::NotAllowed,
        4 => JoinResult::TimedOut,
        5 => JoinResult::WrongSecret,
        _ => return None,
    })
}

impl Msg {
    pub fn encode(&self) -> Vec<u8> {
        self.encode_onto(Vec::new())
    }

    /// `encode`, appended to `buf` (a header the caller already wrote), so a datagram is
    /// built in one buffer.
    pub fn encode_onto(&self, buf: Vec<u8>) -> Vec<u8> {
        let mut w = W(buf);
        match self {
            Msg::Hello {
                version,
                game,
                id,
                name,
                udp,
                key,
                nonce,
            } => {
                w.u8(1);
                w.u16(*version);
                w.str(game);
                w.id(id);
                w.str(name);
                w.u16(*udp);
                w.0.extend_from_slice(key);
                w.0.extend_from_slice(nonce);
            }
            Msg::Proof { sig } => {
                w.u8(18);
                w.0.extend_from_slice(sig);
            }
            Msg::Search { search, query } => {
                w.u8(2);
                w.u64(*search);
                w.query(query);
            }
            Msg::SearchReply { search, lobbies } => {
                w.u8(3);
                w.u64(*search);
                w.u32(lobbies.len() as u32);
                for l in lobbies {
                    w.lobby(l);
                }
            }
            Msg::Join { lobby, secret } => {
                w.u8(4);
                w.str(lobby);
                w.str(secret);
            }
            Msg::JoinReply { lobby, result, claim } => {
                w.u8(5);
                w.str(lobby);
                w.u8(*result as u8);
                w.claim(claim);
            }
            Msg::State { lobby, secret } => {
                w.u8(6);
                w.lobby(lobby);
                w.0.extend_from_slice(secret);
            }
            Msg::Leave { lobby } => {
                w.u8(7);
                w.str(lobby);
            }
            Msg::MemberAttrs { lobby, attrs } => {
                w.u8(8);
                w.str(lobby);
                w.attrs(attrs);
            }
            Msg::Closed { lobby } => {
                w.u8(9);
                w.str(lobby);
            }
            Msg::Kicked { lobby } => {
                w.u8(10);
                w.str(lobby);
            }
            Msg::ChanRequest { socket } => {
                w.u8(11);
                w.str(socket);
            }
            Msg::ChanAccept { socket } => {
                w.u8(12);
                w.str(socket);
            }
            Msg::ChanData {
                socket,
                channel,
                reliable,
                data,
            } => {
                w.u8(13);
                w.str(socket);
                w.u8(*channel);
                w.u8(*reliable as u8);
                w.bytes(data);
            }
            Msg::ChanClose { socket } => {
                w.u8(14);
                w.str(socket);
            }
            Msg::Presence { kv } => {
                w.u8(15);
                w.u32(kv.len() as u32);
                for (k, v) in kv {
                    w.str(k);
                    w.str(v);
                }
            }
            Msg::Invite { connect, lobby } => {
                w.u8(16);
                w.str(connect);
                w.u8(lobby.is_some() as u8);
                if let Some(l) = lobby {
                    w.lobby(l);
                }
            }
            Msg::Moved => w.u8(17),
            Msg::Session { mine, yours, sent, recv, first } => {
                w.u8(19);
                for v in [mine, yours, sent, recv, first] {
                    w.u64(*v);
                }
            }
            Msg::Ack { recv } => {
                w.u8(20);
                w.u64(*recv);
            }
            Msg::Bye => w.u8(21),
            Msg::JoinElsewhere { lobby, owner } => {
                w.u8(22);
                w.str(lobby);
                w.id(owner);
            }
            Msg::OwnerCheck { lobby, owner } => {
                w.u8(23);
                w.str(lobby);
                w.id(owner);
            }
            Msg::OwnerHere { lobby } => {
                w.u8(24);
                w.str(lobby);
            }
            Msg::Numbered { n, msg } => {
                w.u8(25);
                w.u64(*n);
                let head = std::mem::take(&mut w.0);
                w.0 = msg.encode_onto(head);
            }
            Msg::Resend { from } => {
                w.u8(26);
                w.u64(*from);
            }
            Msg::Relay { stream, from, to, data } => {
                w.u8(27);
                w.u64(*stream);
                w.id(from);
                w.id(to);
                w.bytes(data);
            }
            Msg::RelayEnd { stream, from, to } => {
                w.u8(28);
                w.u64(*stream);
                w.id(from);
                w.id(to);
            }
        }
        w.0
    }

    /// What goes before a message sent as number `n` of a session (`Msg::Numbered`), so
    /// the message itself is encoded straight after it, not cloned into one.
    pub fn numbered_head(n: u64) -> Vec<u8> {
        let mut h = Vec::with_capacity(9);
        h.push(25);
        h.extend_from_slice(&n.to_le_bytes());
        h
    }

    pub fn decode(b: &[u8]) -> Option<Msg> {
        let mut r = R::new(b);
        let msg = match r.u8()? {
            1 => Msg::Hello {
                version: r.u16()?,
                game: r.str()?,
                id: r.id()?,
                name: r.str()?,
                udp: r.u16()?,
                key: r.take(32)?.try_into().ok()?,
                nonce: r.take(16)?.try_into().ok()?,
            },
            18 => Msg::Proof { sig: r.take(64)?.try_into().ok()? },
            2 => Msg::Search {
                search: r.u64()?,
                query: r.query()?,
            },
            3 => {
                let search = r.u64()?;
                let n = r.count()?;
                let mut lobbies = Vec::with_capacity(n.min(PREALLOC));
                for _ in 0..n {
                    lobbies.push(r.lobby()?);
                }
                Msg::SearchReply { search, lobbies }
            }
            4 => Msg::Join { lobby: r.str()?, secret: r.str()? },
            5 => Msg::JoinReply {
                lobby: r.str()?,
                result: join_result(r.u8()?)?,
                claim: r.claim()?,
            },
            6 => Msg::State { lobby: r.lobby()?, secret: r.take(32)?.try_into().ok()? },
            7 => Msg::Leave { lobby: r.str()? },
            8 => Msg::MemberAttrs {
                lobby: r.str()?,
                attrs: r.attrs()?,
            },
            9 => Msg::Closed { lobby: r.str()? },
            10 => Msg::Kicked { lobby: r.str()? },
            11 => Msg::ChanRequest { socket: r.str()? },
            12 => Msg::ChanAccept { socket: r.str()? },
            13 => Msg::ChanData {
                socket: r.str()?,
                channel: r.u8()?,
                reliable: r.u8()? != 0,
                data: r.bytes()?,
            },
            14 => Msg::ChanClose { socket: r.str()? },
            15 => {
                let n = r.count()?;
                let mut kv = Vec::with_capacity(n.min(PREALLOC));
                for _ in 0..n {
                    kv.push((r.str()?, r.str()?));
                }
                Msg::Presence { kv }
            }
            16 => {
                let connect = r.str()?;
                let lobby = if r.u8()? != 0 { Some(r.lobby()?) } else { None };
                Msg::Invite { connect, lobby }
            }
            17 => Msg::Moved,
            19 => Msg::Session { mine: r.u64()?, yours: r.u64()?, sent: r.u64()?, recv: r.u64()?, first: r.u64()? },
            20 => Msg::Ack { recv: r.u64()? },
            21 => Msg::Bye,
            22 => Msg::JoinElsewhere { lobby: r.str()?, owner: r.id()? },
            23 => Msg::OwnerCheck { lobby: r.str()?, owner: r.id()? },
            24 => Msg::OwnerHere { lobby: r.str()? },
            25 => {
                let n = r.u64()?;
                // Exactly one message that counts: never a number around a number. That is
                // checked before decoding what is inside, so a frame of numbers nested a
                // million deep is turned away at once instead of recursing off the stack.
                let inner = &b[r.p..];
                if inner.first() == Some(&25) {
                    return None;
                }
                let msg = Msg::decode(inner)?;
                if !crate::link::counted(&msg) {
                    return None;
                }
                return Some(Msg::Numbered { n, msg: Box::new(msg) });
            }
            26 => Msg::Resend { from: r.u64()? },
            27 => Msg::Relay { stream: r.u64()?, from: r.id()?, to: r.id()?, data: r.bytes()? },
            28 => Msg::RelayEnd { stream: r.u64()?, from: r.id()?, to: r.id()? },
            _ => return None,
        };
        (r.p == b.len()).then_some(msg)
    }
}

/// The discovery beacon: who is here, for which game, and where to connect.
#[derive(Clone, Debug, PartialEq)]
pub struct Beacon {
    pub game: String,
    pub id: PeerId,
    pub port: u16,
}

const BEACON_MAGIC: &[u8; 4] = b"PPR1";

impl Beacon {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W::default();
        w.0.extend_from_slice(BEACON_MAGIC);
        w.u16(VERSION);
        w.str(&self.game);
        w.id(&self.id);
        w.u16(self.port);
        w.0
    }

    /// A beacon's wire version, game and id, whatever its version: copies on other
    /// versions are told apart from no copy at all (`Sink::other_version`). Every version
    /// so far starts the same way.
    pub fn peek(b: &[u8]) -> Option<(u16, String, PeerId)> {
        if b.get(..4)? != BEACON_MAGIC {
            return None;
        }
        let mut r = R::new(&b[4..]);
        Some((r.u16()?, r.str()?, r.id()?))
    }

    pub fn decode(b: &[u8]) -> Option<Beacon> {
        if b.get(..4)? != BEACON_MAGIC {
            return None;
        }
        let mut r = R::new(&b[4..]);
        if r.u16()? != VERSION {
            return None;
        }
        Some(Beacon {
            game: r.str()?,
            id: r.id()?,
            port: r.u16()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_round_trips() {
        let mut lobby = Lobby::new("eos.lobby");
        lobby.id = "x".into();
        lobby.members.push(Member {
            id: PeerId([7; 16]),
            attrs: vec![Attr {
                key: "k".into(),
                value: Value::Double(1.5),
                public: false,
            }],
        });
        let mut query = Query::new("eos.lobby");
        query.member = Some(PeerId([3; 16]));
        query.filters = vec![
            Filter::Bucket {
                op: Op::Eq,
                value: "b".into(),
            },
            Filter::MinSlots(2),
            Filter::Attr {
                key: "m".into(),
                op: Op::Contains,
                value: Value::Str("v".into()),
            },
        ];
        let all = [
            Msg::Hello {
                version: VERSION,
                game: "g".into(),
                id: PeerId([1; 16]),
                name: "n".into(),
                udp: 50000,
                key: [7; 32],
                nonce: [8; 16],
            },
            Msg::Proof { sig: [9; 64] },
            Msg::Search { search: 9, query },
            Msg::SearchReply {
                search: 9,
                lobbies: vec![lobby.clone()],
            },
            Msg::JoinReply {
                lobby: "x".into(),
                result: JoinResult::Full,
                claim: Claim::make(&[3; 32], "x", PeerId([1; 16]), 2),
            },
            Msg::State {
                lobby: lobby.clone(),
                secret: [5; 32],
            },
            Msg::ChanData {
                socket: "s".into(),
                channel: 3,
                reliable: true,
                data: vec![1, 2, 3],
            },
            Msg::Presence {
                kv: vec![
                    ("connect".into(), "abc".into()),
                    ("status".into(), "".into()),
                ],
            },
            Msg::Invite {
                connect: "abc".into(),
                lobby: Some(lobby),
            },
            Msg::Invite {
                connect: String::new(),
                lobby: None,
            },
            Msg::Moved,
            Msg::Session { mine: 1, yours: 2, sent: 3, recv: 4, first: 5 },
            Msg::Ack { recv: u64::MAX },
            Msg::Bye,
            Msg::JoinElsewhere { lobby: "x".into(), owner: PeerId([9; 16]) },
            Msg::OwnerCheck { lobby: "x".into(), owner: PeerId([8; 16]) },
            Msg::OwnerHere { lobby: "x".into() },
            Msg::Numbered { n: 7, msg: Box::new(Msg::Leave { lobby: "x".into() }) },
            Msg::Resend { from: 9 },
            Msg::Relay { stream: 3, from: PeerId([1; 16]), to: PeerId([2; 16]), data: vec![4, 5] },
            Msg::RelayEnd { stream: 3, from: PeerId([1; 16]), to: PeerId([2; 16]) },
        ];
        for m in all {
            assert_eq!(Msg::decode(&m.encode()), Some(m));
        }
    }

    /// A number goes only around one message that counts: not around another number,
    /// nor around what is about the stream itself.
    #[test]
    fn numbers_wrap_one_counted_message() {
        let wrap = |inner: &Msg| {
            let mut b = Msg::numbered_head(4);
            b.extend_from_slice(&inner.encode());
            b
        };
        let leave = Msg::Leave { lobby: "x".into() };
        assert_eq!(Msg::decode(&wrap(&leave)), Some(Msg::Numbered { n: 4, msg: Box::new(leave.clone()) }));
        assert_eq!(wrap(&leave), Msg::Numbered { n: 4, msg: Box::new(leave.clone()) }.encode());
        for not in [Msg::Ack { recv: 1 }, Msg::Bye, Msg::Numbered { n: 1, msg: Box::new(leave) }] {
            assert!(Msg::decode(&wrap(&not)).is_none(), "{not:?}");
        }
        // Numbers nested as deep as a stranger's largest frame allows are refused, not
        // decoded one level at a time until the stack runs out.
        let deep = Msg::numbered_head(1).repeat(2 * 1024 * 1024 / 9);
        assert!(Msg::decode(&deep).is_none());
    }

    #[test]
    fn a_claim_holds_only_for_its_lobby_owner_and_term() {
        let secret = crate::lobby::new_lobby_secret();
        let c = Claim::make(&secret, "lobby", PeerId([1; 16]), 3);
        assert_eq!(c.key, crate::lobby::lobby_key(&secret));
        assert!(c.holds("lobby", PeerId([1; 16])));
        assert!(!c.holds("lobby", PeerId([2; 16])));
        assert!(!c.holds("other", PeerId([1; 16])));
        assert!(!Claim { term: 4, ..c }.holds("lobby", PeerId([1; 16])));
        assert!(!Claim::default().holds("lobby", PeerId::default()));
    }

    #[test]
    fn truncated_or_padded_messages_are_dropped() {
        let m = Msg::Leave {
            lobby: "abc".into(),
        }
        .encode();
        assert!(Msg::decode(&m[..m.len() - 1]).is_none());
        let mut padded = m.clone();
        padded.push(0);
        assert!(Msg::decode(&padded).is_none());
        // A count far larger than the frame.
        assert!(Msg::decode(&[3, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0x7f]).is_none());
    }

    /// A list longer than the room set aside up front still decodes whole.
    #[test]
    fn long_lists_outgrow_the_room_set_aside() {
        let n = PREALLOC * 4 + 3;
        let kv: Vec<(String, String)> = (0..n).map(|i| (format!("k{i}"), "v".into())).collect();
        let m = Msg::Presence { kv };
        assert_eq!(Msg::decode(&m.encode()), Some(m));
        let mut lobby = Lobby::new("eos.lobby");
        lobby.attrs = (0..n).map(|i| Attr { key: format!("a{i}"), value: Value::Int(i as i64), public: true }).collect();
        lobby.members = (0..n).map(|i| Member { id: PeerId([i as u8; 16]), attrs: Vec::new() }).collect();
        let m = Msg::SearchReply { search: 1, lobbies: vec![lobby; 3] };
        assert_eq!(Msg::decode(&m.encode()), Some(m));
    }

    /// A frame packed with the smallest elements there are (an attribute with no key and
    /// a bool is 7 bytes, and far more once decoded) is refused once that would pass the
    /// decode budget; a few of them are fine.
    #[test]
    fn tiny_elements_cant_blow_up_in_memory() {
        let tiny = |n: usize| Msg::MemberAttrs { lobby: "x".into(), attrs: (0..n).map(|_| Attr { key: String::new(), value: Value::Bool(true), public: true }).collect() };
        assert!(Msg::decode(&tiny(100).encode()).is_some());
        assert!(Msg::decode(&tiny(200_000).encode()).is_none());
    }

    /// A frame that claims as many elements as it has bytes, with none of them there, is
    /// dropped without room for them being set aside first (16 million lobbies would be
    /// gigabytes: more than a 32-bit game has).
    #[test]
    fn a_count_without_its_elements_costs_nothing() {
        for kind in [3u8, 15] {
            let mut frame = vec![kind];
            if kind == 3 {
                frame.extend_from_slice(&0u64.to_le_bytes());
            }
            let rest = MAX_FRAME - frame.len() - 4;
            frame.extend_from_slice(&(rest as u32).to_le_bytes());
            frame.resize(frame.len() + rest, 0xff);
            assert_eq!(frame.len(), MAX_FRAME);
            assert!(Msg::decode(&frame).is_none());
        }
    }

    #[test]
    fn beacon_round_trips() {
        let b = Beacon {
            game: "eos:abc".into(),
            id: PeerId([4; 16]),
            port: 5000,
        };
        assert_eq!(Beacon::peek(&b.encode()), Some((VERSION, b.game.clone(), b.id)));
        assert_eq!(Beacon::decode(&b.encode()), Some(b.clone()));
        assert!(Beacon::decode(b"nope").is_none());
        // Another version's: not a beacon to answer, but seen for what it is.
        let mut other = b.encode();
        other[4..6].copy_from_slice(&(VERSION - 1).to_le_bytes());
        assert!(Beacon::decode(&other).is_none());
        assert_eq!(Beacon::peek(&other), Some((VERSION - 1, b.game, b.id)));
    }
}
