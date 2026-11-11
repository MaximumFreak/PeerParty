//! A `.bat` or `.cmd` given as the game: PeerParty reads it and starts the program it
//! runs, with that program's arguments, folder and variables, rather than running the
//! script (cmd.exe would be the process PeerParty went into, and it isn't the game).
//!
//! Only simple wrappers are read: `@echo off`, comments (`rem`, `::`), `setlocal`,
//! `title`, `cls`, `chcp`, `cd` / `cd /d` / `pushd` / `popd`, `set NAME=value`, `exit`,
//! and one line that runs a program (`game.exe args`, `"path\game.exe" args`,
//! `start "" [/d folder] [/wait] game.exe args`). `%~dp0`, `%0`, `%1`..`%9`, `%*` and
//! `%NAME%` are filled in. Anything else (`if`, `goto`, `call`, a second program, `&`,
//! pipes) is an error that says to start the program itself.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub struct Wrapped {
    pub exe: PathBuf,
    pub args: Vec<OsString>,
    /// The folder the script changed to, if it did.
    pub cwd: Option<PathBuf>,
    /// The variables it set, in order.
    pub vars: Vec<(String, String)>,
}

/// Whether `path` is a script this module reads.
pub fn is_script(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("bat") || e.eq_ignore_ascii_case("cmd"))
}

/// Reads the script at `script` (a full path) and finds what it runs.
pub fn read(script: &Path, args: &[OsString]) -> Result<Wrapped, String> {
    let bytes = std::fs::read(script).map_err(|e| format!("{}: {e}", script.display()))?;
    let text = String::from_utf8(bytes.clone()).unwrap_or_else(|_| bytes.iter().map(|&b| b as char).collect());
    parse(script, &text, args, |p| p.is_file())
}

fn unsupported(script: &Path, n: usize, line: &str) -> String {
    format!(
        "{} line {n}: `{}` is more than PeerParty reads in a .bat or .cmd (only cd, set, comments and one program line); start the game's .exe instead",
        script.display(),
        line.trim()
    )
}

/// Splits a command line the way cmd and the C runtime do for simple cases: spaces
/// separate, double quotes group (and are dropped).
fn split(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut any) = (false, false);
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            ' ' | '\t' if !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

pub fn parse(script: &Path, text: &str, args: &[OsString], exists: impl Fn(&Path) -> bool) -> Result<Wrapped, String> {
    let dir = script.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut cwd: Option<PathBuf> = None;
    let mut stack: Vec<Option<PathBuf>> = Vec::new();
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut found: Option<Wrapped> = None;
    let quoted_args: Vec<String> = args.iter().map(|a| crate::crt_quote(&a.to_string_lossy())).collect();
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.trim().trim_start_matches('@').trim();
        let lower = line.to_ascii_lowercase();
        let first = lower.split_whitespace().next().unwrap_or("");
        if line.is_empty() || line.starts_with("::") || first == "rem" || first == "echo" || first == "echo." || first == "setlocal" || first == "endlocal" || first == "title" || first == "cls" || first == "chcp" {
            continue;
        }
        if first == "exit" {
            break;
        }
        let control = matches!(first, "if" | "goto" | "call" | "for" | "shift" | "choice" | "timeout" | "pause" | "cmd" | "powershell");
        if control || line.contains(['&', '|', '<', '>', '^']) || line.starts_with(':') {
            return Err(unsupported(script, n, raw));
        }
        let line = expand(line, script, &dir, &quoted_args, &vars);
        let here = |p: &str| -> PathBuf {
            let p = Path::new(p.trim().trim_matches('"'));
            if p.is_absolute() { p.to_path_buf() } else { cwd.clone().unwrap_or_else(|| dir.clone()).join(p) }
        };
        match first {
            "cd" | "chdir" | "pushd" => {
                let rest = line[first.len()..].trim();
                let rest = rest.strip_prefix("/d").or_else(|| rest.strip_prefix("/D")).unwrap_or(rest).trim();
                if rest.is_empty() {
                    continue;
                }
                if first == "pushd" {
                    stack.push(cwd.clone());
                }
                cwd = Some(here(rest));
            }
            "popd" => cwd = stack.pop().flatten(),
            "set" => {
                let rest = line[3..].trim().trim_matches('"');
                let Some((k, v)) = rest.split_once('=') else { return Err(unsupported(script, n, raw)) };
                if k.starts_with('/') {
                    return Err(unsupported(script, n, raw));
                }
                vars.push((k.trim().to_string(), v.to_string()));
            }
            _ => {
                if found.is_some() {
                    return Err(unsupported(script, n, raw));
                }
                let mut words = split(&line);
                let mut start_dir = None;
                if words.first().is_some_and(|w| w.eq_ignore_ascii_case("start")) {
                    words.remove(0);
                    // The window title: start's first quoted word.
                    if line[5..].trim_start().starts_with('"') {
                        words.remove(0);
                    }
                    while words.first().is_some_and(|w| w.starts_with('/')) {
                        let w = words.remove(0).to_ascii_lowercase();
                        if w == "/d" && !words.is_empty() {
                            start_dir = Some(here(&words.remove(0)));
                        } else if let Some(d) = w.strip_prefix("/d") {
                            start_dir = Some(here(d));
                        }
                    }
                }
                let Some(program) = words.first() else { return Err(unsupported(script, n, raw)) };
                let base = start_dir.clone().or_else(|| cwd.clone()).unwrap_or_else(|| dir.clone());
                let p = Path::new(program);
                let mut exe = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
                if exe.extension().is_none() {
                    exe.set_extension("exe");
                }
                if !exe.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) || !exists(&exe) {
                    return Err(unsupported(script, n, raw));
                }
                found = Some(Wrapped { exe, args: words[1..].iter().map(OsString::from).collect(), cwd: start_dir.or_else(|| cwd.clone()), vars: Vec::new() });
            }
        }
    }
    let mut w = found.ok_or_else(|| format!("{}: no program to start in it; start the game's .exe instead", script.display()))?;
    w.vars = vars;
    Ok(w)
}

/// Fills in `%~dp0`, `%0`, `%1`..`%9`, `%*`, `%%` and `%NAME%` (the script's own
/// variables first, then ours; an unknown one is empty, as in cmd).
fn expand(line: &str, script: &Path, dir: &Path, args: &[String], vars: &[(String, String)]) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let lower = after.to_ascii_lowercase();
        if let Some(r) = after.strip_prefix('%') {
            out.push('%');
            rest = r;
        } else if lower.starts_with("~dp0") {
            let mut d = dir.to_string_lossy().into_owned();
            if !d.ends_with('\\') {
                d.push('\\');
            }
            out.push_str(&d);
            rest = &after[4..];
        } else if after.starts_with('*') {
            out.push_str(&args.join(" "));
            rest = &after[1..];
        } else if let Some(d) = after.chars().next().and_then(|c| c.to_digit(10)) {
            if d == 0 {
                out.push_str(&script.to_string_lossy());
            } else if let Some(a) = args.get(d as usize - 1) {
                out.push_str(a);
            }
            rest = &after[1..];
        } else if let Some(end) = after.find('%') {
            let name = &after[..end];
            let value = vars.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone()).or_else(|| std::env::var(name).ok()).unwrap_or_default();
            out.push_str(&value);
            rest = &after[end + 1..];
        } else {
            out.push('%');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, args: &[&str]) -> Result<Wrapped, String> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        parse(Path::new(r"D:\Games\Some Game\play.bat"), text, &args, |p| p.extension().is_some_and(|e| e == "exe"))
    }

    #[test]
    fn reads_a_simple_wrapper() {
        let w = run("@echo off\r\nrem start it\r\n:: windowed\r\ncd /d \"%~dp0bin\"\r\nset GAME_MODE=lan\r\nGame.exe -windowed %*\r\nexit /b\r\n", &["-x", "two words"]).unwrap();
        assert_eq!(w.exe, Path::new(r"D:\Games\Some Game\bin\Game.exe"));
        assert_eq!(w.args, ["-windowed", "-x", "two words"].map(OsString::from));
        assert_eq!(w.cwd.as_deref(), Some(Path::new(r"D:\Games\Some Game\bin")));
        assert_eq!(w.vars, [("GAME_MODE".to_string(), "lan".to_string())]);
    }

    #[test]
    fn reads_start_and_quoted_paths() {
        let w = run("start \"\" /d \"%~dp0Binaries\" /wait \"%~dp0Binaries\\Game.exe\" -log", &[]).unwrap();
        assert_eq!(w.exe, Path::new(r"D:\Games\Some Game\Binaries\Game.exe"));
        assert_eq!(w.args, [OsString::from("-log")]);
        assert_eq!(w.cwd.as_deref(), Some(Path::new(r"D:\Games\Some Game\Binaries")));
        let w = run("set EXE=Game\npushd sub\npopd\n%EXE% %1", &["a"]).unwrap();
        assert_eq!(w.exe, Path::new(r"D:\Games\Some Game\Game.exe"));
        assert_eq!(w.args, [OsString::from("a")]);
        assert_eq!(w.cwd, None);
    }

    #[test]
    fn refuses_anything_else() {
        for text in ["if exist x.exe x.exe", "goto start", "Game.exe & Other.exe", "Game.exe\nOther.exe", "call other.bat", "Game.exe > log.txt", "@echo off", "notes.txt"] {
            let e = run(text, &[]).unwrap_err();
            assert!(e.contains(".exe instead"), "{text}: {e}");
        }
    }
}
