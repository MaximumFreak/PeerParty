//! SteamIDs on the network when players clash.
//!
//! Two copies of a game whose steam_api gives every player the same SteamID (one
//! account set up the same way on both PCs) both call themselves that SteamID. Left
//! alone, each game thinks the other player is itself: `GetLobbyOwner` names the local
//! player, an invite comes "from" it, and packets to the other player loop back.
//!
//! Normally that never happens: each PC's game is shown a SteamID of its own
//! (steamlan's `own_id_on`). This module is for when a game keeps steam_api's SteamID
//! (the "Steam player ID" mode set to the game's own, or a player on an older
//! PeerParty). Then the game's SteamID never changes, and what changes is what the
//! *other* players are shown:
//!
//! - Each copy publishes the SteamID its game uses (`peerparty:steamid_game`) and the one
//!   it goes by on the network (`peerparty:steamid`). They're the same until a clash.
//! - A copy that finds another player whose game uses its SteamID goes by its LAN
//!   SteamID (`lan_steam_id`, from its LAN id) on the network from then on,
//!   logs it, and republishes. Both sides do this, so a third copy shows each of them
//!   under its own LAN SteamID.
//! - A player whose game uses our SteamID is shown here under `ordered_steam_id`: our
//!   SteamID with an account id below ours if their LAN id sorts before ours, above it
//!   otherwise. Every copy then has the players in the same order of SteamID (a game
//!   that seats players by it agrees on who's player 1);
//!   with LAN SteamIDs (accounts >= 0x80000000) each copy came first on its own screen.
//! - A player still told under our SteamID, or two players told under one SteamID
//!   (copies from before this, which don't publish the game key), are shown under their
//!   LAN SteamIDs here.
//! - Payloads: while a clash is on, the 8-byte little-endian SteamID and its 17-digit
//!   decimal form are swapped in place (same size) in P2P, networking-sockets and lobby
//!   chat messages, and in lobby data and rich presence read from a player: what a
//!   clashing sender's game calls itself becomes what we show it as, and our network
//!   SteamID becomes our game's own. Copies from before this get our game's SteamID
//!   swapped back to our network one on the way out.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use peerparty_core::{Lan, PeerId};

use crate::log;

pub const KEY_STEAM_ID: &str = "peerparty:steamid";
pub const KEY_STEAM_GAME: &str = "peerparty:steamid_game";

/// How often the view is worked out again without a presence event.
const REFRESH: Duration = Duration::from_secs(2);

/// A LAN SteamID: public universe, individual, instance 1 (the desktop instance every
/// real account has; some games treat any other instance as no SteamID at all and keep
/// their saves under 0) with an account id from the peer id and its top bit set. Real
/// accounts are below 0x80000000, so the two never collide.
pub fn lan_steam_id(peer: PeerId) -> u64 {
    let account = u32::from_le_bytes(peer.0[0..4].try_into().unwrap()) | 0x8000_0000;
    (1u64 << 56) | (1u64 << 52) | (1u64 << 32) | account as u64
}

/// What a LAN player told us in presence.
#[derive(Clone, Copy, Debug)]
pub struct Told {
    pub peer: PeerId,
    /// `peerparty:steamid`: what they go by on the network.
    pub net: Option<u64>,
    /// `peerparty:steamid_game`: what their game calls itself (copies from before clash
    /// handling don't send it).
    pub game: Option<u64>,
}

/// How a LAN player looks from here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The SteamID our game is given for them.
    pub shown: u64,
    /// The SteamID their game uses for itself.
    pub game: u64,
    /// The SteamID they go by on the network.
    pub net: u64,
    /// Whether they handle clashes themselves (they publish the game key).
    pub aware: bool,
}

/// The SteamID our game is given for `peer`, a player whose game uses our SteamID
/// (`mine`; we're `me` on the LAN). It keeps our SteamID's universe, type and instance,
/// and its account id sits below ours if `peer`'s LAN id sorts before ours and above
/// otherwise. Every copy uses the same order, so a game that puts players in order of
/// SteamID (or account id) agrees with the others on who's first: with a plain LAN
/// SteamID (account >= 0x80000000) every copy would come first on its own screen, both
/// in player 1's slot.
pub fn ordered_steam_id(mine: u64, me: PeerId, peer: PeerId) -> u64 {
    let acc = mine as u32;
    let key = u32::from_be_bytes(peer.0[0..4].try_into().unwrap()) as u64;
    let account = if peer.0 < me.0 {
        // Not below MIN_ACCOUNT where there's room, so account swaps still apply.
        let low = if acc >= 2 * MIN_ACCOUNT { MIN_ACCOUNT } else { 1 };
        if acc <= low {
            return lan_steam_id(peer);
        }
        low as u64 + ((key * (acc - low) as u64) >> 32)
    } else {
        if acc == u32::MAX {
            return lan_steam_id(peer);
        }
        acc as u64 + 1 + ((key * (u32::MAX - acc) as u64) >> 32)
    };
    (mine & !0xffff_ffff) | account
}

/// The pure part: each player's `Seen`, whether one of them clashes with us (`mine`,
/// our game's SteamID, 0 while unknown; we're `me` on the LAN), and the players newly
/// shown under a SteamID other than their network one. `aliased` holds the players
/// already shown that way (sticky: the game may have kept the SteamID). A player whose
/// game uses our SteamID is shown under `ordered_steam_id`; other shared ones under
/// their LAN SteamID.
pub fn work_out(mine: u64, me: PeerId, told: &[Told], aliased: &HashSet<PeerId>) -> (HashMap<PeerId, Seen>, bool, Vec<PeerId>) {
    let net_of = |t: &Told| t.net.unwrap_or_else(|| lan_steam_id(t.peer));
    let mut seen = HashMap::new();
    let mut clash = false;
    let mut newly = Vec::new();
    for t in told {
        let net = net_of(t);
        let game = t.game.unwrap_or(net);
        let with_us = mine != 0 && (game == mine || net == mine);
        clash |= with_us;
        let shared = (mine != 0 && net == mine) || told.iter().any(|o| o.peer != t.peer && net_of(o) == net);
        if (with_us || shared) && !aliased.contains(&t.peer) {
            newly.push(t.peer);
        }
        let shown = if with_us {
            ordered_steam_id(mine, me, t.peer)
        } else if shared || aliased.contains(&t.peer) {
            lan_steam_id(t.peer)
        } else {
            net
        };
        seen.insert(t.peer, Seen { shown, game, net, aware: t.game.is_some() });
    }
    (seen, clash, newly)
}

fn push_swap(v: &mut Vec<(u64, u64)>, from: u64, to: u64) {
    if from != 0 && from != to && !v.iter().any(|(f, _)| *f == from) {
        v.push((from, to));
    }
}

/// SteamIDs swapped in what `from` sent: pairs of (in the bytes, for our game). What
/// the sender's game calls itself is what we show it as; a player's network SteamID
/// (`others` are the players besides `from`) is what we show them as; our network
/// SteamID is our game's own.
pub fn swaps_in(from: Option<Seen>, others: &[Seen], mine: u64, my_net: u64) -> Vec<(u64, u64)> {
    let mut v = Vec::new();
    if let Some(s) = from {
        push_swap(&mut v, s.game, s.shown);
    }
    let all: Vec<&Seen> = from.iter().chain(others).collect();
    for s in &all {
        // A network SteamID two players go by (older copies) names neither for sure.
        let shared = all.iter().filter(|o| o.net == s.net).count() > 1;
        if s.net != mine && s.net != my_net && !shared {
            push_swap(&mut v, s.net, s.shown);
        }
    }
    if mine != 0 {
        push_swap(&mut v, my_net, mine);
    }
    v
}

/// SteamIDs swapped in what we send `to`: whoever our game shows under a SteamID of its
/// own goes by their network one, so the receiver can show them its way. Copies that
/// don't handle clashes also get our game's SteamID as our network one (aware ones swap
/// that on their side).
pub fn swaps_out(to: Option<Seen>, others: &[Seen], mine: u64, my_net: u64) -> Vec<(u64, u64)> {
    let Some(t) = to else { return Vec::new() };
    let mut v = Vec::new();
    if !t.aware && mine != 0 {
        push_swap(&mut v, mine, my_net);
    }
    push_swap(&mut v, t.shown, t.net);
    for s in others {
        // An older copy told under our SteamID: naming it as that would name us.
        if s.net != mine {
            push_swap(&mut v, s.shown, s.net);
        }
    }
    v
}

/// What `rewrite` swapped: whole SteamIDs, and account IDs (a SteamID's low 32 bits).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Swapped {
    pub ids: usize,
    pub accounts: usize,
}

impl Swapped {
    pub fn any(self) -> bool {
        self.ids + self.accounts > 0
    }
}

/// Account IDs below this are too common as plain numbers to swap as 4 bytes.
const MIN_ACCOUNT: u32 = 0x100;

/// The account IDs (low 32 bits, 4 little-endian bytes) to swap with `swaps`: games
/// with their own netcode may name players by account ID (a `hostAccountID`, a
/// player list), and every copy has the same one. Only with `PEERPARTY_STEAM_ACCOUNTS=1`
/// (steamlan's `accounts_on`).
fn account_swaps(swaps: &[(u64, u64)]) -> Vec<([u8; 4], [u8; 4])> {
    let mut v: Vec<([u8; 4], [u8; 4])> = Vec::new();
    for &(a, b) in swaps {
        let (a, b) = (a as u32, b as u32);
        if a == b || a < MIN_ACCOUNT || v.iter().any(|(x, _)| *x == a.to_le_bytes()) {
            continue;
        }
        v.push((a.to_le_bytes(), b.to_le_bytes()));
        v.push((a.to_be_bytes(), b.to_be_bytes()));
    }
    v
}

/// `s` as UTF-16LE (C# strings, `Encoding.Unicode`).
fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect()
}

/// Whether `data[i..]` holds a UTF-16LE digit.
fn utf16_digit(data: &[u8], i: usize) -> bool {
    data.get(i).is_some_and(|c| c.is_ascii_digit()) && data.get(i + 1) == Some(&0)
}

/// Swaps SteamIDs in place, all at once (a swapped value isn't swapped again): as
/// 8 bytes either way round, and as decimal text (UTF-8 or UTF-16LE) not inside a
/// longer number; with `accounts` (binary payloads, not text) their account IDs too, as
/// 4 bytes either way round. Games with their own stream code often write big-endian.
pub fn rewrite(data: &mut [u8], swaps: &[(u64, u64)], accounts: bool) -> Swapped {
    let mut n = Swapped::default();
    if swaps.is_empty() {
        return n;
    }
    let bin: Vec<([u8; 8], [u8; 8])> = swaps
        .iter()
        .flat_map(|(a, b)| [(a.to_le_bytes(), b.to_le_bytes()), (a.to_be_bytes(), b.to_be_bytes())])
        .collect();
    let acc = if accounts { account_swaps(swaps) } else { Vec::new() };
    let wide: Vec<(Vec<u8>, Vec<u8>)> = swaps
        .iter()
        .map(|(a, b)| (utf16(&a.to_string()), utf16(&b.to_string())))
        .filter(|(a, b)| a.len() == b.len())
        .collect();
    let txt: Vec<(Vec<u8>, Vec<u8>)> = swaps
        .iter()
        .map(|(a, b)| (a.to_string().into_bytes(), b.to_string().into_bytes()))
        .filter(|(a, b)| a.len() == b.len())
        .collect();
    let mut i = 0;
    'at: while i < data.len() {
        for (a, b) in &bin {
            if data[i..].starts_with(a) {
                data[i..i + 8].copy_from_slice(b);
                i += 8;
                n.ids += 1;
                continue 'at;
            }
        }
        if !(i >= 2 && utf16_digit(data, i - 2)) {
            for (a, b) in &wide {
                let end = i + a.len();
                if data[i..].starts_with(a) && !utf16_digit(data, end) {
                    data[i..end].copy_from_slice(b);
                    i = end;
                    n.ids += 1;
                    continue 'at;
                }
            }
        }
        for (a, b) in &acc {
            if data[i..].starts_with(a) {
                data[i..i + 4].copy_from_slice(b);
                i += 4;
                n.accounts += 1;
                continue 'at;
            }
        }
        if i == 0 || !data[i - 1].is_ascii_digit() {
            for (a, b) in &txt {
                let end = i + a.len();
                if data[i..].starts_with(a) && data.get(end).is_none_or(|c| !c.is_ascii_digit()) {
                    data[i..end].copy_from_slice(b);
                    i = end;
                    n.ids += 1;
                    continue 'at;
                }
            }
        }
        i += 1;
    }
    n
}

/// Where any of `ids` turns up in `data`, in every encoding we know of, for the
/// `PEERPARTY_STEAM_DUMP` diagnostic: "<id> <encoding>@<offset>".
pub fn find_ids(data: &[u8], ids: &[u64]) -> Vec<String> {
    let mut out = Vec::new();
    let mut look = |what: String, pat: Vec<u8>| {
        if pat.is_empty() || pat.len() > data.len() {
            return;
        }
        for i in 0..=data.len() - pat.len() {
            if data[i..].starts_with(&pat) {
                out.push(format!("{what}@{i}"));
            }
        }
    };
    for &id in ids.iter().filter(|&&id| id != 0) {
        let acc = id as u32;
        look(format!("{id} u64le"), id.to_le_bytes().to_vec());
        look(format!("{id} u64be"), id.to_be_bytes().to_vec());
        look(format!("{id} dec"), id.to_string().into_bytes());
        look(format!("{id} dec16"), utf16(&id.to_string()));
        look(format!("{id} hex"), format!("{id:016x}").into_bytes());
        look(format!("{id} HEX"), format!("{id:016X}").into_bytes());
        look(format!("{id} acc u32le"), acc.to_le_bytes().to_vec());
        look(format!("{id} acc u32be"), acc.to_be_bytes().to_vec());
        if acc >= 1000 {
            look(format!("{id} acc dec"), acc.to_string().into_bytes());
            look(format!("{id} acc dec16"), utf16(&acc.to_string()));
        }
        if (0x100..=0xffff).contains(&acc) {
            look(format!("{id} acc u16le"), (acc as u16).to_le_bytes().to_vec());
            look(format!("{id} acc u16be"), (acc as u16).to_be_bytes().to_vec());
        }
        let mut v = Vec::new();
        let mut x = id;
        loop {
            let b = (x & 0x7f) as u8;
            x >>= 7;
            if x == 0 {
                v.push(b);
                break;
            }
            v.push(b | 0x80);
        }
        look(format!("{id} varint"), v);
    }
    out
}

/// The first `max` bytes of `data` as hex.
pub fn hex_head(data: &[u8], max: usize) -> String {
    let mut s: String = data.iter().take(max).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
    if data.len() > max {
        s.push_str(" ..");
    }
    s
}

fn parse(kv: &[(String, String)], key: &str) -> Option<u64> {
    kv.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.parse().ok())
}

/// One copy's SteamID book: what each LAN player is shown as, and whether we go by our
/// LAN SteamID on the network.
#[derive(Default)]
pub struct Book {
    /// Our game's SteamID (0 until known).
    pub mine: u64,
    /// Our LAN SteamID.
    pub alias: u64,
    /// Another player uses our SteamID: we go by `alias` on the network (sticky).
    pub clashed: bool,
    seen: HashMap<PeerId, Seen>,
    aliased: HashSet<PeerId>,
    by_id: HashMap<u64, PeerId>,
    last: Option<Instant>,
    pub dirty: bool,
}

/// What a `refresh` changed that the caller acts on.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// We clashed just now: republish presence.
    pub clashed_now: bool,
}

impl Book {
    /// Every SteamID in play here (ours, our network one, and each player's shown,
    /// game and network ones), for the dump diagnostic.
    pub fn all_ids(&self) -> Vec<u64> {
        let mut v = vec![self.mine, self.alias];
        for s in self.seen.values() {
            v.extend([s.shown, s.game, s.net]);
        }
        v.retain(|&x| x != 0);
        v.sort_unstable();
        v.dedup();
        v
    }

    /// The SteamID we go by on the network.
    pub fn net_id(&self) -> u64 {
        if self.clashed && self.alias != 0 { self.alias } else { self.mine }
    }

    /// Presence keys to publish (none until our SteamID is known).
    pub fn presence(&self) -> Vec<(String, String)> {
        if self.mine == 0 {
            return Vec::new();
        }
        vec![
            (KEY_STEAM_ID.into(), self.net_id().to_string()),
            (KEY_STEAM_GAME.into(), self.mine.to_string()),
        ]
    }

    pub fn stale(&self) -> bool {
        self.dirty || self.last.is_none_or(|t| t.elapsed() >= REFRESH)
    }

    /// Works the view out again from what the LAN players told us.
    pub fn refresh(&mut self, lan: &Lan) -> Changes {
        self.alias = lan_steam_id(lan.me());
        let told: Vec<Told> = lan
            .peers()
            .into_iter()
            .map(|(peer, _)| {
                let kv = lan.presence(peer);
                Told { peer, net: parse(&kv, KEY_STEAM_ID), game: parse(&kv, KEY_STEAM_GAME) }
            })
            .collect();
        let (seen, clash, newly) = work_out(self.mine, lan.me(), &told, &self.aliased);
        for p in newly {
            let s = seen[&p];
            let name = lan.peer_name(p).unwrap_or_default();
            let who = if self.mine != 0 && s.net == self.mine { "we".to_string() } else { "another player".to_string() };
            log!(
                "steam-lan SteamID clash: {name} goes by SteamID {}, which {who} also use (one account on both PCs?); this game sees them as {} from now on",
                s.net,
                s.shown
            );
            self.aliased.insert(p);
        }
        for (p, s) in &seen {
            if self.by_id.get(&s.shown) != Some(p) {
                self.by_id.retain(|k, v| v != p || *k == s.shown);
                self.by_id.insert(s.shown, *p);
            }
        }
        self.seen = seen;
        self.last = Some(Instant::now());
        self.dirty = false;
        let mut out = Changes::default();
        if clash && self.mine != 0 && !self.clashed && self.mine != self.alias {
            self.clashed = true;
            out.clashed_now = true;
            log!(
                "steam-lan SteamID clash: another player's game also uses our SteamID {} (one account on both PCs?). On the network we go by {} from now on; this game still sees {}, so its saves stay where they are",
                self.mine,
                self.alias,
                self.mine
            );
        }
        out
    }

    pub fn seen(&self, peer: PeerId) -> Option<Seen> {
        self.seen.get(&peer).copied()
    }

    /// The SteamID our game is given for `peer`, if we know them.
    pub fn shown(&self, peer: PeerId) -> Option<u64> {
        self.seen(peer).map(|s| s.shown)
    }

    /// Remembers a SteamID handed out for a player we have no presence for.
    pub fn note(&mut self, id: u64, peer: PeerId) {
        self.by_id.insert(id, peer);
    }

    /// The LAN player (not us) our game knows as `id`.
    pub fn peer(&self, id: u64) -> Option<PeerId> {
        self.by_id.get(&id).copied()
    }

    fn others(&self, but: PeerId) -> Vec<Seen> {
        self.seen.iter().filter(|(p, _)| **p != but).map(|(_, s)| *s).collect()
    }

    pub fn swaps_in(&self, from: PeerId) -> Vec<(u64, u64)> {
        swaps_in(self.seen(from), &self.others(from), self.mine, self.net_id())
    }

    pub fn swaps_out(&self, to: PeerId) -> Vec<(u64, u64)> {
        swaps_out(self.seen(to), &self.others(to), self.mine, self.net_id())
    }
}

/// `data` from `from`, with SteamIDs (and with `accounts`, account IDs) as our game
/// knows them.
pub fn swapped_in(book: &Book, from: PeerId, data: &mut [u8], accounts: bool) -> Swapped {
    rewrite(data, &book.swaps_in(from), accounts)
}

/// `data` for `to`, with SteamIDs (and with `accounts`, account IDs) as they know them.
pub fn swapped_out<'a>(book: &Book, to: PeerId, data: &'a [u8], accounts: bool) -> Cow<'a, [u8]> {
    let swaps = book.swaps_out(to);
    if swaps.is_empty() {
        return Cow::Borrowed(data);
    }
    let mut v = data.to_vec();
    rewrite(&mut v, &swaps, accounts);
    Cow::Owned(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHARED: u64 = 76561197960270388; // a shared account id 0x1234

    fn p(b: u8) -> PeerId {
        PeerId([b; 16])
    }

    #[test]
    fn clash_with_us_and_between_others_shows_other_ids() {
        let none = HashSet::new();
        let me = p(5);
        // An older copy telling our SteamID: shown under an ordered one, and we clash.
        let (s, clash, newly) = work_out(SHARED, me, &[Told { peer: p(1), net: Some(SHARED), game: None }], &none);
        assert!(clash);
        assert_eq!(newly, vec![p(1)]);
        assert_eq!(s[&p(1)].shown, ordered_steam_id(SHARED, me, p(1)));
        // An aware copy that already moved to its LAN SteamID: its game is on ours, so
        // it's shown under an ordered one too, and we clash.
        let (s, clash, newly) = work_out(SHARED, me, &[Told { peer: p(1), net: Some(lan_steam_id(p(1))), game: Some(SHARED) }], &none);
        assert!(clash);
        assert_eq!(newly, vec![p(1)]);
        assert_eq!(s[&p(1)].shown, ordered_steam_id(SHARED, me, p(1)));
        // Two others on one SteamID (we're someone else): both shown under LAN ones.
        let told = [Told { peer: p(1), net: Some(SHARED), game: None }, Told { peer: p(2), net: Some(SHARED), game: None }];
        let (s, clash, _) = work_out(76561197960265729, me, &told, &none);
        assert!(!clash);
        assert_ne!(s[&p(1)].shown, s[&p(2)].shown);
        assert_ne!(s[&p(1)].shown, SHARED);
        // No clash: SteamIDs pass through.
        let (s, clash, newly) = work_out(SHARED, me, &[Told { peer: p(1), net: Some(SHARED + 1), game: Some(SHARED + 1) }], &none);
        assert!(!clash && newly.is_empty());
        assert_eq!(s[&p(1)].shown, SHARED + 1);
        // Sticky: once shown under a LAN SteamID, it stays.
        let sticky: HashSet<PeerId> = [p(1)].into();
        let (s, _, _) = work_out(SHARED, me, &[Told { peer: p(1), net: Some(SHARED + 1), game: None }], &sticky);
        assert_eq!(s[&p(1)].shown, lan_steam_id(p(1)));
    }

    #[test]
    fn ordered_ids_keep_one_order_on_every_copy() {
        // Each copy's game is on SHARED (account 0x1234) and sees itself as SHARED. Whoever's
        // LAN id sorts first is below SHARED on the others' screens and they're above it on
        // its own: every screen has the same player first (a game's player 1 slot).
        for (x, y) in [(p(1), p(2)), (p(0x80), p(0x7f)), (PeerId([0; 16]), PeerId([255; 16]))] {
            let (first, second) = if x.0 < y.0 { (x, y) } else { (y, x) };
            let second_on_first = ordered_steam_id(SHARED, first, second);
            let first_on_second = ordered_steam_id(SHARED, second, first);
            assert!(SHARED < second_on_first, "{second_on_first}");
            assert!(first_on_second < SHARED, "{first_on_second}");
            for id in [second_on_first, first_on_second] {
                assert_eq!(id >> 32, SHARED >> 32, "same universe, type and instance as ours");
                assert!(id as u32 >= MIN_ACCOUNT && id != SHARED);
                assert_eq!(id.to_string().len(), SHARED.to_string().len());
            }
        }
        // Three copies: A < B < C on every screen.
        let (a, b, c) = (p(1), p(2), p(3));
        assert!(ordered_steam_id(SHARED, a, b) < ordered_steam_id(SHARED, a, c));
        assert!(ordered_steam_id(SHARED, b, a) < SHARED && SHARED < ordered_steam_id(SHARED, b, c));
        assert!(ordered_steam_id(SHARED, c, a) < ordered_steam_id(SHARED, c, b));
        assert!(ordered_steam_id(SHARED, c, b) < SHARED);
        // No room below account 1: a LAN SteamID instead.
        assert_eq!(ordered_steam_id(1 << 56 | 1 << 52 | 1 << 32 | 1, b, a), lan_steam_id(a));
    }

    #[test]
    fn three_copies_agree_on_who_is_who() {
        // A and B share SHARED and have both moved; C (real account) sees them by their
        // network SteamIDs. A copy naming B to C names B's network SteamID, which C shows.
        let (a, b, c) = (p(1), p(2), p(3));
        let real = 76561198000000001;
        let told_by = |x: PeerId| match x {
            x if x == c => Told { peer: c, net: Some(real), game: Some(real) },
            x => Told { peer: x, net: Some(lan_steam_id(x)), game: Some(SHARED) },
        };
        let none = HashSet::new();
        let (from_a, ..) = work_out(SHARED, a, &[told_by(b), told_by(c)], &none);
        let (from_b, ..) = work_out(SHARED, b, &[told_by(a), told_by(c)], &none);
        let (from_c, ..) = work_out(real, c, &[told_by(a), told_by(b)], &none);
        assert_eq!(from_c[&b].shown, lan_steam_id(b));
        assert_eq!(from_a[&c].shown, real);
        assert_eq!(from_b[&c].shown, real);
        assert!(from_b[&a].shown < SHARED && SHARED < from_a[&b].shown);
        // A's game names B (as A shows it) and itself to C.
        let out = swaps_out(Some(from_a[&c]), &[from_a[&b]], SHARED, lan_steam_id(a));
        let mut d = [from_a[&b].shown.to_le_bytes(), SHARED.to_le_bytes()].concat();
        rewrite(&mut d, &out, true);
        let into_c = swaps_in(Some(from_c[&a]), &[from_c[&b]], real, real);
        rewrite(&mut d, &into_c, true);
        assert_eq!(&d[0..8], &from_c[&b].shown.to_le_bytes());
        assert_eq!(&d[8..16], &from_c[&a].shown.to_le_bytes());
        // ... and to B: B reads itself as its own SHARED and A as B shows A.
        let out = swaps_out(Some(from_a[&b]), &[from_a[&c]], SHARED, lan_steam_id(a));
        let mut d = [from_a[&b].shown.to_le_bytes(), SHARED.to_le_bytes()].concat();
        rewrite(&mut d, &out, true);
        let into_b = swaps_in(Some(from_b[&a]), &[from_b[&c]], SHARED, lan_steam_id(b));
        rewrite(&mut d, &into_b, true);
        assert_eq!(&d[0..8], &SHARED.to_le_bytes());
        assert_eq!(&d[8..16], &from_b[&a].shown.to_le_bytes());
    }

    #[test]
    fn rewrite_swaps_binary_and_decimal_at_once() {
        let (x, y) = (SHARED, lan_steam_id(p(9)));
        let mut d = Vec::new();
        d.extend_from_slice(b"hi");
        d.extend_from_slice(&x.to_le_bytes());
        d.extend_from_slice(&y.to_le_bytes());
        d.extend_from_slice(format!("|{x}|1{x}|").as_bytes());
        let n = rewrite(&mut d, &[(x, y), (y, x)], true);
        assert_eq!(n, Swapped { ids: 3, accounts: 0 }, "a whole SteamID isn't also swapped as an account");
        assert_eq!(&d[2..10], &y.to_le_bytes());
        assert_eq!(&d[10..18], &x.to_le_bytes());
        assert_eq!(&d[18..], format!("|{y}|1{x}|").as_bytes(), "a longer number is left alone");
        assert_eq!(x.to_string().len(), y.to_string().len());
    }

    #[test]
    fn rewrite_swaps_account_ids_in_binary_only() {
        let (x, y) = (SHARED, lan_steam_id(p(9)));
        let (ax, ay) = (x as u32, y as u32);
        assert_eq!(ax, 0x1234);
        let mut d = Vec::new();
        d.push(7);
        d.extend_from_slice(&ax.to_le_bytes());
        d.extend_from_slice(&ay.to_le_bytes());
        d.extend_from_slice(&4660u16.to_le_bytes());
        let mut text = d.clone();
        let n = rewrite(&mut d, &[(x, y), (y, x)], true);
        assert_eq!(n, Swapped { ids: 0, accounts: 2 });
        assert_eq!(&d[1..5], &ay.to_le_bytes());
        assert_eq!(&d[5..9], &ax.to_le_bytes());
        assert_eq!(&d[9..], &4660u16.to_le_bytes(), "a 16-bit 4660 is left alone");
        // Text (lobby data, rich presence) and accounts off: untouched.
        assert!(!rewrite(&mut text, &[(x, y), (y, x)], false).any());
        // Tiny account IDs aren't swapped as 4 bytes.
        let mut small = 5u32.to_le_bytes().to_vec();
        assert!(!rewrite(&mut small, &[((1 << 56) | 5, y)], true).any());
    }

    #[test]
    fn rewrite_swaps_big_endian_and_utf16() {
        let (x, y) = (SHARED, lan_steam_id(p(9)));
        let mut d = Vec::new();
        d.extend_from_slice(&x.to_be_bytes());
        d.extend_from_slice(&(x as u32).to_be_bytes());
        d.extend_from_slice(&utf16(&format!("<{x}>")));
        d.extend_from_slice(&utf16(&format!("1{x}")));
        let n = rewrite(&mut d, &[(x, y), (y, x)], true);
        assert_eq!(n, Swapped { ids: 2, accounts: 1 });
        assert_eq!(&d[0..8], &y.to_be_bytes());
        assert_eq!(&d[8..12], &(y as u32).to_be_bytes());
        assert_eq!(&d[12..12 + 2 * 19], &utf16(&format!("<{y}>"))[..]);
        assert_eq!(&d[12 + 2 * 19..], &utf16(&format!("1{x}"))[..], "a longer number is left alone");
    }

    #[test]
    fn find_ids_names_encodings_and_offsets() {
        let mut d = vec![0xaa];
        d.extend_from_slice(&SHARED.to_be_bytes());
        d.extend_from_slice(b"x4660");
        let f = find_ids(&d, &[SHARED]);
        assert!(f.contains(&format!("{SHARED} u64be@1")), "{f:?}");
        assert!(f.contains(&format!("{SHARED} acc u32be@5")), "{f:?}");
        assert!(f.contains(&format!("{SHARED} acc u16be@7")), "{f:?}");
        assert!(f.contains(&format!("{SHARED} acc dec@10")), "{f:?}");
        assert_eq!(hex_head(&[1, 2, 255], 2), "01 02 ..");
    }

    #[test]
    fn swaps_for_old_and_new_copies() {
        let alias = lan_steam_id(p(7));
        let aware = Seen { shown: lan_steam_id(p(1)), game: SHARED, net: lan_steam_id(p(1)), aware: true };
        // From an aware clashing copy: its SHARED is it, our alias is us.
        assert_eq!(swaps_in(Some(aware), &[], SHARED, alias), vec![(SHARED, lan_steam_id(p(1))), (alias, SHARED)]);
        // To an aware copy shown as its network SteamID: nothing, it swaps on its side.
        assert!(swaps_out(Some(aware), &[], SHARED, alias).is_empty());
        // Shown under an ordered SteamID: that goes as its network one, which it swaps
        // back to its own SHARED.
        let ordered = Seen { shown: ordered_steam_id(SHARED, p(7), p(1)), ..aware };
        assert_eq!(swaps_out(Some(ordered), &[], SHARED, alias), vec![(ordered.shown, ordered.net)]);
        assert_eq!(swaps_in(Some(ordered), &[], SHARED, alias), vec![(SHARED, ordered.shown), (ordered.net, ordered.shown), (alias, SHARED)]);
        // To an older copy shown here under its LAN id: our SHARED goes as our alias, its LAN id as its SHARED.
        let old = Seen { shown: lan_steam_id(p(1)), game: SHARED, net: SHARED, aware: false };
        assert_eq!(swaps_out(Some(old), &[], SHARED, alias), vec![(SHARED, alias), (lan_steam_id(p(1)), SHARED)]);
        // ... and naming that older copy to someone else doesn't send our SteamID.
        assert_eq!(swaps_out(Some(ordered), &[old], SHARED, alias), vec![(ordered.shown, ordered.net)]);
        // Two older copies on one network SteamID: it isn't swapped in.
        let old2 = Seen { shown: lan_steam_id(p(2)), game: 5 << 32, net: 5 << 32, aware: false };
        let old3 = Seen { shown: lan_steam_id(p(3)), ..old2 };
        assert!(swaps_in(None, &[old2, old3], SHARED, SHARED).is_empty());
        // No clash anywhere: nothing.
        let fine = Seen { shown: SHARED + 1, game: SHARED + 1, net: SHARED + 1, aware: true };
        assert!(swaps_in(Some(fine), &[], SHARED, SHARED).is_empty());
        assert!(swaps_out(Some(fine), &[], SHARED, SHARED).is_empty());
    }

    // ---- two copies over loopback, one shared account ----

    fn start(game: &str, port: u16, name: &str) -> Lan {
        Lan::start(peerparty_core::Options {
            game: game.into(),
            identity: peerparty_core::Identity::ephemeral(name),
            discovery_port: port,
            extra_targets: Vec::new(),
            log: |_| {},
            internet: None,
            public: None,
            lan_discovery: true,
        })
        .expect("start")
    }

    fn pump(lans: &[&Lan], events: &mut [Vec<peerparty_core::Event>], what: &str, mut done: impl FnMut(&mut [Vec<peerparty_core::Event>]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !done(events) {
            for (l, e) in lans.iter().zip(events.iter_mut()) {
                l.tick();
                e.extend(l.poll());
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn two_copies_on_one_shared_account_see_each_other_and_route() {
        use peerparty_core::{Event, JoinResult, Lobby, MemberStatus, Query};
        // Loopback only: 127.0.0.1, no beacons on the network.
        unsafe { std::env::set_var("PEERPARTY_LOOPBACK", "1") };
        let port = 47900 + (std::process::id() % 90) as u16;
        let game = format!("test:steamids:{}", std::process::id());
        let (a, b) = (start(&game, port, "a"), start(&game, port, "b"));
        let mut ev = vec![Vec::new(), Vec::new()];
        pump(&[&a, &b], &mut ev, "the copies to meet", |e| {
            e[0].contains(&Event::PeerJoined(b.me())) && e[1].contains(&Event::PeerJoined(a.me()))
        });

        // Both games are on account 0x1234.
        let mut book_a = Book { mine: SHARED, ..Default::default() };
        let mut book_b = Book { mine: SHARED, ..Default::default() };
        book_a.alias = lan_steam_id(a.me());
        book_b.alias = lan_steam_id(b.me());
        a.set_presence(book_a.presence());
        b.set_presence(book_b.presence());
        pump(&[&a, &b], &mut ev, "presence", |_| {
            a.presence(b.me()).iter().any(|(k, _)| k == KEY_STEAM_GAME) && b.presence(a.me()).iter().any(|(k, _)| k == KEY_STEAM_GAME)
        });

        // Each finds the clash, moves to its LAN SteamID on the network and republishes.
        assert!(book_a.refresh(&a).clashed_now);
        assert!(book_b.refresh(&b).clashed_now);
        a.set_presence(book_a.presence());
        b.set_presence(book_b.presence());
        let (alias_a, alias_b) = (lan_steam_id(a.me()), lan_steam_id(b.me()));
        pump(&[&a, &b], &mut ev, "republished SteamIDs", |_| {
            a.presence(b.me()).contains(&(KEY_STEAM_ID.into(), alias_b.to_string()))
                && b.presence(a.me()).contains(&(KEY_STEAM_ID.into(), alias_a.to_string()))
        });
        assert!(!book_a.refresh(&a).clashed_now, "a clash is reported once");
        book_b.refresh(&b);
        // Each game keeps its own SteamID and sees the other under a different one, on
        // the same side of its own on both: one order of players everywhere.
        assert_eq!((book_a.mine, book_b.mine), (SHARED, SHARED));
        let (shown_b, shown_a) = (ordered_steam_id(SHARED, a.me(), b.me()), ordered_steam_id(SHARED, b.me(), a.me()));
        assert_eq!(book_a.shown(b.me()), Some(shown_b));
        assert_eq!(book_b.shown(a.me()), Some(shown_a));
        assert_ne!(book_b.shown(a.me()), Some(SHARED));
        assert_eq!(shown_a < SHARED, a.me().0 < b.me().0);
        assert_eq!(shown_b > SHARED, a.me().0 < b.me().0);

        // A hosts; B's GetLobbyOwner is A (not B itself), and joining goes to A.
        let hosted = a.create_lobby(Lobby::new("steam")).unwrap();
        let search = b.search(Query::new("steam"));
        let mut found = None;
        pump(&[&a, &b], &mut ev, "B's lobby search", |e| {
            if let Some(Event::SearchDone { results, .. }) = e[1].iter().find(|x| matches!(x, Event::SearchDone { search: s, .. } if *s == search)) {
                found = results.first().cloned();
                return true;
            }
            false
        });
        let found = found.expect("B finds A's lobby");
        let owner_for_b = book_b.shown(found.owner).unwrap();
        assert_eq!(owner_for_b, shown_a);
        assert_ne!(owner_for_b, book_b.mine, "the owner isn't B itself");
        assert_eq!(book_b.peer(owner_for_b), Some(a.me()));
        b.join(&found).unwrap();
        pump(&[&a, &b], &mut ev, "the join", |e| {
            e[1].contains(&Event::JoinDone { lobby: hosted.id.clone(), result: JoinResult::Ok })
                && e[0].contains(&Event::MemberStatus { lobby: hosted.id.clone(), member: b.me(), status: MemberStatus::Joined })
        });
        // A's members: itself under its own SteamID, B under B's LAN one.
        assert_eq!(book_a.shown(b.me()), Some(shown_b));

        // P2P: B's game sends to A's SteamID a packet naming itself (SHARED) and A (shown_a).
        let to = book_b.peer(shown_a).expect("routes to A");
        assert_eq!(to, a.me());
        let mut body = Vec::new();
        body.extend_from_slice(&SHARED.to_le_bytes());
        body.extend_from_slice(&shown_a.to_le_bytes());
        // Account IDs too, as a game's own netcode may send them (a player list).
        body.extend_from_slice(&(SHARED as u32).to_le_bytes());
        body.extend_from_slice(&(shown_a as u32).to_le_bytes());
        body.extend_from_slice(format!("host={shown_a};me={SHARED}").as_bytes());
        let out = swapped_out(&book_b, to, &body, true);
        assert_eq!(&out[8..16], &alias_a.to_le_bytes(), "A goes as its network SteamID; B's SHARED is left for A");
        assert_eq!(&out[0..8], &SHARED.to_le_bytes());
        b.send(to, "steam.p2p", 0, &out, true, true, true);
        let mut got = None;
        pump(&[&a, &b], &mut ev, "the packet", |e| {
            if e[0].iter().any(|x| matches!(x, Event::ChanRequest { peer, socket } if *peer == b.me() && socket == "steam.p2p")) {
                a.accept(b.me(), "steam.p2p");
            }
            got = got.take().or_else(|| a.receive_where(Some(0), |s| s == "steam.p2p"));
            got.is_some()
        });
        let mut p = got.unwrap();
        assert_eq!(p.peer, b.me());
        assert_eq!(book_a.shown(p.peer), Some(shown_b), "the sender is B, not A");
        assert_eq!(swapped_in(&book_a, p.peer, &mut p.data, true), Swapped { ids: 4, accounts: 2 });
        // A's game reads B as A shows B and itself as its own, account IDs too: B no
        // longer looks like A in a list of players' account IDs.
        assert_eq!(&p.data[0..8], &shown_b.to_le_bytes());
        assert_eq!(&p.data[8..16], &SHARED.to_le_bytes());
        assert_eq!(&p.data[16..20], &(shown_b as u32).to_le_bytes());
        assert_eq!(&p.data[20..24], &(SHARED as u32).to_le_bytes());
        assert_ne!(shown_b as u32, SHARED as u32);
        assert_eq!(&p.data[24..], format!("host={SHARED};me={shown_b}").as_bytes());
    }
}
