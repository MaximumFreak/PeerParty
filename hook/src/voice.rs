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
//! waveOut (winmm): the communications default microphone at 16 kHz mono in 20 ms
//! frames, sent only while it carries voice (a level gate with a short hang-over), and
//! the room's voices mixed to the communications default speakers. The microphone is opened only while a room wants to
//! send, and closed when none does. `PEERPARTY_VOICE=0` turns the devices off (rooms,
//! data and game-supplied audio still work).
//!
//! The microphone goes through Windows' Voice Capture DSP when it can (aec.rs): echo
//! cancellation against whatever the speakers play, noise suppression and automatic
//! gain, on the communications default devices. Without it (or with
//! `PEERPARTY_VOICE_AEC=0`) the plain waveIn path below is used.
//!
//! A room can also narrow who it sends to and hears (`set_filters`, for Party's chat
//! permissions), hand a speaker's audio to the game instead of the speakers
//! (`set_no_play`), and send data to chosen members (`send_data_to`). EOS uses none of
//! these, so its rooms behave as before.
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

pub const SOCKET_PREFIX: &str = "rtc:";
const CH_CTRL: u8 = 0;
const CH_AUDIO: u8 = 1;
const CH_DATA: u8 = 2;
const JOIN: u8 = 1;
const HERE: u8 = 2;
const LEAVE: u8 = 3;
const AUDIO: u8 = 4;
const DATA: u8 = 5;

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
    heard: HashMap<PeerId, Instant>,
    /// Only these members get our audio (`None`: all of them).
    send_to: Option<HashSet<PeerId>>,
    /// Only these members' audio is taken in (`None`: all but `muted`).
    hear: Option<HashSet<PeerId>>,
    /// Members whose audio goes to the game only (events), not to the speakers.
    no_play: HashSet<PeerId>,
    /// When we last sent audio, for "talking" indicators.
    last_sent: Option<Instant>,
}

#[derive(Default)]
struct State {
    rooms: HashMap<String, Room>,
}

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
        s.rooms.insert(
            room.to_string(),
            Room {
                members: BTreeSet::new(),
                events: VecDeque::new(),
                sending: true,
                receiving: true,
                muted: HashSet::new(),
                data_sending: true,
                device_in: device_in && devices_allowed(),
                device_out: device_out && devices_allowed(),
                volume_out: 1.0,
                peer_volume: HashMap::new(),
                heard: HashMap::new(),
                send_to: None,
                hear: None,
                no_play: HashSet::new(),
                last_sent: None,
            },
        );
    }
    for (peer, _) in lan.peers() {
        // Also answers a room channel the peer asked for while nobody here listened.
        lan.accept(peer, &socket(room));
        send(lan, peer, room, CH_CTRL, &[JOIN], true);
    }
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
    if rate == 0 || channels == 0 {
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
        r.members
            .iter()
            .copied()
            .filter(|p| r.send_to.as_ref().is_none_or(|t| t.contains(p)))
            .collect::<Vec<_>>()
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
        r.members.iter().copied().collect::<Vec<_>>()
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
    if !joined(room) {
        return false;
    }
    let mut msg = Vec::with_capacity(data.len() + 1);
    msg.push(DATA);
    msg.extend_from_slice(data);
    for p in peers {
        send(lan, *p, room, CH_DATA, &msg, true);
    }
    true
}

/// Plays audio the game mixed itself (Party's render stream) on the speakers, when a
/// room has them open.
pub fn play_local(f: &Frame, gain: f32) {
    devices::play("", PeerId([0; 16]), f, gain);
}

/// Whether Windows has a microphone / speakers at all (counting devices opens nothing).
pub fn input_present() -> bool {
    devices::input_count() > 0
}

pub fn output_present() -> bool {
    devices::output_count() > 0
}

/// Core events: room sockets are always accepted; players who leave the LAN leave rooms.
pub fn on_event(lan: &Lan, ev: &Event) {
    match ev {
        Event::ChanRequest { peer, socket } if socket.starts_with(SOCKET_PREFIX) => lan.accept(*peer, socket),
        // A player met after we joined a room hasn't heard our join: tell them now (two
        // copies that each joined before meeting would otherwise never see each other).
        Event::PeerJoined(peer) => {
            let rooms: Vec<String> = S.lock().unwrap().rooms.keys().cloned().collect();
            for room in rooms {
                lan.accept(*peer, &socket(&room));
                send(lan, *peer, &room, CH_CTRL, &[JOIN], true);
            }
        }
        Event::PeerLeft(peer) => {
            let mut s = S.lock().unwrap();
            for r in s.rooms.values_mut() {
                if r.members.remove(peer) {
                    r.events.push_back(RoomEvent::Left(*peer));
                }
            }
        }
        _ => {}
    }
}

/// Moves arrived room traffic into each room's events.
pub fn pump(lan: &Lan) {
    while let Some(p) = lan.receive_where(None, |s| s.starts_with(SOCKET_PREFIX)) {
        let room = &p.socket[SOCKET_PREFIX.len()..];
        let Some(&kind) = p.data.first() else { continue };
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
                    let allowed = r.hear.as_ref().is_none_or(|h| h.contains(&p.peer));
                    if r.receiving && allowed && !r.muted.contains(&p.peer) {
                        if let Some(f) = decode_audio(&p.data) {
                            if r.heard.insert(p.peer, Instant::now()).is_none() {
                                r.events.push_back(RoomEvent::Speaking(p.peer, true));
                            }
                            if r.device_out && !r.no_play.contains(&p.peer) {
                                let gain = r.volume_out * r.peer_volume.get(&p.peer).copied().unwrap_or(1.0);
                                devices::play(room, p.peer, &f, gain);
                            }
                            r.events.push_back(RoomEvent::Audio(p.peer, f));
                        }
                    }
                }
                DATA => r.events.push_back(RoomEvent::Data(p.peer, p.data[1..].to_vec())),
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
    //! waveIn / waveOut on the default devices, each on its own thread.
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

    fn format() -> WaveFormatEx {
        WaveFormatEx {
            format_tag: 1, // WAVE_FORMAT_PCM
            channels: 1,
            samples_per_sec: DEVICE_RATE,
            avg_bytes_per_sec: DEVICE_RATE * 2,
            block_align: 2,
            bits_per_sample: 16,
            cb_size: 0,
        }
    }

    static WANT_IN: AtomicBool = AtomicBool::new(false);
    static WANT_OUT: AtomicBool = AtomicBool::new(false);
    static STARTED: std::sync::Once = std::sync::Once::new();
    /// Per (room, speaker) queues of 16 kHz mono samples waiting to be played.
    static QUEUES: LazyLock<Mutex<HashMap<(String, PeerId), VecDeque<i16>>>> = LazyLock::new(Mutex::default);
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
            STARTED.call_once(|| {
                std::thread::spawn(capture_loop);
                std::thread::spawn(playback_loop);
            });
        }
    }

    pub(super) fn play(room: &str, peer: PeerId, f: &Frame, gain: f32) {
        let samples = to_device(f);
        let mut q = QUEUES.lock().unwrap();
        let queue = q.entry((room.to_string(), peer)).or_default();
        queue.extend(samples.iter().map(|s| (*s as f32 * gain).clamp(-32768.0, 32767.0) as i16));
        while queue.len() > MAX_QUEUE {
            queue.pop_front();
        }
    }

    /// One frame of everyone's voices, added together.
    fn mix() -> Vec<i16> {
        let mut out = vec![0i32; FRAME];
        let mut q = QUEUES.lock().unwrap();
        for queue in q.values_mut() {
            for o in out.iter_mut() {
                match queue.pop_front() {
                    Some(s) => *o += s as i32,
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

    fn buffers() -> Buffers {
        let mut data: Vec<Vec<i16>> = (0..BUFFERS).map(|_| vec![0i16; FRAME]).collect();
        let hdrs = Box::new(std::array::from_fn(|i| WaveHdr {
            data: data[i].as_mut_ptr() as *mut u8,
            buffer_length: (FRAME * 2) as u32,
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

    /// The microphone through the Voice Capture DSP, while a room wants it. False if the
    /// DSP couldn't start (waveIn takes over).
    fn capture_with_aec(gate: &mut Gate) -> bool {
        let Some(mut cap) = aec::Capture::open() else { return false };
        let mut pending: Vec<i16> = Vec::with_capacity(FRAME * 4);
        while WANT_IN.load(Ordering::Relaxed) {
            if !cap.read(&mut pending) {
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

    fn capture_loop() {
        let mut gate = Gate { hold: 0 };
        let mut aec = aec_wanted();
        loop {
            if !WANT_IN.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            if aec {
                if capture_with_aec(&mut gate) {
                    continue;
                }
                aec = false;
            }
            let mut h = 0usize;
            let r = unsafe { waveInOpen(&mut h, communications(false), &format(), 0, 0, 0) };
            if r != 0 {
                log!("voice: no microphone (waveInOpen {r}); sending nothing");
                while WANT_IN.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(500));
                }
                continue;
            }
            log!("voice: microphone open (communications default, 16 kHz mono, no echo cancellation)");
            let mut b = buffers();
            for hdr in b.hdrs.iter_mut() {
                unsafe {
                    waveInPrepareHeader(h, hdr, HDR);
                    waveInAddBuffer(h, hdr, HDR);
                }
            }
            unsafe { waveInStart(h) };
            while WANT_IN.load(Ordering::Relaxed) {
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
            let mut h = 0usize;
            let r = unsafe { waveOutOpen(&mut h, communications(true), &format(), 0, 0, 0) };
            if r != 0 {
                log!("voice: no speakers (waveOutOpen {r}); voices aren't played");
                while WANT_OUT.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(500));
                }
                continue;
            }
            let mut b = buffers();
            for hdr in b.hdrs.iter_mut() {
                unsafe { waveOutPrepareHeader(h, hdr, HDR) };
                hdr.flags |= WHDR_DONE;
            }
            while WANT_OUT.load(Ordering::Relaxed) {
                for hdr in b.hdrs.iter_mut() {
                    if hdr.flags & WHDR_DONE != 0 {
                        let frame = mix();
                        unsafe {
                            std::ptr::copy_nonoverlapping(frame.as_ptr(), hdr.data as *mut i16, FRAME);
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
