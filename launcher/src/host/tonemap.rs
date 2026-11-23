//! A copy's frame, as its hook read it back (`ring::PixelFrames`), turned into 8-bit
//! BGRA sRGB rows: what the Linux session's window draws (vkblit.rs), which blits them
//! into an SDR swap chain.
//!
//! SDR frames only have their channels put in order. HDR ones are tone-mapped as the
//! Windows window's shader does for an SDR display (gpu.rs): HDR10 decoded from PQ to
//! nits and BT.709 primaries, scRGB (a float frame) taken as 1.0 = 80 nits, both shown
//! with 203 nits as white and their highlights rolled off under it by the brightest
//! channel. Lookup tables keep it to a few operations a pixel.

use std::sync::OnceLock;

/// The nits shown as white (BT.2408's reference white), as in gpu.rs.
const HDR_WHITE_NITS: f32 = 203.0;
/// Linear light in `0..=1` to an sRGB byte, in this many steps.
const ENCODE_STEPS: usize = 16384;

struct Tables {
    /// A 10-bit PQ code value's linear light, 1.0 being `HDR_WHITE_NITS`.
    pq: Vec<f32>,
    /// Linear light (`0..=1`, `ENCODE_STEPS` steps) to its sRGB-encoded byte.
    encode: Vec<u8>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let pq = (0..1024)
            .map(|c| {
                let e = (c as f32 / 1023.0).powf(1.0 / 78.84375);
                let y = ((e - 0.835_937_5).max(0.0) / (18.851_563 - 18.6875 * e)).powf(1.0 / 0.159_301_76);
                10000.0 * y / HDR_WHITE_NITS
            })
            .collect();
        let encode = (0..ENCODE_STEPS)
            .map(|i| {
                let c = i as f32 / (ENCODE_STEPS - 1) as f32;
                let e = if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
                (e * 255.0 + 0.5).clamp(0.0, 255.0) as u8
            })
            .collect();
        Tables { pq, encode }
    })
}

/// HDR highlights brought under SDR white: untouched to 0.8, then rolled off toward
/// 1.0, by the brightest channel so hues don't shift (gpu.rs's `roll_off`).
fn roll_off(c: [f32; 3]) -> [f32; 3] {
    let m = c[0].max(c[1]).max(c[2]);
    if !m.is_finite() {
        return [1.0; 3];
    }
    if m <= 0.8 {
        return c;
    }
    let k = (m - 0.8) / 0.2;
    let s = (0.8 + 0.2 * k / (1.0 + k)) / m;
    [c[0] * s, c[1] * s, c[2] * s]
}

/// Linear light (1.0 = white) to BGRA bytes.
fn encode(t: &Tables, c: [f32; 3]) -> [u8; 4] {
    let c = roll_off([c[0].max(0.0), c[1].max(0.0), c[2].max(0.0)]);
    let b = |v: f32| t.encode[((v.min(1.0) * (ENCODE_STEPS - 1) as f32) + 0.5) as usize];
    [b(c[2]), b(c[1]), b(c[0]), 255]
}

/// Linear BT.2020 to BT.709 primaries.
fn from_2020(c: [f32; 3]) -> [f32; 3] {
    [
        1.660_491 * c[0] - 0.587_641 * c[1] - 0.072_850 * c[2],
        -0.124_550 * c[0] + 1.132_900 * c[1] - 0.008_349 * c[2],
        -0.018_151 * c[0] - 0.100_579 * c[1] + 1.118_730 * c[2],
    ]
}

/// A float16's value, kept finite: NaN is 0 and infinity the largest half (a game's
/// stray NaN or infinite pixel shows as black or white, not spreading through the maths).
fn half(h: u16) -> f32 {
    let (s, e, m) = ((h >> 15) & 1, i32::from((h >> 10) & 0x1f), f32::from(h & 0x3ff));
    let v = match e {
        0 => m * f32::powi(2.0, -24),
        31 if m != 0.0 => return 0.0,
        31 => 65504.0,
        e => (1.0 + m / 1024.0) * f32::powi(2.0, e - 15),
    };
    if s == 1 { -v } else { v }
}

/// Whether frames of `format` can be converted (`frame_ring::bytes_per_pixel`'s).
pub fn supported(format: u32) -> bool {
    matches!(format, 28 | 29 | 87 | 88 | 91 | 93 | 24 | 10)
}

/// `w x h` pixels of `format` and `color_space` (rows `stride` bytes apart in `src`)
/// into `dst` as tight BGRA rows (`w * 4` bytes each), opaque (games often leave their
/// back buffer's alpha at 0).
pub fn to_bgra(format: u32, color_space: u32, src: &[u8], stride: usize, w: usize, h: usize, dst: &mut [u8]) {
    use peerparty_protocol::frame_ring::COLOR_SPACE_HDR10;
    let row = w * 4;
    if dst.len() < row * h {
        return;
    }
    let t = tables();
    for y in 0..h {
        let Some(s) = src.get(y * stride..) else { return };
        let d = &mut dst[y * row..(y + 1) * row];
        match format {
            87 | 91 | 88 | 93 if s.len() >= row => {
                for (o, i) in d.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
                    o.copy_from_slice(&[i[0], i[1], i[2], 255]);
                }
            }
            28 | 29 if s.len() >= row => {
                for (o, i) in d.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
                    o.copy_from_slice(&[i[2], i[1], i[0], 255]);
                }
            }
            24 if s.len() >= row => {
                let hdr10 = color_space == COLOR_SPACE_HDR10;
                for (o, i) in d.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
                    let v = u32::from_le_bytes([i[0], i[1], i[2], i[3]]);
                    let (r, g, b) = ((v & 0x3ff) as usize, ((v >> 10) & 0x3ff) as usize, ((v >> 20) & 0x3ff) as usize);
                    let px = if hdr10 { encode(t, from_2020([t.pq[r], t.pq[g], t.pq[b]])) } else { [(b >> 2) as u8, (g >> 2) as u8, (r >> 2) as u8, 255] };
                    o.copy_from_slice(&px);
                }
            }
            10 if s.len() >= w * 8 => {
                let k = 80.0 / HDR_WHITE_NITS;
                for (o, i) in d.chunks_exact_mut(4).zip(s.chunks_exact(8)) {
                    let c = |n: usize| half(u16::from_le_bytes([i[n], i[n + 1]])) * k;
                    o.copy_from_slice(&encode(t, [c(0), c(2), c(4)]));
                }
            }
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pq_code(nits: f32) -> u32 {
        let y = (nits / 10000.0).powf(0.159_301_76);
        ((((0.835_937_5 + 18.851_563 * y) / (1.0 + 18.6875 * y)).powf(78.84375)) * 1023.0).round() as u32
    }

    /// The same numbers the Windows window's shader gives for an SDR display (gpu.rs's
    /// test): 203-nit grey just under white, 100-nit grey under the knee, SDR as it is.
    #[test]
    fn hdr_frames_are_tone_mapped_like_the_shader() {
        let mut out = vec![0u8; 4];
        let px = |c: u32| (c | c << 10 | c << 20 | 3 << 30).to_le_bytes();
        to_bgra(24, 12, &px(pq_code(203.0)), 4, 1, 1, &mut out);
        assert!((242..=245).contains(&out[1]), "203 nits: {out:?}");
        to_bgra(24, 12, &px(pq_code(100.0)), 4, 1, 1, &mut out);
        assert!((183..=189).contains(&out[1]), "100 nits: {out:?}");
        // A BT.2020 red is a BT.709 red, out of gamut and clamped.
        to_bgra(24, 12, &(pq_code(100.0) | 3 << 30).to_le_bytes(), 4, 1, 1, &mut out);
        assert!(out[2] > 200 && out[1] == 0 && out[0] == 0, "red: {out:?}");
        // scRGB 2.5 (200 nits).
        let two_and_a_half = 0x4100u16.to_le_bytes();
        let f = [two_and_a_half, two_and_a_half, two_and_a_half, 0x3c00u16.to_le_bytes()].concat();
        to_bgra(10, 10, &f, 8, 1, 1, &mut out);
        assert!((240..=245).contains(&out[0]), "200 nits of scRGB: {out:?}");
        // SDR 10-bit and 8-bit, put in BGRA order.
        to_bgra(24, 0, &(0x3ffu32 | 3 << 30).to_le_bytes(), 4, 1, 1, &mut out);
        assert_eq!(out, [0, 0, 255, 255]);
        to_bgra(28, 0, &[1, 2, 3, 4], 4, 1, 1, &mut out);
        assert_eq!(out, [3, 2, 1, 255], "opaque");
        to_bgra(88, 0, &[1, 2, 3, 0], 4, 1, 1, &mut out);
        assert_eq!(out, [1, 2, 3, 255]);
        to_bgra(87, 0, &[1, 2, 3, 0], 4, 1, 1, &mut out);
        assert_eq!(out, [1, 2, 3, 255], "a back buffer's alpha 0 doesn't make the pane transparent");
    }

    /// A float frame's infinite and NaN pixels come out white and black, not as NaN
    /// spreading through the roll-off (which made infinity white and others garbage).
    #[test]
    fn infinite_and_nan_pixels_stay_in_range() {
        let mut out = vec![0u8; 4];
        let px = |h: u16| [h.to_le_bytes(), h.to_le_bytes(), h.to_le_bytes(), 0x3c00u16.to_le_bytes()].concat();
        to_bgra(10, 10, &px(0x7c00), 8, 1, 1, &mut out);
        assert_eq!(out, [255, 255, 255, 255], "+infinity is white");
        to_bgra(10, 10, &px(0x7e00), 8, 1, 1, &mut out);
        assert_eq!(out, [0, 0, 0, 255], "NaN is black");
        to_bgra(10, 10, &px(0xfc00), 8, 1, 1, &mut out);
        assert_eq!(out, [0, 0, 0, 255], "-infinity is black");
        assert!(roll_off([f32::INFINITY, 0.0, 0.0]).iter().all(|c| c.is_finite()));
    }

    #[test]
    fn rows_follow_their_stride() {
        let src = [1u8, 2, 3, 4, 9, 9, 9, 9, 5, 6, 7, 8, 9, 9, 9, 9];
        let mut out = vec![0u8; 8];
        to_bgra(87, 0, &src, 8, 1, 2, &mut out);
        assert_eq!(out, [1, 2, 3, 255, 5, 6, 7, 255]);
        assert!(!supported(2) && supported(10));
    }
}
