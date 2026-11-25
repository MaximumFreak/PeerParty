//! Channels: keyed by (peer, socket name) and open once both sides agree, with what is
//! held until then, and the inbox of received packets waiting for the game.

use super::*;
use std::borrow::Borrow;
use std::hash::{Hash, Hasher};

/// A `(T, String)` map key looked up as `(T, &str)` (`by`): channels and inbox queues
/// are found once a packet, and a lookup that built the `String` allocated each time.
pub(super) trait StrKey<T> {
    fn parts(&self) -> (T, &str);
}

impl<T: Copy> StrKey<T> for (T, String) {
    fn parts(&self) -> (T, &str) {
        (self.0, &self.1)
    }
}

impl<T: Copy> StrKey<T> for (T, &str) {
    fn parts(&self) -> (T, &str) {
        (self.0, self.1)
    }
}

impl<'a, T: Copy + 'a> Borrow<dyn StrKey<T> + 'a> for (T, String) {
    fn borrow(&self) -> &(dyn StrKey<T> + 'a) {
        self
    }
}

// Hashes as `(T, String)` does: a `String` hashes as its `str`.
impl<T: Hash> Hash for dyn StrKey<T> + '_ {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.parts().hash(h)
    }
}

impl<T: PartialEq> PartialEq for dyn StrKey<T> + '_ {
    fn eq(&self, o: &Self) -> bool {
        self.parts() == o.parts()
    }
}

impl<T: Eq> Eq for dyn StrKey<T> + '_ {}

/// `(t, s)` as a key of a `(T, String)` map.
pub(super) fn by<'a, T: Copy + 'a>(k: &'a (T, &'a str)) -> &'a (dyn StrKey<T> + 'a) {
    k
}

/// Emptied inbox queues kept for their socket's next packet, at most: a queue made and
/// dropped for every packet cost two allocations each time, and without a bound a
/// stranger naming new sockets would leave empty queues behind for good.
pub(super) const KEPT_EMPTY_QUEUES: usize = 64;

/// Received packets kept for the game at most: a game that stops reading, or a socket
/// nobody reads, would otherwise grow the inbox without limit. The oldest go first.
pub(super) const INBOX_MAX_PACKETS: usize = 65_536;
/// How long what a peer sent on a channel before closing it stays readable.
pub(super) const CLOSED_KEEP: Duration = Duration::from_secs(30);
pub(super) const INBOX_MAX_BYTES: usize = 64 << 20;
/// What one public player's packets may take of the inbox: past it, its new ones are
/// dropped, so a stranger sending faster than the game reads can't push friends'
/// packets out (the inbox drops its oldest when full, whoever sent them).
pub(super) const PUBLIC_INBOX_PACKETS: usize = 8192;
pub(super) const PUBLIC_INBOX_BYTES: usize = 8 << 20;
/// Packets held per channel for a peer that hasn't agreed yet, at most; past that a send
/// is dropped (and says so).
pub(super) const DELAYED_MAX_PACKETS: usize = 4096;
pub(super) const DELAYED_MAX_BYTES: usize = 16 << 20;
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ChanState {
    /// We asked; waiting for the peer.
    Pending,
    /// The peer asked; waiting for the game.
    Requested,
    Open,
}

pub(super) struct Chan {
    pub(super) state: ChanState,
    pub(super) delayed: Vec<(u8, bool, Vec<u8>)>,
    pub(super) delayed_bytes: usize,
    /// What went each way since the last traffic report, and when last (`ChanTraffic`).
    pub(super) traffic: ChanTraffic,
}

impl Chan {
    pub(super) fn new(state: ChanState) -> Chan {
        Chan {
            state,
            delayed: Vec::new(),
            delayed_bytes: 0,
            traffic: ChanTraffic::default(),
        }
    }

    /// Holds a packet until the channel opens; false when the hold is full.
    pub(super) fn hold(&mut self, channel: u8, reliable: bool, data: &[u8], max_n: usize, max_bytes: usize) -> bool {
        if self.delayed.len() >= max_n || self.delayed_bytes + data.len() > max_bytes {
            return false;
        }
        self.delayed_bytes += data.len();
        self.delayed.push((channel, reliable, data.to_vec()));
        true
    }

    pub(super) fn take_delayed(&mut self) -> Vec<(u8, bool, Vec<u8>)> {
        self.delayed_bytes = 0;
        std::mem::take(&mut self.delayed)
    }
}

/// How often the engine logs what went over its channels (`State::traffic_report`).
pub(super) const TRAFFIC_EVERY: Duration = Duration::from_secs(30);

/// What went over one channel since the last traffic report: packets and bytes the game
/// sent, packets and bytes that came from the peer, packets the game read, and when
/// each last happened (those aren't reset by a report).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChanTraffic {
    pub sent: u64,
    pub sent_bytes: u64,
    pub got: u64,
    pub got_bytes: u64,
    pub read: u64,
    pub last_sent: Option<Instant>,
    pub last_got: Option<Instant>,
    pub last_read: Option<Instant>,
}

impl ChanTraffic {
    fn note_sent(&mut self, bytes: usize) {
        self.sent += 1;
        self.sent_bytes += bytes as u64;
        self.last_sent = Some(Instant::now());
    }

    fn note_got(&mut self, bytes: usize) {
        self.got += 1;
        self.got_bytes += bytes as u64;
        self.last_got = Some(Instant::now());
    }

    fn note_read(&mut self) {
        self.read += 1;
        self.last_read = Some(Instant::now());
    }

    /// Whether anything went either way since the last report.
    pub fn any(&self) -> bool {
        self.sent + self.got + self.read > 0
    }

    /// The counts start again (a report went out); the times stay.
    fn reset(&mut self) {
        *self = ChanTraffic { last_sent: self.last_sent, last_got: self.last_got, last_read: self.last_read, ..ChanTraffic::default() };
    }

    /// "sent 120 (4.1 KB), got 118 (3.9 KB, the last 15 ms ago), read 118 (the last 15
    /// ms ago)" as of `now`.
    pub fn describe(&self, now: Instant) -> String {
        let ago = |t: Option<Instant>| t.map_or_else(|| "never".to_string(), |t| format!("{} ago", ago(now.saturating_duration_since(t))));
        format!(
            "sent {} ({}, the last {}), got {} ({}, the last {}), read {} (the last {})",
            self.sent,
            size_text(self.sent_bytes),
            ago(self.last_sent),
            self.got,
            size_text(self.got_bytes),
            ago(self.last_got),
            self.read,
            ago(self.last_read)
        )
    }
}

/// A span for the log: "15 ms", "2.4 s", "3 min".
pub(super) fn ago(d: Duration) -> String {
    match d.as_millis() {
        ms @ 0..10_000 => format!("{ms} ms"),
        ms @ 10_000..180_000 => format!("{:.1} s", ms as f64 / 1000.0),
        ms => format!("{} min", ms / 60_000),
    }
}

/// A byte count for the log: "512 B", "4.1 KB", "2.3 MB".
pub(super) fn size_text(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

/// Received channel data waiting for the game, bounded (see INBOX_MAX_PACKETS).
///
/// One queue per socket and channel, each packet numbered as it arrives: a read takes
/// the oldest packet of the queues it wants, and the inbox's oldest goes first when it
/// is full, both in a step per queue. One shared queue made every read walk past the
/// packets of channels nobody reads (L-38).
#[derive(Default)]
pub(super) struct Inbox {
    /// By (channel, socket); emptied ones may stay (`KEPT_EMPTY_QUEUES`).
    pub(super) queues: HashMap<(u8, String), VecDeque<(u64, Packet)>>,
    pub(super) next: u64,
    pub(super) len: usize,
    pub(super) bytes: usize,
    /// Packets and bytes in `q` by sender.
    pub(super) per_peer: HashMap<PeerId, (usize, usize)>,
    /// Packets dropped to make room, ever.
    pub(super) dropped: u64,
    /// Public players' packets dropped for going past their share, ever.
    pub(super) over_share: u64,
    /// Threads in `Lan::wait_for_packet`, woken (`arrived`, with the state lock) by a
    /// push; with none, a push signals nothing.
    pub(super) waiting: usize,
    pub(super) arrived: Arc<Condvar>,
}

impl Inbox {
    pub(super) fn push(&mut self, p: Packet) {
        self.push_capped(p, INBOX_MAX_PACKETS, INBOX_MAX_BYTES);
    }

    /// A public player's packet: dropped if its packets already hold their share.
    pub(super) fn push_stranger(&mut self, p: Packet) {
        self.push_shared(p, PUBLIC_INBOX_PACKETS, PUBLIC_INBOX_BYTES);
    }

    pub(super) fn push_shared(&mut self, p: Packet, max_n: usize, max_bytes: usize) {
        let (n, bytes) = self.per_peer.get(&p.peer).copied().unwrap_or_default();
        if n + 1 > max_n || (n > 0 && bytes + p.data.len() > max_bytes) {
            self.over_share += 1;
            return;
        }
        self.push(p);
    }

    pub(super) fn push_capped(&mut self, p: Packet, max_n: usize, max_bytes: usize) {
        self.bytes += p.data.len();
        self.len += 1;
        let e = self.per_peer.entry(p.peer).or_default();
        e.0 += 1;
        e.1 += p.data.len();
        let n = self.next;
        self.next += 1;
        if let Some(q) = self.queues.get_mut(by(&(p.channel, p.socket.as_str()))) {
            q.push_back((n, p));
        } else {
            self.queues.insert((p.channel, p.socket.clone()), VecDeque::from([(n, p)]));
        }
        if self.waiting > 0 {
            self.arrived.notify_all();
        }
        while self.len > max_n || (self.bytes > max_bytes && self.len > 1) {
            let Some(old) = self.take(None, |_| true) else { break };
            self.dropped += 1;
            drop(old);
        }
    }

    /// The queue holding the oldest packet on a socket `want` accepts (and `channel`,
    /// if given).
    pub(super) fn oldest(&self, channel: Option<u8>, want: impl Fn(&str) -> bool) -> Option<&VecDeque<(u64, Packet)>> {
        self.queues
            .iter()
            .filter(|((ch, socket), q)| !q.is_empty() && channel.is_none_or(|c| c == *ch) && want(socket))
            .min_by_key(|(_, q)| q.front().map(|(n, _)| *n))
            .map(|(_, q)| q)
    }

    /// The size of the packet `take` would hand out.
    pub(super) fn peek_len(&self, channel: Option<u8>, want: impl Fn(&str) -> bool) -> Option<usize> {
        self.oldest(channel, want)?.front().map(|(_, p)| p.data.len())
    }

    /// The oldest packet on a socket `want` accepts (and `channel`, if given).
    pub(super) fn take(&mut self, channel: Option<u8>, want: impl Fn(&str) -> bool) -> Option<Packet> {
        let many = self.queues.len() > KEPT_EMPTY_QUEUES;
        let (k, q) = self
            .queues
            .iter_mut()
            .filter(|((ch, socket), q)| !q.is_empty() && channel.is_none_or(|c| c == *ch) && want(socket))
            .min_by_key(|(_, q)| q.front().map(|(n, _)| *n))?;
        let (_, p) = q.pop_front()?;
        if q.is_empty() && many {
            let k = k.clone();
            self.queues.remove(&k);
        }
        self.forget(&p);
        Some(p)
    }

    /// Takes a packet that left the inbox off the counts.
    pub(super) fn forget(&mut self, p: &Packet) {
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

    pub(super) fn retain(&mut self, mut f: impl FnMut(&Packet) -> bool) {
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
    pub(super) fn iter(&self) -> impl Iterator<Item = &Packet> {
        let mut all: Vec<&(u64, Packet)> = self.queues.values().flatten().collect();
        all.sort_by_key(|(n, _)| *n);
        all.into_iter().map(|(_, p)| p)
    }
}

impl Lan {
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
        if !s.chans.contains_key(by(&(peer, socket))) {
            if !auto_accept {
                return SendResult::NoChannel;
            }
            s.chans.insert(
                (peer, socket.to_string()),
                Chan::new(ChanState::Pending),
            );
            s.send_to(
                peer,
                &Msg::ChanRequest {
                    socket: socket.into(),
                },
            );
        }
        let chan = s.chans.get_mut(by(&(peer, socket))).unwrap();
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
            chan.traffic.note_sent(data.len());
            return SendResult::Held;
        }
        chan.traffic.note_sent(data.len());
        if reliable {
            let msg = Msg::ChanData { socket: socket.into(), channel, reliable, data: data.to_vec() };
            s.send_to(peer, &msg);
        } else {
            // A datagram has no number to take under the lock, and its send_to (or
            // the TCP queue it falls back to) needn't hold every other thread up.
            let conn = s.peers.get(&peer).filter(|p| p.gone.is_none()).map(|p| p.conn.clone());
            drop(s);
            if let Some(conn) = conn {
                conn.send_unreliable(&Msg::ChanData { socket: socket.into(), channel, reliable, data: data.to_vec() });
            }
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
        let c = s.chans.get(by(&(peer, socket)))?;
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

    /// Waits up to `timeout` for a packet on a socket `want` accepts (true: one waits),
    /// for a thread that reads its sockets' packets as they come rather than polling.
    pub fn wait_for_packet(&self, timeout: Duration, want: impl Fn(&str) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut s = self.inner.state.lock().unwrap();
        loop {
            if s.inbox.peek_len(None, &want).is_some() {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            let arrived = s.inbox.arrived.clone();
            s.inbox.waiting += 1;
            s = arrived.wait_timeout(s, left).unwrap().0;
            s.inbox.waiting -= 1;
        }
    }

    pub fn receive_where(&self, channel: Option<u8>, want: impl Fn(&str) -> bool) -> Option<Packet> {
        let mut s = self.inner.state.lock().unwrap();
        let p = s.inbox.take(channel, want)?;
        if let Some(c) = s.chans.get_mut(by(&(p.peer, p.socket.as_str()))) {
            c.traffic.note_read();
        }
        Some(p)
    }

    /// `peer`, its connection's state, what went over our channels with it since the
    /// last traffic report (`TRAFFIC_EVERY`) and how many of its packets wait unread, as
    /// one text for the log: for a front end to log when the game ends a session (had
    /// the link gone quiet first?).
    pub fn describe_link(&self, peer: PeerId) -> String {
        self.inner.state.lock().unwrap().link_text(peer, Instant::now(), false)
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
    /// Every TRAFFIC_EVERY, a line for each peer whose channels carried anything since
    /// the last one or that has packets waiting unread: what went each way by socket,
    /// what waits, and the connection's own state. A log then shows whether a session
    /// that ended had gone quiet first, or the game stopped reading, or neither.
    pub(super) fn traffic_report(&mut self, now: Instant) -> Vec<String> {
        let since = *self.traffic_at.get_or_insert(now);
        if now.saturating_duration_since(since) < TRAFFIC_EVERY {
            return Vec::new();
        }
        self.traffic_at = Some(now);
        let mut peers: Vec<PeerId> = self
            .chans
            .iter()
            .filter(|((peer, _), c)| c.traffic.any() || self.inbox.per_peer.contains_key(peer))
            .map(|((peer, _), _)| *peer)
            .collect();
        peers.sort_by_key(|p| p.to_string());
        peers.dedup();
        let span = ago(now.saturating_duration_since(since));
        let mut lines: Vec<String> =
            peers.iter().map(|p| format!("lan traffic over the last {span} with {}", self.link_text(*p, now, true))).collect();
        for c in self.chans.values_mut() {
            c.traffic.reset();
        }
        if !lines.is_empty() && (self.inbox.dropped > 0 || self.inbox.over_share > 0) {
            lines.push(format!(
                "lan inbox: {} packet(s) dropped so far for lack of room (the game read too slowly), {} of public players' past their share",
                self.inbox.dropped, self.inbox.over_share
            ));
        }
        lines
    }

    /// `peer`, its connection's state, what went over our channels with it since the
    /// last traffic report by socket (`only_active`: just the ones that carried
    /// something), and how many of its packets wait unread, as of `now`.
    pub(super) fn link_text(&self, peer: PeerId, now: Instant, only_active: bool) -> String {
        let mut chans: Vec<String> = self
            .chans
            .iter()
            .filter(|((p, _), c)| *p == peer && (!only_active || c.traffic.any()))
            .map(|((_, socket), c)| format!("{socket}: {}", c.traffic.describe(now)))
            .collect();
        chans.sort();
        if chans.is_empty() {
            chans.push("no channels".into());
        }
        let (unread, unread_bytes) = self.inbox.per_peer.get(&peer).copied().unwrap_or_default();
        let link = match self.peers.get(&peer) {
            Some(p) if p.gone.is_some() => format!("{}: its connection dropped, waiting for it to come back", p.name),
            Some(p) => format!(
                "{}: connected {} ago, {}, {} queued to send, {} stale datagrams dropped",
                p.name,
                ago(now.saturating_duration_since(p.joined)),
                if p.conn.udp_ready() { "datagrams over UDP" } else { "everything over TCP" },
                size_text(p.conn.queued_bytes() as u64),
                p.conn.dgram_drops.load(std::sync::atomic::Ordering::Relaxed)
            ),
            None => "no longer connected".to_string(),
        };
        format!("{peer} ({link}): {}; {unread} packet(s) ({}) wait unread", chans.join("; "), size_text(unread_bytes as u64))
    }

    pub(super) fn open_chan(&mut self, key: &(PeerId, String)) {
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
    pub(super) fn forget_closed_unread(&mut self, now: Instant) {
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

    pub(super) fn close_chan(&mut self, key: (PeerId, String), local: bool) {
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
}

impl State {
    /// A channel message from `from` (`State::handle` passes them on); `ordered`: it came
    /// off the TCP stream rather than as a datagram.
    pub(super) fn handle_chan(&mut self, from: PeerId, msg: Msg, ordered: bool) {
        match msg {
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
                match self.chans.get_mut(by(&(from, socket.as_str()))) {
                    Some(c) if c.state == ChanState::Open => {
                        c.traffic.note_got(data.len());
                        let p = Packet { peer: from, socket, channel, data, ordered };
                        if self.peers.get(&from).is_some_and(|p| p.stranger) {
                            self.inbox.push_stranger(p);
                        } else {
                            self.inbox.push(p);
                        }
                    }
                    None => {
                        self.chans.insert(
                            (from, socket.clone()),
                            Chan::new(ChanState::Requested),
                        );
                        self.events
                            .push_back(Event::ChanRequest { peer: from, socket });
                    }
                    Some(_) => {}
                }
            }
            Msg::ChanClose { socket } => self.close_chan((from, socket), false),
            _ => {}
        }
    }
}
