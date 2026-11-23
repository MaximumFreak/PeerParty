//! `peerparty --invite` and the window's Invite button: named events the hook's invite
//! thread waits on (hook/src/invite.rs), and the window's Close all.

use std::ptr;

use windows_sys::Win32::Foundation::CloseHandle;

use super::{os_error, wide};

/// The event the hook's invite thread watches (hook/src/invite.rs).
pub const INVITE_EVENT: &str = "Local\\peerparty.invite";
/// The hook's `EVENT_LAST` and `EVENT_ACK` (hook/src/invite.rs).
pub const INVITE_LAST_EVENT: &str = "Local\\peerparty.invite.last";
pub const INVITE_ACK_EVENT: &str = "Local\\peerparty.invite.ack";

/// Asks a running game to invite every player on the network: the game in front, or
/// with `last` the one that was last in front (for the launcher's own button, pressed
/// while the launcher is). Whether a game took it up: a game not in front, or none
/// running, doesn't, and saying "sent" then sent the player looking for an invite that
/// never went.
pub fn signal_invite(last: bool) -> Result<bool, String> {
    signal_named(if last { INVITE_LAST_EVENT } else { INVITE_EVENT }, INVITE_ACK_EVENT, 1500)
}

/// Sets `event` until a game answers on `ack` or `wait_ms` is up, then clears both. An
/// answer left from before doesn't count: `ack` is cleared first.
fn signal_named(event: &str, ack: &str, wait_ms: u32) -> Result<bool, String> {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent, WaitForSingleObject};
    let open = |name: &str| unsafe { CreateEventW(ptr::null(), 1, 0, wide(name).as_ptr()) };
    unsafe {
        let (event, ack) = (open(event), open(ack));
        if event.is_null() || ack.is_null() {
            let e = os_error("CreateEvent");
            for h in [event, ack] {
                if !h.is_null() {
                    CloseHandle(h);
                }
            }
            return Err(e);
        }
        ResetEvent(ack);
        SetEvent(event);
        // A game's trigger thread wakes on the event; one that is busy takes a moment.
        let taken = WaitForSingleObject(ack, wait_ms) == WAIT_OBJECT_0;
        ResetEvent(event);
        ResetEvent(ack);
        CloseHandle(event);
        CloseHandle(ack);
        Ok(taken)
    }
}

/// The event PeerParty's window sets to close the game this peerparty.exe (process
/// `pid`) started: Close all on its Now playing card.
pub fn close_event(pid: u32) -> Vec<u16> {
    wide(format!("Local\\peerparty-close-{pid}"))
}

/// Close all: asks the peerparty.exe of process `pid` to close the game it started.
/// Whether it was asked (it has the event while its game runs).
pub fn close_game(pid: u32) -> bool {
    use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
    unsafe {
        let ev = OpenEventW(EVENT_MODIFY_STATE, 0, close_event(pid).as_ptr());
        if ev.is_null() {
            return false;
        }
        let asked = SetEvent(ev) != 0;
        CloseHandle(ev);
        asked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The launcher's names for the invite events are the hook's.
    #[test]
    fn invite_event_names_match_the_hook() {
        let hook = include_str!("../../../../hook/src/invite/mod.rs");
        for (name, value) in [("EVENT_NAME", INVITE_EVENT), ("EVENT_LAST", INVITE_LAST_EVENT), ("EVENT_ACK", INVITE_ACK_EVENT)] {
            let line = format!("pub const {name}: &str = \"{}\";", value.replace('\\', "\\\\"));
            assert!(hook.contains(&line), "hook/src/invite/mod.rs has no `{line}`");
        }
    }

    /// An invite reports whether a game took it: no game is a "no" (after the wait), an
    /// answer left from an earlier invite doesn't count, a game's answer is a "yes", and
    /// the event is cleared after either (a game started later isn't triggered by it).
    /// Names of its own, so a game running on this PC isn't asked.
    #[test]
    fn an_invite_says_whether_a_game_took_it() {
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
        let id = std::process::id();
        let (event, ack) = (format!("Local\\peerparty.test.invite.{id}"), format!("Local\\peerparty.test.invite.ack.{id}"));
        let open = |name: &str| {
            let h = unsafe { CreateEventW(ptr::null(), 1, 0, wide(name).as_ptr()) };
            assert!(!h.is_null());
            h as usize
        };
        let (ev, ak) = (open(&event), open(&ack));
        // Nobody listening, and an old answer still set.
        unsafe { SetEvent(ak as _) };
        let start = std::time::Instant::now();
        assert_eq!(signal_named(&event, &ack, 300), Ok(false));
        assert!(start.elapsed() >= std::time::Duration::from_millis(250));
        assert_eq!(unsafe { WaitForSingleObject(ev as _, 0) }, WAIT_TIMEOUT, "the event was left set");
        // A game that answers, the way hook/src/invite.rs does.
        let game = std::thread::spawn(move || {
            let woke = unsafe { WaitForSingleObject(ev as _, 5000) } == WAIT_OBJECT_0;
            if woke {
                unsafe { SetEvent(ak as _) };
            }
            woke
        });
        assert_eq!(signal_named(&event, &ack, 5000), Ok(true));
        assert!(game.join().unwrap());
        assert_eq!(unsafe { WaitForSingleObject(ev as _, 0) }, WAIT_TIMEOUT, "the event was left set");
        assert_eq!(unsafe { WaitForSingleObject(ak as _, 0) }, WAIT_TIMEOUT, "the answer was left set");
        unsafe {
            CloseHandle(ev as _);
            CloseHandle(ak as _);
        }
    }
}
