//! Everything the launcher window and the overlay's drawing ask of the operating
//! system, behind one set of functions: a port to another system fills in its own file
//! next to `windows.rs` (`other.rs` is the stand-in that compiles anywhere and does
//! nothing system-specific). The game-side parts (injection, the hook, the firewall, the
//! overlay's windows) stay Windows-only for now.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(windows))]
mod other;
#[cfg(not(windows))]
pub use other::*;

use std::path::PathBuf;

/// One face in a font file (a `.ttc` holds several).
#[derive(Clone, Debug)]
pub struct FontFile {
    pub path: PathBuf,
    pub index: u32,
}

/// The interface fonts the system has (any may be missing; egui's own fonts fill in).
#[derive(Clone, Debug, Default)]
pub struct UiFonts {
    pub regular: Option<FontFile>,
    pub semibold: Option<FontFile>,
    /// An icon font with the code points `ui::icons` names.
    pub icons: Option<FontFile>,
}

/// An image's pixels, RGBA, row by row.
pub struct Rgba {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}
