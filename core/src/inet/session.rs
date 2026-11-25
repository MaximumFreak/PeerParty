//! One tunnel session: the handshake (INIT, COOKIE, CONFIRM), the sealed data packets
//! and the frames inside them (stream segments and acks, mesh datagrams, pings, IPv6
//! addresses, goodbyes), and what a tick or a packet leaves to do (`Act`).

use super::*;

/// The first byte of every tunnel packet. STUN's is below 0x40, the DHT's is 'd'.
pub(super) const MAGIC: u8 = 0xC7;
/// The tunnel's own version, apart from the mesh's.
pub(super) const PROTO: u8 = 1;
pub(super) const T_INIT: u8 = 1;
pub(super) const T_CONFIRM: u8 = 2;
pub(super) const T_DATA: u8 = 3;
/// Public sessions' answer to a first INIT: a cookie, not a session (see COOKIE_FOR).
pub(super) const T_COOKIE: u8 = 4;
pub(super) const HEAD: usize = 3;
pub(super) const INIT_LEN: usize = HEAD + 8 + 48 + 16;
/// An INIT carrying the cookie it was answered with, before its MAC.
pub(super) const INIT_COOKIE_LEN: usize = INIT_LEN + 16;
/// [head][room id][the INIT's blob][cookie]: no bigger than the INIT it answers.
pub(super) const COOKIE_LEN: usize = HEAD + 8 + 48 + 16;
pub(super) const CONFIRM_AD: usize = HEAD + 8 + 48 + 48;
pub(super) const F_SEG: u8 = 1;
pub(super) const F_ACK: u8 = 2;
pub(super) const F_DGRAM: u8 = 3;
pub(super) const F_PING: u8 = 4;
pub(super) const F_PONG: u8 = 5;
pub(super) const F_CLOSE: u8 = 6;
/// After F_CLOSE: the sender is leaving for good (it quit, switched internet play off,
/// or blocked us), not just ending this session. Its mesh peer then leaves at once
/// instead of being waited for (`Host::tunnel_down`).
pub(super) const CLOSE_LEAVING: u8 = 1;
/// What `frames` ends a session with for such a goodbye.
pub(super) const LEAVING: &str = "\0leaving";
/// Our IPv6 addresses (`[n]`, then n × 16-byte address and port, little-endian), sent
/// inside the session in a packet of its own (older peers skip a frame they don't know
/// and the rest of its packet). The peer then tries them: direct IPv6 needs no NAT.
pub(super) const F_ADDRS6: u8 = 7;
/// IPv6 dual stack: the session starts over IPv4 (the DHT and STUN are IPv4) and moves
/// to an IPv6 address the peer sent once a packet from there decrypts.
pub(super) const MAX_ALT: usize = 4;
pub(super) const PROBE_FOR: Duration = Duration::from_secs(20);
pub(super) const PROBE_EVERY: Duration = Duration::from_secs(1);
/// Back to IPv4 when the IPv6 path has been quiet this long (the peer pings every
/// KEEPALIVE when idle).
pub(super) const PATH_QUIET: Duration = Duration::from_secs(12);
pub(super) const ADVERT_EVERY: Duration = Duration::from_secs(60);
/// Plaintext bytes in one data packet, for the stream (datagrams may be bigger).
pub(super) const MAX_PLAIN: usize = 1200;
pub(super) const INIT_EVERY: Duration = Duration::from_millis(250);
pub(super) const INIT_FAST_FOR: Duration = Duration::from_secs(5);
pub(super) const INIT_SLOW: Duration = Duration::from_secs(1);
pub(super) const KEEPALIVE: Duration = Duration::from_secs(5);
pub(super) const DEAD_AFTER: Duration = Duration::from_secs(30);
/// Our mesh finished its side of a tunnelled connection (it moved to a better path):
/// the session stays until the peer has everything we sent, this long at most.
pub(super) const FINISH_FOR: Duration = Duration::from_secs(10);
pub(super) const MAX_EARLY: usize = 8 << 20;
pub(super) const PUBLIC_MAX_EARLY: usize = 1 << 20;
pub(super) const PUBLIC_HALF_OPEN: Duration = Duration::from_secs(10);
pub(super) const PUBLIC_PER_KEY: usize = 4;
pub(super) const PUBLIC_HALF_OPEN_PER_IP: usize = 8;
/// ... and in all, whatever addresses they come from (a UDP packet's source is whatever
/// its sender wrote there): half the session table. The other half stays free for
/// finished sessions and for our own punches to players the DHT found, so a flood of
/// first packets from made-up addresses can't keep us from meeting anyone. A player we
/// punch as well gets in through that punch.
pub(super) const PUBLIC_HALF_OPEN_TOTAL: usize = PUBLIC_MAX_SESSIONS / 2;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PeerInfo {
    pub(super) id: PeerId,
    pub(super) game: String,
    pub(super) name: String,
}

impl PeerInfo {
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut b = self.id.0.to_vec();
        for s in [&self.game, &self.name] {
            let s = &s.as_bytes()[..s.len().min(1024)];
            b.extend_from_slice(&(s.len() as u16).to_le_bytes());
            b.extend_from_slice(s);
        }
        b
    }

    pub(super) fn decode(b: &[u8]) -> Option<PeerInfo> {
        let id = PeerId(b.get(..16)?.try_into().ok()?);
        let mut rest = &b[16..];
        let mut strs = Vec::new();
        for _ in 0..2 {
            let len = u16::from_le_bytes(rest.get(..2)?.try_into().ok()?) as usize;
            strs.push(String::from_utf8_lossy(rest.get(2..2 + len)?).into_owned());
            rest = &rest[2 + len..];
        }
        let name = strs.pop()?;
        let game = strs.pop()?;
        Some(PeerInfo { id, game, name })
    }
}

/// What a session is part of: a room (the players given its code; each signs with its own
/// key, `Config::room_key`) or public sessions (anyone who runs the game with them on: a
/// verified key per player, a block list, tighter limits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Room,
    Public,
}

pub(super) struct Session {
    pub(super) mode: Mode,
    pub(super) addr: SocketAddr,
    pub(super) epoch: u64,
    pub(super) eph: Ephemeral,
    pub(super) peer: Option<[u8; 48]>,
    pub(super) keys: Option<Keys>,
    pub(super) confirmed: bool,
    pub(super) info: Option<PeerInfo>,
    pub(super) counter: u64,
    pub(super) replay: Replay,
    pub(super) stream: Stream,
    pub(super) created: Instant,
    pub(super) last_rx: Instant,
    pub(super) last_tx: Instant,
    pub(super) last_init: Option<Instant>,
    pub(super) last_confirm: Option<Instant>,
    pub(super) filter: HelloRewriter,
    pub(super) early: Vec<u8>,
    pub(super) bridge: Option<Bridge>,
    pub(super) bridging: bool,
    pub(super) next_bridge: Instant,
    /// When our mesh finished writing to the bridge (see FINISH_FOR).
    pub(super) finishing: Option<Instant>,
    pub(super) rtt: Option<Duration>,
    /// The peer started it (it sent the first INIT we saw).
    pub(super) inbound: bool,
    /// The cookie the peer answered our INIT with (public sessions), sent in each INIT
    /// after it.
    pub(super) cookie: Option<[u8; 16]>,
    /// Public sessions: the player's verified key and its fingerprint.
    pub(super) key: Option<[u8; public::KEY_LEN]>,
    pub(super) fp: Option<String>,
    /// Where data goes: `addr`, or one of `alt` once a packet from it decrypted.
    pub(super) path: SocketAddr,
    /// The last packet that came in on `path`.
    pub(super) path_rx: Instant,
    /// The peer's IPv6 addresses (F_ADDRS6), checked.
    pub(super) alt: Vec<SocketAddr>,
    pub(super) probe_until: Option<Instant>,
    pub(super) next_probe: Instant,
    pub(super) next_advert: Instant,
    /// Keeps the IPv4 path (and its NAT mapping) open while on IPv6.
    pub(super) v4_ping: Instant,
    /// How long it punches before giving up, while unanswered.
    pub(super) punch_for: Duration,
}

/// What a tick or a packet leaves to do once the sessions are unlocked.
pub(super) enum Act {
    Bridge(SocketAddr, u64, PeerId),
    /// A session's bridge goes; whether its peer said it's leaving for good.
    Gone(Option<Bridge>, bool),
    Log(String),
    /// A session ended: whether it had ever connected.
    Ended(SocketAddr, bool),
    /// A handshake with this address finished: it's live (see Cand::answered).
    Heard(SocketAddr),
    Ban(SocketAddr),
    /// A public player's key to remember (`public::Pins::remember`, which writes a file).
    RememberPin(PeerId, [u8; public::KEY_LEN]),
}

pub(super) struct Ctx<'a> {
    pub(super) sock: &'a UdpSocket,
    pub(super) sock6: Option<&'a UdpSocket>,
    pub(super) allow_private: bool,
    pub(super) room_id: [u8; 8],
    /// Borrowed, not copied: key material lives in one place (Shared), wiped on drop.
    pub(super) init_key: &'a [u8; 32],
    pub(super) psk: &'a [u8; 32],
    pub(super) me: &'a PeerInfo,
    #[cfg(any(test, feature = "test-util"))]
    pub(super) loss: u8,
    /// Our key, which signs our CONFIRMs (the public one, or the room's).
    pub(super) signer: Option<&'a public::IdentityKey>,
}

impl Ctx<'_> {
    pub(super) fn send(&self, to: SocketAddr, d: &[u8]) {
        #[cfg(any(test, feature = "test-util"))]
        if self.loss > 0 && crypto::random_array::<1>()[0] as u32 * 100 / 256 < self.loss as u32 {
            return;
        }
        let sock = match (to.is_ipv6(), self.sock6) {
            (false, _) => self.sock,
            (true, Some(s)) => s,
            (true, None) => return,
        };
        let _ = sock.send_to(d, to);
    }
}

/// An IPv6 address worth trying: global unicast (no Teredo or documentation range), or
/// loopback and unique-local in tests.
pub(super) fn usable_v6(a: SocketAddr, allow_private: bool) -> bool {
    let SocketAddr::V6(v) = a else { return false };
    if v.port() == 0 {
        return false;
    }
    let ip = *v.ip();
    let s = ip.segments();
    let global = s[0] & 0xe000 == 0x2000 && !(s[0] == 0x2001 && (s[1] == 0 || s[1] == 0xdb8));
    global || (allow_private && (ip.is_loopback() || s[0] & 0xfe00 == 0xfc00))
}

pub(super) fn addrs6_frame(own: &[SocketAddr]) -> Vec<u8> {
    let own: Vec<&SocketAddr> = own.iter().filter(|a| a.is_ipv6()).take(MAX_ALT).collect();
    let mut f = vec![F_ADDRS6, own.len() as u8];
    for a in own {
        if let SocketAddr::V6(v) = a {
            f.extend_from_slice(&v.ip().octets());
            f.extend_from_slice(&v.port().to_le_bytes());
        }
    }
    f
}

pub(super) fn ping_frame(s: &Session, now: Instant) -> Vec<u8> {
    let mut ping = vec![F_PING];
    ping.extend_from_slice(&(now.duration_since(s.created).as_micros() as u64).to_le_bytes());
    ping
}

pub(super) fn head(t: u8) -> [u8; HEAD] {
    [MAGIC, PROTO, t]
}

impl Session {
    pub(super) fn new(addr: SocketAddr, epoch: u64, now: Instant, inbound: bool, mode: Mode) -> Session {
        Session {
            mode,
            addr,
            epoch,
            eph: Ephemeral::new(),
            peer: None,
            keys: None,
            confirmed: false,
            info: None,
            counter: 0,
            replay: Replay::default(),
            stream: Stream::default(),
            created: now,
            last_rx: now,
            last_tx: now,
            last_init: None,
            last_confirm: None,
            filter: HelloRewriter::default(),
            early: Vec::new(),
            bridge: None,
            bridging: false,
            next_bridge: now,
            finishing: None,
            rtt: None,
            inbound,
            cookie: None,
            key: None,
            fp: None,
            path: addr,
            path_rx: now,
            alt: Vec::new(),
            probe_until: None,
            next_probe: now,
            next_advert: now,
            v4_ping: now,
            punch_for: PUNCH_FOR,
        }
    }

    pub(super) fn send_init(&mut self, cx: &Ctx, now: Instant) {
        let mut p = head(T_INIT).to_vec();
        p.extend_from_slice(&cx.room_id);
        p.extend_from_slice(&self.eph.blob);
        if let Some(c) = &self.cookie {
            p.extend_from_slice(c);
        }
        let tag = crypto::mac16(cx.init_key, &[&p]);
        p.extend_from_slice(&tag);
        cx.send(self.addr, &p);
        self.last_init = Some(now);
    }

    pub(super) fn send_confirm(&mut self, cx: &Ctx, now: Instant) {
        let (Some(peer), Some(keys)) = (&self.peer, &self.keys) else { return };
        let mut p = head(T_CONFIRM).to_vec();
        p.extend_from_slice(&cx.room_id);
        p.extend_from_slice(&self.eph.blob);
        p.extend_from_slice(peer);
        let body = match cx.signer {
            Some(k) => public::advert(k, &cx.room_id, &self.eph.blob, peer, &cx.me.encode()),
            None => cx.me.encode(),
        };
        let sealed = keys.seal(crypto::CONFIRM_COUNTER, &p, &body);
        p.extend_from_slice(&sealed);
        cx.send(self.addr, &p);
        self.last_confirm = Some(now);
    }

    pub(super) fn set_peer(&mut self, cx: &Ctx, blob: [u8; 48]) -> bool {
        match Keys::derive(cx.psk, &self.eph, &blob) {
            Some(k) => {
                self.peer = Some(blob);
                self.keys = Some(k);
                true
            }
            None => false,
        }
    }

    pub(super) fn send_plain(&mut self, cx: &Ctx, plain: &[u8], now: Instant) {
        self.send_plain_to(cx, self.path, plain, now);
    }

    pub(super) fn send_plain_to(&mut self, cx: &Ctx, to: SocketAddr, plain: &[u8], now: Instant) {
        let Some(keys) = &self.keys else { return };
        let c = self.counter;
        self.counter += 1;
        let mut p = Vec::with_capacity(HEAD + 8 + plain.len() + 16);
        p.extend_from_slice(&head(T_DATA));
        p.extend_from_slice(&c.to_le_bytes());
        let ad = p.len();
        p.extend_from_slice(plain);
        keys.seal_in_place(c, &mut p, ad);
        cx.send(to, &p);
        // Only what goes the path counts toward its keepalive: probes of other addresses
        // don't keep the path's NAT mapping open.
        if to == self.path {
            self.last_tx = now;
        }
    }

    /// Sends the stream's segments and acknowledgements, packed into packets.
    pub(super) fn flush(&mut self, cx: &Ctx, now: Instant) {
        let outs = self.stream.poll(now);
        let mut plain = Vec::with_capacity(MAX_PLAIN);
        for o in outs {
            let mut f = Vec::new();
            match o {
                Out::Seg { seq, data } => {
                    f.push(F_SEG);
                    f.extend_from_slice(&seq.to_le_bytes());
                    f.extend_from_slice(&(data.len() as u16).to_le_bytes());
                    f.extend_from_slice(&data);
                }
                Out::Ack { next, sack } => {
                    f.push(F_ACK);
                    f.extend_from_slice(&next.to_le_bytes());
                    f.extend_from_slice(&sack.to_le_bytes());
                }
            }
            if plain.len() + f.len() > MAX_PLAIN && !plain.is_empty() {
                self.send_plain(cx, &plain, now);
                plain.clear();
            }
            plain.extend_from_slice(&f);
        }
        if !plain.is_empty() {
            self.send_plain(cx, &plain, now);
        }
    }

    pub(super) fn send_dgram(&mut self, cx: &Ctx, data: &[u8], now: Instant) {
        let mut plain = Vec::with_capacity(data.len() + 3);
        plain.push(F_DGRAM);
        plain.extend_from_slice(&(data.len().min(u16::MAX as usize) as u16).to_le_bytes());
        plain.extend_from_slice(&data[..data.len().min(u16::MAX as usize)]);
        self.send_plain(cx, &plain, now);
    }

    pub(super) fn send_close(&mut self, cx: &Ctx, now: Instant, leaving: bool) {
        let frame: &[u8] = if leaving { &[F_CLOSE, CLOSE_LEAVING] } else { &[F_CLOSE] };
        for _ in 0..3 {
            self.send_plain(cx, frame, now);
        }
    }

    /// In-order stream bytes from the peer, for our mesh (or kept until the bridge is up).
    pub(super) fn deliver(&mut self, data: Vec<u8>) -> Result<(), String> {
        if data.is_empty() {
            return Ok(());
        }
        let (Some(b), Some(info)) = (&self.bridge, &self.info) else {
            let max = if self.mode == Mode::Public { PUBLIC_MAX_EARLY } else { MAX_EARLY };
            if self.early.len() + data.len() > max {
                return Err("too much data before the tunnel's mesh connection was up".into());
            }
            self.early.extend_from_slice(&data);
            return Ok(());
        };
        let name = (self.mode == Mode::Public).then_some(info.name.as_str());
        // Past the Hello, the segment's own bytes go on (no copy).
        let out = peer_hello(&mut self.filter, &data, info.id, b.relay_port, name)?.unwrap_or(data);
        if !out.is_empty() {
            let len = out.len();
            if b.to_tcp_bytes.fetch_add(len, Ordering::Relaxed) + len > crate::mesh::SEND_QUEUE_MAX {
                b.to_tcp_bytes.fetch_sub(len, Ordering::Relaxed);
                return Err("our mesh stopped taking the peer's data (send queue full)".into());
            }
            if b.to_tcp.send(out).is_err() {
                b.to_tcp_bytes.fetch_sub(len, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    pub(super) fn status(&self) -> PeerStatus {
        PeerStatus {
            tag: redact::addr(self.addr),
            key: self.fp.clone(),
            id: self.info.as_ref().map(|i| i.id),
            name: self.info.as_ref().map(|i| i.name.clone()).unwrap_or_default(),
            connected: self.confirmed,
            bridged: self.bridge.is_some(),
            rtt: self.rtt.or(self.stream.rtt()),
            resent: self.stream.resent,
            ipv6: self.path.is_ipv6(),
        }
    }
}

/// What a cookie is the MAC of: the address (IPv4 as IPv4-mapped), port, blob and slot.
pub(super) fn cookie_input(from: SocketAddr, blob: &[u8; 48], slot: u64) -> [u8; 16 + 2 + 48 + 8] {
    let ip = match from.ip() {
        IpAddr::V4(v) => v.to_ipv6_mapped().octets(),
        IpAddr::V6(v) => v.octets(),
    };
    let mut b = [0u8; 16 + 2 + 48 + 8];
    b[..16].copy_from_slice(&ip);
    b[16..18].copy_from_slice(&from.port().to_le_bytes());
    b[18..66].copy_from_slice(blob);
    b[66..].copy_from_slice(&slot.to_le_bytes());
    b
}

pub(super) fn on_packet(sh: &Arc<Shared>, ss: &mut HashMap<SocketAddr, Session>, from: SocketAddr, d: &[u8], now: Instant) -> Vec<Act> {
    let cx = sh.cx();
    let mut acts = Vec::new();
    let tag = || redact::addr(from);
    match d[2] {
        T_INIT => {
            let n = d.len();
            if !(n == INIT_LEN || n == INIT_COOKIE_LEN) || d[HEAD..HEAD + 8] != cx.room_id {
                return acts;
            }
            if !crypto::mac16_ok(cx.init_key, &[&d[..n - 16]], &d[n - 16..]) {
                return acts;
            }
            let blob: [u8; 48] = d[HEAD + 8..HEAD + 56].try_into().unwrap();
            if sh.mode() == Mode::Public {
                // Anyone can send these: limit what a stranger can make us do.
                if sh.is_blocked_addr(from, now) {
                    return acts;
                }
                // Nothing is kept for an address until it shows it gets our packets
                // (COOKIE_FOR): it's sent a cookie, and comes back with it.
                if !ss.contains_key(&from) && !(n == INIT_COOKIE_LEN && sh.cookie_ok(from, &blob, &d[INIT_LEN - 16..INIT_LEN], now)) {
                    if sh.cookies.lock().unwrap().allow(IpAddr::V4(Ipv4Addr::UNSPECIFIED), now) {
                        let mut p = head(T_COOKIE).to_vec();
                        p.extend_from_slice(&cx.room_id);
                        p.extend_from_slice(&blob);
                        p.extend_from_slice(&sh.cookie(from, &blob, sh.cookie_slot(now)));
                        cx.send(from, &p);
                    }
                    return acts;
                }
                let fresh = ss.get(&from).is_none_or(|s| s.peer != Some(blob));
                if fresh {
                    if !ss.contains_key(&from) {
                        // Half-open counts only handshakes it started: our own punches to
                        // its other ports (an earlier run's, found on the DHT) mustn't
                        // lock its live port out.
                        let same = || ss.values().filter(|s| s.addr.ip() == from.ip());
                        if same().count() >= PUBLIC_PER_IP
                            || same().filter(|s| s.inbound && !s.confirmed).count() >= PUBLIC_HALF_OPEN_PER_IP
                            || ss.values().filter(|s| s.inbound && !s.confirmed).count() >= PUBLIC_HALF_OPEN_TOTAL
                        {
                            return acts;
                        }
                    }
                    if !sh.limit.lock().unwrap().allow(from.ip(), now) {
                        return acts;
                    }
                }
            }
            if !ss.contains_key(&from) {
                if ss.len() >= sh.max_sessions {
                    // A handshake from a live address beats a punch to one that has
                    // never answered (often a stale DHT find): drop the oldest of those.
                    let stale = ss
                        .values()
                        .filter(|s| !s.inbound && !s.confirmed && s.peer.is_none())
                        .min_by_key(|s| s.created)
                        .map(|s| s.addr);
                    match stale {
                        Some(old) => {
                            ss.remove(&old);
                            acts.push(Act::Ended(old, false));
                        }
                        None => {
                            let mut logged = sh.full_logged.lock().unwrap();
                            if logged.len() < 1024 && logged.insert(from) {
                                acts.push(Act::Log(format!(
                                    "{} is punching through to us, but all {} sessions are in use: ignored",
                                    tag(),
                                    sh.max_sessions
                                )));
                            }
                            return acts;
                        }
                    }
                }
                acts.push(Act::Log(format!("{} is punching through to us", tag())));
                ss.insert(from, Session::new(from, sh.epoch(), now, true, sh.mode()));
            }
            let s = ss.get_mut(&from).unwrap();
            if s.peer != Some(blob) {
                if s.confirmed {
                    // A first packet proves nothing about who sent it (in public
                    // sessions anyone can make one, and a source address can be made
                    // up), so it doesn't end a session that is alive: only one that has
                    // missed two keepalives, as when the game at the other end was
                    // started again. At 2 s, an idle session could be reset at will.
                    if now.duration_since(s.last_rx) < sh.cfg.replace_quiet {
                        return acts;
                    }
                    // The peer started over (it lost our session): so do we.
                    let old = ss.remove(&from).unwrap();
                    acts.push(Act::Log(format!("{} reconnected", tag())));
                    acts.push(Act::Gone(old.bridge, false));
                    ss.insert(from, Session::new(from, sh.epoch(), now, true, sh.mode()));
                }
                let s = ss.get_mut(&from).unwrap();
                if !s.set_peer(&cx, blob) {
                    return acts;
                }
            }
            let s = ss.get_mut(&from).unwrap();
            if s.last_confirm.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100)) {
                s.send_confirm(&cx, now);
            }
            if !s.confirmed && s.last_init.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100)) {
                s.send_init(&cx, now);
            }
        }
        T_COOKIE => {
            // Our INIT was answered with a cookie: it goes in our INITs from now on.
            if d.len() != COOKIE_LEN || d[HEAD..HEAD + 8] != cx.room_id {
                return acts;
            }
            let Some(s) = ss.get_mut(&from) else { return acts };
            if s.confirmed || d[HEAD + 8..HEAD + 56] != s.eph.blob[..] {
                return acts;
            }
            s.cookie = Some(d[HEAD + 56..COOKIE_LEN].try_into().unwrap());
            if s.last_init.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100)) {
                s.send_init(&cx, now);
            }
        }
        T_CONFIRM => {
            if d.len() < CONFIRM_AD + 16 || d[HEAD..HEAD + 8] != cx.room_id {
                return acts;
            }
            let Some(s) = ss.get_mut(&from) else { return acts };
            let sender: [u8; 48] = d[HEAD + 8..HEAD + 56].try_into().unwrap();
            if d[HEAD + 56..CONFIRM_AD] != s.eph.blob[..] || s.confirmed {
                return acts;
            }
            if s.peer != Some(sender) && !s.set_peer(&cx, sender) {
                return acts;
            }
            let Some(plain) = s.keys.as_ref().and_then(|k| k.open(crypto::CONFIRM_COUNTER, &d[..CONFIRM_AD], &d[CONFIRM_AD..])) else {
                return acts;
            };
            // The body is signed with the player's own key, over both sides' fresh
            // keys, so it can't be replayed or moved to another session.
            let Some((body, signed_by)) = public::open_advert(&plain, &cx.room_id, &sender, &s.eph.blob) else {
                if sh.mode() == Mode::Room {
                    // In a room only an older PeerParty sends these: the code is right.
                    ss.remove(&from);
                    acts.push(Act::Ban(from));
                    acts.push(Act::Log(format!(
                        "{} has the room code but sent an unsigned handshake: an older peerparty? Everyone in the room needs this version",
                        tag()
                    )));
                }
                return acts;
            };
            let Some(mut info) = PeerInfo::decode(body) else { return acts };
            info.name = public::sanitize_name(&info.name);
            if info.id == cx.me.id || cx.signer.is_some_and(|k| k.public() == signed_by) {
                ss.remove(&from);
                acts.push(Act::Ban(from));
                acts.push(Act::Log(format!("{} is ourselves (our own announcement)", tag())));
                return acts;
            }
            // A player's id is its key's hash, in rooms and public sessions alike: the
            // signature just checked is that key's, so nobody passes as another, and
            // nobody can be first to an id that isn't theirs.
            if info.id != PeerId::of_key(&signed_by) {
                ss.remove(&from);
                acts.push(Act::Ban(from));
                acts.push(Act::Log(format!(
                    "{} ({}) claims to be {}, which isn't its key's id ({}): refused",
                    tag(),
                    info.name,
                    info.id,
                    public::fingerprint(&signed_by)
                )));
                return acts;
            }
            // Only public players are told apart by key elsewhere (status, blocking).
            let key = sh.pubs.as_ref().map(|_| signed_by);
            let fp = key.as_ref().map(public::fingerprint);
            if info.game != cx.me.game {
                ss.remove(&from);
                acts.push(Act::Ban(from));
                acts.push(Act::Log(format!("{} ({}) is in this room but plays {}, not {}", tag(), info.name, info.game, cx.me.game)));
                return acts;
            }
            if let (Some(p), Some(k), Some(fp)) = (&sh.pubs, &key, &fp) {
                if p.blocked.is_blocked(fp) {
                    ss.remove(&from);
                    sh.block_addr(from, Some(fp.as_str()), now);
                    acts.push(Act::Ban(from));
                    acts.push(Act::Log(format!("{} is {fp}, on the block list: ignored", tag())));
                    return acts;
                }
                let friend = p.friends.is_blocked(fp);
                if p.friends_only && !friend {
                    ss.remove(&from);
                    acts.push(Act::Ban(from));
                    acts.push(Act::Log(format!("{} is {fp}, not on the friends list (friends-only): refused", tag())));
                    return acts;
                }
                if !friend && !p.pins.knows_key(k) && sh.new_keys_cooling(from.ip(), now) {
                    ss.remove(&from);
                    acts.push(Act::Ban(from));
                    acts.push(Act::Log(format!(
                        "{} is a new key ({fp}) from the address of a player blocked lately: refused for now",
                        tag()
                    )));
                    return acts;
                }
                // Written to the file once the sessions' lock is let go.
                acts.push(Act::RememberPin(info.id, *k));
                if ss.values().filter(|o| o.confirmed && o.fp.as_deref() == Some(fp.as_str())).count() >= PUBLIC_PER_KEY {
                    ss.remove(&from);
                    acts.push(Act::Log(format!("{} is {fp} again: {PUBLIC_PER_KEY} sessions with that key already, refused", tag())));
                    acts.push(Act::Ended(from, false));
                    return acts;
                }
            }
            if let Some(other) = ss.values().find(|o| o.addr != from && o.confirmed && o.info.as_ref().is_some_and(|i| i.id == info.id)) {
                let at = redact::addr(other.addr);
                let old = other.addr;
                let quiet = now.duration_since(other.last_rx);
                if quiet < sh.cfg.replace_quiet {
                    // Already up at another of its addresses, and that one is alive: keep it.
                    // This address counts as one that didn't answer, so discovery backs off
                    // it (and lets a found one go) instead of a handshake every 2 s.
                    ss.remove(&from);
                    acts.push(Act::Log(format!("{} is {} again (already connected at {at})", tag(), info.name)));
                    acts.push(Act::Ended(from, false));
                    return acts;
                }
                // The old session went quiet: the peer started over (a restarted game).
                let gone = ss.remove(&old).unwrap();
                acts.push(Act::Log(format!(
                    "{} is {} again; its old session at {at} has been quiet for {}s: replaced",
                    tag(),
                    info.name,
                    quiet.as_secs()
                )));
                acts.push(Act::Gone(gone.bridge, false));
                acts.push(Act::Ended(old, true));
            }
            let s = ss.get_mut(&from).unwrap();
            s.confirmed = true;
            s.last_rx = now;
            // Only now, with the handshake finished: an init alone proves nothing (a
            // public one can come from anyone, from any source address).
            acts.push(Act::Heard(from));
            match &fp {
                Some(fp) => acts.push(Act::Log(format!("connected to public player {} ({}, key {fp}) at {}", info.name, info.id, tag()))),
                None => acts.push(Act::Log(format!("connected to {} ({}) at {}", info.name, info.id, tag()))),
            }
            s.info = Some(info);
            s.key = key;
            s.fp = fp;
            s.send_confirm(&cx, now);
        }
        T_DATA => {
            if d.len() < HEAD + 8 + 16 {
                return acts;
            }
            // From the session's own address, or from an IPv6 address its peer sent us.
            let key = if ss.contains_key(&from) {
                from
            } else if from.is_ipv6() {
                match ss.iter().find(|(_, s)| s.confirmed && s.alt.contains(&from)) {
                    Some((k, _)) => *k,
                    None => return acts,
                }
            } else {
                return acts;
            };
            let s = ss.get_mut(&key).unwrap();
            if !s.confirmed {
                return acts;
            }
            let c = u64::from_le_bytes(d[HEAD..HEAD + 8].try_into().unwrap());
            if c == crypto::CONFIRM_COUNTER || !s.replay.fresh(c) {
                return acts;
            }
            let Some(plain) = s.keys.as_ref().and_then(|k| k.open(c, &d[..HEAD + 8], &d[HEAD + 8..])) else {
                return acts;
            };
            s.replay.commit(c);
            s.last_rx = now;
            if from == s.path {
                s.path_rx = now;
            } else if from != s.addr && !s.path.is_ipv6() {
                // It decrypted, so it's the peer: prefer the direct IPv6 path.
                s.path = from;
                s.path_rx = now;
                s.probe_until = None;
                acts.push(Act::Log(format!("{}: direct over IPv6 now ({})", redact::addr(key), tag())));
            }
            let r = frames(s, &cx, &plain, now);
            // Acknowledgements may have opened the window for data waiting, or asked for a
            // resend: sent now, not at the next tick. (An acknowledgement alone waits for
            // the tick, as a delayed one.)
            if r.is_ok() && s.stream.wants_send() {
                s.flush(&cx, now);
            }
            if let Err(e) = r {
                let old = ss.remove(&key).unwrap();
                let leaving = e == LEAVING;
                if !e.is_empty() && !leaving {
                    acts.push(Act::Log(format!("{}: {e}", redact::addr(key))));
                }
                acts.push(Act::Gone(old.bridge, leaving));
                acts.push(Act::Ended(key, true));
            }
        }
        _ => {}
    }
    acts
}

/// Handles a data packet's frames. `Err` ends the session (an empty one: a goodbye).
pub(super) fn frames(s: &mut Session, cx: &Ctx, mut p: &[u8], now: Instant) -> Result<(), String> {
    // One pong per packet: a packet of nothing but pings would otherwise be thousands
    // of packets back, sealed and sent one by one under the sessions' lock.
    let mut ponged = false;
    while let Some(&f) = p.first() {
        p = &p[1..];
        match f {
            F_SEG => {
                let (Some(seq), Some(len)) = (p.get(..4), p.get(4..6)) else { return Err("bad segment".into()) };
                let seq = u32::from_le_bytes(seq.try_into().unwrap());
                let len = u16::from_le_bytes(len.try_into().unwrap()) as usize;
                if len > stream::MAX_SEG_IN {
                    return Err("a segment bigger than any peerparty sends".into());
                }
                let data = p.get(6..6 + len).ok_or("bad segment")?.to_vec();
                p = &p[6 + len..];
                let ready = s.stream.on_segment(seq, data);
                s.deliver(ready)?;
            }
            F_ACK => {
                let a = p.get(..12).ok_or("bad ack")?;
                let next = u32::from_le_bytes(a[..4].try_into().unwrap());
                let sack = u64::from_le_bytes(a[4..].try_into().unwrap());
                p = &p[12..];
                s.stream.on_ack(next, sack, now);
            }
            F_DGRAM => {
                let len = u16::from_le_bytes(p.get(..2).ok_or("bad datagram")?.try_into().unwrap()) as usize;
                let data = p.get(2..2 + len).ok_or("bad datagram")?;
                p = &p[2 + len..];
                // A mesh datagram names its sender, and our mesh takes it at its word
                // from this address (every tunnel ends at this one). Here is where it is
                // known who sent it: the player this session was proved to be, and no
                // other.
                if s.info.as_ref().is_none_or(|i| !crate::mesh::datagram_is_from(data, i.id)) {
                    continue;
                }
                if let Some(b) = &s.bridge {
                    let port = b.mesh_udp.load(Ordering::Relaxed);
                    if port != 0 {
                        let _ = b.relay.send_to(data, (Ipv4Addr::LOCALHOST, port));
                    }
                }
            }
            F_PING => {
                let t = p.get(..8).ok_or("bad ping")?.to_vec();
                p = &p[8..];
                if !std::mem::replace(&mut ponged, true) {
                    let mut pong = vec![F_PONG];
                    pong.extend_from_slice(&t);
                    s.send_plain(cx, &pong, now);
                }
            }
            F_PONG => {
                let t = u64::from_le_bytes(p.get(..8).ok_or("bad pong")?.try_into().unwrap());
                p = &p[8..];
                let sent = Duration::from_micros(t);
                s.rtt = now.duration_since(s.created).checked_sub(sent);
            }
            F_ADDRS6 => {
                let n = *p.first().ok_or("bad addresses")? as usize;
                let body = p.get(1..1 + n * 18).ok_or("bad addresses")?;
                p = &p[1 + n * 18..];
                if cx.sock6.is_some() {
                    s.alt = body
                        .as_chunks::<18>()
                        .0
                        .iter()
                        .map(|c| SocketAddr::from((Ipv6Addr::from(<[u8; 16]>::try_from(&c[..16]).unwrap()), u16::from_le_bytes([c[16], c[17]]))))
                        .filter(|a| usable_v6(*a, cx.allow_private))
                        .take(MAX_ALT)
                        .collect();
                    if s.path != s.addr && !s.alt.contains(&s.path) {
                        s.path = s.addr;
                    }
                    if s.path == s.addr && !s.alt.is_empty() {
                        s.probe_until = Some(now + PROBE_FOR);
                        s.next_probe = now;
                    }
                }
            }
            F_CLOSE => return Err(if p.first() == Some(&CLOSE_LEAVING) { LEAVING.into() } else { String::new() }),
            _ => return Ok(()), // a newer frame: skip the rest
        }
    }
    Ok(())
}

pub(super) fn tick_sessions(sh: &Arc<Shared>, ss: &mut HashMap<SocketAddr, Session>, now: Instant) -> Vec<Act> {
    let cx = sh.cx();
    let mut acts = Vec::new();
    let mut remove = Vec::new();
    let own6 = if ss.values().any(|s| s.confirmed) { sh.own_v6(now) } else { Vec::new() };
    for s in ss.values_mut() {
        if !s.confirmed {
            let half_open = sh.mode() == Mode::Public && s.inbound && now.duration_since(s.created) >= PUBLIC_HALF_OPEN;
            if half_open || now.duration_since(s.created) >= s.punch_for {
                // One-way failures show in the log: they reached us, we never got
                // their confirmation (our packets aren't reaching them), or the other
                // way round.
                let why = if s.inbound {
                    format!(
                        "{} was punching through to us, but the handshake never finished in {}s: our replies aren't reaching them (a one-way path)",
                        redact::addr(s.addr),
                        now.duration_since(s.created).as_secs()
                    )
                } else if s.peer.is_some() {
                    format!(
                        "{} answered our punch, but the handshake never finished in {}s",
                        redact::addr(s.addr),
                        now.duration_since(s.created).as_secs()
                    )
                } else {
                    String::new()
                };
                remove.push((s.addr, false, why));
            } else {
                // Fast at first, then once a second: the DHT hands out some junk
                // addresses, and a real peer that starts punching later gets an
                // answer at once anyway (an INIT is answered within 100 ms).
                let every = if now.duration_since(s.created) < INIT_FAST_FOR { INIT_EVERY } else { INIT_SLOW };
                if s.last_init.is_none_or(|t| now.duration_since(t) >= every) {
                    s.send_init(&cx, now);
                }
            }
            continue;
        }
        if now.duration_since(s.last_rx) >= DEAD_AFTER {
            remove.push((s.addr, true, format!("{} went quiet for {}s: dropped", redact::addr(s.addr), DEAD_AFTER.as_secs())));
            continue;
        }
        if s.bridge.as_ref().is_some_and(|b| b.closed.load(Ordering::Relaxed)) {
            s.send_close(&cx, now, false);
            remove.push((s.addr, true, format!("{}: the mesh connection closed", redact::addr(s.addr))));
            continue;
        }
        if s.bridge.as_ref().is_some_and(|b| b.eof.load(Ordering::Relaxed)) {
            // Our mesh is done with this connection: close once the peer has had all of
            // it (a goodbye straight away could drop the tail still in flight).
            let since = *s.finishing.get_or_insert(now);
            if s.stream.idle() || now.duration_since(since) >= FINISH_FOR {
                s.send_close(&cx, now, false);
                remove.push((s.addr, true, format!("{}: the mesh connection finished", redact::addr(s.addr))));
                continue;
            }
        }
        s.flush(&cx, now);
        if now.duration_since(s.last_tx) >= KEEPALIVE {
            let ping = ping_frame(s, now);
            s.send_plain(&cx, &ping, now);
        }
        if cx.sock6.is_some() {
            if now >= s.next_advert && !own6.is_empty() {
                s.send_plain(&cx, &addrs6_frame(&own6), now);
                s.next_advert = now + ADVERT_EVERY;
            }
            if s.path != s.addr {
                if now.duration_since(s.path_rx) >= PATH_QUIET {
                    acts.push(Act::Log(format!("{}: the IPv6 path went quiet: back to IPv4", redact::addr(s.addr))));
                    s.path = s.addr;
                } else if now.duration_since(s.v4_ping) >= KEEPALIVE {
                    let ping = ping_frame(s, now);
                    s.send_plain_to(&cx, s.addr, &ping, now);
                    s.v4_ping = now;
                }
            } else if s.probe_until.is_some_and(|t| now < t) && now >= s.next_probe {
                let ping = ping_frame(s, now);
                for a in s.alt.clone() {
                    s.send_plain_to(&cx, a, &ping, now);
                }
                s.next_probe = now + PROBE_EVERY;
            }
        }
        if let Some(info) = &s.info
            && s.bridge.is_none()
            && !s.bridging
            && now >= s.next_bridge
        {
            if sh.host.knows(info.id) {
                s.next_bridge = now + Duration::from_secs(5);
            } else {
                s.bridging = true;
                acts.push(Act::Bridge(s.addr, s.epoch, info.id));
            }
        }
    }
    for (addr, was_up, why) in remove {
        let s = ss.remove(&addr).unwrap();
        if !why.is_empty() {
            acts.push(Act::Log(why));
        }
        acts.push(Act::Gone(s.bridge, false));
        acts.push(Act::Ended(addr, was_up));
    }
    acts
}
