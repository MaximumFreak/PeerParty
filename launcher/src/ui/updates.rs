//! The update check in the window (crate::update): the daily check, a message when a
//! newer PeerParty is out, and the Updates card in Settings > About.

use eframe::egui::{self, RichText};

use super::App;
use super::icons::Icon;
use super::theme::{self, TEXT, TEXT2, TEXT3};
use super::widgets::{self, Kind};
use crate::t;
use crate::update::{self, Status};

impl App {
    /// Each frame: the daily check when it is due, and one message per new version.
    pub(super) fn updates_tick(&mut self, ctx: &egui::Context) {
        if !self.live {
            return;
        }
        let now = update::now_secs();
        if self.settings.check_updates && now.saturating_sub(self.settings.update_checked) >= update::EVERY.as_secs() && !update::busy() {
            self.settings.update_checked = now;
            self.save_settings();
            let c = ctx.clone();
            update::check_in_background(move || c.request_repaint());
        }
        if let Some(r) = update::available()
            && self.update_told.as_deref() != Some(r.version.as_str())
        {
            self.update_told = Some(r.version.clone());
            self.say(t!("update.snack", version = r.version), true);
        }
    }

    pub(super) fn updates_card(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let status = update::status();
        let addon = update::in_addon();
        let games = !self.sessions.is_empty();
        let mut check = false;
        let mut download = None;
        let mut notes = None;
        let mut elsewhere = None;
        let mut cancel = false;
        let mut install = None;
        let mut toggled = false;
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::card_title(ui, Icon::Refresh, theme::ACCENT, t!("update.title"), t!("update.sub"));
            let on = &mut self.settings.check_updates;
            widgets::setting(ui, t!("update.auto"), t!("update.auto_sub"), |ui| {
                toggled = widgets::switch(ui, on).changed();
            });
            widgets::divider(ui);
            let line = |ui: &mut egui::Ui, text: &str, color| {
                ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
            };
            let note = |ui: &mut egui::Ui, text: &str| {
                ui.add(egui::Label::new(RichText::new(text).size(12.0).color(TEXT3)).wrap());
            };
            match &status {
                None | Some(Status::UpToDate) | Some(Status::Failed(_)) => {
                    match &status {
                        Some(Status::UpToDate) => line(ui, &t!("update.up_to_date", version = env!("CARGO_PKG_VERSION")), TEXT),
                        Some(Status::Failed(why)) => line(ui, why, theme::WARN),
                        _ => line(ui, t!("update.never"), TEXT2),
                    }
                    ui.add_space(6.0);
                    check = widgets::button(ui, Some(Icon::Refresh), t!("update.check"), Kind::Secondary).clicked();
                }
                Some(Status::Checking) => line(ui, t!("update.checking"), TEXT2),
                Some(Status::Available(r)) => {
                    line(ui, &t!("update.available", version = r.version), TEXT);
                    if addon {
                        note(ui, &t!("update.addon"));
                    } else if !r.installable() {
                        note(ui, r.why.as_deref().unwrap_or(t!("update.cant_check")));
                    } else {
                        note(ui, t!("update.available_sub"));
                    }
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        if !addon && r.installable() && widgets::button(ui, Some(Icon::Open), t!("update.download"), Kind::Primary).clicked() {
                            download = Some(r.clone());
                        }
                        if widgets::button(ui, Some(Icon::Link), t!("update.notes"), Kind::Secondary).clicked() {
                            notes = Some(r.page.clone());
                        }
                    });
                }
                Some(Status::Waiting(r)) => {
                    line(ui, &t!("update.available", version = r.version), TEXT);
                    note(ui, t!("update.waiting"));
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        if widgets::button(ui, Some(Icon::Folder), t!("update.elsewhere"), Kind::Secondary).clicked() {
                            elsewhere = Some(r.clone());
                        }
                        cancel = widgets::button(ui, Some(Icon::Close), t!("update.cancel"), Kind::Secondary).clicked();
                    });
                }
                Some(Status::Ready(r, file)) => {
                    line(ui, &t!("update.ready", version = r.version), TEXT);
                    if games {
                        note(ui, t!("update.games_running"));
                    }
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.add_enabled_ui(!games, |ui| {
                            if widgets::button(ui, Some(Icon::Check), t!("update.install"), Kind::Primary).clicked() {
                                install = Some((r.clone(), file.clone()));
                            }
                        });
                        cancel = widgets::button(ui, Some(Icon::Close), t!("update.cancel"), Kind::Secondary).clicked();
                    });
                }
                Some(Status::Restarting) => line(ui, t!("update.restarting"), TEXT2),
            }
        });
        if toggled {
            self.save_settings();
        }
        if check {
            self.settings.update_checked = update::now_secs();
            self.save_settings();
            let c = ctx.clone();
            update::check_in_background(move || c.request_repaint());
        }
        if let Some(url) = notes {
            ctx.open_url(egui::OpenUrl::new_tab(url));
        }
        if let Some(r) = download {
            let c = ctx.clone();
            let open = ctx.clone();
            update::download(r, move |url| open.open_url(egui::OpenUrl::new_tab(url)), move || c.request_repaint());
        }
        if let Some(r) = elsewhere {
            let picked = rfd::FileDialog::new().set_title(t!("update.elsewhere")).add_filter("Programs", &["exe"]).pick_file();
            if let Some(file) = picked
                && update::use_file(r, file).is_err()
            {
                self.say(t!("update.wrong_file"), false);
            }
        }
        if cancel {
            update::cancel();
        }
        if let Some((r, file)) = install
            && update::install(r, file)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}
