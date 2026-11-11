//! Messages between copies, and their byte format. Little-endian, length-prefixed
//! strings and lists. Everything a peer sends is untrusted: decoding stops at the first
//! malformed field and the message is dropped.

use crate::id::PeerId;
use crate::lobby::{Attr, Filter, Lobby, Member, Op, Permission, Query, Value};

/// 3: the hello carries the sender's key and is followed by `Proof`; a peer id is its
/// key's hash.
pub const VERSION: u16 = 3;
/// Larger frames are a broken or hostile peer.
pub const MAX_FRAME: usize = 1 << 24;
/// Room set aside for a list before its elements are read, at most. A count only has to
/// be no more than the bytes left in the frame (`R::count`), and an element in memory is
/// many times its smallest form on the wire: a frame that only claims millions of
/// lobbies must not get gigabytes set aside for them (a 32-bit game has no such room,
/// and running out ends the process). The list still grows as far as real elements come.
const PREALLOC: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinResult {
    Ok,
    Full,
    NotFound,
    NotAllowed,
    TimedOut,
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
    },
    JoinReply {
        lobby: String,
        result: JoinResult,
    },
    /// The owner's full snapshot, sent to every member after any change.
    State {
        lobby: Lobby,
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
    /// the new connection. Older copies drop it as malformed.
    Moved,
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
}

impl<'a> R<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        R { b, p: 0 }
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
        (n <= self.b.len() - self.p).then_some(n)
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
        })
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
            Msg::Join { lobby } => {
                w.u8(4);
                w.str(lobby);
            }
            Msg::JoinReply { lobby, result } => {
                w.u8(5);
                w.str(lobby);
                w.u8(*result as u8);
            }
            Msg::State { lobby } => {
                w.u8(6);
                w.lobby(lobby);
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
        }
        w.0
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
            4 => Msg::Join { lobby: r.str()? },
            5 => Msg::JoinReply {
                lobby: r.str()?,
                result: join_result(r.u8()?)?,
            },
            6 => Msg::State { lobby: r.lobby()? },
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
            },
            Msg::State {
                lobby: lobby.clone(),
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
        ];
        for m in all {
            assert_eq!(Msg::decode(&m.encode()), Some(m));
        }
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
        assert_eq!(Beacon::decode(&b.encode()), Some(b));
        assert!(Beacon::decode(b"nope").is_none());
    }
}
