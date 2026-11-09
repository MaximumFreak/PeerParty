//! The game's side of PeerParty's overlay. The overlay itself (pages, controllers,
//! drawing) lives in the peerparty.exe that started the game (launcher/src/host); this
//! is what the game process does for it:
//!
//! - SDK calls that would open Steam's, Epic's or GOG's overlay (an invite dialog, the
//!   friends list) ask the launcher for the matching page (`steam_dialog`,
//!   `invite_dialog`, `page`). Without a launcher they return false and the caller does
//!   what it did before.
//! - While the overlay shows, the game hears its SDK's overlay event
//!   (`GameOverlayActivated_t`, EOS's display settings) and controllers stop reaching it
//!   (padblock.rs). Keyboard and mouse are held back by the launcher itself.
//! - `frame_ready`: the launcher's shared frame for drawing inside the game (present.rs).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use peerparty_core::{Lobby, PeerId};

use crate::log;

static VISIBLE: AtomicBool = AtomicBool::new(false);

/// What the game's invite dialog asked to invite to: used by the overlay's invite
/// buttons until the overlay closes.
static DIALOG: Mutex<Option<(String, Option<Lobby>)>> = Mutex::new(None);

pub fn visible() -> bool {
    VISIBLE.load(Ordering::Relaxed)
}

/// Tests: the overlay "shows" without anything else happening.
#[cfg(test)]
pub fn set_visible_for_test(on: bool) {
    VISIBLE.store(on, Ordering::Relaxed);
}

/// The launcher showed or hid the overlay.
pub fn set_visible(on: bool) {
    if VISIBLE.swap(on, Ordering::Relaxed) == on {
        return;
    }
    log!("overlay {}", if on { "shown" } else { "hidden" });
    if on {
        crate::padblock::install();
    } else {
        *DIALOG.lock().unwrap() = None;
        crate::padblock::hidden();
    }
    crate::steamlan::overlay_activated(on);
    crate::eos::overlay_visible(on);
    crate::galaxy::overlay_changed(on);
}

/// Asks for an overlay page. False when there's no overlay to show it.
pub fn page(page: &str, peer: Option<PeerId>) -> bool {
    crate::ipc::open(page, peer)
}

/// The game's own invite dialog: the overlay's invite page, inviting to `connect`
/// and/or `lobby`.
pub fn invite_dialog(connect: &str, lobby: Option<Lobby>) -> bool {
    if !crate::ipc::connected() {
        return false;
    }
    *DIALOG.lock().unwrap() = Some((connect.to_string(), lobby));
    page("invite", None)
}

/// What the game's invite dialog asked to invite to, while the overlay is open.
pub fn dialog_target() -> Option<(String, Option<Lobby>)> {
    if !visible() {
        return None;
    }
    DIALOG.lock().unwrap().clone()
}

/// Steam's `ActivateGameOverlay` / `ActivateGameOverlayToUser` dialog names, to pages.
pub fn steam_dialog(dialog: &str, peer: Option<PeerId>) -> bool {
    let d = dialog.to_ascii_lowercase();
    let p = match (d.as_str(), peer) {
        (_, Some(_)) => "profile",
        ("friends" | "players" | "community" | "chat" | "officialgamegroup", None) => "friends",
        ("lobbyinvite", None) => "invite",
        ("settings", None) => "settings",
        _ => "home",
    };
    page(p, peer)
}

/// The launcher's frame for drawing inside the game is ready (present.rs).
pub fn frame_ready(name: &str) {
    crate::present::attach(name);
}
