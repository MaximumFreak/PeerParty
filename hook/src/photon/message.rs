//! Message framing: `[0xF3][kind | 0x80 if encrypted][payload]`, the Init handshake
//! (classic 41-byte form and the HTTP-style form clients send when they carry a
//! token), and the per-protocol encoding of each kind.
//!
//! Port of the `Serialize`/`Deserialize` halves of Luxon's `ser_gp_binary_v16.cpp`,
//! `ser_gp_binary_v18.cpp` and `ser_interface.cpp` (BSD-3-Clause, see LICENSE-Luxon).
//! Two client behaviours Luxon doesn't cover are handled here: clients also accept
//! magic 0xFD, and the "AuthOnce" connection modes
//! open with `POST /?init&app=..&protocol=GpBinaryV18 HTTP/1.1` whose body is the
//! serialized token.

use super::crypto::Aes256;
use super::value::*;
use super::{gp16, gp18};

pub const MAGIC: u8 = 0xF3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Proto {
    V16,
    V18,
}

impl Proto {
    pub fn from_version(major: u8, minor: u8) -> Option<Proto> {
        match (major, minor) {
            (1, 6) => Some(Proto::V16),
            (1, 8) => Some(Proto::V18),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Proto::V16 => "GpBinaryV16",
            Proto::V18 => "GpBinaryV18",
        }
    }

    pub fn write_value(self, w: &mut Vec<u8>, v: &Value) -> R<()> {
        match self {
            Proto::V16 => gp16::write_value(w, v),
            Proto::V18 => gp18::write_value(w, v),
        }
    }

    pub fn read_value(self, r: &mut Rd) -> R<Value> {
        match self {
            Proto::V16 => gp16::read_value(r),
            Proto::V18 => gp18::read_value(r),
        }
    }

    fn write_params(self, w: &mut Vec<u8>, p: &Params) -> R<()> {
        match self {
            Proto::V16 => gp16::write_params(w, p),
            Proto::V18 => gp18::write_params(w, p),
        }
    }

    fn read_params(self, r: &mut Rd) -> R<Params> {
        match self {
            Proto::V16 => gp16::read_params(r),
            Proto::V18 => gp18::read_params(r),
        }
    }

    fn write_response(self, w: &mut Vec<u8>, resp: &Response) -> R<()> {
        match self {
            Proto::V16 => gp16::write_response_body(w, resp),
            Proto::V18 => gp18::write_response_body(w, resp),
        }
    }

    fn read_response(self, r: &mut Rd) -> R<Response> {
        match self {
            Proto::V16 => gp16::read_response_body(r),
            Proto::V18 => gp18::read_response_body(r),
        }
    }

    /// Encodes one message; `key` encrypts its payload (never Init, InitResponse or
    /// Disconnect, as in Luxon).
    pub fn encode(self, msg: &Msg, key: Option<&Aes256>) -> R<Vec<u8>> {
        let mut p = Vec::new();
        let k = match msg {
            Msg::Init(i) => {
                write_init(&mut p, i);
                kind::INIT
            }
            Msg::InitResponse => {
                if self == Proto::V18 {
                    p.push(0);
                }
                kind::INIT_RESPONSE
            }
            Msg::Request(q) => {
                p.push(q.code);
                self.write_params(&mut p, &q.params)?;
                kind::OPERATION
            }
            Msg::Response(r) => {
                self.write_response(&mut p, r)?;
                kind::OPERATION_RESPONSE
            }
            Msg::Event(e) => {
                p.push(e.code);
                self.write_params(&mut p, &e.params)?;
                kind::EVENT
            }
            Msg::InternalRequest(q) => {
                p.push(q.code);
                self.write_params(&mut p, &q.params)?;
                kind::INTERNAL_REQUEST
            }
            Msg::InternalResponse(r) => {
                self.write_response(&mut p, r)?;
                kind::INTERNAL_RESPONSE
            }
            Msg::Disconnect(d) => {
                match self {
                    Proto::V16 => gp16::write_disconnect_body(&mut p, d)?,
                    Proto::V18 => gp18::write_disconnect_body(&mut p, d)?,
                }
                kind::DISCONNECT
            }
            Msg::Other(k, bytes) => {
                p.extend_from_slice(bytes);
                *k & 0x7F
            }
        };
        let encrypt = key.filter(|_| !matches!(k, kind::INIT | kind::INIT_RESPONSE | kind::DISCONNECT));
        let mut out = vec![MAGIC, k | if encrypt.is_some() { 0x80 } else { 0 }];
        match encrypt {
            Some(aes) => out.extend_from_slice(&aes.encrypt_cbc(&p)),
            None => out.extend_from_slice(&p),
        }
        Ok(out)
    }

    /// Decodes one message. Returns the message and whether it arrived encrypted.
    pub fn decode(self, data: &[u8], key: Option<&Aes256>) -> R<(Msg, bool)> {
        if data.len() < 2 {
            return Err("message too short");
        }
        if data[0] != MAGIC && data[0] != 0xFD {
            return Err("bad message magic");
        }
        let encrypted = data[1] & 0x80 != 0;
        let k = data[1] & 0x7F;
        let plain;
        let body: &[u8] = if encrypted && k != kind::INIT && k != kind::INIT_RESPONSE {
            let aes = key.ok_or("encrypted message before the key exchange")?;
            plain = aes.decrypt_cbc(&data[2..]).ok_or("message didn't decrypt")?;
            &plain
        } else if encrypted {
            return Err("init marked encrypted");
        } else {
            &data[2..]
        };
        let mut r = Rd::new(body);
        let msg = match k {
            kind::INIT => return Ok((Msg::Init(parse_init(body)?), false)),
            kind::INIT_RESPONSE => Msg::InitResponse,
            kind::OPERATION => {
                let code = r.u8()?;
                Msg::Request(Request { code, params: self.read_params(&mut r)? })
            }
            kind::OPERATION_RESPONSE => Msg::Response(self.read_response(&mut r)?),
            kind::EVENT => {
                let code = r.u8()?;
                Msg::Event(EventMsg { code, params: self.read_params(&mut r)? })
            }
            kind::INTERNAL_REQUEST => {
                let code = r.u8()?;
                Msg::InternalRequest(Request { code, params: self.read_params(&mut r)? })
            }
            kind::INTERNAL_RESPONSE => Msg::InternalResponse(self.read_response(&mut r)?),
            kind::DISCONNECT => Msg::Disconnect(match self {
                Proto::V16 => gp16::read_disconnect_body(&mut r)?,
                Proto::V18 => gp18::read_disconnect_body(&mut r)?,
            }),
            other => return Ok((Msg::Other(other, body.to_vec()), encrypted)),
        };
        if k != kind::INIT_RESPONSE && r.left() != 0 {
            return Err("trailing bytes after message");
        }
        Ok((msg, encrypted))
    }
}

fn write_init(p: &mut Vec<u8>, i: &Init) {
    p.push(i.proto_major);
    p.push(i.proto_minor);
    p.push(i.sdk << 1);
    p.push((if i.ipv6 { 0x80 } else { 0 }) | ((i.version[0] & 7) << 4) | (i.version[1] & 0x0F));
    p.push(i.version[2]);
    p.push(i.version[3]);
    p.push(0);
    let mut app = [0u8; 32];
    let n = i.app_id.len().min(32);
    app[..n].copy_from_slice(&i.app_id.as_bytes()[..n]);
    p.extend_from_slice(&app);
}

/// The classic Init payload: exactly 39 bytes after the two header bytes.
fn parse_init(p: &[u8]) -> R<Init> {
    if p.len() != 39 {
        return Err("init payload must be 39 bytes");
    }
    let end = p[7..].iter().position(|b| *b == 0).unwrap_or(32);
    Ok(Init {
        proto_major: p[0],
        proto_minor: p[1],
        sdk: p[2] >> 1,
        version: [(p[3] >> 4) & 7, p[3] & 0x0F, p[4], p[5]],
        ipv6: p[3] & 0x80 != 0,
        app_id: String::from_utf8_lossy(&p[7..7 + end]).into_owned(),
    })
}

/// The Init a client opens a connection with, in either form.
#[derive(Clone, Debug, PartialEq)]
pub struct Hello {
    pub init: Init,
    /// The token an AuthOnce client sends in its HTTP-style init.
    pub token: Option<String>,
}

/// Recognises the first message on a connection.
pub fn parse_hello(data: &[u8]) -> R<Hello> {
    if data.starts_with(b"POST ") {
        return parse_http_init(data);
    }
    if data.len() < 2 || (data[0] != MAGIC && data[0] != 0xFD) || data[1] != kind::INIT {
        return Err("first message isn't an Init");
    }
    Ok(Hello { init: parse_init(&data[2..])?, token: None })
}

/// `POST /?init&app=<id>&clientversion=<a.b.c.d>&protocol=GpBinaryV18&sid=..[&IPv6]
/// HTTP/1.1` + headers + a body holding the token as a serialized string.
fn parse_http_init(data: &[u8]) -> R<Hello> {
    let split = data.windows(4).position(|w| w == b"\r\n\r\n").ok_or("HTTP init without a body")?;
    let head = std::str::from_utf8(&data[..split]).map_err(|_| "HTTP init head isn't text")?;
    let body = &data[split + 4..];
    let line = head.lines().next().unwrap_or("");
    let target = line.split(' ').nth(1).ok_or("HTTP init without a target")?;
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut init = Init { proto_major: 1, proto_minor: 8, ..Init::default() };
    let mut proto = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "app" => init.app_id = percent_decode(v),
            "clientversion" => {
                for (i, part) in v.split('.').take(4).enumerate() {
                    init.version[i] = part.parse().unwrap_or(0);
                }
            }
            "protocol" => {
                proto = match v {
                    "GpBinaryV16" => Some(Proto::V16),
                    "GpBinaryV18" => Some(Proto::V18),
                    _ => return Err("HTTP init with an unknown protocol"),
                }
            }
            "IPv6" => init.ipv6 = true,
            _ => {}
        }
    }
    let proto = proto.unwrap_or(Proto::V18);
    (init.proto_major, init.proto_minor) = match proto {
        Proto::V16 => (1, 6),
        Proto::V18 => (1, 8),
    };
    let token = if body.is_empty() {
        None
    } else {
        match proto.read_value(&mut Rd::new(body))? {
            Value::Str(s) => Some(s),
            Value::Null => None,
            _ => return Err("HTTP init body isn't a token string"),
        }
    };
    Ok(Hello { init, token })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Bytes, not a slice of the text: two bytes after a '%' can end inside a
        // character, and slicing there panics.
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
