//! WebSocket (RFC 6455) for the servers the stand-ins run: the handshake's accept key and
//! the framing. Client frames are masked, the server's aren't. Photon's WebSocket
//! transport (photon/ws.rs) and Unity's Wire (ugs/wire.rs) speak their own protocols
//! inside these frames.

use std::io::{self, Read};

/// Text frames (Wire's).
pub const OP_TEXT: u8 = 1;
pub const OP_BINARY: u8 = 2;
pub const OP_CLOSE: u8 = 8;
pub const OP_PING: u8 = 9;
pub const OP_PONG: u8 = 10;

/// RFC 6455 section 1.3's GUID, appended to the client's key.
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// RFC 6455 section 4.2.2: the server's Sec-WebSocket-Accept for the client's key.
pub fn accept_key(key: &str) -> String {
    crate::util::encoding::base64(&crate::util::crypto::sha1(format!("{}{GUID}", key.trim()).as_bytes()))
}

/// A server frame (unmasked), FIN set.
pub fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => out.push(n as u8),
        n if n <= 0xffff => {
            out.push(126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

/// One complete frame at the start of `buf`: (fin, opcode, unmasked payload, bytes used).
/// None while it isn't all there (or its length can't be one).
pub fn parse_frame(buf: &[u8]) -> Option<(bool, u8, Vec<u8>, usize)> {
    if buf.len() < 2 {
        return None;
    }
    let fin = buf[0] & 0x80 != 0;
    let opcode = buf[0] & 0x0f;
    let masked = buf[1] & 0x80 != 0;
    let mut len = (buf[1] & 0x7f) as u64;
    let mut at = 2;
    if len == 126 {
        if buf.len() < 4 {
            return None;
        }
        len = u16::from_be_bytes([buf[2], buf[3]]) as u64;
        at = 4;
    } else if len == 127 {
        if buf.len() < 10 {
            return None;
        }
        len = u64::from_be_bytes(buf[2..10].try_into().unwrap());
        at = 10;
    }
    let mask = if masked {
        if buf.len() < at + 4 {
            return None;
        }
        let m = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
        at += 4;
        Some(m)
    } else {
        None
    };
    let end = at.checked_add(usize::try_from(len).ok()?)?;
    if buf.len() < end {
        return None;
    }
    let mut payload = buf[at..end].to_vec();
    if let Some(m) = mask {
        unmask(&mut payload, m);
    }
    Some((fin, opcode, payload, end))
}

/// Reads one frame from a stream: (fin, opcode, unmasked payload). A frame longer than
/// `max` is an error before anything is allocated for it.
pub fn read_frame(s: &mut impl Read, max: usize) -> io::Result<(bool, u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    s.read_exact(&mut h)?;
    let (fin, opcode, masked) = (h[0] & 0x80 != 0, h[0] & 0x0F, h[1] & 0x80 != 0);
    let len = match h[1] & 0x7F {
        126 => {
            let mut b = [0u8; 2];
            s.read_exact(&mut b)?;
            u16::from_be_bytes(b) as u64
        }
        127 => {
            let mut b = [0u8; 8];
            s.read_exact(&mut b)?;
            u64::from_be_bytes(b)
        }
        n => n as u64,
    };
    if len > max as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too big"));
    }
    let mut mask = [0u8; 4];
    if masked {
        s.read_exact(&mut mask)?;
    }
    let mut payload = vec![0u8; len as usize];
    s.read_exact(&mut payload)?;
    if masked {
        unmask(&mut payload, mask);
    }
    Ok((fin, opcode, payload))
}

fn unmask(payload: &mut [u8], mask: [u8; 4]) {
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_matches_rfc_6455() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert_eq!(accept_key(" dGhlIHNhbXBsZSBub25jZQ== "), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn frames_round_trip_with_masks_and_long_lengths() {
        // RFC 6455 section 5.7: a masked "Hello".
        let masked = [0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58];
        let (fin, op, p, used) = parse_frame(&masked).unwrap();
        assert!(fin && op == OP_TEXT && p == b"Hello" && used == masked.len());
        assert!(parse_frame(&masked[..6]).is_none(), "incomplete frames wait");
        assert_eq!(read_frame(&mut io::Cursor::new(masked), 5).unwrap().2, b"Hello");
        for n in [0usize, 125, 126, 300, 65535, 70000] {
            let data = vec![7u8; n];
            let f = frame(OP_BINARY, &data);
            let (_, op, p, used) = parse_frame(&f).unwrap();
            assert_eq!((op, p.len(), used), (OP_BINARY, n, f.len()));
            let (fin, op, got) = read_frame(&mut io::Cursor::new(f), 70000).unwrap();
            assert!(fin && op == OP_BINARY && got == data);
        }
    }

    #[test]
    fn huge_lengths_are_refused_without_allocating() {
        // A 64-bit length near the largest number: not a frame anything waits for.
        let mut f = vec![0x82, 0x7f];
        f.extend_from_slice(&u64::MAX.to_be_bytes());
        assert!(read_frame(&mut io::Cursor::new(f.clone()), 1 << 20).is_err());
        assert!(parse_frame(&f).is_none());
        let mut f = vec![0x82, 0xff];
        f.extend_from_slice(&(usize::MAX as u64 - 2).to_be_bytes());
        f.extend_from_slice(&[1, 2, 3, 4]);
        assert!(parse_frame(&f).is_none(), "the length plus the header overflows");
    }
}
