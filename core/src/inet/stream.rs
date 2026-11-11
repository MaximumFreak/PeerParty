//! A reliable, ordered byte stream over the tunnel's datagrams: the mesh's TCP connection
//! rides on it. Numbered segments, cumulative acknowledgements with a 64-segment
//! selective bitmap, retransmission on a timer (RFC 6298's estimate) or when later
//! segments are acknowledged first, and a congestion window that halves on loss.
//! Time comes from the caller, so all of it is tested without a network.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// Bytes in one segment: with the headers, a packet stays under 1200 bytes.
pub const MAX_SEG: usize = 1100;
/// The biggest segment we take. Ours are MAX_SEG; a peer may hold RECV_WINDOW segments
/// ahead of a gap it never fills, so their size is what bounds that memory (4 MiB at
/// this size, where 64 KiB segments would be 128 MiB a session).
pub const MAX_SEG_IN: usize = 2048;
/// Bytes the sender queues before the writer is told to wait.
pub const MAX_QUEUED: usize = 4 << 20;
const MAX_IN_FLIGHT: usize = 1024;
/// Segments the receiver holds ahead of a gap.
const RECV_WINDOW: u32 = 2048;
const MIN_RTO: Duration = Duration::from_millis(150);
const MAX_RTO: Duration = Duration::from_secs(4);
const INITIAL_RTO: Duration = Duration::from_millis(500);
const INITIAL_CWND: f64 = 16.0;

struct Sent {
    data: Vec<u8>,
    at: Instant,
    tries: u32,
    /// Marked lost by fast retransmit: due on the next poll.
    fast: bool,
}

/// One segment or acknowledgement to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    Seg { seq: u32, data: Vec<u8> },
    Ack { next: u32, sack: u64 },
}

pub struct Stream {
    queued: VecDeque<u8>,
    next_seq: u32,
    in_flight: BTreeMap<u32, Sent>,
    cwnd: f64,
    ssthresh: f64,
    srtt: Option<Duration>,
    rttvar: Duration,
    rto: Duration,
    /// No more halving until this sequence is acknowledged (one cut per loss event).
    recovery: Option<u32>,
    expected: u32,
    ahead: BTreeMap<u32, Vec<u8>>,
    ack_due: bool,
    /// Segments sent again: a measure of the path's loss.
    pub resent: u64,
}

impl Default for Stream {
    fn default() -> Self {
        Stream {
            queued: VecDeque::new(),
            next_seq: 0,
            in_flight: BTreeMap::new(),
            cwnd: INITIAL_CWND,
            ssthresh: f64::INFINITY,
            srtt: None,
            rttvar: Duration::ZERO,
            rto: INITIAL_RTO,
            recovery: None,
            expected: 0,
            ahead: BTreeMap::new(),
            ack_due: false,
            resent: 0,
        }
    }
}

impl Stream {
    pub fn write(&mut self, data: &[u8]) {
        self.queued.extend(data);
    }

    /// Bytes waiting to be sent or acknowledged.
    pub fn backlog(&self) -> usize {
        self.queued.len() + self.in_flight.len() * MAX_SEG
    }

    pub fn idle(&self) -> bool {
        self.queued.is_empty() && self.in_flight.is_empty()
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.srtt
    }

    /// What to send now: an acknowledgement if one is due, retransmissions whose timer
    /// ran out, and new segments while the window has room.
    pub fn poll(&mut self, now: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        if self.ack_due {
            self.ack_due = false;
            out.push(self.ack());
        }
        let mut timed_out = false;
        for (&seq, s) in self.in_flight.iter_mut() {
            if s.fast {
                // Lost by the selective acknowledgements: resend now, once.
                s.fast = false;
                s.at = now;
                s.tries += 1;
                self.resent += 1;
                out.push(Out::Seg { seq, data: s.data.clone() });
            } else if now.duration_since(s.at) >= self.rto * (1u32 << s.tries.min(4)) {
                s.at = now;
                s.tries += 1;
                self.resent += 1;
                timed_out = true;
                out.push(Out::Seg { seq, data: s.data.clone() });
            }
        }
        if timed_out {
            self.ssthresh = (self.cwnd / 2.0).max(4.0);
            self.cwnd = self.ssthresh;
            self.recovery = Some(self.next_seq);
        }
        while !self.queued.is_empty()
            && (self.in_flight.len() as f64) < self.cwnd
            && self.in_flight.len() < MAX_IN_FLIGHT
        {
            let n = self.queued.len().min(MAX_SEG);
            let data: Vec<u8> = self.queued.drain(..n).collect();
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            self.in_flight.insert(seq, Sent { data: data.clone(), at: now, tries: 0, fast: false });
            out.push(Out::Seg { seq, data });
        }
        out
    }

    fn ack(&self) -> Out {
        let mut sack = 0u64;
        for &s in self.ahead.keys() {
            let off = s.wrapping_sub(self.expected).wrapping_sub(1);
            if off < 64 {
                sack |= 1u64 << off;
            }
        }
        Out::Ack { next: self.expected, sack }
    }

    /// A segment arrived: returns the bytes that are now in order.
    pub fn on_segment(&mut self, seq: u32, data: Vec<u8>) -> Vec<u8> {
        self.ack_due = true;
        let off = seq.wrapping_sub(self.expected);
        if off >= RECV_WINDOW {
            return Vec::new(); // old (a duplicate) or too far ahead
        }
        self.ahead.entry(seq).or_insert(data);
        let mut ready = Vec::new();
        while let Some(d) = self.ahead.remove(&self.expected) {
            ready.extend_from_slice(&d);
            self.expected = self.expected.wrapping_add(1);
        }
        ready
    }

    pub fn on_ack(&mut self, next: u32, sack: u64, now: Instant) {
        let mut acked_new = 0usize;
        let mut sample = None;
        // Everything below `next`, then the selectively acknowledged ones.
        let below: Vec<u32> = self
            .in_flight
            .keys()
            .copied()
            .filter(|&s| next.wrapping_sub(s).wrapping_sub(1) < MAX_IN_FLIGHT as u32 * 4)
            .collect();
        let sacked: Vec<u32> = (0..64u32)
            .filter(|&i| sack & (1u64 << i) != 0)
            .map(|i| next.wrapping_add(1 + i))
            .collect();
        for s in below.into_iter().chain(sacked.iter().copied()) {
            if let Some(sent) = self.in_flight.remove(&s) {
                acked_new += 1;
                if sent.tries == 0 {
                    sample = Some(now.duration_since(sent.at));
                }
            }
        }
        if let Some(r) = sample {
            self.rtt_sample(r);
        }
        if let Some(r) = self.recovery
            && !self.in_flight.keys().any(|&s| r.wrapping_sub(s).wrapping_sub(1) < 1 << 30)
        {
            self.recovery = None;
        }
        // Growth: slow start, then about one segment per round trip.
        for _ in 0..acked_new {
            if self.cwnd < self.ssthresh {
                self.cwnd += 1.0;
            } else {
                self.cwnd += 1.0 / self.cwnd;
            }
        }
        self.cwnd = self.cwnd.min(MAX_IN_FLIGHT as f64);
        // Fast retransmit: a hole the peer has seen three later segments past, sent more
        // than a round trip and a quarter ago (the quarter allows for reordering, as
        // RACK does). Offset 0 is `next` itself; offset i + 1 is bit i of the bitmap.
        if sack != 0 {
            let srtt = self.srtt.unwrap_or(INITIAL_RTO);
            let srtt = srtt + srtt / 4;
            let mut lost = false;
            let mut above = 0;
            for off in (0..=64u32).rev() {
                if off > 0 && sack & (1u64 << (off - 1)) != 0 {
                    above += 1;
                    continue;
                }
                if above >= 3
                    && let Some(s) = self.in_flight.get_mut(&next.wrapping_add(off))
                    && !s.fast
                    && now.duration_since(s.at) >= srtt
                {
                    s.fast = true;
                    lost = true;
                }
            }
            if lost && self.recovery.is_none() {
                self.ssthresh = (self.cwnd / 2.0).max(4.0);
                self.cwnd = self.ssthresh;
                self.recovery = Some(self.next_seq);
            }
        }
    }

    fn rtt_sample(&mut self, r: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(r);
                self.rttvar = r / 2;
            }
            Some(s) => {
                let diff = s.abs_diff(r);
                self.rttvar = (self.rttvar * 3 + diff) / 4;
                self.srtt = Some((s * 7 + r) / 8);
            }
        }
        let s = self.srtt.unwrap();
        self.rto = (s + (self.rttvar * 4).max(Duration::from_millis(10))).clamp(MIN_RTO, MAX_RTO);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny deterministic random source for the lossy link.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn chance(&mut self, pct: u64) -> bool {
            self.next() % 100 < pct
        }
    }

    /// Moves `total` bytes from a to b over a link with `loss`% loss, reordering and a
    /// 30 ms delay; returns what b got and the simulated time it took.
    fn transfer(total: usize, loss: u64, seed: u64) -> (Vec<u8>, Duration, u64) {
        let mut rng = Rng(seed);
        let (mut a, mut b) = (Stream::default(), Stream::default());
        let data: Vec<u8> = (0..total).map(|i| (i * 7 % 251) as u8).collect();
        let t0 = Instant::now();
        let mut now = t0;
        let delay = Duration::from_millis(30);
        let mut to_b: Vec<(Instant, Out)> = Vec::new();
        let mut to_a: Vec<(Instant, Out)> = Vec::new();
        let mut got = Vec::new();
        let mut written = 0;
        while got.len() < total {
            assert!(now - t0 < Duration::from_secs(600), "stalled at {} of {total}", got.len());
            if written < total && a.backlog() < 256 * 1024 {
                let n = (total - written).min(50_000);
                a.write(&data[written..written + n]);
                written += n;
            }
            for o in a.poll(now) {
                if !rng.chance(loss) {
                    let jitter = Duration::from_millis(rng.next() % 10);
                    to_b.push((now + delay + jitter, o));
                }
            }
            for o in b.poll(now) {
                if !rng.chance(loss) {
                    to_a.push((now + delay, o));
                }
            }
            let (due, rest): (Vec<_>, Vec<_>) = to_b.drain(..).partition(|(t, _)| *t <= now);
            to_b = rest;
            for (_, o) in due {
                if let Out::Seg { seq, data } = o {
                    got.extend(b.on_segment(seq, data));
                }
            }
            let (due, rest): (Vec<_>, Vec<_>) = to_a.drain(..).partition(|(t, _)| *t <= now);
            to_a = rest;
            for (_, o) in due {
                if let Out::Ack { next, sack } = o {
                    a.on_ack(next, sack, now);
                }
            }
            now += Duration::from_millis(5);
        }
        assert_eq!(got, data, "bytes arrived in order and intact");
        (got, now - t0, a.resent)
    }

    #[test]
    fn clean_link() {
        let (_, took, resent) = transfer(2_000_000, 0, 1);
        assert_eq!(resent, 0);
        assert!(took < Duration::from_secs(20), "{took:?}");
    }

    #[test]
    fn lossy_link_still_delivers_everything_in_order() {
        for (loss, seed) in [(2, 7), (10, 11), (30, 13)] {
            let (_, took, resent) = transfer(300_000, loss, seed);
            assert!(resent > 0, "{loss}%");
            println!("{loss}% loss: {took:?}, {resent} resent");
        }
    }

    #[test]
    fn duplicates_and_old_segments_are_dropped() {
        let mut b = Stream::default();
        assert_eq!(b.on_segment(1, vec![2]), Vec::<u8>::new());
        assert_eq!(b.poll(Instant::now()), vec![Out::Ack { next: 0, sack: 1 }]);
        assert_eq!(b.on_segment(0, vec![1]), vec![1, 2]);
        assert_eq!(b.on_segment(0, vec![9]), Vec::<u8>::new());
        assert_eq!(b.on_segment(1, vec![9]), Vec::<u8>::new());
        assert_eq!(b.on_segment(5000, vec![9]), Vec::<u8>::new());
        assert_eq!(b.on_segment(2, vec![3]), vec![3]);
    }

    #[test]
    fn sequence_numbers_wrap() {
        let mut a = Stream { next_seq: u32::MAX - 2, ..Default::default() };
        let mut b = Stream { expected: u32::MAX - 2, ..Default::default() };
        let now = Instant::now();
        a.write(&vec![5u8; MAX_SEG * 6]);
        let mut got = Vec::new();
        for o in a.poll(now) {
            if let Out::Seg { seq, data } = o {
                got.extend(b.on_segment(seq, data));
            }
        }
        assert_eq!(got.len(), MAX_SEG * 6);
        let Some(Out::Ack { next, sack }) = b.poll(now).into_iter().next() else { panic!() };
        assert_eq!(next, 3);
        a.on_ack(next, sack, now);
        assert!(a.idle());
    }
}