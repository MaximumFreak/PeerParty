//! Lobby chat that arrives from other players (Steam's lobby chat, Galaxy's lobby
//! messages): taken only from a member of a lobby we're in, bounded in size, and kept
//! in a list per lobby that drops its oldest entries.
//!
//! A message is held for a moment when its sender isn't a member yet: a player who
//! just joined often says hello at once, and that can arrive before the owner's news
//! that they're in. Held messages wait at most `HOLD_FOR`, and only `MAX_HELD` of them
//! in all, so a stranger sending chat for lobbies they aren't in costs little.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use peerparty_core::PeerId;

/// How long a message from a player who isn't (yet) a member waits.
pub const HOLD_FOR: Duration = Duration::from_secs(2);
/// Messages waiting at once, from everyone: the oldest go first.
pub const MAX_HELD: usize = 64;
/// Entries a lobby's list keeps; a game reads each one as its callback arrives.
pub const MAX_KEPT: usize = 512;

/// One lobby's messages, numbered from 0 in arrival order. The numbers are the ids the
/// game asks for, so they keep counting when old entries are dropped.
pub struct Log<T> {
    lists: HashMap<u64, (u32, VecDeque<T>)>,
}

impl<T> Default for Log<T> {
    fn default() -> Self {
        Log { lists: HashMap::new() }
    }
}

impl<T> Log<T> {
    /// Adds a message to `lobby`'s list: its id.
    pub fn push(&mut self, lobby: u64, entry: T) -> u32 {
        let (base, list) = self.lists.entry(lobby).or_default();
        list.push_back(entry);
        if list.len() > MAX_KEPT {
            list.pop_front();
            *base = base.wrapping_add(1);
        }
        base.wrapping_add(list.len() as u32 - 1)
    }

    /// Message `id` of `lobby`, while it's kept.
    pub fn get(&self, lobby: u64, id: u32) -> Option<&T> {
        let (base, list) = self.lists.get(&lobby)?;
        list.get(id.wrapping_sub(*base) as usize)
    }

    /// Drops a lobby's list (we left it).
    pub fn forget(&mut self, lobby: u64) {
        self.lists.remove(&lobby);
    }
}

/// A message from another player: (lobby, sender, body).
pub type Msg = (u64, PeerId, Vec<u8>);

/// Messages waiting for their sender to be a member.
#[derive(Default)]
pub struct Inbox {
    held: VecDeque<(Instant, Msg)>,
}

impl Inbox {
    /// Sorts what arrived (after anything held before): the messages to give the game
    /// now, in order. `member(lobby, sender)` says whether we're in that lobby and the
    /// sender is a member of it. Bodies over `max_len` bytes are dropped. A sender's
    /// later message never overtakes one of theirs that is still held.
    pub fn sort(&mut self, fresh: Vec<Msg>, now: Instant, max_len: usize, member: impl Fn(u64, PeerId) -> bool) -> Vec<Msg> {
        let mut take = Vec::new();
        let mut keep: VecDeque<(Instant, Msg)> = VecDeque::new();
        let earlier = std::mem::take(&mut self.held);
        let all = earlier.into_iter().chain(fresh.into_iter().map(|m| (now, m)));
        for (at, m) in all {
            if m.2.len() > max_len {
                continue;
            }
            let behind = keep.iter().any(|(_, k)| k.0 == m.0 && k.1 == m.1);
            if !behind && member(m.0, m.1) {
                take.push(m);
            } else if now.duration_since(at) < HOLD_FOR {
                keep.push_back((at, m));
            }
        }
        while keep.len() > MAX_HELD {
            keep.pop_front();
        }
        self.held = keep;
        take
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: u8) -> PeerId {
        PeerId([n; 16])
    }

    #[test]
    fn ids_keep_counting_when_old_entries_go() {
        let mut log = Log::default();
        for i in 0..(MAX_KEPT as u32 + 10) {
            assert_eq!(log.push(7, i), i);
        }
        assert_eq!(log.get(7, 0), None, "the oldest went");
        assert_eq!(log.get(7, 9), None);
        assert_eq!(log.get(7, 10), Some(&10));
        assert_eq!(log.get(7, MAX_KEPT as u32 + 9), Some(&(MAX_KEPT as u32 + 9)));
        assert_eq!(log.get(7, MAX_KEPT as u32 + 10), None);
        assert_eq!(log.get(8, 0), None);
        log.forget(7);
        assert_eq!(log.get(7, 10), None);
        assert_eq!(log.push(7, 1), 0, "a lobby joined again starts over");
    }

    #[test]
    fn only_members_of_our_lobbies_are_heard() {
        let mut inbox = Inbox::default();
        let t0 = Instant::now();
        let member = |lobby: u64, who: PeerId| lobby == 1 && who == p(2);
        let fresh = vec![(1, p(2), b"hi".to_vec()), (1, p(3), b"not a member".to_vec()), (5, p(2), b"another lobby".to_vec())];
        let got = inbox.sort(fresh, t0, 4096, member);
        assert_eq!(got, vec![(1, p(2), b"hi".to_vec())]);
        // The others wait, and are dropped once they've waited too long.
        assert_eq!(inbox.held.len(), 2);
        assert!(inbox.sort(vec![], t0 + HOLD_FOR, 4096, member).is_empty());
        assert!(inbox.held.is_empty());
        // Too long: dropped at once.
        assert!(inbox.sort(vec![(1, p(2), vec![0; 4097])], t0, 4096, member).is_empty());
        assert!(inbox.held.is_empty());
    }

    #[test]
    fn a_new_members_hello_waits_for_the_news_and_keeps_its_order() {
        let mut inbox = Inbox::default();
        let t0 = Instant::now();
        let first = inbox.sort(vec![(1, p(4), b"hello".to_vec())], t0, 64, |_, _| false);
        assert!(first.is_empty());
        // Their join is known now: what waited comes first, then what's new.
        let later = t0 + Duration::from_millis(300);
        let got = inbox.sort(vec![(1, p(4), b"again".to_vec())], later, 64, |_, who| who == p(4));
        assert_eq!(got, vec![(1, p(4), b"hello".to_vec()), (1, p(4), b"again".to_vec())]);
    }

    #[test]
    fn held_messages_are_bounded() {
        let mut inbox = Inbox::default();
        let t0 = Instant::now();
        let flood: Vec<Msg> = (0..1000u32).map(|i| (u64::from(i), p(9), vec![1])).collect();
        assert!(inbox.sort(flood, t0, 64, |_, _| false).is_empty());
        assert_eq!(inbox.held.len(), MAX_HELD);
        assert_eq!(inbox.held.back().unwrap().1 .0, 999, "the newest are kept");
    }
}