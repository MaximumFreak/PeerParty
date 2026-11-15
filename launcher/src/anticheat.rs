//! The check before a game starts: PeerParty isn't for games with anti-cheat (LEGAL.md).
//! The hook stops a game once an anti-cheat module loads in it (hook/src/watch.rs), but by
//! then PeerParty is in the process the anti-cheat watches. So the launcher looks first
//! and doesn't start, or inject into, a program that is an anti-cheat starter, or one
//! with an anti-cheat module beside it to load at start-up. An anti-cheat installer in a
//! subfolder (often shipped and never run offline) isn't enough: the hook's check stays
//! the backstop for the rest.

use std::path::Path;

/// Whether a file name is an anti-cheat program or module (the hook's `is_anticheat`).
fn is_anticheat(file: &str) -> bool {
    let f = file.to_ascii_lowercase();
    f.contains("easyanticheat")
        || f.contains("start_protected_game")
        || f.contains("battleye")
        || f.starts_with("beclient")
        || f.starts_with("beservice")
        || f.starts_with("bedaisy")
        || f.starts_with("eac_launcher")
}

/// Why `exe` won't be started, when it's anti-cheat's own starter or has an anti-cheat
/// module (a .dll) in its folder.
pub fn refuse(exe: &Path) -> Option<String> {
    let name = exe.file_name()?.to_string_lossy().into_owned();
    if is_anticheat(&name) {
        return Some(name);
    }
    let dir = exe.parent()?;
    let entries = std::fs::read_dir(dir).ok()?;
    entries.flatten().take(5000).find_map(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        let module = n.to_ascii_lowercase().ends_with(".dll");
        (module && is_anticheat(&n) && e.path().is_file()).then_some(n)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anti_cheat_starters_and_modules_beside_the_game_are_refused() {
        let dir = std::env::temp_dir().join(format!("peerparty-anticheat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("EasyAntiCheat")).unwrap();
        std::fs::write(dir.join("Game.exe"), b"MZ").unwrap();
        // An installer in a subfolder alone: the game may run without it.
        std::fs::write(dir.join("EasyAntiCheat").join("EasyAntiCheat_EOS_Setup.exe"), b"MZ").unwrap();
        assert_eq!(refuse(&dir.join("Game.exe")), None);
        assert_eq!(refuse(&dir.join("start_protected_game.exe")).as_deref(), Some("start_protected_game.exe"));
        std::fs::write(dir.join("BEClient_x64.dll"), b"MZ").unwrap();
        assert_eq!(refuse(&dir.join("Game.exe")).as_deref(), Some("BEClient_x64.dll"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
