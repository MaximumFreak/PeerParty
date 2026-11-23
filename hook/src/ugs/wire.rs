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
//! (https/server.rs hands an Upgrade request here).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use crate::util::json::Json;
use crate::log;
use crate::https::server::Request;
use crate::https::tls::TlsStream;
use crate::util::ws::{OP_BINARY, OP_CLOSE, OP_PING, OP_PONG, OP_TEXT, accept_key, frame, parse_frame};

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
        let mut o = crate::sys::lock(&OFFSETS);
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
    let subs = crate::sys::lock(&SUBS);
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
    if let Some(subs) = crate::sys::lock(&SUBS).remove(channel) {
        for (_, tx) in subs {
            let _ = tx.send(Out::Text(msg.clone()));
        }
    }
}

fn subscribe(channel: &str, conn: u64, tx: &Sender<Out>) {
    let mut s = crate::sys::lock(&SUBS);
    let list = s.entry(channel.to_string()).or_default();
    if !list.iter().any(|(c, _)| *c == conn) {
        list.push((conn, tx.clone()));
    }
    log!("ugs wire: subscribed to {channel}");
}

fn unsubscribe(channel: Option<&str>, conn: u64) {
    let mut s = crate::sys::lock(&SUBS);
    for (ch, list) in s.iter_mut() {
        if channel.is_none_or(|c| c == ch) {
            list.retain(|(c, _)| *c != conn);
        }
    }
    s.retain(|_, l| !l.is_empty());
}

fn offset_of(channel: &str) -> u64 {
    crate::sys::lock(&OFFSETS).get(channel).copied().unwrap_or(0)
}


// ---- Centrifuge ----

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
        if cmd.get("id").as_f64().unwrap_or(0.0) == 0.0 {
            continue; // `{}`: the client's pong.
        }
        // Echoed as it came (an integer stays exact).
        let id = cmd.get("id").clone();
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
                        ("client", Json::str(crate::util::encoding::uuid())),
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

/// The longest message (all its frames together) a socket takes. Wire's commands are a
/// few hundred bytes; anything near this is not a game talking to Wire, and the socket
/// is closed (1009, message too big) rather than buffered without end.
const MESSAGE_MAX: usize = 1 << 20;

/// Whether what a socket holds has outgrown any message it may send: the frames of the
/// message so far plus what is waiting to be read as a frame (a header is at most 14).
fn too_big(message: usize, pending: usize) -> bool {
    message > MESSAGE_MAX || pending > MESSAGE_MAX + 14
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
                Ok(Out::Text(t)) => tls.write_all(&frame(OP_TEXT, t.as_bytes()))?,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        while let Some((fin, opcode, payload, used)) = parse_frame(&pending) {
            pending.drain(..used);
            match opcode {
                // A continuation (0), text or binary frame.
                0 | OP_TEXT | OP_BINARY => {
                    message.extend_from_slice(&payload);
                    if fin {
                        let text = String::from_utf8_lossy(&message).to_string();
                        message.clear();
                        for r in commands(&text, conn, &tx) {
                            tls.write_all(&frame(OP_TEXT, r.as_bytes()))?;
                        }
                    }
                }
                OP_CLOSE => {
                    let _ = tls.write_all(&frame(OP_CLOSE, &payload[..payload.len().min(2)]));
                    return Ok(());
                }
                OP_PING => tls.write_all(&frame(OP_PONG, &payload))?,
                _ => {}
            }
        }
        if too_big(message.len(), pending.len()) {
            let _ = tls.write_all(&frame(OP_CLOSE, &1009u16.to_be_bytes()));
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("a message over {MESSAGE_MAX} bytes")));
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

    #[test]
    fn a_socket_takes_no_message_past_the_limit() {
        assert!(!too_big(MESSAGE_MAX, MESSAGE_MAX + 14));
        assert!(too_big(MESSAGE_MAX + 1, 0), "frames of one message together");
        assert!(too_big(0, MESSAGE_MAX + 15), "one frame still arriving");
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
