//! Fusion 2's Shared mode simulation layer (Fusion.Runtime 2.0.x), as its client half
//! reads and writes it. Fusion 1 and 2 share the sockets layer (Connect / Accepted, the
//! 14-byte notify header, fragments, acks) and the message envelope; the rest is new.
//!
//! A notify packet's body:
//! - an 8-byte header (SimulationPacketHeader): message count, area-of-interest cells,
//!   object updates, object destroys (a byte each, so at most 255 of each a packet),
//!   then the sender's tick (i32);
//! - the messages (envelopes as in Fusion 1), then padding to a byte;
//! - server to client only: time feedback, four zigzag VarLength(6) values in 1/256 s:
//!   how early the client's ticks arrive (average, deviation) and the packet receive
//!   interval (average, deviation);
//! - destroys: a NetworkId (VarLength 8) each;
//! - object updates: the id as a zigzag VarLength(6) step from the one before (from 0),
//!   a create-header bit, then (with it) header words 1-19, each a presence bit and a
//!   zigzag VarLength(6). Then the changed words: a VarLength(4) step from the word
//!   before (19 after a header, else 0) and a zigzag VarLength(6) value; a 0 step ends
//!   them. Words 0-6 are fixed at creation. An object flagged HasMainNetworkTRSP with 34
//!   words or more keeps its main transform in words 22-28 as one block
//!   (NetworkTransformSerializer): the step lands on 22, then three zigzag VarLength(4)
//!   positions in 1/1024 and a 32-bit compressed rotation; the next step counts from 28.
//!
//! The object header (NetworkObjectHeader, 20 words): 0 Id, 1 WordCount | BehaviourCount
//! << 16, 2-3 Type (NetworkObjectTypeId: a value, and the kind in the low 2 bits of word
//! 3: prefab 0, struct 2, scene object 3), 4 NestingRoot, 5 NestingKey, 6 Flags,
//! 7 InputAuthority, 8 StateAuthority, 9 the receiving player's interest flags, 10-19
//! reserved.
//!
//! Fusion 2 has no global words and no join / leave / scene messages. The server keeps
//! "struct" objects (flag 0x20) that clients read like any other:
//! - 1 RuntimeConfig: tick rates (client, client send, server, server send), server
//!   mode, player count, master client, host player, topology (Shared 2). A client
//!   waits for it before it runs;
//! - 3 SceneInfo and 4 PhysicsInfo, with state authority PlayerRef.MasterClient (-1),
//!   which a Shared client resolves to RuntimeConfig's master client: the master client
//!   writes them;
//! - a PlayerData struct per player (type struct 1): its PlayerRef, player object and
//!   Photon actor. A client's player list is these objects.
//!
//! Internal messages left: SetAlwaysInterested 3, RequestStateAuthority 4 (object,
//! acquire), SetPlayerObject 6 (object), SetAreaOfInterest 7.
//!
//! Reliable data (`SendReliableDataToPlayer` / `SendReliableDataToServer`) travels in
//! unfragmented `NOTIFY_RELIABLE` (kind 8) packets sharing the notify sequence space:
//! a 48-byte little-endian `ReliableId` (64-bit per-connection sequence masked to 32 bits,
//! slice length, total length, 0-based source PlayerId, sender transfer id, target PlayerId
//! where -1 is the server, 16-byte ReliableKey, 4-byte padding) followed by the slice bytes.
//! Each connection reorders incoming slices and assigns its own outgoing sequence; lost
//! packets are re-queued on ack timeout/loss.

use super::*;

/// Header words.
mod w {
    pub const ID: usize = 0;
    pub const COUNTS: usize = 1;
    pub const TYPE: usize = 2;
    pub const FLAGS: usize = 6;
    pub const STATE_AUTHORITY: usize = 8;
    pub const PLAYER_DATA: usize = 9;
    /// Words before this are fixed at creation (READ_ONLY_WORD_COUNT).
    pub const READ_ONLY: usize = 7;
    pub const HEADER: usize = 20;
    /// NetworkTransformSerializer's block.
    pub const TRSP: usize = 22;
    pub const TRSP_LAST: usize = 28;
    pub const TRSP_MIN_WORDS: usize = 34;
}

/// NetworkObjectHeaderFlags.
mod f {
    pub const HAS_MAIN_TRSP: i32 = 8;
    pub const ALLOW_STATE_AUTHORITY_OVERRIDE: i32 = 0x10;
    pub const STRUCT: i32 = 0x20;
    pub const AREA_OF_INTEREST: i32 = 0x100;
}

/// Internal message types.
mod kind {
    pub const SET_ALWAYS_INTERESTED: i32 = 3;
    pub const REQUEST_STATE_AUTHORITY: i32 = 4;
    pub const SET_PLAYER_OBJECT: i32 = 6;
    pub const SET_AREA_OF_INTEREST: i32 = 7;
}

/// RuntimeConfig's data words (SimulationRuntimeConfig).
mod rc {
    pub const TICK_RATES: usize = 20;
    pub const SERVER_MODE: usize = 24;
    pub const PLAYER_MAX: usize = 25;
    pub const MASTER: usize = 26;
    pub const TOPOLOGY: usize = 28;
    pub const WORDS: usize = 9;
}

const RUNTIME_CONFIG: u32 = 1;
const SCENE_INFO: u32 = 3;
const PHYSICS_INFO: u32 = 4;
/// Ids up to here are Fusion's reserved ones; the server numbers its own after them,
/// far below clients' (their connection counter << 19).
const MAX_RESERVED_ID: u32 = 1023;
/// PlayerRef.MasterClient.
const MASTER_CLIENT: i32 = -1;
/// NetworkObjectTypeId of PlayerData structs (struct 1), and their words: player,
/// player object, actor.
const PLAYER_DATA_TYPE: (i32, i32) = (1, 2);
const PLAYER_DATA_WORDS: usize = 3;
/// SimulationModes.Server and Topologies.Shared.
const SERVER_MODE_SERVER: i32 = 1;
const TOPOLOGY_SHARED: i32 = 2;
/// Counts in the packet header are bytes.
const PER_PACKET: usize = 255;
/// Fusion 2's `Fusion.Sockets.ReliableId` size in bytes.
pub(super) const RELIABLE_ID_SIZE: usize = 48;
/// Fusion's `NetPeerGroup.SendReliable` slice cap (`0x440`), small enough to go unfragmented.
const RELIABLE_SLICE_MAX: usize = 1088;
/// Outgoing `NOTIFY_RELIABLE` packets per peer per tick, so a large transfer can't overrun
/// the 64-bit notify ack mask.
const RELIABLE_PER_TICK: usize = 4;
/// Per-peer byte caps on buffered out-of-order incoming slices and queued outgoing slices.
const RELIABLE_IN_BYTES_MAX: usize = 1 << 20;
const RELIABLE_OUT_BYTES_MAX: usize = 8 << 20;

/// Fusion 2's 48-byte `Fusion.Sockets.ReliableId` header on a `NOTIFY_RELIABLE` packet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ReliableId {
    /// Per-connection sequence (32-bit masked in `ReliableBuffer`, starting at 1).
    pub sequence: u64,
    pub slice_len: i32,
    pub total_len: i32,
    /// Sender's 0-based `PlayerId` (`raw PlayerRef - 1`; `-1` = None).
    pub source: i32,
    /// Sender's monotonically increasing transfer counter.
    pub source_send: i32,
    /// Target's 0-based `PlayerId` (`-1` = Server / `PlayerRef.None`, `-2` = `MasterClient`).
    pub target: i32,
    pub key: [i32; 4],
    pub padding: i32,
}

impl ReliableId {
    fn read(b: &[u8]) -> Option<ReliableId> {
        if b.len() < RELIABLE_ID_SIZE {
            return None;
        }
        let i32_at = |at: usize| i32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        Some(ReliableId {
            sequence: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            slice_len: i32_at(8),
            total_len: i32_at(12),
            source: i32_at(16),
            source_send: i32_at(20),
            target: i32_at(24),
            key: [i32_at(28), i32_at(32), i32_at(36), i32_at(40)],
            padding: i32_at(44),
        })
    }

    fn bytes(&self) -> [u8; RELIABLE_ID_SIZE] {
        let mut b = [0u8; RELIABLE_ID_SIZE];
        b[0..8].copy_from_slice(&self.sequence.to_le_bytes());
        b[8..12].copy_from_slice(&self.slice_len.to_le_bytes());
        b[12..16].copy_from_slice(&self.total_len.to_le_bytes());
        b[16..20].copy_from_slice(&self.source.to_le_bytes());
        b[20..24].copy_from_slice(&self.source_send.to_le_bytes());
        b[24..28].copy_from_slice(&self.target.to_le_bytes());
        for (i, k) in self.key.iter().enumerate() {
            b[28 + i * 4..32 + i * 4].copy_from_slice(&k.to_le_bytes());
        }
        b[44..48].copy_from_slice(&self.padding.to_le_bytes());
        b
    }
}

/// One slice of a Fusion 2 reliable data transfer.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ReliableSlice {
    pub id: ReliableId,
    pub data: Vec<u8>,
}

/// Fusion 2's server state.
pub(in crate::photon) struct State {
    /// Resolved tick rates: client, client send, server, server send.
    rates: [u32; 4],
    next_id: u32,
    parse_failures: u32,
    /// The scene info as last logged.
    scene: Vec<i32>,
    /// (player, object) pairs whose refused writes were logged, once each.
    refused: std::collections::HashSet<(i32, u32)>,
}

/// The packet header.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Head {
    messages: u8,
    cells: u8,
    updates: u8,
    destroys: u8,
    tick: i32,
}

impl Head {
    fn read(r: &mut BitReader) -> Option<Head> {
        Some(Head { messages: r.u8()?, cells: r.u8()?, updates: r.u8()?, destroys: r.u8()?, tick: r.u32()? as i32 })
    }
    fn bytes(&self) -> [u8; 8] {
        let t = self.tick.to_le_bytes();
        [self.messages, self.cells, self.updates, self.destroys, t[0], t[1], t[2], t[3]]
    }
}

/// Whether an object's words 22-28 go as the transform block.
fn trsp(flags: i32, words: usize) -> bool {
    flags & f::HAS_MAIN_TRSP != 0 && words >= w::TRSP_MIN_WORDS
}

/// Seconds as TimeFeedback carries them: zigzag of the value in 1/256, rounded half
/// away from zero (Maths' float to fixed point).
fn fixed(v: f64) -> u64 {
    let x = v * 256.0;
    zigzag(i64::from((x + if x < 0.0 { -0.5 } else { 0.5 }) as i32))
}

impl Object {
    fn new_v2(words: Vec<i32>) -> Object {
        let n = words.len();
        Object { words, changed: vec![0; n], authority_at: w::STATE_AUTHORITY, flags_at: w::FLAGS }
    }

    /// A struct object of the server's: `data` words after the header.
    fn struct_v2(id: u32, data: usize, ty: (i32, i32), authority: i32) -> Object {
        let n = w::HEADER + data;
        let mut words = vec![0; n];
        words[w::ID] = id as i32;
        words[w::COUNTS] = n as i32;
        words[w::TYPE] = ty.0;
        words[w::TYPE + 1] = ty.1;
        words[w::FLAGS] = f::STRUCT;
        words[w::STATE_AUTHORITY] = authority;
        Object::new_v2(words)
    }
}

/// Resolved tick rates from a TickRateSelection: Fusion's tables give each client
/// rate its lower send and server rates; index 0 (what games leave) is the client
/// rate itself, and a higher index is taken as halving it again.
pub fn resolve_rates(client: u32, server: u32, client_send: u32, server_send: u32) -> [u32; 4] {
    let at = |i: u32| (client >> i.min(8)).max(1);
    [client, at(client_send), at(server), at(server_send)]
}

impl Server {
    /// Fusion 2's server, with the session's resolved tick rates.
    pub fn new_v2(rates: [u32; 4], players: usize) -> Server {
        let mut s = Server::new(rates[2], players);
        s.v2 = Some(State { rates, next_id: MAX_RESERVED_ID + 1, parse_failures: 0, scene: Vec::new(), refused: Default::default() });
        s
    }

    pub(super) fn joined_v2(&mut self, actor: i32) {
        let tick = self.tick + 1;
        if !self.objects.contains_key(&RUNTIME_CONFIG) {
            let rates = self.v2.as_ref().unwrap().rates;
            let mut c = Object::struct_v2(RUNTIME_CONFIG, rc::WORDS, (0, 0), 0);
            for (i, r) in rates.iter().enumerate() {
                c.set(rc::TICK_RATES + i, *r as i32, tick);
            }
            c.set(rc::SERVER_MODE, SERVER_MODE_SERVER, tick);
            c.set(rc::PLAYER_MAX, self.players as i32, tick);
            c.set(rc::TOPOLOGY, TOPOLOGY_SHARED, tick);
            self.objects.insert(RUNTIME_CONFIG, c);
            // Their state authority is the master client, whoever that is at the time.
            self.objects.insert(SCENE_INFO, Object::struct_v2(SCENE_INFO, 13, (0, 0), MASTER_CLIENT));
            self.objects.insert(PHYSICS_INFO, Object::struct_v2(PHYSICS_INFO, 10, (0, 0), MASTER_CLIENT));
            self.log.push(format!(
                "photon: Fusion 2 Shared: runtime config (tick rates client {} / send {}, server {} / send {}; {} players), scene info and physics info objects up",
                rates[0], rates[1], rates[2], rates[3], self.players
            ));
        }
        let s = self.v2.as_mut().unwrap();
        let id = s.next_id;
        s.next_id += 1;
        let c = self.conns.get_mut(&actor).unwrap();
        c.player_data = id;
        let raw = c.raw_player();
        let mut o = Object::struct_v2(id, PLAYER_DATA_WORDS, PLAYER_DATA_TYPE, 0);
        o.set(w::HEADER, raw, tick);
        o.set(w::HEADER + 2, actor, tick);
        self.objects.insert(id, o);
        self.log.push(format!("photon: Fusion 2 Shared: player {} (actor {actor}) has player data object {id}", raw - 1));
        self.sync_master_v2();
    }

    pub(super) fn left_v2(&mut self, c: &Conn) {
        if c.player_data != 0 {
            self.destroy(c.player_data, None);
        }
        self.sync_master_v2();
    }

    /// RuntimeConfig's master client follows the room's.
    pub(super) fn sync_master_v2(&mut self) {
        let raw = self.master_raw();
        let tick = self.tick + 1;
        let master = self.master;
        let Some(o) = self.objects.get_mut(&RUNTIME_CONFIG) else { return };
        if o.words[rc::MASTER] != raw {
            o.set(rc::MASTER, raw, tick);
            let actor = self.conns.iter().find(|(_, c)| c.raw_player() == raw).map(|(a, _)| *a);
            let who = match actor {
                Some(a) if a == master => format!("player {} (actor {a}, the room's master)", raw - 1),
                Some(a) => format!("player {} (actor {a}; the room's master, actor {master}, isn't connected to Fusion)", raw - 1),
                None => "nobody (no player is connected)".into(),
            };
            self.log.push(format!("photon: Fusion 2 Shared: master client is {who}"));
        }
    }

    /// The master client's PlayerRef: the room's master; while that one isn't connected
    /// to Fusion, the one RuntimeConfig names if it still is, else the first player. A
    /// session without one has nobody to write its scene.
    fn master_raw(&self) -> i32 {
        if let Some(c) = self.conns.get(&self.master) {
            return c.raw_player();
        }
        let named = self.objects.get(&RUNTIME_CONFIG).map_or(0, |o| o.words[rc::MASTER]);
        if named > 0 && self.conns.values().any(|c| c.raw_player() == named) {
            return named;
        }
        self.conns.values().map(|c| c.raw_player()).min().unwrap_or(0)
    }

    /// The scene info's data words, for the status line.
    pub(super) fn scene_v2(&self) -> String {
        match self.objects.get(&SCENE_INFO) {
            Some(o) => format!("{:?}", &o.words[w::HEADER..]),
            None => "-".into(),
        }
    }

    // -------------------------------------------------------------- what clients send

    pub(super) fn body_v2(&mut self, actor: i32, body: &[u8], now: i64, master: i32, out: &mut Out) {
        let mut r = BitReader::new(body);
        let mut last = 0u32;
        let parsed = (|| {
            let head = Head::read(&mut r)?;
            r.align();
            let mut got = Vec::new();
            for _ in 0..head.messages {
                got.push(Message::read(&mut r)?);
            }
            r.align();
            Some((head, got))
        })();
        let Some((head, got)) = parsed else {
            self.parse_failed(actor, "its header or messages", r.pos(), body.len(), None);
            return;
        };
        self.timing_v2(actor, head.tick, now);
        if self.state_v2(actor, &mut r, head, &mut last).is_none() {
            self.parse_failed(actor, "its objects", r.pos(), body.len(), Some(last));
        }
        for m in got {
            self.incoming(actor, m, master, out);
        }
    }

    fn parse_failed(&mut self, actor: i32, part: &str, at: usize, len: usize, object: Option<u32>) {
        let s = self.v2.as_mut().unwrap();
        s.parse_failures += 1;
        let n = s.parse_failures;
        // The first few, then every power of two: a client this server misreads sends
        // one every tick.
        if n <= 4 || n.is_power_of_two() {
            let obj = object.map(|o| format!(", object {o}")).unwrap_or_default();
            self.log.push(format!(
                "photon: Fusion 2 Shared packet from actor {actor} didn't parse ({part}, at bit {at} of {}{obj}; {n} so far)",
                len * 8
            ));
        }
    }

    /// The client's tick against the server's clock, for the time feedback.
    fn timing_v2(&mut self, actor: i32, tick: i32, now: i64) {
        let started = self.started_ms.unwrap_or(now);
        let tick_ms = self.tick_ms();
        let c = self.conns.get_mut(&actor).unwrap();
        if tick > c.input_tick {
            c.input_tick = tick;
            let due = started as f64 + f64::from(tick - 1) * tick_ms;
            c.input_offset.add((due - now as f64) / 1000.0);
        }
        if let Some(last) = c.last_packet_ms
            && now - last >= 10
        {
            c.packet_interval.add((now - last) as f64 / 1000.0);
        }
        if c.last_packet_ms.is_none_or(|l| now - l >= 10) {
            c.last_packet_ms = Some(now);
        }
    }

    fn state_v2(&mut self, actor: i32, r: &mut BitReader, head: Head, last: &mut u32) -> Option<()> {
        let raw = self.conns[&actor].raw_player();
        let master_raw = self.master_raw();
        let may_write = |authority: i32| authority == raw || (authority == MASTER_CLIENT && raw == master_raw);
        let mut destroys = Vec::new();
        for _ in 0..head.destroys {
            destroys.push(r.varlen(8)? as u32);
        }
        let tick = self.tick + 1;
        let mut id: i64 = 0;
        for _ in 0..head.updates {
            id += unzigzag(r.varlen(6)?);
            let oid = u32::try_from(id).ok()?;
            *last = oid;
            let header = if r.bool()? {
                let mut h = [0i32; w::HEADER];
                h[w::ID] = oid as i32;
                for v in h.iter_mut().skip(1) {
                    if r.bool()? {
                        *v = unzigzag(r.varlen(6)?) as i32;
                    }
                }
                Some(h)
            } else {
                None
            };
            // The object's size and transform block: its header, or the object kept. One
            // never heard of can't be read past (where its words end depends on them).
            let (count, block) = match (&header, self.objects.get(&oid)) {
                (_, Some(o)) => (o.words.len(), trsp(o.flags(), o.words.len())),
                (Some(h), None) => {
                    let n = (h[w::COUNTS] & 0xffff) as usize;
                    (n, trsp(h[w::FLAGS], n))
                }
                (None, None) => return None,
            };
            let mut words = Vec::new();
            let mut at = if header.is_some() { w::HEADER - 1 } else { 0 };
            loop {
                let step = r.varlen(4)? as u32 as i32;
                if step <= 0 {
                    break;
                }
                at = at.checked_add(step as usize)?;
                if at >= count {
                    return None;
                }
                if at < w::READ_ONLY {
                    r.varlen(6)?;
                    continue;
                }
                if block && (w::TRSP..=w::TRSP_LAST).contains(&at) {
                    if at != w::TRSP {
                        return None;
                    }
                    for k in 0..3 {
                        words.push((w::TRSP + k, unzigzag(r.varlen(4)?) as i32));
                    }
                    words.push((w::TRSP + 3, r.u32()? as i32));
                    at = w::TRSP_LAST;
                    continue;
                }
                words.push((at, unzigzag(r.varlen(6)?) as i32));
            }
            // A header sent again must match the one kept: its size and flags decide how
            // this server reads and writes the object's words.
            if let (Some(h), Some(o)) = (&header, self.objects.get(&oid))
                && (h[w::COUNTS] != o.words[w::COUNTS] || h[w::FLAGS] != o.words[w::FLAGS])
            {
                let (was, now) = ((o.words[w::COUNTS], o.words[w::FLAGS]), (h[w::COUNTS], h[w::FLAGS]));
                let s = self.v2.as_mut().unwrap();
                if s.refused.len() < 512 && s.refused.insert((-raw, oid)) {
                    self.log.push(format!(
                        "photon: Fusion 2 Shared: player {} sent object {oid}'s header again with words/flags {now:?}, kept {was:?} (logged once per object)",
                        raw - 1
                    ));
                }
            }
            if !self.objects.contains_key(&oid) {
                let Some(h) = header else { continue };
                if !self.room_for(count) {
                    continue;
                }
                let mut ws = vec![0; count];
                ws[..w::HEADER].copy_from_slice(&h);
                ws[w::PLAYER_DATA] = 0;
                // The creator holds state authority (or the master client, for an object it
                // gives the master client), until it says otherwise.
                let a = h[w::STATE_AUTHORITY];
                ws[w::STATE_AUTHORITY] = if a == MASTER_CLIENT && raw == master_raw { MASTER_CLIENT } else { raw };
                self.objects.insert(oid, Object::new_v2(ws));
            }
            let o = self.objects.get_mut(&oid).unwrap();
            if !may_write(o.authority()) {
                // The server's own structs come back from every client unchanged (Fusion
                // writes them all); only a player writing another's object is news.
                let authority = o.authority();
                let s = self.v2.as_mut().unwrap();
                if authority != 0 && !words.is_empty() && s.refused.len() < 512 && s.refused.insert((raw, oid)) {
                    self.log.push(format!(
                        "photon: Fusion 2 Shared: player {} wrote object {oid}, whose state authority is {authority}; not taken (logged once per object)",
                        raw - 1
                    ));
                }
                continue;
            }
            let scene = oid == SCENE_INFO;
            for (i, v) in words {
                // The state authority only moves on requests; the interest word is the
                // server's, per receiver.
                if i != w::STATE_AUTHORITY && i != w::PLAYER_DATA {
                    o.set(i, v, tick);
                }
            }
            if scene {
                let now = o.words[w::HEADER..].to_vec();
                let s = self.v2.as_mut().unwrap();
                if s.scene != now {
                    s.scene = now.clone();
                    self.log.push(format!("photon: Fusion 2 Shared: scene info from player {} is {now:?}", raw - 1));
                }
            }
        }
        for id in destroys {
            // The server's own objects are its own: no player destroys them.
            let ours = id <= MAX_RESERVED_ID || self.conns.values().any(|c| c.player_data == id);
            let Some(authority) = self.objects.get(&id).map(|o| o.authority()) else { continue };
            if !ours && (authority == 0 || may_write(authority)) {
                self.destroy(id, Some(actor));
            } else {
                // The player has dropped it, but the others keep it and this server goes on
                // sending it to that player too.
                self.log.push(format!(
                    "photon: Fusion 2 Shared: player {} destroyed object {id}, whose state authority is {authority}{}; not taken",
                    raw - 1,
                    if ours { " (the server's own)" } else { "" }
                ));
            }
        }
        Some(())
    }

    pub(super) fn internal_v2(&mut self, actor: i32, k: i32, m: &Message, _master: i32) {
        let raw = self.conns[&actor].raw_player();
        let master_raw = self.master_raw();
        let tick = self.tick + 1;
        match k {
            kind::REQUEST_STATE_AUTHORITY => {
                let (Some(id), Some(acquire)) = (m.i32_at(4), m.i32_at(8)) else { return };
                let Some(o) = self.objects.get_mut(&(id as u32)) else { return };
                let now = o.authority();
                let mine = now == raw || (now == MASTER_CLIENT && raw == master_raw);
                let next = if acquire != 0 {
                    if now == 0 || mine || o.flags() & f::ALLOW_STATE_AUTHORITY_OVERRIDE != 0 {
                        // A master client object stays the master client's.
                        if now == MASTER_CLIENT && mine { now } else { raw }
                    } else {
                        now
                    }
                } else if now == raw {
                    0
                } else {
                    now
                };
                if next != now {
                    o.set(w::STATE_AUTHORITY, next, tick);
                    self.log.push(format!("photon: Fusion 2 Shared: object {id}'s state authority {now} -> {next} (asked by player {})", raw - 1));
                } else if acquire != 0 && !mine {
                    self.log.push(format!("photon: Fusion 2 Shared: player {} may not take object {id} from {now}", raw - 1));
                }
            }
            kind::SET_PLAYER_OBJECT => {
                let Some(obj) = m.i32_at(4) else { return };
                let pd = self.conns[&actor].player_data;
                if let Some(o) = self.objects.get_mut(&pd) {
                    o.set(w::HEADER + 1, obj, tick);
                    self.log.push(format!("photon: Fusion 2 Shared: player {}'s player object is {obj}", raw - 1));
                }
            }
            // Interest management isn't done: everyone gets every object.
            kind::SET_ALWAYS_INTERESTED | kind::SET_AREA_OF_INTEREST => {}
            other => self.log.push(format!("photon: Fusion 2 Shared: internal message {other} from player {} not served", raw - 1)),
        }
    }

    // -------------------------------------------------------------- what the server sends

    pub(super) fn send_state_v2(&mut self, actor: i32, out: &mut Out) {
        let tick = self.tick;
        let c = self.conns.get_mut(&actor).unwrap();
        let mut head = Head { tick, ..Head::default() };
        let mut wr = BitWriter::default();
        wr.u64(0);
        // Messages.
        let mut messages = Vec::new();
        while let Some(m) = c.out.front() {
            if messages.len() >= PER_PACKET {
                break;
            }
            let mut probe = BitWriter::default();
            m.write(&mut probe);
            if !messages.is_empty() && wr.bit_len() + probe.bit_len() > MESSAGE_BITS {
                break;
            }
            let m = c.out.pop_front().unwrap();
            m.write(&mut wr);
            messages.push(m);
        }
        wr.pad();
        head.messages = messages.len() as u8;
        for v in [c.input_offset.val, c.input_offset.dev, c.packet_interval.val, c.packet_interval.dev] {
            wr.varlen(fixed(v), 6);
        }
        let n = c.destroys.len().min(PER_PACKET);
        let destroys: Vec<u32> = c.destroys.drain(..n).collect();
        for id in &destroys {
            wr.varlen(u64::from(*id), 8);
        }
        head.destroys = destroys.len() as u8;
        // Objects: the server's structs (the lowest ids) first, as Fusion sends them.
        let state_at = wr.bit_len();
        let mut sent = Vec::new();
        let mut prev_id: i64 = 0;
        for (id, o) in self.objects.iter() {
            if sent.len() >= PER_PACKET || wr.bit_len() - state_at >= OBJECT_BITS {
                break;
            }
            let s = c.objects.entry(*id).or_insert(Sent { tick: 0, create_unconfirmed: true });
            if s.tick == tick {
                continue;
            }
            let create = s.create_unconfirmed;
            // An update sends what changed since; nothing changed, nothing goes. A create
            // goes again only when its packet was lost (its sent tick back to 0) or the
            // object changed meanwhile, and then with every word that was ever set.
            let changed = o.changed[w::READ_ONLY..].iter().any(|t| *t > s.tick);
            if !changed && !(create && s.tick == 0) {
                s.tick = tick;
                continue;
            }
            let since = if create { 0 } else { s.tick };
            wr.varlen(zigzag(i64::from(*id) - prev_id), 6);
            prev_id = i64::from(*id);
            wr.bool(create);
            if create {
                for i in 1..w::HEADER {
                    let v = if i == w::PLAYER_DATA {
                        // Nobody is out of interest here.
                        i32::from(o.flags() & f::AREA_OF_INTEREST != 0)
                    } else {
                        o.words[i]
                    };
                    wr.bool(v != 0);
                    if v != 0 {
                        wr.varlen(zigzag(i64::from(v)), 6);
                    }
                }
            }
            let block = trsp(o.flags(), o.words.len());
            let mut prev = if create { w::HEADER - 1 } else { 0 };
            let mut i = if create { w::HEADER } else { w::READ_ONLY };
            while i < o.words.len() {
                if block && i == w::TRSP {
                    if o.changed[w::TRSP..=w::TRSP_LAST].iter().any(|t| *t > since) {
                        wr.varlen((w::TRSP - prev) as u64, 4);
                        for k in 0..3 {
                            wr.varlen(zigzag(i64::from(o.words[w::TRSP + k])), 4);
                        }
                        wr.u32(o.words[w::TRSP + 3] as u32);
                        prev = w::TRSP_LAST;
                    }
                    i = w::TRSP_LAST + 1;
                    continue;
                }
                if i != w::PLAYER_DATA && o.changed[i] > since {
                    wr.varlen((i - prev) as u64, 4);
                    wr.varlen(zigzag(i64::from(o.words[i])), 6);
                    prev = i;
                }
                i += 1;
            }
            wr.varlen(0, 4);
            // A lost create is sent again whichever of its packets went missing.
            sent.push((*id, if create { 0 } else { s.tick }));
            s.tick = tick;
        }
        head.updates = sent.len() as u8;
        wr.data[..8].copy_from_slice(&head.bytes());
        self.send_notify(actor, &wr.data, Envelope { seq: 0, objects: sent, destroys, messages, globals: None, reliable: None }, out);
    }

    // -------------------------------------------------------------- reliable data

    /// An incoming `NOTIFY_RELIABLE` packet's body (after the 14-byte notify header).
    pub(super) fn reliable_v2(&mut self, actor: i32, body: &[u8]) {
        let Some(id) = ReliableId::read(body) else {
            self.parse_failed(actor, "reliable data header", 0, body.len(), None);
            return;
        };
        let Ok(slice_len) = usize::try_from(id.slice_len) else {
            self.parse_failed(actor, "reliable data slice length", 0, body.len(), None);
            return;
        };
        if slice_len == 0
            || id.total_len < id.slice_len
            || RELIABLE_ID_SIZE.checked_add(slice_len).is_none_or(|end| end > body.len())
        {
            self.parse_failed(actor, "reliable data payload", 0, body.len(), None);
            return;
        }
        let slice = ReliableSlice { id, data: body[RELIABLE_ID_SIZE..RELIABLE_ID_SIZE + slice_len].to_vec() };
        let seq = id.sequence as u32;
        let c = self.conns.get_mut(&actor).unwrap();
        let d = seq.wrapping_sub(c.reliable_in_seq) as i32;
        if d <= 0 || d > AHEAD_MAX as i32 {
            return;
        }
        if d > 1 {
            if !c.reliable_in_waiting.contains_key(&seq) {
                let held: usize = c.reliable_in_waiting.values().map(|s| s.data.len()).sum();
                if c.reliable_in_waiting.len() < AHEAD_MAX as usize && held.saturating_add(slice.data.len()) <= RELIABLE_IN_BYTES_MAX {
                    c.reliable_in_waiting.insert(seq, slice);
                }
            }
            return;
        }
        c.reliable_in_seq = seq;
        let mut ready = vec![slice];
        while let Some(next) = c.reliable_in_waiting.remove(&c.reliable_in_seq.wrapping_add(1)) {
            c.reliable_in_seq = c.reliable_in_seq.wrapping_add(1);
            ready.push(next);
        }
        for s in ready {
            self.route_reliable_v2(actor, s);
        }
    }

    fn route_reliable_v2(&mut self, actor: i32, mut slice: ReliableSlice) {
        let from_player = self.conns[&actor].player as i32;
        slice.id.source = from_player;
        let size = if slice.id.slice_len == slice.id.total_len {
            format!("{} bytes", slice.id.slice_len)
        } else {
            format!("{}/{} bytes", slice.id.slice_len, slice.id.total_len)
        };
        let to_session = slice.id.target == -1 || slice.id.target == -2;
        let target_actor = if to_session {
            let master_raw = self.master_raw();
            if master_raw > 0 {
                self.conns.iter().find(|(_, c)| c.raw_player() == master_raw).map(|(a, _)| *a)
            } else {
                None
            }
        } else if slice.id.target >= 0 {
            self.conns.iter().find(|(_, c)| c.player as i32 == slice.id.target).map(|(a, _)| *a)
        } else {
            None
        };
        let Some(to_actor) = target_actor else {
            let dest = if to_session {
                "session (no master client)".to_string()
            } else {
                format!("player {} (not connected)", slice.id.target)
            };
            self.log.push(format!(
                "photon: Fusion 2 Shared: reliable data from player {from_player} to {dest} dropped: {size}, key {:?}, send #{}",
                slice.id.key, slice.id.source_send
            ));
            return;
        };
        let to_player = self.conns[&to_actor].player as i32;
        if to_session {
            slice.id.target = to_player;
        }
        let chunks = slice.data.len().div_ceil(RELIABLE_SLICE_MAX);
        let to_conn = self.conns.get_mut(&to_actor).unwrap();
        let queued_bytes: usize = to_conn.reliable_out.iter().map(|s| s.data.len()).sum();
        if to_conn.reliable_out.len() + chunks > OUT_MAX || queued_bytes.saturating_add(slice.data.len()) > RELIABLE_OUT_BYTES_MAX {
            self.log.push(format!(
                "photon: Fusion 2 Shared: reliable data from player {from_player} to player {to_player} (actor {to_actor}) dropped (queue full): {size}, key {:?}, send #{}",
                slice.id.key, slice.id.source_send
            ));
            return;
        }
        for chunk in slice.data.chunks(RELIABLE_SLICE_MAX) {
            to_conn.reliable_out_seq = to_conn.reliable_out_seq.wrapping_add(1);
            let mut id = slice.id;
            id.sequence = u64::from(to_conn.reliable_out_seq);
            id.slice_len = chunk.len() as i32;
            to_conn.reliable_out.push_back(ReliableSlice { id, data: chunk.to_vec() });
        }
        let via = if to_session {
            format!("session -> master player {to_player} (actor {to_actor})")
        } else {
            format!("player {to_player} (actor {to_actor})")
        };
        self.log.push(format!(
            "photon: Fusion 2 Shared: reliable data from player {from_player} to {via}: {size}, key {:?}, send #{}",
            slice.id.key, slice.id.source_send
        ));
    }

    /// Sends queued `NOTIFY_RELIABLE` slices to `actor`, unfragmented (`fragment = 0`):
    /// Fusion's `NetPeerGroup.HandlePacketNotifyData` only dispatches reliable data when
    /// `fragment == 0`, so slices are kept at or under `RELIABLE_SLICE_MAX`.
    pub(super) fn send_reliable_v2(&mut self, actor: i32, out: &mut Out) {
        for _ in 0..RELIABLE_PER_TICK {
            let c = self.conns.get_mut(&actor).unwrap();
            let Some(slice) = c.reliable_out.pop_front() else {
                break;
            };
            c.send_seq = c.send_seq.wrapping_add(1);
            let h = Notify { kind: packet::NOTIFY_RELIABLE, fragment: 0, seq: c.send_seq, ack: c.recv_seq, mask: c.recv_mask };
            let mut bytes = Vec::with_capacity(packet::HEADER + RELIABLE_ID_SIZE + slice.data.len());
            bytes.extend_from_slice(&h.bytes());
            bytes.extend_from_slice(&slice.id.bytes());
            bytes.extend_from_slice(&slice.data);
            c.window.push_back(Envelope {
                seq: c.send_seq,
                objects: Vec::new(),
                destroys: Vec::new(),
                messages: Vec::new(),
                globals: None,
                reliable: Some(slice),
            });
            while c.window.len() > 1024 {
                c.window.pop_front();
            }
            out.push((actor, bytes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Fusion 2 client as far as the tests need: the handshake, notify headers, and
    /// its side of the packet layout.
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
        feedback: Vec<f64>,
        destroyed: Vec<u32>,
        rpcs: Vec<Vec<u8>>,
        msg_seq: u64,
        delivered_seq: u64,
        reliable_send_seq: u32,
        reliable_send_id: i32,
        reliable_recv_seq: u32,
        reliable_waiting: BTreeMap<u32, ReliableSlice>,
        /// Reassembled incoming reliable transfers, keyed by `(source, source_send)`.
        reliable_parts: HashMap<(i32, i32), Vec<u8>>,
        /// Completed incoming reliable transfers: `(source, target, key, payload)`.
        reliable: Vec<(i32, i32, [i32; 4], Vec<u8>)>,
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
                feedback: Vec::new(),
                destroyed: Vec::new(),
                rpcs: Vec::new(),
                msg_seq: 0,
                delivered_seq: 0,
                reliable_send_seq: 0,
                reliable_send_id: 0,
                reliable_recv_seq: 0,
                reliable_waiting: BTreeMap::new(),
                reliable_parts: HashMap::new(),
                reliable: Vec::new(),
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
            self.player = i32::from(i16::from_le_bytes([a[10], a[11]])) + 1;
            self.counter = u32::from_le_bytes(a[24..28].try_into().unwrap());
        }

        /// Sends the objects this client holds (all words; with the header when `create`),
        /// destroys and messages, in one packet.
        fn send(&mut self, s: &mut Server, now: i64, objects: &[(u32, Vec<i32>, bool)], destroys: &[u32], msgs: Vec<Message>) -> Out {
            let mut out = Out::new();
            for p in self.packets(objects, destroys, msgs) {
                out.extend(s.packet(self.actor, &p, now, 1));
            }
            out
        }

        fn packets(&mut self, objects: &[(u32, Vec<i32>, bool)], destroys: &[u32], msgs: Vec<Message>) -> Vec<Vec<u8>> {
            let mut wr = BitWriter::default();
            let head = Head { messages: msgs.len() as u8, cells: 0, updates: objects.len() as u8, destroys: destroys.len() as u8, tick: self.tick + 1 };
            wr.bytes(&head.bytes());
            for mut m in msgs {
                if m.flags & msg::UNRELIABLE == 0 {
                    self.msg_seq += 1;
                    m.seq = self.msg_seq;
                }
                m.write(&mut wr);
            }
            wr.pad();
            for d in destroys {
                wr.varlen(u64::from(*d), 8);
            }
            let mut prev_id = 0i64;
            for (id, words, create) in objects {
                wr.varlen(zigzag(i64::from(*id) - prev_id), 6);
                prev_id = i64::from(*id);
                wr.bool(*create);
                if *create {
                    for v in &words[1..w::HEADER] {
                        wr.bool(*v != 0);
                        if *v != 0 {
                            wr.varlen(zigzag(i64::from(*v)), 6);
                        }
                    }
                }
                let block = trsp(words[w::FLAGS], words.len());
                let mut prev = if *create { w::HEADER - 1 } else { 0 };
                let mut i = w::HEADER;
                while i < words.len() {
                    if block && i == w::TRSP {
                        wr.varlen((w::TRSP - prev) as u64, 4);
                        for k in 0..3 {
                            wr.varlen(zigzag(i64::from(words[w::TRSP + k])), 4);
                        }
                        wr.u32(words[w::TRSP + 3] as u32);
                        prev = w::TRSP_LAST;
                        i = w::TRSP_LAST + 1;
                        continue;
                    }
                    if words[i] != 0 {
                        wr.varlen((i - prev) as u64, 4);
                        wr.varlen(zigzag(i64::from(words[i])), 6);
                        prev = i;
                    }
                    i += 1;
                }
                wr.varlen(0, 4);
            }
            self.seq = self.seq.wrapping_add(1);
            let h = Notify { kind: packet::NOTIFY_DATA, fragment: 0, seq: self.seq, ack: self.recv, mask: self.mask };
            let mut b = h.bytes().to_vec();
            b.extend_from_slice(&wr.data);
            fragments(&b)
        }

        /// Encodes one `NOTIFY_RELIABLE` slice packet as Fusion 2's client sends it.
        fn reliable_packet(&mut self, id: ReliableId, payload: &[u8]) -> Vec<u8> {
            self.seq = self.seq.wrapping_add(1);
            let h = Notify { kind: packet::NOTIFY_RELIABLE, fragment: 0, seq: self.seq, ack: self.recv, mask: self.mask };
            let mut b = Vec::with_capacity(packet::HEADER + RELIABLE_ID_SIZE + payload.len());
            b.extend_from_slice(&h.bytes());
            b.extend_from_slice(&id.bytes());
            b.extend_from_slice(payload);
            b
        }

        /// Sends reliable data as Fusion 2's client does (`SendReliableDataToPlayer` when
        /// `target >= 0`, `SendReliableDataToServer` when `target == -1`).
        fn send_reliable(&mut self, s: &mut Server, now: i64, master: i32, target: i32, key: [i32; 4], data: &[u8]) {
            self.reliable_send_id += 1;
            let send_id = self.reliable_send_id;
            for chunk in data.chunks(RELIABLE_SLICE_MAX) {
                self.reliable_send_seq = self.reliable_send_seq.wrapping_add(1);
                let id = ReliableId {
                    sequence: u64::from(self.reliable_send_seq),
                    slice_len: chunk.len() as i32,
                    total_len: data.len() as i32,
                    source: self.player - 1,
                    source_send: send_id,
                    target,
                    key,
                    padding: 0,
                };
                let p = self.reliable_packet(id, chunk);
                let _ = s.packet(self.actor, &p, now, master);
            }
        }

        fn recv(&mut self, out: &Out) {
            for (to, p) in out {
                if *to != self.actor || (p[0] != packet::NOTIFY_DATA && p[0] != packet::NOTIFY_RELIABLE) {
                    continue;
                }
                let h = Notify::read(p).unwrap();
                if h.kind == packet::NOTIFY_RELIABLE {
                    assert_eq!(h.fragment, 0, "Fusion 2 only dispatches unfragmented NOTIFY_RELIABLE packets");
                }
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
                if h.kind == packet::NOTIFY_RELIABLE {
                    self.read_reliable(&whole[packet::HEADER..]);
                } else {
                    self.read(&whole[packet::HEADER..]);
                }
            }
        }

        /// Reads a `NOTIFY_RELIABLE` body as Fusion 2's `ReliableBuffer` + `NetworkRunner` do.
        fn read_reliable(&mut self, b: &[u8]) {
            let id = ReliableId::read(b).unwrap();
            let slice_len = id.slice_len as usize;
            let data = b[RELIABLE_ID_SIZE..RELIABLE_ID_SIZE + slice_len].to_vec();
            let seq = id.sequence as u32;
            let d = seq.wrapping_sub(self.reliable_recv_seq) as i32;
            assert!(d > 0, "unexpected duplicate/old reliable seq {seq} (at {})", self.reliable_recv_seq);
            let slice = ReliableSlice { id, data };
            let mut ready = Vec::new();
            if d == 1 {
                self.reliable_recv_seq = seq;
                ready.push(slice);
                while let Some(next) = self.reliable_waiting.remove(&self.reliable_recv_seq.wrapping_add(1)) {
                    self.reliable_recv_seq = self.reliable_recv_seq.wrapping_add(1);
                    ready.push(next);
                }
            } else {
                self.reliable_waiting.insert(seq, slice);
            }
            for s in ready {
                let k = (s.id.source, s.id.source_send);
                let buf = self.reliable_parts.entry(k).or_default();
                buf.extend_from_slice(&s.data);
                if buf.len() == s.id.total_len as usize {
                    let full = self.reliable_parts.remove(&k).unwrap();
                    self.reliable.push((s.id.source, s.id.target, s.id.key, full));
                }
            }
        }

        /// Reads a packet as Fusion 2's client does (Simulation.OnNotifyData).
        fn read(&mut self, b: &[u8]) {
            let mut r = BitReader::new(b);
            let head = Head::read(&mut r).unwrap();
            self.tick = head.tick;
            r.align();
            for _ in 0..head.messages {
                let m = Message::read(&mut r).unwrap();
                if m.flags & msg::UNRELIABLE == 0 {
                    if m.seq != self.delivered_seq + 1 {
                        continue;
                    }
                    self.delivered_seq += 1;
                }
                self.rpcs.push(m.data.clone());
            }
            r.align();
            self.feedback = (0..4).map(|_| unzigzag(r.varlen(6).unwrap()) as f64 / 256.0).collect();
            for _ in 0..head.destroys {
                let id = r.varlen(8).unwrap() as u32;
                self.objects.remove(&id);
                self.destroyed.push(id);
            }
            let mut id = 0i64;
            for _ in 0..head.updates {
                id += unzigzag(r.varlen(6).unwrap());
                let oid = id as u32;
                let mut at = 0;
                if r.bool().unwrap() {
                    let mut h = vec![0i32; w::HEADER];
                    h[0] = oid as i32;
                    for v in h.iter_mut().skip(1) {
                        if r.bool().unwrap() {
                            *v = unzigzag(r.varlen(6).unwrap()) as i32;
                        }
                    }
                    let n = (h[w::COUNTS] & 0xffff) as usize;
                    let o = self.objects.entry(oid).or_insert_with(|| vec![0; n]);
                    o[..w::HEADER].copy_from_slice(&h);
                    at = w::HEADER - 1;
                }
                let o = self.objects.get_mut(&oid).expect("object update before its create header");
                let block = trsp(o[w::FLAGS], o.len());
                loop {
                    let step = r.varlen(4).unwrap() as i32;
                    if step <= 0 {
                        break;
                    }
                    at += step as usize;
                    if block && at == w::TRSP {
                        for k in 0..3 {
                            o[w::TRSP + k] = unzigzag(r.varlen(4).unwrap()) as i32;
                        }
                        o[w::TRSP + 3] = r.u32().unwrap() as i32;
                        at = w::TRSP_LAST;
                        continue;
                    }
                    let v = unzigzag(r.varlen(6).unwrap()) as i32;
                    if at >= w::READ_ONLY {
                        o[at] = v;
                    }
                }
            }
            assert!(r.left() < 8, "{} bits left over", r.left());
        }

        fn ack(&mut self, s: &mut Server, now: i64) {
            let h = Notify { kind: packet::NOTIFY_ACKS, fragment: 0, seq: 0, ack: self.recv, mask: self.mask };
            s.packet(self.actor, &h.bytes(), now, 1);
        }

        /// The players its PlayerData objects list: (player, player object, actor).
        fn players(&self) -> Vec<(i32, i32, i32)> {
            self.objects
                .values()
                .filter(|o| (o[w::TYPE], o[w::TYPE + 1]) == PLAYER_DATA_TYPE)
                .map(|o| (o[w::HEADER], o[w::HEADER + 1], o[w::HEADER + 2]))
                .collect()
        }
    }

    fn server() -> Server {
        Server::new_v2([60, 60, 60, 60], 4)
    }

    fn object(id: u32, owner: i32, n: usize, value: i32, flags: i32) -> Vec<i32> {
        let mut o = vec![0; n];
        o[w::ID] = id as i32;
        o[w::COUNTS] = n as i32;
        o[w::TYPE] = 7;
        o[w::FLAGS] = flags;
        o[w::STATE_AUTHORITY] = owner;
        o[w::HEADER] = value;
        o
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

    /// The first packet Fusion 2.0.9 got from the Fusion 1 layout started reading a
    /// message 64 bits in: the body starts with the 8-byte header.
    #[test]
    fn a_packet_starts_with_the_header_then_messages() {
        let mut s = server();
        let mut a = Client::new(1);
        a.connect(&mut s, 0);
        let out = s.tick(20);
        let (_, p) = out.iter().find(|(to, _)| *to == 1).unwrap();
        let body = &p[packet::HEADER..];
        let head = Head::read(&mut BitReader::new(body)).unwrap();
        assert_eq!((head.messages, head.cells, head.destroys), (0, 0, 0));
        assert_eq!(head.tick, s.tick);
        // The runtime config, scene info, physics info and a's player data.
        assert_eq!(head.updates, 4);
        a.recv(&out);
        let rc = &a.objects[&RUNTIME_CONFIG];
        assert_eq!(&rc[rc::TICK_RATES..rc::TICK_RATES + 4], &[60, 60, 60, 60]);
        assert_eq!(rc[rc::PLAYER_MAX], 4);
        assert_eq!(rc[rc::TOPOLOGY], TOPOLOGY_SHARED);
        assert_eq!(rc[rc::MASTER], a.player, "actor 1 is the room's master");
        assert_eq!(a.objects[&SCENE_INFO][w::STATE_AUTHORITY], MASTER_CLIENT);
        assert_eq!(a.objects[&SCENE_INFO].len(), w::HEADER + 13);
        assert_eq!(a.objects[&RUNTIME_CONFIG][w::FLAGS], f::STRUCT);
        assert_eq!(a.players(), vec![(a.player, 0, 1)]);
    }

    #[test]
    fn players_join_and_see_each_others_objects() {
        let mut s = server();
        let (mut a, mut b) = (Client::new(1), Client::new(2));
        a.connect(&mut s, 0);
        b.connect(&mut s, 5);
        assert_eq!((a.player, b.player), (1, 2));
        let id = a.counter << 19 | 1;
        let o = object(id, a.player, 40, 77, 2);
        a.send(&mut s, 10, &[(id, o.clone(), true)], &[], vec![]);
        let out = s.tick(20);
        a.recv(&out);
        b.recv(&out);
        assert_eq!(b.objects[&id][w::HEADER], 77);
        assert_eq!(b.objects[&id][w::STATE_AUTHORITY], a.player);
        assert_eq!(b.players(), vec![(1, 0, 1), (2, 0, 2)]);

        // An update reaches b; b can't change a's object.
        let mut o2 = o.clone();
        o2[w::HEADER] = 78;
        a.send(&mut s, 30, &[(id, o2, false)], &[], vec![]);
        let mut forged = o.clone();
        forged[w::HEADER] = 1;
        b.send(&mut s, 31, &[(id, forged, false)], &[], vec![]);
        a.ack(&mut s, 32);
        b.ack(&mut s, 32);
        let out = s.tick(40);
        b.recv(&out);
        assert_eq!(b.objects[&id][w::HEADER], 78);

        // The player object, by message.
        let mut set = Message::internal(kind::SET_PLAYER_OBJECT, &[id as i32], 3, None);
        set.flags |= msg::TARGET_SERVER;
        a.send(&mut s, 45, &[], &[], vec![set]);
        b.ack(&mut s, 46);
        let out = s.tick(60);
        b.recv(&out);
        assert_eq!(b.players(), vec![(1, id as i32, 1), (2, 0, 2)]);

        // a leaves: its object goes (DestroyWhenStateAuthorityLeaves), and its player data
        // with it; b is the master client now.
        s.packet(a.actor, &[packet::COMMAND, packet::DISCONNECT, 3], 61, 2);
        b.ack(&mut s, 62);
        let out = s.tick(80);
        b.recv(&out);
        assert!(!b.objects.contains_key(&id));
        assert_eq!(b.players(), vec![(2, 0, 2)]);
        assert_eq!(b.objects[&RUNTIME_CONFIG][rc::MASTER], b.player);
    }

    #[test]
    fn the_master_client_writes_the_scene_info() {
        let mut s = server();
        let (mut a, mut b) = (Client::new(1), Client::new(2));
        a.connect(&mut s, 0);
        b.connect(&mut s, 0);
        let mut scene = vec![0; w::HEADER + 13];
        scene[w::HEADER] = 1;
        scene[w::HEADER + 1] = 42;
        let mut forged = scene.clone();
        forged[w::HEADER + 1] = 666;
        // b isn't the master (actor 1 is): its write doesn't count.
        b.send(&mut s, 5, &[(SCENE_INFO, forged, false)], &[], vec![]);
        a.send(&mut s, 6, &[(SCENE_INFO, scene, false)], &[], vec![]);
        let out = s.tick(20);
        b.recv(&out);
        assert_eq!(b.objects[&SCENE_INFO][w::HEADER + 1], 42);
        assert!(s.log.iter().any(|l| l.contains("scene info from player 0")), "{:?}", s.log);
    }

    #[test]
    fn transforms_go_as_their_block() {
        let mut s = server();
        let (mut a, mut b) = (Client::new(1), Client::new(2));
        a.connect(&mut s, 0);
        b.connect(&mut s, 0);
        let id = a.counter << 19 | 3;
        let mut o = object(id, a.player, 40, 5, f::HAS_MAIN_TRSP);
        o[w::TRSP..w::TRSP + 4].copy_from_slice(&[1024, -2048, 7, 0x7abc_def0]);
        o[w::TRSP_LAST + 1] = 99;
        a.send(&mut s, 1, &[(id, o, true)], &[], vec![]);
        let out = s.tick(20);
        b.recv(&out);
        let got = &b.objects[&id];
        assert_eq!(&got[w::TRSP..w::TRSP + 4], &[1024, -2048, 7, 0x7abc_def0]);
        assert_eq!(got[w::TRSP_LAST + 1], 99);
        assert_eq!(got[w::HEADER], 5);
    }

    #[test]
    fn lost_packets_are_resent_and_rpcs_routed() {
        let mut s = Server::new_v2([30, 30, 30, 30], 4);
        let (mut a, mut b, mut c) = (Client::new(1), Client::new(2), Client::new(3));
        a.connect(&mut s, 0);
        b.connect(&mut s, 0);
        c.connect(&mut s, 0);
        let id = a.counter << 19 | 1;
        a.send(&mut s, 1, &[(id, object(id, a.player, 30, 5, 0), true)], &[], vec![rpc(b"all", None), rpc(b"to c", Some(c.player))]);
        // The first state packet to b is lost: b never acknowledges it.
        let lost = s.tick(40);
        c.recv(&lost);
        let later = s.tick(80);
        let Some((_, p)) = later.iter().find(|(to, _)| *to == b.actor) else { panic!() };
        let h = Notify::read(p).unwrap();
        b.recv = h.seq;
        b.mask = 1;
        b.ack(&mut s, 81);
        let again = s.tick(120);
        b.recv(&again);
        assert_eq!(b.objects.get(&id).map(|o| o[w::HEADER]), Some(5));
        assert_eq!(b.rpcs, vec![b"all".to_vec()]);
        assert_eq!(c.rpcs, vec![b"all".to_vec(), b"to c".to_vec()]);
    }

    #[test]
    fn authority_moves_on_request() {
        let mut s = server();
        let (mut a, mut b) = (Client::new(1), Client::new(2));
        a.connect(&mut s, 0);
        b.connect(&mut s, 0);
        let id = a.counter << 19 | 1;
        a.send(&mut s, 1, &[(id, object(id, a.player, 30, 1, f::ALLOW_STATE_AUTHORITY_OVERRIDE), true)], &[], vec![]);
        let out = s.tick(20);
        a.recv(&out);
        a.ack(&mut s, 21);
        let mut req = Message::internal(kind::REQUEST_STATE_AUTHORITY, &[id as i32, 1], 3, None);
        req.flags |= msg::TARGET_SERVER;
        b.send(&mut s, 25, &[], &[], vec![req]);
        let out = s.tick(40);
        a.recv(&out);
        assert_eq!(a.objects[&id][w::STATE_AUTHORITY], b.player);
        a.ack(&mut s, 41);
        b.send(&mut s, 45, &[(id, object(id, b.player, 30, 9, 0x10), false)], &[], vec![]);
        a.send(&mut s, 46, &[(id, object(id, a.player, 30, 2, 0x10), false)], &[], vec![]);
        let out = s.tick(60);
        a.recv(&out);
        assert_eq!(a.objects[&id][w::HEADER], 9);
    }

    #[test]
    fn big_states_go_over_several_packets() {
        let mut s = Server::new_v2([60, 60, 60, 60], 2);
        let (mut a, mut b) = (Client::new(1), Client::new(2));
        a.connect(&mut s, 0);
        b.connect(&mut s, 0);
        // More objects than a packet header can count (255), and more than 4 KB of them.
        let objects: Vec<(u32, Vec<i32>, bool)> = (1..300)
            .map(|i| {
                let id = a.counter << 19 | i;
                let mut o = object(id, a.player, 30, 0, 0);
                for (k, v) in o.iter_mut().enumerate().skip(w::HEADER) {
                    *v = 1000 + (i as i32) * 7 + k as i32;
                }
                (id, o, true)
            })
            .collect();
        for (n, chunk) in objects.chunks(100).enumerate() {
            a.send(&mut s, 1 + n as i64, chunk, &[], vec![]);
        }
        for t in 0..12 {
            let out = s.tick(20 + t * 20);
            assert!(out.iter().filter(|(to, _)| *to == b.actor).count() >= 1);
            b.recv(&out);
            b.ack(&mut s, 25 + t * 20);
        }
        for (id, o, _) in &objects {
            assert_eq!(b.objects.get(id).map(|x| x[w::HEADER + 5]), Some(o[w::HEADER + 5]));
        }
    }

    #[test]
    fn a_client_object_never_seen_without_its_header_stops_the_read() {
        let mut s = server();
        let mut a = Client::new(1);
        a.connect(&mut s, 0);
        let id = a.counter << 19 | 9;
        a.send(&mut s, 1, &[(id, object(id, a.player, 30, 1, 0), false)], &[], vec![rpc(b"x", None)]);
        assert!(!s.objects.contains_key(&id));
        assert!(s.log.iter().any(|l| l.contains("didn't parse (its objects")), "{:?}", s.log);
    }

    #[test]
    fn rates_resolve_from_the_selection() {
        assert_eq!(resolve_rates(60, 0, 0, 0), [60, 60, 60, 60]);
        assert_eq!(resolve_rates(64, 1, 0, 2), [64, 64, 32, 16]);
        assert_eq!(fixed(-0.5 / 256.0), zigzag(-1));
        assert_eq!(fixed(0.010), zigzag(3));
    }

    #[test]
    fn reliable_data_reaches_its_target_player() {
        let mut s = server();
        let (mut a, mut b) = (Client::new(1), Client::new(2));
        a.connect(&mut s, 0);
        b.connect(&mut s, 5);
        // Same wire shape Dumb Ways to Build sends: player 1 sends its account JSON to
        // player 0 (target = 0), and player 0 replies with the list to player 1 (target = 1).
        let p1_info = br#"{"Platform":1,"Username":"Timbera","PlatformUserId":"76561201321775147"}"#;
        b.send_reliable(&mut s, 10, 1, 0, [20, 0, 0, 1], p1_info);
        let out = s.tick(20);
        a.recv(&out);
        b.recv(&out);
        assert_eq!(a.reliable, vec![(1, 0, [20, 0, 0, 1], p1_info.to_vec())]);
        assert!(b.reliable.is_empty());
        assert!(
            s.log.iter().any(|l| l.contains("reliable data from player 1 to player 0 (actor 1): 72 bytes, key [20, 0, 0, 1], send #1")),
            "{:?}",
            s.log
        );

        let all_info = br#"[{"Platform":1,"Username":"Timber"},{"Platform":1,"Username":"Timbera"}]"#;
        a.send_reliable(&mut s, 30, 1, 1, [21, 0, 0, 0], all_info);
        let out = s.tick(40);
        a.recv(&out);
        b.recv(&out);
        assert_eq!(b.reliable, vec![(0, 1, [21, 0, 0, 0], all_info.to_vec())]);
        assert!(
            s.log.iter().any(|l| l.contains("reliable data from player 0 to player 1 (actor 2): 72 bytes, key [21, 0, 0, 0], send #1")),
            "{:?}",
            s.log
        );
    }

    #[test]
    fn reliable_data_to_server_goes_to_the_master_client() {
        let mut s = server();
        let (mut a, mut b) = (Client::new(1), Client::new(2));
        a.connect(&mut s, 0);
        b.connect(&mut s, 5);
        // SendReliableDataToServer sets target = -1 (PlayerRef.None.PlayerId).
        b.send_reliable(&mut s, 10, 1, -1, [20, 0, 0, 1], b"hello host");
        let out = s.tick(20);
        a.recv(&out);
        b.recv(&out);
        assert_eq!(a.reliable, vec![(1, 0, [20, 0, 0, 1], b"hello host".to_vec())]);
        assert!(
            s.log.iter().any(|l| l.contains("reliable data from player 1 to session -> master player 0 (actor 1)")),
            "{:?}",
            s.log
        );

        // Master leaves; actor 2 (player 1) becomes master and receives a third player's ToServer data.
        s.packet(a.actor, &[packet::COMMAND, packet::DISCONNECT, 3], 25, 2);
        let mut c = Client::new(3);
        c.connect(&mut s, 30);
        c.send_reliable(&mut s, 35, 2, -1, [20, 0, 0, 0], b"to new master");
        let out = s.tick(40);
        b.recv(&out);
        assert_eq!(b.reliable, vec![(0, 1, [20, 0, 0, 0], b"to new master".to_vec())]);
    }

    #[test]
    fn reliable_data_reorders_resequences_per_receiver_and_resends_on_loss() {
        let mut s = Server::new_v2([30, 30, 30, 30], 4);
        let (mut a, mut b, mut c) = (Client::new(1), Client::new(2), Client::new(3));
        a.connect(&mut s, 0);
        b.connect(&mut s, 0);
        c.connect(&mut s, 0);

        // b sends a 2-slice payload to a, but slice 2 arrives before slice 1.
        let id1 = ReliableId { sequence: 1, slice_len: 5, total_len: 10, source: 1, source_send: 1, target: 0, key: [1, 2, 3, 4], padding: 0 };
        let id2 = ReliableId { sequence: 2, slice_len: 5, total_len: 10, source: 1, source_send: 1, target: 0, key: [1, 2, 3, 4], padding: 0 };
        let p2 = b.reliable_packet(id2, b"world");
        let p1 = b.reliable_packet(id1, b"hello");
        s.packet(b.actor, &p2, 5, 1);
        // Held back until slice 1 arrives.
        let out = s.tick(40);
        a.recv(&out);
        assert!(a.reliable.is_empty());

        // Now slice 1 arrives, and c also sends its own sequence #1 to a.
        s.packet(b.actor, &p1, 45, 1);
        c.send_reliable(&mut s, 46, 1, 0, [9, 0, 0, 0], b"from c");
        // Drop the first NOTIFY_RELIABLE packet to a, deliver the rest, then ack only what arrived.
        let out = s.tick(80);
        let mut dropped_one = false;
        let partial: Out = out
            .into_iter()
            .filter(|(to, p)| {
                if *to == a.actor && p[0] == packet::NOTIFY_RELIABLE && !dropped_one {
                    dropped_one = true;
                    return false;
                }
                true
            })
            .collect();
        assert!(dropped_one);
        a.recv(&partial);
        // a got slices 2 and 3 out of order (buffered in a.reliable_waiting), still missing slice 1.
        assert!(a.reliable.is_empty());
        a.ack(&mut s, 85);

        // Next tick resends the lost slice 1; a delivers both transfers cleanly with monotonic sequences 1, 2, 3.
        let out = s.tick(120);
        a.recv(&out);
        assert_eq!(a.reliable_recv_seq, 3);
        assert_eq!(
            a.reliable,
            vec![(1, 0, [1, 2, 3, 4], b"helloworld".to_vec()), (2, 0, [9, 0, 0, 0], b"from c".to_vec())]
        );
    }

    #[test]
    fn reliable_data_oversized_slice_is_resliced_unfragmented() {
        let mut s = server();
        let (mut a, mut b) = (Client::new(1), Client::new(2));
        a.connect(&mut s, 0);
        b.connect(&mut s, 0);
        let big: Vec<u8> = (0..2500).map(|i| (i % 251) as u8).collect();
        let id = ReliableId {
            sequence: 1,
            slice_len: big.len() as i32,
            total_len: big.len() as i32,
            source: 1,
            source_send: 1,
            target: 0,
            key: [7, 8, 9, 10],
            padding: 0,
        };
        let p = b.reliable_packet(id, &big);
        s.packet(b.actor, &p, 5, 1);
        let out = s.tick(20);
        let rel_pkts: Vec<&Vec<u8>> = out.iter().filter(|(to, p)| *to == a.actor && p[0] == packet::NOTIFY_RELIABLE).map(|(_, p)| p).collect();
        assert_eq!(rel_pkts.len(), 3, "2500 bytes splits into 1088 + 1088 + 324");
        assert!(rel_pkts.iter().all(|p| p[1] == 0 && p.len() <= packet::HEADER + RELIABLE_ID_SIZE + RELIABLE_SLICE_MAX));
        a.recv(&out);
        assert_eq!(a.reliable, vec![(1, 0, [7, 8, 9, 10], big)]);
    }

    #[test]
    fn fuzz_relayed_packets() {
        use crate::photon::tests::{rounds, Rng};
        let mut s = server();
        let (mut a, mut b) = (Client::new(1), Client::new(2));
        a.connect(&mut s, 0);
        b.connect(&mut s, 0);
        let base = a.counter << 19;
        let mut trsp_obj = object(base | 3, a.player, 40, 1, f::HAS_MAIN_TRSP);
        trsp_obj[w::TRSP] = 5;
        let mut seeds = a.packets(&[(base | 1, object(base | 1, 1, 64, 5, 0), true), (base | 3, trsp_obj, true)], &[], vec![rpc(b"r", None)]);
        seeds.extend(b.packets(&[], &[base | 1], vec![]));
        seeds.extend(a.packets(&[(base | 1, object(base | 1, 1, 64, 7, 0), false)], &[], vec![]));
        let rel_id = ReliableId { sequence: 1, slice_len: 12, total_len: 12, source: 1, source_send: 1, target: 0, key: [20, 0, 0, 1], padding: 0 };
        seeds.push(b.reliable_packet(rel_id, b"reliablefuzz"));
        let mut rng = Rng(0x5eed_0205);
        for i in 0..rounds() {
            let now = 10 + i as i64;
            let p = rng.mutate(&seeds[i % seeds.len()]);
            let _ = s.packet(1 + (i % 3) as i32, &p, now, 1);
            if i % 16 == 0 {
                let _ = s.tick(now);
            }
        }
        let words: usize = s.objects.values().map(|o| o.words.len()).sum();
        assert!(words <= ROOM_WORDS, "{words} words held");
    }
}
