//! Reachability: the router's port mapping (PCP, NAT-PMP, UPnP) kept for as long as we
//! run, and `probe`, a check of the internet path with no peer needed.

use super::*;

/// This PC's LAN address (the one its default route uses), or None offline or in
/// loopback mode. Sends nothing: connecting a UDP socket only picks a route.
pub(super) fn lan_ipv4() -> Option<std::net::Ipv4Addr> {
    if crate::loopback_only() {
        return None;
    }
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

/// Keeps a port mapping on the router: makes one, renews it at half its lease, and
/// tries again later (a minute, doubling to half an hour) when the router has none.
pub(super) fn portmap_loop(sh: &Arc<Shared>) {
    let mut next = Instant::now();
    let mut fails = 0u32;
    while !sh.stop_mapping.load(Ordering::Relaxed) {
        if Instant::now() >= next {
            let cur = sh.mapping.lock().unwrap().clone();
            let renewing = cur.is_some();
            let res = match cur {
                Some(mut m) => m.renew().map(|_| m),
                None => match lan_ipv4() {
                    Some(ip) => portmap::map(ip, portmap::default_gateway(), sh.local_port),
                    None => Err("no LAN address".into()),
                },
            };
            match res {
                Ok(m) => {
                    fails = 0;
                    next = m.renew_at();
                    set_mapping(sh, Some(m), None);
                }
                Err(e) => {
                    fails += 1;
                    next = Instant::now() + Duration::from_secs((60u64 << fails.min(5)).min(1800));
                    let note = if renewing { format!("lost: {e}") } else { format!("none ({e})") };
                    set_mapping(sh, None, Some(note));
                }
            }
        }
        // Woken early when the layer stops (`release_port_map` alone lets it see
        // `stop_mapping` within this).
        if sh.naps.nap(Duration::from_millis(200)) {
            return;
        }
    }
}

/// Records a new mapping (or none) and points the DHT announce at it.
pub(super) fn set_mapping(sh: &Arc<Shared>, m: Option<portmap::Mapping>, why: Option<String>) {
    let note = match &m {
        Some(m) => match m.external() {
            Some(e) => format!("{}: external port {}", m.method.name(), e.port()),
            None => format!("{}: mapped, but the router is behind another NAT (not announced)", m.method.name()),
        },
        None => format!("none ({})", why.as_deref().unwrap_or("?")),
    };
    let ext = m.as_ref().and_then(portmap::Mapping::external);
    {
        let mut cur = sh.mapping.lock().unwrap();
        if sh.stop_mapping.load(Ordering::Relaxed) {
            if let Some(m) = m {
                m.remove();
            }
            return;
        }
        *cur = m;
    }
    let mut n = sh.map_note.lock().unwrap();
    if n.as_deref() != Some(note.as_str()) {
        sh.log(&format!("port mapping: {}", why.map(|w| w.to_string()).unwrap_or(note.clone())));
        *n = Some(note);
        drop(n);
        let mut d = sh.disc.lock().unwrap();
        if let Some(e) = ext {
            d.selves.insert(e);
        }
        let port = ext.map(|e| e.port());
        if let Some(x) = &mut d.dht
            && x.mapped_port != port
        {
            x.mapped_port = port;
            d.next_lookup = Instant::now();
        }
    }
}

/// What `probe` found out about this PC's internet connection.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub local_port: u16,
    pub stun: Vec<(String, Option<SocketAddr>)>,
    pub nat: Nat,
    pub dht_seeds: usize,
    pub dht_replies: u64,
    pub dht_external: Option<SocketAddr>,
    pub announced_to: usize,
    /// A second socket looked the test topic up and found the first socket's address.
    pub found_own_announce: bool,
    /// The router forwarded (and then stopped forwarding) a test port.
    pub port_mapped: bool,
    pub lines: Vec<String>,
}

/// Checks the real internet path, with no peer needed: STUN against every server, a DHT
/// bootstrap, an announce under a random topic, and a second socket looking that topic
/// up. Takes up to about a minute.
pub fn probe(stun_servers: &[String], bootstrap: &[String]) -> io::Result<Probe> {
    let mut p = Probe::default();
    let a = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    let b = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    for s in [&a, &b] {
        s.set_read_timeout(Some(Duration::from_millis(100)))?;
    }
    p.local_port = a.local_addr()?.port();
    let mut buf = vec![0u8; 65536];
    // STUN.
    let mut tx = HashMap::new();
    for name in stun_servers {
        let addrs = resolve(std::slice::from_ref(name));
        match addrs.first() {
            Some(&s) => {
                let txid = crypto::random_array::<12>();
                tx.insert(txid, name.clone());
                let _ = a.send_to(&stun::request(&txid), s);
            }
            None => p.lines.push(format!("stun {name}: no address (DNS)")),
        }
    }
    let mut got: HashMap<String, SocketAddr> = HashMap::new();
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until && got.len() < tx.len() {
        if let Ok((n, _)) = a.recv_from(&mut buf)
            && let Some((id, mapped)) = stun::parse_response(&buf[..n])
            && let Some(name) = tx.get(&id)
        {
            got.insert(name.clone(), mapped);
        }
    }
    for name in stun_servers {
        let m = got.get(name).copied();
        p.lines.push(match m {
            Some(m) => format!("stun {name}: we are {}", if redact::debug() { m.to_string() } else { redact::own(m) }),
            None => format!("stun {name}: no answer"),
        });
        p.stun.push((name.clone(), m));
    }
    let mapped: Vec<SocketAddr> = p.stun.iter().filter_map(|(_, m)| *m).collect();
    p.nat = Nat::classify(None, &mapped);
    p.lines.push(format!("nat: {}", p.nat.describe()));
    // Port mapping: ask the router to forward the test port, then delete it.
    let pm = match lan_ipv4() {
        Some(ip) => portmap::map(ip, portmap::default_gateway(), p.local_port),
        None => Err("no LAN address".into()),
    };
    p.lines.push(match pm {
        Ok(m) => {
            p.port_mapped = m.external().is_some();
            let line = match m.external() {
                Some(e) => format!("port mapping: {} forwards external port {} to us", m.method.name(), e.port()),
                None => format!("port mapping: {} works, but this router is behind another NAT (carrier NAT?)", m.method.name()),
            };
            m.remove();
            line
        }
        Err(e) => format!("port mapping: none ({e})"),
    });
    // DHT: announce from a, then look up from b.
    let seeds = resolve(bootstrap);
    p.dht_seeds = seeds.len();
    p.lines.push(format!("dht: {} bootstrap address(es)", seeds.len()));
    let topic: [u8; 20] = crypto::random_array();
    let run = |sock: &UdpSocket, announce: bool, buf: &mut [u8]| -> (Dht, Vec<dht::Done>) {
        let mut d = Dht::new(seeds.clone(), Vec::new());
        let t0 = Instant::now();
        d.lookup(topic, announce.then_some(1), t0);
        let mut done = Vec::new();
        while Instant::now() < t0 + Duration::from_secs(40) {
            done.extend(d.tick(Instant::now()));
            for (to, q) in d.drain_out() {
                let _ = sock.send_to(&q, to);
            }
            if !d.busy() && done.iter().all(|x: &dht::Done| x.announced == 0 || announce) {
                // Give announces a moment to be answered.
                let until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < until {
                    if let Ok((n, from)) = sock.recv_from(buf) {
                        d.handle(from, &buf[..n]);
                    }
                }
                break;
            }
            if let Ok((n, from)) = sock.recv_from(buf) {
                d.handle(from, &buf[..n]);
            }
        }
        (d, done)
    };
    let (da, done_a) = run(&a, true, &mut buf);
    p.dht_replies = da.replies;
    p.dht_external = da.external;
    p.announced_to = done_a.iter().map(|d| d.announced).sum();
    p.lines.push(format!(
        "dht: {} replies, {} nodes, external address {}, announced to {} node(s)",
        da.replies,
        da.good_nodes().len(),
        da.external.map(|e| if redact::debug() { e.to_string() } else { redact::own(e) }).unwrap_or("unknown".into()),
        p.announced_to
    ));
    let (mut db, _) = run(&b, false, &mut buf);
    let found: Vec<SocketAddr> = db.drain_found().into_iter().map(|(_, a)| a).collect();
    let want_port = da.external.map(|e| e.port()).or(mapped.first().map(|m| m.port()));
    p.found_own_announce = found.iter().any(|f| Some(f.port()) == want_port);
    p.lines.push(format!(
        "dht: a second socket found {} address(es) under the test topic{}",
        found.len(),
        if p.found_own_announce { ", including the first socket's" } else { "" }
    ));
    Ok(p)
}
