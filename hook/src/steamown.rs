//! One SteamID for the game, whatever it asks.
//!
//! With a SteamID of its own for this PC (steamlan's `own_id_on`), PeerParty answers the
//! game's own questions with it (`GetSteamID`, lobby members, P2P identities, persona
//! names), but steam_api underneath still knows the player by the SteamID it gives. So
//! without more, the game would meet both: stats requested for "its" SteamID would be
//! for a stranger as far as steam_api knows, and a callback naming the player
//! (`UserStatsReceived_t`, `PersonaStateChange_t`, `AvatarImageLoaded_t`...) would name
//! someone the game isn't. This module maps between the two where they cross:
//!
//! - **Into steam_api**: SteamIDs and account ids PeerParty passes on (the friends
//!   functions in steamlan.rs) and the flat functions that take one (`ISteamUserStats`'
//!   per-user stats, achievements and leaderboard entries, `ISteamUGC`'s per-user
//!   queries) get steam_api's for ours.
//! - **Out of steam_api**: every callback and call result it delivers, whichever way the
//!   game dispatches (`ManualDispatch_GetNextCallback`, `GetAPICallResult`, or the
//!   `CCallbackBase` objects of classic dispatch), has steam_api's SteamID swapped for
//!   ours wherever it appears as 8 bytes, as do the flat functions that return one
//!   (`ISteamApps::GetAppOwner`, a leaderboard entry's user). A SteamID's 8 bytes are
//!   distinctive (universe and account type in the top byte), so this is safe on any
//!   payload; a bare account id isn't, and isn't swapped on the way out.
//!
//! Classic dispatch calls the game's objects directly, so each registered object gets a
//! copy of its class's vtable (one per class) whose two `Run` slots swap and then call the
//! class's own; the copy keeps the RTTI pointer before it and every other slot as it was.
//!
//! Not covered: a native game calling `ISteamUserStats`, `ISteamApps` or `ISteamUGC`
//! through their C++ interfaces with its own SteamID (PeerParty has no vtable layouts
//! for them; games ask for their own stats with `RequestCurrentStats`, which takes no
//! id), and SteamIDs inside structs those return. `PEERPARTY_SKIP=steamown` turns all
//! of this off.

use std::collections::{HashMap, HashSet};
use std::ffi::c_char;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::log;
use crate::pe::Export;
use crate::steamobj::vmethod;

/// The SteamID the game is shown, and steam_api's for the same player; 0 while unset.
static OWN: AtomicU64 = AtomicU64::new(0);
static STEAM: AtomicU64 = AtomicU64::new(0);

/// The game is shown `own`; steam_api knows the player as `steam`.
pub fn set(own: u64, steam: u64) {
    if own == steam || own == 0 || steam == 0 || crate::net::skip("steamown") {
        return;
    }
    STEAM.store(steam, Ordering::SeqCst);
    OWN.store(own, Ordering::SeqCst);
    log!("steamown: steam_api's SteamID {steam} is {own} to the game, both ways (calls in, callbacks out)");
}

fn pair() -> Option<(u64, u64)> {
    let own = OWN.load(Ordering::Relaxed);
    (own != 0).then(|| (own, STEAM.load(Ordering::Relaxed)))
}

/// A SteamID the game passes, as steam_api knows it.
pub fn to_steam(id: u64) -> u64 {
    match pair() {
        Some((own, steam)) if id == own => steam,
        _ => id,
    }
}

/// A SteamID from steam_api, as the game knows it.
pub fn to_game(id: u64) -> u64 {
    match pair() {
        Some((own, steam)) if id == steam => own,
        _ => id,
    }
}

/// An account id the game passes (ours is distinctive: the top bit set), as steam_api
/// knows it.
fn account_to_steam(acc: u32) -> u32 {
    match pair() {
        Some((own, steam)) if acc == own as u32 => steam as u32,
        _ => acc,
    }
}

/// Swaps steam_api's SteamID for ours wherever it appears as 8 little-endian bytes.
pub fn rewrite(data: &mut [u8]) -> usize {
    let Some((own, steam)) = pair() else { return 0 };
    let (from, to) = (steam.to_le_bytes(), own.to_le_bytes());
    let mut n = 0;
    let mut i = 0;
    while i + 8 <= data.len() {
        if data[i..i + 8] == from {
            data[i..i + 8].copy_from_slice(&to);
            n += 1;
            i += 8;
        } else {
            i += 1;
        }
    }
    n
}

/// As `rewrite`, on memory steam_api handed out, if it's writable.
///
/// # Safety
/// `p` must point to `len` bytes, or be checked unusable here.
pub unsafe fn rewrite_raw(p: *mut u8, len: usize) -> usize {
    if pair().is_none() || p.is_null() || len < 8 || len > 1 << 20 || !writable(p as usize, len) {
        return 0;
    }
    let n = rewrite(unsafe { std::slice::from_raw_parts_mut(p, len) });
    if n > 0 {
        crate::log::throttled("steamown callback", 4, |k| log!("steamown: steam_api's SteamID swapped for ours in a callback ({n}x, n={k})"));
    }
    n
}

fn query(p: usize) -> Option<windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION> {
    use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_GUARD};
    if p < 0x1_0000 {
        return None;
    }
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
    (n != 0 && info.State == MEM_COMMIT && info.Protect & PAGE_GUARD == 0).then_some(info)
}

/// Whether `[p, p+len)` is committed memory whose protection allows `ok`.
fn region_allows(p: usize, len: usize, ok: impl Fn(u32) -> bool) -> bool {
    let mut at = p;
    while at < p + len {
        let Some(info) = query(at) else { return false };
        if !ok(info.Protect & 0xff) {
            return false;
        }
        at = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

fn writable(p: usize, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_READWRITE, PAGE_WRITECOPY};
    region_allows(p, len, |prot| matches!(prot, PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY))
}

fn readable(p: usize, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_NOACCESS};
    region_allows(p, len, |prot| prot != PAGE_NOACCESS && prot != 0)
}

// ---- classic dispatch ----

const W: usize = size_of::<usize>();
/// Slots copied from a callback class's vtable: CCallbackBase's three, and room for a
/// subclass's own virtuals.
const SLOTS: usize = 16;

/// Original vtable -> PeerParty's copy (its slot 0).
static COPIES: LazyLock<Mutex<HashMap<usize, usize>>> = LazyLock::new(Mutex::default);
static OURS: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(Mutex::default);

/// Whether registered callback objects get the swapping vtable: in a game started through PeerParty, unless
/// the game keeps steam_api's SteamID (`steamid` mode official, or Valve's own Steam
/// client already in the game), or `PEERPARTY_SKIP=steamown`. A game whose SteamID
/// turns out to be kept anyway only pays a check per callback.
fn wrap_wanted() -> bool {
    crate::net::enabled()
        && !crate::net::skip("steamown")
        && !std::env::var("PEERPARTY_MODE_STEAMID").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "official" | "off" | "game" | "0"))
        && !crate::steamlan::steam_client_in_game()
}

/// A `CCallbackBase` the game registers (a callback, or a call result): from now on
/// steam_api's runs of it go through `run`/`run_result` here.
///
/// # Safety
/// `obj` is the object the game passed to `SteamAPI_RegisterCallback`/`RegisterCallResult`.
pub unsafe fn wrap(obj: usize) {
    if obj == 0 || !wrap_wanted() || !readable(obj, W) {
        return;
    }
    let vtable = unsafe { *(obj as *const usize) };
    if vtable == 0 || OURS.lock().unwrap().contains(&vtable) {
        return;
    }
    let copy = {
        let mut copies = COPIES.lock().unwrap();
        match copies.get(&vtable) {
            Some(c) => *c,
            None => {
                let Some(c) = copy_vtable(vtable) else { return };
                copies.insert(vtable, c);
                OURS.lock().unwrap().insert(c);
                c
            }
        }
    };
    if writable(obj, W) {
        unsafe { *(obj as *mut usize) = copy };
    }
}

/// [original vtable, its RTTI pointer, slots...]: returns the address of slot 0.
fn copy_vtable(vtable: usize) -> Option<usize> {
    if !readable(vtable, 3 * W) {
        return None;
    }
    let rtti = if readable(vtable - W, W) { unsafe { *((vtable - W) as *const usize) } } else { 0 };
    let mut v = vec![vtable, rtti];
    for i in 0..SLOTS {
        let at = vtable + i * W;
        if !readable(at, W) {
            break;
        }
        v.push(unsafe { *(at as *const usize) });
    }
    // MSVC puts the two Run overloads in reverse declaration order: the call result's
    // first, then the callback's (steam.rs `deliver_registered`).
    v[2] = run_result as *const () as usize;
    v[3] = run as *const () as usize;
    let leaked: &'static mut [usize] = Box::leak(v.into_boxed_slice());
    Some(leaked.as_ptr() as usize + 2 * W)
}

/// The class's own vtable, behind PeerParty's copy in `this`.
unsafe fn original(this: usize) -> usize {
    unsafe {
        let copy = *(this as *const usize);
        *((copy - 2 * W) as *const usize)
    }
}

/// `GetCallbackSizeBytes`, slot 2.
unsafe fn param_size(this: usize, vtable: usize) -> usize {
    unsafe {
        let f = *((vtable + 2 * W) as *const usize);
        #[cfg(target_arch = "x86")]
        let f: unsafe extern "thiscall" fn(usize) -> i32 = std::mem::transmute(f);
        #[cfg(not(target_arch = "x86"))]
        let f: unsafe extern "C" fn(usize) -> i32 = std::mem::transmute(f);
        f(this).max(0) as usize
    }
}

vmethod! {
    /// `Run(void* param)`.
    fn run(this: usize, param: *mut u8) {
        unsafe {
            let vtable = original(this);
            if pair().is_some() {
                rewrite_raw(param, param_size(this, vtable));
            }
            let f = *((vtable + W) as *const usize);
            #[cfg(target_arch = "x86")]
            let f: unsafe extern "thiscall" fn(usize, *mut u8) = std::mem::transmute(f);
            #[cfg(not(target_arch = "x86"))]
            let f: unsafe extern "C" fn(usize, *mut u8) = std::mem::transmute(f);
            f(this, param)
        }
    }
}

vmethod! {
    /// `Run(void* param, bool io_failure, SteamAPICall_t call)`.
    fn run_result(this: usize, param: *mut u8, failed: bool, call: u64) {
        unsafe {
            let vtable = original(this);
            if pair().is_some() && !failed {
                rewrite_raw(param, param_size(this, vtable));
            }
            let f = *(vtable as *const usize);
            #[cfg(target_arch = "x86")]
            let f: unsafe extern "thiscall" fn(usize, *mut u8, bool, u64) = std::mem::transmute(f);
            #[cfg(not(target_arch = "x86"))]
            let f: unsafe extern "C" fn(usize, *mut u8, bool, u64) = std::mem::transmute(f);
            f(this, param, failed, call)
        }
    }
}

// ---- flat functions ----

static REAL: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(Mutex::default);

const FLAT: &[&str] = &[
    "SteamAPI_ISteamUserStats_RequestUserStats",
    "SteamAPI_ISteamUserStats_GetUserStatInt32",
    "SteamAPI_ISteamUserStats_GetUserStatFloat",
    "SteamAPI_ISteamUserStats_GetUserAchievement",
    "SteamAPI_ISteamUserStats_GetUserAchievementAndUnlockTime",
    "SteamAPI_ISteamUserStats_DownloadLeaderboardEntriesForUsers",
    "SteamAPI_ISteamUserStats_GetDownloadedLeaderboardEntry",
    "SteamAPI_ISteamApps_GetAppOwner",
    "SteamAPI_ISteamUGC_CreateQueryUserUGCRequest",
];

/// steam_api's exports: the real flat functions this module stands in front of.
pub fn note(exports: &[Export]) {
    let mut real = REAL.lock().unwrap();
    for e in exports {
        if FLAT.contains(&e.name.as_str()) {
            real.insert(e.name.clone(), e.addr as usize);
        }
    }
}

fn real(name: &str) -> Option<usize> {
    REAL.lock().unwrap().get(name).copied()
}

/// PeerParty's stand-in for flat function `name`, if it maps SteamIDs.
pub fn resolve(name: &str) -> Option<usize> {
    if crate::net::skip("steamown") || real(name).is_none() {
        return None;
    }
    let f: usize = match name {
        "SteamAPI_ISteamUserStats_RequestUserStats" => request_user_stats as *const () as usize,
        "SteamAPI_ISteamUserStats_GetUserStatInt32" => get_user_stat_i32 as *const () as usize,
        "SteamAPI_ISteamUserStats_GetUserStatFloat" => get_user_stat_f32 as *const () as usize,
        "SteamAPI_ISteamUserStats_GetUserAchievement" => get_user_achievement as *const () as usize,
        "SteamAPI_ISteamUserStats_GetUserAchievementAndUnlockTime" => get_user_achievement_time as *const () as usize,
        "SteamAPI_ISteamUserStats_DownloadLeaderboardEntriesForUsers" => download_entries_for_users as *const () as usize,
        "SteamAPI_ISteamUserStats_GetDownloadedLeaderboardEntry" => get_downloaded_entry as *const () as usize,
        "SteamAPI_ISteamApps_GetAppOwner" => get_app_owner as *const () as usize,
        "SteamAPI_ISteamUGC_CreateQueryUserUGCRequest" => create_query_user_ugc as *const () as usize,
        _ => return None,
    };
    Some(f)
}

macro_rules! flat {
    ($name:literal, fn($($t:ty),*) -> $r:ty, $default:expr $(, $arg:expr)*) => {
        match real($name) {
            Some(f) => {
                let f: unsafe extern "C" fn($($t),*) -> $r = unsafe { std::mem::transmute(f) };
                unsafe { f($($arg),*) }
            }
            None => $default,
        }
    };
}

unsafe extern "C" fn request_user_stats(this: usize, id: u64) -> u64 {
    flat!("SteamAPI_ISteamUserStats_RequestUserStats", fn(usize, u64) -> u64, 0, this, to_steam(id))
}

unsafe extern "C" fn get_user_stat_i32(this: usize, id: u64, name: *const c_char, out: *mut i32) -> bool {
    flat!("SteamAPI_ISteamUserStats_GetUserStatInt32", fn(usize, u64, *const c_char, *mut i32) -> bool, false, this, to_steam(id), name, out)
}

unsafe extern "C" fn get_user_stat_f32(this: usize, id: u64, name: *const c_char, out: *mut f32) -> bool {
    flat!("SteamAPI_ISteamUserStats_GetUserStatFloat", fn(usize, u64, *const c_char, *mut f32) -> bool, false, this, to_steam(id), name, out)
}

unsafe extern "C" fn get_user_achievement(this: usize, id: u64, name: *const c_char, out: *mut bool) -> bool {
    flat!("SteamAPI_ISteamUserStats_GetUserAchievement", fn(usize, u64, *const c_char, *mut bool) -> bool, false, this, to_steam(id), name, out)
}

unsafe extern "C" fn get_user_achievement_time(this: usize, id: u64, name: *const c_char, out: *mut bool, time: *mut u32) -> bool {
    flat!(
        "SteamAPI_ISteamUserStats_GetUserAchievementAndUnlockTime",
        fn(usize, u64, *const c_char, *mut bool, *mut u32) -> bool,
        false,
        this,
        to_steam(id),
        name,
        out,
        time
    )
}

unsafe extern "C" fn download_entries_for_users(this: usize, board: u64, ids: *mut u64, n: i32) -> u64 {
    let mut mapped: Vec<u64> = if ids.is_null() || n <= 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(ids, n as usize) }.iter().map(|&id| to_steam(id)).collect()
    };
    let p = if mapped.is_empty() { ids } else { mapped.as_mut_ptr() };
    flat!("SteamAPI_ISteamUserStats_DownloadLeaderboardEntriesForUsers", fn(usize, u64, *mut u64, i32) -> u64, 0, this, board, p, n)
}

/// `LeaderboardEntry_t` starts with the entry's CSteamID.
unsafe extern "C" fn get_downloaded_entry(this: usize, entries: u64, index: i32, entry: *mut u64, details: *mut i32, max: i32) -> bool {
    let ok = flat!(
        "SteamAPI_ISteamUserStats_GetDownloadedLeaderboardEntry",
        fn(usize, u64, i32, *mut u64, *mut i32, i32) -> bool,
        false,
        this,
        entries,
        index,
        entry,
        details,
        max
    );
    if ok && !entry.is_null() {
        unsafe { *entry = to_game(*entry) };
    }
    ok
}

unsafe extern "C" fn get_app_owner(this: usize) -> u64 {
    let raw = flat!("SteamAPI_ISteamApps_GetAppOwner", fn(usize) -> u64, 0, this);
    let id = to_game(raw);
    crate::log::throttled("steamown GetAppOwner", 3, |_| log!("steamown: GetAppOwner is {raw} from steam_api, {id} to the game"));
    id
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn create_query_user_ugc(this: usize, account: u32, list: i32, matching: i32, sort: i32, creator: u32, consumer: u32, page: u32) -> u64 {
    flat!(
        "SteamAPI_ISteamUGC_CreateQueryUserUGCRequest",
        fn(usize, u32, i32, i32, i32, u32, u32, u32) -> u64,
        0,
        this,
        account_to_steam(account),
        list,
        matching,
        sort,
        creator,
        consumer,
        page
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests share the one mapping.
    static ONE: Mutex<()> = Mutex::new(());

    #[test]
    fn swaps_only_whole_steamids_and_maps_both_ways() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let own = 0x0110_0001_8000_0042u64;
        let steam = 0x0110_0001_0000_1234u64;
        OWN.store(0, Ordering::SeqCst);
        assert_eq!(to_steam(own), own);
        let mut cb = [0u8; 24];
        cb[..8].copy_from_slice(&480u64.to_le_bytes());
        cb[12..20].copy_from_slice(&steam.to_le_bytes());
        assert_eq!(rewrite(&mut cb.clone()), 0);
        STEAM.store(steam, Ordering::SeqCst);
        OWN.store(own, Ordering::SeqCst);
        assert_eq!(to_steam(own), steam);
        assert_eq!(to_game(steam), own);
        assert_eq!(to_steam(12345), 12345);
        assert_eq!(account_to_steam(own as u32), 0x1234);
        assert_eq!(account_to_steam(0x1234), 0x1234);
        // UserStatsReceived_t-like: the SteamID at an unaligned offset is found; a bare
        // account id (0x1234) elsewhere is left alone.
        cb[4..8].copy_from_slice(&0x1234u32.to_le_bytes());
        assert_eq!(rewrite(&mut cb), 1);
        assert_eq!(u64::from_le_bytes(cb[12..20].try_into().unwrap()), own);
        assert_eq!(u32::from_le_bytes(cb[4..8].try_into().unwrap()), 0x1234);
        OWN.store(0, Ordering::SeqCst);
    }

    vmethod! {
        fn test_run_result(this: usize, param: *mut u8, _failed: bool, _call: u64) {
            unsafe { *(this as *mut usize).add(1) = 100 + *param as usize };
        }
    }
    vmethod! {
        fn test_run(this: usize, param: *mut u8) {
            unsafe { *(this as *mut usize).add(1) = *(param.add(8)) as usize };
        }
    }
    vmethod! {
        fn test_size(_this: usize) -> i32 { 16 }
    }

    #[test]
    fn a_wrapped_callback_object_sees_the_games_steamid() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let own = 0x0110_0001_8000_0077u64;
        let steam = 0x0110_0001_0000_4321u64;
        // A CCallbackBase: vtable [run result, run, size], then a field the test reads.
        let vt: &'static [usize] = Box::leak(Box::new([0usize, test_run_result as *const () as usize, test_run as *const () as usize, test_size as *const () as usize]));
        let mut obj = Box::new([vt.as_ptr() as usize + W, 0usize]);
        let p = obj.as_mut_ptr() as usize;
        let copy = copy_vtable(vt.as_ptr() as usize + W).unwrap();
        unsafe { *(p as *mut usize) = copy };
        STEAM.store(steam, Ordering::SeqCst);
        OWN.store(own, Ordering::SeqCst);
        let mut param = [0u8; 16];
        param[..8].copy_from_slice(&steam.to_le_bytes());
        param[8] = 7;
        unsafe { run(p, param.as_mut_ptr()) };
        assert_eq!(u64::from_le_bytes(param[..8].try_into().unwrap()), own);
        assert_eq!(obj[1], 7);
        unsafe { run_result(p, param.as_mut_ptr(), false, 1) };
        assert_eq!(obj[1], 100 + (own as u8) as usize);
        OWN.store(0, Ordering::SeqCst);
    }
}
