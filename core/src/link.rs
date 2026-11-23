//! What outlives a dropped connection to a peer, so a short break doesn't make the peer
//! leave (see `lan::LINK_GRACE`).
//!
//! Every message on a peer's ordered stream that counts in its session (`Msg::is_counted`)
//! carries its number (`Msg::Numbered`), each side numbering what it sends from 1. What
//! was sent stays kept until the peer says it has everything up to it (`Msg::Ack`).
//! The receiver goes by the numbers, not by how many came: one it already has is
//! dropped, and one past a gap (messages still on another connection, as when both
//! sides dialed at once and both connections carry data for a moment, or lost on a
//! connection that was cut) is remembered and held back until the gap fills, so what
//! is delivered stays in order; the gap is asked for again (`Msg::Resend`), and given
//! up on in the end. When the connection drops and the peer comes back on a new
//! one, each side first says where it stands (`Msg::Session`): its process's session,
//! the one it holds for the other, how many it sent, how far it has everything it
//! received, and the oldest one it still keeps. Both sides then make the same decision
//! from the same numbers (`Link::can_resume`): pick up where it left, each side sending
//! again what the other hasn't got, or start over (a leave and a fresh join).

use std::collections::{BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use crate::wire::Msg;

/// Unacknowledged messages kept for a peer, at most (bytes). Past it the oldest go, and
/// a resume that would need them becomes a leave and a fresh join instead.
pub(crate) const KEEP_MAX: usize = 4 << 20;
/// A peer hears how far we've read at most this often (and on a quiet stream, each
/// `Lan::tick`).
pub(crate) const ACK_EVERY: Duration = Duration::from_millis(200);
/// How long a gap in what came (see `Link::received`) may last before it is asked for
/// again, and how often after that; after GAP_TRIES asks it is given up on.
pub(crate) const GAP_WAIT: Duration = Duration::from_secs(2);
const GAP_TRIES: u32 = 3;
/// Messages past a gap remembered, at most: a peer that numbers wildly can't make the
/// set grow without end (the gap is then given up on).
const BEYOND_MAX: usize = 1 << 16;
/// A peer's asks to send again are answered at most this often (each may be KEEP_MAX).
const RESEND_EVERY: Duration = Duration::from_secs(1);

fn size(m: &Msg) -> usize {
    match m {
        Msg::ChanData { socket, data, .. } => socket.len() + data.len() + 16,
        _ => m.encode().len(),
    }
}

pub(crate) struct Link {
    /// The peer's process session, from its `Msg::Session` (0 until it says).
    pub session: u64,
    /// Counted messages we sent it.
    sent: u64,
    /// The last of those it hasn't acknowledged, oldest first.
    kept: VecDeque<Msg>,
    kept_bytes: usize,
    /// How far we have every counted message it sent (its numbers 1..=recv).
    recv: u64,
    /// The ones past `recv` that came (past a gap), and since when there's a gap.
    beyond: BTreeSet<u64>,
    gap_since: Option<Instant>,
    /// Asks for the gap so far, and when the last went.
    gap_asks: u32,
    gap_asked: Option<Instant>,
    /// When we last sent again what the peer asked for.
    resent_at: Option<Instant>,
    /// What we last told it we have, and when.
    acked: u64,
    acked_at: Option<Instant>,
    /// More was sent while the peer was away than is kept (KEEP_MAX): it can't pick up.
    overfull: bool,
}

/// Where one side stands, as its `Msg::Session` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Standing {
    pub sent: u64,
    pub recv: u64,
    /// The oldest message still kept (sent + 1 when nothing is).
    pub first: u64,
}

impl Link {
    pub fn new(session: u64) -> Link {
        Link {
            session,
            sent: 0,
            kept: VecDeque::new(),
            kept_bytes: 0,
            recv: 0,
            beyond: BTreeSet::new(),
            gap_since: None,
            gap_asks: 0,
            gap_asked: None,
            resent_at: None,
            acked: 0,
            acked_at: None,
            overfull: false,
        }
    }

    /// The number the next counted message we send gets.
    pub fn next(&self) -> u64 {
        self.sent + 1
    }

    /// A message we sent, or (`away`: the connection is down) would have. While
    /// connected, the oldest unacknowledged go past KEEP_MAX (a resume needing them then
    /// fails, the same way on both sides); while away nothing goes, since the peer has
    /// none of it, and past KEEP_MAX the link is `overfull`.
    pub fn sent(&mut self, m: &Msg, away: bool) {
        if !m.is_counted() {
            return;
        }
        self.sent += 1;
        self.kept_bytes += size(m);
        self.kept.push_back(m.clone());
        if away {
            self.overfull |= self.kept_bytes > KEEP_MAX;
            return;
        }
        while self.kept_bytes > KEEP_MAX {
            let Some(old) = self.kept.pop_front() else { break };
            self.kept_bytes -= size(&old);
        }
    }

    pub fn overfull(&self) -> bool {
        self.overfull
    }

    /// The bytes kept for replay (at most KEEP_MAX while the peer is here).
    pub fn kept_bytes(&self) -> usize {
        self.kept_bytes
    }

    /// The peer's message number `n` came: false when we already have it (a resend, or
    /// the same message on two connections), which is then dropped. One past a gap is
    /// counted (and remembered); the caller holds it back while `n > recv()`.
    pub fn received(&mut self, n: u64, now: Instant) -> bool {
        if n <= self.recv || self.beyond.contains(&n) {
            return false;
        }
        if self.recv.checked_add(1) != Some(n) && self.beyond.len() >= BEYOND_MAX {
            self.give_up_gap();
            // That can take `recv` past `n`: `n` was in the gap given up on, and is
            // taken now without being remembered (everything up to `recv` counts as had).
            if n <= self.recv {
                return true;
            }
        }
        if self.recv.checked_add(1) == Some(n) {
            self.recv = n;
            self.fill();
        } else {
            self.beyond.insert(n);
            self.gap_since.get_or_insert(now);
        }
        true
    }

    /// `recv` up past what came after it, the gap closed once it's filled.
    fn fill(&mut self) {
        let before = self.recv;
        while self.recv < u64::MAX && self.beyond.first() == Some(&(self.recv + 1)) {
            self.beyond.pop_first();
            self.recv += 1;
        }
        if self.beyond.is_empty() {
            self.gap_since = None;
            self.gap_asks = 0;
            self.gap_asked = None;
        } else if self.recv != before {
            // Part filled: what's left is asked for in its own time.
            self.gap_asks = 0;
        }
    }

    /// How far we have every message the peer sent: what came past it is held back
    /// until it gets there.
    pub fn recv(&self) -> u64 {
        self.recv
    }

    /// What's missing before the first message past the gap is taken as lost (what the
    /// caller held back then goes, in order).
    pub fn give_up_gap(&mut self) {
        if let Some(&first) = self.beyond.first() {
            self.recv = first - 1;
            self.fill();
        }
    }

    /// Whether a gap is due to be asked for (`Gap::Ask`, with how far we have
    /// everything) or given up on (`Gap::GaveUp`, how many messages were lost).
    pub fn gap_due(&mut self, now: Instant) -> Option<Gap> {
        let since = self.gap_since?;
        if now.duration_since(since) < GAP_WAIT || self.gap_asked.is_some_and(|t| now.duration_since(t) < GAP_WAIT) {
            return None;
        }
        if self.gap_asks >= GAP_TRIES {
            let lost = self.beyond.first().map_or(0, |f| f - 1 - self.recv);
            self.give_up_gap();
            return Some(Gap::GaveUp(lost));
        }
        self.gap_asks += 1;
        self.gap_asked = Some(now);
        Some(Gap::Ask(self.recv))
    }

    /// What we keep past `from`, with its numbers, for a peer that asked for it again
    /// (`Msg::Resend`); nothing when it asked a moment ago.
    pub fn resend(&mut self, from: u64, now: Instant) -> Vec<(u64, Msg)> {
        if self.resent_at.is_some_and(|t| now.duration_since(t) < RESEND_EVERY) {
            return Vec::new();
        }
        self.resent_at = Some(now);
        self.acked(from);
        self.numbered()
    }

    fn numbered(&self) -> Vec<(u64, Msg)> {
        let first = self.standing().first;
        self.kept.iter().cloned().enumerate().map(|(i, m)| (first + i as u64, m)).collect()
    }

    pub fn standing(&self) -> Standing {
        Standing { sent: self.sent, recv: self.recv, first: self.sent + 1 - self.kept.len() as u64 }
    }

    /// The peer has everything up to `upto`: no need to keep it.
    pub fn acked(&mut self, upto: u64) {
        let mut first = self.standing().first;
        while first <= upto.min(self.sent) {
            let Some(old) = self.kept.pop_front() else { break };
            self.kept_bytes -= size(&old);
            first += 1;
        }
    }

    /// What to acknowledge, when it's time.
    pub fn ack_due(&mut self, now: Instant) -> Option<u64> {
        if self.recv == self.acked {
            return None;
        }
        if self.acked_at.is_some_and(|t| now.duration_since(t) < ACK_EVERY) {
            return None;
        }
        self.acked = self.recv;
        self.acked_at = Some(now);
        Some(self.recv)
    }

    /// What the peer hasn't got: everything after its `recv`, oldest first, with its
    /// numbers (what it has past a gap it drops).
    pub fn replay(&mut self, their_recv: u64) -> Vec<(u64, Msg)> {
        self.acked(their_recv);
        // Our own `Msg::Session` told the peer how far we've read.
        self.acked = self.recv;
        self.numbered()
    }
}

/// A gap in what came from a peer, when its time comes (`Link::gap_due`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gap {
    /// Ask for everything past this again.
    Ask(u64),
    /// Asked enough: this many messages are taken as lost.
    GaveUp(u64),
}

/// Whether the two sides can pick up where they left: each has kept everything the
/// other is missing. Both sides compute this from the same numbers (each side's from its
/// `Msg::Session`).
pub(crate) fn can_resume(ours: Standing, theirs: Standing) -> bool {
    fits(ours, theirs) && fits(theirs, ours)
}

/// Whether `from` still keeps everything after what `to` received.
fn fits(from: Standing, to: Standing) -> bool {
    to.recv <= from.sent && to.recv.saturating_add(1) >= from.first
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What comes past a gap is counted but stays past `recv` (the caller holds it)
    /// until the gap fills, or is given up on.
    #[test]
    fn past_a_gap_waits_until_it_fills_or_is_given_up() {
        let t = Instant::now();
        let mut l = Link::new(1);
        assert!(l.received(1, t) && l.recv() == 1);
        // Both sides dialed at once: 3 came on the kept connection before 2 on the other.
        assert!(l.received(3, t) && l.recv() == 1, "3 waits for 2");
        assert!(!l.received(3, t), "and isn't taken twice");
        assert!(l.received(2, t) && l.recv() == 3, "2 lets 3 go after it");
        // Lost for good: given up on, what came after it goes.
        assert!(l.received(6, t) && l.recv() == 3);
        l.give_up_gap();
        assert_eq!(l.recv(), 6);
        assert!(!l.received(4, t), "one of the given-up turning up late is old");
    }

    fn data(n: u8, len: usize) -> Msg {
        Msg::ChanData { socket: "s".into(), channel: 0, reliable: true, data: vec![n; len] }
    }

    /// A full gap list given up on for a message below it: nothing past `recv` is left
    /// behind it, and nothing already had comes again.
    #[test]
    fn a_full_gap_never_leaves_numbers_behind_recv() {
        let mut l = Link::new(1);
        let now = Instant::now();
        for i in 0..BEYOND_MAX as u64 {
            assert!(l.received(100_000 + 2 * i, now));
        }
        assert!(l.received(2, now), "in the gap given up on, so taken");
        assert_eq!(l.recv, 100_000);
        assert!(l.beyond.first().is_none_or(|&f| f > l.recv));
        assert!(!l.received(2, now) && !l.received(3, now) && !l.received(100_000, now));
    }

    #[test]
    fn only_messages_on_the_ordered_stream_count() {
        let mut l = Link::new(1);
        l.sent(&Msg::Ack { recv: 3 }, false);
        l.sent(&Msg::ChanData { socket: "s".into(), channel: 0, reliable: false, data: vec![1] }, false);
        l.sent(&Msg::Moved, false);
        assert_eq!(l.standing(), Standing { sent: 0, recv: 0, first: 1 });
        l.sent(&data(1, 10), false);
        l.sent(&Msg::Leave { lobby: "x".into() }, false);
        assert_eq!(l.standing(), Standing { sent: 2, recv: 0, first: 1 });
        assert_eq!(l.next(), 3);
    }

    #[test]
    fn acknowledged_messages_are_let_go_and_the_rest_replayed() {
        let mut l = Link::new(1);
        for n in 1..=5 {
            l.sent(&data(n, 4), false);
        }
        l.acked(2);
        assert_eq!(l.standing(), Standing { sent: 5, recv: 0, first: 3 });
        l.acked(9);
        assert_eq!(l.standing().first, 6, "never past what was sent");
        for n in 6..=8 {
            l.sent(&data(n, 4), false);
        }
        let again = l.replay(6);
        assert_eq!(again, vec![(7, data(7, 4)), (8, data(8, 4))]);
        assert_eq!(l.standing().first, 7);
    }

    #[test]
    fn both_sides_agree_whether_to_resume() {
        let (mut a, mut b) = (Link::new(1), Link::new(2));
        for n in 0..4 {
            a.sent(&data(n, 1), false);
        }
        // b got two of a's four; b sent one that a got.
        let t = Instant::now();
        assert!(b.received(1, t) && b.received(2, t));
        b.sent(&data(9, 1), false);
        assert!(a.received(1, t));
        assert!(can_resume(a.standing(), b.standing()) && can_resume(b.standing(), a.standing()));
        // a let go of too much (it can't have): nothing to send b's third from.
        a.acked(3);
        assert!(!can_resume(a.standing(), b.standing()) && !can_resume(b.standing(), a.standing()));
    }

    #[test]
    fn what_is_kept_is_bounded_and_then_no_resume() {
        let mut a = Link::new(1);
        let b = Link::new(2);
        for n in 0..5 {
            a.sent(&data(n, KEEP_MAX / 4), false);
        }
        assert!(a.kept_bytes <= KEEP_MAX && !a.overfull());
        assert!(a.standing().first > 1);
        assert!(!can_resume(a.standing(), b.standing()) && !can_resume(b.standing(), a.standing()));
        // While the peer is away nothing goes: past the bound the link is overfull.
        let mut c = Link::new(1);
        for n in 0..5 {
            c.sent(&data(n, KEEP_MAX / 4), true);
        }
        assert_eq!(c.standing().first, 1);
        assert!(c.overfull());
    }

    #[test]
    fn acknowledgements_are_spaced() {
        let mut l = Link::new(1);
        let t = Instant::now();
        assert_eq!(l.ack_due(t), None);
        l.received(1, t);
        assert_eq!(l.ack_due(t), Some(1));
        l.received(2, t);
        assert_eq!(l.ack_due(t), None);
        assert_eq!(l.ack_due(t + ACK_EVERY), Some(2));
        assert_eq!(l.ack_due(t + ACK_EVERY * 2), None);
    }

    /// Messages lost on one connection while later ones came on another (a tie-break's
    /// retired connection, cut): the later ones are taken, the gap asked for, and what
    /// is sent again is taken only where it was missing, never twice.
    #[test]
    fn a_gap_is_asked_for_and_nothing_comes_twice() {
        let t = Instant::now();
        let mut l = Link::new(1);
        assert!(l.received(1, t));
        // 2 and 3 were lost; 4 and 5 came.
        assert!(l.received(4, t) && l.received(5, t));
        assert!(!l.received(4, t), "a second 4");
        assert_eq!(l.standing().recv, 1, "everything only up to 1");
        assert_eq!(l.ack_due(t), Some(1));
        assert_eq!(l.gap_due(t), None, "not yet");
        assert_eq!(l.gap_due(t + GAP_WAIT), Some(Gap::Ask(1)));
        assert_eq!(l.gap_due(t + GAP_WAIT), None, "asked a moment ago");
        // The sender's answer: 2..=5 again. Only 2 and 3 are new.
        let fresh: Vec<u64> = (2..=5).filter(|n| l.received(*n, t + GAP_WAIT)).collect();
        assert_eq!(fresh, vec![2, 3]);
        assert_eq!(l.standing().recv, 5);
        assert_eq!(l.gap_due(t + GAP_WAIT * 9), None, "no gap left");
    }

    #[test]
    fn a_gap_never_filled_is_given_up_on() {
        let t = Instant::now();
        let mut l = Link::new(1);
        l.received(3, t);
        let mut at = t;
        for _ in 0..GAP_TRIES {
            at += GAP_WAIT;
            assert_eq!(l.gap_due(at), Some(Gap::Ask(0)));
        }
        at += GAP_WAIT;
        assert_eq!(l.gap_due(at), Some(Gap::GaveUp(2)));
        assert_eq!(l.standing().recv, 3);
        assert!(l.received(4, at));
        assert!(!l.received(2, at), "a lost one turning up late is old");
    }

    #[test]
    fn a_resend_is_numbered_and_spaced() {
        let t = Instant::now();
        let mut l = Link::new(1);
        for n in 1..=4 {
            l.sent(&data(n, 2), false);
        }
        let again = l.resend(2, t);
        assert_eq!(again.iter().map(|(n, _)| *n).collect::<Vec<_>>(), vec![3, 4]);
        assert!(l.resend(2, t).is_empty(), "asked again at once");
        assert_eq!(l.resend(2, t + RESEND_EVERY).len(), 2);
    }
}
