//! The overlay host's Windows-only parts, as they are off Windows: there the game is a
//! Windows program under Wine, usually inside Gamescope, so the overlay is drawn inside
//! the game's frames (frame.rs) and worked with controllers (pad.rs, from evdev). Nothing
//! here is needed for that:
//!
//! - `input`: Windows' low-level keyboard and mouse hooks. None here: no keys or mouse
//!   come in, and the opening hotkeys are left to the game.
//! - `window`: the layered window over the game. None can show over a game in Gamescope.
//! - `gamebar`: Windows' Game Bar. There's none.

pub mod input {
    /// What the host takes from the hooks on Windows; matched there, never made here.
    #[allow(dead_code)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Input {
        Key { vk: u32, down: bool },
        Move { dx: i32, dy: i32 },
        Button { left: bool, down: bool },
        Wheel(f32),
    }

    pub fn heartbeat() {}

    pub fn start() {}

    pub fn capture(_on: bool) {}

    pub fn watch_hotkeys(_vks: &[u32]) {}

    pub fn take_hotkey() -> Option<u32> {
        None
    }

    pub fn take() -> Vec<Input> {
        Vec::new()
    }

    pub fn pending() -> bool {
        false
    }

    pub fn alt_down() -> bool {
        false
    }
}

pub mod window {
    use crate::host::raster::Canvas;

    /// Never made (`new` is None): the overlay goes into the game's frames instead.
    #[allow(dead_code)]
    pub struct Layered(());

    impl Layered {
        pub fn new() -> Option<Layered> {
            None
        }

        pub fn present(&mut self, _canvas: &Canvas, _x: i32, _y: i32, _dirty: &[[i32; 4]]) {}

        pub fn hide(&mut self) {}
    }
}

pub mod gamebar {
    use crate::host::desktop::Rect;

    pub struct GameBar;

    impl GameBar {
        pub fn new() -> GameBar {
            GameBar
        }

        pub fn open(&mut self, _area: Option<Rect>) -> bool {
            false
        }
    }
}
