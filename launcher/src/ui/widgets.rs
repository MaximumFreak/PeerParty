//! The window's controls, drawn in its own look. Each takes keyboard focus like egui's
//! own widgets, so Tab, the arrow keys and Enter (and the controller, which the window
//! turns into those keys) reach all of them; the focus ring shows only while the
//! keyboard or a controller is what's being used.

use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, EventFilter, Key, Margin, Pos2, Rect, Response, Sense, Stroke, StrokeKind,
    Ui, Vec2, pos2, vec2,
};

use super::fx;
use super::icons::Icon;
use super::theme::{self, ACCENT, ACCENT_HI, CARD, CARD_HI, FIELD, TEXT, TEXT2, TEXT3, VIOLET};

static NAV: AtomicBool = AtomicBool::new(false);

/// The keyboard or a controller is what's in use (focus rings show).
pub fn nav_mode() -> bool {
    NAV.load(Ordering::Relaxed)
}

pub fn set_nav_mode(on: bool) {
    NAV.store(on, Ordering::Relaxed);
}

fn focused(r: &Response) -> bool {
    r.has_focus() && nav_mode()
}

/// The ring for a focused control.
pub fn ring(ui: &Ui, r: &Response, radius: f32) {
    if focused(r) {
        fx::focus_ring(ui.painter(), r.rect, radius);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

/// A button: an optional icon, a label.
pub fn button(ui: &mut Ui, icon: Option<Icon>, label: &str, kind: Kind) -> Response {
    let font = theme::strong(14.0);
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, TEXT);
    let icon_w = if icon.is_some() { if label.is_empty() { 16.0 } else { 24.0 } } else { 0.0 };
    let w = galley.size().x + icon_w + if label.is_empty() { 18.0 } else { 30.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 34.0), Sense::click());
    paint_button(ui, &resp, rect, icon, galley, kind, 8.0);
    resp
}

/// A wide button that fills the width it's given.
pub fn wide_button(ui: &mut Ui, icon: Option<Icon>, label: &str, kind: Kind, height: f32) -> Response {
    let galley = ui.painter().layout_no_wrap(label.to_string(), theme::strong(14.0), TEXT);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    paint_button(ui, &resp, rect, icon, galley, kind, 10.0);
    resp
}

fn paint_button(ui: &Ui, resp: &Response, rect: Rect, icon: Option<Icon>, galley: std::sync::Arc<egui::Galley>, kind: Kind, r: f32) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    let enabled = ui.is_enabled();
    let hot = theme::anim(ui.ctx(), resp.id.with("hot"), enabled && (resp.hovered() || focused(resp)));
    let p = ui.painter();
    let (text, lift) = match kind {
        Kind::Primary => {
            fx::glow(p, rect, r, ACCENT.gamma_multiply(0.18 + 0.32 * hot), 10.0 + 8.0 * hot);
            let a = theme::mix(ACCENT, ACCENT_HI, hot * 0.35);
            let b = theme::mix(theme::mix(ACCENT, VIOLET, 0.55), ACCENT_HI, hot * 0.2);
            fx::gradient(p, rect, r, false, &[(0.0, a), (1.0, b)]);
            fx::gradient(p, rect.shrink(1.0), r - 1.0, true, &[(0.0, Color32::from_white_alpha(40)), (0.5, Color32::TRANSPARENT)]);
            (Color32::WHITE, 0.0)
        }
        Kind::Secondary => {
            p.rect_filled(rect, r, theme::mix(CARD_HI, Color32::from_rgb(0x2e, 0x32, 0x3e), hot));
            if let Some(at) = resp.hover_pos() {
                fx::spotlight(p, rect, r, at, rect.width().max(80.0), Color32::from_white_alpha(14));
            }
            p.rect_stroke(rect, r, Stroke::new(1.0, theme::mix(theme::line(), theme::line_strong(), hot)), StrokeKind::Inside);
            (TEXT, 0.0)
        }
        Kind::Ghost => {
            if hot > 0.0 {
                p.rect_filled(rect, r, Color32::from_white_alpha((14.0 * hot) as u8));
            }
            (theme::mix(TEXT2, TEXT, hot), 0.0)
        }
        Kind::Danger => {
            if hot > 0.0 {
                p.rect_filled(rect, r, theme::BAD.gamma_multiply(0.14 * hot));
            }
            p.rect_stroke(rect, r, Stroke::new(1.0, theme::BAD.gamma_multiply(0.4)), StrokeKind::Inside);
            (theme::BAD, 0.0)
        }
    };
    let text = if enabled { text } else { TEXT3 };
    let total = galley.size().x + if icon.is_some() { if galley.size().x > 0.0 { 24.0 } else { 16.0 } } else { 0.0 };
    let mut x = rect.center().x - total / 2.0;
    let y = rect.center().y - lift;
    if let Some(i) = icon {
        i.paint(p, pos2(x + 8.0, y), 15.0, text);
        x += if galley.size().x > 0.0 { 24.0 } else { 16.0 };
    }
    p.galley(pos2(x, y - galley.size().y / 2.0), galley, text);
    ring(ui, resp, r);
}

/// A square button with only an icon.
pub fn icon_button(ui: &mut Ui, icon: Icon, tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(34.0), Sense::click());
    let hot = theme::anim(ui.ctx(), resp.id.with("hot"), resp.hovered() || focused(&resp));
    let p = ui.painter();
    if hot > 0.0 {
        p.rect_filled(rect, 8.0, Color32::from_white_alpha((16.0 * hot) as u8));
    }
    icon.paint(p, rect.center(), 16.0, theme::mix(TEXT2, TEXT, hot));
    ring(ui, &resp, 8.0);
    if tip.is_empty() { resp } else { resp.on_hover_text(tip) }
}

/// An on / off switch.
pub fn switch(ui: &mut Ui, on: &mut bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(42.0, 24.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = theme::anim(ui.ctx(), resp.id, *on);
    let hot = resp.hovered() || focused(&resp);
    let p = ui.painter();
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
    ring(ui, &resp, r);
    resp
}

/// Options side by side, one chosen. Each is its own focus stop. Returns whether the
/// choice changed.
pub fn segmented<T: PartialEq + Clone>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    let font = theme::strong(13.0);
    let widths: Vec<f32> = options.iter().map(|(_, l)| ui.painter().layout_no_wrap(l.to_string(), font.clone(), TEXT).size().x + 26.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + 6.0;
    let (track, _) = ui.allocate_exact_size(vec2(total, 32.0), Sense::hover());
    ui.painter().rect_filled(track, 9.0, FIELD);
    ui.painter().rect_stroke(track, 9.0, Stroke::new(1.0, theme::line()), StrokeKind::Inside);
    let mut x = track.left() + 3.0;
    for ((v, label), w) in options.iter().zip(widths) {
        let r = Rect::from_min_size(pos2(x, track.top() + 3.0), vec2(w, track.height() - 6.0));
        let id = ui.scope_id().with(("seg", label));
        let resp = ui.interact(r, id, Sense::click());
        if resp.clicked() && *value != *v {
            *value = v.clone();
            changed = true;
        }
        let sel = *value == *v;
        let t = theme::anim(ui.ctx(), id.with("sel"), sel);
        let p = ui.painter();
        if t > 0.0 {
            fx::glow(p, r, 7.0, ACCENT.gamma_multiply(0.2 * t), 6.0);
            fx::gradient(p, r, 7.0, true, &[(0.0, theme::mix(CARD_HI, ACCENT, 0.55).gamma_multiply(t)), (1.0, theme::mix(CARD_HI, ACCENT, 0.35).gamma_multiply(t))]);
        } else if resp.hovered() {
            p.rect_filled(r, 7.0, Color32::from_white_alpha(10));
        }
        p.text(r.center(), Align2::CENTER_CENTER, *label, font.clone(), theme::mix(TEXT2, Color32::WHITE, t));
        ring(ui, &resp, 7.0);
        x += w;
    }
    changed
}

/// A value picked from a list: left / right (or the arrows) step through it, a click
/// opens the whole list. The response says whether it changed.
pub fn stepper(ui: &mut Ui, id_salt: impl std::hash::Hash + std::fmt::Debug, labels: &[String], index: &mut usize, width: f32) -> Response {
    let n = labels.len().max(1);
    let before = *index;
    let (rect, mut resp) = ui.allocate_exact_size(vec2(width, 32.0), Sense::click());
    let id = resp.id.with(id_salt);
    if resp.has_focus() {
        // Left / right change the value here instead of moving the focus.
        ui.memory_mut(|m| m.set_focus_lock_filter(resp.id, EventFilter { horizontal_arrows: true, ..Default::default() }));
        let (l, r) = ui.input(|i| (i.key_pressed(Key::ArrowLeft), i.key_pressed(Key::ArrowRight)));
        if l {
            *index = (*index + n - 1) % n;
        }
        if r {
            *index = (*index + 1) % n;
        }
    }
    let left = Rect::from_min_size(rect.min, vec2(30.0, rect.height()));
    let right = Rect::from_min_size(pos2(rect.right() - 30.0, rect.top()), vec2(30.0, rect.height()));
    let ptr = resp.interact_pointer_pos();
    let mut open_list = false;
    if resp.clicked() {
        match ptr {
            Some(p) if left.contains(p) => *index = (*index + n - 1) % n,
            Some(p) if right.contains(p) => *index = (*index + 1) % n,
            Some(_) => open_list = true,
            None => *index = (*index + 1) % n,
        }
    }
    let hot = resp.hovered() || focused(&resp);
    let p = ui.painter();
    p.rect_filled(rect, 9.0, if hot { CARD_HI } else { FIELD });
    p.rect_stroke(rect, 9.0, Stroke::new(1.0, if hot { theme::line_strong() } else { theme::line() }), StrokeKind::Inside);
    let arrow = |c: Pos2, dir: f32, lit: bool| {
        let s = 4.0;
        let col = if lit { TEXT } else { TEXT3 };
        p.line_segment([c + vec2(dir * -s * 0.5, -s), c + vec2(dir * s * 0.5, 0.0)], Stroke::new(1.6, col));
        p.line_segment([c + vec2(dir * s * 0.5, 0.0), c + vec2(dir * -s * 0.5, s)], Stroke::new(1.6, col));
    };
    let ph = resp.hover_pos();
    arrow(left.center(), -1.0, ph.is_some_and(|q| left.contains(q)) || focused(&resp));
    arrow(right.center(), 1.0, ph.is_some_and(|q| right.contains(q)) || focused(&resp));
    let label = labels.get(*index).map(String::as_str).unwrap_or("");
    let text_rect = rect.shrink2(vec2(32.0, 0.0));
    let galley = p.layout(label.to_string(), theme::font(13.5), TEXT, text_rect.width());
    let at = pos2(text_rect.center().x - galley.size().x / 2.0, rect.center().y - galley.size().y / 2.0);
    p.with_clip_rect(text_rect).galley(at, galley, TEXT);
    ring(ui, &resp, 9.0);
    let popup_id = id.with("list");
    if open_list {
        egui::Popup::open_id(ui.ctx(), popup_id);
    }
    egui::Popup::from_response(&resp)
        .id(popup_id)
        .open_memory(None)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
        .width(width)
        .show(|ui| {
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for (i, l) in labels.iter().enumerate() {
                    if ui.selectable_label(i == *index, l).clicked() {
                        *index = i;
                    }
                }
            });
        });
    if *index != before {
        resp.mark_changed();
    }
    resp
}

/// A one-line text field in the window's look.
pub fn text_field(ui: &mut Ui, value: &mut String, hint: &str, width: f32, limit: usize) -> Response {
    let r = ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(egui::RichText::new(hint).color(TEXT3))
            .char_limit(limit)
            .desired_width(width)
            .margin(Margin::symmetric(10, 7))
            .background_color(FIELD),
    );
    if r.has_focus() {
        fx::glow(ui.painter(), r.rect, 6.0, ACCENT.gamma_multiply(0.25), 8.0);
    }
    r
}

/// A card: a raised surface holding a group of settings or a section.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    card_with(ui, None, add)
}

/// A card lit by `light` from near its top left (a section's own colour).
pub fn card_with<R>(ui: &mut Ui, light: Option<Color32>, add: impl FnOnce(&mut Ui) -> R) -> R {
    let bg = ui.painter().add(egui::Shape::Noop);
    let out = egui::Frame::new().corner_radius(CornerRadius::same(theme::R_LG)).inner_margin(Margin::same(18)).show(ui, add);
    let rect = out.response.rect;
    let r = theme::R_LG as f32;
    let mut shapes = vec![
        egui::Shadow { offset: [0, 6], blur: 22, spread: 0, color: Color32::from_black_alpha(70) }.as_shape(rect, r).into(),
        egui::Shape::rect_filled(rect, r, CARD),
    ];
    if let Some(c) = light {
        let at = pos2(rect.left() + rect.width() * 0.18, rect.top() + 2.0);
        shapes.push(fx::spotlight_shape(rect, r, at, rect.width().max(rect.height()) * 0.75, c.gamma_multiply(0.13)));
    }
    shapes.push(fx::gradient_shape(rect, r, true, &[(0.0, Color32::from_white_alpha(7)), (0.3, Color32::TRANSPARENT)]));
    shapes.push(egui::Shape::rect_stroke(rect, r, Stroke::new(1.0, theme::line()), StrokeKind::Inside));
    ui.painter().set(bg, egui::Shape::Vec(shapes));
    out.inner
}

/// A setting: its name and what it does on the left, its control on the right.
pub fn setting<R>(ui: &mut Ui, title: &str, detail: &str, control: impl FnOnce(&mut Ui) -> R) -> R {
    let mut out = None;
    ui.horizontal(|ui| {
        let ctl_w = 260.0f32.min(ui.available_width() * 0.45);
        let text_w = (ui.available_width() - ctl_w - 16.0).max(120.0);
        ui.vertical(|ui| {
            ui.set_width(text_w);
            ui.add(egui::Label::new(egui::RichText::new(title).font(theme::strong(14.5)).color(TEXT)).wrap());
            if !detail.is_empty() {
                ui.add_space(-4.0);
                ui.add(egui::Label::new(egui::RichText::new(detail).size(12.5).color(TEXT2)).wrap());
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            out = Some(control(ui));
        });
    });
    out.unwrap()
}

/// A hairline between settings in a card.
pub fn divider(ui: &mut Ui) {
    ui.add_space(4.0);
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(r, 0.0, theme::line());
    ui.add_space(4.0);
}

/// A card's title row: an icon in a lit tile, a title, a line under it.
pub fn card_title(ui: &mut Ui, icon: Icon, color: Color32, title: &str, sub: &str) {
    ui.horizontal(|ui| {
        let (tile, _) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::hover());
        let p = ui.painter();
        fx::glow(p, tile, 11.0, color.gamma_multiply(0.22), 12.0);
        fx::gradient(p, tile, 11.0, true, &[(0.0, color.gamma_multiply(0.42)), (1.0, color.gamma_multiply(0.18))]);
        p.rect_stroke(tile, 11.0, Stroke::new(1.0, color.gamma_multiply(0.5)), StrokeKind::Inside);
        icon.paint(p, tile.center(), 18.0, Color32::WHITE);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.add_space(1.0);
            ui.label(egui::RichText::new(title).font(theme::strong(17.0)).color(TEXT));
            if !sub.is_empty() {
                ui.add_space(-6.0);
                ui.add(egui::Label::new(egui::RichText::new(sub).size(12.5).color(TEXT2)).wrap());
            }
        });
    });
    ui.add_space(6.0);
}

/// A small label above a group ("ONLINE SERVICES").
pub fn section(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(egui::RichText::new(title.to_uppercase()).font(theme::strong(11.5)).color(TEXT3));
    ui.add_space(2.0);
}

/// A page's title and what it's for.
pub fn page_title(ui: &mut Ui, title: &str, sub: &str) {
    ui.label(egui::RichText::new(title).font(theme::strong(28.0)).color(TEXT));
    if !sub.is_empty() {
        ui.add_space(-4.0);
        ui.add(egui::Label::new(egui::RichText::new(sub).size(14.0).color(TEXT2)).wrap());
    }
    ui.add_space(14.0);
}

/// A filter pill with an optional count.
pub fn chip(ui: &mut Ui, label: &str, count: Option<usize>, selected: bool) -> Response {
    let font = theme::strong(13.0);
    let g = ui.painter().layout_no_wrap(label.to_string(), font.clone(), TEXT);
    let cg = count.map(|n| ui.painter().layout_no_wrap(n.to_string(), theme::font(12.0), TEXT3));
    let w = g.size().x + 26.0 + cg.as_ref().map_or(0.0, |c| c.size().x + 8.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    let t = theme::anim(ui.ctx(), resp.id.with("sel"), selected);
    let hot = resp.hovered() || focused(&resp);
    let p = ui.painter();
    let r = rect.height() / 2.0;
    if t > 0.0 {
        fx::glow(p, rect, r, ACCENT.gamma_multiply(0.22 * t), 9.0);
        fx::gradient(p, rect, r, true, &[(0.0, theme::mix(CARD_HI, ACCENT, 0.5).gamma_multiply(t)), (1.0, theme::mix(CARD_HI, ACCENT, 0.3).gamma_multiply(t))]);
        p.rect_stroke(rect, r, Stroke::new(1.0, ACCENT_HI.gamma_multiply(0.5 * t)), StrokeKind::Inside);
    } else {
        p.rect_filled(rect, r, if hot { CARD_HI } else { CARD });
        p.rect_stroke(rect, r, Stroke::new(1.0, theme::line()), StrokeKind::Inside);
    }
    let x = rect.left() + 13.0;
    p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g.clone(), theme::mix(TEXT2, Color32::WHITE, t.max(if hot { 0.5 } else { 0.0 })));
    if let Some(c) = cg {
        p.galley(pos2(x + g.size().x + 8.0, rect.center().y - c.size().y / 2.0), c, theme::mix(TEXT3, TEXT2, t));
    }
    ring(ui, &resp, r);
    resp
}

/// A small coloured label ("Steam", "Running").
pub fn badge(p: &egui::Painter, at: Pos2, text: &str, color: Color32, solid: bool) -> Rect {
    let g = p.layout_no_wrap(text.to_string(), theme::strong(11.0), Color32::WHITE);
    let r = Rect::from_min_size(at, vec2(g.size().x + 14.0, 20.0));
    if solid {
        p.rect_filled(r, 10.0, Color32::from_black_alpha(170));
        p.rect_stroke(r, 10.0, Stroke::new(1.0, Color32::from_white_alpha(22)), StrokeKind::Inside);
    } else {
        p.rect_filled(r, 10.0, color.gamma_multiply(0.18));
        p.rect_stroke(r, 10.0, Stroke::new(1.0, color.gamma_multiply(0.45)), StrokeKind::Inside);
    }
    p.galley(pos2(r.left() + 7.0, r.center().y - g.size().y / 2.0), g, if solid { theme::TEXT } else { theme::mix(color, Color32::WHITE, 0.35) });
    r
}

/// A line of text with an icon before it (a status, a note).
pub fn note(ui: &mut Ui, icon: Icon, color: Color32, text: &str) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
        icon.paint(ui.painter(), r.center(), 14.0, color);
        ui.add(egui::Label::new(egui::RichText::new(text).size(13.0).color(if color == TEXT3 { TEXT2 } else { theme::mix(color, TEXT, 0.4) })).wrap());
    });
}
