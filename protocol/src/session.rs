//! More than one copy of a game on one screen: what the launcher tells each copy about
//! its place (`InstanceSpec`, in `PEERPARTY_INSTANCE` as JSON), and how a screen is cut
//! into panes for them.

use crate::json::Json;

/// The most panes a layout makes or a spec lists.
pub const MAX_PANES: usize = 16;

/// Marks a key message (`WM_KEYDOWN` and the rest) a session's window posted to a copy's
/// window, in one of the `lParam` bits Windows leaves reserved (25). The session's window
/// already turned the key into its `WM_CHAR`, which it posts too, so the copy's hook
/// keeps the game's `TranslateMessage` from making a second one from the key.
pub const FORWARDED_KEY: isize = 1 << 25;
/// The longest identity override kept.
const MAX_IDENTITY: usize = 256;

/// One copy of the game in a session (0 is the first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceId(pub u32);

/// A pane of the screen, in screen pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaneRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl PaneRect {
    pub fn to_json(&self) -> Json {
        Json::obj(vec![("x", Json::int(self.x.into())), ("y", Json::int(self.y.into())), ("width", Json::u64(self.width.into())), ("height", Json::u64(self.height.into()))])
    }

    pub fn from_json(v: &Json) -> Option<PaneRect> {
        let i = |k: &str| v.get(k).as_i64().and_then(|n| i32::try_from(n).ok());
        let u = |k: &str| v.get(k).as_u64().and_then(|n| u32::try_from(n).ok());
        Some(PaneRect { x: i("x")?, y: i("y")?, width: u("width")?, height: u("height")? })
    }
}

/// Fixed-point denominator for `NormRect` coordinates (0..=10_000 = 0.00%..=100.00%).
pub const NORM_SCALE: u16 = 10_000;
/// Minimum normalized width or height of an interactive pane (10% of the window).
pub const NORM_MIN: u16 = 1_000;

/// A resolution-independent rectangle in `[0, NORM_SCALE]` of a monitor's client area.
/// Stored as integer basis points so layouts stay exact (`Eq`/`Hash`), free of NaN,
/// and scale seamlessly across 1080p, 1440p, 4K, and ultrawide displays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NormRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl NormRect {
    pub const FULL: NormRect = NormRect { x: 0, y: 0, w: NORM_SCALE, h: NORM_SCALE };

    /// Clamps `x, y, w, h` inside `[0, NORM_SCALE]` with at least `NORM_MIN` extent.
    pub fn clamped(x: u16, y: u16, w: u16, h: u16) -> NormRect {
        let w = w.clamp(NORM_MIN, NORM_SCALE);
        let h = h.clamp(NORM_MIN, NORM_SCALE);
        let x = x.min(NORM_SCALE - w);
        let y = y.min(NORM_SCALE - h);
        NormRect { x, y, w, h }
    }

    /// Converts normalized float coordinates (`0.0..=1.0`) to a clamped `NormRect`.
    pub fn from_f32(x: f32, y: f32, w: f32, h: f32) -> NormRect {
        let to_u = |v: f32| (v.clamp(0.0, 1.0) * f32::from(NORM_SCALE)).round() as u16;
        let x0 = to_u(x);
        let y0 = to_u(y);
        let x1 = to_u(x + w).max(x0.saturating_add(NORM_MIN)).min(NORM_SCALE);
        let y1 = to_u(y + h).max(y0.saturating_add(NORM_MIN)).min(NORM_SCALE);
        Self::clamped(x0, y0, x1 - x0, y1 - y0)
    }

    pub fn right(self) -> u16 {
        self.x.saturating_add(self.w).min(NORM_SCALE)
    }

    pub fn bottom(self) -> u16 {
        self.y.saturating_add(self.h).min(NORM_SCALE)
    }

    /// Resolves to physical client pixels using edge rounding so adjacent panes sharing a
    /// normalized edge share the exact same pixel boundary with zero 1 px seams.
    pub fn to_pane_rect(self, screen_w: u32, screen_h: u32) -> PaneRect {
        let px = |v: u16, total: u32| ((u64::from(total) * u64::from(v) + u64::from(NORM_SCALE) / 2) / u64::from(NORM_SCALE)) as u32;
        let x0 = px(self.x, screen_w);
        let y0 = px(self.y, screen_h);
        let x1 = px(self.right(), screen_w);
        let y1 = px(self.bottom(), screen_h);
        PaneRect { x: x0 as i32, y: y0 as i32, width: x1.saturating_sub(x0), height: y1.saturating_sub(y0) }
    }

    pub fn to_json(&self) -> Json {
        Json::obj(vec![
            ("x", Json::u64(self.x.into())),
            ("y", Json::u64(self.y.into())),
            ("w", Json::u64(self.w.into())),
            ("h", Json::u64(self.h.into())),
        ])
    }

    pub fn from_json(v: &Json) -> Option<NormRect> {
        let u = |k: &str| v.get(k).as_u64().and_then(|n| u16::try_from(n).ok());
        Some(Self::clamped(u("x")?, u("y")?, u("w")?, u("h")?))
    }
}

/// How a pane's game frames map into its destination rect on the compositor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PaneFit {
    /// Preserve aspect ratio with letterbox or pillarbox bars.
    #[default]
    Fit,
    /// Scale to fill the pane completely, cropping overflow evenly (zero black bars).
    Fill,
    /// Stretch edge-to-edge across the pane.
    Stretch,
}

impl PaneFit {
    pub fn as_str(self) -> &'static str {
        match self {
            PaneFit::Fit => "fit",
            PaneFit::Fill => "fill",
            PaneFit::Stretch => "stretch",
        }
    }

    pub fn parse(s: &str) -> Option<PaneFit> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "fit" => PaneFit::Fit,
            "fill" | "crop" => PaneFit::Fill,
            "stretch" => PaneFit::Stretch,
            _ => return None,
        })
    }

    pub fn next(self) -> PaneFit {
        match self {
            PaneFit::Fit => PaneFit::Fill,
            PaneFit::Fill => PaneFit::Stretch,
            PaneFit::Stretch => PaneFit::Fit,
        }
    }
}

/// What aspect ratio the game is told to render at inside its pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum VirtualAspect {
    /// Match the pane's physical pixel dimensions.
    #[default]
    MatchPane,
    /// Lock the game's virtual screen to a fixed aspect ratio inside the pane so games
    /// with fixed-aspect HUDs stay legible in tall or narrow panes.
    Ratio { num: u16, den: u16 },
}

impl VirtualAspect {
    /// Standard aspect presets cycled in the live layout editor.
    pub const PRESETS: [VirtualAspect; 5] = [
        VirtualAspect::MatchPane,
        VirtualAspect::Ratio { num: 16, den: 9 },
        VirtualAspect::Ratio { num: 16, den: 10 },
        VirtualAspect::Ratio { num: 4, den: 3 },
        VirtualAspect::Ratio { num: 21, den: 9 },
    ];

    pub fn label(self) -> String {
        match self {
            VirtualAspect::MatchPane => "match".into(),
            VirtualAspect::Ratio { num, den } => format!("{num}:{den}"),
        }
    }

    pub fn parse(s: &str) -> Option<VirtualAspect> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("match") || s.eq_ignore_ascii_case("auto") || s.is_empty() {
            return Some(VirtualAspect::MatchPane);
        }
        let (a, b) = s.split_once(':')?;
        let num: u16 = a.parse().ok()?;
        let den: u16 = b.parse().ok()?;
        (num >= 1 && den >= 1 && num <= 64 && den <= 64).then_some(VirtualAspect::Ratio { num, den })
    }

    pub fn next(self) -> VirtualAspect {
        let i = Self::PRESETS.iter().position(|&a| a == self).map_or(0, |i| (i + 1) % Self::PRESETS.len());
        Self::PRESETS[i]
    }

    /// Computes the `(width, height)` the game is told to render at for a physical `pane`
    /// inside a `screen_w x screen_h` monitor. In `Fill` mode with an explicit ratio,
    /// scales up to cover the pane (clamped to the monitor) so cropped frames stay sharp.
    pub fn size_for(self, pane: PaneRect, fit: PaneFit, screen_w: u32, screen_h: u32) -> (u32, u32) {
        if pane.width == 0 || pane.height == 0 {
            return (pane.width, pane.height);
        }
        let VirtualAspect::Ratio { num, den } = self else {
            return (pane.width, pane.height);
        };
        let (num, den) = (u64::from(num.max(1)), u64::from(den.max(1)));
        let (pw, ph) = (u64::from(pane.width), u64::from(pane.height));
        let (mw, mh) = (u64::from(screen_w.max(pane.width)), u64::from(screen_h.max(pane.height)));
        let lhs = pw * den;
        let rhs = ph * num;
        let (mut w, mut h) = if fit == PaneFit::Fill {
            // Cover the pane at `num:den`, then fit inside monitor bounds so the
            // contained game window never exceeds the monitor.
            let (cw, ch) = if lhs >= rhs {
                (pw, (pw * den + num / 2) / num)
            } else {
                ((ph * num + den / 2) / den, ph)
            };
            if cw > mw || ch > mh {
                if mw * den <= mh * num {
                    (mw, (mw * den + num / 2) / num)
                } else {
                    ((mh * num + den / 2) / den, mh)
                }
            } else {
                (cw, ch)
            }
        } else if lhs >= rhs {
            // Pane is wider than target aspect: match pane height.
            ((ph * num + den / 2) / den, ph)
        } else {
            // Pane is taller than target aspect: match pane width.
            (pw, (pw * den + num / 2) / num)
        };
        // Even dimensions avoid chroma/swapchain alignment quirks in D3D/Vulkan engines.
        w = (w & !1).max(2).min(mw);
        h = (h & !1).max(2).min(mh);
        (w as u32, h as u32)
    }
}

/// Scales a pane's virtual render dimensions by `scale_pct` (`50..=100`; `0` or `>= 100`
/// leaves them untouched). Results are rounded to even pixels and kept above a safe
/// minimum floor so games never receive degenerate swapchain sizes.
pub fn scale_dimensions(width: u32, height: u32, scale_pct: u8) -> (u32, u32) {
    if width == 0 || height == 0 || scale_pct == 0 || scale_pct >= 100 {
        return (width, height);
    }
    let pct = u64::from(scale_pct.clamp(50, 100));
    let min_w = width.min(320).max(2);
    let min_h = height.min(200).max(2);
    let mut w = ((u64::from(width) * pct + 50) / 100) as u32;
    let mut h = ((u64::from(height) * pct + 50) / 100) as u32;
    w = (w & !1).clamp(min_w & !1, width);
    h = (h & !1).clamp(min_h & !1, height);
    (w.max(2), h.max(2))
}

/// One pane in a resolution-independent custom layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaneSlot {
    pub rect: NormRect,
    pub fit: PaneFit,
    pub aspect: VirtualAspect,
    /// Draw & hit-test order for overlapping Picture-in-Picture panes (higher is on top).
    pub z: u8,
    /// The pane is its game's picture, and keeps the picture's shape when resized: its
    /// game draws one shape whatever its pane's (Layout Studio's Trim bars and Fit
    /// pictures), so a pane of another shape only gets black bars.
    pub keep_picture: bool,
}

impl PaneSlot {
    pub fn new(rect: NormRect) -> PaneSlot {
        PaneSlot { rect, fit: PaneFit::Fit, aspect: VirtualAspect::MatchPane, z: 0, keep_picture: false }
    }

    pub fn to_json(&self) -> Json {
        let mut fields = vec![
            ("rect", self.rect.to_json()),
            ("fit", Json::str(self.fit.as_str())),
            ("aspect", Json::str(&self.aspect.label())),
            ("z", Json::u64(self.z.into())),
        ];
        if self.keep_picture {
            fields.push(("keep_picture", Json::Bool(true)));
        }
        Json::obj(fields)
    }

    pub fn from_json(v: &Json) -> Option<PaneSlot> {
        Some(PaneSlot {
            rect: NormRect::from_json(v.get("rect"))?,
            fit: v.get("fit").as_str().and_then(PaneFit::parse).unwrap_or_default(),
            aspect: v.get("aspect").as_str().and_then(VirtualAspect::parse).unwrap_or_default(),
            z: v.get("z").as_u64().and_then(|n| u8::try_from(n).ok()).unwrap_or(0),
            keep_picture: v.get("keep_picture").as_bool().unwrap_or(false),
        })
    }

    /// Compact shell-safe wire encoding (`x,y,w,h,fit,aspect,z`, then `,p` for a pane that
    /// keeps its picture's shape).
    pub fn encode(&self) -> String {
        let keep = if self.keep_picture { ",p" } else { "" };
        format!("{},{},{},{},{},{},{}{keep}", self.rect.x, self.rect.y, self.rect.w, self.rect.h, self.fit.as_str(), self.aspect.label(), self.z)
    }

    pub fn decode(s: &str) -> Option<PaneSlot> {
        let parts: Vec<&str> = s.split(',').collect();
        if parts.len() < 4 || parts.len() > 8 {
            return None;
        }
        let x: u16 = parts[0].parse().ok()?;
        let y: u16 = parts[1].parse().ok()?;
        let w: u16 = parts[2].parse().ok()?;
        let h: u16 = parts[3].parse().ok()?;
        let fit = match parts.get(4) {
            Some(s) => PaneFit::parse(s)?,
            None => PaneFit::default(),
        };
        let aspect = match parts.get(5) {
            Some(s) => VirtualAspect::parse(s)?,
            None => VirtualAspect::default(),
        };
        let z = match parts.get(6) {
            Some(s) => s.parse::<u8>().ok()?,
            None => 0,
        };
        let keep_picture = match parts.get(7) {
            Some(&"p") => true,
            Some(_) => return None,
            None => false,
        };
        Some(PaneSlot { rect: NormRect::clamped(x, y, w, h), fit, aspect, z, keep_picture })
    }
}

/// How the screen is shared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutPreset {
    /// One copy, the whole screen.
    Single,
    /// Side by side, left to right.
    Horizontal,
    /// One above the other, top to bottom.
    Vertical,
    /// Quarters (more than four copies: the smallest grid that holds them).
    Grid2x2,
    /// Two side by side above, the rest wide underneath (3 copies: 2 + 1).
    TopTwo,
    /// One wide above, the rest side by side underneath (3 copies: 1 + 2).
    BottomTwo,
    /// Resolution-independent custom pane slots (with fit, virtual aspect, and Z-order).
    Slots(Vec<PaneSlot>),
    /// Legacy raw-pixel custom panes, in order.
    Custom(Vec<PaneRect>),
}

/// `total` cut into `n` parts that add up to it exactly: part `i`'s start and length.
fn split(total: u32, n: u32, i: u32) -> (i32, u32) {
    let at = |k: u32| (u64::from(total) * u64::from(k) / u64::from(n.max(1))) as u32;
    (at(i) as i32, at(i + 1) - at(i))
}

/// A `cols` by `rows` grid's first `count` cells, row by row.
fn grid(w: u32, h: u32, cols: u32, rows: u32, count: usize) -> Vec<PaneRect> {
    (0..count as u32)
        .map(|i| {
            let (x, width) = split(w, cols, i % cols);
            let (y, height) = split(h, rows, i / cols);
            PaneRect { x, y, width, height }
        })
        .collect()
}

/// Two horizontal halves: `top_n` copies across the top, the remaining copies across the
/// bottom (when `count <= top_n`, all on one row at full height).
fn two_rows(w: u32, h: u32, top_n: u32, count: usize) -> Vec<PaneRect> {
    let n = count as u32;
    if n == 0 {
        return Vec::new();
    }
    if n <= top_n {
        return grid(w, h, n, 1, count);
    }
    let (y0, h0) = split(h, 2, 0);
    let (y1, h1) = split(h, 2, 1);
    let bot_n = n - top_n;
    let mut out = Vec::with_capacity(count);
    for i in 0..top_n {
        let (x, width) = split(w, top_n, i);
        out.push(PaneRect { x, y: y0, width, height: h0 });
    }
    for j in 0..bot_n {
        let (x, width) = split(w, bot_n, j);
        out.push(PaneRect { x, y: y1, width, height: h1 });
    }
    out
}

/// `NORM_SCALE` cut into `n` parts that add up to it exactly.
fn split_norm(n: u32, i: u32) -> (u16, u16) {
    let at = |k: u32| (u32::from(NORM_SCALE) * k / n.max(1)) as u16;
    (at(i), at(i + 1) - at(i))
}

/// Normalized `cols` by `rows` grid slots.
fn grid_slots(cols: u32, rows: u32, count: usize) -> Vec<PaneSlot> {
    (0..count as u32)
        .map(|i| {
            let (x, w) = split_norm(cols, i % cols);
            let (y, h) = split_norm(rows, i / cols);
            PaneSlot::new(NormRect { x, y, w, h })
        })
        .collect()
}

/// Normalized two-row layout (`top_n` on top, the rest underneath).
fn two_rows_slots(top_n: u32, count: usize) -> Vec<PaneSlot> {
    let n = count as u32;
    if n == 0 {
        return Vec::new();
    }
    if n <= top_n {
        return grid_slots(n, 1, count);
    }
    let (y0, h0) = split_norm(2, 0);
    let (y1, h1) = split_norm(2, 1);
    let bot_n = n - top_n;
    let mut out = Vec::with_capacity(count);
    for i in 0..top_n {
        let (x, w) = split_norm(top_n, i);
        out.push(PaneSlot::new(NormRect { x, y: y0, w, h: h0 }));
    }
    for j in 0..bot_n {
        let (x, w) = split_norm(bot_n, j);
        out.push(PaneSlot::new(NormRect { x, y: y1, w, h: h1 }));
    }
    out
}

impl LayoutPreset {
    /// Picture-in-Picture slots: player 1 full screen (`z = 0`), players 2+ stacked in
    /// neat corner inset windows (`z = 1+`).
    pub fn pip_slots(count: usize) -> Vec<PaneSlot> {
        let count = count.min(MAX_PANES);
        if count == 0 {
            return Vec::new();
        }
        let mut out = vec![PaneSlot::new(NormRect::FULL)];
        let inset_w: u16 = 3_200;
        let inset_h: u16 = 3_200;
        let pad: u16 = 250;
        for i in 1..count {
            let col = ((i - 1) / 2) as u16;
            let row = ((i - 1) % 2) as u16;
            let x = NORM_SCALE.saturating_sub(pad + inset_w).saturating_sub(col * (inset_w + pad));
            let y = if row == 0 {
                NORM_SCALE.saturating_sub(pad + inset_h)
            } else {
                pad
            };
            out.push(PaneSlot { rect: NormRect::clamped(x, y, inset_w, inset_h), fit: PaneFit::Fit, aspect: VirtualAspect::MatchPane, z: i.min(255) as u8, keep_picture: false });
        }
        out
    }

    /// Converts this layout into `count` normalized `PaneSlot`s (for live editing, fit,
    /// virtual aspect, and Z-order).
    pub fn to_slots(&self, count: usize) -> Vec<PaneSlot> {
        let count = count.min(MAX_PANES);
        let n = count as u32;
        match self {
            LayoutPreset::Single => vec![PaneSlot::new(NormRect::FULL); count],
            LayoutPreset::Horizontal => grid_slots(n, 1, count),
            LayoutPreset::Vertical => grid_slots(1, n, count),
            LayoutPreset::Grid2x2 => {
                let cols = if count <= 4 { 2 } else { (1..).find(|c: &u32| c * c >= n).unwrap_or(1) };
                let rows = if count <= 4 { 2 } else { n.div_ceil(cols) };
                grid_slots(cols, rows, count)
            }
            LayoutPreset::TopTwo => two_rows_slots(2, count),
            LayoutPreset::BottomTwo => two_rows_slots(1, count),
            LayoutPreset::Slots(slots) => (0..count).map(|i| slots.get(i).copied().unwrap_or_else(|| PaneSlot::new(NormRect::FULL))).collect(),
            LayoutPreset::Custom(_) => vec![PaneSlot::new(NormRect::FULL); count],
        }
    }

    /// Compact shell-safe string encoding for `LayoutPreset::Slots`:
    /// `slots:x,y,w,h,fit,aspect,z;...`
    pub fn format_slots(slots: &[PaneSlot]) -> String {
        let mut s = String::from("slots:");
        for (i, slot) in slots.iter().take(MAX_PANES).enumerate() {
            if i > 0 {
                s.push(';');
            }
            s.push_str(&slot.encode());
        }
        s
    }

    /// Parses `slots:x,y,w,h,fit,aspect,z;...` into a non-empty `Vec<PaneSlot>`.
    pub fn parse_slots(text: &str) -> Option<Vec<PaneSlot>> {
        let rest = text.trim().strip_prefix("slots:")?;
        if rest.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        for item in rest.split(';') {
            if out.len() >= MAX_PANES {
                return None;
            }
            out.push(PaneSlot::decode(item)?);
        }
        (!out.is_empty()).then_some(out)
    }

    /// The panes for `count` copies on a `screen_w` by `screen_h` screen, one each (at
    /// most `MAX_PANES`). A custom layout with fewer panes than copies gives the rest
    /// the whole screen.
    pub fn compute_panes(&self, screen_w: u32, screen_h: u32, count: usize) -> Vec<PaneRect> {
        let count = count.min(MAX_PANES);
        let whole = PaneRect { x: 0, y: 0, width: screen_w, height: screen_h };
        let n = count as u32;
        match self {
            LayoutPreset::Single => vec![whole; count],
            LayoutPreset::Horizontal => grid(screen_w, screen_h, n, 1, count),
            LayoutPreset::Vertical => grid(screen_w, screen_h, 1, n, count),
            LayoutPreset::Grid2x2 => {
                let cols = if count <= 4 { 2 } else { (1..).find(|c: &u32| c * c >= n).unwrap_or(1) };
                let rows = if count <= 4 { 2 } else { n.div_ceil(cols) };
                grid(screen_w, screen_h, cols, rows, count)
            }
            LayoutPreset::TopTwo => two_rows(screen_w, screen_h, 2, count),
            LayoutPreset::BottomTwo => two_rows(screen_w, screen_h, 1, count),
            LayoutPreset::Slots(slots) => (0..count).map(|i| slots.get(i).map_or(whole, |s| s.rect.to_pane_rect(screen_w, screen_h))).collect(),
            LayoutPreset::Custom(panes) => (0..count).map(|i| panes.get(i).copied().unwrap_or(whole)).collect(),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            LayoutPreset::Single => "single",
            LayoutPreset::Horizontal => "horizontal",
            LayoutPreset::Vertical => "vertical",
            LayoutPreset::Grid2x2 => "grid2x2",
            LayoutPreset::TopTwo => "top2",
            LayoutPreset::BottomTwo => "bottom2",
            LayoutPreset::Slots(_) => "slots",
            LayoutPreset::Custom(_) => "custom",
        }
    }

    pub fn to_json(&self) -> Json {
        let mut pairs = vec![("kind", Json::str(self.as_str()))];
        match self {
            LayoutPreset::Slots(slots) => pairs.push(("slots", Json::Arr(slots.iter().map(PaneSlot::to_json).collect()))),
            LayoutPreset::Custom(panes) => pairs.push(("panes", Json::Arr(panes.iter().map(PaneRect::to_json).collect()))),
            _ => {}
        }
        Json::obj(pairs)
    }

    pub fn from_json(v: &Json) -> Option<LayoutPreset> {
        Some(match v.get("kind").as_str()? {
            "single" => LayoutPreset::Single,
            "horizontal" => LayoutPreset::Horizontal,
            "vertical" => LayoutPreset::Vertical,
            "grid2x2" => LayoutPreset::Grid2x2,
            "top2" => LayoutPreset::TopTwo,
            "bottom2" => LayoutPreset::BottomTwo,
            "slots" => LayoutPreset::Slots(v.get("slots").as_arr().iter().take(MAX_PANES).map(PaneSlot::from_json).collect::<Option<_>>()?),
            "custom" => LayoutPreset::Custom(v.get("panes").as_arr().iter().take(MAX_PANES).map(PaneRect::from_json).collect::<Option<_>>()?),
            _ => return None,
        })
    }
}

/// A kind of input device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputDeviceKind {
    Controller,
    KeyboardMouse,
    Wheel,
}

/// `InputAssignment::KeyboardMouse`'s device for every keyboard and mouse together (a
/// split-screen player who plays with the keyboard and mouse, not one of several).
pub const ANY_KEYBOARD: u64 = 0;

/// Which input reaches a copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputAssignment {
    /// Everything (one copy, or a player who wants it so).
    All,
    /// Nothing (a copy that's only watched).
    None,
    /// One controller, by its index.
    Controller { index: u32 },
    /// A keyboard and mouse, by device id; `ANY_KEYBOARD`: the PC's keyboards and mice
    /// together, while the copy's pane has the focus.
    KeyboardMouse { device_id: u64 },
    /// A wheel, by device id.
    Wheel { device_id: u64 },
}

impl InputAssignment {
    /// The kind of device it names (None for all or none).
    pub fn kind(&self) -> Option<InputDeviceKind> {
        match self {
            InputAssignment::All | InputAssignment::None => None,
            InputAssignment::Controller { .. } => Some(InputDeviceKind::Controller),
            InputAssignment::KeyboardMouse { .. } => Some(InputDeviceKind::KeyboardMouse),
            InputAssignment::Wheel { .. } => Some(InputDeviceKind::Wheel),
        }
    }

    pub fn to_json(&self) -> Json {
        match self {
            InputAssignment::All => Json::obj(vec![("kind", Json::str("all"))]),
            InputAssignment::None => Json::obj(vec![("kind", Json::str("none"))]),
            InputAssignment::Controller { index } => Json::obj(vec![("kind", Json::str("controller")), ("index", Json::u64((*index).into()))]),
            InputAssignment::KeyboardMouse { device_id } => Json::obj(vec![("kind", Json::str("keyboard_mouse")), ("device_id", Json::u64(*device_id))]),
            InputAssignment::Wheel { device_id } => Json::obj(vec![("kind", Json::str("wheel")), ("device_id", Json::u64(*device_id))]),
        }
    }

    pub fn from_json(v: &Json) -> Option<InputAssignment> {
        let id = || v.get("device_id").as_u64();
        Some(match v.get("kind").as_str()? {
            "all" => InputAssignment::All,
            "none" => InputAssignment::None,
            "controller" => InputAssignment::Controller { index: v.get("index").as_u64().and_then(|n| u32::try_from(n).ok())? },
            "keyboard_mouse" => InputAssignment::KeyboardMouse { device_id: id()? },
            "wheel" => InputAssignment::Wheel { device_id: id()? },
            _ => return None,
        })
    }
}

/// One copy's place in a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceSpec {
    pub id: InstanceId,
    /// Its player slot on this PC (0: the first player's identity and saves).
    pub slot: u32,
    /// A name to play under instead of the slot's own.
    pub identity_override: Option<String>,
    /// Where its window goes; None leaves the window alone.
    pub pane: Option<PaneRect>,
    /// The screen size the game is told it has (None: the real one).
    pub virtual_width: Option<u32>,
    pub virtual_height: Option<u32>,
    /// The DPI the game is told (None: the real one).
    pub dpi: Option<u32>,
    /// The game is told it has the focus while another copy has it.
    pub focus_spoof: bool,
    /// Its frames are captured for a compositor (`shm::FLAG_COMPOSITOR_OUT`).
    pub compositor_capture: bool,
    pub input_assignment: InputAssignment,
}

impl InstanceSpec {
    /// A copy that's alone on the screen: no pane, no spoofing, all input.
    pub fn solo(id: InstanceId, slot: u32) -> InstanceSpec {
        InstanceSpec {
            id,
            slot,
            identity_override: None,
            pane: None,
            virtual_width: None,
            virtual_height: None,
            dpi: None,
            focus_spoof: false,
            compositor_capture: false,
            input_assignment: InputAssignment::All,
        }
    }

    pub fn to_json(&self) -> Json {
        let opt = |v: Option<u32>| v.map_or(Json::Null, |n| Json::u64(n.into()));
        Json::obj(vec![
            ("id", Json::u64(self.id.0.into())),
            ("slot", Json::u64(self.slot.into())),
            ("identity_override", self.identity_override.as_deref().map_or(Json::Null, Json::str)),
            ("pane", self.pane.as_ref().map_or(Json::Null, PaneRect::to_json)),
            ("virtual_width", opt(self.virtual_width)),
            ("virtual_height", opt(self.virtual_height)),
            ("dpi", opt(self.dpi)),
            ("focus_spoof", Json::Bool(self.focus_spoof)),
            ("compositor_capture", Json::Bool(self.compositor_capture)),
            ("input_assignment", self.input_assignment.to_json()),
        ])
    }

    /// A spec, or None when its id or slot is missing or a field is malformed. Fields an
    /// older launcher didn't write take `solo`'s values.
    pub fn from_json(v: &Json) -> Option<InstanceSpec> {
        let u32_of = |k: &str| v.get(k).as_u64().and_then(|n| u32::try_from(n).ok());
        let opt_u32 = |k: &str| match v.get(k) {
            Json::Null => Some(None),
            x => x.as_u64().and_then(|n| u32::try_from(n).ok()).map(Some),
        };
        let pane = match v.get("pane") {
            Json::Null => None,
            p => Some(PaneRect::from_json(p)?),
        };
        let input_assignment = match v.get("input_assignment") {
            Json::Null => InputAssignment::All,
            a => InputAssignment::from_json(a)?,
        };
        Some(InstanceSpec {
            id: InstanceId(u32_of("id")?),
            slot: u32_of("slot")?,
            identity_override: v.get("identity_override").as_str().map(|s| s.chars().take(MAX_IDENTITY).collect()),
            pane,
            virtual_width: opt_u32("virtual_width")?,
            virtual_height: opt_u32("virtual_height")?,
            dpi: opt_u32("dpi")?,
            focus_spoof: v.get("focus_spoof").as_bool().unwrap_or(false),
            compositor_capture: v.get("compositor_capture").as_bool().unwrap_or(false),
            input_assignment,
        })
    }

    /// The spec in `PEERPARTY_INSTANCE`, if there's a valid one.
    pub fn from_env() -> Option<InstanceSpec> {
        let text = std::env::var(crate::env::INSTANCE).ok()?;
        InstanceSpec::from_json(&Json::parse(&text)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, width: u32, height: u32) -> PaneRect {
        PaneRect { x, y, width, height }
    }

    #[test]
    fn panes_tile_the_screen() {
        assert_eq!(LayoutPreset::Single.compute_panes(1920, 1080, 1), [r(0, 0, 1920, 1080)]);
        assert_eq!(LayoutPreset::Horizontal.compute_panes(1920, 1080, 2), [r(0, 0, 960, 1080), r(960, 0, 960, 1080)]);
        assert_eq!(LayoutPreset::Vertical.compute_panes(1920, 1080, 2), [r(0, 0, 1920, 540), r(0, 540, 1920, 540)]);
        // Odd sizes: the parts still add up to the whole.
        let three = LayoutPreset::Horizontal.compute_panes(1000, 600, 3);
        assert_eq!(three, [r(0, 0, 333, 600), r(333, 0, 333, 600), r(666, 0, 334, 600)]);
        assert_eq!(
            LayoutPreset::Grid2x2.compute_panes(1920, 1080, 4),
            [r(0, 0, 960, 540), r(960, 0, 960, 540), r(0, 540, 960, 540), r(960, 540, 960, 540)]
        );
        assert_eq!(LayoutPreset::Grid2x2.compute_panes(1920, 1080, 3).len(), 3, "a quarter stays empty");
        assert_eq!(
            LayoutPreset::TopTwo.compute_panes(1920, 1080, 3),
            [r(0, 0, 960, 540), r(960, 0, 960, 540), r(0, 540, 1920, 540)]
        );
        assert_eq!(
            LayoutPreset::BottomTwo.compute_panes(1920, 1080, 3),
            [r(0, 0, 1920, 540), r(0, 540, 960, 540), r(960, 540, 960, 540)]
        );
        assert_eq!(LayoutPreset::TopTwo.compute_panes(1920, 1080, 2), [r(0, 0, 960, 1080), r(960, 0, 960, 1080)]);
        assert_eq!(LayoutPreset::BottomTwo.compute_panes(1920, 1080, 2), [r(0, 0, 1920, 540), r(0, 540, 1920, 540)]);
        let five = LayoutPreset::Grid2x2.compute_panes(900, 900, 5);
        assert_eq!(five.len(), 5);
        assert_eq!(five[4], r(300, 450, 300, 450), "3x2 cells");
        let custom = LayoutPreset::Custom(vec![r(10, 10, 100, 100)]);
        assert_eq!(custom.compute_panes(1920, 1080, 2), [r(10, 10, 100, 100), r(0, 0, 1920, 1080)]);
        assert!(LayoutPreset::Horizontal.compute_panes(1920, 1080, 0).is_empty());
        assert_eq!(LayoutPreset::Horizontal.compute_panes(1920, 1080, 1000).len(), MAX_PANES);
        assert_eq!(LayoutPreset::Vertical.compute_panes(0, 0, 2), [r(0, 0, 0, 0), r(0, 0, 0, 0)]);
        for l in [LayoutPreset::Single, LayoutPreset::Horizontal, LayoutPreset::Vertical, LayoutPreset::Grid2x2, LayoutPreset::TopTwo, LayoutPreset::BottomTwo, custom] {
            assert_eq!(LayoutPreset::from_json(&l.to_json()), Some(l));
        }
        assert_eq!(LayoutPreset::from_json(&Json::parse(r#"{"kind":"spiral"}"#).unwrap()), None);

        // Normalized slots: resolution-independent, zero-seam edge rounding, fit/aspect/z.
        let slots = vec![
            PaneSlot { rect: NormRect::clamped(0, 0, 6000, 10_000), fit: PaneFit::Fill, aspect: VirtualAspect::Ratio { num: 16, den: 9 }, z: 0, keep_picture: false },
            PaneSlot { rect: NormRect::clamped(6000, 0, 4000, 10_000), fit: PaneFit::Fit, aspect: VirtualAspect::MatchPane, z: 1, keep_picture: true },
        ];
        let preset = LayoutPreset::Slots(slots.clone());
        let p = preset.compute_panes(1920, 1080, 2);
        assert_eq!(p[0].x + p[0].width as i32, p[1].x, "adjacent panes share exact boundary");
        assert_eq!(p[0].width + p[1].width, 1920);
        assert_eq!(LayoutPreset::from_json(&preset.to_json()), Some(preset));
        let wire = LayoutPreset::format_slots(&slots);
        assert_eq!(LayoutPreset::parse_slots(&wire), Some(slots));
        assert_eq!(LayoutPreset::parse_slots("slots:"), None);
        assert_eq!(LayoutPreset::parse_slots("bad"), None);
        let pip = LayoutPreset::pip_slots(3);
        assert_eq!(pip.len(), 3);
        assert_eq!(pip[0].rect, NormRect::FULL);
        assert!(pip[1].z > pip[0].z && pip[2].z > pip[1].z);
        // VirtualAspect sizing: 16:9 inside a tall 960x1080 pane fits to 960x540, and
        // in Fill mode covers up to monitor width (1920x1080).
        let tall = r(0, 0, 960, 1080);
        let aspect169 = VirtualAspect::Ratio { num: 16, den: 9 };
        assert_eq!(aspect169.size_for(tall, PaneFit::Fit, 1920, 1080), (960, 540));
        assert_eq!(aspect169.size_for(tall, PaneFit::Fill, 1920, 1080), (1920, 1080));
        assert_eq!(scale_dimensions(1920, 1080, 100), (1920, 1080));
        assert_eq!(scale_dimensions(1920, 1080, 0), (1920, 1080));
        assert_eq!(scale_dimensions(1920, 1080, 75), (1440, 810));
        assert_eq!(scale_dimensions(960, 1080, 50), (480, 540));
        assert_eq!(scale_dimensions(960, 540, 67), (642, 362));
        assert_eq!(scale_dimensions(300, 180, 50), (300, 180), "never shrinks below safe floor when already small");
    }

    #[test]
    fn specs_round_trip_and_read_forgivingly() {
        let mut spec = InstanceSpec::solo(InstanceId(1), 1);
        assert_eq!(InstanceSpec::from_json(&spec.to_json()), Some(spec.clone()));
        spec.identity_override = Some("Player Two".into());
        spec.pane = Some(r(-1920, 0, 960, 1080));
        spec.virtual_width = Some(960);
        spec.virtual_height = Some(1080);
        spec.dpi = Some(144);
        spec.focus_spoof = true;
        spec.compositor_capture = true;
        for input in [
            InputAssignment::None,
            InputAssignment::Controller { index: 3 },
            InputAssignment::KeyboardMouse { device_id: u64::MAX },
            InputAssignment::Wheel { device_id: 76561198012345679 },
        ] {
            spec.input_assignment = input;
            let text = spec.to_json().to_string();
            assert_eq!(InstanceSpec::from_json(&Json::parse(&text).unwrap()), Some(spec.clone()), "{text}");
        }
        assert_eq!(InputAssignment::Wheel { device_id: 1 }.kind(), Some(InputDeviceKind::Wheel));
        assert_eq!(InputAssignment::All.kind(), None);
        // The least a launcher writes.
        assert_eq!(InstanceSpec::from_json(&Json::parse(r#"{"id":2,"slot":0}"#).unwrap()), Some(InstanceSpec::solo(InstanceId(2), 0)));
        for bad in [r#"{"slot":0}"#, r#"{"id":1,"slot":-1}"#, r#"{"id":1,"slot":0,"dpi":"x"}"#, r#"{"id":1,"slot":0,"pane":{"x":0}}"#, r#"{"id":1,"slot":0,"input_assignment":{"kind":"wheel"}}"#, r#"{"id":4294967296,"slot":0}"#] {
            assert_eq!(InstanceSpec::from_json(&Json::parse(bad).unwrap()), None, "{bad}");
        }
        let long = format!(r#"{{"id":1,"slot":0,"identity_override":"{}"}}"#, "x".repeat(10_000));
        assert_eq!(InstanceSpec::from_json(&Json::parse(&long).unwrap()).unwrap().identity_override.unwrap().len(), MAX_IDENTITY);
    }
}
