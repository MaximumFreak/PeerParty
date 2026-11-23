//! The game's side of PeerParty's overlay. The overlay itself (pages, controllers,
//! drawing) lives in the peerparty.exe that started the game (launcher/src/host); this
//! is what the game process does for it:
//!
//! - SDK calls that would open Steam's, Epic's or GOG's overlay (an invite dialog, the
//!   friends list) ask the launcher for the matching page (`steam_dialog`,
//!   `invite_dialog`, `page`). Without a launcher they return false and the caller does
//!   what it did before.
//! - While the overlay shows, the game hears its SDK's overlay event
//!   (each `OverlayListener`: `GameOverlayActivated_t`, EOS's display settings, Galaxy's
//!   overlay listener) and controllers stop reaching it
//!   (padblock.rs). Keyboard and mouse are held back by the launcher itself.
//! - When it closes with a controller button still down (the Start or B that closed
//!   it), controllers stay held back until the launcher reads that button up (`pads_held`):
//!   let go at once, the game would read the button as a fresh press and open its pause
//!   menu or back out of a menu.
//! - When it opens with a controller button down (the game's own invite button, pressed
//!   to ask for it), a controller the game reads directly (HID) goes on reporting as it
//!   is until the launcher reads that button up (`pads_held` again), and is held at a
//!   report from then on (`hid_reports_fresh`): held at one with the button down, the
//!   game would read it held for as long as the overlay shows, and press the button it's
//!   on over and over.
//! - `frame_ready`: the launcher's shared frame for drawing inside the game (present.rs).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use peerparty_core::{Lobby, PeerId};

use crate::log;

static VISIBLE: AtomicBool = AtomicBool::new(false);

/// An SDK whose game hears PeerParty's overlay as its own (the table: hub/mod.rs
/// `OVERLAY`): Steam's `GameOverlayActivated_t`, EOS's display settings, Galaxy's
/// overlay visibility listener.
pub struct OverlayListener {
    /// The overlay opened (true) or closed.
    pub on_overlay_visible: fn(bool),
}

/// Set while controllers are held back after the overlay closed (`HOLD` says how long).
static HELD: AtomicBool = AtomicBool::new(false);
static HOLD: Mutex<Hold> = Mutex::new(Hold { until: None });
/// The launcher says when the buttons are up (within a second); this is in case it
/// never does.
const PADS_HELD_FOR: Duration = Duration::from_secs(2);

/// Controllers held back from the game after the overlay closed with buttons down.
struct Hold {
    until: Option<Instant>,
}

impl Hold {
    fn start(&mut self, now: Instant) {
        self.until = Some(now + PADS_HELD_FOR);
    }

    /// Ends `d` from `now`, if it's on and would last longer.
    fn shorten(&mut self, now: Instant, d: Duration) {
        if let Some(u) = self.until.as_mut() {
            *u = (*u).min(now + d);
        }
    }

    fn on(&self, now: Instant) -> bool {
        self.until.is_some_and(|u| now < u)
    }

    /// True if a hold ended (timed out or not): padblock's state for that opening goes.
    fn end(&mut self) -> bool {
        self.until.take().is_some()
    }
}

/// Set while the overlay is open and the buttons down when it opened aren't up yet
/// (`hid_reports_fresh`), and a moment after they are.
static FRESH: Mutex<Hold> = Mutex::new(Hold { until: None });
/// How long reports are still taken once the launcher read the buttons up: its reading
/// and the game's are a few milliseconds apart.
const FRESH_AFTER_UP: Duration = Duration::from_millis(60);

/// Whether a controller the game reads directly reports as it is while the overlay
/// shows (the buttons that opened it aren't up yet), its held report taken afresh.
pub fn hid_reports_fresh() -> bool {
    crate::sys::lock(&FRESH).on(Instant::now())
}

/// What the game's invite dialog asked to invite to: used by the overlay's invite
/// buttons until the overlay closes.
static DIALOG: Mutex<Option<(String, Option<Lobby>)>> = Mutex::new(None);

pub fn visible() -> bool {
    VISIBLE.load(Ordering::Relaxed)
}

/// Whether controllers are held back from the game: while the overlay shows, and after
/// it closed with buttons down until they're up (padblock's detours ask this).
pub fn pads_held_back() -> bool {
    visible() || (HELD.load(Ordering::Relaxed) && crate::sys::lock(&HOLD).on(Instant::now()))
}

/// Tests that make the overlay "show" take turns (the switch is one per process).
#[cfg(test)]
pub fn test_turn() -> std::sync::MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    crate::sys::lock(&TURN)
}

/// Tests: the overlay "shows" without anything else happening.
#[cfg(test)]
pub fn set_visible_for_test(on: bool) {
    VISIBLE.store(on, Ordering::Relaxed);
}

/// The launcher showed or hid the overlay.
pub fn set_visible(on: bool) {
    set_visible_holding(on, false);
}

/// `pads_held`: it closed with controller buttons down, so controllers stay held back
/// until `pads_released` (or PADS_HELD_FOR); or it opened with them down, so a
/// controller the game reads directly is held at a report taken once they're up.
pub fn set_visible_holding(on: bool, pads_held: bool) {
    // Held before VISIBLE goes false, so no read in between gets through.
    if !on && pads_held && visible() {
        crate::sys::lock(&HOLD).start(Instant::now());
        HELD.store(true, Ordering::Relaxed);
    }
    if on && pads_held && !visible() {
        crate::sys::lock(&FRESH).start(Instant::now());
    }
    if !on {
        crate::sys::lock(&FRESH).end();
    }
    if VISIBLE.swap(on, Ordering::Relaxed) == on {
        return;
    }
    let note = match (on, pads_held) {
        (true, true) => ", the game's controllers held once the buttons down now are up",
        (false, true) => ", controllers held back until their buttons are up",
        _ => "",
    };
    log!("overlay {}{note}", if on { "shown" } else { "hidden" });
    if on {
        // A hold from the last close that timed out: its state goes before this opening.
        if end_hold() {
            crate::input::hidden();
        }
        crate::input::install();
    } else {
        *crate::sys::lock(&DIALOG) = None;
        if !HELD.load(Ordering::Relaxed) {
            crate::input::hidden();
        }
    }
    for l in crate::hub::OVERLAY {
        (l.on_overlay_visible)(on);
    }
}

/// The launcher read the buttons that were down when the overlay closed up again (or
/// stopped waiting for them, or went away): controllers reach the game again. While
/// it's open, the ones down when it opened are up: the game's held reports are taken
/// from now.
pub fn pads_released() {
    if visible() {
        crate::sys::lock(&FRESH).shorten(Instant::now(), FRESH_AFTER_UP);
        return;
    }
    if end_hold() && !visible() {
        log!("overlay: controller buttons up, controllers reach the game again");
        crate::input::hidden();
    }
}

fn end_hold() -> bool {
    HELD.store(false, Ordering::Relaxed);
    crate::sys::lock(&HOLD).end()
}

/// Asks the launcher to close the overlay (a game's "hide the friends list").
pub fn close() {
    crate::ipc::send(peerparty_protocol::ipc::HookMsg::Close);
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
    *crate::sys::lock(&DIALOG) = Some((connect.to_string(), lobby));
    page("invite", None)
}

/// What the game's invite dialog asked to invite to, while the overlay is open.
pub fn dialog_target() -> Option<(String, Option<Lobby>)> {
    if !visible() {
        return None;
    }
    crate::sys::lock(&DIALOG).clone()
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

/// The launcher's frame for drawing inside the game is ready (`video`).
pub fn frame_ready(name: &str) {
    crate::video::attach(name);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hold lasts until it's ended or PADS_HELD_FOR passes, and ending it says once
    /// that there was one (so padblock's state for that opening goes once).
    #[test]
    fn a_hold_ends_when_released_or_after_a_while() {
        let now = Instant::now();
        let mut h = Hold { until: None };
        assert!(!h.on(now));
        assert!(!h.end(), "nothing to end");
        h.start(now);
        assert!(h.on(now) && h.on(now + Duration::from_millis(1900)));
        assert!(!h.on(now + PADS_HELD_FOR), "not held for ever if the launcher never says");
        assert!(h.end());
        assert!(!h.on(now) && !h.end());
        // Timed out, it still ends once.
        h.start(now);
        assert!(!h.on(now + Duration::from_secs(3)));
        assert!(h.end() && !h.end());
        // Shortened: over soon after, never made longer, nothing when off.
        h.start(now);
        h.shorten(now, FRESH_AFTER_UP);
        assert!(h.on(now) && !h.on(now + FRESH_AFTER_UP));
        h.shorten(now, PADS_HELD_FOR * 2);
        assert!(!h.on(now + FRESH_AFTER_UP));
        assert!(h.end());
        h.shorten(now, FRESH_AFTER_UP);
        assert!(!h.on(now));
    }
}
