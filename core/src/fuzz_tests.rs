//! Randomized robustness tests for everything that parses bytes from the network: the
//! LAN wire messages and beacons, the DHT's bencode and replies, STUN responses, room
//! codes and public adverts. Each decoder gets valid messages mutated (bit flips,
//! truncation, insertions, lying lengths) and pure noise; none may panic, and whatever a
//! decoder accepts must survive a round trip.

use crate::inet::{crypto, dht, public, stun};
use crate::lobby::{Attr, Filter, Lobby, Member, Op, Query, Value};
use crate::wire::{Beacon, Msg, VERSION};
use crate::{JoinResult, PeerId};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }

    fn noise(&mut self, max: usize) -> Vec<u8> {
        let n = self.below(max);
        self.bytes(n)
    }

    /// A mutated copy of `v`.
    fn mutate(&mut self, v: &[u8]) -> Vec<u8> {
        let mut m = v.to_vec();
        for _ in 0..1 + self.below(4) {
            match self.below(6) {
                0 if !m.is_empty() => {
                    let i = self.below(m.len());
                    m[i] ^= 1 << self.below(8);
                }
                1 if !m.is_empty() => {
                    let n = self.below(m.len());
                    m.truncate(n);
                }
                2 => {
                    let i = self.below(m.len() + 1);
                    let n = 1 + self.below(8);
                    let extra = self.bytes(n);
                    m.splice(i..i, extra);
                }
                3 if m.len() >= 4 => {
                    // A length or count field that lies: 0xff.. or 0x7f..
                    let i = self.below(m.len() - 3);
                    let b = if self.next().is_multiple_of(2) { 0xff } else { 0x7f };
                    m[i..i + 4].copy_from_slice(&[b, b, b, b]);
                }
                4 if !m.is_empty() => {
                    let i = self.below(m.len());
                    m[i] = [0, 1, 0x7f, 0x80, 0xff][self.below(5)];
                }
                _ => {
                    let i = self.below(m.len() + 1);
                    m.truncate(i);
                    let extra = self.noise(16);
                    m.extend(extra);
                }
            }
        }
        m
    }
}

const ROUNDS: usize = 20_000;

fn sample_msgs() -> Vec<Msg> {
    let mut lobby = Lobby::new("eos.lobby");
    lobby.id = "lobby-1".into();
    lobby.members.push(Member {
        id: PeerId([7; 16]),
        attrs: vec![Attr { key: "k".into(), value: Value::Double(1.5), public: false }],
    });
    let mut query = Query::new("eos.lobby");
    query.member = Some(PeerId([3; 16]));
    query.filters = vec![
        Filter::Bucket { op: Op::Eq, value: "b".into() },
        Filter::MinSlots(2),
        Filter::Attr { key: "m".into(), op: Op::Contains, value: Value::Str("v".into()) },
    ];
    vec![
        Msg::Hello { version: VERSION, game: "eos:game".into(), id: PeerId([1; 16]), name: "Robin".into(), udp: 50000 },
        Msg::Search { search: 9, query },
        Msg::SearchReply { search: 9, lobbies: vec![lobby.clone(), lobby.clone()] },
        Msg::JoinReply { lobby: "x".into(), result: JoinResult::Full },
        Msg::State { lobby: lobby.clone() },
        Msg::ChanData { socket: "s".into(), channel: 3, reliable: true, data: vec![1, 2, 3, 4, 5] },
        Msg::Presence { kv: vec![("connect".into(), "abc".into()), ("status".into(), "".into())] },
        Msg::Invite { connect: "abc".into(), lobby: Some(lobby) },
        Msg::Leave { lobby: "abc".into() },
        Msg::Moved,
    ]
}

#[test]
fn wire_messages_survive_mutation_and_noise() {
    let mut r = Rng(0x1234_5678_9abc_def1);
    let seeds: Vec<Vec<u8>> = sample_msgs().iter().map(Msg::encode).collect();
    let mut accepted = 0;
    for i in 0..ROUNDS {
        let d = if i % 4 == 0 { r.noise(300) } else { let k = r.below(seeds.len()); r.mutate(&seeds[k]) };
        if let Some(m) = Msg::decode(&d) {
            accepted += 1;
            // Whatever we accept re-encodes to something we read back the same (compared
            // as bytes: a flipped bit can make a double NaN, which isn't equal to itself).
            let e = m.encode();
            assert_eq!(Msg::decode(&e).map(|m| m.encode()), Some(e), "{d:?}");
        }
    }
    // Some mutations (a flipped bit in a string) are still valid messages.
    assert!(accepted > 0);
}

#[test]
fn beacons_survive_mutation_and_noise() {
    let mut r = Rng(0x0bad_cafe_dead_beef);
    let seed = Beacon { game: "eos:abc".into(), id: PeerId([4; 16]), port: 5000 }.encode();
    for i in 0..ROUNDS {
        let d = if i % 3 == 0 { r.noise(200) } else { r.mutate(&seed) };
        if let Some(b) = Beacon::decode(&d) {
            assert_eq!(Beacon::decode(&b.encode()), Some(b));
        }
    }
}

#[test]
fn bencode_and_dht_replies_survive_mutation_and_noise() {
    let mut r = Rng(0x5eed_5eed_5eed_5eed);
    let nodes: Vec<u8> = (0..8).flat_map(|i| {
        let mut n = vec![i as u8; 20];
        n.extend_from_slice(&[203, 0, 113, i as u8, 0x1f, 0x90]);
        n
    }).collect();
    let reply = dht::dict(vec![
        ("t", dht::B::Bytes(b"aa".to_vec())),
        ("y", dht::B::Bytes(b"r".to_vec())),
        ("r", dht::dict(vec![
            ("id", dht::B::Bytes(vec![9; 20])),
            ("token", dht::B::Bytes(b"tok".to_vec())),
            ("nodes", dht::B::Bytes(nodes)),
            ("values", dht::B::List(vec![dht::B::Bytes(vec![198, 51, 100, 7, 0x13, 0x88]); 4])),
        ])),
    ])
    .to_vec();
    let query = dht::dict(vec![
        ("t", dht::B::Bytes(b"bb".to_vec())),
        ("y", dht::B::Bytes(b"q".to_vec())),
        ("q", dht::B::Bytes(b"get_peers".to_vec())),
        ("a", dht::dict(vec![("id", dht::B::Bytes(vec![1; 20])), ("info_hash", dht::B::Bytes(vec![2; 20]))])),
    ])
    .to_vec();
    let mut client = dht::Dht::new(vec!["127.0.0.1:9".parse().unwrap()], Vec::new());
    let mut node = dht::TestNode::default();
    let from = "127.0.0.1:6881".parse().unwrap();
    for i in 0..ROUNDS {
        let d = match i % 4 {
            0 => r.noise(400),
            1 => r.mutate(&query),
            _ => r.mutate(&reply),
        };
        if let Some(v) = dht::B::decode(&d) {
            assert_eq!(dht::B::decode(&v.to_vec()), Some(v));
        }
        client.handle(from, &d);
        let _ = node.answer(from, &d);
    }
    // Lengths and nesting that would overflow or recurse forever.
    for junk in [&b"18446744073709551615:x"[..], b"18446744073709551616:x", b"i99999999999999999999e", b"i-e", b"d18446744073709551615:"] {
        assert_eq!(dht::B::decode(junk), None, "{junk:?}");
    }
    let deep = "l".repeat(100_000);
    assert_eq!(dht::B::decode(deep.as_bytes()), None);
}

#[test]
fn stun_responses_survive_mutation_and_noise() {
    let mut r = Rng(0x57u64 << 40 | 0x0123);
    let txid = [5u8; 12];
    let cookie = [0x21u8, 0x12, 0xa4, 0x42];
    let mut attr = vec![0, 1];
    attr.extend_from_slice(&(54321u16 ^ 0x2112).to_be_bytes());
    for (b, k) in [203u8, 0, 113, 9].iter().zip(cookie) {
        attr.push(b ^ k);
    }
    let mut seed = Vec::new();
    seed.extend_from_slice(&0x0101u16.to_be_bytes());
    seed.extend_from_slice(&((4 + attr.len()) as u16).to_be_bytes());
    seed.extend_from_slice(&cookie);
    seed.extend_from_slice(&txid);
    seed.extend_from_slice(&0x0020u16.to_be_bytes());
    seed.extend_from_slice(&(attr.len() as u16).to_be_bytes());
    seed.extend_from_slice(&attr);
    assert!(stun::parse_response(&seed).is_some(), "the seed itself parses");
    for i in 0..ROUNDS {
        let d = if i % 3 == 0 { r.noise(120) } else { r.mutate(&seed) };
        let _ = stun::parse_response(&d);
        let _ = stun::is_stun(&d);
    }
}

#[test]
fn room_codes_and_adverts_survive_noise() {
    let mut r = Rng(0xc0de_c0de_c0de_c0de);
    let code = crate::inet::RoomCode::generate().to_string();
    for i in 0..ROUNDS {
        let s: String = if i % 2 == 0 {
            String::from_utf8_lossy(&r.mutate(code.as_bytes())).into_owned()
        } else {
            let n = r.below(40);
            (0..n).map(|_| char::from_u32(r.next() as u32 % 0x3000).unwrap_or('?')).collect()
        };
        if let Some(c) = crate::inet::RoomCode::parse(&s) {
            assert_eq!(crate::inet::RoomCode::parse(&c.to_string()).map(|c| c.room_id()), Some(c.room_id()));
        }
    }
    let key = public::IdentityKey::generate();
    let (room, a, b) = (crypto::random_array::<8>(), crypto::random_array::<48>(), crypto::random_array::<48>());
    let ad = public::advert(&key, &room, &a, &b, b"details");
    for i in 0..5_000 {
        let d = if i % 2 == 0 { r.noise(300) } else { r.mutate(&ad) };
        if d != ad {
            assert!(public::open_advert(&d, &room, &a, &b).is_none());
        }
        let _ = public::sanitize_name(&String::from_utf8_lossy(&d));
    }
}
