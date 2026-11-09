//! Windows Firewall: whether its rules keep other players from reaching a game, and the
//! fix, when the player asks for it.
//!
//! Windows asks the first time a program listens for connections. "Cancel" there (or a
//! standard user without an administrator's password) leaves inbound Block rules for
//! that exe, and a Block rule beats any Allow rule, so other players' connections to
//! this PC are dropped from then on (they time out; ours to them still work). PeerParty's
//! sockets live in the game's process (the hook), so the rules that matter are the
//! game exe's.
//!
//! Reading needs no administrator: the rules come from the firewall's own store in the
//! registry (local rules and Group Policy ones) and are matched here, as plain code with
//! tests. Only which network profile is in use (and whether the firewall is on for it)
//! comes from `INetFwPolicy2`. Rules for other programs, and ones narrowed to some
//! ports, addresses, interfaces, a service or an app package, are left out: they may
//! not touch the game's connections, and a warning that may be wrong is worse than none.
//!
//! Fixing never happens without the player's click: `peerparty --fix-firewall <exe>
//! [--public]`, started elevated (Windows' administrator prompt), turns off the exe's
//! inbound Block rules and adds an inbound Allow rule for it on private and domain
//! networks (public ones too only with `--public`). If the prompt is declined or the
//! fix fails, the firewall's own settings open instead, for the player to do it there.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const DOMAIN: u32 = 1;
pub const PRIVATE: u32 = 2;
pub const PUBLIC: u32 = 4;
const ALL: u32 = DOMAIN | PRIVATE | PUBLIC;

/// What the Allow rules we add are called, so another fix replaces them.
const OURS: &str = "peerparty: ";

const RULES_KEY: &str = r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules";
const POLICY_RULES_KEY: &str = r"SOFTWARE\Policies\Microsoft\WindowsFirewall\FirewallRules";
const PROFILES_KEY: &str = r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy";

/// One firewall rule, as far as it matters here.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Rule {
    /// The rule's id: its registry value name (`Get-NetFirewallRule -Name`).
    pub id: String,
    pub name: String,
    /// The program, expanded and lowercased (`None`: every program).
    pub app: Option<String>,
    pub allow: bool,
    pub inbound: bool,
    pub active: bool,
    /// IP protocol number (`None`: any).
    pub protocol: Option<u8>,
    /// DOMAIN | PRIVATE | PUBLIC.
    pub profiles: u32,
    /// Limited to some ports, addresses, interfaces, a service or an app package.
    pub narrowed: bool,
    /// From Group Policy: only whoever runs the policy can change it.
    pub policy: bool,
}

/// Parses one rule from the store (`v2.10|Action=Block|Active=TRUE|Dir=In|...|`).
/// `expand` fills in `%SystemRoot%` and the like.
pub fn parse(id: &str, text: &str, policy: bool, expand: &dyn Fn(&str) -> String) -> Option<Rule> {
    let mut fields = text.split('|');
    if !fields.next()?.starts_with("v2.") {
        return None;
    }
    let mut r = Rule { id: id.into(), policy, active: true, ..Default::default() };
    let (mut action, mut dir, mut profiles) = (None, None, 0);
    for f in fields {
        let Some((k, v)) = f.split_once('=') else { continue };
        match k {
            // "ByPass" (allow if secure) counts as an allow.
            "Action" => action = Some(!v.eq_ignore_ascii_case("Block")),
            "Active" => r.active = v.eq_ignore_ascii_case("TRUE"),
            "Dir" => dir = Some(v.eq_ignore_ascii_case("In")),
            "Protocol" => r.protocol = v.parse::<u16>().ok().filter(|p| *p < 256).map(|p| p as u8),
            "Profile" => {
                profiles |= match v {
                    "Domain" => DOMAIN,
                    "Private" => PRIVATE,
                    "Public" => PUBLIC,
                    _ => 0,
                }
            }
            "App" => r.app = Some(normalize(&expand(v))),
            "Name" => r.name = v.into(),
            "IF" | "IFType" | "Svc" | "PFN" | "AppPkgId" | "LUAuth" | "RUAuth" | "RMauth" | "LUOwn" => r.narrowed = true,
            k if k.starts_with("LPort") || k.starts_with("RPort") || k.starts_with("LA") || k.starts_with("RA") || k.starts_with("Security") => {
                r.narrowed = true
            }
            _ => {}
        }
    }
    r.allow = action?;
    r.inbound = dir?;
    r.profiles = if profiles == 0 { ALL } else { profiles };
    Some(r)
}

/// A path as the store keeps it: lowercase, backslashes, no quotes or `\\?\`.
pub fn normalize(p: &str) -> String {
    let p = p.trim().trim_matches('"');
    let p = p.strip_prefix(r"\\?\").unwrap_or(p);
    p.replace('/', "\\").to_lowercase()
}

/// `%NAME%` from the environment (left as it is when unset).
pub fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) => {
                let name = &after[..j];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The firewall's state for each network profile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Profiles {
    /// The profiles of the networks this PC is on.
    pub current: u32,
    /// The profiles the firewall is on for.
    pub enabled: u32,
    /// "Block all incoming connections, including those in the list of allowed apps".
    pub block_all: u32,
    /// The profiles whose default for inbound connections is Block (the usual).
    pub default_block: u32,
}

/// What the firewall does to a game's inbound connections.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Verdict {
    /// The exe's file name.
    pub exe: String,
    /// Block rules for it on a network profile in use.
    pub blocks: Vec<Rule>,
    /// The profile in use blocks every incoming connection.
    pub block_all: bool,
    /// Allowed in for TCP and UDP (or the firewall lets everything in).
    pub allowed: bool,
    /// The network in use is a public one (the fix covers those only when asked).
    pub public: bool,
}

impl Verdict {
    /// Other players can't connect to the game.
    pub fn blocked(&self) -> bool {
        !self.blocks.is_empty() || self.block_all
    }

    /// The warning to show, if any.
    pub fn warning(&self) -> Option<String> {
        if self.block_all {
            return Some(crate::t!("firewall.blocks_all").into());
        }
        if self.blocks.is_empty() {
            return None;
        }
        Some(if self.blocks.iter().all(|r| r.policy) { crate::t!("firewall.blocks_policy", exe = self.exe) } else { crate::t!("firewall.blocks", exe = self.exe) })
    }

    /// A gentler note: nothing blocks it, but Windows hasn't been told to let it in yet
    /// (it asks when the game first goes online).
    pub fn note(&self) -> Option<String> {
        (!self.blocked() && !self.allowed).then(|| crate::t!("firewall.not_allowed", exe = self.exe))
    }
}

/// Matches the rules against a game's exe.
pub fn assess(rules: &[Rule], exe: &Path, p: Profiles) -> Verdict {
    let name = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| exe.display().to_string());
    let on = p.current & p.enabled;
    if on == 0 {
        return Verdict { exe: name, allowed: true, ..Default::default() };
    }
    let target = normalize(&exe.to_string_lossy());
    let ours = |r: &&Rule| {
        r.inbound && r.active && !r.narrowed && r.profiles & on != 0 && r.app.as_deref() == Some(target.as_str()) && r.protocol.is_none_or(|x| x == 6 || x == 17)
    };
    let blocks: Vec<Rule> = rules.iter().filter(ours).filter(|r| !r.allow).cloned().collect();
    let lets_in = |proto: u8| rules.iter().filter(ours).any(|r| r.allow && r.protocol.is_none_or(|x| x == proto));
    let defaults_open = on & !p.default_block == on;
    Verdict {
        exe: name,
        blocks,
        block_all: p.block_all & on != 0,
        allowed: defaults_open || (lets_in(6) && lets_in(17)),
        public: on & PUBLIC != 0,
    }
}

/// Every firewall rule: the local store's, then Group Policy's.
pub fn rules() -> Vec<Rule> {
    let mut out = Vec::new();
    for (key, policy) in [(RULES_KEY, false), (POLICY_RULES_KEY, true)] {
        for (id, text) in reg_values(key) {
            if let Some(r) = parse(&id, &text, policy, &expand_env) {
                out.push(r);
            }
        }
    }
    out
}

/// The firewall's profiles, from `INetFwPolicy2` (or, failing that, the registry, with
/// every profile taken as in use).
pub fn profiles() -> Profiles {
    com_profiles().unwrap_or_else(|| {
        let mut enabled = 0;
        for (bit, name) in [(DOMAIN, "DomainProfile"), (PRIVATE, "StandardProfile"), (PUBLIC, "PublicProfile")] {
            if reg_dword(&format!(r"{PROFILES_KEY}\{name}"), "EnableFirewall").unwrap_or(1) != 0 {
                enabled |= bit;
            }
        }
        Profiles { current: ALL, enabled, block_all: 0, default_block: ALL }
    })
}

/// Checks a game: reads the rules (at most every few seconds; they're shared between
/// games) and matches them.
#[derive(Default)]
pub struct Checker {
    seen: Option<(Instant, Vec<Rule>, Profiles)>,
}

impl Checker {
    const FRESH: Duration = Duration::from_secs(10);

    pub fn verdict(&mut self, exe: &Path) -> Verdict {
        if self.seen.as_ref().is_none_or(|(at, _, _)| at.elapsed() >= Self::FRESH) {
            self.seen = Some((Instant::now(), rules(), profiles()));
        }
        let (_, rules, p) = self.seen.as_ref().unwrap();
        assess(rules, exe, *p)
    }

    /// Reads them again next time (after a fix).
    pub fn forget(&mut self) {
        self.seen = None;
    }
}

/// The PowerShell that fixes it: turns `disable` off, removes `remove` (our earlier
/// rules), and adds an Allow rule for `exe` on `profiles`.
pub fn fix_script(disable: &[&str], remove: &[&str], exe: &str, profiles: u32) -> String {
    let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let list = |ids: &[&str]| ids.iter().map(|i| q(i)).collect::<Vec<_>>().join(",");
    let mut s = String::from("$ErrorActionPreference='Stop'; Import-Module NetSecurity; ");
    if !disable.is_empty() {
        s.push_str(&format!("Disable-NetFirewallRule -Name {}; ", list(disable)));
    }
    if !remove.is_empty() {
        s.push_str(&format!("Remove-NetFirewallRule -Name {}; ", list(remove)));
    }
    let names: Vec<&str> = [(DOMAIN, "Domain"), (PRIVATE, "Private"), (PUBLIC, "Public")].into_iter().filter(|(b, _)| profiles & b != 0).map(|(_, n)| n).collect();
    let file = Path::new(exe).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| exe.to_string());
    s.push_str(&format!(
        "New-NetFirewallRule -DisplayName {} -Description {} -Group 'peerparty' -Direction Inbound -Action Allow -Program {} -Profile {} | Out-Null",
        q(&format!("{OURS}{file}")),
        q("Lets other players connect to this game. Added by peerparty when you asked it to fix the firewall."),
        q(exe),
        names.join(",")
    ));
    s
}

/// What `--fix-firewall` changes for an exe: the Block rules to turn off (local ones on
/// the profiles being opened), and our own earlier Allow rules to replace.
pub fn plan<'a>(rules: &'a [Rule], exe: &Path, profiles: u32) -> (Vec<&'a str>, Vec<&'a str>, usize) {
    let target = normalize(&exe.to_string_lossy());
    let for_exe = |r: &&Rule| r.inbound && r.app.as_deref() == Some(target.as_str());
    let disable = rules.iter().filter(for_exe).filter(|r| !r.allow && r.active && !r.policy && !r.narrowed && r.profiles & profiles != 0).map(|r| r.id.as_str()).collect();
    let remove = rules.iter().filter(for_exe).filter(|r| r.allow && !r.policy && r.name.starts_with(OURS)).map(|r| r.id.as_str()).collect();
    let policy = rules.iter().filter(for_exe).filter(|r| !r.allow && r.active && r.policy).count();
    (disable, remove, policy)
}

/// `peerparty --fix-firewall <exe> [--public]`, run elevated.
pub fn fix(exe: &Path, public: bool) -> Result<String, String> {
    let profiles = PRIVATE | DOMAIN | if public { PUBLIC } else { 0 };
    let all = rules();
    let (disable, remove, policy) = plan(&all, exe, profiles);
    let script = fix_script(&disable, &remove, &exe.to_string_lossy(), profiles);
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .creation_flags_no_window()
        .output()
        .map_err(|e| format!("couldn't run PowerShell: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("it failed").trim().to_string();
        return Err(format!("the firewall change failed: {line}"));
    }
    let mut msg = format!("turned off {} block rule(s) and allowed {} in", disable.len(), exe.display());
    if policy > 0 {
        msg.push_str(&format!("; {policy} rule(s) from Group Policy still block it (only its administrator can change those)"));
    }
    Ok(msg)
}

trait NoWindow {
    fn creation_flags_no_window(&mut self) -> &mut Self;
}

impl NoWindow for std::process::Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(0x0800_0000) // CREATE_NO_WINDOW
    }
}

/// How asking for the fix went.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Fixed,
    /// The administrator prompt was declined (or couldn't show).
    Declined,
    Failed(String),
}

/// Runs `peerparty --fix-firewall` elevated (Windows shows its administrator prompt) and
/// waits for it. Blocks: call it off the UI thread.
pub fn fix_elevated(peerparty: &Path, exe: &Path, public: bool) -> Outcome {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    let verb = wide("runas");
    let file = wide(&peerparty.to_string_lossy());
    let mut params = format!("--fix-firewall \"{}\"", exe.display());
    if public {
        params.push_str(" --public");
    }
    let params = wide(&params);
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = 0; // SW_HIDE
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let e = unsafe { GetLastError() };
        return if e == 1223 { Outcome::Declined } else { Outcome::Failed(format!("couldn't start the fix (error {e})")) };
    }
    if info.hProcess.is_null() {
        return Outcome::Failed("the fix didn't start".into());
    }
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    match code {
        0 => Outcome::Fixed,
        // peerparty.exe started from a drive the administrator can't see (a mapped network
        // drive, say) never gets as far as the fix.
        _ => Outcome::Failed(format!("the fix didn't work (exit code {code})")),
    }
}

/// Opens the firewall's "Allowed apps" settings (or, failing that, its full rules).
pub fn open_settings() -> bool {
    shell_open("control.exe", "/name Microsoft.WindowsFirewall /page pageConfigureApps") || shell_open("wf.msc", "")
}

fn shell_open(file: &str, params: &str) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    let (verb, file, params) = (wide("open"), wide(file), wide(params));
    let r = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), params.as_ptr(), std::ptr::null(), 1) };
    r as usize > 32
}

/// The fix as the player's click does it: elevated, else the firewall's settings.
/// Returns what to tell the player, and whether it was fixed.
pub fn fix_or_open(peerparty: &Path, exe: &Path, public: bool) -> (String, bool) {
    let name = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let why = match fix_elevated(peerparty, exe, public) {
        Outcome::Fixed => {
            let v = assess(&rules(), exe, profiles());
            if !v.blocked() {
                return (crate::t!("firewall.fixed", exe = name), true);
            }
            let left = if v.block_all {
                crate::t!("firewall.left_block_all")
            } else if v.blocks.iter().all(|r| r.policy) {
                crate::t!("firewall.left_policy")
            } else if v.public && !public {
                crate::t!("firewall.left_public")
            } else {
                crate::t!("firewall.left_rules")
            };
            return (crate::t!("firewall.still_blocked", exe = name, why = left), false);
        }
        Outcome::Declined => crate::t!("firewall.declined").to_string(),
        Outcome::Failed(e) => e,
    };
    if open_settings() {
        (crate::t!("firewall.opened_settings", exe = name, why = why), false)
    } else {
        (crate::t!("firewall.open_yourself", exe = name, why = why), false)
    }
}

/// The exe a running process was started from.
pub fn process_exe(pid: u32) -> Option<PathBuf> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) } != 0;
    unsafe { CloseHandle(h) };
    ok.then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn hklm(key: &str) -> Option<windows_sys::Win32::System::Registry::HKEY> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, RegOpenKeyExW};
    let k = wide(key);
    let mut h = std::ptr::null_mut();
    (unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, k.as_ptr(), 0, KEY_READ | KEY_WOW64_64KEY, &mut h) } == 0).then_some(h)
}

/// A key's string values, as (name, text).
fn reg_values(key: &str) -> Vec<(String, String)> {
    use windows_sys::Win32::System::Registry::{REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegEnumValueW};
    let Some(h) = hklm(key) else { return Vec::new() };
    let mut out = Vec::new();
    let mut name = vec![0u16; 16384];
    let mut data = vec![0u8; 65536];
    for i in 0.. {
        let mut name_len = name.len() as u32;
        let mut data_len = data.len() as u32;
        let mut kind = 0u32;
        let r = unsafe { RegEnumValueW(h, i, name.as_mut_ptr(), &mut name_len, std::ptr::null(), &mut kind, data.as_mut_ptr(), &mut data_len) };
        match r {
            0 => {}
            234 => continue, // ERROR_MORE_DATA: too big to be a rule
            _ => break,      // ERROR_NO_MORE_ITEMS, or anything else
        }
        if kind != REG_SZ && kind != REG_EXPAND_SZ {
            continue;
        }
        let units: Vec<u16> = data[..data_len as usize].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|u| *u != 0).collect();
        out.push((String::from_utf16_lossy(&name[..name_len as usize]), String::from_utf16_lossy(&units)));
    }
    unsafe { RegCloseKey(h) };
    out
}

fn reg_dword(key: &str, value: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_SUBKEY_WOW6464KEY, RegGetValueW};
    let (k, v) = (wide(key), wide(value));
    let mut out = 0u32;
    let mut len = 4u32;
    let r = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), v.as_ptr(), RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY, std::ptr::null_mut(), (&mut out as *mut u32).cast(), &mut len) };
    (r == 0).then_some(out)
}

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);

const CLSID_NET_FW_POLICY2: Guid = Guid(0xE2B3C97F, 0x6AE1, 0x41AC, [0x81, 0x7A, 0xF6, 0xF9, 0x21, 0x66, 0xD7, 0xDD]);
const IID_INET_FW_POLICY2: Guid = Guid(0x98325047, 0xC671, 0x4174, [0x8D, 0x81, 0xDE, 0xFC, 0xD3, 0xF0, 0x31, 0x86]);

/// Calls method `slot` of a COM object.
macro_rules! vcall {
    ($obj:expr, $slot:expr, fn($($t:ty),*) -> $r:ty $(, $arg:expr)*) => {{
        let vt = unsafe { *($obj as *const *const usize) };
        let f: unsafe extern "system" fn(usize $(, $t)*) -> $r = unsafe { std::mem::transmute(*vt.add($slot)) };
        unsafe { f($obj $(, $arg)*) }
    }};
}

fn com_profiles() -> Option<Profiles> {
    use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
    let init = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    let result = (|| {
        let mut obj = std::ptr::null_mut();
        let hr = unsafe {
            CoCreateInstance(
                &CLSID_NET_FW_POLICY2 as *const Guid as *const _,
                std::ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_INET_FW_POLICY2 as *const Guid as *const _,
                &mut obj,
            )
        };
        if hr < 0 || obj.is_null() {
            return None;
        }
        let obj = obj as usize;
        // INetFwPolicy2, after IDispatch's 7 slots: 7 get_CurrentProfileTypes,
        // 8 get_FirewallEnabled, 12 get_BlockAllInboundTraffic, 23 get_DefaultInboundAction.
        let mut current = 0i32;
        let ok = vcall!(obj, 7, fn(*mut i32) -> i32, &mut current) >= 0;
        let mut p = Profiles { current: current as u32 & ALL, enabled: 0, block_all: 0, default_block: 0 };
        for bit in [DOMAIN, PRIVATE, PUBLIC] {
            let (mut on, mut all, mut action) = (0i16, 0i16, 0i32);
            if vcall!(obj, 8, fn(i32, *mut i16) -> i32, bit as i32, &mut on) >= 0 && on != 0 {
                p.enabled |= bit;
            }
            if vcall!(obj, 12, fn(i32, *mut i16) -> i32, bit as i32, &mut all) >= 0 && all != 0 {
                p.block_all |= bit;
            }
            // NET_FW_ACTION_BLOCK is 0; a failed read counts as Block, the default.
            if vcall!(obj, 23, fn(i32, *mut i32) -> i32, bit as i32, &mut action) < 0 || action == 0 {
                p.default_block |= bit;
            }
        }
        vcall!(obj, 2, fn() -> u32);
        (ok && p.current != 0).then_some(p)
    })();
    if init >= 0 {
        unsafe { CoUninitialize() };
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(s: &str) -> String {
        s.to_string()
    }

    fn rule(text: &str) -> Rule {
        parse("id", text, false, &none).unwrap()
    }

    const PRIVATE_ONLY: Profiles = Profiles { current: PRIVATE, enabled: ALL, block_all: 0, default_block: ALL };

    #[test]
    fn parses_what_windows_leaves_when_its_prompt_is_cancelled() {
        let r = rule(r"v2.10|Action=Block|Active=TRUE|Dir=In|Protocol=6|Profile=Private|App=C:\games\some game\game.exe|Name=game|Desc=game|");
        assert_eq!(
            r,
            Rule {
                id: "id".into(),
                name: "game".into(),
                app: Some(r"c:\games\some game\game.exe".into()),
                allow: false,
                inbound: true,
                active: true,
                protocol: Some(6),
                profiles: PRIVATE,
                narrowed: false,
                policy: false,
            }
        );
        // No Profile: all of them; no Protocol: any; repeated Profile fields add up.
        let r = rule(r"v2.33|Action=Allow|Active=TRUE|Dir=In|App=C:\x.exe|Name=x|Defer=User|");
        assert_eq!((r.profiles, r.protocol, r.allow), (ALL, None, true));
        assert_eq!(rule(r"v2.10|Action=Allow|Dir=In|Profile=Domain|Profile=Public|App=C:\x.exe|").profiles, DOMAIN | PUBLIC);
        // Ports, a service or a package narrow it.
        assert!(rule(r"v2.33|Action=Allow|Active=FALSE|Dir=In|Protocol=17|LPort=3389|App=%SystemRoot%\system32\svchost.exe|Svc=termservice|").narrowed);
        assert!(parse("id", "not a rule", false, &none).is_none());
        assert!(parse("id", "v2.10|Active=TRUE|Dir=In|", false, &none).is_none());
    }

    #[test]
    fn expands_environment_variables_in_programs() {
        let e = |s: &str| s.replace("%SystemRoot%", r"C:\Windows");
        assert_eq!(parse("id", r"v2.10|Action=Allow|Dir=In|App=%SystemRoot%\System32\x.exe|", false, &e).unwrap().app.as_deref(), Some(r"c:\windows\system32\x.exe"));
        // SAFETY of the test: only reads.
        let root = std::env::var("SystemRoot").unwrap_or_default();
        assert_eq!(expand_env(r"%SystemRoot%\a"), format!(r"{root}\a"));
        assert_eq!(expand_env("%PEERPARTY_NOT_SET_ANYWHERE%x"), "%PEERPARTY_NOT_SET_ANYWHERE%x");
        assert_eq!(expand_env("50% off"), "50% off");
    }

    #[test]
    fn a_block_rule_for_the_game_on_the_network_in_use_is_a_warning() {
        let rules = vec![
            rule(r"v2.10|Action=Block|Active=TRUE|Dir=In|Protocol=6|Profile=Private|App=C:\Games\Puzzler\puzzler.exe|Name=k|"),
            rule(r"v2.10|Action=Block|Active=TRUE|Dir=In|Protocol=17|Profile=Private|App=C:\games\puzzler\PUZZLER.EXE|Name=k|"),
            rule(r"v2.10|Action=Allow|Active=TRUE|Dir=In|Protocol=6|Profile=Public|App=c:\games\puzzler\puzzler.exe|Name=k|"),
        ];
        let v = assess(&rules, Path::new(r"C:\GAMES\Puzzler\Puzzler.exe"), PRIVATE_ONLY);
        assert_eq!(v.blocks.len(), 2);
        assert!(v.blocked());
        assert!(v.warning().unwrap().contains("Puzzler.exe"));
        assert!(v.note().is_none());
        // On a public network, those Private rules don't apply; the Public allow is only TCP.
        let public = Profiles { current: PUBLIC, ..PRIVATE_ONLY };
        let v = assess(&rules, Path::new(r"C:\Games\Puzzler\puzzler.exe"), public);
        assert!(!v.blocked());
        assert!(!v.allowed);
        assert!(v.note().is_some());
        // Another program's rule, a turned-off one, an outbound one, a port-narrowed one,
        // and one for another protocol don't count.
        let others = vec![
            rule(r"v2.10|Action=Block|Active=TRUE|Dir=In|App=C:\Games\Other\puzzler.exe|"),
            rule(r"v2.10|Action=Block|Active=FALSE|Dir=In|App=C:\Games\Puzzler\puzzler.exe|"),
            rule(r"v2.10|Action=Block|Active=TRUE|Dir=Out|App=C:\Games\Puzzler\puzzler.exe|"),
            rule(r"v2.10|Action=Block|Active=TRUE|Dir=In|LPort=27015|App=C:\Games\Puzzler\puzzler.exe|"),
            rule(r"v2.10|Action=Block|Active=TRUE|Dir=In|Protocol=1|App=C:\Games\Puzzler\puzzler.exe|"),
        ];
        assert!(!assess(&others, Path::new(r"C:\Games\Puzzler\puzzler.exe"), PRIVATE_ONLY).blocked());
    }

    #[test]
    fn block_beats_allow_and_allow_needs_tcp_and_udp() {
        let exe = Path::new(r"D:\g\game.exe");
        let allow = vec![
            rule(r"v2.10|Action=Allow|Active=TRUE|Dir=In|Protocol=6|Profile=Private|App=D:\g\game.exe|"),
            rule(r"v2.10|Action=Allow|Active=TRUE|Dir=In|Protocol=17|Profile=Private|App=D:\g\game.exe|"),
        ];
        let v = assess(&allow, exe, PRIVATE_ONLY);
        assert!(v.allowed && !v.blocked() && v.warning().is_none() && v.note().is_none());
        assert!(!assess(&allow[..1], exe, PRIVATE_ONLY).allowed);
        let mut both = allow.clone();
        both.push(rule(r"v2.10|Action=Block|Active=TRUE|Dir=In|App=D:\g\game.exe|"));
        assert!(assess(&both, exe, PRIVATE_ONLY).blocked());
    }

    #[test]
    fn firewall_off_or_open_by_default_or_shut_tight() {
        let exe = Path::new(r"D:\g\game.exe");
        let block = vec![rule(r"v2.10|Action=Block|Active=TRUE|Dir=In|App=D:\g\game.exe|")];
        let off = Profiles { enabled: PUBLIC, ..PRIVATE_ONLY };
        let v = assess(&block, exe, off);
        assert!(!v.blocked() && v.allowed);
        let open = Profiles { default_block: PUBLIC, ..PRIVATE_ONLY };
        assert!(assess(&[], exe, open).allowed);
        let tight = Profiles { block_all: PRIVATE, ..PRIVATE_ONLY };
        let v = assess(&[], exe, tight);
        assert!(v.blocked() && v.warning().unwrap().contains("all incoming"));
    }

    #[test]
    fn policy_rules_are_named_and_left_alone_by_the_fix() {
        let exe = Path::new(r"D:\g\game.exe");
        let rules = vec![
            parse("gp", r"v2.10|Action=Block|Active=TRUE|Dir=In|App=D:\g\game.exe|", true, &none).unwrap(),
            parse("mine", r"v2.10|Action=Block|Active=TRUE|Dir=In|Profile=Private|App=D:\g\game.exe|", false, &none).unwrap(),
            parse("pub", r"v2.10|Action=Block|Active=TRUE|Dir=In|Profile=Public|App=D:\g\game.exe|", false, &none).unwrap(),
            parse("old", r"v2.10|Action=Allow|Active=TRUE|Dir=In|App=D:\g\game.exe|Name=peerparty: game.exe|", false, &none).unwrap(),
            parse("theirs", r"v2.10|Action=Allow|Active=TRUE|Dir=In|App=D:\g\game.exe|Name=game|", false, &none).unwrap(),
        ];
        let only_gp = assess(&rules[..1], exe, PRIVATE_ONLY);
        assert!(only_gp.warning().unwrap().contains("policy"));
        // Private and domain: the public-only block stays; public too: it goes.
        assert_eq!(plan(&rules, exe, PRIVATE | DOMAIN), (vec!["mine"], vec!["old"], 1));
        assert_eq!(plan(&rules, exe, ALL).0, vec!["mine", "pub"]);
    }

    #[test]
    fn the_fix_script_quotes_and_names_profiles() {
        let s = fix_script(&["TCP Query User{X}c:\\it's\\game.exe"], &["old"], r"C:\it's\game.exe", PRIVATE | DOMAIN);
        assert!(s.contains(r"Disable-NetFirewallRule -Name 'TCP Query User{X}c:\it''s\game.exe';"), "{s}");
        assert!(s.contains("Remove-NetFirewallRule -Name 'old';"), "{s}");
        assert!(s.contains(r"-Program 'C:\it''s\game.exe' -Profile Domain,Private"), "{s}");
        assert!(s.contains("-DisplayName 'peerparty: game.exe'"), "{s}");
        let s = fix_script(&[], &[], r"C:\g.exe", ALL);
        assert!(!s.contains("Disable-") && !s.contains("Remove-") && s.contains("-Profile Domain,Private,Public"), "{s}");
    }

    /// This PC's own firewall, read only (no rule is changed): the parser takes every
    /// rule the store has, and the profile read agrees with what PowerShell says.
    #[test]
    fn reads_this_pcs_firewall() {
        let rules = rules();
        let total = reg_values(RULES_KEY).len();
        assert!(total == 0 || rules.len() * 10 >= total * 9, "{} of {total} rules parsed", rules.len());
        let p = profiles();
        assert!(p.current != 0 && p.current & !ALL == 0, "{p:?}");
    }
}
