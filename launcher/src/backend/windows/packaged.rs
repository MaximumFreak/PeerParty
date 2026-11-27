//! Microsoft Store / Xbox app (GDK) games. They're MSIX packages: started like any
//! program they fail or quit, because they need their package identity, and their
//! package's entry point is `GameLaunchHelper.exe`, which then starts the real game.
//!
//! So PeerParty starts them the documented way for packaged apps:
//!
//! 1. `IPackageDebugSettings::EnableDebugging(<package>, "peerparty.exe --attach", env)`:
//!    Windows will start the package's next process suspended and run that command with
//!    `-p <pid> -tid <thread>` appended, with PeerParty's variables (`env`) in the game's
//!    environment.
//! 2. `IApplicationActivationManager::ActivateApplication(<family>!<app id>)` starts it,
//!    as the Xbox app would. `DisableDebugging` follows at once, so starting the game
//!    any other way afterwards is untouched.
//! 3. `peerparty --attach -p … -tid …` (a second copy of peerparty.exe, run by Windows)
//!    injects the hook into the suspended helper and resumes it. The hook follows the
//!    helper into the game it starts (hook/src/child.rs).
//! 4. This copy finds the game's process (the `Executable` MicrosoftGame.config names),
//!    hosts the overlay for it and waits for it to exit.
//!
//! Package names come from the install folder's appxmanifest.xml (Identity, Application
//! Id); the full and family names are computed by Windows (`PackageFullNameFromId`).

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HANDLE;

use super::{Guid, vcall, wide};

pub struct Package {
    pub full_name: String,
    pub aumid: String,
    /// The game's own program (MicrosoftGame.config's Executable), not the helper.
    pub game: PathBuf,
}

/// The value of attribute `name` in an XML start tag: `name="..."`, not the end of
/// another name (`xName="..."`).
pub(super) fn attr(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let mut from = 0;
    while let Some(i) = tag[from..].find(&key).map(|i| i + from) {
        if !tag[..i].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            let rest = &tag[i + key.len()..];
            return rest.find('"').map(|end| rest[..end].to_string());
        }
        from = i + key.len();
    }
    None
}

/// Every start tag `<name ...` in `xml`, up to its `>`. The name ends at whitespace, `/`
/// or `>` (configs break the line after it as often as they put a space), so `<Executable`
/// isn't found in `<ExecutableList>`.
fn tags<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("<{name}");
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(i) = xml[from..].find(&open).map(|i| i + from) {
        from = i + open.len();
        if !xml[from..].starts_with(|c: char| c.is_whitespace() || c == '/' || c == '>') {
            continue;
        }
        let Some(end) = xml[from..].find('>') else { break };
        found.push(&xml[i..from + end]);
    }
    found
}

pub(super) fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    tags(xml, name).into_iter().next()
}

/// The MicrosoftGame.config in a package's folder, if it has one.
pub(super) fn read_config(content: &Path) -> Option<String> {
    ["MicrosoftGame.config", "MicrosoftGame.Config"].iter().find_map(|n| std::fs::read_to_string(content.join(n)).ok())
}

/// The programs a MicrosoftGame.config lists: each Executable's Name (a path in the
/// package's folder) and Id (the appxmanifest Application that starts it), in order.
fn executables(config: &str) -> Vec<(String, Option<String>)> {
    tags(config, "Executable").into_iter().filter_map(|t| Some((attr(t, "Name")?.replace('/', "\\"), attr(t, "Id")))).collect()
}

/// The program a package's library entry starts: the one DesktopRegistration's
/// MultiplayerProtocol names when it lists several (the one other players join), else the
/// first. Any other one is picked like a second program of any game.
pub(super) fn main_program(config: &str) -> Option<String> {
    let programs = executables(config);
    let multiplayer = tag(config, "MultiplayerProtocol").and_then(|t| attr(t, "Executable")).map(|n| n.replace('/', "\\"));
    multiplayer
        .and_then(|m| programs.iter().find(|(n, _)| n.eq_ignore_ascii_case(&m)))
        .or(programs.first())
        .map(|(n, _)| n.clone())
}

/// The package a program belongs to, if it's installed as one (an appxmanifest.xml in
/// one of its folders).
pub fn find(exe: &Path) -> Option<Package> {
    let content = exe.ancestors().skip(1).find(|d| d.join("appxmanifest.xml").is_file())?.to_path_buf();
    let manifest = std::fs::read_to_string(content.join("appxmanifest.xml")).ok()?;
    let identity = tag(&manifest, "Identity")?;
    let name = attr(identity, "Name")?;
    let publisher = attr(identity, "Publisher")?;
    let version = attr(identity, "Version").unwrap_or_else(|| "0.0.0.0".into());
    let arch = attr(identity, "ProcessorArchitecture").unwrap_or_else(|| "x64".into());
    // Part of the full name when present (often "Base").
    let resource = attr(identity, "ResourceId").unwrap_or_default();
    let config = read_config(&content).unwrap_or_default();
    let (game, app_id) = pick(exe, &content, &config, &manifest)?;
    let (full_name, family) = names(&name, &publisher, &version, &arch, &resource)?;
    Some(Package { full_name, aumid: format!("{family}!{app_id}"), game })
}

/// The program a launch of `exe` runs and the Application that starts it. A package can
/// list several programs (each its own Application, matched by Id); `exe` is one of them,
/// or else (the launch helper, a tool) the package's main one.
fn pick(exe: &Path, content: &Path, config: &str, manifest: &str) -> Option<(PathBuf, String)> {
    let programs = executables(config);
    let wanted = exe.to_string_lossy().to_ascii_lowercase();
    let listed = programs.iter().find(|(n, _)| content.join(n).to_string_lossy().to_ascii_lowercase() == wanted);
    let main = main_program(config);
    let chosen = listed.or_else(|| programs.iter().find(|(n, _)| Some(n) == main.as_ref()));
    let apps: Vec<String> = tags(manifest, "Application").into_iter().filter_map(|t| attr(t, "Id")).collect();
    let app = chosen
        .and_then(|(_, id)| id.as_ref())
        .and_then(|id| apps.iter().find(|a| a.eq_ignore_ascii_case(id)))
        .or(apps.first())?
        .clone();
    let game = chosen.map(|(n, _)| content.join(n)).unwrap_or_else(|| exe.to_path_buf());
    if programs.len() > 1 {
        let how = if listed.is_some() { "the one picked" } else { "the package's main one, as the one picked isn't listed" };
        eprintln!("peerparty: package lists {} programs; starting {} through app {app} ({how})", programs.len(), game.display());
    }
    Some((game, app))
}

/// The package's full and family names, as Windows computes them.
fn names(name: &str, publisher: &str, version: &str, arch: &str, resource: &str) -> Option<(String, String)> {
    use windows_sys::Win32::Storage::Packaging::Appx::{PACKAGE_ID, PACKAGE_VERSION, PACKAGE_VERSION_0, PackageFamilyNameFromId, PackageFullNameFromId};
    let v: Vec<u64> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    let part = |i: usize| v.get(i).copied().unwrap_or(0) & 0xffff;
    let packed = (part(0) << 48) | (part(1) << 32) | (part(2) << 16) | part(3);
    let arch = match arch.to_ascii_lowercase().as_str() {
        "x86" => 0,
        "arm" => 5,
        "neutral" => 11,
        "arm64" => 12,
        _ => 9,
    };
    let (n, p, r) = (wide(name), wide(publisher), wide(resource));
    let id = PACKAGE_ID {
        reserved: 0,
        processorArchitecture: arch,
        version: PACKAGE_VERSION { Anonymous: PACKAGE_VERSION_0 { Version: packed } },
        name: n.as_ptr() as *mut u16,
        publisher: p.as_ptr() as *mut u16,
        resourceId: if resource.is_empty() { std::ptr::null_mut() } else { r.as_ptr() as *mut u16 },
        publisherId: std::ptr::null_mut(),
    };
    let get = |f: unsafe extern "system" fn(*const PACKAGE_ID, *mut u32, windows_sys::core::PWSTR) -> u32| {
        let mut buf = vec![0u16; 256];
        let mut len = buf.len() as u32;
        (unsafe { f(&id, &mut len, buf.as_mut_ptr()) } == 0).then(|| String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)]))
    };
    Some((get(PackageFullNameFromId)?, get(PackageFamilyNameFromId)?))
}

const CLSID_PACKAGE_DEBUG_SETTINGS: Guid = Guid(0xB1AEC16F, 0x2383, 0x4852, [0xB0, 0xE9, 0x8F, 0x0B, 0x1D, 0xC6, 0x6B, 0x4D]);
const IID_IPACKAGE_DEBUG_SETTINGS: Guid = Guid(0xF27C3930, 0x8029, 0x4AD1, [0x94, 0xE3, 0x3D, 0xBA, 0x41, 0x78, 0x10, 0xC1]);
const CLSID_APPLICATION_ACTIVATION_MANAGER: Guid = Guid(0x45BA127D, 0x10A8, 0x46EA, [0x8A, 0xB7, 0x56, 0xEA, 0x90, 0x78, 0x94, 0x3C]);
const IID_IAPPLICATION_ACTIVATION_MANAGER: Guid = Guid(0x2e941141, 0x7f97, 0x4756, [0xba, 0x1d, 0x9d, 0xec, 0xde, 0x89, 0x4a, 0x3d]);

fn create(clsid: &Guid, iid: &Guid) -> Result<usize, String> {
    use windows_sys::Win32::System::Com::{CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
    unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    let mut obj = std::ptr::null_mut();
    let hr = unsafe { CoCreateInstance(clsid as *const Guid as *const _, std::ptr::null_mut(), CLSCTX_ALL, iid as *const Guid as *const _, &mut obj) };
    if hr < 0 || obj.is_null() {
        return Err(format!("CoCreateInstance failed ({hr:#x})"));
    }
    Ok(obj as usize)
}

/// Starts the package with PeerParty injected (steps 1-3 above). Returns the helper's
/// process id.
pub fn start(pkg: &Package, peerparty: &Path, args: &str, env: &[(String, String)]) -> Result<u32, String> {
    let debug = create(&CLSID_PACKAGE_DEBUG_SETTINGS, &IID_IPACKAGE_DEBUG_SETTINGS)?;
    let command = wide(&format!("\"{}\" --attach", peerparty.display()));
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in env {
        block.extend(OsStr::new(&format!("{k}={v}")).encode_wide());
        block.push(0);
    }
    block.push(0);
    let full = wide(&pkg.full_name);
    let hr = vcall!(debug, 3, fn(*const u16, *const u16, *const u16) -> i32, full.as_ptr(), command.as_ptr(), block.as_ptr());
    if hr < 0 {
        vcall!(debug, 2, fn() -> u32);
        return Err(format!("couldn't arrange for peerparty to start with {} ({hr:#x}); is it installed?", pkg.full_name));
    }
    let result = (|| {
        let manager = create(&CLSID_APPLICATION_ACTIVATION_MANAGER, &IID_IAPPLICATION_ACTIVATION_MANAGER)?;
        let aumid = wide(&pkg.aumid);
        let arguments = wide(args);
        let mut pid = 0u32;
        let hr = vcall!(manager, 3, fn(*const u16, *const u16, u32, *mut u32) -> i32, aumid.as_ptr(), arguments.as_ptr(), 0, &mut pid);
        vcall!(manager, 2, fn() -> u32);
        if hr < 0 {
            return Err(format!("starting {} failed ({hr:#x})", pkg.aumid));
        }
        Ok(pid)
    })();
    vcall!(debug, 4, fn(*const u16) -> i32, full.as_ptr());
    vcall!(debug, 2, fn() -> u32);
    result
}

/// The game's process once the helper has started it (by its program's path), waiting up
/// to two minutes; the helper's own process when it never appears.
pub fn wait_for_game(pkg: &Package, helper: u32) -> Option<(HANDLE, u32)> {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
    let want = pkg.game.to_string_lossy().to_ascii_lowercase();
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        for pid in processes() {
            // The process started may be the game itself (no launch helper in between):
            // then it's the one, not one to pass over.
            if image(pid).is_some_and(|p| p.to_ascii_lowercase() == want) {
                let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
                if !h.is_null() {
                    return Some((h, pid));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, helper) };
    (!h.is_null()).then_some((h, helper))
}

fn processes() -> Vec<u32> {
    use windows_sys::Win32::System::ProcessStatus::EnumProcesses;
    let mut pids = vec![0u32; 4096];
    let mut got = 0u32;
    if unsafe { EnumProcesses(pids.as_mut_ptr(), (pids.len() * 4) as u32, &mut got) } == 0 {
        return Vec::new();
    }
    pids.truncate(got as usize / 4);
    pids
}

fn image(pid: u32) -> Option<String> {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) } != 0;
    unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
    ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

/// `peerparty --attach -p <pid> -tid <tid>`: Windows started a package's process
/// suspended for us (step 3). Inject and resume it.
/// `--attach -p <pid> -tid <tid>`: Windows runs this for a packaged game's first
/// process (start() above), and the hook runs it for a child of the other bitness,
/// which it can't inject itself (a 32-bit starter of a 64-bit game).
pub fn attach(args: &[String]) -> Result<u32, String> {
    use windows_sys::Win32::System::Threading::*;
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<u32>().ok());
    let (pid, tid) = (value("-p").ok_or("no -p")?, value("-tid").ok_or("no -tid")?);
    let process = unsafe { OpenProcess(PROCESS_ALL_ACCESS, 0, pid) };
    let thread = unsafe { OpenThread(THREAD_ALL_ACCESS, 0, tid) };
    if process.is_null() || thread.is_null() {
        let e = std::io::Error::last_os_error();
        for h in [process, thread] {
            if !h.is_null() {
                unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
            }
        }
        return Err(format!("couldn't open process {pid}: {e}"));
    }
    let (mut machine, mut native) = (0u16, 0u16);
    unsafe { IsWow64Process2(process, &mut machine, &mut native) };
    let x86 = machine == 0x014c;
    let pi = PROCESS_INFORMATION { hProcess: process, hThread: thread, dwProcessId: pid, dwThreadId: tid };
    let result = crate::hooks::extract(x86).and_then(|dll| unsafe {
        if x86 { super::wow64::inject(&pi, &dll) } else { super::launch::inject_x64(&pi, &dll) }
    });
    // The hook's child following (hook/src/child.rs) asks for this when the program
    // that started the child wanted it suspended.
    if !args.iter().any(|a| a == "--keep-suspended") {
        unsafe { ResumeThread(thread) };
    }
    unsafe {
        windows_sys::Win32::Foundation::CloseHandle(thread);
        windows_sys::Win32::Foundation::CloseHandle(process);
    }
    result.map(|_| 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names_follow_windows_rules() {
        // A known package: Microsoft's Windows Terminal (publisher id 8wekyb3d8bbwe).
        let (full, family) = names("Microsoft.WindowsTerminal", "CN=Microsoft Corporation, O=Microsoft Corporation, L=Redmond, S=Washington, C=US", "1.21.3231.0", "x64", "").unwrap();
        assert_eq!(family, "Microsoft.WindowsTerminal_8wekyb3d8bbwe");
        assert_eq!(full, "Microsoft.WindowsTerminal_1.21.3231.0_x64__8wekyb3d8bbwe");
        // A GDK-style package.
        let (full, _) = names("Contoso.SampleGame", "CN=Contoso", "1.4.6.0", "x64", "").unwrap();
        assert_eq!(full, "Contoso.SampleGame_1.4.6.0_x64__h91ms92gdsmmt");
        // One with a resource id (Identity has ResourceId="Base").
        let (full, _) = names("Fabrikam.578840CD64788", "CN=Fabrikam", "1.124.4948.0", "x64", "Base").unwrap();
        assert_eq!(full, "Fabrikam.578840CD64788_1.124.4948.0_x64_Base_rf71fm6tkk4qe");
    }

    #[test]
    fn attributes_are_read_by_whole_name() {
        let t = r#"<Identity xName="no" Name="Contoso.Game" Publisher="CN=Contoso" Version="1.0.0.0" Broken="x"#;
        assert_eq!(attr(t, "Name").as_deref(), Some("Contoso.Game"));
        assert_eq!(attr(t, "Publisher").as_deref(), Some("CN=Contoso"));
        assert_eq!(attr(t, "Missing"), None);
        assert_eq!(attr(t, "Broken"), None);
        assert_eq!(attr(r#"<Application Id="App""#, "Id").as_deref(), Some("App"));
    }

    // A package with a campaign and a multiplayer program, each its own Application, the
    // tags broken after their names.
    const TWO_PROGRAMS: &str = "<ExecutableList>\n  <Executable\n    Name=\"solo.exe\"\n    Id=\"GameSolo\" />\n  <Executable\n    Name=\"bin/versus.exe\"\n    Id=\"GameVersus\" />\n</ExecutableList>\n<DesktopRegistration>\n  <MultiplayerProtocol\n    Executable=\"bin/versus.exe\">true</MultiplayerProtocol>\n</DesktopRegistration>";
    const TWO_APPS: &str = "<Applications>\n<Application Id=\"GameSolo\" Executable=\"GameLaunchHelper.exe\">\n</Application>\n<Application\n  Id=\"GameVersus\" Executable=\"GameLaunchHelper.exe\"></Application></Applications>";

    #[test]
    fn tags_end_at_any_whitespace_not_inside_longer_names() {
        assert_eq!(tags(TWO_PROGRAMS, "Executable").len(), 2);
        assert_eq!(tags(TWO_APPS, "Application").len(), 2);
        assert_eq!(tag("<Identity\r\n\tName=\"A\"/>", "Identity").and_then(|t| attr(t, "Name")).as_deref(), Some("A"));
    }

    #[test]
    fn the_main_program_is_the_multiplayer_one() {
        assert_eq!(main_program(TWO_PROGRAMS).as_deref(), Some(r"bin\versus.exe"));
        assert_eq!(main_program("<Executable Name=\"only.exe\" Id=\"Game\"/>").as_deref(), Some("only.exe"));
        // A MultiplayerProtocol naming no listed program is passed over.
        assert_eq!(main_program("<Executable Name=\"a.exe\"/><Executable Name=\"b.exe\"/><MultiplayerProtocol Executable=\"c.exe\">").as_deref(), Some("a.exe"));
        assert_eq!(main_program("<ExecutableList></ExecutableList>"), None);
    }

    #[test]
    fn each_program_starts_through_its_own_application() {
        let content = Path::new(r"C:\Games\Contoso\Content");
        let run = |exe: &str| pick(&content.join(exe), content, TWO_PROGRAMS, TWO_APPS).unwrap();
        assert_eq!(run("solo.exe"), (content.join("solo.exe"), "GameSolo".into()));
        assert_eq!(run(r"BIN\Versus.exe"), (content.join(r"bin\versus.exe"), "GameVersus".into()));
        // The launch helper (or anything not listed) starts the main program.
        assert_eq!(run("gamelaunchhelper.exe"), (content.join(r"bin\versus.exe"), "GameVersus".into()));
        // No config: the first Application, and the program as given.
        let helper = content.join("helper.exe");
        assert_eq!(pick(&helper, content, "", TWO_APPS).unwrap(), (helper, "GameSolo".into()));
    }

    #[test]
    fn finds_installed_xbox_games() {
        // Only where the Xbox app has installed games (by default C:\XboxGames).
        let Ok(dir) = std::fs::read_dir(r"C:\XboxGames") else { return };
        for e in dir.flatten() {
            let content = e.path().join("Content");
            let Some(exe) = std::fs::read_dir(&content).ok().and_then(|d| {
                d.flatten().map(|f| f.path()).find(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")))
            }) else {
                continue;
            };
            if let Some(p) = find(&exe) {
                assert!(p.aumid.contains('!'), "{}", p.aumid);
                assert!(p.full_name.contains('_'), "{}", p.full_name);
            }
        }
    }
}
