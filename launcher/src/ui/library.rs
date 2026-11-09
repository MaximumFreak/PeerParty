//! The library: search, filters, and the games as covers (or a list). A card lights up
//! where the pointer is and its edge catches that light; the selected one glows.

use eframe::egui::{self, Align2, Color32, CornerRadius, Margin, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};

use super::icons::Icon;
use super::scan::{Game, Source};
use super::theme::{self, ACCENT, ACCENT_HI, TEXT, TEXT2, TEXT3};
use super::{Action, App, Category, IconSlot, file_uri, first_focus, fx, hover_in, store, widgets};
use crate::t;

const CARD_W: f32 = 164.0;
const CARD_H: f32 = 246.0;
const GAP: f32 = 18.0;
const ROW_H: f32 = 56.0;

impl App {
    pub(super) fn library(&mut self, ui: &mut egui::Ui) {
        self.top_bar_ids.clear();
        ui.add_space(20.0);
        self.top_bar(ui);
        ui.add_space(10.0);
        self.chips(ui);
        ui.add_space(14.0);
        if self.settings.list_view {
            self.list(ui);
        } else {
            self.grid(ui);
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 170.0).clamp(220.0, 460.0);
            let search = ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text(egui::RichText::new(t!("library.search")).color(TEXT3))
                    .desired_width(w)
                    .margin(Margin { left: 36, right: 12, top: 9, bottom: 9 })
                    .background_color(theme::FIELD),
            );
            if search.has_focus() {
                fx::glow(ui.painter(), search.rect, 6.0, ACCENT.gamma_multiply(0.25), 8.0);
            }
            Icon::Search.paint(ui.painter(), search.rect.left_center() + vec2(18.0, 0.0), 15.0, if search.has_focus() { ACCENT_HI } else { TEXT3 });
            self.top_bar_ids.insert(search.id);
            if std::mem::take(&mut self.focus_search) {
                search.request_focus();
            }
            if self.scanning {
                ui.add(egui::Spinner::new().size(16.0).color(TEXT3));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let add = widgets::button(ui, Some(Icon::Add), t!("library.add"), widgets::Kind::Primary);
                self.top_bar_ids.insert(add.id);
                egui::Popup::menu(&add).show(|ui| {
                    ui.set_min_width(240.0);
                    if ui.button(t!("library.add_game")).clicked() {
                        self.add_game();
                    }
                    if ui.button(t!("library.add_folder")).clicked() {
                        self.add_folder();
                    }
                });
                let r = widgets::icon_button(ui, Icon::Refresh, t!("library.rescan"));
                self.top_bar_ids.insert(r.id);
                if r.clicked() {
                    self.rescan();
                }
                let (icon, tip) = if self.settings.list_view { (Icon::Grid, t!("library.grid_view")) } else { (Icon::List, t!("library.list_view")) };
                let r = widgets::icon_button(ui, icon, tip);
                self.top_bar_ids.insert(r.id);
                if r.clicked() {
                    self.settings.list_view = !self.settings.list_view;
                    self.save_settings();
                }
            });
        });
    }

    fn chips(&mut self, ui: &mut egui::Ui) {
        let cats = self.categories();
        if !cats.contains(&self.category) {
            self.category = Category::All;
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            for c in cats {
                let label = match c {
                    Category::All => t!("filter.all"),
                    Category::Recent => t!("filter.recent"),
                    Category::Hidden => t!("filter.hidden"),
                    Category::Source(s) => super::source_name(s),
                };
                let n = self.count(c);
                let r = widgets::chip(ui, label, Some(n), self.category == c);
                self.top_bar_ids.insert(r.id);
                if self.category == c {
                    first_focus(&r, &mut self.focus_first);
                }
                if r.clicked() {
                    self.category = c;
                }
            }
        });
    }

    /// The scroll offset that brings row `row` of `row_h` into a view `view_h` tall.
    fn offset_for(&self, ui: &egui::Ui, id: egui::Id, row: usize, row_h: f32, view_h: f32) -> Option<f32> {
        let cur = egui::scroll_area::State::load(ui.ctx(), id).map_or(0.0, |s| s.clamped_offset().y);
        let y = row as f32 * row_h;
        if y < cur {
            Some(y)
        } else if y + row_h > cur + view_h {
            Some(y + row_h - view_h)
        } else {
            None
        }
    }

    fn grid(&mut self, ui: &mut egui::Ui) {
        let visible = self.visible();
        if visible.is_empty() {
            self.empty_state(ui);
            return;
        }
        let width = ui.available_width();
        let cols = (((width + GAP) / (CARD_W + GAP)).floor() as usize).max(1);
        self.grid_cols = cols;
        let rows = visible.len().div_ceil(cols);
        let mut actions = Vec::new();
        let mut need = Vec::new();
        let mut area = egui::ScrollArea::vertical().id_salt("grid").auto_shrink([false, false]);
        if std::mem::take(&mut self.scroll_to_selected) {
            let at = self.selected.as_ref().and_then(|k| visible.iter().position(|&i| store::key_of(&self.entries[i].exe) == *k));
            if let Some(o) = at.and_then(|a| self.offset_for(ui, ui.make_persistent_id("grid"), a / cols, CARD_H + GAP, ui.available_height())) {
                area = area.vertical_scroll_offset(o);
            }
        }
        // Cards spread evenly across the width.
        let spare = (width - cols as f32 * CARD_W - (cols - 1) as f32 * GAP).max(0.0);
        let gap = GAP + if cols > 1 { spare / (cols - 1) as f32 } else { 0.0 };
        area.show_rows(ui, CARD_H + GAP, rows, |ui, range| {
            for row in range {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap.min(GAP + 24.0);
                    for &i in visible.iter().skip(row * cols).take(cols) {
                        self.card(ui, i, &mut actions, &mut need);
                    }
                });
                ui.add_space(GAP - ui.spacing().item_spacing.y);
            }
        });
        self.request_icons(need);
        self.apply(actions);
    }

    /// A game's picture, filling `rect`: its cover, else its art across a lit
    /// background, else its icon on one.
    pub(super) fn paint_picture(&self, ui: &egui::Ui, g: &Game, key: &str, rect: Rect, round: CornerRadius, need: &mut Vec<String>) {
        if let Some(cover) = &g.cover {
            egui::Image::from_uri(file_uri(cover)).corner_radius(round).fit_to_exact_size(rect.size()).paint_at(ui, rect);
            return;
        }
        let p = ui.painter_at(rect);
        let glow = self.glow_of(g, key);
        let base = theme::tint_for(&g.name);
        let r = round.nw as f32;
        fx::gradient(&p, rect, r, true, &[(0.0, theme::mix(base, glow, 0.10)), (1.0, theme::mix(base, Color32::BLACK, 0.6))]);
        fx::light(&p, rect.center() - vec2(0.0, rect.height() * 0.12), Vec2::splat(rect.width() * 0.75), glow.gamma_multiply(0.16));
        if let Some(art) = &g.art {
            if g.source == Source::Steam {
                // A wide header: fit across the middle.
                let h = rect.width() * 215.0 / 460.0;
                let r = Rect::from_center_size(rect.center() - vec2(0.0, rect.height() * 0.08), vec2(rect.width(), h));
                egui::Image::from_uri(file_uri(art)).fit_to_exact_size(r.size()).paint_at(ui, r);
                return;
            }
        }
        let side = (rect.width() * 0.46).min(96.0);
        let icon_rect = Rect::from_center_size(rect.center() - vec2(0.0, rect.height() * 0.1), Vec2::splat(side));
        match self.icons.get(key) {
            Some(IconSlot::Ready(tex)) => {
                p.image(tex.id(), icon_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
            Some(IconSlot::Missing) | Some(IconSlot::Loading) => {
                let letter = g.name.chars().next().unwrap_or('?').to_uppercase().to_string();
                p.text(icon_rect.center(), Align2::CENTER_CENTER, letter, theme::strong(side * 0.62), Color32::from_white_alpha(170));
            }
            None => need.push(key.to_string()),
        }
    }

    fn card(&self, ui: &mut egui::Ui, i: usize, actions: &mut Vec<Action>, need: &mut Vec<String>) {
        let g = &self.entries[i];
        let key = store::key_of(&g.exe);
        let (rect, resp) = ui.allocate_exact_size(vec2(CARD_W, CARD_H), Sense::click());
        if !ui.is_rect_visible(rect) {
            return;
        }
        if g.cover.is_none() && !self.icons.contains_key(&key) {
            need.push(key.clone());
        }
        let selected = self.selected.as_deref() == Some(key.as_str());
        let hot = theme::anim(ui.ctx(), resp.id.with("hot"), resp.hovered());
        let sel = theme::anim(ui.ctx(), resp.id.with("sel"), selected);
        let rect = rect.expand(2.5 * hot.max(sel));
        let r = 12.0;
        let p = ui.painter().clone();
        let glow = self.glow_of(g, &key);
        fx::shadow(&p, rect, r, 18 + (10.0 * hot) as u8, 6, 110);
        if sel > 0.0 {
            fx::glow(&p, rect, r, ACCENT.gamma_multiply(0.5 * sel), 18.0);
        } else if hot > 0.0 {
            fx::glow(&p, rect, r, glow.gamma_multiply(0.22 * hot), 16.0);
        }
        self.paint_picture(ui, g, &key, rect, CornerRadius::same(r as u8), need);
        // The name on a dark fade at the bottom.
        fx::gradient(&p, rect, r, true, &[(0.0, Color32::TRANSPARENT), (0.55, Color32::TRANSPARENT), (0.82, Color32::from_black_alpha(150)), (1.0, Color32::from_black_alpha(220))]);
        let mut job = egui::text::LayoutJob::simple(g.name.clone(), theme::strong(14.0), TEXT, CARD_W - 24.0);
        job.wrap.max_rows = 2;
        let galley = p.layout_job(job);
        p.galley(pos2(rect.left() + 12.0, rect.bottom() - 12.0 - galley.size().y), galley, TEXT);
        widgets::badge(&p, rect.left_top() + vec2(10.0, 10.0), self.origin(g, &key), TEXT, true);
        if !self.running(&key).is_empty() {
            fx::dot(&p, rect.right_top() + vec2(-16.0, 20.0), 5.0, theme::GOOD);
        }
        if let Some(at) = hover_in(ui, rect).filter(|_| self.welcome.is_none()) {
            fx::spotlight(&p, rect, r, at, 150.0, Color32::from_white_alpha((30.0 * hot) as u8));
            fx::rim(&p, rect, r, at, 140.0, Color32::from_white_alpha(18), Color32::from_white_alpha((150.0 * hot) as u8), 1.2);
        } else {
            p.rect_stroke(rect, r, Stroke::new(1.0, Color32::from_white_alpha(16)), StrokeKind::Inside);
        }
        if sel > 0.0 {
            p.rect_stroke(rect, r, Stroke::new(2.0, ACCENT_HI.gamma_multiply(sel)), StrokeKind::Inside);
        }
        if resp.clicked() {
            actions.push(Action::Select(key.clone()));
        }
        if resp.double_clicked() {
            actions.push(Action::Play(key.clone()));
        }
        let resp = if self.welcome.is_none() { resp.on_hover_text(g.exe.display().to_string()) } else { resp };
        resp.context_menu(|ui| self.menu(ui, g, &key, actions));
    }

    fn menu(&self, ui: &mut egui::Ui, g: &Game, key: &str, actions: &mut Vec<Action>) {
        ui.set_min_width(220.0);
        if ui.button(t!("game.play")).clicked() {
            actions.push(Action::Play(key.into()));
            ui.close();
        }
        if ui.button(t!("game.show_in_folder")).clicked() {
            actions.push(Action::Reveal(g.exe.clone()));
            ui.close();
        }
        if ui.button(t!("game.save_bat")).clicked() {
            actions.push(Action::MakeBat(key.into()));
            ui.close();
        }
        let hidden = self.hidden(key);
        if ui.button(if hidden { t!("game.unhide") } else { t!("game.hide") }).clicked() {
            actions.push(Action::Hide(key.into(), !hidden));
            ui.close();
        }
    }

    fn list(&mut self, ui: &mut egui::Ui) {
        let visible = self.visible();
        if visible.is_empty() {
            self.empty_state(ui);
            return;
        }
        let mut actions = Vec::new();
        let mut need = Vec::new();
        let mut area = egui::ScrollArea::vertical().id_salt("list").auto_shrink([false, false]);
        if std::mem::take(&mut self.scroll_to_selected) {
            let at = self.selected.as_ref().and_then(|k| visible.iter().position(|&i| store::key_of(&self.entries[i].exe) == *k));
            if let Some(o) = at.and_then(|a| self.offset_for(ui, ui.make_persistent_id("list"), a, ROW_H, ui.available_height())) {
                area = area.vertical_scroll_offset(o);
            }
        }
        area.show_rows(ui, ROW_H, visible.len(), |ui, range| {
            for &i in &visible[range] {
                let g = &self.entries[i];
                let key = store::key_of(&g.exe);
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width() - 8.0, ROW_H - 6.0), Sense::click());
                let selected = self.selected.as_deref() == Some(key.as_str());
                let hot = theme::anim(ui.ctx(), resp.id.with("hot"), resp.hovered());
                let p = ui.painter().clone();
                if selected {
                    fx::glow(&p, rect, 10.0, ACCENT.gamma_multiply(0.3), 10.0);
                    fx::gradient(&p, rect, 10.0, false, &[(0.0, theme::mix(theme::CARD, ACCENT, 0.35)), (1.0, theme::CARD)]);
                } else {
                    p.rect_filled(rect, 10.0, theme::mix(theme::CARD, theme::CARD_HI, hot));
                }
                if let Some(at) = hover_in(ui, rect) {
                    fx::spotlight(&p, rect, 10.0, at, 220.0, Color32::from_white_alpha((14.0 * hot) as u8));
                }
                p.rect_stroke(rect, 10.0, Stroke::new(1.0, if selected { ACCENT_HI.gamma_multiply(0.6) } else { theme::line() }), StrokeKind::Inside);
                let icon = Rect::from_min_size(rect.min + vec2(10.0, 7.0), Vec2::splat(rect.height() - 14.0));
                match self.icons.get(&key) {
                    Some(IconSlot::Ready(tex)) => {
                        p.image(tex.id(), icon, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                    }
                    None => need.push(key.clone()),
                    _ => {
                        p.rect_filled(icon, 6.0, theme::tint_for(&g.name));
                    }
                }
                let tx = icon.right() + 12.0;
                p.text(pos2(tx, rect.center().y - 8.0), Align2::LEFT_CENTER, &g.name, theme::strong(14.5), TEXT);
                let path = p.layout_no_wrap(g.exe.display().to_string(), theme::font(11.5), TEXT3);
                p.with_clip_rect(Rect::from_min_max(pos2(tx, rect.top()), pos2(rect.right() - 150.0, rect.bottom()))).galley(pos2(tx, rect.center().y + 3.0), path, TEXT3);
                let origin = self.origin(g, &key);
                let bw = p.layout_no_wrap(origin.to_string(), theme::strong(11.0), TEXT).size().x + 14.0;
                widgets::badge(&p, pos2(rect.right() - 14.0 - bw, rect.center().y - 10.0), origin, TEXT2, false);
                if !self.running(&key).is_empty() {
                    fx::dot(&p, pos2(rect.right() - 30.0 - bw, rect.center().y), 4.5, theme::GOOD);
                }
                if resp.clicked() {
                    actions.push(Action::Select(key.clone()));
                }
                if resp.double_clicked() {
                    actions.push(Action::Play(key.clone()));
                }
                resp.context_menu(|ui| self.menu(ui, g, &key, &mut actions));
            }
        });
        self.request_icons(need);
        self.apply(actions);
    }

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        ui.add_space(90.0);
        ui.vertical_centered(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(84.0, 84.0), Sense::hover());
            let p = ui.painter();
            fx::light(p, r.center(), Vec2::splat(90.0), ACCENT.gamma_multiply(0.16));
            let icon = if self.scanning { Icon::Refresh } else if self.search.is_empty() { Icon::Library } else { Icon::Search };
            icon.paint(p, r.center(), 38.0, TEXT2);
            ui.add_space(8.0);
            let (title, sub) = if self.scanning {
                (t!("library.scanning").to_string(), String::new())
            } else if !self.search.is_empty() {
                (t!("library.no_match", text = self.search), t!("library.no_match_sub").to_string())
            } else if self.category == Category::Recent {
                (t!("library.no_recent").to_string(), t!("library.no_recent_sub").to_string())
            } else {
                (t!("library.empty").to_string(), t!("library.empty_sub").to_string())
            };
            ui.label(egui::RichText::new(title).font(theme::strong(19.0)).color(TEXT));
            if !sub.is_empty() {
                ui.label(egui::RichText::new(sub).color(TEXT2));
            }
            if !self.scanning && self.search.is_empty() && self.category != Category::Recent {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let w = 2.0 * 160.0 + 8.0;
                    ui.add_space((ui.available_width() - w).max(0.0) / 2.0);
                    if widgets::button(ui, Some(Icon::Add), t!("library.add_game"), widgets::Kind::Primary).clicked() {
                        self.add_game();
                    }
                    if widgets::button(ui, Some(Icon::Folder), t!("library.add_folder"), widgets::Kind::Secondary).clicked() {
                        self.add_folder();
                    }
                });
            }
        });
    }
}
