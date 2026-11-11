//! Who the player is: Connect (ProductUserIds), Auth (EpicAccountIds) and UserInfo.
//! Every login succeeds at once with the LAN identity; display names come from the LAN
//! peers' names. Nothing is checked against Epic, and nothing claims ownership.
//!
//! External accounts: an Xbox Live account (`EOS_EAT_XBL`) maps to the LAN player
//! listed under that XUID (xboxlive/mod.rs), both ways, so a GDK game that finds its
//! friends on Xbox Live can turn a friend into an EOS user.

use std::ffi::c_char;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::log;

use super::abi::{self, result::*};
use super::ids::{self, Kind};
use super::{
    Kind as Listen, add_listener, call, hand_out, lan, later, listeners, owned_str, release,
    remove_listener, trace,
};

static CONNECTED: AtomicBool = AtomicBool::new(false);
static AUTHED: AtomicBool = AtomicBool::new(false);

pub fn resolve(name: &str) -> Option<usize> {
    Some(match name {
        "EOS_Connect_Login" => connect_login as *const () as usize,
        "EOS_Connect_Logout" => connect_logout as *const () as usize,
        "EOS_Connect_CreateUser" => connect_create_user as *const () as usize,
        "EOS_Connect_GetLoggedInUsersCount" => connect_count as *const () as usize,
        "EOS_Connect_GetLoggedInUserByIndex" => connect_by_index as *const () as usize,
        "EOS_Connect_GetLoginStatus" => connect_status as *const () as usize,
        "EOS_Connect_AddNotifyAuthExpiration" => notify_never as *const () as usize,
        "EOS_Connect_RemoveNotifyAuthExpiration" => remove_notify as *const () as usize,
        "EOS_Connect_AddNotifyLoginStatusChanged" => connect_notify_status as *const () as usize,
        "EOS_Connect_RemoveNotifyLoginStatusChanged" => remove_notify as *const () as usize,
        "EOS_Connect_QueryProductUserIdMappings" => connect_query_ok as *const () as usize,
        "EOS_Connect_QueryExternalAccountMappings" => connect_query_ok as *const () as usize,
        "EOS_Connect_CopyProductUserInfo" => connect_copy_user_info as *const () as usize,
        "EOS_Connect_GetProductUserExternalAccountCount" => {
            connect_account_count as *const () as usize
        }
        "EOS_Connect_CopyProductUserExternalAccountByIndex" => {
            connect_copy_user_info as *const () as usize
        }
        "EOS_Connect_CopyProductUserExternalAccountByAccountType" => {
            connect_copy_by_account_type as *const () as usize
        }
        "EOS_Connect_GetExternalAccountMapping" => connect_get_mapping as *const () as usize,
        "EOS_Connect_ExternalAccountInfo_Release" => free as *const () as usize,
        "EOS_Connect_CopyIdToken" => connect_copy_id_token as *const () as usize,
        "EOS_Connect_IdToken_Release" => free as *const () as usize,
        "EOS_ProductUserId_ToString" => puid_to_string as *const () as usize,
        "EOS_ProductUserId_FromString" => puid_from_string as *const () as usize,
        "EOS_ProductUserId_IsValid" => puid_is_valid as *const () as usize,

        "EOS_Auth_Login" => auth_login as *const () as usize,
        "EOS_Auth_Logout" => auth_logout as *const () as usize,
        "EOS_Auth_GetLoggedInAccountsCount" => auth_count as *const () as usize,
        "EOS_Auth_GetLoggedInAccountByIndex" => auth_by_index as *const () as usize,
        "EOS_Auth_GetLoginStatus" => auth_status as *const () as usize,
        "EOS_Auth_GetSelectedAccountId" => auth_selected as *const () as usize,
        "EOS_Auth_CopyUserAuthToken" => auth_copy_token as *const () as usize,
        "EOS_Auth_Token_Release" => free as *const () as usize,
        "EOS_Auth_CopyIdToken" => auth_copy_id_token as *const () as usize,
        "EOS_Auth_IdToken_Release" => free as *const () as usize,
        "EOS_Auth_AddNotifyLoginStatusChanged" => auth_notify_status as *const () as usize,
        "EOS_Auth_RemoveNotifyLoginStatusChanged" => remove_notify as *const () as usize,
        "EOS_EpicAccountId_ToString" => epic_to_string as *const () as usize,
        "EOS_EpicAccountId_FromString" => epic_from_string as *const () as usize,
        "EOS_EpicAccountId_IsValid" => epic_is_valid as *const () as usize,

        "EOS_Friends_QueryFriends" => friends_query as *const () as usize,
        "EOS_Friends_GetFriendsCount" => friends_count as *const () as usize,
        "EOS_Friends_GetFriendAtIndex" => friend_at_index as *const () as usize,
        "EOS_Friends_GetStatus" => friend_status as *const () as usize,
        "EOS_Friends_AddNotifyFriendsUpdate" => notify_never as *const () as usize,
        "EOS_Friends_RemoveNotifyFriendsUpdate" => remove_notify as *const () as usize,

        "EOS_UserInfo_QueryUserInfo" => userinfo_query as *const () as usize,
        "EOS_UserInfo_CopyUserInfo" => userinfo_copy as *const () as usize,
        "EOS_UserInfo_Release" => free as *const () as usize,
        _ => return None,
    })
}

fn me() -> Option<peerparty_core::PeerId> {
    lan().map(|l| l.me())
}

/// EOS_USERINFO_MAX_DISPLAYNAME_UTF8_LENGTH.
const MAX_NAME_BYTES: usize = 64;

fn name_of(peer: peerparty_core::PeerId) -> String {
    let name = lan()
        .and_then(|l| l.peer_name(peer))
        .unwrap_or_else(|| format!("Player {}", &peer.hex()[..6]));
    super::clamp(&name, MAX_NAME_BYTES).to_string()
}

unsafe extern "system" fn free(p: usize) {
    release(p);
}

unsafe extern "system" fn notify_never(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Listen::Never, f, cd, None)
}

unsafe extern "system" fn remove_notify(_h: usize, id: u64) {
    remove_listener(id);
}

// ---- Connect ----

unsafe extern "system" fn connect_login(_h: usize, _o: usize, cd: usize, cb: usize) {
    let me = me();
    trace("EOS_Connect_Login", || format!("-> {me:?}"));
    let was = CONNECTED.swap(me.is_some(), Ordering::Relaxed);
    later(move || {
        let info = abi::ConnectLoginInfo {
            ResultCode: if me.is_some() { SUCCESS } else { NO_CONNECTION },
            ClientData: cd,
            LocalUserId: me.map_or(0, ids::puid),
            ContinuanceToken: 0,
        };
        unsafe { call(cb, &info) };
        if let (Some(me), false) = (me, was) {
            for (f, cd) in listeners(Listen::ConnectLoginStatus, None) {
                let status = abi::ConnectLoginStatusChangedInfo {
                    ClientData: cd,
                    LocalUserId: ids::puid(me),
                    PreviousStatus: 0,
                    CurrentStatus: abi::LOGIN_STATUS_LOGGED_IN,
                };
                unsafe { call(f, &status) };
            }
        }
    });
}

unsafe extern "system" fn connect_logout(_h: usize, _o: usize, cd: usize, cb: usize) {
    CONNECTED.store(false, Ordering::Relaxed);
    complete_with_me(cd, cb, ids::puid);
}

unsafe extern "system" fn connect_create_user(_h: usize, _o: usize, cd: usize, cb: usize) {
    CONNECTED.store(me().is_some(), Ordering::Relaxed);
    complete_with_me(cd, cb, ids::puid);
}

unsafe extern "system" fn connect_query_ok(_h: usize, _o: usize, cd: usize, cb: usize) {
    complete_with_me(cd, cb, ids::puid);
}

/// Completes `{ResultCode, ClientData, LocalUserId}` with our id.
fn complete_with_me(cd: usize, cb: usize, id: fn(peerparty_core::PeerId) -> usize) {
    let me = me();
    later(move || {
        let info = abi::CompletionId {
            ResultCode: if me.is_some() { SUCCESS } else { NO_CONNECTION },
            ClientData: cd,
            Id: me.map_or(0, id),
        };
        unsafe { call(cb, &info) };
    });
}

unsafe extern "system" fn connect_count(_h: usize) -> i32 {
    CONNECTED.load(Ordering::Relaxed) as i32
}

unsafe extern "system" fn connect_by_index(_h: usize, index: i32) -> usize {
    match (index, me(), CONNECTED.load(Ordering::Relaxed)) {
        (0, Some(me), true) => ids::puid(me),
        _ => 0,
    }
}

unsafe extern "system" fn connect_status(_h: usize, id: usize) -> i32 {
    let mine = me().is_some_and(|m| ids::peer_of_puid(id) == Some(m));
    if mine && CONNECTED.load(Ordering::Relaxed) {
        abi::LOGIN_STATUS_LOGGED_IN
    } else {
        0
    }
}

unsafe extern "system" fn connect_notify_status(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Listen::ConnectLoginStatus, f, cd, None)
}

unsafe extern "system" fn connect_account_count(
    _h: usize,
    o: *const abi::ProductUserTarget,
) -> u32 {
    if o.is_null() {
        return 0;
    }
    ids::peer_of_puid(unsafe { (*o).TargetUserId }).is_some() as u32
}

/// One "external account" per player: the LAN identity, shown by its name.
unsafe extern "system" fn connect_copy_user_info(
    _h: usize,
    o: *const abi::ProductUserTarget,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let Some(peer) = ids::peer_of_puid(unsafe { (*o).TargetUserId }) else {
        return INVALID_USER;
    };
    let mut keep = Vec::new();
    let info = abi::ExternalAccountInfo {
        ApiVersion: abi::CONNECT_EXTERNALACCOUNTINFO_API,
        ProductUserId: ids::puid(peer),
        DisplayName: owned_str(&mut keep, &name_of(peer)),
        AccountId: owned_str(&mut keep, &peer.hex()),
        // EOS_EAT_EPIC
        AccountIdType: 0,
        LastLoginTime: -1,
    };
    unsafe { *out = hand_out(info, keep) };
    SUCCESS
}

/// EOS_EExternalAccountType: EPIC 0, STEAM 1, PSN 2, XBL 3.
const EAT_EPIC: i32 = 0;
const EAT_XBL: i32 = 3;

/// The LAN player behind an external account id: an Xbox user by XUID, an Epic one by
/// the LAN id it's shown as.
fn peer_of_external(kind: i32, id: &str) -> Option<peerparty_core::PeerId> {
    match kind {
        EAT_XBL => crate::xboxlive::peer_of(id.trim().parse().ok()?),
        EAT_EPIC => peerparty_core::PeerId::from_hex(id.trim()),
        _ => None,
    }
}

unsafe extern "system" fn connect_get_mapping(_h: usize, o: *const abi::GetExternalAccountMappingOptions) -> usize {
    if o.is_null() {
        return 0;
    }
    let (kind, id) = unsafe { ((*o).AccountIdType, (*o).TargetExternalUserId) };
    let id = if id.is_null() { String::new() } else { crate::read::cstr(id as usize).unwrap_or_default() };
    let peer = peer_of_external(kind, &id);
    trace("EOS_Connect_GetExternalAccountMapping", || format!("type {kind} {id} -> {peer:?}"));
    peer.map_or(0, ids::puid)
}

/// The player's account of the type asked for: Xbox Live (their XUID) or the LAN one.
unsafe extern "system" fn connect_copy_by_account_type(
    _h: usize,
    o: *const abi::CopyByAccountTypeOptions,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    if unsafe { (*o).AccountIdType } != EAT_XBL {
        return unsafe { connect_copy_user_info(_h, o.cast(), out) };
    }
    unsafe { *out = 0 };
    let Some(peer) = ids::peer_of_puid(unsafe { (*o).TargetUserId }) else {
        return INVALID_USER;
    };
    let mut keep = Vec::new();
    let info = abi::ExternalAccountInfo {
        ApiVersion: abi::CONNECT_EXTERNALACCOUNTINFO_API,
        ProductUserId: ids::puid(peer),
        DisplayName: owned_str(&mut keep, &name_of(peer)),
        AccountId: owned_str(&mut keep, &crate::xboxlive::xuid_of(peer).to_string()),
        AccountIdType: EAT_XBL,
        LastLoginTime: -1,
    };
    unsafe { *out = hand_out(info, keep) };
    SUCCESS
}

unsafe extern "system" fn connect_copy_id_token(
    _h: usize,
    o: *const abi::LocalUserOptions,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let Some(peer) = ids::peer_of_puid(unsafe { (*o).LocalUserId }) else {
        return INVALID_USER;
    };
    let mut keep = Vec::new();
    let token = abi::ConnectIdToken {
        ApiVersion: abi::CONNECT_IDTOKEN_API,
        ProductUserId: ids::puid(peer),
        JsonWebToken: owned_str(&mut keep, &format!("peerparty.{}", peer.hex())),
    };
    unsafe { *out = hand_out(token, keep) };
    SUCCESS
}

unsafe extern "system" fn puid_to_string(id: usize, buf: *mut c_char, len: *mut i32) -> i32 {
    unsafe { ids::to_string(Kind::Product, id, buf, len) }
}

unsafe extern "system" fn puid_from_string(s: *const c_char) -> usize {
    unsafe { ids::from_string(Kind::Product, s) }
}

unsafe extern "system" fn puid_is_valid(id: usize) -> i32 {
    ids::peer_of_puid(id).is_some() as i32
}

// ---- Auth ----

/// EOS_ELoginCredentialType, as the log names it.
fn credential_name(kind: i32) -> &'static str {
    match kind {
        0 => "password",
        1 => "exchange code",
        2 => "persistent auth (a saved login)",
        3 => "device code",
        4 => "developer (Epic's Dev Auth Tool)",
        5 => "refresh token",
        6 => "account portal (Epic's sign-in page)",
        7 => "external auth (another platform's token)",
        _ => "unknown",
    }
}

/// How the game signs in to an Epic account: the credential type, and whether a token
/// came with it. Never the id or the token themselves.
fn describe_login(o: usize) -> String {
    use crate::read::readable;
    if !readable(o, size_of::<abi::AuthLoginOptions>()) {
        return "options unreadable".into();
    }
    let creds = unsafe { (o as *const abi::AuthLoginOptions).read_unaligned() }.Credentials as usize;
    if !readable(creds, size_of::<abi::AuthCredentials>()) {
        return "no credentials".into();
    }
    let c = unsafe { (creds as *const abi::AuthCredentials).read_unaligned() };
    let token = c.Token as usize;
    let given = readable(token, 1) && unsafe { *(token as *const u8) } != 0;
    format!("credential type {} ({}), token {}", credential_name(c.Type), c.Type, if given { "given" } else { "none" })
}

unsafe extern "system" fn auth_login(_h: usize, o: usize, cd: usize, cb: usize) {
    let me = me();
    crate::log::throttled("EOS_Auth_Login credentials", 4, |_| log!("eos-lan EOS_Auth_Login: {}", describe_login(o)));
    trace("EOS_Auth_Login", || format!("-> {me:?}"));
    let was = AUTHED.swap(me.is_some(), Ordering::Relaxed);
    later(move || {
        let id = me.map_or(0, ids::epic);
        let info = abi::AuthLoginInfo {
            ResultCode: if me.is_some() { SUCCESS } else { NO_CONNECTION },
            ClientData: cd,
            LocalUserId: id,
            PinGrantInfo: 0,
            ContinuanceToken: 0,
            AccountFeatureRestrictedInfo: 0,
            SelectedAccountId: id,
        };
        unsafe { call(cb, &info) };
        if me.is_some() && !was {
            for (f, cd) in listeners(Listen::AuthLoginStatus, None) {
                let status = abi::AuthLoginStatusChangedInfo {
                    ClientData: cd,
                    LocalUserId: id,
                    PrevStatus: 0,
                    CurrentStatus: abi::LOGIN_STATUS_LOGGED_IN,
                };
                unsafe { call(f, &status) };
            }
        }
    });
}

unsafe extern "system" fn auth_logout(_h: usize, _o: usize, cd: usize, cb: usize) {
    AUTHED.store(false, Ordering::Relaxed);
    complete_with_me(cd, cb, ids::epic);
}

unsafe extern "system" fn auth_count(_h: usize) -> i32 {
    AUTHED.load(Ordering::Relaxed) as i32
}

unsafe extern "system" fn auth_by_index(_h: usize, index: i32) -> usize {
    match (index, me(), AUTHED.load(Ordering::Relaxed)) {
        (0, Some(me), true) => ids::epic(me),
        _ => 0,
    }
}

unsafe extern "system" fn auth_status(_h: usize, id: usize) -> i32 {
    let mine = me().is_some_and(|m| ids::peer_of_epic(id) == Some(m));
    if mine && AUTHED.load(Ordering::Relaxed) {
        abi::LOGIN_STATUS_LOGGED_IN
    } else {
        0
    }
}

unsafe extern "system" fn auth_selected(_h: usize, local: usize, out: *mut usize) -> i32 {
    if out.is_null() || ids::peer_of_epic(local).is_none() {
        return INVALID_USER;
    }
    unsafe { *out = local };
    SUCCESS
}

unsafe extern "system" fn auth_copy_token(
    _h: usize,
    _o: usize,
    local: usize,
    out: *mut usize,
) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let Some(peer) = ids::peer_of_epic(local) else {
        return INVALID_USER;
    };
    let mut keep = Vec::new();
    let token = abi::AuthToken {
        ApiVersion: abi::AUTH_TOKEN_API,
        App: owned_str(&mut keep, "peerparty"),
        ClientId: owned_str(&mut keep, "peerparty"),
        AccountId: ids::epic(peer),
        AccessToken: owned_str(&mut keep, &format!("peerparty.{}", peer.hex())),
        ExpiresIn: 86400.0,
        ExpiresAt: owned_str(&mut keep, "2099-01-01T00:00:00.000Z"),
        // EOS_ATT_USER
        AuthType: 1,
        RefreshToken: owned_str(&mut keep, ""),
        RefreshExpiresIn: 0.0,
        RefreshExpiresAt: owned_str(&mut keep, ""),
    };
    unsafe { *out = hand_out(token, keep) };
    SUCCESS
}

unsafe extern "system" fn auth_copy_id_token(
    _h: usize,
    o: *const abi::AuthCopyIdTokenOptions,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let Some(peer) = ids::peer_of_epic(unsafe { (*o).AccountId }) else {
        return INVALID_USER;
    };
    let mut keep = Vec::new();
    let token = abi::AuthIdToken {
        ApiVersion: abi::AUTH_IDTOKEN_API,
        AccountId: ids::epic(peer),
        JsonWebToken: owned_str(&mut keep, &format!("peerparty.{}", peer.hex())),
    };
    unsafe { *out = hand_out(token, keep) };
    SUCCESS
}

unsafe extern "system" fn auth_notify_status(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Listen::AuthLoginStatus, f, cd, None)
}

unsafe extern "system" fn epic_to_string(id: usize, buf: *mut c_char, len: *mut i32) -> i32 {
    unsafe { ids::to_string(Kind::Epic, id, buf, len) }
}

unsafe extern "system" fn epic_from_string(s: *const c_char) -> usize {
    unsafe { ids::from_string(Kind::Epic, s) }
}

unsafe extern "system" fn epic_is_valid(id: usize) -> i32 {
    ids::peer_of_epic(id).is_some() as i32
}

// ---- Friends: every LAN player of this game ----

fn lan_friends() -> Vec<peerparty_core::PeerId> {
    let mut v: Vec<_> = lan()
        .map(|l| l.peers())
        .unwrap_or_default()
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    v.sort();
    v
}

unsafe extern "system" fn friends_query(
    _h: usize,
    o: *const abi::LocalUserOptions,
    cd: usize,
    cb: usize,
) {
    let local = if o.is_null() {
        0
    } else {
        unsafe { (*o).LocalUserId }
    };
    later(move || {
        let info = abi::CompletionId {
            ResultCode: SUCCESS,
            ClientData: cd,
            Id: local,
        };
        unsafe { call(cb, &info) };
    });
}

unsafe extern "system" fn friends_count(_h: usize, _o: usize) -> i32 {
    lan_friends().len() as i32
}

unsafe extern "system" fn friend_at_index(_h: usize, o: *const abi::IndexOptions) -> usize {
    if o.is_null() {
        return 0;
    }
    let i = unsafe { (*o).Index } as usize;
    lan_friends().get(i).map_or(0, |p| ids::epic(*p))
}

unsafe extern "system" fn friend_status(_h: usize, o: *const abi::FriendStatusOptions) -> i32 {
    if o.is_null() {
        return 0;
    }
    let target = ids::peer_of_epic(unsafe { (*o).TargetUserId });
    if target.is_some_and(|t| lan_friends().contains(&t)) {
        abi::FRIENDS_STATUS_FRIENDS
    } else {
        0
    }
}

// ---- UserInfo ----

unsafe extern "system" fn userinfo_query(
    _h: usize,
    o: *const abi::UserInfoTargetOptions,
    cd: usize,
    cb: usize,
) {
    let (local, target) = if o.is_null() {
        (0, 0)
    } else {
        unsafe { ((*o).LocalUserId, (*o).TargetUserId) }
    };
    let known = ids::peer_of_epic(target).is_some();
    later(move || {
        let info = abi::CompletionTwoIds {
            ResultCode: if known { SUCCESS } else { NOT_FOUND },
            ClientData: cd,
            First: local,
            Second: target,
        };
        unsafe { call(cb, &info) };
    });
}

unsafe extern "system" fn userinfo_copy(
    _h: usize,
    o: *const abi::UserInfoTargetOptions,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let Some(peer) = ids::peer_of_epic(unsafe { (*o).TargetUserId }) else {
        return NOT_FOUND;
    };
    let name = name_of(peer);
    let mut keep = Vec::new();
    let info = abi::UserInfo {
        ApiVersion: abi::USERINFO_API,
        UserId: ids::epic(peer),
        Country: owned_str(&mut keep, ""),
        DisplayName: owned_str(&mut keep, &name),
        PreferredLanguage: owned_str(&mut keep, "en"),
        Nickname: owned_str(&mut keep, &name),
        DisplayNameSanitized: owned_str(&mut keep, &name),
    };
    unsafe { *out = hand_out(info, keep) };
    SUCCESS
}

#[cfg(test)]
mod tests {
    use super::{abi, describe_login};

    fn login(kind: i32, token: &std::ffi::CStr) -> String {
        let creds = abi::AuthCredentials { ApiVersion: 3, Id: c"".as_ptr(), Token: token.as_ptr(), Type: kind };
        let opts = abi::AuthLoginOptions { ApiVersion: 3, Credentials: &creds };
        describe_login(&opts as *const _ as usize)
    }

    #[test]
    fn names_the_sign_in_method_but_not_the_token() {
        let launched = login(1, c"secret-code");
        assert_eq!(launched, "credential type exchange code (1), token given");
        assert!(!launched.contains("secret"));
        assert_eq!(login(1, c""), "credential type exchange code (1), token none");
        assert_eq!(login(2, c""), "credential type persistent auth (a saved login) (2), token none");
        assert_eq!(describe_login(0), "options unreadable");
    }
}
