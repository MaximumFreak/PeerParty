//! Demonware's lobby service: one TCP connection per signed-in game (port 3074).
//!
//! Messages are a little-endian i32 length, then that many bytes. The handshake:
//!
//! 1. The client's header: its first u32 is 0xC8, a second u32, then its data (no
//!    length of its own; it's all the client sends before waiting).
//! 2. The server's header: `16 00 00 00 AB 81`, u32 0xD2, then 16 random bytes.
//! 3. The client's auth (`AB 82`), which carries the server ticket from the sign-in
//!    (auth.rs) and ends in an 8-byte proof.
//! 4. The server's answer (`AB 83`) with the 8-byte response from the derived keys.
//!
//! The keys (crypto.rs `derive`) come from the session key and the transcript: the
//! client header after its first 8 bytes, the server header whole, and the client auth
//! whole but for its last 8 bytes. After that each message is `AB 85`, a u32 count, a
//! 16-byte IV, the AES-128-CBC body and 8 bytes of HMAC-SHA1 over all before them. A
//! call's body is a u32 length, 0x86, the service id and the task's arguments in
//! Demonware's typed encoding (each value after a type byte), the task id first. The
//! reply's body is a u32 length, 1 (a task reply), then a typed transaction id, error
//! code, task id, result count and the results.
//!
//! Tasks with no answer of their own here get success with no results, and are logged
//! once each with their arguments, so a game's log says what it asked for.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::log;

use super::auth::SERVER_TICKET_MARK;
use super::crypto::{self, Keys};
use super::hex;

/// The session key of the latest sign-in, for a client auth whose server ticket isn't
/// PeerParty's (it should always be).
static LAST_KEY: Mutex<Option<[u8; 24]>> = Mutex::new(None);

pub fn remember_session_key(key: [u8; 24]) {
    *crate::sys::lock(&LAST_KEY) = Some(key);
}

/// The session key in a client auth message: the 24 bytes before PeerParty's mark in
/// the server ticket it carries, else the latest sign-in's.
pub(crate) fn session_key_in(auth: &[u8]) -> Option<[u8; 24]> {
    let at = auth.windows(4).position(|w| w == SERVER_TICKET_MARK);
    if let Some(i) = at.filter(|i| *i >= 24) {
        return auth[i - 24..i].try_into().ok();
    }
    *crate::sys::lock(&LAST_KEY)
}

const CLIENT_HEADER: u32 = 0xC8;
const SERVER_CHALLENGE: u32 = 0xD2;
const MARK: u8 = 0xAB;
const CLIENT_AUTH: u8 = 0x82;
const SERVER_HEADER: u8 = 0x81;
const SERVER_AUTH_DONE: u8 = 0x83;
const ENCRYPTED: u8 = 0x85;
const CALL: u8 = 0x86;
const TASK_REPLY: u8 = 1;

/// The largest message taken: publisher and user files can reach several megabytes, and
/// only this PC reaches here.
const MAX_MESSAGE: usize = 16 << 20;

/// Demonware's names for the service ids games use, for the log.
fn service_name(id: u8) -> &'static str {
    match id {
        3 => "bdTeams",
        4 => "bdStats",
        6 => "bdMessaging",
        8 => "bdProfiles",
        10 => "bdStorage",
        12 => "bdTitleUtilities",
        15 => "bdObjectStore",
        18 => "bdBandwidthTest",
        19 => "bdStats2",
        21 => "bdMatchMaking",
        23 => "bdCounter",
        27 => "bdDML",
        28 => "bdGroup",
        36 => "bdFacebook",
        38 => "bdAnticheat",
        50 => "bdContentStreaming",
        65 => "bdUserGroups",
        67 => "bdEventLog",
        68 => "bdRichPresence",
        80 => "bdMarketplace",
        81 | 82 => "bdLeague",
        91 => "bdStats3",
        95 => "bdPublisherVariables",
        96 => "bdDDL",
        103 => "bdPresence",
        104 => "bdMarketingComms",
        139 => "bdReward",
        _ => "an unknown service",
    }
}

/// Demonware's typed encoding (byte_buffer with data types): a type byte, then the value.
pub mod typed {
    pub const BOOL: u8 = 1;
    pub const UBYTE: u8 = 3;
    pub const UINT16: u8 = 6;
    pub const UINT32: u8 = 8;
    pub const UINT64: u8 = 10;
    pub const FLOAT32: u8 = 13;

    pub fn u8(out: &mut Vec<u8>, v: u8) {
        out.extend_from_slice(&[UBYTE, v]);
    }
    pub fn u16(out: &mut Vec<u8>, v: u16) {
        out.push(UINT16);
        out.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(out: &mut Vec<u8>, v: u32) {
        out.push(UINT32);
        out.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(out: &mut Vec<u8>, v: u64) {
        out.push(UINT64);
        out.extend_from_slice(&v.to_le_bytes());
    }
    pub fn f32(out: &mut Vec<u8>, v: f32) {
        out.push(FLOAT32);
        out.extend_from_slice(&v.to_le_bytes());
    }
    pub fn bool(out: &mut Vec<u8>, v: bool) {
        out.extend_from_slice(&[BOOL, v as u8]);
    }
    /// A string: its bytes and a NUL, no length.
    pub fn string(out: &mut Vec<u8>, v: &[u8]) {
        out.push(super::super::mm::STRING);
        out.extend_from_slice(v);
        out.push(0);
    }
}

/// The most DDL checksums one verify call is answered for.
const MAX_DDL_FILES: u32 = 256;

/// One connection's state.
struct Conn {
    transcript: Vec<u8>,
    keys: Option<Keys>,
    sent: u32,
}

static TRANSACTION: AtomicU64 = AtomicU64::new(0);

/// A task reply's body (before encryption): success, with `results` (each already in
/// the typed encoding) or none.
pub fn task_reply(task: u8, results: &[Vec<u8>]) -> Vec<u8> {
    let mut b = Vec::new();
    typed::u64(&mut b, TRANSACTION.fetch_add(1, Ordering::Relaxed) + 1);
    typed::u32(&mut b, 0);
    typed::u8(&mut b, task);
    typed::u32(&mut b, results.len() as u32);
    if !results.is_empty() {
        typed::u32(&mut b, results.len() as u32);
        for r in results {
            b.extend_from_slice(r);
        }
    }
    b
}

/// A task reply's body for an error: the error code, then the transaction id again in
/// place of the results.
pub fn task_error(task: u8, error: u32) -> Vec<u8> {
    let id = TRANSACTION.fetch_add(1, Ordering::Relaxed) + 1;
    let mut b = Vec::new();
    typed::u64(&mut b, id);
    typed::u32(&mut b, error);
    typed::u8(&mut b, task);
    typed::u64(&mut b, id);
    b
}

/// How one lobby call was answered, with an optional throttle suffix (`detail`, such as a
/// file name) so distinct files under one task each appear in the log.
enum Dispatch {
    Results { results: Vec<Vec<u8>>, detail: String, note: String },
    Error { code: u32, detail: String, why: String },
    Empty,
}

/// Dispatches service `service`'s task `task` (`args` after the task id).
fn dispatch(service: u8, task: u8, args: &[u8]) -> Dispatch {
    if service == 10 {
        return match super::storage::handle(task, args) {
            super::storage::StorageReply::Results { results, detail, note } => Dispatch::Results { results, detail, note },
            super::storage::StorageReply::Error { code, detail, why } => Dispatch::Error { code, detail, why },
            super::storage::StorageReply::Unhandled => Dispatch::Empty,
        };
    }
    match (service, task) {
        // bdDML getUserData (2): geolocation record (bdDMLRawData).
        (27, 2) => {
            let mut r = Vec::new();
            typed::string(&mut r, b"US");
            typed::string(&mut r, b"United States");
            typed::string(&mut r, b"New York");
            typed::string(&mut r, b"New York");
            typed::f32(&mut r, 0.0);
            typed::f32(&mut r, 0.0);
            typed::u32(&mut r, 0);
            typed::string(&mut r, b"+00:00");
            Dispatch::Results { results: vec![r], detail: String::new(), note: "1 result(s)".into() }
        }
        // bdDML getUserHierarchicalData (3): empty success.
        (27, 3) => Dispatch::Results { results: Vec::new(), detail: String::new(), note: "empty hierarchical data".into() },
        // bdPublisherVariables retrievePublisherVariables (1): empty variable set.
        (95, 1) => {
            let mut r = super::mm::Reader::new(args);
            let group = r.string().map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
            let ns = r.string().map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
            let note = format!("group {group:?}, namespace {ns:?}: empty variable set");
            Dispatch::Results { results: Vec::new(), detail: format!("{group}:{ns}"), note }
        }
        // bdDDL verifyDDLFiles: a count, then each DDL file's checksum. Every one matches
        // (a bdDDLChecksumResult: the checksum, then true): the server holds no other
        // copy of the game's data layouts to compare with.
        (96, 1) => {
            let mut r = super::mm::Reader::new(args);
            let Some(n) = r.u32().map(|n| n.min(MAX_DDL_FILES)) else { return Dispatch::Empty };
            let mut out = Vec::new();
            for _ in 0..n {
                let Some(sum) = r.string() else { break };
                let mut v = Vec::new();
                typed::string(&mut v, sum);
                typed::bool(&mut v, true);
                out.push(v);
            }
            let note = format!("{} result(s)", out.len());
            Dispatch::Results { results: out, detail: String::new(), note }
        }
        // bdTitleUtilities getServerTime: a bdTimeStamp.
        (12, 6) => {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0);
            let mut r = Vec::new();
            typed::u32(&mut r, now);
            Dispatch::Results { results: vec![r], detail: String::new(), note: "1 result(s)".into() }
        }
        (21, _) => match super::mm::answer(task, args) {
            Some(results) => {
                let note = format!("{} result(s)", results.len());
                Dispatch::Results { results, detail: String::new(), note }
            }
            None => Dispatch::Empty,
        },
        _ => Dispatch::Empty,
    }
}

/// An encrypted message with `body` (a type byte and data are already in it).
pub fn seal(keys: &Keys, count: u32, body: &[u8]) -> Vec<u8> {
    let mut plain = (body.len() as u32 - 1).to_le_bytes().to_vec();
    plain.extend_from_slice(body);
    plain.resize(plain.len().div_ceil(16) * 16, 0);
    let iv: [u8; 16] = crate::util::crypto::random_bytes(16).try_into().unwrap();
    let enc = crypto::cbc_encrypt(&keys.server_to_client, &iv, &plain);
    let mut msg = ((30 + enc.len()) as i32).to_le_bytes().to_vec();
    msg.extend_from_slice(&[MARK, ENCRYPTED]);
    msg.extend_from_slice(&count.to_le_bytes());
    msg.extend_from_slice(&iv);
    msg.extend_from_slice(&enc);
    let mac = crypto::hmac_sha1(&keys.hmac, &msg);
    msg.extend_from_slice(&mac[..8]);
    msg
}

/// Opens an encrypted message from the client (`msg` from its `AB 85` on, without the
/// length): the call's body (u32 length, then the rest), or None.
pub fn open(keys: &Keys, msg: &[u8]) -> Option<Vec<u8>> {
    // AB 85, count (4), IV (16), body, MAC (8).
    if msg.len() < 2 + 4 + 16 + 16 + 8 {
        return None;
    }
    let iv: [u8; 16] = msg[6..22].try_into().ok()?;
    let body = &msg[22..msg.len() - 8];
    crypto::cbc_decrypt(&keys.client_to_server, &iv, body)
}

pub fn serve(mut conn: TcpStream) {
    let peer = conn.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    log!("demonware lobby: a game connected from {peer}");
    let _ = conn.set_nodelay(true);
    let mut c = Conn { transcript: Vec::new(), keys: None, sent: 0 };
    match run(&mut conn, &mut c) {
        Ok(()) => log!("demonware lobby: the game closed its connection"),
        Err(e) => log!("demonware lobby: connection ended: {e}"),
    }
}

/// Reads what the client sends before it stops to wait (its header has no length).
fn read_burst(conn: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut buf = [0u8; 4096];
    conn.set_read_timeout(None)?;
    let n = conn.read(&mut buf)?;
    if n == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let mut out = buf[..n].to_vec();
    conn.set_read_timeout(Some(Duration::from_millis(150)))?;
    loop {
        match conn.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => break,
            Err(e) => return Err(e),
        }
        if out.len() > MAX_MESSAGE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "client header too big"));
        }
    }
    conn.set_read_timeout(None)?;
    Ok(out)
}

/// Reads one length-prefixed message: (its length bytes and all, its body).
fn read_message(conn: &mut TcpStream) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut len = [0u8; 4];
    conn.read_exact(&mut len)?;
    let n = i32::from_le_bytes(len);
    if n <= 0 {
        return Ok((len.to_vec(), Vec::new()));
    }
    if n as usize > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("message of {n} bytes")));
    }
    let mut body = vec![0u8; n as usize];
    conn.read_exact(&mut body)?;
    let mut whole = len.to_vec();
    whole.extend_from_slice(&body);
    Ok((whole, body))
}

fn run(conn: &mut TcpStream, c: &mut Conn) -> io::Result<()> {
    // 1. The client's header.
    let header = read_burst(conn)?;
    let first = header.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
    log!(
        "demonware lobby: client header ({} bytes, starts {first:#x}): {}",
        header.len(),
        hex(&header[..header.len().min(64)])
    );
    if first != CLIENT_HEADER || header.len() < 8 {
        log!("demonware lobby: not the client header this service knows; closing");
        return Ok(());
    }
    c.transcript.extend_from_slice(&header[8..]);

    // 2. The server's header.
    let mut challenge = vec![0x16, 0, 0, 0, MARK, SERVER_HEADER];
    challenge.extend_from_slice(&SERVER_CHALLENGE.to_le_bytes());
    challenge.extend_from_slice(&crate::util::crypto::random_bytes(16));
    c.transcript.extend_from_slice(&challenge);
    conn.write_all(&challenge)?;

    loop {
        let (whole, body) = read_message(conn)?;
        if body.is_empty() {
            // A keep-alive: answered with one.
            conn.write_all(&[0, 0, 0, 0])?;
            continue;
        }
        if body.len() < 2 || body[0] != MARK {
            log!("demonware lobby: unknown message ({} bytes): {}", body.len(), hex(&body[..body.len().min(64)]));
            continue;
        }
        match body[1] {
            CLIENT_AUTH => {
                if whole.len() < 8 {
                    continue;
                }
                c.transcript.extend_from_slice(&whole[..whole.len() - 8]);
                let Some(key) = session_key_in(&body) else {
                    log!("demonware lobby: client auth without a sign-in first; closing");
                    return Ok(());
                };
                let keys = crypto::derive(&key, &c.transcript);
                let mut done = vec![0x0A, 0, 0, 0, MARK, SERVER_AUTH_DONE];
                done.extend_from_slice(&keys.response);
                conn.write_all(&done)?;
                log!("demonware lobby: client auth ({} bytes): signed in, keys derived", body.len());
                c.keys = Some(keys);
            }
            ENCRYPTED => {
                let Some(keys) = c.keys.clone() else {
                    log!("demonware lobby: an encrypted message before the client auth");
                    continue;
                };
                let Some(plain) = open(&keys, &body) else {
                    log!("demonware lobby: an encrypted message that doesn't decrypt ({} bytes)", body.len());
                    continue;
                };
                call(conn, c, &keys, &plain)?;
            }
            other => log!(
                "demonware lobby: unknown message type {other:#x} ({} bytes): {}",
                body.len(),
                hex(&body[..body.len().min(64)])
            ),
        }
    }
}

/// One decrypted call: u32 length, 0x86, service id, then the typed task id and args.
fn call(conn: &mut TcpStream, c: &mut Conn, keys: &Keys, plain: &[u8]) -> io::Result<()> {
    if plain.len() < 6 || plain[4] != CALL {
        log!("demonware lobby: a message that isn't a call: {}", hex(&plain[..plain.len().min(48)]));
        return Ok(());
    }
    // The length counts the service id and the task's bytes (after 0x86); the rest is
    // the cipher's zero padding.
    let len = u32::from_le_bytes(plain[..4].try_into().unwrap()) as usize;
    let service = plain[5];
    let end = 5usize.saturating_add(len).clamp(6, plain.len());
    let rest = &plain[6..end];
    // The task id, typed (3 then the id), or bare in builds without types.
    let (task, args) = match rest {
        [typed::UBYTE, t, args @ ..] => (*t, args),
        [t, args @ ..] => (*t, args),
        [] => (0, &[][..]),
    };
    let outcome = dispatch(service, task, args);
    let (detail, how, reply_body) = match outcome {
        Dispatch::Error { code, detail, why } => (detail, format!(" (error {code}: {why})"), task_error(task, code)),
        Dispatch::Results { results, detail, note } => (detail, format!(" ({note})"), task_reply(task, &results)),
        Dispatch::Empty => (String::new(), " (no answer of its own: empty success)".to_string(), task_reply(task, &[])),
    };
    let key = if detail.is_empty() {
        format!("demonware call {service} {task}")
    } else {
        format!("demonware call {service} {task} {detail}")
    };
    crate::log::throttled(&key, 2, |n| {
        log!(
            "demonware lobby: {} ({service}) task {task}{how}: args {} n={n}",
            service_name(service),
            hex(&args[..args.len().min(if service == 21 { 400 } else { 96 })])
        )
    });
    let mut body = vec![TASK_REPLY];
    body.extend_from_slice(&reply_body);
    c.sent = c.sent.wrapping_add(1);
    conn.write_all(&seal(keys, c.sent, &body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    /// A client doing the handshake as Demonware's does, then one call.
    #[test]
    fn handshake_then_an_encrypted_call_gets_an_encrypted_reply() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || serve(listener.accept().unwrap().0));
        let session_key = [0x5a; 24];
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut transcript = Vec::new();

        let mut header = CLIENT_HEADER.to_le_bytes().to_vec();
        header.extend_from_slice(&[1, 0, 0, 0]);
        header.extend_from_slice(b"client header data");
        s.write_all(&header).unwrap();
        transcript.extend_from_slice(&header[8..]);

        let mut challenge = [0u8; 26];
        s.read_exact(&mut challenge).unwrap();
        assert_eq!(challenge[..6], [0x16, 0, 0, 0, MARK, SERVER_HEADER]);
        assert_eq!(u32::from_le_bytes(challenge[6..10].try_into().unwrap()), SERVER_CHALLENGE);
        transcript.extend_from_slice(&challenge);

        let mut ticket = [0u8; 128];
        ticket[..24].copy_from_slice(&session_key);
        ticket[24..28].copy_from_slice(SERVER_TICKET_MARK);
        let mut auth_body = vec![MARK, CLIENT_AUTH];
        auth_body.extend_from_slice(&ticket);
        auth_body.extend_from_slice(&[0xEE; 8]);
        let mut auth = (auth_body.len() as i32).to_le_bytes().to_vec();
        auth.extend_from_slice(&auth_body);
        s.write_all(&auth).unwrap();
        transcript.extend_from_slice(&auth[..auth.len() - 8]);
        let keys = crypto::derive(&session_key, &transcript);

        let mut done = [0u8; 14];
        s.read_exact(&mut done).unwrap();
        assert_eq!(done[..6], [0x0A, 0, 0, 0, MARK, SERVER_AUTH_DONE]);
        assert_eq!(done[6..], keys.response);

        // getServerTime: bdTitleUtilities (12), task 6.
        let mut call = vec![0, 0, 0, 0, CALL, 12, typed::UBYTE, 6];
        let n = (call.len() - 5) as u32;
        call[..4].copy_from_slice(&n.to_le_bytes());
        call.resize(16, 0);
        let iv = [3u8; 16];
        let enc = crypto::cbc_encrypt(&keys.client_to_server, &iv, &call);
        let mut msg = vec![MARK, ENCRYPTED, 1, 0, 0, 0];
        msg.extend_from_slice(&iv);
        msg.extend_from_slice(&enc);
        msg.extend_from_slice(&[0; 8]);
        let mut framed = (msg.len() as i32).to_le_bytes().to_vec();
        framed.extend_from_slice(&msg);
        s.write_all(&framed).unwrap();

        let mut len = [0u8; 4];
        s.read_exact(&mut len).unwrap();
        let mut reply = vec![0u8; i32::from_le_bytes(len) as usize];
        s.read_exact(&mut reply).unwrap();
        assert_eq!(reply[..2], [MARK, ENCRYPTED]);
        let mut signed = len.to_vec();
        signed.extend_from_slice(&reply[..reply.len() - 8]);
        assert_eq!(reply[reply.len() - 8..], crypto::hmac_sha1(&keys.hmac, &signed)[..8]);
        let iv: [u8; 16] = reply[6..22].try_into().unwrap();
        let plain = crypto::cbc_decrypt(&keys.server_to_client, &iv, &reply[22..reply.len() - 8]).unwrap();
        // Length, task reply, then typed transaction id, error 0, task 6, one result.
        assert_eq!(plain[4], TASK_REPLY);
        let body = &plain[5..];
        assert_eq!(body[0], typed::UINT64);
        assert_eq!(body[9..14], [typed::UINT32, 0, 0, 0, 0]);
        assert_eq!(body[14..16], [typed::UBYTE, 6]);
        assert_eq!(body[16..21], [typed::UINT32, 1, 0, 0, 0]);
        assert_eq!(body[26], typed::UINT32);
    }

    #[test]
    fn every_ddl_checksum_matches() {
        // Two checksums, as a game sends them (trailing byte included).
        let mut args = Vec::new();
        typed::u32(&mut args, 2);
        typed::string(&mut args, b"0123456789abcdef0123456789abcdef");
        typed::string(&mut args, b"fedcba9876543210fedcba9876543210");
        args.push(0);
        let Dispatch::Results { results: got, .. } = dispatch(96, 1, &args) else { panic!("expected DDL results") };
        assert_eq!(got.len(), 2);
        let mut first = Vec::new();
        typed::string(&mut first, b"0123456789abcdef0123456789abcdef");
        first.extend_from_slice(&[typed::BOOL, 1]);
        assert_eq!(got[0], first);
        // A count larger than what follows answers what's there; a huge one is bounded.
        let mut short = Vec::new();
        typed::u32(&mut short, u32::MAX);
        typed::string(&mut short, b"aa");
        let Dispatch::Results { results: got_short, .. } = dispatch(96, 1, &short) else { panic!("expected DDL results") };
        assert_eq!(got_short.len(), 1);
    }

    #[test]
    fn a_publisher_file_is_answered_not_there() {
        let mut args = Vec::new();
        typed::string(&mut args, b"");
        typed::string(&mut args, b"nonexistent_keys_test.txt");
        args.push(0);
        let Dispatch::Error { code, why, detail } = dispatch(10, 21, &args) else { panic!("expected BD_NO_FILE") };
        assert_eq!(code, super::super::storage::NO_FILE);
        assert_eq!(detail, "nonexistent_keys_test.txt");
        assert!(why.contains("nonexistent_keys_test.txt"), "{why}");
        // Error body: transaction id, the code, the task, the transaction id again.
        let b = task_error(21, super::super::storage::NO_FILE);
        assert_eq!(b.len(), 9 + 5 + 2 + 9);
        assert_eq!(b[9..14], [typed::UINT32, 0xE8, 3, 0, 0]);
        assert_eq!(b[14..16], [typed::UBYTE, 21]);
        assert_eq!(b[..9], b[16..]);
    }

    #[test]
    fn dml_and_publisher_variables_answer_cleanly() {
        let Dispatch::Results { results: dml, .. } = dispatch(27, 2, &[]) else { panic!("expected bdDML result") };
        assert_eq!(dml.len(), 1);
        let mut r = super::super::mm::Reader::new(&dml[0]);
        assert_eq!(r.string(), Some(&b"US"[..]));
        assert_eq!(r.string(), Some(&b"United States"[..]));
        assert_eq!(r.string(), Some(&b"New York"[..]));
        assert_eq!(r.string(), Some(&b"New York"[..]));
        assert!(matches!(dispatch(27, 3, &[]), Dispatch::Results { results, .. } if results.is_empty()));

        let mut pv = Vec::new();
        typed::string(&mut pv, b"5446");
        typed::string(&mut pv, b"client");
        match dispatch(95, 1, &pv) {
            Dispatch::Results { results, detail, note } => {
                assert!(results.is_empty());
                assert_eq!(detail, "5446:client");
                assert!(note.contains("client"), "{note}");
            }
            _ => panic!("expected bdPublisherVariables result"),
        }
    }

    #[test]
    fn the_session_key_comes_from_the_server_ticket() {
        let mut auth = vec![MARK, CLIENT_AUTH, 9, 9];
        auth.extend_from_slice(&[0x77; 24]);
        auth.extend_from_slice(SERVER_TICKET_MARK);
        assert_eq!(session_key_in(&auth), Some([0x77; 24]));
    }
}
