//! Voice and data rooms on the LAN core, for the SDKs' voice chat (EOS RTC, see
//! eos/rtc.rs; PlayFab Party chat, see party/chat.rs). There is no voice server on a
//! LAN: a room is a core socket
//! `rtc:<room>` between the copies that joined it. Members announce themselves on the
//! control channel (join, here, leave); audio goes as raw 16-bit PCM frames on an
//! unreliable channel (no Opus: a LAN has room for 32 KB/s per speaker, and it keeps
//! PeerParty free of a codec), and room data (EOS_RTCData) on a reliable one.
//!
//! Sound devices: when a game leaves microphone and speakers to the SDK (EOS's
//! default; "manual audio" off), PeerParty does the SDK's part with Windows' waveIn /
//! waveOut (winmm): the microphone at 16 kHz mono in 20 ms frames, sent only while it
//! carries voice (a level gate with a short hang-over), and the room's voices mixed in
//! stereo to the speakers, each at its own volume and place left to right (`set_pan`,
//! for Vivox's positional channels). Which microphone and speakers: the game's choice
//! through its SDK (endpoints.rs), else Windows' default communications devices; a
//! change of choice, or of the default the choice follows, moves the open device. The
//! microphone is opened only while a room wants to send, and closed when none does.
//! `PEERPARTY_VOICE=0` turns the devices off (rooms, data and game-supplied audio still
//! work).
//!
//! The microphone goes through Windows' Voice Capture DSP when it can (aec.rs): echo
//! cancellation against whatever the speakers play, noise suppression and automatic
//! gain, on the same devices. Without it (or with `PEERPARTY_VOICE_AEC=0`) the plain
//! waveIn path below is used.
//!
//! A room can also narrow who it sends to and hears (`set_filters`, for Party's chat
//! permissions), hand a speaker's audio to the game instead of the speakers
//! (`set_no_play`), and send data to chosen members (`send_data_to`). EOS uses none of
//! these, so its rooms behave as before.
//!
//! Who is in a room: whoever says so, among the players this one plays with (the LAN, a
//! room code). A public player is a stranger, and a room's name is no secret (it is a
//! lobby's id, which a search hands out), so one is only let in, and only told the room
//! exists, while the core vouches for them (`Lan::knows`: we invited them, or they are
//! in a lobby with us). `sweep` keeps that up to date as players come into lobbies and
//! leave them.
//!
//! The capture/playback loop is the classic double-buffered waveIn/waveOut
//! pattern from Microsoft's Waveform Audio documentation
//! (learn.microsoft.com/windows/win32/multimedia/waveform-audio).

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use peerparty_core::{Event, Lan, PeerId, SendResult};

use crate::log;

mod aec;
mod com;
mod endpoints;

pub use endpoints::{Choice, Endpoint, Side, chosen, snapshot, start as watch_devices, version as devices_version};

pub const SOCKET_PREFIX: &str = "rtc:";
const CH_CTRL: u8 = 0;
const CH_AUDIO: u8 = 1;
const CH_DATA: u8 = 2;
const JOIN: u8 = 1;
const HERE: u8 = 2;
const LEAVE: u8 = 3;
const AUDIO: u8 = 4;
const DATA: u8 = 5;
/// A room's events waiting for the game: past this the oldest go (a game that never
/// takes them, or a player sending faster than it does).
const EVENTS_MAX: usize = 4096;

/// Device format: 16 kHz mono, 20 ms frames.
pub const DEVICE_RATE: u32 = 16_000;
const FRAME: usize = 320;
/// A speaker stops "speaking" this long after its last frame.
const SPEAKING_HOLD: Duration = Duration::from_millis(300);

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub rate: u32,
    pub channels: u32,
    /// Interleaved samples, `frames * channels` of them.
    pub samples: Vec<i16>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RoomEvent {
    Joined(PeerId),
    Left(PeerId),
    Audio(PeerId, Frame),
    Data(PeerId, Vec<u8>),
    Speaking(PeerId, bool),
}

struct Room {
    members: BTreeSet<PeerId>,
    events: VecDeque<RoomEvent>,
    sending: bool,
    receiving: bool,
    muted: HashSet<PeerId>,
    data_sending: bool,
    /// The SDK's sound devices are PeerParty's (not "manual audio").
    device_in: bool,
    device_out: bool,
    volume_out: f32,
    peer_volume: HashMap<PeerId, f32>,
    /// Where each speaker sits, -1 (left) to 1 (right).
    peer_pan: HashMap<PeerId, f32>,
    heard: HashMap<PeerId, Instant>,
    /// Only these members get our audio (`None`: all of them).
    send_to: Option<HashSet<PeerId>>,
    /// Only these members' audio is taken in (`None`: all but `muted`).
    hear: Option<HashSet<PeerId>>,
    /// Members whose audio goes to the game only (events), not to the speakers.
    no_play: HashSet<PeerId>,
    /// When we last sent audio, for "talking" indicators.
    last_sent: Option<Instant>,
    /// Public players told of the room since they became known to us (see `sweep`).
    greeted: HashSet<PeerId>,
    /// Members the player blocked: no audio or data either way (EOS BlockParticipant).
    blocked: HashSet<PeerId>,
}

impl Room {
    fn new(device_in: bool, device_out: bool) -> Room {
        Room {
            members: BTreeSet::new(),
            events: VecDeque::new(),
            sending: true,
            receiving: true,
            muted: HashSet::new(),
            data_sending: true,
            device_in,
            device_out,
            volume_out: 1.0,
            peer_volume: HashMap::new(),
            peer_pan: HashMap::new(),
            heard: HashMap::new(),
            send_to: None,
            hear: None,
            no_play: HashSet::new(),
            last_sent: None,
            greeted: HashSet::new(),
            blocked: HashSet::new(),
        }
    }

    /// Who gets our audio.
    fn audio_to(&self) -> Vec<PeerId> {
        self.members
            .iter()
            .copied()
            .filter(|p| self.send_to.as_ref().is_none_or(|t| t.contains(p)) && !self.blocked.contains(p))
            .collect()
    }

    /// Who gets the room's data.
    fn data_to(&self) -> Vec<PeerId> {
        self.members.iter().copied().filter(|p| !self.blocked.contains(p)).collect()
    }

    /// Whether a member's audio is taken in.
    fn hears(&self, p: PeerId) -> bool {
        self.receiving
            && self.hear.as_ref().is_none_or(|h| h.contains(&p))
            && !self.muted.contains(&p)
            && !self.blocked.contains(&p)
    }
}

#[derive(Default)]
struct State {
    rooms: HashMap<String, Room>,
    /// When `sweep` last ran.
    swept: Option<Instant>,
}

/// How often `sweep` looks at who the public players in and around the rooms are.
const SWEEP_EVERY: Duration = Duration::from_millis(500);
/// An audio frame's sample rate and channel count as a peer may give them: anything a
/// microphone or a game's mixer produces, and nothing that makes resampling one frame
/// cost what a second of sound should.
const RATES: std::ops::RangeInclusive<u32> = 8_000..=96_000;
const MAX_CHANNELS: u32 = 8;

static S: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);

fn socket(room: &str) -> String {
    format!("{SOCKET_PREFIX}{room}")
}

pub fn devices_allowed() -> bool {
    std::env::var("PEERPARTY_VOICE").map_or(true, |v| v.trim() != "0")
}

fn send(lan: &Lan, peer: PeerId, room: &str, ch: u8, data: &[u8], reliable: bool) -> SendResult {
    lan.send(peer, &socket(room), ch, data, reliable, reliable, true)
}

/// Joins `room` and tells every LAN player; the ones in it answer.
pub fn join(lan: &Lan, room: &str, device_in: bool, device_out: bool) {
    {
        let mut s = S.lock().unwrap();
        if s.rooms.contains_key(room) {
            return;
        }
        s.rooms.insert(room.to_string(), Room::new(device_in && devices_allowed(), device_out && devices_allowed()));
    }
    for (peer, _) in lan.peers() {
        // A stranger hears of the room from `sweep`, once there is reason to tell them.
        if lan.peer_trust(peer) == Some(peerparty_core::Trust::Public) {
            continue;
        }
        // Also answers a room channel the peer asked for while nobody here listened.
        lan.accept(peer, &socket(room));
        send(lan, peer, room, CH_CTRL, &[JOIN], true);
    }
    S.lock().unwrap().swept = None;
    log!("voice joined room {room} (devices: in={device_in} out={device_out}, allowed={})", devices_allowed());
    devices::update();
}

pub fn leave(lan: &Lan, room: &str) -> bool {
    let Some(r) = S.lock().unwrap().rooms.remove(room) else { return false };
    for peer in &r.members {
        send(lan, *peer, room, CH_CTRL, &[LEAVE], true);
    }
    log!("voice left room {room}");
    devices::update();
    true
}

pub fn joined(room: &str) -> bool {
    S.lock().unwrap().rooms.contains_key(room)
}

pub fn rooms() -> Vec<String> {
    S.lock().unwrap().rooms.keys().cloned().collect()
}

pub fn members(room: &str) -> Vec<PeerId> {
    S.lock().unwrap().rooms.get(room).map(|r| r.members.iter().copied().collect()).unwrap_or_default()
}

fn with_room<T>(room: &str, f: impl FnOnce(&mut Room) -> T) -> Option<T> {
    S.lock().unwrap().rooms.get_mut(room).map(f)
}

pub fn set_sending(room: &str, on: bool) -> bool {
    let r = with_room(room, |r| r.sending = on).is_some();
    devices::update();
    r
}

/// Audio from one participant (or everyone, `None`) on or off.
pub fn set_receiving(room: &str, peer: Option<PeerId>, on: bool) -> bool {
    with_room(room, |r| match peer {
        None => r.receiving = on,
        Some(p) if on => {
            r.muted.remove(&p);
        }
        Some(p) => {
            r.muted.insert(p);
        }
    })
    .is_some()
}

/// A participant blocked or unblocked: blocked, nothing goes either way between us,
/// audio or data (EOS's BlockParticipant), unlike muting, which only stops what we hear.
pub fn set_blocked(room: &str, peer: PeerId, on: bool) -> bool {
    with_room(room, |r| {
        if on {
            r.blocked.insert(peer);
        } else {
            r.blocked.remove(&peer);
        }
    })
    .is_some()
}

pub fn set_data_sending(room: &str, on: bool) -> bool {
    with_room(room, |r| r.data_sending = on).is_some()
}

/// Whether PeerParty runs the microphone and speakers for this room (still subject to
/// `PEERPARTY_VOICE`).
pub fn set_devices(room: &str, device_in: bool, device_out: bool) -> bool {
    let changed = with_room(room, |r| {
        let (i, o) = (device_in && devices_allowed(), device_out && devices_allowed());
        let changed = r.device_in != i || r.device_out != o;
        r.device_in = i;
        r.device_out = o;
        changed
    });
    if changed == Some(true) {
        log!("voice room {room}: devices in={device_in} out={device_out} (allowed={})", devices_allowed());
    }
    devices::update();
    changed.is_some()
}

/// Narrows who gets our audio and whose audio we take in (`None`: everyone).
pub fn set_filters(room: &str, send_to: Option<HashSet<PeerId>>, hear: Option<HashSet<PeerId>>) -> bool {
    with_room(room, |r| {
        r.send_to = send_to;
        r.hear = hear;
    })
    .is_some()
}

/// Members whose audio the game takes itself (room events only, no speakers).
pub fn set_no_play(room: &str, peers: HashSet<PeerId>) -> bool {
    with_room(room, |r| r.no_play = peers).is_some()
}

/// Whether we sent audio to the room in the last moment.
pub fn talking(room: &str) -> bool {
    with_room(room, |r| r.last_sent.is_some_and(|t| t.elapsed() <= SPEAKING_HOLD)).unwrap_or(false)
}

/// Speaker volume (EOS: 0-100, 50 = unchanged), for a participant or the whole room.
pub fn set_volume(room: &str, peer: Option<PeerId>, volume: f32) -> bool {
    let v = (volume / 50.0).clamp(0.0, 2.0);
    with_room(room, |r| match peer {
        None => r.volume_out = v,
        Some(p) => {
            r.peer_volume.insert(p, v);
        }
    })
    .is_some()
}

/// Where a participant's voice plays, from -1 (all left) through 0 (both sides alike)
/// to 1 (all right).
pub fn set_pan(room: &str, peer: PeerId, pan: f32) -> bool {
    let pan = if pan.is_finite() { pan.clamp(-1.0, 1.0) } else { 0.0 };
    with_room(room, |r| {
        r.peer_pan.insert(peer, pan);
    })
    .is_some()
}

/// The game's choice of microphone (`output` false) or speakers, for every room.
pub fn choose_device(output: bool, c: Choice) {
    endpoints::choose(output, c);
}

/// Left and right gains for a place, -1 (left) to 1 (right): the near side at full
/// volume, the far side down to a quarter (-12 dB) at the extreme, so a voice off to one
/// side is still heard by both ears (a fully one-sided voice is tiring on headphones).
/// The middle plays as a mono voice did.
pub fn pan_gains(pan: f32) -> [f32; 2] {
    let p = if pan.is_finite() { pan.clamp(-1.0, 1.0) } else { 0.0 };
    let far = 1.0 - 0.75 * p.abs();
    if p < 0.0 { [1.0, far] } else { [far, 1.0] }
}

/// A frame as 16 kHz stereo: its first two channels, or a mono frame on both sides.
pub fn to_stereo(f: &Frame) -> Vec<[i16; 2]> {
    let ch = f.channels.max(1) as usize;
    if ch < 2 {
        return to_device(f).into_iter().map(|s| [s, s]).collect();
    }
    let side = |i: usize| Frame { rate: f.rate, channels: 1, samples: f.samples.iter().skip(i).step_by(ch).copied().collect() };
    let (l, r) = (resample(&side(0), DEVICE_RATE), resample(&side(1), DEVICE_RATE));
    l.into_iter().zip(r).map(|(l, r)| [l, r]).collect()
}

fn encode_audio(f: &Frame) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + f.samples.len() * 2);
    out.push(AUDIO);
    out.extend_from_slice(&f.rate.to_le_bytes());
    out.push(f.channels as u8);
    for s in &f.samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn decode_audio(b: &[u8]) -> Option<Frame> {
    if b.len() < 6 || b[0] != AUDIO {
        return None;
    }
    let rate = u32::from_le_bytes(b[1..5].try_into().ok()?);
    let channels = b[5] as u32;
    // A rate of 1 made the resampler write 16,000 samples for each one sent.
    if !RATES.contains(&rate) || !(1..=MAX_CHANNELS).contains(&channels) {
        return None;
    }
    // A frame is a few hundredths of a second; one claiming over a second isn't one.
    if (b.len() - 6) / 2 > (rate * channels) as usize {
        return None;
    }
    let samples = b[6..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    Some(Frame { rate, channels, samples })
}

/// Sends a frame the game (or the microphone) produced to the room. False if not in it.
pub fn send_audio(lan: &Lan, room: &str, frame: &Frame) -> bool {
    let members = {
        let s = S.lock().unwrap();
        let Some(r) = s.rooms.get(room) else { return false };
        if !r.sending {
            return true;
        }
        r.audio_to()
    };
    with_room(room, |r| r.last_sent = Some(Instant::now()));
    let msg = encode_audio(frame);
    for p in members {
        send(lan, p, room, CH_AUDIO, &msg, false);
    }
    true
}

pub fn send_data(lan: &Lan, room: &str, data: &[u8]) -> bool {
    let members = {
        let s = S.lock().unwrap();
        let Some(r) = s.rooms.get(room) else { return false };
        if !r.data_sending {
            return true;
        }
        r.data_to()
    };
    let mut msg = Vec::with_capacity(data.len() + 1);
    msg.push(DATA);
    msg.extend_from_slice(data);
    for p in members {
        send(lan, p, room, CH_DATA, &msg, true);
    }
    true
}

/// Room data to chosen players, members yet or not (their room may still be
/// announcing itself). False if we're not in the room.
pub fn send_data_to(lan: &Lan, room: &str, peers: &[PeerId], data: &[u8]) -> bool {
    let Some(blocked) = with_room(room, |r| r.blocked.clone()) else { return false };
    let mut msg = Vec::with_capacity(data.len() + 1);
    msg.push(DATA);
    msg.extend_from_slice(data);
    for p in peers.iter().filter(|p| !blocked.contains(p)) {
        send(lan, *p, room, CH_DATA, &msg, true);
    }
    true
}

/// Plays audio the game mixed itself (Party's render stream) on the speakers, when a
/// room has them open; stereo stays stereo.
pub fn play_local(f: &Frame, gain: f32) {
    devices::play("", PeerId([0; 16]), f, [gain, gain]);
}

/// Core events: room sockets are always accepted; players who leave the LAN leave rooms.
pub fn on_event(lan: &Lan, ev: &Event) {
    match ev {
        Event::ChanRequest { peer, socket } if socket.starts_with(SOCKET_PREFIX) => {
            if lan.knows(*peer) {
                lan.accept(*peer, socket);
            }
        }
        // A player met after we joined a room hasn't heard our join: tell them now (two
        // copies that each joined before meeting would otherwise never see each other).
        // A stranger hears of it from `sweep`.
        Event::PeerJoined(peer) if lan.peer_trust(*peer) != Some(peerparty_core::Trust::Public) => {
            let rooms: Vec<String> = S.lock().unwrap().rooms.keys().cloned().collect();
            for room in rooms {
                lan.accept(*peer, &socket(&room));
                send(lan, *peer, &room, CH_CTRL, &[JOIN], true);
            }
        }
        Event::PeerLeft(peer) => {
            let mut s = S.lock().unwrap();
            for r in s.rooms.values_mut() {
                r.greeted.remove(peer);
                if r.members.remove(peer) {
                    r.events.push_back(RoomEvent::Left(*peer));
                }
            }
        }
        _ => {}
    }
}

/// Public players and the rooms, every `SWEEP_EVERY`: one the core no longer vouches
/// for (they left the lobby we shared, or we did) is out of every room; one it has
/// come to vouch for is told of each room, as a LAN player is on arriving. Their
/// answer, or their own join, makes them a member.
fn sweep(lan: &Lan) {
    {
        let mut s = S.lock().unwrap();
        if s.rooms.is_empty() || s.swept.is_some_and(|t| t.elapsed() < SWEEP_EVERY) {
            return;
        }
        s.swept = Some(Instant::now());
    }
    let strangers: Vec<(PeerId, bool)> = lan
        .peers()
        .into_iter()
        .filter(|(p, _)| lan.peer_trust(*p) == Some(peerparty_core::Trust::Public))
        .map(|(p, _)| (p, lan.knows(p)))
        .collect();
    if strangers.is_empty() {
        return;
    }
    let mut greet = Vec::new();
    {
        let mut s = S.lock().unwrap();
        for (name, r) in s.rooms.iter_mut() {
            for (p, known) in &strangers {
                if !known {
                    r.greeted.remove(p);
                    if r.members.remove(p) {
                        r.events.push_back(RoomEvent::Left(*p));
                    }
                } else if r.greeted.insert(*p) && !r.members.contains(p) {
                    greet.push((name.clone(), *p));
                }
            }
        }
    }
    for (room, p) in greet {
        lan.accept(p, &socket(&room));
        send(lan, p, &room, CH_CTRL, &[JOIN], true);
    }
}

/// Moves arrived room traffic into each room's events.
pub fn pump(lan: &Lan) {
    sweep(lan);
    // Whether each sender is someone a room takes anything from, asked once a pass.
    let mut known: HashMap<PeerId, bool> = HashMap::new();
    while let Some(p) = lan.receive_where(None, |s| s.starts_with(SOCKET_PREFIX)) {
        let room = &p.socket[SOCKET_PREFIX.len()..];
        let Some(&kind) = p.data.first() else { continue };
        // Saying "join", or sending audio, used to be all it took to be sent the
        // room's microphones.
        if kind != LEAVE && !*known.entry(p.peer).or_insert_with(|| lan.knows(p.peer)) {
            continue;
        }
        let mut reply = false;
        {
            let mut s = S.lock().unwrap();
            let Some(r) = s.rooms.get_mut(room) else { continue };
            match kind {
                JOIN | HERE => {
                    if r.members.insert(p.peer) {
                        r.events.push_back(RoomEvent::Joined(p.peer));
                    }
                    reply = kind == JOIN;
                }
                LEAVE => {
                    if r.members.remove(&p.peer) {
                        r.events.push_back(RoomEvent::Left(p.peer));
                    }
                }
                AUDIO => {
                    if !r.members.contains(&p.peer) {
                        r.members.insert(p.peer);
                        r.events.push_back(RoomEvent::Joined(p.peer));
                    }
                    if r.hears(p.peer) {
                        if let Some(f) = decode_audio(&p.data) {
                            if r.heard.insert(p.peer, Instant::now()).is_none() {
                                r.events.push_back(RoomEvent::Speaking(p.peer, true));
                            }
                            if r.device_out && !r.no_play.contains(&p.peer) {
                                let gain = r.volume_out * r.peer_volume.get(&p.peer).copied().unwrap_or(1.0);
                                let [left, right] = pan_gains(r.peer_pan.get(&p.peer).copied().unwrap_or(0.0));
                                devices::play(room, p.peer, &f, [gain * left, gain * right]);
                            }
                            r.events.push_back(RoomEvent::Audio(p.peer, f));
                        }
                    }
                }
                DATA if r.blocked.contains(&p.peer) => {}
                DATA => {
                    // What isn't taken piles up: a room's data is a game's small
                    // messages, and a game that has stopped taking them loses the oldest.
                    if r.events.len() >= EVENTS_MAX {
                        r.events.pop_front();
                    }
                    r.events.push_back(RoomEvent::Data(p.peer, p.data[1..].to_vec()))
                }
                _ => {}
            }
        }
        if reply {
            send(lan, p.peer, room, CH_CTRL, &[HERE], true);
        }
    }
    let now = Instant::now();
    let mut s = S.lock().unwrap();
    for r in s.rooms.values_mut() {
        let quiet: Vec<PeerId> =
            r.heard.iter().filter(|(_, t)| now.duration_since(**t) > SPEAKING_HOLD).map(|(p, _)| *p).collect();
        for p in quiet {
            r.heard.remove(&p);
            r.events.push_back(RoomEvent::Speaking(p, false));
        }
    }
}

/// The room's events since the last call, oldest first.
pub fn take(room: &str) -> Vec<RoomEvent> {
    with_room(room, |r| r.events.drain(..).collect()).unwrap_or_default()
}

/// Interleaved samples at any rate and channel count, as 16 kHz mono.
pub fn to_device(f: &Frame) -> Vec<i16> {
    resample(f, DEVICE_RATE)
}

/// Interleaved samples at any rate and channel count, as mono at `rate` (linear).
pub fn resample(f: &Frame, rate: u32) -> Vec<i16> {
    let ch = f.channels.max(1) as usize;
    let mono: Vec<i32> = f.samples.chunks(ch).map(|c| c.iter().map(|s| *s as i32).sum::<i32>() / c.len() as i32).collect();
    if mono.is_empty() || f.rate == 0 {
        return Vec::new();
    }
    if f.rate == rate {
        return mono.iter().map(|s| *s as i16).collect();
    }
    let n = (mono.len() as u64 * rate as u64 / f.rate as u64) as usize;
    (0..n)
        .map(|i| {
            let pos = i as f64 * f.rate as f64 / rate as f64;
            let a = pos.floor() as usize;
            let b = (a + 1).min(mono.len() - 1);
            let t = pos - a as f64;
            (mono[a.min(mono.len() - 1)] as f64 * (1.0 - t) + mono[b] as f64 * t) as i16
        })
        .collect()
}

/// The microphone's frame, sent to every room PeerParty does the devices for.
fn from_microphone(samples: &[i16]) {
    let Some(lan) = crate::net::lan() else { return };
    let rooms: Vec<String> = S
        .lock()
        .unwrap()
        .rooms
        .iter()
        .filter(|(_, r)| r.device_in && r.sending)
        .map(|(k, _)| k.clone())
        .collect();
    let f = Frame { rate: DEVICE_RATE, channels: 1, samples: samples.to_vec() };
    for room in rooms {
        send_audio(lan, &room, &f);
    }
}

/// Whether a 20 ms frame carries voice: its level against a fixed floor, held open for
/// 300 ms so words aren't clipped.
struct Gate {
    hold: u32,
}

impl Gate {
    fn open(&mut self, samples: &[i16]) -> bool {
        let n = samples.len().max(1) as f64;
        let rms = (samples.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / n).sqrt();
        if rms > 500.0 {
            self.hold = 15;
        } else if self.hold > 0 {
            self.hold -= 1;
        } else {
            return false;
        }
        true
    }
}

mod devices {
    //! waveIn / waveOut on the chosen devices, each on its own thread.
    use super::*;

    #[repr(C)]
    struct WaveFormatEx {
        format_tag: u16,
        channels: u16,
        samples_per_sec: u32,
        avg_bytes_per_sec: u32,
        block_align: u16,
        bits_per_sample: u16,
        cb_size: u16,
    }

    #[repr(C)]
    struct WaveHdr {
        data: *mut u8,
        buffer_length: u32,
        bytes_recorded: u32,
        user: usize,
        flags: u32,
        loops: u32,
        next: usize,
        reserved: usize,
    }

    #[link(name = "winmm")]
    unsafe extern "system" {
        fn waveInOpen(h: *mut usize, dev: u32, fmt: *const WaveFormatEx, cb: usize, inst: usize, flags: u32) -> u32;
        fn waveInPrepareHeader(h: usize, hdr: *mut WaveHdr, size: u32) -> u32;
        fn waveInUnprepareHeader(h: usize, hdr: *mut WaveHdr, size: u32) -> u32;
        fn waveInAddBuffer(h: usize, hdr: *mut WaveHdr, size: u32) -> u32;
        fn waveInStart(h: usize) -> u32;
        fn waveInReset(h: usize) -> u32;
        fn waveInClose(h: usize) -> u32;
        fn waveOutOpen(h: *mut usize, dev: u32, fmt: *const WaveFormatEx, cb: usize, inst: usize, flags: u32) -> u32;
        fn waveOutPrepareHeader(h: usize, hdr: *mut WaveHdr, size: u32) -> u32;
        fn waveOutUnprepareHeader(h: usize, hdr: *mut WaveHdr, size: u32) -> u32;
        fn waveOutWrite(h: usize, hdr: *mut WaveHdr, size: u32) -> u32;
        fn waveOutReset(h: usize) -> u32;
        fn waveOutClose(h: usize) -> u32;
        fn waveInGetNumDevs() -> u32;
        fn waveInMessage(h: usize, msg: u32, a: usize, b: usize) -> u32;
        fn waveOutMessage(h: usize, msg: u32, a: usize, b: usize) -> u32;
        fn waveOutGetNumDevs() -> u32;
    }

    /// The waveIn / waveOut number of endpoint `id` (DRV_QUERYFUNCTIONINSTANCEID gives
    /// each wave device's endpoint id).
    pub(super) fn wave_index(output: bool, id: &str) -> Option<u32> {
        const QUERY_ID: u32 = 0x0800 + 17;
        const QUERY_ID_SIZE: u32 = 0x0800 + 18;
        let message = |dev: u32, msg: u32, a: usize, b: usize| unsafe {
            if output { waveOutMessage(dev as usize, msg, a, b) } else { waveInMessage(dev as usize, msg, a, b) }
        };
        let count = if output { output_count() } else { input_count() };
        (0..count).find(|&dev| {
            let mut size = 0u32;
            if message(dev, QUERY_ID_SIZE, &mut size as *mut u32 as usize, 0) != 0 || !(2..=4096).contains(&size) {
                return false;
            }
            let mut buf = vec![0u16; size as usize / 2];
            if message(dev, QUERY_ID, buf.as_mut_ptr() as usize, size as usize) != 0 {
                return false;
            }
            let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..n]).eq_ignore_ascii_case(id)
        })
    }

    /// The wave device to open for an endpoint (the communications default when Windows
    /// can't say which one it is).
    fn wave_device(output: bool, ep: Option<&Endpoint>) -> u32 {
        ep.and_then(|e| wave_index(output, &e.id)).unwrap_or_else(|| communications(output))
    }

    fn label(ep: Option<&Endpoint>) -> String {
        ep.map_or_else(|| "the communications default".to_string(), |e| format!("'{}'", e.name))
    }

    /// The endpoints the devices should be on now: [microphone, speakers].
    fn targets() -> [Option<Endpoint>; 2] {
        [endpoints::effective(false), endpoints::effective(true)]
    }

    /// How often an open device checks whether it should move.
    const RECHECK: Duration = Duration::from_millis(250);

    /// The speakers open now: Some(endpoint id, or None for the communications
    /// default by number), None while closed.
    pub(super) static SPEAKERS_OPEN: Mutex<Option<Option<String>>> = Mutex::new(None);

    /// The communications default device (Sound settings), where voice chat belongs and
    /// where the echo canceller listens; the default device if Windows won't say.
    fn communications(output: bool) -> u32 {
        const DRVM_MAPPER_CONSOLEVOICECOM_GET: u32 = 0x2000 + 23;
        let (mut id, mut status) = (WAVE_MAPPER, 0u32);
        let (a, b) = (&mut id as *mut u32 as usize, &mut status as *mut u32 as usize);
        let r = if output {
            unsafe { waveOutMessage(WAVE_MAPPER as usize, DRVM_MAPPER_CONSOLEVOICECOM_GET, a, b) }
        } else {
            unsafe { waveInMessage(WAVE_MAPPER as usize, DRVM_MAPPER_CONSOLEVOICECOM_GET, a, b) }
        };
        if r == 0 { id } else { WAVE_MAPPER }
    }

    pub(super) fn input_count() -> u32 {
        unsafe { waveInGetNumDevs() }
    }

    pub(super) fn output_count() -> u32 {
        unsafe { waveOutGetNumDevs() }
    }

    const WAVE_MAPPER: u32 = u32::MAX;
    const WHDR_DONE: u32 = 1;
    const BUFFERS: usize = 4;
    const HDR: u32 = std::mem::size_of::<WaveHdr>() as u32;

    /// 16 kHz 16-bit PCM: mono for the microphone, stereo for the speakers.
    fn format(channels: u16) -> WaveFormatEx {
        WaveFormatEx {
            format_tag: 1, // WAVE_FORMAT_PCM
            channels,
            samples_per_sec: DEVICE_RATE,
            avg_bytes_per_sec: DEVICE_RATE * 2 * channels as u32,
            block_align: 2 * channels,
            bits_per_sample: 16,
            cb_size: 0,
        }
    }

    static WANT_IN: AtomicBool = AtomicBool::new(false);
    static WANT_OUT: AtomicBool = AtomicBool::new(false);
    static STARTED: std::sync::Once = std::sync::Once::new();
    /// Per (room, speaker) queues of 16 kHz stereo samples waiting to be played.
    static QUEUES: LazyLock<Mutex<HashMap<(String, PeerId), VecDeque<[i16; 2]>>>> = LazyLock::new(Mutex::default);
    /// At most this much audio waits per speaker (the rest is late and dropped).
    const MAX_QUEUE: usize = DEVICE_RATE as usize * 3 / 10;

    /// Opens or closes the devices to match what the rooms want.
    pub(super) fn update() {
        let (want_in, want_out) = {
            let s = S.lock().unwrap();
            (
                s.rooms.values().any(|r| r.device_in && r.sending),
                s.rooms.values().any(|r| r.device_out),
            )
        };
        WANT_IN.store(want_in, Ordering::Relaxed);
        WANT_OUT.store(want_out, Ordering::Relaxed);
        if want_in || want_out {
            endpoints::start();
            STARTED.call_once(|| {
                std::thread::spawn(capture_loop);
                std::thread::spawn(playback_loop);
            });
        }
    }

    /// Queues a frame for the speakers at `gain` [left, right].
    pub(super) fn play(room: &str, peer: PeerId, f: &Frame, gain: [f32; 2]) {
        let samples = to_stereo(f);
        let scale = |s: i16, g: f32| (s as f32 * g).clamp(-32768.0, 32767.0) as i16;
        let mut q = QUEUES.lock().unwrap();
        let queue = q.entry((room.to_string(), peer)).or_default();
        queue.extend(samples.iter().map(|[l, r]| [scale(*l, gain[0]), scale(*r, gain[1])]));
        while queue.len() > MAX_QUEUE {
            queue.pop_front();
        }
    }

    /// One frame of everyone's voices, added together (interleaved stereo).
    fn mix() -> Vec<i16> {
        let mut out = vec![0i32; FRAME * 2];
        let mut q = QUEUES.lock().unwrap();
        for queue in q.values_mut() {
            for o in out.chunks_exact_mut(2) {
                match queue.pop_front() {
                    Some([l, r]) => {
                        o[0] += l as i32;
                        o[1] += r as i32;
                    }
                    None => break,
                }
            }
        }
        q.retain(|_, v| !v.is_empty());
        out.iter().map(|s| (*s).clamp(-32768, 32767) as i16).collect()
    }

    struct Buffers {
        hdrs: Box<[WaveHdr; BUFFERS]>,
        _data: Vec<Vec<i16>>,
    }

    /// Buffers of `samples` 16-bit samples each.
    fn buffers(samples: usize) -> Buffers {
        let mut data: Vec<Vec<i16>> = (0..BUFFERS).map(|_| vec![0i16; samples]).collect();
        let hdrs = Box::new(std::array::from_fn(|i| WaveHdr {
            data: data[i].as_mut_ptr() as *mut u8,
            buffer_length: (samples * 2) as u32,
            bytes_recorded: 0,
            user: 0,
            flags: 0,
            loops: 0,
            next: 0,
            reserved: 0,
        }));
        Buffers { hdrs, _data: data }
    }

    /// Whether to try Windows' echo cancellation first (`PEERPARTY_VOICE_AEC=0` doesn't).
    fn aec_wanted() -> bool {
        std::env::var("PEERPARTY_VOICE_AEC").map_or(true, |v| v != "0")
    }

    /// The microphone through the Voice Capture DSP, while a room wants it and the
    /// devices stay `target`. False if the DSP couldn't start (waveIn takes over).
    fn capture_with_aec(gate: &mut Gate, target: &[Option<Endpoint>; 2]) -> bool {
        let Some(mic) = &target[0] else { return false };
        let Some(mut cap) = aec::Capture::open(&mic.id, target[1].as_ref().map(|e| e.id.as_str())) else { return false };
        log!(
            "voice: microphone {} open, through Windows' echo cancellation and noise suppression (speakers {} as its reference)",
            label(Some(mic)),
            label(target[1].as_ref())
        );
        let mut pending: Vec<i16> = Vec::with_capacity(FRAME * 4);
        let mut checked = Instant::now();
        while WANT_IN.load(Ordering::Relaxed) {
            if checked.elapsed() >= RECHECK {
                checked = Instant::now();
                if targets() != *target {
                    break;
                }
            }
            if !cap.read(&mut pending) {
                // A DSP that opens but can't read would otherwise be reopened at once,
                // over and over.
                std::thread::sleep(Duration::from_secs(1));
                break;
            }
            let mut at = 0;
            while pending.len() - at >= FRAME {
                let frame = &pending[at..at + FRAME];
                if gate.open(frame) {
                    from_microphone(frame);
                }
                at += FRAME;
            }
            pending.drain(..at);
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(cap);
        log!("voice: microphone closed");
        true
    }

    /// Waits while a room wants the device and the targets stay as they are.
    fn wait_while_unchanged(want: &AtomicBool, target: &[Option<Endpoint>; 2]) {
        while want.load(Ordering::Relaxed) && targets() == *target {
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn capture_loop() {
        let mut gate = Gate { hold: 0 };
        let mut aec = aec_wanted();
        loop {
            if !WANT_IN.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            let target = targets();
            if target[0].is_none() && input_count() == 0 {
                log!("voice: Windows has no microphone; sending nothing until one appears");
                wait_while_unchanged(&WANT_IN, &target);
                continue;
            }
            // Without an endpoint (Windows wouldn't list them) waveIn's default is tried.
            if aec && target[0].is_some() {
                if capture_with_aec(&mut gate, &target) {
                    continue;
                }
                aec = false;
            }
            let mut h = 0usize;
            let r = unsafe { waveInOpen(&mut h, wave_device(false, target[0].as_ref()), &format(1), 0, 0, 0) };
            if r != 0 {
                log!("voice: no microphone (waveInOpen {r}); sending nothing");
                wait_while_unchanged(&WANT_IN, &target);
                continue;
            }
            log!("voice: microphone {} open (16 kHz mono, no echo cancellation)", label(target[0].as_ref()));
            let mut b = buffers(FRAME);
            for hdr in b.hdrs.iter_mut() {
                unsafe {
                    waveInPrepareHeader(h, hdr, HDR);
                    waveInAddBuffer(h, hdr, HDR);
                }
            }
            unsafe { waveInStart(h) };
            let mut checked = Instant::now();
            while WANT_IN.load(Ordering::Relaxed) {
                if checked.elapsed() >= RECHECK {
                    checked = Instant::now();
                    if targets()[0] != target[0] {
                        break;
                    }
                }
                for hdr in b.hdrs.iter_mut() {
                    if hdr.flags & WHDR_DONE != 0 {
                        let n = hdr.bytes_recorded as usize / 2;
                        let samples = unsafe { std::slice::from_raw_parts(hdr.data as *const i16, n) }.to_vec();
                        unsafe { waveInAddBuffer(h, hdr, HDR) };
                        if gate.open(&samples) {
                            from_microphone(&samples);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            unsafe {
                waveInReset(h);
                for hdr in b.hdrs.iter_mut() {
                    waveInUnprepareHeader(h, hdr, HDR);
                }
                waveInClose(h);
            }
            log!("voice: microphone closed");
        }
    }

    fn playback_loop() {
        loop {
            if !WANT_OUT.load(Ordering::Relaxed) {
                QUEUES.lock().unwrap().clear();
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            let target = targets();
            let mut h = 0usize;
            let r = unsafe { waveOutOpen(&mut h, wave_device(true, target[1].as_ref()), &format(2), 0, 0, 0) };
            if r != 0 {
                log!("voice: no speakers (waveOutOpen {r}); voices aren't played");
                wait_while_unchanged(&WANT_OUT, &target);
                continue;
            }
            log!("voice: speakers {} open (16 kHz stereo)", label(target[1].as_ref()));
            *SPEAKERS_OPEN.lock().unwrap() = Some(target[1].as_ref().map(|e| e.id.clone()));
            let mut b = buffers(FRAME * 2);
            for hdr in b.hdrs.iter_mut() {
                unsafe { waveOutPrepareHeader(h, hdr, HDR) };
                hdr.flags |= WHDR_DONE;
            }
            let mut checked = Instant::now();
            while WANT_OUT.load(Ordering::Relaxed) {
                if checked.elapsed() >= RECHECK {
                    checked = Instant::now();
                    if targets()[1] != target[1] {
                        break;
                    }
                }
                for hdr in b.hdrs.iter_mut() {
                    if hdr.flags & WHDR_DONE != 0 {
                        let frame = mix();
                        unsafe {
                            std::ptr::copy_nonoverlapping(frame.as_ptr(), hdr.data as *mut i16, FRAME * 2);
                            hdr.flags &= !WHDR_DONE;
                            waveOutWrite(h, hdr, HDR);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            unsafe {
                waveOutReset(h);
                for hdr in b.hdrs.iter_mut() {
                    waveOutUnprepareHeader(h, hdr, HDR);
                }
                waveOutClose(h);
            }
            *SPEAKERS_OPEN.lock().unwrap() = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_frames_round_trip() {
        let f = Frame { rate: 48_000, channels: 2, samples: vec![1, -2, 300, -32768, 32767, 0] };
        assert_eq!(decode_audio(&encode_audio(&f)), Some(f));
        assert_eq!(decode_audio(&[AUDIO, 0, 0, 0, 0, 1]), None);
    }

    /// A frame says its own rate, and playing it means resampling to the device's: at a
    /// rate of 1 each sample sent became 16,000 to compute, on the game's thread.
    #[test]
    fn frames_with_absurd_formats_are_dropped() {
        let frame = |rate: u32, channels: u8, samples: usize| {
            let mut b = vec![AUDIO];
            b.extend_from_slice(&rate.to_le_bytes());
            b.push(channels);
            b.resize(6 + samples * 2, 0);
            b
        };
        assert!(decode_audio(&frame(16_000, 1, 320)).is_some());
        assert!(decode_audio(&frame(48_000, 2, 1920)).is_some());
        for (rate, channels, samples) in [(1, 1, 4000), (7_999, 1, 320), (4_000_000_000, 1, 320), (16_000, 9, 320), (16_000, 0, 320), (16_000, 1, 16_001)] {
            assert_eq!(decode_audio(&frame(rate, channels, samples)), None, "{rate} Hz, {channels} channels, {samples} samples");
        }
    }

    /// A public player isn't in a room for saying so, or for sending audio; they are
    /// once the player here has invited them, and out again when that no longer holds.
    #[test]
    fn a_stranger_is_in_a_room_only_while_vouched_for() {
        use peerparty_core::inet::{self, public};
        use peerparty_core::{Identity, Lobby, Options, Permission, Trust};
        fn quiet(_: &str) {}
        let port = |n: u16| 36_000 + (std::process::id() % 1000) as u16 * 8 + n;
        let start = |name: &str, mine: u16, theirs: u16, disc: u16| {
            let mut c = inet::Config::public(public::PublicCfg::ephemeral(name));
            c.port = mine;
            c.peers = vec![format!("127.0.0.1:{theirs}")];
            (c.dht, c.bootstrap, c.stun, c.node_cache, c.allow_private) = (false, Vec::new(), Vec::new(), None, true);
            Lan::start(Options {
                game: format!("test:voice-strangers:{}", std::process::id()),
                identity: Identity::ephemeral(name),
                discovery_port: disc,
                extra_targets: Vec::new(),
                log: quiet,
                internet: None,
                public: Some(c),
                lan_discovery: false,
            })
            .unwrap()
        };
        // `me` is this copy, with voice.rs's rooms; `them` is a stranger driven by hand.
        let (me, them) = (start("me", port(0), port(1), port(2)), start("them", port(1), port(0), port(3)));
        me.set_public_voice(true);
        them.set_public_voice(true);
        let room = format!("strangers-{}", std::process::id());
        let sock = socket(&room);
        // What `them` got on the room's socket, and one turn of both copies.
        let mut got: Vec<Vec<u8>> = Vec::new();
        let mut turn = |got: &mut Vec<Vec<u8>>| {
            for l in [&me, &them] {
                l.tick();
            }
            for e in me.poll() {
                on_event(&me, &e);
            }
            for e in them.poll() {
                if let Event::ChanRequest { peer, socket } = &e {
                    them.accept(*peer, socket);
                }
            }
            pump(&me);
            while let Some(p) = them.receive_where(None, |s| s == sock) {
                got.push(p.data);
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let wait = |what: &str, got: &mut Vec<Vec<u8>>, turn: &mut dyn FnMut(&mut Vec<Vec<u8>>), done: &dyn Fn(&[Vec<u8>]) -> bool| {
            let until = Instant::now() + Duration::from_secs(30);
            while !done(got) {
                assert!(Instant::now() < until, "timed out waiting for {what}");
                turn(got);
            }
        };
        wait("the two to meet", &mut got, &mut turn, &|_| me.peer_trust(them.me()) == Some(Trust::Public) && them.peer_trust(me.me()).is_some());

        join(&me, &room, false, false);
        // The stranger says it joined, says it's here, and sends audio and data.
        let audio = encode_audio(&Frame { rate: 16_000, channels: 1, samples: vec![9; 320] });
        for (ch, msg, reliable) in [(CH_CTRL, vec![JOIN], true), (CH_CTRL, vec![HERE], true), (CH_AUDIO, audio.clone(), false), (CH_DATA, vec![DATA, 1], true)] {
            them.send(me.me(), &sock, ch, &msg, reliable, reliable, true);
        }
        for _ in 0..150 {
            turn(&mut got);
        }
        assert!(members(&room).is_empty(), "a stranger joined a room by saying so");
        assert!(got.is_empty(), "a stranger was told of, or answered from, a room: {got:?}");
        assert!(take(&room).is_empty());

        // The player here invites them (to a lobby of theirs): now they are told of the
        // room, and their answer makes them a member who is sent its audio.
        let mut l = Lobby::new("test.lobby");
        l.permission = Permission::InviteOnly;
        let lobby = me.create_lobby(l).unwrap();
        me.invite(Some(them.me()), "", Some(&lobby));
        wait("the room to be announced", &mut got, &mut turn, &|g| g.iter().any(|m| m == &[JOIN]));
        them.send(me.me(), &sock, CH_CTRL, &[HERE], true, true, true);
        wait("the stranger to be a member", &mut got, &mut turn, &|_| members(&room) == [them.me()]);
        got.clear();
        send_audio(&me, &room, &Frame { rate: 16_000, channels: 1, samples: vec![3; 320] });
        wait("our audio to reach them", &mut got, &mut turn, &|g| g.iter().any(|m| m.first() == Some(&AUDIO)));

        // Shown the door (which takes the invite back), they are out of the room too.
        them.join_by_id("test.lobby", &lobby.id).unwrap();
        wait("them to join the lobby", &mut got, &mut turn, &|_| me.lobby(&lobby.id).is_some_and(|l| l.members.len() == 2));
        me.kick(&lobby.id, them.me()).unwrap();
        wait("them to be out of the room", &mut got, &mut turn, &|_| members(&room).is_empty());
        got.clear();
        send_audio(&me, &room, &Frame { rate: 16_000, channels: 1, samples: vec![3; 320] });
        them.send(me.me(), &sock, CH_AUDIO, &audio, false, false, true);
        for _ in 0..100 {
            turn(&mut got);
        }
        assert!(got.is_empty() && members(&room).is_empty(), "audio went to, or came from, someone no longer in the room");
        leave(&me, &room);
        me.shutdown();
        them.shutdown();
    }

    /// Blocking a member stops our audio and data to them as well as theirs to us;
    /// muting only stops theirs.
    #[test]
    fn blocking_works_both_ways() {
        let (a, b) = (PeerId([1; 16]), PeerId([2; 16]));
        let mut r = Room::new(false, false);
        r.members.extend([a, b]);
        assert_eq!(r.audio_to(), [a, b]);
        r.muted.insert(b);
        assert!(!r.hears(b) && r.hears(a));
        assert_eq!(r.audio_to(), [a, b], "muting stops only what we hear");
        r.muted.clear();
        r.blocked.insert(b);
        assert!(!r.hears(b) && r.hears(a));
        assert_eq!(r.audio_to(), [a]);
        assert_eq!(r.data_to(), [a]);
        r.blocked.remove(&b);
        assert_eq!(r.audio_to(), [a, b]);
        assert_eq!(r.data_to(), [a, b]);
        assert!(r.hears(b));
    }

    #[test]
    fn frames_become_16k_mono() {
        // 10 ms of 48 kHz stereo is 160 samples at 16 kHz.
        let f = Frame { rate: 48_000, channels: 2, samples: vec![1000; 960] };
        let out = to_device(&f);
        assert_eq!(out.len(), 160);
        assert!(out.iter().all(|s| *s == 1000));
        let same = Frame { rate: 16_000, channels: 1, samples: vec![5, 6, 7] };
        assert_eq!(to_device(&same), vec![5, 6, 7]);
        // 40 ms of 16 kHz is 960 samples at 24 kHz (Party's voice format).
        let up = resample(&Frame { rate: 16_000, channels: 1, samples: vec![-700; 640] }, 24_000);
        assert_eq!(up.len(), 960);
        assert!(up.iter().all(|s| *s == -700));
    }

    #[test]
    fn panning_keeps_the_near_side_full_and_the_far_side_audible() {
        assert_eq!(pan_gains(0.0), [1.0, 1.0]);
        assert_eq!(pan_gains(1.0), [0.25, 1.0]);
        assert_eq!(pan_gains(-1.0), [1.0, 0.25]);
        assert_eq!(pan_gains(5.0), [0.25, 1.0]);
        assert_eq!(pan_gains(f32::NAN), [1.0, 1.0]);
        let [l, r] = pan_gains(0.5);
        assert!((l - 0.625).abs() < 1e-6 && r == 1.0);
    }

    #[test]
    fn frames_play_in_stereo() {
        // Mono goes to both sides; stereo keeps its sides (48 kHz to 16 kHz).
        assert_eq!(to_stereo(&Frame { rate: 16_000, channels: 1, samples: vec![7, -7] }), vec![[7, 7], [-7, -7]]);
        let st = to_stereo(&Frame { rate: 48_000, channels: 2, samples: [100, -100].repeat(480) });
        assert_eq!(st.len(), 160);
        assert!(st.iter().all(|s| *s == [100, -100]));
    }

    /// The speakers open on the chosen device and move when the choice does; silence
    /// plays (nothing to check on a PC with fewer than two speakers, or with
    /// `PEERPARTY_VOICE=0`).
    #[test]
    fn the_speakers_open_on_the_chosen_device_and_follow_the_choice() {
        let s = snapshot();
        if s.output.list.len() < 2 || !devices_allowed() {
            eprintln!("fewer than two speakers, or devices off: skipped");
            return;
        }
        let before = chosen(true);
        let room = format!("speakers-test-{}", std::process::id());
        S.lock().unwrap().rooms.insert(room.clone(), Room::new(false, true));
        devices::update();
        let open_on = |want: Option<Option<String>>| {
            let until = Instant::now() + Duration::from_secs(10);
            while *devices::SPEAKERS_OPEN.lock().unwrap() != want {
                assert!(Instant::now() < until, "speakers didn't come to {want:?}");
                play_local(&Frame { rate: DEVICE_RATE, channels: 2, samples: vec![0; FRAME * 2] }, 1.0);
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        for e in s.output.list.iter().take(2) {
            choose_device(true, Choice::Device(e.id.clone()));
            open_on(Some(Some(e.id.clone())));
        }
        S.lock().unwrap().rooms.remove(&room);
        devices::update();
        open_on(None);
        choose_device(true, before);
    }

    /// Every endpoint Windows lists has its waveIn / waveOut number, so a chosen device
    /// opens (nothing to check on a PC without sound devices).
    #[test]
    fn endpoints_map_to_wave_devices() {
        let s = snapshot();
        for (output, side) in [(false, &s.input), (true, &s.output)] {
            for e in &side.list {
                assert!(devices::wave_index(output, &e.id).is_some(), "no wave device for {e:?}");
            }
        }
    }

    #[test]
    fn the_gate_holds_after_speech() {
        let mut g = Gate { hold: 0 };
        assert!(!g.open(&[0; 320]));
        assert!(g.open(&[3000; 320]));
        for _ in 0..15 {
            assert!(g.open(&[0; 320]));
        }
        assert!(!g.open(&[0; 320]));
    }
}
