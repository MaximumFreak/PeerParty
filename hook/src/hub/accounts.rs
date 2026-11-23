//! A LAN player's accounts on the platforms PeerParty stands in for, for the layers that
//! need another platform's ids: Demonware signs in with the player's SteamID, EOS maps
//! Steam and Xbox Live external accounts, PlayFab Party's Xbox Live chat permissions
//! follow what the real Xbox Live privacy service said. Each answer is the owning
//! layer's (steam/, xboxlive/); a layer asks here rather than reaching into another's
//! internals.

use peerparty_core::PeerId;

/// This player's SteamID (0 before the LAN starts).
pub fn my_steam_id() -> u64 {
    crate::steam::lan::my_steam_id()
}

/// The SteamID `peer` is shown under.
pub fn steam_id(peer: PeerId) -> u64 {
    crate::steam::lan::steam_id(peer)
}

/// The LAN player shown under SteamID `id`.
pub fn peer_of_steam_id(id: u64) -> Option<PeerId> {
    crate::steam::lan::peer_of(id)
}

/// The XUID `peer` is shown under.
pub fn xuid(peer: PeerId) -> u64 {
    crate::xboxlive::xuid_of(peer)
}

/// The LAN player shown under XUID `xuid`.
pub fn peer_of_xuid(xuid: u64) -> Option<PeerId> {
    crate::xboxlive::peer_of(xuid)
}

/// Every chat permission (`xboxlive::real::ALL_CHAT`).
pub const XBOX_ALL_CHAT: u32 = crate::xboxlive::real::ALL_CHAT;

/// What the real Xbox Live privacy service lets this player do with `target` in chat:
/// (permission mask, reason).
pub fn xbox_chat_permission(target: u64) -> (u32, u32) {
    crate::xboxlive::real::chat_permission(target)
}

/// Goes up each time the real privacy service says something new.
pub fn xbox_privacy_generation() -> u64 {
    crate::xboxlive::real::generation()
}
