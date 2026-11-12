//! DTLS 1.2 with a pre-shared key, server side: what Unity Relay speaks to a game that
//! asks for a secure relay connection, so the relay stand-in (relay.rs) can too.
//!
//! Unity Transport 2.x (Runtime/Layers/DTLSLayer.cs, Runtime/TLS/UnityTLSConfiguration.cs)
//! puts a DTLS client (UnityTLS, on MbedTLS) under its relay layer when the relay server
//! data says `IsSecure`. The PSK identity is the 16-byte allocation id and the PSK is
//! the allocation's 64-byte HMAC key (the allocation's `key`); there is no certificate.
//! Every Relay message (Bind, Ping, Relay...) then goes inside DTLS application data.
//! Its 37 bytes of DTLS overhead with a relay (13 header + 8 explicit nonce + 16 tag)
//! fit TLS_PSK_WITH_AES_128_GCM_SHA256 (RFC 5487), the one suite answered here.
//!
//! The handshake (RFC 6347 on RFC 5246, PSK as RFC 4279):
//!   client: ClientHello
//!   server: HelloVerifyRequest(cookie)            (`check_hello`, no state kept)
//!   client: ClientHello(cookie)
//!   server: ServerHello, ServerHelloDone          (no ServerKeyExchange: no hint)
//!   client: ClientKeyExchange(identity), ChangeCipherSpec, Finished
//!   server: ChangeCipherSpec, Finished
//! Extended master secret (RFC 7627) and secure renegotiation's empty
//! renegotiation_info (RFC 5746) are echoed when offered; nothing else is. A client's
//! retransmitted flight gets the last server flight again. Handshake messages must
//! arrive whole (MbedTLS doesn't fragment messages this small).
//!
//! The cookie (RFC 6347 4.2.1) is an HMAC of the client's address and random with a
//! secret of the relay's: a session, and its memory, exists only for an address that
//! answered from where it claims to be. Encrypted records pass a replay window (RFC
//! 6347 4.1.2.6) once authenticated. There's no renegotiation.

use std::time::Instant;

use crate::photon::crypto::{hmac_sha256, random_bytes, sha256, Aes};

const CT_CCS: u8 = 20;
const CT_ALERT: u8 = 21;
const CT_HANDSHAKE: u8 = 22;
const CT_APP: u8 = 23;

const HS_CLIENT_HELLO: u8 = 1;
const HS_SERVER_HELLO: u8 = 2;
const HS_HELLO_VERIFY_REQUEST: u8 = 3;
const HS_SERVER_HELLO_DONE: u8 = 14;
const HS_CLIENT_KEY_EXCHANGE: u8 = 16;
const HS_FINISHED: u8 = 20;

const DTLS12: [u8; 2] = [0xFE, 0xFD];
/// HelloVerifyRequest's version, whatever is negotiated (RFC 6347 4.2.1).
const DTLS10: [u8; 2] = [0xFE, 0xFF];
const COOKIE_LEN: usize = 16;
pub const PSK_AES_128_GCM_SHA256: u16 = 0x00A8;
const EXT_EMS: u16 = 0x0017;
const EXT_RENEGOTIATION: u16 = 0xFF01;
const SCSV_RENEGOTIATION: u16 = 0x00FF;

const ALERT_BAD_RECORD_MAC: u8 = 20;
const ALERT_HANDSHAKE_FAILURE: u8 = 40;
const ALERT_DECRYPT_ERROR: u8 = 51;
const ALERT_UNKNOWN_PSK_IDENTITY: u8 = 115;

/// A datagram that starts like a DTLS record (not a Relay message, whose first byte is 0xDA).
pub fn is_dtls(pkt: &[u8]) -> bool {
    pkt.len() >= 13 && (CT_CCS..=CT_APP).contains(&pkt[0]) && pkt[1] == 0xFE
}

/// What a datagram from an address without a session is.
#[derive(Debug, PartialEq)]
pub enum Hello {
    /// Not a ClientHello: nothing is kept or answered.
    Other,
    /// A ClientHello with this address's cookie: start a session.
    Verified,
    /// A ClientHello without it: send this HelloVerifyRequest back, and keep nothing.
    Verify(Vec<u8>),
}

fn cookie(secret: &[u8], peer: &[u8], random: &[u8; 32]) -> Vec<u8> {
    let mut m = peer.to_vec();
    m.extend_from_slice(random);
    hmac_sha256(secret, &m)[..COOKIE_LEN].to_vec()
}

/// The cookie exchange (RFC 6347 4.2.1), statelessly: `peer` is the sender's address
/// (any stable bytes), `secret` the relay's.
pub fn check_hello(secret: &[u8], peer: &[u8], d: &[u8]) -> Hello {
    let Some(&(kind, _, seq, frag)) = records(d).first() else { return Hello::Other };
    if kind != CT_HANDSHAKE || seq >> 48 != 0 {
        return Hello::Other;
    }
    let Some(&(t, msg_seq, body, _)) = handshake_msgs(frag).as_ref().and_then(|m| m.first()) else { return Hello::Other };
    if t != HS_CLIENT_HELLO {
        return Hello::Other;
    }
    let Some(hello) = ClientHello::parse(body) else { return Hello::Other };
    let want = cookie(secret, peer, &hello.random);
    if hello.cookie.len() == want.len() && hello.cookie.iter().zip(&want).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0 {
        return Hello::Verified;
    }
    let mut b = DTLS10.to_vec();
    b.push(want.len() as u8);
    b.extend_from_slice(&want);
    let m = handshake_msg(HS_HELLO_VERIFY_REQUEST, msg_seq, &b);
    // The ClientHello's record sequence number, as RFC 6347 asks.
    let mut out = vec![CT_HANDSHAKE, DTLS10[0], DTLS10[1]];
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&(m.len() as u16).to_be_bytes());
    out.extend_from_slice(&m);
    Hello::Verify(out)
}

/// TLS 1.2's PRF with SHA-256 (RFC 5246 section 5).
pub fn prf(secret: &[u8], label: &str, seed: &[u8], len: usize) -> Vec<u8> {
    let mut ls = label.as_bytes().to_vec();
    ls.extend_from_slice(seed);
    let mut out = Vec::with_capacity(len + 32);
    let mut a = hmac_sha256(secret, &ls);
    while out.len() < len {
        let mut m = a.to_vec();
        m.extend_from_slice(&ls);
        out.extend_from_slice(&hmac_sha256(secret, &m));
        a = hmac_sha256(secret, &a);
    }
    out.truncate(len);
    out
}

/// The premaster secret for plain PSK (RFC 4279 section 2).
pub fn psk_premaster(psk: &[u8]) -> Vec<u8> {
    let n = (psk.len() as u16).to_be_bytes();
    let mut pms = n.to_vec();
    pms.extend(std::iter::repeat_n(0, psk.len()));
    pms.extend_from_slice(&n);
    pms.extend_from_slice(psk);
    pms
}

/// One direction's AES-128-GCM state.
pub struct Cipher {
    aes: Aes,
    salt: [u8; 4],
}

impl Cipher {
    /// `seq` is the record's epoch and sequence number (8 bytes); `header` its type and
    /// version as in the record.
    pub fn seal(&self, seq: u64, kind: u8, version: [u8; 2], data: &[u8]) -> Vec<u8> {
        let explicit = seq.to_be_bytes();
        let mut out = explicit.to_vec();
        out.extend_from_slice(&self.aes.gcm_seal(&self.nonce(&explicit), &aad(seq, kind, version, data.len()), data));
        out
    }

    pub fn open(&self, seq: u64, kind: u8, version: [u8; 2], frag: &[u8]) -> Option<Vec<u8>> {
        let explicit: [u8; 8] = frag.get(..8)?.try_into().ok()?;
        let len = frag.len().checked_sub(8 + 16)?;
        self.aes.gcm_open(&self.nonce(&explicit), &aad(seq, kind, version, len), &frag[8..])
    }

    fn nonce(&self, explicit: &[u8; 8]) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[..4].copy_from_slice(&self.salt);
        n[4..].copy_from_slice(explicit);
        n
    }
}

fn aad(seq: u64, kind: u8, version: [u8; 2], len: usize) -> Vec<u8> {
    let mut a = seq.to_be_bytes().to_vec();
    a.push(kind);
    a.extend_from_slice(&version);
    a.extend_from_slice(&(len as u16).to_be_bytes());
    a
}

/// Master secret -> (client write, server write) ciphers.
pub fn ciphers(master: &[u8], client_random: &[u8; 32], server_random: &[u8; 32]) -> (Cipher, Cipher) {
    let mut seed = server_random.to_vec();
    seed.extend_from_slice(client_random);
    let kb = prf(master, "key expansion", &seed, 40);
    let c = Cipher { aes: Aes::new(&kb[..16]), salt: kb[32..36].try_into().unwrap() };
    let s = Cipher { aes: Aes::new(&kb[16..32]), salt: kb[36..40].try_into().unwrap() };
    (c, s)
}

/// A DTLS handshake message as one fragment (the form the transcript hashes).
pub fn handshake_msg(kind: u8, msg_seq: u16, body: &[u8]) -> Vec<u8> {
    let len = (body.len() as u32).to_be_bytes();
    let mut m = vec![kind, len[1], len[2], len[3]];
    m.extend_from_slice(&msg_seq.to_be_bytes());
    m.extend_from_slice(&[0, 0, 0, len[1], len[2], len[3]]);
    m.extend_from_slice(body);
    m
}

/// One record from a datagram: (type, version, epoch << 48 | seq, fragment).
pub fn records(d: &[u8]) -> Vec<(u8, [u8; 2], u64, &[u8])> {
    let mut out = vec![];
    let mut at = 0;
    while at + 13 <= d.len() {
        let kind = d[at];
        let version = [d[at + 1], d[at + 2]];
        let seq = u64::from_be_bytes(d[at + 3..at + 11].try_into().unwrap());
        let len = u16::from_be_bytes([d[at + 11], d[at + 12]]) as usize;
        let Some(frag) = d.get(at + 13..at + 13 + len) else { break };
        out.push((kind, version, seq, frag));
        at += 13 + len;
    }
    out
}

/// Handshake messages in a record: (type, message_seq, body, whole message), or None
/// if one is fragmented or cut short.
pub fn handshake_msgs(frag: &[u8]) -> Option<Vec<(u8, u16, &[u8], &[u8])>> {
    let mut out = vec![];
    let mut at = 0;
    while at < frag.len() {
        let h = frag.get(at..at + 12)?;
        let len = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        let offset = u32::from_be_bytes([0, h[6], h[7], h[8]]) as usize;
        let flen = u32::from_be_bytes([0, h[9], h[10], h[11]]) as usize;
        if offset != 0 || flen != len {
            return None;
        }
        let whole = frag.get(at..at + 12 + len)?;
        out.push((h[0], u16::from_be_bytes([h[4], h[5]]), &whole[12..], whole));
        at += 12 + len;
    }
    Some(out)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Hello,
    KeyExchange,
    Finished,
    Open,
    Closed,
}

/// What one datagram from the client led to.
#[derive(Default)]
pub struct Received {
    /// Datagrams for the client.
    pub send: Vec<Vec<u8>>,
    /// Application data (Relay messages) from the client.
    pub data: Vec<Vec<u8>>,
    /// The handshake finished with this datagram.
    pub opened: bool,
    /// The session is over (alert, or a handshake that can't go on).
    pub closed: bool,
    /// Why, for the log.
    pub problem: Option<String>,
}

/// One client's DTLS session on the relay's socket.
pub struct Session {
    state: State,
    client_random: [u8; 32],
    server_random: [u8; 32],
    transcript: Vec<u8>,
    ems: bool,
    renegotiation: bool,
    pub identity: Vec<u8>,
    master: Vec<u8>,
    client: Option<Cipher>,
    server: Option<Cipher>,
    /// The next record sequence number per epoch.
    send_seq: [u64; 2],
    /// Our first handshake message's message_seq: the ClientHello's (1 after a cookie).
    msg_seq: u16,
    /// The client's epoch-1 records seen: the highest sequence number, and the 64 below it.
    replay_top: Option<u64>,
    replay_bits: u64,
    /// The last flight, sent again when the client repeats its own.
    flight: Option<Vec<u8>>,
    pub last_seen: Instant,
}

impl Session {
    pub fn new(now: Instant) -> Self {
        Session {
            state: State::Hello,
            client_random: [0; 32],
            server_random: [0; 32],
            transcript: vec![],
            ems: false,
            renegotiation: false,
            identity: vec![],
            master: vec![],
            client: None,
            server: None,
            send_seq: [0; 2],
            msg_seq: 0,
            replay_top: None,
            replay_bits: 0,
            flight: None,
            last_seen: now,
        }
    }

    pub fn is_open(&self) -> bool {
        self.state == State::Open
    }

    /// Whether an authenticated epoch-1 record was seen before (or is too old to tell),
    /// and if not, notes it.
    fn replayed(&mut self, seq: u64) -> bool {
        let n = seq & 0xFFFF_FFFF_FFFF;
        match self.replay_top {
            None => {
                self.replay_top = Some(n);
                self.replay_bits = 1;
                false
            }
            Some(top) if n > top => {
                let shift = n - top;
                self.replay_bits = if shift >= 64 { 1 } else { self.replay_bits << shift | 1 };
                self.replay_top = Some(n);
                false
            }
            Some(top) => {
                let back = top - n;
                if back >= 64 || self.replay_bits >> back & 1 == 1 {
                    return true;
                }
                self.replay_bits |= 1 << back;
                false
            }
        }
    }

    /// A datagram from the client. `psk_for` gives the key for a PSK identity.
    pub fn recv(&mut self, d: &[u8], now: Instant, psk_for: &dyn Fn(&[u8]) -> Option<Vec<u8>>) -> Received {
        let mut r = Received::default();
        if self.state == State::Closed {
            r.closed = true;
            return r;
        }
        self.last_seen = now;
        let mut resend = false;
        for (kind, version, seq, frag) in records(d) {
            let epoch = (seq >> 48) as u16;
            let opened = match (epoch, &self.client) {
                (1, Some(c)) => Some(c.open(seq, kind, version, frag)),
                _ => None,
            };
            let body = match (epoch, opened) {
                (0, _) => frag.to_vec(),
                (1, Some(o)) => match o {
                    Some(_) if self.replayed(seq) => continue,
                    Some(b) => b,
                    // The first encrypted record is the client's Finished: if it doesn't
                    // open, the two sides derived different keys.
                    None if self.state == State::Finished => {
                        self.fail(&mut r, ALERT_BAD_RECORD_MAC, "the client's Finished doesn't decrypt (wrong PSK?)".into());
                        return r;
                    }
                    None => continue,
                },
                _ => continue,
            };
            // Our epoch-0 records number on from the client's ClientHello: after a cookie
            // exchange the HelloVerifyRequest took that number, and a client drops a
            // record numbered like one it has had (RFC 6347 4.2.1).
            if epoch == 0 && self.state == State::Hello {
                self.send_seq[0] = self.send_seq[0].max(seq & 0xFFFF_FFFF_FFFF);
            }
            match kind {
                CT_HANDSHAKE => {
                    let Some(msgs) = handshake_msgs(&body) else {
                        r.problem = Some("a fragmented handshake message (not handled)".into());
                        continue;
                    };
                    for (t, ms, b, whole) in msgs {
                        if self.handshake(t, ms, epoch, b, whole, psk_for, &mut r, &mut resend) {
                            return r;
                        }
                    }
                }
                CT_APP if self.state == State::Open && epoch == 1 => r.data.push(body),
                CT_ALERT => {
                    // close_notify, or any fatal alert, ends the session.
                    if body.len() >= 2 && (body[1] == 0 || body[0] == 2) {
                        r.problem = Some(format!("the client sent alert {} (level {})", body[1], body[0]));
                        self.state = State::Closed;
                        r.closed = true;
                        return r;
                    }
                }
                // ChangeCipherSpec: the client's next records are epoch 1.
                _ => {}
            }
        }
        if resend && r.send.is_empty() {
            if let Some(f) = &self.flight {
                r.send.push(f.clone());
            }
        }
        r
    }

    /// One handshake message. Returns true when the session ended.
    #[allow(clippy::too_many_arguments)]
    fn handshake(
        &mut self,
        kind: u8,
        msg_seq: u16,
        epoch: u16,
        body: &[u8],
        whole: &[u8],
        psk_for: &dyn Fn(&[u8]) -> Option<Vec<u8>>,
        r: &mut Received,
        resend: &mut bool,
    ) -> bool {
        match (kind, self.state) {
            (HS_CLIENT_HELLO, _) if epoch == 0 => {
                let Some(hello) = ClientHello::parse(body) else {
                    return self.fail(r, ALERT_HANDSHAKE_FAILURE, "an unreadable ClientHello".into());
                };
                if self.state != State::Hello {
                    if hello.random == self.client_random {
                        *resend = true;
                        return false;
                    }
                    // A new handshake from the same address: the game reconnected.
                    *self = Session::new(self.last_seen);
                }
                if !hello.suites.contains(&PSK_AES_128_GCM_SHA256) {
                    let offered: Vec<String> = hello.suites.iter().map(|s| format!("{s:04x}")).collect();
                    return self.fail(
                        r,
                        ALERT_HANDSHAKE_FAILURE,
                        format!("the client doesn't offer TLS_PSK_WITH_AES_128_GCM_SHA256 (offers {})", offered.join(" ")),
                    );
                }
                self.client_random = hello.random;
                self.msg_seq = msg_seq;
                self.server_random = random_bytes(32).try_into().unwrap();
                self.ems = hello.extensions.contains(&EXT_EMS);
                self.renegotiation =
                    hello.extensions.contains(&EXT_RENEGOTIATION) || hello.suites.contains(&SCSV_RENEGOTIATION);
                self.transcript.extend_from_slice(whole);

                let mut sh = DTLS12.to_vec();
                sh.extend_from_slice(&self.server_random);
                sh.push(0); // no session id: no resumption
                sh.extend_from_slice(&PSK_AES_128_GCM_SHA256.to_be_bytes());
                sh.push(0); // no compression
                let mut ext = vec![];
                if self.renegotiation {
                    ext.extend_from_slice(&EXT_RENEGOTIATION.to_be_bytes());
                    ext.extend_from_slice(&[0, 1, 0]);
                }
                if self.ems {
                    ext.extend_from_slice(&EXT_EMS.to_be_bytes());
                    ext.extend_from_slice(&[0, 0]);
                }
                if !ext.is_empty() {
                    sh.extend_from_slice(&(ext.len() as u16).to_be_bytes());
                    sh.extend_from_slice(&ext);
                }
                let sh = handshake_msg(HS_SERVER_HELLO, self.msg_seq, &sh);
                let done = handshake_msg(HS_SERVER_HELLO_DONE, self.msg_seq + 1, &[]);
                self.transcript.extend_from_slice(&sh);
                self.transcript.extend_from_slice(&done);
                let mut flight = self.record(CT_HANDSHAKE, 0, &sh);
                flight.extend_from_slice(&self.record(CT_HANDSHAKE, 0, &done));
                self.flight = Some(flight.clone());
                r.send.push(flight);
                self.state = State::KeyExchange;
                false
            }
            (HS_CLIENT_KEY_EXCHANGE, State::KeyExchange) if epoch == 0 => {
                let n = body.get(..2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize).unwrap_or(usize::MAX);
                let Some(identity) = body.get(2..2usize.saturating_add(n)) else {
                    return self.fail(r, ALERT_HANDSHAKE_FAILURE, "an unreadable ClientKeyExchange".into());
                };
                let Some(psk) = psk_for(identity) else {
                    return self.fail(r, ALERT_UNKNOWN_PSK_IDENTITY, format!("unknown PSK identity {}", hex(identity)));
                };
                self.identity = identity.to_vec();
                self.transcript.extend_from_slice(whole);
                let pms = psk_premaster(&psk);
                self.master = if self.ems {
                    prf(&pms, "extended master secret", &sha256(&self.transcript), 48)
                } else {
                    let mut seed = self.client_random.to_vec();
                    seed.extend_from_slice(&self.server_random);
                    prf(&pms, "master secret", &seed, 48)
                };
                let (c, s) = ciphers(&self.master, &self.client_random, &self.server_random);
                self.client = Some(c);
                self.server = Some(s);
                self.state = State::Finished;
                false
            }
            (HS_FINISHED, State::Finished) if epoch == 1 => {
                let want = prf(&self.master, "client finished", &sha256(&self.transcript), 12);
                if body != want.as_slice() {
                    return self.fail(r, ALERT_DECRYPT_ERROR, "the client's Finished doesn't match (wrong PSK?)".into());
                }
                self.transcript.extend_from_slice(whole);
                let verify = prf(&self.master, "server finished", &sha256(&self.transcript), 12);
                let fin = handshake_msg(HS_FINISHED, self.msg_seq + 2, &verify);
                let mut flight = self.record(CT_CCS, 0, &[1]);
                flight.extend_from_slice(&self.record(CT_HANDSHAKE, 1, &fin));
                self.flight = Some(flight.clone());
                r.send.push(flight);
                self.state = State::Open;
                r.opened = true;
                false
            }
            // The client's last flight again: ours was lost.
            (HS_CLIENT_KEY_EXCHANGE | HS_FINISHED, State::Open) => {
                *resend = true;
                false
            }
            _ => false,
        }
    }

    fn fail(&mut self, r: &mut Received, alert: u8, why: String) -> bool {
        let epoch = if self.state == State::Open { 1 } else { 0 };
        r.send.push(self.record(CT_ALERT, epoch, &[2, alert]));
        r.problem = Some(why);
        r.closed = true;
        self.state = State::Closed;
        true
    }

    /// A record from the server in `epoch` (1 is encrypted).
    fn record(&mut self, kind: u8, epoch: u16, data: &[u8]) -> Vec<u8> {
        let e = epoch as usize;
        let seq = (epoch as u64) << 48 | self.send_seq[e];
        self.send_seq[e] += 1;
        let frag = match (epoch, &self.server) {
            (1, Some(s)) => s.seal(seq, kind, DTLS12, data),
            _ => data.to_vec(),
        };
        let mut out = vec![kind, DTLS12[0], DTLS12[1]];
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&(frag.len() as u16).to_be_bytes());
        out.extend_from_slice(&frag);
        out
    }

    /// Application data for the client, once open.
    pub fn seal(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        self.is_open().then(|| self.record(CT_APP, 1, data))
    }
}

struct ClientHello {
    random: [u8; 32],
    cookie: Vec<u8>,
    suites: Vec<u16>,
    extensions: Vec<u16>,
}

impl ClientHello {
    fn parse(b: &[u8]) -> Option<Self> {
        let random: [u8; 32] = b.get(2..34)?.try_into().ok()?;
        let mut at = 34;
        let sid = *b.get(at)? as usize;
        at += 1 + sid;
        let n = *b.get(at)? as usize;
        let cookie = b.get(at + 1..at + 1 + n)?.to_vec();
        at += 1 + n;
        let n = u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?) as usize;
        let suites = b.get(at + 2..at + 2 + n)?.chunks(2).filter(|c| c.len() == 2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        at += 2 + n;
        let comp = *b.get(at)? as usize;
        at += 1 + comp;
        let mut extensions = vec![];
        if let Some(l) = b.get(at..at + 2) {
            let end = (at + 2 + u16::from_be_bytes([l[0], l[1]]) as usize).min(b.len());
            at += 2;
            while at + 4 <= end {
                extensions.push(u16::from_be_bytes([b[at], b[at + 1]]));
                at += 4 + u16::from_be_bytes([b[at + 2], b[at + 3]]) as usize;
            }
        }
        Some(ClientHello { random, cookie, suites, extensions })
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A DTLS-PSK client, enough to drive the server in tests.
#[cfg(test)]
pub mod client {
    use super::*;

    pub struct Client {
        pub identity: Vec<u8>,
        pub psk: Vec<u8>,
        pub random: [u8; 32],
        pub ems: bool,
        transcript: Vec<u8>,
        master: Vec<u8>,
        out: Option<Cipher>,
        inn: Option<Cipher>,
        seq: [u64; 2],
        /// The next handshake message's message_seq.
        msg_seq: u16,
        cookie: Vec<u8>,
        pub open: bool,
    }

    impl Client {
        pub fn new(identity: &[u8], psk: &[u8], ems: bool) -> Self {
            Client {
                identity: identity.to_vec(),
                psk: psk.to_vec(),
                random: random_bytes(32).try_into().unwrap(),
                ems,
                transcript: vec![],
                master: vec![],
                out: None,
                inn: None,
                seq: [0; 2],
                msg_seq: 0,
                cookie: vec![],
                open: false,
            }
        }

        fn next_msg(&mut self) -> u16 {
            self.msg_seq += 1;
            self.msg_seq - 1
        }

        fn record(&mut self, kind: u8, epoch: u16, data: &[u8]) -> Vec<u8> {
            let seq = (epoch as u64) << 48 | self.seq[epoch as usize];
            self.seq[epoch as usize] += 1;
            let frag = match (epoch, &self.out) {
                (1, Some(c)) => c.seal(seq, kind, DTLS12, data),
                _ => data.to_vec(),
            };
            let mut out = vec![kind, 0xFE, 0xFD];
            out.extend_from_slice(&seq.to_be_bytes());
            out.extend_from_slice(&(frag.len() as u16).to_be_bytes());
            out.extend_from_slice(&frag);
            out
        }

        pub fn hello(&mut self) -> Vec<u8> {
            let mut b = DTLS12.to_vec();
            b.extend_from_slice(&self.random);
            b.push(0); // session id
            b.push(self.cookie.len() as u8);
            b.extend_from_slice(&self.cookie);
            let suites = [0xC02Bu16, PSK_AES_128_GCM_SHA256, 0x00A9, SCSV_RENEGOTIATION];
            b.extend_from_slice(&((suites.len() * 2) as u16).to_be_bytes());
            for s in suites {
                b.extend_from_slice(&s.to_be_bytes());
            }
            b.extend_from_slice(&[1, 0]);
            let mut ext = vec![0x00, 0x0d, 0, 4, 0, 2, 4, 1]; // signature_algorithms, ignored
            if self.ems {
                ext.extend_from_slice(&[0x00, 0x17, 0, 0]);
            }
            b.extend_from_slice(&(ext.len() as u16).to_be_bytes());
            b.extend_from_slice(&ext);
            let ms = self.next_msg();
            let m = handshake_msg(HS_CLIENT_HELLO, ms, &b);
            // A ClientHello answered by a HelloVerifyRequest isn't in the transcript.
            self.transcript = m.clone();
            self.record(CT_HANDSHAKE, 0, &m)
        }

        /// The server's datagram: what to send back, and application data received.
        pub fn recv(&mut self, d: &[u8]) -> (Option<Vec<u8>>, Vec<Vec<u8>>) {
            let mut reply = None;
            let mut data = vec![];
            for (kind, version, seq, frag) in records(d) {
                let body = if seq >> 48 == 0 { frag.to_vec() } else { self.inn.as_ref().unwrap().open(seq, kind, version, frag).expect("server record opens") };
                match kind {
                    CT_HANDSHAKE => {
                        for (t, ms, b, whole) in handshake_msgs(&body).unwrap() {
                            match t {
                                HS_HELLO_VERIFY_REQUEST => {
                                    assert_eq!(ms, 0, "HelloVerifyRequest's message_seq");
                                    self.cookie = b[3..3 + b[2] as usize].to_vec();
                                    reply = Some(self.hello());
                                }
                                HS_SERVER_HELLO => {
                                    assert_eq!(ms, self.msg_seq - 1, "ServerHello's message_seq is the ClientHello's");
                                    self.transcript.extend_from_slice(whole);
                                    let sr: [u8; 32] = b[2..34].try_into().unwrap();
                                    assert_eq!(u16::from_be_bytes([b[35], b[36]]), PSK_AES_128_GCM_SHA256);
                                    self.master = sr.to_vec(); // server random, until the key exchange
                                }
                                HS_SERVER_HELLO_DONE => {
                                    self.transcript.extend_from_slice(whole);
                                    let sr: [u8; 32] = self.master[..32].try_into().unwrap();
                                    let mut cke = (self.identity.len() as u16).to_be_bytes().to_vec();
                                    cke.extend_from_slice(&self.identity);
                                    let ms = self.next_msg();
                                    let m = handshake_msg(HS_CLIENT_KEY_EXCHANGE, ms, &cke);
                                    self.transcript.extend_from_slice(&m);
                                    let pms = psk_premaster(&self.psk);
                                    self.master = if self.ems {
                                        prf(&pms, "extended master secret", &sha256(&self.transcript), 48)
                                    } else {
                                        let mut seed = self.random.to_vec();
                                        seed.extend_from_slice(&sr);
                                        prf(&pms, "master secret", &seed, 48)
                                    };
                                    let (c, s) = ciphers(&self.master, &self.random, &sr);
                                    let mut out = self.record(CT_HANDSHAKE, 0, &m);
                                    out.extend_from_slice(&self.record(CT_CCS, 0, &[1]));
                                    self.out = Some(c);
                                    self.inn = Some(s);
                                    let v = prf(&self.master, "client finished", &sha256(&self.transcript), 12);
                                    let ms = self.next_msg();
                                    let fin = handshake_msg(HS_FINISHED, ms, &v);
                                    self.transcript.extend_from_slice(&fin);
                                    out.extend_from_slice(&self.record(CT_HANDSHAKE, 1, &fin));
                                    reply = Some(out);
                                }
                                HS_FINISHED => {
                                    let v = prf(&self.master, "server finished", &sha256(&self.transcript), 12);
                                    assert_eq!(b, v.as_slice(), "server Finished");
                                    self.open = true;
                                }
                                _ => {}
                            }
                        }
                    }
                    CT_APP => data.push(body),
                    _ => {}
                }
            }
            (reply, data)
        }

        pub fn seal(&mut self, data: &[u8]) -> Vec<u8> {
            self.record(CT_APP, 1, data)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::client::Client;
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn prf_matches_the_published_tls12_sha256_vector() {
        // The TLS 1.2 PRF (SHA-256) test vector posted to the IETF TLS list.
        let out = prf(&unhex("9bbe436ba940f017b17652849a71db35"), "test label", &unhex("a0ba9f936cda311827a6f796ffd5198c"), 100);
        assert_eq!(hex(&out[..16]), "e3f229ba727be17b8d122620557cd453");
        assert_eq!(out.len(), 100);
    }

    fn handshake(ems: bool) {
        let now = Instant::now();
        let psk = vec![7u8; 64];
        let id = [3u8; 16];
        let key = psk.clone();
        let lookup = move |i: &[u8]| (i == id).then(|| key.clone());
        let mut s = Session::new(now);
        let mut c = Client::new(&id, &psk, ems);
        let hello = c.hello();
        assert!(is_dtls(&hello));
        let r = s.recv(&hello, now, &lookup);
        assert_eq!(r.send.len(), 1);
        // The ClientHello again (our flight was lost) gets the same flight back.
        assert_eq!(s.recv(&hello, now, &lookup).send, r.send);
        let (flight3, _) = c.recv(&r.send[0]);
        let flight3 = flight3.unwrap();
        let r = s.recv(&flight3, now, &lookup);
        assert!(r.opened, "{:?}", r.problem);
        assert_eq!(s.ems, ems);
        c.recv(&r.send[0]);
        assert!(c.open);
        // Flight 3 again: flight 4 again.
        assert_eq!(s.recv(&flight3, now, &lookup).send, r.send);
        // Data both ways; a record replayed, or older than the window, is dropped.
        let first = c.seal(b"bind me");
        let r = s.recv(&first, now, &lookup);
        assert_eq!(r.data, vec![b"bind me".to_vec()]);
        assert!(s.recv(&first, now, &lookup).data.is_empty(), "replayed");
        let later: Vec<Vec<u8>> = (0..70).map(|i| c.seal(&[i])).collect();
        assert_eq!(s.recv(&later[69], now, &lookup).data, vec![vec![69u8]]);
        assert_eq!(s.recv(&later[10], now, &lookup).data, vec![vec![10u8]], "late but inside the window");
        assert!(s.recv(&later[10], now, &lookup).data.is_empty(), "replayed");
        assert!(s.recv(&later[2], now, &lookup).data.is_empty(), "older than the window");
        let down = s.seal(&[0xDA, 0x72, 0, 1]).unwrap();
        assert_eq!(c.recv(&down).1, vec![vec![0xDA, 0x72, 0, 1]]);
        assert_eq!(s.identity, id);
    }

    #[test]
    fn a_cookie_is_asked_for_and_checked_without_keeping_state() {
        let secret = [5u8; 32];
        let mut c = Client::new(&[3u8; 16], &[7u8; 64], true);
        let first = c.hello();
        let Hello::Verify(hvr) = check_hello(&secret, b"10.0.0.2:5000", &first) else { panic!("no HelloVerifyRequest") };
        let (again, _) = c.recv(&hvr);
        let again = again.unwrap();
        assert_eq!(check_hello(&secret, b"10.0.0.2:5000", &again), Hello::Verified);
        // The same cookie from another address, or under another secret, isn't one.
        assert!(matches!(check_hello(&secret, b"10.0.0.9:5000", &again), Hello::Verify(_)));
        assert!(matches!(check_hello(&[6u8; 32], b"10.0.0.2:5000", &again), Hello::Verify(_)));
        assert_eq!(check_hello(&secret, b"x", &[23, 0xFE, 0xFD, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0]), Hello::Other);
        // The session then completes with message_seq following the second ClientHello.
        let lookup = |i: &[u8]| (i == [3u8; 16]).then(|| vec![7u8; 64]);
        let now = Instant::now();
        let mut s = Session::new(now);
        let r = s.recv(&again, now, &lookup);
        let (f3, _) = c.recv(&r.send[0]);
        let r = s.recv(&f3.unwrap(), now, &lookup);
        assert!(r.opened, "{:?}", r.problem);
        c.recv(&r.send[0]);
        assert!(c.open);
    }

    #[test]
    fn a_psk_handshake_opens_and_carries_data() {
        handshake(true);
        handshake(false);
    }

    #[test]
    fn a_wrong_psk_or_identity_is_refused() {
        let now = Instant::now();
        let lookup = |i: &[u8]| (i == [3u8; 16]).then(|| vec![7u8; 64]);
        for (id, psk) in [([3u8; 16], vec![8u8; 64]), ([4u8; 16], vec![7u8; 64])] {
            let mut s = Session::new(now);
            let mut c = Client::new(&id, &psk, true);
            let r = s.recv(&c.hello(), now, &lookup);
            let (f3, _) = c.recv(&r.send[0]);
            let r = s.recv(&f3.unwrap(), now, &lookup);
            assert!(r.closed && !r.opened);
            assert!(r.problem.is_some());
            assert!(s.seal(b"x").is_none());
        }
    }

    fn openssl() -> Option<std::path::PathBuf> {
        let candidates = [
            "openssl".into(),
            std::path::PathBuf::from(r"C:\Program Files\Git\mingw64\bin\openssl.exe"),
            std::path::PathBuf::from(r"C:\Program Files\Git\usr\bin\openssl.exe"),
        ];
        candidates.into_iter().find(|p| {
            use std::os::windows::process::CommandExt;
            std::process::Command::new(p)
                // No console window of its own (tests mustn't flash one on screen).
                .creation_flags(0x0800_0000)
                .arg("version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
    }

    /// An independent client: OpenSSL's s_client in DTLS 1.2 PSK mode, against a
    /// session on a real UDP socket that echoes what it gets. Skipped without OpenSSL
    /// (Git for Windows ships one).
    #[test]
    fn openssl_s_client_completes_the_handshake() {
        use std::io::{Read, Write};
        use std::net::UdpSocket;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        let Some(openssl) = openssl() else {
            eprintln!("openssl not found; skipped");
            return;
        };
        let identity = b"peerparty-alloc!"; // 16 bytes, like an allocation id
        let psk: Vec<u8> = (0..64u8).collect();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        sock.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let server = {
            let psk = psk.clone();
            std::thread::spawn(move || {
                let lookup = move |i: &[u8]| (i == identity).then(|| psk.clone());
                let secret = random_bytes(32);
                let mut session: Option<Session> = None;
                let mut verified = false;
                let mut got = vec![];
                let deadline = Instant::now() + Duration::from_secs(20);
                let mut buf = [0u8; 2048];
                while Instant::now() < deadline && !got.ends_with(b"\n") {
                    let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
                    // As the relay does: a session only for an address that echoed its cookie.
                    if session.is_none() {
                        match check_hello(&secret, from.to_string().as_bytes(), &buf[..n]) {
                            Hello::Verify(hvr) => {
                                sock.send_to(&hvr, from).unwrap();
                                continue;
                            }
                            Hello::Other => continue,
                            Hello::Verified => {
                                verified = true;
                                session = Some(Session::new(Instant::now()));
                            }
                        }
                    }
                    let s = session.as_mut().unwrap();
                    let r = s.recv(&buf[..n], Instant::now(), &lookup);
                    for p in r.send {
                        sock.send_to(&p, from).unwrap();
                    }
                    for d in r.data {
                        got.extend_from_slice(&d);
                        let mut echo = b"echo:".to_vec();
                        echo.extend_from_slice(&d);
                        sock.send_to(&s.seal(&echo).unwrap(), from).unwrap();
                    }
                    assert!(r.problem.is_none(), "{:?}", r.problem);
                }
                let s = session.expect("a session");
                assert!(verified);
                (s.is_open(), s.ems, got)
            })
        };
        let psk_hex: String = psk.iter().map(|b| format!("{b:02x}")).collect();
        use std::os::windows::process::CommandExt;
        let mut child = Command::new(openssl)
            // No console window of its own (tests mustn't flash one on screen).
            .creation_flags(0x0800_0000)
            .args(["s_client", "-dtls1_2", "-connect", &format!("127.0.0.1:{port}"), "-cipher", "PSK-AES128-GCM-SHA256"])
            .args(["-psk", &psk_hex, "-psk_identity", std::str::from_utf8(identity).unwrap(), "-quiet"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // s_client sends stdin once the handshake is done; give it a moment.
        std::thread::sleep(Duration::from_millis(1500));
        child.stdin.as_mut().unwrap().write_all(b"hello relay\n").unwrap();
        child.stdin.as_mut().unwrap().flush().unwrap();
        let (open, ems, got) = server.join().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut buf = vec![0u8; 256];
            let n = stdout.read(&mut buf).unwrap_or(0);
            buf.truncate(n);
            buf
        });
        std::thread::sleep(Duration::from_millis(500));
        let _ = child.kill();
        let echoed = reader.join().unwrap();
        assert!(open, "handshake with s_client");
        assert!(ems, "OpenSSL offers the extended master secret");
        assert_eq!(got, b"hello relay\n");
        assert_eq!(String::from_utf8_lossy(&echoed), "echo:hello relay\n");
    }

    #[test]
    fn plain_relay_messages_are_not_dtls() {
        assert!(!is_dtls(&[0xDA, 0x72, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(!is_dtls(&[22, 0xFE, 0xFD]));
    }
}
