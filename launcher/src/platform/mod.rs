//! Everything the launcher window and the overlay's drawing ask of the operating
//! system, behind one set of functions with the same names on every system:
//! `windows.rs` and `linux.rs` (which other Unix systems use too). Starting and hosting
//! games is `backend`'s; these use it where the two meet (process lookups, the game
//! libraries, Close all).

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
mod linux;
#[cfg(unix)]
pub use linux::*;

use std::path::PathBuf;

/// PeerParty's own folder: the one `PEERPARTY_DATA_DIR` names when it's an absolute path
/// (`peerparty-data` beside the exe, which portable.rs puts there, or a test run's own),
/// else the usual one (`usual_data_dir`). Once this process has moved the files
/// (`portable::switch`), where they went.
pub fn data_dir() -> PathBuf {
    crate::portable::moved_to()
        .or_else(|| std::env::var_os(peerparty_protocol::env::DATA_DIR).map(PathBuf::from).filter(|p| p.is_absolute()))
        .unwrap_or_else(usual_data_dir)
}

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
