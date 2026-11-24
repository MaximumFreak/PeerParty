//! Split screen as one window ("multi-window within a window"): a session's copies of
//! the game shown side by side, in panes of one top-level window the session host owns,
//! cut by a `LayoutPreset` and switched live (`set_layout`, `cycle_layout`, `focus_pane`,
//! `set_zoomed`).
//!
//! Each copy is told its pane (`sync_instances`: its `InstanceSpec`'s pane, and its
//! virtual screen the pane's size, so the game renders at the pane's own aspect), and is
//! shown in it:
//!
//! - On Windows, drawn by the window itself (gpu.rs) from the shared textures the copy's
//!   hook copies each frame into (`HookMsg::Surface`, `set_surface`): Direct3D 11 and 12,
//!   Vulkan and OpenGL games on the same graphics card as the window.
//! - Otherwise (Direct3D 9, another graphics card, before the first frame), through a DWM
//!   thumbnail of the copy's window (`DwmRegisterThumbnail`, from dwmapi.dll in System32
//!   only), which needs nothing from the game's graphics. Each pane switches on its own:
//!   its thumbnail hides once the window draws its frames.
//! - Elsewhere, the layout alone, with the same API: under Gamescope the copies' frames
//!   come to this window through their hooks (not yet), since nothing shows over a game
//!   there.
//!
//! Panes are in physical pixels of the window's client area, which follows its monitor:
//! a 4K screen at 200% has 3840 x 2160 of them, and `MonitorBounds::scale` says how many
//! per point. Zooming shows the focused copy on the whole window and tells it so: its
//! pane (and so its resolution) is the whole window until the zoom ends, as a layout
//! change would; the others keep theirs, hidden.

use std::path::Path;

use peerparty_protocol::session::{LayoutPreset, PaneFit, PaneRect, PaneSlot};

use super::instance::InstanceConn;

/// Where the window's client area is on screen, in physical pixels, and its monitor's
/// DPI (96 is 100%).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonitorBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
}

impl MonitorBounds {
    /// Physical pixels per point (1.0 at 96 DPI, 2.0 at 192).
    #[allow(dead_code)]
    pub fn scale(&self) -> f32 {
        self.dpi.max(1) as f32 / 96.0
    }

    /// The size in points.
    #[allow(dead_code)]
    pub fn logical_size(&self) -> (u32, u32) {
        let s = self.scale();
        ((self.width as f32 / s).round() as u32, (self.height as f32 / s).round() as u32)
    }

    /// The primary monitor (where a session's window opens).
    #[cfg(windows)]
    pub fn primary() -> MonitorBounds {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        let dpi = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForSystem() };
        MonitorBounds { x: 0, y: 0, width: w.max(1) as u32, height: h.max(1) as u32, dpi: if dpi == 0 { 96 } else { dpi } }
    }

    /// The screen: `PEERPARTY_SCREEN` (`1280x800`), else the Steam Deck's.
    #[cfg(unix)]
    pub fn primary() -> MonitorBounds {
        let (w, h) = std::env::var("PEERPARTY_SCREEN").ok().as_deref().and_then(super::desktop::landscape_mode).unwrap_or((1280, 800));
        MonitorBounds { x: 0, y: 0, width: w as u32, height: h as u32, dpi: 96 }
    }

    /// Every monitor, numbered as `--monitor` and the game page number them: the main one
    /// first, then the others left to right (top to bottom where they're level). Windows'
    /// own numbers aren't used: they don't follow where the monitors are, and change when
    /// one is plugged into another port.
    #[cfg(windows)]
    pub fn all() -> Vec<MonitorBounds> {
        use windows_sys::Win32::Foundation::{LPARAM, RECT};
        use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
        use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
        unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, out: LPARAM) -> i32 {
            let out = unsafe { &mut *(out as *mut Vec<(bool, MonitorBounds)>) };
            let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if unsafe { GetMonitorInfoW(m, &mut info) } == 0 {
                return 1;
            }
            let (mut dx, mut dy) = (0u32, 0u32);
            let dpi = if unsafe { GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) } == 0 && dx != 0 { dx } else { 96 };
            let r = info.rcMonitor;
            let b = MonitorBounds { x: r.left, y: r.top, width: (r.right - r.left).max(1) as u32, height: (r.bottom - r.top).max(1) as u32, dpi };
            // MONITORINFOF_PRIMARY
            out.push((info.dwFlags & 1 != 0, b));
            1
        }
        let mut found: Vec<(bool, MonitorBounds)> = Vec::new();
        unsafe { EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(each), &mut found as *mut _ as LPARAM) };
        let v = in_order(found);
        if v.is_empty() { vec![MonitorBounds::primary()] } else { v }
    }

    #[cfg(unix)]
    pub fn all() -> Vec<MonitorBounds> {
        vec![MonitorBounds::primary()]
    }

    /// Monitor `n` (from 1, as `all` numbers them), else the main one, with why when `n`
    /// wasn't there.
    pub fn numbered(n: Option<u32>) -> (MonitorBounds, Option<String>) {
        let all = MonitorBounds::all();
        match n {
            None | Some(1) => (all[0], None),
            Some(n) => match all.get(n as usize - 1) {
                Some(b) => (*b, None),
                None => (all[0], Some(format!("monitor {n} was asked for, but this PC has {}: the session opens on the main one", all.len()))),
            },
        }
    }
}

/// Monitors (each with whether it's the main one) in `MonitorBounds::all`'s order.
#[allow(dead_code)]
fn in_order(mut found: Vec<(bool, MonitorBounds)>) -> Vec<MonitorBounds> {
    found.sort_by_key(|(main, b)| (!*main, b.x, b.y));
    found.into_iter().map(|(_, b)| b).collect()
}

/// The presets `cycle_layout` goes through, in order (for 3 copies, the 2+1 and 1+2
/// layouts come first so no quarter sits empty).
const CYCLE: [LayoutPreset; 3] = [LayoutPreset::Horizontal, LayoutPreset::Vertical, LayoutPreset::Grid2x2];
const CYCLE_3: [LayoutPreset; 5] = [LayoutPreset::TopTwo, LayoutPreset::BottomTwo, LayoutPreset::Horizontal, LayoutPreset::Vertical, LayoutPreset::Grid2x2];

/// An empty pane: a copy not shown.
const HIDDEN: PaneRect = PaneRect { x: 0, y: 0, width: 0, height: 0 };

/// Fits a `src_w x src_h` source image inside `pane` while preserving aspect ratio
/// (centered letterbox or pillarbox). Fills `pane` edge-to-edge when aspect ratios match
/// within 1% or source size is unknown.
pub fn fit_aspect(pane: PaneRect, src_w: u32, src_h: u32) -> PaneRect {
    if pane.width == 0 || pane.height == 0 || src_w == 0 || src_h == 0 {
        return pane;
    }
    let lhs = u64::from(src_w) * u64::from(pane.height);
    let rhs = u64::from(pane.width) * u64::from(src_h);
    if lhs.abs_diff(rhs) * 100 <= lhs.max(rhs) {
        return pane;
    }
    let (w, h) = if lhs > rhs {
        let h = ((u64::from(pane.width) * u64::from(src_h) + u64::from(src_w) / 2) / u64::from(src_w))
            .clamp(1, u64::from(pane.height)) as u32;
        (pane.width, h)
    } else {
        let w = ((u64::from(pane.height) * u64::from(src_w) + u64::from(src_h) / 2) / u64::from(src_h))
            .clamp(1, u64::from(pane.width)) as u32;
        (w, pane.height)
    };
    PaneRect {
        x: pane.x.saturating_add(((pane.width - w) / 2) as i32),
        y: pane.y.saturating_add(((pane.height - h) / 2) as i32),
        width: w,
        height: h,
    }
}

/// Picks the effective `(width, height)` aspect source for a pane: the hook's reported
/// backbuffer viewport when known, else the window's client rect, else DWM's reported
/// thumbnail source size.
pub fn effective_source_size(client: (u32, u32), dwm_src: (u32, u32), viewport: Option<(u32, u32)>) -> (u32, u32) {
    if let Some((w, h)) = viewport.filter(|&(w, h)| w > 0 && h > 0) {
        return (w, h);
    }
    if client.0 > 0 && client.1 > 0 {
        return client;
    }
    dwm_src
}

/// Computes `(dest_rect, uv_rect)` for drawing a `src_w x src_h` frame inside `pane`
/// according to `fit`:
/// - `Fit`: letterboxed/pillarboxed inside `pane` with full UVs `[0, 0, 1, 1]`.
/// - `Fill`: fills `pane` edge-to-edge with centered crop UVs (zero black bars, zero stretch).
/// - `Stretch`: fills `pane` edge-to-edge with full UVs.
pub fn compute_fit(pane: PaneRect, src_w: u32, src_h: u32, fit: PaneFit) -> (PaneRect, [f32; 4]) {
    const FULL: [f32; 4] = [0.0, 0.0, 1.0, 1.0];
    if pane.width == 0 || pane.height == 0 || src_w == 0 || src_h == 0 {
        return (pane, FULL);
    }
    match fit {
        PaneFit::Fit => (fit_aspect(pane, src_w, src_h), FULL),
        PaneFit::Stretch => (pane, FULL),
        PaneFit::Fill => {
            let lhs = u64::from(src_w) * u64::from(pane.height);
            let rhs = u64::from(pane.width) * u64::from(src_h);
            if lhs.abs_diff(rhs) * 100 <= lhs.max(rhs) {
                return (pane, FULL);
            }
            if lhs > rhs {
                // Source is wider than pane: crop left and right equally.
                let keep = (rhs as f64 / lhs as f64).clamp(0.05, 1.0) as f32;
                let pad = (1.0 - keep) * 0.5;
                (pane, [pad, 0.0, 1.0 - pad, 1.0])
            } else {
                // Source is taller than pane: crop top and bottom equally.
                let keep = (lhs as f64 / rhs as f64).clamp(0.05, 1.0) as f32;
                let pad = (1.0 - keep) * 0.5;
                (pane, [0.0, pad, 1.0, 1.0 - pad])
            }
        }
    }
}

/// Maps a point `(cx, cy)` in compositor client coordinates inside fitted rect `dest`
/// (sampling source UV rectangle `uv = [u0, v0, u1, v1]`) to target window client
/// coordinates `(gx, gy)` in `0..src_w` × `0..src_h`.
pub fn map_point_to_source_uv(cx: i32, cy: i32, dest: PaneRect, uv: [f32; 4], src_w: u32, src_h: u32) -> Option<(i32, i32)> {
    if dest.width == 0 || dest.height == 0 || src_w == 0 || src_h == 0 {
        return None;
    }
    let rx = ((f64::from(cx) - f64::from(dest.x)) / f64::from(dest.width)).clamp(0.0, 1.0);
    let ry = ((f64::from(cy) - f64::from(dest.y)) / f64::from(dest.height)).clamp(0.0, 1.0);
    let u = f64::from(uv[0]) + rx * f64::from(uv[2] - uv[0]);
    let v = f64::from(uv[1]) + ry * f64::from(uv[3] - uv[1]);
    let gx = (u * f64::from(src_w)).floor().clamp(0.0, f64::from(src_w - 1)) as i32;
    let gy = (v * f64::from(src_h)).floor().clamp(0.0, f64::from(src_h - 1)) as i32;
    Some((gx, gy))
}

/// Maps a point `(cx, cy)` in compositor client coordinates inside fitted thumbnail rect
/// `dest` to target window client coordinates `(gx, gy)` in `0..src_w` × `0..src_h`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn map_point_to_source(cx: i32, cy: i32, dest: PaneRect, src_w: u32, src_h: u32) -> Option<(i32, i32)> {
    map_point_to_source_uv(cx, cy, dest, [0.0, 0.0, 1.0, 1.0], src_w, src_h)
}

pub struct CompositorWindow {
    layout: LayoutPreset,
    bounds: MonitorBounds,
    /// How many copies there are (one pane each).
    count: usize,
    /// The pane with the focus: the one the overlay serves and the keyboard goes to.
    focus: usize,
    /// The focused pane shown on the whole window.
    zoomed: bool,
    /// Each copy's pane, in the client area.
    panes: Vec<PaneRect>,
    /// Each copy's normalized slot (fit, virtual aspect, Z-order).
    slots: Vec<PaneSlot>,
    /// Virtual render resolution percentage (`50..=100`; `100` = native pane size).
    render_scale: u8,
    /// Apply CAS sharpening in the GPU compositor when scaling a pane up.
    upscale: bool,
    #[cfg(windows)]
    window: Option<win::HostWindow>,
    #[cfg(unix)]
    window: Option<super::x11::LinuxWindow>,
}

impl CompositorWindow {
    /// The layout alone, for `count` copies on `bounds` (no window: tests, and where
    /// there's none to make).
    pub fn new(layout: LayoutPreset, bounds: MonitorBounds, count: usize) -> CompositorWindow {
        let mut c = CompositorWindow {
            layout,
            bounds,
            count,
            focus: 0,
            zoomed: false,
            panes: Vec::new(),
            slots: Vec::new(),
            render_scale: 100,
            upscale: true,
            #[cfg(windows)]
            window: None,
            #[cfg(unix)]
            window: None,
        };
        c.recompute();
        c
    }

    /// Sets the virtual render scale percentage (`50..=100`) and whether CAS sharpening
    /// runs on upscaled panes.
    pub fn set_quality(&mut self, render_scale: u8, upscale: bool) {
        self.render_scale = if render_scale == 0 || render_scale >= 100 { 100 } else { render_scale.clamp(50, 100) };
        self.upscale = upscale;
        #[cfg(windows)]
        if let Some(w) = self.window.as_mut() {
            w.set_upscale(upscale);
        }
    }

    /// Sets the compositor's VSync interval (`1` = full refresh, `2` = 1/2, `3` = 1/3).
    pub fn set_sync_interval(&mut self, interval: u8) {
        let interval = interval.clamp(1, 4);
        #[cfg(windows)]
        if let Some(w) = self.window.as_mut() {
            w.set_sync_interval(interval);
        }
        #[cfg(not(windows))]
        let _ = interval;
    }

    /// The layout and, on Windows, its window, shown on `bounds` with `title` and `exe`'s icon.
    pub fn open(layout: LayoutPreset, bounds: MonitorBounds, count: usize, title: &str, exe: &Path) -> CompositorWindow {
        #[allow(unused_mut)]
        let mut c = CompositorWindow::new(layout, bounds, count);
        #[cfg(windows)]
        {
            c.window = win::HostWindow::create(bounds, title, exe);
            if let Some(b) = c.window.as_ref().and_then(|w| w.client_bounds()) {
                c.bounds = b;
                c.recompute();
            }
        }
        #[cfg(unix)]
        {
            c.window = super::x11::LinuxWindow::open(bounds, title);
            if let Some(w) = &c.window {
                c.bounds = w.client_bounds();
                c.recompute();
            }
            let _ = exe;
        }
        c
    }

    pub fn layout(&self) -> &LayoutPreset {
        &self.layout
    }

    pub fn bounds(&self) -> MonitorBounds {
        self.bounds
    }

    /// Each copy's pane in the client area (what the games are told the size of).
    #[allow(dead_code)]
    pub fn panes(&self) -> &[PaneRect] {
        &self.panes
    }

    /// Each copy's normalized slot (fit, virtual aspect, Z-order).
    pub fn slots(&self) -> &[PaneSlot] {
        &self.slots
    }

    /// The virtual render size `(width, height)` copy `i` is told for its pane (with
    /// `render_scale` applied when below 100%).
    pub fn virtual_size(&self, i: usize) -> Option<(u32, u32)> {
        let pane = self.told_panes().get(i).copied()?;
        let (w, h) = if self.zoomed && i == self.focus {
            (pane.width, pane.height)
        } else {
            let slot = self.slots.get(i).copied().unwrap_or_else(|| PaneSlot::new(peerparty_protocol::session::NormRect::FULL));
            slot.aspect.size_for(pane, slot.fit, self.bounds.width, self.bounds.height)
        };
        Some(peerparty_protocol::session::scale_dimensions(w, h, self.render_scale))
    }

    /// Pane `i` on screen, as its copy is told it (`told_panes`).
    pub fn screen_pane(&self, i: usize) -> Option<PaneRect> {
        self.told_panes().get(i).map(|p| PaneRect { x: p.x.saturating_add(self.bounds.x), y: p.y.saturating_add(self.bounds.y), ..*p })
    }

    /// The pane each copy is told (its window and resolution follow it): its layout pane,
    /// or zoomed, the whole window for the focused one. Zoom used to leave the zoomed
    /// game at its pane's resolution, drawn with bars on the whole window.
    pub fn told_panes(&self) -> Vec<PaneRect> {
        let mut panes = self.panes.clone();
        if self.zoomed
            && let Some(p) = panes.get_mut(self.focus)
        {
            *p = PaneRect { x: 0, y: 0, width: self.bounds.width, height: self.bounds.height };
        }
        panes
    }

    /// Where each copy is shown in the client area: its pane, or zoomed, the focused one
    /// on the whole window and the others hidden.
    pub fn display_panes(&self) -> Vec<PaneRect> {
        if !self.zoomed {
            return self.panes.clone();
        }
        let whole = PaneRect { x: 0, y: 0, width: self.bounds.width, height: self.bounds.height };
        (0..self.panes.len()).map(|i| if i == self.focus { whole } else { HIDDEN }).collect()
    }

    pub fn focused(&self) -> usize {
        self.focus
    }

    pub fn zoomed(&self) -> bool {
        self.zoomed
    }

    /// The pane under a point of the client area (highest Z-order wins for overlapping
    /// Picture-in-Picture panes).
    pub fn pane_at(&self, x: i32, y: i32) -> Option<usize> {
        let inside = |p: &PaneRect| x >= p.x && y >= p.y && i64::from(x) < i64::from(p.x) + i64::from(p.width) && i64::from(y) < i64::from(p.y) + i64::from(p.height);
        let panes = self.display_panes();
        let mut order: Vec<usize> = (0..panes.len()).collect();
        order.sort_by_key(|&i| (self.slots.get(i).map_or(0, |s| s.z), i));
        order.into_iter().rev().find(|&i| inside(&panes[i]))
    }

    fn recompute(&mut self) {
        self.panes = self.layout.compute_panes(self.bounds.width, self.bounds.height, self.count);
        self.slots = self.layout.to_slots(self.count);
        self.focus = self.focus.min(self.panes.len().saturating_sub(1));
    }

    /// Live preview during interactive drag in Layout Studio: updates the GPU compositor's
    /// displayed rectangles, crop UVs, and Z-order immediately at 60+ fps without telling
    /// the game processes to rebuild swap chains mid-drag. Call `sync_instances` to commit.
    pub fn preview_slots(&mut self, slots: Vec<PaneSlot>, instances: &[InstanceConn]) {
        self.zoomed = false;
        self.layout = LayoutPreset::Slots(slots);
        self.recompute();
        #[cfg(windows)]
        if self.window.is_some() {
            let shown = self.display_panes();
            let slot_meta: Vec<(PaneFit, u8)> = self.slots.iter().map(|s| (s.fit, s.z)).collect();
            let w = self.window.as_mut().expect("checked");
            for (i, inst) in instances.iter().enumerate() {
                let display_pane = shown.get(i).copied().unwrap_or(HIDDEN);
                let (fit, z) = slot_meta.get(i).copied().unwrap_or((PaneFit::Fit, 0));
                let vp = inst.viewport.as_ref().map(|v| (v.width, v.height));
                w.preview_pane(i, display_pane, fit, z, vp);
            }
            w.push_panes();
        }
        #[cfg(not(windows))]
        let _ = instances;
    }

    /// Another layout, now.
    pub fn set_layout(&mut self, layout: LayoutPreset) {
        self.layout = layout;
        self.recompute();
    }

    /// The next preset: side by side, one above the other, quarters, and round again (for
    /// three copies, 2+1 and 1+2 too; a custom or single layout goes on to the first).
    pub fn cycle_layout(&mut self) -> &LayoutPreset {
        let list: &[LayoutPreset] = if self.count == 3 { &CYCLE_3 } else { &CYCLE };
        let next = list.iter().position(|l| *l == self.layout).map_or(0, |i| (i + 1) % list.len());
        self.set_layout(list[next].clone());
        &self.layout
    }

    /// The focus goes to pane `i`; false for a pane there isn't.
    pub fn focus_pane(&mut self, i: usize) -> bool {
        if i >= self.panes.len() {
            return false;
        }
        self.focus = i;
        true
    }

    /// The focused copy on the whole window, or back to the layout.
    pub fn set_zoomed(&mut self, on: bool) {
        self.zoomed = on;
    }

    /// The window moved, was resized or went to another monitor.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn set_bounds(&mut self, bounds: MonitorBounds) {
        if bounds != self.bounds {
            self.bounds = bounds;
            self.recompute();
        }
    }

    /// A copy more or fewer.
    pub fn set_count(&mut self, count: usize) {
        self.count = count;
        self.recompute();
    }

    /// Copy `i`'s new set of frames (`HookMsg::Surface`), and its ring block: the window
    /// draws its pane from them. On Windows its shared textures (duplicated out of the
    /// copy's process) or its pixel file; elsewhere its pixel file. Whether they could be
    /// opened (false with no window: nothing to do).
    pub fn set_frames(&mut self, i: usize, m: &peerparty_protocol::SurfaceMsg, ring: Option<std::sync::Arc<super::ring::RingBlock>>) -> bool {
        #[cfg(windows)]
        if let Some(w) = self.window.as_mut() {
            let Some(s) = super::gpu::SharedSurface::duplicate(m) else { return false };
            w.set_surface(i, Some(std::sync::Arc::new(s)), ring);
            return true;
        }
        #[cfg(unix)]
        if let Some(w) = self.window.as_mut() {
            let Some(p) = super::ring::PixelFrames::open(m) else { return false };
            w.set_pixels(i, Some(std::sync::Arc::new(p)), ring);
            return true;
        }
        let _ = (i, m, ring);
        true
    }

    /// The name of the event the copies' hooks set after each frame (None: no window
    /// drawing them, or one that looks at their rings once a refresh instead).
    pub fn frame_event(&self) -> Option<String> {
        #[cfg(windows)]
        return self.window.as_ref().and_then(|w| w.gpu.as_ref()).map(|g| g.event_name.clone());
        #[cfg(not(windows))]
        None
    }

    /// What changed in each copy's pane since last asked (for the copies' logs).
    pub fn take_notes(&mut self) -> Vec<(usize, String)> {
        #[cfg(windows)]
        if let Some(w) = self.window.as_mut() {
            return std::mem::take(&mut w.notes);
        }
        Vec::new()
    }

    /// Whether the window draws the copies' frames itself (so they should be shared).
    pub fn draws_frames(&self) -> bool {
        #[cfg(windows)]
        return self.window.as_ref().is_some_and(|w| w.gpu.is_some());
        #[cfg(unix)]
        return self.window.is_some();
    }

    /// The window's handle (0: none).
    pub fn hwnd(&self) -> u64 {
        #[cfg(windows)]
        if let Some(w) = &self.window {
            return w.hwnd as u64;
        }
        0
    }

    /// Where the player last clicked in the window's client area, once.
    pub fn take_click(&mut self) -> Option<(i32, i32)> {
        #[cfg(windows)]
        if let Some(w) = self.window.as_mut() {
            return w.take_click();
        }
        None
    }

    /// Picks up the window's own changes (the player resized or moved it). Whether the
    /// panes changed.
    pub fn tick(&mut self) -> bool {
        #[cfg(windows)]
        if let Some(b) = self.window.as_mut().and_then(|w| w.take_resized()).and_then(|_| self.window.as_ref()?.client_bounds()) {
            // Moved to a monitor of the same size, or its DPI changed: the panes are the
            // same in the window, not on screen, and the copies are told.
            let before = (self.panes.clone(), self.bounds);
            self.set_bounds(b);
            return (self.panes.clone(), self.bounds) != before;
        }
        #[cfg(unix)]
        if let Some(b) = self.window.as_ref().filter(|w| w.take_resized()).map(|w| w.client_bounds()) {
            let before = self.panes.clone();
            self.set_bounds(b);
            return self.panes != before;
        }
        false
    }

    /// Keeps adopted game windows tucked behind the compositor, hidden when the
    /// compositor is minimized, restored after Win+D/minimize, and refreshes their
    /// aspect-fitted DWM thumbnails when a game resizes its own window or backbuffer.
    pub fn maintain(&mut self, instances: &[InstanceConn]) {
        #[cfg(windows)]
        if let Some(w) = self.window.as_mut() {
            w.maintain(self.bounds, instances);
        }
        #[cfg(not(windows))]
        let _ = instances;
    }

    /// Forwards queued keyboard and mouse messages from the compositor window to the
    /// focused pane (or switches focus to the clicked pane first). Returns the newly
    /// focused pane if a mouse click changed focus. While `overlay_open` or `kbm_off`,
    /// only releases for already-held keys and buttons are delivered.
    pub fn forward_input(&mut self, focus: usize, overlay_open: bool, kbm_off: bool, instances: &[InstanceConn]) -> Option<usize> {
        #[cfg(windows)]
        if self.window.is_some() {
            let panes = self.display_panes();
            let zs: Vec<u8> = self.slots.iter().map(|s| s.z).collect();
            let w = self.window.as_mut().expect("checked");
            return w.forward_input(focus, overlay_open || kbm_off, &panes, &zs, instances);
        }
        #[cfg(not(windows))]
        let _ = (focus, overlay_open, kbm_off, instances);
        None
    }

    /// Where the mouse is in copy `i`'s window, on screen as the game counts it (for
    /// `GetCursorPos` through its input block): where the session's mouse last was over
    /// its pane, else the middle of its window.
    pub fn cursor_screen(&self, i: usize) -> Option<(i32, i32)> {
        #[cfg(windows)]
        if let Some(w) = &self.window {
            return w.cursor_for(i);
        }
        let _ = i;
        None
    }

    /// Keeps the real mouse inside pane `confine` (for a game that locked its cursor),
    /// and hides the pointer over the window while confined, while the focused game hid
    /// its cursor (`game_hide`), or while the mouse has sat idle (`idle_hide`). Returns
    /// whether confinement changed.
    pub fn update_pointer(&mut self, confine: Option<usize>, game_hide: bool, idle_hide: bool) -> bool {
        #[cfg(windows)]
        if let Some(w) = self.window.as_mut() {
            return w.update_pointer(confine, game_hide, idle_hide);
        }
        let _ = (confine, game_hide, idle_hide);
        false
    }

    /// Notifies copy `i`'s window that it is active/focused so engines like Unity accept
    /// forwarded keyboard & mouse input without stealing Win32 foreground from us.
    pub fn activate_pane(&self, i: usize, instances: &[InstanceConn]) {
        #[cfg(windows)]
        if let Some(inst) = instances.get(i).filter(|inst| !inst.gone) {
            let h = inst.best_hwnd();
            if h != 0 {
                win::notify_active(h);
            }
        }
        #[cfg(not(windows))]
        let _ = (i, instances);
    }

    /// Whether the user asked to close the compositor window (Alt+F4 or taskbar Close).
    pub fn take_close_requested(&mut self) -> bool {
        #[cfg(windows)]
        if let Some(w) = self.window.as_mut() {
            return w.take_close_requested();
        }
        #[cfg(unix)]
        if let Some(w) = self.window.as_ref() {
            return w.take_close_requested();
        }
        false
    }

    /// Asks each running copy to close as its close button would (every window of it,
    /// its hidden one too), `why` said in its log.
    pub fn close_instances(&self, instances: &[InstanceConn], why: &str) {
        for (i, inst) in instances.iter().enumerate().filter(|(_, inst)| !inst.gone) {
            let known = inst.best_hwnd();
            let asked = match inst.stop.as_ref() {
                Some(s) => s.ask(known),
                None => {
                    #[cfg(windows)]
                    win::post_close(known);
                    usize::from(known != 0)
                }
            };
            crate::launch::note(inst.pid, &format!("session: closing ({why}): player {}'s game was asked to close ({asked} window(s))", i + 1));
        }
    }

    /// Ends every copy still running, all of its programs, `why` said in its log.
    pub fn terminate_instances(&self, instances: &[InstanceConn], why: &str) {
        for (i, inst) in instances.iter().enumerate().filter(|(_, inst)| !inst.gone) {
            crate::launch::note(inst.pid, &format!("session: player {}'s game is ended ({why})", i + 1));
            eprintln!("peerparty: session: player {}'s game is ended ({why})", i + 1);
            match inst.stop.as_ref() {
                Some(s) => s.force(),
                None => {
                    #[cfg(windows)]
                    win::terminate(inst.pid);
                }
            }
        }
    }

    /// Each copy's spec as its pane says: where it is on screen, the virtual screen the
    /// size of its pane at the monitor's DPI, the focus spoofed while there's more than
    /// one; a copy whose spec changed is told. On Windows each copy whose window is
    /// known is shown in its pane. The number of copies told.
    pub fn sync_instances(&mut self, instances: &mut [InstanceConn]) -> usize {
        if instances.len() > self.count {
            self.set_count(instances.len());
        }
        let many = self.count > 1 || instances.len() > 1;
        let mut told = 0;
        let mut layout_sizes: Vec<PaneRect> = Vec::with_capacity(instances.len());
        for (i, inst) in instances.iter_mut().enumerate() {
            let Some(pane) = self.screen_pane(i) else {
                layout_sizes.push(HIDDEN);
                continue;
            };
            let (vw, vh) = self.virtual_size(i).unwrap_or((pane.width, pane.height));
            let told_pane = PaneRect { x: pane.x, y: pane.y, width: vw, height: vh };
            layout_sizes.push(PaneRect { x: pane.x.saturating_sub(self.bounds.x), y: pane.y.saturating_sub(self.bounds.y), width: vw, height: vh });
            let mut spec = inst.spec.clone();
            spec.pane = Some(told_pane);
            spec.virtual_width = Some(vw);
            spec.virtual_height = Some(vh);
            spec.dpi = Some(self.bounds.dpi);
            spec.focus_spoof = many;
            if spec != inst.spec {
                inst.spec = spec;
                if !inst.gone {
                    inst.send_instance();
                    told += 1;
                }
            }
        }
        #[cfg(windows)]
        if self.window.is_some() {
            let shown = self.display_panes();
            let slot_meta: Vec<(PaneFit, u8)> = self.slots.iter().map(|s| (s.fit, s.z)).collect();
            let bounds = self.bounds;
            let w = self.window.as_mut().expect("checked");
            for (i, inst) in instances.iter().enumerate() {
                let source = inst.best_hwnd();
                let layout_pane = layout_sizes.get(i).copied().unwrap_or(HIDDEN);
                let display_pane = shown.get(i).copied().unwrap_or(HIDDEN);
                let (fit, z) = slot_meta.get(i).copied().unwrap_or((PaneFit::Fit, 0));
                let vp = inst.viewport.as_ref().map(|v| (v.width, v.height));
                w.show(i, if inst.gone { 0 } else { source }, layout_pane, display_pane, fit, z, bounds, vp);
                if inst.gone {
                    w.set_surface(i, None, None);
                }
            }
            w.push_panes();
        }
        #[cfg(unix)]
        if self.window.is_some() {
            let shown = self.display_panes();
            let w = self.window.as_mut().expect("checked");
            for (i, inst) in instances.iter().enumerate() {
                if inst.gone {
                    w.set_pixels(i, None, None);
                }
            }
            w.show(shown);
        }
        told
    }

}

#[cfg(windows)]
mod win {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{BLACK_BRUSH, ClientToScreen, GetStockObject, HBRUSH, ScreenToClient};
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_F4};
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    use super::super::gpu::{FULL_UV, Gpu, PaneDraw, RingBlock, SharedSurface};
    use super::{InstanceConn, MonitorBounds, PaneFit, PaneRect, compute_fit, effective_source_size, map_point_to_source_uv};
    use std::sync::Arc;

    type Thumbnail = isize;

    /// `DWM_THUMBNAIL_PROPERTIES` (`dwmapi.h` wraps all structs in `#include <pshpack1.h>`).
    #[repr(C, packed(1))]
    struct ThumbnailProperties {
        flags: u32,
        destination: RECT,
        source: RECT,
        opacity: u8,
        visible: BOOL,
        client_area_only: BOOL,
    }

    const DWM_TNP_RECTDESTINATION: u32 = 0x1;
    const DWM_TNP_OPACITY: u32 = 0x4;
    const DWM_TNP_VISIBLE: u32 = 0x8;
    const DWM_TNP_SOURCECLIENTAREAONLY: u32 = 0x10;

    /// Borderless fullscreen style for the compositor window: zero non-client border.
    pub(super) const COMPOSITOR_STYLE: u32 = WS_POPUP | WS_MINIMIZEBOX | WS_SYSMENU | WS_VISIBLE | WS_CLIPSIBLINGS | WS_CLIPCHILDREN;

    /// Chrome flags stripped from contained split-screen game windows.
    const GAME_CHROME: u32 = WS_CAPTION | WS_THICKFRAME | WS_BORDER | WS_DLGFRAME | WS_MAXIMIZEBOX | WS_MINIMIZEBOX | WS_SYSMENU;

    /// dwmapi.dll's thumbnail calls, from System32 only (see `only_system_dlls` in
    /// main.rs): looked up when the first window is made.
    struct Dwm {
        register: unsafe extern "system" fn(HWND, HWND, *mut Thumbnail) -> i32,
        unregister: unsafe extern "system" fn(Thumbnail) -> i32,
        update: unsafe extern "system" fn(Thumbnail, *const ThumbnailProperties) -> i32,
        query_size: Option<unsafe extern "system" fn(Thumbnail, *mut SIZE) -> i32>,
    }

    fn dwm() -> Option<&'static Dwm> {
        static DWM: OnceLock<Option<Dwm>> = OnceLock::new();
        DWM.get_or_init(|| {
            let name: Vec<u16> = "dwmapi.dll".encode_utf16().chain(Some(0)).collect();
            let m = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
            if m.is_null() {
                return None;
            }
            let get = |f: &std::ffi::CStr| unsafe { GetProcAddress(m, f.as_ptr().cast()) };
            let (r, u, p) = (get(c"DwmRegisterThumbnail")?, get(c"DwmUnregisterThumbnail")?, get(c"DwmUpdateThumbnailProperties")?);
            let q = get(c"DwmQueryThumbnailSourceSize");
            // SAFETY: dwmapi's exports have these signatures (dwmapi.h).
            unsafe {
                Some(Dwm {
                    register: std::mem::transmute(r),
                    unregister: std::mem::transmute(u),
                    update: std::mem::transmute(p),
                    query_size: q.map(|f| std::mem::transmute(f)),
                })
            }
        })
        .as_ref()
    }

    /// Ends process `pid` with exit code 1 (ended, not finished).
    pub(super) fn terminate(pid: u32) {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
        if pid == 0 {
            return;
        }
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                windows_sys::Win32::Foundation::CloseHandle(h);
            }
        }
    }

    pub(super) fn post_close(hwnd: u64) {
        if hwnd != 0 {
            unsafe { PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0) };
        }
    }

    /// Tells a contained game window that it is active/focused without stealing Win32
    /// foreground from the compositor window.
    pub(super) fn notify_active(hwnd: u64) {
        if hwnd == 0 {
            return;
        }
        let h = hwnd as HWND;
        unsafe {
            PostMessageW(h, WM_ACTIVATEAPP, 1, 0);
            PostMessageW(h, WM_ACTIVATE, 1, 0);
            PostMessageW(h, WM_SETFOCUS, 0, 0);
        }
    }

    #[derive(Clone, Copy)]
    enum ForwardedInput {
        Mouse { msg: u32, w: WPARAM, cx: i32, cy: i32, is_wheel: bool, is_button_down: bool },
        Key { msg: u32, w: WPARAM, l: LPARAM },
        /// The session's window lost the keyboard (Alt+Tab, a menu, a dialog): what's
        /// down in the games is let go, as its releases go elsewhere now.
        ReleaseAll,
    }

    /// The message that lets `held` go, for a game window it went down in with `lp`.
    fn release_of(held: Held, lp: LPARAM) -> (u32, WPARAM, LPARAM) {
        match held {
            // A key's release: the previous state and transition bits set.
            Held::Key(vk) => (WM_KEYUP, vk, lp | 0xC000_0000u32 as i32 as LPARAM | peerparty_protocol::session::FORWARDED_KEY),
            Held::Button(msg, x) => (msg, (x as WPARAM) << 16, lp),
        }
    }

    /// Whether client point `(x, y)` is on the frames drawn in `dest` (not in the bars
    /// beside them, or an empty grid cell).
    fn on_frames(dest: PaneRect, x: i32, y: i32) -> bool {
        x >= dest.x && y >= dest.y && i64::from(x) < i64::from(dest.x) + i64::from(dest.width) && i64::from(y) < i64::from(dest.y) + i64::from(dest.height)
    }

    /// Runs `f` with this thread's DPI awareness the game window `h`'s own, so window
    /// coordinates come out as the game counts them: a game that isn't DPI aware works
    /// in scaled pixels, and a mouse message or cursor position in real ones would land
    /// off where the player pointed.
    fn in_dpi_of<T>(h: HWND, f: impl FnOnce() -> T) -> T {
        use windows_sys::Win32::UI::HiDpi::{GetWindowDpiAwarenessContext, SetThreadDpiAwarenessContext};
        let ctx = unsafe { GetWindowDpiAwarenessContext(h) };
        if ctx.is_null() {
            return f();
        }
        let was = unsafe { SetThreadDpiAwarenessContext(ctx) };
        let out = f();
        if !was.is_null() {
            unsafe { SetThreadDpiAwarenessContext(was) };
        }
        out
    }

    /// The pointer is hidden over the session's window (a game locked/hid its cursor, or
    /// the mouse sat idle while controllers play).
    static HIDE_POINTER: AtomicBool = AtomicBool::new(false);
    /// The game itself locked or hid the cursor, so moving the mouse doesn't unhide it.
    static FORCE_HIDE_POINTER: AtomicBool = AtomicBool::new(false);
    /// When the mouse last moved or clicked over the session's window (`mono_ms`; 0: never).
    static LAST_MOUSE_MS: AtomicU64 = AtomicU64::new(0);
    /// How long the mouse sits still before auto-hiding in controller/idle mode.
    const IDLE_HIDE_MS: u64 = 1500;

    fn mono_ms() -> u64 {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
    }

    fn mouse_idle() -> bool {
        let last = LAST_MOUSE_MS.load(Ordering::Relaxed);
        last == 0 || mono_ms().saturating_sub(last) >= IDLE_HIDE_MS
    }

    /// A key or mouse button down in a game window, until it's let go.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Held {
        Key(usize),
        /// A mouse button, by the message that lets it go (and which X button).
        Button(u32, u16),
    }

    /// The mouse button a message is about, by the message that lets it go.
    fn button(msg: u32, w: WPARAM) -> Option<Held> {
        let x = ((w >> 16) & 0xffff) as u16;
        match msg {
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_LBUTTONUP => Some(Held::Button(WM_LBUTTONUP, 0)),
            WM_RBUTTONDOWN | WM_RBUTTONDBLCLK | WM_RBUTTONUP => Some(Held::Button(WM_RBUTTONUP, 0)),
            WM_MBUTTONDOWN | WM_MBUTTONUP => Some(Held::Button(WM_MBUTTONUP, 0)),
            WM_XBUTTONDOWN | WM_XBUTTONUP => Some(Held::Button(WM_XBUTTONUP, x)),
            _ => None,
        }
    }

    /// What `ev` presses, if anything.
    fn pressed(ev: &ForwardedInput) -> Option<Held> {
        match *ev {
            ForwardedInput::Key { msg: WM_KEYDOWN | WM_SYSKEYDOWN, w, .. } => Some(Held::Key(w)),
            ForwardedInput::Mouse { msg, w, is_button_down: true, .. } => button(msg, w),
            ForwardedInput::Mouse { msg: msg @ (WM_LBUTTONDBLCLK | WM_RBUTTONDBLCLK), w, .. } => button(msg, w),
            _ => None,
        }
    }

    /// What `ev` lets go, if anything.
    fn released(ev: &ForwardedInput) -> Option<Held> {
        match *ev {
            ForwardedInput::Key { msg: WM_KEYUP | WM_SYSKEYUP, w, .. } => Some(Held::Key(w)),
            ForwardedInput::Mouse { msg: msg @ (WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP), w, .. } => button(msg, w),
            _ => None,
        }
    }

    fn input_queue() -> &'static Mutex<Vec<ForwardedInput>> {
        static Q: OnceLock<Mutex<Vec<ForwardedInput>>> = OnceLock::new();
        Q.get_or_init(|| Mutex::new(Vec::new()))
    }

    fn push_input(ev: ForwardedInput) {
        if let Ok(mut q) = input_queue().lock()
            && q.len() < 256
        {
            q.push(ev);
        }
    }

    /// The window was resized or moved since the last `take_resized` (its procedure
    /// sets it).
    static RESIZED: AtomicBool = AtomicBool::new(false);
    /// The user asked to close the compositor window (`WM_CLOSE`).
    static CLOSE_REQUESTED: AtomicBool = AtomicBool::new(false);
    /// The last click in the client area (its procedure sets it): x and y in the low and
    /// high halves, `NO_CLICK` when taken.
    static CLICK: AtomicU64 = AtomicU64::new(NO_CLICK);
    const NO_CLICK: u64 = u64::MAX;

    /// A click's position packed into `CLICK`, and back.
    pub(super) fn pack(x: i32, y: i32) -> u64 {
        u64::from(x as u32) | u64::from(y as u32) << 32
    }

    pub(super) fn unpack(v: u64) -> Option<(i32, i32)> {
        (v != NO_CLICK).then_some((v as u32 as i32, (v >> 32) as u32 as i32))
    }

    fn is_session_chord(vk: usize) -> bool {
        crate::host::ctrl_alt(|k| unsafe { GetKeyState(k) } < 0) && matches!(vk as u32, 0x45 | 0x4C | 0x5A | 0x31..=0x34 | 0x61..=0x64)
    }

    unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        match m {
            WM_SIZE => {
                if w != SIZE_MINIMIZED as usize {
                    RESIZED.store(true, Ordering::Release);
                }
            }
            WM_MOVE | WM_DPICHANGED | WM_DISPLAYCHANGE => RESIZED.store(true, Ordering::Release),
            WM_KILLFOCUS | WM_CANCELMODE => push_input(ForwardedInput::ReleaseAll),
            WM_ACTIVATE if w & 0xffff == WA_INACTIVE as usize => push_input(ForwardedInput::ReleaseAll),
            WM_SETCURSOR if HIDE_POINTER.load(Ordering::Relaxed) && (l & 0xffff) as u32 == HTCLIENT => {
                unsafe { SetCursor(std::ptr::null_mut()) };
                return 1;
            }
            WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_RBUTTONDBLCLK | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let (x, y) = (i32::from(l as u16 as i16), i32::from((l >> 16) as u16 as i16));
                let is_button_down = matches!(m, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN);
                if m == WM_MOUSEMOVE || is_button_down {
                    LAST_MOUSE_MS.store(mono_ms(), Ordering::Relaxed);
                    if !FORCE_HIDE_POINTER.load(Ordering::Relaxed) && HIDE_POINTER.swap(false, Ordering::Relaxed) {
                        unsafe { SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_ARROW)) };
                    }
                }
                if m == WM_LBUTTONDOWN {
                    CLICK.store(pack(x, y), Ordering::Release);
                }
                push_input(ForwardedInput::Mouse { msg: m, w, cx: x, cy: y, is_wheel: false, is_button_down });
                return 0;
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let mut pt = POINT { x: i32::from(l as u16 as i16), y: i32::from((l >> 16) as u16 as i16) };
                unsafe { ScreenToClient(h, &mut pt) };
                push_input(ForwardedInput::Mouse { msg: m, w, cx: pt.x, cy: pt.y, is_wheel: true, is_button_down: false });
                return 0;
            }
            WM_KEYDOWN | WM_KEYUP | WM_CHAR => {
                if !is_session_chord(w) {
                    push_input(ForwardedInput::Key { msg: m, w, l });
                }
                return 0;
            }
            WM_SYSKEYDOWN | WM_SYSKEYUP => {
                // Keep Alt+F4 working through DefWindowProcW so it triggers WM_CLOSE.
                if w != usize::from(VK_F4) {
                    if !is_session_chord(w) {
                        push_input(ForwardedInput::Key { msg: m, w, l });
                    }
                    return 0;
                }
            }
            // Ask the running game copies to close cleanly; the session ends with them.
            WM_CLOSE => {
                CLOSE_REQUESTED.store(true, Ordering::Release);
                return 0;
            }
            _ => {}
        }
        unsafe { DefWindowProcW(h, m, w, l) }
    }

    fn client_size(hwnd: HWND) -> (u32, u32) {
        let mut r: RECT = unsafe { std::mem::zeroed() };
        if unsafe { GetClientRect(hwnd, &mut r) } == 0 {
            return (0, 0);
        }
        ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32)
    }

    fn dwm_source_size(dwm: &Dwm, thumbnail: Thumbnail) -> (u32, u32) {
        let Some(query) = dwm.query_size else { return (0, 0) };
        let mut sz = SIZE { cx: 0, cy: 0 };
        if unsafe { query(thumbnail, &mut sz) } >= 0 {
            (sz.cx.max(0) as u32, sz.cy.max(0) as u32)
        } else {
            (0, 0)
        }
    }

    fn extract_icon(exe: &Path, size: i32) -> HICON {
        use std::os::windows::ffi::OsStrExt;
        let path: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut icon: HICON = std::ptr::null_mut();
        let mut id = 0u32;
        let n = unsafe { PrivateExtractIconsW(path.as_ptr(), 0, size, size, &mut icon, &mut id, 1, 0) };
        if n == 0 || n == u32::MAX { std::ptr::null_mut() } else { icon }
    }

    /// True if `candidate` sits above `anchor` in the Win32 Z-order.
    fn is_above(candidate: HWND, anchor: HWND) -> bool {
        let mut cur = unsafe { GetWindow(anchor, GW_HWNDPREV) };
        while !cur.is_null() {
            if cur == candidate {
                return true;
            }
            cur = unsafe { GetWindow(cur, GW_HWNDPREV) };
        }
        false
    }

    /// Strips chrome & taskbar/Alt+Tab presence from a game window, resizes it to its
    /// pane, and tucks it strictly behind `compositor`. The calls that wait on the game's
    /// window thread are asked of it without waiting (`ShowWindowAsync`,
    /// `SWP_ASYNCWINDOWPOS`): a game busy loading, or hung, mustn't stall the session's
    /// window, its overlays and every other player's input with it. A hung one isn't
    /// touched at all (its style changes would wait for it). False when it wasn't.
    fn contain_window(compositor: HWND, src: HWND, pane: PaneRect, bounds: MonitorBounds, hidden: bool) -> bool {
        if src.is_null() || unsafe { IsWindow(src) } == 0 || unsafe { IsHungAppWindow(src) } != 0 {
            return false;
        }
        unsafe {
            let style = GetWindowLongW(src, GWL_STYLE) as u32;
            let want_style = (style & !GAME_CHROME) | WS_POPUP;
            let ex = GetWindowLongW(src, GWL_EXSTYLE) as u32;
            let want_ex = (ex | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) & !WS_EX_APPWINDOW;
            let had_taskbar = (ex & WS_EX_TOOLWINDOW) == 0 || (ex & WS_EX_APPWINDOW) != 0;
            if had_taskbar && IsWindowVisible(src) != 0 {
                // Shell removes an existing taskbar button only across a hide/show transition.
                ShowWindowAsync(src, SW_HIDE);
            }
            if want_style != style {
                SetWindowLongW(src, GWL_STYLE, want_style as i32);
            }
            if want_ex != ex {
                SetWindowLongW(src, GWL_EXSTYLE, want_ex as i32);
            }
            // A topmost window stays above the session's whatever goes behind what: it's
            // made a plain one first.
            if ex & WS_EX_TOPMOST != 0 {
                SetWindowPos(src, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS);
            }
            if hidden {
                ShowWindowAsync(src, SW_HIDE);
                return true;
            }
            if IsIconic(src) != 0 || IsWindowVisible(src) == 0 || had_taskbar {
                ShowWindowAsync(src, SW_SHOWNOACTIVATE);
                if IsIconic(src) != 0 {
                    ShowWindowAsync(src, SW_RESTORE);
                    SetForegroundWindow(compositor);
                }
            }
            let (w, h) = (i32::try_from(pane.width).unwrap_or(i32::MAX), i32::try_from(pane.height).unwrap_or(i32::MAX));
            if w > 0 && h > 0 {
                SetWindowPos(
                    src,
                    compositor,
                    bounds.x.saturating_add(pane.x),
                    bounds.y.saturating_add(pane.y),
                    w,
                    h,
                    SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_ASYNCWINDOWPOS,
                );
            } else {
                SetWindowPos(src, compositor, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_ASYNCWINDOWPOS);
            }
        }
        notify_active(src as u64);
        true
    }

    /// Keeps `src` strictly behind `compositor` and inside `bounds` (without fighting a
    /// game that refuses to shrink below its minimum size).
    fn keep_behind(compositor: HWND, src: HWND, bounds: MonitorBounds) {
        if src.is_null() || unsafe { IsWindow(src) } == 0 || unsafe { IsHungAppWindow(src) } != 0 {
            return;
        }
        if unsafe { GetWindowLongW(src, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST != 0 {
            unsafe { SetWindowPos(src, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS) };
        }
        unsafe {
            if GetForegroundWindow() == src {
                SetForegroundWindow(compositor);
            }
            let mut wr: RECT = std::mem::zeroed();
            let have_rect = GetWindowRect(src, &mut wr) != 0;
            let actual_w = wr.right - wr.left;
            let actual_h = wr.bottom - wr.top;
            let max_x = bounds.x.saturating_add(bounds.width as i32).saturating_sub(actual_w).max(bounds.x);
            let max_y = bounds.y.saturating_add(bounds.height as i32).saturating_sub(actual_h).max(bounds.y);
            let clamped_x = wr.left.clamp(bounds.x, max_x);
            let clamped_y = wr.top.clamp(bounds.y, max_y);
            let needs_move = have_rect && (clamped_x != wr.left || clamped_y != wr.top);
            let above = is_above(src, compositor);
            if needs_move {
                SetWindowPos(src, compositor, clamped_x, clamped_y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS);
            } else if above {
                SetWindowPos(src, compositor, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS);
            }
        }
    }

    /// A copy shown in a pane: its window, thumbnail, layout pane, and fitted rect.
    struct Shown {
        source: u64,
        thumbnail: Thumbnail,
        layout_pane: PaneRect,
        display_pane: PaneRect,
        dest: PaneRect,
        uv: [f32; 4],
        fit: PaneFit,
        z: u8,
        src_size: (u32, u32),
        adopted_at: Instant,
        grace_retried: bool,
        /// The window draws its frames itself: its thumbnail is hidden.
        drawn: bool,
        /// Its window's style and extended style before it was put in its pane, given
        /// back when the session's window goes while it's still there.
        style: (u32, u32),
    }

    pub struct HostWindow {
        pub hwnd: HWND,
        shown: Vec<Option<Shown>>,
        /// Draws the copies' frames from their shared textures (gpu.rs).
        pub gpu: Option<Gpu>,
        /// Per copy: its ring block and textures, once its hook reported them.
        surfaces: Vec<(Option<Arc<RingBlock>>, Option<Arc<SharedSurface>>)>,
        hidden_for_minimize: bool,
        icon_big: HICON,
        icon_small: HICON,
        /// Where the mouse last was over each copy's pane, on screen as that game counts
        /// it (`cursor_for`).
        cursor_screen: Vec<Option<(i32, i32)>>,
        /// The real cursor is kept inside this copy's pane (`confine`).
        confined: Option<usize>,
        /// Each copy's display pane, fit, and z, as `show`/`preview_pane` last saw them
        /// (whether or not its window is known yet).
        display: Vec<(PaneRect, PaneFit, u8)>,
        /// Keys and buttons down in a game window, where (and how) they went.
        pressed: Vec<(Held, isize, LPARAM)>,
        /// What changed in each copy's pane, for its log (`take_notes`).
        pub notes: Vec<(usize, String)>,
        /// Whether CAS sharpening is enabled for upscaled panes.
        upscale: bool,
    }

    impl HostWindow {
        /// A borderless fullscreen top-level window over `bounds`, with `title` and `exe`'s icon.
        pub fn create(bounds: MonitorBounds, title: &str, exe: &Path) -> Option<HostWindow> {
            static CLASS: std::sync::Once = std::sync::Once::new();
            let class: Vec<u16> = "peerparty-compositor".encode_utf16().chain(Some(0)).collect();
            let title_text = if title.trim().is_empty() { "PeerParty" } else { title.trim() };
            let title_w: Vec<u16> = title_text.encode_utf16().chain(Some(0)).collect();
            let module = unsafe { GetModuleHandleW(std::ptr::null()) };
            CLASS.call_once(|| {
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(proc),
                    hInstance: module,
                    hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
                    hbrBackground: unsafe { GetStockObject(BLACK_BRUSH) } as HBRUSH,
                    lpszClassName: class.as_ptr(),
                    ..unsafe { std::mem::zeroed() }
                };
                unsafe { RegisterClassW(&wc) };
            });
            CLOSE_REQUESTED.store(false, Ordering::Release);
            // A session before this one may have left these set.
            RESIZED.store(false, Ordering::Release);
            CLICK.store(NO_CLICK, Ordering::Release);
            HIDE_POINTER.store(false, Ordering::Relaxed);
            FORCE_HIDE_POINTER.store(false, Ordering::Relaxed);
            LAST_MOUSE_MS.store(0, Ordering::Relaxed);
            if let Ok(mut q) = input_queue().lock() {
                q.clear();
            }
            let (w, h) = (i32::try_from(bounds.width).unwrap_or(i32::MAX), i32::try_from(bounds.height).unwrap_or(i32::MAX));
            // A test's session stays out of sight (PEERPARTY_SESSION_HIDDEN).
            let hidden = std::env::var(peerparty_protocol::env::SESSION_HIDDEN).is_ok_and(|v| v.trim() == "1");
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_APPWINDOW,
                    class.as_ptr(),
                    title_w.as_ptr(),
                    if hidden { COMPOSITOR_STYLE & !WS_VISIBLE } else { COMPOSITOR_STYLE },
                    bounds.x,
                    bounds.y,
                    w,
                    h,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    module,
                    std::ptr::null(),
                )
            };
            if hwnd.is_null() {
                return None;
            }
            let icon_big = extract_icon(exe, 32);
            let icon_small = extract_icon(exe, 16);
            unsafe {
                if !icon_big.is_null() {
                    SendMessageW(hwnd, WM_SETICON, ICON_BIG as WPARAM, icon_big as LPARAM);
                }
                if !icon_small.is_null() {
                    SendMessageW(hwnd, WM_SETICON, ICON_SMALL as WPARAM, icon_small as LPARAM);
                }
                if hidden {
                    SetWindowPos(hwnd, HWND_TOP, bounds.x, bounds.y, w, h, SWP_FRAMECHANGED | SWP_NOACTIVATE);
                    eprintln!("peerparty: session: its window is hidden (PEERPARTY_SESSION_HIDDEN=1)");
                } else {
                    SetWindowPos(hwnd, HWND_TOP, bounds.x, bounds.y, w, h, SWP_FRAMECHANGED | SWP_SHOWWINDOW);
                    SetForegroundWindow(hwnd);
                }
            }
            let gpu = Gpu::start(hwnd);
            Some(HostWindow { hwnd, shown: Vec::new(), gpu, surfaces: Vec::new(), hidden_for_minimize: false, icon_big, icon_small, cursor_screen: Vec::new(), confined: None, display: Vec::new(), pressed: Vec::new(), notes: Vec::new(), upscale: true })
        }

        pub fn set_upscale(&mut self, on: bool) {
            if self.upscale != on {
                self.upscale = on;
                self.push_panes();
            }
        }

        pub fn set_sync_interval(&mut self, interval: u8) {
            if let Some(gpu) = &self.gpu {
                gpu.set_sync_interval(interval);
            }
        }

        /// Where the mouse is in copy `i`'s window, on screen as the game counts it: where
        /// it last was over its pane, else the middle of its window (a pane given the
        /// keyboard and mouse by Ctrl+Alt+1-4 hasn't had the mouse over it yet).
        pub fn cursor_for(&self, i: usize) -> Option<(i32, i32)> {
            if let Some(p) = self.cursor_screen.get(i).copied().flatten() {
                return Some(p);
            }
            let src = self.shown.get(i)?.as_ref()?.source as HWND;
            Some(in_dpi_of(src, || {
                let (w, h) = client_size(src);
                let mut pt = POINT { x: (w / 2) as i32, y: (h / 2) as i32 };
                unsafe { ClientToScreen(src, &mut pt) };
                (pt.x, pt.y)
            }))
        }

        /// Keeps the real cursor inside copy `confine`'s frames (None: lets it go), and
        /// hides the pointer over the window when confined, when the focused game hid its
        /// cursor (`game_hide`), or when the mouse is idle (`idle_hide`). Returns whether
        /// confinement changed.
        pub fn update_pointer(&mut self, confine: Option<usize>, game_hide: bool, idle_hide: bool) -> bool {
            let dest = confine.and_then(|i| self.shown.get(i)?.as_ref()).map(|s| s.dest).filter(|d| d.width > 0 && d.height > 0);
            let i = confine.filter(|_| dest.is_some());
            if let Some(d) = dest {
                let mut tl = POINT { x: d.x, y: d.y };
                unsafe { ClientToScreen(self.hwnd, &mut tl) };
                let r = RECT { left: tl.x, top: tl.y, right: tl.x.saturating_add(d.width as i32), bottom: tl.y.saturating_add(d.height as i32) };
                // Again every tick: Windows lets a clip go when another window takes the
                // foreground for a moment.
                unsafe { ClipCursor(&r) };
            }
            let confine_changed = i != self.confined;
            if confine_changed {
                if dest.is_none() {
                    unsafe { ClipCursor(std::ptr::null()) };
                }
                self.confined = i;
            }
            let force_hide = i.is_some() || game_hide;
            FORCE_HIDE_POINTER.store(force_hide, Ordering::Relaxed);
            let want_hide = force_hide || (idle_hide && mouse_idle());
            if HIDE_POINTER.swap(want_hide, Ordering::Relaxed) != want_hide {
                // The pointer's look is asked for again when it next moves; this shows the
                // change at once.
                let mut pt = POINT { x: 0, y: 0 };
                unsafe {
                    GetCursorPos(&mut pt);
                    SetCursorPos(pt.x, pt.y);
                }
            }
            confine_changed
        }

        /// The client area on screen, with the window's monitor's DPI.
        pub fn client_bounds(&self) -> Option<MonitorBounds> {
            let mut r: RECT = unsafe { std::mem::zeroed() };
            let mut tl = POINT { x: 0, y: 0 };
            if unsafe { GetClientRect(self.hwnd, &mut r) } == 0 {
                return None;
            }
            unsafe { ClientToScreen(self.hwnd, &mut tl) };
            let dpi = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(self.hwnd) };
            Some(MonitorBounds { x: tl.x, y: tl.y, width: (r.right - r.left).max(0) as u32, height: (r.bottom - r.top).max(0) as u32, dpi: if dpi == 0 { 96 } else { dpi } })
        }

        /// Whether it was resized or moved since the last look.
        pub fn take_resized(&mut self) -> Option<()> {
            RESIZED.swap(false, Ordering::AcqRel).then_some(())
        }

        /// The last click since the last look.
        pub fn take_click(&mut self) -> Option<(i32, i32)> {
            unpack(CLICK.swap(NO_CLICK, Ordering::AcqRel))
        }

        pub fn take_close_requested(&mut self) -> bool {
            CLOSE_REQUESTED.swap(false, Ordering::AcqRel)
        }

        /// Updates pane `i`'s visual destination, fit mode, and Z-order immediately for
        /// live Layout Studio dragging without touching the game window's Win32 bounds.
        pub fn preview_pane(&mut self, i: usize, display_pane: PaneRect, fit: PaneFit, z: u8, viewport: Option<(u32, u32)>) {
            if self.display.len() <= i {
                self.display.resize(i + 1, (super::HIDDEN, PaneFit::Fit, 0));
            }
            self.display[i] = (display_pane, fit, z);
            if let Some(Some(s)) = self.shown.get_mut(i) {
                s.fit = fit;
                s.z = z;
                self.update(i, display_pane, viewport, false, false);
            }
        }

        /// Shows window `source` (0: none) in pane `i` of the client area; a display pane
        /// of no size hides its thumbnail.
        #[allow(clippy::too_many_arguments)]
        pub fn show(&mut self, i: usize, source: u64, layout_pane: PaneRect, display_pane: PaneRect, fit: PaneFit, z: u8, bounds: MonitorBounds, viewport: Option<(u32, u32)>) {
            if self.shown.len() <= i {
                self.shown.resize_with(i + 1, || None);
            }
            if self.display.len() <= i {
                self.display.resize(i + 1, (super::HIDDEN, PaneFit::Fit, 0));
            }
            self.display[i] = (display_pane, fit, z);
            if self.shown[i].as_ref().is_some_and(|s| s.source != source) {
                let old = self.shown[i].take().expect("checked");
                if old.thumbnail != 0
                    && let Some(dwm) = dwm()
                {
                    unsafe { (dwm.unregister)(old.thumbnail) };
                }
            }
            if source == 0 {
                return;
            }
            let minimized = unsafe { IsIconic(self.hwnd) } != 0;
            if self.shown[i].is_none() {
                let src = source as HWND;
                let style = unsafe { (GetWindowLongW(src, GWL_STYLE) as u32, GetWindowLongW(src, GWL_EXSTYLE) as u32) };
                if !contain_window(self.hwnd, src, layout_pane, bounds, minimized) {
                    self.notes.push((i, format!("session: the game's window 0x{source:x} isn't answering (busy or hung): put in its pane once it does")));
                }
                // The thumbnail is only the fallback for frames the window can't draw
                // itself: without one, the pane and its input still work.
                let mut thumbnail: Thumbnail = 0;
                match dwm() {
                    Some(dwm) if unsafe { (dwm.register)(self.hwnd, src, &mut thumbnail) } >= 0 => {}
                    _ => {
                        thumbnail = 0;
                        self.notes.push((i, format!("session: no DWM thumbnail of the game's window 0x{source:x}: its pane shows only the frames it hands over")));
                    }
                }
                // A copy that made a new window (a mode switch, a device reset) may still
                // be handing over frames this window draws: its new thumbnail starts
                // hidden, or it would cover them until `push_panes` saw it.
                let drawn = self.gpu.as_ref().is_some_and(|g| g.live(i));
                if drawn {
                    self.notes.push((i, format!("session: the game's window is now 0x{source:x}; its pane keeps drawing the frames it hands over")));
                }
                self.shown[i] = Some(Shown {
                    source,
                    thumbnail,
                    layout_pane,
                    display_pane,
                    dest: super::HIDDEN,
                    uv: FULL_UV,
                    fit,
                    z,
                    src_size: (0, 0),
                    adopted_at: Instant::now(),
                    grace_retried: false,
                    drawn,
                    style,
                });
                self.update(i, display_pane, viewport, true, true);
                return;
            }
            if let Some(s) = self.shown[i].as_mut() {
                s.fit = fit;
                s.z = z;
            }
            if self.shown[i].as_ref().is_some_and(|s| s.layout_pane != layout_pane) {
                if let Some(s) = self.shown[i].as_mut() {
                    s.layout_pane = layout_pane;
                }
                contain_window(self.hwnd, source as HWND, layout_pane, bounds, minimized);
            }
            self.update(i, display_pane, viewport, false, true);
        }

        /// Copy `i`'s textures and ring block (None: it has none, or ended).
        pub fn set_surface(&mut self, i: usize, surface: Option<Arc<SharedSurface>>, ring: Option<Arc<RingBlock>>) {
            if self.surfaces.len() <= i {
                self.surfaces.resize(i + 1, (None, None));
            }
            self.surfaces[i] = (ring, surface);
            // Fitted to the new frames' size before the drawing thread hears of them.
            if let Some((pane, src)) = self.shown.get(i).and_then(|s| s.as_ref()).map(|s| (s.display_pane, s.src_size)) {
                self.update(i, pane, Some(src).filter(|s| s.0 > 0), false, true);
            }
            self.push_panes();
        }

        /// Tells the drawing thread where each copy's frames go and from what, and hides
        /// the thumbnails of the panes it draws (shows them again when it stops).
        pub fn push_panes(&mut self) {
            let Some(gpu) = self.gpu.as_ref() else { return };
            let n = self.shown.len().max(self.surfaces.len());
            let panes: Vec<PaneDraw> = (0..n)
                .map(|i| {
                    let (ring, surface) = self.surfaces.get(i).cloned().unwrap_or((None, None));
                    // Frames before the copy's window is known go in its pane too, fitted
                    // by their own size.
                    let (dest, uv, z) = match self.shown.get(i).and_then(|s| s.as_ref()) {
                        Some(s) => (s.dest, s.uv, s.z),
                        None => match (surface.as_ref().map(|s| s.size), self.display.get(i)) {
                            (Some((w, h)), Some(&(p, fit, z))) if w > 0 && h > 0 => {
                                let (d, uv) = compute_fit(p, w, h, fit);
                                (d, uv, z)
                            }
                            _ => (super::HIDDEN, FULL_UV, 0),
                        },
                    };
                    PaneDraw { dest, uv, z, sharpen: self.upscale, ring, surface }
                })
                .collect();
            gpu.set(panes);
            let live: Vec<bool> = (0..self.shown.len()).map(|i| gpu.live(i)).collect();
            for (i, drawn) in live.into_iter().enumerate() {
                if let Some(s) = self.shown[i].as_mut()
                    && s.drawn != drawn
                {
                    s.drawn = drawn;
                    let (pane, src) = (s.display_pane, s.src_size);
                    self.update(i, pane, Some(src).filter(|s| s.0 > 0), true, true);
                }
            }
        }

        /// Periodic maintenance: syncs hide/show with compositor minimize, restores any
        /// game windows minimized by Win+D, keeps game windows behind the compositor,
        /// does one 1-second grace re-containment, and updates aspect-fitted thumbnails.
        pub fn maintain(&mut self, bounds: MonitorBounds, instances: &[InstanceConn]) {
            let minimized = unsafe { IsIconic(self.hwnd) } != 0;
            if minimized != self.hidden_for_minimize {
                self.hidden_for_minimize = minimized;
                for s in self.shown.iter().flatten() {
                    if minimized {
                        unsafe { ShowWindowAsync(s.source as HWND, SW_HIDE) };
                    } else {
                        contain_window(self.hwnd, s.source as HWND, s.layout_pane, bounds, false);
                    }
                }
            }
            if minimized {
                return;
            }
            for i in 0..self.shown.len() {
                let Some(s) = self.shown[i].as_mut() else { continue };
                let src = s.source as HWND;
                let mut force_thumb = false;
                // Win+D can minimize background game windows without firing WM_SIZE on
                // us. Put back only while the session's window is in front: with the
                // desktop shown, putting them back fights Windows (and the player).
                if unsafe { IsIconic(src) != 0 || IsWindowVisible(src) == 0 } {
                    if unsafe { GetForegroundWindow() } == self.hwnd {
                        contain_window(self.hwnd, src, s.layout_pane, bounds, false);
                        force_thumb = true;
                    }
                } else if !s.grace_retried && s.adopted_at.elapsed() >= Duration::from_secs(1) {
                    s.grace_retried = true;
                    let (cw, ch) = client_size(src);
                    let ex = unsafe { GetWindowLongW(src, GWL_EXSTYLE) } as u32;
                    let needs_style = (ex & WS_EX_TOOLWINDOW) == 0 || (ex & WS_EX_APPWINDOW) != 0;
                    let wrong_size = s.layout_pane.width > 0
                        && s.layout_pane.height > 0
                        && (cw != s.layout_pane.width || ch != s.layout_pane.height);
                    if needs_style || wrong_size {
                        contain_window(self.hwnd, src, s.layout_pane, bounds, false);
                        force_thumb = true;
                    }
                }
                keep_behind(self.hwnd, src, bounds);
                let display_pane = s.display_pane;
                let vp = instances.get(i).and_then(|inst| inst.viewport.as_ref()).map(|v| (v.width, v.height));
                self.update(i, display_pane, vp, force_thumb, true);
            }
            self.push_panes();
        }

        /// Drains keyboard/mouse messages received by the compositor window and forwards
        /// them to the focused pane's game window with mapped coordinates.
        pub fn forward_input(&mut self, mut focus: usize, overlay_open: bool, panes: &[PaneRect], zs: &[u8], instances: &[InstanceConn]) -> Option<usize> {
            let events: Vec<ForwardedInput> = match input_queue().lock() {
                Ok(mut q) => std::mem::take(&mut *q),
                Err(_) => return None,
            };
            if events.is_empty() {
                return None;
            }
            let mut switched = None;
            let mut order: Vec<usize> = (0..panes.len()).collect();
            order.sort_by_key(|&i| (zs.get(i).copied().unwrap_or(0), i));
            order.reverse();
            let pane_at = |x: i32, y: i32| {
                order.iter().copied().find(|&i| {
                    let p = panes[i];
                    x >= p.x && y >= p.y && i64::from(x) < i64::from(p.x) + i64::from(p.width) && i64::from(y) < i64::from(p.y) + i64::from(p.height)
                })
            };
            for ev in events {
                if let ForwardedInput::ReleaseAll = ev {
                    if !self.pressed.is_empty() {
                        self.notes.push((focus, format!("session: the session's window lost the keyboard: {} key(s) and button(s) still down in the games let go", self.pressed.len())));
                    }
                    for (held, hwnd, lp) in self.pressed.drain(..) {
                        let (msg, w, l) = release_of(held, lp);
                        unsafe { PostMessageW(hwnd as HWND, msg, w, l) };
                    }
                    continue;
                }
                // A key or button let go goes to the copy that got it pressed, wherever the
                // focus is now and even while an overlay has the keyboard: else it stays
                // down in that copy for good.
                if let Some(held) = released(&ev)
                    && let Some(i) = self.pressed.iter().position(|(h, _, _)| *h == held)
                {
                    let (_, hwnd, lp) = self.pressed.swap_remove(i);
                    let here = self.shown.get(focus).and_then(|s| s.as_ref()).map(|s| s.source as isize);
                    // Let go where it went down: as any other message, where the mouse is now.
                    if !overlay_open && here == Some(hwnd) && self.forward_one(ev, &mut focus, &mut switched, &pane_at, instances) {
                        continue;
                    }
                    let (msg, w, lp) = match ev {
                        ForwardedInput::Mouse { msg, w, .. } => (msg, w, lp),
                        ForwardedInput::Key { msg, w, l } => (msg, w, l | peerparty_protocol::session::FORWARDED_KEY),
                        ForwardedInput::ReleaseAll => continue,
                    };
                    unsafe { PostMessageW(hwnd as HWND, msg, w, lp) };
                    continue;
                }
                if !overlay_open {
                    self.forward_one(ev, &mut focus, &mut switched, &pane_at, instances);
                }
            }
            switched
        }

        /// Forwards `ev` to the focused pane's game window (a click on another pane
        /// focuses it first: `focus`, `switched`). False when it went nowhere.
        fn forward_one(&mut self, ev: ForwardedInput, focus: &mut usize, switched: &mut Option<usize>, pane_at: &dyn Fn(i32, i32) -> Option<usize>, instances: &[InstanceConn]) -> bool {
            match ev {
                ForwardedInput::Mouse { msg, w, cx, cy, is_wheel, is_button_down } => {
                    if is_button_down
                        && let Some(hit) = pane_at(cx, cy)
                        && hit < instances.len()
                        && !instances[hit].gone
                        && hit != *focus
                    {
                        *focus = hit;
                        *switched = Some(hit);
                        if let Some(Some(s)) = self.shown.get(hit) {
                            notify_active(s.source);
                        }
                    }
                    let Some(Some(s)) = self.shown.get(*focus) else { return false };
                    let src = s.source as HWND;
                    // A click or wheel turn on the bars beside the frames, or on an empty
                    // grid cell, is on nothing of the game's: dropped, not moved onto the
                    // frames' edge.
                    let clicks = is_button_down || is_wheel || matches!(msg, WM_LBUTTONDBLCLK | WM_RBUTTONDBLCLK);
                    if clicks && !on_frames(s.dest, cx, cy) {
                        // The first few, for the log.
                        static TOLD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                        if TOLD.fetch_add(1, Ordering::Relaxed) < 3 {
                            let d = s.dest;
                            self.notes.push((*focus, format!("session: a click at ({cx}, {cy}) beside the frames (drawn at {}x{} at ({}, {})) went to no game", d.width, d.height, d.x, d.y)));
                        }
                        return false;
                    }
                    let dest = s.dest;
                    let uv = s.uv;
                    let Some((gx, gy, sx, sy)) = in_dpi_of(src, || {
                        let (cw, ch) = client_size(src);
                        let (gx, gy) = map_point_to_source_uv(cx, cy, dest, uv, cw, ch)?;
                        let mut screen_pt = POINT { x: gx, y: gy };
                        unsafe { ClientToScreen(src, &mut screen_pt) };
                        Some((gx, gy, screen_pt.x, screen_pt.y))
                    }) else {
                        return false;
                    };
                    let screen_pt = POINT { x: sx, y: sy };
                    if self.cursor_screen.len() <= *focus {
                        self.cursor_screen.resize(*focus + 1, None);
                    }
                    self.cursor_screen[*focus] = Some((screen_pt.x, screen_pt.y));
                    let lp = if is_wheel {
                        ((screen_pt.x as u16 as u32) | ((screen_pt.y as u16 as u32) << 16)) as LPARAM
                    } else {
                        ((gx as u16 as u32) | ((gy as u16 as u32) << 16)) as LPARAM
                    };
                    if let Some(held) = pressed(&ev) {
                        self.press(held, src as isize, lp);
                    }
                    unsafe { PostMessageW(src, msg, w, lp) };
                }
                ForwardedInput::Key { msg, w, l } => {
                    let Some(Some(s)) = self.shown.get(*focus) else { return false };
                    let src = s.source as isize;
                    // This window's message loop already made the key's WM_CHAR, posted
                    // next: the mark keeps the game's TranslateMessage from making it
                    // again (the hook, window.rs `forwarded_key`).
                    let l = if msg == WM_CHAR { l } else { l | peerparty_protocol::session::FORWARDED_KEY };
                    if let Some(held) = pressed(&ev) {
                        self.press(held, src, l);
                    }
                    unsafe { PostMessageW(src as HWND, msg, w, l) };
                }
                ForwardedInput::ReleaseAll => return false,
            }
            true
        }

        /// `held` went down in the game window `hwnd` (with `lp`, for its release).
        fn press(&mut self, held: Held, hwnd: isize, lp: LPARAM) {
            self.pressed.retain(|(h, _, _)| *h != held);
            if self.pressed.len() < 64 {
                self.pressed.push((held, hwnd, lp));
            }
        }

        fn update(&mut self, i: usize, display_pane: PaneRect, viewport: Option<(u32, u32)>, force: bool, log: bool) {
            let dwm = dwm();
            // The frames the window draws itself are fitted by their own size: the game
            // may draw at a size its window and viewport don't say.
            let frames = self.surfaces.get(i).and_then(|(_, s)| s.as_ref()).map(|s| s.size).filter(|s| s.0 > 0 && s.1 > 0);
            let Some(s) = self.shown[i].as_mut() else { return };
            let client = client_size(s.source as HWND);
            let dwm_src = match dwm {
                Some(d) if s.thumbnail != 0 => dwm_source_size(d, s.thumbnail),
                _ => (0, 0),
            };
            let src_size = frames.unwrap_or_else(|| effective_source_size(client, dwm_src, viewport));
            let (dest, uv) = compute_fit(display_pane, src_size.0, src_size.1, s.fit);
            if !force && s.display_pane == display_pane && s.dest == dest && s.uv == uv && s.src_size == src_size {
                return;
            }
            if log && (s.dest != dest || s.uv != uv || s.src_size != src_size) {
                let from = match (frames, viewport) {
                    (Some(_), _) => "its frames".to_string(),
                    (None, Some(v)) if v == src_size => "the game's viewport".to_string(),
                    _ => format!("its window (client {}x{}, thumbnail {}x{})", client.0, client.1, dwm_src.0, dwm_src.1),
                };
                let d = dest;
                self.notes.push((
                    i,
                    format!(
                        "session: pane {}x{} at ({}, {}); {}x{} from {from}, drawn at {}x{} at ({}, {})",
                        display_pane.width, display_pane.height, display_pane.x, display_pane.y, src_size.0, src_size.1, d.width, d.height, d.x, d.y
                    ),
                ));
            }
            let Some(s) = self.shown[i].as_mut() else { return };
            s.display_pane = display_pane;
            s.dest = dest;
            s.uv = uv;
            s.src_size = src_size;
            let (w, h) = (i32::try_from(dest.width).unwrap_or(i32::MAX), i32::try_from(dest.height).unwrap_or(i32::MAX));
            let props = ThumbnailProperties {
                flags: DWM_TNP_RECTDESTINATION | DWM_TNP_OPACITY | DWM_TNP_VISIBLE | DWM_TNP_SOURCECLIENTAREAONLY,
                destination: RECT { left: dest.x, top: dest.y, right: dest.x.saturating_add(w), bottom: dest.y.saturating_add(h) },
                source: RECT { left: 0, top: 0, right: 0, bottom: 0 },
                opacity: 255,
                visible: BOOL::from(dest.width > 0 && dest.height > 0 && !s.drawn),
                client_area_only: 1,
            };
            if let Some(dwm) = dwm.filter(|_| s.thumbnail != 0) {
                unsafe { (dwm.update)(s.thumbnail, &props) };
            }
        }
    }

    impl Drop for HostWindow {
        fn drop(&mut self) {
            if self.confined.is_some() {
                unsafe { ClipCursor(std::ptr::null()) };
            }
            HIDE_POINTER.store(false, Ordering::Relaxed);
            FORCE_HIDE_POINTER.store(false, Ordering::Relaxed);
            // The drawing thread goes before the window it draws in; one that won't stop
            // keeps the window (hidden), rather than presenting to a destroyed one.
            let stopped = self.gpu.as_mut().is_none_or(|g| g.stop());
            self.gpu = None;
            for s in self.shown.drain(..).flatten() {
                if s.thumbnail != 0
                    && let Some(dwm) = dwm()
                {
                    unsafe { (dwm.unregister)(s.thumbnail) };
                }
                // A game still running gets its window back as it was: its frame, its
                // taskbar button, shown (it ends up on its own if the session's window
                // goes first).
                let src = s.source as HWND;
                if unsafe { IsWindow(src) } != 0 && unsafe { IsHungAppWindow(src) } == 0 {
                    unsafe {
                        SetWindowLongW(src, GWL_STYLE, s.style.0 as i32);
                        SetWindowLongW(src, GWL_EXSTYLE, s.style.1 as i32);
                        SetWindowPos(src, std::ptr::null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_ASYNCWINDOWPOS);
                        ShowWindowAsync(src, SW_SHOWNOACTIVATE);
                    }
                }
            }
            unsafe {
                if stopped {
                    DestroyWindow(self.hwnd);
                } else {
                    ShowWindow(self.hwnd, SW_HIDE);
                }
                if !self.icon_big.is_null() {
                    DestroyIcon(self.icon_big);
                }
                if !self.icon_small.is_null() {
                    DestroyIcon(self.icon_small);
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_thumbnail_properties_are_dwms() {
            assert_eq!(std::mem::size_of::<ThumbnailProperties>(), 45);
            assert_eq!(std::mem::offset_of!(ThumbnailProperties, opacity), 36);
            assert_eq!(std::mem::offset_of!(ThumbnailProperties, visible), 37);
            assert_eq!(std::mem::offset_of!(ThumbnailProperties, client_area_only), 41);
            assert!(dwm().is_some(), "dwmapi.dll's thumbnail calls, from System32");
        }

        #[test]
        fn compositor_style_has_zero_border() {
            let mut r = RECT { left: 0, top: 0, right: 3440, bottom: 1440 };
            assert_ne!(unsafe { AdjustWindowRectEx(&mut r, COMPOSITOR_STYLE, 0, WS_EX_APPWINDOW) }, 0);
            assert_eq!((r.left, r.top, r.right, r.bottom), (0, 0, 3440, 1440));
        }

        #[test]
        fn clicks_pack_with_their_sign() {
            for (x, y) in [(0, 0), (1919, 1079), (-5, 7), (32767, -32768)] {
                assert_eq!(unpack(pack(x, y)), Some((x, y)));
            }
            assert_eq!(unpack(NO_CLICK), None);
        }

        /// A release is paired with its press: the same key, the same mouse button.
        #[test]
        fn presses_and_releases_pair_up() {
            let key = |msg, w| ForwardedInput::Key { msg, w, l: 0 };
            let mouse = |msg, w, down| ForwardedInput::Mouse { msg, w, cx: 0, cy: 0, is_wheel: false, is_button_down: down };
            assert_eq!(pressed(&key(WM_KEYDOWN, 0x57)), Some(Held::Key(0x57)));
            assert_eq!(released(&key(WM_KEYUP, 0x57)), Some(Held::Key(0x57)));
            assert_eq!(pressed(&key(WM_SYSKEYDOWN, 0x12)), released(&key(WM_SYSKEYUP, 0x12)));
            assert_eq!(pressed(&key(WM_CHAR, 0x61)), None);
            assert_eq!(released(&key(WM_KEYDOWN, 0x57)), None);
            assert_eq!(pressed(&mouse(WM_LBUTTONDOWN, 0, true)), released(&mouse(WM_LBUTTONUP, 0, false)));
            assert_eq!(pressed(&mouse(WM_RBUTTONDBLCLK, 0, false)), released(&mouse(WM_RBUTTONUP, 0, false)), "a double click presses too");
            assert_ne!(pressed(&mouse(WM_XBUTTONDOWN, 1 << 16, true)), released(&mouse(WM_XBUTTONUP, 2 << 16, false)), "X1 isn't X2");
            assert_eq!(pressed(&mouse(WM_MOUSEMOVE, 0, false)), None);
            assert_eq!(released(&mouse(WM_MOUSEMOVE, 0, false)), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_protocol::ipc::HostMsg;
    use peerparty_protocol::session::{InstanceId, InstanceSpec};

    fn r(x: i32, y: i32, width: u32, height: u32) -> PaneRect {
        PaneRect { x, y, width, height }
    }

    const HD: MonitorBounds = MonitorBounds { x: 0, y: 0, width: 1920, height: 1080, dpi: 96 };

    /// `--monitor`'s numbers: the main monitor 1, the rest left to right, then top down.
    #[test]
    fn monitors_numbered_main_first_then_left_to_right() {
        let at = |x: i32, y: i32| MonitorBounds { x, y, ..HD };
        let found = vec![(false, at(1920, 0)), (false, at(-1920, 0)), (true, at(0, 0)), (false, at(0, -1080))];
        let v = in_order(found);
        assert_eq!(v.iter().map(|b| (b.x, b.y)).collect::<Vec<_>>(), [(0, 0), (-1920, 0), (0, -1080), (1920, 0)]);
        assert!(!MonitorBounds::all().is_empty(), "always one, the main one at least");
        let (main, why) = MonitorBounds::numbered(Some(99));
        assert_eq!(main, MonitorBounds::all()[0]);
        assert!(why.is_some_and(|w| w.contains("monitor 99")));
        assert_eq!(MonitorBounds::numbered(None), (MonitorBounds::all()[0], None));
    }

    #[test]
    fn layouts_switch_live() {
        let mut c = CompositorWindow::new(LayoutPreset::Horizontal, HD, 2);
        assert_eq!(c.panes(), [r(0, 0, 960, 1080), r(960, 0, 960, 1080)]);
        c.set_layout(LayoutPreset::Vertical);
        assert_eq!(c.panes(), [r(0, 0, 1920, 540), r(0, 540, 1920, 540)]);
        assert_eq!(c.cycle_layout(), &LayoutPreset::Grid2x2);
        assert_eq!(c.panes(), [r(0, 0, 960, 540), r(960, 0, 960, 540)]);
        assert_eq!(c.cycle_layout(), &LayoutPreset::Horizontal, "round again");
        assert_eq!(c.cycle_layout(), &LayoutPreset::Vertical);
        c.set_layout(LayoutPreset::Custom(vec![r(0, 0, 1280, 1080), r(1280, 0, 640, 360)]));
        assert_eq!(c.panes()[1], r(1280, 0, 640, 360));
        assert_eq!(c.cycle_layout(), &LayoutPreset::Horizontal, "a custom layout goes on to side by side");
        c.set_layout(LayoutPreset::Single);
        assert_eq!(c.panes(), [r(0, 0, 1920, 1080); 2]);
        c.set_count(3);
        c.set_layout(LayoutPreset::TopTwo);
        assert_eq!(c.panes(), [r(0, 0, 960, 540), r(960, 0, 960, 540), r(0, 540, 1920, 540)]);
        assert_eq!(c.cycle_layout(), &LayoutPreset::BottomTwo);
        assert_eq!(c.panes(), [r(0, 0, 1920, 540), r(0, 540, 960, 540), r(960, 540, 960, 540)]);
        assert_eq!(c.cycle_layout(), &LayoutPreset::Horizontal);
        c.set_count(4);
        c.set_layout(LayoutPreset::Grid2x2);
        assert_eq!(c.panes().len(), 4);
        assert_eq!(c.panes()[3], r(960, 540, 960, 540));
    }

    /// The focused pane: chosen, kept within the panes, zoomed onto the whole window, and
    /// told the whole window while zoomed (the others keep their panes).
    #[test]
    fn focus_and_zoom() {
        let mut c = CompositorWindow::new(LayoutPreset::Grid2x2, HD, 4);
        assert_eq!(c.focused(), 0);
        assert!(c.focus_pane(3));
        assert!(!c.focus_pane(4), "no fifth pane");
        assert_eq!(c.focused(), 3);
        assert_eq!(c.pane_at(1000, 600), Some(3));
        assert_eq!(c.pane_at(10, 10), Some(0));
        assert_eq!(c.pane_at(1920, 10), None, "right of the window");
        c.set_zoomed(true);
        assert!(c.zoomed());
        assert_eq!(c.display_panes(), [HIDDEN, HIDDEN, HIDDEN, r(0, 0, 1920, 1080)]);
        assert_eq!(c.panes()[3], r(960, 540, 960, 540), "the layout stays");
        assert_eq!(c.told_panes()[3], r(0, 0, 1920, 1080), "the zoomed game is told the whole window");
        assert_eq!(c.told_panes()[0], c.panes()[0], "the others keep theirs");
        assert_eq!(c.pane_at(10, 10), Some(3), "zoomed, the whole window is its");
        c.set_zoomed(false);
        assert_eq!(c.display_panes(), c.panes());
        assert_eq!(c.told_panes(), c.panes());
        c.set_count(2);
        assert_eq!(c.focused(), 1, "the focus stays on a pane there is");
    }

    /// Panes are physical pixels: a 4K monitor at 200% has twice the 1080p one's, and
    /// a monitor left of the main one puts them at negative x on screen.
    #[test]
    fn panes_follow_the_monitor_and_its_scale() {
        let uhd = MonitorBounds { x: -3840, y: 0, width: 3840, height: 2160, dpi: 192 };
        assert_eq!((uhd.scale(), uhd.logical_size()), (2.0, (1920, 1080)));
        let qhd = MonitorBounds { x: 0, y: 0, width: 2560, height: 1440, dpi: 144 };
        assert_eq!((qhd.scale(), qhd.logical_size()), (1.5, (1707, 960)));
        assert_eq!(HD.scale(), 1.0);
        let mut c = CompositorWindow::new(LayoutPreset::Horizontal, HD, 2);
        c.set_bounds(uhd);
        assert_eq!(c.panes(), [r(0, 0, 1920, 2160), r(1920, 0, 1920, 2160)]);
        assert_eq!(c.screen_pane(1), Some(r(-1920, 0, 1920, 2160)));
        assert_eq!(c.screen_pane(2), None);
        c.set_bounds(qhd);
        c.set_layout(LayoutPreset::Grid2x2);
        assert_eq!(c.panes()[1], r(1280, 0, 1280, 720));
        assert!(!c.tick(), "no window, nothing of its own changed");
        assert_eq!(c.hwnd(), 0);
    }

    /// Each copy is told its pane and a virtual screen its size, once; a layout change
    /// tells them again, and a copy that ended isn't sent anything.
    #[test]
    fn copies_are_told_their_panes() {
        let mut instances: Vec<InstanceConn> = (0..2).map(|i| InstanceConn::detached(InstanceSpec::solo(InstanceId(i), i))).collect();
        let mut c = CompositorWindow::new(LayoutPreset::Horizontal, MonitorBounds { x: 100, y: 0, width: 1920, height: 1080, dpi: 120 }, 2);
        assert_eq!(c.sync_instances(&mut instances), 2);
        let s = &instances[1].spec;
        assert_eq!((s.pane, s.virtual_width, s.virtual_height, s.dpi, s.focus_spoof), (Some(r(1060, 0, 960, 1080)), Some(960), Some(1080), Some(120), true));
        assert_eq!(c.sync_instances(&mut instances), 0, "nothing changed");
        c.set_layout(LayoutPreset::Vertical);
        instances[0].end();
        assert_eq!(c.sync_instances(&mut instances), 1, "the copy still running");
        assert_eq!(instances[0].spec.pane, Some(r(100, 0, 1920, 540)), "the ended one's spec follows anyway");
        assert_eq!(HostMsg::Instance(instances[1].spec.clone()).to_json().get("spec").get("virtual_height").as_u64(), Some(540));
        // A copy alone has no one to hide its focus from.
        let mut one = vec![InstanceConn::detached(InstanceSpec::solo(InstanceId(0), 0))];
        let mut c = CompositorWindow::new(LayoutPreset::Single, HD, 1);
        c.sync_instances(&mut one);
        assert!(!one[0].spec.focus_spoof);
        assert_eq!(one[0].spec.pane, Some(r(0, 0, 1920, 1080)));
        // Instances added: a pane each.
        one.push(InstanceConn::detached(InstanceSpec::solo(InstanceId(1), 1)));
        c.set_layout(LayoutPreset::Horizontal);
        c.sync_instances(&mut one);
        assert_eq!(c.panes().len(), 2);
    }

    #[test]
    fn thumbnails_preserve_aspect_without_stretching() {
        // Matching aspect fills edge-to-edge.
        let half = r(0, 0, 1720, 1440);
        assert_eq!(fit_aspect(half, 1720, 1440), half);
        // 16:9 inside a tall side-by-side pane: letterboxed vertically, centered.
        let fitted = fit_aspect(r(0, 0, 960, 1080), 1920, 1080);
        assert_eq!(fitted, r(0, 270, 960, 540));
        // 16:9 inside a super-wide top/bottom ultrawide pane (3440x720): pillarboxed horizontally, centered.
        let fitted = fit_aspect(r(0, 720, 3440, 720), 1920, 1080);
        assert_eq!(fitted, r(1080, 720, 1280, 720));
        // Unknown source size falls back to filling the pane.
        assert_eq!(fit_aspect(half, 0, 0), half);
        // Effective source size prefers reported backbuffer viewport, then client, then DWM.
        assert_eq!(effective_source_size((1720, 1440), (1720, 1440), Some((1280, 720))), (1280, 720));
        assert_eq!(effective_source_size((1698, 1097), (1720, 1440), None), (1698, 1097));
        assert_eq!(effective_source_size((0, 0), (1280, 720), None), (1280, 720));
        // Point mapping maps compositor client coords inside a letterboxed thumbnail back to game client coords.
        let letterboxed = r(1720, 236, 1720, 968);
        assert_eq!(map_point_to_source(1720, 236, letterboxed, 1280, 720), Some((0, 0)));
        assert_eq!(map_point_to_source(2580, 720, letterboxed, 1280, 720), Some((640, 360)));
        assert_eq!(map_point_to_source(1720, 0, letterboxed, 1280, 720), Some((0, 0)), "clamped from top bar");

        // Fill mode crops evenly without distortion, and inverse UV mapping lands in the visible middle.
        let (dest, uv) = compute_fit(r(0, 0, 960, 1080), 1920, 1080, PaneFit::Fill);
        assert_eq!(dest, r(0, 0, 960, 1080));
        assert!((uv[0] - 0.25).abs() < 0.001 && (uv[2] - 0.75).abs() < 0.001);
        assert_eq!(uv[1], 0.0);
        assert_eq!(uv[3], 1.0);
        assert_eq!(map_point_to_source_uv(0, 0, dest, uv, 1920, 1080), Some((480, 0)));
        assert_eq!(map_point_to_source_uv(480, 540, dest, uv, 1920, 1080), Some((960, 540)));

        // Z-ordered hit-testing: PiP inset on top of full-screen pane wins hits inside it.
        let mut pip = CompositorWindow::new(LayoutPreset::Slots(LayoutPreset::pip_slots(2)), HD, 2);
        assert_eq!(pip.pane_at(100, 100), Some(0));
        assert_eq!(pip.pane_at(1600, 900), Some(1), "PiP pane on top wins click");
        // Virtual aspect override changes what virtual size the copy is told.
        let mut slots = pip.slots().to_vec();
        slots[0].aspect = peerparty_protocol::session::VirtualAspect::Ratio { num: 4, den: 3 };
        pip.set_layout(LayoutPreset::Slots(slots));
        assert_eq!(pip.virtual_size(0), Some((1440, 1080)));
    }
}
