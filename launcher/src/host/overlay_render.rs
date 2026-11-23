//! Drawing the overlay and its corner notifications: the egui contexts, the software
//! canvas and its dirty rectangles (render.rs, raster.rs), and where a frame goes, into
//! the game's own frames (the copy's frame block) or a layered window over the game.

use std::time::{Duration, Instant};

use eframe::egui;

use super::desktop::Rect;
use super::instance::InstanceConn;
use super::ui::Overlay;
use super::{raster, render, window};

/// The shortest time between two overlay frames (a mouse reports hundreds of moves a
/// second; each frame takes them all).
pub const FRAME_GAP: Duration = Duration::from_millis(7);

/// The overlay's scale on a game `height` pixels tall, with the player's own on top.
pub fn scale_for(height: f32, overlay_scale: f32) -> f32 {
    (height / 900.0).clamp(0.8, 3.0) * overlay_scale
}

pub struct OverlayRenderer {
    pub ctx: egui::Context,
    pub toast_ctx: egui::Context,
    /// The overlay's canvas and how it's drawn (render.rs).
    pub render: render::Renderer,
    toast_textures: raster::Textures,
    toast_canvas: raster::Canvas,
    window: Option<window::Layered>,
    toast_window: Option<window::Layered>,
    /// The fonts are loaded into both contexts (the first time anything is drawn: a
    /// game whose overlay never shows never reads them).
    fonts: bool,
    pub ppp: f32,
    /// When the last frame was drawn (frames are at most this often: `FRAME_GAP`).
    pub last_frame: Instant,
    /// Something changed: the next tick draws a frame.
    pub dirty: bool,
}

impl OverlayRenderer {
    pub fn new() -> OverlayRenderer {
        OverlayRenderer {
            ctx: egui::Context::default(),
            toast_ctx: egui::Context::default(),
            render: render::Renderer::default(),
            toast_textures: raster::Textures::default(),
            toast_canvas: raster::Canvas::new(0, 0),
            window: None,
            toast_window: None,
            fonts: false,
            ppp: 1.0,
            last_frame: Instant::now(),
            dirty: true,
        }
    }

    /// Loads the interface fonts the first time the overlay or a notification draws.
    pub fn fonts(&mut self) {
        if !self.fonts {
            self.fonts = true;
            crate::ui::theme::install_fonts(&self.ctx);
            crate::ui::theme::install_fonts(&self.toast_ctx);
        }
    }

    /// The canvas's size in points.
    pub fn screen_points(&self) -> egui::Vec2 {
        egui::vec2(self.render.canvas.w as f32 / self.ppp, self.render.canvas.h as f32 / self.ppp)
    }

    /// The canvas fits a game `w` by `h` at scale `ppp`: resized (and redrawn whole) when
    /// it didn't.
    pub fn fit(&mut self, w: usize, h: usize, ppp: f32) {
        if (ppp - self.ppp).abs() > 0.001 || self.render.canvas.w != w || self.render.canvas.h != h {
            self.ppp = ppp;
            self.render.resize(w, h);
            self.render.invalidate();
            self.dirty = true;
        }
    }

    /// The overlay closed: its window goes.
    pub fn hide_window(&mut self) {
        if let Some(w) = self.window.as_mut() {
            w.hide();
        }
    }

    pub fn hide_toast_window(&mut self) {
        if let Some(w) = self.toast_window.as_mut() {
            w.hide();
        }
    }

    /// Shows the canvas: inside the game's frames when `ingame` (and the copy has a frame
    /// block), else in a window over it at (`x`, `y`). `dirty` is what changed.
    pub fn present(&mut self, inst: &mut InstanceConn, overlay: &Overlay, ingame: bool, x: i32, y: i32, dirty: &[[i32; 4]]) {
        if ingame {
            inst.frame_for(self.render.canvas.w * self.render.canvas.h);
            if let Some(f) = inst.frame.as_mut() {
                let rects = overlay.panel_px(self.ppp).into_iter().collect::<Vec<_>>();
                f.write(&self.render.canvas, 0, 0, true, &rects, Some(dirty));
                self.hide_window();
                inst.unseen.handed(inst.ingame, Instant::now());
                return;
            }
        }
        if self.window.is_none() {
            self.window = window::Layered::new();
        }
        if let Some(w) = self.window.as_mut() {
            w.present(&self.render.canvas, x, y, dirty);
        }
    }

    /// Draws the corner notifications over the copy at `r` (its pane, or its window;
    /// `time`: seconds since the session started): inside its frames when `ingame` (the
    /// whole canvas, mostly clear), else in a small window of their own.
    pub fn toasts(&mut self, inst: &mut InstanceConn, overlay: &mut Overlay, ingame: bool, r: Rect, overlay_scale: f32, time: f64) {
        self.fonts();
        let (w, h) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
        let ppp = scale_for(h, overlay_scale);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w / ppp, h / ppp))),
            time: Some(time),
            ..Default::default()
        };
        self.toast_ctx.set_pixels_per_point(ppp);
        let mut area = None;
        let out = self.toast_ctx.run_ui(input, |ui| area = overlay.toasts_frame(ui));
        self.toast_textures.apply(out.textures_delta);
        let Some(area) = area else {
            self.hide_toast_window();
            return;
        };
        let prims = self.toast_ctx.tessellate(out.shapes, out.pixels_per_point);
        let x0 = (area.min.x * ppp).floor().max(0.0);
        let y0 = (area.min.y * ppp).floor().max(0.0);
        let cw = ((area.max.x * ppp).ceil() - x0).max(1.0) as usize;
        let ch = ((area.max.y * ppp).ceil() - y0).max(1.0) as usize;
        if ingame {
            inst.frame_for(w as usize * h as usize);
        }
        if ingame && inst.frame.is_some() {
            // Inside the game: the full-size canvas, mostly clear.
            self.render.resize(w as usize, h as usize);
            self.render.invalidate();
            let canvas = &mut self.render.canvas;
            canvas.clear(egui::Color32::TRANSPARENT);
            self.toast_textures.draw(canvas, &prims, out.pixels_per_point, (0.0, 0.0));
            let rect = [x0 as u32, y0 as u32, cw as u32, ch as u32];
            if let Some(f) = inst.frame.as_mut() {
                f.write(&self.render.canvas, 0, 0, true, &[rect], None);
            }
            return;
        }
        self.toast_canvas.resize(cw, ch);
        self.toast_canvas.clear(egui::Color32::TRANSPARENT);
        self.toast_textures.draw(&mut self.toast_canvas, &prims, out.pixels_per_point, (x0, y0));
        if self.toast_window.is_none() {
            self.toast_window = window::Layered::new();
        }
        if let Some(win) = self.toast_window.as_mut() {
            let all = [0, 0, self.toast_canvas.w as i32, self.toast_canvas.h as i32];
            win.present(&self.toast_canvas, r.left + x0 as i32, r.top + y0 as i32, &[all]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scale_follows_the_games_height() {
        assert_eq!(scale_for(900.0, 1.0), 1.0);
        assert_eq!(scale_for(2160.0, 1.0), 2.4, "4K");
        assert_eq!(scale_for(400.0, 1.0), 0.8, "never smaller than 0.8");
        assert_eq!(scale_for(9000.0, 1.0), 3.0, "nor bigger than 3");
        assert_eq!(scale_for(1800.0, 1.25), 2.5, "the player's own on top");
    }

    /// A new size or scale redraws the canvas whole; the same one doesn't.
    #[test]
    fn the_canvas_fits_the_game() {
        let mut r = OverlayRenderer::new();
        r.dirty = false;
        r.fit(1920, 1080, 1.2);
        assert!(r.dirty);
        assert_eq!((r.render.canvas.w, r.render.canvas.h, r.ppp), (1920, 1080, 1.2));
        let points = r.screen_points();
        assert!((points.x - 1600.0).abs() < 0.01 && (points.y - 900.0).abs() < 0.01, "{points:?}");
        r.dirty = false;
        r.fit(1920, 1080, 1.2);
        assert!(!r.dirty, "nothing changed");
    }
}
