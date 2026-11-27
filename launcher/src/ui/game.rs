//! A game's panel: its picture lit in its own colour, Play, who can join, and what's
//! particular to this game, the rarely needed parts folded away.

use std::time::Duration;

use eframe::egui::{self, Color32, CornerRadius, Rect, Sense, Vec2, pos2, vec2};

use super::icons::Icon;
use super::scan::Source;
use super::settings::{layers, mode_labels, mode_values, sdk_name};
use super::store::{GameOptions, sdk_modes};
use super::theme::{self, ACCENT, ACCENT_HI, TEXT, TEXT2, TEXT3};
use super::widgets::{self, Kind};
use super::{Action, App, View, first_focus, fx, mainexe, store};
use crate::t;

/// How long the Play button's spark lasts.
const SPARK: Duration = Duration::from_millis(900);
/// The panel's poster: 2:3, as the library's covers are.
const POSTER: Vec2 = vec2(184.0, 276.0);

impl App {
    pub(super) fn details(&mut self, ui: &mut egui::Ui, key: String) {
        let Some(g) = self.lib.game(&key).cloned() else {
            self.lib.selected = None;
            return;
        };
        let mut o = self.settings.options(&key);
        let before = o.clone();
        let mut actions = Vec::new();
        let mut close = false;
        let full = ui.max_rect();
        let glow = self.icons.glow_of(&g, &key);
        {
            // The panel's one divider (the panel draws none of its own).
            let p = ui.painter();
            fx::light(p, pos2(full.center().x, full.top() + 160.0), vec2(full.width() * 0.85, 340.0), glow.gamma_multiply(0.20));
            p.rect_filled(Rect::from_min_max(full.left_top(), pos2(full.left() + 1.0, full.bottom())), 0.0, theme::line());
        }
        egui::ScrollArea::vertical().id_salt("game").auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::new().inner_margin(egui::Margin { left: 26, right: 26, top: 22, bottom: 26 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                // The picture: a 2:3 poster like the library's, whether or not the game
                // has a cover (one without is drawn from its art or icon), with Close
                // beside its top.
                let top = ui.cursor().min;
                ui.vertical_centered(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(POSTER.x, POSTER.y), Sense::hover());
                    let p = ui.painter();
                    fx::shadow(p, rect, 14.0, 30, 12, 150);
                    fx::glow(p, rect, 14.0, glow.gamma_multiply(0.30), 34.0);
                    let mut need = Vec::new();
                    self.paint_picture(ui, &g, &key, rect, CornerRadius::same(14), &mut need);
                    ui.painter().rect_stroke(rect, 14.0, egui::Stroke::new(1.0, Color32::from_white_alpha(22)), egui::StrokeKind::Inside);
                    self.request_icons(need);
                });
                // Drawn over the poster's row, taking no room of its own.
                let at = Rect::from_min_size(pos2(top.x + ui.available_width() - 34.0, top.y), vec2(34.0, 34.0));
                let mut corner = ui.new_child(egui::UiBuilder::new().max_rect(at));
                if widgets::icon_button(&mut corner, Icon::Close, t!("game.close")).clicked() {
                    close = true;
                }
                ui.add_space(18.0);
                ui.add(egui::Label::new(egui::RichText::new(&g.name).font(theme::strong(23.0)).color(TEXT)).wrap());
                let running = self.sessions.running(&key);
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
                // Alone, as which player: only once another player has saves here.
                if !xbox && o.players <= 1 && (o.play_as >= 2 || self.later_saves(&key, &g, &o)) {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(t!("game.play_as_label")).size(13.0).color(TEXT2));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let labels: Vec<String> = (0..4u32).map(|s| t!("saves.player", n = s + 1, name = self.player_name(s))).collect();
                            let mut i = usize::from(o.play_as.max(1) - 1).min(3);
                            if widgets::stepper(ui, "play-as", &labels, &mut i, 220.0).changed() {
                                o.play_as = if i == 0 { 0 } else { (i + 1) as u8 };
                            }
                        });
                    });
                    ui.add_space(6.0);
                }
                if !xbox {
                    if o.players >= 2 {
                        ui.add_space(4.0);
                        widgets::card(ui, |ui| {
                            ui.set_width(ui.available_width());
                            self.local_players(ui, &key, &mut o, &running);
                        });
                    } else {
                        self.local_players(ui, &key, &mut o, &running);
                    }
                } else {
                    widgets::note(ui, Icon::Info, TEXT3, t!("game.xbox_one_copy"));
                }
                if let Some(exe) = self.steam_id_missing(&g, &key) {
                    ui.add_space(10.0);
                    widgets::note(ui, Icon::Warning, theme::WARN, t!("game.no_steam_id"));
                    ui.horizontal(|ui| {
                        ui.add_space(22.0);
                        if widgets::button(ui, Some(Icon::Folder), t!("game.no_steam_id_folder"), Kind::Secondary).clicked() {
                            actions.push(Action::Reveal(exe));
                        }
                    });
                }
                ui.add_space(12.0);
                // With players on this PC the panel is about them: who can join shrinks to
                // a line (unless the firewall needs fixing).
                self.who_can_join(ui, &key, !xbox && o.players >= 2, &mut actions);
                // Players' saves: where more than one player is in play, or a later
                // player has saves of its own.
                if !xbox && (o.players >= 2 || o.play_as >= 2 || self.later_saves(&key, &g, &o)) {
                    ui.add_space(12.0);
                    self.saves_card(ui, &g, &key, &mut o, &running, &mut actions);
                }
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
                        actions.push(Action::Reveal(o.exe.clone().unwrap_or_else(|| g.exe.clone())));
                    }
                    let hidden = self.hidden(&key);
                    if widgets::button(ui, Some(Icon::Hidden), if hidden { t!("game.unhide") } else { t!("game.hide") }, Kind::Ghost).clicked() {
                        actions.push(Action::Hide(key.clone(), !hidden));
                    }
                });
            });
        });
        if close {
            self.lib.selected = None;
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

    /// Advanced's Change…: the programs in the game's folder, best first, and any other.
    /// A game can ship its tools beside it (an editor, a packer), as big as it and loading
    /// the same SDKs, and the library may take one of them for the game.
    fn program_picker(&mut self, ui: &mut egui::Ui, g: &super::scan::Game, key: &str, o: &mut GameOptions) {
        let exe = o.exe.clone().unwrap_or_else(|| g.exe.clone());
        let resp = widgets::button(ui, None, &format!("{}  \u{25BE}", t!("game.adv_change")), Kind::Secondary).on_hover_text(t!("game.program_tip"));
        if resp.clicked() {
            // Read each time the list opens, not every frame: it walks the folder.
            let mut list = mainexe::programs(&g.dir);
            for p in [Some(g.exe.clone()), o.exe.clone()].into_iter().flatten().rev() {
                if !list.iter().any(|l| store::key_of(l) == store::key_of(&p)) {
                    list.insert(0, p);
                }
            }
            self.lib.programs = Some((key.to_string(), list));
        }
        egui::Popup::menu(&resp).show(|ui| {
            ui.set_min_width(260.0);
            let list = match &self.lib.programs {
                Some((k, list)) if k == key => list.clone(),
                _ => Vec::new(),
            };
            egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                for p in &list {
                    let rel = p.strip_prefix(&g.dir).unwrap_or(p).display().to_string();
                    let found = store::key_of(p) == key;
                    let label = if found { t!("game.program_found", name = rel) } else { rel };
                    if ui.selectable_label(store::key_of(p) == store::key_of(&exe), label).clicked() {
                        o.exe = (!found).then(|| p.clone());
                        ui.close();
                    }
                }
            });
            ui.separator();
            if ui.button(t!("game.program_other")).clicked() {
                if let Some(p) = rfd::FileDialog::new().set_directory(&g.dir).add_filter("Programs", &["exe"]).pick_file() {
                    o.exe = (store::key_of(&p) != key).then_some(p);
                }
                ui.close();
            }
        });
    }

    /// Players on this PC: how many, and with two or more how they share it (split
    /// screen's layout, monitor and inputs, or when a second instance starts) and their
    /// names. One player sees the count alone.
    fn local_players(&mut self, ui: &mut egui::Ui, key: &str, o: &mut GameOptions, running: &[u8]) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(t!("game.players_here")).size(13.5).color(if o.players >= 2 { TEXT } else { TEXT2 }));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut n = o.players.clamp(1, 4);
                if widgets::segmented(ui, &mut n, &[(1u8, "1"), (2u8, "2"), (3u8, "3"), (4u8, "4")]) {
                    let was = o.players;
                    o.players = n;
                    if n == 1 {
                        o.split_screen = false;
                        o.split_layout.clear();
                    } else {
                        if was <= 1 || n >= 3 {
                            o.split_screen = true;
                        }
                        if n != 3 && matches!(o.split_layout.as_str(), "top2" | "bottom2") {
                            o.split_layout.clear();
                        }
                    }
                }
            });
        });
        if o.players >= 2 {
            widgets::divider(ui);
            let mut split = o.split_screen;
            stacked(ui, t!("game.split_screen"), t!("game.split_screen_sub"), |ui| {
                if widgets::switch(ui, &mut split).changed() {
                    o.split_screen = split;
                    // Turned on again later, two players start side by side
                    // rather than in the grid three or four had.
                    if !split {
                        o.split_layout.clear();
                    }
                    if !split && o.players > 2 {
                        o.players = 2;
                    }
                }
            });
            if o.split_screen {
                ui.add_space(8.0);
                label_tip(ui, t!("game.split_layout"), t!("game.split_screen_note"));
                let mut layout = o.effective_split_layout();
                let mut layout_opts: Vec<(&str, &str)> = if o.players == 3 {
                    vec![
                        ("top2", t!("game.layout_top2")),
                        ("bottom2", t!("game.layout_bottom2")),
                        ("horizontal", t!("game.layout_horizontal")),
                        ("grid", t!("game.layout_grid")),
                    ]
                } else {
                    vec![
                        ("horizontal", t!("game.layout_horizontal")),
                        ("vertical", t!("game.layout_vertical")),
                        ("grid", t!("game.layout_grid")),
                    ]
                };
                if o.has_custom_layout() {
                    layout_opts.push(("custom", t!("game.layout_custom")));
                }
                if choice(ui, "split-layout", &mut layout, &layout_opts, ui.available_width()) {
                    o.split_layout = layout.to_string();
                }
                // Which monitor, once there's more than one.
                let monitors = monitors();
                if monitors.len() >= 2 || o.split_monitor >= 2 {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(t!("game.monitor")).size(13.5).color(TEXT));
                    let mut labels: Vec<String> = monitors
                        .iter()
                        .enumerate()
                        .map(|(i, b)| {
                            let size = format!("{}×{}", b.width, b.height);
                            if i == 0 { t!("game.monitor_main", size = size) } else { t!("game.monitor_n", n = i + 1, size = size) }
                        })
                        .collect();
                    // One chosen that isn't plugged in now stays chosen.
                    let want = usize::from(o.split_monitor.max(1));
                    while labels.len() < want {
                        labels.push(t!("game.monitor_gone", n = labels.len() + 1));
                    }
                    let mut i = want - 1;
                    if widgets::stepper(ui, "split-monitor", &labels, &mut i, ui.available_width()).changed() {
                        o.split_monitor = if i == 0 { 0 } else { (i + 1) as u8 };
                    }
                }
                widgets::divider(ui);
                self.player_devices(ui, key, o);
                ui.add_space(4.0);
                let kbm_forced = o.has_kbm_player();
                let mut kbm_on = !o.split_no_kbm || kbm_forced;
                ui.add_enabled_ui(!kbm_forced, |ui| {
                    stacked(ui, t!("game.split_kbm"), t!("game.split_kbm_sub"), |ui| {
                        if widgets::switch(ui, &mut kbm_on).changed() {
                            o.split_no_kbm = !kbm_on;
                        }
                    });
                });
            } else {
                ui.add(egui::Label::new(egui::RichText::new(t!("game.players_two_note")).size(12.0).color(TEXT3)).wrap());
                // The label wraps beside the stepper rather than running under it.
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 168.0).max(80.0);
                    ui.vertical(|ui| {
                        ui.set_width(w);
                        ui.add(egui::Label::new(egui::RichText::new(t!("game.second_after")).size(13.5).color(TEXT)).wrap());
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let opts = store::SECOND_DELAYS;
                        let labels: Vec<String> = opts.iter().map(|s| t!("common.seconds", n = s)).collect();
                        let mut i = opts.iter().position(|s| *s >= self.settings.second_delay).unwrap_or(4);
                        if widgets::stepper(ui, "delay", &labels, &mut i, 160.0).changed() {
                            self.settings.second_delay = opts[i];
                            self.save_settings();
                        }
                    });
                });
            }
        }
        // Names: only where a second copy on this PC is in play (two players here, or
        // Play adding one); most players never start two, and never see them.
        if o.players >= 2 || !running.is_empty() {
            if o.players >= 2 {
                widgets::divider(ui);
            } else {
                ui.add_space(8.0);
            }
            let main = match self.settings.prefs.name.trim() {
                "" => crate::platform::user_name(),
                n => n.to_string(),
            };
            // Players 3 and 4: split screen alone starts them.
            let last = if o.uses_split_screen() && running.is_empty() { o.players.clamp(2, 4) } else { 2 };
            for n in 2..=last {
                let saved = match n {
                    2 => &self.settings.prefs.other_name,
                    3 => &self.settings.prefs.third_name,
                    _ => &self.settings.prefs.fourth_name,
                };
                let mut name = saved.clone();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(t!("game.player_name", n = n)).size(13.0).color(TEXT2));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let w = ui.available_width().clamp(120.0, 180.0);
                        let field = ui.push_id(("player-name", n), |ui| widgets::text_field(ui, &mut name, &format!("{main} ({n})"), w, 32)).inner;
                        if n == 2 {
                            // Your own name stays as it is; only a second instance takes this one.
                            field.on_hover_text(t!("game.other_name_sub"));
                        }
                    });
                });
                if name != *saved {
                    match n {
                        2 => self.settings.prefs.other_name = name,
                        3 => self.settings.prefs.third_name = name,
                        _ => self.settings.prefs.fourth_name = name,
                    }
                    self.save_settings();
                }
            }
        }
    }

    /// Per-player split-screen input assignment: simple `Auto` by default, with a live
    /// connected-device picker (controllers, extended >4 Xbox pads, racing wheels, flight
    /// sticks) and one-click press-to-detect binding.
    fn player_devices(&mut self, ui: &mut egui::Ui, key: &str, o: &mut GameOptions) {
        if self.nav.binding_input.as_ref().is_some_and(|(bk, _, _)| bk != key) {
            self.nav.binding_input = None;
        }
        let devices = self.nav.pads.devices();
        let active_dev = self.nav.pads.active_device();
        let count = usize::from(o.players.clamp(2, crate::launch::SplitScreen::MAX_INSTANCES as u8));
        // Header row: title + info tooltip on left, live connected-device pill on right.
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(t!("game.player_inputs")).size(13.5).color(TEXT));
            let (ir, iresp) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
            Icon::Info.paint(ui.painter(), ir.center(), 13.0, if iresp.hovered() { TEXT2 } else { TEXT3 });
            iresp.on_hover_text(t!("game.inputs_note"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (dot_col, pill_text, tip) = if devices.is_empty() {
                    (TEXT3, t!("game.input_none_found").to_string(), String::new())
                } else {
                    let any_act = devices.iter().any(|d| d.active);
                    let list = devices.iter().map(|d| format!("{} ({})", d.label, d.badge)).collect::<Vec<_>>().join("\n");
                    (if any_act { theme::GOOD } else { theme::GOOD.gamma_multiply(0.75) }, t!("game.input_count", n = devices.len()), list)
                };
                let galley = ui.painter().layout_no_wrap(pill_text, theme::font(11.5), if devices.is_empty() { TEXT3 } else { TEXT2 });
                let (pr, presp) = ui.allocate_exact_size(vec2(galley.size().x + 22.0, 20.0), Sense::hover());
                let p = ui.painter();
                p.rect_filled(pr, 10.0, theme::FIELD);
                p.rect_stroke(pr, 10.0, egui::Stroke::new(1.0, theme::line()), egui::StrokeKind::Inside);
                fx::dot(p, pos2(pr.left() + 9.0, pr.center().y), 3.0, dot_col);
                p.galley(pos2(pr.left() + 16.0, pr.center().y - galley.size().y / 2.0), galley, if devices.is_empty() { TEXT3 } else { TEXT2 });
                if !tip.is_empty() {
                    presp.on_hover_text(tip);
                }
            });
        });
        ui.add_space(4.0);
        for k in 0..count {
            ui.add_space(4.0);
            let listening = self.nav.binding_input.as_ref().is_some_and(|(bk, slot, _)| bk == key && *slot == k);
            if listening {
                let armed = self.nav.binding_input.as_ref().is_some_and(|(_, _, a)| *a);
                let keys_idle = ui.ctx().input(|i| i.keys_down.is_empty());
                if !armed {
                    if active_dev.is_none() && keys_idle {
                        if let Some((_, _, a)) = self.nav.binding_input.as_mut() {
                            *a = true;
                        }
                    }
                } else if let Some(dev) = active_dev.as_ref() {
                    o.set_split_device(k, &dev.id, Some(&dev.label));
                    self.nav.binding_input = None;
                } else {
                    let esc = ui.ctx().input(|i| i.key_pressed(egui::Key::Escape));
                    let any_key = ui.ctx().input(|i| {
                        i.events.iter().any(|e| matches!(e, egui::Event::Key { pressed: true, repeat: false, key, .. } if *key != egui::Key::Escape))
                    });
                    if esc {
                        self.nav.binding_input = None;
                    } else if any_key {
                        o.set_split_device(k, "kbm", None);
                        self.nav.binding_input = None;
                    }
                }
                ui.horizontal(|ui| {
                    ui.add_sized(vec2(64.0, 32.0), egui::Label::new(egui::RichText::new(t!("game.input_player", n = k + 1)).size(13.0).color(TEXT)));
                    let cancel_w = 64.0;
                    let pill_w = (ui.available_width() - cancel_w - 6.0).max(100.0);
                    let (rect, _) = ui.allocate_exact_size(vec2(pill_w, 32.0), Sense::hover());
                    let p = ui.painter();
                    fx::glow(p, rect, 9.0, ACCENT.gamma_multiply(0.25), 8.0);
                    p.rect_filled(rect, 9.0, ACCENT.gamma_multiply(0.14));
                    p.rect_stroke(rect, 9.0, egui::Stroke::new(1.2, ACCENT), egui::StrokeKind::Inside);
                    let pulse = ((ui.ctx().input(|i| i.time) * 5.0).sin() as f32 * 0.5 + 0.5).clamp(0.35, 1.0);
                    fx::dot(p, pos2(rect.left() + 14.0, rect.center().y), 3.5, ACCENT_HI.gamma_multiply(pulse));
                    let text_rect = Rect::from_min_max(pos2(rect.left() + 26.0, rect.top()), pos2(rect.right() - 8.0, rect.bottom()));
                    let mut job = egui::text::LayoutJob::simple_singleline(t!("game.input_listening").to_string(), theme::strong(12.5), TEXT);
                    job.wrap = egui::text::TextWrapping::truncate_at_width(text_rect.width().max(20.0));
                    let g = p.layout_job(job);
                    p.with_clip_rect(text_rect).galley(pos2(text_rect.left(), rect.center().y - g.size().y / 2.0), g, TEXT);
                    let (cr, cresp) = ui.allocate_exact_size(vec2(cancel_w, 32.0), Sense::click());
                    let chot = cresp.hovered() || cresp.has_focus();
                    let cp = ui.painter();
                    cp.rect_filled(cr, 9.0, if chot { theme::CARD_HI } else { theme::FIELD });
                    cp.rect_stroke(cr, 9.0, egui::Stroke::new(1.0, if chot { theme::line_strong() } else { theme::line() }), egui::StrokeKind::Inside);
                    cp.text(cr.center(), egui::Align2::CENTER_CENTER, t!("game.input_cancel"), theme::font(12.0), if chot { TEXT } else { TEXT2 });
                    widgets::ring(ui, &cresp, 9.0);
                    if cresp.clicked() {
                        self.nav.binding_input = None;
                    }
                });
                continue;
            }

            let cur = o.split_input(k);
            let parsed = crate::launch::SlotInput::named(&cur).unwrap_or_default();
            let (icon_opt, dot_opt, display_label, badge_text, is_offline) = match parsed {
                crate::launch::SlotInput::Auto => (Some(Icon::Bolt), None, t!("game.input_auto").to_string(), String::new(), false),
                crate::launch::SlotInput::KeyboardMouse => (Some(Icon::Keyboard), None, t!("game.input_kbm").to_string(), String::new(), false),
                crate::launch::SlotInput::Controller => (Some(Icon::Gamepad), None, t!("game.input_any_pad").to_string(), String::new(), false),
                crate::launch::SlotInput::Device(_) => match devices.iter().find(|d| d.id == cur) {
                    Some(dev) => {
                        let col = if dev.active { theme::GOOD } else { theme::GOOD.gamma_multiply(0.75) };
                        (None, Some(col), dev.label.clone(), dev.badge.clone(), false)
                    }
                    None => {
                        let saved = o.split_input_names.get(&cur).cloned().unwrap_or_else(|| cur.clone());
                        (None, Some(theme::WARN), saved, t!("game.input_offline").to_string(), true)
                    }
                },
            };

            ui.horizontal(|ui| {
                ui.add_sized(vec2(64.0, 32.0), egui::Label::new(egui::RichText::new(t!("game.input_player", n = k + 1)).size(13.0).color(TEXT2)));
                let detect_w = 32.0;
                let picker_w = (ui.available_width() - detect_w - 6.0).max(120.0);
                let (rect, resp) = ui.allocate_exact_size(vec2(picker_w, 32.0), Sense::click());
                let id = ui.make_persistent_id(("split-device-picker", key, k));
                let popup_id = id.with("popup");
                if resp.clicked() {
                    egui::Popup::toggle_id(ui.ctx(), popup_id);
                }
                let hot = resp.hovered() || resp.has_focus() || egui::Popup::is_id_open(ui.ctx(), popup_id);
                let p = ui.painter();
                p.rect_filled(rect, 9.0, if hot { theme::CARD_HI } else { theme::FIELD });
                let border = if is_offline { theme::WARN.gamma_multiply(0.45) } else if hot { theme::line_strong() } else { theme::line() };
                p.rect_stroke(rect, 9.0, egui::Stroke::new(1.0, border), egui::StrokeKind::Inside);
                let mut left_x = rect.left() + 10.0;
                if let Some(ic) = icon_opt {
                    let col = if parsed == crate::launch::SlotInput::Auto { ACCENT_HI } else { TEXT2 };
                    ic.paint(p, pos2(left_x + 6.0, rect.center().y), 13.5, col);
                    left_x += 20.0;
                } else if let Some(col) = dot_opt {
                    fx::dot(p, pos2(left_x + 5.0, rect.center().y), 3.5, col);
                    left_x += 16.0;
                }
                // Right side: down chevron + optional mini badge.
                let mut right_x = rect.right() - 14.0;
                let cv = pos2(right_x, rect.center().y);
                let st = egui::Stroke::new(1.5, if hot { TEXT } else { TEXT3 });
                p.line_segment([cv + vec2(-3.5, -1.5), cv + vec2(0.0, 2.0)], st);
                p.line_segment([cv + vec2(0.0, 2.0), cv + vec2(3.5, -1.5)], st);
                right_x -= 12.0;
                if !badge_text.is_empty() {
                    let bcol = if is_offline { theme::WARN } else { TEXT3 };
                    let bgal = p.layout_no_wrap(badge_text, theme::font(11.0), bcol);
                    let bw = bgal.size().x + 10.0;
                    if right_x - bw > left_x + 48.0 {
                        let br = Rect::from_min_size(pos2(right_x - bw, rect.center().y - 9.0), vec2(bw, 18.0));
                        p.rect_filled(br, 5.0, if is_offline { theme::WARN.gamma_multiply(0.14) } else { Color32::from_white_alpha(10) });
                        p.galley(pos2(br.left() + 5.0, br.center().y - bgal.size().y / 2.0), bgal, bcol);
                        right_x -= bw + 6.0;
                    }
                }
                let text_rect = Rect::from_min_max(pos2(left_x, rect.top()), pos2(right_x.max(left_x + 12.0), rect.bottom()));
                let mut job = egui::text::LayoutJob::simple_singleline(display_label, theme::font(13.0), TEXT);
                job.wrap = egui::text::TextWrapping::truncate_at_width(text_rect.width());
                let g = p.layout_job(job);
                p.with_clip_rect(text_rect).galley(pos2(text_rect.left(), rect.center().y - g.size().y / 2.0), g, TEXT);
                widgets::ring(ui, &resp, 9.0);

                // Dropdown popup with quick modes, live connected devices, and Detect.
                let pop_w = picker_w.max(264.0);
                egui::Popup::from_response(&resp)
                    .id(popup_id)
                    .open_memory(None)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .width(pop_w)
                    .show(|ui| {
                        ui.set_min_width(pop_w - 8.0);
                        let mut quick = |token: &str, icon: Icon, label: &str, sub: &str| {
                            let selected = cur == token;
                            if device_menu_row(ui, Some(icon), None, label, "", selected, None).on_hover_text(sub).clicked() {
                                o.set_split_device(k, token, None);
                                ui.close();
                            }
                        };
                        quick("auto", Icon::Bolt, t!("game.input_auto"), t!("game.input_auto_sub"));
                        quick("kbm", Icon::Keyboard, t!("game.input_kbm"), t!("game.input_kbm_sub"));
                        quick("pad", Icon::Gamepad, t!("game.input_any_pad"), t!("game.input_any_pad_sub"));
                        ui.separator();
                        ui.label(egui::RichText::new(t!("game.input_connected")).size(11.5).color(TEXT3));
                        if devices.is_empty() {
                            ui.add_space(2.0);
                            ui.label(egui::RichText::new(t!("game.input_none_found")).size(12.5).color(TEXT3));
                            ui.add_space(2.0);
                        } else {
                            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                                for dev in &devices {
                                    let selected = cur == dev.id;
                                    let other_owner = (0..count).find(|&j| j != k && o.split_input(j) == dev.id).map(|j| t!("game.input_assigned_to", n = j + 1));
                                    let dot = if dev.active { theme::GOOD } else { theme::GOOD.gamma_multiply(0.55) };
                                    if device_menu_row(ui, None, Some(dot), &dev.label, &dev.badge, selected, other_owner.as_deref()).clicked() {
                                        o.set_split_device(k, &dev.id, Some(&dev.label));
                                        ui.close();
                                    }
                                }
                            });
                        }
                        ui.separator();
                        if device_menu_row(ui, Some(Icon::Search), None, t!("game.input_detect"), "", false, None).clicked() {
                            self.nav.binding_input = Some((key.to_string(), k, false));
                            ui.close();
                        }
                    });

                // Compact live-detect target button beside the dropdown.
                let (dr, dresp) = ui.allocate_exact_size(vec2(detect_w, 32.0), Sense::click());
                let dhot = dresp.hovered() || dresp.has_focus();
                let dp = ui.painter();
                dp.rect_filled(dr, 9.0, if dhot { theme::CARD_HI } else { theme::FIELD });
                dp.rect_stroke(dr, 9.0, egui::Stroke::new(1.0, if dhot { ACCENT.gamma_multiply(0.65) } else { theme::line() }), egui::StrokeKind::Inside);
                let col = if dhot { ACCENT_HI } else { TEXT2 };
                let c = dr.center();
                dp.circle_stroke(c, 5.5, egui::Stroke::new(1.4, col));
                dp.circle_filled(c, 2.0, col);
                widgets::ring(ui, &dresp, 9.0);
                if dresp.on_hover_text(t!("game.input_detect_tip")).clicked() {
                    self.nav.binding_input = Some((key.to_string(), k, false));
                }
            });
        }
    }

    fn play_button(&mut self, ui: &mut egui::Ui, g: &super::scan::Game, key: &str, o: &GameOptions, running: &[u8], actions: &mut Vec<Action>) {
        // An Xbox app game runs one copy per PC: Windows won't start another while one
        // runs, so the button says so instead.
        let xbox = g.source == Source::Xbox;
        let waiting = self.sessions.pending.iter().find(|p| p.key == key).map(|p| (p.player, p.at.saturating_duration_since(std::time::Instant::now()).as_secs() + 1));
        let label: String = match (o.uses_split_screen() && !xbox, o.players >= 2 && !xbox, running.is_empty(), xbox) {
            // The two-player start's second copy is waiting: Play starts it now.
            _ if waiting.is_some() => {
                let (player, left) = waiting.unwrap_or_default();
                t!("game.start_now", player = player, n = left)
            }
            (_, _, false, true) => t!("game.already_running").to_string(),
            (true, _, true, _) => t!("game.play_split", n = o.players.clamp(2, 4)),
            (false, true, true, _) => t!("game.play_two").to_string(),
            (false, false, true, _) if o.play_as >= 2 => t!("game.play_as", name = self.player_name(u32::from(o.play_as) - 1)),
            (false, false, true, _) => t!("game.play").to_string(),
            // With a copy running, Play adds one more, whatever the count says.
            (_, _, false, _) => t!("game.play_another").to_string(),
        };
        let enabled = !(xbox && !running.is_empty());
        let resp = ui.add_enabled_ui(enabled, |ui| widgets::wide_button(ui, Some(Icon::Play), &label, Kind::Primary, 54.0)).inner;
        first_focus(&resp, &mut self.nav.focus_play);
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
        if let Some((at, seed)) = self.sessions.spark {
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
                self.sessions.spark = None;
            }
        }
    }

    /// The program, when this is a Steam game (a `steam_api` in its folder) whose app id
    /// can't be found (steamid.rs), so Steam won't know which game it is. Not for Epic's
    /// or GOG's copies, which can carry an unused `steam_api` from the Steam version.
    /// The folder is looked through once; the id again every few seconds while the panel
    /// shows, so adding a steam_appid.txt clears the note.
    fn steam_id_missing(&mut self, g: &super::scan::Game, key: &str) -> Option<std::path::PathBuf> {
        if matches!(g.source, Source::Xbox | Source::Epic | Source::Gog) {
            return None;
        }
        let exe = self.settings.options(key).exe.unwrap_or_else(|| g.exe.clone());
        let now = std::time::Instant::now();
        let fresh = |at: std::time::Instant| now.duration_since(at) < Duration::from_secs(5);
        let entry = self.checks.steam_ids.entry(exe.clone()).or_insert_with(|| {
            let look = crate::steamid::Game::look(&exe, Some(&g.dir));
            let missing = look.missing(&exe);
            (look, missing, now)
        });
        if entry.0.uses_steam && !fresh(entry.2) {
            entry.1 = entry.0.missing(&exe);
            entry.2 = now;
        }
        entry.1.then_some(exe)
    }

    /// Who can play with you in this game: people on this network (unless the firewall
    /// keeps them out, fixed here), friends with your room code, anyone online. `compact`:
    /// one line saying so, when nothing needs fixing.
    fn who_can_join(&mut self, ui: &mut egui::Ui, key: &str, compact: bool, actions: &mut Vec<Action>) {
        let fw = self.firewall_for(key);
        let prefs = self.settings.prefs.clone();
        let mut open_together = false;
        // Not "Windows will ask the first time": that's every game not yet played, and
        // players on one PC don't go through the firewall.
        let trouble = fw.as_ref().is_some_and(|(v, _)| v.blocked());
        if compact && !trouble {
            widgets::card(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
                    ui.painter().rect_filled(r, 8.0, theme::GOOD.gamma_multiply(0.14));
                    Icon::People.paint(ui.painter(), r.center(), 14.0, theme::mix(theme::GOOD, Color32::WHITE, 0.25));
                    ui.vertical(|ui| {
                        ui.set_width((ui.available_width() - 44.0).max(60.0));
                        ui.add_space(-1.0);
                        ui.label(egui::RichText::new(t!("join.title")).size(13.5).color(TEXT));
                        ui.add_space(-7.0);
                        ui.add(egui::Label::new(egui::RichText::new(super::together::summary(&prefs)).size(12.0).color(TEXT2)).truncate());
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if widgets::icon_button(ui, Icon::ChevronRight, t!("join.change")).clicked() {
                            open_together = true;
                        }
                    });
                });
            });
            if open_together {
                self.open_view(View::Together, false);
            }
            return;
        }
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
                        if ui.add_enabled_ui(self.checks.fixing.is_none(), |ui| widgets::button(ui, Some(Icon::Shield), label, Kind::Secondary)).inner.on_hover_text(t!("join.fix_tip")).clicked() {
                            actions.push(Action::FixFirewall(exe.clone(), self.checks.fw_public));
                        }
                        widgets::switch(ui, &mut self.checks.fw_public).on_hover_text(t!("join.public_too_tip"));
                        ui.label(egui::RichText::new(t!("join.public_too")).size(12.5).color(TEXT2));
                    });
                    if v.public && !self.checks.fw_public {
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
        widgets::section(ui, t!("settings.services"));
        for (sdk, label, default) in sdk_modes() {
            let global = self.settings.prefs.modes.get(sdk).cloned().unwrap_or_else(|| default.to_string());
            let mut labels = vec![t!("common.default_value", value = mode_labels(sdk)[mode_values().iter().position(|m| *m == global).unwrap_or(0)])];
            labels.extend(mode_labels(sdk).iter().map(|s| s.to_string()));
            let cur = o.modes.get(sdk).cloned().unwrap_or_default();
            let mut i = if cur.is_empty() { 0 } else { 1 + mode_values().iter().position(|m| *m == cur).unwrap_or(0) };
            ui.label(egui::RichText::new(sdk_name(sdk, label)).size(13.0).color(TEXT2));
            if widgets::stepper(ui, ("mode", sdk), &labels, &mut i, ui.available_width()).changed() {
                if i == 0 {
                    o.modes.remove(sdk);
                } else {
                    o.modes.insert(sdk.to_string(), mode_values()[i - 1].to_string());
                }
            }
        }
        widgets::section(ui, t!("game.adv_layers"));
        ui.add(egui::Label::new(egui::RichText::new(t!("game.adv_layers_sub")).size(12.0).color(TEXT3)).wrap());
        // Each switch is on while its layer is (the default): off puts it on the game's
        // skip list (`PEERPARTY_SKIP`).
        for (layer, what) in layers() {
            let mut on = !o.skip.iter().any(|s| s == layer);
            let changed = ui
                .horizontal(|ui| {
                    let c = widgets::switch(ui, &mut on).changed();
                    ui.add(egui::Label::new(egui::RichText::new(what).size(13.0).color(TEXT2)).wrap());
                    c
                })
                .inner;
            if changed {
                o.skip.retain(|s| s != layer);
                if !on {
                    o.skip.push(layer.to_string());
                }
            }
        }
        if o.uses_split_screen() && g.source != Source::Xbox {
            advanced_split(ui, o);
        }
        widgets::section(ui, t!("game.adv_program"));
        let exe = o.exe.clone().unwrap_or_else(|| g.exe.clone());
        ui.add(egui::Label::new(egui::RichText::new(exe.display().to_string()).monospace().size(12.0).color(TEXT2)).wrap());
        ui.horizontal_wrapped(|ui| {
            self.program_picker(ui, g, key, o);
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
        // The whole command, as a prompt runs it: peerparty.exe's own path, and each
        // argument quoted the way peerparty reads it back. Shown read-only (it can be
        // selected and copied, not typed into: the options above change it).
        let quote = |a: &str| if a.is_empty() || a.contains([' ', '\t', '"']) { crate::crt_quote(a) } else { a.to_string() };
        let mut cmd = vec![quote(&self.sessions.peerparty.to_string_lossy())];
        cmd.extend(self.command(g, o).iter().map(|a| quote(a)));
        let text = cmd.join(" ");
        ui.add(egui::TextEdit::multiline(&mut text.as_str()).font(egui::TextStyle::Monospace).desired_rows(2).desired_width(f32::INFINITY).background_color(theme::FIELD));
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

/// Split screen's rarely needed parts: frame pacing, start stagger and how the window
/// shows the copies (`PEERPARTY_COMPOSITOR`).
fn advanced_split(ui: &mut egui::Ui, o: &mut GameOptions) {
    widgets::section(ui, t!("game.adv_split"));
    let mut sync = !o.split_no_sync;
    let sync_changed = ui
        .horizontal(|ui| {
            let c = widgets::switch(ui, &mut sync).changed();
            ui.add(egui::Label::new(egui::RichText::new(t!("game.adv_sync")).size(13.0).color(TEXT2)).wrap());
            c
        })
        .inner;
    if sync_changed {
        o.split_no_sync = !sync;
        if o.split_no_sync && o.split_fps.sync_divisor() > 1 {
            o.split_fps = crate::launch::FpsCap::Display;
        }
    }
    ui.add(egui::Label::new(egui::RichText::new(t!("game.adv_sync_sub")).size(12.0).color(TEXT3)).wrap());
    ui.add_space(6.0);
    ui.label(egui::RichText::new(t!("game.adv_fps")).size(13.0).color(TEXT2));
    use crate::launch::FpsCap;
    let fps_vals: [FpsCap; 8] = [
        FpsCap::Display,
        FpsCap::Divisor(2),
        FpsCap::Divisor(3),
        FpsCap::Fixed(30),
        FpsCap::Fixed(40),
        FpsCap::Fixed(60),
        FpsCap::Fixed(90),
        FpsCap::Fixed(120),
    ];
    let fps_labels: Vec<String> = vec![
        t!("game.adv_fps_display").to_string(),
        t!("game.adv_fps_div", d = 2),
        t!("game.adv_fps_div", d = 3),
        t!("game.adv_fps_n", n = 30),
        t!("game.adv_fps_n", n = 40),
        t!("game.adv_fps_n", n = 60),
        t!("game.adv_fps_n", n = 90),
        t!("game.adv_fps_n", n = 120),
    ];
    let mut fi = fps_vals.iter().position(|&v| v == o.split_fps).unwrap_or(0);
    if widgets::stepper(ui, "split-fps", &fps_labels, &mut fi, ui.available_width()).changed() {
        o.split_fps = fps_vals[fi];
        if o.split_fps.sync_divisor() > 1 {
            o.split_no_sync = false;
        }
    }
    ui.add(egui::Label::new(egui::RichText::new(t!("game.adv_fps_sub")).size(12.0).color(TEXT3)).wrap());
    ui.add_space(6.0);
    ui.label(egui::RichText::new(t!("game.adv_render_scale")).size(13.0).color(TEXT2));
    let scale_vals: [u8; 5] = [100, 85, 75, 67, 50];
    let scale_labels: Vec<String> = vec![
        t!("game.adv_render_scale_native").to_string(),
        "85%".into(),
        "75%".into(),
        "67%".into(),
        "50%".into(),
    ];
    let cur_scale = o.effective_split_render_scale();
    let mut rsci = scale_vals.iter().position(|&v| v == cur_scale).unwrap_or(0);
    if widgets::stepper(ui, "split-render-scale", &scale_labels, &mut rsci, ui.available_width()).changed() {
        o.split_render_scale = scale_vals[rsci];
    }
    ui.add(egui::Label::new(egui::RichText::new(t!("game.adv_render_scale_sub")).size(12.0).color(TEXT3)).wrap());
    ui.add_space(6.0);
    let mut upscale = !o.split_no_upscale;
    let upscale_changed = ui
        .horizontal(|ui| {
            let c = widgets::switch(ui, &mut upscale).changed();
            ui.add(egui::Label::new(egui::RichText::new(t!("game.adv_upscale")).size(13.0).color(TEXT2)).wrap());
            c
        })
        .inner;
    if upscale_changed {
        o.split_no_upscale = !upscale;
    }
    ui.add(egui::Label::new(egui::RichText::new(t!("game.adv_upscale_sub")).size(12.0).color(TEXT3)).wrap());
    ui.add_space(6.0);
    let mut vram = !o.split_no_vram_budget;
    let vram_changed = ui
        .horizontal(|ui| {
            let c = widgets::switch(ui, &mut vram).changed();
            ui.add(egui::Label::new(egui::RichText::new(t!("game.adv_vram_budget")).size(13.0).color(TEXT2)).wrap());
            c
        })
        .inner;
    if vram_changed {
        o.split_no_vram_budget = !vram;
    }
    ui.add(egui::Label::new(egui::RichText::new(t!("game.adv_vram_budget_sub")).size(12.0).color(TEXT3)).wrap());
    ui.add_space(6.0);
    let mut cpu_bal = !o.split_no_cpu_balance;
    let cpu_changed = ui
        .horizontal(|ui| {
            let c = widgets::switch(ui, &mut cpu_bal).changed();
            ui.add(egui::Label::new(egui::RichText::new(t!("game.adv_cpu_balance")).size(13.0).color(TEXT2)).wrap());
            c
        })
        .inner;
    if cpu_changed {
        o.split_no_cpu_balance = !cpu_bal;
    }
    ui.add(egui::Label::new(egui::RichText::new(t!("game.adv_cpu_balance_sub")).size(12.0).color(TEXT3)).wrap());
    ui.add_space(6.0);
    ui.label(egui::RichText::new(t!("game.adv_stagger")).size(13.0).color(TEXT2));
    let stagger_vals = ["", "0", "5", "10", "15", "20", "30"];
    let stagger_labels: Vec<String> = vec![
        t!("game.adv_stagger_smart").to_string(),
        t!("game.adv_stagger_none").to_string(),
        t!("common.seconds", n = 5),
        t!("common.seconds", n = 10),
        t!("common.seconds", n = 15),
        t!("common.seconds", n = 20),
        t!("common.seconds", n = 30),
    ];
    let mut si = stagger_vals.iter().position(|v| *v == o.split_stagger).unwrap_or(0);
    if widgets::stepper(ui, "split-stagger", &stagger_labels, &mut si, ui.available_width()).changed() {
        o.split_stagger = stagger_vals[si].to_string();
    }
    ui.add(egui::Label::new(egui::RichText::new(t!("game.adv_stagger_sub")).size(12.0).color(TEXT3)).wrap());
    ui.add_space(6.0);
    ui.label(egui::RichText::new(t!("game.adv_compositor")).size(13.0).color(TEXT2));
    let mut how = match o.compositor.as_str() {
        "dwm" => "dwm",
        "pixels" => "pixels",
        _ => "",
    };
    let opts = [("", t!("game.adv_compositor_frames")), ("dwm", t!("game.adv_compositor_windows")), ("pixels", t!("game.adv_compositor_pixels"))];
    ui.push_id("compositor", |ui| {
        if widgets::segmented(ui, &mut how, &opts) {
            o.compositor = how.to_string();
        }
    });
    ui.add(egui::Label::new(egui::RichText::new(t!("game.adv_compositor_sub")).size(12.0).color(TEXT3)).wrap());
}

/// Anything under a game's Advanced that differs from the recommended.
fn advanced_changed(o: &GameOptions) -> bool {
    !o.modes.is_empty()
        || !o.skip.is_empty()
        || !o.game_key.is_empty()
        || o.exe.is_some()
        || !o.args.trim().is_empty()
        || !o.compositor.is_empty()
        || !o.split_stagger.is_empty()
        || o.split_no_sync
        || o.effective_split_render_scale() < 100
        || o.split_no_upscale
        || o.split_fps != crate::launch::FpsCap::Display
        || o.split_no_vram_budget
        || o.split_no_cpu_balance
}

/// This PC's monitors, as `--monitor` numbers them, looked at every few seconds (one
/// plugged in while the page shows is listed soon after).
fn monitors() -> Vec<crate::host::MonitorBounds> {
    type Seen = Option<(std::time::Instant, Vec<crate::host::MonitorBounds>)>;
    static SEEN: std::sync::Mutex<Seen> = std::sync::Mutex::new(None);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    match seen.as_ref() {
        Some((at, v)) if at.elapsed() < Duration::from_secs(3) => v.clone(),
        _ => {
            let v = crate::host::MonitorBounds::all();
            *seen = Some((std::time::Instant::now(), v.clone()));
            v
        }
    }
}

/// One row in the split-screen device picker popup: optional icon or status dot, label,
/// optional badge and current-owner pill (`P2`), plus checkmark when selected.
fn device_menu_row(
    ui: &mut egui::Ui,
    icon: Option<Icon>,
    dot: Option<Color32>,
    label: &str,
    badge: &str,
    selected: bool,
    owner: Option<&str>,
) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    let hot = resp.hovered() || resp.has_focus();
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 7.0, ACCENT.gamma_multiply(0.20));
    } else if hot {
        p.rect_filled(rect, 7.0, Color32::from_white_alpha(14));
    }
    let mut left_x = rect.left() + 8.0;
    if let Some(ic) = icon {
        ic.paint(p, pos2(left_x + 6.0, rect.center().y), 13.5, if selected { ACCENT_HI } else { TEXT2 });
        left_x += 20.0;
    } else if let Some(col) = dot {
        fx::dot(p, pos2(left_x + 5.0, rect.center().y), 3.5, col);
        left_x += 16.0;
    }
    let mut right_x = rect.right() - 8.0;
    if selected {
        Icon::Check.paint(p, pos2(right_x - 6.0, rect.center().y), 13.0, ACCENT_HI);
        right_x -= 18.0;
    }
    if let Some(own) = owner {
        let g = p.layout_no_wrap(own.to_string(), theme::strong(10.5), ACCENT_HI);
        let bw = g.size().x + 10.0;
        let br = Rect::from_min_size(pos2(right_x - bw, rect.center().y - 8.5), vec2(bw, 17.0));
        p.rect_filled(br, 5.0, ACCENT.gamma_multiply(0.18));
        p.galley(pos2(br.left() + 5.0, br.center().y - g.size().y / 2.0), g, ACCENT_HI);
        right_x -= bw + 5.0;
    }
    if !badge.is_empty() {
        let g = p.layout_no_wrap(badge.to_string(), theme::font(11.0), TEXT3);
        let bw = g.size().x + 10.0;
        if right_x - bw > left_x + 48.0 {
            let br = Rect::from_min_size(pos2(right_x - bw, rect.center().y - 8.5), vec2(bw, 17.0));
            p.rect_filled(br, 5.0, Color32::from_white_alpha(10));
            p.galley(pos2(br.left() + 5.0, br.center().y - g.size().y / 2.0), g, TEXT3);
            right_x -= bw + 5.0;
        }
    }
    let text_rect = Rect::from_min_max(pos2(left_x, rect.top()), pos2(right_x.max(left_x + 16.0), rect.bottom()));
    let mut job = egui::text::LayoutJob::simple_singleline(label.to_string(), theme::font(13.0), if selected { Color32::WHITE } else { TEXT });
    job.wrap = egui::text::TextWrapping::truncate_at_width(text_rect.width());
    let g = p.layout_job(job);
    p.with_clip_rect(text_rect).galley(pos2(text_rect.left(), rect.center().y - g.size().y / 2.0), g, if selected { Color32::WHITE } else { TEXT });
    widgets::ring(ui, &resp, 7.0);
    resp
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

/// A label with an info mark after it whose tooltip says more, for what would otherwise
/// be a paragraph under the control.
fn label_tip(ui: &mut egui::Ui, text: &str, tip: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(text).size(13.5).color(TEXT));
        let (r, resp) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
        Icon::Info.paint(ui.painter(), r.center(), 13.0, if resp.hovered() { TEXT2 } else { TEXT3 });
        resp.on_hover_text(tip);
    });
}

/// How wide `widgets::segmented` draws these options.
fn segmented_width<T>(ui: &egui::Ui, options: &[(T, &str)]) -> f32 {
    let font = theme::strong(13.0);
    options.iter().map(|(_, l)| ui.painter().layout_no_wrap(l.to_string(), font.clone(), TEXT).size().x + 26.0).sum::<f32>() + 6.0
}

/// One of a few options: side by side where they fit in `width`, else a stepper (three
/// players' layouts and a custom one don't fit a row). Returns whether it changed.
fn choice<T: PartialEq + Clone>(ui: &mut egui::Ui, id: &str, value: &mut T, options: &[(T, &str)], width: f32) -> bool {
    if segmented_width(ui, options) <= width {
        return ui.push_id(id, |ui| widgets::segmented(ui, value, options)).inner;
    }
    let labels: Vec<String> = options.iter().map(|(_, l)| l.to_string()).collect();
    let mut i = options.iter().position(|(v, _)| v == value).unwrap_or(0);
    if widgets::stepper(ui, id, &labels, &mut i, width).changed() {
        *value = options[i].0.clone();
        return true;
    }
    false
}

/// A setting for a narrow column: the name over its control.
pub(super) fn stacked(ui: &mut egui::Ui, title: &str, detail: &str, control: impl FnOnce(&mut egui::Ui)) {
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
    let room = rect.width() - 44.0 - if changed { 52.0 } else { 36.0 };
    let mut job = egui::text::LayoutJob::simple_singleline(sub.to_string(), theme::font(12.0), TEXT3);
    job.wrap = egui::text::TextWrapping::truncate_at_width(room.max(0.0));
    let g = p.layout_job(job);
    p.galley(pos2(rect.left() + 44.0, rect.center().y + 2.0), g, TEXT3);
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
