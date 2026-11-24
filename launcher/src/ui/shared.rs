//! Shared UI primitives, option tables, timers, and background helpers used by both the
//! launcher window (`ui/`) and the in-game overlay host (`host/`).

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Rect, Stroke, StrokeKind, TextureHandle, TextureOptions, pos2, vec2};

use crate::firewall;
use crate::t;
use crate::ui::fx;
use crate::ui::theme::{self, ACCENT, FIELD, VIOLET};

/// Controller glyph style values, in display/cycle order.
pub const GLYPH_VALUES: [&str; 4] = ["auto", "xbox", "playstation", "nintendo"];

/// Translated label for a `glyphs` value.
pub fn glyph_label(value: &str) -> &'static str {
    match value {
        "xbox" => "Xbox",
        "playstation" => "PlayStation",
        "nintendo" => "Nintendo",
        _ => t!("settings.glyphs_auto"),
    }
}

/// Overlay scale presets (75% to 150% in 12.5% steps).
pub const OVERLAY_SCALES: [f32; 7] = [0.75, 0.875, 1.0, 1.125, 1.25, 1.375, 1.5];

/// Opening hotkeys in `Prefs.hotkeys` order: `(display_label, i18n_sub_key)`.
pub const HOTKEY_LABELS: [(&str, &str); 4] = [
    ("Shift+Tab", "settings.key_steam"),
    ("Shift+F3", "settings.key_epic"),
    ("Shift+F1", "settings.key_ea"),
    ("Shift+F2", "settings.key_ubisoft"),
];

/// Paints PeerParty's toggle switch pill at `rect` (`t` in `0.0..=1.0`, `hot` highlighted).
pub fn paint_switch(p: &egui::Painter, rect: Rect, t: f32, hot: bool) {
    let r = rect.height() / 2.0;
    if t > 0.0 {
        fx::glow(p, rect, r, ACCENT.gamma_multiply(0.30 * t), 8.0);
    }
    p.rect_filled(rect, r, theme::mix(FIELD, ACCENT, t));
    if t > 0.0 {
        fx::gradient(p, rect, r, false, &[(0.0, ACCENT.gamma_multiply(t)), (1.0, theme::mix(ACCENT, VIOLET, 0.5).gamma_multiply(t))]);
    }
    p.rect_stroke(rect, r, Stroke::new(1.0, if hot { theme::line_strong() } else { theme::line() }), StrokeKind::Inside);
    let x = egui::lerp(rect.left() + r..=rect.right() - r, t);
    let knob = pos2(x, rect.center().y);
    p.circle_filled(knob + vec2(0.0, 1.0), r - 3.0, Color32::from_black_alpha(70));
    p.circle_filled(knob, r - 3.5, theme::mix(Color32::from_gray(200), Color32::WHITE, t));
}

/// PeerParty's 3-node brand mark (`icon_art`), rendered crisp at `r`'s pixel size and
/// cached on the egui context so both the launcher window and overlay draw the same icon.
pub fn brand_mark(p: &egui::Painter, r: Rect) {
    let ppp = p.ctx().pixels_per_point();
    let n = (r.width() * ppp).round().max(16.0) as usize;
    let id = egui::Id::unique(("brand_mark", n));
    let tex = p.ctx().data(|d| d.get_temp::<TextureHandle>(id)).unwrap_or_else(|| {
        let img = egui::ColorImage::from_rgba_unmultiplied([n, n], &crate::icon_art::icon_rgba(n));
        let tex = p.ctx().load_texture(format!("brand_mark_{n}"), img, TextureOptions::LINEAR);
        p.ctx().data_mut(|d| d.insert_temp(id, tex.clone()));
        tex
    });
    let min = pos2((r.min.x * ppp).round() / ppp, (r.min.y * ppp).round() / ppp);
    let r = Rect::from_min_size(min, vec2(n as f32 / ppp, n as f32 / ppp));
    fx::glow(p, r, r.width() * 14.0 / 64.0, VIOLET.gamma_multiply(0.30), 12.0);
    p.image(tex.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
}

/// D-pad / stick direction repeat timer: fires once after 400 ms, then every 110 ms while
/// the exact same direction mask stays held.
#[derive(Default)]
pub struct RepeatTimer(Option<(u32, Instant)>);

impl RepeatTimer {
    const INITIAL: Duration = Duration::from_millis(400);
    const PERIOD: Duration = Duration::from_millis(110);

    pub fn reset(&mut self) {
        self.0 = None;
    }

    /// Returns any repeated direction bits due at `now` for the currently held `dirs`.
    pub fn step(&mut self, dirs: u32, now: Instant) -> u32 {
        match self.0 {
            Some((d, t)) if d == dirs && dirs != 0 && now.saturating_duration_since(t) > Self::INITIAL => {
                self.0 = Some((d, t + Self::PERIOD));
                dirs
            }
            Some((d, _)) if d == dirs => 0,
            _ => {
                self.0 = (dirs != 0).then_some((dirs, now));
                0
            }
        }
    }
}

/// Runs `firewall::fix_or_open` off the UI/host thread, notifying `on_done` when it finishes.
pub fn spawn_firewall_fix(peerparty: PathBuf, exe: PathBuf, public: bool, on_done: impl FnOnce() + Send + 'static) -> Receiver<(String, bool)> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(firewall::fix_or_open(&peerparty, &exe, public));
        on_done();
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeat_timer_fires_after_initial_delay_and_period() {
        let mut r = RepeatTimer::default();
        let t0 = Instant::now();
        assert_eq!(r.step(1, t0), 0);
        assert_eq!(r.step(1, t0 + Duration::from_millis(350)), 0);
        assert_eq!(r.step(1, t0 + Duration::from_millis(401)), 1);
        assert_eq!(r.step(1, t0 + Duration::from_millis(480)), 0);
        assert_eq!(r.step(1, t0 + Duration::from_millis(515)), 1);
        // Changing direction resets the initial wait.
        assert_eq!(r.step(2, t0 + Duration::from_millis(520)), 0);
        assert_eq!(r.step(2, t0 + Duration::from_millis(700)), 0);
        r.reset();
        assert_eq!(r.step(0, t0 + Duration::from_millis(800)), 0);
    }
}
