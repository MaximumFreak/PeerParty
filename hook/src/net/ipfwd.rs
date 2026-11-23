//! Games where a player types the host's IP address: their socket calls to a peer's
//! stand-in address (198.18.x.y, shown in the overlay's Internet page) are redirected
//! through the forwarder (peerparty_core::fwd), so they work with internet players.
//! PeerParty's own servers on another PC (Photon's) are reached the same way, LAN or
//! not, so their host's firewall never has to let a player in: the forwarder runs in
//! every game once the LAN is up (it starts the LAN itself only for internet play).
//!
//! A connected UDP socket (`connect` on a datagram socket, as Photon's client does) is
//! connected to the forwarder's local socket for that peer and port, so its plain
//! `send` and `recv` work unhooked: the replies come from the address it connected to.
//!
//! Hooked: connect, WSAConnect, ConnectEx (handed out by WSAIoctl), sendto, WSASendTo,
//! recvfrom and WSARecvFrom. An overlapped WSARecvFrom that completes later has its
//! sender fixed when the game learns it finished, however it asks: its completion
//! routine, GetQueuedCompletionStatus(Ex), WSAGetOverlappedResult,
//! GetOverlappedResult(Ex) (padblock's detours, shared), a thread-pool IO object's
//! callback (CreateThreadpoolIo) or BindIoCompletionCallback's. The last two are wrapped
//! for socket and pipe handles only (GetFileType says "pipe" for both).
//! `PEERPARTY_SKIP=ipfwd` turns it off.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use peerparty_core::fwd::{self, Forwarder};
use peerparty_core::Event;
use retour::GenericDetour;

use crate::log;
use crate::sys::win::proc_addr;
use crate::sys::pe::Export;

type Connect = unsafe extern "system" fn(usize, *const u8, i32) -> i32;
type WsaConnect = unsafe extern "system" fn(usize, *const u8, i32, *mut u8, *mut u8, *mut u8, *mut u8) -> i32;
type SendTo = unsafe extern "system" fn(usize, *const u8, i32, i32, *const u8, i32) -> i32;
type WsaSendTo =
    unsafe extern "system" fn(usize, *const u8, u32, *mut u32, u32, *const u8, i32, *mut u8, *mut u8) -> i32;
type RecvFrom = unsafe extern "system" fn(usize, *mut u8, i32, i32, *mut u8, *mut i32) -> i32;
type WsaRecvFrom =
    unsafe extern "system" fn(usize, *const u8, u32, *mut u32, *mut u32, *mut u8, *mut i32, *mut u8, *mut u8) -> i32;

static CONNECT: OnceLock<GenericDetour<Connect>> = OnceLock::new();
static WSACONNECT: OnceLock<GenericDetour<WsaConnect>> = OnceLock::new();

/// Whether PeerParty itself has hooked Winsock's connect (net.rs tells its own patch from
/// another module's).
pub fn hooks_connect() -> bool {
    CONNECT.get().is_some()
}
static SENDTO: OnceLock<GenericDetour<SendTo>> = OnceLock::new();
static WSASENDTO: OnceLock<GenericDetour<WsaSendTo>> = OnceLock::new();
static RECVFROM: OnceLock<GenericDetour<RecvFrom>> = OnceLock::new();
static WSARECVFROM: OnceLock<GenericDetour<WsaRecvFrom>> = OnceLock::new();

type WsaIoctl =
    unsafe extern "system" fn(usize, u32, *const u8, u32, *mut u8, u32, *mut u32, *mut u8, *mut u8) -> i32;
type ConnectEx = unsafe extern "system" fn(usize, *const u8, i32, *const u8, u32, *mut u32, *mut u8) -> i32;
type GetQueued = unsafe extern "system" fn(usize, *mut u32, *mut usize, *mut *mut u8, u32) -> i32;
type GetQueuedEx = unsafe extern "system" fn(usize, *mut OverlappedEntry, u32, *mut u32, u32, i32) -> i32;
type WsaGetResult = unsafe extern "system" fn(usize, *mut u8, *mut u32, i32, *mut u32) -> i32;
type Completion = unsafe extern "system" fn(u32, u32, *mut u8, u32);
type CreateTpIo = unsafe extern "system" fn(usize, usize, usize, usize) -> usize;
/// PTP_WIN32_IO_CALLBACK: instance, context, OVERLAPPED, IoResult, bytes, the IO object.
type TpIoCallback = unsafe extern "system" fn(usize, usize, *mut u8, u32, usize, usize);
type BindIo = unsafe extern "system" fn(usize, usize, u32) -> i32;

static WSAIOCTL: OnceLock<GenericDetour<WsaIoctl>> = OnceLock::new();
static GETQUEUED: OnceLock<GenericDetour<GetQueued>> = OnceLock::new();
static GETQUEUED_EX: OnceLock<GenericDetour<GetQueuedEx>> = OnceLock::new();
static WSAGETRESULT: OnceLock<GenericDetour<WsaGetResult>> = OnceLock::new();
static CREATE_TP_IO: OnceLock<GenericDetour<CreateTpIo>> = OnceLock::new();
/// kernel32 has its own copy of CreateThreadpoolIo (Windows 11 24H2: it doesn't call
/// kernelbase's), so both are detoured.
static CREATE_TP_IO_K32: OnceLock<GenericDetour<CreateTpIo>> = OnceLock::new();
static BIND_IO: OnceLock<GenericDetour<BindIo>> = OnceLock::new();
/// The routine BindIoCompletionCallback was first given for a socket: ours goes in its
/// place (it gets no context to say whose it is, so only one is stood in for; a
/// runtime's IO thread pool uses one for every handle).
static BIND_ROUTINE: OnceLock<usize> = OnceLock::new();

/// A thread-pool IO object's callback and context, behind ours.
struct TpWrap {
    callback: TpIoCallback,
    context: usize,
}
/// The provider's ConnectEx that WSAIoctl handed out first; ours goes out in its place.
static REAL_CONNECTEX: OnceLock<usize> = OnceLock::new();

/// OVERLAPPED_ENTRY.
#[repr(C)]
struct OverlappedEntry {
    key: usize,
    ov: *mut u8,
    internal: usize,
    bytes: u32,
}

/// An overlapped WSARecvFrom still in flight: its socket, where the game wants the
/// sender written (and that buffer's size), and its completion routine if it gave one.
///
/// A receive the game collects some way that isn't hooked leaves its entry behind, and
/// the OVERLAPPED may then carry another operation: an entry goes when the same
/// OVERLAPPED starts a send or a ConnectEx, a completion that names a socket must be
/// on this one, and the sender buffer must still be there to be read (L-93).
#[derive(Clone, Copy)]
struct Pending {
    socket: usize,
    from: usize,
    cap: i32,
    routine: Option<Completion>,
    /// The OVERLAPPED's hEvent when it had one: a game that waits on that event and then
    /// reads the result itself is seen through its wait (`completed_by_events`).
    event: usize,
}

/// OVERLAPPED's hEvent, after Internal, InternalHigh and the 8-byte offset.
const OV_EVENT: usize = 2 * size_of::<usize>() + 8;
/// STATUS_PENDING in OVERLAPPED.Internal: not finished yet.
const STATUS_PENDING: usize = 0x103;

/// The event an OVERLAPPED names (0 for none or one we can't read).
unsafe fn ov_event(ov: *mut u8) -> usize {
    if ov.is_null() || !crate::sys::read::readable(ov as usize, OV_EVENT + size_of::<usize>()) {
        return 0;
    }
    // The low bit only says "no completion port packet" for that call.
    unsafe { std::ptr::read_unaligned(ov.add(OV_EVENT).cast::<usize>()) & !1 }
}

/// A wait on `handles` returned with them signalled: receives noted with one of them as
/// their event that have finished (OVERLAPPED.Internal no longer STATUS_PENDING) get
/// their sender fixed, before the game reads it.
fn completed_by_events(handles: &[usize]) {
    if handles.is_empty() || !ANY_PENDING.load(Ordering::Relaxed) {
        return;
    }
    let done: Vec<(usize, bool)> = {
        let m = crate::sys::lock(&PENDING);
        m.iter()
            .filter(|(_, p)| p.event != 0 && handles.contains(&p.event))
            .filter_map(|(ov, _)| {
                crate::sys::read::readable(*ov, size_of::<usize>()).then(|| unsafe { std::ptr::read_volatile(*ov as *const usize) }).filter(|st| *st != STATUS_PENDING).map(|st| (*ov, st == 0))
            })
            .collect()
    };
    for (ov, ok) in done {
        completed(ov, ok);
    }
}

type WaitOne = unsafe extern "system" fn(usize, u32) -> u32;
type WaitOneEx = unsafe extern "system" fn(usize, u32, i32) -> u32;
type WaitMany = unsafe extern "system" fn(u32, *const usize, i32, u32) -> u32;
type WaitManyEx = unsafe extern "system" fn(u32, *const usize, i32, u32, i32) -> u32;
type WsaWaitEvents = unsafe extern "system" fn(u32, *const usize, i32, u32, i32) -> u32;
type WsaRecvMsg = unsafe extern "system" fn(usize, *mut u8, *mut u32, *mut u8, *mut u8) -> i32;
type WsaSendMsg = unsafe extern "system" fn(usize, *const u8, u32, *mut u32, *mut u8, *mut u8) -> i32;

static WAIT_ONE: OnceLock<GenericDetour<WaitOne>> = OnceLock::new();
static WAIT_ONE_EX: OnceLock<GenericDetour<WaitOneEx>> = OnceLock::new();
static WAIT_MANY: OnceLock<GenericDetour<WaitMany>> = OnceLock::new();
static WAIT_MANY_EX: OnceLock<GenericDetour<WaitManyEx>> = OnceLock::new();
static WSA_WAIT: OnceLock<GenericDetour<WsaWaitEvents>> = OnceLock::new();
static REAL_RECVMSG: OnceLock<usize> = OnceLock::new();
static REAL_SENDMSG: OnceLock<usize> = OnceLock::new();

/// WSAID_WSARECVMSG {f689d7c8-6f1f-436b-8a53-e54fe351c322} and WSAID_WSASENDMSG
/// {a441e712-754f-43ca-84a7-0dee44cf606d}, as laid out in memory.
const WSAID_WSARECVMSG: [u8; 16] = [0xc8, 0xd7, 0x89, 0xf6, 0x1f, 0x6f, 0x6b, 0x43, 0x8a, 0x53, 0xe5, 0x4f, 0xe3, 0x51, 0xc3, 0x22];
const WSAID_WSASENDMSG: [u8; 16] = [0x12, 0xe7, 0x41, 0xa4, 0x4f, 0x75, 0xca, 0x43, 0x84, 0xa7, 0x0d, 0xee, 0x44, 0xcf, 0x60, 0x6d];

/// The handles a wait that returned `r` found signalled: all of them for a wait-all, the
/// one it names otherwise (WAIT_OBJECT_0 + i).
fn signalled(r: u32, n: u32, handles: *const usize, all: bool) -> Vec<usize> {
    if handles.is_null() || r >= n || n > 64 {
        return Vec::new();
    }
    let hs = unsafe { std::slice::from_raw_parts(handles, n as usize) };
    if all { hs.to_vec() } else { vec![hs[r as usize]] }
}

/// After a wait: fixes what finished, keeping the thread's last error.
fn after_wait(handles: Vec<usize>) {
    if handles.is_empty() || !ANY_PENDING.load(Ordering::Relaxed) {
        return;
    }
    let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    completed_by_events(&handles);
    unsafe { windows_sys::Win32::Foundation::SetLastError(err) };
}

unsafe extern "system" fn detour_wait_one(h: usize, ms: u32) -> u32 {
    let r = unsafe { WAIT_ONE.get().unwrap().call(h, ms) };
    if r == 0 {
        after_wait(vec![h]);
    }
    r
}

unsafe extern "system" fn detour_wait_one_ex(h: usize, ms: u32, alertable: i32) -> u32 {
    let r = unsafe { WAIT_ONE_EX.get().unwrap().call(h, ms, alertable) };
    if r == 0 {
        after_wait(vec![h]);
    }
    r
}

unsafe extern "system" fn detour_wait_many(n: u32, handles: *const usize, all: i32, ms: u32) -> u32 {
    let r = unsafe { WAIT_MANY.get().unwrap().call(n, handles, all, ms) };
    if ANY_PENDING.load(Ordering::Relaxed) {
        after_wait(signalled(r, n, handles, all != 0));
    }
    r
}

unsafe extern "system" fn detour_wait_many_ex(n: u32, handles: *const usize, all: i32, ms: u32, alertable: i32) -> u32 {
    let r = unsafe { WAIT_MANY_EX.get().unwrap().call(n, handles, all, ms, alertable) };
    if ANY_PENDING.load(Ordering::Relaxed) {
        after_wait(signalled(r, n, handles, all != 0));
    }
    r
}

unsafe extern "system" fn detour_wsa_wait(n: u32, events: *const usize, all: i32, ms: u32, alertable: i32) -> u32 {
    let r = unsafe { WSA_WAIT.get().unwrap().call(n, events, all, ms, alertable) };
    if ANY_PENDING.load(Ordering::Relaxed) {
        after_wait(signalled(r, n, events, all != 0));
    }
    r
}

/// WSAMSG's name and namelen (the first two fields).
unsafe fn msg_name(msg: *const u8) -> (*mut u8, *mut i32) {
    if msg.is_null() {
        return (std::ptr::null_mut(), std::ptr::null_mut());
    }
    unsafe { (std::ptr::read_unaligned(msg.cast::<*mut u8>()), msg.add(size_of::<usize>()) as *mut i32) }
}

/// WSARecvMsg (an extension function from WSAIoctl): its sender is fixed as
/// WSARecvFrom's is, now or when it completes.
unsafe extern "system" fn detour_recvmsg(s: usize, msg: *mut u8, got: *mut u32, ov: *mut u8, done: *mut u8) -> i32 {
    let real = unsafe { std::mem::transmute::<usize, WsaRecvMsg>(*REAL_RECVMSG.get().unwrap()) };
    let (from, fromlen) = unsafe { msg_name(msg) };
    let track = !ov.is_null() && !from.is_null() && FWD.get().is_some();
    let mut routine = done;
    if track {
        let cap = unsafe { *fromlen };
        let orig = (!done.is_null()).then(|| unsafe { std::mem::transmute::<*mut u8, Completion>(done) });
        note_pending(ov as usize, Pending { socket: s, from: from as usize, cap, routine: orig, event: unsafe { ov_event(ov) } });
        if let Some(f) = orig {
            crate::sys::lock(&ROUTINES).insert(ov as usize, f);
            routine = completion_shim as *const () as *mut u8;
        }
    }
    let r = unsafe { real(s, msg, got, ov, routine) };
    let err = unsafe { windows_sys::Win32::Networking::WinSock::WSAGetLastError() };
    if r == 0 && !from.is_null() {
        unsafe { fix_from(from, fromlen) };
    }
    let failed = r != 0 && err != WSA_IO_PENDING;
    if track && (failed || (r == 0 && routine == done)) {
        take_pending(ov as usize);
    }
    if track && failed && routine != done {
        crate::sys::lock(&ROUTINES).remove(&(ov as usize));
    }
    if r != 0 {
        unsafe { windows_sys::Win32::Networking::WinSock::WSASetLastError(err) };
    }
    r
}

/// WSASendMsg: a datagram to a stand-in goes where sendto's would.
unsafe extern "system" fn detour_sendmsg(s: usize, msg: *const u8, flags: u32, sent: *mut u32, ov: *mut u8, done: *mut u8) -> i32 {
    let real = unsafe { std::mem::transmute::<usize, WsaSendMsg>(*REAL_SENDMSG.get().unwrap()) };
    overlapped_reused(ov as usize);
    let (to, len) = unsafe { msg_name(msg) };
    let len = if len.is_null() { 0 } else { unsafe { *len } };
    let Some(buf) = (unsafe { udp_target(s, to, len) }) else {
        return unsafe { real(s, msg, flags, sent, ov, done) };
    };
    // A copy of the WSAMSG naming the forwarder's address instead (the game's stays as
    // it was).
    // sizeof(WSAMSG): name, namelen, lpBuffers, dwBufferCount, Control (WSABUF), dwFlags.
    const WSAMSG_SIZE: usize = if cfg!(target_pointer_width = "64") { 56 } else { 28 };
    let mut copy = [0u8; WSAMSG_SIZE];
    unsafe { std::ptr::copy_nonoverlapping(msg, copy.as_mut_ptr(), WSAMSG_SIZE) };
    unsafe { std::ptr::write_unaligned(copy.as_mut_ptr().cast::<*const u8>(), buf.as_ptr()) };
    unsafe { std::ptr::write_unaligned(copy.as_mut_ptr().add(size_of::<usize>()).cast::<i32>(), len.min(ADDR_MAX)) };
    unsafe { real(s, copy.as_ptr(), flags, sent, ov, done) }
}

/// Receives in flight, by their OVERLAPPED. ANY_PENDING spares the completion hooks a
/// lock when there are none.
static PENDING: LazyLock<Mutex<HashMap<usize, Pending>>> = LazyLock::new(Mutex::default);
static ANY_PENDING: AtomicBool = AtomicBool::new(false);
/// More than this many in flight: the game collects them some way that isn't hooked,
/// so the table is cleared rather than grown.
const MAX_PENDING: usize = 4096;

const SIO_GET_EXTENSION_FUNCTION_POINTER: u32 = 0xC800_0006;
/// WSAID_CONNECTEX, {25a207b9-ddf3-4660-8ee9-76e58c74063e}, as laid out in memory.
const WSAID_CONNECTEX: [u8; 16] = [0xb9, 0x07, 0xa2, 0x25, 0xf3, 0xdd, 0x60, 0x46, 0x8e, 0xe9, 0x76, 0xe5, 0x8c, 0x74, 0x06, 0x3e];
const WSA_IO_PENDING: i32 = 997;
const WSA_IO_INCOMPLETE: i32 = 996;

static FWD: OnceLock<Arc<Forwarder>> = OnceLock::new();

/// Whether the forwarder runs (internet play is on and winsock is hooked): typed-IP
/// addresses (stand-ins) then reach internet peers.
pub fn active() -> bool {
    FWD.get().is_some()
}

/// The internet peer behind a client a server in this process sees at `addr`: a
/// stand-in (a rewritten datagram sender), one of our UDP aliases, or the loopback end
/// of a stream we opened to the game.
pub fn forwarded_peer(addr: std::net::SocketAddr) -> Option<peerparty_core::PeerId> {
    let f = FWD.get()?;
    if let std::net::IpAddr::V4(v4) = addr.ip()
        && fwd::is_stand_in(v4)
    {
        return f.peer_at(v4);
    }
    f.forwarded_peer(addr)
}

const AF_INET: u16 = 2;
const AF_INET6: u16 = 23;

/// Whether internet play is configured (a room code or public sessions): only then do
/// typed-IP games need the forwarder (on a LAN they reach each other's real addresses).
pub(crate) fn internet_configured() -> bool {
    let c = crate::config::HookConfig::global();
    c.room.is_some() || c.public == Some(true)
}

/// ws2_32.dll loaded: hooks its socket calls and starts the engine and the forwarder on
/// a thread of their own (never from inside a hooked call).
pub fn attach_winsock(exports: &[Export]) -> usize {
    if !crate::net::enabled() || crate::net::skip("ipfwd") {
        return 0;
    }
    let mut n = 0;
    n += hook_one(exports, "connect", |a| arm(&CONNECT, unsafe { std::mem::transmute::<*const (), Connect>(a) }, detour_connect, "connect"));
    n += hook_one(exports, "WSAConnect", |a| {
        arm(&WSACONNECT, unsafe { std::mem::transmute::<*const (), WsaConnect>(a) }, detour_wsaconnect, "WSAConnect")
    });
    n += hook_one(exports, "sendto", |a| arm(&SENDTO, unsafe { std::mem::transmute::<*const (), SendTo>(a) }, detour_sendto, "sendto"));
    n += hook_one(exports, "WSASendTo", |a| {
        arm(&WSASENDTO, unsafe { std::mem::transmute::<*const (), WsaSendTo>(a) }, detour_wsasendto, "WSASendTo")
    });
    n += hook_one(exports, "recvfrom", |a| {
        arm(&RECVFROM, unsafe { std::mem::transmute::<*const (), RecvFrom>(a) }, detour_recvfrom, "recvfrom")
    });
    n += hook_one(exports, "WSARecvFrom", |a| {
        arm(&WSARECVFROM, unsafe { std::mem::transmute::<*const (), WsaRecvFrom>(a) }, detour_wsarecvfrom, "WSARecvFrom")
    });
    n += hook_one(exports, "WSAIoctl", |a| {
        arm(&WSAIOCTL, unsafe { std::mem::transmute::<*const (), WsaIoctl>(a) }, detour_wsaioctl, "WSAIoctl")
    });
    n += hook_one(exports, "WSAGetOverlappedResult", |a| {
        arm(&WSAGETRESULT, unsafe { std::mem::transmute::<*const (), WsaGetResult>(a) }, detour_wsagetresult, "WSAGetOverlappedResult")
    });
    // IO completion ports and thread-pool IO live in kernelbase (kernel32's call there).
    if let Some(kb) = module("kernelbase.dll") {
        if let Some(a) = proc_addr(kb, c"GetQueuedCompletionStatus") {
            n += usize::from(arm(&GETQUEUED, unsafe { std::mem::transmute::<usize, GetQueued>(a) }, detour_getqueued, "GetQueuedCompletionStatus"));
        }
        if let Some(a) = proc_addr(kb, c"GetQueuedCompletionStatusEx") {
            n += usize::from(arm(&GETQUEUED_EX, unsafe { std::mem::transmute::<usize, GetQueuedEx>(a) }, detour_getqueued_ex, "GetQueuedCompletionStatusEx"));
        }
        if let Some(a) = proc_addr(kb, c"CreateThreadpoolIo") {
            n += usize::from(arm(&CREATE_TP_IO, unsafe { std::mem::transmute::<usize, CreateTpIo>(a) }, detour_create_tp_io, "CreateThreadpoolIo"));
        }
        // Waits on an OVERLAPPED's event, after which a game reads the result itself.
        if let Some(a) = proc_addr(kb, c"WaitForSingleObject") {
            n += usize::from(arm(&WAIT_ONE, unsafe { std::mem::transmute::<usize, WaitOne>(a) }, detour_wait_one, "WaitForSingleObject"));
        }
        if let Some(a) = proc_addr(kb, c"WaitForSingleObjectEx") {
            n += usize::from(arm(&WAIT_ONE_EX, unsafe { std::mem::transmute::<usize, WaitOneEx>(a) }, detour_wait_one_ex, "WaitForSingleObjectEx"));
        }
        if let Some(a) = proc_addr(kb, c"WaitForMultipleObjects") {
            n += usize::from(arm(&WAIT_MANY, unsafe { std::mem::transmute::<usize, WaitMany>(a) }, detour_wait_many, "WaitForMultipleObjects"));
        }
        if let Some(a) = proc_addr(kb, c"WaitForMultipleObjectsEx") {
            n += usize::from(arm(&WAIT_MANY_EX, unsafe { std::mem::transmute::<usize, WaitManyEx>(a) }, detour_wait_many_ex, "WaitForMultipleObjectsEx"));
        }
    }
    n += hook_one(exports, "WSAWaitForMultipleEvents", |a| {
        arm(&WSA_WAIT, unsafe { std::mem::transmute::<*const (), WsaWaitEvents>(a) }, detour_wsa_wait, "WSAWaitForMultipleEvents")
    });
    // kernel32's own code, on ntdll's thread pool.
    if let Some(k32) = module("kernel32.dll") {
        if let Some(a) = proc_addr(k32, c"BindIoCompletionCallback") {
            n += usize::from(arm(&BIND_IO, unsafe { std::mem::transmute::<usize, BindIo>(a) }, detour_bind_io, "BindIoCompletionCallback"));
        }
        let kb = module("kernelbase.dll").and_then(|kb| proc_addr(kb, c"CreateThreadpoolIo"));
        if let Some(a) = proc_addr(k32, c"CreateThreadpoolIo").map(|a| unsafe { past_jump(a) })
            && Some(a) != kb
        {
            n += usize::from(arm(&CREATE_TP_IO_K32, unsafe { std::mem::transmute::<usize, CreateTpIo>(a) }, detour_create_tp_io_k32, "kernel32 CreateThreadpoolIo"));
        }
    }
    // GetOverlappedResult(Ex): shared dispatcher in `overlapped` notifies both ipfwd and input::hid.
    n += crate::overlapped::install();
    if n > 0 {
        STARTING.store(true, Ordering::Relaxed);
        let _ = std::thread::Builder::new().name("peerparty-ipfwd-start".into()).spawn(|| {
            // Internet play needs the LAN now (a typed-IP game has nothing else to start
            // it), once this process is the game rather than its starter; otherwise it
            // waits for whatever starts it (a game SDK), so a dedicated server still
            // says it's one first.
            let lan = if internet_configured() {
                lan_once_the_game()
            } else {
                loop {
                    if let Some(l) = crate::net::lan_if_started() {
                        break Some(l);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            };
            let Some(lan) = lan else {
                STARTING.store(false, Ordering::Relaxed);
                return;
            };
            let f = Forwarder::new(lan.clone(), Some(crate::net::pump), core_log);
            f.ensure_pump();
            if FWD.set(f).is_ok() {
                log!("ipfwd: on; games that ask for an IP reach internet players at their 198.18.x.y address (overlay: Internet), and other PCs' Photon servers are reached over PeerParty");
            }
        });
    }
    n
}

/// How long a process with no game SDK waits before it takes itself for the game, with
/// no program it started still to say whether it is; and at most, one still to say.
const STARTER_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
const STARTER_WAIT_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// The LAN for internet play, started here once this process is the game: a game SDK
/// loaded, or STARTER_WAIT passed with no program it started pending. A starter that
/// hands the game to a program it started never starts one: it would take this PC's
/// first player slot (its name, its saves), and the game would be the second player,
/// "Name (2)". Whatever starts it first meanwhile (a game SDK) is used.
fn lan_once_the_game() -> Option<&'static peerparty_core::Lan> {
    let since = std::time::Instant::now();
    loop {
        if let Some(l) = crate::net::lan_if_started() {
            return Some(l);
        }
        if crate::ipc::handed_over() {
            log!("ipfwd: a program this one started is the game; no LAN in this one");
            return None;
        }
        let waited = since.elapsed();
        if crate::child::sdk_seen() || (waited >= STARTER_WAIT && !crate::child::starter_pending()) || waited >= STARTER_WAIT_MAX {
            return crate::net::lan();
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn core_log(line: &str) {
    log!("{line}");
}

/// An engine event (net.rs hands every one over): channel events for the forwarder,
/// and the address to type for a player who arrives over the internet.
pub fn event(ev: &Event) {
    let Some(f) = FWD.get() else { return };
    if Forwarder::wants(ev) {
        f.event(ev);
    }
    if let Event::PeerJoined(peer) = ev
        && let Some(lan) = crate::net::lan_if_started()
        && lan.peer_ip(*peer).is_some_and(|ip| matches!(ip, std::net::IpAddr::V4(v) if fwd::is_stand_in(v)))
    {
        let name = lan.peer_name(*peer).unwrap_or_default();
        log!("ipfwd: {name} is at {} for games that ask for an IP", Forwarder::address_of(*peer));
    }
}

fn module(name: &str) -> Option<windows_sys::Win32::Foundation::HMODULE> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let m = unsafe { GetModuleHandleW(wide.as_ptr()) };
    (!m.is_null()).then_some(m)
}

/// Where a function that starts with a `jmp rel32` (kernel32's exports that lead to
/// their code elsewhere in kernel32) really starts; else `a`.
unsafe fn past_jump(a: usize) -> usize {
    let p = a as *const u8;
    if unsafe { *p } == 0xE9 {
        let rel = unsafe { std::ptr::read_unaligned(p.add(1).cast::<i32>()) };
        return (a as isize + 5 + rel as isize) as usize;
    }
    a
}
fn hook_one(exports: &[Export], name: &str, install: impl FnOnce(*const ()) -> bool) -> usize {
    crate::sys::detour::hook_one(exports, "ipfwd", name, install)
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, target: T, detour: T, label: &str) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    crate::sys::detour::arm(slot, target, detour, "ipfwd", label)
}

/// A socket address the game passed: IPv4, or IPv4-mapped IPv6 (`true`).
unsafe fn parse(sa: *const u8, len: i32) -> Option<(SocketAddrV4, bool)> {
    if sa.is_null() || len < 16 {
        return None;
    }
    let b = unsafe { std::slice::from_raw_parts(sa, (len as usize).min(28)) };
    let port = u16::from_be_bytes([b[2], b[3]]);
    match u16::from_le_bytes([b[0], b[1]]) {
        AF_INET => Some((SocketAddrV4::new(Ipv4Addr::new(b[4], b[5], b[6], b[7]), port), false)),
        AF_INET6 if b.len() >= 24 => {
            let a: [u8; 16] = b[8..24].try_into().ok()?;
            std::net::Ipv6Addr::from(a).to_ipv4_mapped().map(|v4| (SocketAddrV4::new(v4, port), true))
        }
        _ => None,
    }
}

/// Writes `to` over the address part of `sa` (its family kept).
unsafe fn put(sa: *mut u8, to: SocketAddrV4, v6: bool) {
    let b = unsafe { std::slice::from_raw_parts_mut(sa, if v6 { 24 } else { 8 }) };
    b[2..4].copy_from_slice(&to.port().to_be_bytes());
    if v6 {
        b[8..24].copy_from_slice(&to.ip().to_ipv6_mapped().octets());
    } else {
        b[4..8].copy_from_slice(&to.ip().octets());
    }
}

/// A copy of the game's address with `to` in it.
/// The size of what `rewritten` hands back (a sockaddr_in6, the larger). The length
/// Winsock is told with it is cut to this: a game's own may be a SOCKADDR_STORAGE's,
/// and Winsock would read that far past the end of ours.
const ADDR_MAX: i32 = 28;

unsafe fn rewritten(sa: *const u8, len: i32, to: SocketAddrV4, v6: bool) -> [u8; 28] {
    let mut buf = [0u8; 28];
    let n = (len as usize).min(28);
    unsafe {
        std::ptr::copy_nonoverlapping(sa, buf.as_mut_ptr(), n);
        put(buf.as_mut_ptr(), to, v6);
    }
    buf
}

/// The socket's own port, binding it (as sending would) if it isn't bound yet.
unsafe fn local_port(s: usize, v6: bool) -> Option<u16> {
    use windows_sys::Win32::Networking::WinSock::{SOCKADDR, bind, getsockname};
    let mut buf = [0u8; 28];
    let mut len = 28i32;
    if unsafe { getsockname(s, buf.as_mut_ptr().cast::<SOCKADDR>(), &mut len) } != 0 {
        let mut any = [0u8; 28];
        any[0..2].copy_from_slice(&(if v6 { AF_INET6 } else { AF_INET }).to_le_bytes());
        let n = if v6 { 28 } else { 16 };
        if unsafe { bind(s, any.as_ptr().cast::<SOCKADDR>(), n) } != 0 {
            return None;
        }
        len = 28;
        if unsafe { getsockname(s, buf.as_mut_ptr().cast::<SOCKADDR>(), &mut len) } != 0 {
            return None;
        }
    }
    Some(u16::from_be_bytes([buf[2], buf[3]]))
}

/// Where a datagram to `to` really goes (`None`: unchanged).
unsafe fn udp_target(s: usize, to: *const u8, len: i32) -> Option<[u8; 28]> {
    let (dest, v6) = unsafe { parse(to, len) }?;
    if !fwd::is_stand_in(*dest.ip()) {
        return None;
    }
    let f = forwarder_soon()?;
    let local = unsafe { local_port(s, v6) }?;
    let via = f.udp_dest(dest, local)?;
    Some(unsafe { rewritten(to, len, via, v6) })
}

/// The forwarder, for traffic to a stand-in address. It starts on a thread of its own
/// once the hooks are in (never inside a hooked call), so a game quick enough to reach a
/// stand-in first waits for it a moment (up to FWD_WAIT) instead of sending into
/// nowhere. Only traffic to a stand-in waits, and only once the hooks are armed.
fn forwarder_soon() -> Option<&'static Arc<Forwarder>> {
    const FWD_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
    if let Some(f) = FWD.get() {
        return Some(f);
    }
    if !STARTING.load(Ordering::Relaxed) {
        return None;
    }
    let until = std::time::Instant::now() + FWD_WAIT;
    while std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(20));
        if let Some(f) = FWD.get() {
            return Some(f);
        }
    }
    None
}

/// The forwarder's start is under way (its thread is running).
static STARTING: AtomicBool = AtomicBool::new(false);

/// Where a connection to `to` really goes (`None`: unchanged).
unsafe fn tcp_target(to: *const u8, len: i32) -> Option<[u8; 28]> {
    let (dest, v6) = unsafe { parse(to, len) }?;
    if !fwd::is_stand_in(*dest.ip()) {
        return None;
    }
    let f = forwarder_soon()?;
    let via = f.tcp_dest(dest)?;
    log!("ipfwd: a connection to {dest} goes over peerparty");
    Some(unsafe { rewritten(to, len, via, v6) })
}

/// Shows a datagram from one of the forwarder's local sockets as from the peer.
unsafe fn fix_from(from: *mut u8, fromlen: *mut i32) {
    let Some(f) = FWD.get() else { return };
    if from.is_null() || fromlen.is_null() {
        return;
    }
    let len = unsafe { *fromlen };
    // A game socket bound to a LAN address gets its peers' datagrams from aliases on
    // that address, not loopback: those are rewritten too.
    if let Some((src, v6)) = unsafe { parse(from, len) }
        && f.may_be_alias(*src.ip())
        && let Some(peer) = f.udp_from(src)
    {
        unsafe { put(from, peer, v6) };
    }
}

/// Whether `s` is a datagram (UDP) socket.
unsafe fn is_datagram(s: usize) -> bool {
    use windows_sys::Win32::Networking::WinSock::{SO_TYPE, SOCK_DGRAM, SOL_SOCKET, getsockopt};
    let mut t = 0i32;
    let mut n = size_of::<i32>() as i32;
    unsafe { getsockopt(s, SOL_SOCKET, SO_TYPE, (&mut t as *mut i32).cast(), &mut n) == 0 && t == SOCK_DGRAM as i32 }
}

/// Where a `connect` to `to` really goes: a UDP socket to the forwarder's local socket
/// for that peer and port, a TCP one to its stream listener (`None`: unchanged).
unsafe fn connect_target(s: usize, to: *const u8, len: i32) -> Option<[u8; 28]> {
    if unsafe { is_datagram(s) } {
        let via = unsafe { udp_target(s, to, len) };
        if via.is_some() {
            crate::log::throttled("ipfwd udp connect", 4, |n| log!("ipfwd: a UDP socket connected to a stand-in goes over peerparty (n={n})"));
        }
        return via;
    }
    unsafe { tcp_target(to, len) }
}

unsafe extern "system" fn detour_connect(s: usize, name: *const u8, len: i32) -> i32 {
    let real = CONNECT.get().unwrap();
    match unsafe { connect_target(s, name, len) } {
        Some(buf) => unsafe { real.call(s, buf.as_ptr(), len.min(ADDR_MAX)) },
        None => unsafe { real.call(s, name, len) },
    }
}

unsafe extern "system" fn detour_wsaconnect(
    s: usize,
    name: *const u8,
    len: i32,
    caller: *mut u8,
    callee: *mut u8,
    sqos: *mut u8,
    gqos: *mut u8,
) -> i32 {
    let real = WSACONNECT.get().unwrap();
    match unsafe { connect_target(s, name, len) } {
        Some(buf) => unsafe { real.call(s, buf.as_ptr(), len.min(ADDR_MAX), caller, callee, sqos, gqos) },
        None => unsafe { real.call(s, name, len, caller, callee, sqos, gqos) },
    }
}

unsafe extern "system" fn detour_sendto(s: usize, data: *const u8, n: i32, flags: i32, to: *const u8, len: i32) -> i32 {
    let real = SENDTO.get().unwrap();
    match unsafe { udp_target(s, to, len) } {
        Some(buf) => unsafe { real.call(s, data, n, flags, buf.as_ptr(), len.min(ADDR_MAX)) },
        None => unsafe { real.call(s, data, n, flags, to, len) },
    }
}

unsafe extern "system" fn detour_wsasendto(
    s: usize,
    bufs: *const u8,
    count: u32,
    sent: *mut u32,
    flags: u32,
    to: *const u8,
    len: i32,
    ov: *mut u8,
    done: *mut u8,
) -> i32 {
    let real = WSASENDTO.get().unwrap();
    overlapped_reused(ov as usize);
    match unsafe { udp_target(s, to, len) } {
        Some(buf) => unsafe { real.call(s, bufs, count, sent, flags, buf.as_ptr(), len.min(ADDR_MAX), ov, done) },
        None => unsafe { real.call(s, bufs, count, sent, flags, to, len, ov, done) },
    }
}

unsafe extern "system" fn detour_recvfrom(s: usize, data: *mut u8, n: i32, flags: i32, from: *mut u8, fromlen: *mut i32) -> i32 {
    let r = unsafe { RECVFROM.get().unwrap().call(s, data, n, flags, from, fromlen) };
    if r >= 0 {
        unsafe { fix_from(from, fromlen) };
    }
    r
}

unsafe extern "system" fn detour_wsarecvfrom(
    s: usize,
    bufs: *const u8,
    count: u32,
    got: *mut u32,
    flags: *mut u32,
    from: *mut u8,
    fromlen: *mut i32,
    ov: *mut u8,
    done: *mut u8,
) -> i32 {
    let real = WSARECVFROM.get().unwrap();
    // Overlapped with a sender wanted: noted before the call, since another thread can
    // collect the completion before this one returns. A completion routine is swapped
    // for ours, which fixes the sender and then calls it (it runs even when the
    // receive completes at once).
    let track = !ov.is_null() && !from.is_null() && !fromlen.is_null() && FWD.get().is_some();
    let mut routine = done;
    if track {
        let cap = unsafe { *fromlen };
        let orig = (!done.is_null()).then(|| unsafe { std::mem::transmute::<*mut u8, Completion>(done) });
        note_pending(ov as usize, Pending { socket: s, from: from as usize, cap, routine: orig, event: unsafe { ov_event(ov) } });
        if let Some(f) = orig {
            crate::sys::lock(&ROUTINES).insert(ov as usize, f);
            routine = completion_shim as *const () as *mut u8;
        }
    }
    let r = unsafe { real.call(s, bufs, count, got, flags, from, fromlen, ov, routine) };
    let err = unsafe { windows_sys::Win32::Networking::WinSock::WSAGetLastError() };
    // 0: it completed now and `from` is filled in (even when overlapped).
    if r == 0 {
        unsafe { fix_from(from, fromlen) };
    }
    let failed = r != 0 && err != WSA_IO_PENDING;
    // Nothing completes later when it failed, or when it finished now without a
    // routine of ours to run (a completion port may still hand it out; its sender is
    // fixed already).
    if track && (failed || (r == 0 && routine == done)) {
        take_pending(ov as usize);
    }
    // Failed at once: no routine runs for it.
    if track && failed && routine != done {
        crate::sys::lock(&ROUTINES).remove(&(ov as usize));
    }
    if r != 0 {
        unsafe { windows_sys::Win32::Networking::WinSock::WSASetLastError(err) };
    }
    r
}

/// The game's completion routines for receives our shim stands in for, by OVERLAPPED.
/// Apart from PENDING: a receive can be collected first some other way (the game asks
/// WSAGetOverlappedResult while its routine is still to run), and the routine must
/// still be found when ours runs. Only the shim (or a receive that failed at once)
/// takes one out.
static ROUTINES: LazyLock<Mutex<HashMap<usize, Completion>>> = LazyLock::new(Mutex::default);

fn note_pending(ov: usize, p: Pending) {
    let mut m = crate::sys::lock(&PENDING);
    if m.len() >= MAX_PENDING && !m.contains_key(&ov) {
        log!("ipfwd: {MAX_PENDING} overlapped receives never collected through a hooked call; forgetting them");
        // Not the ones with a completion routine: ours has to find the game's to call.
        m.retain(|_, p| p.routine.is_some());
    }
    m.insert(ov, p);
    ANY_PENDING.store(true, Ordering::Relaxed);
}

fn take_pending(ov: usize) -> Option<Pending> {
    if ov == 0 || !ANY_PENDING.load(Ordering::Relaxed) {
        return None;
    }
    let mut m = crate::sys::lock(&PENDING);
    let p = m.remove(&ov);
    if m.is_empty() {
        ANY_PENDING.store(false, Ordering::Relaxed);
    }
    p
}

/// An OVERLAPPED starts another operation: a receive noted for it was collected some
/// way we didn't see, and is forgotten (one with a routine of ours ran it already).
fn overlapped_reused(ov: usize) {
    if ov != 0 && ANY_PENDING.load(Ordering::Relaxed) {
        let mut m = crate::sys::lock(&PENDING);
        if m.get(&ov).is_some_and(|p| p.routine.is_none()) {
            m.remove(&ov);
        }
        if m.is_empty() {
            ANY_PENDING.store(false, Ordering::Relaxed);
        }
    }
}

/// An overlapped receive finished (`ok`: successfully): its sender is fixed.
fn completed(ov: usize, ok: bool) -> Option<Pending> {
    let p = take_pending(ov)?;
    // fix_from checks the buffer still holds one of the forwarder's loopback aliases,
    // so one the game has reused since is left alone; one no longer there isn't read.
    if ok && p.cap > 0 && crate::sys::read::readable(p.from, p.cap as usize) {
        let mut len = p.cap;
        unsafe { fix_from(p.from as *mut u8, &mut len) };
    }
    Some(p)
}

/// A completion on socket `s`: only a receive noted on that socket is taken.
fn completed_on(s: usize, ov: usize, ok: bool) -> Option<Pending> {
    if ov == 0 || !ANY_PENDING.load(Ordering::Relaxed) {
        return None;
    }
    if crate::sys::lock(&PENDING).get(&ov).is_some_and(|p| p.socket != s) {
        return None;
    }
    completed(ov, ok)
}

/// GetOverlappedResult(Ex) returned `ok` (padblock's detours call this): a receive
/// that finished has its sender fixed. The thread's last error is kept.
pub fn overlapped_result(ov: usize, ok: bool) {
    if ov == 0 || !ANY_PENDING.load(Ordering::Relaxed) {
        return;
    }
    let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    // ERROR_IO_INCOMPLETE: still running; anything else that failed is done.
    if ok || err != WSA_IO_INCOMPLETE as u32 {
        completed(ov, ok);
    }
    unsafe { windows_sys::Win32::Foundation::SetLastError(err) };
}

/// A socket (or a pipe: GetFileType can't tell them apart). The thread's last error
/// is kept.
fn socket_like(h: usize) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
    let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    let pipe = h != 0 && h != usize::MAX && unsafe { GetFileType(h as _) } == FILE_TYPE_PIPE;
    unsafe { windows_sys::Win32::Foundation::SetLastError(err) };
    pipe
}

/// CreateThreadpoolIo for a socket: the callback is wrapped, so a receive that
/// completes through it has its sender fixed first. The wrapper (two words) lives as
/// long as the process: the pool may still call it after CloseThreadpoolIo returns.
unsafe extern "system" fn detour_create_tp_io(file: usize, callback: usize, context: usize, env: usize) -> usize {
    unsafe { wrap_tp_io(CREATE_TP_IO.get().unwrap(), file, callback, context, env) }
}

unsafe extern "system" fn detour_create_tp_io_k32(file: usize, callback: usize, context: usize, env: usize) -> usize {
    unsafe { wrap_tp_io(CREATE_TP_IO_K32.get().unwrap(), file, callback, context, env) }
}

unsafe fn wrap_tp_io(real: &GenericDetour<CreateTpIo>, file: usize, callback: usize, context: usize, env: usize) -> usize {
    if callback == 0 || FWD.get().is_none() || !socket_like(file) {
        return unsafe { real.call(file, callback, context, env) };
    }
    let callback = unsafe { std::mem::transmute::<usize, TpIoCallback>(callback) };
    let wrap = Box::into_raw(Box::new(TpWrap { callback, context }));
    let io = unsafe { real.call(file, tp_io_shim as *const () as usize, wrap as usize, env) };
    if io == 0 {
        // Not made: nothing will call it.
        drop(unsafe { Box::from_raw(wrap) });
    }
    io
}

unsafe extern "system" fn tp_io_shim(instance: usize, context: usize, ov: *mut u8, result: u32, bytes: usize, io: usize) {
    let w = unsafe { &*(context as *const TpWrap) };
    completed(ov as usize, result == 0);
    unsafe { (w.callback)(instance, w.context, ov, result, bytes, io) };
}

/// BindIoCompletionCallback for a socket: its routine goes behind ours (see
/// BIND_ROUTINE).
unsafe extern "system" fn detour_bind_io(file: usize, routine: usize, flags: u32) -> i32 {
    let real = BIND_IO.get().unwrap();
    if routine == 0 || FWD.get().is_none() || !socket_like(file) {
        return unsafe { real.call(file, routine, flags) };
    }
    if *BIND_ROUTINE.get_or_init(|| routine) != routine {
        static SAID: AtomicBool = AtomicBool::new(false);
        if !SAID.swap(true, Ordering::Relaxed) {
            log!("ipfwd: BindIoCompletionCallback with a second routine: its receives keep a loopback sender");
        }
        return unsafe { real.call(file, routine, flags) };
    }
    unsafe { real.call(file, bind_shim as *const () as usize, flags) }
}

unsafe extern "system" fn bind_shim(err: u32, bytes: u32, ov: *mut u8) {
    completed(ov as usize, err == 0);
    let f = unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(u32, u32, *mut u8)>(*BIND_ROUTINE.get().unwrap()) };
    unsafe { f(err, bytes, ov) };
}

unsafe extern "system" fn completion_shim(err: u32, bytes: u32, ov: *mut u8, flags: u32) {
    // The sender, unless another way of collecting it fixed it already.
    completed(ov as usize, err == 0);
    let f = crate::sys::lock(&ROUTINES).remove(&(ov as usize));
    if let Some(f) = f {
        unsafe { f(err, bytes, ov, flags) };
    }
}

unsafe extern "system" fn detour_getqueued(port: usize, bytes: *mut u32, key: *mut usize, ov: *mut *mut u8, ms: u32) -> i32 {
    let r = unsafe { GETQUEUED.get().unwrap().call(port, bytes, key, ov, ms) };
    if ANY_PENDING.load(Ordering::Relaxed) && !ov.is_null() {
        let o = unsafe { *ov };
        if !o.is_null() {
            let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            completed(o as usize, r != 0);
            unsafe { windows_sys::Win32::Foundation::SetLastError(err) };
        }
    }
    r
}

unsafe extern "system" fn detour_getqueued_ex(
    port: usize,
    entries: *mut OverlappedEntry,
    count: u32,
    removed: *mut u32,
    ms: u32,
    alertable: i32,
) -> i32 {
    let r = unsafe { GETQUEUED_EX.get().unwrap().call(port, entries, count, removed, ms, alertable) };
    if r != 0 && ANY_PENDING.load(Ordering::Relaxed) && !entries.is_null() && !removed.is_null() {
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        let n = unsafe { *removed }.min(count) as usize;
        for e in unsafe { std::slice::from_raw_parts(entries, n) } {
            // Internal holds the NTSTATUS: below 0x8000_0000 is success.
            completed(e.ov as usize, (e.internal as u32) < 0x8000_0000);
        }
        unsafe { windows_sys::Win32::Foundation::SetLastError(err) };
    }
    r
}

unsafe extern "system" fn detour_wsagetresult(s: usize, ov: *mut u8, bytes: *mut u32, wait: i32, flags: *mut u32) -> i32 {
    let r = unsafe { WSAGETRESULT.get().unwrap().call(s, ov, bytes, wait, flags) };
    if ANY_PENDING.load(Ordering::Relaxed) {
        if r != 0 {
            completed_on(s, ov as usize, true);
        } else {
            let err = unsafe { windows_sys::Win32::Networking::WinSock::WSAGetLastError() };
            // Still running: keep waiting. Anything else: it failed.
            if err != WSA_IO_INCOMPLETE {
                completed_on(s, ov as usize, false);
            }
            unsafe { windows_sys::Win32::Networking::WinSock::WSASetLastError(err) };
        }
    }
    r
}

/// WSAIoctl hands out ConnectEx: ours goes out instead, so its address is redirected
/// like connect's.
unsafe extern "system" fn detour_wsaioctl(
    s: usize,
    code: u32,
    inbuf: *const u8,
    inlen: u32,
    outbuf: *mut u8,
    outlen: u32,
    returned: *mut u32,
    ov: *mut u8,
    done: *mut u8,
) -> i32 {
    let r = unsafe { WSAIOCTL.get().unwrap().call(s, code, inbuf, inlen, outbuf, outlen, returned, ov, done) };
    if r == 0
        && code == SIO_GET_EXTENSION_FUNCTION_POINTER
        && !inbuf.is_null()
        && inlen >= 16
        && !outbuf.is_null()
        && outlen as usize >= std::mem::size_of::<usize>()
        && unsafe { std::slice::from_raw_parts(inbuf, 16) } == WSAID_CONNECTEX
    {
        let real = unsafe { std::ptr::read_unaligned(outbuf.cast::<usize>()) };
        // Another provider's ConnectEx (rare: a layered provider on some sockets) is
        // left as it is; ours can only stand in for one.
        if real != 0 && real != detour_connectex as *const () as usize && *REAL_CONNECTEX.get_or_init(|| real) == real {
            unsafe { std::ptr::write_unaligned(outbuf.cast::<usize>(), detour_connectex as *const () as usize) };
        }
    }
    // WSARecvMsg and WSASendMsg the same way.
    if r == 0 && code == SIO_GET_EXTENSION_FUNCTION_POINTER && !inbuf.is_null() && inlen >= 16 && !outbuf.is_null() && outlen as usize >= size_of::<usize>() {
        let id = unsafe { std::slice::from_raw_parts(inbuf, 16) };
        let (slot, ours) = if id == WSAID_WSARECVMSG {
            (&REAL_RECVMSG, detour_recvmsg as *const () as usize)
        } else if id == WSAID_WSASENDMSG {
            (&REAL_SENDMSG, detour_sendmsg as *const () as usize)
        } else {
            return r;
        };
        let real = unsafe { std::ptr::read_unaligned(outbuf.cast::<usize>()) };
        if real != 0 && real != ours && *slot.get_or_init(|| real) == real {
            unsafe { std::ptr::write_unaligned(outbuf.cast::<usize>(), ours) };
        }
    }
    r
}

unsafe extern "system" fn detour_connectex(
    s: usize,
    name: *const u8,
    len: i32,
    send: *const u8,
    send_len: u32,
    sent: *mut u32,
    ov: *mut u8,
) -> i32 {
    let real = unsafe { std::mem::transmute::<usize, ConnectEx>(*REAL_CONNECTEX.get().unwrap()) };
    overlapped_reused(ov as usize);
    match unsafe { tcp_target(name, len) } {
        Some(buf) => unsafe { real(s, buf.as_ptr(), len.min(ADDR_MAX), send, send_len, sent, ov) },
        None => unsafe { real(s, name, len, send, send_len, sent, ov) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "system" fn routine(_: u32, _: u32, _: *mut u8, _: u32) {}

    /// The tests that use PENDING run one at a time (one of them clears it).
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn overlapped_receives_are_tracked_until_collected() {
        let _one = crate::sys::lock(&SERIAL);
        let plain = Pending { socket: 5, from: 0x1000, cap: 16, routine: None, event: 0 };
        let with_routine = Pending { socket: 5, from: 0x2000, cap: 16, routine: Some(routine), event: 0 };
        note_pending(1, plain);
        note_pending(2, with_routine);
        assert!(ANY_PENDING.load(Ordering::Relaxed));
        // Collected once; a second completion of the same OVERLAPPED isn't ours.
        assert_eq!(completed(1, false).map(|p| p.from), Some(0x1000));
        assert!(completed(1, true).is_none());
        // Overflowing forgets the plain ones, never one whose routine we stand in for.
        for ov in 100..100 + MAX_PENDING {
            note_pending(ov, plain);
        }
        note_pending(9, plain);
        assert!(take_pending(2).is_some_and(|p| p.routine.is_some()));
        assert!(take_pending(100).is_none());
        assert!(take_pending(9).is_some());
        assert!(take_pending(100 + MAX_PENDING - 1).is_some());
        crate::sys::lock(&PENDING).clear();

        // Left behind (collected unseen), then the OVERLAPPED reused: forgotten, unless
        // a routine of ours still has to run.
        note_pending(3, plain);
        note_pending(4, with_routine);
        overlapped_reused(3);
        overlapped_reused(4);
        assert!(take_pending(3).is_none());
        assert!(take_pending(4).is_some());
        // A completion naming another socket isn't this receive's.
        note_pending(6, plain);
        assert!(completed_on(7, 6, true).is_none());
        assert!(completed_on(5, 6, false).is_some());
        // A sender buffer no longer there isn't read (address 0x1000 is never mapped).
        note_pending(8, plain);
        assert!(completed(8, true).is_some());
        crate::sys::lock(&PENDING).clear();
    }

    /// A receive the game collects by waiting on its OVERLAPPED's event is taken once the
    /// wait says that event is set and the OVERLAPPED says it finished.
    #[test]
    fn a_receive_waited_for_by_its_event_is_collected() {
        let _one = crate::sys::lock(&SERIAL);
        let mut ov = vec![0usize; 8];
        ov[0] = STATUS_PENDING;
        ov[OV_EVENT / size_of::<usize>()] = 0x1234;
        let at = ov.as_mut_ptr() as usize;
        assert_eq!(unsafe { ov_event(at as *mut u8) }, 0x1234);
        let from = [0u8; 16];
        note_pending(at, Pending { socket: 5, from: from.as_ptr() as usize, cap: 16, routine: None, event: 0x1234 });
        completed_by_events(&[0x1234]);
        assert!(crate::sys::lock(&PENDING).contains_key(&at), "still running");
        ov[0] = 0;
        completed_by_events(&[0x9999]);
        assert!(crate::sys::lock(&PENDING).contains_key(&at), "another event");
        completed_by_events(&[0x1234]);
        assert!(!crate::sys::lock(&PENDING).contains_key(&at), "finished and waited for");
        let hs = [7usize, 8, 9];
        assert_eq!(signalled(1, 3, hs.as_ptr(), false), vec![8]);
        assert_eq!(signalled(0, 3, hs.as_ptr(), true), vec![7, 8, 9]);
        assert!(signalled(258, 3, hs.as_ptr(), false).is_empty(), "WAIT_TIMEOUT");
    }

    #[test]
    fn addresses_parse_and_rewrite_in_their_own_family() {
        let mut v4 = [0u8; 16];
        v4[0..2].copy_from_slice(&AF_INET.to_le_bytes());
        v4[2..4].copy_from_slice(&7777u16.to_be_bytes());
        v4[4..8].copy_from_slice(&[198, 18, 3, 4]);
        let (a, v6) = unsafe { parse(v4.as_ptr(), 16) }.unwrap();
        assert_eq!((a, v6), (SocketAddrV4::new(Ipv4Addr::new(198, 18, 3, 4), 7777), false));
        let to = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 50000);
        let b = unsafe { rewritten(v4.as_ptr(), 16, to, false) };
        assert_eq!(unsafe { parse(b.as_ptr(), 16) }, Some((to, false)));

        let mut v6 = [0u8; 28];
        v6[0..2].copy_from_slice(&AF_INET6.to_le_bytes());
        v6[2..4].copy_from_slice(&7777u16.to_be_bytes());
        v6[8..24].copy_from_slice(&Ipv4Addr::new(198, 19, 1, 2).to_ipv6_mapped().octets());
        assert_eq!(unsafe { parse(v6.as_ptr(), 28) }, Some((SocketAddrV4::new(Ipv4Addr::new(198, 19, 1, 2), 7777), true)));
        let b = unsafe { rewritten(v6.as_ptr(), 28, to, true) };
        assert_eq!(unsafe { parse(b.as_ptr(), 28) }, Some((to, true)));
        assert_eq!(&b[0..2], &AF_INET6.to_le_bytes());

        // A real IPv6 address, a short buffer and null are left alone.
        let mut real6 = v6;
        real6[8..24].copy_from_slice(&std::net::Ipv6Addr::LOCALHOST.octets());
        assert_eq!(unsafe { parse(real6.as_ptr(), 28) }, None);
        assert_eq!(unsafe { parse(v4.as_ptr(), 8) }, None);
        assert_eq!(unsafe { parse(std::ptr::null(), 16) }, None);
    }
}
