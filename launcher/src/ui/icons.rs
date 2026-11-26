//! Interface icons: the system's icon font (Segoe Fluent Icons, or Segoe MDL2 Assets on
//! older Windows, which share these code points), or a plain drawn mark without it.
//!
//! Controller buttons in the style of the pad in use (A / Cross / B on Nintendo's
//! layout...), for the window's and the overlay's button hints.

use eframe::egui::{Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Stroke, Vec2, vec2};

use super::theme;
use crate::host::pad::Style;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Library,
    People,
    Person,
    Globe,
    Home,
    Settings,
    Play,
    Search,
    Add,
    Close,
    Grid,
    List,
    Refresh,
    Folder,
    Check,
    Warning,
    Shield,
    Gamepad,
    Mic,
    Info,
    Lock,
    Copy,
    Link,
    Back,
    ChevronRight,
    Hidden,
    Bolt,
    Keyboard,
    Brush,
    Sliders,
    Bell,
    Wifi,
    Paste,
    Leave,
    Help,
    Code,
    Open,
}

impl Icon {
    pub fn code(self) -> char {
        let c = match self {
            Icon::Library => 0xE8F1,
            Icon::People => 0xE716,
            Icon::Person => 0xE77B,
            Icon::Globe => 0xE774,
            Icon::Home => 0xE80F,
            Icon::Settings => 0xE713,
            Icon::Play => 0xE768,
            Icon::Search => 0xE721,
            Icon::Add => 0xE710,
            Icon::Close => 0xE711,
            Icon::Grid => 0xE8A9,
            Icon::List => 0xE8FD,
            Icon::Refresh => 0xE72C,
            Icon::Folder => 0xE8B7,
            Icon::Check => 0xE73E,
            Icon::Warning => 0xE7BA,
            Icon::Shield => 0xEA18,
            Icon::Gamepad => 0xE7FC,
            Icon::Mic => 0xE720,
            Icon::Info => 0xE946,
            Icon::Lock => 0xE72E,
            Icon::Copy => 0xE8C8,
            Icon::Link => 0xE71B,
            Icon::Back => 0xE72B,
            Icon::ChevronRight => 0xE76C,
            Icon::Hidden => 0xED1A,
            Icon::Bolt => 0xE945,
            Icon::Keyboard => 0xE765,
            Icon::Brush => 0xE771,
            Icon::Sliders => 0xE9E9,
            Icon::Bell => 0xEA8F,
            Icon::Wifi => 0xE701,
            Icon::Paste => 0xE77F,
            Icon::Leave => 0xE89B,
            Icon::Help => 0xE897,
            Icon::Code => 0xE943,
            Icon::Open => 0xE8A7,
        };
        char::from_u32(c).unwrap_or('?')
    }

    /// Draws the icon centred on `c`, `size` points tall.
    pub fn paint(self, p: &Painter, c: Pos2, size: f32, color: Color32) {
        if theme::have_icon_font() {
            p.text(c, Align2::CENTER_CENTER, self.code(), FontId::new(size, theme::icons()), color);
            return;
        }
        // Without the font: a few marks that read well enough at a glance.
        let s = size * 0.42;
        let st = Stroke::new((size / 10.0).max(1.2), color);
        match self {
            Icon::Close => {
                p.line_segment([c + vec2(-s, -s), c + vec2(s, s)], st);
                p.line_segment([c + vec2(-s, s), c + vec2(s, -s)], st);
            }
            Icon::Add => {
                p.line_segment([c + vec2(-s, 0.0), c + vec2(s, 0.0)], st);
                p.line_segment([c + vec2(0.0, -s), c + vec2(0.0, s)], st);
            }
            Icon::Play => {
                let pts = vec![c + vec2(-s * 0.7, -s), c + vec2(s, 0.0), c + vec2(-s * 0.7, s)];
                p.add(eframe::egui::Shape::convex_polygon(pts, color, Stroke::NONE));
            }
            Icon::Check => {
                p.line_segment([c + vec2(-s, 0.0), c + vec2(-s * 0.3, s * 0.7)], st);
                p.line_segment([c + vec2(-s * 0.3, s * 0.7), c + vec2(s, -s * 0.7)], st);
            }
            Icon::ChevronRight => {
                p.line_segment([c + vec2(-s * 0.4, -s), c + vec2(s * 0.4, 0.0)], st);
                p.line_segment([c + vec2(s * 0.4, 0.0), c + vec2(-s * 0.4, s)], st);
            }
            Icon::Warning => {
                let pts = vec![c + vec2(0.0, -s), c + vec2(s, s), c + vec2(-s, s), c + vec2(0.0, -s)];
                p.add(eframe::egui::Shape::line(pts, st));
            }
            _ => {
                p.circle_stroke(c, s * 0.8, st);
            }
        }
    }
}

/// A controller button.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Glyph {
    South,
    East,
    West,
    North,
    Lb,
    Rb,
    Lt,
    Rt,
    Back,
    Start,
    Dpad,
    LStick,
    RStick,
}

/// Visual width (in points) of `g` when drawn by [`glyph`].
pub fn glyph_width(g: Glyph) -> f32 {
    match g {
        Glyph::Lb | Glyph::Rb | Glyph::Lt | Glyph::Rt => 26.0,
        Glyph::Back | Glyph::Start => 34.0,
        Glyph::LStick | Glyph::RStick => 20.0,
        Glyph::South | Glyph::East | Glyph::West | Glyph::North | Glyph::Dpad => 18.0,
    }
}

/// A controller button in the style of the pad in use, centred on `c`.
pub fn glyph(p: &Painter, c: Pos2, g: Glyph, style: Style) {
    let r = 9.0;
    let face = |label: &str, fill: Color32| {
        p.circle_filled(c, r, fill);
        p.text(c, Align2::CENTER_CENTER, label, theme::strong(11.0), Color32::WHITE);
    };
    let pill = |label: &str, w: f32| {
        let rect = Rect::from_center_size(c, Vec2::new(w, 16.0));
        p.rect_filled(rect, CornerRadius::same(5), Color32::from_rgb(64, 70, 86));
        p.text(c, Align2::CENTER_CENTER, label, theme::strong(9.5), theme::TEXT);
    };
    match g {
        Glyph::Lb | Glyph::Rb => {
            let label = match (g, style) {
                (Glyph::Lb, Style::PlayStation) => "L1",
                (Glyph::Rb, Style::PlayStation) => "R1",
                (Glyph::Lb, Style::Nintendo) => "L",
                (Glyph::Rb, Style::Nintendo) => "R",
                (Glyph::Lb, _) => "LB",
                _ => "RB",
            };
            pill(label, 26.0);
        }
        Glyph::Lt | Glyph::Rt => {
            let label = match (g, style) {
                (Glyph::Lt, Style::PlayStation) => "L2",
                (Glyph::Rt, Style::PlayStation) => "R2",
                (Glyph::Lt, Style::Nintendo) => "ZL",
                (Glyph::Rt, Style::Nintendo) => "ZR",
                (Glyph::Lt, _) => "LT",
                _ => "RT",
            };
            pill(label, 26.0);
        }
        Glyph::Back => {
            let label = match style {
                Style::PlayStation => "Share",
                Style::Nintendo => "−",
                Style::Xbox => "View",
            };
            pill(label, 34.0);
        }
        Glyph::Start => {
            let label = match style {
                Style::PlayStation => "Opt",
                Style::Nintendo => "+",
                Style::Xbox => "Menu",
            };
            pill(label, 34.0);
        }
        Glyph::Dpad => {
            let col = Color32::from_rgb(180, 188, 206);
            p.rect_filled(Rect::from_center_size(c, Vec2::new(14.0, 5.0)), CornerRadius::same(1), col);
            p.rect_filled(Rect::from_center_size(c, Vec2::new(5.0, 14.0)), CornerRadius::same(1), col);
            p.circle_filled(c, 1.6, Color32::from_rgb(36, 40, 50));
        }
        Glyph::LStick | Glyph::RStick => {
            let label = match (style, g) {
                (Style::Nintendo, Glyph::LStick) => "L",
                (Style::Nintendo, _) => "R",
                (_, Glyph::LStick) => "LS",
                _ => "RS",
            };
            p.circle_filled(c, 9.5, Color32::from_rgb(56, 62, 78));
            p.circle_stroke(c, 9.5, Stroke::new(1.2, Color32::from_rgb(130, 140, 165)));
            p.text(c, Align2::CENTER_CENTER, label, theme::strong(9.5), theme::TEXT);
        }
        _ => match style {
            Style::Xbox => match g {
                Glyph::South => face("A", Color32::from_rgb(56, 160, 72)),
                Glyph::East => face("B", Color32::from_rgb(200, 60, 60)),
                Glyph::West => face("X", Color32::from_rgb(50, 110, 210)),
                _ => face("Y", Color32::from_rgb(210, 160, 40)),
            },
            Style::Nintendo => {
                let grey = Color32::from_rgb(90, 96, 110);
                match g {
                    Glyph::South => face("B", grey),
                    Glyph::East => face("A", grey),
                    Glyph::West => face("Y", grey),
                    _ => face("X", grey),
                }
            }
            Style::PlayStation => {
                p.circle_filled(c, r, Color32::from_rgb(40, 44, 54));
                let s = 4.0;
                match g {
                    Glyph::South => {
                        let col = Color32::from_rgb(120, 160, 230);
                        p.line_segment([c + Vec2::new(-s, -s), c + Vec2::new(s, s)], Stroke::new(1.6, col));
                        p.line_segment([c + Vec2::new(-s, s), c + Vec2::new(s, -s)], Stroke::new(1.6, col));
                    }
                    Glyph::East => {
                        p.circle_stroke(c, s, Stroke::new(1.6, Color32::from_rgb(230, 100, 110)));
                    }
                    Glyph::West => {
                        p.rect_stroke(Rect::from_center_size(c, Vec2::splat(s * 2.0)), 0.0, Stroke::new(1.6, Color32::from_rgb(220, 130, 200)), eframe::egui::StrokeKind::Middle);
                    }
                    _ => {
                        let col = Color32::from_rgb(80, 200, 170);
                        let pts = vec![c + vec2(0.0, -s), c + vec2(s, s * 0.8), c + vec2(-s, s * 0.8), c + vec2(0.0, -s)];
                        p.add(eframe::egui::Shape::line(pts, Stroke::new(1.6, col)));
                    }
                }
            }
        },
    }
}

/// A row of button hints ("A Play", "B Back"), starting at `at` (its left middle).
/// Returns where it ended.
pub fn hints(p: &Painter, at: Pos2, items: &[(Glyph, &str)], style: Style, color: Color32) -> f32 {
    let mut x = at.x;
    for (g, label) in items {
        let w = glyph_width(*g) * 0.5;
        glyph(p, Pos2::new(x + w, at.y), *g, style);
        let t = p.text(Pos2::new(x + w * 2.0 + 7.0, at.y), Align2::LEFT_CENTER, *label, theme::font(13.0), color);
        x = t.right() + 18.0;
    }
    x
}
