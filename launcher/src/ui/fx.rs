//! Light, drawn as plain coloured meshes so it costs the same in the window (GPU) and
//! in the overlay's software canvas: a bloom around a shape (two Gaussians, a tight one
//! and a wide one, like a lens), radial lights, a spotlight that follows the pointer
//! inside a rounded card, the rim of a card catching that light, exact linear gradients
//! on rounded rectangles, and an electric arc for the moment a game starts.
//!
//! Nothing here animates by itself: a caller that wants motion passes the time, and
//! only for as long as the motion lasts. With effects off (`theme::effects`) the lights
//! draw nothing and the gradients stay, so the interface still reads the same.

use eframe::egui::{Color32, Mesh, Painter, Pos2, Rect, Shape, Stroke, Vec2, pos2, vec2};

use super::theme;

/// Points around a rounded rectangle, clockwise from the top edge's left end: `per`
/// steps per corner and `side` extra points along each straight edge (lights need them
/// there; a gradient across an edge with only its two ends would be linear).
pub fn outline(rect: Rect, r: f32, per: usize, side: [usize; 2]) -> Vec<Pos2> {
    let r = r.min(rect.width() / 2.0).min(rect.height() / 2.0).max(0.0);
    let arc = |c: Pos2, start: f32| -> Vec<Pos2> {
        (0..=per)
            .map(|i| {
                let a = (start + 90.0 * i as f32 / per as f32).to_radians();
                c + vec2(a.cos(), a.sin()) * r
            })
            .collect()
    };
    let corners = [
        arc(pos2(rect.right() - r, rect.top() + r), -90.0),
        arc(pos2(rect.right() - r, rect.bottom() - r), 0.0),
        arc(pos2(rect.left() + r, rect.bottom() - r), 90.0),
        arc(pos2(rect.left() + r, rect.top() + r), 180.0),
    ];
    let mut out = Vec::with_capacity(4 * (per + 1) + 2 * (side[0] + side[1]));
    for (i, c) in corners.iter().enumerate() {
        // The straight edge leading into this corner (from the previous corner's end).
        let prev = corners[(i + 3) % 4].last().copied().unwrap_or(rect.center());
        let n = if i % 2 == 0 { side[0] } else { side[1] };
        for k in 1..=n {
            out.push(prev + (c[0] - prev) * (k as f32 / (n + 1) as f32));
        }
        out.extend_from_slice(c);
    }
    out
}

/// Edge subdivisions for lights across `rect`: about one point per `step` points.
fn sides(rect: Rect, step: f32) -> [usize; 2] {
    [((rect.width() / step) as usize).min(40), ((rect.height() / step) as usize).min(40)]
}

/// Joins two outlines of the same length into a closed band.
fn band(mesh: &mut Mesh, inner: &[Pos2], ci: &[Color32], outer: &[Pos2], co: &[Color32]) {
    let base = mesh.vertices.len() as u32;
    let n = inner.len() as u32;
    for (p, c) in inner.iter().zip(ci) {
        mesh.colored_vertex(*p, *c);
    }
    for (p, c) in outer.iter().zip(co) {
        mesh.colored_vertex(*p, *c);
    }
    for i in 0..n {
        let j = (i + 1) % n;
        mesh.add_triangle(base + i, base + j, base + n + i);
        mesh.add_triangle(base + j, base + n + j, base + n + i);
    }
}

fn gauss(d: f32, sigma: f32) -> f32 {
    (-(d * d) / (2.0 * sigma * sigma)).exp()
}

/// A bloom around a rounded rectangle, `reach` points out: a tight Gaussian for the
/// glow hugging the edge and a wide one for the haze, gone by `reach`.
pub fn glow(p: &Painter, rect: Rect, r: f32, color: Color32, reach: f32) {
    p.add(glow_shape(rect, r, color, reach));
}

pub fn glow_shape(rect: Rect, r: f32, color: Color32, reach: f32) -> Shape {
    if !theme::effects() || reach <= 0.5 || color.a() == 0 {
        return Shape::Noop;
    }
    let base = reach / 2.5;
    let falloff = |d: f32| 0.62 * gauss(d, 0.35 * base) + 0.38 * gauss(d, 1.2 * base);
    let rings = 7;
    let mut mesh = Mesh::default();
    let mut prev: Option<(Vec<Pos2>, Vec<Color32>)> = None;
    for i in 0..=rings {
        let d = reach * (i as f32 / rings as f32).powf(1.6);
        let pts = outline(rect.expand(d), r + d, 8, [0, 0]);
        let a = if i == rings { 0.0 } else { falloff(d) };
        let cols = vec![color.gamma_multiply(a); pts.len()];
        if let Some((pp, pc)) = &prev {
            band(&mut mesh, pp, pc, &pts, &cols);
        }
        prev = Some((pts, cols));
    }
    Shape::mesh(mesh)
}

/// A soft round (or oval) light: brightest at `center`, gone at `radius`.
pub fn light(p: &Painter, center: Pos2, radius: Vec2, color: Color32) {
    p.add(light_shape(center, radius, color));
}

pub fn light_shape(center: Pos2, radius: Vec2, color: Color32) -> Shape {
    if !theme::effects() || color.a() == 0 {
        return Shape::Noop;
    }
    let segs = 48;
    let ts = [0.0, 0.18, 0.36, 0.55, 0.75, 1.0];
    let end = (-4.0f32).exp();
    let f = |t: f32| (((-4.0 * t * t).exp() - end) / (1.0 - end)).max(0.0);
    let mut mesh = Mesh::default();
    mesh.colored_vertex(center, color);
    for (ri, t) in ts.iter().enumerate().skip(1) {
        let c = color.gamma_multiply(f(*t));
        for s in 0..segs {
            let a = std::f32::consts::TAU * s as f32 / segs as f32;
            mesh.colored_vertex(center + vec2(a.cos() * radius.x, a.sin() * radius.y) * *t, c);
        }
        let ring = 1 + (ri as u32 - 1) * segs;
        for s in 0..segs {
            let (a, b) = (ring + s, ring + (s + 1) % segs);
            if ri == 1 {
                mesh.add_triangle(0, a, b);
            } else {
                let (pa, pb) = (a - segs, b - segs);
                mesh.add_triangle(pa, pb, a);
                mesh.add_triangle(pb, b, a);
            }
        }
    }
    Shape::mesh(mesh)
}

/// Light from `at` inside a rounded card, kept to the card's shape: brightest at `at`,
/// gone `reach` points away.
pub fn spotlight(p: &Painter, rect: Rect, r: f32, at: Pos2, reach: f32, color: Color32) {
    p.add(spotlight_shape(rect, r, at, reach, color));
}

pub fn spotlight_shape(rect: Rect, r: f32, at: Pos2, reach: f32, color: Color32) -> Shape {
    if !theme::effects() || color.a() == 0 {
        return Shape::Noop;
    }
    let inside = rect.shrink(1.0);
    let at = pos2(at.x.clamp(inside.left(), inside.right()), at.y.clamp(inside.top(), inside.bottom()));
    let edge = outline(rect, r, 6, sides(rect, 22.0));
    let f = |q: Pos2| {
        let t = ((q - at).length() / reach).min(1.0);
        (1.0 - t * t).powi(2)
    };
    let mut mesh = Mesh::default();
    mesh.colored_vertex(at, color);
    let fracs = [0.1, 0.25, 0.45, 0.7, 1.0];
    let n = edge.len() as u32;
    for (ri, s) in fracs.iter().enumerate() {
        for q in &edge {
            let v = at + (*q - at) * *s;
            mesh.colored_vertex(v, color.gamma_multiply(f(v)));
        }
        let ring = 1 + ri as u32 * n;
        for i in 0..n {
            let j = (i + 1) % n;
            if ri == 0 {
                mesh.add_triangle(0, ring + i, ring + j);
            } else {
                mesh.add_triangle(ring - n + i, ring - n + j, ring + i);
                mesh.add_triangle(ring - n + j, ring + j, ring + i);
            }
        }
    }
    Shape::mesh(mesh)
}

/// A card's edge catching light from `at`: a hairline of `width`, `base` everywhere and
/// up to `color` where it's nearest the light.
pub fn rim(p: &Painter, rect: Rect, r: f32, at: Pos2, reach: f32, base: Color32, color: Color32, width: f32) {
    let s = sides(rect, 16.0);
    let outer = outline(rect, r, 8, s);
    let inner = outline(rect.shrink(width), (r - width).max(0.0), 8, s);
    let lit = theme::effects();
    let cols: Vec<Color32> = outer
        .iter()
        .map(|q| {
            let t = if lit { (1.0 - ((*q - at).length() / reach).min(1.0)).powf(1.5) } else { 0.0 };
            theme::mix(base, color, t)
        })
        .collect();
    let mut mesh = Mesh::default();
    band(&mut mesh, &inner, &cols, &outer, &cols);
    p.add(Shape::mesh(mesh));
}

/// A rounded rectangle filled with a linear gradient: `stops` are (position 0 to 1,
/// colour), down the rectangle when `vertical`, else across it. Exact at every stop.
pub fn gradient(p: &Painter, rect: Rect, r: f32, vertical: bool, stops: &[(f32, Color32)]) {
    p.add(gradient_shape(rect, r, vertical, stops));
}

pub fn gradient_shape(rect: Rect, r: f32, vertical: bool, stops: &[(f32, Color32)]) -> Shape {
    let r = r.min(rect.width() / 2.0).min(rect.height() / 2.0).max(0.0);
    let (len, cross0, cross1) = if vertical { (rect.height(), rect.left(), rect.right()) } else { (rect.width(), rect.top(), rect.bottom()) };
    if len <= 0.0 || stops.is_empty() {
        return Shape::Noop;
    }
    let start = if vertical { rect.top() } else { rect.left() };
    // Positions along the axis: the stops, plus enough steps to round the corners.
    let mut ts: Vec<f32> = stops.iter().map(|s| s.0.clamp(0.0, 1.0)).collect();
    let rt = r / len;
    for i in 0..=8 {
        let k = i as f32 / 8.0;
        ts.push(rt * (1.0 - (k * std::f32::consts::FRAC_PI_2).cos()));
        ts.push(1.0 - rt * (1.0 - (k * std::f32::consts::FRAC_PI_2).cos()));
    }
    ts.extend([0.0, 1.0]);
    ts.sort_by(|a, b| a.total_cmp(b));
    ts.dedup_by(|a, b| (*a - *b).abs() < 1e-4);
    let color_at = |t: f32| -> Color32 {
        if t <= stops[0].0 {
            return stops[0].1;
        }
        for w in stops.windows(2) {
            if t <= w[1].0 {
                let k = if w[1].0 > w[0].0 { (t - w[0].0) / (w[1].0 - w[0].0) } else { 1.0 };
                return theme::mix(w[0].1, w[1].1, k);
            }
        }
        stops[stops.len() - 1].1
    };
    // How far the corner pulls the edge in at a point along the axis.
    let inset = |a: f32| -> f32 {
        let d = if a < r { r - a } else if a > len - r { a - (len - r) } else { return 0.0 };
        r - (r * r - d * d).max(0.0).sqrt()
    };
    let mut mesh = Mesh::default();
    for (i, t) in ts.iter().enumerate() {
        let a = t * len;
        let k = inset(a);
        let c = color_at(*t);
        let along = start + a;
        let (p0, p1) = if vertical { (pos2(cross0 + k, along), pos2(cross1 - k, along)) } else { (pos2(along, cross0 + k), pos2(along, cross1 - k)) };
        mesh.colored_vertex(p0, c);
        mesh.colored_vertex(p1, c);
        if i > 0 {
            let b = (i as u32) * 2;
            mesh.add_triangle(b - 2, b - 1, b);
            mesh.add_triangle(b - 1, b + 1, b);
        }
    }
    Shape::mesh(mesh)
}

/// A soft drop shadow under a rounded rectangle.
pub fn shadow(p: &Painter, rect: Rect, r: f32, blur: u8, y: i8, alpha: u8) {
    let s = eframe::egui::Shadow { offset: [0, y], blur, spread: 0, color: Color32::from_black_alpha(alpha) };
    p.add(s.as_shape(rect, r));
}

/// The ring round whatever the keyboard or controller is on.
pub fn focus_ring(p: &Painter, rect: Rect, r: f32) {
    glow(p, rect, r, theme::ACCENT.gamma_multiply(0.55), 10.0);
    p.rect_stroke(rect.expand(1.5), r + 1.5, Stroke::new(2.0, theme::ACCENT_HI), eframe::egui::StrokeKind::Outside);
}

/// A small glowing dot (a running game, a player online).
pub fn dot(p: &Painter, c: Pos2, r: f32, color: Color32) {
    light(p, c, Vec2::splat(r * 3.2), color.gamma_multiply(0.45));
    p.circle_filled(c, r, color);
    p.circle_filled(c - vec2(r * 0.3, r * 0.3), r * 0.35, Color32::from_white_alpha(110));
}

/// A tiny random generator: the same `seed` draws the same arc.
fn rng(seed: u64) -> impl FnMut() -> f32 {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
    }
}

/// A jagged path from `a` to `b` (midpoint displacement).
fn jag(a: Pos2, b: Pos2, rough: f32, levels: usize, next: &mut impl FnMut() -> f32) -> Vec<Pos2> {
    let mut pts = vec![a, b];
    let mut amp = (b - a).length() * rough;
    for _ in 0..levels {
        let mut out = Vec::with_capacity(pts.len() * 2);
        for w in pts.windows(2) {
            let (p0, p1) = (w[0], w[1]);
            let d = p1 - p0;
            let n = vec2(-d.y, d.x).normalized();
            out.push(p0);
            out.push(p0 + d * 0.5 + n * next() * amp);
        }
        out.push(*pts.last().unwrap());
        pts = out;
        amp *= 0.55;
    }
    pts
}

/// Draws a bolt's path: a wide faint glow, a coloured body and a white-hot core.
fn strike(p: &Painter, pts: Vec<Pos2>, color: Color32, strength: f32, width: f32) {
    let s = strength.clamp(0.0, 1.0);
    p.add(Shape::line(pts.clone(), Stroke::new(width * 6.0, color.gamma_multiply(0.10 * s))));
    p.add(Shape::line(pts.clone(), Stroke::new(width * 2.6, color.gamma_multiply(0.35 * s))));
    p.add(Shape::line(pts, Stroke::new(width, theme::mix(color, Color32::WHITE, 0.75).gamma_multiply(0.95 * s))));
}

/// A lightning bolt from `a` to `b` with a fork or two. A new `seed` each few frames
/// makes it crackle.
pub fn bolt(p: &Painter, a: Pos2, b: Pos2, seed: u64, strength: f32, color: Color32) {
    if !theme::effects() || strength <= 0.0 {
        return;
    }
    let mut next = rng(seed);
    let main = jag(a, b, 0.2, 5, &mut next);
    for k in 0..2 {
        let i = ((next() * 0.5 + 0.5) * (main.len() as f32 * 0.6)) as usize + main.len() / 5;
        let Some(&from) = main.get(i) else { continue };
        let dir = (b - a).normalized();
        let side = if k == 0 { 1.0 } else { -1.0 };
        let turn = 0.5 * side + next() * 0.25;
        let d = vec2(dir.x * turn.cos() - dir.y * turn.sin(), dir.x * turn.sin() + dir.y * turn.cos());
        let len = (b - a).length() * (0.18 + 0.12 * next().abs());
        strike(p, jag(from, from + d * len, 0.25, 3, &mut next), color, strength * 0.6, 0.8);
    }
    strike(p, main, color, strength, 1.3);
}

/// Electricity running round a rounded rectangle's edge: `t` from 0 (it starts) to 1
/// (it's gone), the arcs travelling and fading meanwhile.
pub fn electric_edge(p: &Painter, rect: Rect, r: f32, t: f32, seed: u64, color: Color32) {
    if !theme::effects() || !(0.0..1.0).contains(&t) {
        return;
    }
    let edge = outline(rect.expand(1.5), r + 1.5, 8, sides(rect, 10.0));
    let n = edge.len();
    let mut next = rng(seed);
    let strength = (1.0 - t).powf(1.4) * (t * 12.0).min(1.0);
    for k in 0..3 {
        let start = ((k as f32 / 3.0 + t * 0.6 + next().abs() * 0.05) * n as f32) as usize;
        let span = (n as f32 * (0.16 + 0.06 * next().abs())) as usize;
        let mut pts = Vec::with_capacity(span + 1);
        for j in 0..=span {
            let i = (start + j) % n;
            let (prev, nxt) = (edge[(i + n - 1) % n], edge[(i + 1) % n]);
            let d = nxt - prev;
            let normal = vec2(d.y, -d.x).normalized();
            // Tapered at both ends so the arc grows out of the edge and back into it.
            let taper = (std::f32::consts::PI * j as f32 / span.max(1) as f32).sin();
            pts.push(edge[i] + normal * next() * 3.2 * taper);
        }
        strike(p, pts, color, strength, 1.1);
    }
}
