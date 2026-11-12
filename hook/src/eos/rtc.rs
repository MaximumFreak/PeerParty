//! EOS voice and room data (EOS_RTC, EOS_RTCAudio, EOS_RTCData, and the lobby's RTC
//! room) on PeerParty's LAN voice rooms (voice.rs).
//!
//! Rooms and participants are tracked, and `SendAudio` / `SendData` carry the audio and
//! data over the LAN. Struct layouts are from the SDK's eos_rtc_types.h,
//! eos_rtc_audio_types.h, eos_rtc_data_types.h and eos_lobby_types.h (1.19).
//!
//! - A room is joined by name (`EOS_RTC_JoinRoom`) or through a lobby: every LAN lobby
//!   has a room (`<lobby id>:peerparty-voice`) once the game uses lobby voice (it
//!   created a lobby with `bEnableRTCRoom`, or asked for the room's name or connection
//!   state). Joins follow the lobby unless the game joins the room by hand.
//! - Participants appear as their copies join the room (ParticipantStatusChanged,
//!   RTCData's ParticipantUpdated); a listener added once the room has filled hears
//!   who is already there. Speaking shows in RTCAudio's ParticipantUpdated.
//! - Audio: with manual audio input the game's `SendAudio` frames go to the room; else
//!   PeerParty's microphone does (voice.rs). Arriving audio goes to the game's
//!   `AudioBeforeRender` listeners and, unless manual audio output is on, to the
//!   speakers. `AudioBeforeSend` never fires (the microphone isn't the game's here).
//! - Devices: Windows' microphones and speakers, by endpoint id and name, the system
//!   default marked as EOS's default (EOS plays on it until the game picks another).
//!   The device settings move PeerParty's microphone and speakers (voice.rs; an empty
//!   id is the default device); AudioDevicesChanged fires when Windows' list changes.
//!
//! Tested from two Python copies on the EOS SDK (tests/sdk/eos_rtc_test.py); not
//! yet in a game.

use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

use peerparty_core::{Event, Lan};

use super::abi::result::*;
use super::ids::{self, intern, text};
use super::{Kind, add_listener, call, hand_out, lan, later, listeners, owned_str, release, remove_listener, trace};
use crate::voice::{self, Frame, RoomEvent};

const LOBBY_ROOM_SUFFIX: &str = ":peerparty-voice";
/// EOS_RTCDATA_MAX_PACKET_SIZE.
const MAX_DATA: u32 = 1170;
const LIMIT_EXCEEDED: i32 = 22;
const AUDIO_ENABLED: i32 = 1;
const DATA_ENABLED: i32 = 1;
const STATUS_JOINED: i32 = 0;
const STATUS_LEFT: i32 = 1;
/// Fake interface handles: every RTCAudio / RTCData function is PeerParty's, so the
/// SDK never sees them.
const AUDIO_HANDLE: usize = 0x5050_A0D0;
const DATA_HANDLE: usize = 0x5050_DA7A;

/// The game uses lobby voice rooms.
static LOBBY_VOICE: AtomicBool = AtomicBool::new(false);
/// Lobby rooms: manual audio input / output, as the lobby's LocalRTCOptions asked.
static LOBBY_MANUAL: Mutex<(bool, bool)> = Mutex::new((false, false));
/// Lobby ids whose room we're in, and lobbies whose room the game left by hand.
static LOBBY_ROOMS: LazyLock<Mutex<HashMap<String, bool>>> = LazyLock::new(Mutex::default);
static LOCAL: AtomicUsize = AtomicUsize::new(0);

fn key(room: &str) -> String {
    format!("eos:{room}")
}

pub(crate) fn lobby_room(lobby: &str) -> String {
    format!("{lobby}{LOBBY_ROOM_SUFFIX}")
}

fn local() -> usize {
    match LOCAL.load(Ordering::Relaxed) {
        0 => lan().map(|l| ids::puid(l.me())).unwrap_or(0),
        v => v,
    }
}

fn str_of(p: *const c_char) -> String {
    unsafe { text(p) }.unwrap_or_default()
}

#[allow(non_snake_case)]
#[repr(C)]
struct LocalRtcOptions {
    ApiVersion: i32,
    Flags: u32,
    bUseManualAudioInput: i32,
    bUseManualAudioOutput: i32,
}

/// The game created or joined a lobby with voice; `options` is its LocalRTCOptions.
pub(crate) fn lobby_voice(options: usize) {
    if !LOBBY_VOICE.swap(true, Ordering::Relaxed) {
        log_once("lobby voice rooms on");
    }
    if options != 0 {
        let o = unsafe { &*(options as *const LocalRtcOptions) };
        *LOBBY_MANUAL.lock().unwrap() = (o.bUseManualAudioInput != 0, o.bUseManualAudioOutput != 0);
    }
}

fn log_once(what: &str) {
    crate::log::throttled(&format!("eos-rtc {what}"), 1, |_| crate::log!("eos-rtc {what}"));
}

pub(crate) fn lobby_rooms_on() -> bool {
    LOBBY_VOICE.load(Ordering::Relaxed)
}

pub(super) fn resolve(name: &str) -> Option<usize> {
    Some(match name {
        "EOS_RTC_GetAudioInterface" => get_audio as *const () as usize,
        "EOS_RTC_GetDataInterface" => get_data as *const () as usize,
        "EOS_RTC_JoinRoom" => join_room as *const () as usize,
        "EOS_RTC_LeaveRoom" => leave_room as *const () as usize,
        "EOS_RTC_BlockParticipant" => block_participant as *const () as usize,
        "EOS_RTC_AddNotifyParticipantStatusChanged" => notify_participant_status as *const () as usize,
        "EOS_RTC_AddNotifyDisconnected"
        | "EOS_RTC_AddNotifyRoomBeforeJoin"
        | "EOS_RTC_AddNotifyRoomStatisticsUpdated"
        | "EOS_RTCAudio_AddNotifyAudioInputState"
        | "EOS_RTCAudio_AddNotifyAudioOutputState"
        | "EOS_RTCAudio_AddNotifyAudioBeforeSend" => notify_never as *const () as usize,
        "EOS_RTC_SetSetting" | "EOS_RTC_SetRoomSetting" => success as *const () as usize,
        "EOS_RTCAudio_AddNotifyAudioDevicesChanged" => notify_devices_changed as *const () as usize,
        "EOS_RTCAudio_SendAudio" => send_audio as *const () as usize,
        "EOS_RTCAudio_UpdateSending" => update_sending as *const () as usize,
        "EOS_RTCAudio_UpdateReceiving" => update_receiving as *const () as usize,
        "EOS_RTCAudio_UpdateSendingVolume" | "EOS_RTCAudio_UpdateReceivingVolume" => update_volume as *const () as usize,
        "EOS_RTCAudio_UpdateParticipantVolume" => update_participant_volume as *const () as usize,
        "EOS_RTCAudio_AddNotifyParticipantUpdated" => notify_audio_participant as *const () as usize,
        "EOS_RTCAudio_AddNotifyAudioBeforeRender" => notify_before_render as *const () as usize,
        "EOS_RTCAudio_RegisterPlatformUser" | "EOS_RTCAudio_UnregisterPlatformUser" => platform_user as *const () as usize,
        "EOS_RTCAudio_QueryInputDevicesInformation" | "EOS_RTCAudio_QueryOutputDevicesInformation" => {
            query_devices as *const () as usize
        }
        "EOS_RTCAudio_GetInputDevicesCount" | "EOS_RTCAudio_GetAudioInputDevicesCount" => input_count as *const () as usize,
        "EOS_RTCAudio_GetOutputDevicesCount" | "EOS_RTCAudio_GetAudioOutputDevicesCount" => output_count as *const () as usize,
        "EOS_RTCAudio_CopyInputDeviceInformationByIndex" => copy_input as *const () as usize,
        "EOS_RTCAudio_CopyOutputDeviceInformationByIndex" => copy_output as *const () as usize,
        "EOS_RTCAudio_InputDeviceInformation_Release" | "EOS_RTCAudio_OutputDeviceInformation_Release" => {
            release_device as *const () as usize
        }
        "EOS_RTCAudio_GetAudioInputDeviceByIndex" => input_by_index as *const () as usize,
        "EOS_RTCAudio_GetAudioOutputDeviceByIndex" => output_by_index as *const () as usize,
        "EOS_RTCAudio_SetInputDeviceSettings" => input_settings as *const () as usize,
        "EOS_RTCAudio_SetOutputDeviceSettings" => output_settings as *const () as usize,
        "EOS_RTCAudio_SetAudioInputSettings" => old_input_settings as *const () as usize,
        "EOS_RTCAudio_SetAudioOutputSettings" => old_output_settings as *const () as usize,
        "EOS_RTCAudio_RegisterPlatformAudioUser" | "EOS_RTCAudio_UnregisterPlatformAudioUser" => {
            success as *const () as usize
        }
        "EOS_RTCData_AddNotifyDataReceived" => notify_data as *const () as usize,
        "EOS_RTCData_AddNotifyParticipantUpdated" => notify_data_participant as *const () as usize,
        "EOS_RTCData_SendData" => send_data as *const () as usize,
        "EOS_RTCData_UpdateSending" => data_sending as *const () as usize,
        "EOS_RTCData_UpdateReceiving" => data_receiving as *const () as usize,
        "EOS_Lobby_GetRTCRoomName" => get_rtc_room_name as *const () as usize,
        "EOS_Lobby_IsRTCRoomConnected" => is_rtc_room_connected as *const () as usize,
        "EOS_Lobby_AddNotifyRTCRoomConnectionChanged" => notify_lobby_room as *const () as usize,
        "EOS_Lobby_JoinRTCRoom" => join_lobby_room as *const () as usize,
        "EOS_Lobby_LeaveRTCRoom" => leave_lobby_room as *const () as usize,
        n if n.starts_with("EOS_RTC_RemoveNotify")
            || n.starts_with("EOS_RTCAudio_RemoveNotify")
            || n.starts_with("EOS_RTCData_RemoveNotify") =>
        {
            remove_notify as *const () as usize
        }
        _ => return None,
    })
}

unsafe extern "system" fn get_audio(_h: usize) -> usize {
    // EOS plays on the system's default devices until the game picks others.
    static FIRST: std::sync::Once = std::sync::Once::new();
    FIRST.call_once(|| {
        voice::choose_device(false, voice::Choice::System);
        voice::choose_device(true, voice::Choice::System);
        voice::watch_devices();
    });
    AUDIO_HANDLE
}

unsafe extern "system" fn get_data(_h: usize) -> usize {
    DATA_HANDLE
}

unsafe extern "system" fn success(_h: usize, _o: usize) -> i32 {
    SUCCESS
}

unsafe extern "system" fn notify_never(_h: usize, _o: usize, cd: usize, cb: usize) -> u64 {
    add_listener(Kind::Never, cb, cd, None)
}

unsafe extern "system" fn remove_notify(_h: usize, id: u64) {
    remove_listener(id);
}

/// Options that start `{ApiVersion, LocalUserId, RoomName}` (most of RTC's).
#[allow(non_snake_case)]
#[repr(C)]
struct RoomOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    RoomName: *const c_char,
}

fn room_of(o: *const RoomOptions) -> Option<(usize, String)> {
    if o.is_null() {
        return None;
    }
    let o = unsafe { &*o };
    Some((o.LocalUserId, str_of(o.RoomName)))
}

/// Completion info that starts `{ResultCode, ClientData, LocalUserId, RoomName}`,
/// followed by `tail` (the call's own fields).
#[repr(C)]
struct RoomDone<T> {
    result: i32,
    client_data: usize,
    local: usize,
    room: *const c_char,
    tail: T,
}

/// Tails `{ParticipantId, flag}` and `{ParticipantId, Volume}`.
#[repr(C)]
struct PeerFlag {
    peer: usize,
    flag: i32,
}

#[repr(C)]
struct PeerVolume {
    peer: usize,
    volume: f32,
}

fn complete<T: Send + 'static>(cb: usize, cd: usize, r: i32, local: usize, room: &str, tail: T) {
    let room = intern(room) as usize;
    later(move || {
        let info = RoomDone { result: r, client_data: cd, local, room: room as *const c_char, tail };
        unsafe { call(cb, &info) };
    });
}

// ---- EOS_RTC ----

#[allow(non_snake_case)]
#[repr(C)]
struct JoinRoomOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    RoomName: *const c_char,
    ClientBaseUrl: *const c_char,
    ParticipantToken: *const c_char,
    ParticipantId: usize,
    Flags: u32,
    bManualAudioInputEnabled: i32,
    bManualAudioOutputEnabled: i32,
}

/// JoinRoom's completion tail: RoomOptionsCount, RoomOptions.
#[repr(C)]
struct NoRoomOptions {
    count: u32,
    options: usize,
}

unsafe extern "system" fn join_room(_h: usize, o: *const JoinRoomOptions, cd: usize, cb: usize) {
    let (Some(lan), false) = (lan(), o.is_null()) else {
        return complete(cb, cd, INVALID_PARAMETERS, 0, "", NoRoomOptions { count: 0, options: 0 });
    };
    let o = unsafe { &*o };
    let room = str_of(o.RoomName);
    if room.is_empty() {
        return complete(cb, cd, INVALID_PARAMETERS, o.LocalUserId, "", NoRoomOptions { count: 0, options: 0 });
    }
    LOCAL.store(o.LocalUserId, Ordering::Relaxed);
    voice::join(lan, &key(&room), o.bManualAudioInputEnabled == 0, o.bManualAudioOutputEnabled == 0);
    trace("EOS_RTC_JoinRoom", || format!("{room} flags={:#x}", o.Flags));
    complete(cb, cd, SUCCESS, o.LocalUserId, &room, NoRoomOptions { count: 0, options: 0 });
}

unsafe extern "system" fn leave_room(_h: usize, o: *const RoomOptions, cd: usize, cb: usize) {
    let Some((local, room)) = room_of(o) else {
        return complete(cb, cd, INVALID_PARAMETERS, 0, "", ());
    };
    let left = lan().is_some_and(|l| voice::leave(l, &key(&room)));
    complete(cb, cd, if left { SUCCESS } else { NOT_FOUND }, local, &room, ());
}

#[allow(non_snake_case)]
#[repr(C)]
struct BlockOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    RoomName: *const c_char,
    ParticipantId: usize,
    bBlocked: i32,
}

unsafe extern "system" fn block_participant(_h: usize, o: *const BlockOptions, cd: usize, cb: usize) {
    if o.is_null() {
        return complete(cb, cd, INVALID_PARAMETERS, 0, "", PeerFlag { peer: 0, flag: 0 });
    }
    let o = unsafe { &*o };
    let room = str_of(o.RoomName);
    let r = match ids::peer_of_puid(o.ParticipantId) {
        Some(p) if voice::set_blocked(&key(&room), p, o.bBlocked != 0) => SUCCESS,
        _ => NOT_FOUND,
    };
    complete(cb, cd, r, o.LocalUserId, &room, PeerFlag { peer: o.ParticipantId, flag: o.bBlocked });
}

fn notify_room(kind: Kind, o: *const RoomOptions, cd: usize, cb: usize) -> u64 {
    let room = room_of(o).map(|(_, r)| r).filter(|r| !r.is_empty());
    add_listener(kind, cb, cd, room)
}

unsafe extern "system" fn notify_participant_status(_h: usize, o: *const RoomOptions, cd: usize, cb: usize) -> u64 {
    let id = notify_room(Kind::RtcParticipantStatus, o, cd, cb);
    replay_members(o, move |me, name, puid| {
        let info = ParticipantStatusInfo {
            ClientData: cd,
            LocalUserId: me,
            RoomName: name,
            ParticipantId: puid,
            ParticipantStatus: STATUS_JOINED,
            ParticipantMetadataCount: 0,
            ParticipantMetadata: 0,
            bParticipantInBlocklist: 0,
        };
        unsafe { call(cb, &info) };
    });
    id
}

/// A LAN room fills within a tick, long before Epic's would have: a listener added
/// after that hears, on the next tick, who is already there (only it; earlier
/// listeners heard the joins as they happened).
fn replay_members(o: *const RoomOptions, f: impl Fn(usize, *const c_char, usize) + Send + 'static) {
    let Some((_, room)) = room_of(o) else { return };
    let members = voice::members(&key(&room));
    if members.is_empty() {
        return;
    }
    let name = intern(&room) as usize;
    later(move || {
        for p in members {
            f(local(), name as *const c_char, ids::puid(p));
        }
    });
}

// ---- EOS_RTCAudio ----

#[allow(non_snake_case)]
#[repr(C)]
struct AudioBuffer {
    ApiVersion: i32,
    Frames: *mut i16,
    FramesCount: u32,
    SampleRate: u32,
    Channels: u32,
}

#[allow(non_snake_case)]
#[repr(C)]
struct SendAudioOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    RoomName: *const c_char,
    Buffer: *const AudioBuffer,
}

unsafe extern "system" fn send_audio(_h: usize, o: *const SendAudioOptions) -> i32 {
    let (Some(lan), false) = (lan(), o.is_null()) else { return INVALID_PARAMETERS };
    let o = unsafe { &*o };
    if o.Buffer.is_null() {
        return INVALID_PARAMETERS;
    }
    let b = unsafe { &*o.Buffer };
    if b.Frames.is_null() || b.Channels == 0 || b.SampleRate == 0 {
        return INVALID_PARAMETERS;
    }
    let n = b.FramesCount as usize * b.Channels as usize;
    let samples = unsafe { std::slice::from_raw_parts(b.Frames, n) }.to_vec();
    let room = str_of(o.RoomName);
    let f = Frame { rate: b.SampleRate, channels: b.Channels, samples };
    if voice::send_audio(lan, &key(&room), &f) { SUCCESS } else { NOT_FOUND }
}

#[allow(non_snake_case)]
#[repr(C)]
struct StatusOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    RoomName: *const c_char,
    Status: i32,
}

unsafe extern "system" fn update_sending(_h: usize, o: *const StatusOptions, cd: usize, cb: usize) {
    if o.is_null() {
        return complete(cb, cd, INVALID_PARAMETERS, 0, "", 0i32);
    }
    let o = unsafe { &*o };
    let room = str_of(o.RoomName);
    let ok = voice::set_sending(&key(&room), o.Status == AUDIO_ENABLED);
    complete(cb, cd, if ok { SUCCESS } else { NOT_FOUND }, o.LocalUserId, &room, o.Status);
}

/// UpdateReceiving (audio) and RTCData's UpdateReceiving share this layout.
#[allow(non_snake_case)]
#[repr(C)]
struct ReceivingOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    RoomName: *const c_char,
    ParticipantId: usize,
    bEnabled: i32,
}

unsafe extern "system" fn update_receiving(_h: usize, o: *const ReceivingOptions, cd: usize, cb: usize) {
    if o.is_null() {
        return complete(cb, cd, INVALID_PARAMETERS, 0, "", PeerFlag { peer: 0, flag: 0 });
    }
    let o = unsafe { &*o };
    let room = str_of(o.RoomName);
    let peer = ids::peer_of_puid(o.ParticipantId);
    let ok = (o.ParticipantId == 0 || peer.is_some()) && voice::set_receiving(&key(&room), peer, o.bEnabled != 0);
    complete(cb, cd, if ok { SUCCESS } else { NOT_FOUND }, o.LocalUserId, &room, PeerFlag { peer: o.ParticipantId, flag: o.bEnabled });
}

#[allow(non_snake_case)]
#[repr(C)]
struct VolumeOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    RoomName: *const c_char,
    Volume: f32,
}

/// Sending volume isn't applied (the microphone's level is the player's); receiving
/// volume scales what PeerParty plays.
unsafe extern "system" fn update_volume(_h: usize, o: *const VolumeOptions, cd: usize, cb: usize) {
    if o.is_null() {
        return complete(cb, cd, INVALID_PARAMETERS, 0, "", 0f32);
    }
    let o = unsafe { &*o };
    let room = str_of(o.RoomName);
    let ok = voice::joined(&key(&room));
    complete(cb, cd, if ok { SUCCESS } else { NOT_FOUND }, o.LocalUserId, &room, o.Volume);
}

#[allow(non_snake_case)]
#[repr(C)]
struct ParticipantVolumeOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    RoomName: *const c_char,
    ParticipantId: usize,
    Volume: f32,
}

unsafe extern "system" fn update_participant_volume(_h: usize, o: *const ParticipantVolumeOptions, cd: usize, cb: usize) {
    if o.is_null() {
        return complete(cb, cd, INVALID_PARAMETERS, 0, "", PeerVolume { peer: 0, volume: 0.0 });
    }
    let o = unsafe { &*o };
    let room = str_of(o.RoomName);
    let ok = ids::peer_of_puid(o.ParticipantId).is_some_and(|p| voice::set_volume(&key(&room), Some(p), o.Volume));
    complete(cb, cd, if ok { SUCCESS } else { NOT_FOUND }, o.LocalUserId, &room, PeerVolume { peer: o.ParticipantId, volume: o.Volume });
}

unsafe extern "system" fn notify_audio_participant(_h: usize, o: *const RoomOptions, cd: usize, cb: usize) -> u64 {
    notify_room(Kind::RtcAudioParticipant, o, cd, cb)
}

unsafe extern "system" fn notify_before_render(_h: usize, o: *const RoomOptions, cd: usize, cb: usize) -> u64 {
    notify_room(Kind::RtcBeforeRender, o, cd, cb)
}

#[allow(non_snake_case)]
#[repr(C)]
struct PlatformUserOptions {
    ApiVersion: i32,
    PlatformUserId: *const c_char,
}

#[repr(C)]
struct PlatformUserInfo {
    result: i32,
    client_data: usize,
    user: *const c_char,
}

unsafe extern "system" fn platform_user(_h: usize, o: *const PlatformUserOptions, cd: usize, cb: usize) {
    let user = if o.is_null() { 0 } else { intern(&str_of(unsafe { (*o).PlatformUserId })) as usize };
    later(move || {
        let info = PlatformUserInfo { result: SUCCESS, client_data: cd, user: user as *const c_char };
        unsafe { call(cb, &info) };
    });
}

unsafe extern "system" fn query_devices(_h: usize, _o: usize, cd: usize, cb: usize) {
    later(move || {
        let info = super::abi::Completion { ResultCode: SUCCESS, ClientData: cd };
        unsafe { call(cb, &info) };
    });
}

/// Input and output device information share one layout (and the deprecated
/// AudioInputDeviceInfo / AudioOutputDeviceInfo too).
#[allow(non_snake_case)]
#[repr(C)]
struct DeviceInfo {
    ApiVersion: i32,
    bDefaultDevice: i32,
    DeviceId: *const c_char,
    DeviceName: *const c_char,
}

#[allow(non_snake_case)]
#[repr(C)]
struct IndexOptions {
    ApiVersion: i32,
    DeviceIndex: u32,
}

/// Windows' devices one way, and which is EOS's default (the system default).
fn device_list(output: bool) -> Vec<(voice::Endpoint, bool)> {
    let snap = voice::snapshot();
    let side = snap.side(output);
    side.list.iter().map(|e| (e.clone(), side.system.as_deref() == Some(e.id.as_str()))).collect()
}

unsafe extern "system" fn input_count(_h: usize, _o: usize) -> u32 {
    device_list(false).len() as u32
}

unsafe extern "system" fn output_count(_h: usize, _o: usize) -> u32 {
    device_list(true).len() as u32
}

unsafe fn copy_device(o: *const IndexOptions, out: *mut usize, output: bool) -> i32 {
    if o.is_null() || out.is_null() {
        return INVALID_PARAMETERS;
    }
    let Some((e, default)) = device_list(output).into_iter().nth(unsafe { (*o).DeviceIndex } as usize) else {
        return NOT_FOUND;
    };
    let mut keep = Vec::new();
    let info = DeviceInfo {
        ApiVersion: 1,
        bDefaultDevice: default as i32,
        DeviceId: owned_str(&mut keep, &e.id),
        DeviceName: owned_str(&mut keep, &e.name),
    };
    unsafe { *out = hand_out(info, keep) };
    SUCCESS
}

unsafe extern "system" fn copy_input(_h: usize, o: *const IndexOptions, out: *mut usize) -> i32 {
    unsafe { copy_device(o, out, false) }
}

unsafe extern "system" fn copy_output(_h: usize, o: *const IndexOptions, out: *mut usize) -> i32 {
    unsafe { copy_device(o, out, true) }
}

unsafe extern "system" fn release_device(p: usize) {
    release(p);
}

/// The deprecated by-index calls hand out a pointer the SDK owns: each distinct
/// device's info is made once and kept.
fn device_by_index(o: *const IndexOptions, output: bool) -> usize {
    struct Kept(DeviceInfo);
    unsafe impl Send for Kept {}
    static KEPT: LazyLock<Mutex<HashMap<(bool, String, String, bool), usize>>> = LazyLock::new(Mutex::default);
    if o.is_null() {
        return 0;
    }
    let Some((e, default)) = device_list(output).into_iter().nth(unsafe { (*o).DeviceIndex } as usize) else { return 0 };
    let mut kept = KEPT.lock().unwrap();
    *kept.entry((output, e.id.clone(), e.name.clone(), default)).or_insert_with(|| {
        let info = Box::leak(Box::new(Kept(DeviceInfo {
            ApiVersion: 1,
            bDefaultDevice: default as i32,
            DeviceId: intern(&e.id),
            DeviceName: intern(&e.name),
        })));
        &info.0 as *const DeviceInfo as usize
    })
}

unsafe extern "system" fn input_by_index(_h: usize, o: *const IndexOptions) -> usize {
    device_by_index(o, false)
}

unsafe extern "system" fn output_by_index(_h: usize, o: *const IndexOptions) -> usize {
    device_by_index(o, true)
}

/// Device settings start `{ApiVersion, LocalUserId, RealDeviceId}` (the deprecated
/// SetAudioInput/OutputSettings' DeviceId is in the same place).
#[allow(non_snake_case)]
#[repr(C)]
struct DeviceSettingsOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    RealDeviceId: *const c_char,
}

#[repr(C)]
struct DeviceSettingsInfo {
    result: i32,
    client_data: usize,
    device: *const c_char,
}

/// The game's device: an empty id is the default device.
unsafe fn choose(o: *const DeviceSettingsOptions, output: bool) -> String {
    let id = if o.is_null() { String::new() } else { str_of(unsafe { (*o).RealDeviceId }) };
    voice::choose_device(output, if id.is_empty() { voice::Choice::System } else { voice::Choice::Device(id.clone()) });
    id
}

unsafe fn device_settings(o: *const DeviceSettingsOptions, cd: usize, cb: usize, output: bool) {
    let device = intern(&unsafe { choose(o, output) }) as usize;
    later(move || {
        let info = DeviceSettingsInfo { result: SUCCESS, client_data: cd, device: device as *const c_char };
        unsafe { call(cb, &info) };
    });
}

unsafe extern "system" fn input_settings(_h: usize, o: *const DeviceSettingsOptions, cd: usize, cb: usize) {
    unsafe { device_settings(o, cd, cb, false) }
}

unsafe extern "system" fn output_settings(_h: usize, o: *const DeviceSettingsOptions, cd: usize, cb: usize) {
    unsafe { device_settings(o, cd, cb, true) }
}

unsafe extern "system" fn old_input_settings(_h: usize, o: *const DeviceSettingsOptions) -> i32 {
    unsafe { choose(o, false) };
    SUCCESS
}

unsafe extern "system" fn old_output_settings(_h: usize, o: *const DeviceSettingsOptions) -> i32 {
    unsafe { choose(o, true) };
    SUCCESS
}

unsafe extern "system" fn notify_devices_changed(_h: usize, _o: usize, cd: usize, cb: usize) -> u64 {
    add_listener(Kind::RtcAudioDevices, cb, cd, None)
}

/// Windows' devices changed since the last look: AudioDevicesChanged.
fn devices_changed() {
    static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = voice::devices_version();
    let before = SEEN.swap(now, Ordering::Relaxed);
    if before == 0 || before == now {
        return;
    }
    #[allow(non_snake_case)]
    #[repr(C)]
    struct Info {
        ClientData: usize,
    }
    for (f, cd) in listeners(Kind::RtcAudioDevices, None) {
        unsafe { call(f, &Info { ClientData: cd }) };
    }
}

// ---- EOS_RTCData ----

#[allow(non_snake_case)]
#[repr(C)]
struct SendDataOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    RoomName: *const c_char,
    DataLengthBytes: u32,
    Data: *const u8,
}

unsafe extern "system" fn send_data(_h: usize, o: *const SendDataOptions) -> i32 {
    let (Some(lan), false) = (lan(), o.is_null()) else { return INVALID_PARAMETERS };
    let o = unsafe { &*o };
    if o.DataLengthBytes > MAX_DATA {
        return LIMIT_EXCEEDED;
    }
    if o.Data.is_null() && o.DataLengthBytes > 0 {
        return INVALID_PARAMETERS;
    }
    let data = if o.DataLengthBytes == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(o.Data, o.DataLengthBytes as usize) }.to_vec()
    };
    let room = str_of(o.RoomName);
    if voice::send_data(lan, &key(&room), &data) { SUCCESS } else { NOT_FOUND }
}

#[allow(non_snake_case)]
#[repr(C)]
struct DataSendingOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    RoomName: *const c_char,
    bDataEnabled: i32,
}

unsafe extern "system" fn data_sending(_h: usize, o: *const DataSendingOptions, cd: usize, cb: usize) {
    if o.is_null() {
        return complete(cb, cd, INVALID_PARAMETERS, 0, "", 0i32);
    }
    let o = unsafe { &*o };
    let room = str_of(o.RoomName);
    let ok = voice::set_data_sending(&key(&room), o.bDataEnabled != 0);
    complete(cb, cd, if ok { SUCCESS } else { NOT_FOUND }, o.LocalUserId, &room, o.bDataEnabled);
}

/// Receiving data is always on (the data is the game's own; it can ignore it).
unsafe extern "system" fn data_receiving(_h: usize, o: *const ReceivingOptions, cd: usize, cb: usize) {
    if o.is_null() {
        return complete(cb, cd, INVALID_PARAMETERS, 0, "", PeerFlag { peer: 0, flag: 0 });
    }
    let o = unsafe { &*o };
    let room = str_of(o.RoomName);
    let ok = voice::joined(&key(&room));
    complete(cb, cd, if ok { SUCCESS } else { NOT_FOUND }, o.LocalUserId, &room, PeerFlag { peer: o.ParticipantId, flag: o.bEnabled });
}

unsafe extern "system" fn notify_data(_h: usize, o: *const RoomOptions, cd: usize, cb: usize) -> u64 {
    notify_room(Kind::RtcData, o, cd, cb)
}

unsafe extern "system" fn notify_data_participant(_h: usize, o: *const RoomOptions, cd: usize, cb: usize) -> u64 {
    let id = notify_room(Kind::RtcDataParticipant, o, cd, cb);
    replay_members(o, move |me, name, puid| {
        let info = DataParticipantInfo {
            ClientData: cd,
            LocalUserId: me,
            RoomName: name,
            ParticipantId: puid,
            DataStatus: DATA_ENABLED,
        };
        unsafe { call(cb, &info) };
    });
    id
}

// ---- the lobby's room ----

#[allow(non_snake_case)]
#[repr(C)]
struct LobbyRoomOptions {
    ApiVersion: i32,
    LobbyId: *const c_char,
    LocalUserId: usize,
    LocalRTCOptions: usize,
}

fn in_lobby(lobby: &str) -> bool {
    lan().is_some_and(|l| l.my_lobbies("eos.lobby").iter().any(|x| x.id == lobby))
}

unsafe extern "system" fn get_rtc_room_name(
    _h: usize,
    o: *const LobbyRoomOptions,
    out: *mut c_char,
    len: *mut u32,
) -> i32 {
    if o.is_null() || len.is_null() {
        return INVALID_PARAMETERS;
    }
    lobby_voice(0);
    let lobby = str_of(unsafe { (*o).LobbyId });
    if !in_lobby(&lobby) {
        return NOT_FOUND;
    }
    let name = lobby_room(&lobby);
    let need = name.len() as u32 + 1;
    let have = unsafe { *len };
    unsafe { *len = need };
    if out.is_null() || have < need {
        return LIMIT_EXCEEDED;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(name.as_ptr(), out as *mut u8, name.len());
        *out.add(name.len()) = 0;
    }
    SUCCESS
}

unsafe extern "system" fn is_rtc_room_connected(_h: usize, o: *const LobbyRoomOptions, out: *mut i32) -> i32 {
    if o.is_null() || out.is_null() {
        return INVALID_PARAMETERS;
    }
    lobby_voice(0);
    let lobby = str_of(unsafe { (*o).LobbyId });
    if !in_lobby(&lobby) {
        return NOT_FOUND;
    }
    unsafe { *out = voice::joined(&key(&lobby_room(&lobby))) as i32 };
    SUCCESS
}

unsafe extern "system" fn notify_lobby_room(_h: usize, _o: usize, cd: usize, cb: usize) -> u64 {
    lobby_voice(0);
    add_listener(Kind::LobbyRtcConnection, cb, cd, None)
}

#[repr(C)]
struct LobbyRoomDone {
    result: i32,
    client_data: usize,
    lobby: *const c_char,
}

fn lobby_done(cb: usize, cd: usize, r: i32, lobby: &str) {
    let lobby = intern(lobby) as usize;
    later(move || {
        let info = LobbyRoomDone { result: r, client_data: cd, lobby: lobby as *const c_char };
        unsafe { call(cb, &info) };
    });
}

unsafe extern "system" fn join_lobby_room(_h: usize, o: *const LobbyRoomOptions, cd: usize, cb: usize) {
    if o.is_null() {
        return lobby_done(cb, cd, INVALID_PARAMETERS, "");
    }
    let o = unsafe { &*o };
    lobby_voice(o.LocalRTCOptions);
    let lobby = str_of(o.LobbyId);
    if !in_lobby(&lobby) {
        return lobby_done(cb, cd, NOT_FOUND, &lobby);
    }
    LOBBY_ROOMS.lock().unwrap().remove(&lobby);
    sync_lobby_rooms();
    lobby_done(cb, cd, SUCCESS, &lobby);
}

unsafe extern "system" fn leave_lobby_room(_h: usize, o: *const LobbyRoomOptions, cd: usize, cb: usize) {
    if o.is_null() {
        return lobby_done(cb, cd, INVALID_PARAMETERS, "");
    }
    let lobby = str_of(unsafe { (*o).LobbyId });
    let Some(lan) = lan() else { return lobby_done(cb, cd, NOT_FOUND, &lobby) };
    let left = voice::leave(lan, &key(&lobby_room(&lobby)));
    if left {
        LOBBY_ROOMS.lock().unwrap().insert(lobby.clone(), false);
        fire_lobby_room(&lobby, false);
    }
    lobby_done(cb, cd, if left { SUCCESS } else { NOT_FOUND }, &lobby);
}

#[allow(non_snake_case)]
#[repr(C)]
struct LobbyRoomChanged {
    ClientData: usize,
    LobbyId: *const c_char,
    LocalUserId: usize,
    bIsConnected: i32,
    DisconnectReason: i32,
}

fn fire_lobby_room(lobby: &str, connected: bool) {
    let id = intern(lobby);
    for (f, cd) in listeners(Kind::LobbyRtcConnection, None) {
        let info = LobbyRoomChanged {
            ClientData: cd,
            LobbyId: id,
            LocalUserId: local(),
            bIsConnected: connected as i32,
            DisconnectReason: SUCCESS,
        };
        unsafe { call(f, &info) };
    }
}

/// Lobby rooms follow the lobbies: joined when we're in a lobby (unless the game left
/// its room by hand), left with it.
fn sync_lobby_rooms() {
    if !lobby_rooms_on() {
        return;
    }
    let Some(lan) = lan() else { return };
    let lobbies: Vec<String> = lan.my_lobbies("eos.lobby").into_iter().map(|l| l.id).collect();
    let (manual_in, manual_out) = *LOBBY_MANUAL.lock().unwrap();
    let mut changes = Vec::new();
    {
        let mut rooms = LOBBY_ROOMS.lock().unwrap();
        for l in &lobbies {
            if !rooms.contains_key(l) {
                rooms.insert(l.clone(), true);
                changes.push((l.clone(), true));
            }
        }
        let gone: Vec<String> = rooms.keys().filter(|l| !lobbies.contains(l)).cloned().collect();
        for l in gone {
            if rooms.remove(&l) == Some(true) {
                changes.push((l, false));
            }
        }
    }
    for (l, join) in changes {
        let room = key(&lobby_room(&l));
        if join {
            voice::join(lan, &room, !manual_in, !manual_out);
        } else {
            voice::leave(lan, &room);
        }
        fire_lobby_room(&l, join);
    }
}

// ---- events ----

#[allow(non_snake_case)]
#[repr(C)]
struct ParticipantStatusInfo {
    ClientData: usize,
    LocalUserId: usize,
    RoomName: *const c_char,
    ParticipantId: usize,
    ParticipantStatus: i32,
    ParticipantMetadataCount: u32,
    ParticipantMetadata: usize,
    bParticipantInBlocklist: i32,
}

#[allow(non_snake_case)]
#[repr(C)]
struct AudioParticipantInfo {
    ClientData: usize,
    LocalUserId: usize,
    RoomName: *const c_char,
    ParticipantId: usize,
    bSpeaking: i32,
    AudioStatus: i32,
}

#[allow(non_snake_case)]
#[repr(C)]
struct BeforeRenderInfo {
    ClientData: usize,
    LocalUserId: usize,
    RoomName: *const c_char,
    Buffer: *mut AudioBuffer,
    ParticipantId: usize,
}

#[allow(non_snake_case)]
#[repr(C)]
struct DataReceivedInfo {
    ClientData: usize,
    LocalUserId: usize,
    RoomName: *const c_char,
    DataLengthBytes: u32,
    Data: *const u8,
    ParticipantId: usize,
}

#[allow(non_snake_case)]
#[repr(C)]
struct DataParticipantInfo {
    ClientData: usize,
    LocalUserId: usize,
    RoomName: *const c_char,
    ParticipantId: usize,
    DataStatus: i32,
}

pub fn on_event(lan: &Lan, ev: &Event) {
    voice::on_event(lan, ev);
}

/// Runs from EOS_Platform_Tick: room traffic becomes the game's notifications.
pub fn pump(lan: &Lan) {
    sync_lobby_rooms();
    devices_changed();
    voice::pump(lan);
    let me = local();
    for k in voice::rooms() {
        let Some(room) = k.strip_prefix("eos:") else { continue };
        let name = intern(room);
        for ev in voice::take(&k) {
            match ev {
                RoomEvent::Joined(p) | RoomEvent::Left(p) => {
                    let joined = matches!(ev, RoomEvent::Joined(_));
                    trace("RTC participant", || format!("{room} {p} joined={joined}"));
                    let puid = ids::puid(p);
                    for (f, cd) in listeners(Kind::RtcParticipantStatus, Some(room)) {
                        let info = ParticipantStatusInfo {
                            ClientData: cd,
                            LocalUserId: me,
                            RoomName: name,
                            ParticipantId: puid,
                            ParticipantStatus: if joined { STATUS_JOINED } else { STATUS_LEFT },
                            ParticipantMetadataCount: 0,
                            ParticipantMetadata: 0,
                            bParticipantInBlocklist: 0,
                        };
                        unsafe { call(f, &info) };
                    }
                    if joined {
                        for (f, cd) in listeners(Kind::RtcDataParticipant, Some(room)) {
                            let info = DataParticipantInfo {
                                ClientData: cd,
                                LocalUserId: me,
                                RoomName: name,
                                ParticipantId: puid,
                                DataStatus: DATA_ENABLED,
                            };
                            unsafe { call(f, &info) };
                        }
                    }
                }
                RoomEvent::Speaking(p, speaking) => {
                    for (f, cd) in listeners(Kind::RtcAudioParticipant, Some(room)) {
                        let info = AudioParticipantInfo {
                            ClientData: cd,
                            LocalUserId: me,
                            RoomName: name,
                            ParticipantId: ids::puid(p),
                            bSpeaking: speaking as i32,
                            AudioStatus: AUDIO_ENABLED,
                        };
                        unsafe { call(f, &info) };
                    }
                }
                RoomEvent::Audio(p, mut frame) => {
                    let ch = frame.channels.max(1);
                    let mut buffer = AudioBuffer {
                        ApiVersion: 1,
                        Frames: frame.samples.as_mut_ptr(),
                        FramesCount: frame.samples.len() as u32 / ch,
                        SampleRate: frame.rate,
                        Channels: ch,
                    };
                    for (f, cd) in listeners(Kind::RtcBeforeRender, Some(room)) {
                        let info = BeforeRenderInfo {
                            ClientData: cd,
                            LocalUserId: me,
                            RoomName: name,
                            Buffer: &mut buffer,
                            ParticipantId: ids::puid(p),
                        };
                        unsafe { call(f, &info) };
                    }
                }
                // The SDK never delivers more; a game's buffer may be sized to it.
                RoomEvent::Data(_, data) if data.len() > MAX_DATA as usize => {}
                RoomEvent::Data(p, data) => {
                    for (f, cd) in listeners(Kind::RtcData, Some(room)) {
                        let info = DataReceivedInfo {
                            ClientData: cd,
                            LocalUserId: me,
                            RoomName: name,
                            DataLengthBytes: data.len() as u32,
                            Data: data.as_ptr(),
                            ParticipantId: ids::puid(p),
                        };
                        unsafe { call(f, &info) };
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn layouts_match_the_headers_on_x64() {
        assert_eq!(offset_of!(JoinRoomOptions, Flags), 48);
        assert_eq!(offset_of!(JoinRoomOptions, bManualAudioOutputEnabled), 56);
        assert_eq!(offset_of!(ParticipantStatusInfo, ParticipantStatus), 32);
        assert_eq!(offset_of!(ParticipantStatusInfo, bParticipantInBlocklist), 48);
        assert_eq!(offset_of!(BeforeRenderInfo, ParticipantId), 32);
        assert_eq!(offset_of!(DataReceivedInfo, Data), 32);
        assert_eq!(offset_of!(AudioBuffer, SampleRate), 20);
        assert_eq!(size_of::<RoomDone<NoRoomOptions>>(), 48);
        assert_eq!(offset_of!(LobbyRoomChanged, bIsConnected), 24);
        assert_eq!(offset_of!(SendDataOptions, Data), 32);
    }

    #[test]
    fn lobby_rooms_are_named_after_the_lobby() {
        assert_eq!(lobby_room("abc"), "abc:peerparty-voice");
    }
}
