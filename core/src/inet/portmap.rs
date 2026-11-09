//! Port mapping: asking the router to forward our UDP port, so peers reach us without
//! hole punching (it helps most behind a symmetric NAT, which punching rarely gets
//! through). Three ways, tried in order: PCP (RFC 6887) and NAT-PMP (RFC 6886) at the
//! default gateway's UDP 5351, then UPnP IGD (SSDP discovery, then SOAP to the router's
//! WANIPConnection or WANPPPConnection service).
//!
//! A mapping has a lease: the internet layer renews it at half and deletes it when it
//! stops or the game exits. One a crash leaves behind runs out with its lease (a router
//! that only takes permanent UPnP leases keeps it until the next run replaces it).
//!
//! Only the router on our own network is asked: SSDP answers from anything but a LAN
//! address, and description or control URLs on any host but the one that answered, are
//! ignored, and replies are size-capped. Nothing here logs an address.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

/// PCP and NAT-PMP servers listen here on the gateway.
pub const PMP_PORT: u16 = 5351;
/// The lease asked for with PCP and NAT-PMP (routers may give less).
const LEASE: u32 = 7200;
const UPNP_LEASE: u32 = 3600;
const DESCRIPTION: &str = "peerparty";
const MAX_HTTP: usize = 256 << 10;
const SSDP: (Ipv4Addr, u16) = (Ipv4Addr::new(239, 255, 255, 250), 1900);
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Pcp,
    NatPmp,
    Upnp,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Method::Pcp => "PCP",
            Method::NatPmp => "NAT-PMP",
            Method::Upnp => "UPnP",
        }
    }
}

/// A port the router forwards to us.
#[derive(Clone, Debug)]
pub struct Mapping {
    pub method: Method,
    pub internal_port: u16,
    pub external_port: u16,
    /// The router's address on the internet, when it said.
    pub external_ip: Option<Ipv4Addr>,
    /// The lease the router gave (zero: until deleted).
    pub lease: Duration,
    /// When it was made or last renewed.
    pub at: Instant,
    client: Ipv4Addr,
    gateway: SocketAddr,
    nonce: [u8; 12],
    control: Option<Control>,
}

#[derive(Clone, Debug, PartialEq)]
struct Control {
    host: SocketAddr,
    path: String,
    service: String,
}

/// An address on the internet: not private, carrier-grade NAT, loopback and the like.
pub fn is_public(ip: Ipv4Addr) -> bool {
    !super::redact::is_local(IpAddr::V4(ip)) && !ip.is_broadcast() && !ip.is_multicast() && ip.octets()[0] != 0
}

fn is_lan(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_loopback() || ip.is_link_local()
}

impl Mapping {
    /// Where peers on the internet reach us through it. None when the router didn't
    /// say its address, or it isn't a public one: another NAT beyond this router, where
    /// the mapping doesn't help.
    pub fn external(&self) -> Option<SocketAddr> {
        self.external_ip.filter(|ip| is_public(*ip)).map(|ip| SocketAddr::new(ip.into(), self.external_port))
    }

    /// When it should be renewed: at half the lease (every half hour for a permanent one).
    pub fn renew_at(&self) -> Instant {
        let half = if self.lease.is_zero() { Duration::from_secs(1800) } else { (self.lease / 2).max(Duration::from_secs(30)) };
        self.at + half
    }

    /// Asks for the same mapping again.
    pub fn renew(&mut self) -> Result<(), String> {
        let m = match self.method {
            Method::Pcp => pcp_map(self.gateway, self.client, self.internal_port, self.external_port, self.nonce).map_err(|f| f.text("PCP"))?,
            Method::NatPmp => natpmp_map(self.gateway, self.client, self.internal_port, self.external_port, 3).map_err(|f| f.text("NAT-PMP"))?,
            Method::Upnp => {
                let c = self.control.clone().ok_or("UPnP: no control URL")?;
                upnp_one(&c, self.client, self.internal_port, self.external_port).map_err(|e| format!("UPnP: {e}"))?
            }
        };
        let ip = m.external_ip.or(self.external_ip);
        *self = Mapping { external_ip: ip, ..m };
        Ok(())
    }

    /// Deletes it: best effort, and quick (one packet, or one request to the router).
    pub fn remove(&self) {
        match self.method {
            Method::Pcp => {
                let req = pcp_request(self.client, &self.nonce, self.internal_port, self.external_port, 0);
                send_twice(self.client, self.gateway, &req);
            }
            Method::NatPmp => send_twice(self.client, self.gateway, &natpmp_request(self.internal_port, 0, 0)),
            Method::Upnp => {
                if let Some(c) = &self.control {
                    let _ = soap(c, "DeletePortMapping", &delete_args(self.external_port), Duration::from_secs(1));
                }
            }
        }
    }
}

fn send_twice(local: Ipv4Addr, to: SocketAddr, d: &[u8]) {
    if let Ok(s) = UdpSocket::bind((local, 0)) {
        let _ = s.send_to(d, to);
        let _ = s.send_to(d, to);
    }
}

/// The default gateway's address (the next hop toward the internet), when it's on the
/// LAN. Sends nothing.
pub fn default_gateway() -> Option<Ipv4Addr> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetBestRoute, MIB_IPFORWARDROW};
    let mut row: MIB_IPFORWARDROW = unsafe { std::mem::zeroed() };
    let dest = u32::from_ne_bytes([8, 8, 8, 8]);
    if unsafe { GetBestRoute(dest, 0, &mut row) } != 0 {
        return None;
    }
    let gw = Ipv4Addr::from(row.dwForwardNextHop.to_ne_bytes());
    (is_lan(gw) && !gw.is_loopback()).then_some(gw)
}

/// Asks the router to forward UDP `port` to `client` (this PC's LAN address): PCP, then
/// NAT-PMP at `gateway`, then UPnP. Takes a few seconds when nothing answers.
pub fn map(client: Ipv4Addr, gateway: Option<Ipv4Addr>, port: u16) -> Result<Mapping, String> {
    let mut why = Vec::new();
    if let Some(gw) = gateway {
        match map_pmp(SocketAddr::new(gw.into(), PMP_PORT), client, port) {
            Ok(m) => return Ok(m),
            Err(e) => why.push(e),
        }
    } else {
        why.push("PCP/NAT-PMP: no gateway on the LAN".to_string());
    }
    let found = ssdp_search(client, Duration::from_secs(2));
    if found.is_empty() {
        why.push("UPnP: no router answered".into());
    }
    for (_, loc) in found {
        match describe(&loc).and_then(|cs| upnp_map(&cs, client, port)) {
            Ok(m) => return Ok(m),
            Err(e) => why.push(format!("UPnP: {e}")),
        }
    }
    Err(why.join("; "))
}

enum Fail {
    /// Nothing listens there (port unreachable).
    Absent,
    /// No answer.
    Silent,
    /// A NAT-PMP server answered a PCP request.
    OldVersion,
    No(String),
}

impl Fail {
    fn text(self, what: &str) -> String {
        match self {
            Fail::Absent => format!("{what}: the router doesn't listen for it"),
            Fail::Silent => format!("{what}: no answer from the router"),
            Fail::OldVersion => format!("{what}: the router speaks an older version"),
            Fail::No(s) => format!("{what}: {s}"),
        }
    }
}

/// PCP, then NAT-PMP, at `gw`.
fn map_pmp(gw: SocketAddr, client: Ipv4Addr, port: u16) -> Result<Mapping, String> {
    let nonce = super::crypto::random_array::<12>();
    let tries = match pcp_map(gw, client, port, port, nonce) {
        Ok(m) => return Ok(m),
        Err(Fail::Absent) => return Err(Fail::Absent.text("PCP/NAT-PMP")),
        // A NAT-PMP server should answer PCP with its version; an old one may not.
        Err(Fail::Silent) => 1,
        Err(_) => 3,
    };
    natpmp_map(gw, client, port, port, tries).map_err(|f| f.text(if tries == 1 { "PCP/NAT-PMP" } else { "NAT-PMP" }))
}

/// Sends `req` and waits for an answer from `to` that `parse` takes, `tries` times
/// (250, 500, 1000 ms).
fn ask<T>(local: Ipv4Addr, to: SocketAddr, req: &[u8], tries: u32, parse: impl Fn(&[u8]) -> Option<T>) -> Result<T, Fail> {
    let sock = UdpSocket::bind((local, 0)).map_err(|e| Fail::No(e.to_string()))?;
    let mut buf = [0u8; 1100];
    for i in 0..tries {
        let _ = sock.send_to(req, to);
        let until = Instant::now() + Duration::from_millis(250 << i.min(4));
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let _ = sock.set_read_timeout(Some(left));
            match sock.recv_from(&mut buf) {
                Ok((n, from)) if from == to => {
                    if let Some(t) = parse(&buf[..n]) {
                        return Ok(t);
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => return Err(Fail::Absent),
                Err(_) => break,
            }
        }
    }
    Err(Fail::Silent)
}

fn be16(d: &[u8]) -> u16 {
    u16::from_be_bytes([d[0], d[1]])
}

fn be32(d: &[u8]) -> u32 {
    u32::from_be_bytes([d[0], d[1], d[2], d[3]])
}

fn pcp_request(client: Ipv4Addr, nonce: &[u8; 12], internal: u16, external: u16, lifetime: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(60);
    p.extend_from_slice(&[2, 1, 0, 0]);
    p.extend_from_slice(&lifetime.to_be_bytes());
    p.extend_from_slice(&client.to_ipv6_mapped().octets());
    p.extend_from_slice(nonce);
    p.extend_from_slice(&[17, 0, 0, 0]);
    p.extend_from_slice(&internal.to_be_bytes());
    p.extend_from_slice(&external.to_be_bytes());
    p.extend_from_slice(&Ipv4Addr::UNSPECIFIED.to_ipv6_mapped().octets());
    p
}

type PcpOk = (u16, Option<Ipv4Addr>, u32);

fn parse_pcp(d: &[u8], nonce: &[u8; 12], internal: u16) -> Option<Result<PcpOk, Fail>> {
    if d.len() >= 2 && d[0] == 0 {
        return Some(Err(Fail::OldVersion));
    }
    if d.len() < 60 || d[0] != 2 || d[1] != 0x81 || d[24..36] != nonce[..] || d[36] != 17 || be16(&d[40..]) != internal {
        return None;
    }
    if d[3] != 0 {
        return Some(Err(Fail::No(format!("refused (result {})", d[3]))));
    }
    let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&d[44..60]).unwrap()).to_ipv4_mapped().filter(|ip| !ip.is_unspecified());
    Some(Ok((be16(&d[42..]), ip, be32(&d[4..]))))
}

fn pcp_map(gw: SocketAddr, client: Ipv4Addr, internal: u16, external: u16, nonce: [u8; 12]) -> Result<Mapping, Fail> {
    let req = pcp_request(client, &nonce, internal, external, LEASE);
    let (port, ip, life) = ask(client, gw, &req, 2, |d| parse_pcp(d, &nonce, internal))??;
    if port == 0 {
        return Err(Fail::No("no port given".into()));
    }
    Ok(Mapping {
        method: Method::Pcp,
        internal_port: internal,
        external_port: port,
        external_ip: ip,
        lease: Duration::from_secs(life.into()),
        at: Instant::now(),
        client,
        gateway: gw,
        nonce,
        control: None,
    })
}

fn natpmp_request(internal: u16, external: u16, lifetime: u32) -> [u8; 12] {
    let mut p = [0u8; 12];
    p[1] = 1;
    p[4..6].copy_from_slice(&internal.to_be_bytes());
    p[6..8].copy_from_slice(&external.to_be_bytes());
    p[8..12].copy_from_slice(&lifetime.to_be_bytes());
    p
}

fn parse_natpmp(d: &[u8], internal: u16) -> Option<Result<(u16, u32), Fail>> {
    if d.len() < 16 || d[0] != 0 || d[1] != 129 || be16(&d[8..]) != internal {
        return None;
    }
    match be16(&d[2..]) {
        0 => Some(Ok((be16(&d[10..]), be32(&d[12..])))),
        r => Some(Err(Fail::No(format!("refused (result {r})")))),
    }
}

fn parse_natpmp_addr(d: &[u8]) -> Option<Ipv4Addr> {
    (d.len() >= 12 && d[0] == 0 && d[1] == 128 && be16(&d[2..]) == 0).then(|| Ipv4Addr::new(d[8], d[9], d[10], d[11]))
}

fn natpmp_map(gw: SocketAddr, client: Ipv4Addr, internal: u16, external: u16, tries: u32) -> Result<Mapping, Fail> {
    let (port, life) = ask(client, gw, &natpmp_request(internal, external, LEASE), tries, |d| parse_natpmp(d, internal))??;
    if port == 0 {
        return Err(Fail::No("no port given".into()));
    }
    let ip = ask(client, gw, &[0, 0], 2, parse_natpmp_addr).ok().filter(|ip| !ip.is_unspecified());
    Ok(Mapping {
        method: Method::NatPmp,
        internal_port: internal,
        external_port: port,
        external_ip: ip,
        lease: Duration::from_secs(life.into()),
        at: Instant::now(),
        client,
        gateway: gw,
        nonce: [0; 12],
        control: None,
    })
}

// UPnP IGD.

fn header<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// "http://a.b.c.d[:port]/path" (routers give IP addresses, not names).
fn parse_url(u: &str) -> Option<(SocketAddr, String)> {
    let rest = u.get(..7).filter(|s| s.eq_ignore_ascii_case("http://")).map(|_| &u[7..])?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (h, p) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (hostport, 80),
    };
    let ip: Ipv4Addr = h.parse().ok()?;
    clean(path).then(|| (SocketAddr::new(ip.into(), p), path.to_string()))
}

/// Safe to put in a request line or header: printable ASCII, no quotes or brackets.
fn clean(s: &str) -> bool {
    !s.is_empty() && s.len() < 512 && s.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'<' | b'>' | b'\\'))
}

/// Gateways answering an SSDP search on the LAN: (address, description URL).
pub fn ssdp_search(lan: Ipv4Addr, wait: Duration) -> Vec<(Ipv4Addr, String)> {
    let Ok(sock) = UdpSocket::bind((lan, 0)) else { return Vec::new() };
    let _ = sock.set_multicast_ttl_v4(2);
    for st in [
        "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
        "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
        "urn:schemas-upnp-org:service:WANIPConnection:1",
        "urn:schemas-upnp-org:service:WANPPPConnection:1",
    ] {
        let q = format!("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\n\r\n");
        let _ = sock.send_to(q.as_bytes(), SSDP);
    }
    let mut out: Vec<(Ipv4Addr, String)> = Vec::new();
    let mut until = Instant::now() + wait;
    let mut buf = [0u8; 2048];
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let _ = sock.set_read_timeout(Some(left));
        let Ok((n, from)) = sock.recv_from(&mut buf) else {
            if out.is_empty() { continue } else { break }
        };
        let IpAddr::V4(ip) = from.ip() else { continue };
        let text = String::from_utf8_lossy(&buf[..n]);
        if !is_lan(ip) || !text.starts_with("HTTP/1.") {
            continue;
        }
        if let Some(loc) = header(&text, "location")
            && parse_url(loc).is_some_and(|(h, _)| h.ip() == IpAddr::V4(ip))
            && !out.iter().any(|(_, l)| l == loc)
            && out.len() < 4
        {
            out.push((ip, loc.to_string()));
            // The first answer is usually all there is: don't wait out the rest.
            until = until.min(Instant::now() + Duration::from_millis(300));
        }
    }
    out
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(i) = find(b, b"\r\n") {
        let line = String::from_utf8_lossy(&b[..i]);
        let Ok(size) = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16) else { break };
        b = &b[i + 2..];
        if size == 0 || size > b.len() {
            out.extend_from_slice(&b[..size.min(b.len())]);
            break;
        }
        out.extend_from_slice(&b[..size]);
        b = b.get(size + 2..).unwrap_or(&[]);
    }
    out
}

/// Status code and body of a raw HTTP response.
fn parse_http(raw: &[u8]) -> Result<(u16, String), String> {
    let end = find(raw, b"\r\n\r\n").ok_or("no HTTP headers")?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let status: u16 = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|s| s.parse().ok()).ok_or("not HTTP")?;
    let mut body = &raw[end + 4..];
    let owned;
    if header(&head, "transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        owned = dechunk(body);
        body = &owned;
    } else if let Some(n) = header(&head, "content-length").and_then(|v| v.parse::<usize>().ok()) {
        body = &body[..n.min(body.len())];
    }
    Ok((status, String::from_utf8_lossy(body).into_owned()))
}

/// Whether a response has all its body (so we needn't wait for the close).
fn complete(raw: &[u8]) -> bool {
    let Some(end) = find(raw, b"\r\n\r\n") else { return false };
    let head = String::from_utf8_lossy(&raw[..end]);
    if let Some(n) = header(&head, "content-length").and_then(|v| v.parse::<usize>().ok()) {
        return raw.len() >= end + 4 + n;
    }
    raw.ends_with(b"\r\n0\r\n\r\n")
}

fn http(host: SocketAddr, req: &str, timeout: Duration) -> Result<(u16, String), String> {
    let mut s = TcpStream::connect_timeout(&host, timeout).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    while Instant::now() < deadline {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > MAX_HTTP {
                    return Err("reply too big".into());
                }
                if complete(&raw) {
                    break;
                }
            }
            Err(e) if raw.is_empty() => return Err(e.to_string()),
            Err(_) => break,
        }
    }
    parse_http(&raw)
}

/// The text inside each element whose local name (any namespace prefix) is `name`.
fn elements<'a>(s: &'a str, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = s[i..].find('<') {
        let start = i + off + 1;
        let Some(len) = s[start..].find('>') else { break };
        let tag = &s[start..start + len];
        let after = start + len + 1;
        let tname = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        if !tag.starts_with(['/', '?', '!']) && !tag.ends_with('/') && tname.rsplit(':').next() == Some(name) {
            let close = format!("</{tname}>");
            if let Some(c) = s[after..].find(&close) {
                out.push(&s[after..after + c]);
                i = after + c + close.len();
                continue;
            }
        }
        i = after;
    }
    out
}

fn element<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    elements(s, name).into_iter().next().map(str::trim)
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The router's port mapping services, from its description at `location`: WAN IP
/// connection services first, then PPP ones.
fn describe(location: &str) -> Result<Vec<Control>, String> {
    let (host, path) = parse_url(location).ok_or("bad description URL")?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    let (code, body) = http(host, &req, HTTP_TIMEOUT)?;
    if code != 200 {
        return Err(format!("description: HTTP {code}"));
    }
    let base = element(&body, "URLBase").and_then(parse_url).filter(|(h, _)| h.ip() == host.ip()).unwrap_or((host, path));
    let mut out = Vec::new();
    for svc in elements(&body, "service") {
        let (Some(ty), Some(url)) = (element(svc, "serviceType"), element(svc, "controlURL")) else { continue };
        if !(ty.contains(":service:WANIPConnection:") || ty.contains(":service:WANPPPConnection:")) || !clean(ty) {
            continue;
        }
        let url = unescape(url);
        let control = if url.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("http://")) {
            parse_url(&url)
        } else if url.starts_with('/') {
            clean(&url).then(|| (base.0, url.clone()))
        } else {
            let dir = &base.1[..base.1.rfind('/').map_or(0, |i| i + 1)];
            let p = format!("{}{url}", if dir.is_empty() { "/" } else { dir });
            clean(&p).then_some((base.0, p))
        };
        // Stay on the router that answered.
        if let Some((h, p)) = control.filter(|(h, _)| h.ip() == host.ip()) {
            out.push(Control { host: h, path: p, service: ty.to_string() });
        }
    }
    out.sort_by_key(|c| (!c.service.contains("WANIPConnection"), std::cmp::Reverse(c.service.clone())));
    if out.is_empty() {
        return Err("the router has no port mapping service".into());
    }
    Ok(out)
}

#[derive(Debug)]
enum SoapErr {
    Code(u32),
    Other(String),
}

impl std::fmt::Display for SoapErr {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            SoapErr::Code(718) => write!(f, "the port is mapped to another PC"),
            SoapErr::Code(n) => write!(f, "the router refused (error {n})"),
            SoapErr::Other(s) => write!(f, "{s}"),
        }
    }
}

fn soap(c: &Control, action: &str, args: &[(&str, String)], timeout: Duration) -> Result<String, SoapErr> {
    let mut a = String::new();
    for (k, v) in args {
        a += &format!("<{k}>{}</{k}>", escape(v));
    }
    let body = format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{}\">{a}</u:{action}></s:Body></s:Envelope>\r\n",
        c.service
    );
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"{}#{action}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        c.path,
        c.host,
        c.service,
        body.len()
    );
    let (code, resp) = http(c.host, &req, timeout).map_err(SoapErr::Other)?;
    if code == 200 {
        return Ok(resp);
    }
    match element(&resp, "errorCode").and_then(|e| e.parse().ok()) {
        Some(n) => Err(SoapErr::Code(n)),
        None => Err(SoapErr::Other(format!("HTTP {code}"))),
    }
}

fn delete_args(external: u16) -> Vec<(&'static str, String)> {
    vec![("NewRemoteHost", String::new()), ("NewExternalPort", external.to_string()), ("NewProtocol", "UDP".into())]
}

fn add(c: &Control, client: Ipv4Addr, internal: u16, external: u16, lease: u32) -> Result<u32, SoapErr> {
    let args = |lease: u32| {
        vec![
            ("NewRemoteHost", String::new()),
            ("NewExternalPort", external.to_string()),
            ("NewProtocol", "UDP".to_string()),
            ("NewInternalPort", internal.to_string()),
            ("NewInternalClient", client.to_string()),
            ("NewEnabled", "1".to_string()),
            ("NewPortMappingDescription", DESCRIPTION.to_string()),
            ("NewLeaseDuration", lease.to_string()),
        ]
    };
    match soap(c, "AddPortMapping", &args(lease), HTTP_TIMEOUT) {
        Ok(_) => Ok(lease),
        // OnlyPermanentLeasesSupported: we delete it ourselves.
        Err(SoapErr::Code(725)) if lease != 0 => soap(c, "AddPortMapping", &args(0), HTTP_TIMEOUT).map(|_| 0),
        Err(e) => Err(e),
    }
}

/// Maps `external` to us, taking it over when the router says it's taken but it's ours
/// (left from an earlier run, or a renewal the router won't take as one).
fn add_or_reclaim(c: &Control, client: Ipv4Addr, internal: u16, external: u16) -> Result<u32, SoapErr> {
    match add(c, client, internal, external, UPNP_LEASE) {
        Err(SoapErr::Code(718)) => {
            let r = soap(c, "GetSpecificPortMappingEntry", &delete_args(external), HTTP_TIMEOUT)?;
            let ours = element(&r, "NewInternalClient") == Some(client.to_string().as_str())
                && element(&r, "NewInternalPort").and_then(|p| p.parse().ok()) == Some(internal);
            if !ours {
                return Err(SoapErr::Code(718));
            }
            let _ = soap(c, "DeletePortMapping", &delete_args(external), HTTP_TIMEOUT);
            add(c, client, internal, external, UPNP_LEASE)
        }
        r => r,
    }
}

fn upnp_one(c: &Control, client: Ipv4Addr, internal: u16, external: u16) -> Result<Mapping, SoapErr> {
    let lease = add_or_reclaim(c, client, internal, external)?;
    let ip = soap(c, "GetExternalIPAddress", &[], HTTP_TIMEOUT)
        .ok()
        .and_then(|r| element(&r, "NewExternalIPAddress").and_then(|s| s.parse::<Ipv4Addr>().ok()))
        .filter(|ip| !ip.is_unspecified());
    Ok(Mapping {
        method: Method::Upnp,
        internal_port: internal,
        external_port: external,
        external_ip: ip,
        lease: Duration::from_secs(lease.into()),
        at: Instant::now(),
        client,
        gateway: c.host,
        nonce: [0; 12],
        control: Some(c.clone()),
    })
}

/// Our port as the external one, else up to three random high ones.
fn upnp_map(controls: &[Control], client: Ipv4Addr, port: u16) -> Result<Mapping, String> {
    let mut last = String::from("no service");
    for c in controls {
        let mut ports = vec![port];
        for _ in 0..3 {
            ports.push(49152 + u16::from_le_bytes(super::crypto::random_array()) % 16384);
        }
        for ext in ports {
            match upnp_one(c, client, port, ext) {
                Ok(m) => return Ok(m),
                Err(SoapErr::Code(718)) => last = SoapErr::Code(718).to_string(),
                Err(e) => {
                    last = e.to_string();
                    break;
                }
            }
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const LO: Ipv4Addr = Ipv4Addr::LOCALHOST;
    const PUB: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 5);

    fn fake_udp(answer: impl Fn(&[u8]) -> Option<Vec<u8>> + Send + 'static) -> (SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>) {
        let s = UdpSocket::bind((LO, 0)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let addr = s.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1100];
            while let Ok((n, from)) = s.recv_from(&mut buf) {
                log.lock().unwrap().push(buf[..n].to_vec());
                if let Some(r) = answer(&buf[..n]) {
                    let _ = s.send_to(&r, from);
                }
            }
        });
        (addr, seen)
    }

    fn pcp_answer(req: &[u8], port: u16, ip: Ipv4Addr) -> Vec<u8> {
        let mut r = vec![0u8; 60];
        r[0] = 2;
        r[1] = 0x81;
        r[4..8].copy_from_slice(&req[4..8]);
        r[24..42].copy_from_slice(&req[24..42]);
        r[42..44].copy_from_slice(&port.to_be_bytes());
        r[44..60].copy_from_slice(&ip.to_ipv6_mapped().octets());
        r
    }

    #[test]
    fn pcp_maps_renews_and_deletes() {
        let (gw, seen) = fake_udp(|req| (req.len() == 60 && req[0] == 2).then(|| pcp_answer(req, 40000, PUB)));
        let mut m = map_pmp(gw, LO, 1234).unwrap();
        assert_eq!((m.method, m.internal_port, m.external_port), (Method::Pcp, 1234, 40000));
        assert_eq!(m.external(), Some(SocketAddr::new(PUB.into(), 40000)));
        assert_eq!(m.lease, Duration::from_secs(LEASE.into()));
        assert!(m.renew_at() > Instant::now() + Duration::from_secs(3000));
        {
            let s = seen.lock().unwrap();
            let r = &s[0];
            assert_eq!(&r[8..24], &LO.to_ipv6_mapped().octets());
            assert_eq!(r[36], 17);
            assert_eq!(be16(&r[40..]), 1234);
            assert_eq!(be16(&r[42..]), 1234, "asks for the same port outside");
        }
        m.renew().unwrap();
        assert_eq!(m.external_port, 40000);
        let nonce = seen.lock().unwrap()[0][24..36].to_vec();
        assert_eq!(seen.lock().unwrap().last().unwrap()[24..36], nonce[..], "a renewal keeps the nonce");
        m.remove();
        std::thread::sleep(Duration::from_millis(200));
        let s = seen.lock().unwrap();
        assert_eq!(be32(&s.last().unwrap()[4..]), 0, "deleting asks for no lifetime");
    }

    #[test]
    fn pcp_ignores_answers_that_dont_match() {
        let (gw, _) = fake_udp(|req| {
            let mut r = pcp_answer(req, 40000, PUB);
            r[24] ^= 1; // another nonce
            Some(r)
        });
        let t = Instant::now();
        assert!(map_pmp(gw, LO, 1234).is_err());
        assert!(t.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn falls_back_to_nat_pmp() {
        let (gw, _) = fake_udp(|req| match (req.len(), req[0], req.get(1)) {
            (_, 2, _) => Some(vec![0, 0x81, 0, 1, 0, 0, 0, 0]),
            (12, 0, Some(1)) => {
                let mut r = vec![0u8; 16];
                r[1] = 129;
                r[8..10].copy_from_slice(&req[4..6]);
                r[10..12].copy_from_slice(&41000u16.to_be_bytes());
                r[12..16].copy_from_slice(&3600u32.to_be_bytes());
                Some(r)
            }
            (2, 0, Some(0)) => Some(vec![0, 128, 0, 0, 0, 0, 0, 1, 203, 0, 113, 5]),
            _ => None,
        });
        let m = map_pmp(gw, LO, 1234).unwrap();
        assert_eq!((m.method, m.external_port), (Method::NatPmp, 41000));
        assert_eq!(m.external(), Some(SocketAddr::new(PUB.into(), 41000)));
        assert_eq!(m.lease, Duration::from_secs(3600));
    }

    #[test]
    fn a_private_external_address_isnt_used() {
        let (gw, _) = fake_udp(|req| Some(pcp_answer(req, 40000, Ipv4Addr::new(100, 64, 3, 4))));
        let m = map_pmp(gw, LO, 1234).unwrap();
        assert_eq!(m.external(), None, "carrier-grade NAT beyond the router");
    }

    #[test]
    fn nobody_listening_fails_fast() {
        let s = UdpSocket::bind((LO, 0)).unwrap();
        let gw = s.local_addr().unwrap();
        drop(s);
        let t = Instant::now();
        assert!(map_pmp(gw, LO, 1234).is_err());
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }

    #[test]
    fn urls_and_xml() {
        assert_eq!(parse_url("http://192.168.1.1:1900/igd.xml"), Some(("192.168.1.1:1900".parse().unwrap(), "/igd.xml".into())));
        assert_eq!(parse_url("HTTP://10.0.0.1"), Some(("10.0.0.1:80".parse().unwrap(), "/".into())));
        assert_eq!(parse_url("http://router.local/x"), None);
        assert_eq!(parse_url("https://10.0.0.1/x"), None);
        assert_eq!(parse_url("http://10.0.0.1/a b"), None);
        assert_eq!(parse_url("http://10.0.0.1/a\r\nX: y"), None);
        let x = "<root><s:service><serviceType>A</serviceType></s:service><service/><service><serviceType> B </serviceType></service></root>";
        assert_eq!(elements(x, "service").len(), 2);
        assert_eq!(element(x, "serviceType"), Some("A"));
        assert_eq!(elements(x, "serviceType")[1].trim(), "B");
        assert_eq!(element("<a>", "a"), None);
        assert_eq!(dechunk(b"4\r\nWiki\r\n5;x\r\npedia\r\n0\r\n\r\n"), b"Wikipedia");
        let (code, body) = parse_http(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhiXX").unwrap();
        assert_eq!((code, body.as_str()), (200, "hi"));
        assert!(parse_http(b"garbage").is_err());
    }

    /// A UPnP router on loopback: description (chunked), and SOAP that refuses the
    /// first port as another PC's.
    fn fake_router(foreign_control: bool) -> (String, Arc<Mutex<Vec<String>>>) {
        let l = TcpListener::bind((LO, 0)).unwrap();
        let addr = l.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                while !complete(&raw) {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => raw.extend_from_slice(&buf[..n]),
                    }
                    if find(&raw, b"\r\n\r\n").is_some() && raw.starts_with(b"GET") {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&raw).into_owned();
                log.lock().unwrap().push(req.clone());
                let (code, body) = if req.starts_with("GET /desc.xml") {
                    let ctl = if foreign_control { "http://192.0.2.9/ctl" } else { "ctl" };
                    (200, format!(
                        "<?xml version=\"1.0\"?><root><device><serviceList><service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType><controlURL>/l3f</controlURL></service></serviceList><deviceList><device><serviceList><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>{ctl}</controlURL></service></serviceList></device></deviceList></device></root>"
                    ))
                } else if req.contains("#AddPortMapping") && req.contains("<NewExternalPort>1234<") {
                    (500, "<s:Envelope><s:Body><s:Fault><detail><UPnPError><errorCode>718</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>".to_string())
                } else if req.contains("#GetSpecificPortMappingEntry") {
                    (200, "<u:R><NewInternalPort>9</NewInternalPort><NewInternalClient>192.168.1.50</NewInternalClient></u:R>".to_string())
                } else if req.contains("#AddPortMapping") || req.contains("#DeletePortMapping") {
                    (200, "<ok/>".to_string())
                } else if req.contains("#GetExternalIPAddress") {
                    (200, "<u:R><NewExternalIPAddress>203.0.113.5</NewExternalIPAddress></u:R>".to_string())
                } else {
                    (404, String::new())
                };
                let chunked = req.starts_with("GET");
                let resp = if chunked {
                    let (a, b) = body.split_at(body.len() / 2);
                    format!("HTTP/1.1 {code} X\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n", a.len(), b.len())
                } else {
                    format!("HTTP/1.1 {code} X\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                };
                let _ = s.write_all(resp.as_bytes());
            }
        });
        (format!("http://{addr}/desc.xml"), seen)
    }

    #[test]
    fn upnp_maps_another_port_when_ours_is_taken() {
        let (loc, seen) = fake_router(false);
        let cs = describe(&loc).unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].path, "/ctl");
        let m = upnp_map(&cs, LO, 1234).unwrap();
        assert_eq!(m.method, Method::Upnp);
        assert_ne!(m.external_port, 1234, "another PC has 1234");
        assert_eq!(m.external(), Some(SocketAddr::new(PUB.into(), m.external_port)));
        assert_eq!(m.lease, Duration::from_secs(UPNP_LEASE.into()));
        m.remove();
        let s = seen.lock().unwrap();
        let add = s.iter().rev().find(|r| r.contains("#AddPortMapping")).unwrap();
        assert!(add.contains("<NewInternalClient>127.0.0.1<") && add.contains("<NewInternalPort>1234<") && add.contains("<NewProtocol>UDP<"));
        assert!(s.last().unwrap().contains("#DeletePortMapping") && s.last().unwrap().contains(&format!("<NewExternalPort>{}<", m.external_port)));
    }

    #[test]
    fn upnp_stays_on_the_router_that_answered() {
        let (loc, _) = fake_router(true);
        assert!(describe(&loc).is_err());
    }

    /// The real router: `cargo test -p peerparty_core portmap -- --ignored --nocapture`.
    /// Maps a UDP port, renews and deletes it; prints which way worked, no addresses.
    #[test]
    #[ignore]
    fn live_router() {
        let s = UdpSocket::bind("0.0.0.0:0").unwrap();
        s.connect("192.0.2.1:9").unwrap();
        let IpAddr::V4(lan) = s.local_addr().unwrap().ip() else { panic!("no IPv4") };
        let port = UdpSocket::bind("0.0.0.0:0").unwrap().local_addr().unwrap().port();
        let gw = default_gateway();
        println!("gateway found: {}", gw.is_some());
        let t = Instant::now();
        let mut m = map(lan, gw, port).unwrap_or_else(|e| panic!("no mapping: {e}"));
        println!(
            "{} mapped UDP {port} -> external port {} (same: {}), lease {:?}, public address known: {}, in {:?}",
            m.method.name(), m.external_port, m.external_port == port, m.lease, m.external().is_some(), t.elapsed()
        );
        m.renew().unwrap();
        println!("renewed: external port {}", m.external_port);
        m.remove();
        println!("deleted");
    }
}
