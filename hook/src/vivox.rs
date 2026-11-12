//! Vivox voice chat (vivoxsdk.dll, under Unity's com.unity.services.vivox) on
//! PeerParty's LAN voice rooms (voice.rs).
//!
//! A game whose Unity Gaming Services sign-in is PeerParty's (ugs/) hands Vivox
//! PeerParty's unsigned token, which Vivox's servers refuse ("Access Token Malformed",
//! 20124), so the game's voice chat never connects. Here the Vivox core SDK's C API is
//! answered instead: requests, responses and events are the SDK's own structs, and a
//! channel is a LAN voice room (`vivox:<channel URI>`) between the copies that joined it.
//!
//! - Which calls: Unity's package calls VivoxNative.dll (SWIG wrappers), which calls
//!   vivoxsdk.dll. The functions VivoxNative reaches from the wrappers Unity uses are
//!   the ones taken over: `vx_initialize3`, `vx_uninitialize`, `vx_is_initialized`,
//!   `vx_issue_request*`, `vx_get_message`, the `destroy_*` / `vx_destroy_message`
//!   frees, and the `vx_req_*_create` of every request Unity issues. The real SDK is
//!   never initialized, so it opens no socket and no sound device. Everything else
//!   (version, error strings, default config) stays the SDK's own.
//! - Layouts: offsets of every field used are read from VivoxNative's own accessors
//!   (Unity Vivox 16.11.0, Vivox core 5.27.1): see `abi`. Enum values are the SDK's
//!   (`vx_get_*_string`).
//! - Memory: requests, responses and events are PeerParty's, on the process heap (the
//!   heap VivoxNative's runtime frees on), and freed a while after the game took them
//!   (Unity never frees a message: its delete is a no-op).
//! - Flow, as Unity's package expects it: a login is answered, then
//!   `login_state_change(logged_in)`; joining a channel is answered, then
//!   `media_stream_updated(connected)`, `text_stream_updated(connected)` when text was
//!   asked for, and the player's own `participant_added` (Unity's join completes once
//!   it has seen its own participant). Leaving ends with `session_removed`.
//! - Participants: each copy tells the room's other members its account name and display
//!   name (room data); they're reported with the URI Unity builds for an account,
//!   `sip:<account name>@<the channel's domain>`, so `VivoxParticipant.PlayerId` is the
//!   player's own UGS id. Speaking comes from voice.rs's speaking indicator.
//! - Positional channels (`confctl-d-...!p-<audible>-<conversational>-<rolloff>-<model>`):
//!   each copy sends its speaker position (`Set3DPosition`) to the room, at most every
//!   100 ms; each copy turns every speaker down by distance from its own listener with
//!   the channel's fade model (OpenAL's clamped inverse, linear and exponential
//!   distance models, which Vivox's three are), silent past the audible distance, and
//!   places them left or right of where its listener faces (Unity's axes; no panning
//!   when the game turns it off, which Unity sends as a facing equal to "up").
//! - Devices: Windows' microphones and speakers, listed as Vivox lists them ("Default
//!   System Device", "Default Communication Device", then each device by its endpoint
//!   id and name); the game's choice moves voice.rs's devices, and a change in
//!   Windows' devices is told as Vivox's hot-swap event, on which Unity lists them
//!   again. Voice starts on the system defaults, as Vivox does. Mute, transmission
//!   (all, one, no channel), channel volume, speaker level, mute-for-me and block lists
//!   apply.
//!
//! On when PeerParty answers the game's Unity sign-in (`ugs::wanted`), with
//! `PEERPARTY_VIVOX=1` always, off with `PEERPARTY_VIVOX=0` or `PEERPARTY_SKIP=vivox`.
//! Decided at the game's first Vivox call (never under the loader lock); until then,
//! and when off, every call goes to the real SDK.
//!
//! References: Unity's Vivox core reference (docs.unity.com/vivox-core, VxcRequests.h,
//! VxcResponses.h, VxcEvents.h) for what each request and event means; the public
//! com.unity.services.vivox package (16.11.0) for the order Unity expects and the
//! session sequence. No code was taken.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use peerparty_core::{Lan, PeerId};
use windows_sys::Win32::System::Memory::{GetProcessHeap, HEAP_ZERO_MEMORY, HeapAlloc, HeapFree};

use crate::log;
use crate::net::{self, Front};
use crate::pe::Export;
use crate::voice::{self, RoomEvent};

/// Struct layouts and enum values (Vivox core 5.27.1, Unity Vivox 16.11.0's VivoxNative).
mod abi {
    // vx_message_base_t
    pub const MSG_TYPE: usize = 0x00;
    pub const MSG_CREATE_TIME: usize = 0x08;
    pub const MSG_REQUEST: i32 = 1;
    pub const MSG_RESPONSE: i32 = 2;
    pub const MSG_EVENT: i32 = 3;
    // vx_req_base_t / vx_resp_base_t / vx_evt_base_t
    pub const REQ_TYPE: usize = 0x18;
    pub const RESP_TYPE: usize = 0x18;
    pub const RESP_RETURN_CODE: usize = 0x1c;
    pub const RESP_STATUS_CODE: usize = 0x20;
    pub const RESP_REQUEST: usize = 0x30;
    pub const EVT_TYPE: usize = 0x18;
    /// Room for the biggest struct used (set_3d_position ends at 0x130).
    pub const REQ_SIZE: usize = 0x200;
    pub const MSG_SIZE: usize = 0x100;

    /// vx_request_type; a response's type is its request's.
    pub mod req {
        pub const CONNECTOR_CREATE: i32 = 1;
        pub const CONNECTOR_INITIATE_SHUTDOWN: i32 = 2;
        pub const ACCOUNT_LOGOUT: i32 = 4;
        pub const SESSIONGROUP_ADD_SESSION: i32 = 8;
        pub const SESSIONGROUP_REMOVE_SESSION: i32 = 9;
        pub const SESSIONGROUP_SET_TX_SESSION: i32 = 13;
        pub const SESSIONGROUP_SET_TX_ALL_SESSIONS: i32 = 14;
        pub const SESSIONGROUP_SET_TX_NO_SESSION: i32 = 15;
        pub const SESSION_SET_LOCAL_RENDER_VOLUME: i32 = 24;
        pub const SESSION_SET_PARTICIPANT_VOLUME_FOR_ME: i32 = 26;
        pub const SESSION_SET_PARTICIPANT_MUTE_FOR_ME: i32 = 27;
        pub const SESSION_SET_3D_POSITION: i32 = 28;
        pub const CONNECTOR_MUTE_LOCAL_MIC: i32 = 61;
        pub const CONNECTOR_MUTE_LOCAL_SPEAKER: i32 = 62;
        pub const AUX_GET_RENDER_DEVICES: i32 = 87;
        pub const AUX_GET_CAPTURE_DEVICES: i32 = 88;
        pub const AUX_SET_RENDER_DEVICE: i32 = 89;
        pub const AUX_SET_CAPTURE_DEVICE: i32 = 90;
        pub const AUX_SET_MIC_LEVEL: i32 = 93;
        pub const AUX_SET_SPEAKER_LEVEL: i32 = 94;
        pub const ACCOUNT_ANONYMOUS_LOGIN: i32 = 131;
        pub const ACCOUNT_CONTROL_COMMUNICATIONS: i32 = 136;
    }

    /// vx_event_type.
    pub mod evt {
        pub const ACCOUNT_LOGIN_STATE_CHANGE: i32 = 2;
        pub const MEDIA_STREAM_UPDATED: i32 = 20;
        pub const TEXT_STREAM_UPDATED: i32 = 21;
        pub const SESSION_REMOVED: i32 = 25;
        pub const PARTICIPANT_ADDED: i32 = 26;
        pub const PARTICIPANT_REMOVED: i32 = 27;
        pub const PARTICIPANT_UPDATED: i32 = 28;
        pub const AUDIO_DEVICE_HOT_SWAP: i32 = 41;
        pub const CONNECTION_STATE_CHANGED: i32 = 53;
    }

    // vx_login_state_change_state, vx_session_media_state, vx_session_text_state,
    // vx_connection_state
    pub const LOGGED_OUT: i32 = 0;
    pub const LOGGED_IN: i32 = 1;
    pub const LOGGING_IN: i32 = 2;
    pub const LOGGING_OUT: i32 = 3;
    pub const MEDIA_DISCONNECTED: i32 = 1;
    pub const MEDIA_CONNECTED: i32 = 2;
    pub const MEDIA_CONNECTING: i32 = 6;
    pub const MEDIA_DISCONNECTING: i32 = 7;
    pub const TEXT_DISCONNECTED: i32 = 0;
    pub const TEXT_CONNECTED: i32 = 1;
    pub const CONNECTION_CONNECTED: i32 = 1;

    // Error codes (VX_E_*), and vx_get_message's results.
    pub const E_SUCCESS: i32 = 0;
    pub const E_NO_EXIST: i32 = 0x3e9;
    pub const E_INVALID_ARGUMENT: i32 = 0x3f0;
    pub const E_NOT_INITIALIZED: i32 = 0x3f4;
    pub const E_ALREADY_INITIALIZED: i32 = 0x43d;
    pub const GET_MESSAGE_AVAILABLE: i32 = 0;
    pub const GET_MESSAGE_NO_MESSAGE: i32 = -1;

    // vx_control_communications_operation
    pub const COMMS_BLOCK: i32 = 0;
    pub const COMMS_UNBLOCK: i32 = 1;
    pub const COMMS_LIST: i32 = 2;
    pub const COMMS_CLEAR: i32 = 3;

    // Request fields.
    pub const CONNECTOR_CREATE_HANDLE: usize = 0xa8;
    pub const LOGIN_DISPLAYNAME: usize = 0x38;
    pub const LOGIN_ACCOUNT_HANDLE: usize = 0x78;
    pub const LOGIN_ACCT_NAME: usize = 0x80;
    pub const LOGOUT_ACCOUNT_HANDLE: usize = 0x30;
    pub const ADD_GROUP: usize = 0x30;
    pub const ADD_URI: usize = 0x38;
    pub const ADD_CONNECT_AUDIO: usize = 0x50;
    pub const ADD_CONNECT_TEXT: usize = 0x5c;
    pub const ADD_SESSION: usize = 0x68;
    pub const ADD_ACCOUNT: usize = 0x78;
    pub const REMOVE_SESSION: usize = 0x30;
    pub const POS_SESSION: usize = 0x30;
    pub const POS_SPEAKER: usize = 0x38;
    pub const POS_LISTENER: usize = 0xb0;
    pub const POS_LISTENER_AT: usize = 0xe0;
    pub const POS_LISTENER_UP: usize = 0xf8;
    pub const MUTE_LEVEL: usize = 0x38;
    pub const RENDER_VOLUME_SESSION: usize = 0x30;
    pub const RENDER_VOLUME: usize = 0x38;
    pub const MUTE_FOR_ME_SESSION: usize = 0x30;
    pub const MUTE_FOR_ME_URI: usize = 0x38;
    pub const MUTE_FOR_ME_MUTE: usize = 0x40;
    pub const VOLUME_FOR_ME_SESSION: usize = 0x30;
    pub const VOLUME_FOR_ME_URI: usize = 0x38;
    pub const VOLUME_FOR_ME_VOLUME: usize = 0x40;
    pub const TX_SESSION: usize = 0x30;
    pub const DEVICE_SPECIFIER: usize = 0x30;
    pub const LEVEL: usize = 0x30;
    pub const COMMS_OPERATION: usize = 0x38;
    pub const COMMS_USER_URIS: usize = 0x40;

    // Response fields.
    pub const CONNECTOR_RESP_HANDLE: usize = 0x40;
    pub const CONNECTOR_RESP_VERSION: usize = 0x48;
    pub const COMMS_RESP_BLOCKED_URIS: usize = 0x40;
    pub const DEVICES_COUNT: usize = 0x40;
    pub const DEVICES_LIST: usize = 0x48;
    pub const DEVICES_CURRENT: usize = 0x50;
    pub const DEVICES_EFFECTIVE: usize = 0x58;
    pub const DEVICES_DEFAULT: usize = 0x60;
    pub const DEVICES_DEFAULT_COMMUNICATION: usize = 0x68;
    /// vx_resp_aux_set_capture_device_t / _render_: the device opened.
    pub const SET_DEVICE_RESP_OPEN: usize = 0x40;
    // vx_device_t
    pub const DEVICE_ID: usize = 0x00;
    pub const DEVICE_NAME: usize = 0x08;
    pub const DEVICE_TYPE: usize = 0x10;
    pub const DEVICE_SIZE: usize = 0x18;
    // vx_device_type_t
    pub const DEVICE_SPECIFIC: i32 = 0;
    pub const DEVICE_DEFAULT_SYSTEM: i32 = 1;
    pub const DEVICE_DEFAULT_COMMUNICATION: i32 = 3;

    // Event fields.
    pub const LOGIN_EVT_STATE: usize = 0x28;
    pub const LOGIN_EVT_ACCOUNT: usize = 0x30;
    pub const CONNECTION_EVT_STATE: usize = 0x28;
    pub const CONNECTION_EVT_ACCOUNT: usize = 0x30;
    pub const EVT_GROUP: usize = 0x28;
    pub const EVT_SESSION: usize = 0x30;
    pub const MEDIA_EVT_STATE: usize = 0x48;
    pub const TEXT_EVT_ENABLED: usize = 0x38;
    pub const TEXT_EVT_STATE: usize = 0x3c;
    pub const REMOVED_EVT_URI: usize = 0x38;
    pub const PART_URI: usize = 0x38;
    pub const PART_ADDED_ACCOUNT: usize = 0x40;
    pub const PART_ADDED_DISPLAY_NAME: usize = 0x48;
    pub const PART_ADDED_DISPLAYNAME: usize = 0x68;
    pub const PART_ADDED_CURRENT_USER: usize = 0x80;
    pub const PART_REMOVED_ACCOUNT: usize = 0x40;
    pub const PART_REMOVED_CURRENT_USER: usize = 0x60;
    pub const PART_UPDATED_SPEAKING: usize = 0x44;
    pub const PART_UPDATED_VOLUME: usize = 0x48;
    pub const PART_UPDATED_ENERGY: usize = 0x50;
    pub const PART_UPDATED_ACTIVE_MEDIA: usize = 0x58;
    pub const PART_UPDATED_MUTED_FOR_ME: usize = 0x5c;
    pub const PART_UPDATED_CURRENT_USER: usize = 0x90;
}

use abi::*;

const ROOM_PREFIX: &str = "vivox:";
/// Room data: an account introduction, and a speaker position.
const HELLO: u8 = b'H';
const POSITION: u8 = b'P';
/// Positions go to the room at most this often.
const POSITION_EVERY: Duration = Duration::from_millis(100);
/// A message the game took is freed this long after.
const FREE_AFTER: Duration = Duration::from_secs(30);
/// The keys (and names) of Vivox's two default entries in a device list.
const DEFAULT_SYSTEM: &str = "Default System Device";
const DEFAULT_COMMUNICATION: &str = "Default Communication Device";

// ---- memory ----

fn heap_alloc(n: usize) -> usize {
    unsafe { HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, n) as usize }
}

fn heap_free(p: usize) {
    if p != 0 {
        unsafe { HeapFree(GetProcessHeap(), 0, p as *const _) };
    }
}

fn heap_str(s: &str) -> usize {
    let b = s.as_bytes();
    let p = heap_alloc(b.len() + 1);
    if p != 0 {
        unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), p as *mut u8, b.len()) };
    }
    p
}

unsafe fn rd_i32(p: usize, off: usize) -> i32 {
    unsafe { ((p + off) as *const i32).read_unaligned() }
}

unsafe fn rd_ptr(p: usize, off: usize) -> usize {
    unsafe { ((p + off) as *const usize).read_unaligned() }
}

unsafe fn rd_f64(p: usize, off: usize) -> f64 {
    unsafe { ((p + off) as *const f64).read_unaligned() }
}

unsafe fn rd_vec3(p: usize, off: usize) -> [f64; 3] {
    unsafe { [rd_f64(p, off), rd_f64(p, off + 8), rd_f64(p, off + 16)] }
}

unsafe fn rd_str(p: usize, off: usize) -> String {
    let s = unsafe { rd_ptr(p, off) };
    crate::read::cstr(s).unwrap_or_default()
}

unsafe fn wr_i32(p: usize, off: usize, v: i32) {
    unsafe { ((p + off) as *mut i32).write_unaligned(v) }
}

unsafe fn wr_ptr(p: usize, off: usize, v: usize) {
    unsafe { ((p + off) as *mut usize).write_unaligned(v) }
}

unsafe fn wr_f64(p: usize, off: usize, v: f64) {
    unsafe { ((p + off) as *mut f64).write_unaligned(v) }
}

/// A response or event being built, and the blocks it owns (its strings, a response's
/// request, a device list), freed together.
struct Out {
    p: usize,
    owned: Vec<usize>,
}

// Plain heap addresses.
unsafe impl Send for Out {}

impl Out {
    fn new(kind: i32) -> Out {
        let p = heap_alloc(MSG_SIZE);
        if p != 0 {
            unsafe {
                wr_i32(p, MSG_TYPE, kind);
                ((p + MSG_CREATE_TIME) as *mut u64).write_unaligned(now_ms());
            }
        }
        Out { p, owned: Vec::new() }
    }

    fn response(req: usize, kind: i32) -> Out {
        let mut o = Out::new(MSG_RESPONSE);
        o.i32(RESP_TYPE, kind).i32(RESP_RETURN_CODE, 0).i32(RESP_STATUS_CODE, 0).ptr(RESP_REQUEST, req);
        o.owned.push(req);
        o
    }

    fn event(kind: i32) -> Out {
        let mut o = Out::new(MSG_EVENT);
        o.i32(EVT_TYPE, kind);
        o
    }

    fn i32(&mut self, off: usize, v: i32) -> &mut Out {
        if self.p != 0 {
            unsafe { wr_i32(self.p, off, v) };
        }
        self
    }

    fn f64(&mut self, off: usize, v: f64) -> &mut Out {
        if self.p != 0 {
            unsafe { wr_f64(self.p, off, v) };
        }
        self
    }

    fn ptr(&mut self, off: usize, v: usize) -> &mut Out {
        if self.p != 0 {
            unsafe { wr_ptr(self.p, off, v) };
        }
        self
    }

    fn str(&mut self, off: usize, s: &str) -> &mut Out {
        let c = heap_str(s);
        self.owned.push(c);
        self.ptr(off, c)
    }

    fn free(self) {
        for p in self.owned {
            heap_free(p);
        }
        heap_free(self.p);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// ---- URIs, positional properties, the fade ----

/// The domain of a channel or account URI (after its `@`).
fn domain_of(uri: &str) -> &str {
    uri.rsplit_once('@').map(|(_, d)| d).unwrap_or("")
}

/// The URI Unity's package builds for an account (`AccountId.ToString`): its account
/// name is `.issuer.name.` or `.issuer.name.env.`.
fn account_uri(name: &str, domain: &str) -> String {
    if name.starts_with("sip:") {
        return name.to_string();
    }
    format!("sip:{name}@{domain}")
}

/// A channel's room: the URI without its positional properties, so copies agree.
fn room_of(uri: &str) -> String {
    let base = match (uri.find("!p-"), uri.rfind('@')) {
        (Some(p), Some(at)) if p < at => format!("{}{}", &uri[..p], &uri[at..]),
        _ => uri.to_string(),
    };
    format!("{ROOM_PREFIX}{base}")
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Props {
    audible: f64,
    conversational: f64,
    rolloff: f64,
    /// AudioFadeModel: 1 inverse, 2 linear, 3 exponential (by distance).
    model: i32,
}

/// A positional channel's properties (`...!p-32-1-1.000-1@domain`), or None for any
/// other channel.
fn props_of(uri: &str) -> Option<Props> {
    if !uri.starts_with("sip:confctl-d-") {
        return None;
    }
    let start = uri.find("!p-")? + 3;
    let end = uri.rfind('@').filter(|&e| e > start).unwrap_or(uri.len());
    let parts: Vec<&str> = uri[start..end].split('-').collect();
    if parts.len() != 4 {
        return None;
    }
    Some(Props {
        audible: parts[0].parse().ok()?,
        conversational: parts[1].parse().ok()?,
        rolloff: parts[2].parse().ok()?,
        model: parts[3].parse().ok()?,
    })
}

/// How loud a speaker `distance` away is, 0 to 1: the channel's fade model, clamped
/// (full volume within the conversational distance, silence past the audible one).
fn fade(p: &Props, distance: f64) -> f64 {
    if !distance.is_finite() || distance > p.audible {
        return 0.0;
    }
    let c = p.conversational.max(0.001);
    let d = distance.max(c);
    let g = match p.model {
        2 => 1.0 - p.rolloff * (d - c) / (p.audible - c).max(0.001),
        3 => (d / c).powf(-p.rolloff),
        _ => c / (c + p.rolloff * (d - c)),
    };
    if g.is_finite() { g.clamp(0.0, 1.0) } else { 0.0 }
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

/// Where a speaker is for a listener facing `at` with `up` overhead: -1 (left) to 1
/// (right). Unity's axes are left-handed (x right, y up, z forward), so the listener's
/// right is up x at. 0 when the facing gives no right (Unity sends at = up when the
/// game turns panning off) or the speaker is where the listener is.
fn pan(listener: [f64; 3], at: [f64; 3], up: [f64; 3], speaker: [f64; 3]) -> f64 {
    let right = cross(up, at);
    let r = distance(right, [0.0; 3]);
    let rel = [speaker[0] - listener[0], speaker[1] - listener[1], speaker[2] - listener[2]];
    let d = distance(rel, [0.0; 3]);
    if !(r > 1e-6 && d > 1e-6) {
        return 0.0;
    }
    let p = (rel[0] * right[0] + rel[1] * right[1] + rel[2] * right[2]) / (r * d);
    if p.is_finite() { p.clamp(-1.0, 1.0) } else { 0.0 }
}

fn hello(name: &str, display: &str) -> Vec<u8> {
    let mut m = vec![HELLO];
    m.extend_from_slice(name.as_bytes());
    m.push(b'\n');
    m.extend_from_slice(display.as_bytes());
    m
}

/// (account name, display name) from a hello.
fn parse_hello(b: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(b.strip_prefix(&[HELLO])?).ok()?;
    let (name, display) = text.split_once('\n').unwrap_or((text, ""));
    if name.is_empty() || name.len() > 256 || display.len() > 256 {
        return None;
    }
    Some((name.to_string(), display.to_string()))
}

fn position_msg(p: [f64; 3]) -> Vec<u8> {
    let mut m = vec![POSITION];
    for v in p {
        m.extend_from_slice(&v.to_le_bytes());
    }
    m
}

fn parse_position(b: &[u8]) -> Option<[f64; 3]> {
    let b = b.strip_prefix(&[POSITION])?;
    if b.len() != 24 {
        return None;
    }
    let v = |i: usize| f64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    let p = [v(0), v(1), v(2)];
    p.iter().all(|x| x.is_finite()).then_some(p)
}

// ---- state ----

struct Account {
    name: String,
    display: String,
}

struct Remote {
    uri: String,
    name: String,
    position: Option<[f64; 3]>,
    speaking: bool,
    muted: bool,
    /// The player's own volume for them, Vivox's 0..100 (50 unchanged).
    volume: i32,
}

struct Session {
    handle: String,
    group: String,
    /// Our own participant URI in this channel.
    me: String,
    account: String,
    uri: String,
    room: String,
    domain: String,
    props: Option<Props>,
    audio: bool,
    text: bool,
    /// Vivox's -50..50, 0 unchanged.
    volume: i32,
    remotes: HashMap<PeerId, Remote>,
    /// Members we've introduced ourselves to.
    greeted: HashSet<PeerId>,
    self_speaking: bool,
    speaker: Option<[f64; 3]>,
    listener: Option<[f64; 3]>,
    /// The listener's facing: (at, up).
    facing: Option<([f64; 3], [f64; 3])>,
    position_dirty: bool,
    position_sent: Option<Instant>,
}

#[derive(Clone, PartialEq)]
enum Tx {
    All,
    None,
    One(String),
}

struct State {
    initialized: bool,
    queue: VecDeque<Out>,
    taken: VecDeque<(Instant, Out)>,
    accounts: HashMap<String, Account>,
    sessions: HashMap<String, Session>,
    mic_muted: bool,
    speaker_muted: bool,
    /// Vivox's 0..100, 50 unchanged.
    speaker_level: i32,
    tx: Tx,
    /// Account names (`.issuer.name.`) the player blocked.
    blocked: HashSet<String>,
    /// The version of Windows' device list the game was last told of.
    devices_seen: u64,
}

impl Default for State {
    fn default() -> State {
        State {
            initialized: false,
            queue: VecDeque::new(),
            taken: VecDeque::new(),
            accounts: HashMap::new(),
            sessions: HashMap::new(),
            mic_muted: false,
            speaker_muted: false,
            speaker_level: 50,
            tx: Tx::All,
            blocked: HashSet::new(),
            devices_seen: 0,
        }
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);

/// The real SDK's functions, by name (for when PeerParty doesn't answer).
static REAL: OnceLock<HashMap<String, usize>> = OnceLock::new();
static SDK_BASE: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: OnceLock<bool> = OnceLock::new();
/// VivoxNative's imports of the SDK point here.
static IMPORTS_DONE: AtomicBool = AtomicBool::new(false);

fn real(name: &str) -> usize {
    REAL.get().and_then(|m| m.get(name).copied()).unwrap_or(0)
}

/// Whether PeerParty answers Vivox in this game (decided once, at the first call).
fn active() -> bool {
    *ACTIVE.get_or_init(|| {
        let on = net::enabled()
            && !net::skip("vivox")
            && match std::env::var("PEERPARTY_VIVOX").ok().as_deref().map(str::trim) {
                Some("0") => false,
                Some("1") => true,
                _ => crate::ugs::wanted(),
            };
        if on {
            log!("vivox: PeerParty answers the game's Vivox voice chat (LAN voice rooms)");
        } else {
            log!("vivox: left to the real Vivox SDK (PeerParty doesn't answer this game's Unity sign-in)");
        }
        on
    })
}

/// vivoxsdk.dll loaded: its functions come to PeerParty (decided per call later).
pub fn attach(base: usize, module_name: &str, exports: &[Export]) -> HashSet<usize> {
    let _ = REAL.set(exports.iter().map(|e| (e.name.clone(), e.addr as usize)).collect());
    SDK_BASE.store(base, Ordering::Relaxed);
    crate::swap::register(base, module_name, resolve);
    log!("vivox: {module_name} found; its voice calls come to PeerParty once the game starts Vivox");
    // The SDK's own code is never patched: its exports are taken over by name only.
    exports.iter().map(|e| e.addr as usize).collect()
}

/// A GetProcAddress from the game: VivoxNative is loaded and bound by then, so its
/// imports of the SDK can be pointed here.
pub fn looked_up(module: usize) {
    if IMPORTS_DONE.load(Ordering::Relaxed) || SDK_BASE.load(Ordering::Relaxed) == 0 {
        return;
    }
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < Duration::from_millis(50)) {
            return;
        }
        *last = Some(Instant::now());
    }
    let _ = module;
    crate::swap::patch_imports();
    if crate::swap::importers_patched(SDK_BASE.load(Ordering::Relaxed)) > 0 {
        IMPORTS_DONE.store(true, Ordering::Relaxed);
    }
}

fn resolve(name: &str) -> Option<usize> {
    let f: usize = match name {
        "vx_initialize3" => vx_initialize3 as *const () as usize,
        "vx_uninitialize" => vx_uninitialize as *const () as usize,
        "vx_is_initialized" => vx_is_initialized as *const () as usize,
        "vx_issue_request" => vx_issue_request as *const () as usize,
        "vx_issue_request2" => vx_issue_request2 as *const () as usize,
        "vx_issue_request3" => vx_issue_request3 as *const () as usize,
        "vx_get_message" => vx_get_message as *const () as usize,
        "vx_destroy_message" => vx_destroy_message as *const () as usize,
        "destroy_req" => destroy_req as *const () as usize,
        "destroy_resp" => destroy_resp as *const () as usize,
        "destroy_evt" => destroy_evt as *const () as usize,
        _ => return creator(name),
    };
    Some(f)
}

// ---- the exports ----

type Fn0 = unsafe extern "C" fn() -> i32;
type Fn1 = unsafe extern "C" fn(usize) -> i32;
type Fn2 = unsafe extern "C" fn(usize, usize) -> i32;

unsafe fn call0(name: &str) -> i32 {
    match real(name) {
        0 => E_NOT_INITIALIZED,
        f => unsafe { std::mem::transmute::<usize, Fn0>(f)() },
    }
}

unsafe fn call1(name: &str, a: usize) -> i32 {
    match real(name) {
        0 => E_NOT_INITIALIZED,
        f => unsafe { std::mem::transmute::<usize, Fn1>(f)(a) },
    }
}

unsafe fn call2(name: &str, a: usize, b: usize) -> i32 {
    match real(name) {
        0 => E_NOT_INITIALIZED,
        f => unsafe { std::mem::transmute::<usize, Fn2>(f)(a, b) },
    }
}

unsafe extern "C" fn vx_initialize3(config: usize, size: usize) -> i32 {
    if !active() {
        return unsafe { call2("vx_initialize3", config, size) };
    }
    let mut s = STATE.lock().unwrap();
    if s.initialized {
        return E_ALREADY_INITIALIZED;
    }
    s.initialized = true;
    net::subscribe(Front::Vivox);
    // Vivox starts on the system's default devices (Unity's package assumes so).
    voice::choose_device(false, voice::Choice::System);
    voice::choose_device(true, voice::Choice::System);
    voice::watch_devices();
    log!("vivox: initialized (PeerParty's)");
    E_SUCCESS
}

unsafe extern "C" fn vx_uninitialize() -> i32 {
    if !active() {
        return unsafe { call0("vx_uninitialize") };
    }
    let mut s = STATE.lock().unwrap();
    if !s.initialized {
        return E_NOT_INITIALIZED;
    }
    if let Some(lan) = net::lan() {
        for sess in s.sessions.values() {
            voice::leave(lan, &sess.room);
        }
    }
    let queued: Vec<Out> = s.queue.drain(..).collect();
    let taken: Vec<Out> = s.taken.drain(..).map(|(_, o)| o).collect();
    *s = State::default();
    drop(s);
    for o in queued.into_iter().chain(taken) {
        o.free();
    }
    log!("vivox: uninitialized");
    E_SUCCESS
}

unsafe extern "C" fn vx_is_initialized() -> i32 {
    if !active() {
        return unsafe { call0("vx_is_initialized") };
    }
    STATE.lock().unwrap().initialized as i32
}

unsafe extern "C" fn vx_issue_request(req: usize) -> i32 {
    if !active() {
        return unsafe { call1("vx_issue_request", req) };
    }
    issue(req)
}

unsafe extern "C" fn vx_issue_request2(req: usize) -> i32 {
    if !active() {
        return unsafe { call1("vx_issue_request2", req) };
    }
    issue(req)
}

unsafe extern "C" fn vx_issue_request3(req: usize, count: *mut i32) -> i32 {
    if !active() {
        return unsafe { call2("vx_issue_request3", req, count as usize) };
    }
    let r = issue(req);
    if !count.is_null() {
        unsafe { count.write(STATE.lock().unwrap().queue.len() as i32) };
    }
    r
}

unsafe extern "C" fn vx_get_message(out: *mut usize) -> i32 {
    if !active() {
        return unsafe { call1("vx_get_message", out as usize) };
    }
    if out.is_null() {
        return E_INVALID_ARGUMENT;
    }
    let mut s = STATE.lock().unwrap();
    if !s.initialized {
        return E_NOT_INITIALIZED;
    }
    if s.queue.is_empty() {
        pump(&mut s);
    }
    let now = Instant::now();
    while s.taken.front().is_some_and(|(t, _)| now.duration_since(*t) > FREE_AFTER) {
        if let Some((_, o)) = s.taken.pop_front() {
            o.free();
        }
    }
    match s.queue.pop_front() {
        Some(o) => {
            unsafe { out.write(o.p) };
            s.taken.push_back((now, o));
            GET_MESSAGE_AVAILABLE
        }
        None => {
            unsafe { out.write(0) };
            GET_MESSAGE_NO_MESSAGE
        }
    }
}

/// Messages are freed on PeerParty's schedule (see `FREE_AFTER`), whoever asks.
unsafe extern "C" fn vx_destroy_message(msg: usize) -> i32 {
    if !active() {
        return unsafe { call1("vx_destroy_message", msg) };
    }
    E_SUCCESS
}

unsafe extern "C" fn destroy_req(req: usize) -> i32 {
    if !active() {
        return unsafe { call1("destroy_req", req) };
    }
    E_SUCCESS
}

unsafe extern "C" fn destroy_resp(resp: usize) -> i32 {
    if !active() {
        return unsafe { call1("destroy_resp", resp) };
    }
    E_SUCCESS
}

unsafe extern "C" fn destroy_evt(evt: usize) -> i32 {
    if !active() {
        return unsafe { call1("destroy_evt", evt) };
    }
    E_SUCCESS
}

/// `vx_req_<name>_create(vx_req_<name>_t **req)` for every request Unity issues.
macro_rules! creators {
    ($($f:ident => $t:expr),* $(,)?) => {
        $(
            #[allow(non_snake_case)]
            unsafe extern "C" fn $f(out: *mut usize) -> i32 {
                unsafe { create(stringify!($f), $t, out) }
            }
        )*
        fn creator(name: &str) -> Option<usize> {
            match name {
                $(stringify!($f) => Some($f as *const () as usize),)*
                _ => None,
            }
        }
    };
}

creators! {
    vx_req_connector_create_create => req::CONNECTOR_CREATE,
    vx_req_connector_initiate_shutdown_create => req::CONNECTOR_INITIATE_SHUTDOWN,
    vx_req_account_anonymous_login_create => req::ACCOUNT_ANONYMOUS_LOGIN,
    vx_req_account_logout_create => req::ACCOUNT_LOGOUT,
    vx_req_account_control_communications_create => req::ACCOUNT_CONTROL_COMMUNICATIONS,
    vx_req_aux_get_capture_devices_create => req::AUX_GET_CAPTURE_DEVICES,
    vx_req_aux_get_render_devices_create => req::AUX_GET_RENDER_DEVICES,
    vx_req_aux_set_capture_device_create => req::AUX_SET_CAPTURE_DEVICE,
    vx_req_aux_set_render_device_create => req::AUX_SET_RENDER_DEVICE,
    vx_req_aux_set_mic_level_create => req::AUX_SET_MIC_LEVEL,
    vx_req_aux_set_speaker_level_create => req::AUX_SET_SPEAKER_LEVEL,
    vx_req_connector_mute_local_mic_create => req::CONNECTOR_MUTE_LOCAL_MIC,
    vx_req_connector_mute_local_speaker_create => req::CONNECTOR_MUTE_LOCAL_SPEAKER,
    vx_req_session_set_3d_position_create => req::SESSION_SET_3D_POSITION,
    vx_req_session_set_local_render_volume_create => req::SESSION_SET_LOCAL_RENDER_VOLUME,
    vx_req_session_set_participant_mute_for_me_create => req::SESSION_SET_PARTICIPANT_MUTE_FOR_ME,
    vx_req_session_set_participant_volume_for_me_create => req::SESSION_SET_PARTICIPANT_VOLUME_FOR_ME,
    vx_req_sessiongroup_add_session_create => req::SESSIONGROUP_ADD_SESSION,
    vx_req_sessiongroup_remove_session_create => req::SESSIONGROUP_REMOVE_SESSION,
    vx_req_sessiongroup_set_tx_all_sessions_create => req::SESSIONGROUP_SET_TX_ALL_SESSIONS,
    vx_req_sessiongroup_set_tx_no_session_create => req::SESSIONGROUP_SET_TX_NO_SESSION,
    vx_req_sessiongroup_set_tx_session_create => req::SESSIONGROUP_SET_TX_SESSION,
}

unsafe fn create(name: &str, kind: i32, out: *mut usize) -> i32 {
    if !active() {
        return unsafe { call1(name, out as usize) };
    }
    if out.is_null() {
        return E_INVALID_ARGUMENT;
    }
    let p = heap_alloc(REQ_SIZE);
    if p == 0 {
        return abi::E_INVALID_ARGUMENT;
    }
    unsafe {
        wr_i32(p, MSG_TYPE, MSG_REQUEST);
        ((p + MSG_CREATE_TIME) as *mut u64).write_unaligned(now_ms());
        wr_i32(p, REQ_TYPE, kind);
        out.write(p);
    }
    E_SUCCESS
}

// ---- requests ----

fn issue(req: usize) -> i32 {
    if req == 0 {
        return E_INVALID_ARGUMENT;
    }
    let mut s = STATE.lock().unwrap();
    if !s.initialized {
        return E_NOT_INITIALIZED;
    }
    let kind = unsafe { rd_i32(req, REQ_TYPE) };
    crate::log::throttled(&format!("vivox request {kind}"), 3, |n| log!("vivox request {} n={n}", request_name(kind)));
    let lan = net::lan();
    let outs = s.handle(lan, req, kind);
    s.queue.extend(outs);
    E_SUCCESS
}

fn request_name(kind: i32) -> String {
    match kind {
        req::CONNECTOR_CREATE => "connector_create".into(),
        req::CONNECTOR_INITIATE_SHUTDOWN => "connector_initiate_shutdown".into(),
        req::ACCOUNT_ANONYMOUS_LOGIN => "account_anonymous_login".into(),
        req::ACCOUNT_LOGOUT => "account_logout".into(),
        req::SESSIONGROUP_ADD_SESSION => "sessiongroup_add_session".into(),
        req::SESSIONGROUP_REMOVE_SESSION => "sessiongroup_remove_session".into(),
        req::SESSION_SET_3D_POSITION => "session_set_3d_position".into(),
        req::CONNECTOR_MUTE_LOCAL_MIC => "connector_mute_local_mic".into(),
        req::CONNECTOR_MUTE_LOCAL_SPEAKER => "connector_mute_local_speaker".into(),
        req::SESSIONGROUP_SET_TX_ALL_SESSIONS => "sessiongroup_set_tx_all_sessions".into(),
        req::SESSIONGROUP_SET_TX_NO_SESSION => "sessiongroup_set_tx_no_session".into(),
        req::SESSIONGROUP_SET_TX_SESSION => "sessiongroup_set_tx_session".into(),
        req::AUX_GET_CAPTURE_DEVICES => "aux_get_capture_devices".into(),
        req::AUX_GET_RENDER_DEVICES => "aux_get_render_devices".into(),
        req::AUX_SET_CAPTURE_DEVICE => "aux_set_capture_device".into(),
        req::AUX_SET_RENDER_DEVICE => "aux_set_render_device".into(),
        req::AUX_SET_MIC_LEVEL => "aux_set_mic_level".into(),
        req::AUX_SET_SPEAKER_LEVEL => "aux_set_speaker_level".into(),
        req::SESSION_SET_LOCAL_RENDER_VOLUME => "session_set_local_render_volume".into(),
        req::SESSION_SET_PARTICIPANT_VOLUME_FOR_ME => "session_set_participant_volume_for_me".into(),
        req::SESSION_SET_PARTICIPANT_MUTE_FOR_ME => "session_set_participant_mute_for_me".into(),
        req::ACCOUNT_CONTROL_COMMUNICATIONS => "account_control_communications".into(),
        k => format!("type {k}"),
    }
}

impl State {
    fn handle(&mut self, lan: Option<&'static Lan>, req: usize, kind: i32) -> Vec<Out> {
        let mut outs = vec![Out::response(req, kind)];
        match kind {
            req::CONNECTOR_CREATE => {
                let handle = unsafe { rd_str(req, CONNECTOR_CREATE_HANDLE) };
                outs[0].str(CONNECTOR_RESP_HANDLE, &handle).str(CONNECTOR_RESP_VERSION, "peerparty");
            }
            req::CONNECTOR_INITIATE_SHUTDOWN => {}
            req::ACCOUNT_ANONYMOUS_LOGIN => {
                let handle = unsafe { rd_str(req, LOGIN_ACCOUNT_HANDLE) };
                let name = unsafe { rd_str(req, LOGIN_ACCT_NAME) };
                let display = unsafe { rd_str(req, LOGIN_DISPLAYNAME) };
                log!("vivox: logged in as {name} ({display})");
                for state in [LOGGING_IN, LOGGED_IN] {
                    let mut e = Out::event(evt::ACCOUNT_LOGIN_STATE_CHANGE);
                    e.i32(LOGIN_EVT_STATE, state).str(LOGIN_EVT_ACCOUNT, &handle);
                    outs.push(e);
                }
                let mut c = Out::event(evt::CONNECTION_STATE_CHANGED);
                c.i32(CONNECTION_EVT_STATE, CONNECTION_CONNECTED).str(CONNECTION_EVT_ACCOUNT, &handle);
                outs.push(c);
                self.accounts.insert(handle, Account { name, display });
            }
            req::ACCOUNT_LOGOUT => {
                let handle = unsafe { rd_str(req, LOGOUT_ACCOUNT_HANDLE) };
                let mut e = Out::event(evt::ACCOUNT_LOGIN_STATE_CHANGE);
                e.i32(LOGIN_EVT_STATE, LOGGING_OUT).str(LOGIN_EVT_ACCOUNT, &handle);
                outs.push(e);
                let mine: Vec<String> = self.sessions.iter().filter(|(_, v)| v.account == handle).map(|(k, _)| k.clone()).collect();
                for h in mine {
                    outs.extend(self.leave(lan, &h));
                }
                let mut e = Out::event(evt::ACCOUNT_LOGIN_STATE_CHANGE);
                e.i32(LOGIN_EVT_STATE, LOGGED_OUT).str(LOGIN_EVT_ACCOUNT, &handle);
                outs.push(e);
                self.accounts.remove(&handle);
                log!("vivox: logged out");
            }
            req::SESSIONGROUP_ADD_SESSION => outs.extend(self.join(lan, req)),
            req::SESSIONGROUP_REMOVE_SESSION => {
                let h = unsafe { rd_str(req, REMOVE_SESSION) };
                if self.sessions.contains_key(&h) {
                    outs.extend(self.leave(lan, &h));
                } else {
                    outs[0].i32(RESP_RETURN_CODE, 1).i32(RESP_STATUS_CODE, E_NO_EXIST);
                }
            }
            req::SESSION_SET_3D_POSITION => {
                let h = unsafe { rd_str(req, POS_SESSION) };
                let speaker = unsafe { rd_vec3(req, POS_SPEAKER) };
                let listener = unsafe { rd_vec3(req, POS_LISTENER) };
                let facing = unsafe { (rd_vec3(req, POS_LISTENER_AT), rd_vec3(req, POS_LISTENER_UP)) };
                if let Some(sess) = self.sessions.get_mut(&h) {
                    if sess.speaker != Some(speaker) {
                        sess.position_dirty = true;
                    }
                    sess.speaker = Some(speaker);
                    sess.listener = Some(listener);
                    sess.facing = Some(facing);
                    let peers: Vec<PeerId> = sess.remotes.keys().copied().collect();
                    for p in peers {
                        sess.apply_gain(p);
                    }
                }
            }
            req::CONNECTOR_MUTE_LOCAL_MIC => {
                self.mic_muted = unsafe { rd_i32(req, MUTE_LEVEL) } != 0;
                log!("vivox: microphone {}", if self.mic_muted { "muted" } else { "on" });
                self.apply_sending();
            }
            req::CONNECTOR_MUTE_LOCAL_SPEAKER => {
                self.speaker_muted = unsafe { rd_i32(req, MUTE_LEVEL) } != 0;
                self.apply_volumes();
            }
            req::SESSION_SET_LOCAL_RENDER_VOLUME => {
                let h = unsafe { rd_str(req, RENDER_VOLUME_SESSION) };
                let v = unsafe { rd_i32(req, RENDER_VOLUME) };
                if let Some(sess) = self.sessions.get_mut(&h) {
                    sess.volume = v.clamp(-50, 50);
                }
                self.apply_volumes();
            }
            req::AUX_SET_SPEAKER_LEVEL => {
                self.speaker_level = unsafe { rd_i32(req, LEVEL) }.clamp(0, 100);
                self.apply_volumes();
            }
            req::AUX_SET_MIC_LEVEL => {
                crate::log::gap("vivox", "microphone level (PeerParty's microphone level is automatic)");
            }
            req::SESSION_SET_PARTICIPANT_MUTE_FOR_ME => {
                let h = unsafe { rd_str(req, MUTE_FOR_ME_SESSION) };
                let uri = unsafe { rd_str(req, MUTE_FOR_ME_URI) };
                let mute = unsafe { rd_i32(req, MUTE_FOR_ME_MUTE) } != 0;
                let found = self.sessions.get_mut(&h).and_then(|sess| {
                    let p = sess.remotes.iter_mut().find(|(_, r)| r.uri == uri).map(|(p, r)| {
                        r.muted = mute;
                        *p
                    })?;
                    voice::set_receiving(&sess.room, Some(p), !mute);
                    outs.push(sess.updated(Some(p)));
                    Some(p)
                });
                crate::log::throttled(&format!("vivox mute for me {uri}"), 20, |n| match found {
                    Some(_) => log!("vivox: the game {} {uri} for this player n={n}", if mute { "muted" } else { "unmuted" }),
                    None => log!("vivox: the game {} {uri}, who isn't in that channel n={n}", if mute { "muted" } else { "unmuted" }),
                });
            }
            req::SESSION_SET_PARTICIPANT_VOLUME_FOR_ME => {
                let h = unsafe { rd_str(req, VOLUME_FOR_ME_SESSION) };
                let uri = unsafe { rd_str(req, VOLUME_FOR_ME_URI) };
                let v = unsafe { rd_i32(req, VOLUME_FOR_ME_VOLUME) }.clamp(0, 100);
                if let Some(sess) = self.sessions.get_mut(&h) {
                    let found = sess.remotes.iter_mut().find(|(_, r)| r.uri == uri).map(|(p, r)| {
                        r.volume = v;
                        *p
                    });
                    if let Some(p) = found {
                        sess.apply_gain(p);
                    }
                }
                crate::log::throttled(&format!("vivox volume for me {uri}"), 20, |n| {
                    log!("vivox: the game set {uri} to volume {v} (50 unchanged) for this player n={n}")
                });
            }
            req::SESSIONGROUP_SET_TX_ALL_SESSIONS => {
                self.tx = Tx::All;
                self.apply_sending();
            }
            req::SESSIONGROUP_SET_TX_NO_SESSION => {
                self.tx = Tx::None;
                self.apply_sending();
            }
            req::SESSIONGROUP_SET_TX_SESSION => {
                self.tx = Tx::One(unsafe { rd_str(req, TX_SESSION) });
                self.apply_sending();
            }
            req::AUX_GET_CAPTURE_DEVICES | req::AUX_GET_RENDER_DEVICES => {
                let output = kind == req::AUX_GET_RENDER_DEVICES;
                let snap = voice::snapshot();
                self.devices_seen = snap.version;
                fill_devices(&mut outs[0], snap.side(output), &voice::chosen(output));
            }
            req::AUX_SET_CAPTURE_DEVICE | req::AUX_SET_RENDER_DEVICE => {
                let output = kind == req::AUX_SET_RENDER_DEVICE;
                let which = unsafe { rd_str(req, DEVICE_SPECIFIER) };
                let choice = choice_of(&which);
                let snap = voice::snapshot();
                let side = snap.side(output);
                if let voice::Choice::Device(id) = &choice {
                    if side.get(id).is_none() {
                        log!("vivox: the game chose a device Windows doesn't have now ('{id}'); the default communications device plays until it's back");
                    }
                }
                let open = side.effective(&choice).map(|e| e.id.clone()).unwrap_or_default();
                voice::choose_device(output, choice);
                outs[0].str(SET_DEVICE_RESP_OPEN, &open);
            }
            req::ACCOUNT_CONTROL_COMMUNICATIONS => {
                let op = unsafe { rd_i32(req, COMMS_OPERATION) };
                let uris = unsafe { rd_str(req, COMMS_USER_URIS) };
                let names: Vec<String> = uris.split('\n').map(|u| account_name_of(u.trim())).filter(|n| !n.is_empty()).collect();
                let answer = match op {
                    COMMS_BLOCK | COMMS_UNBLOCK => {
                        for n in &names {
                            if op == COMMS_BLOCK {
                                self.blocked.insert(n.clone());
                            } else {
                                self.blocked.remove(n);
                            }
                        }
                        uris.clone()
                    }
                    COMMS_LIST => self.blocked_uris(),
                    COMMS_CLEAR => {
                        let all = self.blocked_uris();
                        self.blocked.clear();
                        all
                    }
                    _ => String::new(),
                };
                self.apply_blocks();
                outs[0].str(COMMS_RESP_BLOCKED_URIS, &answer);
            }
            _ => {
                crate::log::gap("vivox", &format!("request {} (answered as done, nothing carried)", request_name(kind)));
            }
        }
        outs
    }

    fn join(&mut self, lan: Option<&'static Lan>, req: usize) -> Vec<Out> {
        let group = unsafe { rd_str(req, ADD_GROUP) };
        let uri = unsafe { rd_str(req, ADD_URI) };
        let handle = unsafe { rd_str(req, ADD_SESSION) };
        let account = unsafe { rd_str(req, ADD_ACCOUNT) };
        let audio = unsafe { rd_i32(req, ADD_CONNECT_AUDIO) } != 0;
        let text = unsafe { rd_i32(req, ADD_CONNECT_TEXT) } != 0;
        let Some(acct) = self.accounts.get(&account) else {
            log!("vivox: a channel join for account '{account}', which isn't logged in");
            return vec![];
        };
        let domain = domain_of(&uri).to_string();
        let self_uri = account_uri(&acct.name, &domain);
        let room = room_of(&uri);
        let props = props_of(&uri);
        log!(
            "vivox: joined channel {uri} as {self_uri} (audio={audio} text={text}{})",
            props.map(|p| format!(", positional: audible {} m, full volume within {} m, fade model {}", p.audible, p.conversational, p.model)).unwrap_or_default()
        );
        if audio {
            if let Some(lan) = lan {
                voice::join(lan, &room, true, true);
                // Copies already in the room: they answer our join and are greeted then.
            }
        }
        let sess = Session {
            handle: handle.clone(),
            group: group.clone(),
            me: self_uri.clone(),
            account,
            uri: uri.clone(),
            room,
            domain,
            props,
            audio,
            text,
            volume: 0,
            remotes: HashMap::new(),
            greeted: HashSet::new(),
            self_speaking: false,
            speaker: None,
            listener: None,
            facing: None,
            position_dirty: false,
            position_sent: None,
        };
        let mut outs = Vec::new();
        if audio {
            for state in [MEDIA_CONNECTING, MEDIA_CONNECTED] {
                let mut e = Out::event(evt::MEDIA_STREAM_UPDATED);
                e.str(EVT_GROUP, &group).str(EVT_SESSION, &handle).i32(MEDIA_EVT_STATE, state);
                outs.push(e);
            }
        }
        if text {
            let mut e = Out::event(evt::TEXT_STREAM_UPDATED);
            e.str(EVT_GROUP, &group).str(EVT_SESSION, &handle).i32(TEXT_EVT_ENABLED, 1).i32(TEXT_EVT_STATE, TEXT_CONNECTED);
            outs.push(e);
        }
        let mut me = Out::event(evt::PARTICIPANT_ADDED);
        me.str(EVT_GROUP, &group)
            .str(EVT_SESSION, &handle)
            .str(PART_URI, &self_uri)
            .str(PART_ADDED_ACCOUNT, &acct.name)
            .str(PART_ADDED_DISPLAY_NAME, &acct.display)
            .str(PART_ADDED_DISPLAYNAME, &acct.display)
            .i32(PART_ADDED_CURRENT_USER, 1);
        outs.push(me);
        self.sessions.insert(handle.clone(), sess);
        if let Some(sess) = self.sessions.get(&handle) {
            outs.push(sess.updated(None));
        }
        self.apply_sending();
        self.apply_volumes();
        outs
    }

    fn leave(&mut self, lan: Option<&'static Lan>, handle: &str) -> Vec<Out> {
        let Some(sess) = self.sessions.remove(handle) else { return vec![] };
        if let Some(lan) = lan {
            voice::leave(lan, &sess.room);
        }
        log!("vivox: left channel {}", sess.uri);
        let mut outs = Vec::new();
        for r in sess.remotes.values() {
            outs.push(sess.removed(&r.uri, &r.name, false));
        }
        let name = self.accounts.get(&sess.account).map(|a| a.name.clone()).unwrap_or_default();
        outs.push(sess.removed(&sess.me, &name, true));
        if sess.audio {
            for state in [MEDIA_DISCONNECTING, MEDIA_DISCONNECTED] {
                let mut e = Out::event(evt::MEDIA_STREAM_UPDATED);
                e.str(EVT_GROUP, &sess.group).str(EVT_SESSION, handle).i32(MEDIA_EVT_STATE, state);
                outs.push(e);
            }
        }
        if sess.text {
            let mut e = Out::event(evt::TEXT_STREAM_UPDATED);
            e.str(EVT_GROUP, &sess.group).str(EVT_SESSION, handle).i32(TEXT_EVT_STATE, TEXT_DISCONNECTED);
            outs.push(e);
        }
        let mut e = Out::event(evt::SESSION_REMOVED);
        e.str(EVT_GROUP, &sess.group).str(EVT_SESSION, handle).str(REMOVED_EVT_URI, &sess.uri);
        outs.push(e);
        outs
    }

    fn blocked_uris(&self) -> String {
        let domain = self.sessions.values().next().map(|s| s.domain.clone()).unwrap_or_default();
        self.blocked.iter().map(|n| account_uri(n, &domain)).collect::<Vec<_>>().join("\n")
    }

    /// Our audio goes to the channels transmission is set to, unless muted.
    fn apply_sending(&self) {
        for (h, sess) in &self.sessions {
            let tx = match &self.tx {
                Tx::All => true,
                Tx::None => false,
                Tx::One(one) => one == h,
            };
            voice::set_sending(&sess.room, tx && !self.mic_muted);
        }
    }

    fn apply_volumes(&self) {
        for sess in self.sessions.values() {
            let factor = if self.speaker_muted {
                0.0
            } else {
                (sess.volume + 50) as f32 / 50.0 * self.speaker_level as f32 / 50.0
            };
            voice::set_volume(&sess.room, None, factor * 50.0);
        }
    }

    fn apply_blocks(&self) {
        for sess in self.sessions.values() {
            for (p, r) in &sess.remotes {
                voice::set_blocked(&sess.room, *p, self.blocked.contains(&r.name));
            }
        }
    }
}

/// What a device list's key asks for.
fn choice_of(key: &str) -> voice::Choice {
    match key {
        "" | DEFAULT_SYSTEM => voice::Choice::System,
        DEFAULT_COMMUNICATION => voice::Choice::Communications,
        id => voice::Choice::Device(id.to_string()),
    }
}

/// A device list response: Vivox's two default entries, then Windows' devices; the
/// current entry is the choice, the effective one the device it comes to.
fn fill_devices(o: &mut Out, side: &voice::Side, chosen: &voice::Choice) {
    let mut entries = vec![
        (DEFAULT_SYSTEM.to_string(), DEFAULT_SYSTEM.to_string(), DEVICE_DEFAULT_SYSTEM),
        (DEFAULT_COMMUNICATION.to_string(), DEFAULT_COMMUNICATION.to_string(), DEVICE_DEFAULT_COMMUNICATION),
    ];
    entries.extend(side.list.iter().map(|e| (e.id.clone(), e.name.clone(), DEVICE_SPECIFIC)));
    let devs: Vec<usize> = entries
        .iter()
        .map(|(id, name, ty)| {
            let (d, i, n) = (heap_alloc(DEVICE_SIZE), heap_str(id), heap_str(name));
            if d != 0 {
                unsafe {
                    wr_ptr(d, DEVICE_ID, i);
                    wr_ptr(d, DEVICE_NAME, n);
                    wr_i32(d, DEVICE_TYPE, *ty);
                }
            }
            o.owned.extend([d, i, n]);
            d
        })
        .collect();
    let list = heap_alloc(8 * devs.len());
    if list != 0 {
        for (i, d) in devs.iter().enumerate() {
            unsafe { wr_ptr(list, i * 8, *d) };
        }
    }
    o.owned.push(list);
    // A device's own entry, or the default entry `fallback`.
    let find = |id: Option<&str>, fallback: usize| {
        id.and_then(|id| entries.iter().position(|e| e.2 == DEVICE_SPECIFIC && e.0 == id)).map_or(devs[fallback], |i| devs[i])
    };
    let current = match chosen {
        voice::Choice::System => devs[0],
        voice::Choice::Communications => devs[1],
        voice::Choice::Device(id) => find(Some(id), 1),
    };
    o.i32(DEVICES_COUNT, devs.len() as i32)
        .ptr(DEVICES_LIST, list)
        .ptr(DEVICES_CURRENT, current)
        .ptr(DEVICES_EFFECTIVE, find(side.effective(chosen).map(|e| e.id.as_str()), 1))
        .ptr(DEVICES_DEFAULT, find(side.system.as_deref(), 0))
        .ptr(DEVICES_DEFAULT_COMMUNICATION, find(side.communications.as_deref(), 1));
}

/// `.issuer.name.` from `sip:.issuer.name.@domain`.
fn account_name_of(uri: &str) -> String {
    let u = uri.strip_prefix("sip:").unwrap_or(uri);
    u.split('@').next().unwrap_or_default().to_string()
}

impl Session {
    /// A participant's state (`None`: our own).
    fn updated(&self, peer: Option<PeerId>) -> Out {
        let (uri, speaking, muted, current) = match peer.and_then(|p| self.remotes.get(&p)) {
            Some(r) => (r.uri.as_str(), r.speaking, r.muted, 0),
            None => (self.me.as_str(), self.self_speaking, false, 1),
        };
        let mut e = Out::event(evt::PARTICIPANT_UPDATED);
        e.str(EVT_GROUP, &self.group)
            .str(EVT_SESSION, &self.handle)
            .str(PART_URI, uri)
            .i32(PART_UPDATED_SPEAKING, speaking as i32)
            .i32(PART_UPDATED_VOLUME, 50)
            .f64(PART_UPDATED_ENERGY, if speaking { 0.5 } else { 0.0 })
            .i32(PART_UPDATED_ACTIVE_MEDIA, (self.audio as i32) | (self.text as i32) << 1)
            .i32(PART_UPDATED_MUTED_FOR_ME, muted as i32)
            .i32(PART_UPDATED_CURRENT_USER, current);
        e
    }

    fn removed(&self, uri: &str, name: &str, current: bool) -> Out {
        let mut e = Out::event(evt::PARTICIPANT_REMOVED);
        e.str(EVT_GROUP, &self.group)
            .str(EVT_SESSION, &self.handle)
            .str(PART_URI, uri)
            .str(PART_REMOVED_ACCOUNT, name)
            .i32(PART_REMOVED_CURRENT_USER, current as i32);
        e
    }

    /// A speaker's volume and place from where they are and where we listen, times the
    /// player's own volume for them.
    fn apply_gain(&self, peer: PeerId) {
        let (g, p) = match (self.props, self.listener, self.remotes.get(&peer).and_then(|r| r.position)) {
            (Some(props), Some(l), Some(s)) => {
                let d = distance(l, s);
                let g = fade(&props, d);
                let p = self.facing.map_or(0.0, |(at, up)| pan(l, at, up, s));
                crate::log::throttled(&format!("vivox gain {peer}"), 4, |n| {
                    log!("vivox: {peer} is {d:.1} away in {}: volume {:.0}%, pan {p:+.2} n={n}", self.uri, g * 100.0)
                });
                (g, p)
            }
            _ => (1.0, 0.0),
        };
        let mine = self.remotes.get(&peer).map_or(50, |r| r.volume) as f32 / 50.0;
        voice::set_volume(&self.room, Some(peer), g as f32 * mine * 50.0);
        voice::set_pan(&self.room, peer, p as f32);
    }
}

// ---- the LAN side, from the game's polling ----

fn pump(s: &mut State) {
    let Some(lan) = net::lan() else { return };
    for ev in net::take(Front::Vivox) {
        voice::on_event(lan, &ev);
    }
    voice::pump(lan);
    // Windows' devices changed since the game last listed them: Unity lists them again.
    let devices = voice::devices_version();
    if s.devices_seen != 0 && devices != s.devices_seen {
        s.devices_seen = devices;
        log!("vivox: sound devices changed; the game is told");
        s.queue.push_back(Out::event(evt::AUDIO_DEVICE_HOT_SWAP));
    }
    let handles: Vec<String> = s.sessions.keys().cloned().collect();
    let mut outs = Vec::new();
    for h in handles {
        let blocked = s.blocked.clone();
        let Some(acct) = s.sessions.get(&h).and_then(|x| s.accounts.get(&x.account)).map(|a| (a.name.clone(), a.display.clone())) else {
            continue;
        };
        let Some(sess) = s.sessions.get_mut(&h) else { continue };
        for ev in voice::take(&sess.room) {
            match ev {
                RoomEvent::Joined(p) => {
                    greet(lan, sess, p, &acct);
                }
                RoomEvent::Left(p) => {
                    sess.greeted.remove(&p);
                    if let Some(r) = sess.remotes.remove(&p) {
                        log!("vivox: {} left channel {}", r.uri, sess.uri);
                        outs.push(sess.removed(&r.uri, &r.name, false));
                    }
                }
                RoomEvent::Data(p, bytes) => {
                    if let Some((name, display)) = parse_hello(&bytes) {
                        if !sess.greeted.contains(&p) {
                            greet(lan, sess, p, &acct);
                        }
                        if sess.remotes.contains_key(&p) {
                            continue;
                        }
                        let uri = account_uri(&name, &sess.domain);
                        log!("vivox: {uri} ({display}) is in channel {}", sess.uri);
                        let mut e = Out::event(evt::PARTICIPANT_ADDED);
                        e.str(EVT_GROUP, &sess.group)
                            .str(EVT_SESSION, &h)
                            .str(PART_URI, &uri)
                            .str(PART_ADDED_ACCOUNT, &name)
                            .str(PART_ADDED_DISPLAY_NAME, &display)
                            .str(PART_ADDED_DISPLAYNAME, &display)
                            .i32(PART_ADDED_CURRENT_USER, 0);
                        outs.push(e);
                        voice::set_blocked(&sess.room, p, blocked.contains(&name));
                        sess.remotes.insert(p, Remote { uri, name, position: None, speaking: false, muted: false, volume: 50 });
                        sess.apply_gain(p);
                        outs.push(sess.updated(Some(p)));
                    } else if let Some(pos) = parse_position(&bytes) {
                        if let Some(r) = sess.remotes.get_mut(&p) {
                            r.position = Some(pos);
                            sess.apply_gain(p);
                        }
                    }
                }
                RoomEvent::Speaking(p, on) => {
                    if let Some(r) = sess.remotes.get_mut(&p) {
                        if r.speaking != on {
                            r.speaking = on;
                            if on {
                                crate::log::throttled(&format!("vivox speaking {p}"), 5, |n| {
                                    log!("vivox: hearing {} in {}{} n={n}", r.uri, sess.uri, if r.muted { " (muted for this player)" } else { "" })
                                });
                            }
                            outs.push(sess.updated(Some(p)));
                        }
                    }
                }
                RoomEvent::Audio(..) => {}
            }
        }
        let talking = voice::talking(&sess.room);
        if talking != sess.self_speaking {
            sess.self_speaking = talking;
            if talking {
                crate::log::throttled(&format!("vivox talking {h}"), 5, |n| log!("vivox: this player is talking in {} n={n}", sess.uri));
            }
            outs.push(sess.updated(None));
        }
        if sess.position_dirty && sess.position_sent.is_none_or(|t| t.elapsed() >= POSITION_EVERY) {
            if let Some(pos) = sess.speaker {
                voice::send_data(lan, &sess.room, &position_msg(pos));
            }
            sess.position_dirty = false;
            sess.position_sent = Some(Instant::now());
        }
    }
    s.queue.extend(outs);
}

/// Tells a room member who we are, and where.
fn greet(lan: &Lan, sess: &mut Session, peer: PeerId, acct: &(String, String)) {
    voice::send_data_to(lan, &sess.room, &[peer], &hello(&acct.0, &acct.1));
    if let Some(pos) = sess.speaker {
        voice::send_data_to(lan, &sess.room, &[peer], &position_msg(pos));
    }
    sess.greeted.insert(peer);
}

#[cfg(test)]
mod tests {
    use super::*;

    const POSITIONAL: &str = "sip:confctl-d-dish1234-dev.lobbyABC.0f7c3c43-0d1d-4b6e-9a3e-1f2b3c4d5e6f!p-32-1-1.000-1@mtu1xp.vivox.com";

    #[test]
    fn positional_properties_come_from_the_uri() {
        let p = props_of(POSITIONAL).unwrap();
        assert_eq!(p, Props { audible: 32.0, conversational: 1.0, rolloff: 1.0, model: 1 });
        assert!(props_of("sip:confctl-g-dish1234-dev.lobbyABC@mtu1xp.vivox.com").is_none());
        assert!(props_of("sip:confctl-d-x.y!p-1-2-3@d").is_none());
    }

    #[test]
    fn rooms_ignore_positional_properties_and_accounts_get_unitys_uri() {
        assert_eq!(
            room_of(POSITIONAL),
            "vivox:sip:confctl-d-dish1234-dev.lobbyABC.0f7c3c43-0d1d-4b6e-9a3e-1f2b3c4d5e6f@mtu1xp.vivox.com"
        );
        assert_eq!(room_of("sip:confctl-g-a.b@d"), "vivox:sip:confctl-g-a.b@d");
        assert_eq!(domain_of(POSITIONAL), "mtu1xp.vivox.com");
        assert_eq!(account_uri(".dish1234-dev.a1456ddca99c0f11f3715476e228.", "mtu1xp.vivox.com"), "sip:.dish1234-dev.a1456ddca99c0f11f3715476e228.@mtu1xp.vivox.com");
        assert_eq!(account_uri("sip:.i.n.@d", "other"), "sip:.i.n.@d");
        assert_eq!(account_name_of("sip:.i.n.@d"), ".i.n.");
    }

    #[test]
    fn the_fade_models_are_full_near_and_silent_past_the_audible_distance() {
        let inv = Props { audible: 32.0, conversational: 1.0, rolloff: 1.0, model: 1 };
        assert_eq!(fade(&inv, 0.5), 1.0);
        assert!((fade(&inv, 2.0) - 0.5).abs() < 1e-9);
        assert!((fade(&inv, 31.0) - 1.0 / 31.0).abs() < 1e-9);
        assert_eq!(fade(&inv, 33.0), 0.0);
        let lin = Props { model: 2, ..inv };
        assert!((fade(&lin, 16.5) - 0.5).abs() < 1e-9);
        assert_eq!(fade(&lin, 32.0), 0.0);
        let exp = Props { model: 3, rolloff: 2.0, ..inv };
        assert!((fade(&exp, 2.0) - 0.25).abs() < 1e-9);
        assert_eq!(fade(&inv, f64::NAN), 0.0);
    }

    /// Unity's axes: x right, y up, z forward.
    #[test]
    fn speakers_pan_by_where_the_listener_faces() {
        let (o, fwd, up) = ([0.0; 3], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]);
        assert!((pan(o, fwd, up, [5.0, 0.0, 0.0]) - 1.0).abs() < 1e-9, "to the right");
        assert!((pan(o, fwd, up, [-5.0, 0.0, 0.0]) + 1.0).abs() < 1e-9, "to the left");
        assert!(pan(o, fwd, up, [0.0, 0.0, 5.0]).abs() < 1e-9, "ahead");
        assert!(pan(o, fwd, up, [0.0, 0.0, -5.0]).abs() < 1e-9, "behind");
        assert!((pan(o, fwd, up, [1.0, 0.0, 1.0]) - 0.5f64.sqrt()).abs() < 1e-9, "ahead and right");
        // Turned to face +x (right of the world): a speaker at -z is now to the right.
        assert!((pan(o, [1.0, 0.0, 0.0], up, [0.0, 0.0, -3.0]) - 1.0).abs() < 1e-9);
        // Panning off (at = up), unknown facing, or on top of the listener: the middle.
        assert_eq!(pan(o, up, up, [5.0, 0.0, 0.0]), 0.0);
        assert_eq!(pan(o, [0.0; 3], [0.0; 3], [5.0, 0.0, 0.0]), 0.0);
        assert_eq!(pan(o, fwd, up, o), 0.0);
    }

    #[test]
    fn device_lists_lead_with_vivoxs_defaults_and_point_at_the_devices() {
        let ep = |id: &str, name: &str| voice::Endpoint { id: id.into(), name: name.into() };
        let side = voice::Side {
            list: vec![ep("{a}", "Speakers"), ep("{b}", "Headset")],
            system: Some("{a}".into()),
            communications: Some("{b}".into()),
        };
        let entry = |o: &Out, off: usize| unsafe {
            let d = rd_ptr(o.p, off);
            (rd_str(d, DEVICE_ID), rd_i32(d, DEVICE_TYPE))
        };
        let mut o = Out::new(MSG_RESPONSE);
        fill_devices(&mut o, &side, &voice::Choice::System);
        unsafe {
            assert_eq!(rd_i32(o.p, DEVICES_COUNT), 4);
            let list = rd_ptr(o.p, DEVICES_LIST);
            let keys: Vec<String> = (0..4).map(|i| rd_str(rd_ptr(list, i * 8), DEVICE_ID)).collect();
            assert_eq!(keys, [DEFAULT_SYSTEM, DEFAULT_COMMUNICATION, "{a}", "{b}"]);
            assert_eq!(rd_str(rd_ptr(list, 24), DEVICE_NAME), "Headset");
        }
        assert_eq!(entry(&o, DEVICES_CURRENT), (DEFAULT_SYSTEM.into(), DEVICE_DEFAULT_SYSTEM));
        assert_eq!(entry(&o, DEVICES_EFFECTIVE), ("{a}".into(), DEVICE_SPECIFIC));
        assert_eq!(entry(&o, DEVICES_DEFAULT), ("{a}".into(), DEVICE_SPECIFIC));
        assert_eq!(entry(&o, DEVICES_DEFAULT_COMMUNICATION), ("{b}".into(), DEVICE_SPECIFIC));
        o.free();
        // A device chosen by its key, and one that went away (the communications default plays).
        let mut o = Out::new(MSG_RESPONSE);
        fill_devices(&mut o, &side, &choice_of("{a}"));
        assert_eq!(entry(&o, DEVICES_CURRENT).0, "{a}");
        o.free();
        let mut o = Out::new(MSG_RESPONSE);
        fill_devices(&mut o, &side, &choice_of("{gone}"));
        assert_eq!(entry(&o, DEVICES_CURRENT).0, DEFAULT_COMMUNICATION);
        assert_eq!(entry(&o, DEVICES_EFFECTIVE).0, "{b}");
        o.free();
        assert_eq!(choice_of(DEFAULT_COMMUNICATION), voice::Choice::Communications);
        assert_eq!(choice_of(""), voice::Choice::System);
    }

    #[test]
    fn hellos_and_positions_round_trip() {
        assert_eq!(parse_hello(&hello(".i.n.", "Timber")), Some((".i.n.".into(), "Timber".into())));
        assert_eq!(parse_hello(&hello(".i.n.", "")), Some((".i.n.".into(), "".into())));
        assert_eq!(parse_hello(b"H"), None);
        assert_eq!(parse_hello(b"P123"), None);
        assert_eq!(parse_position(&position_msg([1.0, -2.5, 3.0])), Some([1.0, -2.5, 3.0]));
        assert_eq!(parse_position(&position_msg([f64::NAN, 0.0, 0.0])), None);
        assert_eq!(parse_position(b"P"), None);
        assert!((distance([0.0, 0.0, 0.0], [3.0, 4.0, 0.0]) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn built_messages_carry_their_fields() {
        let mut e = Out::event(evt::PARTICIPANT_ADDED);
        e.str(PART_URI, "sip:.i.n.@d").i32(PART_ADDED_CURRENT_USER, 1);
        unsafe {
            assert_eq!(rd_i32(e.p, MSG_TYPE), MSG_EVENT);
            assert_eq!(rd_i32(e.p, EVT_TYPE), evt::PARTICIPANT_ADDED);
            assert_eq!(rd_str(e.p, PART_URI), "sip:.i.n.@d");
            assert_eq!(rd_i32(e.p, PART_ADDED_CURRENT_USER), 1);
        }
        e.free();
    }
}
