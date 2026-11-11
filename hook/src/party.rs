//! PeerParty's PlayFab Party layer (on in every game started through PeerParty): the game's Party networks run
//! between copies of the game on the LAN instead of through Azure relays.
//!
//! Every `Party*` export of the game's Party DLL is swapped by name (swap.rs); the real
//! library is never initialized. A Party network is a LAN lobby (namespace
//! `party.network`, id = the network's GUID) and its network descriptor serializes to
//! `PP1-<guid>`. Each member's endpoint is a member attribute (its entity id), and
//! endpoint messages travel on the LAN channel `party:<guid>`. State changes queue up
//! and reach the game through `PartyStartProcessingStateChanges`, in the order the
//! Party SDK documents (connect, authenticate, endpoint created...).
//!
//! Chat controls, text chat and voice chat are carried too (party/chat.rs): a chat
//! control the game connects to a network is a member attribute like an endpoint,
//! and its voice and text ride voice.rs's room `party:<guid>`. Transcription and
//! text-to-speech report unsupported. A game that never makes a chat control sees
//! exactly what it saw before chat existed here. Struct layouts follow Party.h as
//! mirrored by PlayFab's open-source Unity plugin (github.com/PlayFab/PlayFabPartyUnity,
//! PartyCSharpSDK.Interop) and Party_c.h from the C++ SDK
//! (nuget.org/packages/Microsoft.PlayFab.PlayFabParty.Cpp.Windows).

mod chat;
pub mod xbl;

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CString, c_char};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use peerparty_core::{Attr, Event, JoinResult, Lan, Lobby, MemberStatus, Packet, PeerId, SendResult, Value};

use crate::log;
use crate::net::{self, Front};
use crate::pe::Export;

const NS: &str = "party.network";
const SOCKET_PREFIX: &str = "party:";
const DESCRIPTOR_PREFIX: &str = "PP1-";
const REGION: &str = "LAN";
const ENTITY_KEY: &str = "ep";

const OK: u32 = 0;
/// PeerParty's "not supported here" (chat, kicking, unknown functions).
const E_UNSUPPORTED: u32 = 1;
const E_BAD_HANDLE: u32 = 2;
const E_BAD_ARG: u32 = 3;
/// An audio source stream has no buffer ready.
const E_NO_BUFFER: u32 = 5;
/// Party's own "network no longer exists".
const E_NETWORK_GONE: u32 = 4104;

// PARTY_STATE_CHANGE_TYPE
const REGIONS_CHANGED: u32 = 0;
const DESTROY_LOCAL_USER_COMPLETED: u32 = 1;
const CREATE_NEW_NETWORK_COMPLETED: u32 = 2;
const CONNECT_TO_NETWORK_COMPLETED: u32 = 3;
const AUTHENTICATE_LOCAL_USER_COMPLETED: u32 = 4;
const NETWORK_CONFIGURATION_MADE_AVAILABLE: u32 = 5;
const LOCAL_USER_REMOVED: u32 = 7;
const REMOVE_LOCAL_USER_COMPLETED: u32 = 8;
const CREATE_ENDPOINT_COMPLETED: u32 = 10;
const DESTROY_ENDPOINT_COMPLETED: u32 = 11;
const ENDPOINT_CREATED: u32 = 12;
const ENDPOINT_DESTROYED: u32 = 13;
const REMOTE_DEVICE_CREATED: u32 = 14;
const REMOTE_DEVICE_DESTROYED: u32 = 15;
const REMOTE_DEVICE_JOINED_NETWORK: u32 = 16;
const REMOTE_DEVICE_LEFT_NETWORK: u32 = 17;
const LEAVE_NETWORK_COMPLETED: u32 = 19;
const NETWORK_DESTROYED: u32 = 20;
const ENDPOINT_MESSAGE_RECEIVED: u32 = 21;
const DATA_BUFFERS_RETURNED: u32 = 22;
const CREATE_INVITATION_COMPLETED: u32 = 51;
const INVITATION_CREATED: u32 = 53;

// PARTY_STATE_CHANGE_RESULT
const SUCCEEDED: u32 = 0;
const NETWORK_NO_LONGER_EXISTS: u32 = 11;
const NETWORK_NOT_JOINABLE: u32 = 12;

// PARTY_DESTROYED_REASON
const REASON_REQUESTED: u32 = 0;
const REASON_DISCONNECTED: u32 = 1;
const REASON_KICKED: u32 = 2;
const REASON_CREATION_FAILED: u32 = 4;

const SEND_GUARANTEED: u32 = 1;
const SEND_SEQUENTIAL: u32 = 2;
/// PARTY_SEND_MESSAGE_OPTIONS_DONT_COPY_DATA_BUFFERS: the game keeps its buffers until a
/// data-buffers-returned change. PeerParty copies them while sending, so it queues that
/// change as soon as the send is accepted.
const SEND_DONT_COPY: u32 = 4;
/// How long an endpoint message may wait for its sender's endpoint (or ours) to be shown.
const EARLY_WAIT: Duration = Duration::from_secs(5);
/// Most endpoint messages waiting that way at once.
const EARLY_MAX: usize = 4096;
/// Most batches of state changes the game may hold (started, not all returned) before
/// the oldest is let go.
const MAX_BATCHES: usize = 64;
/// How often the endpoint traffic summary is logged while messages flow.
const TRAFFIC_EVERY: Duration = Duration::from_secs(60);

const INVITATION_ID_BUFFER: usize = 128;

// ---- Party.h structs (pointers as usize so the state can move between threads) ----

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    id: [u8; 37],
    region: [u8; 20],
    opaque: [u8; 301],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NetConfig {
    max_users: u32,
    max_devices: u32,
    max_users_per_device: u32,
    max_devices_per_user: u32,
    max_endpoints_per_device: u32,
    direct_peer: u32,
}

#[repr(C)]
struct Region {
    name: [u8; 20],
    rtt_ms: u32,
}

#[repr(C)]
struct InvitationConfig {
    identifier: usize,
    revocability: u32,
    entity_count: u32,
    entity_ids: usize,
}

#[repr(C)]
struct DataBuffer {
    buffer: usize,
    len: u32,
}

#[repr(C)]
struct ResultOnly {
    ty: u32,
    result: u32,
    detail: u32,
}

#[repr(C)]
struct DestroyLocalUserCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    user: usize,
    async_id: usize,
}

#[repr(C)]
struct CreateNewNetworkCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    user: usize,
    config: NetConfig,
    region_count: u32,
    regions: usize,
    async_id: usize,
    descriptor: Descriptor,
    applied_invitation: usize,
}

#[repr(C)]
struct ConnectCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    descriptor: Descriptor,
    async_id: usize,
    network: usize,
}

#[repr(C)]
struct AuthenticateCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    network: usize,
    user: usize,
    invitation: usize,
    async_id: usize,
}

#[repr(C)]
struct ConfigMadeAvailable {
    ty: u32,
    network: usize,
    config: usize,
}

#[repr(C)]
struct LocalUserRemoved {
    ty: u32,
    network: usize,
    user: usize,
    reason: u32,
}

#[repr(C)]
struct RemoveLocalUserCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    network: usize,
    user: usize,
    async_id: usize,
}

#[repr(C)]
struct CreateEndpointCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    network: usize,
    user: usize,
    async_id: usize,
    endpoint: usize,
}

#[repr(C)]
struct DestroyEndpointCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    network: usize,
    endpoint: usize,
    async_id: usize,
}

#[repr(C)]
struct EndpointCreated {
    ty: u32,
    network: usize,
    endpoint: usize,
}

#[repr(C)]
struct EndpointDestroyed {
    ty: u32,
    network: usize,
    endpoint: usize,
    reason: u32,
    detail: u32,
}

#[repr(C)]
struct DeviceOnly {
    ty: u32,
    device: usize,
}

#[repr(C)]
struct DeviceJoined {
    ty: u32,
    device: usize,
    network: usize,
}

#[repr(C)]
struct DeviceLeft {
    ty: u32,
    reason: u32,
    detail: u32,
    device: usize,
    network: usize,
}

#[repr(C)]
struct LeaveCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    network: usize,
    async_id: usize,
}

#[repr(C)]
struct NetworkDestroyed {
    ty: u32,
    reason: u32,
    detail: u32,
    network: usize,
}

#[repr(C)]
struct MessageReceived {
    ty: u32,
    network: usize,
    sender: usize,
    receiver_count: u32,
    receivers: usize,
    options: u32,
    size: u32,
    buffer: usize,
}

#[repr(C)]
struct DataBuffersReturned {
    ty: u32,
    network: usize,
    sender: usize,
    buffer_count: u32,
    buffers: usize,
    message_id: usize,
}

#[repr(C)]
struct CreateInvitationCompleted {
    ty: u32,
    result: u32,
    detail: u32,
    network: usize,
    user: usize,
    async_id: usize,
    invitation: usize,
}

#[repr(C)]
struct InvitationCreated {
    ty: u32,
    network: usize,
    invitation: usize,
}

// ---- PeerParty's objects ----

struct User {
    entity: CString,
}

struct Device {
    peer: Option<PeerId>,
}

struct Endpoint {
    network: usize,
    peer: Option<PeerId>,
    entity: CString,
    uid: u16,
    device: usize,
    user: usize,
}

struct Invitation {
    network: usize,
    _id: CString,
    creator: CString,
    config: Box<InvitationConfig>,
}

struct Network {
    guid: String,
    descriptor: Descriptor,
    config: Box<NetConfig>,
    connected: bool,
    user: usize,
    local_endpoint: Option<usize>,
    /// Remote endpoints are shown once our own endpoint exists.
    remote: Vec<usize>,
    invitations: Vec<usize>,
    /// Remote devices shown in this network (for an endpoint or a chat control).
    devices: Vec<(usize, PeerId)>,
    /// Remote chat controls shown in it, our chat controls connected to it, and ours
    /// waiting for it to connect (with their async ids).
    chats: Vec<usize>,
    local_chats: Vec<usize>,
    pending_chats: Vec<(usize, usize)>,
}

/// One queued state change: the struct, plus memory it points into.
struct Change {
    _data: Box<dyn std::any::Any + Send>,
    ptr: usize,
    _bytes: Vec<u8>,
    _ptrs: Vec<usize>,
    _texts: Vec<CString>,
}

/// One batch of changes handed to the game: the array it got, the changes it hasn't
/// returned yet, and the handles to forget once it has returned them all.
struct Batch {
    /// Not read again, only kept alive: the game reads it.
    _ptrs: Vec<usize>,
    changes: Vec<Change>,
    forget: Vec<usize>,
}

#[derive(Default)]
struct State {
    initialized: bool,
    local_device: usize,
    users: HashMap<usize, User>,
    devices: HashMap<usize, Device>,
    networks: HashMap<usize, Network>,
    endpoints: HashMap<usize, Endpoint>,
    invitations: HashMap<usize, Invitation>,
    contexts: HashMap<usize, usize>,
    /// Networks we created and haven't connected to yet: guid -> (config, invitation id).
    hosted: HashMap<String, (NetConfig, String)>,
    queue: Vec<Change>,
    /// Batches handed out by PartyStartProcessingStateChanges that the game hasn't
    /// returned all of yet, oldest first. Games may hold several, and return their
    /// changes out of order or a few at a time.
    in_flight: VecDeque<Batch>,
    /// Handles to forget once the game has seen the changes that destroyed them, for
    /// changes still queued (each batch carries those for its own changes).
    forget: Vec<usize>,
    /// Arrays handed out by Get* functions. Party only promises them until the next
    /// PartyStartProcessingStateChanges, so all are kept until then.
    lists: Vec<Vec<usize>>,
    /// A batch was let go for being past MAX_BATCHES (logged once).
    batches_dropped: bool,
    regions: Vec<Region>,
    /// Chat controls (ours and remote ones), in creation order, and audio streams.
    chats: HashMap<usize, chat::Chat>,
    chat_order: Vec<usize>,
    streams: HashMap<usize, chat::Stream>,
    /// The game made a chat control: from then on remote ones are shown and room
    /// traffic is pumped. Until then Party behaves as it did without chat.
    chat_used: bool,
    /// Endpoint messages that came before their sender's endpoint (or ours) was shown,
    /// oldest first, with when they came.
    early: VecDeque<(Packet, Instant)>,
    /// Who has a message in `early`, by (sender, network): a later message from them
    /// waits behind it. A set, not a scan of `early`: with thousands waiting, a scan
    /// per message made each pump quadratic on the game thread.
    early_from: HashSet<(PeerId, usize)>,
    traffic: Traffic,
}

/// Endpoint message counts since PartyInitialize, for the log's traffic summaries.
#[derive(Default)]
struct Traffic {
    sent: u64,
    sent_bytes: u64,
    /// Of `sent`, the ones that went over the connection's ordered stream.
    sent_ordered: u64,
    /// Messages to a target that PeerParty couldn't send (not in `sent`); core logs why,
    /// once per player.
    not_sent: u64,
    received: u64,
    received_bytes: u64,
    /// Of `received`, the ones that waited for an endpoint first.
    waited: u64,
    dropped: u64,
    /// sent + received + dropped at the last summary, and when that was.
    logged: u64,
    last: Option<Instant>,
}

impl State {
    /// Logs the endpoint traffic so far, when there's been any since the last summary.
    fn log_traffic(&mut self, why: &str) {
        let waiting = self.early.len();
        let t = &mut self.traffic;
        t.last = Some(Instant::now());
        let total = t.sent + t.not_sent + t.received + t.dropped;
        if total == t.logged {
            return;
        }
        t.logged = total;
        log!(
            "party-lan traffic ({why}): sent {} messages ({} bytes; {} ordered), not sent {}, received {} ({} bytes; {} waited for an endpoint), dropped {}, {waiting} waiting now",
            t.sent,
            t.sent_bytes,
            t.sent_ordered,
            t.not_sent,
            t.received,
            t.received_bytes,
            t.waited,
            t.dropped
        );
    }

    /// Counts and logs (throttled) an endpoint message the game won't get.
    fn drop_message(&mut self, p: &Packet, why: &str) {
        self.traffic.dropped += 1;
        let n = self.traffic.dropped;
        if n <= 8 || n.is_power_of_two() {
            log!(
                "party-lan dropped a message from {} ({} bytes; {n} dropped so far): {why}",
                p.peer,
                p.data.len().saturating_sub(1)
            );
        }
    }
}

// Only plain data and handles; raw pointers are kept as usize.
unsafe impl Send for State {}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);
static NEXT_HANDLE: AtomicUsize = AtomicUsize::new(0x5041_0000);
static LOADED: AtomicBool = AtomicBool::new(false);
const PARTY_HANDLE: usize = 0x5041_7254;

fn handle() -> usize {
    NEXT_HANDLE.fetch_add(0x10, Ordering::Relaxed)
}

/// Whether the game has a PlayFab Party library loaded (and PeerParty took it over).
pub fn loaded() -> bool {
    LOADED.load(Ordering::Relaxed)
}

pub fn attach(base: usize, module_name: &str, exports: &[Export]) -> HashSet<usize> {
    crate::swap::register(base, module_name, resolve);
    net::subscribe(Front::Party);
    LOADED.store(true, Ordering::Relaxed);
    let ours = exports.iter().filter(|e| e.name.starts_with("Party")).count();
    log!(
        "party-lan on for {module_name}: {ours} Party functions are PeerParty's (chat carried; transcription and text-to-speech report unsupported)"
    );
    HashSet::new()
}

fn trace(name: &str, detail: impl FnOnce() -> String) {
    crate::log::throttled(name, 4, |n| log!("party-lan call {name} n={n} {}", detail()));
}

// ---- helpers ----

fn fixed<const N: usize>(s: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let b = s.as_bytes();
    let n = b.len().min(N - 1);
    out[..n].copy_from_slice(&b[..n]);
    out
}

fn fixed_str(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).to_string()
}

fn descriptor_for(guid: &str) -> Descriptor {
    Descriptor {
        id: fixed(guid),
        region: fixed(REGION),
        opaque: [0; 301],
    }
}

fn new_guid() -> String {
    let h = PeerId::random().hex();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn uid_of(peer: PeerId) -> u16 {
    u16::from_le_bytes([peer.0[0], peer.0[1]]) | 1
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap()
}

unsafe fn text(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        crate::read::cstr(p as usize).unwrap_or_default()
    }
}

unsafe fn put<T>(out: *mut T, v: T) {
    if !out.is_null() {
        unsafe { out.write_unaligned(v) };
    }
}

impl State {
    fn push<T: std::any::Any + Send>(&mut self, c: T) {
        self.push_with(c, Vec::new(), Vec::new(), None, |_, _, _, _| {});
    }

    /// Queues a change whose struct points into `bytes`, `ptrs` or `text`;
    /// `fix` gets their final addresses.
    fn push_with<T: std::any::Any + Send>(
        &mut self,
        c: T,
        bytes: Vec<u8>,
        ptrs: Vec<usize>,
        text: Option<CString>,
        fix: impl FnOnce(&mut T, usize, usize, usize),
    ) {
        self.push_full(c, bytes, ptrs, text.into_iter().collect(), |c, b, p, t| {
            fix(c, b, p, t.first().copied().unwrap_or(0))
        });
    }

    /// `push_with` for changes pointing at several strings.
    fn push_full<T: std::any::Any + Send>(
        &mut self,
        c: T,
        bytes: Vec<u8>,
        ptrs: Vec<usize>,
        texts: Vec<CString>,
        fix: impl FnOnce(&mut T, usize, usize, &[usize]),
    ) {
        let mut data = Box::new(c);
        let b = bytes.as_ptr() as usize;
        let p = ptrs.as_ptr() as usize;
        let t: Vec<usize> = texts.iter().map(|t| t.as_ptr() as usize).collect();
        fix(&mut data, b, p, &t);
        let ptr = &*data as *const T as usize;
        self.queue.push(Change {
            _data: data,
            ptr,
            _bytes: bytes,
            _ptrs: ptrs,
            _texts: texts,
        });
    }

    /// An array for a Get* function to hand out, valid until the next
    /// PartyStartProcessingStateChanges (an earlier array stays valid when a later call
    /// of the same kind returns a different one). One with the same items is reused, so
    /// a game polling between batches doesn't pile up copies.
    fn list(&mut self, _kind: &'static str, items: Vec<usize>) -> (u32, usize) {
        let n = items.len() as u32;
        if items.is_empty() {
            return (0, 0);
        }
        let i = match self.lists.iter().position(|v| *v == items) {
            Some(i) => i,
            None => {
                self.lists.push(items);
                self.lists.len() - 1
            }
        };
        (n, self.lists[i].as_ptr() as usize)
    }

    /// Hands the queued changes to the game as a new batch: (count, array).
    fn start_batch(&mut self) -> (u32, usize) {
        // Get* arrays are only promised until now.
        self.lists.clear();
        if self.queue.is_empty() {
            return (0, 0);
        }
        let changes = std::mem::take(&mut self.queue);
        // Handles destroyed by changes queued from here on (say, a leave the game asks for
        // while handling this batch) stay valid until the game has seen those changes.
        let forget = std::mem::take(&mut self.forget);
        if self.in_flight.len() >= MAX_BATCHES
            && let Some(old) = self.in_flight.pop_front()
        {
            if !self.batches_dropped {
                self.batches_dropped = true;
                log!(
                    "party-lan: the game holds {MAX_BATCHES} state change batches it hasn't finished; letting the oldest go ({} changes not returned)",
                    old.changes.len()
                );
            }
            self.forget_now(old.forget);
        }
        let ptrs: Vec<usize> = changes.iter().map(|c| c.ptr).collect();
        let n = ptrs.len() as u32;
        let p = ptrs.as_ptr() as usize;
        self.in_flight.push_back(Batch { _ptrs: ptrs, changes, forget });
        (n, p)
    }

    /// The game returned `changes` (pointers from the batches it got). Each is freed;
    /// ones it never got, or returns twice, are ignored. Once a batch, and every batch
    /// before it, has been returned in full, the handles its changes destroyed go.
    fn finish_batch(&mut self, changes: &[usize]) {
        for &c in changes {
            for b in self.in_flight.iter_mut() {
                if let Some(i) = b.changes.iter().position(|x| x.ptr == c) {
                    b.changes.swap_remove(i);
                    break;
                }
            }
        }
        while self.in_flight.front().is_some_and(|b| b.changes.is_empty()) {
            if let Some(b) = self.in_flight.pop_front() {
                self.forget_now(b.forget);
            }
        }
    }

    /// Changes the game holds now.
    fn held(&self) -> usize {
        self.in_flight.iter().map(|b| b.changes.len()).sum()
    }

    fn forget_now(&mut self, handles: Vec<usize>) {
        for h in handles {
            self.endpoints.remove(&h);
            self.devices.remove(&h);
            self.networks.remove(&h);
            self.invitations.remove(&h);
            self.contexts.remove(&h);
            if self.chats.remove(&h).is_some() {
                self.chat_order.retain(|c| *c != h);
            }
            self.streams.remove(&h);
        }
    }

    /// Queues the data-buffers-returned change for a DontCopyDataBuffers send: PeerParty
    /// copied the buffers while sending, so the game gets them straight back. The change
    /// points at a copy of the game's buffer array (the same buffers, as Party does).
    fn buffers_returned(&mut self, network: usize, sender: usize, buffers: &[DataBuffer], message_id: usize) {
        // PARTY_DATA_BUFFER is a pointer and a u32, padded to two pointers.
        let ptrs: Vec<usize> = buffers.iter().flat_map(|b| [b.buffer, b.len as usize]).collect();
        let count = buffers.len() as u32;
        self.push_with(
            DataBuffersReturned {
                ty: DATA_BUFFERS_RETURNED,
                network,
                sender,
                buffer_count: count,
                buffers: 0,
                message_id,
            },
            Vec::new(),
            ptrs,
            None,
            |c, _, p, _| c.buffers = if count == 0 { 0 } else { p },
        );
    }

    /// The messages waiting for endpoints, for another try (each goes back if it still
    /// has to wait).
    fn take_early(&mut self) -> VecDeque<(Packet, Instant)> {
        self.early_from.clear();
        std::mem::take(&mut self.early)
    }

    fn network_of_guid(&self, guid: &str) -> Option<usize> {
        self.networks
            .iter()
            .find(|(_, n)| n.guid == guid)
            .map(|(h, _)| *h)
    }

    fn device_for(&mut self, peer: PeerId) -> (usize, bool) {
        // A device already destroyed (forgotten at the end of a batch) isn't reused.
        let found = self
            .devices
            .iter()
            .find(|(h, d)| d.peer == Some(peer) && !self.forgotten(**h))
            .map(|(h, _)| *h);
        if let Some(h) = found {
            return (h, false);
        }
        let h = handle();
        self.devices.insert(h, Device { peer: Some(peer) });
        (h, true)
    }

    /// Brings a network's remote devices, endpoints and chat controls in line with its
    /// LAN lobby. Remote endpoints show once our own endpoint exists (and are left as
    /// they are while it doesn't); remote chat controls show once the game has made a
    /// chat control of its own and authenticated on the network. Per player the order
    /// is device created/joined, endpoint created, chat control created/joined, and
    /// the reverse when they go; without chat that's exactly the order from before.
    fn reconcile(&mut self, lan: &Lan, net: usize) {
        let Some(n) = self.networks.get(&net) else { return };
        if !n.connected {
            return;
        }
        let show_eps = n.local_endpoint.is_some();
        let show_chats = self.chat_used && n.user != 0;
        if !show_eps && !show_chats {
            return;
        }
        let me = lan.me();
        let text_of =
            |attrs: &[Attr], key: &str| peerparty_core::find_attr(attrs, key).map(|a| a.value.text());
        type Wanted = (PeerId, Option<String>, Option<(String, String)>);
        let members: Vec<Wanted> = lan
            .lobby(&n.guid)
            .map(|l| {
                l.members
                    .iter()
                    .filter(|m| m.id != me)
                    .map(|m| {
                        let chat = text_of(&m.attrs, chat::CHAT_KEY).map(|e| {
                            (e, text_of(&m.attrs, chat::CHAT_LANGUAGE_KEY).unwrap_or_default())
                        });
                        (m.id, text_of(&m.attrs, ENTITY_KEY), chat)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let ep_of = |s: &State, peer: PeerId| {
            s.networks[&net]
                .remote
                .iter()
                .copied()
                .find(|e| s.endpoints.get(e).is_some_and(|x| x.peer == Some(peer)))
        };
        let chat_of = |s: &State, peer: PeerId| {
            s.networks[&net]
                .chats
                .iter()
                .copied()
                .find(|c| s.chats.get(c).is_some_and(|x| x.peer == Some(peer)))
        };
        // Who went (or took their endpoint or chat control away).
        let shown = n.devices.clone();
        for (device, peer) in shown {
            let m = members.iter().find(|m| m.0 == peer);
            let ep = ep_of(self, peer);
            let chat = chat_of(self, peer);
            let want_ep = if show_eps { m.is_some_and(|m| m.1.is_some()) } else { ep.is_some() };
            let announced = m.and_then(|m| m.2.as_ref());
            let want_chat = if show_chats { announced.is_some() } else { chat.is_some() };
            if let Some(ep) = ep.filter(|_| !want_ep) {
                self.drop_endpoint(net, ep, REASON_DISCONNECTED);
            }
            if let Some(c) = chat {
                // A different entity means they replaced their chat control.
                let replaced = show_chats && announced.is_some_and(|(e, _)| !self.chats[&c].is(e));
                if !want_chat || replaced {
                    self.remote_chat_left(net, c, REASON_DISCONNECTED);
                }
            }
            if !want_ep && !want_chat {
                self.device_left(net, device, REASON_DISCONNECTED);
            }
        }
        // Who came.
        for (peer, ep_entity, chat) in members {
            let want_ep = show_eps && ep_entity.is_some();
            let want_chat = show_chats && chat.is_some();
            if !want_ep && !want_chat {
                continue;
            }
            let shown = self.networks[&net].devices.iter().find(|d| d.1 == peer).map(|d| d.0);
            let device = match shown {
                Some(d) => d,
                None => {
                    let (device, fresh) = self.device_for(peer);
                    if fresh {
                        self.push(DeviceOnly {
                            ty: REMOTE_DEVICE_CREATED,
                            device,
                        });
                    }
                    self.push(DeviceJoined {
                        ty: REMOTE_DEVICE_JOINED_NETWORK,
                        device,
                        network: net,
                    });
                    self.networks.get_mut(&net).unwrap().devices.push((device, peer));
                    device
                }
            };
            if let Some(entity) = ep_entity.filter(|_| want_ep && ep_of(self, peer).is_none()) {
                let ep = handle();
                self.endpoints.insert(
                    ep,
                    Endpoint {
                        network: net,
                        peer: Some(peer),
                        entity: cstring(&entity),
                        uid: uid_of(peer),
                        device,
                        user: 0,
                    },
                );
                self.networks.get_mut(&net).unwrap().remote.push(ep);
                self.push(EndpointCreated {
                    ty: ENDPOINT_CREATED,
                    network: net,
                    endpoint: ep,
                });
                log!("party-lan {entity} joined network {}", self.networks[&net].guid);
            }
            if let Some((entity, language)) = chat.filter(|_| want_chat && chat_of(self, peer).is_none()) {
                let (c, fresh) = self.remote_chat_for(peer, &entity, &language, device);
                if fresh {
                    self.remote_chat_created(c);
                }
                self.networks.get_mut(&net).unwrap().chats.push(c);
                self.chat_joined(net, c);
                log!("party-lan chat control {entity} ({language}) joined network {}", self.networks[&net].guid);
                self.apply_audio(net);
            }
        }
    }

    fn drop_endpoint(&mut self, net: usize, ep: usize, reason: u32) {
        let Some(e) = self.endpoints.get(&ep) else { return };
        log!("party-lan {} left", e.entity.to_string_lossy());
        if let Some(n) = self.networks.get_mut(&net) {
            n.remote.retain(|x| *x != ep);
        }
        self.push(EndpointDestroyed {
            ty: ENDPOINT_DESTROYED,
            network: net,
            endpoint: ep,
            reason,
            detail: 0,
        });
        self.forget.push(ep);
    }

    /// A remote device left `net`; it's destroyed once no network shows it.
    fn device_left(&mut self, net: usize, device: usize, reason: u32) {
        if let Some(n) = self.networks.get_mut(&net) {
            n.devices.retain(|d| d.0 != device);
        }
        self.push(DeviceLeft {
            ty: REMOTE_DEVICE_LEFT_NETWORK,
            reason,
            detail: 0,
            device,
            network: net,
        });
        let still_used = self
            .networks
            .iter()
            .any(|(h, n)| !self.forgotten(*h) && n.devices.iter().any(|d| d.0 == device));
        if !still_used {
            self.push(DeviceOnly {
                ty: REMOTE_DEVICE_DESTROYED,
                device,
            });
            self.forget.push(device);
        }
    }

    /// Everything of one player's in `net`: endpoint, chat control, device.
    fn drop_peer(&mut self, net: usize, device: usize, peer: PeerId, reason: u32) {
        let Some(n) = self.networks.get(&net) else { return };
        let ep = n.remote.iter().copied().find(|e| self.endpoints.get(e).is_some_and(|x| x.peer == Some(peer)));
        let chat = n.chats.iter().copied().find(|c| self.chats.get(c).is_some_and(|x| x.peer == Some(peer)));
        if let Some(ep) = ep {
            self.drop_endpoint(net, ep, reason);
        }
        if let Some(c) = chat {
            self.remote_chat_left(net, c, reason);
        }
        self.device_left(net, device, reason);
    }
    /// Tears a network down for the game: endpoints, then (for a requested leave)
    /// leave completed, then network destroyed.
    fn end_network(&mut self, lan: &Lan, net: usize, reason: u32, async_id: Option<usize>) {
        let Some(n) = self.networks.get(&net) else { return };
        let guid = n.guid.clone();
        let devices = n.devices.clone();
        let local = n.local_endpoint;
        for (device, peer) in devices {
            self.drop_peer(net, device, peer, reason);
        }
        let stray = self.networks[&net].remote.clone();
        for ep in stray {
            self.drop_endpoint(net, ep, reason);
        }
        if let Some(ep) = local {
            self.push(EndpointDestroyed {
                ty: ENDPOINT_DESTROYED,
                network: net,
                endpoint: ep,
                reason,
                detail: 0,
            });
            self.forget.push(ep);
        }
        self.chats_network_ended(lan, net, reason);
        if let Some(async_id) = async_id {
            self.push(LeaveCompleted {
                ty: LEAVE_NETWORK_COMPLETED,
                result: SUCCEEDED,
                detail: 0,
                network: net,
                async_id,
            });
        }
        self.push(NetworkDestroyed {
            ty: NETWORK_DESTROYED,
            reason,
            detail: 0,
            network: net,
        });
        self.forget.push(net);
        let invites = self.networks[&net].invitations.clone();
        self.forget.extend(invites);
        if let Some(n) = self.networks.get_mut(&net) {
            n.connected = false;
        }
        let _ = lan.leave(&guid);
        lan.close_socket(&format!("{SOCKET_PREFIX}{guid}"));
        log!("party-lan left network {guid}");
        self.log_traffic("left a network");
    }

    fn add_invitation(&mut self, net: usize, id: &str, creator: &str) -> usize {
        let id_c = cstring(id);
        let h = handle();
        let config = Box::new(InvitationConfig {
            identifier: id_c.as_ptr() as usize,
            revocability: 0,
            entity_count: 0,
            entity_ids: 0,
        });
        self.invitations.insert(
            h,
            Invitation {
                network: net,
                _id: id_c,
                creator: cstring(creator),
                config,
            },
        );
        if let Some(n) = self.networks.get_mut(&net) {
            n.invitations.push(h);
        }
        h
    }
}

// ---- LAN events and packets -> state changes ----

fn pump(s: &mut State) {
    let Some(lan) = net::lan() else { return };
    for ev in net::take(Front::Party) {
        if s.chat_used {
            // Voice rooms' channels, and players leaving them.
            crate::voice::on_event(lan, &ev);
        }
        match ev {
            Event::JoinDone { lobby, result } => {
                let Some(h) = s.network_of_guid(&lobby) else { continue };
                if s.networks[&h].connected {
                    continue;
                }
                let descriptor = s.networks[&h].descriptor;
                if result == JoinResult::Ok {
                    s.networks.get_mut(&h).unwrap().connected = true;
                    log!("party-lan connected to network {lobby}");
                    connected(s, h, descriptor, 0);
                    s.chats_network_connected(lan, h);
                } else {
                    log!("party-lan could not join network {lobby}: {result:?}");
                    let code = if result == JoinResult::Full {
                        NETWORK_NOT_JOINABLE
                    } else {
                        NETWORK_NO_LONGER_EXISTS
                    };
                    s.push(ConnectCompleted {
                        ty: CONNECT_TO_NETWORK_COMPLETED,
                        result: code,
                        detail: 0,
                        descriptor,
                        async_id: 0,
                        network: h,
                    });
                    s.chats_network_ended(lan, h, REASON_CREATION_FAILED);
                    s.push(NetworkDestroyed {
                        ty: NETWORK_DESTROYED,
                        reason: REASON_CREATION_FAILED,
                        detail: 0,
                        network: h,
                    });
                    s.forget.push(h);
                }
            }
            Event::MemberStatus {
                lobby,
                member,
                status,
            } => {
                let Some(h) = s.network_of_guid(&lobby) else { continue };
                if member == lan.me() && matches!(status, MemberStatus::Closed | MemberStatus::Kicked) {
                    let reason = if status == MemberStatus::Kicked {
                        REASON_KICKED
                    } else {
                        REASON_DISCONNECTED
                    };
                    if s.networks[&h].connected {
                        s.end_network(lan, h, reason, None);
                    }
                } else {
                    s.reconcile(lan, h);
                }
            }
            Event::LobbyUpdated { lobby } | Event::MemberUpdated { lobby, .. } => {
                if let Some(h) = s.network_of_guid(&lobby) {
                    s.reconcile(lan, h);
                }
            }
            Event::PeerLeft(_) => {
                let nets: Vec<usize> = s.networks.keys().copied().collect();
                for h in nets {
                    s.reconcile(lan, h);
                }
            }
            Event::ChanRequest { peer, socket } => {
                if let Some(guid) = socket.strip_prefix(SOCKET_PREFIX) {
                    if s.network_of_guid(guid).is_some() {
                        lan.accept(peer, &socket);
                    }
                }
            }
            _ => {}
        }
    }
    // Messages that waited for an endpoint go first, so each peer's reach the game in
    // the order they were sent.
    let started = Instant::now();
    let mut todo = s.take_early();
    while let Some(p) = lan.receive_where(None, |sock| sock.starts_with(SOCKET_PREFIX)) {
        todo.push_back((p, Instant::now()));
    }
    let mut reconciled = Vec::new();
    let mut reconcile = |s: &mut State, net: usize| s.reconcile(lan, net);
    for (p, came) in todo {
        receive(s, &mut reconcile, p, came, came < started, &mut reconciled);
    }
    if s.chat_used {
        chat::pump_chat(s, lan);
    }
    if s.traffic.last.is_none_or(|t| t.elapsed() >= TRAFFIC_EVERY) {
        s.log_traffic("so far");
    }
}

/// Hands the game one endpoint message from the LAN, or keeps it in `early` until its
/// sender's endpoint and ours are shown: a peer's first messages can beat the lobby
/// update that shows its endpoint (they come straight from it, the update through the
/// lobby's owner). Messages that wait longer than EARLY_WAIT are dropped. `reconcile`
/// brings a network in line with its lobby; `reconciled` is the networks already
/// reconciled this pump (once each is enough).
fn receive(
    s: &mut State,
    reconcile: &mut dyn FnMut(&mut State, usize),
    p: Packet,
    came: Instant,
    waited: bool,
    reconciled: &mut Vec<usize>,
) {
    let guid = &p.socket[SOCKET_PREFIX.len()..];
    let Some(net) = s.network_of_guid(guid) else {
        s.drop_message(&p, "its network is gone");
        return;
    };
    // Behind an earlier message from the same peer that's still waiting: it waits too.
    let behind = s.early_from.contains(&(p.peer, net));
    let find = |s: &State| {
        s.networks[&net]
            .remote
            .iter()
            .copied()
            .find(|e| s.endpoints.get(e).is_some_and(|ep| ep.peer == Some(p.peer)))
    };
    let mut sender = if behind { None } else { find(s) };
    if sender.is_none() && !behind && !reconciled.contains(&net) {
        reconciled.push(net);
        reconcile(s, net);
        sender = find(s);
    }
    let local = s.networks[&net].local_endpoint;
    let (Some(sender), Some(local)) = (sender, local) else {
        if came.elapsed() >= EARLY_WAIT {
            s.drop_message(&p, "no endpoint was shown for it in time");
        } else if s.early.len() >= EARLY_MAX {
            s.drop_message(&p, "too many messages are waiting for endpoints");
        } else {
            s.early_from.insert((p.peer, net));
            s.early.push_back((p, came));
        }
        return;
    };
    let Some((&flags, payload)) = p.data.split_first() else {
        s.drop_message(&p, "it was empty");
        return;
    };
    let bytes = payload.to_vec();
    s.traffic.received += 1;
    s.traffic.received_bytes += bytes.len() as u64;
    if waited {
        s.traffic.waited += 1;
    }
    trace("EndpointMessageReceived", || {
        format!("from={} bytes={} options={flags:#x} waited={waited}", p.peer, bytes.len())
    });
    {
        let size = bytes.len() as u32;
        s.push_with(
            MessageReceived {
                ty: ENDPOINT_MESSAGE_RECEIVED,
                network: net,
                sender,
                receiver_count: 1,
                receivers: 0,
                options: received_options(p.ordered, flags),
                size,
                buffer: 0,
            },
            bytes,
            vec![local],
            None,
            |c, b, p, _| {
                c.buffer = if size == 0 { 0 } else { b };
                c.receivers = p;
            },
        );
    }
}

/// The delivery options the game is told a message came with. Party reports how it was
/// delivered, which can add to what the sender asked for but never takes away: one that
/// came over the connection's stream was guaranteed and sequential; a datagram was
/// neither (senders only send datagrams for messages that asked for neither).
/// RequiredFragmentation is never set: PeerParty doesn't fragment messages as Party does.
fn received_options(ordered: bool, requested: u8) -> u32 {
    if ordered {
        SEND_GUARANTEED | SEND_SEQUENTIAL
    } else {
        u32::from(requested) & (SEND_GUARANTEED | SEND_SEQUENTIAL)
    }
}

/// Connected to a network: the game hears it, then gets its configuration.
fn connected(s: &mut State, net: usize, descriptor: Descriptor, async_id: usize) {
    s.push(ConnectCompleted {
        ty: CONNECT_TO_NETWORK_COMPLETED,
        result: SUCCEEDED,
        detail: 0,
        descriptor,
        async_id,
        network: net,
    });
    let config = &*s.networks[&net].config as *const NetConfig as usize;
    s.push(ConfigMadeAvailable {
        ty: NETWORK_CONFIGURATION_MADE_AVAILABLE,
        network: net,
        config,
    });
}

// ---- the exports ----

fn resolve(name: &str) -> Option<usize> {
    if !name.starts_with("Party") {
        return None;
    }
    if let Some(f) = chat::resolve(name) {
        return Some(f);
    }
    let f: usize = match name {
        "PartyInitialize" => party_initialize as *const () as usize,
        "PartyCleanup" => party_cleanup as *const () as usize,
        "PartyGetErrorMessage" => party_get_error_message as *const () as usize,
        "PartyStartProcessingStateChanges" => party_start_processing as *const () as usize,
        "PartyFinishProcessingStateChanges" => party_finish_processing as *const () as usize,
        "PartyGetRegions" => party_get_regions as *const () as usize,
        "PartyGetLocalDevice" => party_get_local_device as *const () as usize,
        "PartyCreateLocalUser" => party_create_local_user as *const () as usize,
        "PartyDestroyLocalUser" => party_destroy_local_user as *const () as usize,
        "PartyGetLocalUsers" => party_get_local_users as *const () as usize,
        "PartyLocalUserGetEntityId" => party_local_user_get_entity_id as *const () as usize,
        "PartyLocalUserUpdateEntityToken" => ok2 as *const () as usize,
        "PartyCreateNewNetwork" => party_create_new_network as *const () as usize,
        "PartyConnectToNetwork" => party_connect_to_network as *const () as usize,
        "PartyGetNetworks" => party_get_networks as *const () as usize,
        "PartySerializeNetworkDescriptor" => party_serialize_descriptor as *const () as usize,
        "PartyDeserializeNetworkDescriptor" => party_deserialize_descriptor as *const () as usize,
        "PartyNetworkAuthenticateLocalUser" => party_network_authenticate as *const () as usize,
        "PartyNetworkRemoveLocalUser" => party_network_remove_local_user as *const () as usize,
        "PartyNetworkCreateEndpoint" => party_network_create_endpoint as *const () as usize,
        "PartyNetworkDestroyEndpoint" => party_network_destroy_endpoint as *const () as usize,
        "PartyNetworkLeaveNetwork" => party_network_leave as *const () as usize,
        "PartyNetworkGetEndpoints" => party_network_get_endpoints as *const () as usize,
        "PartyNetworkFindEndpointByUniqueIdentifier" => party_network_find_endpoint as *const () as usize,
        "PartyNetworkGetNetworkDescriptor" => party_network_get_descriptor as *const () as usize,
        "PartyNetworkGetNetworkConfiguration" => party_network_get_configuration as *const () as usize,
        "PartyNetworkGetLocalUsers" => party_network_get_local_users as *const () as usize,
        "PartyNetworkGetDevices" => party_network_get_devices as *const () as usize,
        "PartyNetworkGetInvitations" => party_network_get_invitations as *const () as usize,
        "PartyNetworkCreateInvitation" => party_network_create_invitation as *const () as usize,
        "PartyNetworkGetDeviceConnectionType" => party_network_connection_type as *const () as usize,
        "PartyNetworkGetNetworkStatistics" => party_network_statistics as *const () as usize,
        "PartyEndpointSendMessage" => party_endpoint_send as *const () as usize,
        "PartyEndpointGetEntityId" => party_endpoint_get_entity_id as *const () as usize,
        "PartyEndpointIsLocal" => party_endpoint_is_local as *const () as usize,
        "PartyEndpointGetUniqueIdentifier" => party_endpoint_get_uid as *const () as usize,
        "PartyEndpointGetDevice" => party_endpoint_get_device as *const () as usize,
        "PartyEndpointGetLocalUser" => party_endpoint_get_local_user as *const () as usize,
        "PartyEndpointGetNetwork" => party_endpoint_get_network as *const () as usize,
        "PartyEndpointGetEndpointStatistics" => party_endpoint_statistics as *const () as usize,
        "PartyEndpointFlushMessages" => ok3 as *const () as usize,
        "PartyDeviceIsLocal" => party_device_is_local as *const () as usize,
        "PartyInvitationGetCreatorEntityId" => party_invitation_get_creator as *const () as usize,
        "PartyInvitationGetInvitationConfiguration" => party_invitation_get_configuration as *const () as usize,
        "PartyDeviceGetSharedPropertyKeys" | "PartyEndpointGetSharedPropertyKeys"
        | "PartyNetworkGetSharedPropertyKeys" | "PartyChatControlGetSharedPropertyKeys" => {
            no_items as *const () as usize
        }
        "PartyDeviceGetSharedProperty" | "PartyEndpointGetSharedProperty"
        | "PartyNetworkGetSharedProperty" | "PartyChatControlGetSharedProperty" => {
            no_property as *const () as usize
        }
        "PartyDeviceSetSharedProperties" | "PartyEndpointSetSharedProperties"
        | "PartyNetworkSetSharedProperties" | "PartyChatControlSetSharedProperties" => {
            ok4 as *const () as usize
        }
        "PartyDoWork" | "PartySetOption" | "PartySetWorkMode" | "PartySetThreadAffinityMask"
        | "PartySetMemoryCallbacks" | "PartySetProfilingCallbacksForMethodEntryExit" => {
            ok3 as *const () as usize
        }
        "PartyGetWorkMode" => party_get_work_mode as *const () as usize,
        "PartyGetThreadAffinityMask" => party_get_affinity as *const () as usize,
        "PartyGetMemoryCallbacks" | "PartyGetProfilingCallbacksForMethodEntryExit" => {
            two_nulls as *const () as usize
        }
        n if n.ends_with("GetCustomContext") => get_custom_context as *const () as usize,
        n if n.ends_with("SetCustomContext") => set_custom_context as *const () as usize,
        _ => {
            note(name);
            unsupported as *const () as usize
        }
    };
    Some(f)
}

static NOTED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

fn note(name: &str) {
    if NOTED.lock().unwrap().insert(name.to_string()) {
        crate::log::gap("party", &format!("{name} (not carried on the LAN, reports unsupported)"));
    }
}

// Generic answers. Party's C API returns its error code and writes outputs through
// pointers; on x64 extra arguments are harmless, so one body serves any arity.
extern "system" fn unsupported() -> u32 {
    E_UNSUPPORTED
}

extern "system" fn ok2(_: usize, _: usize) -> u32 {
    OK
}

extern "system" fn ok3(_: usize, _: usize, _: usize) -> u32 {
    OK
}

extern "system" fn ok4(_: usize, _: usize, _: usize, _: usize) -> u32 {
    OK
}

unsafe extern "system" fn no_items(_: usize, count: *mut u32, items: *mut usize) -> u32 {
    unsafe {
        put(count, 0);
        put(items, 0);
    }
    OK
}

/// No such property: an empty PARTY_DATA_BUFFER (all of it, pointer and size).
unsafe extern "system" fn no_property(_: usize, _: *const c_char, value: *mut DataBuffer) -> u32 {
    unsafe { put(value, DataBuffer { buffer: 0, len: 0 }) };
    OK
}

unsafe extern "system" fn two_nulls(a: *mut usize, b: *mut usize) -> u32 {
    unsafe {
        put(a, 0);
        put(b, 0);
    }
    OK
}

unsafe extern "system" fn party_get_work_mode(_: usize, _: u32, mode: *mut u32) -> u32 {
    unsafe { put(mode, 0) };
    OK
}

unsafe extern "system" fn party_get_affinity(_: u32, mask: *mut u64) -> u32 {
    unsafe { put(mask, u64::MAX) };
    OK
}

unsafe extern "system" fn get_custom_context(obj: usize, out: *mut usize) -> u32 {
    let v = STATE.lock().unwrap().contexts.get(&obj).copied().unwrap_or(0);
    unsafe { put(out, v) };
    OK
}

extern "system" fn set_custom_context(obj: usize, value: usize) -> u32 {
    STATE.lock().unwrap().contexts.insert(obj, value);
    OK
}

unsafe extern "system" fn party_initialize(title: *const c_char, out: *mut usize) -> u32 {
    let title = unsafe { text(title) };
    let mut s = STATE.lock().unwrap();
    if s.initialized {
        return E_BAD_ARG;
    }
    s.initialized = true;
    s.local_device = handle();
    let local = s.local_device;
    s.devices.insert(local, Device { peer: None });
    s.regions = vec![Region {
        name: fixed(REGION),
        rtt_ms: 1,
    }];
    s.push(ResultOnly {
        ty: REGIONS_CHANGED,
        result: SUCCEEDED,
        detail: 0,
    });
    drop(s);
    unsafe { put(out, PARTY_HANDLE) };
    let lan = net::lan().is_some();
    log!("party-lan PartyInitialize title={title} lan={lan}");
    OK
}

extern "system" fn party_cleanup(_: usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    if let Some(lan) = net::lan() {
        s.chats_cleanup(lan);
        for n in s.networks.values() {
            let _ = lan.leave(&n.guid);
        }
        for guid in s.hosted.keys() {
            let _ = lan.leave(guid);
        }
    }
    s.log_traffic("cleanup");
    *s = State::default();
    log!("party-lan PartyCleanup");
    OK
}

unsafe extern "system" fn party_get_error_message(err: u32, out: *mut *const c_char) -> u32 {
    let msg: &'static std::ffi::CStr = match err {
        OK => c"Success",
        E_UNSUPPORTED => {
            c"Not supported on PeerParty's LAN (kicking, transcription and text-to-speech aren't carried)"
        }
        E_BAD_HANDLE => c"Unknown handle",
        E_NO_BUFFER => c"No audio buffer is available",
        E_NETWORK_GONE => c"The network no longer exists",
        _ => c"Invalid argument",
    };
    unsafe { put(out, msg.as_ptr()) };
    OK
}

unsafe extern "system" fn party_start_processing(
    _: usize,
    count: *mut u32,
    changes: *mut usize,
) -> u32 {
    let _ = std::panic::catch_unwind(|| {
        if net::lan().is_some() && TICKS.fetch_add(1, Ordering::Relaxed) % 256 == 0 {
            crate::swap::patch_imports();
        }
    });
    let mut s = STATE.lock().unwrap();
    pump(&mut s);
    let (n, p) = s.start_batch();
    if n > 0 {
        let kinds: Vec<u32> = s
            .in_flight
            .back()
            .map(|b| {
                b.changes
                    .iter()
                    .map(|c| unsafe { *(c.ptr as *const u32) })
                    .filter(|t| *t != ENDPOINT_MESSAGE_RECEIVED && *t != DATA_BUFFERS_RETURNED)
                    .collect()
            })
            .unwrap_or_default();
        if !kinds.is_empty() {
            trace("state changes", || format!("{kinds:?}"));
        }
    }
    unsafe {
        put(count, n);
        put(changes, p);
    }
    OK
}

static TICKS: AtomicUsize = AtomicUsize::new(0);

/// The game returns some or all of the changes it got (from one batch or several, in
/// any order); each must come back once.
unsafe extern "system" fn party_finish_processing(_: usize, count: u32, changes: *const usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    if count == 0 {
        return OK;
    }
    // More than it holds can't be right, so the array isn't read past what it could be.
    if changes.is_null() || count as usize > s.held() {
        return E_BAD_ARG;
    }
    let returned: Vec<usize> = (0..count as usize)
        .map(|i| unsafe { changes.add(i).read_unaligned() })
        .collect();
    s.finish_batch(&returned);
    OK
}

unsafe extern "system" fn party_get_regions(_: usize, count: *mut u32, regions: *mut usize) -> u32 {
    let s = STATE.lock().unwrap();
    unsafe {
        put(count, s.regions.len() as u32);
        put(regions, if s.regions.is_empty() { 0 } else { s.regions.as_ptr() as usize });
    }
    OK
}

unsafe extern "system" fn party_get_local_device(_: usize, out: *mut usize) -> u32 {
    let d = STATE.lock().unwrap().local_device;
    unsafe { put(out, d) };
    OK
}

unsafe extern "system" fn party_create_local_user(
    _: usize,
    entity: *const c_char,
    _token: *const c_char,
    out: *mut usize,
) -> u32 {
    let entity = unsafe { text(entity) };
    let h = handle();
    STATE.lock().unwrap().users.insert(
        h,
        User {
            entity: cstring(&entity),
        },
    );
    unsafe { put(out, h) };
    log!("party-lan local user {entity}");
    OK
}

extern "system" fn party_destroy_local_user(_: usize, user: usize, async_id: usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    if s.users.remove(&user).is_none() {
        return E_BAD_HANDLE;
    }
    s.push(DestroyLocalUserCompleted {
        ty: DESTROY_LOCAL_USER_COMPLETED,
        result: SUCCEEDED,
        detail: 0,
        user,
        async_id,
    });
    OK
}

unsafe extern "system" fn party_get_local_users(_: usize, count: *mut u32, out: *mut usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    let users: Vec<usize> = s.users.keys().copied().collect();
    let (n, p) = s.list("users", users);
    unsafe {
        put(count, n);
        put(out, p);
    }
    OK
}

unsafe extern "system" fn party_local_user_get_entity_id(user: usize, out: *mut *const c_char) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(u) = s.users.get(&user) else {
        return E_BAD_HANDLE;
    };
    unsafe { put(out, u.entity.as_ptr()) };
    OK
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn party_create_new_network(
    _: usize,
    user: usize,
    config: *const NetConfig,
    _region_count: u32,
    _regions: usize,
    invitation: *const InvitationConfig,
    async_id: usize,
    descriptor_out: *mut Descriptor,
    applied_invitation: *mut c_char,
) -> u32 {
    let Some(lan) = net::lan() else {
        return E_UNSUPPORTED;
    };
    let config = if config.is_null() {
        NetConfig {
            max_users: 8,
            max_devices: 8,
            max_users_per_device: 1,
            max_devices_per_user: 1,
            max_endpoints_per_device: 1,
            direct_peer: 0,
        }
    } else {
        unsafe { config.read_unaligned() }
    };
    let invitation_id = if invitation.is_null() {
        String::new()
    } else {
        unsafe { text((*invitation).identifier as *const c_char) }
    };
    let invitation_id = if invitation_id.is_empty() {
        new_guid()
    } else {
        invitation_id
    };
    let guid = new_guid();
    let mut lobby = Lobby::new(NS);
    lobby.id = guid.clone();
    lobby.max_members = config.max_devices.clamp(1, 32);
    // A network is joined by its id, which the game hands out through its own lobby
    // or invite. Left public, a stranger could list every network and walk into any:
    // this way a public player gets in from a lobby they share with us, or invited.
    lobby.permission = peerparty_core::Permission::Friends;
    lobby.join_by_id = true;
    lobby.host_migration = true;
    if let Err(e) = lan.create_lobby(lobby) {
        log!("party-lan could not create a network: {e:?}");
        return E_UNSUPPORTED;
    }
    let descriptor = descriptor_for(&guid);
    unsafe { put(descriptor_out, descriptor) };
    if !applied_invitation.is_null() {
        let bytes: [u8; INVITATION_ID_BUFFER] = fixed(&invitation_id);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), applied_invitation.cast(), bytes.len())
        };
    }
    log!(
        "party-lan created network {guid} for {} devices (invitation {invitation_id})",
        config.max_devices
    );
    let mut s = STATE.lock().unwrap();
    s.hosted.insert(guid, (config, invitation_id.clone()));
    s.push_with(
        CreateNewNetworkCompleted {
            ty: CREATE_NEW_NETWORK_COMPLETED,
            result: SUCCEEDED,
            detail: 0,
            user,
            config,
            region_count: 0,
            regions: 0,
            async_id,
            descriptor,
            applied_invitation: 0,
        },
        Vec::new(),
        Vec::new(),
        Some(cstring(&invitation_id)),
        |c, _, _, t| c.applied_invitation = t,
    );
    OK
}

unsafe extern "system" fn party_connect_to_network(
    _: usize,
    descriptor: *const Descriptor,
    async_id: usize,
    out: *mut usize,
) -> u32 {
    let Some(lan) = net::lan() else {
        return E_UNSUPPORTED;
    };
    if descriptor.is_null() {
        return E_BAD_ARG;
    }
    let descriptor = unsafe { descriptor.read_unaligned() };
    let guid = fixed_str(&descriptor.id);
    let mut s = STATE.lock().unwrap();
    if let Some(h) = s.network_of_guid(&guid) {
        unsafe { put(out, h) };
        return OK;
    }
    let (config, invitation) = s.hosted.remove(&guid).unwrap_or((
        NetConfig {
            max_users: 32,
            max_devices: 32,
            max_users_per_device: 1,
            max_devices_per_user: 1,
            max_endpoints_per_device: 1,
            direct_peer: 0,
        },
        String::new(),
    ));
    let h = handle();
    s.networks.insert(
        h,
        Network {
            guid: guid.clone(),
            descriptor,
            config: Box::new(config),
            connected: false,
            user: 0,
            local_endpoint: None,
            remote: Vec::new(),
            invitations: Vec::new(),
            devices: Vec::new(),
            chats: Vec::new(),
            local_chats: Vec::new(),
            pending_chats: Vec::new(),
        },
    );
    unsafe { put(out, h) };
    let me = crate::playfab::entity_id();
    if !invitation.is_empty() {
        s.add_invitation(h, &invitation, &me);
    }
    if lan.is_owner(&guid) || lan.lobby(&guid).is_some() {
        s.networks.get_mut(&h).unwrap().connected = true;
        log!("party-lan connected to our network {guid}");
        connected(&mut s, h, descriptor, async_id);
    } else {
        log!("party-lan joining network {guid}");
        if let Err(e) = lan.join_by_id(NS, &guid) {
            log!("party-lan join {guid}: {e:?}");
        }
    }
    OK
}

unsafe extern "system" fn party_get_networks(_: usize, count: *mut u32, out: *mut usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    let nets: Vec<usize> = s.networks.keys().copied().collect();
    let (n, p) = s.list("networks", nets);
    unsafe {
        put(count, n);
        put(out, p);
    }
    OK
}

unsafe extern "system" fn party_serialize_descriptor(descriptor: *const Descriptor, out: *mut c_char) -> u32 {
    if descriptor.is_null() || out.is_null() {
        return E_BAD_ARG;
    }
    let guid = fixed_str(unsafe { &(*descriptor).id });
    let text = format!("{DESCRIPTOR_PREFIX}{guid}");
    let bytes: [u8; 449] = fixed(&text);
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast(), (text.len() + 1).min(bytes.len())) };
    OK
}

unsafe extern "system" fn party_deserialize_descriptor(text_in: *const c_char, out: *mut Descriptor) -> u32 {
    let text = unsafe { text(text_in) };
    let Some(guid) = text.strip_prefix(DESCRIPTOR_PREFIX) else {
        log!("party-lan descriptor not from peerparty: {text}");
        return E_BAD_ARG;
    };
    unsafe { put(out, descriptor_for(guid)) };
    OK
}

unsafe extern "system" fn party_network_authenticate(
    net: usize,
    user: usize,
    invitation: *const c_char,
    async_id: usize,
) -> u32 {
    let invitation = unsafe { text(invitation) };
    let mut s = STATE.lock().unwrap();
    let Some(n) = s.networks.get_mut(&net) else {
        return E_BAD_HANDLE;
    };
    n.user = user;
    if n.invitations.is_empty() && !invitation.is_empty() {
        let creator = String::new();
        s.add_invitation(net, &invitation, &creator);
    }
    s.push_with(
        AuthenticateCompleted {
            ty: AUTHENTICATE_LOCAL_USER_COMPLETED,
            result: SUCCEEDED,
            detail: 0,
            network: net,
            user,
            invitation: 0,
            async_id,
        },
        Vec::new(),
        Vec::new(),
        Some(cstring(&invitation)),
        |c, _, _, t| c.invitation = t,
    );
    OK
}

extern "system" fn party_network_remove_local_user(net: usize, user: usize, async_id: usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    if !s.networks.contains_key(&net) {
        return E_BAD_HANDLE;
    }
    s.push(LocalUserRemoved {
        ty: LOCAL_USER_REMOVED,
        network: net,
        user,
        reason: 0,
    });
    s.push(RemoveLocalUserCompleted {
        ty: REMOVE_LOCAL_USER_COMPLETED,
        result: SUCCEEDED,
        detail: 0,
        network: net,
        user,
        async_id,
    });
    OK
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn party_network_create_endpoint(
    net: usize,
    user: usize,
    _property_count: u32,
    _keys: usize,
    _values: usize,
    async_id: usize,
    out: *mut usize,
) -> u32 {
    let Some(lan) = net::lan() else {
        return E_UNSUPPORTED;
    };
    let mut s = STATE.lock().unwrap();
    let Some(n) = s.networks.get(&net) else {
        return E_BAD_HANDLE;
    };
    let guid = n.guid.clone();
    if let Some(ep) = n.local_endpoint {
        unsafe { put(out, ep) };
        return OK;
    }
    let entity = s
        .users
        .get(&user)
        .map(|u| u.entity.to_string_lossy().to_string())
        .unwrap_or_else(crate::playfab::entity_id);
    let ep = handle();
    let device = s.local_device;
    s.endpoints.insert(
        ep,
        Endpoint {
            network: net,
            peer: None,
            entity: cstring(&entity),
            uid: uid_of(lan.me()),
            device,
            user,
        },
    );
    s.networks.get_mut(&net).unwrap().local_endpoint = Some(ep);
    unsafe { put(out, ep) };
    s.push(CreateEndpointCompleted {
        ty: CREATE_ENDPOINT_COMPLETED,
        result: SUCCEEDED,
        detail: 0,
        network: net,
        user,
        async_id,
        endpoint: ep,
    });
    s.push(EndpointCreated {
        ty: ENDPOINT_CREATED,
        network: net,
        endpoint: ep,
    });
    let attr = Attr {
        key: ENTITY_KEY.into(),
        value: Value::Str(entity.clone()),
        public: true,
    };
    if let Err(e) = lan.update(&guid, None, &[attr], &[]) {
        log!("party-lan endpoint on {guid}: {e:?}");
    }
    log!("party-lan endpoint {entity} on network {guid}");
    s.reconcile(lan, net);
    OK
}

extern "system" fn party_network_destroy_endpoint(net: usize, ep: usize, async_id: usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    let Some(n) = s.networks.get_mut(&net) else {
        return E_BAD_HANDLE;
    };
    if n.local_endpoint != Some(ep) {
        return E_BAD_HANDLE;
    }
    n.local_endpoint = None;
    let guid = n.guid.clone();
    if let Some(lan) = net::lan() {
        let _ = lan.update(&guid, None, &[], &[ENTITY_KEY.to_string()]);
    }
    s.push(DestroyEndpointCompleted {
        ty: DESTROY_ENDPOINT_COMPLETED,
        result: SUCCEEDED,
        detail: 0,
        network: net,
        endpoint: ep,
        async_id,
    });
    s.push(EndpointDestroyed {
        ty: ENDPOINT_DESTROYED,
        network: net,
        endpoint: ep,
        reason: REASON_REQUESTED,
        detail: 0,
    });
    s.forget.push(ep);
    OK
}

extern "system" fn party_network_leave(net: usize, async_id: usize) -> u32 {
    let Some(lan) = net::lan() else {
        return E_UNSUPPORTED;
    };
    let mut s = STATE.lock().unwrap();
    if !s.networks.contains_key(&net) || s.forgotten(net) {
        return E_NETWORK_GONE;
    }
    s.end_network(lan, net, REASON_REQUESTED, Some(async_id));
    OK
}

unsafe extern "system" fn party_network_get_endpoints(net: usize, count: *mut u32, out: *mut usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    let Some(n) = s.networks.get(&net) else {
        return E_BAD_HANDLE;
    };
    let mut eps: Vec<usize> = n.local_endpoint.into_iter().collect();
    eps.extend(n.remote.iter().copied());
    let (c, p) = s.list("endpoints", eps);
    unsafe {
        put(count, c);
        put(out, p);
    }
    OK
}

unsafe extern "system" fn party_network_find_endpoint(net: usize, uid: u16, out: *mut usize) -> u32 {
    let s = STATE.lock().unwrap();
    let found = s
        .endpoints
        .iter()
        .find(|(_, e)| e.network == net && e.uid == uid)
        .map(|(h, _)| *h);
    match found {
        Some(h) => {
            unsafe { put(out, h) };
            OK
        }
        None => E_BAD_ARG,
    }
}

unsafe extern "system" fn party_network_get_descriptor(net: usize, out: *mut Descriptor) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(n) = s.networks.get(&net) else {
        return E_BAD_HANDLE;
    };
    unsafe { put(out, n.descriptor) };
    OK
}

unsafe extern "system" fn party_network_get_configuration(net: usize, out: *mut usize) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(n) = s.networks.get(&net) else {
        return E_BAD_HANDLE;
    };
    unsafe { put(out, &*n.config as *const NetConfig as usize) };
    OK
}

unsafe extern "system" fn party_network_get_local_users(net: usize, count: *mut u32, out: *mut usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    let Some(n) = s.networks.get(&net) else {
        return E_BAD_HANDLE;
    };
    let users: Vec<usize> = (n.user != 0).then_some(n.user).into_iter().collect();
    let (c, p) = s.list("network users", users);
    unsafe {
        put(count, c);
        put(out, p);
    }
    OK
}

unsafe extern "system" fn party_network_get_devices(net: usize, count: *mut u32, out: *mut usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    let Some(n) = s.networks.get(&net) else {
        return E_BAD_HANDLE;
    };
    let mut devices = vec![s.local_device];
    devices.extend(n.devices.iter().map(|d| d.0));
    let (c, p) = s.list("devices", devices);
    unsafe {
        put(count, c);
        put(out, p);
    }
    OK
}

unsafe extern "system" fn party_network_get_invitations(net: usize, count: *mut u32, out: *mut usize) -> u32 {
    let mut s = STATE.lock().unwrap();
    let Some(n) = s.networks.get(&net) else {
        return E_BAD_HANDLE;
    };
    let invites = n.invitations.clone();
    let (c, p) = s.list("invitations", invites);
    unsafe {
        put(count, c);
        put(out, p);
    }
    OK
}

unsafe extern "system" fn party_network_create_invitation(
    net: usize,
    user: usize,
    config: *const InvitationConfig,
    async_id: usize,
    out: *mut usize,
) -> u32 {
    let id = if config.is_null() {
        String::new()
    } else {
        unsafe { text((*config).identifier as *const c_char) }
    };
    let id = if id.is_empty() { new_guid() } else { id };
    let mut s = STATE.lock().unwrap();
    if !s.networks.contains_key(&net) {
        return E_BAD_HANDLE;
    }
    let creator = s
        .users
        .get(&user)
        .map(|u| u.entity.to_string_lossy().to_string())
        .unwrap_or_default();
    let inv = s.add_invitation(net, &id, &creator);
    unsafe { put(out, inv) };
    s.push(CreateInvitationCompleted {
        ty: CREATE_INVITATION_COMPLETED,
        result: SUCCEEDED,
        detail: 0,
        network: net,
        user,
        async_id,
        invitation: inv,
    });
    s.push(InvitationCreated {
        ty: INVITATION_CREATED,
        network: net,
        invitation: inv,
    });
    OK
}

unsafe extern "system" fn party_network_connection_type(_: usize, _: usize, out: *mut u32) -> u32 {
    // PARTY_DEVICE_CONNECTION_TYPE_DIRECT_PEER_CONNECTION
    unsafe { put(out, 1) };
    OK
}

unsafe extern "system" fn party_network_statistics(_: usize, count: u32, _types: usize, values: *mut u64) -> u32 {
    for i in 0..count as usize {
        unsafe { put(values.add(i), 0) };
    }
    OK
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn party_endpoint_send(
    ep: usize,
    target_count: u32,
    targets: *const usize,
    options: u32,
    _queuing: usize,
    buffer_count: u32,
    buffers: *const DataBuffer,
    message_id: usize,
) -> u32 {
    let Some(lan) = net::lan() else {
        return E_UNSUPPORTED;
    };
    let mut s = STATE.lock().unwrap();
    let Some(e) = s.endpoints.get(&ep) else {
        return E_BAD_HANDLE;
    };
    // Only our own endpoints send: a remote one's messages would go out as if from us.
    if e.peer.is_some() {
        log!("party-lan PartyEndpointSendMessage from a remote endpoint {ep:#x}: refused");
        return E_BAD_ARG;
    }
    let network = e.network;
    let Some(n) = s.networks.get(&network) else {
        return E_NETWORK_GONE;
    };
    if buffer_count > 0 && buffers.is_null() {
        return E_BAD_ARG;
    }
    let peers: Vec<PeerId> = if target_count == 0 || targets.is_null() {
        n.remote
            .iter()
            .filter_map(|r| s.endpoints.get(r).and_then(|x| x.peer))
            .collect()
    } else {
        (0..target_count as usize)
            .filter_map(|i| {
                let t = unsafe { targets.add(i).read_unaligned() };
                s.endpoints.get(&t).and_then(|x| x.peer)
            })
            .collect()
    };
    let bufs: Vec<DataBuffer> = (0..buffer_count as usize)
        .map(|i| unsafe { buffers.add(i).read_unaligned() })
        .collect();
    let mut data = vec![(options & (SEND_GUARANTEED | SEND_SEQUENTIAL)) as u8];
    for b in &bufs {
        if b.buffer != 0 && b.len > 0 {
            data.extend_from_slice(unsafe {
                std::slice::from_raw_parts(b.buffer as *const u8, b.len as usize)
            });
        }
    }
    let socket = format!("{SOCKET_PREFIX}{}", n.guid);
    // Sequential messages must reach each target in the order they were sent, relative
    // to all its other sequential messages, guaranteed or not. Datagrams can't keep that:
    // one sent right after a guaranteed message overtakes it (the datagram leaves at once,
    // the frame waits for the connection's writer thread), and datagrams can reorder
    // among themselves. So only messages that are neither guaranteed nor sequential go as
    // datagrams; the rest ride the connection's ordered stream. The flags byte still says
    // what the game asked for, so receivers are unchanged.
    let ordered = options & (SEND_GUARANTEED | SEND_SEQUENTIAL) != 0;
    let bytes = (data.len() - 1) as u64;
    let count = peers.len() as u64;
    trace("PartyEndpointSendMessage", || {
        format!("to={count} bytes={bytes} options={options:#x} ordered={ordered}")
    });
    let mut sent = 0;
    for p in peers {
        // Held counts as sent: it goes as soon as the channel opens.
        match lan.send(p, &socket, 0, &data, ordered, true, true) {
            SendResult::Sent | SendResult::Held => sent += 1,
            SendResult::Dropped | SendResult::NoChannel => s.traffic.not_sent += 1,
        }
    }
    s.traffic.sent += sent;
    s.traffic.sent_bytes += bytes * sent;
    if ordered {
        s.traffic.sent_ordered += sent;
    }
    if options & SEND_DONT_COPY != 0 {
        // Copied above, so the game can have its buffers back at once.
        s.buffers_returned(network, ep, &bufs, message_id);
    }
    OK
}

unsafe extern "system" fn party_endpoint_get_entity_id(ep: usize, out: *mut *const c_char) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(e) = s.endpoints.get(&ep) else {
        return E_BAD_HANDLE;
    };
    unsafe { put(out, e.entity.as_ptr()) };
    OK
}

unsafe extern "system" fn party_endpoint_is_local(ep: usize, out: *mut u8) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(e) = s.endpoints.get(&ep) else {
        return E_BAD_HANDLE;
    };
    unsafe { put(out, u8::from(e.peer.is_none())) };
    OK
}

unsafe extern "system" fn party_endpoint_get_uid(ep: usize, out: *mut u16) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(e) = s.endpoints.get(&ep) else {
        return E_BAD_HANDLE;
    };
    unsafe { put(out, e.uid) };
    OK
}

unsafe extern "system" fn party_endpoint_get_device(ep: usize, out: *mut usize) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(e) = s.endpoints.get(&ep) else {
        return E_BAD_HANDLE;
    };
    unsafe { put(out, e.device) };
    OK
}

unsafe extern "system" fn party_endpoint_get_local_user(ep: usize, out: *mut usize) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(e) = s.endpoints.get(&ep) else {
        return E_BAD_HANDLE;
    };
    unsafe { put(out, e.user) };
    OK
}

unsafe extern "system" fn party_endpoint_get_network(ep: usize, out: *mut usize) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(e) = s.endpoints.get(&ep) else {
        return E_BAD_HANDLE;
    };
    unsafe { put(out, e.network) };
    OK
}

unsafe extern "system" fn party_endpoint_statistics(
    _: usize,
    _target_count: u32,
    _targets: usize,
    count: u32,
    _types: usize,
    values: *mut u64,
) -> u32 {
    for i in 0..count as usize {
        unsafe { put(values.add(i), 0) };
    }
    OK
}

unsafe extern "system" fn party_device_is_local(device: usize, out: *mut u8) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(d) = s.devices.get(&device) else {
        return E_BAD_HANDLE;
    };
    unsafe { put(out, u8::from(d.peer.is_none())) };
    OK
}

unsafe extern "system" fn party_invitation_get_creator(inv: usize, out: *mut *const c_char) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(i) = s.invitations.get(&inv) else {
        return E_BAD_HANDLE;
    };
    unsafe { put(out, i.creator.as_ptr()) };
    OK
}

unsafe extern "system" fn party_invitation_get_configuration(inv: usize, out: *mut usize) -> u32 {
    let s = STATE.lock().unwrap();
    let Some(i) = s.invitations.get(&inv) else {
        return E_BAD_HANDLE;
    };
    let _ = i.network;
    unsafe { put(out, &*i.config as *const InvitationConfig as usize) };
    OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_party_h() {
        let p = std::mem::size_of::<usize>();
        assert_eq!(std::mem::size_of::<Descriptor>(), 358);
        assert_eq!(std::mem::size_of::<NetConfig>(), 24);
        assert_eq!(std::mem::size_of::<Region>(), 24);
        assert_eq!(std::mem::offset_of!(CreateNewNetworkCompleted, user), 12_usize.next_multiple_of(p));
        assert_eq!(
            std::mem::offset_of!(MessageReceived, buffer),
            if p == 8 { 48 } else { 28 }
        );
        assert_eq!(
            std::mem::offset_of!(ConnectCompleted, async_id),
            (12 + 358_usize).next_multiple_of(p)
        );
    }

    #[test]
    fn descriptors_round_trip() {
        let guid = new_guid();
        let d = descriptor_for(&guid);
        let mut out = [0 as c_char; 449];
        assert_eq!(unsafe { party_serialize_descriptor(&d, out.as_mut_ptr()) }, OK);
        let text = unsafe { text(out.as_ptr()) };
        assert_eq!(text, format!("PP1-{guid}"));
        let mut back = descriptor_for("x");
        let c = cstring(&text);
        assert_eq!(unsafe { party_deserialize_descriptor(c.as_ptr(), &mut back) }, OK);
        assert_eq!(fixed_str(&back.id), guid);
        assert_eq!(fixed_str(&back.region), REGION);
        assert!(!text.contains('|'));
    }

    const NET: usize = 0x100;
    const LOCAL: usize = 0x110;

    /// A state with one connected network, our endpoint shown and no remote ones.
    fn with_network() -> State {
        let mut s = State::default();
        s.networks.insert(
            NET,
            Network {
                guid: "g".into(),
                descriptor: descriptor_for("g"),
                config: Box::default(),
                connected: true,
                user: 0,
                local_endpoint: Some(LOCAL),
                remote: Vec::new(),
                invitations: Vec::new(),
                devices: Vec::new(),
                chats: Vec::new(),
                local_chats: Vec::new(),
                pending_chats: Vec::new(),
            },
        );
        s
    }

    /// Shows `peer`'s endpoint in the network.
    fn show(s: &mut State, peer: PeerId) -> usize {
        let h = 0x200 + 0x10 * s.endpoints.len();
        s.endpoints.insert(
            h,
            Endpoint {
                network: NET,
                peer: Some(peer),
                entity: cstring("e"),
                uid: uid_of(peer),
                device: 0,
                user: 0,
            },
        );
        s.networks.get_mut(&NET).unwrap().remote.push(h);
        h
    }

    fn packet(peer: PeerId, n: u8, ordered: bool) -> Packet {
        Packet {
            peer,
            socket: format!("{SOCKET_PREFIX}g"),
            channel: 0,
            data: vec![if ordered { 1 } else { 0 }, n],
            ordered,
        }
    }

    /// The endpoint messages queued for the game: (sender, first payload byte, options).
    fn delivered(s: &State) -> Vec<(usize, u8, u32)> {
        s.queue
            .iter()
            .filter(|c| unsafe { *(c.ptr as *const u32) } == ENDPOINT_MESSAGE_RECEIVED)
            .map(|c| {
                let m = unsafe { &*(c.ptr as *const MessageReceived) };
                assert_eq!(m.network, NET);
                assert_eq!(m.receiver_count, 1);
                assert_eq!(unsafe { *(m.receivers as *const usize) }, LOCAL);
                assert_eq!(m.size, 1);
                (m.sender, unsafe { *(m.buffer as *const u8) }, m.options)
            })
            .collect()
    }

    /// What pump does with the messages that waited: each gets another try.
    fn retry(s: &mut State, reconcile: &mut dyn FnMut(&mut State, usize)) {
        let mut reconciled = Vec::new();
        for (p, came) in s.take_early() {
            receive(s, reconcile, p, came, true, &mut reconciled);
        }
    }

    const A: PeerId = PeerId([0xA; 16]);
    const B: PeerId = PeerId([0xB; 16]);

    #[test]
    fn early_messages_wait_for_their_senders_endpoint() {
        let mut s = with_network();
        let mut calls = 0;
        let mut reconcile = |_: &mut State, net: usize| {
            assert_eq!(net, NET);
            calls += 1;
        };
        let mut reconciled = Vec::new();
        let now = Instant::now();
        receive(&mut s, &mut reconcile, packet(A, 1, true), now, false, &mut reconciled);
        receive(&mut s, &mut reconcile, packet(A, 2, true), now, false, &mut reconciled);
        // Waiting, in order; the network was reconciled once for both.
        assert!(delivered(&s).is_empty());
        assert_eq!(s.early.len(), 2);
        assert_eq!(reconciled, vec![NET]);
        let a = show(&mut s, A);
        retry(&mut s, &mut reconcile);
        assert_eq!(calls, 1);
        assert!(s.early.is_empty());
        let both = SEND_GUARANTEED | SEND_SEQUENTIAL;
        assert_eq!(delivered(&s), vec![(a, 1, both), (a, 2, both)]);
        assert_eq!(s.traffic.received, 2);
        assert_eq!(s.traffic.waited, 2);
        assert_eq!(s.traffic.dropped, 0);
    }

    #[test]
    fn a_reconcile_that_shows_the_sender_delivers_at_once() {
        let mut s = with_network();
        let mut shown = None;
        let mut reconcile = |s: &mut State, _: usize| shown = Some(show(s, A));
        let mut reconciled = Vec::new();
        receive(&mut s, &mut reconcile, packet(A, 7, false), Instant::now(), false, &mut reconciled);
        assert!(s.early.is_empty());
        // A datagram was neither guaranteed nor sequential.
        assert_eq!(delivered(&s), vec![(shown.unwrap(), 7, 0)]);
        assert_eq!(s.traffic.waited, 0);
    }

    #[test]
    fn later_messages_queue_behind_a_waiting_one_from_the_same_peer() {
        let mut s = with_network();
        let mut reconcile = |_: &mut State, _: usize| {};
        let mut reconciled = Vec::new();
        let now = Instant::now();
        receive(&mut s, &mut reconcile, packet(A, 1, true), now, false, &mut reconciled);
        let a = show(&mut s, A);
        let b = show(&mut s, B);
        // A's endpoint is shown now, but its first message hasn't gone yet: the second
        // waits behind it. B's isn't held up by A's.
        receive(&mut s, &mut reconcile, packet(A, 2, true), now, false, &mut reconciled);
        receive(&mut s, &mut reconcile, packet(B, 3, true), now, false, &mut reconciled);
        let both = SEND_GUARANTEED | SEND_SEQUENTIAL;
        assert_eq!(delivered(&s), vec![(b, 3, both)]);
        assert_eq!(s.early.len(), 2);
        retry(&mut s, &mut reconcile);
        assert_eq!(delivered(&s), vec![(b, 3, both), (a, 1, both), (a, 2, both)]);
    }

    #[test]
    fn early_messages_are_dropped_after_early_wait() {
        let mut s = with_network();
        let mut reconcile = |_: &mut State, _: usize| {};
        let mut reconciled = Vec::new();
        let Some(long_ago) = Instant::now().checked_sub(EARLY_WAIT + Duration::from_secs(1)) else {
            return;
        };
        receive(&mut s, &mut reconcile, packet(A, 1, true), long_ago, false, &mut reconciled);
        assert!(s.early.is_empty());
        assert_eq!(s.traffic.dropped, 1);
        // One that's waited a while but not that long still waits.
        let a_bit_ago = Instant::now().checked_sub(EARLY_WAIT / 2).unwrap_or_else(Instant::now);
        receive(&mut s, &mut reconcile, packet(A, 2, true), a_bit_ago, false, &mut reconciled);
        assert_eq!(s.early.len(), 1);
        assert_eq!(s.traffic.dropped, 1);
    }

    #[test]
    fn at_most_early_max_messages_wait() {
        let mut s = with_network();
        let mut reconcile = |_: &mut State, _: usize| {};
        let mut reconciled = Vec::new();
        let now = Instant::now();
        for i in 0..EARLY_MAX {
            receive(&mut s, &mut reconcile, packet(A, i as u8, true), now, false, &mut reconciled);
        }
        assert_eq!(s.early.len(), EARLY_MAX);
        assert_eq!(s.traffic.dropped, 0);
        receive(&mut s, &mut reconcile, packet(B, 0, true), now, false, &mut reconciled);
        assert_eq!(s.early.len(), EARLY_MAX);
        assert_eq!(s.traffic.dropped, 1);
    }

    #[test]
    fn a_full_early_queue_drains_in_order() {
        let mut s = with_network();
        let mut reconcile = |_: &mut State, _: usize| {};
        let mut reconciled = Vec::new();
        let now = Instant::now();
        for i in 0..EARLY_MAX {
            let peer = if i % 2 == 0 { A } else { B };
            receive(&mut s, &mut reconcile, packet(peer, i as u8, true), now, false, &mut reconciled);
        }
        assert_eq!(s.early_from.len(), 2);
        let a = show(&mut s, A);
        retry(&mut s, &mut reconcile);
        // A's all went, in order; B's all still wait, behind B's first.
        let got = delivered(&s);
        assert_eq!(got.len(), EARLY_MAX / 2);
        assert!(got.iter().enumerate().all(|(i, m)| *m == (a, (2 * i) as u8, SEND_GUARANTEED | SEND_SEQUENTIAL)));
        assert_eq!(s.early.len(), EARLY_MAX / 2);
        assert!(s.early_from.contains(&(B, NET)) && !s.early_from.contains(&(A, NET)));
        // A message from A now goes straight through: nothing of A's waits.
        receive(&mut s, &mut reconcile, packet(A, 9, true), now, false, &mut reconciled);
        assert_eq!(delivered(&s).len(), EARLY_MAX / 2 + 1);
    }

    #[test]
    fn messages_for_a_gone_network_or_empty_ones_are_dropped() {
        let mut s = with_network();
        let mut reconcile = |_: &mut State, _: usize| {};
        let mut reconciled = Vec::new();
        let mut p = packet(A, 1, true);
        p.socket = format!("{SOCKET_PREFIX}other");
        receive(&mut s, &mut reconcile, p, Instant::now(), false, &mut reconciled);
        show(&mut s, A);
        let mut p = packet(A, 1, true);
        p.data.clear();
        receive(&mut s, &mut reconcile, p, Instant::now(), false, &mut reconciled);
        assert!(delivered(&s).is_empty());
        assert!(s.early.is_empty());
        assert_eq!(s.traffic.dropped, 2);
    }

    #[test]
    fn received_options_say_how_a_message_came() {
        let both = SEND_GUARANTEED | SEND_SEQUENTIAL;
        assert_eq!(received_options(true, 0), both);
        assert_eq!(received_options(true, SEND_GUARANTEED as u8), both);
        assert_eq!(received_options(false, 0), 0);
        // Never more than the sender asked for over a datagram, and never other bits.
        assert_eq!(received_options(false, SEND_SEQUENTIAL as u8), SEND_SEQUENTIAL);
        assert_eq!(received_options(false, 0xFF), both);
    }

    fn change(s: &mut State) -> usize {
        s.push(ResultOnly {
            ty: REGIONS_CHANGED,
            result: SUCCEEDED,
            detail: 0,
        });
        s.queue.last().unwrap().ptr
    }

    fn array(n: u32, p: usize) -> Vec<usize> {
        (0..n as usize).map(|i| unsafe { *(p as *const usize).add(i) }).collect()
    }

    #[test]
    fn batches_can_be_held_and_returned_out_of_order() {
        let mut s = with_network();
        assert_eq!(s.start_batch(), (0, 0));
        assert!(s.in_flight.is_empty());
        let c1 = change(&mut s);
        let c2 = change(&mut s);
        let (n, p) = s.start_batch();
        assert_eq!(array(n, p), vec![c1, c2]);
        // The second batch destroys the network.
        let c3 = change(&mut s);
        s.forget.push(NET);
        let (n3, p3) = s.start_batch();
        assert_eq!(array(n3, p3), vec![c3]);
        assert_eq!(s.held(), 3);
        // Both arrays still hold what they did.
        assert_eq!(array(n, p), vec![c1, c2]);
        assert!(s.forgotten(NET));
        // Back out of order, a pointer it never got, and one returned twice: the network
        // stays until every batch up to the one that destroyed it is back.
        s.finish_batch(&[c3, 0x1234]);
        assert!(s.networks.contains_key(&NET));
        assert_eq!(s.in_flight.len(), 2);
        s.finish_batch(&[c2, c3]);
        assert!(s.networks.contains_key(&NET));
        assert_eq!(s.held(), 1);
        s.finish_batch(&[c1]);
        assert!(s.in_flight.is_empty());
        assert!(!s.networks.contains_key(&NET));
        assert!(!s.forgotten(NET));
    }

    #[test]
    fn the_oldest_batch_goes_past_max_batches() {
        let mut s = with_network();
        change(&mut s);
        s.forget.push(NET);
        s.start_batch();
        for _ in 0..MAX_BATCHES {
            change(&mut s);
            s.start_batch();
        }
        assert_eq!(s.in_flight.len(), MAX_BATCHES);
        assert!(s.batches_dropped);
        assert!(!s.networks.contains_key(&NET));
    }

    #[test]
    fn lists_stay_valid_until_the_next_batch() {
        let mut s = State::default();
        assert_eq!(s.list("a", Vec::new()), (0, 0));
        let (n1, p1) = s.list("a", vec![1, 2]);
        let (n2, p2) = s.list("a", vec![3]);
        let (_, p3) = s.list("b", vec![1, 2]);
        assert_ne!(p1, p2);
        // The same items again reuse the array.
        assert_eq!(p3, p1);
        assert_eq!(s.lists.len(), 2);
        // An earlier array of the same kind is still there.
        assert_eq!(array(n1, p1), vec![1, 2]);
        assert_eq!(array(n2, p2), vec![3]);
        s.start_batch();
        assert!(s.lists.is_empty());
    }

    #[test]
    fn dont_copy_sends_get_their_buffers_back() {
        let p = std::mem::size_of::<usize>();
        assert_eq!(std::mem::size_of::<DataBuffer>(), 2 * p);
        assert_eq!(std::mem::offset_of!(DataBuffersReturned, network), p);
        assert_eq!(std::mem::offset_of!(DataBuffersReturned, sender), 2 * p);
        assert_eq!(std::mem::offset_of!(DataBuffersReturned, buffer_count), 3 * p);
        assert_eq!(std::mem::offset_of!(DataBuffersReturned, buffers), 4 * p);
        assert_eq!(std::mem::offset_of!(DataBuffersReturned, message_id), 5 * p);
        let mut s = State::default();
        let one = [1u8; 3];
        let two = [2u8; 5];
        let bufs = [
            DataBuffer { buffer: one.as_ptr() as usize, len: 3 },
            DataBuffer { buffer: two.as_ptr() as usize, len: 5 },
        ];
        s.buffers_returned(NET, LOCAL, &bufs, 0xBEEF);
        let c = unsafe { &*(s.queue[0].ptr as *const DataBuffersReturned) };
        assert_eq!(c.ty, DATA_BUFFERS_RETURNED);
        assert_eq!((c.network, c.sender, c.message_id), (NET, LOCAL, 0xBEEF));
        assert_eq!(c.buffer_count, 2);
        for (i, b) in bufs.iter().enumerate() {
            let back = unsafe { (c.buffers as *const DataBuffer).add(i).read_unaligned() };
            assert_eq!((back.buffer, back.len), (b.buffer, b.len));
        }
        s.buffers_returned(NET, LOCAL, &[], 0);
        let c = unsafe { &*(s.queue[1].ptr as *const DataBuffersReturned) };
        assert_eq!((c.buffer_count, c.buffers), (0, 0));
    }
}
