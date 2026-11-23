//! Typing a room code into the game, for games that join by a code (Photon rooms) and
//! have no invite to carry it: the player's overlay hides, then the code is typed as key
//! presses into the game's window, which has the code box focused (the player opened it).
//! The clipboard gets the code too, so Ctrl+V works where typing doesn't.

use std::time::{Duration, Instant};

use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC,
    MapVirtualKeyW, SendInput, VK_CAPITAL, VK_SHIFT, VkKeyScanW,
};

use crate::log;

const CF_UNICODETEXT: u32 = 13;
/// The longest code we'll type: room codes are a handful of characters.
const MAX_CODE: usize = 64;

/// A code from outside (a player's message or the clipboard) as it can be typed:
/// letters, digits, `-` and `_`; anything else, or too long, is no code.
pub fn clean_code(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty() && t.chars().count() <= MAX_CODE && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).then(|| t.to_string())
}

/// The clipboard's text, if it has any.
pub fn clipboard_text() -> Option<String> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let mut out = None;
        let h = GetClipboardData(CF_UNICODETEXT);
        if !h.is_null() {
            let p = GlobalLock(h).cast::<u16>();
            if !p.is_null() {
                let mut n = 0;
                while n < 4096 && *p.add(n) != 0 {
                    n += 1;
                }
                out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)));
                GlobalUnlock(h);
            }
        }
        CloseClipboard();
        out
    }
}

/// Puts `text` on the clipboard.
pub fn set_clipboard(text: &str) -> bool {
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
        let mut ok = false;
        if !h.is_null() {
            let p = GlobalLock(h).cast::<u16>();
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                GlobalUnlock(h);
                // The clipboard owns the memory once it takes it.
                ok = !SetClipboardData(CF_UNICODETEXT, h).is_null();
            }
        }
        CloseClipboard();
        ok
    }
}

fn key(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

fn send(inputs: &[INPUT]) {
    unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
}

fn scan(vk: u32) -> u16 {
    unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) as u16 }
}

/// One character as a key press: by scan code (what games reading the keyboard directly
/// and ones reading characters both see), with Shift where the layout needs it; a
/// character the layout has no plain key for goes as a Unicode character.
fn type_char(c: char) {
    let mut utf16 = [0u16; 2];
    let units = c.encode_utf16(&mut utf16);
    let found = if units.len() == 1 { unsafe { VkKeyScanW(units[0]) } } else { -1 };
    let (vk, mods) = ((found & 0xff) as u32, (found >> 8) & 0xff);
    let sc = if found == -1 { 0 } else { scan(vk) };
    // Only Shift is handled; Ctrl / Alt combinations are sent as Unicode.
    if found == -1 || mods & !1 != 0 || sc == 0 {
        for u in units.iter() {
            send(&[key(0, *u, KEYEVENTF_UNICODE), key(0, *u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)]);
        }
        return;
    }
    // Caps Lock flips what Shift does to letters.
    let caps = unsafe { GetKeyState(i32::from(VK_CAPITAL)) } & 1 != 0;
    let shift = (mods & 1 != 0) != (caps && c.is_ascii_alphabetic());
    let shift_scan = scan(u32::from(VK_SHIFT));
    if shift {
        send(&[key(0, shift_scan, KEYEVENTF_SCANCODE)]);
    }
    send(&[key(0, sc, KEYEVENTF_SCANCODE), key(0, sc, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP)]);
    if shift {
        send(&[key(0, shift_scan, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP)]);
    }
}

fn game_in_front() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
    pid == std::process::id()
}

/// The lParam of a key message for scan code `sc`: one press, and for a key going up
/// the previous-state and transition bits.
fn key_lparam(sc: u16, up: bool) -> isize {
    let l = 1 | ((sc as isize) << 16);
    if up { l | (1 << 30) | (1 << 31) } else { l }
}

/// One character into this copy's own window as the messages a key press makes: the
/// key down (marked as already translated, window.rs `forwarded_key`), its character,
/// the key up.
fn post_char(hwnd: usize, c: char) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CHAR, WM_KEYDOWN, WM_KEYUP};
    let mut utf16 = [0u16; 2];
    let units = c.encode_utf16(&mut utf16);
    let found = if units.len() == 1 { unsafe { VkKeyScanW(units[0]) } } else { -1 };
    let vk = if found == -1 { 0 } else { (found & 0xff) as usize };
    let sc = if vk == 0 { 0 } else { scan(vk as u32) };
    // An uppercase letter or `_` is its key with Shift: a game reading the keys (not the
    // characters) sees Shift go down around it.
    let shift = vk != 0 && shifted(found);
    let shift_sc = scan(u32::from(VK_SHIFT));
    let mark = peerparty_protocol::session::FORWARDED_KEY;
    let h = hwnd as windows_sys::Win32::Foundation::HWND;
    unsafe {
        if shift {
            PostMessageW(h, WM_KEYDOWN, usize::from(VK_SHIFT), key_lparam(shift_sc, false) | mark);
        }
        if vk != 0 {
            PostMessageW(h, WM_KEYDOWN, vk, key_lparam(sc, false) | mark);
        }
        for u in units.iter() {
            PostMessageW(h, WM_CHAR, usize::from(*u), key_lparam(sc, false));
        }
        if vk != 0 {
            PostMessageW(h, WM_KEYUP, vk, key_lparam(sc, true) | mark);
        }
        if shift {
            PostMessageW(h, WM_KEYUP, usize::from(VK_SHIFT), key_lparam(shift_sc, true) | mark);
        }
    }
}

/// Whether `VkKeyScanW`'s answer for a character needs Shift (its high byte's lowest
/// bit; -1, none).
fn shifted(found: i16) -> bool {
    found != -1 && (found >> 8) & 1 != 0
}

/// Types `code` into the game once the overlay is gone and the game has the keyboard
/// (the player's overlay closes as they pick this). Off the caller's thread.
///
/// A split-screen copy gets it in its own window: the PC's keyboard goes to whichever
/// pane has the focus, which needn't be the one whose player asked.
pub fn type_code(code: String) {
    set_clipboard(&code);
    if crate::window::focus_spoofed() {
        let _ = std::thread::Builder::new().name("peerparty-type".into()).spawn(move || {
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until && crate::overlay::visible() {
                std::thread::sleep(Duration::from_millis(50));
            }
            let hwnd = crate::window::game_window();
            if crate::overlay::visible() || hwnd == 0 {
                log!("type: the overlay stayed open or the game has no window yet; the code {code} is on the clipboard instead");
                return;
            }
            std::thread::sleep(Duration::from_millis(300));
            for c in code.chars() {
                post_char(hwnd, c);
                std::thread::sleep(Duration::from_millis(25));
            }
            log!("type: typed the room code {code} into this copy's window ({} characters)", code.chars().count());
        });
        return;
    }
    let _ = std::thread::Builder::new().name("peerparty-type".into()).spawn(move || {
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until && (crate::overlay::visible() || !game_in_front()) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if crate::overlay::visible() || !game_in_front() {
            // Typing now would go to another program. The clipboard has the code.
            log!("type: the game never got the keyboard back; the code {code} is on the clipboard instead");
            return;
        }
        std::thread::sleep(Duration::from_millis(300));
        for c in code.chars() {
            type_char(c);
            std::thread::sleep(Duration::from_millis(25));
        }
        log!("type: typed the room code {code} into the game ({} characters)", code.chars().count());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shift goes down around an uppercase letter or `_`, not around the rest.
    #[test]
    fn shifted_characters_need_shift() {
        let scan = |c: char| unsafe { VkKeyScanW(c as u16) };
        assert!(!shifted(-1));
        assert!(shifted(0x0141), "Shift+A, as VkKeyScanW gives it");
        assert!(!shifted(0x0041));
        // On this PC's layout, whatever it is: an uppercase letter is shifted, its lowercase isn't.
        if scan('a') != -1 && scan('A') != -1 {
            assert!(shifted(scan('A')) && !shifted(scan('a')));
        }
    }

    #[test]
    fn only_plain_codes_are_typed() {
        assert_eq!(clean_code("  EOXBY \n").as_deref(), Some("EOXBY"));
        assert_eq!(clean_code("ab-12_x").as_deref(), Some("ab-12_x"));
        assert_eq!(clean_code(""), None);
        assert_eq!(clean_code("two words"), None);
        assert_eq!(clean_code(&"A".repeat(65)), None);
    }

    #[test]
    fn key_messages_carry_their_scan_code() {
        assert_eq!(key_lparam(0x1e, false), 0x001e_0001);
        assert_eq!(key_lparam(0x1e, true) as u32, 0xc01e_0001);
        assert!(crate::window::forwarded_key(0x0100, key_lparam(0x1e, false) | peerparty_protocol::session::FORWARDED_KEY));
        assert!(!crate::window::forwarded_key(0x0100, key_lparam(0x1e, false)), "a real key press is translated");
        assert!(!crate::window::forwarded_key(0x0102, peerparty_protocol::session::FORWARDED_KEY), "WM_CHAR isn't a key");
    }
}
