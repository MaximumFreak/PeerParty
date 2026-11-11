//! Update check and update from the GitHub releases.
//!
//! Once a day (Settings > About, "Check for updates", on to begin with) the launcher
//! window asks GitHub's API for the latest release, over WinHTTP. A newer one is
//! announced in the window. Nothing installs by itself, and nothing installs while a
//! game runs through PeerParty.
//!
//! PeerParty never downloads a program itself: an app that fetches an exe and starts it
//! is what antivirus heuristics call a downloader. "Download" opens the release's
//! `peerparty.exe` in the browser, which scans and saves it like any download. The
//! window then finds it in the Downloads folder (or is shown where it was saved) and
//! checks it is a program with the release's size and SHA-256:
//!
//! - the SHA-256 GitHub publishes for the asset (the `digest` in the releases API, read
//!   over HTTPS), or, for a release without one,
//! - the one in a plain checksum file published beside it (`peerparty.exe.sha256`,
//!   `peerparty-release.txt` or `SHA256SUMS`, core/src/release.rs), fetched from GitHub
//!   over HTTPS.
//!
//! A release with neither is announced but not installed from the window, which says so.
//! There is no signing key: the check is that the file is the one published in the
//! project's GitHub release.
//!
//! On "Install and restart" it renames the running exe to `peerparty.exe.old` (Windows
//! allows renaming a running exe, not overwriting it), copies the new one into its place
//! and starts it with `--updated`, putting the old one back if any step fails. The new
//! process deletes the old file.
//!
//! A copy that came with another app's add-on (an `extension.yaml` beside it) is the
//! add-on's to update: the window only says that a newer PeerParty is out.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use peerparty_core::release;

use crate::t;

/// The project's repository on GitHub.
const REPO: &str = "MaximumFreak/PeerParty";
/// GitHub's API, kept apart from its paths: the release check would take the host
/// followed by `/repos` for a link to a repository named "repos".
const API: &str = "https://api.github.com";
/// The release asset that is PeerParty.
const ASSET: &str = "peerparty.exe";
/// Plain checksum files a release may carry beside the exe, for when GitHub has no
/// digest for it; the first one present is used.
const CHECKSUM_FILES: [&str; 4] = ["peerparty.exe.sha256", "peerparty-release.txt", "SHA256SUMS", "SHA256SUMS.txt"];
/// How often the window asks GitHub.
pub const EVERY: Duration = Duration::from_secs(24 * 3600);
/// How long to watch the Downloads folder after "Download".
const WATCH_FOR: Duration = Duration::from_secs(30 * 60);

/// The releases page, with each release's notes.
pub fn releases_page() -> String {
    format!("https://github.com/{REPO}/releases")
}

fn latest_url() -> String {
    format!("{API}/repos/{REPO}/releases/latest")
}

/// What the download must be.
#[derive(Clone, Debug, PartialEq)]
struct Expected {
    sha256: [u8; 32],
    /// GitHub's size for the asset, when it gave one.
    size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    /// Without the leading `v`.
    pub version: String,
    /// The release's page, with its notes.
    pub page: String,
    url: String,
    /// `None`: it can't be checked, so it isn't installed from here; `why` says why
    /// (no `why`: the release publishes no SHA-256 for the exe).
    expected: Option<Expected>,
    /// Already in the player's language.
    pub why: Option<String>,
}

impl Release {
    /// Whether the window can download, check and install it.
    pub fn installable(&self) -> bool {
        self.expected.is_some()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Checking,
    UpToDate,
    Available(Release),
    /// The browser is downloading it; watching the Downloads folder.
    Waiting(Release),
    /// Downloaded and checked, ready to install.
    Ready(Release, PathBuf),
    /// The new exe is in place and starting; this window is closing.
    Restarting,
    /// What went wrong, in the player's language.
    Failed(String),
}

fn state() -> &'static Mutex<Option<Status>> {
    static S: OnceLock<Mutex<Option<Status>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// The last check's result, or `None` before any.
pub fn status() -> Option<Status> {
    state().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set(s: Status) {
    *state().lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
}

/// Whether an update is under way (a check, or between "Download" and the restart).
pub fn busy() -> bool {
    matches!(status(), Some(Status::Checking | Status::Waiting(_) | Status::Ready(..) | Status::Restarting))
}

/// A newer release found by the last check.
pub fn available() -> Option<Release> {
    match status() {
        Some(Status::Available(r) | Status::Waiting(r) | Status::Ready(r, _)) => Some(r),
        _ => None,
    }
}

/// This copy came with another app's add-on, which updates it.
pub fn in_addon() -> bool {
    std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("extension.yaml").is_file())).unwrap_or(false)
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A newer release, for the window's preview pictures.
#[cfg(test)]
pub fn preview() {
    let expected = Some(Expected { sha256: [0; 32], size: Some(1) });
    set(Status::Available(Release { version: "0.2.0".into(), page: releases_page(), url: String::new(), expected, why: None }));
}

/// Asks GitHub on its own thread; `wake` runs when the answer is in [`status`].
pub fn check_in_background(wake: impl Fn() + Send + 'static) {
    if busy() {
        return;
    }
    set(Status::Checking);
    std::thread::spawn(move || {
        set(match latest() {
            Ok(r) if newer(&r.version, env!("CARGO_PKG_VERSION")) => Status::Available(r),
            Ok(_) => Status::UpToDate,
            Err(e) => Status::Failed(t!("update.failed", why = e)),
        });
        wake();
    });
}

fn latest() -> Result<Release, String> {
    let body = http::get(&latest_url(), "application/vnd.github+json", 1 << 20).map_err(|e| t!("update.err_reach", error = e))?;
    let json: serde_json::Value = serde_json::from_slice(&body).map_err(|_| t!("update.err_answer"))?;
    let (mut r, sums) = parse(&json).ok_or_else(|| t!("update.err_no_release"))?;
    if r.expected.is_some() || !newer(&r.version, env!("CARGO_PKG_VERSION")) {
        return Ok(r);
    }
    // No digest from GitHub: a newer release's plain checksum file, if it has one (a small
    // text file, never a program).
    if let Some((url, size)) = sums {
        match http::get(&url, "application/octet-stream", 64 * 1024) {
            Err(e) => r.why = Some(t!("update.err_sum_fetch", error = e)),
            Ok(text) => match release::read_checksum(&String::from_utf8_lossy(&text), ASSET) {
                Some((sha256, listed)) if listed.is_none_or(|l| size.is_none_or(|s| s == l)) => r.expected = Some(Expected { sha256, size: size.or(listed) }),
                _ => r.why = Some(t!("update.err_sum_bad").to_string()),
            },
        }
    }
    Ok(r)
}

/// The release in GitHub's answer, and when GitHub has no digest for the exe, the
/// address of a checksum file published beside it and the exe's size.
fn parse(v: &serde_json::Value) -> Option<(Release, Option<(String, Option<u64>)>)> {
    let version = v["tag_name"].as_str()?.trim_start_matches('v').to_string();
    let page = v["html_url"].as_str().map(str::to_string).unwrap_or_else(releases_page);
    let assets = v["assets"].as_array()?;
    let named = |name: &str| assets.iter().find(|a| a["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)));
    let asset = named(ASSET)?;
    let url = asset["browser_download_url"].as_str()?.to_string();
    let size = asset["size"].as_u64().filter(|&s| s > 0);
    let digest = asset["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).and_then(release::unhex::<32>);
    let sums = CHECKSUM_FILES.iter().find_map(|n| named(n)).and_then(|a| a["browser_download_url"].as_str()).map(str::to_string);
    // Downloads come from GitHub only.
    let ok = |u: &str| u.starts_with("https://github.com/");
    if !ok(&url) || sums.as_deref().is_some_and(|m| !ok(m)) {
        return None;
    }
    let expected = digest.map(|sha256| Expected { sha256, size });
    let sums = if expected.is_none() { sums.map(|u| (u, size)) } else { None };
    Some((Release { version, page, url, expected, why: None }, sums))
}

fn parts(v: &str) -> Vec<u32> {
    v.split(|c: char| !c.is_ascii_digit()).take_while(|p| !p.is_empty()).map(|p| p.parse().unwrap_or(0)).collect()
}

/// Whether version `a` is newer than `b` ("0.1.10" > "0.1.9").
pub fn newer(a: &str, b: &str) -> bool {
    parts(a) > parts(b)
}

/// Where browsers save downloads: the user's Downloads folder.
fn downloads() -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath};
    let mut p = std::ptr::null_mut();
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, 0, std::ptr::null_mut(), &mut p) };
    let dir = if hr == 0 && !p.is_null() {
        let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
        Some(PathBuf::from(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })))
    } else {
        std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Downloads"))
    };
    unsafe { CoTaskMemFree(p as _) };
    dir
}

/// Whether `file` is `r`'s exe: a program with the release's SHA-256, and its size when
/// GitHub gave one.
fn verify(r: &Release, file: &Path) -> Result<(), String> {
    let want = r.expected.as_ref().ok_or_else(|| r.why.clone().unwrap_or_else(|| t!("update.cant_check").to_string()))?;
    let mut head = [0u8; 2];
    let is_program = std::fs::File::open(file).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head)).is_ok() && head == *b"MZ";
    let (sha, size) = release::sha256_file(file).map_err(|e| format!("{}: {e}", file.display()))?;
    if !is_program || want.size.is_some_and(|s| s != size) || sha != want.sha256 {
        return Err(t!("update.err_mismatch", file = file.display(), version = r.version));
    }
    Ok(())
}

/// A finished, checked download of `r` in the Downloads folder (the browser may have
/// named it `peerparty (1).exe`).
fn find_download(r: &Release) -> Option<PathBuf> {
    let want = r.expected.as_ref()?;
    std::fs::read_dir(downloads()?).ok()?.flatten().map(|e| e.path()).find(|p| {
        let name = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        name.starts_with("peerparty") && name.ends_with(".exe") && std::fs::metadata(p).is_ok_and(|md| want.size.is_none_or(|s| s == md.len())) && verify(r, p).is_ok()
    })
}

/// Start getting `r`: `open` has the browser download it, and this watches the
/// Downloads folder until the file is there and checks out. A copy downloaded earlier
/// is used as it is.
pub fn download(r: Release, open: impl FnOnce(&str), wake: impl Fn() + Send + 'static) {
    if !r.installable() || matches!(status(), Some(Status::Waiting(_) | Status::Ready(..) | Status::Restarting)) {
        return;
    }
    if let Some(f) = find_download(&r) {
        set(Status::Ready(r, f));
        return;
    }
    set(Status::Waiting(r.clone()));
    open(&r.url);
    std::thread::spawn(move || {
        let waiting = || matches!(status(), Some(Status::Waiting(ref w)) if *w == r);
        let until = Instant::now() + WATCH_FOR;
        while Instant::now() < until {
            std::thread::sleep(Duration::from_secs(2));
            // Cancelled, or the file was picked by hand.
            if !waiting() {
                return;
            }
            if let Some(f) = find_download(&r) {
                set(Status::Ready(r, f));
                wake();
                return;
            }
        }
        if waiting() {
            set(Status::Available(r));
            wake();
        }
    });
}

/// The downloaded file, picked by hand (saved somewhere other than Downloads). Checked
/// before it is offered.
pub fn use_file(r: Release, file: PathBuf) -> Result<(), String> {
    verify(&r, &file)?;
    set(Status::Ready(r, file));
    Ok(())
}

/// Stop waiting for the download.
pub fn cancel() {
    if let Some(Status::Waiting(r) | Status::Ready(r, _)) = status() {
        set(Status::Available(r));
    }
}

/// Puts the downloaded `file` in place of the running exe and starts it. On success the
/// window closes; on failure everything is as it was and [`status`] says why.
pub fn install(r: Release, file: PathBuf) -> bool {
    if other_copies().is_some() {
        set(Status::Failed(t!("update.install_failed", why = t!("update.err_games"))));
        return false;
    }
    set(Status::Restarting);
    match swap_in(&r, &file) {
        Ok(()) => true,
        Err(e) => {
            set(Status::Failed(t!("update.install_failed", why = e)));
            false
        }
    }
}

fn swap_in(r: &Release, file: &Path) -> Result<(), String> {
    // Checked again: the file may have changed since it was found.
    verify(r, file)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let old = old_path(&exe);
    let _ = std::fs::remove_file(&old);
    let cant = |e: std::io::Error| t!("update.err_replace", folder = exe.parent().unwrap_or(Path::new("")).display(), error = e, file = file.display());
    std::fs::rename(&exe, &old).map_err(cant)?;
    let back = || {
        let _ = std::fs::remove_file(&exe);
        let _ = std::fs::rename(&old, &exe);
    };
    if let Err(e) = std::fs::copy(file, &exe) {
        back();
        return Err(cant(e));
    }
    // The copy must be the checked file, byte for byte.
    if verify(r, &exe).is_err() {
        back();
        return Err(t!("update.err_copy").into());
    }
    if let Err(e) = std::process::Command::new(&exe).arg("--updated").spawn() {
        back();
        return Err(t!("update.err_start", error = e));
    }
    // Used up; left there, the next update's download would be saved as
    // "peerparty (1).exe".
    let _ = std::fs::remove_file(file);
    Ok(())
}

fn old_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

/// The exe an update replaced: deleted once nothing runs from it (the old window may
/// still be closing; a game started from it keeps it until the game ends, and then the
/// next start deletes it).
pub fn clean_up() {
    let Ok(exe) = std::env::current_exe() else { return };
    let old = old_path(&exe);
    if !old.exists() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..30 {
            if std::fs::remove_file(&old).is_ok() || !old.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

/// Other running copies of this peerparty.exe (games started from here, from a game library or
/// from a .bat), if any.
pub fn other_copies() -> Option<usize> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    let me = std::env::current_exe().ok()?;
    let mine = me.to_string_lossy().to_ascii_lowercase();
    let name = me.file_name()?.to_string_lossy().to_ascii_lowercase();
    let own = std::process::id();
    let mut n = 0;
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snap, &mut e) != 0;
        while more {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            if e.th32ProcessID != own && String::from_utf16_lossy(&e.szExeFile[..len]).to_ascii_lowercase() == name {
                let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, e.th32ProcessID);
                if !h.is_null() {
                    let mut buf = [0u16; 1024];
                    let mut l = buf.len() as u32;
                    if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut l) != 0 && String::from_utf16_lossy(&buf[..l as usize]).to_ascii_lowercase() == mine {
                        n += 1;
                    }
                    CloseHandle(h);
                }
            }
            more = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    (n > 0).then_some(n)
}

/// HTTPS GET over WinHTTP (Windows' own client: its proxy settings, its certificate
/// checks), for GitHub's API and a release's small checksum file only.
mod http {
    use std::ffi::c_void;
    use windows_sys::Win32::Networking::WinHttp::*;

    struct H(*mut c_void);
    impl Drop for H {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn err(what: &str) -> String {
        format!("{what}: {}", std::io::Error::last_os_error())
    }

    pub fn get(url: &str, accept: &str, max: usize) -> Result<Vec<u8>, String> {
        let rest = url.strip_prefix("https://").ok_or("only https")?;
        let (host, path) = match rest.split_once('/') {
            Some((h, p)) => (h, format!("/{p}")),
            None => (rest, "/".to_string()),
        };
        unsafe {
            let agent = w(&format!("PeerParty/{}", env!("CARGO_PKG_VERSION")));
            let session = H(WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, std::ptr::null(), std::ptr::null(), 0));
            if session.0.is_null() {
                return Err(err("WinHttpOpen"));
            }
            WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 15_000);
            let conn = H(WinHttpConnect(session.0, w(host).as_ptr(), INTERNET_DEFAULT_HTTPS_PORT as u16, 0));
            if conn.0.is_null() {
                return Err(err("WinHttpConnect"));
            }
            let req = H(WinHttpOpenRequest(conn.0, w("GET").as_ptr(), w(&path).as_ptr(), std::ptr::null(), std::ptr::null(), std::ptr::null(), WINHTTP_FLAG_SECURE));
            if req.0.is_null() {
                return Err(err("WinHttpOpenRequest"));
            }
            let headers = w(&format!("Accept: {accept}\r\nCache-Control: no-cache\r\n"));
            if WinHttpSendRequest(req.0, headers.as_ptr(), u32::MAX, std::ptr::null(), 0, 0, 0) == 0 || WinHttpReceiveResponse(req.0, std::ptr::null_mut()) == 0 {
                return Err(err("the request failed"));
            }
            let mut code = 0u32;
            let mut len = 4u32;
            WinHttpQueryHeaders(req.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, std::ptr::null(), (&mut code as *mut u32).cast(), &mut len, std::ptr::null_mut());
            if code != 200 {
                return Err(format!("HTTP {code}"));
            }
            let mut out = Vec::new();
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let mut n = 0u32;
                if WinHttpReadData(req.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut n) == 0 {
                    return Err(err("reading the answer"));
                }
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n as usize]);
                if out.len() > max {
                    return Err("the answer is too long".into());
                }
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_by_number() {
        assert!(newer("0.1.10", "0.1.9"));
        assert!(newer("0.2.0", "0.1.99"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.0.9", "0.1.0"));
    }

    fn answer() -> serde_json::Value {
        serde_json::json!({
            "tag_name": "v0.2.0",
            "html_url": "https://github.com/MaximumFreak/PeerParty/releases/tag/v0.2.0",
            "assets": [
                {"name": "THIRD-PARTY-NOTICES.md", "browser_download_url": "https://github.com/MaximumFreak/PeerParty/releases/download/v0.2.0/notices", "size": 10},
                {"name": "peerparty.exe", "browser_download_url": "https://github.com/MaximumFreak/PeerParty/releases/download/v0.2.0/peerparty.exe", "size": 5, "digest": format!("sha256:{}", release::hex(&release::sha256(b"MZexe")))},
                {"name": "peerparty.exe.sha256", "browser_download_url": "https://github.com/MaximumFreak/PeerParty/releases/download/v0.2.0/peerparty.exe.sha256", "size": 80}
            ]
        })
    }

    #[test]
    fn github_s_digest_makes_a_release_installable() {
        let (r, sums) = parse(&answer()).unwrap();
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.expected, Some(Expected { sha256: release::sha256(b"MZexe"), size: Some(5) }));
        assert!(r.installable());
        // With GitHub's digest the checksum file isn't needed.
        assert!(sums.is_none());
        // Downloads from anywhere but GitHub are refused.
        let mut v = answer();
        v["assets"][1]["browser_download_url"] = "https://example.com/peerparty.exe".into();
        assert!(parse(&v).is_none());
    }

    #[test]
    fn without_a_digest_the_checksum_file_is_used_or_nothing_installs() {
        let mut v = answer();
        v["assets"][1].as_object_mut().unwrap().remove("digest");
        let (r, sums) = parse(&v).unwrap();
        assert!(!r.installable());
        assert_eq!(sums, Some(("https://github.com/MaximumFreak/PeerParty/releases/download/v0.2.0/peerparty.exe.sha256".to_string(), Some(5))));
        // Neither: announced, not installable, and the reason is the window's own text.
        v["assets"].as_array_mut().unwrap().remove(2);
        let (r, sums) = parse(&v).unwrap();
        assert!(sums.is_none() && !r.installable() && r.why.is_none());
        let dir = std::env::temp_dir().join(format!("peerparty-update-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("peerparty.exe");
        std::fs::write(&f, b"MZexe").unwrap();
        assert!(verify(&r, &f).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_download_must_match_the_release() {
        let dir = std::env::temp_dir().join(format!("peerparty-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("peerparty.exe");
        std::fs::write(&good, b"MZexe").unwrap();
        let (mut r, _) = parse(&answer()).unwrap();
        assert!(verify(&r, &good).is_ok());
        let bad = dir.join("peerparty (1).exe");
        std::fs::write(&bad, b"MZexf").unwrap();
        assert!(verify(&r, &bad).is_err());
        // Not a program, even with the right checksum.
        let text = dir.join("peerparty (2).exe");
        std::fs::write(&text, b"hello").unwrap();
        r.expected = Some(Expected { sha256: release::sha256(b"hello"), size: Some(5) });
        assert!(verify(&r, &text).is_err());
        // The right checksum but another size.
        r.expected = Some(Expected { sha256: release::sha256(b"MZexe"), size: Some(6) });
        assert!(verify(&r, &good).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_old_exe_sits_beside_the_new_one() {
        assert_eq!(old_path(Path::new(r"C:\x\peerparty.exe")), Path::new(r"C:\x\peerparty.exe.old"));
    }
}
