//! EOS_CustomInvites on the LAN: custom invites (an opaque payload the game sets) and
//! "request to join", carried by PeerParty's LAN invites.
//!
//! A custom invite is a networked invite: a message carrying the sender's ProductUserId
//! and the payload the game set with `SetCustomInvite`; the receiver stores it and fires
//! `CustomInviteReceived`, and request to join is sent, accepted and rejected over the
//! same mesh. Struct layouts are from the SDK's eos_custominvites_types.h (1.19).
//!
//! On the LAN core an invite's connect string carries the message:
//! `peerparty:eosci:invite:<payload>`, `peerparty:eosci:rtj`, `peerparty:eosci:rtj-accept`,
//! `peerparty:eosci:rtj-reject` (net.rs routes them to the EOS front end only). As with
//! lobby and session invites, an invite is accepted at once (the game gets Received, then
//! Accepted, as if the player had accepted in Epic's overlay) unless PEERPARTY_INVITES=ask;
//! a request to join is likewise accepted at once when the game listens for the overlay's
//! "accepted".
//!
//! Not tried in a game yet.

use std::collections::{HashMap, HashSet};
use std::ffi::c_char;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use peerparty_core::{Event, Lan, PeerId};

use super::abi::result::*;
use super::ids::{self, intern, text};
use super::{Kind, add_listener, call, lan, later, listeners, remove_listener, trace};

pub(crate) const PREFIX: &str = "peerparty:eosci:";
const INVITE: &str = "peerparty:eosci:invite:";
const RTJ: &str = "peerparty:eosci:rtj";
const RTJ_ACCEPT: &str = "peerparty:eosci:rtj-accept";
const RTJ_REJECT: &str = "peerparty:eosci:rtj-reject";

/// EOS_CUSTOMINVITES_MAX_PAYLOAD_LENGTH.
const MAX_PAYLOAD: usize = 500;
const INVALID_REQUEST: i32 = 11;
/// EOS_ERequestToJoinResponse.
const RTJR_ACCEPTED: i32 = 0;
const RTJR_REJECTED: i32 = 1;

/// Whether a LAN invite's connect string is one of ours.
pub(crate) fn is_ours(connect: &str) -> bool {
    connect.starts_with(PREFIX)
}

/// The answer to a request to join (not something the player answers).
pub(crate) fn is_response(connect: &str) -> bool {
    matches!(parse(connect), Some(Message::Response(_)))
}

pub(crate) fn is_request(connect: &str) -> bool {
    matches!(parse(connect), Some(Message::RequestToJoin))
}

/// The player declined a request to join in the overlay.
pub(crate) fn decline_request(from: PeerId) {
    answer(from, false);
}

#[derive(Default)]
struct State {
    payload: Option<String>,
    /// Received invites not yet finalized: id -> (sender, payload).
    received: HashMap<String, (PeerId, String)>,
    next_id: u64,
    /// Requests to join we received and haven't answered.
    requests: HashSet<PeerId>,
    /// Players we asked to join, and when: only they can answer (`ASKED_FOR`).
    asked: HashMap<PeerId, Instant>,
    rtj_disabled: bool,
}

/// How long a request to join we sent can be answered.
const ASKED_FOR: Duration = Duration::from_secs(600);
/// Players asked at once, and custom invites the game hasn't finalized: the oldest go
/// when there are more (each received invite is a peer's payload kept for the game).
const MAX_ASKED: usize = 64;
const MAX_RECEIVED: usize = 64;

impl State {
    fn ask(&mut self, peer: PeerId, now: Instant) {
        self.asked.retain(|_, at| now.duration_since(*at) < ASKED_FOR);
        while self.asked.len() >= MAX_ASKED {
            let Some(oldest) = self.asked.iter().min_by_key(|(_, at)| **at).map(|(p, _)| *p) else { break };
            self.asked.remove(&oldest);
        }
        self.asked.insert(peer, now);
    }

    /// Whether `from` may answer a request to join (we asked them, lately); an answer
    /// uses the request up.
    fn answered_by(&mut self, from: PeerId, now: Instant) -> bool {
        self.asked.remove(&from).is_some_and(|at| now.duration_since(at) < ASKED_FOR)
    }

    /// Keeps a received custom invite for FinalizeInvite: its id.
    fn keep_received(&mut self, from: PeerId, payload: &str) -> String {
        while self.received.len() >= MAX_RECEIVED {
            let oldest = self.received.keys().min_by_key(|id| invite_number(id)).cloned();
            let Some(oldest) = oldest else { break };
            self.received.remove(&oldest);
        }
        self.next_id += 1;
        let id = format!("peerparty-custom-invite-{}", self.next_id);
        self.received.insert(id.clone(), (from, payload.to_string()));
        id
    }
}

fn invite_number(id: &str) -> u64 {
    id.rsplit('-').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

static S: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);

#[allow(non_snake_case)]
#[repr(C)]
struct SetCustomInviteOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    Payload: *const c_char,
}

#[allow(non_snake_case)]
#[repr(C)]
struct SendCustomInviteOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    TargetUserIds: *const usize,
    TargetUserIdsCount: u32,
}

#[allow(non_snake_case)]
#[repr(C)]
struct SendCustomInviteInfo {
    ResultCode: i32,
    ClientData: usize,
    LocalUserId: usize,
    TargetUserIds: *const usize,
    TargetUserIdsCount: u32,
}

/// OnCustomInviteReceived / Accepted / Rejected callback info (one layout).
#[allow(non_snake_case)]
#[repr(C)]
struct CustomInviteInfo {
    ClientData: usize,
    /// The user that sent the invite.
    TargetUserId: usize,
    /// The local recipient.
    LocalUserId: usize,
    CustomInviteId: *const c_char,
    Payload: *const c_char,
}

#[allow(non_snake_case)]
#[repr(C)]
struct FinalizeInviteOptions {
    ApiVersion: i32,
    TargetUserId: usize,
    LocalUserId: usize,
    CustomInviteId: *const c_char,
    ProcessingResult: i32,
}

/// SendRequestToJoin / AcceptRequestToJoin / RejectRequestToJoin options.
#[allow(non_snake_case)]
#[repr(C)]
struct LocalTargetOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    TargetUserId: usize,
}

/// Completion info of those three: ResultCode, ClientData, LocalUserId, TargetUserId.
type LocalTargetInfo = super::abi::CompletionTwoIds;

#[allow(non_snake_case)]
#[repr(C)]
struct RequestToJoinReceivedInfo {
    ClientData: usize,
    FromUserId: usize,
    ToUserId: usize,
}

#[allow(non_snake_case)]
#[repr(C)]
struct RequestToJoinResponseInfo {
    ClientData: usize,
    FromUserId: usize,
    ToUserId: usize,
    Response: i32,
}

/// OnRequestToJoinAccepted / Rejected (the overlay's answer): TargetUserId sent the request.
#[allow(non_snake_case)]
#[repr(C)]
struct RequestToJoinOverlayInfo {
    ClientData: usize,
    TargetUserId: usize,
    LocalUserId: usize,
}

pub(super) fn resolve(name: &str) -> Option<usize> {
    Some(match name {
        "EOS_CustomInvites_SetCustomInvite" => set_custom_invite as *const () as usize,
        "EOS_CustomInvites_SendCustomInvite" => send_custom_invite as *const () as usize,
        "EOS_CustomInvites_FinalizeInvite" => finalize_invite as *const () as usize,
        "EOS_CustomInvites_SendRequestToJoin" => send_request_to_join as *const () as usize,
        "EOS_CustomInvites_AcceptRequestToJoin" => accept_request_to_join as *const () as usize,
        "EOS_CustomInvites_RejectRequestToJoin" => reject_request_to_join as *const () as usize,
        "EOS_CustomInvites_DisableRequestToJoin" => disable_request_to_join as *const () as usize,
        "EOS_CustomInvites_AddNotifyCustomInviteReceived" => notify_received as *const () as usize,
        "EOS_CustomInvites_AddNotifyCustomInviteAccepted" => notify_accepted as *const () as usize,
        "EOS_CustomInvites_AddNotifyCustomInviteRejected" => notify_rejected as *const () as usize,
        "EOS_CustomInvites_AddNotifyRequestToJoinReceived" => notify_rtj_received as *const () as usize,
        "EOS_CustomInvites_AddNotifyRequestToJoinResponseReceived" => notify_rtj_response as *const () as usize,
        "EOS_CustomInvites_AddNotifyRequestToJoinAccepted" => notify_rtj_accepted as *const () as usize,
        "EOS_CustomInvites_AddNotifyRequestToJoinRejected" => notify_rtj_rejected as *const () as usize,
        // Only for integrated platforms that manage their own sessions; never fires here.
        "EOS_CustomInvites_AddNotifySendCustomNativeInviteRequested" => notify_never as *const () as usize,
        n if n.starts_with("EOS_CustomInvites_RemoveNotify") => remove_notify as *const () as usize,
        _ => return None,
    })
}

/// Sends one of our messages to a LAN player. False if it isn't one.
fn send_to(peer: PeerId, connect: &str) -> bool {
    lan().is_some_and(|l| l.invite(Some(peer), connect, None) > 0)
}

fn me() -> Option<PeerId> {
    lan().map(|l| l.me())
}

unsafe extern "system" fn set_custom_invite(_h: usize, o: *const SetCustomInviteOptions) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let payload = unsafe { text((*o).Payload) }.unwrap_or_default();
    if payload.len() > MAX_PAYLOAD {
        return INVALID_PARAMETERS;
    }
    trace("EOS_CustomInvites_SetCustomInvite", || format!("payload={payload:?}"));
    S.lock().unwrap().payload = Some(payload);
    SUCCESS
}

unsafe extern "system" fn send_custom_invite(_h: usize, o: *const SendCustomInviteOptions, cd: usize, cb: usize) {
    let (local, targets) = if o.is_null() {
        (0, Vec::new())
    } else {
        let o = unsafe { &*o };
        let targets = if o.TargetUserIds.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(o.TargetUserIds, o.TargetUserIdsCount as usize) }.to_vec()
        };
        (o.LocalUserId, targets)
    };
    let payload = S.lock().unwrap().payload.clone();
    let r = match payload {
        _ if targets.is_empty() => INVALID_PARAMETERS,
        // The SDK wants SetCustomInvite first.
        None => INVALID_REQUEST,
        Some(p) => {
            let connect = format!("{INVITE}{p}");
            let sent = targets
                .iter()
                .filter_map(|t| ids::peer_of_puid(*t))
                .filter(|peer| send_to(*peer, &connect))
                .count();
            trace("EOS_CustomInvites_SendCustomInvite", || format!("to {sent} of {} target(s)", targets.len()));
            if sent > 0 { SUCCESS } else { NOT_FOUND }
        }
    };
    later(move || {
        let info = SendCustomInviteInfo {
            ResultCode: r,
            ClientData: cd,
            LocalUserId: local,
            TargetUserIds: targets.as_ptr(),
            TargetUserIdsCount: targets.len() as u32,
        };
        unsafe { call(cb, &info) };
    });
}

unsafe extern "system" fn finalize_invite(_h: usize, o: *const FinalizeInviteOptions) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let id = unsafe { text((*o).CustomInviteId) }.unwrap_or_default();
    if S.lock().unwrap().received.remove(&id).is_some() { SUCCESS } else { NOT_FOUND }
}

fn local_target(o: *const LocalTargetOptions) -> (usize, usize) {
    if o.is_null() { (0, 0) } else { unsafe { ((*o).LocalUserId, (*o).TargetUserId) } }
}

fn complete(cb: usize, cd: usize, r: i32, local: usize, target: usize) {
    later(move || {
        let info = LocalTargetInfo {
            ResultCode: r,
            ClientData: cd,
            First: local,
            Second: target,
        };
        unsafe { call(cb, &info) };
    });
}

unsafe extern "system" fn send_request_to_join(_h: usize, o: *const LocalTargetOptions, cd: usize, cb: usize) {
    let (local, target) = local_target(o);
    let r = match ids::peer_of_puid(target) {
        Some(peer) if send_to(peer, RTJ) => {
            S.lock().unwrap().ask(peer, Instant::now());
            SUCCESS
        }
        Some(_) => NOT_FOUND,
        None => INVALID_PARAMETERS,
    };
    trace("EOS_CustomInvites_SendRequestToJoin", || format!("result={r}"));
    complete(cb, cd, r, local, target);
}

/// Answers a request to join we received; false if there was none from that player.
fn answer(peer: PeerId, accept: bool) -> bool {
    if !S.lock().unwrap().requests.remove(&peer) {
        return false;
    }
    send_to(peer, if accept { RTJ_ACCEPT } else { RTJ_REJECT })
}

unsafe extern "system" fn accept_request_to_join(_h: usize, o: *const LocalTargetOptions, cd: usize, cb: usize) {
    let (local, target) = local_target(o);
    let r = match ids::peer_of_puid(target) {
        // Already answered (accepted at once on arrival) is still a success.
        Some(peer) => {
            answer(peer, true);
            SUCCESS
        }
        None => INVALID_PARAMETERS,
    };
    complete(cb, cd, r, local, target);
}

unsafe extern "system" fn reject_request_to_join(_h: usize, o: *const LocalTargetOptions, cd: usize, cb: usize) {
    let (local, target) = local_target(o);
    let r = match ids::peer_of_puid(target) {
        Some(peer) => {
            answer(peer, false);
            SUCCESS
        }
        None => INVALID_PARAMETERS,
    };
    complete(cb, cd, r, local, target);
}

unsafe extern "system" fn disable_request_to_join(_h: usize, _o: usize) -> i32 {
    S.lock().unwrap().rtj_disabled = true;
    SUCCESS
}

unsafe extern "system" fn notify_received(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::CustomInviteReceived, f, cd, None)
}
unsafe extern "system" fn notify_accepted(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::CustomInviteAccepted, f, cd, None)
}
unsafe extern "system" fn notify_rejected(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::CustomInviteRejected, f, cd, None)
}
unsafe extern "system" fn notify_rtj_received(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::RequestToJoinReceived, f, cd, None)
}
unsafe extern "system" fn notify_rtj_response(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::RequestToJoinResponse, f, cd, None)
}
unsafe extern "system" fn notify_rtj_accepted(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::RequestToJoinAccepted, f, cd, None)
}
unsafe extern "system" fn notify_rtj_rejected(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::RequestToJoinRejected, f, cd, None)
}
unsafe extern "system" fn notify_never(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::Never, f, cd, None)
}
unsafe extern "system" fn remove_notify(_h: usize, id: u64) {
    remove_listener(id);
}

/// A custom invite arrived: Received, then Accepted when the player accepts it (at once
/// with auto-accept on; see invite.rs).
fn receive_invite(from: PeerId, connect: &str, payload: &str) {
    let Some(me) = me() else { return };
    // Games size their buffers by the SDK's limit; a sender can't go over it.
    if payload.len() > MAX_PAYLOAD {
        trace("CustomInviteReceived", || format!("from {from}: dropped, a payload of {} bytes", payload.len()));
        return;
    }
    let decision = crate::invite::decision(from, connect, None);
    let id = S.lock().unwrap().keep_received(from, payload);
    trace("CustomInviteReceived", || format!("{id} from {from}: {payload:?}"));
    let info = |cd| CustomInviteInfo {
        ClientData: cd,
        TargetUserId: ids::puid(from),
        LocalUserId: ids::puid(me),
        CustomInviteId: intern(&id),
        Payload: intern(payload),
    };
    if decision.received() {
        for (f, cd) in listeners(Kind::CustomInviteReceived, None) {
            unsafe { call(f, &info(cd)) };
        }
    }
    if !decision.accept() {
        return;
    }
    for (f, cd) in listeners(Kind::CustomInviteAccepted, None) {
        unsafe { call(f, &info(cd)) };
    }
}

/// A request to join arrived. When the player accepts it (at once with auto-accept on),
/// the requester hears it was accepted and a game listening for the overlay's acceptance
/// gets it; otherwise the game's own Accept / RejectRequestToJoin answers.
fn receive_request(from: PeerId, connect: &str) {
    let decision = crate::invite::decision(from, connect, None);
    let Some(me) = me() else { return };
    if S.lock().unwrap().rtj_disabled {
        trace("RequestToJoinReceived", || format!("from {from}: ignored, the game disabled requests to join"));
        return;
    }
    S.lock().unwrap().requests.insert(from);
    trace("RequestToJoinReceived", || format!("from {from}"));
    for (f, cd) in listeners(Kind::RequestToJoinReceived, None).into_iter().filter(|_| decision.received()) {
        let info = RequestToJoinReceivedInfo {
            ClientData: cd,
            FromUserId: ids::puid(from),
            ToUserId: ids::puid(me),
        };
        unsafe { call(f, &info) };
    }
    if !decision.accept() {
        return;
    }
    answer(from, true);
    let overlay = listeners(Kind::RequestToJoinAccepted, None);
    for (f, cd) in overlay {
        let info = RequestToJoinOverlayInfo {
            ClientData: cd,
            TargetUserId: ids::puid(from),
            LocalUserId: ids::puid(me),
        };
        unsafe { call(f, &info) };
    }
}

fn receive_response(from: PeerId, accepted: bool) {
    let Some(me) = me() else { return };
    if !S.lock().unwrap().answered_by(from, Instant::now()) {
        trace("RequestToJoinResponseReceived", || format!("from {from}: dropped, we didn't ask them"));
        return;
    }
    trace("RequestToJoinResponseReceived", || format!("from {from}: accepted={accepted}"));
    for (f, cd) in listeners(Kind::RequestToJoinResponse, None) {
        let info = RequestToJoinResponseInfo {
            ClientData: cd,
            FromUserId: ids::puid(from),
            ToUserId: ids::puid(me),
            Response: if accepted { RTJR_ACCEPTED } else { RTJR_REJECTED },
        };
        unsafe { call(f, &info) };
    }
}

/// What a LAN invite's connect string asks for, if it is one of ours.
#[derive(Debug, PartialEq, Eq)]
enum Message<'a> {
    Invite(&'a str),
    RequestToJoin,
    Response(bool),
}

fn parse(connect: &str) -> Option<Message<'_>> {
    if let Some(p) = connect.strip_prefix(INVITE) {
        return Some(Message::Invite(p));
    }
    match connect {
        RTJ => Some(Message::RequestToJoin),
        RTJ_ACCEPT => Some(Message::Response(true)),
        RTJ_REJECT => Some(Message::Response(false)),
        _ => None,
    }
}

pub fn on_event(_lan: &Lan, ev: &Event) {
    let Event::Invite { from, connect, lobby: None } = ev else { return };
    match parse(connect) {
        Some(Message::Invite(p)) => receive_invite(*from, connect, p),
        Some(Message::RequestToJoin) => receive_request(*from, connect),
        Some(Message::Response(a)) => receive_response(*from, a),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn messages_round_trip_and_stay_ours() {
        assert_eq!(parse("peerparty:eosci:invite:{\"lobby\":\"x\"}"), Some(Message::Invite("{\"lobby\":\"x\"}")));
        assert_eq!(parse("peerparty:eosci:invite:"), Some(Message::Invite("")));
        assert_eq!(parse(RTJ), Some(Message::RequestToJoin));
        assert_eq!(parse(RTJ_ACCEPT), Some(Message::Response(true)));
        assert_eq!(parse(RTJ_REJECT), Some(Message::Response(false)));
        assert_eq!(parse("+connect_lobby 123"), None);
        assert!(is_ours(RTJ) && is_ours(INVITE) && !is_ours("lobby=abc"));
    }

    /// Only a player we asked to join answers, once, and not after `ASKED_FOR`.
    #[test]
    fn only_players_we_asked_answer_a_request_to_join() {
        let mut s = State::default();
        let (a, b) = (PeerId([1; 16]), PeerId([2; 16]));
        let t0 = Instant::now();
        assert!(!s.answered_by(a, t0), "nobody asked");
        s.ask(a, t0);
        assert!(!s.answered_by(b, t0), "someone else");
        assert!(s.answered_by(a, t0 + Duration::from_secs(5)));
        assert!(!s.answered_by(a, t0 + Duration::from_secs(6)), "used up");
        s.ask(a, t0);
        assert!(!s.answered_by(a, t0 + ASKED_FOR), "too late");
        for n in 0..200u8 {
            s.ask(PeerId([n; 16]), t0 + Duration::from_millis(u64::from(n)));
        }
        assert_eq!(s.asked.len(), MAX_ASKED);
        assert!(s.asked.contains_key(&PeerId([199; 16])), "the newest are kept");
    }

    #[test]
    fn received_invites_are_bounded() {
        let mut s = State::default();
        let from = PeerId([3; 16]);
        let ids: Vec<String> = (0..100).map(|_| s.keep_received(from, "{}")).collect();
        assert_eq!(s.received.len(), MAX_RECEIVED);
        assert!(!s.received.contains_key(&ids[0]), "the oldest went");
        assert!(s.received.contains_key(&ids[99]));
        assert_eq!(invite_number(&ids[99]), 100);
    }

    /// Offsets on x64 per eos_custominvites_types.h (pack 8).
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn structs_match_the_sdk() {
        assert_eq!(offset_of!(SendCustomInviteOptions, TargetUserIds), 16);
        assert_eq!(offset_of!(SendCustomInviteOptions, TargetUserIdsCount), 24);
        assert_eq!(size_of::<SendCustomInviteInfo>(), 40);
        assert_eq!(offset_of!(CustomInviteInfo, Payload), 32);
        assert_eq!(offset_of!(FinalizeInviteOptions, ProcessingResult), 32);
        assert_eq!(offset_of!(RequestToJoinResponseInfo, Response), 24);
        assert_eq!(offset_of!(LocalTargetOptions, TargetUserId), 16);
    }
}
