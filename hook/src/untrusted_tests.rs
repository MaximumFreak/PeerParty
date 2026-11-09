//! Robustness checks for parsers that read bytes from the network (other LAN peers, or a
//! game's own traffic): hostile or cut-short input must be rejected, never panic. A panic on
//! a network thread silently stops that host; one inside an export aborts the game.

use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::photon::crypto::Aes256;
use crate::photon::message::{MAGIC, Proto, parse_hello};
use crate::photon::value::Rd;
use crate::photon::{enet, fusion, gp16, gp18, ws};
use crate::ugs::{dtls, relay, wire};

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

fn survives(name: &str, f: impl Fn(&[u8])) {
    let mut failed = Vec::new();
    for b in inputs() {
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
    survives("wire::parse_frame", |b| { let _ = wire::parse_frame(b); });
    survives("relay::parse_connection_data", |b| { let _ = relay::parse_connection_data(b); });
    survives("dtls records", |b| {
        let _ = dtls::is_dtls(b);
        for (_, _, _, frag) in dtls::records(b) {
            let _ = dtls::handshake_msgs(frag);
        }
        let _ = dtls::handshake_msgs(b);
    });
}