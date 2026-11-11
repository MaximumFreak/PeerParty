//! Photon Fusion's Shared mode server, the part of Photon Cloud's `FusionPlugin` that a
//! Shared mode room talks to as actor 0. In Shared mode every player is a Fusion client;
//! this keeps the one copy of every networked object, takes each object's updates from
//! the player with state authority over it, and sends every player everyone's objects,
//! with the players' join / leave messages, RPC forwarding, authority requests, scene
//! changes and the server's internal state object.
//!
//! The protocol is the one Fusion 2.x clients (Fusion.Sockets and Fusion.Runtime) speak
//! to the plugin. In short:
//! - packets are relayed through the room as event 101 to / from actor 0;
//! - Fusion.Sockets: a Connect / Accepted handshake, then "notify" packets with a 14-byte
//!   sequence / ack header, split into fragments above 432 bytes;
//! - a notify packet holds a state part (inputs, objects) and the reliable message list.
//!
//! Interest management isn't done: everyone gets every object.

use std::collections::{BTreeMap, HashMap, VecDeque};

use super::fusion::{BitReader, BitWriter};

// ------------------------------------------------------------------ bits

impl BitWriter {
    /// `WriteUInt64VarLength(v, block)`: a unary block count, then the blocks.
    pub fn varlen(&mut self, v: u64, block: usize) {
        let block = block.clamp(2, 16);
        let high = if v == 0 { 0 } else { 63 - v.leading_zeros() as usize };
        let k = (high + block) / block;
        self.bits(1 << (k - 1), k);
        self.bits(v, k * block);
    }
    /// `WriteUInt32VarLength(v)`: LEB128 at the current bit.
    pub fn leb(&mut self, mut v: u32) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.u8(b);
                return;
            }
            self.u8(b | 0x80);
        }
    }
    pub fn pad(&mut self) {
        while self.bit_len() % 8 != 0 {
            self.bool(false);
        }
    }
    pub fn aligned(&mut self, b: &[u8]) {
        self.pad();
        self.bytes(b);
    }
    pub fn f64(&mut self, v: f64) {
        self.u64(v.to_bits());
    }
    /// A PlayerRef from its raw value (index + 1, 0 = none).
    pub fn player(&mut self, raw: i32) {
        self.bool(raw > 0);
        if raw > 0 {
            self.leb((raw - 1) as u32);
        }
    }
}

impl BitReader<'_> {
    pub fn varlen(&mut self, block: usize) -> Option<u64> {
        let block = block.clamp(2, 16);
        let mut k = 1;
        while !self.bool()? {
            k += 1;
            if k * block > 64 {
                return None;
            }
        }
        self.bits(k * block)
    }
    pub fn leb(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for i in 0..5 {
            let b = self.u8()?;
            v |= u32::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    pub fn align(&mut self) {
        self.seek((self.pos() + 7) & !7);
    }
    pub fn aligned(&mut self, n: usize) -> Option<Vec<u8>> {
        self.align();
        self.bytes(n)
    }
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn f64(&mut self) -> Option<f64> {
        self.u64().map(f64::from_bits)
    }
    /// A PlayerRef, as its raw value.
    pub fn player(&mut self) -> Option<i32> {
        Some(if self.bool()? { self.leb()? as i32 + 1 } else { 0 })
    }
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    (v >> 1) as i64 ^ -((v & 1) as i64)
}

/// `NetSequencer.Distance` for 16-bit sequences.
fn distance(from: u16, to: u16) -> i32 {
    i32::from(from.wrapping_sub(to) as i16)
}

// ------------------------------------------------------------------ packets

mod packet {
    pub const COMMAND: u8 = 1;
    pub const NOTIFY_DATA: u8 = 3;
    pub const NOTIFY_ACKS: u8 = 4;
    pub const NOTIFY_RELIABLE: u8 = 8;
    pub const CONNECT: u8 = 1;
    pub const ACCEPTED: u8 = 2;
    pub const REFUSED: u8 = 3;
    pub const DISCONNECT: u8 = 4;
    pub const HEADER: usize = 14;
    /// Fusion.Sockets' smallest MTU; bigger notify packets go in fragments.
    pub const MTU: usize = 432;
    pub const SERVER_FULL: u8 = 2;
    pub const DISCONNECT_REQUESTED: u8 = 3;
}

/// The 14-byte notify header.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Notify {
    kind: u8,
    fragment: u8,
    seq: u16,
    ack: u16,
    mask: u64,
}

impl Notify {
    fn read(b: &[u8]) -> Option<Notify> {
        (b.len() >= packet::HEADER).then(|| Notify {
            kind: b[0],
            fragment: b[1],
            seq: u16::from_le_bytes([b[2], b[3]]),
            ack: u16::from_le_bytes([b[4], b[5]]),
            mask: u64::from_le_bytes(b[6..14].try_into().unwrap()),
        })
    }
    fn bytes(&self) -> [u8; packet::HEADER] {
        let mut h = [0u8; packet::HEADER];
        h[0] = self.kind;
        h[1] = self.fragment;
        h[2..4].copy_from_slice(&self.seq.to_le_bytes());
        h[4..6].copy_from_slice(&self.ack.to_le_bytes());
        h[6..14].copy_from_slice(&self.mask.to_le_bytes());
        h
    }
}

// ------------------------------------------------------------------ objects and messages

/// Header words of a NetworkObjectHeader.
mod word {
    pub const ID: usize = 0;
    pub const TYPE: usize = 1;
    pub const NESTING_ROOT: usize = 2;
    pub const NESTING_KEY: usize = 3;
    pub const SCENE_GUID: usize = 4;
    pub const WORD_COUNT: usize = 8;
    pub const TRANSFORM_OFFSET: usize = 9;
    pub const FLAGS: usize = 10;
    /// Words from here travel as deltas; the ones before only in the create header.
    pub const FIRST_DELTA: usize = 11;
    pub const STATE_AUTHORITY: usize = 13;
    pub const HEADER: usize = 20;
}

mod flags {
    pub const DESTROY_WHEN_STATE_AUTHORITY_LEAVES: i32 = 2;
    pub const ALLOW_STATE_AUTHORITY_OVERRIDE: i32 = 0x10;
    pub const NO_PREFAB: i32 = 0x20;
}

/// The server's internal state object (NetworkId.InternalState).
const INTERNAL_STATE: u32 = 1;
/// Objects written per packet stop past this (PacketSizeLimitInBits).
const OBJECT_BITS: usize = 32768;
/// Messages stop past this many bits in a packet.
const MESSAGE_BITS: usize = 9088;
const TIMEOUT_MS: i64 = 10_000;
/// Limits on what clients may have this server keep: an object's size in words, and a
/// room's objects and their words in all (far past what a packet can carry updates for).
const OBJECT_WORDS: usize = 1 << 16;
const ROOM_OBJECTS: usize = 8192;
const ROOM_WORDS: usize = 1 << 21;

struct Object {
    words: Vec<i32>,
    /// The server tick each word last changed at (0 = never).
    changed: Vec<i32>,
}

impl Object {
    fn new(words: Vec<i32>) -> Object {
        let n = words.len();
        Object { words, changed: vec![0; n] }
    }
    fn authority(&self) -> i32 {
        self.words.get(word::STATE_AUTHORITY).copied().unwrap_or(0)
    }
    fn flags(&self) -> i32 {
        self.words.get(word::FLAGS).copied().unwrap_or(0)
    }
    fn set(&mut self, i: usize, v: i32, tick: i32) {
        if i < self.words.len() && self.words[i] != v {
            self.words[i] = v;
            self.changed[i] = tick;
        }
    }
}

/// msg flags (SimulationMessage.FLAG_*).
mod msg {
    pub const USER: u32 = 1;
    pub const REMOTE: u32 = 2;
    pub const UNRELIABLE: u32 = 8;
    pub const TARGET_PLAYER: u32 = 16;
    pub const TARGET_SERVER: u32 = 32;
    pub const INTERNAL: u32 = 64;
    pub const SCENE_LOAD: i32 = 1;
    pub const SCENE_OBJECT_DATA: i32 = 2;
    pub const REQUEST_STATE_AUTHORITY: i32 = 4;
    pub const PLAYER_JOINED_LEFT: i32 = 5;
    pub const SET_PLAYER_OBJECT: i32 = 6;
}

#[derive(Clone, Debug, PartialEq)]
struct Message {
    /// Length of `data` in bits (SimulationMessage.Offset).
    bits: u32,
    tick: i32,
    source: i32,
    target: i32,
    flags: u32,
    seq: u64,
    data: Vec<u8>,
}

impl Message {
    fn read(r: &mut BitReader) -> Option<Message> {
        let bits = r.varlen(10)? as u32;
        let tick = r.varlen(16)? as i32;
        let source = r.player()?;
        let target = r.player()?;
        let flags = if r.bool()? { r.bits(16)? as u32 } else { 0 };
        if flags & msg::USER != 0 {
            // A user-flags bit; Fusion reads no value after it (`ReadInt32(-12)`).
            r.bool()?;
        }
        let seq = if flags & msg::UNRELIABLE == 0 { r.varlen(16)? } else { 0 };
        let data = if bits > 0 { r.aligned(bits.div_ceil(8) as usize)? } else { Vec::new() };
        Some(Message { bits, tick, source, target, flags, seq, data })
    }
    fn write(&self, w: &mut BitWriter) {
        w.varlen(u64::from(self.bits), 10);
        w.varlen(self.tick as u32 as u64, 16);
        w.player(self.source);
        w.player(self.target);
        let f = self.flags & !msg::REMOTE & 0xffff;
        w.bool(f != 0);
        if f != 0 {
            w.bits(u64::from(f), 16);
            if f & msg::USER != 0 {
                w.bool(false);
            }
        }
        if self.flags & msg::UNRELIABLE == 0 {
            w.varlen(self.seq, 16);
        }
        if self.bits > 0 {
            w.aligned(&self.data);
        }
    }
    fn internal(kind: i32, body: &[i32], tick: i32, target: Option<i32>) -> Message {
        let mut data = kind.to_le_bytes().to_vec();
        for v in body {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let mut flags = msg::INTERNAL;
        if target.is_some() {
            flags |= msg::TARGET_PLAYER;
        }
        Message { bits: data.len() as u32 * 8, tick, source: 0, target: 0, flags, seq: 0, data }
    }
    fn internal_kind(&self) -> Option<i32> {
        (self.flags & msg::INTERNAL != 0 && self.data.len() >= 4).then(|| i32::from_le_bytes(self.data[..4].try_into().unwrap()))
    }
    fn i32_at(&self, at: usize) -> Option<i32> {
        self.data.get(at..at + 4).map(|b| i32::from_le_bytes(b.try_into().unwrap()))
    }
}

// ------------------------------------------------------------------ connections

/// An exponential moving average with a deviation, like Fusion's Ema.
#[derive(Clone, Copy, Default)]
struct Ema {
    val: f64,
    dev: f64,
    started: bool,
}

impl Ema {
    fn add(&mut self, x: f64) {
        if !self.started {
            *self = Ema { val: x, dev: 0.0, started: true };
            return;
        }
        const A: f64 = 0.1;
        self.dev += ((x - self.val).abs() - self.dev) * A;
        self.val += (x - self.val) * A;
    }
}

#[derive(Clone, Copy)]
struct Sent {
    tick: i32,
    create_unconfirmed: bool,
}

/// What one sent packet carried, for when it's acknowledged or lost.
struct Envelope {
    seq: u16,
    objects: Vec<(u32, i32)>,
    destroys: Vec<u32>,
    messages: Vec<Message>,
    globals: [i32; 32],
}

struct Conn {
    player: usize,
    local_id: u64,
    remote_id: u64,
    counter: u32,
    last_recv_ms: i64,
    // notify layer
    send_seq: u16,
    recv_seq: u16,
    recv_mask: u64,
    window: VecDeque<Envelope>,
    fragments: Option<(u16, u8, Vec<u8>)>,
    // simulation
    objects: HashMap<u32, Sent>,
    destroys: Vec<u32>,
    acked_globals: [i32; 32],
    out: VecDeque<Message>,
    out_seq: u64,
    in_seq: u64,
    in_waiting: BTreeMap<u64, Message>,
    input_tick: i32,
    input_offset: Ema,
    packet_interval: Ema,
    last_packet_ms: Option<i64>,
}

impl Conn {
    fn raw_player(&self) -> i32 {
        self.player as i32 + 1
    }
    fn queue(&mut self, mut m: Message) {
        if m.flags & msg::UNRELIABLE == 0 {
            self.out_seq += 1;
            m.seq = self.out_seq;
        }
        self.out.push_back(m);
    }
}

/// Packets out: (actor, bytes), each to go as event 101 from actor 0.
pub type Out = Vec<(i32, Vec<u8>)>;

/// One Shared mode session's server.
pub struct Server {
    tick_rate: u32,
    players: usize,
    started_ms: Option<i64>,
    tick: i32,
    conns: HashMap<i32, Conn>,
    counter: u32,
    generation: u32,
    objects: BTreeMap<u32, Object>,
    globals: [i32; 32],
    last_status_ms: i64,
    pub log: Vec<String>,
}

impl Server {
    pub fn new(tick_rate: u32, players: usize) -> Server {
        let players = players.clamp(1, 255);
        let mut globals = [0i32; 32];
        globals[1] = 1; // ServerMode: Server
        globals[2] = players as i32;
        Server {
            tick_rate: tick_rate.clamp(1, 1000),
            players,
            started_ms: None,
            tick: 0,
            conns: HashMap::new(),
            counter: 0,
            generation: 0x2f3a,
            objects: BTreeMap::new(),
            globals,
            last_status_ms: 0,
            log: Vec::new(),
        }
    }

    pub fn players(&self) -> usize {
        self.conns.len()
    }

    fn tick_ms(&self) -> f64 {
        1000.0 / f64::from(self.tick_rate)
    }

    /// When the next server tick is due.
    pub fn next_tick(&self, now: i64) -> i64 {
        match self.started_ms {
            Some(s) if !self.conns.is_empty() => s + (f64::from(self.tick) * self.tick_ms()).ceil() as i64,
            _ => now + 250,
        }
    }

    /// A relayed packet from `actor`.
    pub fn packet(&mut self, actor: i32, data: &[u8], now: i64, master: i32) -> Out {
        let mut out = Out::new();
        let Some(&kind) = data.first() else { return out };
        if kind == packet::COMMAND {
            self.command(actor, data, now, &mut out);
            return out;
        }
        if !self.conns.contains_key(&actor) {
            return out;
        }
        self.conns.get_mut(&actor).unwrap().last_recv_ms = now;
        match kind {
            packet::NOTIFY_DATA | packet::NOTIFY_RELIABLE => self.notify(actor, data, now, master, &mut out),
            packet::NOTIFY_ACKS => {
                if let Some(h) = Notify::read(data) {
                    self.acks(actor, h, now);
                }
            }
            _ => {}
        }
        out
    }

    fn command(&mut self, actor: i32, data: &[u8], now: i64, out: &mut Out) {
        match data.get(1).copied() {
            Some(packet::CONNECT) if data.len() >= 16 => {
                let remote = u64::from_le_bytes(data[8..16].try_into().unwrap());
                if let Some(c) = self.conns.get(&actor) {
                    out.push((actor, accepted(c.local_id, c.remote_id, c.counter)));
                    return;
                }
                let used: Vec<usize> = self.conns.values().map(|c| c.player).collect();
                let Some(player) = (0..self.players).find(|p| !used.contains(p)) else {
                    self.log.push(format!("photon: Fusion Shared session is full ({}); refusing actor {actor}", self.players));
                    out.push((actor, vec![packet::COMMAND, packet::REFUSED, packet::SERVER_FULL]));
                    return;
                };
                self.counter += 1;
                self.generation = self.generation.wrapping_mul(1_103_515_245).wrapping_add(12345) | 1;
                let local_id = (player as u64) << 16 | u64::from(self.generation) << 32;
                let c = Conn {
                    player,
                    local_id,
                    remote_id: remote,
                    counter: self.counter,
                    last_recv_ms: now,
                    send_seq: 0,
                    recv_seq: 0,
                    recv_mask: 0,
                    window: VecDeque::new(),
                    fragments: None,
                    objects: HashMap::new(),
                    destroys: Vec::new(),
                    acked_globals: [0; 32],
                    out: VecDeque::new(),
                    out_seq: 0,
                    in_seq: 0,
                    in_waiting: BTreeMap::new(),
                    input_tick: 0,
                    input_offset: Ema::default(),
                    packet_interval: Ema::default(),
                    last_packet_ms: None,
                };
                out.push((actor, accepted(c.local_id, c.remote_id, c.counter)));
                self.conns.insert(actor, c);
                self.joined(actor, now);
            }
            Some(packet::DISCONNECT) => {
                // Fusion.Sockets' reasons: Unknown 1, Timeout 2, Requested 3,
                // SequenceOutOfBounds 4, SendWindowFull 5, ByRemote 6.
                let reason = data.get(2).copied().unwrap_or(0);
                self.log.push(format!("photon: Fusion Shared actor {actor} disconnects (Fusion reason {reason})"));
                if self.conns.contains_key(&actor) {
                    self.left(actor);
                }
            }
            _ => {}
        }
    }

    fn joined(&mut self, actor: i32, now: i64) {
        if self.started_ms.is_none() {
            self.started_ms = Some(now);
            self.tick = 1;
        }
        let player = self.conns[&actor].player;
        let raw = player as i32 + 1;
        self.log.push(format!("photon: Fusion Shared player {player} is actor {actor}"));
        self.globals[4 + player / 32] |= 1 << (player % 32);
        let words = word::HEADER + 2 * self.players;
        let players = self.players;
        let tick = self.tick + 1;
        let state = self.objects.entry(INTERNAL_STATE).or_insert_with(|| {
            let mut w = vec![0; words];
            w[word::ID] = INTERNAL_STATE as i32;
            w[word::WORD_COUNT] = words as i32;
            w[word::FLAGS] = flags::NO_PREFAB;
            Object::new(w)
        });
        state.set(word::HEADER + players + player, actor + 1, tick);
        // The newcomer hears of everyone already here, then everyone of the newcomer.
        let others: Vec<i32> = self.conns.iter().filter(|(a, _)| **a != actor).map(|(_, c)| c.raw_player()).collect();
        let t = self.tick;
        let c = self.conns.get_mut(&actor).unwrap();
        for o in others {
            c.queue(Message::internal(msg::PLAYER_JOINED_LEFT, &[o, 1], t, Some(raw)));
        }
        for c in self.conns.values_mut() {
            c.queue(Message::internal(msg::PLAYER_JOINED_LEFT, &[raw, 1], t, None));
        }
    }

    /// `actor` left (its Disconnect, a timeout, or it left the room).
    pub fn left(&mut self, actor: i32) {
        let Some(c) = self.conns.remove(&actor) else { return };
        let raw = c.raw_player();
        self.log.push(format!("photon: Fusion Shared player {} (actor {actor}) left", c.player));
        self.globals[4 + c.player / 32] &= !(1 << (c.player % 32));
        let tick = self.tick + 1;
        let players = self.players;
        if let Some(s) = self.objects.get_mut(&INTERNAL_STATE) {
            s.set(word::HEADER + c.player, 0, tick);
            s.set(word::HEADER + players + c.player, 0, tick);
        }
        let owned: Vec<u32> = self.objects.iter().filter(|(_, o)| o.authority() == raw).map(|(id, _)| *id).collect();
        for id in owned {
            if self.objects[&id].flags() & flags::DESTROY_WHEN_STATE_AUTHORITY_LEAVES != 0 {
                self.destroy(id, None);
            } else if let Some(o) = self.objects.get_mut(&id) {
                o.set(word::STATE_AUTHORITY, 0, tick);
            }
        }
        let t = self.tick;
        for c in self.conns.values_mut() {
            c.queue(Message::internal(msg::PLAYER_JOINED_LEFT, &[raw, 0], t, None));
        }
        if self.conns.is_empty() {
            self.started_ms = None;
        }
    }

    /// Whether a new object of `words` words may be kept.
    fn room_for(&self, words: usize) -> bool {
        (word::HEADER..=OBJECT_WORDS).contains(&words)
            && self.objects.len() < ROOM_OBJECTS
            && self.objects.values().map(|o| o.words.len()).sum::<usize>() + words <= ROOM_WORDS
    }

    fn destroy(&mut self, id: u32, by: Option<i32>) {
        if self.objects.remove(&id).is_none() {
            return;
        }
        for (a, c) in self.conns.iter_mut() {
            if c.objects.remove(&id).is_some() && Some(*a) != by {
                c.destroys.push(id);
            }
        }
    }

    // -------------------------------------------------------------- notify layer

    fn notify(&mut self, actor: i32, data: &[u8], now: i64, master: i32, out: &mut Out) {
        let Some(h) = Notify::read(data) else { return };
        if data.len() <= packet::HEADER {
            return;
        }
        let c = self.conns.get_mut(&actor).unwrap();
        let d = distance(h.seq, c.recv_seq);
        if d <= 0 || d > 1024 {
            return;
        }
        let whole = if h.fragment != 0 {
            let index = h.fragment & 0x7f;
            if index == 1 {
                c.fragments = Some((h.seq, 1, data.to_vec()));
            } else if let Some((seq, last, buf)) = c.fragments.as_mut() {
                if *seq == h.seq && *last + 1 == index {
                    *last = index;
                    buf.extend_from_slice(&data[packet::HEADER..]);
                } else {
                    c.fragments = None;
                }
            }
            if h.fragment & 0x80 == 0 {
                return;
            }
            match c.fragments.take() {
                Some((seq, _, buf)) if seq == h.seq => buf,
                _ => return,
            }
        } else {
            c.fragments = None;
            data.to_vec()
        };
        c.recv_seq = h.seq;
        c.recv_mask = if d >= 64 { 1 } else { (c.recv_mask << d) | 1 };
        self.acks(actor, h, now);
        if h.kind == packet::NOTIFY_RELIABLE {
            // Reliable data (SendReliableDataToPlayer / ToServer) isn't served.
            return;
        }
        self.body(actor, &whole[packet::HEADER..], now, master, out);
    }

    fn acks(&mut self, actor: i32, h: Notify, _now: i64) {
        let Some(c) = self.conns.get_mut(&actor) else { return };
        let mut delivered = Vec::new();
        let mut lost = Vec::new();
        while let Some(e) = c.window.front() {
            let d = distance(e.seq, h.ack);
            if d > 0 {
                break;
            }
            let e = c.window.pop_front().unwrap();
            if d <= -64 || h.mask >> (-d) & 1 == 0 {
                lost.push(e);
            } else {
                delivered.push(e);
            }
        }
        for e in delivered {
            c.acked_globals = e.globals;
            for (id, _) in e.objects {
                if let Some(s) = c.objects.get_mut(&id) {
                    s.create_unconfirmed = false;
                }
            }
        }
        for e in lost {
            for (id, reset) in e.objects {
                if let Some(s) = c.objects.get_mut(&id) {
                    s.tick = reset;
                }
            }
            for id in e.destroys {
                if !self.objects.contains_key(&id) {
                    c.destroys.push(id);
                }
            }
            for m in e.messages.into_iter().rev() {
                c.out.push_front(m);
            }
        }
    }

    // -------------------------------------------------------------- what clients send

    fn body(&mut self, actor: i32, body: &[u8], now: i64, master: i32, out: &mut Out) {
        let mut r = BitReader::new(body);
        let Some(has_state) = r.bool() else { return };
        if has_state && self.state(actor, &mut r, now).is_none() {
            self.log.push(format!("photon: Fusion Shared packet from actor {actor} didn't parse"));
            return;
        }
        r.align();
        let Some(count) = r.u16() else { return };
        let mut got = Vec::new();
        for _ in 0..count {
            match Message::read(&mut r) {
                Some(m) => got.push(m),
                None => return,
            }
        }
        for m in got {
            self.incoming(actor, m, master, out);
        }
    }

    fn state(&mut self, actor: i32, r: &mut BitReader, now: i64) -> Option<()> {
        // Inputs: the ticks the client simulated.
        let n = r.u32()? as i32;
        let mut ticks = Vec::new();
        for _ in 0..n.clamp(0, 1024) {
            ticks.push(r.varlen(8)? as i32);
        }
        let aoi = r.u8()?;
        for _ in 0..aoi {
            r.bits(32 * 5)?;
        }
        let raw = self.conns[&actor].raw_player();
        // Destroys.
        let mut destroys = Vec::new();
        if r.bool()? {
            let n = r.varlen(8)?;
            for _ in 0..n.min(4096) {
                destroys.push(r.varlen(8)? as u32);
            }
        }
        // Objects the client has state authority over.
        let tick = self.tick + 1;
        while r.bool()? {
            let id = r.varlen(8)? as u32;
            let mut create = None;
            if r.bool()? {
                let ty = r.varlen(8)? as i32;
                let count = r.varlen(8)? as usize;
                let transform = r.varlen(8)? as i32;
                let fl = r.varlen(8)? as i32;
                let (root, key) = if r.bool()? { (r.varlen(8)? as i32, r.varlen(8)? as i32) } else { (0, 0) };
                let guid = if r.bool()? { Some(r.aligned(16)?) } else { None };
                create = Some((ty, count, transform, fl, root, key, guid));
            }
            let mut words = Vec::new();
            let mut at = word::FIRST_DELTA;
            while r.bool()? {
                at += r.varlen(4)? as usize;
                words.push((at, unzigzag(r.varlen(6)?) as i32));
            }
            if id == INTERNAL_STATE {
                continue;
            }
            if !self.objects.contains_key(&id) {
                let Some((ty, count, transform, fl, root, key, guid)) = create else { continue };
                // The client says how big its object is, and each word is kept twice (its
                // value and when it changed): a short packet naming many objects of the
                // biggest size took hundreds of megabytes.
                if !self.room_for(count) {
                    continue;
                }
                let mut w = vec![0; count];
                w[word::ID] = id as i32;
                w[word::TYPE] = ty;
                w[word::NESTING_ROOT] = root;
                w[word::NESTING_KEY] = key;
                if let Some(g) = guid {
                    for (i, chunk) in g.chunks(4).enumerate() {
                        w[word::SCENE_GUID + i] = i32::from_le_bytes(chunk.try_into().unwrap());
                    }
                }
                w[word::WORD_COUNT] = count as i32;
                w[word::TRANSFORM_OFFSET] = transform;
                w[word::FLAGS] = fl;
                let mut o = Object::new(w);
                // The creator holds state authority until it says otherwise.
                o.set(word::STATE_AUTHORITY, raw, tick);
                self.objects.insert(id, o);
            }
            let o = self.objects.get_mut(&id).unwrap();
            if o.authority() != raw {
                continue;
            }
            for (i, v) in words {
                if i != word::STATE_AUTHORITY {
                    o.set(i, v, tick);
                }
            }
        }
        for id in destroys {
            if self.objects.get(&id).is_some_and(|o| o.authority() == raw || o.authority() == 0) {
                self.destroy(id, Some(actor));
            }
        }
        // Timing feedback.
        let started = self.started_ms.unwrap_or(now);
        let tick_ms = self.tick_ms();
        let c = self.conns.get_mut(&actor).unwrap();
        for t in ticks {
            if t > c.input_tick {
                c.input_tick = t;
                let due = started as f64 + f64::from(t - 1) * tick_ms;
                c.input_offset.add((due - now as f64) / 1000.0);
            }
        }
        if let Some(last) = c.last_packet_ms
            && now - last >= 10
        {
            c.packet_interval.add((now - last) as f64 / 1000.0);
        }
        if c.last_packet_ms.is_none_or(|l| now - l >= 10) {
            c.last_packet_ms = Some(now);
        }
        Some(())
    }

    fn incoming(&mut self, actor: i32, mut m: Message, master: i32, out: &mut Out) {
        let c = self.conns.get_mut(&actor).unwrap();
        m.source = c.raw_player();
        if m.flags & msg::UNRELIABLE != 0 {
            self.route(actor, m, master, out);
            return;
        }
        if m.seq <= c.in_seq {
            return;
        }
        c.in_waiting.insert(m.seq, m);
        let mut ready = Vec::new();
        while let Some(m) = c.in_waiting.remove(&(c.in_seq + 1)) {
            c.in_seq += 1;
            ready.push(m);
        }
        for m in ready {
            self.route(actor, m, master, out);
        }
    }

    fn route(&mut self, actor: i32, m: Message, master: i32, _out: &mut Out) {
        if let Some(kind) = m.internal_kind() {
            if m.flags & msg::TARGET_SERVER != 0 {
                self.internal(actor, kind, &m, master);
            }
            return;
        }
        if m.flags & msg::TARGET_SERVER != 0 {
            return;
        }
        if m.flags & msg::TARGET_PLAYER != 0 {
            let target = m.target;
            if let Some(c) = self.conns.values_mut().find(|c| c.raw_player() == target) {
                let mut f = m;
                f.target = 0;
                c.queue(f);
            }
            return;
        }
        for (a, c) in self.conns.iter_mut() {
            if *a != actor {
                c.queue(m.clone());
            }
        }
    }

    fn internal(&mut self, actor: i32, kind: i32, m: &Message, master: i32) {
        let raw = self.conns[&actor].raw_player();
        let player = self.conns[&actor].player;
        let tick = self.tick + 1;
        match kind {
            msg::SCENE_LOAD => {
                if let Some(scene) = m.i32_at(4) {
                    self.log.push(format!("photon: Fusion Shared scene {scene} (from actor {actor})"));
                    self.globals[0] = scene;
                }
            }
            msg::SCENE_OBJECT_DATA if actor == master || master == 0 => {
                let body = &m.data[4.min(m.data.len())..];
                let words: Vec<i32> = body.chunks_exact(4).map(|b| i32::from_le_bytes(b.try_into().unwrap())).collect();
                if words.len() < word::HEADER || words[word::WORD_COUNT] as usize != words.len() {
                    return;
                }
                let id = words[word::ID] as u32;
                if id == INTERNAL_STATE {
                    return;
                }
                if !self.objects.contains_key(&id) && !self.room_for(words.len()) {
                    return;
                }
                let o = self.objects.entry(id).or_insert_with(|| Object::new(vec![0; words.len()]));
                for (i, v) in words.into_iter().enumerate() {
                    if i < word::FIRST_DELTA {
                        // An object there already may be shorter than this one says.
                        if let Some(w) = o.words.get_mut(i) {
                            *w = v;
                        }
                    } else {
                        o.set(i, v, tick);
                    }
                }
            }
            msg::REQUEST_STATE_AUTHORITY => {
                let (Some(id), Some(acquire)) = (m.i32_at(4), m.i32_at(8)) else { return };
                let Some(o) = self.objects.get_mut(&(id as u32)) else { return };
                let now = o.authority();
                if acquire != 0 {
                    if now == 0 || now == raw || o.flags() & flags::ALLOW_STATE_AUTHORITY_OVERRIDE != 0 {
                        o.set(word::STATE_AUTHORITY, raw, tick);
                    }
                } else if now == raw {
                    o.set(word::STATE_AUTHORITY, 0, tick);
                }
            }
            msg::SET_PLAYER_OBJECT => {
                if let (Some(id), Some(s)) = (m.i32_at(4), self.objects.get_mut(&INTERNAL_STATE)) {
                    s.set(word::HEADER + player, id, tick);
                }
            }
            _ => {}
        }
    }

    // -------------------------------------------------------------- what the server sends

    /// Timers: the server tick (state to everyone) and timeouts.
    pub fn tick(&mut self, now: i64) -> Out {
        let mut out = Out::new();
        let dead: Vec<i32> = self.conns.iter().filter(|(_, c)| now - c.last_recv_ms > TIMEOUT_MS).map(|(a, _)| *a).collect();
        for a in dead {
            self.log.push(format!("photon: Fusion Shared actor {a} timed out"));
            out.push((a, vec![packet::COMMAND, packet::DISCONNECT, packet::DISCONNECT_REQUESTED]));
            self.left(a);
        }
        let Some(started) = self.started_ms else { return out };
        let due = ((now - started) as f64 / self.tick_ms()).floor() as i32 + 1;
        if due <= self.tick {
            return out;
        }
        self.tick = due;
        if now - self.last_status_ms >= 10_000 {
            self.last_status_ms = now;
            let mut who: Vec<String> = self.conns.iter().map(|(a, c)| format!("{} (actor {a})", c.player)).collect();
            who.sort();
            self.log.push(format!(
                "photon: Fusion Shared tick {}: players {}, {} objects, scene {}",
                self.tick,
                who.join(", "),
                self.objects.len(),
                self.globals[0]
            ));
        }
        let actors: Vec<i32> = self.conns.keys().copied().collect();
        for a in actors {
            self.send_state(a, &mut out);
        }
        out
    }

    fn send_state(&mut self, actor: i32, out: &mut Out) {
        let tick = self.tick;
        let globals = self.globals;
        let c = self.conns.get_mut(&actor).unwrap();
        let mut w = BitWriter::default();
        w.bool(true);
        w.f64(c.input_offset.val);
        w.f64(c.input_offset.dev);
        w.f64(c.packet_interval.val);
        w.f64(c.packet_interval.dev);
        w.leb(tick as u32);
        for i in 0..32 {
            let changed = globals[i] != c.acked_globals[i];
            w.bool(changed);
            if changed {
                w.varlen(globals[i] as u32 as u64, 8);
            }
        }
        let destroys: Vec<u32> = std::mem::take(&mut c.destroys);
        w.bool(!destroys.is_empty());
        if !destroys.is_empty() {
            w.varlen(destroys.len() as u64, 8);
            for id in &destroys {
                w.varlen(u64::from(*id), 8);
            }
        }
        let mut sent = Vec::new();
        for (id, o) in self.objects.iter() {
            if w.bit_len() >= OBJECT_BITS {
                break;
            }
            let s = c.objects.entry(*id).or_insert(Sent { tick: 0, create_unconfirmed: true });
            if s.tick == tick {
                continue;
            }
            let start = w.bit_len();
            let mut since = s.tick;
            w.bool(true);
            w.varlen(u64::from(*id), 8);
            w.bool(s.create_unconfirmed);
            if s.create_unconfirmed {
                let ow = &o.words;
                w.varlen(ow[word::TYPE] as u32 as u64, 8);
                w.varlen(ow[word::WORD_COUNT] as u32 as u64, 8);
                w.varlen(ow[word::TRANSFORM_OFFSET] as u32 as u64, 8);
                w.varlen(ow[word::FLAGS] as u32 as u64, 8);
                w.bool(ow[word::NESTING_ROOT] != 0);
                if ow[word::NESTING_ROOT] != 0 {
                    w.varlen(ow[word::NESTING_ROOT] as u32 as u64, 8);
                    w.varlen(ow[word::NESTING_KEY] as u32 as u64, 8);
                }
                let guid = &ow[word::SCENE_GUID..word::SCENE_GUID + 4];
                w.bool(guid.iter().any(|v| *v != 0));
                if guid.iter().any(|v| *v != 0) {
                    let b: Vec<u8> = guid.iter().flat_map(|v| v.to_le_bytes()).collect();
                    w.aligned(&b);
                }
                if o.changed[word::FIRST_DELTA..].iter().any(|t| *t > since) {
                    since = 0;
                }
            }
            let mut prev = word::FIRST_DELTA;
            let mut wrote = false;
            for i in word::FIRST_DELTA..o.words.len() {
                if o.changed[i] > since {
                    w.bool(true);
                    w.varlen((i - prev) as u64, 4);
                    w.varlen(zigzag(i64::from(o.words[i])), 6);
                    prev = i;
                    wrote = true;
                }
            }
            if !wrote || prev == word::FIRST_DELTA {
                // Nothing (Fusion drops an object whose only word is the first one too).
                truncate(&mut w, start);
                s.tick = tick;
                continue;
            }
            w.bool(false);
            sent.push((*id, s.tick));
            s.tick = tick;
        }
        w.bool(false);
        // Messages.
        w.pad();
        let count_at = w.bit_len();
        w.u16(0);
        let mut messages = Vec::new();
        while let Some(m) = c.out.front() {
            let mut probe = BitWriter::default();
            m.write(&mut probe);
            if !messages.is_empty() && w.bit_len() + probe.bit_len() > count_at + MESSAGE_BITS {
                break;
            }
            let m = c.out.pop_front().unwrap();
            m.write(&mut w);
            messages.push(m);
        }
        let n = messages.len() as u16;
        w.data[count_at / 8] = n as u8;
        w.data[count_at / 8 + 1] = (n >> 8) as u8;
        w.pad();
        messages.retain(|m| m.flags & msg::UNRELIABLE == 0);
        c.send_seq = c.send_seq.wrapping_add(1);
        let h = Notify { kind: packet::NOTIFY_DATA, fragment: 0, seq: c.send_seq, ack: c.recv_seq, mask: c.recv_mask };
        c.window.push_back(Envelope { seq: c.send_seq, objects: sent, destroys, messages, globals });
        while c.window.len() > 1024 {
            c.window.pop_front();
        }
        let mut bytes = h.bytes().to_vec();
        bytes.extend_from_slice(&w.data);
        for p in fragments(&bytes) {
            out.push((actor, p));
        }
    }
}

/// Drops everything written after bit `at`.
fn truncate(w: &mut BitWriter, at: usize) {
    let mut keep = BitWriter::default();
    let mut r = BitReader::new(&w.data);
    let mut left = at;
    while left > 0 {
        let n = left.min(64);
        keep.bits(r.bits(n).unwrap(), n);
        left -= n;
    }
    *w = keep;
}

/// Accepted (NetCommandAccepted, 32 bytes with its padding).
fn accepted(local: u64, remote: u64, counter: u32) -> Vec<u8> {
    let mut b = vec![0u8; 32];
    b[0] = packet::COMMAND;
    b[1] = packet::ACCEPTED;
    b[8..16].copy_from_slice(&local.to_le_bytes());
    b[16..24].copy_from_slice(&remote.to_le_bytes());
    b[24..28].copy_from_slice(&counter.to_le_bytes());
    b
}

/// A notify packet as sent: whole, or in MTU-sized fragments.
fn fragments(p: &[u8]) -> Vec<Vec<u8>> {
    if p.len() <= packet::MTU {
        return vec![p.to_vec()];
    }
    let body = &p[packet::HEADER..];
    let chunks: Vec<&[u8]> = body.chunks(packet::MTU - packet::HEADER).collect();
    let n = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let mut f = p[..packet::HEADER].to_vec();
            f[1] = (i + 1) as u8 | if i + 1 == n { 0x80 } else { 0 };
            f.extend_from_slice(c);
            f
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Fusion client as far as the tests need: the handshake, notify headers, and its
    /// side of the state and message encoding.
    struct Client {
        actor: i32,
        id: u64,
        player: i32,
        counter: u32,
        seq: u16,
        recv: u16,
        mask: u64,
        frag: Vec<u8>,
        objects: BTreeMap<u32, Vec<i32>>,
        tick: i32,
        globals: [i32; 32],
        joined: Vec<(i32, i32)>,
        destroyed: Vec<u32>,
        rpcs: Vec<Vec<u8>>,
        msg_seq: u64,
        delivered_seq: u64,
    }

    impl Client {
        fn new(actor: i32) -> Client {
            Client {
                actor,
                id: 0x1234_0000_0000 + actor as u64,
                player: 0,
                counter: 0,
                seq: 0,
                recv: 0,
                mask: 0,
                frag: Vec::new(),
                objects: BTreeMap::new(),
                tick: 0,
                globals: [0; 32],
                joined: Vec::new(),
                destroyed: Vec::new(),
                rpcs: Vec::new(),
                msg_seq: 0,
                delivered_seq: 0,
            }
        }

        fn connect(&mut self, s: &mut Server, now: i64) {
            let mut b = vec![0u8; 152];
            b[0] = packet::COMMAND;
            b[1] = packet::CONNECT;
            b[8..16].copy_from_slice(&self.id.to_le_bytes());
            let out = s.packet(self.actor, &b, now, 1);
            let (_, a) = out.iter().find(|(to, _)| *to == self.actor).unwrap();
            assert_eq!((a[0], a[1]), (packet::COMMAND, packet::ACCEPTED));
            assert_eq!(u64::from_le_bytes(a[16..24].try_into().unwrap()), self.id);
            self.player = i32::from(i16::from_le_bytes([a[10], a[11]])) + 1;
            self.counter = u32::from_le_bytes(a[24..28].try_into().unwrap());
        }

        /// Sends a notify packet: owned objects (all words, created when `create`),
        /// destroys and messages.
        fn send(&mut self, s: &mut Server, now: i64, objects: &[(u32, Vec<i32>, bool)], destroys: &[u32], msgs: Vec<Message>) -> Out {
            let mut w = BitWriter::default();
            w.bool(true);
            w.u32(1);
            w.varlen((self.tick + 1) as u64, 8);
            w.u8(0);
            w.bool(!destroys.is_empty());
            if !destroys.is_empty() {
                w.varlen(destroys.len() as u64, 8);
                for d in destroys {
                    w.varlen(u64::from(*d), 8);
                }
            }
            for (id, words, create) in objects {
                w.bool(true);
                w.varlen(u64::from(*id), 8);
                w.bool(*create);
                if *create {
                    w.varlen(words[word::TYPE] as u64, 8);
                    w.varlen(words.len() as u64, 8);
                    w.varlen(0, 8);
                    w.varlen(words[word::FLAGS] as u64, 8);
                    w.bool(false);
                    w.bool(false);
                }
                let mut prev = word::FIRST_DELTA;
                for (i, v) in words.iter().enumerate().skip(word::FIRST_DELTA) {
                    if *v != 0 {
                        w.bool(true);
                        w.varlen((i - prev) as u64, 4);
                        w.varlen(zigzag(i64::from(*v)), 6);
                        prev = i;
                    }
                }
                w.bool(false);
            }
            w.bool(false);
            w.pad();
            w.u16(msgs.len() as u16);
            for mut m in msgs {
                if m.flags & msg::UNRELIABLE == 0 {
                    self.msg_seq += 1;
                    m.seq = self.msg_seq;
                }
                m.write(&mut w);
            }
            w.pad();
            self.seq = self.seq.wrapping_add(1);
            let h = Notify { kind: packet::NOTIFY_DATA, fragment: 0, seq: self.seq, ack: self.recv, mask: self.mask };
            let mut b = h.bytes().to_vec();
            b.extend_from_slice(&w.data);
            let mut out = Out::new();
            for f in fragments(&b) {
                out.extend(s.packet(self.actor, &f, now, 1));
            }
            out
        }

        /// Reads what the server sent this client, as far as the tests look.
        fn recv(&mut self, out: &Out) {
            for (to, p) in out {
                if *to != self.actor || p[0] != packet::NOTIFY_DATA {
                    continue;
                }
                let h = Notify::read(p).unwrap();
                let whole = if h.fragment != 0 {
                    if h.fragment & 0x7f == 1 {
                        self.frag = p.clone();
                    } else {
                        self.frag.extend_from_slice(&p[packet::HEADER..]);
                    }
                    if h.fragment & 0x80 == 0 {
                        continue;
                    }
                    std::mem::take(&mut self.frag)
                } else {
                    p.clone()
                };
                let d = distance(h.seq, self.recv);
                assert!(d > 0);
                self.recv = h.seq;
                self.mask = if d >= 64 { 1 } else { (self.mask << d) | 1 };
                self.read(&whole[packet::HEADER..]);
            }
        }

        fn read(&mut self, b: &[u8]) {
            let mut r = BitReader::new(b);
            if r.bool().unwrap() {
                for _ in 0..4 {
                    r.f64().unwrap();
                }
                self.tick = r.leb().unwrap() as i32;
                for i in 0..32 {
                    if r.bool().unwrap() {
                        self.globals[i] = r.varlen(8).unwrap() as i32;
                    }
                }
                if r.bool().unwrap() {
                    for _ in 0..r.varlen(8).unwrap() {
                        let id = r.varlen(8).unwrap() as u32;
                        self.objects.remove(&id);
                        self.destroyed.push(id);
                    }
                }
                while r.bool().unwrap() {
                    let id = r.varlen(8).unwrap() as u32;
                    if r.bool().unwrap() {
                        let _ty = r.varlen(8).unwrap();
                        let n = r.varlen(8).unwrap() as usize;
                        let _t = r.varlen(8).unwrap();
                        let _f = r.varlen(8).unwrap();
                        if r.bool().unwrap() {
                            r.varlen(8).unwrap();
                            r.varlen(8).unwrap();
                        }
                        if r.bool().unwrap() {
                            r.aligned(16).unwrap();
                        }
                        self.objects.entry(id).or_insert_with(|| vec![0; n]);
                    }
                    let o = self.objects.get_mut(&id).expect("object update before its create header");
                    let mut at = word::FIRST_DELTA;
                    while r.bool().unwrap() {
                        at += r.varlen(4).unwrap() as usize;
                        o[at] = unzigzag(r.varlen(6).unwrap()) as i32;
                    }
                }
            }
            r.align();
            for _ in 0..r.u16().unwrap() {
                let m = Message::read(&mut r).unwrap();
                if m.flags & msg::UNRELIABLE == 0 {
                    if m.seq != self.delivered_seq + 1 {
                        continue;
                    }
                    self.delivered_seq += 1;
                }
                if m.internal_kind() == Some(msg::PLAYER_JOINED_LEFT) {
                    self.joined.push((m.i32_at(4).unwrap(), m.i32_at(8).unwrap()));
                } else {
                    self.rpcs.push(m.data.clone());
                }
            }
        }

        fn ack(&mut self, s: &mut Server, now: i64) {
            let h = Notify { kind: packet::NOTIFY_ACKS, fragment: 0, seq: 0, ack: self.recv, mask: self.mask };
            s.packet(self.actor, &h.bytes(), now, 1);
        }
    }

    fn object(id: u32, owner: i32, n: usize, value: i32, flags: i32) -> Vec<i32> {
        let mut w = vec![0; n];
        w[word::ID] = id as i32;
        w[word::TYPE] = 7;
        w[word::FLAGS] = flags;
        w[word::STATE_AUTHORITY] = owner;
        w[word::HEADER] = value;
        w
    }

    fn rpc(data: &[u8], target: Option<i32>) -> Message {
        Message {
            bits: data.len() as u32 * 8,
            tick: 5,
            source: 0,
            target: target.unwrap_or(0),
            flags: msg::USER | target.map_or(0, |_| msg::TARGET_PLAYER),
            seq: 0,
            data: data.to_vec(),
        }
    }

    #[test]
    fn varlength_matches_fusion() {
        // WriteUInt32VarLength(5, 8): one block, so a 1 bit then 8 bits of 5.
        let mut w = BitWriter::default();
        w.varlen(5, 8);
        assert_eq!(w.bit_len(), 9);
        assert_eq!(w.data, vec![0b0000_1011, 0]);
        // 300 needs two 8-bit blocks: 0, 1, then 16 bits.
        let mut w = BitWriter::default();
        w.varlen(300, 8);
        assert_eq!(w.bit_len(), 18);
        let mut r = BitReader::new(&w.data);
        assert_eq!(r.varlen(8), Some(300));
        let mut w = BitWriter::default();
        w.leb(300);
        assert_eq!(w.data, vec![0xac, 0x02]);
        assert_eq!(BitReader::new(&w.data).leb(), Some(300));
        assert_eq!(unzigzag(zigzag(-5)), -5);
        assert_eq!(zigzag(-1), 1);
        let m = rpc(&[1, 2, 3], Some(2));
        let mut w = BitWriter::default();
        m.write(&mut w);
        assert_eq!(Message::read(&mut BitReader::new(&w.data)), Some(m));
    }

    /// One short packet naming many objects of the biggest size took hundreds of megabytes.
    #[test]
    fn objects_a_client_declares_are_bounded() {
        let mut s = Server::new(60, 4);
        let mut a = Client::new(1);
        a.connect(&mut s, 0);
        let base = a.counter << 19;
        let sized = |n: usize| -> Vec<(u32, Vec<i32>, bool)> { (1..=40).map(|i| (base | (n as u32 % 4096 + i), vec![0; n], true)).collect() };
        let huge = sized(1 << 20);
        // The server's own state object is there from the start.
        let (had, words) = (s.objects.len(), s.objects.values().map(|o| o.words.len()).sum::<usize>());
        a.send(&mut s, 10, &huge, &[], vec![]);
        assert_eq!(s.objects.len(), had, "past an object's limit");
        let big = sized(OBJECT_WORDS);
        a.send(&mut s, 20, &big, &[], vec![]);
        let fit = (ROOM_WORDS - words) / OBJECT_WORDS;
        assert_eq!(s.objects.len(), had + fit, "up to the room's limit");
        let more = sized(OBJECT_WORDS);
        a.send(&mut s, 30, &more, &[], vec![]);
        assert_eq!(s.objects.len(), had + fit, "and none after");
    }

    #[test]
    fn players_join_and_see_each_others_objects() {
        let mut s = Server::new(60, 4);
        let (mut a, mut b) = (Client::new(1), Client::new(2));
        a.connect(&mut s, 0);
        b.connect(&mut s, 5);
        assert_eq!((a.player, b.player), (1, 2));
        assert_ne!(a.counter, b.counter);

        let id = a.counter << 19 | 1;
        let o = object(id, a.player, 40, 77, flags::DESTROY_WHEN_STATE_AUTHORITY_LEAVES);
        a.send(&mut s, 10, &[(id, o.clone(), true)], &[], vec![]);
        let out = s.tick(20);
        a.recv(&out);
        b.recv(&out);
        assert_eq!(b.objects.get(&id).map(|w| w[word::HEADER]), Some(77));
        assert_eq!(b.objects[&id][word::STATE_AUTHORITY], a.player);
        assert_eq!(b.globals[2], 4);
        assert_eq!(b.globals[4], 0b11);
        // Join messages: b heard of a, then of itself; a of itself, then of b.
        assert_eq!(b.joined, vec![(a.player, 1), (b.player, 1)]);
        assert_eq!(a.joined, vec![(a.player, 1), (b.player, 1)]);
        // The internal state object has both players' actors.
        let st = &b.objects[&INTERNAL_STATE];
        assert_eq!(st[word::HEADER + 4..word::HEADER + 6], [2, 3]);

        // An update reaches b; b can't change a's object.
        let mut o2 = o.clone();
        o2[word::HEADER] = 78;
        a.send(&mut s, 30, &[(id, o2, false)], &[], vec![]);
        let mut forged = o.clone();
        forged[word::HEADER] = 1;
        b.send(&mut s, 31, &[(id, forged, false)], &[], vec![]);
        let out = s.tick(40);
        b.recv(&out);
        assert_eq!(b.objects[&id][word::HEADER], 78);

        // a leaves: its object goes (DestroyWhenStateAuthorityLeaves), b hears it left.
        a.send(&mut s, 50, &[], &[], vec![]);
        s.packet(a.actor, &[packet::COMMAND, packet::DISCONNECT, 3], 51, 1);
        let out = s.tick(60);
        b.recv(&out);
        assert!(b.destroyed.contains(&id));
        assert!(!b.objects.contains_key(&id));
        assert_eq!(b.joined.last(), Some(&(1, 0)));
        assert_eq!(b.globals[4], 0b10);
    }

    #[test]
    fn lost_packets_are_resent_and_rpcs_routed() {
        let mut s = Server::new(30, 4);
        let (mut a, mut b, mut c) = (Client::new(1), Client::new(2), Client::new(3));
        a.connect(&mut s, 0);
        b.connect(&mut s, 0);
        c.connect(&mut s, 0);
        let id = a.counter << 19 | 1;
        a.send(&mut s, 1, &[(id, object(id, a.player, 30, 5, 0), true)], &[], vec![rpc(b"all", None), rpc(b"to c", Some(c.player))]);
        // The first state packet to b is lost: b never acknowledges it.
        let lost = s.tick(40);
        c.recv(&lost);
        a.recv(&lost);
        assert!(!b.objects.contains_key(&id));
        // b's next packet acknowledges a later sequence only, so the lost one is resent.
        let later = s.tick(80);
        let Some((_, p)) = later.iter().find(|(to, _)| *to == b.actor) else { panic!() };
        let h = Notify::read(p).unwrap();
        b.recv = h.seq;
        b.mask = 1;
        b.ack(&mut s, 81);
        let again = s.tick(120);
        b.recv(&again);
        assert_eq!(b.objects.get(&id).map(|w| w[word::HEADER]), Some(5));
        assert_eq!(b.rpcs, vec![b"all".to_vec()]);
        assert_eq!(c.rpcs, vec![b"all".to_vec(), b"to c".to_vec()]);
        assert!(a.rpcs.is_empty());
    }

    #[test]
    fn authority_moves_on_request() {
        let mut s = Server::new(60, 4);
        let (mut a, mut b) = (Client::new(1), Client::new(2));
        a.connect(&mut s, 0);
        b.connect(&mut s, 0);
        let id = a.counter << 19 | 1;
        a.send(&mut s, 1, &[(id, object(id, a.player, 30, 1, flags::ALLOW_STATE_AUTHORITY_OVERRIDE), true)], &[], vec![]);
        s.tick(20);
        let mut req = Message::internal(msg::REQUEST_STATE_AUTHORITY, &[id as i32, 1], 3, None);
        req.flags |= msg::TARGET_SERVER;
        b.send(&mut s, 25, &[], &[], vec![req]);
        let out = s.tick(40);
        a.recv(&out);
        assert_eq!(a.objects[&id][word::STATE_AUTHORITY], b.player);
        // Now b's updates count and a's don't.
        b.send(&mut s, 45, &[(id, object(id, b.player, 30, 9, 0x10), false)], &[], vec![]);
        a.send(&mut s, 46, &[(id, object(id, a.player, 30, 2, 0x10), false)], &[], vec![]);
        let out = s.tick(60);
        a.recv(&out);
        assert_eq!(a.objects[&id][word::HEADER], 9);
    }

    #[test]
    fn big_states_go_in_fragments() {
        let mut s = Server::new(60, 2);
        let (mut a, mut b) = (Client::new(1), Client::new(2));
        a.connect(&mut s, 0);
        b.connect(&mut s, 0);
        let objects: Vec<(u32, Vec<i32>, bool)> = (1..40)
            .map(|i| {
                let id = a.counter << 19 | i;
                let mut o = object(id, a.player, 60, 0, 0);
                for (k, v) in o.iter_mut().enumerate().skip(word::HEADER) {
                    *v = 1000 + (i as i32) * 7 + k as i32;
                }
                (id, o, true)
            })
            .collect();
        // Big enough that a's own packet is fragmented too.
        a.send(&mut s, 1, &objects, &[], vec![]);
        let out = s.tick(20);
        assert!(out.iter().filter(|(to, _)| *to == b.actor).count() > 1);
        b.recv(&out);
        // A packet carries at most 4 KB of objects; the rest follow on the next ticks.
        for t in 1..6 {
            b.ack(&mut s, 20 + t * 20);
            let out = s.tick(30 + t * 20);
            b.recv(&out);
        }
        for (id, w, _) in &objects {
            assert_eq!(b.objects.get(id).map(|o| o[word::HEADER]), Some(w[word::HEADER]));
        }
    }
}
