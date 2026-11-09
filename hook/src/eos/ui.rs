//! EOS_UI, Epic's social overlay, answered by PeerParty's overlay: `ShowFriends` opens
//! its Players page, `ShowNativeProfile` / `ShowBlockPlayer` / `ShowReportPlayer` a
//! player's page, and games listening for the display settings hear PeerParty's overlay
//! open and close as Epic's. Without a launcher to show the overlay, `ShowFriends` and
//! the rest complete with `EOS_NotConfigured`, as on a platform without the overlay.
//!
//! Layouts: eos_ui_types.h (SDK 1.19).

use std::sync::atomic::{AtomicBool, Ordering};

use super::abi::result::*;
use super::abi::{CompletionId, CompletionTwoIds};
use super::ids;
use super::{Kind, add_listener, call, later, listeners, remove_listener};

const NOT_CONFIGURED: i32 = 31;

/// Whether PeerParty's overlay is showing, as the game sees it.
static VISIBLE: AtomicBool = AtomicBool::new(false);

#[allow(non_snake_case)]
#[repr(C)]
struct LocalUserOptions {
    ApiVersion: i32,
    LocalUserId: usize,
}

#[allow(non_snake_case)]
#[repr(C)]
struct LocalTargetOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    TargetUserId: usize,
}

#[allow(non_snake_case)]
#[repr(C)]
struct DisplaySettingsInfo {
    ClientData: usize,
    bIsVisible: i32,
    bIsExclusiveInput: i32,
}

pub(crate) fn resolve(name: &str) -> Option<usize> {
    Some(match name {
        "EOS_UI_ShowFriends" => show_friends as *const () as usize,
        "EOS_UI_HideFriends" => hide_friends as *const () as usize,
        "EOS_UI_GetFriendsVisible" => friends_visible as *const () as usize,
        "EOS_UI_GetFriendsExclusiveInput" => friends_visible as *const () as usize,
        "EOS_UI_ShowNativeProfile" | "EOS_UI_ShowBlockPlayer" | "EOS_UI_ShowReportPlayer" => {
            show_profile as *const () as usize
        }
        "EOS_UI_AddNotifyDisplaySettingsUpdated" => notify_display as *const () as usize,
        "EOS_UI_RemoveNotifyDisplaySettingsUpdated" => remove_notify as *const () as usize,
        "EOS_UI_SetToggleFriendsKey" | "EOS_UI_SetToggleFriendsButton" => success as *const () as usize,
        _ => return None,
    })
}

fn user(o: *const LocalUserOptions) -> usize {
    if o.is_null() { 0 } else { unsafe { (*o).LocalUserId } }
}

unsafe extern "system" fn show_friends(_h: usize, o: *const LocalUserOptions, cd: usize, cb: usize) {
    let local = user(o);
    let r = if crate::overlay::page("friends", None) { SUCCESS } else { NOT_CONFIGURED };
    later(move || unsafe { call(cb, &CompletionId { ResultCode: r, ClientData: cd, Id: local }) });
}

unsafe extern "system" fn hide_friends(_h: usize, o: *const LocalUserOptions, cd: usize, cb: usize) {
    let local = user(o);
    crate::ipc::send(crate::json::Json::obj(vec![("t", crate::json::Json::str("close"))]));
    later(move || unsafe { call(cb, &CompletionId { ResultCode: SUCCESS, ClientData: cd, Id: local }) });
}

unsafe extern "system" fn friends_visible(_h: usize, _o: *const LocalUserOptions) -> i32 {
    i32::from(VISIBLE.load(Ordering::Relaxed))
}

unsafe extern "system" fn show_profile(_h: usize, o: *const LocalTargetOptions, cd: usize, cb: usize) {
    let (local, target) = if o.is_null() { (0, 0) } else { unsafe { ((*o).LocalUserId, (*o).TargetUserId) } };
    let peer = ids::peer_of_epic(target);
    let r = if crate::overlay::page("profile", peer) { SUCCESS } else { NOT_CONFIGURED };
    later(move || unsafe { call(cb, &CompletionTwoIds { ResultCode: r, ClientData: cd, First: local, Second: target }) });
}

unsafe extern "system" fn notify_display(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::DisplaySettings, f, cd, None)
}

unsafe extern "system" fn remove_notify(_h: usize, id: u64) {
    remove_listener(id);
}

unsafe extern "system" fn success(_h: usize, _o: usize) -> i32 {
    SUCCESS
}

/// PeerParty's overlay opened or closed: `DisplaySettingsUpdated` on the game's tick.
pub(crate) fn overlay_visible(on: bool) {
    VISIBLE.store(on, Ordering::Relaxed);
    later(move || {
        for (f, cd) in listeners(Kind::DisplaySettings, None) {
            let info = DisplaySettingsInfo { ClientData: cd, bIsVisible: i32::from(on), bIsExclusiveInput: i32::from(on) };
            unsafe { call(f, &info) };
        }
    });
}
