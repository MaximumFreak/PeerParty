//! Robustness checks for parsers that read bytes from the network (other LAN peers, or a
//! game's own traffic): hostile or cut-short input must be rejected, never panic. A panic on
//! a network thread silently stops that host; one inside an export aborts the game.

use std::io::Cursor;
use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};

use peerparty_core::PeerId;
use peerparty_protocol::json::{Json, MAX_DEPTH};

use crate::demonware::{auth, crypto, lobby, mm, stun};
use crate::https::server::Request;
use crate::photon::message::{MAGIC, Proto, parse_hello};
use crate::photon::value::Rd;
use crate::photon::{enet, fusion, gp16, gp18, ws};
use crate::steam::ids::{self, find_ids, rewrite};
use crate::ugs::{dtls, relay};
use crate::util::crypto::Aes256;
use crate::util::http::{Dechunk, find};
use crate::voice::{self, Opus};

/// Deterministic xorshift, so a failure names an input that reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let n = self.next() as usize % (max + 1);
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// Plausible first bytes, so inputs get past the first check and into the parser's body.
const LEADS: [u8; 12] = [MAGIC, 0xFD, 0x00, 0x01, 0x02, 0x16, 0x17, 0x80, 0x81, 0x82, 0xFE, 0xFF];

fn inputs() -> Vec<Vec<u8>> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut out: Vec<Vec<u8>> = vec![Vec::new(), vec![0; 64], vec![0xFF; 64]];
    for i in 0..6000 {
        let mut b = rng.bytes(if i % 10 == 0 { 2048 } else { 96 });
        if !b.is_empty() && i % 2 == 0 {
            b[0] = LEADS[i / 2 % LEADS.len()];
        }
        if b.len() > 1 && i % 3 == 0 {
            b[1] = (rng.next() % 32) as u8;
        }
        out.push(b);
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().take(48).map(|x| format!("{x:02x}")).collect()
}

fn survives(name: &str, f: impl FnMut(&[u8])) {
    survives_with(name, &[], f);
}

fn survives_with(name: &str, extra: &[Vec<u8>], mut f: impl FnMut(&[u8])) {
    let mut failed = Vec::new();
    let mut all = inputs();
    all.extend(extra.iter().cloned());
    for b in all {
        if catch_unwind(AssertUnwindSafe(|| f(&b))).is_err() {
            failed.push(format!("{} bytes: {}", b.len(), hex(&b)));
        }
    }
    assert!(failed.is_empty(), "{name} panicked on {} inputs, e.g. {:?}", failed.len(), &failed[..failed.len().min(3)]);
}

#[test]
fn photon_parsers_reject_hostile_bytes() {
    let key = Aes256::new(&[7u8; 32]);
    survives("enet::parse", |b| { let _ = enet::parse(b); });
    survives("fusion::decode", |b| { let _ = fusion::decode(b); });
    survives("parse_hello", |b| { let _ = parse_hello(b); });
    // A '%' followed by part of a character: the two bytes after it aren't text.
    let hello = parse_hello("POST /?init&app=a%41%e\u{e9}%\u{e9}\u{e9}%4 HTTP/1.1\r\n\r\n".as_bytes()).unwrap();
    assert_eq!(hello.init.app_id, "aA%e\u{e9}%\u{e9}\u{e9}%4");
    for proto in [Proto::V16, Proto::V18] {
        survives(proto.name(), |b| {
            let _ = proto.decode(b, None);
            let _ = proto.decode(b, Some(&key));
            let _ = proto.read_value(&mut Rd::new(b));
        });
    }
    survives("gp16 bodies", |b| {
        let _ = gp16::read_params(&mut Rd::new(b));
        let _ = gp16::read_response_body(&mut Rd::new(b));
        let _ = gp16::read_disconnect_body(&mut Rd::new(b));
    });
    survives("gp18 bodies", |b| {
        let _ = gp18::read_params(&mut Rd::new(b));
        let _ = gp18::read_response_body(&mut Rd::new(b));
        let _ = gp18::read_disconnect_body(&mut Rd::new(b));
    });
    survives("ws::read_frame", |b| {
        let mut c = Cursor::new(b.to_vec());
        while ws::read_frame(&mut c).is_ok() {}
    });
}

#[test]
fn ugs_parsers_reject_hostile_bytes() {
    survives("ws::parse_frame", |b| { let _ = crate::util::ws::parse_frame(b); });
    survives("relay::parse_connection_data", |b| { let _ = relay::parse_connection_data(b); });
    survives("dtls records", |b| {
        let _ = dtls::is_dtls(b);
        for (_, _, _, frag) in dtls::records(b) {
            let _ = dtls::handshake_msgs(frag);
        }
        let _ = dtls::handshake_msgs(b);
    });
}

fn key16(b: &[u8]) -> [u8; 16] {
    let mut k = [0u8; 16];
    let n = b.len().min(16);
    k[..n].copy_from_slice(&b[..n]);
    k
}

/// A PeerParty app ticket: magic, SteamID, 24-byte key, then a name that may not be UTF-8.
fn app_ticket(steam_id: u64, key: &[u8], name: &[u8]) -> Vec<u8> {
    let mut t = b"PPDW".to_vec();
    t.extend_from_slice(&steam_id.to_le_bytes());
    t.extend_from_slice(&key[..key.len().min(24)]);
    t.extend_from_slice(name);
    t
}

fn demonware_edges() -> Vec<Vec<u8>> {
    let mut v = vec![
        vec![],
        vec![0],
        b"PPDW".to_vec(),
        b"PPDWshort".to_vec(),
        app_ticket(42, &[0xff; 24], &[0xff, 0xfe, 0x00, b'Z']),
        app_ticket(u64::MAX, &[], b"\xff"),
        vec![crate::demonware::stun::IP_REQUEST],
        vec![crate::demonware::stun::NAT_REQUEST],
        vec![crate::demonware::stun::IP_REPLY, 2, 0],
        vec![99, 2, 0],
        vec![0xAB, 0x85],
        vec![0xAB, 0x82],
    ];
    // Blob tag, then a typed u32 length of u32::MAX and no body.
    let mut big = vec![mm::BLOB, crate::demonware::lobby::typed::UINT32];
    big.extend_from_slice(&u32::MAX.to_le_bytes());
    v.push(big.clone());
    v.push(big[..big.len() - 1].to_vec());
    // A real session info, and every cut-short prefix of it.
    let mut info = Vec::new();
    mm::blob(&mut info, &[192, 168, 1, 9, 0x0c, 0x02]);
    crate::demonware::lobby::typed::u32(&mut info, 0x0f);
    crate::demonware::lobby::typed::u32(&mut info, 8);
    crate::demonware::lobby::typed::u64(&mut info, u64::MAX);
    info.push(mm::STRING);
    info.extend_from_slice(&[0xff, 0xfe]);
    info.push(0);
    for i in 0..info.len() {
        v.push(info[..i].to_vec());
    }
    v.push(info);
    // Mark sitting too early to hold a 24-byte session key, then one that does.
    let mut early = vec![0xAB, 0x82];
    early.extend_from_slice(b"PPST");
    v.push(early);
    let mut auth = vec![0xAB, 0x82];
    auth.extend_from_slice(&[0x77; 24]);
    auth.extend_from_slice(b"PPST");
    auth.extend_from_slice(&[0xff; 8]);
    v.push(auth);
    v.push(br#"{"auth_task":"28","iv_seed":"1","title_id":"7","extra_data":"{}"}"#.to_vec());
    v.push(nest("{\"a\":", "}", 80).into_bytes());
    v.push(vec![0xff, 0xfe, b'{', b'"']);
    v
}

fn http_edges() -> Vec<Vec<u8>> {
    let mut v = vec![
        vec![],
        b"0\r\n\r\n".to_vec(),
        b"0\r\n".to_vec(),
        b"zz\r\n".to_vec(),
        b"\xff\r\n".to_vec(),
        b"5\r\nab".to_vec(),
        b"1\r\na\r\n".to_vec(),
        b"\r\n\r\n".to_vec(),
        b";\r\n".to_vec(),
        b"-\r\n".to_vec(),
        b" 0 ;ext\r\n\r\n".to_vec(),
        b"0\r\nNot-A-Trailer\r\n\r\n".to_vec(),
        format!("{:x}\r\n", usize::MAX).into_bytes(),
        format!("{:x}\r\nab\r\n", usize::MAX - 1).into_bytes(),
        format!("{:x}\r\n", usize::MAX / 2).into_bytes(),
        b"1\r\na\r\n".repeat(8),
        b"GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(),
        b"POST /auth/ HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".to_vec(),
        b"POST / HTTP/1.1\r\nContent-Length: 999999999999999999999\r\n\r\n".to_vec(),
    ];
    let whole = b"3\r\nabc\r\n2;x=y\r\nde\r\n0\r\nTrailer: 1\r\n\r\n";
    for i in 0..whole.len() {
        v.push(whole[..i].to_vec());
    }
    v
}

fn feed_chunked(bytes: &[u8], max: usize) -> std::io::Result<bool> {
    let mut d = Dechunk::new(max);
    let mut buf = bytes.to_vec();
    d.feed(&mut buf)
}

fn voice_edges() -> Vec<Vec<u8>> {
    let mut v = vec![vec![], vec![voice::AUDIO], vec![voice::OPUS], vec![voice::AUDIO, 0, 0, 0]];
    for &(rate, ch, n) in &[
        (1u32, 1u8, 320u16),
        (7_999, 1, 160),
        (8_000, 0, 20),
        (8_000, 1, 0),
        (8_000, 1, 20),
        (16_000, 1, 320),
        (16_000, 1, 321),
        (16_000, 3, 320),
        (16_000, 9, 320),
        (48_000, 2, 960),
        (48_000, 2, 2880),
        (96_000, 8, 960),
        (u32::MAX, 255, u16::MAX),
    ] {
        let mut opus = vec![voice::OPUS];
        opus.extend_from_slice(&rate.to_le_bytes());
        opus.push(ch);
        opus.extend_from_slice(&n.to_le_bytes());
        v.push(opus.clone());
        opus.extend_from_slice(&[0xff; 24]);
        v.push(opus);
        let mut raw = vec![voice::AUDIO];
        raw.extend_from_slice(&rate.to_le_bytes());
        raw.push(ch);
        raw.resize(raw.len() + (n as usize).saturating_mul(2).min(64), 0);
        v.push(raw);
    }
    // A header claiming a second of audio, with only a handful of samples after it.
    let mut huge = vec![voice::AUDIO];
    huge.extend_from_slice(&16_000u32.to_le_bytes());
    huge.push(1);
    huge.resize(6 + 8, 0x7f);
    v.push(huge);
    v
}

fn opus_framed(payload: &[u8]) -> Vec<u8> {
    let mut b = vec![voice::OPUS];
    b.extend_from_slice(&16_000u32.to_le_bytes());
    b.push(1);
    b.extend_from_slice(&320u16.to_le_bytes());
    b.extend_from_slice(&payload[..payload.len().min(48)]);
    b
}

fn audio_framed(payload: &[u8]) -> Vec<u8> {
    let mut b = vec![voice::AUDIO];
    b.extend_from_slice(&16_000u32.to_le_bytes());
    b.push(1);
    b.extend_from_slice(&payload[..payload.len().min(64)]);
    b
}

fn play(f: &voice::Frame) {
    let _ = voice::to_device(f);
    let _ = voice::to_stereo(f);
    let _ = voice::resample(f, 48_000);
    let _ = voice::resample(f, 0);
}

fn utf16_le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect()
}

/// `open` * `close` nested `n` deep around a `0`.
fn nest(open: &str, close: &str, n: usize) -> String {
    format!("{}0{}", open.repeat(n), close.repeat(n))
}

fn walk_json(text: &str) {
    let Some(v) = Json::parse(text) else { return };
    let _ = v.as_str();
    let _ = v.as_bool();
    let _ = v.as_i64();
    let _ = v.as_u64();
    let _ = v.as_f64();
    let _ = v.as_arr().len();
    let _ = v.as_obj().len();
    let _ = v.is_null();
    let _ = v.get("a").as_u64();
    let _ = v.get("id").to_string();
    let _ = v.to_string();
}

fn json_edges() -> Vec<String> {
    let mut v = vec![
        String::new(),
        " ".into(),
        "null".into(),
        "{".into(),
        "[".into(),
        "\"".into(),
        "\"\\".into(),
        "\"\\u".into(),
        "\"\\u12\"".into(),
        "\"\\uD800\"".into(),
        "\"\\uD800\\u\"".into(),
        "\"\\uD800\\uDC00\"".into(),
        "-".into(),
        "+".into(),
        "01".into(),
        "-0".into(),
        "1e999999".into(),
        "-9223372036854775809".into(),
        "18446744073709551616".into(),
        "9".repeat(80),
        "{\"a\":1,\"a\":2}".into(),
        "[1,]".into(),
        "truee".into(),
        format!("[{}]", "0,".repeat(2000)),
    ];
    for n in [0, 1, MAX_DEPTH - 1, MAX_DEPTH, MAX_DEPTH + 1, 256, 20_000] {
        v.push(nest("[", "]", n));
        v.push(nest("{\"a\":", "}", n));
        v.push("[".repeat(n));
        v.push("{\"a\":".repeat(n));
    }
    v
}

#[test]
fn demonware_parsers_reject_hostile_bytes() {
    let from: SocketAddr = "203.0.113.9:47590".parse().unwrap();
    let server: SocketAddr = "198.51.100.2:3074".parse().unwrap();
    let v6: SocketAddr = "[2001:db8::1]:3074".parse().unwrap();
    let keys = crypto::derive(&[0x11; 24], b"transcript");
    // A name that isn't UTF-8 still reads; the NUL ends it.
    let ticket = app_ticket(42, &[9; 24], &[0xff, 0xfe, 0, b'A']);
    let player = auth::read_app_ticket(&ticket);
    assert_eq!(player.steam_id, 42);
    assert_eq!(player.key, vec![9; 24]);
    assert!(player.name.chars().all(|c| c != '\0'));
    let mut huge = vec![mm::BLOB, crate::demonware::lobby::typed::UINT32];
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(mm::Reader::new(&huge).blob().is_none());
    assert!(mm::Session::from_info([0; 8], &huge).is_none());
    assert!(lobby::open(&keys, &[0xAB, 0x85]).is_none());
    assert!(crypto::cbc_decrypt(&[0; 16], &[0; 16], &[1, 2, 3]).is_none());
    assert!(stun::answer(&[], from, server).is_none());
    assert!(stun::answer(&[stun::IP_REQUEST], from, server).is_some());
    assert!(stun::answer(&[stun::NAT_REQUEST], v6, v6).is_some());
    let (status, body) = auth::handle(&Request {
        method: "POST".into(),
        path: "/auth/".into(),
        headers: vec![("Host".into(), "title-pc-auth3.prod.demonware.net".into())],
        body: b"\xff\xfe{".to_vec(),
    });
    assert_eq!(status, 400);
    assert_eq!(body, "{}");

    survives_with("demonware", &demonware_edges(), |b| {
        let _ = auth::read_app_ticket(b);
        let path = if b.first().copied().unwrap_or(0) % 2 == 0 { "/auth/" } else { "/nope" };
        let _ = auth::handle(&Request {
            method: "POST".into(),
            path: path.into(),
            headers: vec![("Host".into(), "title-pc-auth3.prod.demonware.net".into())],
            body: b.to_vec(),
        });
        let key = key16(b);
        let iv = key16(b.get(16..).unwrap_or(b));
        let _ = crypto::hmac_sha1(b, b);
        let n = b.first().copied().unwrap_or(0) as usize % 64;
        let _ = crypto::expand(b, b, n);
        let _ = crypto::derive(b, b);
        let _ = crypto::cbc_encrypt(&key, &iv, b);
        let _ = crypto::cbc_decrypt(&key, &iv, b);
        let _ = lobby::open(&keys, b);
        let _ = lobby::session_key_in(b);
        let mut r = mm::Reader::new(b);
        let _ = r.u32();
        let _ = r.u64();
        let _ = r.blob();
        let _ = r.string();
        let _ = r.rest();
        if let Some(s) = mm::Session::from_info([9; 8], b) {
            let _ = s.ids();
            let _ = s.result();
            let _ = mm::Session::decode(&s.encode());
        }
        let _ = mm::Session::decode(&String::from_utf8_lossy(b));
        let _ = crate::demonware::is_host(&String::from_utf8_lossy(b));
        let _ = stun::answer(b, from, server);
        let _ = stun::answer(b, v6, server);
    });
}

#[test]
fn json_parser_rejects_hostile_text() {
    // The parser takes text. Bytes that aren't UTF-8 stop at that boundary.
    let mut raw = vec![b'{'];
    raw.extend_from_slice(&[0xff, 0xfe, b'"', 0x80, b'}']);
    assert!(std::str::from_utf8(&raw).is_err());
    walk_json(&String::from_utf8_lossy(&raw));
    // Past the depth limit the parser stops, including on a stack that a few kilobytes of '[' would blow.
    assert!(Json::parse(&nest("[", "]", MAX_DEPTH - 1)).is_some());
    assert!(Json::parse(&nest("{\"a\":", "}", MAX_DEPTH - 1)).is_some());
    for n in [MAX_DEPTH, 256, 20_000] {
        assert!(Json::parse(&nest("[", "]", n)).is_none(), "array depth {n}");
        assert!(Json::parse(&nest("{\"a\":", "}", n)).is_none(), "object depth {n}");
        assert!(Json::parse(&"[".repeat(n)).is_none());
    }
    survives("json", |b| walk_json(&String::from_utf8_lossy(b)));
    for text in json_edges() {
        walk_json(&text);
    }
}

#[test]
fn https_parsers_reject_hostile_bytes() {
    // Chunked bodies (and a size that must not allocate) are refused, not panicked on.
    assert!(feed_chunked(b"zz\r\n", 100).is_err());
    assert!(feed_chunked(b"\xff\r\n", 100).is_err());
    assert!(feed_chunked(b"-\r\n", 100).is_err());
    assert!(feed_chunked(&format!("{:x}\r\nab\r\n", usize::MAX).into_bytes(), 100).is_err());
    assert!(feed_chunked(&format!("{:x}\r\n", usize::MAX).into_bytes(), usize::MAX).is_err());
    assert!(feed_chunked(&format!("{:x}\r\nab\r\n", usize::MAX - 1).into_bytes(), usize::MAX).is_err());
    assert_eq!(feed_chunked(b"5\r\nab", 100).unwrap(), false);
    assert_eq!(feed_chunked(b"0\r\n\r\n", 0).unwrap(), true);
    assert!(feed_chunked(b"1\r\na\r\n", 0).is_err());
    let whole = b"3\r\nabc\r\n0\r\n\r\n";
    for i in 0..whole.len() {
        let mut d = Dechunk::new(32);
        let mut buf = Vec::new();
        for &c in &whole[..i] {
            buf.push(c);
            let _ = d.feed(&mut buf);
        }
    }
    // TLS records are not parsed in this crate: https/tls.rs gives the bytes to SChannel.
    // What this process reads itself is the chunked body and the header split.
    survives_with("https", &http_edges(), |b| {
        for max in [0usize, 8, 64, usize::MAX] {
            let _ = feed_chunked(b, max);
        }
        let _ = find(b, b"");
        let _ = find(b, b"\r\n");
        let _ = find(b, b"\r\n\r\n");
        let _ = find(b, &[0xff, 0xfe]);
        let status = u16::from_le_bytes([b.first().copied().unwrap_or(0), b.get(1).copied().unwrap_or(0)]);
        let _ = crate::util::http::reason(status);
        // The request head, split the way https/server.rs splits it once the bytes are in.
        if let Some(end) = find(b, b"\r\n\r\n") {
            let text = String::from_utf8_lossy(&b[..end]);
            let mut lines = text.split("\r\n");
            let mut parts = lines.next().unwrap_or_default().split_whitespace();
            let method = parts.next().unwrap_or("GET").to_ascii_uppercase();
            let path = parts.next().unwrap_or("/").to_string();
            let headers: Vec<(String, String)> = lines
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .collect();
            let req = Request { method, path, headers, body: b[end..].to_vec() };
            let _ = req.header("Host");
            let _ = req.header("Transfer-Encoding");
            if let Some(len) = req.header("Content-Length") {
                let _ = len.parse::<usize>();
            }
        }
    });
}

#[test]
fn voice_parsers_reject_hostile_bytes() {
    let peer = PeerId([1; 16]);
    let mut opus = Opus::default();
    assert!(opus.decode(peer, &[]).is_none());
    assert!(opus.decode(peer, &[voice::OPUS]).is_none());
    assert!(voice::decode_audio(&[]).is_none());
    assert!(voice::decode_audio(&[voice::AUDIO, 1, 0, 0, 0, 1]).is_none());
    // A rate of 1 used to make the resampler write thousands of samples per one sent.
    let mut absurd = vec![voice::AUDIO];
    absurd.extend_from_slice(&1u32.to_le_bytes());
    absurd.push(1);
    absurd.resize(6 + 64, 0);
    assert!(voice::decode_audio(&absurd).is_none());

    survives_with("voice", &voice_edges(), |b| {
        if let Some(f) = opus.decode(peer, b) {
            play(&f);
        }
        if let Some(f) = opus.decode(peer, &opus_framed(b)) {
            play(&f);
        }
        if let Some(f) = voice::decode_audio(b) {
            play(&f);
        }
        if let Some(f) = voice::decode_audio(&audio_framed(b)) {
            play(&f);
        }
    });
}

#[test]
fn steam_id_rewrite_rejects_hostile_bytes() {
    const A: u64 = 76561198000000000;
    const B: u64 = 76561198000000001;
    let mut empty = Vec::new();
    assert_eq!(rewrite(&mut empty, &[(A, B)], true), ids::Swapped::default());
    let mut odd = vec![0x31, 0x00, b'1'];
    odd.extend(utf16_le(&A.to_string()));
    odd.push(0);
    let _ = rewrite(&mut odd, &[(A, B)], true);
    assert!(find_ids(&[], &[A, 0]).is_empty());
    assert_eq!(ids::hex_head(&[], 0), "");

    survives_with("steam::ids::rewrite", &[vec![], vec![0], A.to_le_bytes().to_vec(), utf16_le("1")], |b| {
        let mut raw = [0u8; 8];
        let n = b.len().min(8);
        raw[..n].copy_from_slice(&b[..n]);
        let from = u64::from_le_bytes(raw);
        let swaps = [(A, B), (B, A), (from, from.wrapping_add(1))];
        let mut d = b.to_vec();
        d.extend_from_slice(&A.to_le_bytes());
        d.extend_from_slice(&A.to_be_bytes());
        d.extend_from_slice(&(A as u32).to_le_bytes());
        d.extend_from_slice(A.to_string().as_bytes());
        d.extend_from_slice(&utf16_le(&A.to_string()));
        d.extend_from_slice(b);
        let _ = rewrite(&mut d, &swaps, true);
        let mut plain = b.to_vec();
        let _ = rewrite(&mut plain, &swaps, false);
        let _ = rewrite(&mut plain, &[], true);
        let _ = find_ids(b, &[A, B, from, 0]);
        let _ = ids::hex_head(b, 0);
        let _ = ids::hex_head(b, 8);
    });
}