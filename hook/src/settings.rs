//! Settings that can change while the game runs. Each starts from the environment the
//! launcher set up (`PEERPARTY_INVITES`, `PEERPARTY_INVITE_BUTTONS`, ...) and can be
//! changed afterwards by the launcher process over the pipe (ipc.rs), when the player
//! flips a switch in the overlay or the launcher window.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Once;

use crate::log;

/// Invites from other players are accepted without asking. Off unless
/// `PEERPARTY_INVITES=auto`: with it off the game hears an invite arrived (where its SDK
/// has such an event) and the overlay asks the player.
static AUTO_ACCEPT: AtomicBool = AtomicBool::new(false);

/// Which controller buttons invite every other player: a bit set of `GUIDE` and `COMBO`.
/// None unless `PEERPARTY_INVITE_BUTTONS` names some; `peerparty --invite` and the
/// overlay's button always work.
static INVITE_BUTTONS: AtomicU8 = AtomicU8::new(0);

pub const GUIDE: u8 = 1;
pub const COMBO: u8 = 2;

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let invites = std::env::var("PEERPARTY_INVITES").unwrap_or_default();
        AUTO_ACCEPT.store(parse_auto_accept(&invites), Ordering::Relaxed);
        let buttons = std::env::var("PEERPARTY_INVITE_BUTTONS").unwrap_or_default();
        INVITE_BUTTONS.store(parse_buttons(&buttons), Ordering::Relaxed);
    });
}

fn parse_auto_accept(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "auto" | "accept" | "on" | "1" | "lan")
}

fn parse_buttons(v: &str) -> u8 {
    v.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .fold(0, |bits, s| match s.as_str() {
            "guide" | "xbox" => bits | GUIDE,
            "combo" | "view+menu" | "back+start" => bits | COMBO,
            "both" | "all" => bits | GUIDE | COMBO,
            _ => bits,
        })
}

pub fn auto_accept() -> bool {
    init();
    AUTO_ACCEPT.load(Ordering::Relaxed)
}

pub fn invite_buttons() -> u8 {
    init();
    INVITE_BUTTONS.load(Ordering::Relaxed)
}

/// A setting changed in the launcher process. Unknown keys are ignored (an older hook
/// under a newer launcher). Returns whether the key was known.
pub fn set(key: &str, value: &str) -> bool {
    init();
    match key {
        "auto_accept" => {
            let on = parse_auto_accept(value);
            if AUTO_ACCEPT.swap(on, Ordering::Relaxed) != on {
                log!("settings: invites from other players are {}", if on { "accepted at once" } else { "asked about" });
            }
        }
        "invite_buttons" => {
            let bits = parse_buttons(value);
            if INVITE_BUTTONS.swap(bits, Ordering::Relaxed) != bits {
                log!("settings: invite-everyone buttons now {value:?}");
            }
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_parse() {
        assert!(parse_auto_accept("auto"));
        assert!(parse_auto_accept(" ON "));
        assert!(!parse_auto_accept("ask"));
        assert!(!parse_auto_accept(""));
        assert_eq!(parse_buttons(""), 0);
        assert_eq!(parse_buttons("guide"), GUIDE);
        assert_eq!(parse_buttons("combo, guide"), GUIDE | COMBO);
        assert_eq!(parse_buttons("both"), GUIDE | COMBO);
        assert_eq!(parse_buttons("none"), 0);
    }
}
