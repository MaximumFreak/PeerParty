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
pub(crate) mod fullscreen;
mod gl;
mod overlay;
mod pixels;
mod source;
mod vkshaders;
pub(crate) mod vram;
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

/// The last reported framebuffer size (`(0, 0)` before the first present).
pub(super) fn current_viewport() -> (u32, u32) {
    let (w, h, _, _) = *crate::sys::lock(&LAST_VIEWPORT);
    (w, h)
}

/// The copy's pane was resized to `w`x`h`. A windowed game follows its window; one that
/// asked for exclusive fullscreen (or uses Direct3D 9, whose display modes read as the
/// pane's size) often ignores `WM_SIZE` and sizes from the display mode instead. It's
/// told what a real display mode change tells a game (`WM_DISPLAYCHANGE` at the new
/// size), and Source engine Direct3D 9 games are also asked to apply the new mode live.
pub fn pane_resized(w: u32, h: u32) {
    let (vw, vh, hwnd, api) = *crate::sys::lock(&LAST_VIEWPORT);
    source::on_pane_resized(w, h);
    let exclusive = fullscreen::exclusive_wanted();
    if !exclusive && api != "d3d9" {
        return;
    }
    let hwnd = if hwnd != 0 { hwnd } else { crate::window::game_window() };
    let kind = if exclusive { "exclusive fullscreen" } else { "Direct3D 9" };
    let told = hwnd != 0 && {
        use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_DISPLAYCHANGE};
        // wParam: bits per pixel; lParam: the width in the low word, the height in the high.
        let size = (w.min(0xffff) | (h.min(0xffff) << 16)) as isize;
        unsafe { PostMessageW(hwnd as _, WM_DISPLAYCHANGE, 32, size) != 0 }
    };
    if told {
        log!("video: the pane is {w}x{h} now and the game draws {kind} ({vw}x{vh}, {api}): told it the display changed to {w}x{h} (WM_DISPLAYCHANGE to 0x{hwnd:x}); a game that sets its mode again takes the pane's size, one that doesn't has its frames scaled into the pane until its resolution is chosen again in its settings");
    } else {
        log!("video: the pane is {w}x{h} now, but the game draws {kind} ({vw}x{vh}, {api}) and its window isn't known, so it isn't told: its frames are scaled into the pane until its resolution is chosen again in its settings");
    }
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
/// Serializes per-API hook installation so a game thread entering an early-entry hook
/// waits for an in-flight install to finish instead of racing past unhooked COM vtables.
static INSTALL_LOCKS: [Mutex<()>; 4] = [const { Mutex::new(()) }; 4];
/// Something wanted the present hooks (the overlay drawn inside the game, the session's
/// capture or viewport): a graphics module the game loads later gets them too.
static WANTED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Bitmask of APIs currently inside `Api::try_install` on this thread, so a hook
    /// triggered by our own throwaway device creation can never self-deadlock on
    /// `INSTALL_LOCKS`.
    static IN_TRY_INSTALL: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

struct TryInstallGuard {
    prev: u8,
}

impl TryInstallGuard {
    fn enter(api: Api) -> Option<Self> {
        let bit = 1u8 << (api as u8);
        IN_TRY_INSTALL.with(|mask| {
            let prev = mask.get();
            if prev & bit != 0 {
                return None;
            }
            mask.set(prev | bit);
            Some(Self { prev })
        })
    }
}

impl Drop for TryInstallGuard {
    fn drop(&mut self) {
        IN_TRY_INSTALL.with(|mask| mask.set(self.prev));
    }
}

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

    /// Installs its hooks unless they're in or already tried: Some(hooked) when this call
    /// tried, None when another thread already installed (after waiting for any in-flight
    /// install to finish), this thread is already installing it, or its module isn't loaded.
    fn try_install(self) -> Option<bool> {
        if matches!(self.state().load(Ordering::Acquire), HOOKED | FAILED) {
            return None;
        }
        let Some(_reentry) = TryInstallGuard::enter(self) else {
            return None;
        };
        let _guard = crate::sys::lock(&INSTALL_LOCKS[self as usize]);
        if self.state().load(Ordering::Acquire) != NOT_TRIED {
            return None;
        }
        if !self.loaded() {
            return None;
        }
        self.state().store(INSTALLING, Ordering::Release);
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
/// wanted. Direct3D 9 and DXGI arm lightweight entry-point detours here in split screen so
/// their full COM hooks go in synchronously before the game's first factory/device call.
pub fn module_loaded(name: &str, module: windows_sys::Win32::Foundation::HMODULE) {
    let Some(api) = Api::of_module(name) else { return };
    if skipped() {
        return;
    }
    match api {
        Api::Vulkan if crate::ipc::enabled() && prefer_ingame() || capture::wanted() || vram::share().is_some() => vulkan::loaded_in_loader(),
        Api::D3d9 => d3d9::redirect(module),
        Api::Dxgi => dxgi::redirect(name, module),
        _ => {}
    }
    if !WANTED.load(Ordering::Acquire) && !(api == Api::Dxgi && vram::share().is_some()) {
        return;
    }
    if api.state().load(Ordering::Acquire) != NOT_TRIED {
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

/// Installs DXGI hooks synchronously (or waits for an in-flight install to complete),
/// called from `dxgi::redirect` before the game's first DXGI factory or D3D11 device call
/// so output mode enumeration and swap-chain creation never race the background thread.
pub(super) fn ensure_dxgi_hooks() {
    if skipped() {
        return;
    }
    WANTED.store(true, Ordering::Release);
    if let Some(ok) = Api::Dxgi.try_install() {
        log!("video: DXGI {} before the game's first factory/device call", if ok { "hooked" } else { "couldn't be hooked" });
    }
}

/// Installs Direct3D 9 hooks synchronously (or waits for an in-flight install to complete),
/// called from `d3d9::redirect` before the game's first `Direct3DCreate9(Ex)` call so its
/// display-mode, device-creation and present hooks never race the background thread.
pub(super) fn ensure_d3d9_hooks() {
    if skipped() {
        return;
    }
    WANTED.store(true, Ordering::Release);
    if let Some(ok) = Api::D3d9.try_install() {
        log!("video: Direct3D 9 {} before the game's first Direct3DCreate9 call", if ok { "hooked" } else { "couldn't be hooked" });
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

/// Drops any DXGI capture/drawer state bound to `hwnd` when that window is destroyed.
pub(crate) fn window_destroyed(hwnd: usize) {
    if !skipped() {
        let _ = dxgi::window_destroyed(hwnd);
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
    use super::{Api, TryInstallGuard};

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

    #[test]
    fn try_install_reentrancy_guard_avoids_self_deadlock() {
        let _d3d9 = TryInstallGuard::enter(Api::D3d9).expect("first entry succeeds");
        assert!(TryInstallGuard::enter(Api::D3d9).is_none(), "same-thread re-entry into same API is rejected");
        assert!(Api::D3d9.try_install().is_none(), "re-entrant try_install returns None instead of deadlocking");
        let _dxgi = TryInstallGuard::enter(Api::Dxgi).expect("independent API can still be entered");
        assert!(TryInstallGuard::enter(Api::Dxgi).is_none());
        drop(_dxgi);
        assert!(TryInstallGuard::enter(Api::Dxgi).is_some(), "bit cleared after drop");
    }
}
