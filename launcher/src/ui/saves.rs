//! A game's Saves card: each player's saves on this PC, copying, swapping, resetting and
//! restoring them between any two players (saves.rs does the work), and how players 2–4
//! keep theirs. Shown once more than one player is in play for the game, or a later
//! player has saves of its own.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;

use super::icons::Icon;
use super::scan::Game;
use super::store::{self, GameOptions};
use super::theme::{self, TEXT, TEXT2, TEXT3};
use super::widgets::{self, Kind};
use super::{Action, App};
use crate::saves::{GameSaves, PlayerSaves, SaveError};
use crate::t;

/// What the card asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Copy(u32, u32),
    Swap(u32, u32),
    Reset(u32),
    Restore(u32, String),
    Clean(u32),
    PickFolder,
    PlaySolo(u32),
}

/// The card's state: the players' saves as last looked at (looking walks their folders,
/// so not every frame), and the transfer's choice.
#[derive(Default)]
pub struct SavesView {
    seen: Option<(String, Instant, Vec<PlayerSaves>, bool)>,
    /// Whether a later player has saves, by game: looked at often (the panel asks each
    /// frame whether to show the card), so only whether their folders are there.
    later: Option<(String, Instant, bool)>,
    pub from: usize,
    pub to: usize,
}

/// How often the card looks at the disk again on its own: the players' saves (their
/// folders walked), and whether later players have any.
const LOOK_AGAIN: Duration = Duration::from_secs(10);
const LATER_AGAIN: Duration = Duration::from_secs(4);

/// The game's saves, for its key as the launch knows it (the exe Play starts).
pub fn game_saves(g: &Game, o: &GameOptions, folders: &[PathBuf]) -> GameSaves {
    let exe = o.exe.clone().unwrap_or_else(|| g.exe.clone());
    let root = crate::gameroot::find(&exe, folders).map(|(r, _)| r);
    let listed = std::path::absolute(&exe).unwrap_or(exe.clone());
    GameSaves::for_game(&store::key_of(&listed), Some(&exe), root.as_deref())
}

/// A size in KB or MB.
fn size(bytes: u64) -> String {
    if bytes >= 10 << 20 { format!("{} MB", bytes >> 20) } else { format!("{} KB", bytes.div_ceil(1024)) }
}

/// How long ago, roughly ("5 min", "3 h", "2 d").
fn ago(t: SystemTime) -> String {
    let s = t.elapsed().map_or(0, |d| d.as_secs());
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", s / 60),
        3600..=172_799 => format!("{} h", s / 3600),
        _ => format!("{} d", s / 86_400),
    }
}

impl App {
    /// The name player `slot` goes by (the PC's numbering when none is set).
    pub(super) fn player_name(&self, slot: u32) -> String {
        let p = &self.settings.prefs;
        let main = match p.name.trim() {
            "" => crate::platform::user_name(),
            n => n.to_string(),
        };
        let set = match slot {
            0 => main.as_str(),
            1 => p.other_name.trim(),
            2 => p.third_name.trim(),
            _ => p.fourth_name.trim(),
        };
        if set.is_empty() { format!("{main} ({})", slot + 1) } else { set.to_string() }
    }

    /// The players' saves for the card, looked at again every few seconds.
    pub(super) fn saves_seen(&mut self, key: &str, g: &Game, o: &GameOptions) -> (Vec<PlayerSaves>, bool) {
        let fresh = self.saves_view.seen.as_ref().is_some_and(|(k, at, ..)| k == key && at.elapsed() < LOOK_AGAIN);
        if !fresh {
            let saves = game_saves(g, o, &self.settings.folders);
            let known = !saves.discovered().is_empty();
            self.saves_view.seen = Some((key.to_string(), Instant::now(), saves.inspect(), known));
        }
        let (_, _, list, known) = self.saves_view.seen.as_ref().unwrap();
        (list.clone(), *known)
    }

    /// Whether a later player has saves of its own for the game.
    pub(super) fn later_saves(&mut self, key: &str, g: &Game, o: &GameOptions) -> bool {
        let fresh = self.saves_view.later.as_ref().is_some_and(|(k, at, _)| k == key && at.elapsed() < LATER_AGAIN);
        if !fresh {
            let any = game_saves(g, o, &self.settings.folders).later_players_have_saves();
            self.saves_view.later = Some((key.to_string(), Instant::now(), any));
        }
        self.saves_view.later.as_ref().is_some_and(|l| l.2)
    }

    pub(super) fn saves_card(&mut self, ui: &mut egui::Ui, g: &Game, key: &str, o: &mut GameOptions, running: &[u8], actions: &mut Vec<Action>) {
        let (players, known) = self.saves_seen(key, g, o);
        let busy = !running.is_empty() || players.iter().any(|p| p.running);
        let shared = o.save_mode.eq_ignore_ascii_case("shared");
        let shown = usize::from(o.players.clamp(2, 4)).max(players.iter().rposition(PlayerSaves::has_saves).map_or(0, |i| i + 1));
        widgets::card(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(t!("saves.title")).font(theme::strong(15.0)).color(TEXT));
            ui.add_space(2.0);
            if shared {
                widgets::note(ui, Icon::Warning, theme::WARN, t!("saves.shared"));
            } else {
                widgets::note(ui, Icon::Shield, theme::GOOD, t!("saves.own"));
            }
            ui.add_space(4.0);
            for p in players.iter().take(shown) {
                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(t!("saves.player", n = p.slot + 1, name = self.player_name(p.slot))).font(theme::strong(13.5)).color(TEXT));
                    let status = match p.newest {
                        // Player 1's saves are wherever the game keeps them: unknown isn't none.
                        _ if p.slot == 0 && !known => t!("saves.not_found").to_string(),
                        _ if !p.has_saves() => t!("saves.none").to_string(),
                        Some(t) => format!("{} · {}", size(p.bytes), ago(t)),
                        None => size(p.bytes),
                    };
                    ui.label(egui::RichText::new(status).size(12.0).color(TEXT2));
                    let kinds: Vec<&str> = p.kinds.iter().map(|k| k.label()).collect();
                    if !kinds.is_empty() {
                        ui.label(egui::RichText::new(kinds.join(" · ")).size(11.0).color(TEXT3));
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        if widgets::button(ui, Some(Icon::Play), t!("saves.play_solo"), Kind::Secondary).clicked() {
                            actions.push(Action::Saves(key.to_string(), Op::PlaySolo(p.slot)));
                        }
                    });
                    if let Some(f) = &p.folder {
                        if widgets::button(ui, Some(Icon::Folder), t!("saves.open"), Kind::Ghost).clicked() {
                            actions.push(Action::Reveal(f.clone()));
                        }
                    }
                    if p.has_saves() {
                        ui.add_enabled_ui(!busy, |ui| {
                            if widgets::button(ui, None, t!("saves.reset"), Kind::Ghost).on_hover_text(t!("saves.reset_tip")).clicked() {
                                actions.push(Action::Saves(key.to_string(), Op::Reset(p.slot)));
                            }
                        });
                    }
                });
            }
            ui.add_space(6.0);
            // From one player to another, folded away: rarely needed, and tall.
            let labels: Vec<String> = (0..shown).map(|k| t!("game.input_player", n = k + 1)).collect();
            self.saves_view.from = self.saves_view.from.min(shown - 1);
            self.saves_view.to = self.saves_view.to.min(shown - 1);
            if self.saves_view.from == self.saves_view.to {
                self.saves_view.to = (self.saves_view.from + 1) % shown;
            }
            egui::CollapsingHeader::new(egui::RichText::new(t!("saves.transfer")).size(13.5).color(TEXT)).id_salt(("saves-transfer", key)).show(ui, |ui| {
                let w = (ui.available_width() / 2.0 - 16.0).max(120.0);
                ui.horizontal(|ui| {
                    let mut from = self.saves_view.from;
                    if widgets::stepper(ui, "saves-from", &labels, &mut from, w).changed() {
                        self.saves_view.from = from;
                    }
                    ui.label(egui::RichText::new("→").color(TEXT2));
                    let mut to = self.saves_view.to;
                    if widgets::stepper(ui, "saves-to", &labels, &mut to, w).changed() {
                        self.saves_view.to = to;
                    }
                });
                let (from, to) = (self.saves_view.from as u32, self.saves_view.to as u32);
                if players.get(to as usize).is_some_and(PlayerSaves::has_saves) {
                    let color = if to == 0 { theme::WARN } else { TEXT3 };
                    widgets::note(ui, Icon::Info, color, &t!("saves.replace_note", n = to + 1));
                }
                ui.horizontal_wrapped(|ui| {
                    ui.add_enabled_ui(!busy && from != to, |ui| {
                        if widgets::button(ui, Some(Icon::Copy), t!("saves.copy"), Kind::Secondary).on_disabled_hover_text(t!("saves.running")).clicked() {
                            actions.push(Action::Saves(key.to_string(), Op::Copy(from, to)));
                        }
                        if widgets::button(ui, Some(Icon::Refresh), t!("saves.swap"), Kind::Ghost).on_disabled_hover_text(t!("saves.running")).clicked() {
                            actions.push(Action::Saves(key.to_string(), Op::Swap(from, to)));
                        }
                    });
                });
            });
            if busy {
                widgets::note(ui, Icon::Info, TEXT3, t!("saves.running"));
            }
            if !known {
                widgets::note(ui, Icon::Info, TEXT3, t!("saves.unknown"));
                ui.horizontal(|ui| {
                    ui.add_space(22.0);
                    if widgets::button(ui, Some(Icon::Folder), t!("saves.pick"), Kind::Secondary).clicked() {
                        actions.push(Action::Saves(key.to_string(), Op::PickFolder));
                    }
                });
            }
            // Backups, newest first, by player.
            let any_backups = players.iter().any(|p| !p.backups.is_empty());
            if any_backups {
                egui::CollapsingHeader::new(egui::RichText::new(t!("saves.backups")).size(13.5).color(TEXT)).id_salt(("saves-backups", key)).show(ui, |ui| {
                    for p in players.iter().filter(|p| !p.backups.is_empty()) {
                        ui.label(egui::RichText::new(t!("saves.player", n = p.slot + 1, name = self.player_name(p.slot))).size(12.5).color(TEXT2));
                        for b in &p.backups {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(&b.id).size(12.0).color(TEXT3));
                                ui.add_enabled_ui(!busy, |ui| {
                                    if widgets::button(ui, None, t!("saves.restore"), Kind::Ghost).clicked() {
                                        actions.push(Action::Saves(key.to_string(), Op::Restore(p.slot, b.id.clone())));
                                    }
                                });
                            });
                        }
                        if p.backups.len() > 1 && widgets::button(ui, None, t!("saves.clean"), Kind::Ghost).clicked() {
                            actions.push(Action::Saves(key.to_string(), Op::Clean(p.slot)));
                        }
                    }
                });
            }
            ui.add_space(8.0);
            ui.label(egui::RichText::new(t!("saves.mode")).size(13.5).color(TEXT));
            let mut mode = if shared { "shared" } else { "" };
            ui.push_id("save-mode", |ui| {
                if widgets::segmented(ui, &mut mode, &[("", t!("saves.mode_own")), ("shared", t!("saves.mode_shared"))]) {
                    o.save_mode = mode.to_string();
                }
            });
            if !shared {
                ui.add_space(4.0);
                let mut seed = o.seed_saves;
                super::game::stacked(ui, t!("saves.seed"), t!("saves.seed_sub"), |ui| {
                    if widgets::switch(ui, &mut seed).changed() {
                        o.seed_saves = seed;
                    }
                });
            }
        });
    }

    /// Does what the Saves card asked.
    pub(super) fn saves_op(&mut self, key: &str, op: Op) {
        let Some(g) = self.lib.game(key).cloned() else { return };
        let o = self.settings.options(key);
        let saves = game_saves(&g, &o, &self.settings.folders);
        self.saves_view.seen = None;
        self.saves_view.later = None;
        let result: Result<String, SaveError> = match op {
            Op::Copy(from, to) => saves.copy(from, to).map(|r| t!("saves.done_copy", from = from + 1, to = to + 1, n = r.files)),
            Op::Swap(a, b) => saves.swap(a, b).map(|_| t!("saves.done_swap", a = a + 1, b = b + 1)),
            Op::Reset(n) => saves.reset(n).map(|_| t!("saves.done_reset", n = n + 1)),
            Op::Restore(n, id) => saves.restore(n, &id).map(|_| t!("saves.done_restore", n = n + 1)),
            Op::Clean(n) => Ok(t!("saves.done_clean", n = saves.clean_backups(n))),
            Op::PickFolder => {
                let Some(dir) = rfd::FileDialog::new().set_title(t!("saves.pick")).pick_folder() else { return };
                saves.add_discovered_folder(&dir).map(|()| t!("saves.done_pick", folder = dir.display()))
            }
            Op::PlaySolo(slot) => {
                let solo = GameOptions { players: 1, split_screen: false, play_as: (slot + 1) as u8, ..o };
                let args = self.command(&g, &solo);
                if self.spawn(key, &g.name, args, (slot + 1) as u8, 0) {
                    self.say(t!("snack.started", game = g.name), true);
                    self.minimize_after_start();
                }
                return;
            }
        };
        match result {
            Ok(text) => self.say(text, true),
            Err(e) => self.say(t!("saves.failed", error = e), false),
        }
    }
}
