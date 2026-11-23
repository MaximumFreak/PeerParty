//! Demonware matchmaking sessions (bdMatchMaking, service 21), shared between the
//! copies of the game on the LAN through PeerParty's presence.
//!
//! A session's info is bdMatchMakingInfo followed by the game's own fields. The client
//! sends it (to create or update a session) as the host's address (a blob), two u32s,
//! then the game's fields; it reads it back (find results) as the address, the session
//! id (an 8-byte blob), three u32s (the two sent and the player count), then the same
//! game fields. So a session is kept as those three parts and an id.
//!
//! Tasks: 1 create (answers the new id), 2 and 12 update (the id first), 3 delete (the
//! id), 5 find (a query; every LAN session of this title is an answer), 14 find the
//! sessions of some players (u64 ids; a session whose game fields hold one of them is
//! an answer, all of them if none does, since on a LAN the one being joined is there).

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::log;

use super::hex;
use super::lobby::typed;

/// A typed-encoding reader (lobby.rs `typed`): each value after its type byte.
pub struct Reader<'a> {
    b: &'a [u8],
    pub at: usize,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const STRING: u8 = 0x10;
pub(crate) const BLOB: u8 = 0x13;

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Reader { b, at: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }

    fn tag(&mut self, t: u8) -> Option<()> {
        (self.take(1)?[0] == t).then_some(())
    }

    pub fn u32(&mut self) -> Option<u32> {
        self.tag(typed::UINT32)?;
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    pub fn u64(&mut self) -> Option<u64> {
        self.tag(typed::UINT64)?;
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    pub fn blob(&mut self) -> Option<&'a [u8]> {
        self.tag(BLOB)?;
        let n = self.u32()? as usize;
        self.take(n)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn string(&mut self) -> Option<&'a [u8]> {
        self.tag(STRING)?;
        let rest = self.b.get(self.at..)?;
        let n = rest.iter().position(|c| *c == 0)?;
        self.at += n + 1;
        Some(&rest[..n])
    }

    pub fn rest(&self) -> &'a [u8] {
        self.b.get(self.at..).unwrap_or_default()
    }
}

pub fn blob(out: &mut Vec<u8>, v: &[u8]) {
    out.push(BLOB);
    typed::u32(out, v.len() as u32);
    out.extend_from_slice(v);
}

/// A session: its id and info (see the module comment).
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub id: [u8; 8],
    /// The host's address, as the blob's bytes.
    pub addr: Vec<u8>,
    /// The two u32s after the address, typed as sent (10 bytes).
    pub head: Vec<u8>,
    /// The game's own fields, typed as sent.
    pub game: Vec<u8>,
}

impl Session {
    /// The info a client sent (address, two u32s, the game's fields).
    pub fn from_info(id: [u8; 8], info: &[u8]) -> Option<Session> {
        let mut r = Reader::new(info);
        let addr = r.blob()?.to_vec();
        let start = r.at;
        r.u32()?;
        r.u32()?;
        let head = info[start..r.at].to_vec();
        Some(Session { id, addr, head, game: r.rest().to_vec() })
    }

    /// The find result: address, id, the two u32s, the player count, the game's fields.
    pub fn result(&self) -> Vec<u8> {
        let mut out = Vec::new();
        blob(&mut out, &self.addr);
        blob(&mut out, &self.id);
        out.extend_from_slice(&self.head);
        typed::u32(&mut out, 1);
        out.extend_from_slice(&self.game);
        out
    }

    /// Every typed u64 in the game's fields (the players' ids, for task 14).
    pub fn ids(&self) -> Vec<u64> {
        let g = &self.game;
        (0..g.len().saturating_sub(8))
            .filter(|i| g[*i] == typed::UINT64)
            .map(|i| u64::from_le_bytes(g[i + 1..i + 9].try_into().unwrap()))
            .collect()
    }

    /// As text for LAN presence.
    pub fn encode(&self) -> String {
        format!("{}.{}.{}.{}", hex(&self.id), hex(&self.addr), hex(&self.head), hex(&self.game))
    }

    pub fn decode(text: &str) -> Option<Session> {
        let mut parts = text.split('.');
        let mut next = || unhex(parts.next()?);
        Some(Session { id: next()?.try_into().ok()?, addr: next()?, head: next()?, game: next()? })
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// This copy's sessions, by id.
static MINE: Mutex<BTreeMap<[u8; 8], Session>> = Mutex::new(BTreeMap::new());

/// The presence key prefix for a title's sessions.
fn key_prefix() -> String {
    format!("demonware:{}:session:", super::auth::title())
}

/// Shares this copy's sessions (once PeerParty's LAN is up; it always is by the time a
/// game signs in to Demonware, and this never starts it).
fn publish() {
    if crate::net::lan_if_started().is_none() {
        return;
    }
    let prefix = key_prefix();
    let kv: Vec<(String, String)> = MINE
        .lock()
        .unwrap()
        .values()
        .map(|s| (format!("{prefix}{}", hex(&s.id)), s.encode()))
        .collect();
    crate::net::set_presence("demonware", kv);
}

/// Every session of this title on the LAN, this copy's first.
fn all_sessions() -> Vec<Session> {
    let mut out: Vec<Session> = crate::sys::lock(&MINE).values().cloned().collect();
    let Some(lan) = crate::net::lan_if_started() else { return out };
    let prefix = key_prefix();
    for (peer, _) in lan.peers() {
        for (k, v) in lan.presence(peer) {
            if k.starts_with(&prefix) {
                if let Some(s) = Session::decode(&v).filter(|s| !out.iter().any(|o| o.id == s.id)) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// Whether players `a` and `b` (SteamIDs) are in one Demonware session on the LAN. A
/// game that keeps its players together there, its Steam lobby only for invites, has
/// them in different Steam lobbies (it makes a fresh one each time a player joins):
/// this is how the overlay knows they're together all the same.
pub fn together(a: u64, b: u64) -> bool {
    if !super::active() || a == 0 || b == 0 || a == b {
        return false;
    }
    all_sessions().iter().any(|s| {
        let ids = s.ids();
        ids.contains(&a) && ids.contains(&b)
    })
}

/// A new session id.
fn new_id() -> [u8; 8] {
    crate::util::crypto::random_bytes(8).try_into().unwrap()
}

/// Answers a bdMatchMaking task: Some(results) when it's handled here.
pub fn answer(task: u8, args: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut r = Reader::new(args);
    match task {
        // createSession(info) -> the session's id.
        1 => {
            let id = new_id();
            let Some(s) = Session::from_info(id, args) else {
                log!("demonware mm: a session this can't read: {}", hex(&args[..args.len().min(96)]));
                return Some(Vec::new());
            };
            log!(
                "demonware mm: session {} created (host address {}, players {:x?})",
                hex(&id),
                hex(&s.addr),
                s.ids()
            );
            crate::sys::lock(&MINE).insert(id, s);
            publish();
            let mut out = Vec::new();
            blob(&mut out, &id);
            Some(vec![out])
        }
        // updateSession(id, ...): the info after the id (and a u32 for task 12).
        2 | 12 => {
            let id: [u8; 8] = r.blob()?.try_into().ok()?;
            if task == 12 {
                r.u32()?;
            }
            let mut mine = crate::sys::lock(&MINE);
            if mine.contains_key(&id) {
                if let Some(s) = Session::from_info(id, r.rest()) {
                    mine.insert(id, s);
                    drop(mine);
                    publish();
                }
            }
            Some(Vec::new())
        }
        // deleteSession(id).
        3 => {
            let id: [u8; 8] = r.blob()?.try_into().ok()?;
            if crate::sys::lock(&MINE).remove(&id).is_some() {
                log!("demonware mm: session {} deleted", hex(&id));
                publish();
            }
            Some(Vec::new())
        }
        // findSessions(query): every session of this title.
        5 => {
            let found = all_sessions();
            crate::log::throttled("demonware mm find", 3, |n| log!("demonware mm: find sessions: {} on the LAN n={n}", found.len()));
            Some(found.iter().map(Session::result).collect())
        }
        // The sessions of some players (an array of u64 ids).
        14 => {
            let mut wanted = Vec::new();
            while let Some(v) = r.u64() {
                wanted.push(v);
            }
            let all = all_sessions();
            let theirs: Vec<&Session> = all.iter().filter(|s| s.ids().iter().any(|i| wanted.contains(i))).collect();
            let found: Vec<&Session> = if theirs.is_empty() { all.iter().collect() } else { theirs.clone() };
            log!(
                "demonware mm: sessions of players {wanted:x?}: {} of theirs, answering {} of {} on the LAN",
                theirs.len(),
                found.len(),
                all.len()
            );
            Some(found.iter().map(|s| s.result()).collect())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The info the game sent for its session (2026-11-20), cut after two player ids.
    fn info() -> Vec<u8> {
        let mut i = Vec::new();
        blob(&mut i, &[0xc0, 0xa8, 1, 0x45, 2, 0x0c]);
        typed::u32(&mut i, 0x0f);
        typed::u32(&mut i, 8);
        typed::u32(&mut i, 0x9ed97874);
        blob(&mut i, &[5; 16]);
        blob(&mut i, &[6; 8]);
        i.extend_from_slice(b"\x10Player\0");
        typed::u64(&mut i, 0x1234_5678_9abc_def0);
        typed::u64(&mut i, 0);
        i
    }

    #[test]
    fn a_session_reads_back_with_its_id_and_player_count() {
        let s = Session::from_info([9; 8], &info()).unwrap();
        assert_eq!(s.addr, vec![0xc0, 0xa8, 1, 0x45, 2, 0x0c]);
        assert_eq!(s.ids(), vec![0x1234_5678_9abc_def0, 0]);
        let res = s.result();
        let mut r = Reader::new(&res);
        assert_eq!(r.blob().unwrap(), &s.addr[..]);
        assert_eq!(r.blob().unwrap(), &[9; 8]);
        assert_eq!(r.u32(), Some(0x0f));
        assert_eq!(r.u32(), Some(8));
        assert_eq!(r.u32(), Some(1));
        assert_eq!(r.u32(), Some(0x9ed97874));
        assert_eq!(r.blob().unwrap(), &[5; 16]);
        assert_eq!(r.blob().unwrap(), &[6; 8]);
        assert_eq!(r.string().unwrap(), b"Player");
        assert_eq!(r.u64(), Some(0x1234_5678_9abc_def0));
        assert_eq!(Session::decode(&s.encode()), Some(s));
    }

    #[test]
    fn players_in_one_session_are_together() {
        crate::demonware::set_active();
        let mut i = Vec::new();
        blob(&mut i, &[1, 2, 3, 4, 5, 6]);
        typed::u32(&mut i, 0x0f);
        typed::u32(&mut i, 8);
        typed::u64(&mut i, 0x0110_0001_0000_0a01);
        typed::u64(&mut i, 0x0110_0001_0000_0a02);
        typed::u64(&mut i, 0);
        let s = Session::from_info([0xa0; 8], &i).unwrap();
        crate::sys::lock(&MINE).insert(s.id, s);
        assert!(together(0x0110_0001_0000_0a01, 0x0110_0001_0000_0a02));
        assert!(!together(0x0110_0001_0000_0a01, 0x0110_0001_0000_0a03));
        assert!(!together(0x0110_0001_0000_0a01, 0x0110_0001_0000_0a01));
        crate::sys::lock(&MINE).remove(&[0xa0; 8]);
    }

    #[test]
    fn sessions_are_created_found_by_player_and_deleted() {
        let created = answer(1, &info()).unwrap();
        let mut r = Reader::new(&created[0]);
        let id: [u8; 8] = r.blob().unwrap().try_into().unwrap();
        let mut ask = Vec::new();
        typed::u64(&mut ask, 0x1234_5678_9abc_def0);
        let found = answer(14, &ask).unwrap();
        assert!(found.iter().any(|f| f.windows(8).any(|w| w == id)));
        let mut del = Vec::new();
        blob(&mut del, &id);
        answer(3, &del).unwrap();
        assert!(!crate::sys::lock(&MINE).contains_key(&id));
    }
}
