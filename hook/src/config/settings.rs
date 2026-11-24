//! Settings that can change while the game runs. Each starts from the environment the
//! launcher set up (`PEERPARTY_INVITES`, ...) and can be
//! changed afterwards by the launcher process over the pipe (ipc.rs), when the player
//! flips a switch in the overlay or the launcher window.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

use crate::log;

/// Invites from other players are accepted without asking. Off unless
/// `PEERPARTY_INVITES=auto`: with it off the game hears an invite arrived (where its SDK
/// has such an event) and the overlay asks the player.
static AUTO_ACCEPT: AtomicBool = AtomicBool::new(false);

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let invites = &super::HookConfig::global().invites;
        AUTO_ACCEPT.store(parse_auto_accept(invites), Ordering::Relaxed);
    });
}

fn parse_auto_accept(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "auto" | "accept" | "on" | "1" | "lan")
}

pub fn auto_accept() -> bool {
    init();
    AUTO_ACCEPT.load(Ordering::Relaxed)
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
    }
}
