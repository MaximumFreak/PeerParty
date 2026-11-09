//! STUN binding requests (RFC 5389): asking a public server which address and port our
//! UDP socket shows up as on the internet. Comparing two servers' answers tells whether
//! the NAT keeps one mapping for every destination (hole punching works) or picks a new
//! port per destination (symmetric: punching between two of those mostly fails).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub const COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_RESPONSE: u16 = 0x0101;
const MAPPED_ADDRESS: u16 = 0x0001;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// The pre-RFC 5389 number some old servers still use.
const XOR_MAPPED_ADDRESS_OLD: u16 = 0x8020;

pub const SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.cloudflare.com:3478",
];

pub fn request(txid: &[u8; 12]) -> Vec<u8> {
    let mut b = Vec::with_capacity(20);
    b.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&COOKIE);
    b.extend_from_slice(txid);
    b
}

/// Whether a datagram looks like STUN (and so isn't ours or the DHT's).
pub fn is_stun(d: &[u8]) -> bool {
    d.len() >= 20 && d[0] & 0xC0 == 0 && d[4..8] == COOKIE
}

/// A binding response's transaction id and the address it reports.
pub fn parse_response(d: &[u8]) -> Option<([u8; 12], SocketAddr)> {
    if !is_stun(d) || u16::from_be_bytes([d[0], d[1]]) != BINDING_RESPONSE {
        return None;
    }
    let len = u16::from_be_bytes([d[2], d[3]]) as usize;
    let body = d.get(20..20 + len)?;
    let txid: [u8; 12] = d[8..20].try_into().ok()?;
    let (mut plain, mut xored) = (None, None);
    let mut i = 0;
    while i + 4 <= body.len() {
        let kind = u16::from_be_bytes([body[i], body[i + 1]]);
        let alen = u16::from_be_bytes([body[i + 2], body[i + 3]]) as usize;
        let v = body.get(i + 4..i + 4 + alen)?;
        match kind {
            MAPPED_ADDRESS => plain = address(v, None),
            XOR_MAPPED_ADDRESS | XOR_MAPPED_ADDRESS_OLD => xored = address(v, Some(&d[4..20])),
            _ => {}
        }
        i += 4 + alen.div_ceil(4) * 4;
    }
    Some((txid, xored.or(plain)?))
}

/// A (XOR-)MAPPED-ADDRESS value; `xor` is the cookie and transaction id.
fn address(v: &[u8], xor: Option<&[u8]>) -> Option<SocketAddr> {
    let family = *v.get(1)?;
    let mut port = u16::from_be_bytes([*v.get(2)?, *v.get(3)?]);
    let mut ip = v.get(4..)?.to_vec();
    if let Some(x) = xor {
        port ^= u16::from_be_bytes([x[0], x[1]]);
        for (b, k) in ip.iter_mut().zip(x) {
            *b ^= k;
        }
    }
    let ip = match family {
        1 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(ip.get(..4)?).ok()?)),
        2 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(ip.get(..16)?).ok()?)),
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// What the answers say about our NAT.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Nat {
    #[default]
    Unknown,
    /// The internet sees our own address: no NAT in the way.
    Open,
    /// One public port for every destination: hole punching should work.
    Friendly,
    /// A different public port per destination: punching works only with a friendly peer
    /// (and not always then).
    Symmetric,
}

impl Nat {
    /// From the mapped addresses two or more servers reported, and our socket's address.
    pub fn classify(local: Option<SocketAddr>, mapped: &[SocketAddr]) -> Nat {
        let Some(first) = mapped.first() else { return Nat::Unknown };
        if local.is_some_and(|l| l == *first) {
            return Nat::Open;
        }
        if mapped.len() < 2 {
            return Nat::Unknown;
        }
        if mapped.iter().all(|m| m == first) { Nat::Friendly } else { Nat::Symmetric }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Nat::Unknown => "unknown",
            Nat::Open => "open (no NAT)",
            Nat::Friendly => "hole-punch friendly",
            Nat::Symmetric => "symmetric (hole punching may fail)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A binding response as a server sends it, XOR-MAPPED-ADDRESS first.
    fn response(txid: &[u8; 12], addr: SocketAddr) -> Vec<u8> {
        let SocketAddr::V4(a) = addr else { panic!() };
        let mut attr = vec![0, 1];
        attr.extend_from_slice(&(a.port() ^ 0x2112).to_be_bytes());
        for (b, k) in a.ip().octets().iter().zip(COOKIE) {
            attr.push(b ^ k);
        }
        let mut d = Vec::new();
        d.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        d.extend_from_slice(&((4 + attr.len()) as u16).to_be_bytes());
        d.extend_from_slice(&COOKIE);
        d.extend_from_slice(txid);
        d.extend_from_slice(&XOR_MAPPED_ADDRESS.to_be_bytes());
        d.extend_from_slice(&(attr.len() as u16).to_be_bytes());
        d.extend_from_slice(&attr);
        d
    }

    #[test]
    fn parses_a_binding_response() {
        let txid = [7u8; 12];
        let req = request(&txid);
        assert_eq!(req.len(), 20);
        assert!(is_stun(&req));
        let addr: SocketAddr = "203.0.113.9:54321".parse().unwrap();
        assert_eq!(parse_response(&response(&txid, addr)), Some((txid, addr)));
        assert_eq!(parse_response(&req), None, "a request isn't a response");
        let r = response(&txid, addr);
        assert_eq!(parse_response(&r[..r.len() - 2]), None, "truncated");
        assert!(!is_stun(b"d1:rd2:id20:aaaaaaaaaaaaaaaaaaaaee"));
    }

    #[test]
    fn classifies_nats() {
        let a: SocketAddr = "203.0.113.9:1000".parse().unwrap();
        let b: SocketAddr = "203.0.113.9:1001".parse().unwrap();
        assert_eq!(Nat::classify(None, &[a, a]), Nat::Friendly);
        assert_eq!(Nat::classify(None, &[a, b]), Nat::Symmetric);
        assert_eq!(Nat::classify(Some(a), &[a]), Nat::Open);
        assert_eq!(Nat::classify(None, &[a]), Nat::Unknown);
        assert_eq!(Nat::classify(None, &[]), Nat::Unknown);
    }
}