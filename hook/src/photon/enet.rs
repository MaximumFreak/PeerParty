//! Photon's ENet dialect over UDP, as a sans-IO state machine: feed it datagrams and
//! the time, take datagrams and delivered payloads out.
//!
//! Port of Luxon's `src/enet/enet_protocol.cpp`, `enet_peer_core.cpp`,
//! `enet_channel.cpp` and `enet_server.cpp` (BSD-3-Clause, see LICENSE-Luxon):
//! 12-byte big-endian datagram header (peer id, CRC flag 0/204, command count, sent
//! time, challenge), 12-byte command headers, VerifyConnect handshake, acks echoing the
//! sender's time, ordered reliable channels with fragments, unreliable sequenced and
//! unsequenced delivery, reliable-unsequenced (Exit Games' extension), resends with an
//! RTT estimate and a timeout. Differences from Luxon, each from how the real client
//! behaves:
//! a data command arriving while the handshake ack is still in flight completes the
//! handshake (the client only sends data after VerifyConnect), acks in a datagram are
//! handled before the rest, and fragments are kept in plain maps instead of windows.
//! Datagram encryption (header type 1, negotiated only with Photon Cloud's
//! encryption-mode auth) isn't supported; such datagrams are dropped.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

pub mod ct {
    pub const ACK: u8 = 1;
    pub const CONNECT: u8 = 2;
    pub const VERIFY_CONNECT: u8 = 3;
    pub const DISCONNECT: u8 = 4;
    pub const PING: u8 = 5;
    pub const RELIABLE: u8 = 6;
    pub const UNRELIABLE: u8 = 7;
    pub const FRAGMENT: u8 = 8;
    pub const UNSEQUENCED: u8 = 11;
    /// Not handled (Photon servers don't send it to clients); kept to document the value.
    #[allow(dead_code)]
    pub const SERVER_TIME: u8 = 12;
    pub const RELIABLE_UNSEQUENCED: u8 = 14;
    pub const FRAGMENT_UNSEQUENCED: u8 = 15;
    pub const ACK_UNSEQUENCED: u8 = 16;
}

const FLAG_RELIABLE: u8 = 1;
const FLAG_UNSEQUENCED: u8 = 2;
pub const CONTROL: u8 = 0xFF;
const HEADER: usize = 12;
const CMD_HEADER: usize = 12;
const MAX_CMDS: usize = 99;
const CRC_TYPE: u8 = 204;
const MAX_ASSEMBLY: usize = 500 * 1024;
const WINDOW: u32 = 8192;
const MAX_RESENDS: u32 = 7;
const DISCONNECT_TIMEOUT: i64 = 10_000;
const PING_INTERVAL: i64 = 1000;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cmd {
    pub ty: u8,
    pub ch: u8,
    pub flags: u8,
    pub reserved: u8,
    pub seq: u32,
    pub ack_seq: u32,
    pub ack_time: u32,
    pub unrel_seq: u32,
    pub group: u32,
    pub frag_start: u32,
    pub frag_count: u32,
    pub frag_no: u32,
    pub frag_total: u32,
    pub frag_off: u32,
    pub payload: Vec<u8>,
}

impl Cmd {
    fn new(ty: u8, ch: u8, flags: u8) -> Cmd {
        Cmd { ty, ch, flags, reserved: 4, ..Cmd::default() }
    }

    fn extra(&self) -> usize {
        match self.ty {
            ct::ACK | ct::ACK_UNSEQUENCED => 8,
            ct::UNRELIABLE | ct::UNSEQUENCED => 4,
            ct::FRAGMENT | ct::FRAGMENT_UNSEQUENCED => 20,
            _ => 0,
        }
    }

    pub fn wire_len(&self) -> usize {
        CMD_HEADER + self.extra() + self.payload.len()
    }

    fn reliable(&self) -> bool {
        self.flags & FLAG_RELIABLE != 0
    }

    /// Reliable-unsequenced commands have their own sequence space.
    fn unsequenced(&self) -> bool {
        self.flags & FLAG_UNSEQUENCED != 0
    }

    fn write(&self, w: &mut Vec<u8>) {
        w.extend_from_slice(&[self.ty, self.ch, self.flags, self.reserved]);
        w.extend_from_slice(&(self.wire_len() as u32).to_be_bytes());
        w.extend_from_slice(&self.seq.to_be_bytes());
        let fields: &[u32] = match self.ty {
            ct::ACK | ct::ACK_UNSEQUENCED => &[self.ack_seq, self.ack_time],
            ct::UNRELIABLE => &[self.unrel_seq],
            ct::UNSEQUENCED => &[self.group],
            ct::FRAGMENT | ct::FRAGMENT_UNSEQUENCED => {
                &[self.frag_start, self.frag_count, self.frag_no, self.frag_total, self.frag_off]
            }
            _ => &[],
        };
        for f in fields {
            w.extend_from_slice(&f.to_be_bytes());
        }
        w.extend_from_slice(&self.payload);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Header {
    pub peer_id: i16,
    pub crc: bool,
    pub sent_time: u32,
    pub challenge: u32,
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn crc_table() -> &'static [u32; 256] {
    use std::sync::OnceLock;
    static T: OnceLock<[u32; 256]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            }
            *v = c;
        }
        t
    })
}

/// Photon's CRC: CRC-32 table, start 0xFFFFFFFF, no final xor, CRC field zeroed.
pub fn crc(data: &[u8]) -> u32 {
    let t = crc_table();
    data.iter().fold(0xFFFF_FFFFu32, |c, b| (c >> 8) ^ t[((c as u8) ^ b) as usize])
}

pub fn parse(data: &[u8]) -> Result<(Header, Vec<Cmd>), &'static str> {
    if data.len() < HEADER {
        return Err("datagram too short");
    }
    let ty = data[2];
    if ty != 0 && ty != CRC_TYPE {
        return Err("unsupported datagram type (encrypted datagrams aren't supported)");
    }
    let hdr = Header {
        peer_id: i16::from_be_bytes([data[0], data[1]]),
        crc: ty == CRC_TYPE,
        sent_time: be32(data, 4),
        challenge: be32(data, 8),
    };
    let count = data[3] as usize;
    let mut off = HEADER;
    if hdr.crc {
        if data.len() < HEADER + 4 {
            return Err("CRC flag without a CRC");
        }
        let got = be32(data, HEADER);
        let mut copy = data.to_vec();
        copy[HEADER..HEADER + 4].fill(0);
        if crc(&copy) != got {
            return Err("CRC mismatch");
        }
        off += 4;
    }
    if count > MAX_CMDS {
        return Err("too many commands");
    }
    let mut cmds = Vec::with_capacity(count);
    for _ in 0..count {
        if off + CMD_HEADER > data.len() {
            return Err("truncated command header");
        }
        let len = be32(data, off + 4) as usize;
        if len < CMD_HEADER || off + len > data.len() {
            return Err("bad command length");
        }
        let mut c = Cmd {
            ty: data[off],
            ch: data[off + 1],
            flags: data[off + 2],
            reserved: data[off + 3],
            seq: be32(data, off + 8),
            ..Cmd::default()
        };
        let extra = c.extra();
        if len < CMD_HEADER + extra {
            return Err("command missing fields");
        }
        let f = off + CMD_HEADER;
        match c.ty {
            ct::ACK | ct::ACK_UNSEQUENCED => {
                c.ack_seq = be32(data, f);
                c.ack_time = be32(data, f + 4);
            }
            ct::UNRELIABLE => c.unrel_seq = be32(data, f),
            ct::UNSEQUENCED => c.group = be32(data, f),
            ct::FRAGMENT | ct::FRAGMENT_UNSEQUENCED => {
                c.frag_start = be32(data, f);
                c.frag_count = be32(data, f + 4);
                c.frag_no = be32(data, f + 8);
                c.frag_total = be32(data, f + 12);
                c.frag_off = be32(data, f + 16);
            }
            _ => {}
        }
        c.payload = data[f + extra..off + len].to_vec();
        cmds.push(c);
        off += len;
    }
    Ok((hdr, cmds))
}

pub fn build(hdr: &Header, cmds: &[Cmd]) -> Vec<u8> {
    let mut w = Vec::with_capacity(HEADER + 4 + cmds.iter().map(Cmd::wire_len).sum::<usize>());
    w.extend_from_slice(&hdr.peer_id.to_be_bytes());
    w.push(if hdr.crc { CRC_TYPE } else { 0 });
    w.push(cmds.len() as u8);
    w.extend_from_slice(&hdr.sent_time.to_be_bytes());
    w.extend_from_slice(&hdr.challenge.to_be_bytes());
    if hdr.crc {
        w.extend_from_slice(&[0; 4]);
    }
    for c in cmds {
        c.write(&mut w);
    }
    if hdr.crc {
        let v = crc(&w);
        w[HEADER..HEADER + 4].copy_from_slice(&v.to_be_bytes());
    }
    w
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Connecting,
    Connected,
    Disconnected,
    /// Disconnected because reliable commands went unacknowledged.
    TimedOut,
}

#[derive(Default)]
struct Channel {
    out_rel_seq: u32,
    out_unrel_seq: u32,
    out_unseq_rel_seq: u32,
    out_reliable: VecDeque<Cmd>,
    out_unreliable: VecDeque<Cmd>,
    in_rel_seq: u32,
    in_unrel_seq: u32,
    in_reliable: BTreeMap<u32, Cmd>,
    in_unreliable: BTreeMap<u32, Cmd>,
    in_unsequenced: VecDeque<Cmd>,
    unseq_done: u32,
    unseq_seen: BTreeSet<u32>,
    unseq_frags: HashMap<u32, Cmd>,
}

struct Sent {
    cmd: Cmd,
    sent_at: i64,
    count: u32,
    rto: i64,
    deadline: i64,
}

/// One delivered message.
#[derive(Debug, PartialEq)]
pub struct Delivery {
    pub channel: u8,
    pub reliable: bool,
    pub payload: Vec<u8>,
}

pub struct Peer {
    pub state: State,
    pub peer_id: i16,
    challenge: u32,
    crc: bool,
    mtu: usize,
    channel_count: u8,
    channels: HashMap<u8, Channel>,
    acks: Vec<Cmd>,
    sent: Vec<Sent>,
    rtt: i64,
    rtt_var: i64,
    last_ack: i64,
    out_group: u32,
    in_group: u32,
    group_window: [u32; 4],
    pub last_receive: i64,
    is_client: bool,
}

impl Peer {
    fn blank(peer_id: i16, challenge: u32, mtu: usize, channel_count: u8, now: i64) -> Peer {
        Peer {
            state: State::Connecting,
            peer_id,
            challenge,
            crc: false,
            mtu: mtu.clamp(576, 1500),
            channel_count,
            channels: HashMap::new(),
            acks: Vec::new(),
            sent: Vec::new(),
            rtt: 200,
            rtt_var: 5,
            last_ack: now,
            out_group: 0,
            in_group: 0,
            group_window: [0; 4],
            last_receive: now,
            is_client: false,
        }
    }

    /// Server side: a datagram from an unknown address that carries Connect.
    /// Answers with VerifyConnect (32 bytes, the assigned peer id first).
    pub fn accept(hdr: &Header, cmds: &[Cmd], peer_id: i16, now: i64) -> Option<Peer> {
        let connect = cmds.iter().find(|c| c.ty == ct::CONNECT)?;
        let (mut mtu, mut channels) = (1200, 2);
        if connect.payload.len() >= 12 {
            mtu = u16::from_be_bytes([connect.payload[2], connect.payload[3]]) as usize;
            channels = connect.payload[11].max(1);
        }
        let mut p = Peer::blank(peer_id, hdr.challenge, mtu, channels, now);
        p.crc = hdr.crc;
        let mut v = Cmd::new(ct::VERIFY_CONNECT, CONTROL, FLAG_RELIABLE);
        v.payload = vec![0; 32];
        v.payload[..2].copy_from_slice(&peer_id.to_be_bytes());
        p.queue_reliable(v);
        Some(p)
    }

    /// Client side (tests and probes): queues Connect like the Photon client does.
    pub fn connect(challenge: u32, channel_count: u8, now: i64) -> Peer {
        let mut p = Peer::blank(-1, challenge, 1200, channel_count, now);
        p.is_client = true;
        let mut c = Cmd::new(ct::CONNECT, CONTROL, FLAG_RELIABLE);
        // The constants every Photon client sends (Luxon `send_connect`).
        let mut pl = vec![0u8; 32];
        pl[2..4].copy_from_slice(&1200u16.to_be_bytes());
        pl[6] = 128;
        pl[11] = channel_count;
        pl[22] = 19;
        pl[23] = 136;
        pl[27] = 2;
        pl[31] = 2;
        c.payload = pl;
        p.queue_reliable(c);
        p
    }

    fn channel(&mut self, ch: u8) -> Option<&mut Channel> {
        if ch != CONTROL && ch >= self.channel_count {
            return None;
        }
        Some(self.channels.entry(ch).or_default())
    }

    fn queue_reliable(&mut self, mut c: Cmd) {
        let unseq = c.unsequenced();
        let Some(ch) = self.channel(c.ch) else { return };
        if c.seq == 0 {
            c.seq = if unseq {
                ch.out_unseq_rel_seq += 1;
                ch.out_unseq_rel_seq
            } else {
                ch.out_rel_seq += 1;
                ch.out_rel_seq
            };
        }
        ch.out_reliable.push_back(c);
    }

    /// Queues one message. Big reliable messages are split into fragments; unreliable
    /// ones that don't fit are sent reliably (as the Photon client does).
    pub fn send(&mut self, channel: u8, payload: &[u8], reliable: bool, unsequenced: bool) -> bool {
        if self.state != State::Connected || (channel != CONTROL && channel >= self.channel_count) {
            return false;
        }
        let frag_len = self.mtu - HEADER - 36;
        if payload.len() <= frag_len {
            let (ty, flags) = match (reliable, unsequenced) {
                (true, false) => (ct::RELIABLE, FLAG_RELIABLE),
                (true, true) => (ct::RELIABLE_UNSEQUENCED, FLAG_RELIABLE | FLAG_UNSEQUENCED),
                (false, false) => (ct::UNRELIABLE, 0),
                (false, true) => (ct::UNSEQUENCED, FLAG_UNSEQUENCED),
            };
            let mut c = Cmd::new(ty, channel, flags);
            c.payload = payload.to_vec();
            if reliable {
                self.queue_reliable(c);
            } else {
                let unseq_group = if unsequenced {
                    self.out_group += 1;
                    Some(self.out_group)
                } else {
                    None
                };
                let ch = self.channel(channel).unwrap();
                match unseq_group {
                    Some(g) => c.group = g,
                    None => {
                        c.seq = ch.out_rel_seq;
                        ch.out_unrel_seq += 1;
                        c.unrel_seq = ch.out_unrel_seq;
                    }
                }
                ch.out_unreliable.push_back(c);
            }
            return true;
        }
        let ch = self.channel(channel).unwrap();
        let start = if unsequenced { ch.out_unseq_rel_seq } else { ch.out_rel_seq } + 1;
        let count = payload.len().div_ceil(frag_len) as u32;
        for i in 0..count {
            let off = i as usize * frag_len;
            let end = (off + frag_len).min(payload.len());
            let mut c = if unsequenced {
                Cmd::new(ct::FRAGMENT_UNSEQUENCED, channel, FLAG_RELIABLE | FLAG_UNSEQUENCED)
            } else {
                Cmd::new(ct::FRAGMENT, channel, FLAG_RELIABLE)
            };
            c.frag_start = start;
            c.frag_count = count;
            c.frag_no = i;
            c.frag_total = payload.len() as u32;
            c.frag_off = off as u32;
            c.payload = payload[off..end].to_vec();
            self.queue_reliable(c);
        }
        true
    }

    /// Server-initiated disconnect: queue the Disconnect command and stop.
    pub fn disconnect(&mut self, now: i64) -> Vec<Vec<u8>> {
        if matches!(self.state, State::Disconnected | State::TimedOut) {
            return Vec::new();
        }
        self.queue_reliable(Cmd::new(ct::DISCONNECT, CONTROL, FLAG_RELIABLE));
        let out = self.flush(now);
        self.state = State::Disconnected;
        out
    }

    /// One received datagram (already parsed).
    pub fn handle(&mut self, hdr: &Header, cmds: Vec<Cmd>, now: i64) {
        if matches!(self.state, State::Disconnected | State::TimedOut) || hdr.challenge != self.challenge {
            return;
        }
        self.last_receive = now;
        if hdr.crc {
            self.crc = true;
        }
        // Acks first: the client puts them first anyway, and the VerifyConnect ack
        // must land before the Init that follows it.
        let (acks, rest): (Vec<Cmd>, Vec<Cmd>) =
            cmds.into_iter().partition(|c| c.ty == ct::ACK || c.ty == ct::ACK_UNSEQUENCED);
        for c in acks {
            self.on_ack(&c, now);
        }
        for c in rest {
            if c.reliable() {
                let mut a = Cmd::new(
                    if c.unsequenced() { ct::ACK_UNSEQUENCED } else { ct::ACK },
                    c.ch,
                    0,
                );
                a.reserved = c.reserved;
                a.ack_seq = c.seq;
                a.ack_time = hdr.sent_time;
                self.acks.push(a);
            }
            self.execute(c);
        }
    }

    fn on_ack(&mut self, c: &Cmd, now: i64) {
        self.last_ack = now;
        let mut rtt = now - c.ack_time as i64;
        if !(0..=self.rtt * 4).contains(&rtt) {
            rtt = self.rtt * 4;
        }
        let unseq = c.ty == ct::ACK_UNSEQUENCED;
        let Some(i) = self.sent.iter().position(|s| {
            s.cmd.seq == c.ack_seq && s.cmd.ch == c.ch && s.cmd.unsequenced() == unseq
        }) else {
            return;
        };
        let s = self.sent.remove(i);
        match s.cmd.ty {
            ct::VERIFY_CONNECT if self.state == State::Connecting => self.state = State::Connected,
            ct::CONNECT => self.rtt = rtt.max(15),
            _ => {
                // RFC 6298-style smoothing, as Luxon / the Photon client do.
                self.rtt_var -= self.rtt_var / 4;
                self.rtt_var += (rtt - self.rtt).abs() / 4;
                self.rtt += (rtt - self.rtt) / 8;
            }
        }
    }

    fn execute(&mut self, c: Cmd) {
        match c.ty {
            ct::VERIFY_CONNECT => {
                if self.is_client && self.state == State::Connecting && c.payload.len() >= 2 {
                    self.peer_id = i16::from_be_bytes([c.payload[0], c.payload[1]]);
                    self.state = State::Connected;
                }
            }
            ct::DISCONNECT => self.state = State::Disconnected,
            ct::RELIABLE | ct::RELIABLE_UNSEQUENCED | ct::UNRELIABLE | ct::UNSEQUENCED | ct::FRAGMENT
            | ct::FRAGMENT_UNSEQUENCED => {
                if self.state == State::Connecting && !self.is_client {
                    self.state = State::Connected;
                }
                if self.state != State::Connected {
                    return;
                }
                if c.ty == ct::FRAGMENT || c.ty == ct::FRAGMENT_UNSEQUENCED {
                    if c.frag_no >= c.frag_count
                        || c.frag_count > WINDOW
                        || c.frag_total as usize > MAX_ASSEMBLY
                        || c.frag_off >= c.frag_total
                        || c.frag_off as usize + c.payload.len() > c.frag_total as usize
                    {
                        return;
                    }
                    let (start, count, total, ch, seqd) =
                        (c.frag_start, c.frag_count, c.frag_total, c.ch, c.ty == ct::FRAGMENT);
                    if self.queue_incoming(c) {
                        self.try_assemble(ch, start, count, total, seqd);
                    }
                } else {
                    self.queue_incoming(c);
                }
            }
            // Connect retransmits, pings and server-time requests only need the ack.
            _ => {}
        }
    }

    fn queue_incoming(&mut self, c: Cmd) -> bool {
        let reliable = c.reliable();
        let unseq = c.unsequenced();
        if !reliable && unseq {
            // Unsequenced groups: a 128-wide window of seen group numbers.
            let g = c.group;
            let idx = g % 128;
            if g >= self.in_group + 128 {
                self.in_group = g - idx;
                self.group_window = [0; 4];
            } else if g < self.in_group || self.group_window[(idx / 32) as usize] & (1 << (idx % 32)) != 0 {
                return false;
            }
            self.group_window[(idx / 32) as usize] |= 1 << (idx % 32);
            let Some(ch) = self.channel(c.ch) else { return false };
            ch.in_unsequenced.push_back(c);
            return true;
        }
        let Some(ch) = self.channel(c.ch) else { return false };
        if reliable && unseq {
            if c.seq <= ch.unseq_done || ch.unseq_seen.contains(&c.seq) || c.seq > ch.unseq_done + WINDOW {
                return false;
            }
            if c.seq == ch.unseq_done + 1 {
                ch.unseq_done += 1;
            } else {
                ch.unseq_seen.insert(c.seq);
            }
            while ch.unseq_seen.remove(&(ch.unseq_done + 1)) {
                ch.unseq_done += 1;
            }
            if c.ty == ct::FRAGMENT_UNSEQUENCED {
                ch.unseq_frags.insert(c.seq, c);
            } else {
                ch.in_unsequenced.push_back(c);
            }
            return true;
        }
        if reliable {
            if c.seq <= ch.in_rel_seq || c.seq > ch.in_rel_seq + WINDOW || ch.in_reliable.contains_key(&c.seq) {
                return false;
            }
            ch.in_reliable.insert(c.seq, c);
            return true;
        }
        if c.seq < ch.in_rel_seq || c.unrel_seq <= ch.in_unrel_seq || c.unrel_seq > ch.in_unrel_seq + WINDOW {
            return false;
        }
        if ch.in_unreliable.contains_key(&c.unrel_seq) {
            return false;
        }
        ch.in_unreliable.insert(c.unrel_seq, c);
        true
    }

    fn try_assemble(&mut self, chn: u8, start: u32, count: u32, total: u32, sequenced: bool) {
        let Some(ch) = self.channel(chn) else { return };
        fn store(ch: &Channel, s: u32, sequenced: bool) -> Option<&Cmd> {
            if sequenced { ch.in_reliable.get(&s) } else { ch.unseq_frags.get(&s) }
        }
        let Some(first) = store(ch, start, sequenced) else { return };
        if first.ty != ct::FRAGMENT && first.ty != ct::FRAGMENT_UNSEQUENCED {
            return;
        }
        if (start..start + count).any(|s| store(ch, s, sequenced).is_none()) {
            return;
        }
        let mut full = vec![0u8; total as usize];
        let mut head = Cmd::default();
        for s in start..start + count {
            let f = if sequenced { ch.in_reliable.remove(&s) } else { ch.unseq_frags.remove(&s) }.unwrap();
            let off = f.frag_off as usize;
            full[off..off + f.payload.len()].copy_from_slice(&f.payload);
            if s == start {
                head = f;
            }
        }
        head.payload = full;
        if sequenced {
            head.ty = ct::RELIABLE;
            // Delivered at the start sequence; consumes the whole run.
            head.seq = start + count - 1;
            ch.in_reliable.insert(start, head);
        } else {
            head.ty = ct::RELIABLE_UNSEQUENCED;
            ch.in_unsequenced.push_back(head);
        }
    }

    /// Takes the next delivered message, in Photon's order: unsequenced, then
    /// unreliable whose reliable predecessor arrived, then the next reliable.
    pub fn dispatch(&mut self) -> Option<Delivery> {
        if self.state != State::Connected {
            return None;
        }
        let mut keys: Vec<u8> = self.channels.keys().copied().collect();
        keys.sort();
        for k in keys {
            let ch = self.channels.get_mut(&k).unwrap();
            if let Some(c) = ch.in_unsequenced.pop_front() {
                return Some(Delivery { channel: k, reliable: c.reliable(), payload: c.payload });
            }
            // Unreliable, oldest first, dropping ones overtaken by reliable traffic.
            while let Some((&u, c)) = ch.in_unreliable.iter().next() {
                if u <= ch.in_unrel_seq || c.seq < ch.in_rel_seq {
                    ch.in_unreliable.remove(&u);
                    continue;
                }
                if c.seq <= ch.in_rel_seq {
                    let c = ch.in_unreliable.remove(&u).unwrap();
                    ch.in_unrel_seq = c.unrel_seq;
                    return Some(Delivery { channel: k, reliable: false, payload: c.payload });
                }
                break;
            }
            let next = ch.in_rel_seq + 1;
            if let Some(c) = ch.in_reliable.get(&next) {
                if c.ty == ct::FRAGMENT {
                    continue;
                }
                let c = ch.in_reliable.remove(&next).unwrap();
                ch.in_rel_seq = c.seq;
                if c.ty == ct::RELIABLE {
                    return Some(Delivery { channel: k, reliable: true, payload: c.payload });
                }
            }
        }
        None
    }

    /// Builds the datagrams to send now: acks, resends, pings, queued commands.
    pub fn flush(&mut self, now: i64) -> Vec<Vec<u8>> {
        if matches!(self.state, State::Disconnected | State::TimedOut) {
            return Vec::new();
        }
        // Resends.
        let mut resend = Vec::new();
        let mut i = 0;
        while i < self.sent.len() {
            let s = &self.sent[i];
            if now - s.sent_at > s.rto {
                if s.count > MAX_RESENDS || now > s.deadline {
                    self.state = State::TimedOut;
                    return Vec::new();
                }
                resend.push(self.sent.remove(i));
            } else {
                i += 1;
            }
        }
        // Ping when idle, so a vanished client times out.
        if self.state == State::Connected
            && self.sent.is_empty()
            && resend.is_empty()
            && now - self.last_ack > PING_INTERVAL
            && self.channels.values().all(|c| c.out_reliable.is_empty())
        {
            self.queue_reliable(Cmd::new(ct::PING, CONTROL, FLAG_RELIABLE));
            self.last_ack = now;
        }
        let mut queue: Vec<(Cmd, Option<Sent>)> = Vec::new();
        for a in self.acks.drain(..) {
            queue.push((a, None));
        }
        for s in resend {
            let c = s.cmd.clone();
            queue.push((c, Some(s)));
        }
        let mut keys: Vec<u8> = self.channels.keys().copied().collect();
        // Control channel first, then data channels in order.
        keys.sort_by_key(|k| if *k == CONTROL { -1 } else { *k as i32 });
        for k in keys {
            let ch = self.channels.get_mut(&k).unwrap();
            for c in ch.out_reliable.drain(..) {
                queue.push((c, Some(Sent { cmd: Cmd::default(), sent_at: 0, count: 0, rto: 0, deadline: 0 })));
            }
            for c in ch.out_unreliable.drain(..) {
                queue.push((c, None));
            }
        }
        let hdr = Header { peer_id: self.peer_id, crc: self.crc, sent_time: now as u32, challenge: self.challenge };
        let room = self.mtu - HEADER - if self.crc { 4 } else { 0 };
        let mut out = Vec::new();
        let mut batch: Vec<Cmd> = Vec::new();
        let mut used = 0;
        for (c, sent) in queue {
            let len = c.wire_len();
            if !batch.is_empty() && (used + len > room || batch.len() == MAX_CMDS) {
                out.push(build(&hdr, &batch));
                batch.clear();
                used = 0;
            }
            if let Some(mut s) = sent {
                if s.count == 0 {
                    s.cmd = c.clone();
                    s.rto = (self.rtt + 4 * self.rtt_var).max(50);
                    s.deadline = now + DISCONNECT_TIMEOUT;
                } else {
                    s.rto *= 2;
                }
                s.count += 1;
                s.sent_at = now;
                self.sent.push(s);
            }
            used += len;
            batch.push(c);
        }
        if !batch.is_empty() {
            out.push(build(&hdr, &batch));
        }
        out
    }

    /// Milliseconds until `flush` has resend or ping work, for the server's timer.
    pub fn next_deadline(&self, now: i64) -> i64 {
        let mut t = now + PING_INTERVAL;
        for s in &self.sent {
            t = t.min(s.sent_at + s.rto + 1);
        }
        t
    }

    pub fn has_outgoing(&self) -> bool {
        !self.acks.is_empty()
            || self.channels.values().any(|c| !c.out_reliable.is_empty() || !c.out_unreliable.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Delivers every datagram from `a` to `b` (optionally dropping some).
    fn pump(from: &mut Peer, to: &mut Peer, now: i64, drop: impl Fn(usize) -> bool) -> usize {
        let dgs = from.flush(now);
        let n = dgs.len();
        for (i, d) in dgs.into_iter().enumerate() {
            if drop(i) {
                continue;
            }
            let (h, cmds) = parse(&d).unwrap();
            to.handle(&h, cmds, now);
        }
        n
    }

    fn pair() -> (Peer, Peer) {
        let mut client = Peer::connect(0x1234_5678, 2, 0);
        let first = client.flush(0);
        let (h, cmds) = parse(&first[0]).unwrap();
        assert_eq!(h.peer_id, -1);
        let mut server = Peer::accept(&h, &cmds, 7, 0).unwrap();
        server.handle(&h, cmds, 0);
        pump(&mut server, &mut client, 1, |_| false);
        assert_eq!(client.state, State::Connected);
        assert_eq!(client.peer_id, 7);
        assert_eq!(server.state, State::Connecting);
        pump(&mut client, &mut server, 2, |_| false);
        assert_eq!(server.state, State::Connected);
        (client, server)
    }

    #[test]
    fn datagram_round_trip_and_crc() {
        let mut c = Cmd::new(ct::FRAGMENT, 1, FLAG_RELIABLE);
        c.seq = 3;
        c.frag_start = 3;
        c.frag_count = 2;
        c.frag_total = 10;
        c.payload = vec![1, 2, 3];
        let mut a = Cmd::new(ct::ACK, 0, 0);
        a.ack_seq = 9;
        a.ack_time = 77;
        for crc in [false, true] {
            let h = Header { peer_id: 5, crc, sent_time: 100, challenge: 42 };
            let d = build(&h, &[c.clone(), a.clone()]);
            assert_eq!(parse(&d).unwrap(), (h, vec![c.clone(), a.clone()]));
            if crc {
                let mut bad = d.clone();
                *bad.last_mut().unwrap() ^= 1;
                assert!(parse(&bad).is_err());
            }
        }
        // Header layout per Luxon: id, type, count, time, challenge.
        let d = build(&Header { peer_id: -1, crc: false, sent_time: 1, challenge: 2 }, &[]);
        assert_eq!(d, [0xFF, 0xFF, 0, 0, 0, 0, 0, 1, 0, 0, 0, 2]);
        assert!(parse(&d[..11]).is_err());
        assert!(parse(&[0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn handshake_and_ordered_reliable_delivery() {
        let (mut client, mut server) = pair();
        for i in 0..5u8 {
            assert!(client.send(0, &[i], true, false));
        }
        pump(&mut client, &mut server, 10, |_| false);
        let got: Vec<u8> = std::iter::from_fn(|| server.dispatch()).map(|d| d.payload[0]).collect();
        assert_eq!(got, [0, 1, 2, 3, 4]);
        // Wrong challenge is ignored.
        let d = build(&Header { peer_id: 7, crc: false, sent_time: 0, challenge: 1 }, &[]);
        let (h, cmds) = parse(&d).unwrap();
        server.handle(&h, cmds, 11);
        assert_eq!(server.last_receive, 10);
    }

    #[test]
    fn lost_reliable_is_resent_and_order_kept() {
        let (mut client, mut server) = pair();
        client.send(0, b"a", true, false);
        pump(&mut client, &mut server, 10, |_| true); // lost
        client.send(0, b"b", true, false);
        pump(&mut client, &mut server, 20, |_| false);
        assert_eq!(server.dispatch(), None, "b must wait for a");
        pump(&mut server, &mut client, 21, |_| false); // ack for b
        pump(&mut client, &mut server, 400, |_| false); // resend of a
        assert_eq!(server.dispatch().unwrap().payload, b"a");
        assert_eq!(server.dispatch().unwrap().payload, b"b");
        assert_eq!(server.dispatch(), None);
    }

    #[test]
    fn fragments_reassemble_both_ways() {
        let (mut client, mut server) = pair();
        let big: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        client.send(1, &big, true, false);
        client.send(1, b"after", true, false);
        let n = pump(&mut client, &mut server, 10, |_| false);
        assert!(n >= 5);
        assert_eq!(server.dispatch().unwrap().payload, big);
        assert_eq!(server.dispatch().unwrap().payload, b"after");
        server.send(0, &big, true, true);
        pump(&mut server, &mut client, 11, |_| false);
        assert_eq!(client.dispatch().unwrap().payload, big);
    }

    #[test]
    fn unreliable_and_unsequenced() {
        let (mut client, mut server) = pair();
        client.send(0, b"u1", false, false);
        client.send(0, b"u2", false, false);
        client.send(0, b"s1", false, true);
        pump(&mut client, &mut server, 10, |_| false);
        let got: Vec<Vec<u8>> = std::iter::from_fn(|| server.dispatch()).map(|d| d.payload).collect();
        assert_eq!(got, [b"s1".to_vec(), b"u1".to_vec(), b"u2".to_vec()]);
        // A duplicate of an old unreliable is dropped.
        let mut c = Cmd::new(ct::UNRELIABLE, 0, 0);
        c.unrel_seq = 1;
        c.payload = b"dup".to_vec();
        let d = build(&Header { peer_id: 7, crc: false, sent_time: 0, challenge: 0x1234_5678 }, &[c]);
        let (h, cmds) = parse(&d).unwrap();
        server.handle(&h, cmds, 11);
        assert_eq!(server.dispatch(), None);
    }

    #[test]
    fn silent_client_times_out_and_disconnect_is_seen() {
        let (mut client, mut server) = pair();
        let mut t = 10;
        while server.state == State::Connected && t < 60_000 {
            server.flush(t);
            t += 100;
        }
        assert_eq!(server.state, State::TimedOut);
        let (mut client2, mut server2) = pair();
        let dgs = server2.disconnect(5);
        assert_eq!(server2.state, State::Disconnected);
        for d in dgs {
            let (h, cmds) = parse(&d).unwrap();
            client2.handle(&h, cmds, 6);
        }
        assert_eq!(client2.state, State::Disconnected);
        let _ = &mut client;
    }
}
