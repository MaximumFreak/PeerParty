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

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use peerparty_protocol::ipc::HookMsg;

use crate::log;

pub(super) use overlay::{Frame, frame};

/// Which API drew a frame last ("" until one has).
static API: Mutex<&'static str> = Mutex::new("");
static LAST_VIEWPORT: Mutex<(u32, u32, usize, &'static str)> = Mutex::new((0, 0, 0, ""));

fn skipped() -> bool {
    crate::net::skip("present") || crate::net::skip("video")
}

/// Whether the frames a present to window `hwnd` shows go through Vulkan underneath the
/// API it draws with (a translation layer such as DXVK or Zink, on Windows or under
/// Wine): a Vulkan swap chain is made on that window (latched from the moment it's made,
/// so from the first frame and through any stall), or, for a window not known, one
/// presented in the last second. The Vulkan hooks see those frames last, so they draw the
/// overlay into them and copy them for the session; the hooks of the API above only
/// report the viewport, or the overlay would be drawn twice and two sets of frames sent.
/// `hwnd` is only asked for when Vulkan presents at all.
pub(crate) fn lower_layer_presents(hwnd: impl FnOnce() -> usize) -> bool {
    if !vulkan::presents_anywhere() {
        return false;
    }
    vulkan::presents_to(hwnd()) || vulkan::presented_recently()
}

/// The graphics API PeerParty draws into ("" until a frame has been drawn there).

pub fn api() -> &'static str {
    *crate::sys::lock(&API)
}

pub(crate) fn drew(api: &'static str) {
    let mut a = crate::sys::lock(&API);
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

/// The graphics APIs whose present hooks go in, each once its module is loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Api {
    Dxgi,
    D3d9,
    Gl,
    Vulkan,
}

const NOT_TRIED: u8 = 0;
const INSTALLING: u8 = 1;
const HOOKED: u8 = 2;
/// Tried with its module loaded, and it couldn't be hooked: not tried again.
const FAILED: u8 = 3;

/// Per API (`Api as usize`), how far its hooks got.
static STATE: [AtomicU8; 4] = [const { AtomicU8::new(NOT_TRIED) }; 4];
/// Something wanted the present hooks (the overlay drawn inside the game, the session's
/// capture or viewport): a graphics module the game loads later gets them too.
static WANTED: AtomicBool = AtomicBool::new(false);

impl Api {
    const ALL: [Api; 4] = [Api::Dxgi, Api::D3d9, Api::Gl, Api::Vulkan];

    fn label(self) -> &'static str {
        match self {
            Api::Dxgi => "DXGI",
            Api::D3d9 => "Direct3D 9",
            Api::Gl => "OpenGL",
            Api::Vulkan => "Vulkan",
        }
    }

    /// The API a module the game loads belongs to (Direct3D 10, 11 and 12 all present
    /// through DXGI).
    fn of_module(name: &str) -> Option<Api> {
        match name.to_ascii_lowercase().as_str() {
            "dxgi.dll" | "d3d10.dll" | "d3d10_1.dll" | "d3d11.dll" | "d3d12.dll" => Some(Api::Dxgi),
            "d3d9.dll" => Some(Api::D3d9),
            "opengl32.dll" => Some(Api::Gl),
            "vulkan-1.dll" | "winevulkan.dll" => Some(Api::Vulkan),
            _ => None,
        }
    }

    /// Whether what its hooks are made from is loaded.
    fn loaded(self) -> bool {
        match self {
            Api::Dxgi => !com::module("dxgi.dll").is_null(),
            Api::D3d9 => !com::module("d3d9.dll").is_null(),
            Api::Gl => !com::module("opengl32.dll").is_null(),
            Api::Vulkan => !vulkan::module().is_null(),
        }
    }

    fn install(self) -> bool {
        match self {
            Api::Dxgi => dxgi::install(),
            Api::D3d9 => d3d9::install(),
            Api::Gl => gl::install(),
            Api::Vulkan => {
                vulkan::loaded();
                vulkan::install()
            }
        }
    }

    fn state(self) -> &'static AtomicU8 {
        &STATE[self as usize]
    }

    /// Installs its hooks unless they're in or going in: Some(hooked) when this call
    /// tried, None when it didn't (done or under way, or its module isn't loaded, and its
    /// load tries again).
    fn try_install(self) -> Option<bool> {
        if self.state().compare_exchange(NOT_TRIED, INSTALLING, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return None;
        }
        if !self.loaded() {
            self.state().store(NOT_TRIED, Ordering::Release);
            return None;
        }
        let ok = std::panic::catch_unwind(|| self.install()).unwrap_or(false);
        self.state().store(if ok { HOOKED } else { FAILED }, Ordering::Release);
        Some(ok)
    }
}

/// The game loaded a module (watch.rs, under the loader lock). A graphics API's present
/// hooks go in when they're wanted and not in yet, from a thread once the load is through:
/// the throwaway device they're found with means calling into the module, which can't be
/// done under the loader lock or before its start-up ran. Vulkan's lookups go in at once,
/// before the game makes its device, when the overlay draws inside games or capture is
/// wanted. A d3d9.dll may be drawn through DXVK.
pub fn module_loaded(name: &str, module: windows_sys::Win32::Foundation::HMODULE) {
    let Some(api) = Api::of_module(name) else { return };
    if skipped() {
        return;
    }
    match api {
        Api::Vulkan if crate::ipc::enabled() && prefer_ingame() || capture::wanted() => vulkan::loaded_in_loader(),
        Api::D3d9 => d3d9::redirect(module),
        _ => {}
    }
    if !WANTED.load(Ordering::Acquire) || api.state().load(Ordering::Acquire) != NOT_TRIED {
        return;
    }
    let module = name.to_string();
    let spawned = std::thread::Builder::new().name("peerparty-video".into()).spawn(move || {
        if let Some(ok) = api.try_install() {
            log!("video: the game loaded {module} after the present hooks went in: {} {}", api.label(), if ok { "hooked now" } else { "couldn't be hooked" });
        }
    });
    if spawned.is_err() {
        log!("video: the game loaded {name}, but no thread could be started to hook {}", api.label());
    }
}

/// Whether `module_loaded` acts on a module of this name.
pub fn is_graphics_module(name: &str) -> bool {
    Api::of_module(name).is_some()
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
pub fn set_capture(enabled: bool, ring: Option<&str>, event: Option<&str>, pace: Option<&str>, pixels: bool) {
    if skipped() {
        if enabled {
            log!("video: PEERPARTY_SKIP=present (or video) is set: this copy's frames never reach the split-screen window, which shows Windows' own picture of it instead (and nothing while it's hidden)");
        }
        return;
    }
    capture::configure_as(enabled, ring, event, pace, pixels);

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

/// Installs the present hooks of every graphics API whose module is loaded and that
/// isn't hooked yet; the others go in when the game loads theirs (`module_loaded`).
/// Logged the first time, and whenever it hooks something.
fn ensure_hooks() {
    let first = !WANTED.swap(true, Ordering::AcqRel);
    let (mut hooked, mut failed, mut later) = (Vec::new(), Vec::new(), Vec::new());
    for api in Api::ALL {
        match api.try_install() {
            Some(true) => hooked.push(api.label()),
            Some(false) => failed.push(api.label()),
            None if api.state().load(Ordering::Acquire) == NOT_TRIED => later.push(api.label()),
            None => {}
        }
    }
    if !first && hooked.is_empty() && failed.is_empty() {
        return;
    }
    let mut line = format!("video: hooked {}", if hooked.is_empty() { "nothing now".to_string() } else { hooked.join(", ") });
    if !failed.is_empty() {
        line += &format!("; {} loaded but couldn't be hooked", failed.join(", "));
    }
    if !later.is_empty() {
        line += &format!("; {} hooked if the game loads it", later.join(", "));
    }
    log!("{line}");
}

#[cfg(test)]
mod tests {
    use super::Api;

    #[test]
    fn graphics_modules_are_known_by_name() {
        assert_eq!(Api::of_module("D3D11.dll"), Some(Api::Dxgi));
        assert_eq!(Api::of_module("d3d12.dll"), Some(Api::Dxgi));
        assert_eq!(Api::of_module("dxgi.dll"), Some(Api::Dxgi));
        assert_eq!(Api::of_module("d3d9.dll"), Some(Api::D3d9));
        assert_eq!(Api::of_module("OPENGL32.DLL"), Some(Api::Gl));
        assert_eq!(Api::of_module("winevulkan.dll"), Some(Api::Vulkan));
        assert_eq!(Api::of_module("d3dcompiler_47.dll"), None);
    }
}
