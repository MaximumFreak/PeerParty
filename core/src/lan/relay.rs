//! A connection that runs through another player: when only the link between two players
//! breaks (A and the lobby's owner O) while both still reach a third (B, in a lobby with
//! both), A's and O's meshes connect to each other again through B. B carries the bytes
//! of that connection as `Msg::Relay` on its own two connections and can't read them:
//! the two ends agree on keys first (an X25519 exchange, each half signed with the
//! end's identity key, the key its id is the hash of) and everything after is sealed
//! (ChaCha20-Poly1305, numbered each way), so B sees only how much passes. The
//! connection itself is an ordinary mesh connection, its hellos and proofs end to end
//! too, and the two pick up their session where it left (`link`) as after any dropped
//! connection: the game sees nothing. It ranks below every direct path (`lan::rank`), so
//! a direct one that comes back takes over as a better path does. Each end's mesh talks
//! to a loopback socket (as for an internet tunnel): the side that starts it has its
//! mesh dial a listener of ours, the other connects to its own mesh.
//!
//! Either side may start one: the lower id first, the higher a moment later
//! (RELAY_AFTER_HIGHER), and when both did, the lower id's goes ahead. A link can die
//! with only one side noticing for a while (the other's socket still looks open): an
//! offer names the connection its sender lost (both handshake numbers, signed), and
//! when that is the one we still have, ours is gone too, so its place is kept from
//! then (`Inner::lost`) instead of waiting until we notice. An offer kept and sent again
//! later names a connection we no longer have, and changes nothing.
//!
//! Without it, a member cut off from the owner alone was out of the lobby (the owner
//! dropped it once LINK_GRACE was up, `Msg::OwnerCheck` told it so); that still happens
//! when no member can carry the connection.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Weak;
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::{Inner, Role, State, Trust, TunnelEntry, rank};
use crate::id::PeerId;
use crate::inet;
use crate::inet::crypto::{Ephemeral, Keys};
use crate::inet::public;
use crate::wire::Msg;

/// How long a peer's connection has been down before we try reaching it through another
/// player: a short break mends itself first.
const RELAY_AFTER: Duration = Duration::from_millis(500);
/// The higher id waits this long instead: the lower id's offer usually comes first.
const RELAY_AFTER_HIGHER: Duration = Duration::from_millis(1500);
/// How long one player in between gets to bring the connection up before the next is
/// tried.
const RELAY_TRY: Duration = Duration::from_secs(3);
/// Connections we carry for others, and that end here, at most.
const HOPS_MAX: usize = 32;
const ENDS_MAX: usize = 32;
/// Bytes read off our mesh's side of a relayed connection at a time.
const CHUNK: usize = 32 * 1024;
/// While the connection to the player in between has this much not yet written, we stop
/// reading our side for a moment: the connection runs at the pace of its slower half.
const BACKLOG: usize = 1 << 20;
/// A player in between whose connection to the far end has this much not yet written
/// ends the relayed connection rather than queue without end.
const HOP_QUEUE_MAX: usize = 8 << 20;

/// One relayed connection that ends here: (the player in between, the far end, stream).
pub(super) type EndKey = (PeerId, PeerId, u64);

/// What a `Msg::Relay`'s data is between the two ends: the starter's offer, the far
/// end's answer, then sealed bytes of the mesh connection.
const OFFER: u8 = 1;
const ANSWER: u8 = 2;
const DATA: u8 = 3;
/// [OFFER][ephemeral key and nonce, 48][sender's handshake number, 16][receiver's, 16][signature, 64]
const OFFER_LEN: usize = 1 + 48 + 16 + 16 + 64;
/// [ANSWER][ephemeral key and nonce, 48][signature, 64]
const ANSWER_LEN: usize = 1 + 48 + 64;

/// A relayed connection that ends here.
struct End {
    /// What goes to our mesh's side of it.
    tx: Sender<Vec<u8>>,
    crypt: Crypt,
}

enum Crypt {
    /// We offered: the far end's answer keys it. Nothing goes out before.
    Offered(Ephemeral),
    /// Each way's next message number.
    Keyed { keys: Keys, sent: u64, got: u64 },
}

/// What both ends of one relayed connection feed the key exchange: the connection is
/// between these two, starter first, on this stream, and no other.
fn psk(starter: PeerId, far: PeerId, stream: u64) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"peerparty relay v1 psk")
        .chain_update(starter.0)
        .chain_update(far.0)
        .chain_update(stream.to_le_bytes())
        .finalize()
        .into()
}

fn transcript(kind: &[u8], game: &str, from: PeerId, to: PeerId, stream: u64, parts: &[&[u8]]) -> Vec<u8> {
    let mut t = kind.to_vec();
    t.extend_from_slice(&(game.len() as u32).to_le_bytes());
    t.extend_from_slice(game.as_bytes());
    t.extend_from_slice(&from.0);
    t.extend_from_slice(&to.0);
    t.extend_from_slice(&stream.to_le_bytes());
    for p in parts {
        t.extend_from_slice(p);
    }
    t
}

/// What an offer signs: the game, who to whom, the stream, its half of the exchange,
/// and the connection between the two it lost (its handshake numbers, sender's first).
fn offer_transcript(game: &str, from: PeerId, to: PeerId, stream: u64, blob: &[u8; 48], lost: (&[u8; 16], &[u8; 16])) -> Vec<u8> {
    transcript(b"offer", game, from, to, stream, &[blob, lost.0, lost.1])
}

/// What an answer signs: the same, with both halves of the exchange.
fn answer_transcript(game: &str, from: PeerId, to: PeerId, stream: u64, blob: &[u8; 48], offer: &[u8; 48]) -> Vec<u8> {
    transcript(b"answer", game, from, to, stream, &[blob, offer])
}

/// An offer's parts: (blob, sender's number, receiver's number, signature).
type Offer = ([u8; 48], [u8; 16], [u8; 16], [u8; 64]);

fn parse_offer(d: &[u8]) -> Option<Offer> {
    if d.len() != OFFER_LEN || d[0] != OFFER {
        return None;
    }
    Some((d[1..49].try_into().ok()?, d[49..65].try_into().ok()?, d[65..81].try_into().ok()?, d[81..].try_into().ok()?))
}

fn parse_answer(d: &[u8]) -> Option<([u8; 48], [u8; 64])> {
    if d.len() != ANSWER_LEN || d[0] != ANSWER {
        return None;
    }
    Some((d[1..49].try_into().ok()?, d[49..].try_into().ok()?))
}

#[derive(Default)]
pub(super) struct Relays {
    /// Connections that end here.
    ends: HashMap<EndKey, End>,
    /// Connections we carry between two others: (lower id, higher id, stream).
    hops: HashSet<(PeerId, PeerId, u64)>,
    /// Peers we are trying to reach through another, by the far end.
    tries: HashMap<PeerId, Try>,
    /// Relay messages that came, handled outside `State::handle` (`drain`), in order.
    pub(super) inbox: Vec<(PeerId, Msg)>,
}

struct Try {
    /// The player in between now, and the stream (None: nobody is left to try).
    now: Option<(PeerId, u64)>,
    since: Instant,
    tried: Vec<PeerId>,
}

/// A relayed connection to start (`Lan::tick` dials it once the state is unlocked).
pub(super) struct Start {
    pub far: PeerId,
    pub key: EndKey,
    pub listener: TcpListener,
    pub rx: Receiver<Vec<u8>>,
}

fn hop_key(a: PeerId, b: PeerId, stream: u64) -> (PeerId, PeerId, u64) {
    (a.min(b), a.max(b), stream)
}

/// The worse of two ways of reaching us: what a relayed connection is trusted as.
fn worse(a: Trust, b: Trust) -> Trust {
    if rank(a, false) <= rank(b, false) { a } else { b }
}

/// Whether `a` and `b` are both in one lobby we're in (as we see it).
fn share_a_lobby(s: &State, a: PeerId, b: PeerId) -> bool {
    let in_it = |l: &crate::lobby::Lobby, p: PeerId| l.owner == p || l.member(p).is_some();
    s.lobbies.values().any(|h| h.role != Role::Joining && in_it(&h.lobby, a) && in_it(&h.lobby, b))
}

/// Peers to try, every frame: a lobby peer whose connection has been down RELAY_AFTER
/// (RELAY_AFTER_HIGHER when our id is the higher) gets a relayed connection through a
/// player in a lobby with both of us, unless one is already coming up (its offer
/// reached us first); one that doesn't come up in RELAY_TRY, or is refused, gives way
/// to the next player.
pub(super) fn tick(inner: &Inner, s: &mut State, now: Instant) -> Vec<Start> {
    if !inner.relaying {
        return Vec::new();
    }
    let me = inner.me;
    // Back (by any path, this one too) or gone for good: nothing more to try.
    let done: Vec<PeerId> = s
        .relay
        .tries
        .keys()
        .filter(|far| !s.peers.get(far).is_some_and(|p| p.gone.is_some() && !p.bye))
        .copied()
        .collect();
    for far in done {
        s.relay.tries.remove(&far);
    }
    let down: Vec<PeerId> = s
        .peers
        .iter()
        .filter(|(id, p)| {
            let after = if me < **id { RELAY_AFTER } else { RELAY_AFTER_HIGHER };
            !p.bye && !p.waiting && p.gone.is_some_and(|until| now + inner.grace >= until + after)
        })
        .map(|(id, _)| *id)
        .filter(|far| s.lobbies.values().any(|h| h.role != Role::Joining && (h.lobby.owner == *far || h.lobby.member(*far).is_some())))
        .collect();
    let mut starts = Vec::new();
    for far in down {
        let t = s.relay.tries.entry(far).or_insert(Try { now: None, since: now, tried: Vec::new() });
        match t.now {
            // Still coming up, or refused (its end is gone) or too slow: the next one.
            Some((hop, stream)) => {
                let alive = s.relay.ends.contains_key(&(hop, far, stream));
                if alive && now.duration_since(t.since) < RELAY_TRY {
                    continue;
                }
                if alive {
                    s.relay.ends.remove(&(hop, far, stream));
                    s.send_to(hop, &Msg::RelayEnd { stream, from: me, to: far });
                }
            }
            // Everyone tried: again after a while (the far end may not have seen its
            // connection drop yet, and refused).
            None if !t.tried.is_empty() => {
                if now.duration_since(t.since) < RELAY_TRY {
                    continue;
                }
                t.tried.clear();
            }
            None => {}
        }
        let t = s.relay.tries.get_mut(&far).unwrap();
        let tried = t.tried.clone();
        let mut hops: Vec<PeerId> = s
            .peers
            .iter()
            .filter(|(id, p)| **id != far && p.gone.is_none() && !p.stranger && !tried.contains(id))
            .map(|(id, _)| *id)
            .filter(|h| share_a_lobby(s, *h, far))
            .collect();
        let order = super::by_standing(s.peers.iter().map(|(id, p)| (p.trust, p.relayed, p.joined, *id)));
        hops.sort_by_key(|h| order.iter().position(|o| o == h));
        let t = s.relay.tries.get_mut(&far).unwrap();
        let Some(&hop) = hops.first() else {
            if t.tried.is_empty() {
                (inner.log)(&format!("lan peer {far}: no player to reach it through"));
            }
            t.now = None;
            t.since = now;
            t.tried.push(far);
            continue;
        };
        t.tried.push(hop);
        t.since = now;
        let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
            Ok(l) => l,
            Err(e) => {
                (inner.log)(&format!("lan relay to {far}: no listener: {e}"));
                t.now = None;
                continue;
            }
        };
        let stream = u64::from_le_bytes(PeerId::random().0[..8].try_into().unwrap_or_default());
        t.now = Some((hop, stream));
        let key = (hop, far, stream);
        let (tx, rx) = mpsc::channel();
        // Our half of the key exchange, and the connection we lost (both handshake
        // numbers, ours first): the far end's answer keys the connection.
        let mine = Ephemeral::new();
        let (ours, theirs) = s.peers[&far].conn.nonces;
        let sig = inner.key.sign_relay(&offer_transcript(&inner.game, me, far, stream, &mine.blob, (&ours, &theirs)));
        let mut offer = Vec::with_capacity(OFFER_LEN);
        offer.push(OFFER);
        offer.extend_from_slice(&mine.blob);
        offer.extend_from_slice(&ours);
        offer.extend_from_slice(&theirs);
        offer.extend_from_slice(&sig);
        s.send_to(hop, &Msg::Relay { stream, from: me, to: far, data: offer });
        s.relay.ends.insert(key, End { tx, crypt: Crypt::Offered(mine) });
        let trust = worse(s.peers[&far].trust, s.peers[&hop].trust);
        let name = s.peers[&far].name.clone();
        if let Ok(addr) = listener.local_addr() {
            register(inner, addr, far, name, trust);
        }
        (inner.log)(&format!("lan peer {far}: connecting through {hop}"));
        starts.push(Start { far, key, listener, rx });
    }
    starts
}

/// Tells the engine what a relayed connection's loopback end is before the mesh
/// handshakes on it.
fn register(inner: &Inner, mesh: SocketAddr, far: PeerId, name: String, trust: Trust) {
    let tunnel = inet::Tunnel { id: far, public: trust == Trust::Public, key: None, name };
    let mut m = inner.tunnels.lock().unwrap();
    if m.len() >= 1024 {
        m.retain(|_, e| e.down.is_none_or(|d| d.elapsed() < Duration::from_secs(60)));
    }
    if m.len() < 1024 {
        m.insert(mesh, TunnelEntry { tunnel, down: None, relayed: Some(trust) });
    }
}

fn unregister(inner: &Inner, mesh: SocketAddr) {
    if let Some(e) = inner.tunnels.lock().unwrap().get_mut(&mesh) {
        e.down.get_or_insert_with(Instant::now);
    }
}

/// Our side of a relayed connection we started: the mesh dials our listener, and what it
/// says goes to the player in between.
pub(super) fn start(this: Weak<Inner>, mesh: &crate::mesh::Mesh, st: Start) {
    let Ok(addr) = st.listener.local_addr() else { return };
    // Only our own mesh may come to this listener (see `own`).
    let own = crate::own::tcp(addr.port());
    if st.listener.set_nonblocking(true).is_err() {
        return;
    }
    mesh.dial_local(addr.port(), st.far);
    let _ = thread::Builder::new().name("peerparty-relay".into()).spawn(move || {
        let _own = own;
        let deadline = Instant::now() + RELAY_TRY;
        let tcp = loop {
            match st.listener.accept() {
                Ok((t, _)) => break Some(t),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Err(_) => break None,
            }
        };
        match tcp.and_then(|t| t.set_nonblocking(false).ok().map(|_| t)) {
            Some(tcp) => pump(this, st.key, addr, tcp, st.rx),
            None => closed(&this, st.key, addr),
        }
    });
}

/// The far end of a relayed connection reached us: our mesh takes it as it takes an
/// internet tunnel's.
fn answer(this: Weak<Inner>, key: EndKey, trust: Trust, name: String, rx: Receiver<Vec<u8>>) {
    let _ = thread::Builder::new().name("peerparty-relay".into()).spawn(move || {
        let Some(inner) = this.upgrade() else { return };
        let Some(port) = inner.mesh_port.get().copied() else { return };
        let tcp = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), Duration::from_secs(2));
        let Ok(tcp) = tcp else {
            closed(&this, key, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
            return;
        };
        let Ok(addr) = tcp.local_addr() else { return };
        register(&inner, addr, key.1, name, trust);
        drop(inner);
        pump(this, key, addr, tcp, rx);
    });
}

/// Carries a relayed connection between our mesh's loopback socket and the player in
/// between, until either side ends.
fn pump(this: Weak<Inner>, key: EndKey, addr: SocketAddr, tcp: TcpStream, rx: Receiver<Vec<u8>>) {
    let _ = tcp.set_nodelay(true);
    if let Ok(mut writer) = tcp.try_clone() {
        let _ = thread::Builder::new().name("peerparty-relay-in".into()).spawn(move || {
            let mut first = FirstFrame::default();
            while let Ok(data) = rx.recv() {
                let Ok(out) = first.feed(data) else { break };
                if writer.write_all(&out).is_err() {
                    break;
                }
            }
            // Ended (or refused) from the far side: our mesh sees the connection close.
            let _ = writer.shutdown(std::net::Shutdown::Both);
        });
    }
    let mut reader = tcp;
    let mut buf = vec![0u8; CHUNK];
    'read: loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        loop {
            let Some(inner) = this.upgrade() else { break 'read };
            match out(&inner, key, &buf[..n]) {
                Out::Sent => break,
                Out::Wait => {
                    drop(inner);
                    thread::sleep(Duration::from_millis(5));
                }
                Out::Closed => break 'read,
            }
        }
    }
    let _ = reader.shutdown(std::net::Shutdown::Both);
    closed(&this, key, addr);
}

enum Out {
    Sent,
    Wait,
    Closed,
}

/// Sends bytes of our mesh's side, sealed, once the far end has answered.
fn out(inner: &Inner, key: EndKey, data: &[u8]) -> Out {
    let mut guard = inner.state.lock().unwrap();
    let s = &mut *guard;
    let Some(end) = s.relay.ends.get_mut(&key) else { return Out::Closed };
    let Crypt::Keyed { keys, sent, .. } = &mut end.crypt else { return Out::Wait };
    let (hop, far, stream) = key;
    let Some(p) = s.peers.get(&hop) else { return Out::Closed };
    if p.gone.is_some() || p.conn.queued_bytes() > BACKLOG {
        return Out::Wait;
    }
    let mut sealed = Vec::with_capacity(1 + data.len() + 16);
    sealed.push(DATA);
    sealed.extend_from_slice(&keys.seal(*sent, &stream.to_le_bytes(), data));
    *sent += 1;
    p.post(&Msg::Relay { stream, from: inner.me, to: far, data: sealed });
    Out::Sent
}

/// Our side of a relayed connection ended: the player in between hears it.
fn closed(this: &Weak<Inner>, key: EndKey, addr: SocketAddr) {
    let Some(inner) = this.upgrade() else { return };
    unregister(&inner, addr);
    let mut s = inner.state.lock().unwrap();
    if s.relay.ends.remove(&key).is_some() {
        let (hop, far, stream) = key;
        s.send_to(hop, &Msg::RelayEnd { stream, from: inner.me, to: far });
    }
}

/// The relay messages `State::handle` set aside, in order.
pub(super) fn drain(inner: &Inner, s: &mut State) {
    if s.relay.inbox.is_empty() {
        return;
    }
    let me = inner.me;
    for (sender, msg) in std::mem::take(&mut s.relay.inbox) {
        match msg {
            Msg::Relay { stream, from, to, data } if to == me => arrived(inner, s, (sender, from, stream), data),
            Msg::Relay { stream, from, to, data } if from == sender => carry(inner, s, from, to, stream, data),
            Msg::RelayEnd { stream, from, to } if to == me => {
                // Dropping the sender ends our mesh's side.
                s.relay.ends.remove(&(sender, from, stream));
            }
            Msg::RelayEnd { stream, from, to } if from == sender => {
                if s.relay.hops.remove(&hop_key(from, to, stream)) {
                    s.send_to(to, &Msg::RelayEnd { stream, from, to });
                }
            }
            _ => {}
        }
    }
}

/// Bytes for a relayed connection that ends here: the far end's answer to our offer, or
/// sealed bytes of the connection. Anything else, or anything that doesn't open, ends
/// it (the player in between can end it anyway; it can't make it say anything).
fn arrived(inner: &Inner, s: &mut State, key: EndKey, data: Vec<u8>) {
    if !s.relay.ends.contains_key(&key) {
        offered(inner, s, key, data);
        return;
    }
    let (hop, far, stream) = key;
    let me = inner.me;
    let far_key = s.peers.get(&far).map(|p| p.conn.key);
    let Some(end) = s.relay.ends.get_mut(&key) else { return };
    let ok = match &mut end.crypt {
        Crypt::Offered(mine) => {
            let keys = parse_answer(&data)
                .zip(far_key)
                .filter(|((blob, sig), k)| public::relay_proves(k, sig, &answer_transcript(&inner.game, far, me, stream, blob, &mine.blob)))
                .and_then(|((blob, _), _)| Keys::derive(&psk(me, far, stream), mine, &blob));
            match keys {
                Some(keys) => {
                    end.crypt = Crypt::Keyed { keys, sent: 0, got: 0 };
                    true
                }
                None => false,
            }
        }
        Crypt::Keyed { keys, got, .. } => match data.split_first() {
            Some((&DATA, sealed)) => match keys.open(*got, &stream.to_le_bytes(), sealed) {
                Some(plain) => {
                    *got += 1;
                    let _ = end.tx.send(plain);
                    true
                }
                None => false,
            },
            _ => false,
        },
    };
    if !ok {
        (inner.log)(&format!("lan relay from {far} through {hop}: a message that isn't the far end's: ended"));
        s.relay.ends.remove(&key);
        s.send_to(hop, &Msg::RelayEnd { stream, from: me, to: far });
    }
}

/// A new relayed connection: the far end's offer, through `key.0`. Taken when the offer
/// is signed by the far end's key, from a peer whose connection to us is down (and that
/// may come back) or that shows it lost the very connection we still have, through a
/// player we trust as more than a stranger, and unless our own offer to it, which goes
/// ahead, is already out (the lower id's does).
fn offered(inner: &Inner, s: &mut State, key: EndKey, data: Vec<u8>) {
    let (hop, far, stream) = key;
    let me = inner.me;
    let refuse = |s: &mut State| s.send_to(hop, &Msg::RelayEnd { stream, from: me, to: far });
    let (Some((blob, from_nonce, to_nonce, sig)), Some(p)) = (parse_offer(&data), s.peers.get(&far)) else {
        refuse(s);
        return;
    };
    let signed = public::relay_proves(&p.conn.key, &sig, &offer_transcript(&inner.game, far, me, stream, &blob, (&from_nonce, &to_nonce)));
    let far_down = p.gone.is_some() && !p.waiting && !p.bye;
    let ours_lost = p.gone.is_none() && !p.bye && (to_nonce, from_nonce) == p.conn.nonces;
    let conn = p.conn.clone();
    let hop_ok = s.peers.get(&hop).is_some_and(|p| p.gone.is_none() && !p.stranger);
    let ours = s.relay.tries.get(&far).and_then(|t| t.now).filter(|(h, st)| s.relay.ends.contains_key(&(*h, far, *st)));
    if !signed || !(far_down || ours_lost) || !hop_ok || (ours.is_some() && me < far) || s.relay.ends.len() >= ENDS_MAX {
        refuse(s);
        return;
    }
    let mine = Ephemeral::new();
    let Some(keys) = Keys::derive(&psk(far, me, stream), &mine, &blob) else {
        refuse(s);
        return;
    };
    if let Some((h, st)) = ours {
        s.relay.ends.remove(&(h, far, st));
        s.send_to(h, &Msg::RelayEnd { stream: st, from: me, to: far });
    }
    if ours_lost {
        (inner.log)(&format!("lan peer {far} lost its connection to us (it says so through {hop}): ours is gone too"));
        conn.close();
        inner.lost(s, far);
        if !s.peers.contains_key(&far) {
            return;
        }
    }
    let sig = inner.key.sign_relay(&answer_transcript(&inner.game, me, far, stream, &mine.blob, &blob));
    let mut answer_data = Vec::with_capacity(ANSWER_LEN);
    answer_data.push(ANSWER);
    answer_data.extend_from_slice(&mine.blob);
    answer_data.extend_from_slice(&sig);
    s.send_to(hop, &Msg::Relay { stream, from: me, to: far, data: answer_data });
    let (tx, rx) = mpsc::channel();
    s.relay.ends.insert(key, End { tx, crypt: Crypt::Keyed { keys, sent: 0, got: 0 } });
    // This one is our try now: no offer of ours while it comes up.
    s.relay.tries.insert(far, Try { now: Some((hop, stream)), since: Instant::now(), tried: vec![hop] });
    let trust = worse(s.peers[&far].trust, s.peers[&hop].trust);
    (inner.log)(&format!("lan peer {far}: reached us through {hop}"));
    let Some(this) = inner.this.get().cloned() else { return };
    answer(this, key, trust, s.peers[&far].name.clone(), rx);
}

/// Bytes between two others that we carry: a new connection only for two players both
/// in a lobby with us, and ended when the far one can't take more.
fn carry(inner: &Inner, s: &mut State, from: PeerId, to: PeerId, stream: u64, data: Vec<u8>) {
    let key = hop_key(from, to, stream);
    let refuse = |s: &mut State| s.send_to(from, &Msg::RelayEnd { stream, from: to, to: from });
    if !s.relay.hops.contains(&key) {
        let up = |p: PeerId| s.peers.get(&p).is_some_and(|p| p.gone.is_none());
        if !inner.relaying || to == from || !up(to) || !up(from) || !share_a_lobby(s, from, to) || s.relay.hops.len() >= HOPS_MAX {
            refuse(s);
            return;
        }
        s.relay.hops.insert(key);
        (inner.log)(&format!("lan carrying a connection between {from} and {to}"));
    }
    let Some(p) = s.peers.get(&to) else {
        s.relay.hops.remove(&key);
        refuse(s);
        return;
    };
    if p.gone.is_none() && p.conn.queued_bytes() > HOP_QUEUE_MAX {
        (inner.log)(&format!("lan stopped carrying a connection between {from} and {to}: {to} isn't keeping up"));
        s.relay.hops.remove(&key);
        refuse(s);
        s.send_to(to, &Msg::RelayEnd { stream, from, to });
        return;
    }
    p.post(&Msg::Relay { stream, from, to, data });
}

/// A peer left for good: the connections we carry for it end at the other end, and
/// those that ran through it end here.
pub(super) fn peer_gone(s: &mut State, peer: PeerId) {
    let hops: Vec<(PeerId, PeerId, u64)> = s.relay.hops.iter().filter(|(a, b, _)| *a == peer || *b == peer).copied().collect();
    for (a, b, stream) in hops {
        s.relay.hops.remove(&(a, b, stream));
        let other = if a == peer { b } else { a };
        s.send_to(other, &Msg::RelayEnd { stream, from: peer, to: other });
    }
    s.relay.ends.retain(|(hop, _, _), _| *hop != peer);
}

/// Our mesh's own Hello's UDP port can't be reached through a relay: the first frame of
/// the far end's side says none, so unreliable data stays on the connection.
#[derive(Default)]
struct FirstFrame {
    buf: Vec<u8>,
    done: bool,
}

impl FirstFrame {
    fn feed(&mut self, data: Vec<u8>) -> Result<Vec<u8>, ()> {
        if self.done {
            return Ok(data);
        }
        self.buf.extend_from_slice(&data);
        let Some(len) = self.buf.get(..4).map(|l| u32::from_le_bytes(l.try_into().unwrap()) as usize) else {
            return Ok(Vec::new());
        };
        if len > 64 * 1024 {
            return Err(());
        }
        if self.buf.len() < 4 + len {
            return Ok(Vec::new());
        }
        let Some(Msg::Hello { version, game, id, name, key, nonce, .. }) = Msg::decode(&self.buf[4..4 + len]) else {
            return Err(());
        };
        // The proof that follows signs neither the name nor the port.
        let hello = Msg::Hello { version, game, id, name, udp: 0, key, nonce }.encode();
        let mut out = (hello.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(&hello);
        out.extend_from_slice(&self.buf[4 + len..]);
        self.buf = Vec::new();
        self.done = true;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two ends key a connection between themselves: an offer or answer holds only
    /// for its signer, the two ids, the stream and the connection it names, and what one
    /// seals only the other opens, once and in order.
    #[test]
    fn only_the_two_ends_key_and_read_a_relayed_connection() {
        let (ka, kb, kc) = (public::IdentityKey::generate(), public::IdentityKey::generate(), public::IdentityKey::generate());
        let (a, b) = (PeerId::of_key(&ka.public()), PeerId::of_key(&kb.public()));
        let (stream, lost) = (7u64, ([1u8; 16], [2u8; 16]));
        let offer_eph = Ephemeral::new();
        let t = offer_transcript("g", a, b, stream, &offer_eph.blob, (&lost.0, &lost.1));
        let sig = ka.sign_relay(&t);
        assert!(public::relay_proves(&ka.public(), &sig, &t));
        assert!(!public::relay_proves(&kc.public(), &sig, &t), "another player's key");
        assert!(!public::relay_proves(&ka.public(), &kc.sign_relay(&t), &t), "signed by another player");
        for other in [
            offer_transcript("g", a, b, stream + 1, &offer_eph.blob, (&lost.0, &lost.1)),
            offer_transcript("g", b, a, stream, &offer_eph.blob, (&lost.0, &lost.1)),
            offer_transcript("g", a, b, stream, &offer_eph.blob, (&lost.1, &lost.0)),
            offer_transcript("h", a, b, stream, &offer_eph.blob, (&lost.0, &lost.1)),
        ] {
            assert!(!public::relay_proves(&ka.public(), &sig, &other));
        }
        // Not a mesh hello's proof either.
        assert!(!public::hello_proves(&ka.public(), &sig, "g", (a, &lost.0), (b, &lost.1)));
        let answer_eph = Ephemeral::new();
        let at = answer_transcript("g", b, a, stream, &answer_eph.blob, &offer_eph.blob);
        assert!(public::relay_proves(&kb.public(), &kb.sign_relay(&at), &at));
        assert!(!public::relay_proves(&kb.public(), &kb.sign_relay(&at), &t), "an answer isn't an offer");
        let ours = Keys::derive(&psk(a, b, stream), &offer_eph, &answer_eph.blob).unwrap();
        let theirs = Keys::derive(&psk(a, b, stream), &answer_eph, &offer_eph.blob).unwrap();
        let ad = stream.to_le_bytes();
        let sealed = ours.seal(0, &ad, b"mesh bytes");
        assert_eq!(theirs.open(0, &ad, &sealed).as_deref(), Some(&b"mesh bytes"[..]));
        assert!(theirs.open(1, &ad, &sealed).is_none(), "out of order or again");
        assert!(ours.open(0, &ad, &sealed).is_none(), "our own, reflected");
        let mut bent = sealed.clone();
        bent[0] ^= 1;
        assert!(theirs.open(0, &ad, &bent).is_none());
        // Another stream's keys open nothing of this one's.
        let other = Keys::derive(&psk(a, b, stream + 1), &answer_eph, &offer_eph.blob).unwrap();
        assert!(other.open(0, &ad, &sealed).is_none());
        let mut offer = vec![OFFER];
        offer.extend_from_slice(&offer_eph.blob);
        offer.extend_from_slice(&lost.0);
        offer.extend_from_slice(&lost.1);
        offer.extend_from_slice(&sig);
        assert_eq!(parse_offer(&offer), Some((offer_eph.blob, lost.0, lost.1, sig)));
        assert!(parse_offer(&offer[..OFFER_LEN - 1]).is_none() && parse_answer(&offer).is_none());
    }

    #[test]
    fn the_far_ends_hello_loses_its_udp_port_and_the_rest_passes() {
        let hello = Msg::Hello { version: 8, game: "g".into(), id: PeerId([1; 16]), name: "n".into(), udp: 5000, key: [2; 32], nonce: [3; 16] }.encode();
        let mut stream = (hello.len() as u32).to_le_bytes().to_vec();
        stream.extend_from_slice(&hello);
        stream.extend_from_slice(b"after");
        let mut f = FirstFrame::default();
        // In pieces: nothing until the whole frame is there.
        let mut out = f.feed(stream[..3].to_vec()).unwrap();
        assert!(out.is_empty());
        out.extend(f.feed(stream[3..10].to_vec()).unwrap());
        out.extend(f.feed(stream[10..].to_vec()).unwrap());
        out.extend(f.feed(b"more".to_vec()).unwrap());
        let len = u32::from_le_bytes(out[..4].try_into().unwrap()) as usize;
        assert!(matches!(Msg::decode(&out[4..4 + len]), Some(Msg::Hello { udp: 0, .. })));
        assert_eq!(&out[4 + len..], b"aftermore");
        assert!(FirstFrame::default().feed(vec![5, 0, 0, 0, 21, 0, 0, 0, 0]).is_err(), "not a hello");
    }
}
