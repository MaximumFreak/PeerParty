//! What the overlay's host asks of the desktop around the game: whether the game is in
//! front and where its window is, the keyboard's state, waiting for the game to end
//! while the host's windows get their messages, and where the overlay's frame file goes.
//!
//! On Windows that's the game's window and Windows' own state. On Linux the game is a
//! Windows program under Wine, usually inside Gamescope (SteamOS), where the game has the
//! screen: it's always in front, its "window" is the screen, and the overlay is drawn
//! inside the game's frames only (`DRAWS_OVER_GAMES` is false).

/// An area on screen, in pixels (right and bottom exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Who has the foreground: a process, whether its window is a console's, or nothing
/// (`none`: a Windows prompt has the screen).
pub struct Foreground {
    pub pid: u32,
    pub none: bool,
    pub console: bool,
}

/// The `(width, height)` of a mode line such as `1280x800`, landscape (the Steam Deck's
/// panel is a portrait one turned on its side).
#[cfg(any(unix, test))]
pub fn landscape_mode(line: &str) -> Option<(i32, i32)> {
    let (w, h) = line.trim().split_once('x')?;
    let (w, h): (i32, i32) = (w.trim().parse().ok()?, h.trim().trim_end_matches(|c: char| !c.is_ascii_digit()).parse().ok()?);
    (w > 0 && h > 0).then_some((w.max(h), w.min(h)))
}

#[cfg(windows)]
pub use win::*;

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{HANDLE, HWND, POINT, RECT, WAIT_OBJECT_0};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    use super::{Foreground, Rect};

    /// What signals the game's end: its process, or tree.rs's event for its programs.
    pub type Ended = HANDLE;

    /// A window over the game can show (unless the game is in exclusive fullscreen).
    pub const DRAWS_OVER_GAMES: bool = true;

    /// Per-monitor DPI awareness, so the game's window is measured in real pixels.
    pub fn dpi_aware() {
        unsafe { windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    }

    /// Waits up to `ms` for the game to end, handing the host's windows their messages
    /// meanwhile. True once it has ended.
    pub fn wait(process: &Ended, ms: u32) -> bool {
        if unsafe { MsgWaitForMultipleObjects(1, process, 0, ms, QS_ALLINPUT) } == WAIT_OBJECT_0 {
            return true;
        }
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        false
    }

    /// Whether the game has ended, without waiting.
    pub fn has_ended(process: &Ended) -> bool {
        unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(*process, 0) == WAIT_OBJECT_0 }
    }

    /// Whether key `vk` is down.
    pub fn key_down(vk: i32) -> bool {
        unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 }
    }

    pub fn foreground() -> Foreground {
        let mut pid = 0u32;
        let fg = unsafe { GetForegroundWindow() };
        unsafe { GetWindowThreadProcessId(fg, &mut pid) };
        Foreground { pid, none: fg.is_null(), console: !fg.is_null() && is_console(fg) }
    }

    /// The process whose window is in front is `pid`.
    /// Whether `hwnd` is still a window.
    pub fn window_alive(hwnd: u64) -> bool {
        hwnd != 0 && unsafe { IsWindow(hwnd as usize as HWND) } != 0
    }

    pub fn in_front(pid: u32) -> bool {
        foreground().pid == pid
    }

    /// Whether a Direct3D game holds the screen in exclusive fullscreen (Windows' own
    /// notification state, which is what keeps notifications away from such games).
    pub fn exclusive_fullscreen() -> bool {
        let mut state = 0;
        let hr = unsafe { windows_sys::Win32::UI::Shell::SHQueryUserNotificationState(&mut state) };
        hr >= 0 && state == 3 // QUNS_RUNNING_D3D_FULL_SCREEN
    }

    /// Where a frame file goes, for a game reached over TCP.
    pub fn frame_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("peerparty");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// A console's window: the classic console host's, or Windows Terminal's (where Windows 11
    /// opens a program's console by default, a mod loader's log window among them).
    fn is_console(h: HWND) -> bool {
        let mut class = [0u16; 64];
        let n = unsafe { GetClassNameW(h, class.as_mut_ptr(), class.len() as i32) }.max(0) as usize;
        matches!(String::from_utf16_lossy(&class[..n]).as_str(), "ConsoleWindowClass" | "CASCADIA_HOSTING_WINDOW_CLASS")
    }

    /// The game's main window.
    pub struct GameWindow(HWND);

    impl GameWindow {
        pub fn none() -> GameWindow {
            GameWindow(std::ptr::null_mut())
        }

        /// Still a window (and one at all).
        pub fn alive(&self) -> bool {
            !self.0.is_null() && unsafe { IsWindow(self.0) } != 0
        }

        /// The raw window handle (0 if none).
        pub fn hwnd(&self) -> u64 {
            self.0 as usize as u64
        }

        /// The game's main window: its biggest visible unowned top-level window (preferring
        /// regular app windows, falling back to a tool window when split-screen containment
        /// hid it from the taskbar).
        pub fn find(pid: u32) -> GameWindow {
            struct Find {
                pid: u32,
                best: HWND,
                area: i64,
                best_tool: HWND,
                tool_area: i64,
            }
            unsafe extern "system" fn each(h: HWND, l: isize) -> i32 {
                let f = unsafe { &mut *(l as *mut Find) };
                let mut pid = 0u32;
                unsafe { GetWindowThreadProcessId(h, &mut pid) };
                if pid != f.pid || unsafe { IsWindowVisible(h) } == 0 {
                    return 1;
                }
                if !unsafe { GetWindow(h, GW_OWNER) }.is_null() {
                    return 1;
                }
                let mut r: RECT = unsafe { std::mem::zeroed() };
                unsafe { GetClientRect(h, &mut r) };
                let (w, h_px) = (r.right - r.left, r.bottom - r.top);
                let area = i64::from(w) * i64::from(h_px);
                let ex = unsafe { GetWindowLongW(h, GWL_EXSTYLE) } as u32;
                if ex & WS_EX_TOOLWINDOW != 0 {
                    if w > 100 && h_px > 100 && area > f.tool_area {
                        f.tool_area = area;
                        f.best_tool = h;
                    }
                } else if area > f.area {
                    f.area = area;
                    f.best = h;
                }
                1
            }
            let mut f = Find { pid, best: std::ptr::null_mut(), area: 0, best_tool: std::ptr::null_mut(), tool_area: 0 };
            unsafe { EnumWindows(Some(each), &mut f as *mut Find as isize) };
            GameWindow(if !f.best.is_null() { f.best } else { f.best_tool })
        }

        /// Its client area on screen, when it's big enough to draw over.
        pub fn rect(&self) -> Option<Rect> {
            if self.0.is_null() {
                return None;
            }
            let mut r: RECT = unsafe { std::mem::zeroed() };
            let mut tl = POINT { x: 0, y: 0 };
            unsafe {
                GetClientRect(self.0, &mut r);
                windows_sys::Win32::Graphics::Gdi::ClientToScreen(self.0, &mut tl);
            }
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            (w > 100 && h > 100).then_some(Rect { left: tl.x, top: tl.y, right: tl.x + w, bottom: tl.y + h })
        }
    }
}

#[cfg(unix)]
pub use unix::*;

#[cfg(unix)]
mod unix {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use super::{Foreground, Rect, landscape_mode};

    /// Set once the game has ended (by the thread waiting for it, backend/linux/launch.rs).
    pub type Ended = Arc<AtomicBool>;

    /// Under Gamescope nothing shows over the game but what it draws itself.
    pub const DRAWS_OVER_GAMES: bool = false;

    pub fn dpi_aware() {}

    /// Waits up to `ms` for the game to end. True once it has.
    pub fn wait(ended: &Ended, ms: u32) -> bool {
        let until = Instant::now() + Duration::from_millis(u64::from(ms));
        loop {
            if ended.load(Ordering::SeqCst) {
                return true;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            std::thread::sleep(left.min(Duration::from_millis(5)));
        }
    }

    /// Whether the game has ended, without waiting.
    pub fn has_ended(ended: &Ended) -> bool {
        ended.load(Ordering::SeqCst)
    }

    /// The keyboard isn't read off Windows (controllers open the overlay).
    pub fn key_down(_vk: i32) -> bool {
        false
    }

    pub fn foreground() -> Foreground {
        Foreground { pid: 0, none: true, console: false }
    }

    /// The game has the screen.
    /// Off Windows there are no window handles to outlive.
    pub fn window_alive(_hwnd: u64) -> bool {
        true
    }

    pub fn in_front(_pid: u32) -> bool {
        true
    }

    pub fn exclusive_fullscreen() -> bool {
        false
    }

    /// The frame file goes in this user's runtime folder.
    pub fn frame_dir() -> std::path::PathBuf {
        crate::backend::linux::runtime_dir()
    }

    /// The screen the game is on: `PEERPARTY_SCREEN` (`1280x800`), else the first
    /// connected display's preferred mode (`/sys/class/drm`), else the Steam Deck's.
    fn screen() -> (i32, i32) {
        if let Some(m) = std::env::var("PEERPARTY_SCREEN").ok().as_deref().and_then(landscape_mode) {
            return m;
        }
        let mut cards: Vec<_> = std::fs::read_dir("/sys/class/drm").into_iter().flatten().flatten().map(|e| e.path()).collect();
        cards.sort();
        for c in cards {
            let connected = std::fs::read_to_string(c.join("status")).is_ok_and(|s| s.trim() == "connected");
            if let Some(m) = connected.then(|| std::fs::read_to_string(c.join("modes")).ok()).flatten().as_deref().and_then(|t| t.lines().next()).and_then(landscape_mode) {
                return m;
            }
        }
        (1280, 800)
    }

    /// The game's "window": the whole screen.
    pub struct GameWindow(Option<Rect>);

    impl GameWindow {
        pub fn none() -> GameWindow {
            GameWindow(None)
        }

        pub fn alive(&self) -> bool {
            self.0.is_some()
        }

        pub fn hwnd(&self) -> u64 {
            0
        }

        pub fn find(_pid: u32) -> GameWindow {
            let (w, h) = screen();
            GameWindow(Some(Rect { left: 0, top: 0, right: w, bottom: h }))
        }

        pub fn rect(&self) -> Option<Rect> {
            self.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_modes_read_landscape() {
        assert_eq!(landscape_mode("1280x800"), Some((1280, 800)));
        assert_eq!(landscape_mode("800x1280\n"), Some((1280, 800)), "a portrait panel turned on its side");
        assert_eq!(landscape_mode("1920x1080i"), Some((1920, 1080)));
        assert_eq!(landscape_mode("0x0"), None);
        assert_eq!(landscape_mode("wide"), None);
    }
}
