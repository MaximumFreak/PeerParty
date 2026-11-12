//! PlayFab Party chat on the LAN: chat controls, text chat and voice chat.
//!
//! A local chat control that the game connects to a network is announced on the
//! network's LAN lobby as member attributes (`cc` = its entity id, `ccl` = its
//! language); the other copies show it as a remote chat control (created, then joined
//! to that network), the way they show endpoints. Voice and text ride the voice room
//! `party:<guid>` of voice.rs (core socket `rtc:party:<guid>`): audio as raw PCM
//! frames, text as reliable room data sent only to the targeted copies.
//!
//! What follows Party.h:
//! - Chat permissions (`PartyChatControlSetPermissions`) default to none and both
//!   sides must agree: the sender only sends audio to chat controls it gave
//!   `SEND_MICROPHONE_AUDIO`, the receiver only takes audio from those it gave
//!   `RECEIVE_MICROPHONE_AUDIO` (and hasn't muted), and text is only delivered to
//!   receivers that gave the sender `RECEIVE_TEXT` (and haven't muted its text).
//! - `PartyChatControlSendText` to a local target fails synchronously; text reaches a
//!   target only through a network both are connected to.
//! - Audio devices (`PartyChatControlSetAudioInput/Output`): the system default, the
//!   platform user's default (Windows' default communications device) or a device by
//!   its endpoint id (manual) moves voice.rs's microphone or speakers there, and the
//!   selection's device id is that endpoint's; the state is "not found" when Windows
//!   has no such device or `PEERPARTY_VOICE=0`. The microphone only opens while a
//!   connected chat control has an input selected and isn't muted.
//! - Audio manipulation streams: the capture sink (game-made voice, sent like the
//!   microphone), the render sink (game-mixed audio to the speakers) and remote voice
//!   source streams (a remote chat control's voice handed to the game as 24 kHz mono
//!   16-bit, 40 ms buffers, instead of being played).
//!
//! Not carried: transcription (`SetTranscriptionOptions` with transcription turned on
//! reports unsupported; no transcriptions arrive), translation and text filtering (the
//! options are kept, text arrives as sent), text-to-speech (no profiles; synthesizing
//! reports unsupported), a local chat control's own voice stream, remote chat
//! indicators "no remote input" / "remote input muted", and noise suppression.
//!
//! References: Party_c.h / Party.h from the PlayFab Party C++ SDK for Windows
//! (nuget.org/packages/Microsoft.PlayFab.PlayFabParty.Cpp.Windows, 1.7.11 and 1.10.12;
//! the chat structs are the same in both) and the reference docs
//! (learn.microsoft.com/gaming/playfab/features/multiplayer/networking/reference);
//! PlayFab's open-source Unity plugin (github.com/PlayFab/PlayFabPartyUnity,
//! PartyCSharpSDK/Interop and PlayFabMultiplayerManager.cs) for the order games call
//! things in: create chat control, set audio input/output, connect it after creating
//! the endpoint, set permissions on each remote chat control as it's created; and the
//! PlayFabParty demo (github.com/PlayFab/PlayFabParty, NetworkManager.cpp) for text
//! sent to every remote chat control from `PartyGetChatControls`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CString, c_char};

use peerparty_core::{Attr, Lan, PeerId, Value};

use super::{
    DataBuffer, E_BAD_ARG, E_BAD_HANDLE, E_NO_BUFFER, E_UNSUPPORTED, NETWORK_NO_LONGER_EXISTS, OK,
    REASON_REQUESTED, STATE, SUCCEEDED, State, cstring, handle, put, text, trace,
};
use crate::log;
use crate::net;
use crate::voice::{self, Frame, RoomEvent};

pub(super) const CHAT_KEY: &str = "cc";
pub(super) const CHAT_LANGUAGE_KEY: &str = "ccl";
const ROOM_PREFIX: &str = "party:";
const ENTITY_TYPE: &str = "title_player_account";

// PARTY_STATE_CHANGE_TYPE
const CREATE_CHAT_CONTROL_COMPLETED: u32 = 31;
const DESTROY_CHAT_CONTROL_COMPLETED: u32 = 32;
const CHAT_CONTROL_CREATED: u32 = 33;
const CHAT_CONTROL_DESTROYED: u32 = 34;
const SET_CHAT_AUDIO_ENCODER_BITRATE_COMPLETED: u32 = 35;
pub(super) const CHAT_TEXT_RECEIVED: u32 = 36;
const SET_CHAT_AUDIO_INPUT_COMPLETED: u32 = 38;
const SET_CHAT_AUDIO_OUTPUT_COMPLETED: u32 = 39;
const LOCAL_CHAT_AUDIO_INPUT_CHANGED: u32 = 40;
const LOCAL_CHAT_AUDIO_OUTPUT_CHANGED: u32 = 41;
const CHAT_CONTROL_JOINED_NETWORK: u32 = 46;
const CHAT_CONTROL_LEFT_NETWORK: u32 = 47;
const CONNECT_CHAT_CONTROL_COMPLETED: u32 = 48;
const DISCONNECT_CHAT_CONTROL_COMPLETED: u32 = 49;
const POPULATE_AVAILABLE_TEXT_TO_SPEECH_PROFILES_COMPLETED: u32 = 50;
const SET_LANGUAGE_COMPLETED: u32 = 55;
const SET_TRANSCRIPTION_OPTIONS_COMPLETED: u32 = 56;
const SET_TEXT_CHAT_OPTIONS_COMPLETED: u32 = 57;
const CONFIGURE_VOICE_STREAM_COMPLETED: u32 = 58;
const CONFIGURE_CAPTURE_STREAM_COMPLETED: u32 = 59;
const CONFIGURE_RENDER_STREAM_COMPLETED: u32 = 60;

// PARTY_CHAT_PERMISSION_OPTIONS
const SEND_MICROPHONE_AUDIO: u32 = 0x1;
const RECEIVE_MICROPHONE_AUDIO: u32 = 0x4;
const RECEIVE_TEXT: u32 = 0x10;
const ALL_PERMISSIONS: u32 = 0x1F;

// PARTY_AUDIO_DEVICE_SELECTION_TYPE
const SELECTION_NONE: u32 = 0;
const SELECTION_SYSTEM_DEFAULT: u32 = 1;
const SELECTION_MANUAL: u32 = 3;

// PARTY_AUDIO_INPUT_STATE / PARTY_AUDIO_OUTPUT_STATE (same numbers for these three)
const DEVICE_NONE: u32 = 0;
const DEVICE_INITIALIZED: u32 = 1;
const DEVICE_NOT_FOUND: u32 = 2;

// PARTY_LOCAL_CHAT_CONTROL_CHAT_INDICATOR
const LOCAL_SILENT: u32 = 0;
const LOCAL_TALKING: u32 = 1;
const LOCAL_INPUT_MUTED: u32 = 2;
const LOCAL_NO_INPUT: u32 = 3;

// PARTY_CHAT_CONTROL_CHAT_INDICATOR
const REMOTE_SILENT: u32 = 0;
const REMOTE_TALKING: u32 = 1;
const REMOTE_VOICE_DISABLED: u32 = 2;
const REMOTE_MUTED: u32 = 3;

// PARTY_VOICE_CHAT_TRANSCRIPTION_OPTIONS that ask for transcriptions.
const TRANSCRIBE: u32 = 0x1 | 0x2 | 0x4 | 0x40;

// PARTY_AUDIO_SAMPLE_TYPE
const SAMPLE_INTEGER: u32 = 0;
const SAMPLE_FLOAT: u32 = 1;

/// c_maxChatTextMessageLength and c_maxLanguageCodeLength.
const MAX_TEXT: usize = 1023;
const MAX_LANGUAGE: usize = 84;
/// Party's voice streams: 24 kHz mono, 40 ms buffers.
const VOICE_RATE: u32 = 24_000;
const BUFFER_SAMPLES: usize = 960;
/// Room data kind for chat text.
const TEXT_MESSAGE: u8 = 1;

// ---- Party.h chat structs ----

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct AudioFormat {
    pub samples_per_second: u32,
    pub channel_mask: u32,
    pub channel_count: u16,
    pub bits_per_sample: u16,
    pub sample_type: u32,
    pub interleaved: u8,
}

/// PARTY_AUDIO_MANIPULATION_SOURCE/SINK_STREAM_CONFIGURATION (same layout).
#[repr(C)]
pub(super) struct StreamConfig {
    pub format: usize,
    pub max_total_ms: u32,
}

#[repr(C)]
struct CreateChatCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    device: usize,
    user: usize,
    language: usize,
    async_id: usize,
    chat: usize,
}

#[repr(C)]
struct DestroyChatCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    device: usize,
    chat: usize,
    async_id: usize,
}

#[repr(C)]
struct ChatCreated {
    ty: u32,
    chat: usize,
}

#[repr(C)]
struct ChatDestroyed {
    ty: u32,
    chat: usize,
    reason: u32,
    detail: u32,
}

#[repr(C)]
struct SetBitrateCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    chat: usize,
    bitrate: u32,
    async_id: usize,
}

#[repr(C)]
struct TextReceived {
    ty: u32,
    sender: usize,
    receiver_count: u32,
    receivers: usize,
    language: usize,
    text: usize,
    data_size: u32,
    data: usize,
    translation_count: u32,
    translations: usize,
    options: u32,
    original_text: usize,
    detail: u32,
}

/// PARTY_SET_CHAT_AUDIO_INPUT/OUTPUT_COMPLETED_STATE_CHANGE.
#[repr(C)]
struct SetDeviceCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    chat: usize,
    selection: u32,
    context: usize,
    async_id: usize,
}

/// PARTY_LOCAL_CHAT_AUDIO_INPUT/OUTPUT_CHANGED_STATE_CHANGE.
#[repr(C)]
struct DeviceChanged {
    ty: u32,
    chat: usize,
    state: u32,
    detail: u32,
}

#[repr(C)]
struct SetLanguageCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    chat: usize,
    language: usize,
    async_id: usize,
}

/// PARTY_SET_TRANSCRIPTION_OPTIONS / SET_TEXT_CHAT_OPTIONS_COMPLETED_STATE_CHANGE.
#[repr(C)]
struct SetOptionsCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    chat: usize,
    options: u32,
    async_id: usize,
}

#[repr(C)]
struct ChatJoinedNetwork {
    ty: u32,
    network: usize,
    chat: usize,
}

#[repr(C)]
struct ChatLeftNetwork {
    ty: u32,
    reason: u32,
    detail: u32,
    network: usize,
    chat: usize,
}

/// PARTY_CONNECT / DISCONNECT_CHAT_CONTROL_COMPLETED_STATE_CHANGE.
#[repr(C)]
struct ConnectChatCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    network: usize,
    chat: usize,
    async_id: usize,
}

/// PARTY_POPULATE_AVAILABLE_TEXT_TO_SPEECH_PROFILES_COMPLETED_STATE_CHANGE.
#[repr(C)]
struct ChatAsyncCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    chat: usize,
    async_id: usize,
}

/// PARTY_CONFIGURE_AUDIO_MANIPULATION_*_STREAM_COMPLETED_STATE_CHANGE.
#[repr(C)]
struct ConfigureStreamCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    chat: usize,
    config: usize,
    async_id: usize,
}

// ---- PeerParty's chat objects ----

struct Selection {
    ty: u32,
    context: CString,
    device_id: CString,
    state: u32,
}

impl Selection {
    fn none() -> Self {
        Selection {
            ty: SELECTION_NONE,
            context: CString::default(),
            device_id: CString::default(),
            state: DEVICE_NONE,
        }
    }
}

pub(super) struct Chat {
    pub device: usize,
    /// None for our own (local) chat controls.
    pub peer: Option<PeerId>,
    user: usize,
    entity: CString,
    entity_type: CString,
    language: CString,
    /// Networks it's connected to (local) or seen in (remote).
    pub networks: Vec<usize>,
    // Local chat controls only:
    permissions: HashMap<usize, u32>,
    input: Selection,
    output: Selection,
    input_muted: bool,
    audio_muted: HashSet<usize>,
    text_muted: HashSet<usize>,
    volumes: HashMap<usize, f32>,
    bitrate: u32,
    transcription: u32,
    text_options: u32,
    voice_options: u32,
    capture: Option<usize>,
    render: Option<usize>,
    /// Strings handed to the game that a later Set* replaced; kept until cleanup.
    retired: Vec<CString>,
    // Remote chat controls only:
    voice: Option<usize>,
    talking: bool,
}

impl Chat {
    fn new(device: usize, peer: Option<PeerId>, user: usize, entity: &str, language: &str) -> Self {
        Chat {
            device,
            peer,
            user,
            entity: cstring(entity),
            entity_type: cstring(ENTITY_TYPE),
            language: cstring(language),
            networks: Vec::new(),
            permissions: HashMap::new(),
            input: Selection::none(),
            output: Selection::none(),
            input_muted: false,
            audio_muted: HashSet::new(),
            text_muted: HashSet::new(),
            volumes: HashMap::new(),
            bitrate: 0,
            transcription: 0,
            text_options: 0,
            voice_options: 0,
            capture: None,
            render: None,
            retired: Vec::new(),
            voice: None,
            talking: false,
        }
    }

    /// Whether this remote chat control is the one `entity` names.
    pub fn is(&self, entity: &str) -> bool {
        self.entity.as_bytes() == entity.as_bytes()
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum StreamKind {
    Voice,
    Capture,
    Render,
}

pub(super) struct Stream {
    chat: usize,
    kind: StreamKind,
    format: Box<AudioFormat>,
    config: Box<StreamConfig>,
    /// Voice streams: full 40 ms buffers waiting, the part of the next one, and the
    /// buffers the game holds until it returns them.
    queue: VecDeque<Vec<i16>>,
    partial: Vec<i16>,
    lent: HashMap<usize, Vec<i16>>,
    max_buffers: usize,
    dropped: u64,
}

impl Stream {
    fn new(chat: usize, kind: StreamKind, format: AudioFormat, max_total_ms: u32) -> Self {
        let format = Box::new(format);
        let config = Box::new(StreamConfig {
            format: &*format as *const AudioFormat as usize,
            max_total_ms,
        });
        Stream {
            chat,
            kind,
            format,
            config,
            queue: VecDeque::new(),
            partial: Vec::new(),
            lent: HashMap::new(),
            max_buffers: (max_total_ms / 40).max(1) as usize,
            dropped: 0,
        }
    }

    /// Voice for the game, in Party's 40 ms buffers; a full queue drops the newest.
    fn feed(&mut self, samples: &[i16]) {
        self.partial.extend_from_slice(samples);
        while self.partial.len() >= BUFFER_SAMPLES {
            let buf: Vec<i16> = self.partial.drain(..BUFFER_SAMPLES).collect();
            if self.queue.len() + self.lent.len() < self.max_buffers {
                self.queue.push_back(buf);
            } else {
                self.dropped += 1;
            }
        }
    }
}

/// Party's fixed formats: remote voice is 24 kHz mono 16-bit, local (capture) voice
/// 24 kHz mono 32-bit float, both non-interleaved.
fn voice_format(float: bool) -> AudioFormat {
    AudioFormat {
        samples_per_second: VOICE_RATE,
        channel_mask: 0,
        channel_count: 1,
        bits_per_sample: if float { 32 } else { 16 },
        sample_type: if float { SAMPLE_FLOAT } else { SAMPLE_INTEGER },
        interleaved: 0,
    }
}

fn format_supported(f: &AudioFormat) -> bool {
    (8_000..=48_000).contains(&f.samples_per_second)
        && (1..=64).contains(&f.channel_count)
        && matches!(
            (f.sample_type, f.bits_per_sample),
            (SAMPLE_FLOAT, 32) | (SAMPLE_INTEGER, 16) | (SAMPLE_INTEGER, 32)
        )
}

/// A buffer the game submitted, as interleaved 16-bit PCM.
fn to_frame(f: &AudioFormat, bytes: &[u8]) -> Frame {
    let samples: Vec<i16> = match (f.sample_type, f.bits_per_sample) {
        (SAMPLE_FLOAT, 32) => bytes
            .chunks_exact(4)
            .map(|c| (f32::from_le_bytes([c[0], c[1], c[2], c[3]]) * 32767.0).clamp(-32768.0, 32767.0) as i16)
            .collect(),
        (SAMPLE_INTEGER, 16) => bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect(),
        (SAMPLE_INTEGER, 32) => bytes
            .chunks_exact(4)
            .map(|c| (i32::from_le_bytes([c[0], c[1], c[2], c[3]]) >> 16) as i16)
            .collect(),
        _ => Vec::new(),
    };
    let ch = f.channel_count.max(1) as usize;
    let samples = if ch > 1 && f.interleaved == 0 {
        // Planar: all of channel 0, then channel 1...
        let n = samples.len() / ch;
        (0..n * ch).map(|i| samples[(i % ch) * n + i / ch]).collect()
    } else {
        samples
    };
    Frame {
        rate: f.samples_per_second,
        channels: ch as u32,
        samples,
    }
}

fn room_of(guid: &str) -> String {
    format!("{ROOM_PREFIX}{guid}")
}

/// Chat text on the wire: kind, text length, text, then the attached data.
fn encode_text(text: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + text.len() + data.len());
    out.push(TEXT_MESSAGE);
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(data);
    out
}

fn decode_text(b: &[u8]) -> Option<(String, Vec<u8>)> {
    let (&kind, rest) = b.split_first()?;
    if kind != TEXT_MESSAGE || rest.len() < 4 {
        return None;
    }
    let n = u32::from_le_bytes(rest[..4].try_into().ok()?) as usize;
    let body = &rest[4..];
    if body.len() < n {
        return None;
    }
    Some((String::from_utf8_lossy(&body[..n]).into_owned(), body[n..].to_vec()))
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetUserDefaultLocaleName(name: *mut u16, len: i32) -> i32;
}

/// The user's language as Windows has it (BCP 47, say en-AU), Party's default.
fn default_language() -> String {
    let mut buf = [0u16; 85];
    let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    if n > 1 {
        String::from_utf16_lossy(&buf[..(n - 1) as usize])
    } else {
        "en-US".into()
    }
}

// ---- chat state, kept in party.rs's State ----

impl State {
    pub(super) fn forgotten(&self, h: usize) -> bool {
        self.forget.contains(&h) || self.in_flight.iter().any(|b| b.forget.contains(&h))
    }

    /// The remote chat control `peer` announced as `entity`, made if new.
    pub(super) fn remote_chat_for(&mut self, peer: PeerId, entity: &str, language: &str, device: usize) -> (usize, bool) {
        let found = self
            .chats
            .iter()
            .find(|(h, c)| c.peer == Some(peer) && c.is(entity) && !self.forgotten(**h))
            .map(|(h, _)| *h);
        if let Some(h) = found {
            return (h, false);
        }
        let h = handle();
        self.chats.insert(h, Chat::new(device, Some(peer), 0, entity, language));
        self.chat_order.push(h);
        (h, true)
    }

    pub(super) fn remote_chat_created(&mut self, chat: usize) {
        self.push(ChatCreated {
            ty: CHAT_CONTROL_CREATED,
            chat,
        });
    }

    /// A chat control is now in `net` (local after connecting, remote when seen).
    pub(super) fn chat_joined(&mut self, net: usize, chat: usize) {
        if let Some(c) = self.chats.get_mut(&chat) {
            c.networks.push(net);
        }
        self.push(ChatJoinedNetwork {
            ty: CHAT_CONTROL_JOINED_NETWORK,
            network: net,
            chat,
        });
    }

    /// A remote chat control left `net`; once it's in no network it's destroyed.
    pub(super) fn remote_chat_left(&mut self, net: usize, chat: usize, reason: u32) {
        if let Some(n) = self.networks.get_mut(&net) {
            n.chats.retain(|c| *c != chat);
        }
        self.push(ChatLeftNetwork {
            ty: CHAT_CONTROL_LEFT_NETWORK,
            reason,
            detail: 0,
            network: net,
            chat,
        });
        let Some(c) = self.chats.get_mut(&chat) else { return };
        c.networks.retain(|n| *n != net);
        if !c.networks.is_empty() {
            return;
        }
        let voice = c.voice.take();
        self.push(ChatDestroyed {
            ty: CHAT_CONTROL_DESTROYED,
            chat,
            reason,
            detail: 0,
        });
        self.forget.push(chat);
        self.forget.extend(voice);
        // Our settings for it go with it.
        for l in self.chats.values_mut() {
            l.permissions.remove(&chat);
            l.audio_muted.remove(&chat);
            l.text_muted.remove(&chat);
            l.volumes.remove(&chat);
        }
    }

    /// Announces the network's first connected local chat control (or none) to the
    /// other copies.
    fn advertise(&self, lan: &Lan, net: usize) {
        let Some(n) = self.networks.get(&net) else { return };
        let r = match n.local_chats.first().and_then(|c| self.chats.get(c)) {
            Some(c) => {
                let attrs = [
                    Attr {
                        key: CHAT_KEY.into(),
                        value: Value::Str(c.entity.to_string_lossy().into_owned()),
                        public: true,
                    },
                    Attr {
                        key: CHAT_LANGUAGE_KEY.into(),
                        value: Value::Str(c.language.to_string_lossy().into_owned()),
                        public: true,
                    },
                ];
                lan.update(&n.guid, None, &attrs, &[])
            }
            None => lan.update(&n.guid, None, &[], &[CHAT_KEY.to_string(), CHAT_LANGUAGE_KEY.to_string()]),
        };
        if let Err(e) = r {
            log!("party-lan chat announcement on {}: {e:?}", n.guid);
        }
    }

    fn connect_chat(&mut self, lan: &Lan, net: usize, chat: usize, async_id: usize) {
        let Some(n) = self.networks.get_mut(&net) else { return };
        let room = room_of(&n.guid);
        n.local_chats.push(chat);
        let guid = n.guid.clone();
        if !voice::joined(&room) {
            voice::join(lan, &room, false, false);
        }
        self.advertise(lan, net);
        self.push(ConnectChatCompleted {
            ty: CONNECT_CHAT_CONTROL_COMPLETED,
            result: SUCCEEDED,
            detail: 0,
            network: net,
            chat,
            async_id,
        });
        self.chat_joined(net, chat);
        log!("party-lan chat control connected to network {guid}");
        self.reconcile(lan, net);
        self.apply_audio(net);
    }

    /// A local chat control leaves `net` (disconnected, destroyed, or the network ended).
    fn disconnect_chat(&mut self, lan: &Lan, net: usize, chat: usize, reason: u32) {
        let Some(n) = self.networks.get_mut(&net) else { return };
        n.local_chats.retain(|c| *c != chat);
        let room = room_of(&n.guid);
        let guid = n.guid.clone();
        let none_left = n.local_chats.is_empty();
        if let Some(c) = self.chats.get_mut(&chat) {
            c.networks.retain(|x| *x != net);
        }
        self.push(ChatLeftNetwork {
            ty: CHAT_CONTROL_LEFT_NETWORK,
            reason,
            detail: 0,
            network: net,
            chat,
        });
        self.advertise(lan, net);
        if none_left {
            voice::leave(lan, &room);
        } else {
            self.apply_audio(net);
        }
        log!("party-lan chat control left network {guid}");
    }

    /// The network connected: chat controls the game connected early follow it in.
    pub(super) fn chats_network_connected(&mut self, lan: &Lan, net: usize) {
        let pending = match self.networks.get_mut(&net) {
            Some(n) => std::mem::take(&mut n.pending_chats),
            None => return,
        };
        for (chat, async_id) in pending {
            if self.chats.contains_key(&chat) && !self.forgotten(chat) {
                self.connect_chat(lan, net, chat, async_id);
            }
        }
    }

    /// The network failed or ended: chat connections waiting for it fail, connected
    /// local chat controls leave it.
    pub(super) fn chats_network_ended(&mut self, lan: &Lan, net: usize, reason: u32) {
        let Some(n) = self.networks.get_mut(&net) else { return };
        let pending = std::mem::take(&mut n.pending_chats);
        let local = n.local_chats.clone();
        let room = room_of(&n.guid);
        for (chat, async_id) in pending {
            self.push(ConnectChatCompleted {
                ty: CONNECT_CHAT_CONTROL_COMPLETED,
                result: NETWORK_NO_LONGER_EXISTS,
                detail: 0,
                network: net,
                chat,
                async_id,
            });
        }
        for chat in local {
            self.disconnect_chat(lan, net, chat, reason);
        }
        voice::leave(lan, &room);
    }

    /// Leaves every network's voice room (PartyCleanup).
    pub(super) fn chats_cleanup(&self, lan: &Lan) {
        for n in self.networks.values() {
            voice::leave(lan, &room_of(&n.guid));
        }
    }

    /// Tells the network's voice room who may hear us and whom we hear, from the
    /// connected local chat controls' permissions, mutes, volumes, devices and streams.
    pub(super) fn apply_audio(&self, net: usize) {
        let Some(n) = self.networks.get(&net) else { return };
        let room = room_of(&n.guid);
        if !voice::joined(&room) {
            return;
        }
        let mut send_to = HashSet::new();
        let mut hear = HashSet::new();
        let mut no_play = HashSet::new();
        let (mut device_in, mut device_out, mut sending) = (false, false, false);
        for l in &n.local_chats {
            let Some(lc) = self.chats.get(l) else { continue };
            device_in |= lc.input.ty != SELECTION_NONE && lc.capture.is_none();
            device_out |= lc.output.ty != SELECTION_NONE;
            sending |= !lc.input_muted;
            for r in &n.chats {
                let Some(rc) = self.chats.get(r) else { continue };
                let Some(peer) = rc.peer else { continue };
                let perm = lc.permissions.get(r).copied().unwrap_or(0);
                if perm & SEND_MICROPHONE_AUDIO != 0 {
                    send_to.insert(peer);
                }
                if perm & RECEIVE_MICROPHONE_AUDIO != 0 && !lc.audio_muted.contains(r) {
                    hear.insert(peer);
                    let v = lc.volumes.get(r).copied().unwrap_or(1.0);
                    voice::set_volume(&room, Some(peer), v * 50.0);
                }
                if rc.voice.is_some() {
                    no_play.insert(peer);
                }
            }
        }
        voice::set_filters(&room, Some(send_to), Some(hear));
        voice::set_no_play(&room, no_play);
        voice::set_sending(&room, sending);
        voice::set_devices(&room, device_in, device_out);
    }

    fn apply_all(&self) {
        for net in self.networks.keys() {
            self.apply_audio(*net);
        }
    }

    fn text_arrived(&mut self, net: usize, peer: PeerId, bytes: &[u8]) {
        let Some((text, data)) = decode_text(bytes) else { return };
        let Some(n) = self.networks.get(&net) else { return };
        let sender = n.chats.iter().copied().find(|c| self.chats.get(c).is_some_and(|x| x.peer == Some(peer)));
        let Some(sender) = sender else {
            log!("party-lan chat text from a player without a chat control here; dropped");
            return;
        };
        let receivers: Vec<usize> = n
            .local_chats
            .iter()
            .copied()
            .filter(|l| {
                self.chats.get(l).is_some_and(|lc| {
                    lc.permissions.get(&sender).copied().unwrap_or(0) & RECEIVE_TEXT != 0 && !lc.text_muted.contains(&sender)
                })
            })
            .collect();
        trace("chat text received", || {
            format!("{} chars, {} data bytes, {} receivers", text.chars().count(), data.len(), receivers.len())
        });
        if receivers.is_empty() {
            return;
        }
        let language = self.chats[&sender].language.as_ptr() as usize;
        let count = receivers.len() as u32;
        let size = data.len() as u32;
        self.push_full(
            TextReceived {
                ty: CHAT_TEXT_RECEIVED,
                sender,
                receiver_count: count,
                receivers: 0,
                language,
                text: 0,
                data_size: size,
                data: 0,
                translation_count: 0,
                translations: 0,
                options: 0,
                original_text: 0,
                detail: 0,
            },
            data,
            receivers,
            vec![cstring(&text)],
            |c, b, p, t| {
                c.receivers = p;
                c.data = if size == 0 { 0 } else { b };
                c.text = t[0];
            },
        );
    }

    fn voice_arrived(&mut self, peer: PeerId, f: &Frame) {
        let streams: Vec<usize> = self.chats.values().filter(|c| c.peer == Some(peer)).filter_map(|c| c.voice).collect();
        if streams.is_empty() {
            return;
        }
        let samples = voice::resample(f, VOICE_RATE);
        for s in streams {
            if let Some(st) = self.streams.get_mut(&s) {
                st.feed(&samples);
            }
        }
    }
}

/// Room traffic for the networks our chat controls are in: text becomes state
/// changes, voice feeds voice streams and talking indicators.
pub(super) fn pump_chat(s: &mut State, lan: &Lan) {
    voice::pump(lan);
    let nets: Vec<(usize, String)> = s.networks.iter().map(|(h, n)| (*h, room_of(&n.guid))).collect();
    for (net, room) in nets {
        if !voice::joined(&room) {
            continue;
        }
        for ev in voice::take(&room) {
            match ev {
                RoomEvent::Data(peer, bytes) => s.text_arrived(net, peer, &bytes),
                RoomEvent::Audio(peer, f) => s.voice_arrived(peer, &f),
                RoomEvent::Speaking(peer, on) => {
                    for c in s.chats.values_mut().filter(|c| c.peer == Some(peer)) {
                        c.talking = on;
                    }
                }
                RoomEvent::Joined(_) | RoomEvent::Left(_) => {}
            }
        }
    }
}

// ---- the exports ----

pub(super) fn resolve(name: &str) -> Option<usize> {
    let f: usize = match name {
        "PartyGetChatControls" => get_chat_controls as *const () as usize,
        "PartyDeviceGetChatControls" => device_get_chat_controls as *const () as usize,
        "PartyNetworkGetChatControls" => network_get_chat_controls as *const () as usize,
        "PartyDeviceCreateChatControl" => create_chat_control as *const () as usize,
        "PartyDeviceDestroyChatControl" => destroy_chat_control as *const () as usize,
        "PartyNetworkConnectChatControl" => connect_chat_control as *const () as usize,
        "PartyNetworkDisconnectChatControl" => disconnect_chat_control as *const () as usize,
        "PartyChatControlGetLocalUser" => get_local_user as *const () as usize,
        "PartyChatControlSetPermissions" => set_permissions as *const () as usize,
        "PartyChatControlGetPermissions" => get_permissions as *const () as usize,
        "PartyChatControlSendText" => send_text as *const () as usize,
        "PartyChatControlSetAudioInput" => set_audio_input as *const () as usize,
        "PartyChatControlSetAudioOutput" => set_audio_output as *const () as usize,
        "PartyChatControlGetAudioInput" => get_audio_input as *const () as usize,
        "PartyChatControlGetAudioOutput" => get_audio_output as *const () as usize,
        "PartyChatControlPopulateAvailableTextToSpeechProfiles" => populate_tts_profiles as *const () as usize,
        "PartyChatControlGetAvailableTextToSpeechProfiles" => get_tts_profiles as *const () as usize,
        "PartyChatControlGetTextToSpeechProfile" => get_tts_profile as *const () as usize,
        "PartyChatControlSetLanguage" => set_language as *const () as usize,
        "PartyChatControlGetLanguage" => get_language as *const () as usize,
        "PartyChatControlSetTranscriptionOptions" => set_transcription_options as *const () as usize,
        "PartyChatControlGetTranscriptionOptions" => get_transcription_options as *const () as usize,
        "PartyChatControlSetTextChatOptions" => set_text_chat_options as *const () as usize,
        "PartyChatControlGetTextChatOptions" => get_text_chat_options as *const () as usize,
        "PartyChatControlSetAudioRenderVolume" => set_render_volume as *const () as usize,
        "PartyChatControlGetAudioRenderVolume" => get_render_volume as *const () as usize,
        "PartyChatControlSetAudioInputMuted" => set_input_muted as *const () as usize,
        "PartyChatControlGetAudioInputMuted" => get_input_muted as *const () as usize,
        "PartyChatControlSetIncomingAudioMuted" => set_incoming_audio_muted as *const () as usize,
        "PartyChatControlGetIncomingAudioMuted" => get_incoming_audio_muted as *const () as usize,
        "PartyChatControlSetIncomingTextMuted" => set_incoming_text_muted as *const () as usize,
        "PartyChatControlGetIncomingTextMuted" => get_incoming_text_muted as *const () as usize,
        "PartyChatControlSetVoiceAudioOptions" => set_voice_audio_options as *const () as usize,
        "PartyChatControlGetVoiceAudioOptions" => get_voice_audio_options as *const () as usize,
        "PartyChatControlGetLocalChatIndicator" => get_local_chat_indicator as *const () as usize,
        "PartyChatControlGetChatIndicator" => get_chat_indicator as *const () as usize,
        "PartyChatControlSetAudioEncoderBitrate" => set_bitrate as *const () as usize,
        "PartyChatControlGetAudioEncoderBitrate" => get_bitrate as *const () as usize,
        "PartyChatControlIsLocal" => is_local as *const () as usize,
        "PartyChatControlGetDevice" => get_device as *const () as usize,
        "PartyChatControlGetEntityId" => get_entity_id as *const () as usize,
        "PartyChatControlGetEntityType" => get_entity_type as *const () as usize,
        "PartyChatControlGetNetworks" => get_networks as *const () as usize,
        "PartyChatControlConfigureAudioManipulationVoiceStream" => configure_voice_stream as *const () as usize,
        "PartyChatControlConfigureAudioManipulationCaptureStream" => configure_capture_stream as *const () as usize,
        "PartyChatControlConfigureAudioManipulationRenderStream" => configure_render_stream as *const () as usize,
        "PartyChatControlGetAudioManipulationVoiceStream" => get_voice_stream as *const () as usize,
        "PartyChatControlGetAudioManipulationCaptureStream" => get_capture_stream as *const () as usize,
        "PartyChatControlGetAudioManipulationRenderStream" => get_render_stream as *const () as usize,
        "PartyAudioManipulationSourceStreamGetConfiguration" | "PartyAudioManipulationSinkStreamGetConfiguration" => {
            stream_get_configuration as *const () as usize
        }
        "PartyAudioManipulationSourceStreamGetFormat" | "PartyAudioManipulationSinkStreamGetFormat" => {
            stream_get_format as *const () as usize
        }
        "PartyAudioManipulationSourceStreamGetAvailableBufferCount" => source_buffer_count as *const () as usize,
        "PartyAudioManipulationSourceStreamGetNextBuffer" => source_next_buffer as *const () as usize,
        "PartyAudioManipulationSourceStreamReturnBuffer" => source_return_buffer as *const () as usize,
        "PartyAudioManipulationSinkStreamSubmitBuffer" => sink_submit_buffer as *const () as usize,
        _ => return None,
    };
    Some(f)
}

fn local_mut(s: &mut State, chat: usize) -> Result<&mut Chat, u32> {
    match s.chats.get_mut(&chat) {
        Some(c) if c.peer.is_none() => Ok(c),
        Some(_) => Err(E_BAD_ARG),
        None => Err(E_BAD_HANDLE),
    }
}

/// `target` must be a remote chat control: bad handle if unknown, bad argument if local.
fn check_remote(s: &State, target: usize) -> Result<(), u32> {
    match s.chats.get(&target) {
        Some(c) if c.peer.is_some() => Ok(()),
        Some(_) => Err(E_BAD_ARG),
        None => Err(E_BAD_HANDLE),
    }
}

macro_rules! try_party {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(code) => return code,
        }
    };
}

unsafe fn give_list(s: &mut State, kind: &'static str, items: Vec<usize>, count: *mut u32, out: *mut usize) -> u32 {
    let (n, p) = s.list(kind, items);
    unsafe {
        put(count, n);
        put(out, p);
    }
    OK
}

unsafe extern "system" fn get_chat_controls(_: usize, count: *mut u32, out: *mut usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    let all: Vec<usize> = s.chat_order.iter().copied().filter(|h| s.chats.contains_key(h) && !s.forgotten(*h)).collect();
    unsafe { give_list(&mut s, "chat controls", all, count, out) }
}

unsafe extern "system" fn device_get_chat_controls(device: usize, count: *mut u32, out: *mut usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    if !s.devices.contains_key(&device) {
        return E_BAD_HANDLE;
    }
    let local = device == s.local_device;
    let mine: Vec<usize> = s
        .chat_order
        .iter()
        .copied()
        .filter(|h| {
            s.chats.get(h).is_some_and(|c| if local { c.peer.is_none() } else { c.device == device }) && !s.forgotten(*h)
        })
        .collect();
    unsafe { give_list(&mut s, "device chat controls", mine, count, out) }
}

unsafe extern "system" fn network_get_chat_controls(net: usize, count: *mut u32, out: *mut usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    let Some(n) = s.networks.get(&net) else {
        return E_BAD_HANDLE;
    };
    let mut all = n.local_chats.clone();
    all.extend(n.chats.iter().copied());
    unsafe { give_list(&mut s, "network chat controls", all, count, out) }
}

unsafe extern "system" fn create_chat_control(
    device: usize,
    user: usize,
    language: *const c_char,
    async_id: usize,
    out: *mut usize,
) -> u32 {
    let language = unsafe { text(language) };
    let mut s = STATE.lock().unwrap();
    if device != s.local_device || device == 0 {
        return E_BAD_HANDLE;
    }
    let Some(u) = s.users.get(&user) else {
        return E_BAD_HANDLE;
    };
    if language.len() > MAX_LANGUAGE {
        return E_BAD_ARG;
    }
    if s.chats.iter().any(|(h, c)| c.peer.is_none() && c.user == user && !s.forgotten(*h)) {
        return E_BAD_ARG;
    }
    let entity = u.entity.to_string_lossy().into_owned();
    let language = if language.is_empty() { default_language() } else { language };
    let h = handle();
    s.chats.insert(h, Chat::new(device, None, user, &entity, &language));
    s.chat_order.push(h);
    s.chat_used = true;
    unsafe { put(out, h) };
    s.push_with(
        CreateChatCompleted {
            ty: CREATE_CHAT_CONTROL_COMPLETED,
            result: SUCCEEDED,
            detail: 0,
            device,
            user,
            language: 0,
            async_id,
            chat: h,
        },
        Vec::new(),
        Vec::new(),
        Some(cstring(&language)),
        |c, _, _, t| c.language = t,
    );
    s.push(ChatCreated {
        ty: CHAT_CONTROL_CREATED,
        chat: h,
    });
    log!("party-lan chat control for {entity} ({language})");
    OK
}

extern "system" fn destroy_chat_control(device: usize, chat: usize, async_id: usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    if device != s.local_device || s.forgotten(chat) {
        return E_BAD_HANDLE;
    }
    let networks = try_party!(local_mut(&mut s, chat)).networks.clone();
    if let Some(lan) = net::lan() {
        for net in networks {
            s.disconnect_chat(lan, net, chat, REASON_REQUESTED);
        }
    }
    for n in s.networks.values_mut() {
        n.pending_chats.retain(|(c, _)| *c != chat);
    }
    s.push(DestroyChatCompleted {
        ty: DESTROY_CHAT_CONTROL_COMPLETED,
        result: SUCCEEDED,
        detail: 0,
        device,
        chat,
        async_id,
    });
    s.push(ChatDestroyed {
        ty: CHAT_CONTROL_DESTROYED,
        chat,
        reason: REASON_REQUESTED,
        detail: 0,
    });
    s.forget.push(chat);
    let streams: Vec<usize> = s.streams.iter().filter(|(_, st)| st.chat == chat).map(|(h, _)| *h).collect();
    s.forget.extend(streams);
    log!("party-lan chat control destroyed");
    OK
}

extern "system" fn connect_chat_control(net: usize, chat: usize, async_id: usize) -> u32 {
    let Some(lan) = net::lan() else {
        return E_UNSUPPORTED;
    };
    let mut s = STATE.lock().unwrap();
    try_party!(local_mut(&mut s, chat));
    if s.forgotten(chat) || s.forgotten(net) {
        return E_BAD_HANDLE;
    }
    let Some(n) = s.networks.get_mut(&net) else {
        return E_BAD_HANDLE;
    };
    if n.local_chats.contains(&chat) || n.pending_chats.iter().any(|(c, _)| *c == chat) {
        return E_BAD_ARG;
    }
    if n.connected {
        s.connect_chat(lan, net, chat, async_id);
    } else {
        n.pending_chats.push((chat, async_id));
    }
    OK
}

extern "system" fn disconnect_chat_control(net: usize, chat: usize, async_id: usize) -> u32 {
    let Some(lan) = net::lan() else {
        return E_UNSUPPORTED;
    };
    let mut s = STATE.lock().unwrap();
    try_party!(local_mut(&mut s, chat));
    let Some(n) = s.networks.get_mut(&net) else {
        return E_BAD_HANDLE;
    };
    if n.pending_chats.iter().any(|(c, _)| *c == chat) {
        n.pending_chats.retain(|(c, _)| *c != chat);
    } else if n.local_chats.contains(&chat) {
        s.disconnect_chat(lan, net, chat, REASON_REQUESTED);
    } else {
        return E_BAD_ARG;
    }
    s.push(ConnectChatCompleted {
        ty: DISCONNECT_CHAT_CONTROL_COMPLETED,
        result: SUCCEEDED,
        detail: 0,
        network: net,
        chat,
        async_id,
    });
    OK
}

unsafe extern "system" fn get_local_user(chat: usize, out: *mut usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    let c = try_party!(local_mut(&mut s, chat));
    unsafe { put(out, c.user) };
    OK
}

extern "system" fn set_permissions(chat: usize, target: usize, permissions: u32) -> u32 {
    let mut s = STATE.lock().unwrap();
    try_party!(check_remote(&s, target));
    if permissions & !ALL_PERMISSIONS != 0 {
        return E_BAD_ARG;
    }
    try_party!(local_mut(&mut s, chat)).permissions.insert(target, permissions);
    trace("PartyChatControlSetPermissions", || format!("{permissions:#x}"));
    s.apply_all();
    OK
}

unsafe extern "system" fn get_permissions(chat: usize, target: usize, out: *mut u32) -> u32 {
    let mut s = STATE.lock().unwrap();
    try_party!(check_remote(&s, target));
    let c = try_party!(local_mut(&mut s, chat));
    unsafe { put(out, c.permissions.get(&target).copied().unwrap_or(0)) };
    OK
}

unsafe extern "system" fn send_text(
    chat: usize,
    target_count: u32,
    targets: *const usize,
    chat_text: *const c_char,
    buffer_count: u32,
    buffers: *const DataBuffer,
) -> u32 {
    let Some(lan) = net::lan() else {
        return E_UNSUPPORTED;
    };
    if chat_text.is_null() || (target_count > 0 && targets.is_null()) || (buffer_count > 0 && buffers.is_null()) {
        return E_BAD_ARG;
    }
    // Chat text is UTF-8 of any length up to Party's limit (text() is for ASCII ids).
    let message = unsafe { std::ffi::CStr::from_ptr(chat_text) }.to_string_lossy().into_owned();
    if message.chars().count() > MAX_TEXT {
        return E_BAD_ARG;
    }
    let mut data = Vec::new();
    for i in 0..buffer_count as usize {
        let b = unsafe { buffers.add(i).read_unaligned() };
        if b.buffer != 0 && b.len > 0 {
            data.extend_from_slice(unsafe { std::slice::from_raw_parts(b.buffer as *const u8, b.len as usize) });
        }
    }
    let targets: Vec<usize> = (0..target_count as usize).map(|i| unsafe { targets.add(i).read_unaligned() }).collect();
    let mut s = STATE.lock().unwrap();
    let my_networks = try_party!(local_mut(&mut s, chat)).networks.clone();
    // Every target has to be a remote chat control (Party fails the call otherwise).
    for t in &targets {
        try_party!(check_remote(&s, *t));
    }
    let msg = encode_text(&message, &data);
    let mut sent = 0;
    for t in &targets {
        let c = &s.chats[t];
        let Some(peer) = c.peer else { continue };
        // Through the first network both are in; none in common means not delivered.
        let Some(net) = my_networks.iter().find(|n| c.networks.contains(n)) else { continue };
        let room = room_of(&s.networks[net].guid);
        if voice::send_data_to(lan, &room, &[peer], &msg) {
            sent += 1;
        }
    }
    trace("PartyChatControlSendText", || {
        format!("{} chars, {} data bytes, to {sent} of {}", message.chars().count(), data.len(), targets.len())
    });
    OK
}

/// Input and output selections share their rules.
unsafe fn set_device(chat: usize, ty: u32, context: *const c_char, async_id: usize, input: bool) -> u32 {
    if ty > SELECTION_MANUAL {
        return E_BAD_ARG;
    }
    let context = unsafe { text(context) };
    if ty == SELECTION_MANUAL && context.is_empty() {
        return E_BAD_ARG;
    }
    let mut s = STATE.lock().unwrap();
    let c = try_party!(local_mut(&mut s, chat));
    // The device the selection names, if Windows has it. Looking opens nothing; the
    // microphone only opens once audio flows.
    let choice = match ty {
        SELECTION_SYSTEM_DEFAULT => voice::Choice::System,
        SELECTION_MANUAL => voice::Choice::Device(context.clone()),
        _ => voice::Choice::Communications,
    };
    let found = if ty == SELECTION_NONE || !voice::devices_allowed() {
        None
    } else {
        voice::snapshot().side(!input).resolve(&choice).map(|e| e.id.clone())
    };
    let state = match (ty, &found) {
        (SELECTION_NONE, _) => DEVICE_NONE,
        (_, Some(_)) => DEVICE_INITIALIZED,
        (_, None) => DEVICE_NOT_FOUND,
    };
    if found.is_some() {
        voice::choose_device(!input, choice);
    }
    let new = Selection {
        ty,
        context: cstring(&context),
        device_id: cstring(found.as_deref().unwrap_or("")),
        state,
    };
    let old = std::mem::replace(if input { &mut c.input } else { &mut c.output }, new);
    c.retired.push(old.context);
    c.retired.push(old.device_id);
    log!(
        "party-lan chat audio {} selection {ty} -> state {state} (devices allowed={})",
        if input { "input" } else { "output" },
        voice::devices_allowed()
    );
    s.push(DeviceChanged {
        ty: if input { LOCAL_CHAT_AUDIO_INPUT_CHANGED } else { LOCAL_CHAT_AUDIO_OUTPUT_CHANGED },
        chat,
        state,
        detail: 0,
    });
    s.push_with(
        SetDeviceCompleted {
            ty: if input { SET_CHAT_AUDIO_INPUT_COMPLETED } else { SET_CHAT_AUDIO_OUTPUT_COMPLETED },
            result: SUCCEEDED,
            detail: 0,
            chat,
            selection: ty,
            context: 0,
            async_id,
        },
        Vec::new(),
        Vec::new(),
        Some(cstring(&context)),
        |c, _, _, t| c.context = t,
    );
    s.apply_all();
    OK
}

unsafe extern "system" fn set_audio_input(chat: usize, ty: u32, context: *const c_char, async_id: usize) -> u32 {
    unsafe { set_device(chat, ty, context, async_id, true) }
}

unsafe extern "system" fn set_audio_output(chat: usize, ty: u32, context: *const c_char, async_id: usize) -> u32 {
    unsafe { set_device(chat, ty, context, async_id, false) }
}

unsafe fn get_selection(chat: usize, ty: *mut u32, context: *mut usize, device_id: *mut usize, input: bool) -> u32 {
    let mut s = STATE.lock().unwrap();
    let c = try_party!(local_mut(&mut s, chat));
    let sel = if input { &c.input } else { &c.output };
    unsafe {
        put(ty, sel.ty);
        put(context, sel.context.as_ptr() as usize);
        put(device_id, sel.device_id.as_ptr() as usize);
    }
    OK
}

unsafe extern "system" fn get_audio_input(chat: usize, ty: *mut u32, context: *mut usize, device_id: *mut usize) -> u32 {
    unsafe { get_selection(chat, ty, context, device_id, true) }
}

unsafe extern "system" fn get_audio_output(chat: usize, ty: *mut u32, context: *mut usize, device_id: *mut usize) -> u32 {
    unsafe { get_selection(chat, ty, context, device_id, false) }
}

extern "system" fn populate_tts_profiles(chat: usize, async_id: usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    try_party!(local_mut(&mut s, chat));
    // No text-to-speech on the LAN: it completes with no profiles.
    s.push(ChatAsyncCompleted {
        ty: POPULATE_AVAILABLE_TEXT_TO_SPEECH_PROFILES_COMPLETED,
        result: SUCCEEDED,
        detail: 0,
        chat,
        async_id,
    });
    OK
}

unsafe extern "system" fn get_tts_profiles(chat: usize, count: *mut u32, out: *mut usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    try_party!(local_mut(&mut s, chat));
    unsafe {
        put(count, 0);
        put(out, 0);
    }
    OK
}

unsafe extern "system" fn get_tts_profile(chat: usize, _ty: u32, out: *mut usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    try_party!(local_mut(&mut s, chat));
    unsafe { put(out, 0) };
    OK
}

unsafe extern "system" fn set_language(chat: usize, language: *const c_char, async_id: usize) -> u32 {
    let language = unsafe { text(language) };
    if language.len() > MAX_LANGUAGE {
        return E_BAD_ARG;
    }
    let language = if language.is_empty() { default_language() } else { language };
    let mut s = STATE.lock().unwrap();
    let c = try_party!(local_mut(&mut s, chat));
    let old = std::mem::replace(&mut c.language, cstring(&language));
    c.retired.push(old);
    let networks = c.networks.clone();
    s.push_with(
        SetLanguageCompleted {
            ty: SET_LANGUAGE_COMPLETED,
            result: SUCCEEDED,
            detail: 0,
            chat,
            language: 0,
            async_id,
        },
        Vec::new(),
        Vec::new(),
        Some(cstring(&language)),
        |c, _, _, t| c.language = t,
    );
    if let Some(lan) = net::lan() {
        for net in networks {
            s.advertise(lan, net);
        }
    }
    OK
}

unsafe extern "system" fn get_language(chat: usize, out: *mut usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    let c = try_party!(local_mut(&mut s, chat));
    unsafe { put(out, c.language.as_ptr() as usize) };
    OK
}

extern "system" fn set_transcription_options(chat: usize, options: u32, async_id: usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    let c = try_party!(local_mut(&mut s, chat));
    if options & TRANSCRIBE != 0 {
        super::note("PartyChatControlSetTranscriptionOptions with transcription on");
        return E_UNSUPPORTED;
    }
    c.transcription = options;
    s.push(SetOptionsCompleted {
        ty: SET_TRANSCRIPTION_OPTIONS_COMPLETED,
        result: SUCCEEDED,
        detail: 0,
        chat,
        options,
        async_id,
    });
    OK
}

unsafe extern "system" fn get_transcription_options(chat: usize, out: *mut u32) -> u32 {
    let mut s = STATE.lock().unwrap();
    let c = try_party!(local_mut(&mut s, chat));
    unsafe { put(out, c.transcription) };
    OK
}

extern "system" fn set_text_chat_options(chat: usize, options: u32, async_id: usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    // Kept, but text arrives as sent (no translation or filtering on the LAN).
    try_party!(local_mut(&mut s, chat)).text_options = options;
    s.push(SetOptionsCompleted {
        ty: SET_TEXT_CHAT_OPTIONS_COMPLETED,
        result: SUCCEEDED,
        detail: 0,
        chat,
        options,
        async_id,
    });
    OK
}

unsafe extern "system" fn get_text_chat_options(chat: usize, out: *mut u32) -> u32 {
    let mut s = STATE.lock().unwrap();
    let c = try_party!(local_mut(&mut s, chat));
    unsafe { put(out, c.text_options) };
    OK
}

extern "system" fn set_render_volume(chat: usize, target: usize, volume: f32) -> u32 {
    if !(0.0..=1.0).contains(&volume) {
        return E_BAD_ARG;
    }
    let mut s = STATE.lock().unwrap();
    if !s.chats.contains_key(&target) {
        return E_BAD_HANDLE;
    }
    try_party!(local_mut(&mut s, chat)).volumes.insert(target, volume);
    s.apply_all();
    OK
}

unsafe extern "system" fn get_render_volume(chat: usize, target: usize, out: *mut f32) -> u32 {
    let mut s = STATE.lock().unwrap();
    if !s.chats.contains_key(&target) {
        return E_BAD_HANDLE;
    }
    let c = try_party!(local_mut(&mut s, chat));
    unsafe { put(out, c.volumes.get(&target).copied().unwrap_or(1.0)) };
    OK
}

extern "system" fn set_input_muted(chat: usize, muted: u8) -> u32 {
    let mut s = STATE.lock().unwrap();
    try_party!(local_mut(&mut s, chat)).input_muted = muted != 0;
    s.apply_all();
    OK
}

unsafe extern "system" fn get_input_muted(chat: usize, out: *mut u8) -> u32 {
    let mut s = STATE.lock().unwrap();
    let c = try_party!(local_mut(&mut s, chat));
    unsafe { put(out, u8::from(c.input_muted)) };
    OK
}

fn set_incoming_muted(chat: usize, target: usize, muted: u8, audio: bool) -> u32 {
    let mut s = STATE.lock().unwrap();
    try_party!(check_remote(&s, target));
    let c = try_party!(local_mut(&mut s, chat));
    let set = if audio { &mut c.audio_muted } else { &mut c.text_muted };
    if muted != 0 {
        set.insert(target);
    } else {
        set.remove(&target);
    }
    s.apply_all();
    OK
}

unsafe fn get_incoming_muted(chat: usize, target: usize, out: *mut u8, audio: bool) -> u32 {
    let mut s = STATE.lock().unwrap();
    try_party!(check_remote(&s, target));
    let c = try_party!(local_mut(&mut s, chat));
    let set = if audio { &c.audio_muted } else { &c.text_muted };
    unsafe { put(out, u8::from(set.contains(&target))) };
    OK
}

extern "system" fn set_incoming_audio_muted(chat: usize, target: usize, muted: u8) -> u32 {
    set_incoming_muted(chat, target, muted, true)
}

unsafe extern "system" fn get_incoming_audio_muted(chat: usize, target: usize, out: *mut u8) -> u32 {
    unsafe { get_incoming_muted(chat, target, out, true) }
}

extern "system" fn set_incoming_text_muted(chat: usize, target: usize, muted: u8) -> u32 {
    set_incoming_muted(chat, target, muted, false)
}

unsafe extern "system" fn get_incoming_text_muted(chat: usize, target: usize, out: *mut u8) -> u32 {
    unsafe { get_incoming_muted(chat, target, out, false) }
}

extern "system" fn set_voice_audio_options(chat: usize, options: u32) -> u32 {
    let mut s = STATE.lock().unwrap();
    // Noise suppression is kept as a setting; the LAN's level gate is all there is.
    try_party!(local_mut(&mut s, chat)).voice_options = options;
    OK
}

unsafe extern "system" fn get_voice_audio_options(chat: usize, out: *mut u32) -> u32 {
    let mut s = STATE.lock().unwrap();
    let c = try_party!(local_mut(&mut s, chat));
    unsafe { put(out, c.voice_options) };
    OK
}

unsafe extern "system" fn get_local_chat_indicator(chat: usize, out: *mut u32) -> u32 {
    let mut s = STATE.lock().unwrap();
    let networks = try_party!(local_mut(&mut s, chat)).networks.clone();
    let talking = networks.iter().filter_map(|n| s.networks.get(n)).any(|n| voice::talking(&room_of(&n.guid)));
    let c = &s.chats[&chat];
    let has_input = c.capture.is_some() || c.input.state == DEVICE_INITIALIZED;
    let v = if !has_input {
        LOCAL_NO_INPUT
    } else if c.input_muted {
        LOCAL_INPUT_MUTED
    } else if talking {
        LOCAL_TALKING
    } else {
        LOCAL_SILENT
    };
    unsafe { put(out, v) };
    OK
}

unsafe extern "system" fn get_chat_indicator(chat: usize, target: usize, out: *mut u32) -> u32 {
    let mut s = STATE.lock().unwrap();
    try_party!(check_remote(&s, target));
    let talking = s.chats[&target].talking;
    let c = try_party!(local_mut(&mut s, chat));
    let v = if c.audio_muted.contains(&target) {
        REMOTE_MUTED
    } else if c.permissions.get(&target).copied().unwrap_or(0) & RECEIVE_MICROPHONE_AUDIO == 0 {
        REMOTE_VOICE_DISABLED
    } else if talking {
        REMOTE_TALKING
    } else {
        REMOTE_SILENT
    };
    unsafe { put(out, v) };
    OK
}

extern "system" fn set_bitrate(chat: usize, bitrate: u32, async_id: usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    // Kept for the game; LAN voice is uncompressed PCM.
    try_party!(local_mut(&mut s, chat)).bitrate = bitrate;
    s.push(SetBitrateCompleted {
        ty: SET_CHAT_AUDIO_ENCODER_BITRATE_COMPLETED,
        result: SUCCEEDED,
        detail: 0,
        chat,
        bitrate,
        async_id,
    });
    OK
}

unsafe extern "system" fn get_bitrate(chat: usize, out: *mut u32) -> u32 {
    let mut s = STATE.lock().unwrap();
    let c = try_party!(local_mut(&mut s, chat));
    unsafe { put(out, c.bitrate) };
    OK
}

/// A getter that works for local and remote chat controls alike.
unsafe fn chat_get<T>(chat: usize, out: *mut T, f: impl FnOnce(&Chat) -> T) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(c) = s.chats.get(&chat) else {
        return E_BAD_HANDLE;
    };
    unsafe { put(out, f(c)) };
    OK
}

unsafe extern "system" fn is_local(chat: usize, out: *mut u8) -> u32 {
    unsafe { chat_get(chat, out, |c| u8::from(c.peer.is_none())) }
}

unsafe extern "system" fn get_device(chat: usize, out: *mut usize) -> u32 {
    unsafe { chat_get(chat, out, |c| c.device) }
}

unsafe extern "system" fn get_entity_id(chat: usize, out: *mut usize) -> u32 {
    unsafe { chat_get(chat, out, |c| c.entity.as_ptr() as usize) }
}

unsafe extern "system" fn get_entity_type(chat: usize, out: *mut usize) -> u32 {
    unsafe { chat_get(chat, out, |c| c.entity_type.as_ptr() as usize) }
}

unsafe extern "system" fn get_networks(chat: usize, count: *mut u32, out: *mut usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    let Some(c) = s.chats.get(&chat) else {
        return E_BAD_HANDLE;
    };
    let nets = c.networks.clone();
    unsafe { give_list(&mut s, "chat networks", nets, count, out) }
}

// ---- audio manipulation streams ----

fn slot(c: &mut Chat, kind: StreamKind) -> &mut Option<usize> {
    match kind {
        StreamKind::Voice => &mut c.voice,
        StreamKind::Capture => &mut c.capture,
        StreamKind::Render => &mut c.render,
    }
}

/// Makes, replaces or (null configuration) removes a chat control's stream.
unsafe fn configure_stream(chat: usize, config: *const StreamConfig, async_id: usize, kind: StreamKind) -> u32 {
    let requested = if config.is_null() {
        None
    } else {
        let c = unsafe { config.read_unaligned() };
        let format = (c.format != 0).then(|| unsafe { (c.format as *const AudioFormat).read_unaligned() });
        Some((format, c.max_total_ms))
    };
    let mut guard = STATE.lock().unwrap();
    let s = &mut *guard;
    let Some(c) = s.chats.get(&chat) else {
        return E_BAD_HANDLE;
    };
    let local = c.peer.is_none();
    match kind {
        // A local chat control's own voice stream (its processed microphone audio
        // handed to the game) isn't carried.
        StreamKind::Voice if local => {
            super::note("PartyChatControlConfigureAudioManipulationVoiceStream for a local chat control");
            return E_UNSUPPORTED;
        }
        StreamKind::Capture | StreamKind::Render if !local => return E_BAD_ARG,
        _ => {}
    }
    let format = match (kind, requested.as_ref().and_then(|r| r.0)) {
        (StreamKind::Voice, Some(f)) if f != voice_format(false) => return E_BAD_ARG,
        (StreamKind::Capture, Some(f)) if !format_supported(&f) || f.channel_count != 1 => return E_BAD_ARG,
        (StreamKind::Render, Some(f)) if !format_supported(&f) => return E_BAD_ARG,
        (_, Some(f)) => f,
        (StreamKind::Voice, None) => voice_format(false),
        (StreamKind::Capture, None) => voice_format(true),
        (StreamKind::Render, None) => AudioFormat {
            samples_per_second: 48_000,
            channel_mask: 0,
            channel_count: 2,
            bits_per_sample: 32,
            sample_type: SAMPLE_FLOAT,
            interleaved: 1,
        },
    };
    let new = requested.map(|(_, max_ms)| {
        let h = handle();
        s.streams.insert(h, Stream::new(chat, kind, format, max_ms.max(40)));
        h
    });
    let c = s.chats.get_mut(&chat).unwrap();
    let old = std::mem::replace(slot(c, kind), new);
    // The old stream stays valid until the game has seen this batch.
    s.forget.extend(old);
    let config_ptr = new.and_then(|h| s.streams.get(&h)).map_or(0, |st| &*st.config as *const StreamConfig as usize);
    let ty = match kind {
        StreamKind::Voice => CONFIGURE_VOICE_STREAM_COMPLETED,
        StreamKind::Capture => CONFIGURE_CAPTURE_STREAM_COMPLETED,
        StreamKind::Render => CONFIGURE_RENDER_STREAM_COMPLETED,
    };
    s.push(ConfigureStreamCompleted {
        ty,
        result: SUCCEEDED,
        detail: 0,
        chat,
        config: config_ptr,
        async_id,
    });
    log!(
        "party-lan chat {kind:?} stream {} ({} Hz, {} ch, {}-bit)",
        if new.is_some() { "on" } else { "off" },
        format.samples_per_second,
        format.channel_count,
        format.bits_per_sample
    );
    s.apply_all();
    OK
}

unsafe extern "system" fn configure_voice_stream(chat: usize, config: *const StreamConfig, async_id: usize) -> u32 {
    unsafe { configure_stream(chat, config, async_id, StreamKind::Voice) }
}

unsafe extern "system" fn configure_capture_stream(chat: usize, config: *const StreamConfig, async_id: usize) -> u32 {
    unsafe { configure_stream(chat, config, async_id, StreamKind::Capture) }
}

unsafe extern "system" fn configure_render_stream(chat: usize, config: *const StreamConfig, async_id: usize) -> u32 {
    unsafe { configure_stream(chat, config, async_id, StreamKind::Render) }
}

unsafe fn get_stream(chat: usize, out: *mut usize, kind: StreamKind) -> u32 {
    let mut s = STATE.lock().unwrap();
    let Some(c) = s.chats.get_mut(&chat) else {
        return E_BAD_HANDLE;
    };
    let h = *slot(c, kind);
    unsafe { put(out, h.unwrap_or(0)) };
    if h.is_some() { OK } else { E_BAD_ARG }
}

unsafe extern "system" fn get_voice_stream(chat: usize, out: *mut usize) -> u32 {
    unsafe { get_stream(chat, out, StreamKind::Voice) }
}

unsafe extern "system" fn get_capture_stream(chat: usize, out: *mut usize) -> u32 {
    unsafe { get_stream(chat, out, StreamKind::Capture) }
}

unsafe extern "system" fn get_render_stream(chat: usize, out: *mut usize) -> u32 {
    unsafe { get_stream(chat, out, StreamKind::Render) }
}

unsafe extern "system" fn stream_get_configuration(stream: usize, out: *mut StreamConfig) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(st) = s.streams.get(&stream) else {
        return E_BAD_HANDLE;
    };
    let config = StreamConfig {
        format: st.config.format,
        max_total_ms: st.config.max_total_ms,
    };
    unsafe { put(out, config) };
    OK
}

unsafe extern "system" fn stream_get_format(stream: usize, out: *mut AudioFormat) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(st) = s.streams.get(&stream) else {
        return E_BAD_HANDLE;
    };
    unsafe { put(out, *st.format) };
    OK
}

unsafe extern "system" fn source_buffer_count(stream: usize, out: *mut u32) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(st) = s.streams.get(&stream).filter(|st| st.kind == StreamKind::Voice) else {
        return E_BAD_HANDLE;
    };
    unsafe { put(out, st.queue.len() as u32) };
    OK
}

unsafe extern "system" fn source_next_buffer(stream: usize, out: *mut DataBuffer) -> u32 {
    let mut s = STATE.lock().unwrap();
    let Some(st) = s.streams.get_mut(&stream).filter(|st| st.kind == StreamKind::Voice) else {
        return E_BAD_HANDLE;
    };
    let Some(buf) = st.queue.pop_front() else {
        unsafe { put(out, DataBuffer { buffer: 0, len: 0 }) };
        return E_NO_BUFFER;
    };
    let ptr = buf.as_ptr() as usize;
    let len = (buf.len() * 2) as u32;
    st.lent.insert(ptr, buf);
    unsafe { put(out, DataBuffer { buffer: ptr, len }) };
    OK
}

extern "system" fn source_return_buffer(stream: usize, buffer: usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    let Some(st) = s.streams.get_mut(&stream).filter(|st| st.kind == StreamKind::Voice) else {
        return E_BAD_HANDLE;
    };
    if st.lent.remove(&buffer).is_some() { OK } else { E_BAD_ARG }
}

unsafe extern "system" fn sink_submit_buffer(stream: usize, buffer: *const DataBuffer) -> u32 {
    if buffer.is_null() {
        return E_BAD_ARG;
    }
    let b = unsafe { buffer.read_unaligned() };
    let bytes = if b.buffer == 0 || b.len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(b.buffer as *const u8, b.len as usize) }.to_vec()
    };
    let s = STATE.lock().unwrap();
    let Some(st) = s.streams.get(&stream) else {
        return E_BAD_HANDLE;
    };
    let (kind, chat) = (st.kind, st.chat);
    let frame = to_frame(&st.format, &bytes);
    let Some(c) = s.chats.get(&chat) else {
        return E_BAD_HANDLE;
    };
    if frame.samples.is_empty() {
        return OK;
    }
    match kind {
        StreamKind::Capture => {
            // Treated as microphone audio: sent to the rooms of the networks it's in,
            // under the same permissions and mute.
            if c.input_muted {
                return OK;
            }
            let Some(lan) = net::lan() else { return OK };
            let rooms: Vec<String> = c.networks.iter().filter_map(|n| s.networks.get(n)).map(|n| room_of(&n.guid)).collect();
            for room in rooms {
                voice::send_audio(lan, &room, &frame);
            }
            trace("capture stream", || format!("{} samples at {} Hz", frame.samples.len(), frame.rate));
        }
        StreamKind::Render => {
            if c.output.ty != SELECTION_NONE {
                voice::play_local(&frame, 1.0);
            }
        }
        StreamKind::Voice => return E_BAD_HANDLE,
    }
    OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_layouts_match_party_h() {
        let p = std::mem::size_of::<usize>();
        let o = |x: usize| x.next_multiple_of(p);
        assert_eq!(std::mem::size_of::<AudioFormat>(), 20);
        assert_eq!(std::mem::size_of::<StreamConfig>(), 2 * p);
        assert_eq!(std::mem::offset_of!(CreateChatCompleted, device), o(12));
        assert_eq!(std::mem::offset_of!(CreateChatCompleted, chat), o(12) + 4 * p);
        assert_eq!(std::mem::offset_of!(ChatDestroyed, reason), p * 2);
        assert_eq!(std::mem::offset_of!(SetBitrateCompleted, async_id), o(o(12) + p + 4));
        if p == 8 {
            // x64, as the PlayFab Party DLLs games ship are.
            assert_eq!(std::mem::offset_of!(TextReceived, receiver_count), 16);
            assert_eq!(std::mem::offset_of!(TextReceived, receivers), 24);
            assert_eq!(std::mem::offset_of!(TextReceived, text), 40);
            assert_eq!(std::mem::offset_of!(TextReceived, data_size), 48);
            assert_eq!(std::mem::offset_of!(TextReceived, data), 56);
            assert_eq!(std::mem::offset_of!(TextReceived, translations), 72);
            assert_eq!(std::mem::offset_of!(TextReceived, original_text), 88);
            assert_eq!(std::mem::offset_of!(TextReceived, detail), 96);
            assert_eq!(std::mem::size_of::<TextReceived>(), 104);
            assert_eq!(std::mem::offset_of!(SetDeviceCompleted, context), 32);
            assert_eq!(std::mem::offset_of!(DeviceChanged, state), 16);
            assert_eq!(std::mem::offset_of!(ChatLeftNetwork, network), 16);
            assert_eq!(std::mem::offset_of!(ChatLeftNetwork, chat), 24);
            assert_eq!(std::mem::offset_of!(ConnectChatCompleted, async_id), 32);
            assert_eq!(std::mem::offset_of!(ConfigureStreamCompleted, config), 24);
            assert_eq!(std::mem::offset_of!(SetOptionsCompleted, async_id), 32);
        }
    }

    #[test]
    fn chat_text_round_trips() {
        let m = encode_text("hello, LAN \u{2713}", &[1, 2, 3]);
        assert_eq!(decode_text(&m), Some(("hello, LAN \u{2713}".to_string(), vec![1, 2, 3])));
        assert_eq!(decode_text(&encode_text("", &[])), Some((String::new(), Vec::new())));
        assert_eq!(decode_text(&[TEXT_MESSAGE, 9, 0, 0, 0, b'x']), None);
        assert_eq!(decode_text(&[7]), None);
    }

    #[test]
    fn game_audio_becomes_pcm() {
        // Party's capture format: 24 kHz mono float.
        let f = voice_format(true);
        let bytes: Vec<u8> = [0.5f32, -1.0, 0.0].iter().flat_map(|x| x.to_le_bytes()).collect();
        let fr = to_frame(&f, &bytes);
        assert_eq!((fr.rate, fr.channels), (24_000, 1));
        assert_eq!(fr.samples, vec![16383, -32767, 0]);
        // Planar stereo 16-bit becomes interleaved.
        let planar = AudioFormat {
            samples_per_second: 48_000,
            channel_mask: 0,
            channel_count: 2,
            bits_per_sample: 16,
            sample_type: SAMPLE_INTEGER,
            interleaved: 0,
        };
        let bytes: Vec<u8> = [1i16, 2, 3, -1, -2, -3].iter().flat_map(|x| x.to_le_bytes()).collect();
        assert_eq!(to_frame(&planar, &bytes).samples, vec![1, -1, 2, -2, 3, -3]);
        assert!(format_supported(&planar));
        assert!(!format_supported(&AudioFormat { bits_per_sample: 8, ..planar }));
    }

    #[test]
    fn voice_streams_cut_40ms_buffers() {
        let mut st = Stream::new(1, StreamKind::Voice, voice_format(false), 120);
        assert_eq!(st.max_buffers, 3);
        st.feed(&[7; 900]);
        assert!(st.queue.is_empty());
        st.feed(&[7; 100]);
        assert_eq!(st.queue.len(), 1);
        assert_eq!(st.partial.len(), 40);
        st.feed(&[7; 960 * 4]);
        assert_eq!(st.queue.len(), 3);
        assert_eq!(st.dropped, 2);
        assert_eq!(st.config.format, &*st.format as *const AudioFormat as usize);
    }

    #[test]
    fn the_default_language_is_bcp47() {
        let l = default_language();
        assert!(!l.is_empty() && l.len() <= MAX_LANGUAGE && !l.contains('\0'), "{l}");
    }
}
