//! PeerParty's icon: three coloured nodes joined in a triangle on a dark rounded tile,
//! with confetti (a party of peers), drawn at any size. The window uses it, and
//! `build.rs` draws the exe's `.ico` from it, so the two always match.
//!
//! Small sizes are drawn simpler, so they stay clean rather than blurred: below 48
//! pixels the confetti goes, at 32 and below the nodes spread out, and at 24 and below
//! the gaps round the nodes go too.

type Rgb = [f32; 3];

fn hex(c: u32) -> Rgb {
    [((c >> 16) & 0xff) as f32, ((c >> 8) & 0xff) as f32, (c & 0xff) as f32]
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn dist(x: f32, y: f32, cx: f32, cy: f32) -> f32 {
    ((x - cx).powi(2) + (y - cy).powi(2)).sqrt()
}

/// Distance from (x, y) to the segment a-b.
fn seg(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((x - a.0) * dx + (y - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    dist(x, y, a.0 + t * dx, a.1 + t * dy)
}

/// How much detail a size gets.
#[derive(Clone, Copy)]
struct Detail {
    /// The nodes' centres: spread wider at small sizes, so the links between them show.
    at: [(f32, f32); 3],
    node: f32,
    gap: f32,
    link: f32,
    confetti: bool,
}

fn detail(n: usize) -> Detail {
    match n {
        0..=24 => Detail { at: [(15.0, 21.0), (49.0, 21.0), (32.0, 49.0)], node: 7.5, gap: 0.0, link: 3.0, confetti: false },
        25..=32 => Detail { at: [(17.0, 23.0), (47.0, 23.0), (32.0, 47.0)], node: 7.0, gap: 1.5, link: 2.6, confetti: false },
        _ => Detail { at: [(19.0, 24.0), (45.0, 24.0), (32.0, 46.0)], node: 6.5, gap: 1.5, link: 2.2, confetti: n >= 48 },
    }
}

/// The icon as RGBA pixels, `n` by `n`. Drawn on a 64-unit grid and sampled 4x4 per
/// pixel, so edges stay smooth from 16 to 256 pixels.
pub fn icon_rgba(n: usize) -> Vec<u8> {
    const S: usize = 4;
    let d = detail(n);
    let unit = 64.0 / n as f32;
    let mut rgba = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let (mut r, mut g, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..S {
                for sx in 0..S {
                    let fx = (x as f32 + (sx as f32 + 0.5) / S as f32) * unit;
                    let fy = (y as f32 + (sy as f32 + 0.5) / S as f32) * unit;
                    if let Some(c) = sample(fx, fy, d) {
                        r += c[0];
                        g += c[1];
                        b += c[2];
                        a += 1.0;
                    }
                }
            }
            if a > 0.0 {
                let i = (y * n + x) * 4;
                let k = (S * S) as f32;
                rgba[i..i + 4].copy_from_slice(&[(r / a).round() as u8, (g / a).round() as u8, (b / a).round() as u8, (255.0 * a / k).round() as u8]);
            }
        }
    }
    rgba
}

/// The colour at a point of the 64-unit design, `None` outside the tile.
fn sample(x: f32, y: f32, d: Detail) -> Option<Rgb> {
    let r = 14.0;
    let (cx, cy) = (x.clamp(r, 64.0 - r), y.clamp(r, 64.0 - r));
    if (x - cx).powi(2) + (y - cy).powi(2) > r * r {
        return None;
    }
    // The tile: deep navy to deep violet, top left to bottom right.
    let bg = mix(hex(0x111a33), hex(0x231a44), (x + y) / 128.0);
    let nodes = [(d.at[0], hex(0x4fe0ff)), (d.at[1], hex(0xff5ca8)), (d.at[2], hex(0xffd23f))];
    for (c, col) in nodes {
        let dn = dist(x, y, c.0, c.1);
        if dn <= d.node {
            // A soft highlight in the middle.
            return Some(mix(col, hex(0xffffff), (1.0 - dn / d.node) * 0.35));
        }
        if dn <= d.node + d.gap {
            return Some(bg);
        }
    }
    for (a, b) in [(0, 1), (1, 2), (2, 0)] {
        if seg(x, y, nodes[a].0, nodes[b].0) <= d.link {
            return Some(hex(0xd8e0ff));
        }
    }
    if d.confetti {
        let confetti = [((10.5, 49.0), 2.2, hex(0xff5ca8)), ((53.5, 50.0), 2.2, hex(0x4fe0ff)), ((51.0, 9.5), 1.8, hex(0xffd23f)), ((12.0, 11.0), 1.6, hex(0x9a7bff)), ((32.0, 10.0), 1.5, hex(0x4fe0ff))];
        for (c, r, col) in confetti {
            if dist(x, y, c.0, c.1) <= r {
                return Some(col);
            }
        }
    }
    Some(bg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The icon fills its tile and leaves the corners clear at every size the exe has.
    #[test]
    fn draws_at_every_size() {
        for n in [16, 20, 24, 32, 40, 48, 64, 128, 256] {
            let px = icon_rgba(n);
            assert_eq!(px.len(), n * n * 4);
            assert_eq!(px[3], 0, "{n}: corner");
            let edge = (n / 2 * n + 1) * 4;
            assert_eq!(px[edge + 3], 255, "{n}: edge");
        }
    }

    /// PNG previews (`PEERPARTY_ICON_DIR`): the icon at 256 pixels, and a sheet of every
    /// size the exe has, at 1x and blown up 4x, on dark and light backgrounds.
    #[test]
    fn previews() {
        let Some(dir) = std::env::var_os("PEERPARTY_ICON_DIR").map(std::path::PathBuf::from) else { return };
        std::fs::create_dir_all(&dir).unwrap();
        image::save_buffer(dir.join("icon-256.png"), &icon_rgba(256), 256, 256, image::ExtendedColorType::Rgba8).unwrap();
        let sizes = [256usize, 64, 48, 40, 32, 24, 20, 16];
        let pad = 16;
        let row_w: usize = sizes.iter().map(|n| n + pad).sum::<usize>() + pad;
        let small = [32usize, 24, 20, 16];
        let zoom_w: usize = small.iter().map(|n| n * 4 + pad).sum::<usize>() + pad;
        let w = row_w.max(zoom_w);
        let half_h = 256 + 2 * pad + 32 * 4 + pad;
        let h = half_h * 2;
        let mut sheet = vec![0u8; w * h * 4];
        let blit = |sheet: &mut Vec<u8>, px: &[u8], n: usize, zoom: usize, x0: usize, y0: usize| {
            for y in 0..n * zoom {
                for x in 0..n * zoom {
                    let s = ((y / zoom) * n + x / zoom) * 4;
                    let a = px[s + 3] as f32 / 255.0;
                    let d = ((y0 + y) * w + x0 + x) * 4;
                    for c in 0..3 {
                        sheet[d + c] = (px[s + c] as f32 * a + sheet[d + c] as f32 * (1.0 - a)).round() as u8;
                    }
                }
            }
        };
        for (half, bg) in [[0x1a, 0x1d, 0x26], [0xf2, 0xf3, 0xf6]].iter().enumerate() {
            let y0 = half * half_h;
            for y in y0..y0 + half_h {
                for x in 0..w {
                    let i = (y * w + x) * 4;
                    sheet[i..i + 4].copy_from_slice(&[bg[0], bg[1], bg[2], 255]);
                }
            }
            let mut x = pad;
            for n in sizes {
                blit(&mut sheet, &icon_rgba(n), n, 1, x, y0 + pad + (256 - n) / 2);
                x += n + pad;
            }
            let mut x = pad;
            for n in small {
                blit(&mut sheet, &icon_rgba(n), n, 4, x, y0 + 256 + 2 * pad);
                x += n * 4 + pad;
            }
        }
        image::save_buffer(dir.join("icon-sheet.png"), &sheet, w as u32, h as u32, image::ExtendedColorType::Rgba8).unwrap();
    }
}
