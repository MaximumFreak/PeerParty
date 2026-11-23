//! Finding peers: the DHT (announce and look up this hour's topic), STUN, static
//! addresses, the candidates to punch and how they back off, and the block list, friends
//! and pins read again as their files change.

use super::*;

/// Between DHT lookups in the first two minutes while no one is connected
/// (`Config::early_lookup_every`); 30 s after that, 120 s once someone is.
pub(super) const EARLY_LOOKUP_EVERY: Duration = Duration::from_secs(15);
/// How long we punch an address that doesn't answer.
pub(super) const PUNCH_FOR: Duration = Duration::from_secs(40);
/// A DHT find that has never answered is punched for less: the DHT keeps handing out
/// addresses of copies that are long gone (earlier runs, other ports), and those
/// shouldn't hold a punching slot for long.
pub(super) const PUNCH_DHT_FOR: Duration = Duration::from_secs(20);
/// ... and less again once it has failed.
pub(super) const PUNCH_RETRY_FOR: Duration = Duration::from_secs(10);
/// At most this share (in quarters) of the session table punches addresses that haven't
/// answered, so stale finds can't fill it and crowd out a real peer's handshake: 12 of
/// 16 public sessions, 24 of 32 in a room. Addresses that answered this run don't
/// count, and a handshake arriving at a full table replaces the oldest such punch.
pub(super) const PUNCHING_QUARTERS: usize = 3;
/// A DHT find that never answered goes after this many failed tries, and is ignored
/// for GIVE_UP_FOR if a lookup returns it again (a real peer at that address that
/// punches us still gets in: an inbound handshake doesn't need a candidate).
pub(super) const CAND_MAX_FAILS: u32 = 3;
/// An address that answered earlier in the run (a peer that since left) goes after this
/// many failed tries in a row, backing off in between.
pub(super) const ANSWERED_MAX_FAILS: u32 = 8;
pub(super) const GIVE_UP_FOR: Duration = Duration::from_secs(1800);
/// A DHT find that never answered and no lookup has returned for this long goes.
pub(super) const CAND_STALE: Duration = Duration::from_secs(600);
/// At most this many candidate addresses are kept.
pub(super) const MAX_CANDS: usize = 256;
pub(super) const SLOT_SECS: u64 = 3600;

pub(super) struct Cand {
    pub(super) from_dht: bool,
    pub(super) next_try: Instant,
    pub(super) fails: u32,
    /// When a DHT lookup last returned it (or it was added).
    pub(super) seen: Instant,
    /// A lookup of this hour's topic returned it (the last hour's holds older
    /// announcements).
    pub(super) current: bool,
    /// A handshake with it has finished this run: a live address, punched first and
    /// kept.
    pub(super) answered: bool,
}

impl Cand {
    pub(super) fn new(from_dht: bool, now: Instant) -> Cand {
        Cand { from_dht, next_try: now, fails: 0, seen: now, current: false, answered: false }
    }

    /// How long a punch to it lasts.
    pub(super) fn punch_for(&self) -> Duration {
        if self.answered || !self.from_dht {
            PUNCH_FOR
        } else if self.fails == 0 {
            PUNCH_DHT_FOR
        } else {
            PUNCH_RETRY_FOR
        }
    }

    /// Punch order: addresses that answered, then PEERPARTY_INET_PEERS, then this
    /// hour's finds, fewest failures and most recently returned first.
    pub(super) fn rank(&self) -> (bool, bool, bool, u32, std::cmp::Reverse<Instant>) {
        (!self.answered, self.from_dht, !self.current, self.fails, std::cmp::Reverse(self.seen))
    }
}

pub(super) struct Disc {
    pub(super) dht: Option<Dht>,
    pub(super) next_lookup: Instant,
    pub(super) started: Instant,
    pub(super) stun_servers: Vec<SocketAddr>,
    pub(super) stun_tx: HashMap<[u8; 12], SocketAddr>,
    pub(super) stun_mapped: HashMap<SocketAddr, SocketAddr>,
    pub(super) next_stun: Instant,
    pub(super) cands: HashMap<SocketAddr, Cand>,
    pub(super) statics: Vec<SocketAddr>,
    pub(super) selves: HashSet<SocketAddr>,
    pub(super) banned: HashMap<SocketAddr, Instant>,
    /// DHT finds given up on (CAND_MAX_FAILS), until then.
    pub(super) given_up: HashMap<SocketAddr, Instant>,
    pub(super) next_resolve: Instant,
    pub(super) next_save: Instant,
}

pub(super) fn resolve(names: &[String]) -> Vec<SocketAddr> {
    resolve_with(names, |a| a.is_ipv4())
}

/// `PEERPARTY_INET_PEERS`: IPv4, and IPv6 ("[2001:db8::5]:4000") when we have an IPv6
/// socket and the address is one worth trying.
pub(super) fn resolve_peers(names: &[String], v6: bool, allow_private: bool) -> Vec<SocketAddr> {
    resolve_with(names, |a| a.is_ipv4() || (v6 && usable_v6(*a, allow_private)))
}

pub(super) fn resolve_with(names: &[String], keep: impl Fn(&SocketAddr) -> bool) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for n in names {
        if let Ok(addrs) = n.to_socket_addrs() {
            out.extend(addrs.filter(&keep));
        }
    }
    out.dedup();
    out
}

pub(super) fn load_nodes(path: &Option<PathBuf>) -> Vec<([u8; 20], SocketAddr)> {
    let Some(p) = path else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(p) else { return Vec::new() };
    text.lines()
        .filter_map(|l| {
            let (id, addr) = l.trim().split_once(' ')?;
            if id.len() != 40 || !id.is_ascii() {
                return None;
            }
            let mut b = [0u8; 20];
            for (i, x) in b.iter_mut().enumerate() {
                *x = u8::from_str_radix(&id[i * 2..i * 2 + 2], 16).ok()?;
            }
            Some((b, addr.parse().ok()?))
        })
        .take(200)
        .collect()
}

pub(super) fn save_nodes(path: &Option<PathBuf>, nodes: &[([u8; 20], SocketAddr)]) {
    let Some(p) = path else { return };
    if nodes.is_empty() {
        return;
    }
    let text: String = nodes
        .iter()
        .rev()
        .take(200)
        .map(|(id, a)| format!("{} {a}\n", id.iter().map(|b| format!("{b:02x}")).collect::<String>()))
        .collect();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, text);
}

pub(super) fn unix_now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Starts a session to `addr` unless there is one: it punches until answered, for
/// `punch_for` at most.
pub(super) fn punch(sh: &Arc<Shared>, addr: SocketAddr, punch_for: Duration) -> bool {
    let mut ss = sh.sessions.lock().unwrap();
    if ss.contains_key(&addr) || ss.len() >= sh.max_sessions {
        return false;
    }
    if sh.mode() == Mode::Public && ss.values().filter(|s| s.addr.ip() == addr.ip()).count() >= PUBLIC_PER_IP {
        return false;
    }
    let now = Instant::now();
    if sh.is_blocked_addr(addr, now) {
        return false;
    }
    let mut s = Session::new(addr, sh.epoch(), now, false, sh.mode());
    s.punch_for = punch_for;
    s.send_init(&sh.cx(), now);
    ss.insert(addr, s);
    true
}

pub(super) fn disc_loop(sh: &Arc<Shared>) {
    while !sh.stop.load(Ordering::Relaxed) {
        disc_tick(sh, Instant::now());
        if sh.naps.nap(Duration::from_millis(100)) {
            return;
        }
    }
}

/// Ends the sessions of players on the block list, and ignores their addresses a while.
pub(super) fn kick_blocked(sh: &Arc<Shared>) {
    let Some(p) = &sh.pubs else { return };
    let now = Instant::now();
    let mut acts = Vec::new();
    {
        let mut ss = sh.sessions.lock().unwrap();
        let cx = sh.cx();
        let gone: Vec<SocketAddr> =
            ss.values().filter(|s| s.fp.as_deref().is_some_and(|f| p.blocked.is_blocked(f))).map(|s| s.addr).collect();
        for a in gone {
            let mut s = ss.remove(&a).unwrap();
            s.send_close(&cx, now, true);
            sh.block_addr(a, s.fp.as_deref(), now);
            acts.push(Act::Log(format!("{} ({}) is on the block list: dropped", redact::addr(a), s.fp.as_deref().unwrap_or("?"))));
            acts.push(Act::Gone(s.bridge.take(), true));
            acts.push(Act::Ban(a));
        }
    }
    run(sh, acts);
}

pub(super) fn disc_tick(sh: &Arc<Shared>, now: Instant) {
    if let Some(p) = &sh.pubs {
        if p.blocked.refresh() {
            kick_blocked(sh);
        }
        // Friends added or removed in the file count from the next session on.
        p.friends.refresh();
        p.pins.refresh();
    }
    // Names first, without the lock: DNS can take a while.
    let resolve_due = sh.disc.lock().unwrap().next_resolve <= now;
    if resolve_due {
        let seeds = if sh.cfg.dht { resolve(&sh.cfg.bootstrap) } else { Vec::new() };
        let stuns = resolve(&sh.cfg.stun);
        let statics = resolve_peers(&sh.cfg.peers, sh.sock6.is_some(), sh.cfg.allow_private);
        let mut d = sh.disc.lock().unwrap();
        let ok = (seeds.is_empty() == sh.cfg.bootstrap.is_empty() || !sh.cfg.dht)
            && stuns.is_empty() == sh.cfg.stun.is_empty()
            && statics.len() >= sh.cfg.peers.len().min(1);
        d.next_resolve = now + if ok { Duration::from_secs(600) } else { Duration::from_secs(30) };
        if !ok {
            sh.log("couldn't look up some names (offline?): trying again in 30 s");
        }
        if let Some(x) = &mut d.dht
            && !seeds.is_empty()
        {
            x.set_seeds(seeds);
        }
        if !stuns.is_empty() {
            d.stun_servers = stuns;
        }
        for a in &statics {
            if !d.statics.contains(a) {
                d.statics.push(*a);
            }
        }
    }
    let ended: Vec<(SocketAddr, bool)> = std::mem::take(&mut *sh.ended.lock().unwrap());
    let mut d = sh.disc.lock().unwrap();
    let d = &mut *d;
    let local = sh.sock.local_addr().ok();
    for (addr, up) in ended {
        let statik = d.statics.contains(&addr);
        let mut give_up = false;
        if let Some(c) = d.cands.get_mut(&addr) {
            if up {
                c.answered = true;
                c.fails = 0;
                c.next_try = now + Duration::from_secs(2);
            } else {
                c.fails += 1;
                // Addresses that answered this run and PEERPARTY_INET_PEERS are retried
                // soon; DHT finds that never answered back off, then go.
                let wait = if statik || (c.answered && c.fails <= 2) { 5 } else { (15u64 << c.fails.min(4)).min(300) };
                c.next_try = now + Duration::from_secs(wait);
                give_up = !statik
                    && if c.answered { c.fails >= ANSWERED_MAX_FAILS } else { c.from_dht && c.fails >= CAND_MAX_FAILS };
            }
        }
        if give_up {
            d.cands.remove(&addr);
            d.given_up.insert(addr, now + GIVE_UP_FOR);
            sh.log(&format!("giving up on {}: no answer after several tries", redact::addr(addr)));
        }
    }
    // STUN: which public address we have, now and every five minutes.
    if d.next_stun <= now && !d.stun_servers.is_empty() {
        d.next_stun = now + Duration::from_secs(300);
        d.stun_tx.clear();
        for s in d.stun_servers.clone() {
            let txid = crypto::random_array::<12>();
            d.stun_tx.insert(txid, s);
            let _ = sh.sock.send_to(&stun::request(&txid), s);
        }
    }
    // DHT: announce and look up this hour's topic (and the last hour's, early on).
    let connected = sh.sessions.lock().unwrap().values().any(|s| s.confirmed);
    if let Some(x) = &mut d.dht {
        for done in x.tick(now) {
            sh.log(&format!(
                "dht lookup {}: {} node(s) answered, {} address(es) found, announced to {}",
                done.id, done.replied, done.values, done.announced
            ));
        }
        if d.next_lookup <= now && !x.busy() {
            let unix = unix_now();
            let slot = unix / SLOT_SECS;
            let game = &sh.me.game;
            x.lookup(sh.cfg.room.topic(game, slot), Some(sh.local_port), now);
            if unix % SLOT_SECS < 900 && slot > 0 {
                x.lookup(sh.cfg.room.topic(game, slot - 1), None, now);
            }
            let early = now.duration_since(d.started) < Duration::from_secs(120);
            d.next_lookup = now + if connected {
                Duration::from_secs(120)
            } else if early {
                sh.cfg.early_lookup_every
            } else {
                Duration::from_secs(30)
            };
        }
        for (to, q) in x.drain_out() {
            let _ = sh.sock.send_to(&q, to);
        }
        // A find at our own public IP is another copy behind our router. Routers that
        // don't loop their own address back ("hairpinning") drop that path, so also try
        // the same port at our own LAN address: that reaches another copy on this PC
        // when the router kept the port. (Another PC on this LAN is found by LAN
        // discovery anyway.)
        let lan_ip = lan_ipv4();
        let this_hour = sh.cfg.room.topic(&sh.me.game, unix_now() / SLOT_SECS);
        for (topic, addr) in x.drain_found() {
            let ours = d.selves.iter().any(|s| s.ip() == addr.ip()) && !d.selves.contains(&addr);
            let mut add = vec![addr];
            if ours && let Some(ip) = lan_ip {
                let alt = SocketAddr::new(ip.into(), addr.port());
                if alt.port() != sh.local_port {
                    add.push(alt);
                }
            }
            for a in add {
                if d.given_up.get(&a).is_some_and(|t| *t > now) {
                    continue;
                }
                if d.cands.len() >= MAX_CANDS && !d.cands.contains_key(&a) {
                    continue;
                }
                let c = d.cands.entry(a).or_insert_with(|| Cand::new(true, now));
                c.seen = now;
                c.current |= topic == this_hour;
            }
        }
        if let Some(e) = x.external {
            d.selves.insert(e);
        }
    }
    for a in d.statics.clone() {
        d.cands.entry(a).or_insert_with(|| Cand::new(false, now));
    }
    d.banned.retain(|_, until| *until > now);
    d.given_up.retain(|_, until| *until > now);
    if d.next_save <= now {
        d.next_save = now + Duration::from_secs(300);
        if let Some(x) = &d.dht {
            save_nodes(&sh.cfg.node_cache, &x.good_nodes());
        }
    }
    // Punch what's due, best first: a capped number of addresses that haven't
    // answered at a time, so a peer's real address isn't starved by stale ones.
    let mut due: Vec<(SocketAddr, bool)> = {
        let mut v: Vec<(&SocketAddr, &Cand)> = d
            .cands
            .iter()
            .filter(|(a, c)| {
                c.next_try <= now && !d.selves.contains(a) && !d.banned.contains_key(a) && Some(**a) != local
            })
            .collect();
        v.sort_by_key(|(_, c)| c.rank());
        v.into_iter().map(|(a, c)| (*a, c.answered)).collect()
    };
    let mut room = {
        let ss = sh.sessions.lock().unwrap();
        let cap = sh.max_sessions * PUNCHING_QUARTERS / 4;
        cap.saturating_sub(ss.values().filter(|s| !s.inbound && !s.confirmed && s.peer.is_none()).count())
    };
    due.retain(|(_, answered)| {
        if *answered {
            return true;
        }
        let ok = room > 0;
        room = room.saturating_sub(1);
        ok
    });
    for (a, _) in due {
        let c = d.cands.get_mut(&a).unwrap();
        let punch_for = c.punch_for();
        c.next_try = now + punch_for + Duration::from_secs(1);
        if punch(sh, a, punch_for) {
            sh.log(&format!(
                "punching to {} ({})",
                redact::addr(a),
                if c.answered {
                    "it reached us before"
                } else if c.from_dht {
                    "found on the DHT"
                } else {
                    "PEERPARTY_INET_PEERS"
                }
            ));
        }
    }
    // DHT finds that never answered and no lookup returns any more go.
    d.cands.retain(|_, c| !c.from_dht || c.answered || now.duration_since(c.seen) < CAND_STALE);
}
