//! Settings for every game, in sections: General (language, the window, lighting),
//! Overlay, Library (the folders games are found in), Advanced (online services,
//! troubleshooting) and About. Who can join you is Play together's.

use eframe::egui::{self, RichText};

use super::icons::Icon;
use super::store::{Hotkeys, Prefs, SDK_MODES};
use super::theme::{self, TEXT, TEXT2, TEXT3};
use super::widgets::{self, Kind};
use super::{App, Category, View, first_focus};
use crate::t;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    General,
    Overlay,
    Library,
    Advanced,
    About,
}

impl Tab {
    const ALL: [Tab; 5] = [Tab::General, Tab::Overlay, Tab::Library, Tab::Advanced, Tab::About];

    pub fn step(self, d: i32) -> Tab {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0) as i32;
        Self::ALL[(i + d).rem_euclid(Self::ALL.len() as i32) as usize]
    }

    fn name(self) -> &'static str {
        match self {
            Tab::General => t!("settings.general"),
            Tab::Overlay => t!("settings.overlay"),
            Tab::Library => t!("settings.library"),
            Tab::Advanced => t!("settings.advanced"),
            Tab::About => t!("settings.about"),
        }
    }
}

/// PeerParty layers a game can turn off (`PEERPARTY_SKIP`), with what each does.
pub fn layers() -> [(&'static str, &'static str); 13] {
    [
        ("steam", t!("layer.steam")),
        ("eos", t!("layer.eos")),
        ("party", t!("layer.party")),
        ("pfmp", t!("layer.pfmp")),
        ("playfab", t!("layer.playfab")),
        ("galaxy", t!("layer.galaxy")),
        ("photon", t!("layer.photon")),
        ("ugs", t!("layer.ugs")),
        ("invite", t!("layer.invite")),
        ("padblock", t!("layer.padblock")),
        ("present", t!("layer.present")),
        ("swap", t!("layer.swap")),
        ("dns", t!("layer.dns")),
    ]
}

/// The modes a service can be in, in the order the labels list them.
pub fn mode_values() -> [&'static str; 3] {
    ["lan", "auto", "official"]
}

pub fn mode_labels(sdk: &str) -> [&'static str; 3] {
    if sdk == "steamid" {
        [t!("mode.steamid_lan"), t!("mode.steamid_auto"), t!("mode.steamid_official")]
    } else {
        [t!("mode.lan"), t!("mode.auto"), t!("mode.official")]
    }
}

/// A service's name as the list shows it.
pub fn sdk_name(sdk: &str, label: &'static str) -> &'static str {
    match sdk {
        "steam" => t!("sdk.steam_lobbies"),
        "steamid" => t!("sdk.steam_id"),
        _ => label,
    }
}

impl App {
    pub(super) fn settings_page(&mut self, ui: &mut egui::Ui) {
        let before = self.settings.prefs.clone();
        let (folders_before, after_before) = (self.settings.folders.clone(), self.settings.after_launch.clone());
        ui.add_space(24.0);
        widgets::page_title(ui, t!("settings.title"), t!("settings.sub"));
        ui.horizontal_wrapped(|ui| {
            for tab in Tab::ALL {
                let r = widgets::chip(ui, tab.name(), None, self.settings_tab == tab);
                if r.clicked() {
                    self.settings_tab = tab;
                }
            }
        });
        ui.add_space(14.0);
        egui::ScrollArea::vertical().id_salt(("settings", self.settings_tab as u8)).auto_shrink([false, false]).show(ui, |ui| {
            // Never wider than the window (set_max_width can widen a Ui), with a gutter for the scroll bar.
            ui.set_max_width((ui.available_width() - 26.0).min(860.0));
            match self.settings_tab {
                Tab::General => self.general(ui),
                Tab::Overlay => overlay(ui, &mut self.settings.prefs, &mut self.focus_first),
                Tab::Library => self.library_settings(ui),
                Tab::Advanced => self.advanced_settings(ui),
                Tab::About => self.about(ui),
            }
            ui.add_space(30.0);
        });
        if self.settings.prefs != before || self.settings.folders != folders_before || self.settings.after_launch != after_before {
            self.save_settings();
        }
    }

    fn general(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            let p = &mut self.settings.prefs;
            let focus = &mut self.focus_first;
            widgets::setting(ui, t!("settings.language"), t!("settings.language_sub"), |ui| {
                let mut labels = vec![t!("settings.language_auto").to_string()];
                labels.extend(crate::i18n::LANGS.iter().map(|l| l.1.to_string()));
                let mut i = crate::i18n::LANGS.iter().position(|l| l.0 == p.language).map_or(0, |i| i + 1);
                let r = widgets::stepper(ui, "lang", &labels, &mut i, 240.0);
                first_focus(&r, focus);
                if r.changed() {
                    p.language = if i == 0 { String::new() } else { crate::i18n::LANGS[i - 1].0.to_string() };
                    crate::i18n::set(&p.language);
                    theme::install_fonts(&ctx);
                    ctx.request_repaint();
                }
            });
            widgets::divider(ui);
            let p = &mut self.settings.prefs;
            widgets::setting(ui, t!("settings.effects"), t!("settings.effects_sub"), |ui| {
                if widgets::switch(ui, &mut p.effects).changed() {
                    theme::set_effects(p.effects);
                    theme::apply(&ctx);
                }
            });
            widgets::divider(ui);
            let after = &mut self.settings.after_launch;
            widgets::setting(ui, t!("settings.after_launch"), t!("settings.after_launch_sub"), |ui| {
                let mut v = if after == "keep" { "keep" } else { "minimize" };
                if widgets::segmented(ui, &mut v, &[("minimize", t!("settings.after_minimize")), ("keep", t!("settings.after_keep"))]) {
                    *after = v.to_string();
                }
            });
            widgets::divider(ui);
            let p = &mut self.settings.prefs;
            widgets::setting(ui, t!("settings.toasts"), t!("settings.toasts_sub"), |ui| {
                widgets::switch(ui, &mut p.toasts);
            });
        });
        ui.add_space(12.0);
        let mut go = false;
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::setting(ui, t!("settings.name_moved"), t!("settings.name_moved_sub"), |ui| {
                go = widgets::button(ui, Some(Icon::People), t!("nav.together"), Kind::Secondary).clicked();
            });
        });
        if go {
            self.open_view(View::Together, false);
        }
    }

    fn library_settings(&mut self, ui: &mut egui::Ui) {
        let mut remove = None;
        let mut add = false;
        let mut rescan = false;
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::card_title(ui, Icon::Folder, theme::ACCENT, t!("settings.folders"), t!("settings.folders_sub"));
            for (i, f) in self.settings.folders.iter().enumerate() {
                ui.horizontal(|ui| {
                    let r = widgets::icon_button(ui, Icon::Close, t!("settings.folder_remove"));
                    if i == 0 {
                        first_focus(&r, &mut self.focus_first);
                    }
                    if r.clicked() {
                        remove = Some(i);
                    }
                    ui.add(egui::Label::new(RichText::new(f.display().to_string()).color(TEXT)).truncate());
                });
            }
            if self.settings.folders.is_empty() {
                ui.label(RichText::new(t!("settings.no_folders")).color(TEXT3));
            }
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                let r = widgets::button(ui, Some(Icon::Add), t!("library.add_folder"), Kind::Primary);
                first_focus(&r, &mut self.focus_first);
                add = r.clicked();
                rescan = widgets::button(ui, Some(Icon::Refresh), t!("library.rescan"), Kind::Secondary).clicked();
            });
        });
        ui.add_space(12.0);
        let hidden = self.settings.hidden.len();
        let mut show_hidden = false;
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::setting(ui, &t!("settings.hidden", n = hidden), t!("settings.hidden_sub"), |ui| {
                if hidden > 0 {
                    show_hidden = widgets::button(ui, Some(Icon::Hidden), t!("settings.hidden_show"), Kind::Secondary).clicked();
                }
            });
        });
        if let Some(i) = remove {
            self.settings.folders.remove(i);
            self.rescan();
        }
        if add {
            self.add_folder();
        }
        if rescan {
            self.rescan();
            self.say(t!("settings.rescanning"), true);
        }
        if show_hidden {
            self.view = View::Library;
            self.category = Category::Hidden;
        }
    }

    fn advanced_settings(&mut self, ui: &mut egui::Ui) {
        let mut setup = false;
        let mut reset = false;
        let mut logs = false;
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::card_title(ui, Icon::Wifi, theme::VIOLET, t!("settings.services"), t!("settings.services_sub"));
            let p = &mut self.settings.prefs;
            for (n, (sdk, label, default)) in SDK_MODES.iter().enumerate() {
                if n > 0 {
                    widgets::divider(ui);
                }
                let focus = &mut self.focus_first;
                widgets::setting(ui, sdk_name(sdk, label), "", |ui| {
                    let cur = p.modes.get(*sdk).cloned().unwrap_or_else(|| default.to_string());
                    let labels: Vec<String> = mode_labels(sdk).iter().map(|s| s.to_string()).collect();
                    let mut i = mode_values().iter().position(|m| *m == cur).unwrap_or(0);
                    let r = widgets::stepper(ui, sdk, &labels, &mut i, 250.0);
                    if n == 0 {
                        first_focus(&r, focus);
                    }
                    if r.changed() {
                        if mode_values()[i] == *default {
                            p.modes.remove(*sdk);
                        } else {
                            p.modes.insert(sdk.to_string(), mode_values()[i].to_string());
                        }
                    }
                });
            }
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(t!("settings.services_note")).size(12.0).color(TEXT3)).wrap());
            widgets::divider(ui);
            widgets::setting(ui, t!("settings.steam_saves"), t!("settings.steam_saves_sub"), |ui| {
                widgets::switch(ui, &mut p.steam_saves);
            });
        });
        ui.add_space(12.0);
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::card_title(ui, Icon::Help, theme::WARN, t!("settings.troubleshoot"), "");
            widgets::setting(ui, t!("settings.logs"), t!("settings.logs_sub"), |ui| {
                logs = widgets::button(ui, Some(Icon::Open), t!("settings.logs_open"), Kind::Secondary).clicked();
            });
            widgets::divider(ui);
            widgets::setting(ui, t!("settings.setup_again"), t!("settings.setup_again_sub"), |ui| {
                setup = widgets::button(ui, Some(Icon::Bolt), t!("settings.setup_again_go"), Kind::Secondary).clicked();
            });
            widgets::divider(ui);
            let changed = differs_from_recommended(&self.settings.prefs);
            widgets::setting(ui, t!("settings.reset"), if changed { t!("settings.reset_sub_changed") } else { t!("settings.reset_sub") }, |ui| {
                ui.add_enabled_ui(changed, |ui| {
                    reset = widgets::button(ui, Some(Icon::Refresh), t!("settings.reset_go"), Kind::Danger).clicked();
                });
            });
        });
        if logs {
            let dir = crate::platform::logs_dir();
            let _ = std::fs::create_dir_all(&dir);
            crate::platform::reveal(&dir);
        }
        if setup {
            self.welcome = Some(super::welcome::Welcome::again(&self.settings.prefs));
        }
        if reset {
            reset_to_recommended(&mut self.settings.prefs);
            self.say(t!("settings.reset_done"), true);
        }
    }

    fn about(&mut self, ui: &mut egui::Ui) {
        let mut data = false;
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(egui::vec2(46.0, 46.0), egui::Sense::hover());
                super::brand_mark(ui.painter(), r);
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("PeerParty").font(theme::strong(20.0)).color(TEXT));
                    ui.add_space(-6.0);
                    ui.label(RichText::new(t!("settings.version", version = env!("CARGO_PKG_VERSION"))).color(TEXT2));
                });
            });
            ui.add_space(8.0);
            ui.add(egui::Label::new(RichText::new(t!("settings.about_text")).color(TEXT2)).wrap());
            ui.add_space(8.0);
            let r = widgets::button(ui, Some(Icon::Folder), t!("settings.data_folder"), Kind::Secondary);
            first_focus(&r, &mut self.focus_first);
            data = r.clicked();
        });
        ui.add_space(12.0);
        self.updates_card(ui);
        if data {
            let dir = super::store::data_dir();
            let _ = std::fs::create_dir_all(&dir);
            crate::platform::reveal(&dir);
        }
    }
}

fn overlay(ui: &mut egui::Ui, p: &mut Prefs, focus: &mut bool) {
    widgets::card(ui, |ui| {
        ui.set_width(ui.available_width());
        // Off, invites from the network and the room are accepted by the hook (nothing in
        // the game could answer them): said where it's turned off.
        let sub = if p.overlay { t!("settings.overlay_on_sub") } else { t!("settings.overlay_off_sub") };
        widgets::setting(ui, t!("settings.overlay_on"), sub, |ui| {
            let r = widgets::switch(ui, &mut p.overlay);
            first_focus(&r, focus);
        });
    });
    ui.add_space(12.0);
    ui.add_enabled_ui(p.overlay, |ui| {
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::card_title(ui, Icon::Keyboard, theme::ACCENT, t!("settings.open_with"), t!("settings.open_with_sub"));
            let hk: &mut Hotkeys = &mut p.hotkeys;
            for (k, label, sub) in [
                (&mut hk.shift_tab, "Shift+Tab", t!("settings.key_steam")),
                (&mut hk.shift_f3, "Shift+F3", t!("settings.key_epic")),
                (&mut hk.shift_f1, "Shift+F1", t!("settings.key_ea")),
                (&mut hk.shift_f2, "Shift+F2", t!("settings.key_ubisoft")),
            ] {
                widgets::setting(ui, label, sub, |ui| {
                    widgets::switch(ui, k);
                });
                widgets::divider(ui);
            }
            widgets::setting(ui, t!("settings.key_guide"), t!("settings.key_guide_sub"), |ui| {
                widgets::switch(ui, &mut hk.guide);
            });
            widgets::divider(ui);
            let c = hk.chord;
            let chord_text = if c.count_ones() >= 2 { t!("settings.chord_set", buttons = crate::host::pad::names(c)) } else { t!("settings.chord_none").to_string() };
            widgets::setting(ui, t!("settings.chord"), &chord_text, |ui| {
                if c.count_ones() >= 2 && widgets::button(ui, None, t!("common.clear"), Kind::Secondary).clicked() {
                    hk.chord = 0;
                }
            });
        });
        ui.add_space(12.0);
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::card_title(ui, Icon::Brush, theme::VIOLET, t!("settings.look"), "");
            widgets::setting(ui, t!("settings.size"), "", |ui| {
                let sizes = [0.75f32, 0.875, 1.0, 1.125, 1.25, 1.375, 1.5];
                let labels: Vec<String> = sizes.iter().map(|s| format!("{:.0}%", s * 100.0)).collect();
                let mut i = sizes.iter().position(|s| (s - p.overlay_scale).abs() < 0.01).unwrap_or(2);
                if widgets::stepper(ui, "size", &labels, &mut i, 160.0).changed() {
                    p.overlay_scale = sizes[i];
                }
            });
            widgets::divider(ui);
            widgets::setting(ui, t!("settings.glyphs"), t!("settings.glyphs_sub"), |ui| {
                let values = ["auto", "xbox", "playstation", "nintendo"];
                let labels = vec![t!("settings.glyphs_auto").to_string(), "Xbox".into(), "PlayStation".into(), "Nintendo".into()];
                let mut i = values.iter().position(|v| *v == p.glyphs).unwrap_or(0);
                if widgets::stepper(ui, "glyphs", &labels, &mut i, 220.0).changed() {
                    p.glyphs = values[i].to_string();
                }
            });
            widgets::divider(ui);
            widgets::setting(ui, t!("settings.draw"), t!("settings.draw_sub"), |ui| {
                let values = ["auto", "window", "ingame"];
                let labels = vec![t!("settings.draw_auto").to_string(), t!("settings.draw_window").to_string(), t!("settings.draw_ingame").to_string()];
                let mut i = values.iter().position(|v| *v == p.draw).unwrap_or(0);
                if widgets::stepper(ui, "draw", &labels, &mut i, 240.0).changed() {
                    p.draw = values[i].to_string();
                }
            });
        });
    });
}

/// Anything "Reset to recommended" would change.
fn differs_from_recommended(p: &Prefs) -> bool {
    let mut r = p.clone();
    reset_to_recommended(&mut r);
    r != *p
}

/// The recommended answer to everything but who you are and who can join you.
fn reset_to_recommended(p: &mut Prefs) {
    let d = Prefs::default();
    p.modes = d.modes;
    p.hotkeys = d.hotkeys;
    p.overlay = d.overlay;
    p.overlay_scale = d.overlay_scale;
    p.glyphs = d.glyphs;
    p.draw = d.draw;
    p.invite_buttons = d.invite_buttons;
    p.steam_saves = d.steam_saves;
    p.toasts = d.toasts;
    p.effects = d.effects;
    theme::set_effects(p.effects);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reset leaves who you are and who can join alone.
    #[test]
    fn reset_keeps_identity_and_play_together() {
        let mut p = Prefs { name: "Kim".into(), room: "PP1-x".into(), public: true, auto_accept: true, voice: false, overlay_scale: 1.5, ..Prefs::default() };
        p.modes.insert("eos".into(), "official".into());
        assert!(differs_from_recommended(&p));
        reset_to_recommended(&mut p);
        assert!(!differs_from_recommended(&p));
        assert_eq!((p.name.as_str(), p.room.as_str(), p.public, p.auto_accept, p.voice), ("Kim", "PP1-x", true, true, false));
        assert!(p.modes.is_empty() && p.overlay_scale == 1.0);
    }
}
