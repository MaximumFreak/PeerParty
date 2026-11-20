//! Demonware's own STUN (not RFC 5389): three header bytes (type, version, 0), then for
//! an address request (30) the answer 31 with the sender's address, and for a NAT
//! request (20) the answer 21 with the sender's address and the server's. Addresses are
//! 4 bytes in network order and a little-endian port. The sender's address reported is
//! this PC's LAN address with the port it sent from, so other players reach it directly.

use std::net::{SocketAddr, UdpSocket};

use crate::log;

use super::hex;

pub const IP_REQUEST: u8 = 30;
pub const IP_REPLY: u8 = 31;
pub const NAT_REQUEST: u8 = 20;
pub const NAT_REPLY: u8 = 21;

/// The answer to one packet from `from`, or None for one this doesn't know.
pub fn answer(packet: &[u8], from: SocketAddr, server: SocketAddr) -> Option<Vec<u8>> {
    let kind = *packet.first()?;
    let port = from.port();
    let me = super::lan_address().octets();
    let mut out = Vec::with_capacity(15);
    match kind {
        IP_REQUEST => {
            out.extend_from_slice(&[IP_REPLY, 2, 0]);
            out.extend_from_slice(&me);
            out.extend_from_slice(&port.to_le_bytes());
        }
        NAT_REQUEST => {
            out.extend_from_slice(&[NAT_REPLY, 2, 0]);
            out.extend_from_slice(&me);
            out.extend_from_slice(&port.to_le_bytes());
            let s = match server.ip() {
                std::net::IpAddr::V4(v4) => v4.octets(),
                _ => [127, 0, 0, 1],
            };
            out.extend_from_slice(&s);
            out.extend_from_slice(&server.port().to_le_bytes());
        }
        _ => return None,
    }
    Some(out)
}

pub fn serve(sock: UdpSocket) {
    let server = sock.local_addr().ok();
    let mut buf = [0u8; 2048];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
        let packet = &buf[..n];
        let kind = packet.first().copied().unwrap_or(0);
        let Some(server) = server else { continue };
        match answer(packet, from, server) {
            Some(reply) => {
                crate::log::throttled(&format!("demonware stun {kind}"), 3, |c| {
                    log!(
                        "demonware stun: request {kind} from {from}: answered {} ({}:{}) n={c}",
                        hex(&reply),
                        super::lan_address(),
                        from.port()
                    )
                });
                let _ = sock.send_to(&reply, from);
            }
            None => crate::log::throttled(&format!("demonware stun unknown {kind}"), 5, |c| {
                log!("demonware stun: unknown packet from {from}: {} n={c}", hex(packet))
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stun_answers_address_and_nat_requests() {
        let from: SocketAddr = "127.0.0.1:47590".parse().unwrap();
        let server: SocketAddr = "127.80.1.2:3074".parse().unwrap();
        let me = crate::demonware::lan_address().octets();
        let ip = answer(&[IP_REQUEST, 2, 0], from, server).unwrap();
        assert_eq!(ip[..3], [IP_REPLY, 2, 0]);
        assert_eq!(ip[3..7], me);
        assert_eq!(u16::from_le_bytes([ip[7], ip[8]]), 47590);
        let nat = answer(&[NAT_REQUEST, 2, 0], from, server).unwrap();
        assert_eq!(nat[..3], [NAT_REPLY, 2, 0]);
        assert_eq!(nat[9..13], [127, 80, 1, 2]);
        assert_eq!(u16::from_le_bytes([nat[13], nat[14]]), 3074);
        assert!(answer(&[99, 2, 0], from, server).is_none());
        assert!(answer(&[], from, server).is_none());
    }
}
