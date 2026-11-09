//! A small software renderer for egui's triangles, so the overlay needs no GPU, no
//! graphics context and no window of its own: the frame is a premultiplied BGRA buffer
//! that goes to a layered window (window.rs) or into the game's own frame (frame.rs).
//!
//! egui hands over triangle meshes in points, with premultiplied sRGB vertex colours,
//! texture coordinates and a clip rectangle per mesh; blending is in gamma space, as
//! egui expects. Solid triangles (every vertex the same colour and texel, which is how
//! egui draws rectangles and backgrounds) skip the texture lookup.

use std::collections::HashMap;

use eframe::egui;
use egui::epaint::{ClippedPrimitive, Primitive, TextureId};

/// Premultiplied BGRA, one u32 per pixel (B in the low byte), top row first.
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Canvas {
        Canvas { w, h, px: vec![0; w * h] }
    }

    pub fn resize(&mut self, w: usize, h: usize) {
        if self.w != w || self.h != h {
            self.w = w;
            self.h = h;
            self.px = vec![0; w * h];
        }
    }

    /// Fills everything with one premultiplied colour.
    pub fn clear(&mut self, c: egui::Color32) {
        let v = pack(c.r(), c.g(), c.b(), c.a());
        self.px.fill(v);
    }

    /// Copies `r` ([x0, y0, x1, y1], pixels) from a canvas of the same size.
    pub fn copy_rect(&mut self, from: &Canvas, r: [i32; 4]) {
        if from.w != self.w || from.h != self.h {
            return;
        }
        let [x0, y0, x1, y1] = clamp_rect(r, self.w, self.h);
        for y in y0..y1 {
            let row = y * self.w;
            self.px[row + x0..row + x1].copy_from_slice(&from.px[row + x0..row + x1]);
        }
    }

    /// The pixels of `r` (clipped to the canvas), to put back with `put`.
    pub fn save(&self, r: [i32; 4]) -> ([usize; 4], Vec<u32>) {
        let c = clamp_rect(r, self.w, self.h);
        let mut out = Vec::with_capacity((c[2] - c[0]) * (c[3] - c[1]));
        for y in c[1]..c[3] {
            out.extend_from_slice(&self.px[y * self.w + c[0]..y * self.w + c[2]]);
        }
        (c, out)
    }

    pub fn put(&mut self, saved: &([usize; 4], Vec<u32>)) {
        let ([x0, y0, x1, y1], px) = saved;
        if *x1 > self.w || *y1 > self.h {
            return;
        }
        let n = x1 - x0;
        for (i, y) in (*y0..*y1).enumerate() {
            self.px[y * self.w + x0..y * self.w + x1].copy_from_slice(&px[i * n..i * n + n]);
        }
    }

    /// Draws `sprite` (premultiplied) over this canvas with its top-left at (x, y).
    pub fn blend_in(&mut self, sprite: &Canvas, x: i32, y: i32) {
        for sy in 0..sprite.h {
            let ty = y + sy as i32;
            if ty < 0 || ty >= self.h as i32 {
                continue;
            }
            for sx in 0..sprite.w {
                let tx = x + sx as i32;
                if tx < 0 || tx >= self.w as i32 {
                    continue;
                }
                let s = sprite.px[sy * sprite.w + sx];
                let a = (s >> 24) as f32;
                if a > 0.0 {
                    let dst = &mut self.px[ty as usize * self.w + tx as usize];
                    blend(dst, ((s >> 16) & 0xff) as f32, ((s >> 8) & 0xff) as f32, (s & 0xff) as f32, a);
                }
            }
        }
    }
}

/// `r` clipped to a w x h canvas, as usize corners.
pub fn clamp_rect(r: [i32; 4], w: usize, h: usize) -> [usize; 4] {
    let cx = |v: i32, m: usize| (v.max(0) as usize).min(m);
    let (x0, y0) = (cx(r[0], w), cx(r[1], h));
    [x0, y0, cx(r[2], w).max(x0), cx(r[3], h).max(y0)]
}

/// The smallest rectangle holding both (an empty one counts as nothing).
pub fn union(a: [i32; 4], b: [i32; 4]) -> [i32; 4] {
    let empty = |r: [i32; 4]| r[0] >= r[2] || r[1] >= r[3];
    match (empty(a), empty(b)) {
        (true, _) => b,
        (_, true) => a,
        _ => [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])],
    }
}

/// A shape to draw anywhere within `clip`.
pub fn clipped(clip: egui::Rect, shape: egui::Shape) -> egui::epaint::ClippedShape {
    egui::epaint::ClippedShape { clip_rect: clip, shape, transform_after_tessellation: egui::emath::TSTransform::IDENTITY }
}

/// Whether two rectangles overlap.
pub fn overlaps(a: [i32; 4], b: [i32; 4]) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

/// The pixels a shape can touch ([x0, y0, x1, y1]; empty for nothing), with a pixel
/// to spare for egui's anti-aliasing.
pub fn shape_px(s: &egui::epaint::ClippedShape, ppp: f32) -> [i32; 4] {
    if s.transform_after_tessellation != egui::emath::TSTransform::IDENTITY {
        // Moved after tessellation (an animating popup): anywhere.
        return [i32::MIN / 2, i32::MIN / 2, i32::MAX / 2, i32::MAX / 2];
    }
    let r = s.shape.visual_bounding_rect().intersect(s.clip_rect);
    if !r.is_positive() {
        return [0; 4];
    }
    [(r.min.x * ppp).floor() as i32 - 2, (r.min.y * ppp).floor() as i32 - 2, (r.max.x * ppp).ceil() as i32 + 2, (r.max.y * ppp).ceil() as i32 + 2]
}

/// The shapes, with nested `Shape::Vec`s spread out (so a change inside one doesn't
/// count as a change to all of it) and the ones that draw nothing left out.
pub fn flatten(shapes: Vec<egui::epaint::ClippedShape>) -> Vec<egui::epaint::ClippedShape> {
    use egui::emath::TSTransform;
    fn put(out: &mut Vec<egui::epaint::ClippedShape>, clip: egui::Rect, t: TSTransform, s: egui::Shape) {
        match s {
            egui::Shape::Vec(v) => v.into_iter().for_each(|s| put(out, clip, t, s)),
            egui::Shape::Noop => {}
            shape => out.push(egui::epaint::ClippedShape { clip_rect: clip, shape, transform_after_tessellation: t }),
        }
    }
    let mut out = Vec::with_capacity(shapes.len());
    for s in shapes {
        put(&mut out, s.clip_rect, s.transform_after_tessellation, s.shape);
    }
    out
}

/// Where two frames' shapes differ, as a few pixel rectangles: the shapes before the
/// first difference and after the last are the same, and in between, a shape that's in
/// both frames (in the same order relative to the rest) is left out too. Pixels outside
/// the rectangles come out the same from either frame.
pub fn damage(old: &[egui::epaint::ClippedShape], new: &[egui::epaint::ClippedShape], ppp: f32) -> Vec<[i32; 4]> {
    let pre = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let max_suf = old.len().min(new.len()) - pre;
    let suf = old.iter().rev().zip(new.iter().rev()).take(max_suf).take_while(|(a, b)| a == b).count();
    let (om, nm) = (&old[pre..old.len() - suf], &new[pre..new.len() - suf]);
    // In between: walk both in order, pairing up equal shapes (a greedy match, which is
    // all a moved highlight needs) and marking the rest.
    let mut rects: Vec<[i32; 4]> = Vec::new();
    let mut add = |r: [i32; 4]| {
        if r[0] >= r[2] || r[1] >= r[3] {
            return;
        }
        let mut r = r;
        // Merge with every rectangle it touches, until none does.
        loop {
            let Some(i) = rects.iter().position(|q| overlaps(*q, [r[0] - 8, r[1] - 8, r[2] + 8, r[3] + 8])) else { break };
            r = union(r, rects.swap_remove(i));
        }
        rects.push(r);
    };
    let mut j = 0;
    for a in om {
        match nm[j..].iter().position(|b| b == a) {
            Some(k) => {
                for b in &nm[j..j + k] {
                    add(shape_px(b, ppp));
                }
                j += k + 1;
            }
            None => add(shape_px(a, ppp)),
        }
    }
    for b in &nm[j..] {
        add(shape_px(b, ppp));
    }
    // A few rectangles at most: past that, one around them all.
    if rects.len() > 6 {
        let all = rects.iter().fold([0; 4], |acc, r| union(acc, *r));
        rects = vec![all];
    }
    rects
}


fn pack(r: u8, g: u8, b: u8, a: u8) -> u32 {
    u32::from(b) | (u32::from(g) << 8) | (u32::from(r) << 16) | (u32::from(a) << 24)
}

struct Texture {
    w: usize,
    h: usize,
    /// Premultiplied RGBA.
    rgba: Vec<[u8; 4]>,
    linear: bool,
}

#[derive(Default)]
pub struct Textures {
    map: HashMap<TextureId, Texture>,
}

impl Textures {
    /// Applies a frame's texture changes (and marks them handled: egui checks, in debug
    /// builds, that no delta is dropped unapplied).
    pub fn apply(&mut self, mut delta: egui::TexturesDelta) {
        for (id, d) in delta.set.iter().flat_map(|(id, ds)| ds.iter().map(move |d| (id, d))) {
            let egui::ImageData::Color(img) = &d.image;
            let (w, h) = (img.size[0], img.size[1]);
            let pixels: Vec<[u8; 4]> = img.pixels.iter().map(|c| c.to_array()).collect();
            let linear = d.options.magnification == egui::TextureFilter::Linear;
            match d.pos {
                Some([x0, y0]) => {
                    if let Some(t) = self.map.get_mut(id) {
                        for y in 0..h {
                            for x in 0..w {
                                let (tx, ty) = (x0 + x, y0 + y);
                                if tx < t.w && ty < t.h {
                                    t.rgba[ty * t.w + tx] = pixels[y * w + x];
                                }
                            }
                        }
                    }
                }
                None => {
                    self.map.insert(*id, Texture { w, h, rgba: pixels, linear });
                }
            }
        }
        for id in &delta.free {
            self.map.remove(id);
        }
        delta.clear();
    }

    /// Draws the meshes. `ppp` is pixels per point; `origin` is the canvas's top-left
    /// in pixels of egui's screen (a toast drawn into a small canvas).
    pub fn draw(&self, canvas: &mut Canvas, prims: &[ClippedPrimitive], ppp: f32, origin: (f32, f32)) {
        let all = [0, 0, canvas.w as i32, canvas.h as i32];
        self.draw_in(canvas, prims, ppp, origin, all);
    }

    /// Draws the meshes, touching only the pixels inside `limit` ([x0, y0, x1, y1]).
    pub fn draw_in(&self, canvas: &mut Canvas, prims: &[ClippedPrimitive], ppp: f32, origin: (f32, f32), limit: [i32; 4]) {
        for p in prims {
            let Primitive::Mesh(mesh) = &p.primitive else { continue };
            let tex = self.map.get(&mesh.texture_id);
            let clip = p.clip_rect;
            let cx0 = ((clip.min.x * ppp - origin.0).floor().max(0.0) as i32).max(limit[0]);
            let cy0 = ((clip.min.y * ppp - origin.1).floor().max(0.0) as i32).max(limit[1]);
            let cx1 = ((clip.max.x * ppp - origin.0).ceil() as i32).min(canvas.w as i32).min(limit[2]);
            let cy1 = ((clip.max.y * ppp - origin.1).ceil() as i32).min(canvas.h as i32).min(limit[3]);
            if cx0 >= cx1 || cy0 >= cy1 {
                continue;
            }
            for tri in mesh.indices.chunks_exact(3) {
                let v = [
                    &mesh.vertices[tri[0] as usize],
                    &mesh.vertices[tri[1] as usize],
                    &mesh.vertices[tri[2] as usize],
                ];
                triangle(canvas, tex, v, ppp, origin, (cx0, cy0, cx1, cy1));
            }
        }
    }
}

fn sample(t: &Texture, u: f32, v: f32) -> [f32; 4] {
    if !t.linear {
        let x = ((u * t.w as f32) as isize).clamp(0, t.w as isize - 1) as usize;
        let y = ((v * t.h as f32) as isize).clamp(0, t.h as isize - 1) as usize;
        let p = t.rgba[y * t.w + x];
        return [p[0] as f32, p[1] as f32, p[2] as f32, p[3] as f32];
    }
    let fx = u * t.w as f32 - 0.5;
    let fy = v * t.h as f32 - 0.5;
    let x0 = fx.floor();
    let y0 = fy.floor();
    let ax = fx - x0;
    let ay = fy - y0;
    let max_x = t.w as isize - 1;
    let max_y = t.h as isize - 1;
    let xi = (x0 as isize).clamp(0, max_x) as usize;
    let yi = (y0 as isize).clamp(0, max_y) as usize;
    let xj = (x0 as isize + 1).clamp(0, max_x) as usize;
    let yj = (y0 as isize + 1).clamp(0, max_y) as usize;
    let p00 = t.rgba[yi * t.w + xi];
    let p10 = t.rgba[yi * t.w + xj];
    let p01 = t.rgba[yj * t.w + xi];
    let p11 = t.rgba[yj * t.w + xj];
    let mut out = [0f32; 4];
    for c in 0..4 {
        let top = p00[c] as f32 * (1.0 - ax) + p10[c] as f32 * ax;
        let bot = p01[c] as f32 * (1.0 - ax) + p11[c] as f32 * ax;
        out[c] = top * (1.0 - ay) + bot * ay;
    }
    out
}

#[inline]
fn blend(dst: &mut u32, r: f32, g: f32, b: f32, a: f32) {
    if a <= 0.5 {
        return;
    }
    if a >= 254.5 {
        *dst = pack(r as u8, g as u8, b as u8, 255);
        return;
    }
    let k = 1.0 - a / 255.0;
    let d = *dst;
    let db = (d & 0xff) as f32;
    let dg = ((d >> 8) & 0xff) as f32;
    let dr = ((d >> 16) & 0xff) as f32;
    let da = (d >> 24) as f32;
    let c = |s: f32, d: f32| (s + d * k).min(255.0) as u8;
    *dst = pack(c(r, dr), c(g, dg), c(b, db), c(a, da));
}

/// Sub-pixel steps per pixel: vertices snap to 1/256 of a pixel, so the edge functions
/// are exact integers and two triangles sharing an edge agree on every pixel along it.
const SUB: i64 = 256;

fn triangle(
    canvas: &mut Canvas,
    tex: Option<&Texture>,
    v: [&egui::epaint::Vertex; 3],
    ppp: f32,
    origin: (f32, f32),
    clip: (i32, i32, i32, i32),
) {
    let fix = |x: f32| (x * SUB as f32).round() as i64;
    let mut p: [(i64, i64); 3] = [0, 1, 2].map(|i| (fix(v[i].pos.x * ppp - origin.0), fix(v[i].pos.y * ppp - origin.1)));
    let mut v = v;
    let mut area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[1].1 - p[0].1) * (p[2].0 - p[0].0);
    if area == 0 {
        return;
    }
    if area < 0 {
        p.swap(1, 2);
        v.swap(1, 2);
        area = -area;
    }
    let min_x = ((p[0].0.min(p[1].0).min(p[2].0)).div_euclid(SUB) as i32).max(clip.0);
    let min_y = ((p[0].1.min(p[1].1).min(p[2].1)).div_euclid(SUB) as i32).max(clip.1);
    let max_x = (((p[0].0.max(p[1].0).max(p[2].0)) + SUB - 1).div_euclid(SUB) as i32).min(clip.2);
    let max_y = (((p[0].1.max(p[1].1).max(p[2].1)) + SUB - 1).div_euclid(SUB) as i32).min(clip.3);
    if min_x >= max_x || min_y >= max_y {
        return;
    }
    // Edge functions, positive inside: e[0] is opposite vertex 0 (its weight), etc. Each
    // is E(x, y) = a * x + b * y + c at sub-pixel coordinates. A pixel whose centre lies
    // exactly on an edge belongs to the triangle only for a left or top edge (the
    // top-left rule), so no pixel is drawn twice where triangles meet.
    let edge = |a: (i64, i64), b: (i64, i64)| {
        let (ea, eb) = (a.1 - b.1, b.0 - a.0);
        let ec = -(ea * a.0 + eb * a.1);
        let top_left = ea > 0 || (ea == 0 && eb > 0);
        (ea, eb, ec, if top_left { 0 } else { 1 })
    };
    let e = [edge(p[1], p[2]), edge(p[2], p[0]), edge(p[0], p[1])];
    let inv = 1.0 / area as f64;
    let col = v.map(|x| {
        let c = x.color;
        [c.r() as f32, c.g() as f32, c.b() as f32, c.a() as f32]
    });
    let solid = v[0].color == v[1].color && v[1].color == v[2].color && v[0].uv == v[1].uv && v[1].uv == v[2].uv;
    let solid_color = if solid {
        let t = tex.map(|t| sample(t, v[0].uv.x, v[0].uv.y)).unwrap_or([255.0; 4]);
        Some([0, 1, 2, 3].map(|c| col[0][c] * t[c] / 255.0))
    } else {
        None
    };
    let opaque = solid_color.filter(|c| c[3] >= 254.5).map(|c| pack(c[0] as u8, c[1] as u8, c[2] as u8, 255));
    // Per-pixel steps of the weights along a row, for interpolating colour and texture.
    let dw = [0, 1, 2].map(|i| (e[i].0 * SUB) as f64 * inv);
    let w = canvas.w;
    let half = SUB / 2;
    for y in min_y..max_y {
        let py = y as i64 * SUB + half;
        let (mut lo, mut hi) = (min_x as i64, max_x as i64 - 1);
        let mut row = [0i64; 3];
        for (k, &(ea, eb, ec, bias)) in e.iter().enumerate() {
            // E at the centre of pixel x: ea * (x * SUB + half) + eb * py + ec = r + s * x.
            let r = ea * half + eb * py + ec;
            let s = ea * SUB;
            row[k] = r;
            // Inside: r + s * x >= bias.
            if s > 0 {
                lo = lo.max(ceil_div(bias - r, s));
            } else if s < 0 {
                hi = hi.min((r - bias).div_euclid(-s));
            } else if r < bias {
                lo = 1;
                hi = 0;
            }
        }
        if lo > hi {
            continue;
        }
        let (x0, x1) = (lo as usize, hi as usize + 1);
        let base = y as usize * w;
        let span = &mut canvas.px[base + x0..base + x1];
        if let Some(v) = opaque {
            span.fill(v);
            continue;
        }
        if let Some(c) = solid_color {
            for dst in span {
                blend(dst, c[0], c[1], c[2], c[3]);
            }
            continue;
        }
        // Weights at the span's first pixel, then stepped along it.
        let mut wt = [0, 1, 2].map(|k| (row[k] + e[k].0 * SUB * lo) as f64 * inv);
        for dst in span.iter_mut() {
            let (w0, w1, w2) = (wt[0].max(0.0) as f32, wt[1].max(0.0) as f32, wt[2].max(0.0) as f32);
            let mut c = [0f32; 4];
            for i in 0..4 {
                c[i] = col[0][i] * w0 + col[1][i] * w1 + col[2][i] * w2;
            }
            if let Some(t) = tex {
                let u = v[0].uv.x * w0 + v[1].uv.x * w1 + v[2].uv.x * w2;
                let vv = v[0].uv.y * w0 + v[1].uv.y * w1 + v[2].uv.y * w2;
                let s = sample(t, u, vv);
                for i in 0..4 {
                    c[i] = c[i] * s[i] / 255.0;
                }
            }
            blend(dst, c[0], c[1], c[2], c[3]);
            for k in 0..3 {
                wt[k] += dw[k];
            }
        }
    }
}

/// `n / d` rounded up, for d > 0.
fn ceil_div(n: i64, d: i64) -> i64 {
    -((-n).div_euclid(d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_a_frame_of_egui() {
        let ctx = egui::Context::default();
        let mut textures = Textures::default();
        let mut canvas = Canvas::new(200, 100);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 100.0))),
            ..Default::default()
        };
        let out = ctx.run_ui(input, |ui| {
            ui.painter().rect_filled(egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(50.0, 20.0)), 0.0, egui::Color32::RED);
            ui.label("Hello");
        });
        textures.apply(out.textures_delta);
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        canvas.clear(egui::Color32::TRANSPARENT);
        textures.draw(&mut canvas, &prims, out.pixels_per_point, (0.0, 0.0));
        // Inside the red rectangle: opaque red.
        assert_eq!(canvas.px[20 * 200 + 30], 0xffff_0000);
        // Outside everything: still clear.
        assert_eq!(canvas.px[95 * 200 + 195], 0);
        // The label left some opaque text pixels.
        assert!(canvas.px.iter().filter(|&&p| p >> 24 > 200 && p != 0xffff_0000).count() > 10);
    }

    #[test]
    fn blending_is_premultiplied_over() {
        let mut d = pack(0, 0, 255, 255);
        blend(&mut d, 128.0, 0.0, 0.0, 128.0);
        let (r, b, a) = ((d >> 16) & 0xff, d & 0xff, d >> 24);
        assert_eq!(a, 255);
        assert!((r as i32 - 128).abs() <= 1, "{r}");
        assert!((b as i32 - 127).abs() <= 1, "{b}");
    }
}
