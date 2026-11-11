//! Play together: who can join you, for every game. People on this network always can;
//! friends with your room code over the internet; anyone online with public sessions.
//! Your name, invites and voice live here too, since they're about other players.

use eframe::egui::{self, Color32, RichText};

use peerparty_core::inet::RoomCode;
use peerparty_core::inet::public::Blocklist;

use super::icons::Icon;
use super::store::Prefs;
use super::theme::{self, ACCENT, GOOD, TEXT, TEXT2, TEXT3, VIOLET};
use super::widgets::{self, Kind};
use super::{App, first_focus};
use crate::t;

#[derive(Default)]
pub struct State {
    /// A friend's code being typed, while joining.
    join: Option<String>,
    /// The code field takes the focus once, when joining starts.
    join_focus: bool,
    show_code: bool,
    /// Blocked public players, counted when the page opens (not every frame).
    blocked: Option<usize>,
    /// The room just left, until the window closes: Leave forgets a code that may be
    /// nowhere else on this PC, and its button sits beside "Show code".
    left: Option<String>,
    /// The players "Unblock all" let back in, until the window closes: one click, on
    /// people blocked one at a time for a reason, can be taken back.
    unblocked: Vec<(String, String)>,
    /// Public sessions were switched on and wait for a yes; the yes takes the focus once.
    confirm_public: bool,
    confirm_focus: bool,
}

impl State {
    pub fn refresh(&mut self) {
        self.blocked = Some(Blocklist::open(Blocklist::default_path()).len());
    }

    /// A room left some other way than Leave (the setup run again without friends):
    /// offered back the same way.
    pub(super) fn left_room(&mut self, room: String) {
        self.left = Some(room);
    }
}

/// The sidebar's line under Play together.
pub fn summary(p: &Prefs) -> String {
    let room = RoomCode::parse(p.room.trim());
    if !p.internet && (room.is_some() || p.public) {
        return t!("together.sum_paused").to_string();
    }
    match (room, p.public) {
        (Some(c), true) => t!("together.sum_room_public", room = c.fingerprint()),
        (Some(c), false) => t!("together.sum_room", room = c.fingerprint()),
        (None, true) => t!("together.sum_public").to_string(),
        (None, false) => t!("together.sum_home").to_string(),
    }
}

/// The public-sessions switch and what it means. Off to on waits for a yes underneath,
/// since it shows this PC's address to strangers; on to off is at once.
fn public_switch(ui: &mut egui::Ui, p: &mut Prefs, st: &mut State) {
    widgets::setting(ui, t!("together.public_switch"), t!("together.public_switch_sub"), |ui| {
        let mut on = p.public;
        let r = widgets::switch(ui, &mut on);
        if r.clicked() {
            st.confirm_public = on;
            st.confirm_focus = on;
            if !on {
                p.public = false;
            }
        }
        #[cfg(test)]
        ui.ctx().data_mut(|d| d.insert_temp(egui::Id::unique("public-switch"), r.rect));
    });
    widgets::note(ui, Icon::Warning, theme::WARN, t!("together.public_text"));
    if st.confirm_public && !p.public {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(t!("together.public_confirm")).color(TEXT));
            let yes = widgets::button(ui, Some(Icon::Globe), t!("together.public_confirm_go"), Kind::Primary);
            if std::mem::take(&mut st.confirm_focus) {
                yes.request_focus();
            }
            if yes.clicked() {
                p.public = true;
                st.confirm_public = false;
            }
            if widgets::button(ui, None, t!("common.cancel"), Kind::Ghost).clicked() {
                st.confirm_public = false;
            }
            #[cfg(test)]
            ui.ctx().data_mut(|d| d.insert_temp(egui::Id::unique("public-yes"), yes.rect));
        });
    }
}

/// A code with its middle hidden: "PP1-AB12-••••-••••-9F".
fn masked(code: &str) -> String {
    let parts: Vec<&str> = code.split('-').collect();
    if parts.len() < 3 {
        return "•".repeat(code.chars().count().min(12));
    }
    let last = parts.len() - 1;
    parts.iter().enumerate().map(|(i, p)| if i <= 1 || i == last { p.to_string() } else { "•".repeat(p.chars().count()) }).collect::<Vec<_>>().join("-")
}

impl App {
    pub(super) fn together_page(&mut self, ui: &mut egui::Ui) {
        if self.together.blocked.is_none() {
            self.together.refresh();
        }
        let before = self.settings.prefs.clone();
        egui::ScrollArea::vertical().id_salt("together").auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(24.0);
            // Never wider than the window (set_max_width can widen a Ui), with a gutter for the scroll bar.
            ui.set_max_width((ui.available_width() - 26.0).min(1040.0));
            widgets::page_title(ui, t!("together.title"), t!("together.sub"));
            self.name_card(ui);
            ui.add_space(14.0);
            let two = ui.available_width() > 880.0;
            if two {
                ui.columns(2, |cols| {
                    self.home_card(&mut cols[0]);
                    cols[0].add_space(14.0);
                    self.public_card(&mut cols[0]);
                    self.room_card(&mut cols[1]);
                    cols[1].add_space(14.0);
                    self.invites_card(&mut cols[1]);
                });
            } else {
                self.home_card(ui);
                ui.add_space(14.0);
                self.room_card(ui);
                ui.add_space(14.0);
                self.public_card(ui);
                ui.add_space(14.0);
                self.invites_card(ui);
            }
            ui.add_space(14.0);
            self.internet_card(ui);
            ui.add_space(30.0);
        });
        if self.settings.prefs != before {
            self.save_settings();
        }
    }

    fn name_card(&mut self, ui: &mut egui::Ui) {
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            let user = crate::platform::user_name();
            let p = &mut self.settings.prefs;
            // A controller lands on the room's buttons, not in a text field.
            widgets::setting(ui, t!("together.name"), t!("together.name_sub"), |ui| {
                widgets::text_field(ui, &mut p.name, &user, 240.0, 32);
            });
        });
    }

    fn home_card(&mut self, ui: &mut egui::Ui) {
        widgets::card_with(ui, Some(GOOD), |ui| {
            ui.set_width(ui.available_width());
            widgets::card_title(ui, Icon::Home, GOOD, t!("together.home"), t!("together.home_sub"));
            ui.add(egui::Label::new(RichText::new(t!("together.home_text")).color(TEXT2)).wrap());
            ui.add_space(6.0);
            widgets::note(ui, Icon::Shield, TEXT3, t!("together.home_firewall"));
        });
    }

    fn room_card(&mut self, ui: &mut egui::Ui) {
        let mut say = None;
        widgets::card_with(ui, Some(ACCENT), |ui| {
            ui.set_width(ui.available_width());
            widgets::card_title(ui, Icon::People, ACCENT, t!("together.room"), t!("together.room_sub"));
            let p = &mut self.settings.prefs;
            let st = &mut self.together;
            let focus = &mut self.focus_first;
            let room = p.room.trim().to_string();
            match RoomCode::parse(&room) {
                Some(code) => {
                    // The room's name, big, and the code itself hidden until asked.
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(t!("together.room_name", room = code.fingerprint())).font(theme::strong(22.0)).color(TEXT));
                    });
                    let shown = if st.show_code { room.clone() } else { masked(&room) };
                    egui::Frame::new().fill(theme::FIELD).corner_radius(10).inner_margin(egui::Margin::symmetric(12, 9)).stroke(egui::Stroke::new(1.0, theme::line())).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(egui::Label::new(RichText::new(shown).monospace().size(13.5).color(TEXT2)).wrap());
                    });
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        let r = widgets::button(ui, Some(Icon::Copy), t!("together.copy_code"), Kind::Primary);
                        first_focus(&r, focus);
                        if r.clicked() {
                            ui.ctx().copy_text(room.clone());
                            say = Some(t!("together.copied").to_string());
                        }
                        let label = if st.show_code { t!("together.hide_code") } else { t!("together.show_code") };
                        if widgets::button(ui, Some(Icon::Hidden), label, Kind::Secondary).clicked() {
                            st.show_code = !st.show_code;
                        }
                        if widgets::button(ui, Some(Icon::Leave), t!("together.leave"), Kind::Danger).on_hover_text(t!("together.leave_tip")).clicked() {
                            st.left = Some(room.clone());
                            p.room.clear();
                            st.show_code = false;
                        }
                    });
                    ui.add_space(4.0);
                    widgets::note(ui, Icon::Lock, TEXT3, t!("together.room_private"));
                }
                None if !room.is_empty() => {
                    widgets::note(ui, Icon::Warning, theme::BAD, t!("together.bad_code"));
                    let r = widgets::button(ui, None, t!("common.clear"), Kind::Secondary);
                    first_focus(&r, focus);
                    if r.clicked() {
                        p.room.clear();
                    }
                }
                None => match &mut st.join {
                    None => {
                        ui.add(egui::Label::new(RichText::new(t!("together.room_text")).color(TEXT2)).wrap());
                        ui.add_space(6.0);
                        ui.horizontal_wrapped(|ui| {
                            let r = widgets::button(ui, Some(Icon::Add), t!("together.create"), Kind::Primary);
                            first_focus(&r, focus);
                            if r.clicked() {
                                p.room = RoomCode::generate().to_string();
                                st.show_code = true;
                            }
                            if widgets::button(ui, Some(Icon::Link), t!("together.join"), Kind::Secondary).clicked() {
                                st.join = Some(String::new());
                                st.join_focus = true;
                            }
                            // The room just left: one press takes it back.
                            if let Some(code) = st.left.as_deref().and_then(RoomCode::parse)
                                && widgets::button(ui, Some(Icon::Leave), &t!("together.rejoin", room = code.fingerprint()), Kind::Ghost).clicked()
                            {
                                p.room = st.left.take().unwrap_or_default();
                            }
                        });
                    }
                    Some(code) => {
                        ui.label(RichText::new(t!("together.join_prompt")).color(TEXT2));
                        let r = widgets::text_field(ui, code, "PP1-XXXX-XXXX-…", ui.available_width(), 64);
                        if std::mem::take(&mut st.join_focus) {
                            r.request_focus();
                        }
                        let valid = RoomCode::parse(code.trim()).is_some();
                        if !code.trim().is_empty() && !valid {
                            widgets::note(ui, Icon::Warning, theme::WARN, t!("together.join_invalid"));
                        }
                        let mut done = None;
                        ui.horizontal_wrapped(|ui| {
                            if ui.add_enabled_ui(valid, |ui| widgets::button(ui, Some(Icon::Check), t!("together.join_go"), Kind::Primary)).inner.clicked() {
                                done = Some(Some(code.trim().to_string()));
                            }
                            if widgets::button(ui, Some(Icon::Paste), t!("together.paste"), Kind::Secondary).clicked() {
                                ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                                r.request_focus();
                            }
                            if widgets::button(ui, None, t!("common.cancel"), Kind::Ghost).clicked() {
                                done = Some(None);
                            }
                        });
                        widgets::note(ui, Icon::Keyboard, TEXT3, t!("together.join_keyboard"));
                        if let Some(d) = done {
                            if let Some(c) = d {
                                p.room = c;
                            }
                            st.join = None;
                        }
                    }
                },
            }
        });
        if let Some(s) = say {
            self.say(s, true);
        }
    }

    fn public_card(&mut self, ui: &mut egui::Ui) {
        let mut unblock = false;
        let mut reblock = false;
        widgets::card_with(ui, Some(VIOLET), |ui| {
            ui.set_width(ui.available_width());
            widgets::card_title(ui, Icon::Globe, VIOLET, t!("together.public"), t!("together.public_sub"));
            let p = &mut self.settings.prefs;
            public_switch(ui, p, &mut self.together);
            if p.public {
                widgets::divider(ui);
                let fallback = if p.name.trim().is_empty() { t!("together.public_name_hint").to_string() } else { p.name.clone() };
                widgets::setting(ui, t!("together.public_name"), "", |ui| {
                    widgets::text_field(ui, &mut p.public_name, &fallback, 200.0, 32);
                });
                widgets::divider(ui);
                widgets::setting(ui, t!("together.public_voice"), t!("together.public_voice_sub"), |ui| {
                    widgets::switch(ui, &mut p.public_voice);
                });
                widgets::divider(ui);
                let n = self.together.blocked.unwrap_or(0);
                widgets::setting(ui, &t!("together.blocked", n = n), t!("together.blocked_sub"), |ui| {
                    if n > 0 && widgets::button(ui, None, t!("together.unblock_all"), Kind::Secondary).clicked() {
                        unblock = true;
                    }
                    if n == 0 && !self.together.unblocked.is_empty() && widgets::button(ui, None, t!("together.block_again"), Kind::Ghost).clicked() {
                        reblock = true;
                    }
                });
            }
        });
        if unblock {
            let blocked = Blocklist::open(Blocklist::default_path());
            let (mut gone, mut kept) = (Vec::new(), 0);
            for (fp, note) in blocked.list() {
                match blocked.remove(&fp) {
                    Ok(_) => gone.push((fp, note)),
                    Err(_) => kept += 1,
                }
            }
            self.together.unblocked = gone;
            self.together.refresh();
            // The list is a file; if it couldn't be written, they are still blocked.
            if kept == 0 { self.say(t!("together.unblocked"), true) } else { self.say(t!("together.unblock_failed"), false) }
        }
        if reblock {
            let blocked = Blocklist::open(Blocklist::default_path());
            let mut back = std::mem::take(&mut self.together.unblocked);
            back.retain(|(fp, note)| blocked.add(fp, note).is_err());
            let ok = back.is_empty();
            self.together.unblocked = back;
            self.together.refresh();
            self.say(if ok { t!("together.blocked_again") } else { t!("together.unblock_failed") }, ok);
        }
    }

    fn invites_card(&mut self, ui: &mut egui::Ui) {
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::card_title(ui, Icon::Bell, theme::WARN, t!("together.invites"), "");
            let p = &mut self.settings.prefs;
            widgets::setting(ui, t!("together.auto_accept"), t!("together.auto_accept_sub"), |ui| {
                widgets::switch(ui, &mut p.auto_accept);
            });
            widgets::divider(ui);
            widgets::setting(ui, t!("together.invite_buttons"), t!("together.invite_buttons_sub"), |ui| {
                let values = ["", "guide", "combo", "both"];
                let labels = vec![t!("together.ib_none").to_string(), t!("together.ib_guide").to_string(), t!("together.ib_combo").to_string(), t!("together.ib_both").to_string()];
                let mut i = values.iter().position(|v| *v == p.invite_buttons).unwrap_or(0);
                if widgets::stepper(ui, "invite_buttons", &labels, &mut i, 220.0).changed() {
                    p.invite_buttons = values[i].to_string();
                }
            });
            widgets::divider(ui);
            widgets::setting(ui, t!("together.voice"), t!("together.voice_sub"), |ui| {
                widgets::switch(ui, &mut p.voice);
            });
        });
    }

    fn internet_card(&mut self, ui: &mut egui::Ui) {
        let p = &mut self.settings.prefs;
        let configured = RoomCode::parse(p.room.trim()).is_some() || p.public;
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_enabled_ui(configured, |ui| {
                widgets::setting(ui, t!("together.internet"), if configured { t!("together.internet_sub") } else { t!("together.internet_unset") }, |ui| {
                    let mut on = p.internet && configured;
                    if widgets::switch(ui, &mut on).changed() {
                        p.internet = on;
                    }
                });
            });
            ui.add_space(2.0);
            widgets::note(ui, Icon::Info, Color32::from_gray(120), t!("together.internet_how"));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Pos2, Rect, vec2};

    /// Turning public sessions on waits for a yes; turning them off doesn't.
    #[test]
    fn public_sessions_ask_before_they_turn_on() {
        crate::i18n::set("en");
        let ctx = egui::Context::default();
        let (mut p, mut st) = (Prefs::default(), State::default());
        p.public = false;
        let mut t = 0.0;
        let mut run = |p: &mut Prefs, st: &mut State, click: Option<&str>| {
            let mut events = Vec::new();
            if let Some(name) = click {
                let r: Rect = ctx.data(|d| d.get_temp(egui::Id::unique(name))).expect(name);
                let at = r.center();
                events.push(egui::Event::PointerMoved(at));
                for pressed in [true, false] {
                    events.push(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
                }
            }
            t += 0.5;
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 700.0))), time: Some(t), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| public_switch(ui, p, st));
            out.textures_delta.clear();
        };
        run(&mut p, &mut st, None);
        run(&mut p, &mut st, Some("public-switch"));
        run(&mut p, &mut st, None);
        assert!(!p.public && st.confirm_public, "switching on asks first");
        run(&mut p, &mut st, Some("public-yes"));
        assert!(p.public && !st.confirm_public, "yes turns them on");
        run(&mut p, &mut st, Some("public-switch"));
        assert!(!p.public && !st.confirm_public, "off is at once");
    }
}
