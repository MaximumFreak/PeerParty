//! Turning the overlay's frames into pixels quickly: a frame is drawn only where it
//! differs from the last one, over a kept copy of what never changes (the dimmed game
//! and the panel's background, whose lights are most of the drawing), and the pointer
//! moves without drawing anything else.
//!
//! A typical frame (the selection moving, a value changing) redraws two rows; the
//! pointer moving over nothing that reacts to it redraws only the pointer.

use eframe::egui;
use egui::epaint::ClippedShape;

use super::raster::{self, Canvas, Textures};

/// Pixels around the pointer's tip in its sprite (room for its outline).
const CURSOR_PAD: i32 = 3;

pub struct Renderer {
    pub canvas: Canvas,
    pub textures: Textures,
    /// The dimmed game and the panel's background, at the canvas's size and scale.
    base: Canvas,
    base_key: Option<(usize, usize, u32)>,
    /// The last frame's shapes; None when the canvas needs drawing whole.
    shapes: Option<Vec<ClippedShape>>,
    /// The pointer, drawn once per scale, and what's under it on the canvas.
    sprite: Option<(u32, Canvas)>,
    under: Option<([usize; 4], Vec<u32>)>,
}

impl Default for Renderer {
    fn default() -> Renderer {
        Renderer {
            canvas: Canvas::new(0, 0),
            textures: Textures::default(),
            base: Canvas::new(0, 0),
            base_key: None,
            shapes: None,
            sprite: None,
            under: None,
        }
    }
}

impl Renderer {
    /// The canvas holds something else now (or changed size): the next frame draws all
    /// of it.
    pub fn invalidate(&mut self) {
        self.shapes = None;
        self.under = None;
    }

    pub fn resize(&mut self, w: usize, h: usize) {
        if self.canvas.w != w || self.canvas.h != h {
            self.canvas.resize(w, h);
            self.invalidate();
        }
    }

    /// Whether the pointer is on the canvas.
    pub fn pointer_shown(&self) -> bool {
        self.under.is_some()
    }

    /// Takes the pointer off the canvas; adds where it was to `rects`.
    pub fn pointer_off(&mut self, rects: &mut Vec<[i32; 4]>) {
        if let Some(under) = self.under.take() {
            self.canvas.put(&under);
            let [x0, y0, x1, y1] = under.0;
            rects.push([x0 as i32, y0 as i32, x1 as i32, y1 as i32]);
        }
    }

    /// Draws a frame's output: only where it differs from the last frame. `background`
    /// gives the shapes that never change for a screen (in points). Adds what changed to
    /// `rects`. Take the pointer off first.
    pub fn draw(&mut self, ctx: &egui::Context, out: egui::FullOutput, background: impl FnOnce(egui::Rect) -> Vec<egui::Shape>, rects: &mut Vec<[i32; 4]>) {
        let (w, h) = (self.canvas.w, self.canvas.h);
        // A new font atlas moves every glyph: everything is drawn again.
        let atlas = out.textures_delta.set.values().flatten().any(|d| d.pos.is_none());
        self.textures.apply(out.textures_delta);
        let ppp = out.pixels_per_point;
        let shapes = raster::flatten(out.shapes);
        let key = (w, h, ppp.to_bits());
        if self.base_key != Some(key) {
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w as f32 / ppp, h as f32 / ppp));
            self.base.resize(w, h);
            self.base.clear(egui::Color32::from_black_alpha(150));
            let bg = background(screen).into_iter().map(|shape| raster::clipped(screen, shape)).collect();
            let prims = ctx.tessellate(bg, ppp);
            self.textures.draw(&mut self.base, &prims, ppp, (0.0, 0.0));
            self.base_key = Some(key);
            self.shapes = None;
        }
        let damage = match &self.shapes {
            Some(old) if !atlas => raster::damage(old, &shapes, ppp),
            _ => vec![[0, 0, w as i32, h as i32]],
        };
        if !damage.is_empty() {
            // Only the shapes that touch a changed part are drawn, and only there.
            let touching: Vec<ClippedShape> = shapes
                .iter()
                .filter(|s| {
                    let b = raster::shape_px(s, ppp);
                    damage.iter().any(|d| raster::overlaps(*d, b))
                })
                .cloned()
                .collect();
            let prims = ctx.tessellate(touching, ppp);
            for d in &damage {
                self.canvas.copy_rect(&self.base, *d);
                self.textures.draw_in(&mut self.canvas, &prims, ppp, (0.0, 0.0), *d);
            }
            rects.extend(damage);
        }
        self.shapes = Some(shapes);
    }

    /// Draws the pointer with its tip at `at` (points); adds where to `rects`.
    pub fn pointer_on(&mut self, ctx: &egui::Context, at: egui::Pos2, ppp: f32, rects: &mut Vec<[i32; 4]>) {
        let bits = ppp.to_bits();
        if self.sprite.as_ref().is_none_or(|(k, _)| *k != bits) {
            self.sprite = Some((bits, cursor_sprite(ctx, &mut self.textures, ppp)));
        }
        let Some((_, sprite)) = &self.sprite else { return };
        let x = (at.x * ppp).round() as i32 - CURSOR_PAD;
        let y = (at.y * ppp).round() as i32 - CURSOR_PAD;
        let r = [x, y, x + sprite.w as i32, y + sprite.h as i32];
        let under = self.canvas.save(r);
        self.canvas.blend_in(sprite, x, y);
        self.under = Some(under);
        rects.push(r);
    }
}

/// The overlay's own pointer (the real cursor is held still while it shows), drawn once
/// at a scale: its tip is at (CURSOR_PAD, CURSOR_PAD).
fn cursor_sprite(ctx: &egui::Context, textures: &mut Textures, ppp: f32) -> Canvas {
    let at = egui::Pos2::new(CURSOR_PAD as f32 / ppp, CURSOR_PAD as f32 / ppp);
    let pts = vec![at, at + egui::vec2(0.0, 16.0), at + egui::vec2(4.5, 12.0), at + egui::vec2(11.0, 12.0)];
    let shape = egui::Shape::convex_polygon(pts, egui::Color32::WHITE, egui::Stroke::new(1.2, egui::Color32::BLACK));
    let size = ((14.0 * ppp).ceil() as usize + 2 * CURSOR_PAD as usize, (19.0 * ppp).ceil() as usize + 2 * CURSOR_PAD as usize);
    let clip = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(size.0 as f32 / ppp, size.1 as f32 / ppp));
    let prims = ctx.tessellate(vec![raster::clipped(clip, shape)], ppp);
    let mut sprite = Canvas::new(size.0, size.1);
    textures.draw(&mut sprite, &prims, ppp, (0.0, 0.0));
    sprite
}

#[cfg(test)]
mod tests {
    use super::super::ui::{self, GameState, Nav, Overlay, Page};
    use super::*;
    use crate::ui::store::Prefs;

    fn state() -> GameState {
        GameState::from_json(&serde_json::json!({
            "t": "state",
            "me": {"id": "aa", "name": "Kim"},
            "peers": [
                {"id": "bb", "name": "Sam", "with_me": true, "ip": "192.168.1.20", "via": "lan"},
                {"id": "cc", "name": "Alex", "status": "In the menus", "ip": "192.168.1.31", "via": "lan"},
                {"id": "dd", "name": "Pat", "via": "room"}
            ],
            "pending": [{"id": 3, "from": "cc", "name": "Alex", "kind": "invited you"}],
            "can_invite": true,
            "lan_unreachable": 0,
            "sdks": ["Eos"],
            "internet": [{"mode": "room", "online": true, "nat": "hole-punch friendly", "peers": [{"name": "Pat", "rtt_ms": 23}]}]
        }))
    }

    /// One step of input for the overlay.
    enum Step {
        Nav(Nav),
        Move(egui::Pos2),
    }

    /// A run of frames drawn the fast way (only what changed, the pointer on its own)
    /// gives the same pixels as drawing each frame whole, and the fast frames are fast.
    #[test]
    fn partial_frames_match_whole_ones() {
        crate::ui::theme::set_effects(true);
        let (w, h, ppp) = (2560usize, 1440usize, 1.6f32);
        let ctx = egui::Context::default();
        crate::ui::theme::install_fonts(&ctx);
        ctx.set_pixels_per_point(ppp);
        let whole_ctx = egui::Context::default();
        crate::ui::theme::install_fonts(&whole_ctx);
        whole_ctx.set_pixels_per_point(ppp);
        let mut fast = Renderer::default();
        fast.resize(w, h);
        let mut whole = Renderer::default();
        whole.resize(w, h);
        let mut o = Overlay::new("Sample Game".into());
        o.state = state();
        o.paints_background = false;
        o.open(Page::Home, false);
        let mut o2 = Overlay::new("Sample Game".into());
        o2.state = state();
        o2.paints_background = false;
        o2.open(Page::Home, false);
        let (mut prefs, mut prefs2) = (Prefs::default(), Prefs::default());
        let screen = egui::vec2(w as f32 / ppp, h as f32 / ppp);
        let down = Nav { down: true, ..Nav::default() };
        let mut steps = vec![Step::Nav(Nav::default()), Step::Nav(Nav::default())];
        for _ in 0..3 {
            steps.push(Step::Nav(down));
        }
        steps.push(Step::Move(egui::pos2(screen.x * 0.5, screen.y * 0.45)));
        steps.push(Step::Move(egui::pos2(screen.x * 0.5 + 3.0, screen.y * 0.45 + 2.0)));
        steps.push(Step::Move(egui::pos2(screen.x * 0.5, screen.y * 0.62)));
        steps.push(Step::Nav(Nav { next_tab: true, ..Nav::default() }));
        steps.push(Step::Nav(down));
        steps.push(Step::Nav(Nav { next_tab: true, ..Nav::default() }));
        steps.push(Step::Nav(Nav { next_tab: true, ..Nav::default() }));
        for _ in 0..4 {
            steps.push(Step::Nav(down));
        }
        // The controller-button pictures, round and back.
        for _ in 0..4 {
            steps.push(Step::Nav(Nav { right: true, ..Nav::default() }));
        }
        steps.push(Step::Nav(Nav { up: true, ..Nav::default() }));
        steps.push(Step::Nav(Nav { select: true, ..Nav::default() }));
        let mut cursor: Option<egui::Pos2> = None;
        let mut slowest = std::time::Duration::ZERO;
        let mut total = std::time::Duration::ZERO;
        for (i, step) in steps.iter().enumerate() {
            let (nav, events) = match step {
                Step::Nav(n) => (*n, vec![]),
                Step::Move(p) => {
                    cursor = Some(*p);
                    (Nav::default(), vec![egui::Event::PointerMoved(*p)])
                }
            };
            let input = |events: Vec<egui::Event>| egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, screen)),
                time: Some(i as f64 * 0.1),
                events,
                ..Default::default()
            };
            let t = std::time::Instant::now();
            let mut rects = Vec::new();
            fast.pointer_off(&mut rects);
            let mut cmds = Vec::new();
            let out = ctx.run_ui(input(events.clone()), |ui| o.frame(ui, nav, &mut prefs, &mut cmds));
            fast.draw(&ctx, out, |s| ui::background(ui::panel_rect(s)), &mut rects);
            if let Some(c) = cursor {
                fast.pointer_on(&ctx, c, ppp, &mut rects);
            }
            let took = t.elapsed();
            if i >= 2 {
                slowest = slowest.max(took);
                total += took;
            }
            // The same frame, drawn whole.
            whole.invalidate();
            let mut cmds = Vec::new();
            let out = whole_ctx.run_ui(input(events), |ui| o2.frame(ui, nav, &mut prefs2, &mut cmds));
            let mut r2 = Vec::new();
            whole.draw(&whole_ctx, out, |s| ui::background(ui::panel_rect(s)), &mut r2);
            if let Some(c) = cursor {
                whole.pointer_on(&whole_ctx, c, ppp, &mut r2);
            }
            assert_eq!(o.page, o2.page, "step {i}");
            let differ = fast.canvas.px.iter().zip(&whole.canvas.px).filter(|(a, b)| a != b).count();
            if differ > 0 {
                if let Some(d) = std::env::var_os("PEERPARTY_PREVIEW_DIR") {
                    for (name, c) in [("fast", &fast.canvas), ("whole", &whole.canvas)] {
                        let rgba: Vec<u8> = c.px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, 255]).collect();
                        image::RgbaImage::from_raw(w as u32, h as u32, rgba).unwrap().save(std::path::Path::new(&d).join(format!("render-{name}.png"))).unwrap();
                    }
                }
            }
            assert_eq!(differ, 0, "step {i}: {differ} pixels differ from drawing the frame whole");
            eprintln!("step {i}: {took:?}, {} rects", rects.len());
        }
        eprintln!("slowest {slowest:?}, mean {:?}", total / (steps.len() as u32 - 2));
    }
}
