//! A game's panel: its picture lit in its own colour, Play, who can join, and what's
//! particular to this game, the rarely needed parts folded away.

use std::time::Duration;

use eframe::egui::{self, Color32, CornerRadius, Rect, Sense, Vec2, pos2, vec2};

use super::icons::Icon;
use super::scan::Source;
use super::settings::{layers, mode_labels, mode_values, sdk_name};
use super::store::{GameOptions, SDK_MODES};
use super::theme::{self, ACCENT, ACCENT_HI, TEXT, TEXT2, TEXT3};
use super::widgets::{self, Kind};
use super::{Action, App, View, first_focus, fx, store};
use crate::t;

/// How long the Play button's spark lasts.
const SPARK: Duration = Duration::from_millis(900);

impl App {
    pub(super) fn details(&mut self, ui: &mut egui::Ui, key: String) {
        let Some(g) = self.game(&key).cloned() else {
            self.selected = None;
            return;
        };
        let mut o = self.settings.options(&key);
        let before = o.clone();
        let mut actions = Vec::new();
        let mut close = false;
        let full = ui.max_rect();
        let glow = self.glow_of(&g, &key);
        {
            let p = ui.painter();
            fx::light(p, pos2(full.center().x, full.top() + 120.0), vec2(full.width() * 0.85, 300.0), glow.gamma_multiply(0.20));
            p.rect_filled(Rect::from_min_max(full.left_top(), pos2(full.left() + 1.0, full.bottom())), 0.0, theme::line());
        }
        egui::ScrollArea::vertical().id_salt("game").auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::new().inner_margin(egui::Margin { left: 22, right: 22, top: 14, bottom: 22 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if widgets::icon_button(ui, Icon::Close, t!("game.close")).clicked() {
                        close = true;
                    }
                });
                // The picture.
                let w = ui.available_width();
                let (pw, ph) = if g.cover.is_some() { (200.0, 300.0) } else { (w, 170.0) };
                ui.vertical_centered(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(pw, ph), Sense::hover());
                    let p = ui.painter();
                    fx::shadow(p, rect, 14.0, 30, 12, 150);
                    fx::glow(p, rect, 14.0, glow.gamma_multiply(0.30), 34.0);
                    let mut need = Vec::new();
                    self.paint_picture(ui, &g, &key, rect, CornerRadius::same(14), &mut need);
                    ui.painter().rect_stroke(rect, 14.0, egui::Stroke::new(1.0, Color32::from_white_alpha(22)), egui::StrokeKind::Inside);
                    self.request_icons(need);
                });
                ui.add_space(16.0);
                ui.add(egui::Label::new(egui::RichText::new(&g.name).font(theme::strong(23.0)).color(TEXT)).wrap());
                let running = self.running(&key);
                ui.horizontal_wrapped(|ui| {
                    let origin = self.origin(&g, &key);
                    let (r, _) = ui.allocate_exact_size(vec2(ui.painter().layout_no_wrap(origin.to_string(), theme::strong(11.0), TEXT).size().x + 14.0, 20.0), Sense::hover());
                    widgets::badge(ui.painter(), r.min, origin, TEXT2, false);
                    if !running.is_empty() {
                        let label = if running.len() > 1 { t!("game.running_n", n = running.len()) } else { t!("game.running").to_string() };
                        let (r, _) = ui.allocate_exact_size(vec2(ui.painter().layout_no_wrap(label.clone(), theme::strong(11.0), TEXT).size().x + 14.0, 20.0), Sense::hover());
                        widgets::badge(ui.painter(), r.min, &label, theme::GOOD, false);
                    }
                });
                ui.add_space(14.0);
                self.play_button(ui, &g, &key, &o, &running, &mut actions);
                ui.add_space(10.0);
                let xbox = g.source == Source::Xbox;
                if !xbox {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(t!("game.players_here")).size(13.0).color(TEXT2));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let mut n = o.players.clamp(1, 2);
                            if widgets::segmented(ui, &mut n, &[(1u8, "1"), (2u8, "2")]) {
                                o.players = n;
                            }
                        });
                    });
                    if o.players >= 2 {
                        ui.add(egui::Label::new(egui::RichText::new(t!("game.players_two_note")).size(12.0).color(TEXT3)).wrap());
                    }
                } else {
                    widgets::note(ui, Icon::Info, TEXT3, t!("game.xbox_one_copy"));
                }
                ui.add_space(12.0);
                self.who_can_join(ui, &key, &mut actions);
                ui.add_space(12.0);
                let own = o.auto_accept.is_some() || o.overlay.is_some() || o.voice.is_some() || o.steam_saves.is_some();
                fold(ui, "game-own", Icon::Sliders, t!("game.own_title"), t!("game.own_sub"), own, |ui| {
                    let p = self.settings.prefs.clone();
                    tri(ui, t!("game.own_auto_accept"), &mut o.auto_accept, p.auto_accept);
                    tri(ui, t!("game.own_overlay"), &mut o.overlay, p.overlay);
                    tri(ui, t!("game.own_voice"), &mut o.voice, p.voice);
                    tri(ui, t!("game.own_saves"), &mut o.steam_saves, p.steam_saves);
                });
                ui.add_space(6.0);
                let adv = advanced_changed(&o);
                fold(ui, "game-advanced", Icon::Code, t!("game.adv_title"), t!("game.adv_sub"), adv, |ui| {
                    self.advanced(ui, &g, &key, &mut o, &mut actions);
                });
                ui.add_space(14.0);
                ui.horizontal_wrapped(|ui| {
                    if widgets::button(ui, Some(Icon::Folder), t!("game.show_in_folder"), Kind::Ghost).clicked() {
                        actions.push(Action::Reveal(g.exe.clone()));
                    }
                    let hidden = self.hidden(&key);
                    if widgets::button(ui, Some(Icon::Hidden), if hidden { t!("game.unhide") } else { t!("game.hide") }, Kind::Ghost).clicked() {
                        actions.push(Action::Hide(key.clone(), !hidden));
                    }
                });
            });
        });
        if close {
            self.selected = None;
        }
        if o != before {
            if o == GameOptions::default() {
                self.settings.games.remove(&key);
            } else {
                self.settings.games.insert(key.clone(), o);
            }
            self.save_settings();
        }
        self.apply(actions);
    }

    fn play_button(&mut self, ui: &mut egui::Ui, g: &super::scan::Game, key: &str, o: &GameOptions, running: &[u8], actions: &mut Vec<Action>) {
        // An Xbox app game runs one copy per PC: Windows won't start another while one
        // runs, so the button says so instead.
        let xbox = g.source == Source::Xbox;
        let label = match (o.players >= 2 && !xbox, running.is_empty(), xbox) {
            (_, false, true) => t!("game.already_running"),
            (true, true, _) => t!("game.play_two"),
            (false, true, _) => t!("game.play"),
            (true, false, _) => t!("game.play_two_more"),
            (false, false, _) => t!("game.play_another"),
        };
        let enabled = !(xbox && !running.is_empty());
        let resp = ui.add_enabled_ui(enabled, |ui| widgets::wide_button(ui, Some(Icon::Play), label, Kind::Primary, 54.0)).inner;
        first_focus(&resp, &mut self.focus_play);
        let resp = if !enabled {
            resp.on_disabled_hover_text(t!("game.xbox_running_tip"))
        } else if !running.is_empty() {
            resp.on_hover_text(t!("game.play_another_tip"))
        } else {
            resp
        };
        if resp.clicked() {
            actions.push(Action::Play(key.to_string()));
        }
        // The spark: electricity round the button, and a bolt off each end.
        if let Some((at, seed)) = self.spark {
            let e = at.elapsed();
            if e < SPARK {
                let t = e.as_secs_f32() / SPARK.as_secs_f32();
                let crackle = seed ^ (e.as_millis() as u64 / 45);
                let r = resp.rect;
                let p = ui.painter();
                fx::electric_edge(p, r, 10.0, t, crackle, ACCENT_HI);
                let s = (1.0 - t).powi(2) * (t * 10.0).min(1.0);
                fx::bolt(p, r.left_center(), r.left_center() + vec2(-24.0, -18.0), crackle.wrapping_add(1), s, ACCENT_HI);
                fx::bolt(p, r.right_center(), r.right_center() + vec2(24.0, 16.0), crackle.wrapping_add(2), s, theme::VIOLET);
                fx::glow(p, r, 10.0, ACCENT_HI.gamma_multiply(0.5 * s), 26.0);
                ui.ctx().request_repaint();
            } else {
                self.spark = None;
            }
        }
    }

    /// Who can play with you in this game: people on this network (unless the firewall
    /// keeps them out, fixed here), friends with your room code, anyone online.
    fn who_can_join(&mut self, ui: &mut egui::Ui, key: &str, actions: &mut Vec<Action>) {
        let fw = self.firewall_for(key);
        let prefs = self.settings.prefs.clone();
        let mut open_together = false;
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(t!("join.title")).font(theme::strong(15.0)).color(TEXT));
            ui.add_space(2.0);
            let (home_color, home_status) = match &fw {
                Some((v, _)) if v.blocked() => (theme::WARN, t!("join.blocked").to_string()),
                Some((v, _)) if !v.allowed => (theme::WARN, t!("join.not_allowed_yet").to_string()),
                _ => (theme::GOOD, t!("join.always_on").to_string()),
            };
            join_row(ui, Icon::Home, t!("join.home"), &home_status, home_color);
            if let Some((v, exe)) = &fw {
                if v.blocked() || !v.allowed {
                    let text = v.warning().or_else(|| v.note()).unwrap_or_default();
                    ui.add(egui::Label::new(egui::RichText::new(text).size(12.0).color(TEXT2)).wrap());
                    ui.horizontal_wrapped(|ui| {
                        let label = if v.blocked() { t!("common.fix") } else { t!("join.allow_now") };
                        if ui.add_enabled_ui(self.fixing.is_none(), |ui| widgets::button(ui, Some(Icon::Shield), label, Kind::Secondary)).inner.on_hover_text(t!("join.fix_tip")).clicked() {
                            actions.push(Action::FixFirewall(exe.clone(), self.fw_public));
                        }
                        widgets::switch(ui, &mut self.fw_public).on_hover_text(t!("join.public_too_tip"));
                        ui.label(egui::RichText::new(t!("join.public_too")).size(12.5).color(TEXT2));
                    });
                    if v.public && !self.fw_public {
                        ui.add(egui::Label::new(egui::RichText::new(t!("join.public_network")).size(12.0).color(TEXT3)).wrap());
                    }
                }
            }
            let paused = !prefs.internet;
            let room = peerparty_core::inet::RoomCode::parse(prefs.room.trim());
            let (room_text, room_color) = match &room {
                Some(_) if paused => (t!("join.paused").to_string(), TEXT3),
                Some(c) => (t!("join.room", room = c.fingerprint()), theme::GOOD),
                None => (t!("join.no_room").to_string(), TEXT3),
            };
            join_row(ui, Icon::People, t!("join.friends"), &room_text, room_color);
            let (pub_text, pub_color) = match (prefs.public, paused) {
                (true, true) => (t!("join.paused"), TEXT3),
                (true, false) => (t!("common.on"), theme::GOOD),
                _ => (t!("common.off"), TEXT3),
            };
            join_row(ui, Icon::Globe, t!("join.anyone"), pub_text, pub_color);
            ui.add_space(4.0);
            if widgets::button(ui, Some(Icon::ChevronRight), t!("join.change"), Kind::Ghost).clicked() {
                open_together = true;
            }
        });
        if open_together {
            self.open_view(View::Together, false);
        }
    }

    fn advanced(&mut self, ui: &mut egui::Ui, g: &super::scan::Game, key: &str, o: &mut GameOptions, actions: &mut Vec<Action>) {
        if o.players >= 2 && g.source != Source::Xbox {
            stacked(ui, t!("game.adv_copy_saves"), t!("game.adv_copy_saves_sub"), |ui| {
                widgets::switch(ui, &mut o.copy_saves);
            });
            stacked(ui, t!("game.adv_delay"), "", |ui| {
                let opts = [3u32, 5, 8, 10, 12, 15, 20, 30, 45, 60];
                let labels: Vec<String> = opts.iter().map(|s| t!("common.seconds", n = s)).collect();
                let mut i = opts.iter().position(|s| *s >= self.settings.second_delay).unwrap_or(4);
                if widgets::stepper(ui, "delay", &labels, &mut i, 160.0).changed() {
                    self.settings.second_delay = opts[i];
                    self.save_settings();
                }
            });
        }
        widgets::section(ui, t!("settings.services"));
        for (sdk, label, default) in SDK_MODES {
            let global = self.settings.prefs.modes.get(*sdk).cloned().unwrap_or_else(|| default.to_string());
            let mut labels = vec![t!("common.default_value", value = mode_labels(sdk)[mode_values().iter().position(|m| *m == global).unwrap_or(0)])];
            labels.extend(mode_labels(sdk).iter().map(|s| s.to_string()));
            let cur = o.modes.get(*sdk).cloned().unwrap_or_default();
            let mut i = if cur.is_empty() { 0 } else { 1 + mode_values().iter().position(|m| *m == cur).unwrap_or(0) };
            ui.label(egui::RichText::new(sdk_name(sdk, label)).size(13.0).color(TEXT2));
            if widgets::stepper(ui, ("mode", sdk), &labels, &mut i, ui.available_width()).changed() {
                if i == 0 {
                    o.modes.remove(*sdk);
                } else {
                    o.modes.insert(sdk.to_string(), mode_values()[i - 1].to_string());
                }
            }
        }
        widgets::section(ui, t!("game.adv_layers"));
        ui.add(egui::Label::new(egui::RichText::new(t!("game.adv_layers_sub")).size(12.0).color(TEXT3)).wrap());
        for (layer, what) in layers() {
            let mut off = o.skip.iter().any(|s| s == layer);
            let changed = ui
                .horizontal(|ui| {
                    let c = widgets::switch(ui, &mut off).changed();
                    ui.add(egui::Label::new(egui::RichText::new(what).size(13.0).color(TEXT2)).wrap());
                    c
                })
                .inner;
            if changed {
                o.skip.retain(|s| s != layer);
                if off {
                    o.skip.push(layer.to_string());
                }
            }
        }
        widgets::section(ui, t!("game.adv_program"));
        let exe = o.exe.clone().unwrap_or_else(|| g.exe.clone());
        ui.add(egui::Label::new(egui::RichText::new(exe.display().to_string()).monospace().size(12.0).color(TEXT2)).wrap());
        ui.horizontal_wrapped(|ui| {
            if widgets::button(ui, None, t!("game.adv_change"), Kind::Secondary).clicked() {
                if let Some(p) = rfd::FileDialog::new().set_directory(&g.dir).add_filter("Programs", &["exe"]).pick_file() {
                    o.exe = (store::key_of(&p) != key).then_some(p);
                }
            }
            if o.exe.is_some() && widgets::button(ui, None, t!("game.adv_use_found"), Kind::Ghost).clicked() {
                o.exe = None;
            }
            if widgets::button(ui, Some(Icon::Folder), t!("game.adv_open_folder"), Kind::Ghost).clicked() {
                actions.push(Action::Reveal(g.dir.clone()));
            }
        });
        ui.label(egui::RichText::new(t!("game.adv_args")).size(13.0).color(TEXT2));
        widgets::text_field(ui, &mut o.args, t!("game.adv_args_hint"), ui.available_width(), 1000);
        ui.label(egui::RichText::new(t!("game.adv_game_key")).size(13.0).color(TEXT2));
        widgets::text_field(ui, &mut o.game_key, t!("game.adv_game_key_hint"), ui.available_width(), 120).on_hover_text(t!("game.adv_game_key_tip"));
        widgets::section(ui, t!("game.adv_command"));
        let cmd: Vec<String> = self.command(g, o).into_iter().map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a }).collect();
        let mut text = format!("peerparty {}", cmd.join(" "));
        ui.add(egui::TextEdit::multiline(&mut text).font(egui::TextStyle::Monospace).desired_rows(2).desired_width(f32::INFINITY).background_color(theme::FIELD));
        ui.horizontal_wrapped(|ui| {
            if widgets::button(ui, Some(Icon::Copy), t!("common.copy"), Kind::Secondary).clicked() {
                ui.ctx().copy_text(text.clone());
                self.say(t!("snack.copied"), true);
            }
            if widgets::button(ui, None, t!("game.save_bat"), Kind::Secondary).clicked() {
                actions.push(Action::MakeBat(key.to_string()));
            }
        });
    }
}

/// Anything under a game's Advanced that differs from the recommended.
fn advanced_changed(o: &GameOptions) -> bool {
    o.copy_saves || !o.modes.is_empty() || !o.skip.is_empty() || !o.game_key.is_empty() || o.exe.is_some() || !o.args.trim().is_empty()
}

/// A line of "who can join": an icon, who, and how it stands.
fn join_row(ui: &mut egui::Ui, icon: Icon, who: &str, status: &str, color: Color32) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(r, 8.0, color.gamma_multiply(0.14));
        icon.paint(p, r.center(), 14.0, theme::mix(color, Color32::WHITE, 0.25));
        ui.vertical(|ui| {
            ui.add_space(-1.0);
            ui.label(egui::RichText::new(who).size(13.5).color(TEXT));
            ui.add_space(-7.0);
            ui.label(egui::RichText::new(status).size(12.0).color(if color == TEXT3 { TEXT3 } else { theme::mix(color, TEXT, 0.3) }));
        });
    });
}

/// A setting for a narrow column: the name over its control.
fn stacked(ui: &mut egui::Ui, title: &str, detail: &str, control: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let w = ui.available_width() - 56.0;
        ui.vertical(|ui| {
            ui.set_width(w);
            ui.add(egui::Label::new(egui::RichText::new(title).size(13.5).color(TEXT)).wrap());
            if !detail.is_empty() {
                ui.add_space(-6.0);
                ui.add(egui::Label::new(egui::RichText::new(detail).size(12.0).color(TEXT3)).wrap());
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
}

/// A game's own answer to a setting for every game: the default (what that is now),
/// on, or off.
fn tri(ui: &mut egui::Ui, label: &str, v: &mut Option<bool>, global: bool) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(label).size(13.5).color(TEXT));
    let def = t!("common.default_value", value = if global { t!("common.on") } else { t!("common.off") });
    ui.push_id(label, |ui| {
        widgets::segmented(ui, v, &[(None, def.as_str()), (Some(true), t!("common.on")), (Some(false), t!("common.off"))]);
    });
}

/// A folding section: a header row (icon, title, a line, a dot when something in it
/// differs from the recommended) that opens what's under it.
pub(super) fn fold(ui: &mut egui::Ui, id: &str, icon: Icon, title: &str, sub: &str, changed: bool, add: impl FnOnce(&mut egui::Ui)) {
    let id = ui.make_persistent_id(id);
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 50.0), Sense::click());
    if resp.clicked() {
        state.toggle(ui);
    }
    let open = state.openness(ui.ctx());
    let hot = theme::anim(ui.ctx(), id.with("hot"), resp.hovered());
    let p = ui.painter();
    p.rect_filled(rect, 12.0, theme::mix(theme::CARD, theme::CARD_HI, hot.max(open * 0.4)));
    p.rect_stroke(rect, 12.0, egui::Stroke::new(1.0, theme::line()), egui::StrokeKind::Inside);
    icon.paint(p, pos2(rect.left() + 22.0, rect.center().y), 16.0, TEXT2);
    p.text(pos2(rect.left() + 44.0, rect.center().y - 8.0), egui::Align2::LEFT_CENTER, title, theme::strong(14.0), TEXT);
    let g = p.layout_no_wrap(sub.to_string(), theme::font(12.0), TEXT3);
    p.with_clip_rect(rect.shrink2(vec2(44.0, 0.0))).galley(pos2(rect.left() + 44.0, rect.center().y + 2.0), g, TEXT3);
    if changed {
        let c = pos2(rect.right() - 40.0, rect.center().y);
        fx::dot(p, c, 3.5, ACCENT);
    }
    // The chevron turns down as it opens.
    let c = pos2(rect.right() - 20.0, rect.center().y);
    let a = open * std::f32::consts::FRAC_PI_2;
    let rot = |v: Vec2| vec2(v.x * a.cos() - v.y * a.sin(), v.x * a.sin() + v.y * a.cos());
    let st = egui::Stroke::new(1.6, TEXT2);
    p.line_segment([c + rot(vec2(-2.5, -5.0)), c + rot(vec2(2.5, 0.0))], st);
    p.line_segment([c + rot(vec2(2.5, 0.0)), c + rot(vec2(-2.5, 5.0))], st);
    widgets::ring(ui, &resp, 12.0);
    state.show_body_unindented(ui, |ui| {
        ui.add_space(6.0);
        egui::Frame::new().inner_margin(egui::Margin { left: 6, right: 6, top: 0, bottom: 8 }).show(ui, add);
    });
}
