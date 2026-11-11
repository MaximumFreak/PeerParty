//! Windows' side of `platform`: the registry, program icons, Explorer, the foreground
//! window, the user's languages, the system fonts, the animation setting and process
//! priority.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub use super::Rgba;
use super::{FontFile, UiFonts};
use std::{mem, ptr};

use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject,
    GetDIBits, GetObjectW, HBITMAP,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, RRF_RT_REG_SZ, RegCloseKey,
    RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO, PrivateExtractIconsW};

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

pub enum Hive {
    User,
    Machine,
}

fn root(h: &Hive) -> HKEY {
    match h {
        Hive::User => HKEY_CURRENT_USER,
        Hive::Machine => HKEY_LOCAL_MACHINE,
    }
}

/// A string value. Machine keys are read from the 32-bit view (where launchers write).
pub fn reg_string(hive: Hive, key: &str, value: &str) -> Option<String> {
    let k = wide(key);
    let v = wide(value);
    let mut buf = vec![0u16; 2048];
    let mut len = (buf.len() * 2) as u32;
    let flags = RRF_RT_REG_SZ | if matches!(hive, Hive::Machine) { 0x0002_0000 } else { 0 };
    let rc = unsafe {
        RegGetValueW(root(&hive), k.as_ptr(), v.as_ptr(), flags, ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len)
    };
    if rc != 0 {
        return None;
    }
    let n = (len as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// The names of a key's subkeys.
pub fn reg_subkeys(hive: Hive, key: &str) -> Vec<String> {
    let k = wide(key);
    let mut h: HKEY = ptr::null_mut();
    let access = KEY_READ | if matches!(hive, Hive::Machine) { KEY_WOW64_32KEY } else { 0 };
    if unsafe { RegOpenKeyExW(root(&hive), k.as_ptr(), 0, access, &mut h) } != 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0.. {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        let rc = unsafe {
            RegEnumKeyExW(h, i, name.as_mut_ptr(), &mut len, ptr::null(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut())
        };
        if rc != 0 {
            break;
        }
        out.push(String::from_utf16_lossy(&name[..len as usize]));
    }
    unsafe { RegCloseKey(h) };
    out
}

/// A program's own icon at about `size` pixels.
pub fn exe_icon(exe: &Path, size: i32) -> Option<Rgba> {
    let path = wide(exe);
    let mut icon: HICON = ptr::null_mut();
    let mut id = 0u32;
    let n = unsafe { PrivateExtractIconsW(path.as_ptr(), 0, size, size, &mut icon, &mut id, 1, 0) };
    if n == 0 || n == u32::MAX || icon.is_null() {
        return None;
    }
    let out = unsafe { icon_pixels(icon) };
    unsafe { DestroyIcon(icon) };
    out
}

unsafe fn bitmap_pixels(bmp: HBITMAP, width: i32, height: i32) -> Option<Vec<u8>> {
    unsafe {
        let dc = CreateCompatibleDC(ptr::null_mut());
        let mut info: BITMAPINFO = mem::zeroed();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..mem::zeroed()
        };
        let mut buf = vec![0u8; (width * height * 4) as usize];
        let lines = GetDIBits(dc, bmp, 0, height as u32, buf.as_mut_ptr().cast(), &mut info, DIB_RGB_COLORS);
        DeleteDC(dc);
        (lines == height).then_some(buf)
    }
}

unsafe fn icon_pixels(icon: HICON) -> Option<Rgba> {
    unsafe {
        let mut ii: ICONINFO = mem::zeroed();
        if GetIconInfo(icon, &mut ii) == 0 {
            return None;
        }
        let cleanup = |ii: &ICONINFO| {
            if !ii.hbmColor.is_null() {
                DeleteObject(ii.hbmColor);
            }
            if !ii.hbmMask.is_null() {
                DeleteObject(ii.hbmMask);
            }
        };
        if ii.hbmColor.is_null() {
            cleanup(&ii);
            return None;
        }
        let mut bm: BITMAP = mem::zeroed();
        GetObjectW(ii.hbmColor, mem::size_of::<BITMAP>() as i32, (&mut bm as *mut BITMAP).cast());
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        let Some(mut px) = bitmap_pixels(ii.hbmColor, w, h) else {
            cleanup(&ii);
            return None;
        };
        // Old icons carry no alpha: the mask says what's transparent.
        if px.chunks(4).all(|p| p[3] == 0) {
            let mask = bitmap_pixels(ii.hbmMask, w, h);
            for (i, p) in px.chunks_mut(4).enumerate() {
                let clear = mask.as_ref().is_some_and(|m| m[i * 4] != 0);
                p[3] = if clear { 0 } else { 255 };
            }
        }
        cleanup(&ii);
        for p in px.chunks_mut(4) {
            p.swap(0, 2);
        }
        Some(Rgba {
            width: w as usize,
            height: h as usize,
            pixels: px,
        })
    }
}

/// Shows a folder (or selects a file) in Explorer.
pub fn reveal(path: &Path) {
    let mut cmd = std::process::Command::new("explorer.exe");
    if path.is_file() {
        cmd.arg(format!("/select,{}", path.display()));
    } else {
        cmd.arg(path);
    }
    let _ = cmd.spawn();
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const NORMAL_PRIORITY_CLASS: u32 = 0x0000_0020;

/// Starts a helper (peerparty.exe itself) without a console window, and at normal
/// priority whatever this process runs at (see `background`).
pub fn quiet(cmd: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW | NORMAL_PRIORITY_CLASS)
}

/// This thread's window is the foreground window: Windows gives the keyboard (and so
/// the player's attention) to it, not to a game started from it or anything else.
pub fn in_front() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe {
        let ours = GetActiveWindow();
        !ours.is_null() && GetForegroundWindow() == ours
    }
}

const LAUNCHER_MUTEX: &str = r"Local\peerparty.launcher";

/// Takes the named mutex for the rest of the process (Windows lets go of it at exit),
/// or None when another process holds it. One that can't be made at all counts as
/// taken, so a launcher never refuses to open over it.
fn claim(name: &str) -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let h = unsafe { CreateMutexW(ptr::null(), 0, wide(name).as_ptr()) };
    if !h.is_null() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(h) };
        return None;
    }
    Some(h)
}

/// Only one launcher window runs: a second (the exe opened twice, or a shortcut and
/// the exe) would keep its own copy of the settings and save over the first's games,
/// hidden games and folders. False when another one has it: that window comes to the
/// front instead.
///
/// `after_update`: the window that put this exe in place (update.rs) started it and is
/// closing, still holding the mutex. It is waited for (10 s at most); otherwise this one
/// would bring that closing window to the front and quit, leaving none.
pub fn first_launcher(after_update: bool) -> bool {
    if claim(LAUNCHER_MUTEX).is_some() {
        return true;
    }
    if after_update {
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if claim(LAUNCHER_MUTEX).is_some() {
                return true;
            }
        }
    }
    // The other may be starting and not have its window yet.
    for _ in 0..30 {
        if let Some(w) = launcher_window() {
            use windows_sys::Win32::UI::WindowsAndMessaging::{IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow};
            unsafe {
                if IsIconic(w) != 0 {
                    ShowWindow(w, SW_RESTORE);
                }
                SetForegroundWindow(w);
            }
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// A top-level window is another launcher's: its title is the launcher's and its
/// process runs an exe of the same name as this one (a browser tab or a folder called
/// PeerParty isn't). The overlay's windows have their own title.
fn is_launcher(title: &str, exe: Option<&Path>, ours: &Path) -> bool {
    title == "PeerParty" && exe.and_then(|e| e.file_name()).is_some_and(|n| ours.file_name().is_some_and(|o| n.eq_ignore_ascii_case(o)))
}

fn launcher_window() -> Option<windows_sys::Win32::Foundation::HWND> {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, GetWindowThreadProcessId};
    struct Find {
        ours: PathBuf,
        found: HWND,
    }
    unsafe extern "system" fn each(w: HWND, l: LPARAM) -> i32 {
        let f = unsafe { &mut *(l as *mut Find) };
        let mut buf = [0u16; 32];
        let n = unsafe { GetWindowTextW(w, buf.as_mut_ptr(), buf.len() as i32) }.max(0) as usize;
        let title = String::from_utf16_lossy(&buf[..n]);
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(w, &mut pid) };
        if pid != std::process::id() && is_launcher(&title, crate::firewall::process_exe(pid).as_deref(), &f.ours) {
            f.found = w;
            return 0;
        }
        1
    }
    let mut f = Find { ours: std::env::current_exe().ok()?, found: ptr::null_mut() };
    unsafe { EnumWindows(Some(each), &mut f as *mut Find as LPARAM) };
    (!f.found.is_null()).then_some(f.found)
}

/// The user's display languages, most preferred first ("de-DE", "en-US").
pub fn languages() -> Vec<String> {
    use windows_sys::Win32::Globalization::{GetUserDefaultLocaleName, GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};
    let mut out = Vec::new();
    let (mut n, mut len) = (0u32, 0u32);
    unsafe {
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut n, ptr::null_mut(), &mut len) != 0 && len > 0 {
            let mut buf = vec![0u16; len as usize];
            if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut n, buf.as_mut_ptr(), &mut len) != 0 {
                out.extend(buf.split(|c| *c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy));
            }
        }
        let mut buf = [0u16; 85];
        let n = GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32);
        if n > 1 {
            out.push(String::from_utf16_lossy(&buf[..n as usize - 1]));
        }
    }
    out
}

fn fonts_dir() -> PathBuf {
    let win = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
    win.join("Fonts")
}

/// The first of these font files that exists, with the face to use in it.
fn first_font(names: &[(&str, u32)]) -> Option<FontFile> {
    let dir = fonts_dir();
    names.iter().map(|(n, i)| (dir.join(n), *i)).find(|(p, _)| p.is_file()).map(|(path, index)| FontFile { path, index })
}

/// The interface fonts: Segoe UI, its semibold, and the Fluent (or MDL2) icon font.
pub fn ui_fonts() -> UiFonts {
    UiFonts {
        regular: first_font(&[("segoeui.ttf", 0)]),
        semibold: first_font(&[("seguisb.ttf", 0), ("segoeuib.ttf", 0)]),
        icons: first_font(&[("SegoeIcons.ttf", 0), ("segmdl2.ttf", 0)]),
    }
}

/// A font for a character the interface fonts lack (another script, a symbol), by
/// Unicode block. `lang` picks between the Chinese, Japanese and Korean fonts.
pub fn fallback_font(c: char, lang: &str) -> Option<FontFile> {
    // The collections' "UI" faces (Yu Gothic UI, Microsoft YaHei UI, JhengHei UI) are
    // drawn to sit on Segoe UI's baseline, so a Latin word among them lines up.
    let cjk: &[(&str, u32)] = match lang {
        "ja" => &[("YuGothM.ttc", 1), ("meiryo.ttc", 2), ("msgothic.ttc", 0), ("msyh.ttc", 1)],
        "ko" => &[("malgun.ttf", 0), ("msyh.ttc", 1)],
        "zh-TW" => &[("msjh.ttc", 1), ("msjh.ttf", 0), ("msyh.ttc", 1)],
        _ => &[("msyh.ttc", 1), ("msyh.ttf", 0), ("simsun.ttc", 0), ("YuGothM.ttc", 1)],
    };
    let names: &[(&str, u32)] = match c as u32 {
        0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7AF => &[("malgun.ttf", 0)],
        0x3040..=0x30FF | 0x31F0..=0x31FF => &[("YuGothM.ttc", 1), ("meiryo.ttc", 2), ("msgothic.ttc", 0)],
        0x2E80..=0x2FDF | 0x3000..=0x303F | 0x3100..=0x312F | 0x3190..=0x31EF | 0x3200..=0x9FFF | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF => cjk,
        0x0E00..=0x0EFF => &[("LeelawUI.ttf", 0), ("leelawad.ttf", 0)],
        0x0900..=0x0DFF => &[("Nirmala.ttc", 0), ("Nirmala.ttf", 0)],
        0x1F000..=0x1FAFF | 0x2600..=0x27BF => &[("seguiemj.ttf", 0), ("seguisym.ttf", 0)],
        _ => &[("seguisym.ttf", 0)],
    };
    first_font(names)
}

/// Windows' "Animation effects" (Settings > Accessibility > Visual effects).
pub fn animations_enabled() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW};
    let mut on = 1i32;
    let ok = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&mut on as *mut i32).cast(), 0) };
    ok == 0 || on != 0
}

/// Background mode: this process's CPU, disk and memory priority drop below a game's.
/// The window takes it while a game it started runs and the window isn't in front.
pub fn background(on: bool) {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, PROCESS_MODE_BACKGROUND_BEGIN, PROCESS_MODE_BACKGROUND_END, SetPriorityClass};
    unsafe { SetPriorityClass(GetCurrentProcess(), if on { PROCESS_MODE_BACKGROUND_BEGIN } else { PROCESS_MODE_BACKGROUND_END }) };
}

/// The name the user signs in to this PC with.
pub fn user_name() -> String {
    std::env::var("USERNAME").unwrap_or_default()
}

/// Where PeerParty's logs go.
pub fn logs_dir() -> PathBuf {
    std::env::temp_dir().join("peerparty").join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The second launcher finds the mutex taken; once the first is gone it's free.
    #[test]
    fn a_second_launcher_finds_the_first() {
        let name = format!(r"Local\peerparty.launcher-test-{}", std::process::id());
        let first = claim(&name).expect("free");
        assert!(!first.is_null());
        assert!(claim(&name).is_none());
        unsafe { windows_sys::Win32::Foundation::CloseHandle(first) };
        let again = claim(&name).expect("free again");
        unsafe { windows_sys::Win32::Foundation::CloseHandle(again) };
    }

    #[test]
    fn only_a_launcher_window_is_brought_up() {
        let ours = Path::new(r"C:\Apps\PeerParty\peerparty.exe");
        assert!(is_launcher("PeerParty", Some(Path::new(r"D:\Other\PEERPARTY.EXE")), ours));
        assert!(!is_launcher("PeerParty", Some(Path::new(r"C:\Browser\browser.exe")), ours));
        assert!(!is_launcher("PeerParty", None, ours));
        assert!(!is_launcher("PeerParty overlay", Some(ours), ours));
    }
}