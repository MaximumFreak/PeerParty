//! Where a stand-in keeps a game's data on this PC: `%LOCALAPPDATA%\peerparty\<sdk>\<game>`
//! (the game being the exe's name, lowercase), and names from a game kept to one
//! ordinary file there. PlayFab's saved player (playfab/api.rs), Unity's Cloud Save and
//! Remote Config (ugs/data.rs), EOS's title and player storage (eos/storage.rs) and
//! Galaxy's stats and storage (galaxy/ext.rs) all live under it. A test names a folder
//! of its own instead with the layer's `PEERPARTY_*_DIR` variable (config/mod.rs).
//!
//! saves.rs keeps a second copy's own saves out of these folders (`peerparty_dir`).

use std::path::PathBuf;

/// `%LOCALAPPDATA%\peerparty`, PeerParty's folder for the player's data, or the folder
/// `PEERPARTY_DATA_DIR` names (tests give each run one of its own).
pub fn peerparty_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os(peerparty_protocol::env::DATA_DIR).map(PathBuf::from).filter(|p| p.is_absolute()) {
        return Some(d);
    }
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("peerparty"))
}

/// `%LOCALAPPDATA%\peerparty\<sdk>`.
pub fn sdk_dir(sdk: &str) -> Option<PathBuf> {
    Some(peerparty_dir()?.join(sdk))
}

/// This game's name for its folders: the exe's file name without `.exe`, lowercase.
pub fn game_name() -> Option<String> {
    Some(std::env::current_exe().ok()?.file_stem()?.to_string_lossy().to_ascii_lowercase())
}

/// `%LOCALAPPDATA%\peerparty\<sdk>\<game>`.
pub fn game_dir(sdk: &str) -> Option<PathBuf> {
    Some(sdk_dir(sdk)?.join(game_name()?))
}

/// A name from a request (an owner, an access class) as one path component: anything
/// but ASCII letters, digits, `-` and `_` becomes `_`.
pub fn component(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// A file name a game gave (a cloud-storage file), as one file in one folder: path
/// separators, `:` and characters Windows refuses become `_`, a trailing dot or space
/// (which Windows drops, so two names would share a file) gets a `_` after it, and a
/// device name (`CON`, `NUL`, `COM1.sav`...), which would open the device instead of a
/// file, gets one in front. Empty, `.` and `..` are no name.
pub fn file_name(name: &str) -> Option<String> {
    let mut n: String = name.chars().map(|c| if c < ' ' || "/\\:<>\"|?*".contains(c) { '_' } else { c }).collect();
    if n.is_empty() || n == "." || n == ".." {
        return None;
    }
    if n.ends_with(['.', ' ']) {
        n.push('_');
    }
    let stem = n.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem[3..].chars().count() == 1
            && stem[3..].chars().all(|c| c.is_ascii_digit() || "¹²³".contains(c)));
    if device {
        n.insert(0, '_');
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_file_names_stay_one_ordinary_file() {
        assert_eq!(file_name("../save.dat").as_deref(), Some(".._save.dat"));
        assert_eq!(file_name(r"a\b:c").as_deref(), Some("a_b_c"));
        assert_eq!(file_name("what?<x>|\"*\u{1}").as_deref(), Some("what__x_____"));
        assert_eq!(file_name("slot1.").as_deref(), Some("slot1._"));
        assert_eq!(file_name("slot1 ").as_deref(), Some("slot1 _"));
        assert_eq!(file_name("CON").as_deref(), Some("_CON"));
        assert_eq!(file_name("nul.sav").as_deref(), Some("_nul.sav"));
        assert_eq!(file_name("com1.dat").as_deref(), Some("_com1.dat"));
        assert_eq!(file_name("LPT¹").as_deref(), Some("_LPT¹"));
        assert_eq!(file_name("console.sav").as_deref(), Some("console.sav"));
        assert_eq!(file_name("com10").as_deref(), Some("com10"));
        assert_eq!(file_name(".."), None);
        assert_eq!(file_name(""), None);
    }

    #[test]
    fn components_and_folders() {
        assert_eq!(component(r"../a b\c:d"), "___a_b_c_d");
        assert_eq!(component("ok-Name_1"), "ok-Name_1");
        let game = game_name().unwrap();
        assert!(!game.is_empty() && game == game.to_ascii_lowercase());
        if let Some(d) = game_dir("ugs") {
            // PeerParty's folder is the test one under cargo (PEERPARTY_DATA_DIR).
            assert_eq!(d, peerparty_dir().unwrap().join("ugs").join(&game));
        }
    }
}
