//! Graphics interception layer: swapchain / present hooking, inbound overlay blitting
//! (`overlay.rs`), a split-screen copy's frames for the session's window (`capture.rs`),
//! and viewport reporting to the session host.
//!
//! Replaces the older `present` module while preserving zero-overhead behavior when neither
//! the in-game overlay nor compositor capture is active.
//!
//! Compatibility: `PEERPARTY_SKIP=present` continues to disable this layer alongside
//! `PEERPARTY_SKIP=video`.

pub(crate) mod capture;
mod com;
mod d3d11;
mod d3d12;
mod d3d9;
mod dxgi;
mod fullscreen;
mod gl;
mod overlay;
mod pixels;
mod vkshaders;
mod vulkan;

use std::sync::{Mutex, Once};

use peerparty_protocol::ipc::HookMsg;

use crate::log;

pub(super) use overlay::{Frame, frame};

/// Which API drew a frame last ("" until one has).
static API: Mutex<&'static str> = Mutex::new("");
static INSTALL: Once = Once::new();
static LAST_VIEWPORT: Mutex<(u32, u32, usize, &'static str)> = Mutex::new((0, 0, 0, ""));

fn skipped() -> bool {
    crate::net::skip("present") || crate::net::skip("video")
}

/// Whether the game's frames go through Vulkan underneath the API it draws with (a
/// translation layer such as DXVK, on Windows or under Wine): a Vulkan swap chain
/// presented in the last second. The Vulkan hooks see those frames last, so they draw the
/// overlay into them and copy them for the session; the hooks of the API above only
/// report the viewport, or the overlay would be drawn twice and two sets of frames sent.
pub(crate) fn lower_layer_presents() -> bool {
    vulkan::presented_recently()
}

/// The graphics API PeerParty draws into ("" until a frame has been drawn there).

pub fn api() -> &'static str {
    *API.lock().unwrap()
}

pub(crate) fn drew(api: &'static str) {
    let mut a = API.lock().unwrap();
    if *a != api {
        log!("video: drawing the overlay inside the game's {api} frames");
        *a = api;
    }
}

/// The game showed a frame: a split-screen copy's log gets its frame rate every 30
/// seconds (two or more copies share one graphics card, and a pane that feels slow may
/// just be drawing few frames).
pub(crate) fn presented() {
    if !crate::window::wants_hooks() {
        return;
    }
    use std::time::{Duration, Instant};
    struct Pace {
        since: Instant,
        last: Instant,
        frames: u32,
        longest: Duration,
    }
    static PACE: Mutex<Option<Pace>> = Mutex::new(None);
    let now = Instant::now();
    let line = {
        let mut pace = crate::sys::lock(&PACE);
        let p = pace.get_or_insert(Pace { since: now, last: now, frames: 0, longest: Duration::ZERO });
        p.frames += 1;
        p.longest = p.longest.max(now - p.last);
        p.last = now;
        let span = now - p.since;
        (span >= Duration::from_secs(30)).then(|| {
            let line = format!(
                "video: {:.0} frames a second over the last {:.0} s, the longest {} ms",
                f64::from(p.frames) / span.as_secs_f64(),
                span.as_secs_f64(),
                p.longest.as_millis()
            );
            *p = Pace { since: now, last: now, frames: 0, longest: Duration::ZERO };
            line
        })
    };
    if let Some(line) = line {
        log!("{line}");
        crate::window::log_mouse();
        crate::input::log_summary();
    }
}

/// Reports the game's active framebuffer size, HWND, and graphics API to the session host
/// (`HookMsg::Viewport`) and notifies `window::note_game_window`. Deduplicated so steady-
/// state presents send nothing.
pub(crate) fn report_viewport(width: u32, height: u32, hwnd: usize, api: &'static str) {
    if hwnd != 0 {
        crate::window::note_game_window(hwnd);
    }
    if width == 0 || height == 0 {
        return;
    }
    let key = (width, height, hwnd, api);
    {
        let mut last = crate::sys::lock(&LAST_VIEWPORT);
        if *last == key {
            return;
        }
        *last = key;
    }
    log!("video: viewport {width}x{height} ({api}, hwnd=0x{hwnd:x})");
    if crate::ipc::connected() {
        crate::ipc::send(HookMsg::Viewport {
            width,
            height,
            hwnd: hwnd as u64,
            api: api.to_string(),
        });
    }
}

/// Whether the overlay draws inside the game's frames from the start: the player set it
/// to (`PEERPARTY_DRAW=ingame`), or left it on `auto` where a window over the game
/// wouldn't show (Gamescope, wine.rs).
pub fn prefer_ingame() -> bool {
    crate::sys::wine::draw_ingame(
        std::env::var(peerparty_protocol::env::DRAW).ok().as_deref(),
        crate::sys::wine::prefer_ingame_overlay(),
    )
}

/// The game loaded vulkan-1.dll: Vulkan's hooks go in now when the overlay draws inside
/// games (`prefer_ingame`) or capture is active.
pub fn vulkan_loaded() {
    if (crate::ipc::enabled() && prefer_ingame() || capture::wanted()) && !skipped() {
        vulkan::loaded();
    }
}

/// The game loaded d3d9.dll (watch.rs): a split-screen copy may draw it through DXVK.
pub fn d3d9_loaded(module: windows_sys::Win32::Foundation::HMODULE) {
    if !skipped() {
        d3d9::redirect(module);
    }
}

/// The launcher made its overlay frame block: open it and hook the game's presents.

pub fn attach(name: &str) {
    if skipped() {
        return;
    }
    if !overlay::attach(name) {
        return;
    }
    ensure_hooks();
}

/// The session's window draws this copy's frames, or stops (`HostMsg::Capture`).
pub fn set_capture(enabled: bool, ring: Option<&str>, event: Option<&str>, pixels: bool) {
    if skipped() {
        return;
    }
    capture::configure_as(enabled, ring, event, pixels);

    if enabled {
        ensure_hooks();
    }
}

/// Ensures graphics present/resize hooks are installed so a split-screen session receives
/// exact backbuffer dimensions (`HookMsg::Viewport`) on startup and resolution changes.
pub fn ensure_viewport_hooks() {
    if !skipped() {
        ensure_hooks();
    }
}

fn ensure_hooks() {
    INSTALL.call_once(|| {
        let mut hooked = Vec::new();
        if dxgi::install() {
            hooked.push("DXGI");
        }
        if d3d9::install() {
            hooked.push("Direct3D 9");
        }
        if gl::install() {
            hooked.push("OpenGL");
        }
        if vulkan::install() {
            hooked.push("Vulkan");
        }
        log!(
            "video: hooked {}",
            if hooked.is_empty() {
                "nothing (no known graphics API loaded)".into()
            } else {
                hooked.join(", ")
            }
        );
    });
}
