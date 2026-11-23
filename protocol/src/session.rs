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
    /// The player's own panes, in order.
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

impl LayoutPreset {
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
            LayoutPreset::Custom(panes) => (0..count).map(|i| panes.get(i).copied().unwrap_or(whole)).collect(),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            LayoutPreset::Single => "single",
            LayoutPreset::Horizontal => "horizontal",
            LayoutPreset::Vertical => "vertical",
            LayoutPreset::Grid2x2 => "grid2x2",
            LayoutPreset::Custom(_) => "custom",
        }
    }

    pub fn to_json(&self) -> Json {
        let mut pairs = vec![("kind", Json::str(self.as_str()))];
        if let LayoutPreset::Custom(panes) = self {
            pairs.push(("panes", Json::Arr(panes.iter().map(PaneRect::to_json).collect())));
        }
        Json::obj(pairs)
    }

    pub fn from_json(v: &Json) -> Option<LayoutPreset> {
        Some(match v.get("kind").as_str()? {
            "single" => LayoutPreset::Single,
            "horizontal" => LayoutPreset::Horizontal,
            "vertical" => LayoutPreset::Vertical,
            "grid2x2" => LayoutPreset::Grid2x2,
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

/// Which input reaches a copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputAssignment {
    /// Everything (one copy, or a player who wants it so).
    All,
    /// Nothing (a copy that's only watched).
    None,
    /// One controller, by its index.
    Controller { index: u32 },
    /// A keyboard and mouse, by device id.
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
        let five = LayoutPreset::Grid2x2.compute_panes(900, 900, 5);
        assert_eq!(five.len(), 5);
        assert_eq!(five[4], r(300, 450, 300, 450), "3x2 cells");
        let custom = LayoutPreset::Custom(vec![r(10, 10, 100, 100)]);
        assert_eq!(custom.compute_panes(1920, 1080, 2), [r(10, 10, 100, 100), r(0, 0, 1920, 1080)]);
        assert!(LayoutPreset::Horizontal.compute_panes(1920, 1080, 0).is_empty());
        assert_eq!(LayoutPreset::Horizontal.compute_panes(1920, 1080, 1000).len(), MAX_PANES);
        assert_eq!(LayoutPreset::Vertical.compute_panes(0, 0, 2), [r(0, 0, 0, 0), r(0, 0, 0, 0)]);
        for l in [LayoutPreset::Single, LayoutPreset::Horizontal, LayoutPreset::Vertical, LayoutPreset::Grid2x2, custom] {
            assert_eq!(LayoutPreset::from_json(&l.to_json()), Some(l));
        }
        assert_eq!(LayoutPreset::from_json(&Json::parse(r#"{"kind":"spiral"}"#).unwrap()), None);
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
