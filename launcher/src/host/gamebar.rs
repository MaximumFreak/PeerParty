//! Whether the Xbox Game Bar overlay is open over the game, so the overlay leaves the
//! controller, keyboard and mouse to it meanwhile.
//!
//! Game Bar's open overlay is a full-screen CoreWindow owned by GameBar.exe that never
//! becomes the foreground window (the game stays in front, so the host's foreground
//! check doesn't notice it) and that `EnumWindows` doesn't list. So the game's area is
//! hit-tested instead: open when at least 4 of 5 points land on a Game Bar window (a
//! pinned widget is small). The same probe as DSZ's (virt/gamebar.rs), checked there
//! against Game Bar 7.326.
//!
//! Other overlays that take the foreground (Task View, Alt+Tab, the Start menu) already
//! close PeerParty's: the game is no longer in front.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, WindowFromPoint};

/// Look this often (each look is five WindowFromPoint calls).
const EVERY: Duration = Duration::from_millis(100);
/// Forget which processes are Game Bar this often (process ids get reused).
const NAMES_FOR: Duration = Duration::from_secs(30);

pub struct GameBar {
    open: bool,
    checked: Option<Instant>,
    names: HashMap<u32, bool>,
    names_at: Instant,
}

impl GameBar {
    pub fn new() -> GameBar {
        GameBar { open: false, checked: None, names: HashMap::new(), names_at: Instant::now() }
    }

    /// Whether Game Bar covers `area` (the game's client area on screen), looked at
    /// most every EVERY.
    pub fn open(&mut self, area: Option<RECT>) -> bool {
        let now = Instant::now();
        if self.checked.is_some_and(|t| now.duration_since(t) < EVERY) {
            return self.open;
        }
        self.checked = Some(now);
        if now.duration_since(self.names_at) >= NAMES_FOR {
            self.names.clear();
            self.names_at = now;
        }
        self.open = area.is_some_and(|r| covered(r, &mut self.names));
        self.open
    }
}

/// The five points looked at in `r`: the middle and the middle of each quarter.
fn points(r: RECT) -> [(i32, i32); 5] {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let at = |fx: i32, fy: i32| (r.left + w * fx / 4, r.top + h * fy / 4);
    [at(2, 2), at(1, 1), at(3, 1), at(1, 3), at(3, 3)]
}

/// At least 4 of the 5 points land on Game Bar. `names` caches process id -> Game Bar.
fn covered(r: RECT, names: &mut HashMap<u32, bool>) -> bool {
    let mut hits = 0;
    for (i, (x, y)) in points(r).into_iter().enumerate() {
        let mut pid = 0u32;
        unsafe {
            let hwnd = WindowFromPoint(POINT { x, y });
            if !hwnd.is_null() {
                GetWindowThreadProcessId(hwnd, &mut pid);
            }
        }
        if pid != 0 && *names.entry(pid).or_insert_with(|| image_name(pid).is_some_and(|n| is_gamebar_exe(&n))) {
            hits += 1;
        }
        // Two misses: it can't reach 4.
        if i + 1 - hits >= 2 {
            return false;
        }
    }
    hits >= 4
}

fn is_gamebar_exe(name: &str) -> bool {
    name.eq_ignore_ascii_case("gamebar.exe") || name.eq_ignore_ascii_case("xboxgamebarwidgets.exe")
}

/// A process's exe file name.
fn image_name(pid: u32) -> Option<String> {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) } != 0;
    unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
    if !ok {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    path.rsplit(['\\', '/']).next().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamebar_processes() {
        assert!(is_gamebar_exe("GameBar.exe"));
        assert!(is_gamebar_exe("XboxGameBarWidgets.exe"));
        assert!(!is_gamebar_exe("GameBarFTServer.exe"));
        assert!(!is_gamebar_exe("explorer.exe"));
    }

    #[test]
    fn points_fall_inside_the_game() {
        let r = RECT { left: 100, top: 50, right: 1100, bottom: 850 };
        let p = points(r);
        assert_eq!(p[0], (600, 450));
        for (x, y) in p {
            assert!(x > r.left && x < r.right && y > r.top && y < r.bottom);
        }
    }
}
