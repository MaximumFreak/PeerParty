//! Wire (wss://wire.unity3d.com/v2/ws): the channel Unity's Lobby SDK subscribes to for
//! lobby events. Unity's Multiplayer Sessions always subscribe (their LobbyHandler is
//! built with polling off), and a failed subscription fails the session, so this is
//! needed, not optional.
//!
//! Wire is Centrifuge's JSON client protocol (github.com/centrifugal/protocol, MIT)
//! over a WebSocket (RFC 6455), as the SDK's Unity.Services.Wire.Internal speaks it:
//! commands `{"id":n,"connect":{token,subs}}`, `subscribe {channel,token}`,
//! `unsubscribe`, `ping`, several per frame separated by newlines; replies carry the
//! command's id; pushes have none: `{"push":{"channel":c,"pub":{"data":{"payload":
//! "<LobbyPatches JSON>","version":"1"},"offset":n}}}` for an event, `{"push":{"channel":
//! c,"unsubscribe":{...}}}` for a kick. The connect reply says `ping: 0`, so the client
//! expects no server pings. Channels are the `uri` Lobby's tokens call hands out
//! (lobby.rs `channel`). The socket rides the same HTTPS server as the REST calls
//! (playfab/http.rs hands an Upgrade request here).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use crate::json::Json;
use crate::log;
use crate::playfab::http::Request;
use crate::playfab::tls::TlsStream;

enum Out {
    Text(String),
}

/// Who listens on each channel: (connection id, its outbox).
static SUBS: LazyLock<Mutex<HashMap<String, Vec<(u64, Sender<Out>)>>>> = LazyLock::new(Mutex::default);
/// Each channel's publication offset.
static OFFSETS: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(Mutex::default);
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// Sends a lobby event (a LobbyPatches JSON text) to the channel's subscribers.
pub fn publish(channel: &str, payload: &str) {
    let offset = {
        let mut o = OFFSETS.lock().unwrap();
        let n = o.entry(channel.to_string()).or_insert(0);
        *n += 1;
        *n
    };
    let msg = Json::obj(vec![(
        "push",
        Json::obj(vec![
            ("channel", Json::str(channel)),
            (
                "pub",
                Json::obj(vec![
                    ("data", Json::obj(vec![("payload", Json::str(payload)), ("version", Json::str("1"))])),
                    ("offset", Json::Num(offset as f64)),
                ]),
            ),
        ]),
    )])
    .to_string();
    let subs = SUBS.lock().unwrap();
    let n = subs.get(channel).map(|v| v.len()).unwrap_or(0);
    for (_, tx) in subs.get(channel).into_iter().flatten() {
        let _ = tx.send(Out::Text(msg.clone()));
    }
    crate::log::throttled(&format!("ugs wire {channel}"), 5, |c| {
        log!("ugs wire: lobby event on {channel} to {n} subscriber(s) n={c}")
    });
}

/// Tells the channel's subscribers they were removed (the SDK's KickedFromLobby).
pub fn kick(channel: &str) {
    let msg = Json::obj(vec![(
        "push",
        Json::obj(vec![
            ("channel", Json::str(channel)),
            ("unsubscribe", Json::obj(vec![("code", Json::Num(2000.0)), ("reason", Json::str("removed from the lobby"))])),
        ]),
    )])
    .to_string();
    if let Some(subs) = SUBS.lock().unwrap().remove(channel) {
        for (_, tx) in subs {
            let _ = tx.send(Out::Text(msg.clone()));
        }
    }
}

fn subscribe(channel: &str, conn: u64, tx: &Sender<Out>) {
    let mut s = SUBS.lock().unwrap();
    let list = s.entry(channel.to_string()).or_default();
    if !list.iter().any(|(c, _)| *c == conn) {
        list.push((conn, tx.clone()));
    }
    log!("ugs wire: subscribed to {channel}");
}

fn unsubscribe(channel: Option<&str>, conn: u64) {
    let mut s = SUBS.lock().unwrap();
    for (ch, list) in s.iter_mut() {
        if channel.is_none_or(|c| c == ch) {
            list.retain(|(c, _)| *c != conn);
        }
    }
    s.retain(|_, l| !l.is_empty());
}

fn offset_of(channel: &str) -> u64 {
    OFFSETS.lock().unwrap().get(channel).copied().unwrap_or(0)
}

// ---- SHA-1 (FIPS 180-4), for the WebSocket handshake only ----

pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

/// RFC 6455 section 4.2.2: the server's Sec-WebSocket-Accept for the client's key.
pub fn accept_key(key: &str) -> String {
    super::base64(&sha1(format!("{}258EAFA5-E914-47DA-95CA-C5AB0DC85B11", key.trim()).as_bytes()))
}

// ---- frames (RFC 6455 section 5) ----

/// One complete frame at the start of `buf`: (fin, opcode, unmasked payload, bytes used).
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
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    Some((fin, opcode, payload, end))
}

/// A server frame (unmasked), FIN set.
pub fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x80 | opcode];
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

// ---- Centrifuge ----

fn uuid() -> String {
    let h = peerparty_core::PeerId::random().hex();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn subscribe_result(channel: &str) -> Json {
    Json::obj(vec![
        ("expires", Json::Bool(false)),
        ("ttl", Json::Num(0.0)),
        ("recoverable", Json::Bool(false)),
        ("epoch", Json::str("peerparty")),
        ("recovered", Json::Bool(false)),
        ("offset", Json::Num(offset_of(channel) as f64)),
        ("positioned", Json::Bool(false)),
    ])
}

/// Answers one frame's commands; returns the replies to send. Subscriptions are
/// registered with `conn` / `tx`.
fn commands(text: &str, conn: u64, tx: &Sender<Out>) -> Vec<String> {
    let mut replies = Vec::new();
    for line in text.split('\n').map(str::trim).filter(|l| !l.is_empty()) {
        let Some(cmd) = Json::parse(line) else {
            log!("ugs wire: unreadable command {line}");
            continue;
        };
        let id = cmd.get("id").as_f64().unwrap_or(0.0);
        if id == 0.0 {
            continue; // `{}`: the client's pong.
        }
        let id = Json::Num(id);
        let reply = if !cmd.get("connect").is_null() {
            let mut subs = Vec::new();
            if let Json::Obj(requested) = cmd.get("connect").get("subs") {
                for (ch, _) in requested {
                    subscribe(ch, conn, tx);
                    subs.push((ch.clone(), subscribe_result(ch)));
                }
            }
            Json::obj(vec![
                ("id", id),
                (
                    "connect",
                    Json::obj(vec![
                        ("client", Json::str(uuid())),
                        ("version", Json::str("peerparty")),
                        ("expires", Json::Bool(false)),
                        ("ttl", Json::Num(0.0)),
                        ("ping", Json::Num(0.0)),
                        ("pong", Json::Bool(false)),
                        ("subs", Json::Obj(subs)),
                    ]),
                ),
            ])
        } else if !cmd.get("subscribe").is_null() {
            let ch = cmd.get("subscribe").get("channel").as_str().unwrap_or_default().to_string();
            subscribe(&ch, conn, tx);
            Json::obj(vec![("id", id), ("subscribe", subscribe_result(&ch))])
        } else if !cmd.get("unsubscribe").is_null() {
            let ch = cmd.get("unsubscribe").get("channel").as_str().unwrap_or_default().to_string();
            unsubscribe(Some(&ch), conn);
            Json::obj(vec![("id", id), ("unsubscribe", Json::obj(vec![]))])
        } else if !cmd.get("ping").is_null() {
            Json::obj(vec![("id", id), ("ping", Json::obj(vec![]))])
        } else {
            Json::obj(vec![("id", id), ("error", Json::obj(vec![("code", Json::Num(102.0)), ("message", Json::str("unknown command"))]))])
        };
        replies.push(reply.to_string());
    }
    replies
}

/// Runs one WebSocket on an upgraded HTTPS connection until it closes.
pub fn serve(mut tls: TlsStream, req: &Request, mut pending: Vec<u8>) {
    let Some(key) = req.header("Sec-WebSocket-Key").map(str::to_string) else {
        let _ = tls.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(&key)
    );
    if tls.write_all(head.as_bytes()).is_err() {
        return;
    }
    let conn = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    log!("ugs wire: socket {conn} open");
    let (tx, rx): (Sender<Out>, Receiver<Out>) = mpsc::channel();
    let _ = tls.set_read_timeout(Some(Duration::from_millis(50)));
    let mut message: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16384];
    let result: io::Result<()> = (|| loop {
        loop {
            match rx.try_recv() {
                Ok(Out::Text(t)) => tls.write_all(&frame(1, t.as_bytes()))?,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        while let Some((fin, opcode, payload, used)) = parse_frame(&pending) {
            pending.drain(..used);
            match opcode {
                0 | 1 | 2 => {
                    message.extend_from_slice(&payload);
                    if fin {
                        let text = String::from_utf8_lossy(&message).to_string();
                        message.clear();
                        for r in commands(&text, conn, &tx) {
                            tls.write_all(&frame(1, r.as_bytes()))?;
                        }
                    }
                }
                8 => {
                    let _ = tls.write_all(&frame(8, &payload[..payload.len().min(2)]));
                    return Ok(());
                }
                9 => tls.write_all(&frame(10, &payload))?,
                _ => {}
            }
        }
        match tls.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e),
        }
    })();
    unsubscribe(None, conn);
    match result {
        Ok(()) => log!("ugs wire: socket {conn} closed"),
        Err(e) => log!("ugs wire: socket {conn} ended: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn sha1_matches_fips_180_vectors() {
        assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn accept_key_matches_rfc_6455() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn frames_round_trip_with_masks_and_long_lengths() {
        // RFC 6455 section 5.7: a masked "Hello".
        let masked = [0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58];
        let (fin, op, p, used) = parse_frame(&masked).unwrap();
        assert!(fin && op == 1 && p == b"Hello" && used == masked.len());
        assert!(parse_frame(&masked[..6]).is_none(), "incomplete frames wait");
        for n in [0usize, 125, 126, 65535, 70000] {
            let data = vec![7u8; n];
            let f = frame(2, &data);
            let (_, op, p, used) = parse_frame(&f).unwrap();
            assert_eq!((op, p.len(), used), (2, n, f.len()));
        }
    }

    #[test]
    fn centrifuge_commands_get_replies_and_subscriptions() {
        let (tx, rx) = mpsc::channel();
        let batch = concat!(
            r#"{"id":1,"connect":{"token":"t","subs":{"lobby:pre":{"channel":"lobby:pre"}}},"subscribe":null,"unsubscribe":null,"ping":null}"#,
            "\n",
            r#"{"id":2,"connect":null,"subscribe":{"channel":"lobby:L","token":"peerparty","recover":false,"offset":0,"epoch":null},"unsubscribe":null,"ping":null}"#,
            "\n{}\n",
            r#"{"id":3,"ping":{}}"#
        );
        let replies: Vec<Json> = commands(batch, 77, &tx).iter().map(|r| Json::parse(r).unwrap()).collect();
        assert_eq!(replies.len(), 3);
        assert_eq!(replies[0].get("id").as_f64(), Some(1.0));
        assert_eq!(replies[0].get("connect").get("ping").as_f64(), Some(0.0));
        assert!(!replies[0].get("connect").get("subs").get("lobby:pre").is_null());
        assert_eq!(replies[1].get("subscribe").get("epoch").as_str(), Some("peerparty"));
        assert!(!replies[2].get("ping").is_null());
        publish("lobby:L", r#"{"version":2,"patches":[]}"#);
        let Out::Text(push) = rx.try_recv().unwrap();
        let push = Json::parse(&push).unwrap();
        assert!(push.get("id").is_null(), "pushes carry no id");
        let p = push.get("push");
        assert_eq!(p.get("channel").as_str(), Some("lobby:L"));
        assert_eq!(p.get("pub").get("data").get("payload").as_str(), Some(r#"{"version":2,"patches":[]}"#));
        kick("lobby:L");
        let Out::Text(k) = rx.try_recv().unwrap();
        assert!(!Json::parse(&k).unwrap().get("push").get("unsubscribe").is_null());
        unsubscribe(None, 77);
        publish("lobby:pre", "{}");
        assert!(rx.try_recv().is_err());
    }
}
