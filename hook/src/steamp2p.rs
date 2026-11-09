//! ISteamNetworking's P2P packets on LAN channels (socket "steam:p2p", the game's
//! channel number as the channel), and the ISteamUser methods two LAN players need from
//! each other: the local SteamID, and auth checks that pass between LAN players (a real
//! Steam ticket from a LAN SteamID would fail).
//!
//! Sessions follow Steam: sending to a player opens the session from our side, and a
//! player sending to us first is a `P2PSessionRequest_t` until the game accepts it.
//! Unreliable sends ride the same connection; on a LAN they arrive anyway.
//!
//! A game server has its own ISteamNetworking (`SteamGameServerNetworking()`). Packets
//! a client sends to a game server's SteamID go to its host on socket "steam:p2p:gs",
//! where only the server's interface reads them; the server's replies come back on
//! "steam:p2p:gc", and the client sees them from the server's SteamID. So a listen
//! server's own client and its server never read each other's packets.

use peerparty_core::{CloseReason, Event, Lan, SendResult};

use crate::log;
use crate::net;
use crate::steamlan::{bytes, my_steam_id, peer_of, steam_id};
use crate::steamobj::{call_real, vmethod};

const SOCKET: &str = "steam:p2p";
/// Client to game server, and game server to client.
const TO_SERVER: &str = "steam:p2p:gs";
const TO_CLIENT: &str = "steam:p2p:gc";

/// Game server hosts this client has sent to: their replies need no accepting, as on
/// Steam, where the session the client opened carries both ways.
static CONTACTED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<peerparty_core::PeerId>>> =
    std::sync::LazyLock::new(Default::default);

const P2P_SESSION_REQUEST: i32 = 1202;
const P2P_SESSION_CONNECT_FAIL: i32 = 1203;
const VALIDATE_AUTH_TICKET_RESPONSE: i32 = 143;
// EP2PSessionError
const P2P_TIMEOUT: u8 = 4;

#[repr(C)]
struct SessionRequest {
    remote: u64,
}

#[repr(C)]
struct ConnectFail {
    remote: u64,
    error: u8,
}

#[repr(C)]
struct SessionState {
    active: u8,
    connecting: u8,
    error: u8,
    relay: u8,
    bytes_queued: i32,
    packets_queued: i32,
    ip: u32,
    port: u16,
}

#[repr(C)]
struct AuthResponse {
    steam_id: u64,
    response: i32,
    owner: u64,
}

fn lan() -> Option<&'static Lan> {
    net::lan()
}

fn channel(c: i32) -> u8 {
    c as u8
}

/// What a client's ISteamNetworking reads: other players, and game servers' replies.
fn is_steam_p2p(s: &str) -> bool {
    s == SOCKET || s == TO_CLIENT
}

fn is_to_server(s: &str) -> bool {
    s == TO_SERVER
}

/// Where a client's packet for `remote` goes: (host, socket).
fn route(remote: u64) -> Option<(peerparty_core::PeerId, &'static str)> {
    if crate::steamgs::is_server(remote) {
        return Some((crate::steamgs::host_of(remote)?, TO_SERVER));
    }
    Some((peer_of(remote)?, SOCKET))
}

/// Who sent a packet a client read: a game server for its replies, else the player.
fn sender(p: &peerparty_core::Packet) -> u64 {
    if p.socket == TO_CLIENT {
        if let Some(id) = crate::steamgs::server_id_of(p.peer) {
            return id;
        }
    }
    steam_id(p.peer)
}

// ---- ISteamNetworking ----

pub fn networking() -> Vec<(&'static str, usize)> {
    vec![
        ("SendP2PPacket", send_p2p_packet as *const () as usize),
        ("IsP2PPacketAvailable", is_p2p_packet_available as *const () as usize),
        ("ReadP2PPacket", read_p2p_packet as *const () as usize),
        ("AcceptP2PSessionWithUser", accept_session as *const () as usize),
        ("CloseP2PSessionWithUser", close_session as *const () as usize),
        ("CloseP2PChannelWithUser", close_channel as *const () as usize),
        ("GetP2PSessionState", get_session_state as *const () as usize),
        ("AllowP2PPacketRelay", allow_relay as *const () as usize),
    ]
}

vmethod! {
    fn send_p2p_packet(_this: usize, remote: u64, data: *const u8, len: u32, kind: i32, chan: i32) -> bool {
        let (Some(lan), Some((peer, socket))) = (lan(), route(remote)) else {
            crate::log::throttled(&format!("steam-lan p2p to unknown {remote}"), 2, |_| {
                log!("steam-lan SendP2PPacket to {remote}, who isn't on the LAN")
            });
            return false;
        };
        if data.is_null() && len > 0 {
            return false;
        }
        let body = if len == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(data, len as usize) } };
        // EP2PSend: 0 unreliable, 1 unreliable no delay, 2 reliable, 3 reliable buffered.
        let reliable = kind >= 2;
        if socket == TO_SERVER {
            CONTACTED.lock().unwrap().insert(peer);
        }
        let body = crate::steamlan::ids_out(peer, body);
        let r = lan.send(peer, socket, channel(chan), &body, reliable, true, true);
        crate::log::throttled("steam-lan SendP2PPacket", 4, |n| {
            log!("steam-lan SendP2PPacket n={n} to {remote} {len} bytes ch={chan} -> {r:?}")
        });
        !matches!(r, SendResult::Dropped | SendResult::NoChannel)
    }
}

vmethod! {
    fn is_p2p_packet_available(_this: usize, size: *mut u32, chan: i32) -> bool {
        let Some(n) = lan().and_then(|l| l.next_packet_size_where(Some(channel(chan)), is_steam_p2p)) else {
            return false;
        };
        if !size.is_null() {
            unsafe { *size = n as u32 };
        }
        true
    }
}

vmethod! {
    fn read_p2p_packet(_this: usize, dest: *mut u8, cap: u32, size: *mut u32, remote: *mut u64, chan: i32) -> bool {
        let Some(mut p) = lan().and_then(|l| l.receive_where(Some(channel(chan)), is_steam_p2p)) else {
            return false;
        };
        crate::steamlan::ids_in(p.peer, &mut p.data);
        // A short buffer truncates; the packet is consumed either way.
        let n = p.data.len().min(cap as usize);
        unsafe {
            if n > 0 && !dest.is_null() {
                std::ptr::copy_nonoverlapping(p.data.as_ptr(), dest, n);
            }
            if !size.is_null() {
                *size = n as u32;
            }
            if !remote.is_null() {
                *remote = sender(&p);
            }
        }
        true
    }
}

vmethod! {
    fn accept_session(_this: usize, remote: u64) -> bool {
        let socket = if crate::steamgs::is_server(remote) { TO_CLIENT } else { SOCKET };
        match (lan(), route(remote)) {
            (Some(lan), Some((p, _))) => {
                lan.accept(p, socket);
                log!("steam-lan P2P session with {remote} accepted");
                true
            }
            _ => false,
        }
    }
}

vmethod! {
    fn close_session(_this: usize, remote: u64) -> bool {
        match (lan(), route(remote)) {
            (Some(lan), Some((p, socket))) => {
                lan.close(p, Some(socket));
                if socket == TO_SERVER {
                    lan.close(p, Some(TO_CLIENT));
                }
                true
            }
            _ => false,
        }
    }
}

vmethod! { fn close_channel(_this: usize, remote: u64, _chan: i32) -> bool { route(remote).is_some() } }

vmethod! {
    fn get_session_state(_this: usize, remote: u64, out: *mut SessionState) -> bool {
        if route(remote).is_none() {
            return false;
        }
        if !out.is_null() {
            unsafe {
                out.write(SessionState {
                    active: 1,
                    connecting: 0,
                    error: 0,
                    relay: 0,
                    bytes_queued: 0,
                    packets_queued: 0,
                    ip: 0,
                    port: 0,
                })
            };
        }
        true
    }
}

vmethod! { fn allow_relay(_this: usize, _allow: bool) -> bool { true } }

// ---- the game server's ISteamNetworking ----

pub fn server_networking() -> Vec<(&'static str, usize)> {
    vec![
        ("SendP2PPacket", gs_send as *const () as usize),
        ("IsP2PPacketAvailable", gs_available as *const () as usize),
        ("ReadP2PPacket", gs_read as *const () as usize),
        ("AcceptP2PSessionWithUser", gs_accept as *const () as usize),
        ("CloseP2PSessionWithUser", gs_close as *const () as usize),
        ("CloseP2PChannelWithUser", close_channel as *const () as usize),
        ("GetP2PSessionState", get_session_state as *const () as usize),
        ("AllowP2PPacketRelay", allow_relay as *const () as usize),
    ]
}

vmethod! {
    fn gs_send(_this: usize, remote: u64, data: *const u8, len: u32, kind: i32, chan: i32) -> bool {
        let (Some(lan), Some(peer)) = (lan(), peer_of(remote)) else {
            return false;
        };
        if data.is_null() && len > 0 {
            return false;
        }
        let body = if len == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(data, len as usize) } };
        let body = crate::steamlan::ids_out(peer, body);
        let r = lan.send(peer, TO_CLIENT, channel(chan), &body, kind >= 2, true, true);
        crate::log::throttled("steam-lan server SendP2PPacket", 4, |n| {
            log!("steam-lan game server SendP2PPacket n={n} to {remote} {len} bytes ch={chan} -> {r:?}")
        });
        !matches!(r, SendResult::Dropped | SendResult::NoChannel)
    }
}

vmethod! {
    fn gs_available(_this: usize, size: *mut u32, chan: i32) -> bool {
        let Some(n) = lan().and_then(|l| l.next_packet_size_where(Some(channel(chan)), is_to_server)) else {
            return false;
        };
        if !size.is_null() {
            unsafe { *size = n as u32 };
        }
        true
    }
}

vmethod! {
    fn gs_read(_this: usize, dest: *mut u8, cap: u32, size: *mut u32, remote: *mut u64, chan: i32) -> bool {
        let Some(mut p) = lan().and_then(|l| l.receive_where(Some(channel(chan)), is_to_server)) else {
            return false;
        };
        crate::steamlan::ids_in(p.peer, &mut p.data);
        let n = p.data.len().min(cap as usize);
        unsafe {
            if n > 0 && !dest.is_null() {
                std::ptr::copy_nonoverlapping(p.data.as_ptr(), dest, n);
            }
            if !size.is_null() {
                *size = n as u32;
            }
            if !remote.is_null() {
                *remote = steam_id(p.peer);
            }
        }
        true
    }
}

vmethod! {
    fn gs_accept(_this: usize, remote: u64) -> bool {
        match (lan(), peer_of(remote)) {
            (Some(lan), Some(p)) => {
                lan.accept(p, TO_SERVER);
                log!("steam-lan game server P2P session with {remote} accepted");
                true
            }
            _ => false,
        }
    }
}

vmethod! {
    fn gs_close(_this: usize, remote: u64) -> bool {
        match (lan(), peer_of(remote)) {
            (Some(lan), Some(p)) => {
                lan.close(p, Some(TO_SERVER));
                lan.close(p, Some(TO_CLIENT));
                true
            }
            _ => false,
        }
    }
}

// ---- ISteamUser ----

pub fn user() -> Vec<(&'static str, usize)> {
    vec![
        ("GetSteamID", get_steam_id as *const () as usize),
        ("GetAuthSessionTicket", get_auth_session_ticket as *const () as usize),
        ("GetAuthTicketForWebApi", get_auth_ticket_for_web_api as *const () as usize),
        ("BLoggedOn", logged_on as *const () as usize),
        ("BeginAuthSession", begin_auth_session as *const () as usize),
        ("EndAuthSession", end_auth_session as *const () as usize),
        ("UserHasLicenseForApp", user_has_license as *const () as usize),
    ]
}

vmethod! {
    /// Returns a CSteamID: through the hidden pointer MSVC passes after `this`.
    fn get_steam_id(this: usize, ret: *mut u64) -> *mut u64 {
        let id = my_steam_id();
        let id = if id != 0 {
            id
        } else {
            let mut real = 0u64;
            let _ = call_real!(this, "GetSteamID", fn(*mut u64) -> *mut u64, std::ptr::null_mut(), &mut real);
            real
        };
        crate::log::throttled("steam-user GetSteamID", 3, |_| log!("steam-user GetSteamID answers {id}"));
        unsafe { *ret = id };
        ret
    }
}

vmethod! {
    fn get_auth_session_ticket(
        this: usize,
        ticket: *mut u8,
        max: i32,
        len: *mut u32,
        identity: *const core::ffi::c_void,
    ) -> u32 {
        let h = call_real!(
            this,
            "GetAuthSessionTicket",
            fn(*mut u8, i32, *mut u32, *const core::ffi::c_void) -> u32,
            0,
            ticket,
            max,
            len,
            identity
        );
        crate::steamlan::auth_ticket_issued(h)
    }
}

vmethod! {
    fn get_auth_ticket_for_web_api(this: usize, identity: *const core::ffi::c_char) -> u32 {
        let h = call_real!(
            this,
            "GetAuthTicketForWebApi",
            fn(*const core::ffi::c_char) -> u32,
            0,
            identity
        );
        crate::steamlan::web_ticket_issued(h)
    }
}

vmethod! {
    /// Reports signed in (see steamlan's `logged_on`).
    fn logged_on(_this: usize) -> bool {
        true
    }
}

vmethod! {
    fn begin_auth_session(this: usize, ticket: *const u8, len: i32, user: u64) -> i32 {
        if peer_of(user).is_none() {
            return call_real!(this, "BeginAuthSession", fn(*const u8, i32, u64) -> i32, 1, ticket, len, user);
        }
        // k_EBeginAuthSessionResultOK, then k_EAuthSessionResponseOK.
        // Sign-in between PeerParty players (LEGAL.md, "Sign-in is answered locally"): OK
        // for the player at that address, named as their own owner. Steam isn't asked.
        crate::steam::inject(
            VALIDATE_AUTH_TICKET_RESPONSE,
            bytes(&AuthResponse { steam_id: user, response: 0, owner: user }),
        );
        0
    }
}

vmethod! {
    fn end_auth_session(this: usize, user: u64) {
        if peer_of(user).is_none() {
            call_real!(this, "EndAuthSession", fn(u64) -> (), (), user);
        }
    }
}

vmethod! {
    /// steam_api answers; PeerParty never says "has a licence" itself.
    /// With nothing to ask, or an answer that isn't one of EUserHasLicenseForAppResult's
    /// three, it's k_EUserHasLicenseResultNoAuth (2): can't verify.
    fn user_has_license(this: usize, user: u64, app: u32) -> i32 {
        let r = call_real!(this, "UserHasLicenseForApp", fn(u64, u32) -> i32, 2, user, app);
        if (0..=2).contains(&r) { r } else { 2 }
    }
}

// ---- LAN events -> Steam callbacks ----

pub fn on_event(ev: &Event) {
    match ev {
        Event::ChanRequest { peer, socket } if socket == TO_CLIENT && CONTACTED.lock().unwrap().contains(peer) => {
            if let Some(lan) = lan() {
                lan.accept(*peer, TO_CLIENT);
            }
        }
        Event::ChanRequest { peer, socket } if is_steam_p2p(socket) => {
            let remote = if socket == TO_CLIENT {
                crate::steamgs::server_id_of(*peer).unwrap_or_else(|| steam_id(*peer))
            } else {
                steam_id(*peer)
            };
            log!("steam-lan P2P session request from {remote}");
            crate::steam::inject(P2P_SESSION_REQUEST, bytes(&SessionRequest { remote }));
        }
        Event::ChanRequest { peer, socket } if is_to_server(socket) => {
            let remote = steam_id(*peer);
            log!("steam-lan game server P2P session request from {remote}");
            crate::steam::inject_server(P2P_SESSION_REQUEST, bytes(&SessionRequest { remote }));
        }
        Event::ChanClosed {
            peer,
            socket,
            reason: CloseReason::Lost,
        } if is_steam_p2p(socket) || is_to_server(socket) => {
            let fail = bytes(&ConnectFail {
                remote: steam_id(*peer),
                error: P2P_TIMEOUT,
            });
            if is_to_server(socket) {
                crate::steam::inject_server(P2P_SESSION_CONNECT_FAIL, fail);
            } else {
                crate::steam::inject(P2P_SESSION_CONNECT_FAIL, fail);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structs_have_steams_layout() {
        assert_eq!(size_of::<SessionRequest>(), 8);
        assert_eq!(size_of::<ConnectFail>(), 16);
        assert_eq!(size_of::<SessionState>(), 20);
        assert_eq!(size_of::<AuthResponse>(), 24);
    }
}
