//! Photon's WebSocket transport (`ConnectionProtocol.WebSocket`), served like TCP: each
//! WebSocket connection becomes an ENet peer of the server's own (host.rs).
//!
//! What the client does, as seen from Photon's own client on the wire:
//! - It opens `ws://<server>:<port>/?libversion=<a.b.c.d>&sid=<n>&app=<id>` (plus
//!   `&xInit=` when it carries a token) with the serialization protocol as the
//!   subprotocol (`GpBinaryV16` / `GpBinaryV18`).
//! - Each binary message is one Photon message (`0xF3 ...`) without TCP's 7-byte frame.
//! - Without a token it sends no Init: its first message is a ping, an internal
//!   operation request (kind 6, code 1, parameter 1 = its time), and the answer (kind
//!   7, parameters 1 = its time, 2 = the server's) counts as the Init response. So the
//!   Init the server needs is made here from the URL, and pings are answered here.
//! - With a token it sends TCP's framed Init (`0xFB`, length, channel, mode, then the
//!   HTTP-style Init with the token), which goes to the server as it is.
//!
//! WebSocket framing per RFC 6455: the handshake's `Sec-WebSocket-Accept` is the
//! base64 SHA-1 of the key and the RFC's GUID; client frames are masked, ours aren't.

use super::message::Proto;
use super::value::{Msg, Response, Value};
use std::io::{self, Read, Write};

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_MESSAGE: usize = 1 << 20;

/// Text frames aren't sent (only binary ones); kept to document the opcode.
#[allow(dead_code)]
pub const OP_TEXT: u8 = 1;
pub const OP_BINARY: u8 = 2;
pub const OP_CLOSE: u8 = 8;
pub const OP_PING: u8 = 9;
pub const OP_PONG: u8 = 10;

/// What the client asked for in its handshake.
pub struct Handshake {
    pub proto: Proto,
    /// The Init to hand the server, unless the client sends its own (token).
    pub init: Option<Vec<u8>>,
}

/// One frame from the server: unmasked, final.
pub fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(payload.len() + 10);
    f.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => f.push(n as u8),
        n if n <= 0xFFFF => {
            f.push(126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            f.push(127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    f.extend_from_slice(payload);
    f
}

fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == name).then_some(v)
    })
}

/// Reads the client's upgrade request and answers it (101). Err when it isn't one.
pub fn accept(s: &mut (impl Read + Write)) -> io::Result<Handshake> {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 << 10 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "handshake too long"));
        }
        s.read_exact(&mut b)?;
        head.push(b[0]);
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut lines = text.split("\r\n");
    let target = lines.next().and_then(|l| l.split(' ').nth(1)).unwrap_or("/").to_string();
    let mut key = None;
    let mut protocols = String::new();
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        match k.trim().to_ascii_lowercase().as_str() {
            "sec-websocket-key" => key = Some(v.trim().to_string()),
            "sec-websocket-protocol" => protocols = v.trim().to_string(),
            _ => {}
        }
    }
    let Some(key) = key else {
        let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a WebSocket upgrade"));
    };
    let offered: Vec<&str> = protocols.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
    let chosen = ["GpBinaryV18", "GpBinaryV16"].into_iter().find(|p| offered.contains(p));
    let accept = crate::ugs::base64(&crate::ugs::wire::sha1(format!("{key}{GUID}").as_bytes()));
    let mut reply = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n");
    if let Some(p) = chosen {
        reply += &format!("Sec-WebSocket-Protocol: {p}\r\n");
    }
    reply += "\r\n";
    s.write_all(reply.as_bytes())?;
    let proto = if chosen == Some("GpBinaryV16") { Proto::V16 } else { Proto::V18 };
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let init = if query_param(query, "xInit").is_some() {
        None
    } else {
        let app = query_param(query, "app").unwrap_or("LoadBalancing");
        let version = query_param(query, "libversion").unwrap_or("4.1.0.0");
        let sid = query_param(query, "sid").unwrap_or("0");
        let ipv6 = if query_param(query, "IPv6").is_some() { "&IPv6" } else { "" };
        Some(format!("POST /?init&app={app}&clientversion={version}&protocol={}&sid={sid}{ipv6} HTTP/1.1\r\n\r\n", proto.name()).into_bytes())
    };
    Ok(Handshake { proto, init })
}

/// Reads one frame: (fin, opcode, payload, unmasked).
pub fn read_frame(s: &mut impl Read) -> io::Result<(bool, u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    s.read_exact(&mut h)?;
    let (fin, opcode, masked) = (h[0] & 0x80 != 0, h[0] & 0x0F, h[1] & 0x80 != 0);
    let len = match h[1] & 0x7F {
        126 => {
            let mut b = [0u8; 2];
            s.read_exact(&mut b)?;
            u16::from_be_bytes(b) as usize
        }
        127 => {
            let mut b = [0u8; 8];
            s.read_exact(&mut b)?;
            u64::from_be_bytes(b) as usize
        }
        n => n as usize,
    };
    if len > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too big"));
    }
    let mut mask = [0u8; 4];
    if masked {
        s.read_exact(&mut mask)?;
    }
    let mut payload = vec![0u8; len];
    s.read_exact(&mut payload)?;
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    Ok((fin, opcode, payload))
}

/// What one whole client message is for the server.
pub enum Message {
    /// A Photon message for the server (a framed Init with its frame taken off).
    ToServer(Vec<u8>),
    /// A ping, with the answer to send back.
    Ping(Vec<u8>),
}

/// Sorts a client message; `server_time` stamps ping answers.
pub fn sort(msg: Vec<u8>, proto: Proto, server_time: i32) -> Message {
    if msg.first() == Some(&0xFB) && msg.len() >= 7 {
        return Message::ToServer(msg[7..].to_vec());
    }
    // An internal operation request (kind 6), code 1: a ping.
    if msg.len() >= 3 && msg[0] == 0xF3 && msg[1] & 0x7F == 6 && msg[2] == 1 {
        if let Ok((Msg::InternalRequest(q), _)) = proto.decode(&msg, None) {
            let theirs = q.params.iter().find(|(k, _)| *k == 1).map(|(_, v)| v.clone()).unwrap_or(Value::Int(0));
            let resp = Msg::InternalResponse(Response { code: 1, ret: 0, debug: None, params: vec![(1, theirs), (2, Value::Int(server_time))] });
            if let Ok(bytes) = proto.encode(&resp, None) {
                return Message::Ping(bytes);
            }
        }
    }
    Message::ToServer(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::value::Request;

    struct Duplex {
        input: io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Read for Duplex {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.input.read(b)
        }
    }
    impl Write for Duplex {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn handshake_answers_with_the_rfc_example_key_and_makes_the_init() {
        let req = "GET /?libversion=4.1.6.10&sid=10&app=myapp HTTP/1.1\r\nHost: ns\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                   Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: GpBinaryV16\r\nSec-WebSocket-Version: 13\r\n\r\n";
        let mut d = Duplex { input: io::Cursor::new(req.as_bytes().to_vec()), output: Vec::new() };
        let h = accept(&mut d).unwrap();
        let reply = String::from_utf8(d.output).unwrap();
        assert!(reply.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"), "{reply}");
        assert!(reply.contains("Sec-WebSocket-Protocol: GpBinaryV16\r\n"));
        assert_eq!(h.proto, Proto::V16);
        let hello = super::super::message::parse_hello(&h.init.unwrap()).unwrap();
        assert_eq!(hello.init.app_id, "myapp");
        assert_eq!((hello.init.proto_major, hello.init.proto_minor), (1, 6));
        assert!(hello.token.is_none());
        // With a token the client sends its own Init.
        let req = req.replace("app=myapp", "app=myapp&xInit=");
        let mut d = Duplex { input: io::Cursor::new(req.into_bytes()), output: Vec::new() };
        assert!(accept(&mut d).unwrap().init.is_none());
    }

    #[test]
    fn frames_round_trip_and_pings_are_answered() {
        for n in [5usize, 300, 70000] {
            let payload = vec![7u8; n];
            let f = frame(OP_BINARY, &payload);
            let (fin, op, got) = read_frame(&mut io::Cursor::new(f)).unwrap();
            assert!(fin && op == OP_BINARY && got == payload);
        }
        // A masked client frame.
        let mut f = vec![0x82, 0x83, 1, 2, 3, 4];
        f.extend([b'a' ^ 1, b'b' ^ 2, b'c' ^ 3]);
        assert_eq!(read_frame(&mut io::Cursor::new(f)).unwrap().2, b"abc");
        for proto in [Proto::V16, Proto::V18] {
            let ping = proto.encode(&Msg::InternalRequest(Request { code: 1, params: vec![(1, Value::Int(1234))] }), None).unwrap();
            let Message::Ping(answer) = sort(ping, proto, 99) else { panic!("not a ping") };
            let (Msg::InternalResponse(r), _) = proto.decode(&answer, None).unwrap() else { panic!("not a response") };
            assert_eq!(r.code, 1);
            assert!(r.params.contains(&(1, Value::Int(1234))) && r.params.contains(&(2, Value::Int(99))));
        }
        let framed_init = [vec![0xFB, 0, 0, 0, 12, 0, 1], b"POST".to_vec()].concat();
        assert!(matches!(sort(framed_init, Proto::V18, 0), Message::ToServer(m) if m == b"POST"));
    }
}
