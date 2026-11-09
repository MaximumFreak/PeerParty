//! The look the launcher window and the overlay share: colours, type sizes, the fonts,
//! and whether lighting and animations are on.
//!
//! Neutrals lean slightly blue so the accent light reads warm against them; the accent
//! is a clear blue with a violet partner for gradients. Text never sits on pure black.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, vec2};

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

pub const BG: Color32 = rgb(0x0d, 0x0e, 0x12);
pub const SIDEBAR: Color32 = rgb(0x11, 0x12, 0x17);
pub const SURFACE: Color32 = rgb(0x15, 0x17, 0x1d);
pub const CARD: Color32 = rgb(0x1b, 0x1d, 0x25);
pub const CARD_HI: Color32 = rgb(0x23, 0x26, 0x30);
pub const FIELD: Color32 = rgb(0x0f, 0x10, 0x15);
pub const TEXT: Color32 = rgb(0xee, 0xf0, 0xf6);
pub const TEXT2: Color32 = rgb(0xa6, 0xab, 0xbc);
pub const TEXT3: Color32 = rgb(0x6f, 0x75, 0x88);
pub const ACCENT: Color32 = rgb(0x5b, 0x8d, 0xff);
pub const ACCENT_HI: Color32 = rgb(0x86, 0xab, 0xff);
pub const VIOLET: Color32 = rgb(0x9a, 0x72, 0xff);
pub const GOOD: Color32 = rgb(0x3d, 0xd6, 0x9a);
pub const WARN: Color32 = rgb(0xff, 0xbe, 0x55);
pub const BAD: Color32 = rgb(0xff, 0x70, 0x70);

/// Hairlines: card borders and separators.
pub fn line() -> Color32 {
    Color32::from_white_alpha(16)
}

pub fn line_strong() -> Color32 {
    Color32::from_white_alpha(28)
}

pub const R_SM: u8 = 6;
pub const R_MD: u8 = 10;
pub const R_LG: u8 = 14;

static EFFECTS: AtomicBool = AtomicBool::new(true);

/// Lighting and animations: the player's setting and Windows' "Animation effects".
pub fn effects() -> bool {
    EFFECTS.load(Ordering::Relaxed)
}

pub fn set_effects(on: bool) {
    EFFECTS.store(on && crate::platform::animations_enabled(), Ordering::Relaxed);
}

/// How long transitions take (none with effects off).
pub fn anim_time() -> f32 {
    if effects() { 0.16 } else { 0.0 }
}

/// A value easing to `on` (1) or off (0) over a short transition; repaints only while it
/// moves.
pub fn anim(ctx: &egui::Context, id: egui::Id, on: bool) -> f32 {
    ctx.animate_bool_with_time_and_easing(id, on, anim_time(), egui::emath::easing::cubic_out)
}

pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn icons() -> FontFamily {
    FontFamily::Name("icons".into())
}

pub fn font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn strong(size: f32) -> FontId {
    FontId::new(size, semibold())
}

static ICON_FONT: AtomicBool = AtomicBool::new(false);

/// Whether the icon font was found (`icons` draws simple shapes without it).
pub fn have_icon_font() -> bool {
    ICON_FONT.load(Ordering::Relaxed)
}

fn load(f: &crate::platform::FontFile) -> Option<egui::FontData> {
    let bytes = std::fs::read(&f.path).ok()?;
    let mut d = egui::FontData::from_owned(bytes);
    d.index = f.index;
    Some(d)
}

/// The system's interface fonts first (egui's own after them, for what they lack), a
/// semibold family and the icon font; any other script's font is found the first time
/// a character needs it. Called again when the language changes (the provider stays).
pub fn install_fonts(ctx: &egui::Context) {
    let sys = crate::platform::ui_fonts();
    let mut defs = egui::FontDefinitions::default();
    let defaults = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut proportional = defaults.clone();
    if let Some(d) = sys.regular.as_ref().and_then(load) {
        defs.font_data.insert("ui".into(), Arc::new(d));
        proportional.insert(0, "ui".into());
    }
    let mut bold = proportional.clone();
    if let Some(d) = sys.semibold.as_ref().and_then(load) {
        defs.font_data.insert("ui-semibold".into(), Arc::new(d));
        bold.insert(0, "ui-semibold".into());
    }
    let mut icon_family = defaults.clone();
    if let Some(d) = sys.icons.as_ref().and_then(load) {
        defs.font_data.insert("ui-icons".into(), Arc::new(d));
        icon_family.insert(0, "ui-icons".into());
        ICON_FONT.store(true, Ordering::Relaxed);
    }
    defs.families.insert(FontFamily::Proportional, proportional);
    defs.families.insert(semibold(), bold);
    defs.families.insert(icons(), icon_family);
    ctx.set_fonts(defs);
    let once = egui::Id::unique("peerparty-font-provider");
    if ctx.data(|d| d.get_temp::<bool>(once)).is_some() {
        return;
    }
    ctx.data_mut(|d| d.insert_temp(once, true));
    // Each font file is read once and handed to every family that asks for it (egui asks
    // once per family and character, and keeps what it gets in that family's chain).
    let loaded: Mutex<HashMap<PathBuf, Option<egui::FontData>>> = Mutex::new(HashMap::new());
    ctx.add_font_provider(Arc::new(move |req: &egui::FallbackRequest<'_>| -> Option<egui::FontInsert> {
        let c = req.cluster.chars().next()?;
        let f = crate::platform::fallback_font(c, crate::i18n::font_lang())?;
        let data = loaded
            .lock()
            .ok()?
            .entry(f.path.clone())
            .or_insert_with(|| load(&f))
            .clone()?;
        let name = format!("fallback-{}", f.path.file_name()?.to_string_lossy());
        let families = [FontFamily::Proportional, semibold()]
            .into_iter()
            .map(|family| egui::InsertFontFamily { family, priority: egui::FontPriority::Lowest })
            .collect();
        Some(egui::FontInsert::new(&name, data, families))
    }));
}

/// egui's own widgets (text fields, menus, scroll bars, tooltips) in the same look.
pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.window_stroke = Stroke::new(1.0, line_strong());
    v.window_corner_radius = CornerRadius::same(R_MD);
    v.menu_corner_radius = CornerRadius::same(R_MD);
    v.extreme_bg_color = FIELD;
    v.faint_bg_color = CARD;
    v.code_bg_color = FIELD;
    v.selection.bg_fill = ACCENT.gamma_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0, ACCENT_HI);
    v.hyperlink_color = ACCENT_HI;
    v.warn_fg_color = WARN;
    v.error_fg_color = BAD;
    v.popup_shadow = egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(140) };
    v.window_shadow = v.popup_shadow;
    for (w, fill, text) in [
        (&mut v.widgets.noninteractive, SURFACE, TEXT2),
        (&mut v.widgets.inactive, CARD, TEXT),
        (&mut v.widgets.hovered, CARD_HI, TEXT),
        (&mut v.widgets.active, CARD_HI, TEXT),
        (&mut v.widgets.open, CARD_HI, TEXT),
    ] {
        w.corner_radius = CornerRadius::same(R_SM);
        w.weak_bg_fill = fill;
        w.bg_fill = fill;
        w.fg_stroke = Stroke::new(1.0, text);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, line());
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, line());
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, line_strong());
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.spacing.item_spacing = vec2(8.0, 8.0);
        s.spacing.button_padding = vec2(12.0, 6.0);
        s.spacing.interact_size.y = 30.0;
        s.spacing.scroll = egui::style::ScrollStyle::floating();
        s.animation_time = anim_time();
        for (ts, size) in [
            (egui::TextStyle::Body, 14.0),
            (egui::TextStyle::Button, 14.0),
            (egui::TextStyle::Small, 12.0),
            (egui::TextStyle::Heading, 24.0),
            (egui::TextStyle::Monospace, 13.0),
        ] {
            if let Some(f) = s.text_styles.get_mut(&ts) {
                f.size = size;
            }
        }
    });
}

/// `a` to `b` by `t` (0 to 1), in gamma space (what the eye reads as even steps on screen).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

/// A muted colour from a name, so games without art still look apart.
pub fn tint_for(name: &str) -> Color32 {
    let h = name.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    let hue = (h % 360) as f32 / 360.0;
    Color32::from(egui::ecolor::Hsva::new(hue, 0.50, 0.24, 1.0))
}

/// The colour a picture glows with: its pixels' average, weighted toward the vivid ones,
/// brightened to a light's strength.
pub fn glow_of(pixels: &[u8]) -> Option<Color32> {
    let (mut r, mut g, mut b, mut w) = (0f32, 0f32, 0f32, 0f32);
    for p in pixels.chunks_exact(4) {
        if p[3] < 128 {
            continue;
        }
        let (pr, pg, pb) = (p[0] as f32, p[1] as f32, p[2] as f32);
        let max = pr.max(pg).max(pb);
        let min = pr.min(pg).min(pb);
        let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
        let weight = 0.15 + sat * sat * (max / 255.0);
        r += pr * weight;
        g += pg * weight;
        b += pb * weight;
        w += weight;
    }
    if w == 0.0 {
        return None;
    }
    let c = Color32::from_rgb((r / w) as u8, (g / w) as u8, (b / w) as u8);
    let mut hsva = egui::ecolor::Hsva::from(c);
    hsva.v = hsva.v.max(0.75);
    hsva.s = (hsva.s * 1.2).clamp(0.25, 0.85);
    Some(Color32::from(hsva))
}
