//! Live Compositor Layout Studio ("Edit Mode"): a translucent interactive canvas drawn
//! directly over the live running split-screen game frames.
//!
//! Players drag shared dividers to rebalance panes (e.g. 50/50 -> 65/35), grab corners
//! or edges for free-form resizing and Picture-in-Picture, drag one pane onto another to
//! swap players, cycle per-pane Fit/Fill/Stretch and virtual aspect ratios (Match, 16:9,
//! 16:10, 4:3, 21:9), mute individual copies, expand a player to full screen, and save
//! their custom layout for the game while watching the live game HUDs underneath update
//! in real time. Full controller support with live button prompts is built in alongside
//! mouse and keyboard editing.

use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Stroke, Vec2, pos2, vec2};
use peerparty_protocol::session::{LayoutPreset, NORM_MIN, NORM_SCALE, NormRect, PaneFit, PaneSlot, VirtualAspect};

use super::desktop::{self, Rect as ScreenRect};
use super::overlay_render::{FRAME_GAP, scale_for};
use super::{input, pad, render, window};
use crate::t;
use crate::ui::icons::{self, Glyph};
use crate::ui::theme::{self, ACCENT, ACCENT_HI, GOOD, TEXT, TEXT2, WARN};

const BAR_BG: Color32 = Color32::from_rgba_premultiplied(16, 18, 25, 232);
const PILL_BG: Color32 = Color32::from_rgba_premultiplied(16, 18, 25, 220);
const BTN_BG: Color32 = Color32::from_rgb(32, 36, 48);
const BTN_HOV: Color32 = Color32::from_rgb(46, 52, 68);
const BTN_ACTIVE: Color32 = Color32::from_rgb(52, 88, 172);
const BORDER: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 34);

/// Distinct accent color per player slot (P1..P4).
const PLAYER_COLORS: [Color32; 4] = [
    Color32::from_rgb(82, 146, 255),  // P1: blue
    Color32::from_rgb(52, 211, 153),  // P2: emerald
    Color32::from_rgb(251, 191, 36),  // P3: amber
    Color32::from_rgb(167, 139, 250), // P4: violet
];

/// Magnetic snap stops in `0..=NORM_SCALE` (edges, quarters, thirds, half).
const SNAP_STOPS: [u16; 7] = [0, 2_500, 3_333, 5_000, 6_667, 7_500, NORM_SCALE];
/// Magnetic snap radius (1.8% of screen dimension).
const SNAP_RADIUS: u16 = 180;
/// Tolerance for considering two pane edges shared along a divider.
const SEAM_TOL: u16 = 80;
/// Controller resize step (5% of screen normally, 1% when holding LT for fine control).
const PAD_STEP_COARSE: i32 = 500;
const PAD_STEP_FINE: i32 = 100;

/// Per-player metadata shown in Layout Studio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerInfo {
    pub name: String,
    pub muted: bool,
    pub starting: bool,
    pub gone: bool,
}

/// Snapshot of session state passed into Layout Studio each frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StudioInfo {
    pub players: Vec<PlayerInfo>,
    pub screen: (u32, u32),
    pub keyboard: Option<usize>,
    pub zoomed: Option<usize>,
    pub sync: bool,
}

/// Combined controller state passed into Layout Studio each frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StudioPad {
    pub buttons: u32,
    pub lt: bool,
    pub rt: bool,
    pub rx: f32,
    pub ry: f32,
    pub style: pad::Style,
    pub connected: bool,
}

/// Actions emitted by Layout Studio back to `SessionHost`.
#[derive(Clone, Debug, PartialEq)]
pub enum StudioAction {
    /// Visual-only update during an active drag (no `HostMsg::Instance` spam).
    Preview(Vec<PaneSlot>),
    /// Commit layout to the game copies (`HostMsg::Instance` + window resize).
    Commit(Vec<PaneSlot>),
    /// Apply a built-in preset (updates layout and commits).
    Preset(LayoutPreset),
    /// Toggle WASAPI mute for copy `i`.
    ToggleMute(usize),
    /// Toggle full-window view ("Full Screen") for copy `i`.
    ToggleZoom(usize),
    /// Cycle keyboard/mouse target (+1/-1 across players + Off).
    KeyboardStep(i32),
    /// Turn compositor-locked frame pacing ("Sync games to screen") on or off live.
    SetSync(bool),
    /// Persist custom slots for this game in `ui.json`.
    Save(Vec<PaneSlot>),
    /// Persist frame-sync preference for this game in `ui.json`.
    SaveSync(bool),
    /// Exit Layout Studio.
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum PadMode {
    /// Selecting panes and triggering quick actions on the selected pane / toolbar.
    #[default]
    Browse,
    /// Resizing, moving, or swapping the selected pane with the controller.
    Adjust,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    /// Vertical line (slides left/right, edits `x` / `w`).
    Vertical,
    /// Horizontal line (slides up/down, edits `y` / `h`).
    Horizontal,
}

/// A shared interior seam between two groups of adjacent panes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SharedDivider {
    axis: Axis,
    /// Seam coordinate in `0..=NORM_SCALE`.
    pos: u16,
    /// Span along the perpendicular axis `(min, max)` in `0..=NORM_SCALE`.
    span: (u16, u16),
    /// Panes whose `right()` (or `bottom()`) sits on `pos`.
    neg: Vec<usize>,
    /// Panes whose `x` (or `y`) sits on `pos`.
    pos_panes: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Edges {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

impl Edges {
    fn any(self) -> bool {
        self.left || self.right || self.top || self.bottom
    }
}

#[derive(Clone, Debug)]
enum DragMode {
    Divider {
        divider: SharedDivider,
        start_slots: Vec<PaneSlot>,
    },
    Resize {
        pane: usize,
        edges: Edges,
        start_rect: NormRect,
        anchor: Pos2,
    },
    Move {
        pane: usize,
        start_rect: NormRect,
        anchor: Pos2,
        swap_target: Option<usize>,
    },
}

pub struct LayoutStudio {
    pub active: bool,
    pub slots: Vec<PaneSlot>,
    /// Initial slots and sync setting when Studio opened (for Reset and Done auto-save).
    initial: Vec<PaneSlot>,
    initial_sync: bool,
    selected: usize,
    snap: bool,
    drag: Option<DragMode>,
    /// Pointer position in points.
    cursor: Pos2,
    pointer_down: bool,
    alt_down: bool,
    esc_prev: bool,
    pad_prev: u32,
    lt_prev: bool,
    rt_prev: bool,
    pad_mode: PadMode,
    /// Whether controller prompts are currently shown (switches live between pad and KBM).
    using_pad: bool,
    pad_style: pad::Style,
    pad_connected: bool,
    pad_moving: bool,
    last_move_tick: Instant,
    repeat: crate::ui::shared::RepeatTimer,
    ctx: egui::Context,
    render: render::Renderer,
    window: Option<window::Layered>,
    fonts: bool,
    ppp: f32,
    last_info: Option<StudioInfo>,
    pub last_frame: Instant,
    pub dirty: bool,
}

impl LayoutStudio {
    pub fn new() -> LayoutStudio {
        let now = Instant::now();
        LayoutStudio {
            active: false,
            slots: Vec::new(),
            initial: Vec::new(),
            initial_sync: true,
            selected: 0,
            snap: true,
            drag: None,
            cursor: Pos2::new(640.0, 360.0),
            pointer_down: false,
            alt_down: false,
            esc_prev: false,
            pad_prev: 0,
            lt_prev: false,
            rt_prev: false,
            pad_mode: PadMode::Browse,
            using_pad: false,
            pad_style: pad::Style::Xbox,
            pad_connected: false,
            pad_moving: false,
            last_move_tick: now,
            repeat: crate::ui::shared::RepeatTimer::default(),
            ctx: egui::Context::default(),
            render: render::Renderer::default(),
            window: None,
            fonts: false,
            ppp: 1.0,
            last_info: None,
            last_frame: now,
            dirty: true,
        }
    }

    /// Opens Layout Studio over the live session with `slots`, initial `focus` pane,
    /// current `sync` state, and initial controller snapshot `pad`.
    pub fn open(&mut self, slots: Vec<PaneSlot>, focus: usize, sync: bool, bounds: ScreenRect, overlay_scale: f32, pad: StudioPad) {
        self.active = true;
        self.slots = slots.clone();
        self.initial = slots;
        self.initial_sync = sync;
        self.selected = focus.min(self.slots.len().saturating_sub(1));
        self.drag = None;
        self.pointer_down = false;
        self.alt_down = input::alt_down() || desktop::key_down(0x12);
        self.esc_prev = desktop::key_down(0x1B);
        self.pad_prev = pad.buttons;
        self.lt_prev = pad.lt;
        self.rt_prev = pad.rt;
        self.pad_mode = PadMode::Browse;
        self.using_pad = pad.connected;
        self.pad_style = pad.style;
        self.pad_connected = pad.connected;
        self.pad_moving = false;
        self.last_move_tick = Instant::now();
        self.last_info = None;
        self.repeat.reset();
        let w = (bounds.right - bounds.left).max(1) as usize;
        let h = (bounds.bottom - bounds.top).max(1) as usize;
        let ppp = scale_for(h as f32, overlay_scale);
        self.fit(w, h, ppp);
        self.cursor = pos2(w as f32 / 2.0 / ppp, h as f32 / 2.0 / ppp);
        self.dirty = true;
        self.render.invalidate();
    }

    /// Closes Layout Studio and hides its layered window.
    pub fn close(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        self.drag = None;
        self.pointer_down = false;
        self.pad_mode = PadMode::Browse;
        self.pad_moving = false;
        if let Some(w) = self.window.as_mut() {
            w.hide();
        }
    }

    /// Updates `slots` externally (e.g. when a preset is applied).
    pub fn set_slots(&mut self, slots: Vec<PaneSlot>) {
        self.slots = slots;
        self.selected = self.selected.min(self.slots.len().saturating_sub(1));
        self.drag = None;
        self.dirty = true;
    }

    fn ensure_fonts(&mut self) {
        if !self.fonts {
            self.fonts = true;
            theme::install_fonts(&self.ctx);
        }
    }

    fn fit(&mut self, w: usize, h: usize, ppp: f32) {
        if (ppp - self.ppp).abs() > 0.001 || self.render.canvas.w != w || self.render.canvas.h != h {
            self.ppp = ppp;
            self.render.resize(w, h);
            self.render.invalidate();
            self.dirty = true;
        }
    }

    fn screen_points(&self) -> Vec2 {
        vec2(self.render.canvas.w as f32 / self.ppp, self.render.canvas.h as f32 / self.ppp)
    }

    /// Runs one interactive tick of Layout Studio: reads captured keyboard/mouse and
    /// controller input, updates drags/buttons, redraws the translucent layered canvas,
    /// and returns actions for `SessionHost`.
    pub fn tick(
        &mut self,
        bounds: ScreenRect,
        overlay_scale: f32,
        time: f64,
        pad: StudioPad,
        info: &StudioInfo,
    ) -> Vec<StudioAction> {
        if !self.active {
            return Vec::new();
        }
        if input::pending() && self.last_frame.elapsed() < FRAME_GAP {
            return Vec::new();
        }
        self.ensure_fonts();
        let w = (bounds.right - bounds.left).max(1) as usize;
        let h = (bounds.bottom - bounds.top).max(1) as usize;
        let ppp = scale_for(h as f32, overlay_scale);
        self.fit(w, h, ppp);
        let screen = self.screen_points();

        let mut actions = Vec::new();
        let mut events = Vec::new();
        let mut moved = false;
        let mut pressed_now = false;
        let mut released_now = false;
        let mut close_asked = false;

        // Alt is passed through by the low-level keyboard hook (so Alt+Tab still works),
        // so read its live state directly rather than waiting for an Input::Key event.
        let alt_now = input::alt_down() || desktop::key_down(0x12);
        if alt_now != self.alt_down {
            self.alt_down = alt_now;
            self.dirty = true;
        }
        // Poll Esc rising edge alongside the hook queue so Esc always closes Layout Studio
        // even if another low-level keyboard hook ahead of ours handled or remapped it.
        let esc_now = desktop::key_down(0x1B);
        if esc_now && !self.esc_prev {
            close_asked = true;
        }
        self.esc_prev = esc_now;

        for ev in input::take() {
            match ev {
                input::Input::Key { vk, down } => {
                    if !down {
                        continue;
                    }
                    match vk {
                        0x1B => close_asked = true, // Esc
                        0x25 => self.select_dir(-1, 0),
                        0x26 => self.select_dir(0, -1),
                        0x27 => self.select_dir(1, 0),
                        0x28 => self.select_dir(0, 1),
                        _ => {}
                    }
                }
                input::Input::Move { dx, dy } => {
                    self.cursor.x = (self.cursor.x + dx as f32 / ppp).clamp(0.0, (screen.x - 1.0).max(0.0));
                    self.cursor.y = (self.cursor.y + dy as f32 / ppp).clamp(0.0, (screen.y - 1.0).max(0.0));
                    moved = true;
                    if self.using_pad {
                        self.leave_pad_mode(&mut actions);
                    }
                }
                input::Input::Button { left: true, down } => {
                    if self.using_pad {
                        self.leave_pad_mode(&mut actions);
                    }
                    if down && !self.pointer_down {
                        pressed_now = true;
                    } else if !down && self.pointer_down {
                        released_now = true;
                    }
                    self.pointer_down = down;
                    events.push(egui::Event::PointerButton {
                        pos: self.cursor,
                        button: egui::PointerButton::Primary,
                        pressed: down,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                input::Input::Button { left: false, down } => {
                    if self.using_pad {
                        self.leave_pad_mode(&mut actions);
                    }
                    events.push(egui::Event::PointerButton {
                        pos: self.cursor,
                        button: egui::PointerButton::Secondary,
                        pressed: down,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                input::Input::Wheel(_) => {}
            }
        }
        if moved || pressed_now || released_now {
            events.insert(0, egui::Event::PointerMoved(self.cursor));
            self.dirty = true;
        }

        if self.handle_pad(pad, info, &mut actions) {
            close_asked = true;
        }

        if close_asked {
            if self.pad_moving {
                self.pad_moving = false;
                actions.push(StudioAction::Commit(self.slots.clone()));
            }
            if self.slots != self.initial {
                actions.push(StudioAction::Save(self.slots.clone()));
            }
            if info.sync != self.initial_sync {
                actions.push(StudioAction::SaveSync(info.sync));
            }
            actions.push(StudioAction::Close);
            return actions;
        }

        if self.last_info.as_ref() != Some(info) {
            self.dirty = true;
        }

        let want_pointer = !self.using_pad;
        let pointer_changed = moved || want_pointer != self.render.pointer_shown();
        if !self.dirty && !pointer_changed {
            return actions;
        }

        self.last_frame = Instant::now();
        let mut rects: Vec<[i32; 4]> = Vec::new();
        self.render.pointer_off(&mut rects);

        if self.dirty {
            self.dirty = false;
            self.last_info = Some(info.clone());
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, screen)),
                time: Some(time),
                events,
                ..Default::default()
            };
            let ctx = self.ctx.clone();
            ctx.set_pixels_per_point(self.ppp);
            let out = ctx.run_ui(raw, |ui| {
                self.draw_ui(ui, screen, info, pressed_now, released_now, &mut actions);
            });
            if out.viewport_output.values().any(|v| v.repaint_delay < Duration::from_millis(100)) {
                self.dirty = true;
            }
            self.render.draw(&ctx, out, |_| Vec::new(), &mut rects);
        }

        if want_pointer {
            self.render.pointer_on(&self.ctx, self.cursor, self.ppp, &mut rects);
        }
        if !rects.is_empty() {
            if self.window.is_none() {
                self.window = window::Layered::new();
            }
            if let Some(win) = self.window.as_mut() {
                win.present(&self.render.canvas, bounds.left, bounds.top, &rects);
            }
        }
        actions
    }

    fn leave_pad_mode(&mut self, actions: &mut Vec<StudioAction>) {
        self.using_pad = false;
        if self.pad_mode != PadMode::Browse {
            self.pad_mode = PadMode::Browse;
            if self.pad_moving {
                self.pad_moving = false;
                actions.push(StudioAction::Commit(self.slots.clone()));
            }
        }
        self.dirty = true;
    }

    fn select_dir(&mut self, dx: i32, dy: i32) {
        if let Some(next) = select_pane_dir(&self.slots, self.selected, dx, dy)
            && next != self.selected
        {
            self.selected = next;
            self.dirty = true;
        }
    }

    /// Processes controller buttons, sticks and triggers for Layout Studio.
    /// Returns `true` if the controller asked to save and close Studio.
    fn handle_pad(&mut self, pad: StudioPad, info: &StudioInfo, actions: &mut Vec<StudioAction>) -> bool {
        let now = Instant::now();
        let dt = now.saturating_duration_since(self.last_move_tick).as_secs_f32().clamp(0.0, 0.05);
        self.last_move_tick = now;

        if pad.style != self.pad_style || pad.connected != self.pad_connected {
            self.pad_style = pad.style;
            self.pad_connected = pad.connected;
            self.dirty = true;
        }

        let pressed_pad = pad.buttons & !self.pad_prev;
        self.pad_prev = pad.buttons;
        let lt_pressed = pad.lt && !self.lt_prev;
        self.lt_prev = pad.lt;
        let rt_pressed = pad.rt && !self.rt_prev;
        self.rt_prev = pad.rt;

        let dirs = pad.buttons & (pad::UP | pad::DOWN | pad::LEFT | pad::RIGHT);
        let fire = pressed_pad | self.repeat.step(dirs, now);
        let stick_active = pad.rx != 0.0 || pad.ry != 0.0;
        if (pressed_pad != 0 || fire != 0 || lt_pressed || rt_pressed || stick_active) && !self.using_pad {
            self.using_pad = true;
            self.drag = None;
            self.dirty = true;
        }

        if pressed_pad & pad::START != 0 {
            return true;
        }

        match self.pad_mode {
            PadMode::Browse => {
                if pressed_pad & pad::EAST != 0 {
                    return true;
                }
                if !self.slots.is_empty() {
                    if fire & pad::LEFT != 0 {
                        self.select_dir(-1, 0);
                    } else if fire & pad::RIGHT != 0 {
                        self.select_dir(1, 0);
                    } else if fire & pad::UP != 0 {
                        self.select_dir(0, -1);
                    } else if fire & pad::DOWN != 0 {
                        self.select_dir(0, 1);
                    }
                    if pressed_pad & pad::SOUTH != 0 {
                        self.pad_mode = PadMode::Adjust;
                        self.pad_moving = false;
                        self.dirty = true;
                    }
                    if pressed_pad & pad::NORTH != 0 {
                        let i = self.selected;
                        self.slots[i].fit = self.slots[i].fit.next();
                        actions.push(StudioAction::Commit(self.slots.clone()));
                        self.dirty = true;
                    }
                    if pressed_pad & pad::WEST != 0 {
                        let i = self.selected;
                        self.slots[i].aspect = self.slots[i].aspect.next();
                        actions.push(StudioAction::Commit(self.slots.clone()));
                        self.dirty = true;
                    }
                    if lt_pressed {
                        actions.push(StudioAction::ToggleMute(self.selected));
                        self.dirty = true;
                    }
                    if rt_pressed {
                        actions.push(StudioAction::ToggleZoom(self.selected));
                        self.dirty = true;
                    }
                    if pressed_pad & (pad::LB | pad::RB) != 0 {
                        let presets = preset_list(self.slots.len());
                        let cur_slots = self.slots.clone();
                        let cur_idx = presets.iter().position(|(_, p, _)| p.to_slots(self.slots.len()) == cur_slots).unwrap_or(0) as i32;
                        let step = if pressed_pad & pad::RB != 0 { 1 } else { -1 };
                        let next = presets[(cur_idx + step).rem_euclid(presets.len() as i32) as usize].1.clone();
                        self.slots = next.to_slots(self.slots.len());
                        actions.push(StudioAction::Preset(next));
                        self.dirty = true;
                    }
                    if pressed_pad & pad::BACK != 0 {
                        actions.push(StudioAction::SetSync(!info.sync));
                        self.dirty = true;
                    }
                    if pressed_pad & pad::RSTICK != 0 {
                        let layout_changed = self.slots != self.initial;
                        let sync_changed = info.sync != self.initial_sync;
                        if layout_changed {
                            self.slots = self.initial.clone();
                            self.drag = None;
                            actions.push(StudioAction::Commit(self.slots.clone()));
                        }
                        if sync_changed {
                            actions.push(StudioAction::SetSync(self.initial_sync));
                        }
                        if layout_changed || sync_changed {
                            self.dirty = true;
                        }
                    }
                }
            }
            PadMode::Adjust => {
                if pressed_pad & (pad::SOUTH | pad::EAST) != 0 {
                    if self.pad_moving {
                        self.pad_moving = false;
                        actions.push(StudioAction::Commit(self.slots.clone()));
                    }
                    self.pad_mode = PadMode::Browse;
                    self.dirty = true;
                    return false;
                }
                if !self.slots.is_empty() {
                    let fine = pad.lt;
                    let dx = if fire & pad::RIGHT != 0 { 1 } else if fire & pad::LEFT != 0 { -1 } else { 0 };
                    let dy = if fire & pad::DOWN != 0 { 1 } else if fire & pad::UP != 0 { -1 } else { 0 };
                    if (dx != 0 || dy != 0) && resize_pane_step(&mut self.slots, self.selected, dx, dy, self.snap && !fine, fine) {
                        actions.push(StudioAction::Commit(self.slots.clone()));
                        self.dirty = true;
                    }
                    if stick_active {
                        let speed = if fine { 1_500.0 } else { 4_500.0 };
                        let mx = (pad.rx * speed * dt).round() as i32;
                        let my = (-pad.ry * speed * dt).round() as i32;
                        if mx != 0 || my != 0 {
                            let i = self.selected;
                            bring_to_front(&mut self.slots, i);
                            let cur = self.slots[i].rect;
                            let next = move_rect_by(&self.slots, i, cur, mx, my, self.snap && !fine);
                            if next != cur {
                                self.slots[i].rect = next;
                                self.pad_moving = true;
                                actions.push(StudioAction::Preview(self.slots.clone()));
                                self.dirty = true;
                            }
                        }
                    } else if self.pad_moving {
                        self.pad_moving = false;
                        actions.push(StudioAction::Commit(self.slots.clone()));
                        self.dirty = true;
                    }
                    if pressed_pad & pad::WEST != 0 && self.slots.len() > 1 {
                        swap_next_visual(&mut self.slots, self.selected);
                        actions.push(StudioAction::Commit(self.slots.clone()));
                        self.dirty = true;
                    }
                    if pressed_pad & pad::NORTH != 0 {
                        self.snap = !self.snap;
                        self.dirty = true;
                    }
                }
            }
        }
        false
    }

    fn draw_ui(
        &mut self,
        ui: &mut egui::Ui,
        screen: Vec2,
        info: &StudioInfo,
        pressed_now: bool,
        released_now: bool,
        actions: &mut Vec<StudioAction>,
    ) {
        let count = self.slots.len();
        let snap_on = self.snap && !self.alt_down;
        let dividers = find_shared_dividers(&self.slots);
        let style = self.pad_style;
        let pad_browse = self.using_pad && self.pad_mode == PadMode::Browse;
        let pad_adjust = self.using_pad && self.pad_mode == PadMode::Adjust;

        // Track interactive button rects so direct pane drags don't steal button clicks.
        let mut ui_blocked: Vec<Rect> = Vec::new();

        // 1. Top floating toolbar.
        let bar_h = 42.0;
        let bar_y = 14.0;
        let top_area = egui::Area::new(egui::Id::unique("studio-toolbar"))
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_TOP, vec2(0.0, bar_y))
            .show(ui.ctx(), |ui| {
                egui::Frame::new()
                    .fill(BAR_BG)
                    .stroke(Stroke::new(1.0, BORDER))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(egui::Margin::symmetric(12, 6))
                    .show(ui, |ui| {
                        ui.set_min_height(bar_h - 12.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.label(egui::RichText::new(t!("studio.title")).font(theme::strong(13.5)).color(TEXT));
                            ui.separator();
                            if pad_browse {
                                inline_glyph(ui, Glyph::Lb, style);
                            }
                            for (label, preset, tip) in preset_list(count) {
                                let is_cur = preset.to_slots(count) == self.slots;
                                if pill_button(ui, label, is_cur, false, None, style, tip) {
                                    self.slots = preset.to_slots(count);
                                    self.drag = None;
                                    actions.push(StudioAction::Preset(preset));
                                    self.dirty = true;
                                }
                            }
                            if pad_browse {
                                inline_glyph(ui, Glyph::Rb, style);
                            }
                            ui.separator();
                            let kbm_label = match info.keyboard {
                                Some(k) => format!("KBM: P{}", k + 1),
                                None => format!("KBM: {}", t!("common.off")),
                            };
                            if pill_button(ui, &kbm_label, info.keyboard.is_some(), false, None, style, Some(t!("studio.tip_kbm"))) {
                                actions.push(StudioAction::KeyboardStep(1));
                                self.dirty = true;
                            }
                            let sync_label = if info.sync { t!("studio.sync_on") } else { t!("studio.sync_off") };
                            let sync_glyph = pad_browse.then_some(Glyph::Back);
                            if pill_button(ui, sync_label, info.sync, false, sync_glyph, style, Some(t!("studio.tip_sync"))) {
                                actions.push(StudioAction::SetSync(!info.sync));
                                self.dirty = true;
                            }
                            let alt_bypass = self.snap && self.alt_down;
                            let snap_label = if alt_bypass { t!("studio.snap_alt_off") } else { t!("studio.snap") };
                            let snap_glyph = pad_adjust.then_some(Glyph::North);
                            if pill_button_warn(ui, snap_label, self.snap && !alt_bypass, alt_bypass, snap_glyph, style, Some(t!("studio.tip_snap"))) {
                                self.snap = !self.snap;
                                self.dirty = true;
                            }
                            ui.separator();
                            let layout_changed = self.slots != self.initial;
                            let sync_changed = info.sync != self.initial_sync;
                            if (layout_changed || sync_changed)
                                && pill_button(ui, t!("studio.reset"), false, false, pad_browse.then_some(Glyph::RStick), style, Some(t!("studio.tip_reset")))
                            {
                                if layout_changed {
                                    self.slots = self.initial.clone();
                                    self.drag = None;
                                    actions.push(StudioAction::Commit(self.slots.clone()));
                                }
                                if sync_changed {
                                    actions.push(StudioAction::SetSync(self.initial_sync));
                                }
                                self.dirty = true;
                            }
                            let done_glyph = if pad_browse {
                                Some(Glyph::East)
                            } else if pad_adjust {
                                Some(Glyph::Start)
                            } else {
                                None
                            };
                            if pill_button(ui, t!("studio.done"), false, true, done_glyph, style, Some(t!("studio.tip_done"))) {
                                if layout_changed {
                                    actions.push(StudioAction::Save(self.slots.clone()));
                                }
                                if sync_changed {
                                    actions.push(StudioAction::SaveSync(info.sync));
                                }
                                actions.push(StudioAction::Close);
                            }
                        });
                    });
            });
        let bar_rect = top_area.response.rect;
        ui_blocked.push(bar_rect.expand(4.0));

        // 2. Per-pane control pills (top of each pane, avoiding overlap with top toolbar).
        let mut order: Vec<usize> = (0..count).collect();
        order.sort_by_key(|&i| (self.slots[i].z, i));

        for &i in &order {
            let pr = norm_to_egui(self.slots[i].rect, screen);
            let p_info = info.players.get(i);
            let pname = p_info.map(|p| p.name.as_str()).filter(|s| !s.is_empty()).unwrap_or("");
            let muted = p_info.is_some_and(|p| p.muted);
            let starting = p_info.is_some_and(|p| p.starting);
            let gone = p_info.is_some_and(|p| p.gone);
            let color = if gone { TEXT2 } else { PLAYER_COLORS[i % PLAYER_COLORS.len()] };
            let phys = self.slots[i].rect.to_pane_rect(info.screen.0, info.screen.1);
            let (vw, vh) = self.slots[i].aspect.size_for(phys, self.slots[i].fit, info.screen.0, info.screen.1);
            let mut pill_y = pr.top() + 10.0;
            if pr.intersects(bar_rect.expand(6.0)) && pill_y < bar_rect.bottom() + 8.0 {
                pill_y = bar_rect.bottom() + 8.0;
            }
            let pill_x = (pr.left() + 10.0).min((pr.right() - 180.0).max(pr.left() + 6.0));
            let is_sel = i == self.selected;
            let show_pad = is_sel && pad_browse;
            let compact = pr.width() < if show_pad { 440.0 } else { 360.0 };
            let resp = egui::Area::new(egui::Id::unique(("studio-pill", i)))
                .order(egui::Order::Middle)
                .fixed_pos(pos2(pill_x, pill_y))
                .show(ui.ctx(), |ui| {
                    egui::Frame::new()
                        .fill(PILL_BG)
                        .stroke(Stroke::new(if is_sel { 1.8 } else { 1.2 }, if is_sel { color } else { color.gamma_multiply(0.75) }))
                        .corner_radius(CornerRadius::same(9))
                        .inner_margin(egui::Margin::symmetric(8, 4))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 5.0;
                                // Player badge dot + name + live resolution readout.
                                let title = if compact || pname.is_empty() {
                                    format!("P{}", i + 1)
                                } else {
                                    format!("P{} · {pname}", i + 1)
                                };
                                ui.label(egui::RichText::new(title).font(theme::strong(12.5)).color(color));
                                if !compact {
                                    let res = if gone {
                                        t!("overlay.seat_gone").to_string()
                                    } else if starting {
                                        t!("studio.starting").to_string()
                                    } else {
                                        format!("{vw}×{vh}")
                                    };
                                    ui.label(egui::RichText::new(res).font(theme::font(11.5)).color(TEXT2));
                                }
                                if show_pad && pill_button(ui, t!("studio.edit_pane"), false, true, Some(Glyph::South), style, None) {
                                    self.pad_mode = PadMode::Adjust;
                                    self.dirty = true;
                                }
                                let fit_txt = match self.slots[i].fit {
                                    PaneFit::Fit => t!("studio.fit"),
                                    PaneFit::Fill => t!("studio.fill"),
                                    PaneFit::Stretch => t!("studio.stretch"),
                                };
                                let fit_label = if compact { fit_txt.to_string() } else { format!("{}: {fit_txt}", t!("studio.scale_prefix")) };
                                if pill_button(ui, &fit_label, self.slots[i].fit != PaneFit::Fit, false, show_pad.then_some(Glyph::North), style, Some(t!("studio.tip_scale"))) {
                                    self.slots[i].fit = self.slots[i].fit.next();
                                    self.selected = i;
                                    actions.push(StudioAction::Commit(self.slots.clone()));
                                    self.dirty = true;
                                }
                                let asp_val = match self.slots[i].aspect {
                                    VirtualAspect::MatchPane => t!("studio.aspect_match").to_string(),
                                    VirtualAspect::Ratio { num, den } => format!("{num}:{den}"),
                                };
                                let asp_label = if compact { asp_val } else { format!("{}: {asp_val}", t!("studio.ratio_prefix")) };
                                if pill_button(ui, &asp_label, self.slots[i].aspect != VirtualAspect::MatchPane, false, show_pad.then_some(Glyph::West), style, Some(t!("studio.tip_ratio"))) {
                                    self.slots[i].aspect = self.slots[i].aspect.next();
                                    self.selected = i;
                                    actions.push(StudioAction::Commit(self.slots.clone()));
                                    self.dirty = true;
                                }
                                let mute_txt = if muted { t!("studio.muted") } else { t!("studio.sound") };
                                if pill_button(ui, mute_txt, muted, false, show_pad.then_some(Glyph::Lt), style, Some(t!("studio.tip_mute"))) {
                                    self.selected = i;
                                    actions.push(StudioAction::ToggleMute(i));
                                    self.dirty = true;
                                }
                                if (!compact || show_pad) && pr.width() >= 220.0
                                    && pill_button(ui, t!("studio.zoom"), info.zoomed == Some(i), false, show_pad.then_some(Glyph::Rt), style, Some(t!("studio.tip_zoom")))
                                {
                                    actions.push(StudioAction::ToggleZoom(i));
                                    self.dirty = true;
                                }
                            });
                        });
                });
            ui_blocked.push(resp.response.rect.expand(2.0));
        }

        // 3. Direct manipulation (dividers, corners/edges, move/swap).
        let over_ui = ui_blocked.iter().any(|r| r.contains(self.cursor));
        let hovered_divider = if !over_ui && !self.using_pad && self.drag.is_none() {
            hit_divider(&dividers, self.cursor, screen)
        } else {
            None
        };

        if pressed_now && !over_ui {
            if let Some(di) = hovered_divider {
                self.drag = Some(DragMode::Divider {
                    divider: dividers[di].clone(),
                    start_slots: self.slots.clone(),
                });
                self.dirty = true;
            } else if let Some((pane, edges)) = hit_pane_edges(&self.slots, self.cursor, screen) {
                self.selected = pane;
                bring_to_front(&mut self.slots, pane);
                if edges.any() {
                    self.drag = Some(DragMode::Resize {
                        pane,
                        edges,
                        start_rect: self.slots[pane].rect,
                        anchor: self.cursor,
                    });
                } else {
                    self.drag = Some(DragMode::Move {
                        pane,
                        start_rect: self.slots[pane].rect,
                        anchor: self.cursor,
                        swap_target: None,
                    });
                }
                self.dirty = true;
            }
        }

        if self.pointer_down {
            let mut preview_changed = false;
            match self.drag.clone() {
                Some(DragMode::Divider { divider, start_slots }) => {
                    let next = apply_divider_drag(&start_slots, &divider, self.cursor, screen, snap_on);
                    if next != self.slots {
                        self.slots = next;
                        preview_changed = true;
                    }
                }
                Some(DragMode::Resize { pane, edges, start_rect, anchor }) => {
                    let next_rect = apply_resize_drag(&self.slots, pane, start_rect, edges, self.cursor - anchor, screen, snap_on);
                    if next_rect != self.slots[pane].rect {
                        self.slots[pane].rect = next_rect;
                        preview_changed = true;
                    }
                }
                Some(DragMode::Move { pane, start_rect, anchor, .. }) => {
                    let next_rect = apply_move_drag(&self.slots, pane, start_rect, self.cursor - anchor, screen, snap_on);
                    let swap_target = find_swap_target(&self.slots, pane, start_rect, self.cursor, screen);
                    if let Some(DragMode::Move { swap_target: st, .. }) = self.drag.as_mut() {
                        *st = swap_target;
                    }
                    if next_rect != self.slots[pane].rect {
                        self.slots[pane].rect = next_rect;
                        preview_changed = true;
                    }
                }
                None => {}
            }
            if preview_changed {
                actions.push(StudioAction::Preview(self.slots.clone()));
                self.dirty = true;
            }
        }

        if released_now && let Some(drag) = self.drag.take() {
            match drag {
                DragMode::Move { pane, start_rect, swap_target: Some(other), .. } if other != pane => {
                    // Restore dragged pane's pre-drag rect, then swap slots (rect & z) cleanly!
                    self.slots[pane].rect = start_rect;
                    swap_slot_rects(&mut self.slots, pane, other);
                    actions.push(StudioAction::Commit(self.slots.clone()));
                }
                _ => {
                    actions.push(StudioAction::Commit(self.slots.clone()));
                }
            }
            self.dirty = true;
        }

        // 4. Paint pane frames, corner handles, shared dividers, and swap/adjust indicators.
        let p = ui.painter();
        let swap_target = match &self.drag {
            Some(DragMode::Move { pane, swap_target: Some(target), .. }) => Some((*pane, *target)),
            _ => None,
        };

        for &i in &order {
            let pr = norm_to_egui(self.slots[i].rect, screen);
            let color = PLAYER_COLORS[i % PLAYER_COLORS.len()];
            let is_sel = i == self.selected;
            let is_swap = swap_target.is_some_and(|(_, t)| t == i);

            if pad_adjust && !is_sel {
                p.rect_filled(pr, 2.0, Color32::from_rgba_premultiplied(0, 0, 0, 85));
            } else if is_swap {
                p.rect_filled(pr, 4.0, color.gamma_multiply(0.24));
            } else if is_sel {
                p.rect_filled(pr, 2.0, color.gamma_multiply(if pad_adjust { 0.10 } else { 0.05 }));
            }

            let border_w = if is_swap || (pad_adjust && is_sel) { 3.5 } else if is_sel { 2.5 } else { 1.5 };
            let border_col = if is_swap { GOOD } else if pad_adjust && is_sel { ACCENT_HI } else if is_sel { color } else { color.gamma_multiply(0.75) };
            p.rect_stroke(pr.shrink(1.0), 3.0, Stroke::new(border_w, border_col), egui::StrokeKind::Inside);

            // Crisp corner L-handles so resize affordability is obvious immediately.
            paint_corner_handles(p, pr, border_col, if is_sel { 18.0 } else { 12.0 });

            if is_swap && let Some((src, dst)) = swap_target {
                let msg = t!("studio.swap_hint", a = src + 1, b = dst + 1);
                let pill = Rect::from_center_size(pr.center(), vec2(170.0, 38.0));
                p.rect_filled(pill, 10.0, BAR_BG);
                p.rect_stroke(pill, 10.0, Stroke::new(1.5, GOOD), egui::StrokeKind::Inside);
                p.text(pill.center(), Align2::CENTER_CENTER, msg, theme::strong(14.0), TEXT);
            }
        }

        // Controller Adjust Mode card centered on the selected pane.
        if pad_adjust && self.selected < self.slots.len() {
            let i = self.selected;
            let pr = norm_to_egui(self.slots[i].rect, screen);
            let phys = self.slots[i].rect.to_pane_rect(info.screen.0, info.screen.1);
            let (vw, vh) = self.slots[i].aspect.size_for(phys, self.slots[i].fit, info.screen.0, info.screen.1);
            let pw = (f32::from(self.slots[i].rect.w) / f32::from(NORM_SCALE) * 100.0).round() as u32;
            let ph = (f32::from(self.slots[i].rect.h) / f32::from(NORM_SCALE) * 100.0).round() as u32;
            let center = pr.center();
            egui::Area::new(egui::Id::unique("studio-adjust-card"))
                .order(egui::Order::Foreground)
                .pivot(Align2::CENTER_CENTER)
                .fixed_pos(center)
                .show(ui.ctx(), |ui| {
                    egui::Frame::new()
                        .fill(BAR_BG)
                        .stroke(Stroke::new(1.5, ACCENT_HI))
                        .corner_radius(CornerRadius::same(12))
                        .inner_margin(egui::Margin::symmetric(16, 10))
                        .show(ui, |ui| {
                            ui.vertical_centered(|ui| {
                                ui.spacing_mut().item_spacing.y = 6.0;
                                let head = format!("{}  ·  {vw}×{vh} ({pw}% × {ph}%)", t!("studio.editing_pane", n = i + 1));
                                ui.label(egui::RichText::new(head).font(theme::strong(13.5)).color(TEXT));
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 12.0;
                                    pad_hint_item(ui, Glyph::Dpad, t!("studio.pad_resize"), style);
                                    pad_hint_item(ui, Glyph::RStick, t!("studio.pad_move"), style);
                                    pad_hint_item(ui, Glyph::West, t!("studio.pad_swap"), style);
                                });
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 12.0;
                                    pad_hint_item(ui, Glyph::Lt, t!("studio.pad_fine"), style);
                                    pad_hint_item(ui, Glyph::North, if self.snap { t!("studio.snap") } else { t!("studio.snap_off") }, style);
                                    pad_hint_item(ui, Glyph::South, t!("studio.pad_back"), style);
                                });
                            });
                        });
                });
        }

        // Shared divider bars + live percentage split readout.
        let active_div = match &self.drag {
            Some(DragMode::Divider { divider, .. }) => Some(divider.clone()),
            _ => hovered_divider.and_then(|di| dividers.get(di).cloned()),
        };
        for (di, d) in dividers.iter().enumerate() {
            let is_hot = hovered_divider == Some(di)
                || matches!(&self.drag, Some(DragMode::Divider { divider, .. }) if divider.axis == d.axis && divider.neg == d.neg && divider.pos_panes == d.pos_panes);
            let rect = divider_egui_rect(d, screen, if is_hot { 6.0 } else { 2.0 });
            let col = if is_hot { ACCENT_HI } else { Color32::from_rgba_premultiplied(255, 255, 255, 70) };
            p.rect_filled(rect, 3.0, col);
        }
        if let Some(d) = active_div
            && matches!(self.drag, Some(DragMode::Divider { .. }))
        {
            let pos = d.neg.first().map(|&i| match d.axis {
                Axis::Vertical => self.slots[i].rect.right(),
                Axis::Horizontal => self.slots[i].rect.bottom(),
            }).unwrap_or(d.pos);
            let pct_a = (f32::from(pos) / f32::from(NORM_SCALE) * 100.0).round() as u32;
            let pct_b = 100u32.saturating_sub(pct_a);
            let txt = if self.alt_down && self.snap {
                format!("{pct_a}%  |  {pct_b}%  ·  {}", t!("studio.free_tag"))
            } else {
                format!("{pct_a}%  |  {pct_b}%")
            };
            let w = if self.alt_down && self.snap { 156.0 } else { 104.0 };
            let badge = Rect::from_center_size(self.cursor + vec2(0.0, -26.0), vec2(w, 26.0));
            p.rect_filled(badge, 8.0, BAR_BG);
            p.rect_stroke(badge, 8.0, Stroke::new(1.0, if self.alt_down && self.snap { WARN } else { ACCENT_HI }), egui::StrokeKind::Inside);
            p.text(badge.center(), Align2::CENTER_CENTER, txt, theme::strong(12.5), TEXT);
        }

        // 5. Bottom prompt / hint bar (real controller button glyphs in pad mode, clear KBM hints in mouse mode).
        egui::Area::new(egui::Id::unique("studio-hints"))
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -14.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::new()
                    .fill(BAR_BG)
                    .stroke(Stroke::new(1.0, BORDER))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(egui::Margin::symmetric(14, 7))
                    .show(ui, |ui| {
                        if self.using_pad {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 12.0;
                                match self.pad_mode {
                                    PadMode::Browse => {
                                        pad_hint_item(ui, Glyph::Dpad, t!("studio.pad_select"), style);
                                        pad_hint_item(ui, Glyph::South, t!("studio.pad_edit"), style);
                                        pad_hint_pair(ui, Glyph::Lb, Glyph::Rb, t!("studio.pad_preset"), style);
                                        pad_hint_item(ui, Glyph::North, t!("studio.scale_prefix"), style);
                                        pad_hint_item(ui, Glyph::West, t!("studio.ratio_prefix"), style);
                                        pad_hint_item(ui, Glyph::Lt, t!("studio.sound"), style);
                                        pad_hint_item(ui, Glyph::Rt, t!("studio.zoom"), style);
                                        pad_hint_item(ui, Glyph::Back, t!("studio.pad_sync"), style);
                                        pad_hint_item(ui, Glyph::East, t!("studio.done"), style);
                                    }
                                    PadMode::Adjust => {
                                        pad_hint_item(ui, Glyph::Dpad, t!("studio.pad_resize"), style);
                                        pad_hint_item(ui, Glyph::Lt, t!("studio.pad_fine"), style);
                                        pad_hint_item(ui, Glyph::RStick, t!("studio.pad_move"), style);
                                        pad_hint_item(ui, Glyph::West, t!("studio.pad_swap"), style);
                                        pad_hint_item(ui, Glyph::North, t!("studio.snap"), style);
                                        pad_hint_item(ui, Glyph::South, t!("studio.pad_back"), style);
                                    }
                                }
                            });
                        } else {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 10.0;
                                ui.label(egui::RichText::new(t!("studio.hints")).font(theme::font(12.0)).color(TEXT2));
                                if self.alt_down && self.snap {
                                    ui.label(egui::RichText::new(t!("studio.snap_alt_off")).font(theme::strong(12.0)).color(WARN));
                                }
                                if self.pad_connected {
                                    ui.separator();
                                    pad_hint_item(ui, Glyph::Dpad, t!("studio.pad_available"), style);
                                }
                            });
                        }
                    });
            });
    }
}

fn inline_glyph(ui: &mut egui::Ui, g: Glyph, style: pad::Style) {
    let w = icons::glyph_width(g);
    let (rect, _) = ui.allocate_exact_size(vec2(w, 22.0), egui::Sense::hover());
    icons::glyph(ui.painter(), rect.center(), g, style);
}

fn pad_hint_item(ui: &mut egui::Ui, g: Glyph, label: &str, style: pad::Style) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        let w = icons::glyph_width(g);
        let (rect, _) = ui.allocate_exact_size(vec2(w, 20.0), egui::Sense::hover());
        icons::glyph(ui.painter(), rect.center(), g, style);
        ui.label(egui::RichText::new(label).font(theme::font(12.0)).color(TEXT));
    });
}

fn pad_hint_pair(ui: &mut egui::Ui, a: Glyph, b: Glyph, label: &str, style: pad::Style) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let (ra, _) = ui.allocate_exact_size(vec2(icons::glyph_width(a), 20.0), egui::Sense::hover());
        icons::glyph(ui.painter(), ra.center(), a, style);
        let (rb, _) = ui.allocate_exact_size(vec2(icons::glyph_width(b), 20.0), egui::Sense::hover());
        icons::glyph(ui.painter(), rb.center(), b, style);
        ui.label(egui::RichText::new(label).font(theme::font(12.0)).color(TEXT));
    });
}

fn pill_button(ui: &mut egui::Ui, label: &str, active: bool, primary: bool, glyph: Option<Glyph>, style: pad::Style, tip: Option<&str>) -> bool {
    pill_button_inner(ui, label, active, primary, false, glyph, style, tip)
}

fn pill_button_warn(ui: &mut egui::Ui, label: &str, active: bool, warn: bool, glyph: Option<Glyph>, style: pad::Style, tip: Option<&str>) -> bool {
    pill_button_inner(ui, label, active, false, warn, glyph, style, tip)
}

#[allow(clippy::too_many_arguments)]
fn pill_button_inner(
    ui: &mut egui::Ui,
    label: &str,
    active: bool,
    primary: bool,
    warn: bool,
    glyph: Option<Glyph>,
    style: pad::Style,
    tip: Option<&str>,
) -> bool {
    let fill = if primary { ACCENT } else if active { BTN_ACTIVE } else { BTN_BG };
    let text_col = if warn { WARN } else if primary || active { TEXT } else { TEXT2 };
    let g = ui.painter().layout_no_wrap(label.to_string(), theme::strong(12.0), text_col);
    let gw = glyph.map(|gl| icons::glyph_width(gl) + 5.0).unwrap_or(0.0);
    let size = vec2(g.size().x + gw + 16.0, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let bg = if resp.hovered() && !primary { BTN_HOV } else { fill };
    ui.painter().rect_filled(rect, 7.0, bg);
    if warn {
        ui.painter().rect_stroke(rect, 7.0, Stroke::new(1.0, WARN.gamma_multiply(0.8)), egui::StrokeKind::Inside);
    } else if active || primary {
        ui.painter().rect_stroke(rect, 7.0, Stroke::new(1.0, ACCENT_HI.gamma_multiply(0.55)), egui::StrokeKind::Inside);
    }
    let mut x = rect.left() + 8.0;
    if let Some(gl) = glyph {
        let w = icons::glyph_width(gl);
        icons::glyph(ui.painter(), pos2(x + w * 0.5, rect.center().y), gl, style);
        x += w + 5.0;
    }
    ui.painter().galley(pos2(x, rect.center().y - g.size().y / 2.0), g, text_col);
    let resp = match tip {
        Some(text) => resp.on_hover_text(text),
        None => resp,
    };
    resp.clicked()
}

fn paint_corner_handles(p: &egui::Painter, r: Rect, color: Color32, len: f32) {
    let s = Stroke::new(3.0, color);
    let r = r.shrink(2.0);
    let len = len.min(r.width() * 0.25).min(r.height() * 0.25);
    // Top-left
    p.line_segment([r.left_top(), pos2(r.left() + len, r.top())], s);
    p.line_segment([r.left_top(), pos2(r.left(), r.top() + len)], s);
    // Top-right
    p.line_segment([r.right_top(), pos2(r.right() - len, r.top())], s);
    p.line_segment([r.right_top(), pos2(r.right(), r.top() + len)], s);
    // Bottom-left
    p.line_segment([r.left_bottom(), pos2(r.left() + len, r.bottom())], s);
    p.line_segment([r.left_bottom(), pos2(r.left(), r.bottom() - len)], s);
    // Bottom-right
    p.line_segment([r.right_bottom(), pos2(r.right() - len, r.bottom())], s);
    p.line_segment([r.right_bottom(), pos2(r.right(), r.bottom() - len)], s);
}

fn preset_list(count: usize) -> Vec<(&'static str, LayoutPreset, Option<&'static str>)> {
    let mut list = Vec::new();
    if count == 3 {
        list.push((t!("game.layout_top2"), LayoutPreset::TopTwo, None));
        list.push((t!("game.layout_bottom2"), LayoutPreset::BottomTwo, None));
    }
    list.push((t!("game.layout_horizontal"), LayoutPreset::Horizontal, None));
    list.push((t!("game.layout_vertical"), LayoutPreset::Vertical, None));
    if count >= 3 {
        list.push((t!("game.layout_grid"), LayoutPreset::Grid2x2, None));
    }
    list.push((t!("studio.preset_pip"), LayoutPreset::Slots(LayoutPreset::pip_slots(count)), Some(t!("studio.tip_pip"))));
    list
}

/// Spatial pane navigation: picks the nearest pane in direction `(dx, dy)` (where `dx` or `dy`
/// is ±1), falling back to cyclic next/prev when no pane lies strictly in that direction.
fn select_pane_dir(slots: &[PaneSlot], cur: usize, dx: i32, dy: i32) -> Option<usize> {
    let n = slots.len();
    if n <= 1 || cur >= n {
        return None;
    }
    let c = slots[cur].rect;
    let cx = f32::from(c.x) + f32::from(c.w) * 0.5;
    let cy = f32::from(c.y) + f32::from(c.h) * 0.5;
    let mut best: Option<(usize, f32)> = None;
    for (i, s) in slots.iter().enumerate() {
        if i == cur {
            continue;
        }
        let ox = f32::from(s.rect.x) + f32::from(s.rect.w) * 0.5;
        let oy = f32::from(s.rect.y) + f32::from(s.rect.h) * 0.5;
        let vx = ox - cx;
        let vy = oy - cy;
        let along = vx * dx as f32 + vy * dy as f32;
        if along <= 10.0 {
            continue;
        }
        let perp = (vx * dy as f32 - vy * dx as f32).abs();
        let score = along + perp * 2.5;
        if best.is_none_or(|(_, b)| score < b) {
            best = Some((i, score));
        }
    }
    if let Some((i, _)) = best {
        return Some(i);
    }
    // Wrap around cleanly so D-Pad always moves even in a 1D strip.
    let step = if dx > 0 || dy > 0 { 1 } else { n - 1 };
    Some((cur + step) % n)
}

/// Resizes `pane` by one controller step in `(dx, dy)`: slides a shared interior divider
/// when `pane` borders one on that axis (keeping zero gaps between tiled panes), else
/// adjusts `pane`'s own width/height directly (for PiP / free-form panes).
fn resize_pane_step(slots: &mut [PaneSlot], pane: usize, dx: i32, dy: i32, snap: bool, fine: bool) -> bool {
    if pane >= slots.len() {
        return false;
    }
    let step = if fine { PAD_STEP_FINE } else { PAD_STEP_COARSE };
    let mut changed = false;
    let try_axis = |slots: &mut [PaneSlot], axis: Axis, dir: i32| -> bool {
        if dir == 0 {
            return false;
        }
        let divs = find_shared_dividers(slots);
        // Prefer a shared divider bordered by this pane (right/bottom edge first, then left/top).
        let on_neg = divs.iter().find(|d| d.axis == axis && d.neg.contains(&pane)).cloned();
        let on_pos = divs.iter().find(|d| d.axis == axis && d.pos_panes.contains(&pane)).cloned();
        if let Some(d) = on_neg.or(on_pos) {
            let mut min_pos = NORM_MIN;
            let mut max_pos = NORM_SCALE - NORM_MIN;
            for &i in &d.neg {
                let base = match axis {
                    Axis::Vertical => slots[i].rect.x,
                    Axis::Horizontal => slots[i].rect.y,
                };
                min_pos = min_pos.max(base.saturating_add(NORM_MIN));
            }
            for &i in &d.pos_panes {
                let far = match axis {
                    Axis::Vertical => slots[i].rect.right(),
                    Axis::Horizontal => slots[i].rect.bottom(),
                };
                max_pos = max_pos.min(far.saturating_sub(NORM_MIN));
            }
            if min_pos > max_pos {
                return false;
            }
            let raw = (i32::from(d.pos) + dir * step).clamp(i32::from(min_pos), i32::from(max_pos)) as u16;
            let target = step_snap(d.pos, raw, dir, snap).clamp(min_pos, max_pos);
            if target == d.pos {
                return false;
            }
            for &i in &d.neg {
                match axis {
                    Axis::Vertical => slots[i].rect.w = target.saturating_sub(slots[i].rect.x).max(NORM_MIN),
                    Axis::Horizontal => slots[i].rect.h = target.saturating_sub(slots[i].rect.y).max(NORM_MIN),
                }
            }
            for &i in &d.pos_panes {
                match axis {
                    Axis::Vertical => {
                        let far = slots[i].rect.right();
                        slots[i].rect.x = target;
                        slots[i].rect.w = far.saturating_sub(target).max(NORM_MIN);
                    }
                    Axis::Horizontal => {
                        let far = slots[i].rect.bottom();
                        slots[i].rect.y = target;
                        slots[i].rect.h = far.saturating_sub(target).max(NORM_MIN);
                    }
                }
            }
            return true;
        }
        // Standalone / floating pane (e.g. PiP): grow/shrink the pane directly.
        let r = slots[pane].rect;
        let next = match axis {
            Axis::Vertical => {
                let nw = (i32::from(r.w) + dir * step).clamp(i32::from(NORM_MIN), i32::from(NORM_SCALE)) as u16;
                let nx = r.x.min(NORM_SCALE - nw);
                NormRect::clamped(nx, r.y, nw, r.h)
            }
            Axis::Horizontal => {
                let nh = (i32::from(r.h) + dir * step).clamp(i32::from(NORM_MIN), i32::from(NORM_SCALE)) as u16;
                let ny = r.y.min(NORM_SCALE - nh);
                NormRect::clamped(r.x, ny, r.w, nh)
            }
        };
        if next != r {
            slots[pane].rect = next;
            return true;
        }
        false
    };
    changed |= try_axis(slots, Axis::Vertical, dx);
    changed |= try_axis(slots, Axis::Horizontal, dy);
    changed
}

/// Snaps a stepped divider move onto clean landmarks (`1/4`, `1/3`, `1/2`, `2/3`, `3/4`)
/// when a step crosses or lands near one, without getting stuck on the current stop.
fn step_snap(from: u16, to: u16, dir: i32, snap: bool) -> u16 {
    if !snap {
        return to;
    }
    let lo = from.min(to);
    let hi = from.max(to);
    for &stop in &SNAP_STOPS {
        if stop == from {
            continue;
        }
        if (dir > 0 && stop > lo && stop <= hi) || (dir < 0 && stop >= lo && stop < hi) {
            return stop;
        }
    }
    snap_value(to, &[], true)
}

/// Moves `pane`'s rectangle by `(mx, my)` normalized units, clamped inside the screen.
fn move_rect_by(slots: &[PaneSlot], pane: usize, start: NormRect, mx: i32, my: i32, snap: bool) -> NormRect {
    let max_x = i32::from(NORM_SCALE - start.w);
    let max_y = i32::from(NORM_SCALE - start.h);
    let mut x = (i32::from(start.x) + mx).clamp(0, max_x) as u16;
    let mut y = (i32::from(start.y) + my).clamp(0, max_y) as u16;
    if snap {
        let vx = peer_edges(slots, pane, Axis::Vertical);
        let vy = peer_edges(slots, pane, Axis::Horizontal);
        // Smaller snap radius for continuous analog movement so it doesn't feel sticky.
        let near = |v: u16, extra: &[u16]| -> Option<u16> {
            SNAP_STOPS.iter().chain(extra.iter()).copied().min_by_key(|&s| v.abs_diff(s)).filter(|&s| v.abs_diff(s) <= 90)
        };
        if let Some(sx) = near(x, &vx) {
            x = sx.min(NORM_SCALE - start.w);
        } else if let Some(sr) = near(x + start.w, &vx) && sr >= start.w {
            x = sr - start.w;
        }
        if let Some(sy) = near(y, &vy) {
            y = sy.min(NORM_SCALE - start.h);
        } else if let Some(sb) = near(y + start.h, &vy) && sb >= start.h {
            y = sb - start.h;
        }
    }
    NormRect::clamped(x, y, start.w, start.h)
}

fn swap_slot_rects(slots: &mut [PaneSlot], a: usize, b: usize) {
    if a == b || a >= slots.len() || b >= slots.len() {
        return;
    }
    let a_rect = slots[a].rect;
    let a_z = slots[a].z;
    slots[a].rect = slots[b].rect;
    slots[a].z = slots[b].z;
    slots[b].rect = a_rect;
    slots[b].z = a_z;
}

/// Swaps `pane` with the next pane in screen reading order (top-to-bottom, left-to-right),
/// so tapping Swap repeatedly cycles `pane` cleanly through every slot on screen.
fn swap_next_visual(slots: &mut [PaneSlot], pane: usize) {
    if slots.len() <= 1 || pane >= slots.len() {
        return;
    }
    let mut order: Vec<usize> = (0..slots.len()).collect();
    order.sort_by_key(|&i| (slots[i].rect.y / 500, slots[i].rect.x, i));
    let pos = order.iter().position(|&i| i == pane).unwrap_or(0);
    let other = order[(pos + 1) % order.len()];
    swap_slot_rects(slots, pane, other);
}

fn norm_to_egui(r: NormRect, screen: Vec2) -> Rect {
    let s = f32::from(NORM_SCALE);
    let x0 = f32::from(r.x) / s * screen.x;
    let y0 = f32::from(r.y) / s * screen.y;
    let x1 = f32::from(r.right()) / s * screen.x;
    let y1 = f32::from(r.bottom()) / s * screen.y;
    Rect::from_min_max(pos2(x0, y0), pos2(x1, y1))
}

fn divider_egui_rect(d: &SharedDivider, screen: Vec2, thickness: f32) -> Rect {
    let s = f32::from(NORM_SCALE);
    let half = thickness * 0.5;
    match d.axis {
        Axis::Vertical => {
            let x = f32::from(d.pos) / s * screen.x;
            let y0 = f32::from(d.span.0) / s * screen.y;
            let y1 = f32::from(d.span.1) / s * screen.y;
            Rect::from_min_max(pos2(x - half, y0 + 8.0), pos2(x + half, (y1 - 8.0).max(y0 + 12.0)))
        }
        Axis::Horizontal => {
            let y = f32::from(d.pos) / s * screen.y;
            let x0 = f32::from(d.span.0) / s * screen.x;
            let x1 = f32::from(d.span.1) / s * screen.x;
            Rect::from_min_max(pos2(x0 + 8.0, y - half), pos2((x1 - 8.0).max(x0 + 12.0), y + half))
        }
    }
}

fn hit_divider(dividers: &[SharedDivider], pt: Pos2, screen: Vec2) -> Option<usize> {
    dividers.iter().position(|d| divider_egui_rect(d, screen, 18.0).contains(pt))
}

/// Finds shared interior vertical & horizontal seams between panes so dragging a divider
/// resizes both sides simultaneously with zero gaps.
fn find_shared_dividers(slots: &[PaneSlot]) -> Vec<SharedDivider> {
    let mut out = Vec::new();
    let mut v_candidates: Vec<u16> = Vec::new();
    let mut h_candidates: Vec<u16> = Vec::new();
    for s in slots {
        let r = s.rect.right();
        let b = s.rect.bottom();
        if r > SEAM_TOL && r < NORM_SCALE - SEAM_TOL && !v_candidates.iter().any(|&c| c.abs_diff(r) <= SEAM_TOL) {
            v_candidates.push(r);
        }
        if b > SEAM_TOL && b < NORM_SCALE - SEAM_TOL && !h_candidates.iter().any(|&c| c.abs_diff(b) <= SEAM_TOL) {
            h_candidates.push(b);
        }
    }
    for pos in v_candidates {
        let neg: Vec<usize> = slots.iter().enumerate().filter(|(_, s)| s.rect.right().abs_diff(pos) <= SEAM_TOL).map(|(i, _)| i).collect();
        let pos_panes: Vec<usize> = slots.iter().enumerate().filter(|(_, s)| s.rect.x.abs_diff(pos) <= SEAM_TOL).map(|(i, _)| i).collect();
        if !neg.is_empty() && !pos_panes.is_empty() {
            let (mut y0, mut y1) = (NORM_SCALE, 0u16);
            for &i in neg.iter().chain(&pos_panes) {
                y0 = y0.min(slots[i].rect.y);
                y1 = y1.max(slots[i].rect.bottom());
            }
            if y1 > y0 {
                out.push(SharedDivider { axis: Axis::Vertical, pos, span: (y0, y1), neg, pos_panes });
            }
        }
    }
    for pos in h_candidates {
        let neg: Vec<usize> = slots.iter().enumerate().filter(|(_, s)| s.rect.bottom().abs_diff(pos) <= SEAM_TOL).map(|(i, _)| i).collect();
        let pos_panes: Vec<usize> = slots.iter().enumerate().filter(|(_, s)| s.rect.y.abs_diff(pos) <= SEAM_TOL).map(|(i, _)| i).collect();
        if !neg.is_empty() && !pos_panes.is_empty() {
            let (mut x0, mut x1) = (NORM_SCALE, 0u16);
            for &i in neg.iter().chain(&pos_panes) {
                x0 = x0.min(slots[i].rect.x);
                x1 = x1.max(slots[i].rect.right());
            }
            if x1 > x0 {
                out.push(SharedDivider { axis: Axis::Horizontal, pos, span: (x0, x1), neg, pos_panes });
            }
        }
    }
    out
}

fn snap_value(v: u16, extra: &[u16], enabled: bool) -> u16 {
    if !enabled {
        return v;
    }
    let mut best = v;
    let mut best_d = SNAP_RADIUS + 1;
    for &stop in SNAP_STOPS.iter().chain(extra.iter()) {
        let d = v.abs_diff(stop);
        if d < best_d {
            best_d = d;
            best = stop;
        }
    }
    best
}

fn apply_divider_drag(start_slots: &[PaneSlot], d: &SharedDivider, pt: Pos2, screen: Vec2, snap: bool) -> Vec<PaneSlot> {
    let raw = match d.axis {
        Axis::Vertical => ((pt.x / screen.x.max(1.0)).clamp(0.0, 1.0) * f32::from(NORM_SCALE)).round() as u16,
        Axis::Horizontal => ((pt.y / screen.y.max(1.0)).clamp(0.0, 1.0) * f32::from(NORM_SCALE)).round() as u16,
    };
    let mut min_pos = NORM_MIN;
    let mut max_pos = NORM_SCALE - NORM_MIN;
    for &i in &d.neg {
        let base = match d.axis {
            Axis::Vertical => start_slots[i].rect.x,
            Axis::Horizontal => start_slots[i].rect.y,
        };
        min_pos = min_pos.max(base.saturating_add(NORM_MIN));
    }
    for &i in &d.pos_panes {
        let far = match d.axis {
            Axis::Vertical => start_slots[i].rect.right(),
            Axis::Horizontal => start_slots[i].rect.bottom(),
        };
        max_pos = max_pos.min(far.saturating_sub(NORM_MIN));
    }
    if min_pos > max_pos {
        return start_slots.to_vec();
    }
    let target = snap_value(raw, &[], snap).clamp(min_pos, max_pos);
    let mut next = start_slots.to_vec();
    for &i in &d.neg {
        match d.axis {
            Axis::Vertical => next[i].rect.w = target.saturating_sub(next[i].rect.x).max(NORM_MIN),
            Axis::Horizontal => next[i].rect.h = target.saturating_sub(next[i].rect.y).max(NORM_MIN),
        }
    }
    for &i in &d.pos_panes {
        match d.axis {
            Axis::Vertical => {
                let far = start_slots[i].rect.right();
                next[i].rect.x = target;
                next[i].rect.w = far.saturating_sub(target).max(NORM_MIN);
            }
            Axis::Horizontal => {
                let far = start_slots[i].rect.bottom();
                next[i].rect.y = target;
                next[i].rect.h = far.saturating_sub(target).max(NORM_MIN);
            }
        }
    }
    next
}

fn hit_pane_edges(slots: &[PaneSlot], pt: Pos2, screen: Vec2) -> Option<(usize, Edges)> {
    let mut order: Vec<usize> = (0..slots.len()).collect();
    order.sort_by_key(|&i| (slots[i].z, i));
    for i in order.into_iter().rev() {
        let r = norm_to_egui(slots[i].rect, screen);
        if !r.expand(6.0).contains(pt) {
            continue;
        }
        let edge_m = 12.0;
        let corner_m = 20.0;
        let mut e = Edges {
            left: (pt.x - r.left()).abs() <= edge_m,
            right: (pt.x - r.right()).abs() <= edge_m,
            top: (pt.y - r.top()).abs() <= edge_m,
            bottom: (pt.y - r.bottom()).abs() <= edge_m,
        };
        // Generous corner grabs.
        if (pt.x - r.left()).abs() <= corner_m && (pt.y - r.top()).abs() <= corner_m {
            (e.left, e.top) = (true, true);
        } else if (pt.x - r.right()).abs() <= corner_m && (pt.y - r.top()).abs() <= corner_m {
            (e.right, e.top) = (true, true);
        } else if (pt.x - r.left()).abs() <= corner_m && (pt.y - r.bottom()).abs() <= corner_m {
            (e.left, e.bottom) = (true, true);
        } else if (pt.x - r.right()).abs() <= corner_m && (pt.y - r.bottom()).abs() <= corner_m {
            (e.right, e.bottom) = (true, true);
        }
        return Some((i, e));
    }
    None
}

fn peer_edges(slots: &[PaneSlot], skip: usize, axis: Axis) -> Vec<u16> {
    let mut out = Vec::new();
    for (i, s) in slots.iter().enumerate() {
        if i == skip {
            continue;
        }
        match axis {
            Axis::Vertical => {
                out.push(s.rect.x);
                out.push(s.rect.right());
            }
            Axis::Horizontal => {
                out.push(s.rect.y);
                out.push(s.rect.bottom());
            }
        }
    }
    out
}

fn apply_resize_drag(slots: &[PaneSlot], pane: usize, start: NormRect, edges: Edges, delta: Vec2, screen: Vec2, snap: bool) -> NormRect {
    let dx = (delta.x / screen.x.max(1.0) * f32::from(NORM_SCALE)).round() as i32;
    let dy = (delta.y / screen.y.max(1.0) * f32::from(NORM_SCALE)).round() as i32;
    let vx = peer_edges(slots, pane, Axis::Vertical);
    let vy = peer_edges(slots, pane, Axis::Horizontal);
    let mut x0 = i32::from(start.x);
    let mut x1 = i32::from(start.right());
    let mut y0 = i32::from(start.y);
    let mut y1 = i32::from(start.bottom());
    let min = i32::from(NORM_MIN);
    let max = i32::from(NORM_SCALE);
    if edges.left {
        let raw = (x0 + dx).clamp(0, x1 - min) as u16;
        x0 = i32::from(snap_value(raw, &vx, snap)).min(x1 - min);
    }
    if edges.right {
        let raw = (x1 + dx).clamp(x0 + min, max) as u16;
        x1 = i32::from(snap_value(raw, &vx, snap)).max(x0 + min);
    }
    if edges.top {
        let raw = (y0 + dy).clamp(0, y1 - min) as u16;
        y0 = i32::from(snap_value(raw, &vy, snap)).min(y1 - min);
    }
    if edges.bottom {
        let raw = (y1 + dy).clamp(y0 + min, max) as u16;
        y1 = i32::from(snap_value(raw, &vy, snap)).max(y0 + min);
    }
    NormRect::clamped(x0 as u16, y0 as u16, (x1 - x0) as u16, (y1 - y0) as u16)
}

fn apply_move_drag(slots: &[PaneSlot], pane: usize, start: NormRect, delta: Vec2, screen: Vec2, snap: bool) -> NormRect {
    let dx = (delta.x / screen.x.max(1.0) * f32::from(NORM_SCALE)).round() as i32;
    let dy = (delta.y / screen.y.max(1.0) * f32::from(NORM_SCALE)).round() as i32;
    let max_x = i32::from(NORM_SCALE - start.w);
    let max_y = i32::from(NORM_SCALE - start.h);
    let mut x = (i32::from(start.x) + dx).clamp(0, max_x) as u16;
    let mut y = (i32::from(start.y) + dy).clamp(0, max_y) as u16;
    if snap {
        let vx = peer_edges(slots, pane, Axis::Vertical);
        let vy = peer_edges(slots, pane, Axis::Horizontal);
        let sx = snap_value(x, &vx, true);
        if sx != x {
            x = sx.min(NORM_SCALE - start.w);
        } else {
            let sr = snap_value(x + start.w, &vx, true);
            if sr != x + start.w && sr >= start.w {
                x = sr - start.w;
            }
        }
        let sy = snap_value(y, &vy, true);
        if sy != y {
            y = sy.min(NORM_SCALE - start.h);
        } else {
            let sb = snap_value(y + start.h, &vy, true);
            if sb != y + start.h && sb >= start.h {
                y = sb - start.h;
            }
        }
    }
    NormRect::clamped(x, y, start.w, start.h)
}

fn find_swap_target(slots: &[PaneSlot], dragging: usize, start_rect: NormRect, pt: Pos2, screen: Vec2) -> Option<usize> {
    let drag_area = rect_area(start_rect);
    let mut order: Vec<usize> = (0..slots.len()).filter(|&i| i != dragging).collect();
    order.sort_by_key(|&i| (slots[i].z, i));
    order.into_iter().rev().find(|&i| {
        // A floating PiP pane dragged over the larger background pane it already sat on top
        // of should move freely rather than snapping into a swap with the background.
        if rects_overlap(start_rect, slots[i].rect) && drag_area < rect_area(slots[i].rect) {
            return false;
        }
        let r = norm_to_egui(slots[i].rect, screen);
        // Center 65% of the other pane acts as a clear swap drop zone.
        let inner = Rect::from_center_size(r.center(), r.size() * 0.65);
        inner.contains(pt)
    })
}

/// Raises `pane` above every other pane in Z-order (compactly re-indexed 0..N), while
/// keeping smaller floating PiP panes that sit completely inside `pane` above it so
/// clicking the background pane never hides PiP windows behind it.
fn bring_to_front(slots: &mut [PaneSlot], pane: usize) {
    if slots.len() <= 1 || pane >= slots.len() {
        return;
    }
    // Keep Z=0 on non-overlapping tiled panes unless panes overlap.
    let overlaps = (0..slots.len()).any(|j| {
        j != pane && rects_overlap(slots[pane].rect, slots[j].rect)
    });
    if !overlaps {
        return;
    }
    let pane_rect = slots[pane].rect;
    let pane_area = rect_area(pane_rect);
    let mut ord: Vec<usize> = (0..slots.len()).collect();
    ord.sort_by_key(|&i| {
        let enclosed_pip = i != pane && rect_contains(pane_rect, slots[i].rect) && rect_area(slots[i].rect) < pane_area;
        let bucket = if enclosed_pip { 2u8 } else if i == pane { 1 } else { 0 };
        (bucket, slots[i].z, i)
    });
    for (rank, i) in ord.into_iter().enumerate() {
        slots[i].z = rank as u8;
    }
}

fn rect_area(r: NormRect) -> u32 {
    u32::from(r.w) * u32::from(r.h)
}

fn rect_contains(outer: NormRect, inner: NormRect) -> bool {
    outer.x <= inner.x && outer.y <= inner.y && outer.right() >= inner.right() && outer.bottom() >= inner.bottom()
}

fn rects_overlap(a: NormRect, b: NormRect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_info(n: usize) -> StudioInfo {
        StudioInfo {
            players: (0..n)
                .map(|i| PlayerInfo { name: format!("Player{}", i + 1), muted: false, starting: false, gone: false })
                .collect(),
            screen: (1280, 720),
            keyboard: Some(0),
            zoomed: None,
            sync: true,
        }
    }

    #[test]
    fn shared_divider_drag_slides_both_sides_without_gaps() {
        let slots = LayoutPreset::Horizontal.to_slots(2);
        let divs = find_shared_dividers(&slots);
        assert_eq!(divs.len(), 1);
        assert_eq!(divs[0].axis, Axis::Vertical);
        assert_eq!(divs[0].pos, 5_000);
        // Drag to ~66.5% -> magnetically snaps to 2/3 (6667).
        let moved = apply_divider_drag(&slots, &divs[0], pos2(1278.0, 500.0), vec2(1920.0, 1080.0), true);
        assert_eq!(moved[0].rect.w, 6_667);
        assert_eq!(moved[1].rect.x, 6_667);
        assert_eq!(moved[1].rect.w, 3_333);
        assert_eq!(moved[0].rect.right(), moved[1].rect.x, "zero seam");
    }

    #[test]
    fn top_two_has_both_horizontal_and_vertical_dividers() {
        let slots = LayoutPreset::TopTwo.to_slots(3);
        let divs = find_shared_dividers(&slots);
        assert_eq!(divs.len(), 2, "one vertical seam on top row, one horizontal seam across middle");
        let h = divs.iter().find(|d| d.axis == Axis::Horizontal).unwrap();
        assert_eq!(h.neg, vec![0, 1]);
        assert_eq!(h.pos_panes, vec![2]);
    }

    #[test]
    fn spatial_selection_navigates_grid_and_wraps() {
        let grid = LayoutPreset::Grid2x2.to_slots(4);
        assert_eq!(select_pane_dir(&grid, 0, 1, 0), Some(1));
        assert_eq!(select_pane_dir(&grid, 0, 0, 1), Some(2));
        assert_eq!(select_pane_dir(&grid, 1, 0, 1), Some(3));
        assert_eq!(select_pane_dir(&grid, 3, -1, 0), Some(2));
        assert_eq!(select_pane_dir(&grid, 2, 0, -1), Some(0));
        // 1D horizontal strip wraps cleanly on Up/Down too.
        let horiz = LayoutPreset::Horizontal.to_slots(2);
        assert_eq!(select_pane_dir(&horiz, 0, 0, 1), Some(1));
        assert_eq!(select_pane_dir(&horiz, 1, 0, -1), Some(0));
    }

    #[test]
    fn controller_browse_and_adjust_modes_work_end_to_end() {
        crate::i18n::set("en");
        let mut s = LayoutStudio::new();
        let bounds = ScreenRect { left: 0, top: 0, right: 1280, bottom: 720 };
        let idle = StudioPad { connected: true, style: pad::Style::Xbox, ..Default::default() };
        s.open(LayoutPreset::Horizontal.to_slots(2), 0, true, bounds, 1.0, idle);
        assert!(s.using_pad);
        let info = test_info(2);
        let mut actions = Vec::new();

        // Browse: LT mutes, RT toggles Full Screen, Back toggles Sync.
        assert!(!s.handle_pad(StudioPad { lt: true, ..idle }, &info, &mut actions));
        assert_eq!(actions, vec![StudioAction::ToggleMute(0)]);
        actions.clear();
        assert!(!s.handle_pad(StudioPad { rt: true, ..idle }, &info, &mut actions));
        assert_eq!(actions, vec![StudioAction::ToggleZoom(0)]);
        actions.clear();
        assert!(!s.handle_pad(StudioPad { buttons: pad::BACK, ..idle }, &info, &mut actions));
        assert_eq!(actions, vec![StudioAction::SetSync(false)]);
        actions.clear();
        // Release buttons before entering Adjust mode.
        assert!(!s.handle_pad(idle, &info, &mut actions));

        // Press A -> enters Adjust mode.
        assert!(!s.handle_pad(StudioPad { buttons: pad::SOUTH, ..idle }, &info, &mut actions));
        assert_eq!(s.pad_mode, PadMode::Adjust);
        assert!(!s.handle_pad(idle, &info, &mut actions));

        // D-Pad Right in Adjust mode slides the shared center divider cleanly with zero gap.
        assert!(!s.handle_pad(StudioPad { buttons: pad::RIGHT, ..idle }, &info, &mut actions));
        assert!(s.slots[0].rect.w > 5_000);
        assert_eq!(s.slots[0].rect.right(), s.slots[1].rect.x, "zero seam after controller resize");
        assert!(matches!(actions.last(), Some(StudioAction::Commit(_))));
        actions.clear();
        assert!(!s.handle_pad(idle, &info, &mut actions));

        // X in Adjust mode swaps pane 0 and pane 1's slots.
        let before0 = s.slots[0].rect;
        let before1 = s.slots[1].rect;
        assert!(!s.handle_pad(StudioPad { buttons: pad::WEST, ..idle }, &info, &mut actions));
        assert_eq!(s.slots[0].rect, before1);
        assert_eq!(s.slots[1].rect, before0);
        actions.clear();
        assert!(!s.handle_pad(idle, &info, &mut actions));

        // B in Adjust mode returns to Browse (does NOT close Studio yet).
        assert!(!s.handle_pad(StudioPad { buttons: pad::EAST, ..idle }, &info, &mut actions));
        assert_eq!(s.pad_mode, PadMode::Browse);
        assert!(!s.handle_pad(idle, &info, &mut actions));

        // B in Browse mode closes Studio.
        assert!(s.handle_pad(StudioPad { buttons: pad::EAST, ..idle }, &info, &mut actions));
    }

    #[test]
    fn pip_drag_moves_freely_without_swapping_into_background() {
        let mut slots = LayoutPreset::pip_slots(3);
        let screen = vec2(1920.0, 1080.0);
        // Dragging PiP pane 1 across the middle of fullscreen pane 0 does NOT swap with pane 0.
        assert_eq!(find_swap_target(&slots, 1, slots[1].rect, pos2(960.0, 540.0), screen), None);
        // Dragging PiP pane 1 directly onto PiP pane 2 DOES swap them.
        let p2_center = norm_to_egui(slots[2].rect, screen).center();
        assert_eq!(find_swap_target(&slots, 1, slots[1].rect, p2_center, screen), Some(2));
        // Dragging fullscreen pane 0 onto PiP pane 1 DOES swap them, and clicking pane 0
        // never buries the enclosed PiP panes behind it.
        bring_to_front(&mut slots, 0);
        assert!(slots[1].z > slots[0].z && slots[2].z > slots[0].z, "PiP panes stay above fullscreen background");
        let p1_center = norm_to_egui(slots[1].rect, screen).center();
        assert_eq!(find_swap_target(&slots, 0, slots[0].rect, p1_center, screen), Some(1));
    }

    #[test]
    fn external_info_change_marks_studio_dirty_immediately() {
        crate::i18n::set("en");
        let mut s = LayoutStudio::new();
        let bounds = ScreenRect { left: 0, top: 0, right: 1280, bottom: 720 };
        let idle = StudioPad { connected: true, style: pad::Style::Xbox, ..Default::default() };
        s.open(LayoutPreset::Horizontal.to_slots(2), 0, true, bounds, 1.0, idle);
        let mut info = test_info(2);
        s.last_info = Some(info.clone());
        s.dirty = false;

        // Simulating Sync or Mute toggled by host after tick() returned: next tick sees new info.
        info.sync = false;
        assert_ne!(s.last_info.as_ref(), Some(&info));
        s.last_info = Some(info.clone());
        info.players[0].muted = true;
        assert_ne!(s.last_info.as_ref(), Some(&info));
    }

    #[test]
    fn studio_renders_to_software_canvas_in_mouse_and_controller_modes() {
        crate::i18n::set("en");
        let mut s = LayoutStudio::new();
        let bounds = ScreenRect { left: 0, top: 0, right: 1280, bottom: 720 };
        let info = test_info(3);

        for (using_pad, pad_mode, style) in [
            (false, PadMode::Browse, pad::Style::Xbox),
            (true, PadMode::Browse, pad::Style::Xbox),
            (true, PadMode::Adjust, pad::Style::PlayStation),
        ] {
            let pad = StudioPad { connected: using_pad, style, ..Default::default() };
            s.open(LayoutPreset::TopTwo.to_slots(3), 0, true, bounds, 1.0, pad);
            s.using_pad = using_pad;
            s.pad_mode = pad_mode;
            s.ensure_fonts();
            let screen = s.screen_points();
            let mut actions = Vec::new();
            let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, screen)), time: Some(1.0), ..Default::default() };
            let ctx = s.ctx.clone();
            ctx.set_pixels_per_point(s.ppp);
            let out = ctx.run_ui(raw, |ui| s.draw_ui(ui, screen, &info, false, false, &mut actions));
            let mut rects = Vec::new();
            s.render.draw(&ctx, out, |_| Vec::new(), &mut rects);
            let lit = s.render.canvas.px.iter().filter(|&&px| (px >> 24) > 0).count();
            assert!(lit > 5_000, "studio canvas drew {lit} visible pixels (pad={using_pad}, mode={pad_mode:?})");
        }
    }
}
