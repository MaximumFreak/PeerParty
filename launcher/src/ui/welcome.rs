//! The first-run setup, over the library: the language and your name, then who you
//! play with (and voice and invites to match), then what was set. Nothing is written
//! until Start; Back never writes. Every step works with a controller.

use eframe::egui::{self, Color32, RichText, Sense, Vec2, pos2, vec2};

use peerparty_core::inet::RoomCode;

use super::icons::Icon;
use super::store::Prefs;
use super::theme::{self, ACCENT, ACCENT_HI, GOOD, TEXT, TEXT2, TEXT3, VIOLET};
use super::widgets::{self, Kind};
use super::{App, View, first_focus, fx};
use crate::t;

pub struct Welcome {
    pub(super) step: usize,
    name: String,
    language: String,
    friends: bool,
    public: bool,
    voice: bool,
    public_voice: bool,
    auto_accept: bool,
}

impl Welcome {
    pub fn new(p: &Prefs) -> Welcome {
        Welcome {
            step: 0,
            name: p.name.clone(),
            language: p.language.clone(),
            friends: RoomCode::parse(p.room.trim()).is_some(),
            public: p.public,
            voice: p.voice,
            // Recommended off with strangers, whatever the stored default.
            public_voice: false,
            auto_accept: if p.public { p.auto_accept } else { true },
        }
    }

    pub fn back(&mut self) {
        self.step = self.step.saturating_sub(1);
    }

    /// The answers, as settings.
    pub fn apply(&self, p: &mut Prefs) {
        p.name = self.name.trim().to_string();
        p.language = self.language.clone();
        if self.friends {
            if RoomCode::parse(p.room.trim()).is_none() {
                p.room = RoomCode::generate().to_string();
            }
        } else {
            p.room.clear();
        }
        p.public = self.public;
        if self.public {
            p.public_voice = self.public_voice;
        }
        p.voice = self.voice;
        p.auto_accept = self.auto_accept;
        p.internet = true;
    }
}

/// A big choice the setup offers: an icon, what it is, and whether it's picked.
fn choice(ui: &mut egui::Ui, icon: Icon, color: Color32, title: &str, sub: &str, on: bool, fixed: bool) -> egui::Response {
    let w = ui.available_width();
    let galley = ui.painter().layout(sub.to_string(), theme::font(12.5), TEXT2, w - 120.0);
    let h = (galley.size().y + 46.0).max(70.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), if fixed { Sense::hover() } else { Sense::click() });
    let t = theme::anim(ui.ctx(), resp.id.with("on"), on);
    let hot = theme::anim(ui.ctx(), resp.id.with("hot"), resp.hovered() && !fixed);
    let p = ui.painter();
    if t > 0.0 {
        fx::glow(p, rect, 14.0, color.gamma_multiply(0.28 * t), 16.0);
    }
    fx::gradient(p, rect, 14.0, false, &[(0.0, theme::mix(theme::CARD_HI, color, 0.16 * t + 0.04 * hot)), (1.0, theme::CARD)]);
    if let Some(at) = super::hover_in(ui, rect) {
        fx::spotlight(p, rect, 14.0, at, 260.0, Color32::from_white_alpha((14.0 * hot) as u8));
    }
    p.rect_stroke(rect, 14.0, egui::Stroke::new(1.0 + t, theme::mix(Color32::from_white_alpha(18), color, t * 0.8)), egui::StrokeKind::Inside);
    let tile = egui::Rect::from_center_size(pos2(rect.left() + 38.0, rect.center().y), Vec2::splat(40.0));
    fx::gradient(p, tile, 11.0, true, &[(0.0, color.gamma_multiply(0.45)), (1.0, color.gamma_multiply(0.2))]);
    icon.paint(p, tile.center(), 19.0, Color32::WHITE);
    p.text(pos2(rect.left() + 72.0, rect.top() + 24.0), egui::Align2::LEFT_CENTER, title, theme::strong(15.5), TEXT);
    p.galley(pos2(rect.left() + 72.0, rect.top() + 38.0), galley, TEXT2);
    // The tick on the right.
    let c = pos2(rect.right() - 30.0, rect.center().y);
    if t > 0.0 {
        p.circle_filled(c, 12.0, color.gamma_multiply(t));
        Icon::Check.paint(p, c, 12.0, Color32::WHITE.gamma_multiply(t));
    } else {
        p.circle_stroke(c, 11.0, egui::Stroke::new(1.5, Color32::from_white_alpha(60)));
    }
    widgets::ring(ui, &resp, 14.0);
    resp
}

impl App {
    pub(super) fn welcome_modal(&mut self, ctx: &egui::Context) {
        let Some(mut w) = self.welcome.take() else { return };
        let mut done: Option<bool> = None;
        let screen = ctx.content_rect();
        let frame = egui::Frame::new().fill(theme::SURFACE).corner_radius(18).inner_margin(egui::Margin::same(30)).stroke(egui::Stroke::new(1.0, theme::line_strong()));
        let modal = egui::Modal::new(egui::Id::unique("welcome")).frame(egui::Frame::NONE).backdrop_color(Color32::from_black_alpha(175)).show(ctx, |ui| {
            let width = (screen.width() - 80.0).clamp(420.0, 640.0);
            let shell = ui.painter().add(egui::Shape::Noop);
            let out = frame.show(ui, |ui| {
                ui.set_width(width);
                // Where you are in the setup: three dots.
                ui.horizontal(|ui| {
                    for i in 0..3 {
                        let (r, _) = ui.allocate_exact_size(vec2(if i == w.step { 26.0 } else { 8.0 }, 8.0), Sense::hover());
                        let c = if i <= w.step { ACCENT_HI } else { Color32::from_white_alpha(40) };
                        if i == w.step {
                            fx::glow(ui.painter(), r, 4.0, ACCENT.gamma_multiply(0.6), 8.0);
                        }
                        ui.painter().rect_filled(r, 4.0, c);
                    }
                });
                ui.add_space(16.0);
                match w.step {
                    0 => self.welcome_hello(ui, &mut w, &mut done),
                    1 => self.welcome_who(ui, &mut w),
                    _ => self.welcome_done(ui, &w),
                }
                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    if w.step == 0 {
                        if widgets::button(ui, None, t!("welcome.skip"), Kind::Ghost).clicked() {
                            done = Some(false);
                        }
                    } else if widgets::button(ui, Some(Icon::Back), t!("common.back"), Kind::Ghost).clicked() {
                        w.back();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let last = w.step >= 2;
                        let label = if last { t!("welcome.start") } else { t!("common.next") };
                        let r = widgets::button(ui, Some(if last { Icon::Play } else { Icon::ChevronRight }), label, Kind::Primary);
                        if w.step > 0 {
                            first_focus(&r, &mut self.focus_first);
                        }
                        if r.clicked() {
                            if last {
                                done = Some(true);
                            } else {
                                w.step += 1;
                                self.focus_first = true;
                            }
                        }
                    });
                });
            });
            // The card's own light: a bloom round it and a glow from its top.
            let r = out.response.rect;
            ui.painter().set(
                shell,
                egui::Shape::Vec(vec![
                    fx::glow_shape(r, 18.0, ACCENT.gamma_multiply(0.16), 46.0),
                    egui::Shadow { offset: [0, 18], blur: 48, spread: 0, color: Color32::from_black_alpha(160) }.as_shape(r, 18.0).into(),
                ]),
            );
            let p = ui.painter();
            fx::spotlight(p, r, 18.0, pos2(r.center().x, r.top() + 2.0), r.width() * 0.6, ACCENT.gamma_multiply(0.12));
        });
        let _ = modal;
        match done {
            Some(apply) => {
                if apply {
                    w.apply(&mut self.settings.prefs);
                    crate::i18n::set(&self.settings.prefs.language);
                    if w.friends {
                        self.open_view(View::Together, false);
                        self.say(t!("welcome.room_ready"), true);
                    }
                }
                self.settings.onboarded = true;
                self.save_settings();
            }
            None => self.welcome = Some(w),
        }
    }

    fn welcome_hello(&mut self, ui: &mut egui::Ui, w: &mut Welcome, _done: &mut Option<bool>) {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(54.0, 54.0), Sense::hover());
            super::brand_mark(ui.painter(), r);
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(t!("welcome.title")).font(theme::strong(26.0)).color(TEXT));
                ui.add_space(-6.0);
                ui.label(RichText::new(t!("welcome.sub")).color(TEXT2));
            });
        });
        ui.add_space(18.0);
        ui.add(egui::Label::new(RichText::new(t!("welcome.intro")).color(TEXT2)).wrap());
        ui.add_space(16.0);
        ui.label(RichText::new(t!("settings.language")).font(theme::strong(14.0)).color(TEXT));
        let mut labels = vec![t!("settings.language_auto").to_string()];
        labels.extend(crate::i18n::LANGS.iter().map(|l| l.1.to_string()));
        let mut i = crate::i18n::LANGS.iter().position(|l| l.0 == w.language).map_or(0, |i| i + 1);
        let r = widgets::stepper(ui, "welcome-lang", &labels, &mut i, ui.available_width());
        first_focus(&r, &mut self.focus_first);
        if r.changed() {
            w.language = if i == 0 { String::new() } else { crate::i18n::LANGS[i - 1].0.to_string() };
            // Shown in the new language straight away (saved only on Start).
            crate::i18n::set(&w.language);
            theme::install_fonts(ui.ctx());
        }
        ui.add_space(12.0);
        ui.label(RichText::new(t!("together.name")).font(theme::strong(14.0)).color(TEXT));
        widgets::text_field(ui, &mut w.name, &crate::platform::user_name(), ui.available_width(), 32);
        ui.label(RichText::new(t!("welcome.name_sub")).size(12.5).color(TEXT3));
    }

    fn welcome_who(&mut self, ui: &mut egui::Ui, w: &mut Welcome) {
        ui.label(RichText::new(t!("welcome.who")).font(theme::strong(24.0)).color(TEXT));
        ui.label(RichText::new(t!("welcome.who_sub")).color(TEXT2));
        ui.add_space(14.0);
        choice(ui, Icon::Home, GOOD, t!("together.home"), t!("welcome.home"), true, true);
        ui.add_space(8.0);
        if choice(ui, Icon::People, ACCENT, t!("together.room"), t!("welcome.friends"), w.friends, false).clicked() {
            w.friends = !w.friends;
        }
        ui.add_space(8.0);
        if choice(ui, Icon::Globe, VIOLET, t!("together.public"), t!("welcome.public"), w.public, false).clicked() {
            w.public = !w.public;
            // Strangers: invites asked about, not taken.
            w.auto_accept = !w.public;
        }
        ui.add_space(14.0);
        widgets::setting(ui, t!("together.voice"), t!("welcome.voice_sub"), |ui| {
            widgets::switch(ui, &mut w.voice);
        });
        if w.public {
            widgets::setting(ui, t!("together.public_voice"), t!("welcome.public_voice_sub"), |ui| {
                widgets::switch(ui, &mut w.public_voice);
            });
        }
        widgets::setting(ui, t!("together.auto_accept"), if w.public { t!("welcome.auto_accept_public") } else { t!("welcome.auto_accept_home") }, |ui| {
            widgets::switch(ui, &mut w.auto_accept);
        });
    }

    fn welcome_done(&mut self, ui: &mut egui::Ui, w: &Welcome) {
        ui.label(RichText::new(t!("welcome.done")).font(theme::strong(24.0)).color(TEXT));
        ui.label(RichText::new(t!("welcome.done_sub")).color(TEXT2));
        ui.add_space(14.0);
        let mut line = |icon: Icon, color: Color32, text: &str| {
            widgets::note(ui, icon, color, text);
            ui.add_space(2.0);
        };
        line(Icon::Home, GOOD, t!("welcome.sum_home"));
        line(Icon::People, if w.friends { ACCENT } else { TEXT3 }, if w.friends { t!("welcome.sum_friends") } else { t!("welcome.sum_no_friends") });
        line(Icon::Globe, if w.public { VIOLET } else { TEXT3 }, if w.public { t!("welcome.sum_public") } else { t!("welcome.sum_no_public") });
        line(Icon::Mic, if w.voice { GOOD } else { TEXT3 }, if w.voice { t!("welcome.sum_voice") } else { t!("welcome.sum_no_voice") });
        ui.add_space(10.0);
        widgets::note(ui, Icon::Gamepad, TEXT3, t!("welcome.tip_overlay"));
        widgets::note(ui, Icon::Settings, TEXT3, t!("welcome.tip_change"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The answers become settings only through `apply`: friends make a room code
    /// (keeping one there is), none clears it, strangers get their own voice answer.
    #[test]
    fn answers_become_settings() {
        let mut p = Prefs::default();
        let mut w = Welcome::new(&p);
        assert!(!w.friends && !w.public && w.auto_accept && !w.public_voice);
        w.name = " Kim ".into();
        w.friends = true;
        w.public = true;
        w.voice = false;
        w.apply(&mut p);
        assert_eq!(p.name, "Kim");
        let code = p.room.clone();
        assert!(RoomCode::parse(&code).is_some());
        assert!(p.public && !p.public_voice && !p.voice);
        w.apply(&mut p);
        assert_eq!(p.room, code, "an existing room is kept");
        w.friends = false;
        w.apply(&mut p);
        assert!(p.room.is_empty());
    }
}
