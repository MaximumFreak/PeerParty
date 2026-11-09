//! EOS P2P on the LAN core's channels. A connection is (remote user, socket name); it
//! opens once both sides agree, and the notifications fire as EOS fires them: a request
//! when the peer asks first, established when both agree, closed when either side or
//! the network ends it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use peerparty_core::{CloseReason, Event, Lan, PeerId, SendResult};

use super::abi::{self, result::*};
use super::ids;
use super::{Kind, add_listener, call, lan, later, listeners, remove_listener, trace};

static NAT_QUERIED: AtomicBool = AtomicBool::new(false);
static RELAY: AtomicI32 = AtomicI32::new(1);
static PORTS: Mutex<(u16, u16)> = Mutex::new((7777, 99));
static QUEUE_LIMITS: Mutex<(u64, u64)> = Mutex::new((0, 0));

pub fn resolve(name: &str) -> Option<usize> {
    Some(match name {
        "EOS_P2P_SendPacket" => send_packet as *const () as usize,
        "EOS_P2P_ReceivePacket" => receive_packet as *const () as usize,
        "EOS_P2P_GetNextReceivedPacketSize" => next_packet_size as *const () as usize,
        "EOS_P2P_AcceptConnection" => accept_connection as *const () as usize,
        "EOS_P2P_CloseConnection" => close_connection as *const () as usize,
        "EOS_P2P_CloseConnections" => close_connections as *const () as usize,
        "EOS_P2P_AddNotifyPeerConnectionRequest" => notify_request as *const () as usize,
        "EOS_P2P_AddNotifyPeerConnectionEstablished" => notify_established as *const () as usize,
        "EOS_P2P_AddNotifyPeerConnectionClosed" => notify_closed as *const () as usize,
        "EOS_P2P_AddNotifyPeerConnectionInterrupted" => notify_interrupted as *const () as usize,
        "EOS_P2P_AddNotifyIncomingPacketQueueFull" => notify_never as *const () as usize,
        n if n.starts_with("EOS_P2P_RemoveNotify") => remove_notify as *const () as usize,
        "EOS_P2P_QueryNATType" => query_nat as *const () as usize,
        "EOS_P2P_GetNATType" => get_nat as *const () as usize,
        "EOS_P2P_SetRelayControl" => set_relay as *const () as usize,
        "EOS_P2P_GetRelayControl" => get_relay as *const () as usize,
        "EOS_P2P_SetPortRange" => set_ports as *const () as usize,
        "EOS_P2P_GetPortRange" => get_ports as *const () as usize,
        "EOS_P2P_SetPacketQueueSize" => set_queue_size as *const () as usize,
        "EOS_P2P_GetPacketQueueInfo" => queue_info as *const () as usize,
        "EOS_P2P_ClearPacketQueue" => clear_queue as *const () as usize,
        _ => return None,
    })
}

/// A socket name: 1-32 characters before the NUL.
unsafe fn socket_name(s: *const abi::SocketId) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let raw = unsafe { &(*s).SocketName };
    let end = raw.iter().position(|&b| b == 0)?;
    (end > 0).then(|| String::from_utf8_lossy(&raw[..end]).into_owned())
}

fn socket_id(name: &str) -> abi::SocketId {
    let mut s = abi::SocketId {
        ApiVersion: abi::P2P_SOCKETID_API,
        SocketName: [0; abi::SOCKET_NAME_SIZE],
    };
    let n = name.len().min(abi::SOCKET_NAME_SIZE - 1);
    s.SocketName[..n].copy_from_slice(&name.as_bytes()[..n]);
    s
}

fn is_me(local: usize) -> bool {
    lan().is_some_and(|l| ids::peer_of_puid(local) == Some(l.me()))
}

unsafe extern "system" fn send_packet(_h: usize, o: *const abi::SendPacketOptions) -> i32 {
    let Some(lan) = lan() else {
        return NO_CONNECTION;
    };
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let o = unsafe { &*o };
    let Some(socket) = (unsafe { socket_name(o.SocketId) }) else {
        return INVALID_PARAMETERS;
    };
    if o.DataLengthBytes > 0 && o.Data.is_null() {
        return INVALID_PARAMETERS;
    }
    let Some(remote) = ids::peer_of_puid(o.RemoteUserId) else {
        return INVALID_USER;
    };
    if !is_me(o.LocalUserId) {
        return INVALID_USER;
    }
    if o.DataLengthBytes > abi::P2P_MAX_PACKET_SIZE {
        return LIMIT_EXCEEDED;
    }
    let data = if o.DataLengthBytes == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(o.Data, o.DataLengthBytes as usize) }
    };
    // A game from before the reliability field never asked to lose packets.
    let reliable = o.ApiVersion < abi::SENDPACKET_RELIABILITY || o.Reliability != 0;
    let auto_accept =
        o.ApiVersion < abi::SENDPACKET_DISABLE_AUTO_ACCEPT || o.bDisableAutoAcceptConnection == 0;
    let r = lan.send(
        remote,
        &socket,
        o.Channel,
        data,
        reliable,
        o.bAllowDelayedDelivery != 0,
        auto_accept,
    );
    trace("EOS_P2P_SendPacket", || {
        format!("{remote} {socket}/{} {}B {r:?}", o.Channel, data.len())
    });
    match r {
        SendResult::NoChannel => NO_CONNECTION,
        _ => SUCCESS,
    }
}

unsafe fn requested_channel(version: i32, cutoff: i32, p: *const u8) -> Option<u8> {
    (version >= cutoff && !p.is_null()).then(|| unsafe { *p })
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn receive_packet(
    _h: usize,
    o: *const abi::ReceivePacketOptions,
    out_peer: *mut usize,
    out_socket: *mut abi::SocketId,
    out_channel: *mut u8,
    out_data: *mut u8,
    out_bytes: *mut u32,
) -> i32 {
    let Some(lan) = lan() else {
        return NO_CONNECTION;
    };
    if o.is_null()
        || out_peer.is_null()
        || out_socket.is_null()
        || out_channel.is_null()
        || out_bytes.is_null()
    {
        return INVALID_PARAMETERS;
    }
    let o = unsafe { &*o };
    let channel = unsafe {
        requested_channel(
            o.ApiVersion,
            abi::RECEIVEPACKET_REQUESTED_CHANNEL,
            o.RequestedChannel,
        )
    };
    let Some(p) = lan.receive_where(channel, is_eos_socket) else {
        return NOT_FOUND;
    };
    // A short buffer truncates; the packet is consumed either way.
    let n = p.data.len().min(o.MaxDataSizeBytes as usize);
    unsafe {
        *out_peer = ids::puid(p.peer);
        out_socket.write(socket_id(&p.socket));
        *out_channel = p.channel;
        if n > 0 && !out_data.is_null() {
            std::ptr::copy_nonoverlapping(p.data.as_ptr(), out_data, n);
        }
        *out_bytes = n as u32;
    }
    SUCCESS
}

unsafe extern "system" fn next_packet_size(
    _h: usize,
    o: *const abi::PacketSizeOptions,
    out: *mut u32,
) -> i32 {
    let Some(lan) = lan() else {
        return NO_CONNECTION;
    };
    if o.is_null() || out.is_null() {
        return INVALID_PARAMETERS;
    }
    let o = unsafe { &*o };
    let channel = unsafe {
        requested_channel(
            o.ApiVersion,
            abi::PACKETSIZE_REQUESTED_CHANNEL,
            o.RequestedChannel,
        )
    };
    match lan.next_packet_size_where(channel, is_eos_socket) {
        Some(n) => {
            unsafe { *out = n as u32 };
            SUCCESS
        }
        None => NOT_FOUND,
    }
}

/// EOS socket names are alphanumeric; other front ends' sockets have a ':' in them.
fn is_eos_socket(name: &str) -> bool {
    !name.contains(':')
}

unsafe fn peer_socket(o: *const abi::PeerSocketOptions) -> Option<(PeerId, Option<String>)> {
    if o.is_null() || !is_me(unsafe { (*o).LocalUserId }) {
        return None;
    }
    let peer = ids::peer_of_puid(unsafe { (*o).RemoteUserId })?;
    Some((peer, unsafe { socket_name((*o).SocketId) }))
}

unsafe extern "system" fn accept_connection(_h: usize, o: *const abi::PeerSocketOptions) -> i32 {
    let (Some(lan), Some((peer, Some(socket)))) = (lan(), unsafe { peer_socket(o) }) else {
        return INVALID_PARAMETERS;
    };
    trace("EOS_P2P_AcceptConnection", || format!("{peer} {socket}"));
    lan.accept(peer, &socket);
    SUCCESS
}

unsafe extern "system" fn close_connection(_h: usize, o: *const abi::PeerSocketOptions) -> i32 {
    let (Some(lan), Some((peer, socket))) = (lan(), unsafe { peer_socket(o) }) else {
        return INVALID_PARAMETERS;
    };
    lan.close(peer, socket.as_deref());
    SUCCESS
}

unsafe extern "system" fn close_connections(_h: usize, o: *const abi::LocalSocketOptions) -> i32 {
    let Some(lan) = lan() else {
        return NO_CONNECTION;
    };
    if o.is_null() || !is_me(unsafe { (*o).LocalUserId }) {
        return INVALID_USER;
    }
    let Some(socket) = (unsafe { socket_name((*o).SocketId) }) else {
        return INVALID_PARAMETERS;
    };
    lan.close_socket(&socket);
    SUCCESS
}

unsafe fn notify(kind: Kind, o: *const abi::LocalSocketOptions, cd: usize, f: usize) -> u64 {
    let socket = if o.is_null() {
        None
    } else {
        unsafe { socket_name((*o).SocketId) }
    };
    add_listener(kind, f, cd, socket)
}

unsafe extern "system" fn notify_request(
    _h: usize,
    o: *const abi::LocalSocketOptions,
    cd: usize,
    f: usize,
) -> u64 {
    unsafe { notify(Kind::P2PRequest, o, cd, f) }
}

unsafe extern "system" fn notify_established(
    _h: usize,
    o: *const abi::LocalSocketOptions,
    cd: usize,
    f: usize,
) -> u64 {
    unsafe { notify(Kind::P2PEstablished, o, cd, f) }
}

unsafe extern "system" fn notify_closed(
    _h: usize,
    o: *const abi::LocalSocketOptions,
    cd: usize,
    f: usize,
) -> u64 {
    unsafe { notify(Kind::P2PClosed, o, cd, f) }
}

unsafe extern "system" fn notify_interrupted(
    _h: usize,
    o: *const abi::LocalSocketOptions,
    cd: usize,
    f: usize,
) -> u64 {
    unsafe { notify(Kind::P2PInterrupted, o, cd, f) }
}

unsafe extern "system" fn notify_never(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::Never, f, cd, None)
}

unsafe extern "system" fn remove_notify(_h: usize, id: u64) {
    remove_listener(id);
}

/// A LAN peer is always reached directly.
unsafe extern "system" fn query_nat(_h: usize, _o: usize, cd: usize, cb: usize) {
    NAT_QUERIED.store(true, Ordering::Relaxed);
    later(move || {
        let info = abi::NatTypeInfo {
            ResultCode: SUCCESS,
            ClientData: cd,
            // EOS_NAT_Open
            NATType: 1,
        };
        unsafe { call(cb, &info) };
    });
}

unsafe extern "system" fn get_nat(_h: usize, _o: usize, out: *mut i32) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    if !NAT_QUERIED.load(Ordering::Relaxed) {
        return NOT_FOUND;
    }
    unsafe { *out = 1 };
    SUCCESS
}

unsafe extern "system" fn set_relay(_h: usize, o: *const abi::I32Options) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    RELAY.store(unsafe { (*o).Value }, Ordering::Relaxed);
    SUCCESS
}

unsafe extern "system" fn get_relay(_h: usize, _o: usize, out: *mut i32) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = RELAY.load(Ordering::Relaxed) };
    SUCCESS
}

unsafe extern "system" fn set_ports(_h: usize, o: *const abi::PortRangeOptions) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    *PORTS.lock().unwrap() = unsafe { ((*o).Port, (*o).MaxAdditionalPortsToTry) };
    SUCCESS
}

unsafe extern "system" fn get_ports(_h: usize, _o: usize, port: *mut u16, extra: *mut u16) -> i32 {
    if port.is_null() || extra.is_null() {
        return INVALID_PARAMETERS;
    }
    let (p, n) = *PORTS.lock().unwrap();
    unsafe {
        *port = p;
        *extra = n;
    }
    SUCCESS
}

unsafe extern "system" fn set_queue_size(_h: usize, o: *const abi::PacketQueueSizeOptions) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    *QUEUE_LIMITS.lock().unwrap() = unsafe {
        (
            (*o).IncomingPacketQueueMaxSizeBytes,
            (*o).OutgoingPacketQueueMaxSizeBytes,
        )
    };
    SUCCESS
}

unsafe extern "system" fn queue_info(_h: usize, _o: usize, out: *mut abi::PacketQueueInfo) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    let (count, bytes) = lan().map_or((0, 0), |l| l.inbox_size());
    let (max_in, max_out) = *QUEUE_LIMITS.lock().unwrap();
    unsafe {
        out.write(abi::PacketQueueInfo {
            IncomingPacketQueueMaxSizeBytes: max_in,
            IncomingPacketQueueCurrentSizeBytes: bytes,
            IncomingPacketQueueCurrentPacketCount: count,
            OutgoingPacketQueueMaxSizeBytes: max_out,
            OutgoingPacketQueueCurrentSizeBytes: 0,
            OutgoingPacketQueueCurrentPacketCount: 0,
        })
    };
    SUCCESS
}

unsafe extern "system" fn clear_queue(_h: usize, o: *const abi::PeerSocketOptions) -> i32 {
    let (Some(lan), Some((peer, socket))) = (lan(), unsafe { peer_socket(o) }) else {
        return INVALID_PARAMETERS;
    };
    lan.clear(Some(peer), socket.as_deref());
    SUCCESS
}

pub fn on_event(lan: &Lan, ev: &Event) {
    let local = ids::puid(lan.me());
    match ev {
        Event::ChanRequest { socket, .. }
        | Event::ChanOpen { socket, .. }
        | Event::ChanClosed { socket, .. }
            if !is_eos_socket(socket) => {}
        Event::ChanRequest { peer, socket } => {
            trace("P2P request", || format!("{peer} {socket}"));
            let sid = socket_id(socket);
            for (f, cd) in listeners(Kind::P2PRequest, Some(socket)) {
                let info = abi::ConnectionInfo {
                    ClientData: cd,
                    LocalUserId: local,
                    RemoteUserId: ids::puid(*peer),
                    SocketId: &sid,
                };
                unsafe { call(f, &info) };
            }
        }
        Event::ChanOpen { peer, socket } => {
            trace("P2P established", || format!("{peer} {socket}"));
            let sid = socket_id(socket);
            for (f, cd) in listeners(Kind::P2PEstablished, Some(socket)) {
                let info = abi::ConnectionEstablishedInfo {
                    ClientData: cd,
                    LocalUserId: local,
                    RemoteUserId: ids::puid(*peer),
                    SocketId: &sid,
                    // EOS_CET_NewConnection, EOS_NCT_DirectConnection
                    ConnectionType: 0,
                    NetworkType: 1,
                };
                unsafe { call(f, &info) };
            }
        }
        Event::ChanClosed {
            peer,
            socket,
            reason,
        } => {
            trace("P2P closed", || format!("{peer} {socket} {reason:?}"));
            let sid = socket_id(socket);
            if *reason == CloseReason::Lost {
                for (f, cd) in listeners(Kind::P2PInterrupted, Some(socket)) {
                    let info = abi::ConnectionInfo {
                        ClientData: cd,
                        LocalUserId: local,
                        RemoteUserId: ids::puid(*peer),
                        SocketId: &sid,
                    };
                    unsafe { call(f, &info) };
                }
            }
            let code = match reason {
                CloseReason::ByLocal => 1,
                CloseReason::ByPeer => 2,
                CloseReason::Lost => 3,
            };
            for (f, cd) in listeners(Kind::P2PClosed, Some(socket)) {
                let info = abi::ConnectionClosedInfo {
                    ClientData: cd,
                    LocalUserId: local,
                    RemoteUserId: ids::puid(*peer),
                    SocketId: &sid,
                    Reason: code,
                };
                unsafe { call(f, &info) };
            }
        }
        _ => {}
    }
}
