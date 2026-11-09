//! The slice of the EOS C ABI the LAN layer reads and writes, as `#[repr(C)]` mirrors of
//! the SDK 1.19 structs. Pointer-sized fields make them lay out right on x86 and x64.
//!
//! Option structs only ever gain fields at the end, and a game passes the version it was
//! built with in `ApiVersion`. A field newer than that version isn't there, and reading
//! it reads the game's memory past the struct. So every field after the first few is
//! read through a cutoff below: the ApiVersion that introduced it. Output structs are
//! written at the latest version; an older game reads only the prefix it knows.

#![allow(non_snake_case, dead_code)]

use std::ffi::c_char;

pub type Bool = i32;
pub type Handle = usize;
pub type Id = usize;

pub mod result {
    pub const SUCCESS: i32 = 0;
    pub const NO_CONNECTION: i32 = 1;
    pub const INVALID_USER: i32 = 3;
    pub const INVALID_PARAMETERS: i32 = 10;
    pub const NOT_IMPLEMENTED: i32 = 16;
    pub const NOT_FOUND: i32 = 18;
    pub const LIMIT_EXCEEDED: i32 = 22;
    pub const TIMED_OUT: i32 = 27;
    pub const ALREADY_PENDING: i32 = 9;
    pub const LOBBY_NOT_OWNER: i32 = 9000;
    pub const LOBBY_TOO_MANY_PLAYERS: i32 = 9004;
    pub const LOBBY_NOT_ALLOWED: i32 = 9016;
    pub const LOBBY_PRESENCE_LOBBY_EXISTS: i32 = 9018;
}

pub const LOGIN_STATUS_LOGGED_IN: i32 = 2;
pub const P2P_MAX_PACKET_SIZE: u32 = 1170;
pub const SOCKET_NAME_SIZE: usize = 33;
pub const PUID_STRING_SIZE: i32 = 33;
/// Longest EpicAccountId string, NUL included.
pub const EPIC_ID_STRING_SIZE: i32 = 33;

// Field cutoffs (the ApiVersion a field first appeared in).
pub const CREATELOBBY_PRESENCE: i32 = 2;
pub const CREATELOBBY_ALLOW_INVITES: i32 = 3;
pub const CREATELOBBY_BUCKET: i32 = 4;
pub const CREATELOBBY_DISABLE_MIGRATION: i32 = 5;
pub const CREATELOBBY_LOBBY_ID: i32 = 7;
pub const CREATELOBBY_JOIN_BY_ID: i32 = 8;
pub const SENDPACKET_RELIABILITY: i32 = 2;
pub const SENDPACKET_DISABLE_AUTO_ACCEPT: i32 = 3;
pub const RECEIVEPACKET_REQUESTED_CHANNEL: i32 = 2;
pub const PACKETSIZE_REQUESTED_CHANNEL: i32 = 2;

// Latest versions, written into structs we hand out.
pub const LOBBYDETAILS_INFO_API: i32 = 3;
pub const LOBBYDETAILS_MEMBERINFO_API: i32 = 1;
pub const LOBBY_ATTRIBUTE_API: i32 = 1;
pub const LOBBY_ATTRIBUTEDATA_API: i32 = 1;
pub const P2P_SOCKETID_API: i32 = 1;
pub const CONNECT_EXTERNALACCOUNTINFO_API: i32 = 1;
pub const CONNECT_IDTOKEN_API: i32 = 1;
pub const AUTH_TOKEN_API: i32 = 2;
pub const AUTH_IDTOKEN_API: i32 = 1;
pub const USERINFO_API: i32 = 3;

// Lobby attribute value types and visibility.
pub const ATTR_BOOL: i32 = 0;
pub const ATTR_INT64: i32 = 1;
pub const ATTR_DOUBLE: i32 = 2;
pub const ATTR_STRING: i32 = 3;
pub const VISIBILITY_PUBLIC: i32 = 0;

/// Every completion callback info starts like this.
#[repr(C)]
pub struct Completion {
    pub ResultCode: i32,
    pub ClientData: usize,
}

/// A completion info carrying an id after the prefix (LobbyId, LocalUserId...).
#[repr(C)]
pub struct CompletionId {
    pub ResultCode: i32,
    pub ClientData: usize,
    pub Id: usize,
}

#[repr(C)]
pub struct CompletionTwoIds {
    pub ResultCode: i32,
    pub ClientData: usize,
    pub First: usize,
    pub Second: usize,
}

#[repr(C)]
pub struct PlatformOptions {
    pub ApiVersion: i32,
    pub Reserved: usize,
    pub ProductId: *const c_char,
    pub SandboxId: *const c_char,
}

// ---- Connect / Auth / UserInfo ----

#[repr(C)]
pub struct ConnectLoginInfo {
    pub ResultCode: i32,
    pub ClientData: usize,
    pub LocalUserId: Id,
    pub ContinuanceToken: usize,
}

#[repr(C)]
pub struct ConnectLoginStatusChangedInfo {
    pub ClientData: usize,
    pub LocalUserId: Id,
    pub PreviousStatus: i32,
    pub CurrentStatus: i32,
}

#[repr(C)]
pub struct ProductUserTarget {
    pub ApiVersion: i32,
    pub TargetUserId: Id,
}

/// EOS_Connect_GetExternalAccountMappingsOptions.
#[repr(C)]
pub struct GetExternalAccountMappingOptions {
    pub ApiVersion: i32,
    pub LocalUserId: Id,
    pub AccountIdType: i32,
    pub TargetExternalUserId: *const c_char,
}

/// EOS_Connect_CopyProductUserExternalAccountByAccountTypeOptions.
#[repr(C)]
pub struct CopyByAccountTypeOptions {
    pub ApiVersion: i32,
    pub TargetUserId: Id,
    pub AccountIdType: i32,
}

#[repr(C)]
pub struct ExternalAccountInfo {
    pub ApiVersion: i32,
    pub ProductUserId: Id,
    pub DisplayName: *const c_char,
    pub AccountId: *const c_char,
    pub AccountIdType: i32,
    pub LastLoginTime: i64,
}

#[repr(C)]
pub struct LocalUserOptions {
    pub ApiVersion: i32,
    pub LocalUserId: Id,
}

#[repr(C)]
pub struct ConnectIdToken {
    pub ApiVersion: i32,
    pub ProductUserId: Id,
    pub JsonWebToken: *const c_char,
}

/// EOS_Auth_LoginOptions, the fields every version has.
#[repr(C)]
pub struct AuthLoginOptions {
    pub ApiVersion: i32,
    pub Credentials: *const AuthCredentials,
}

/// EOS_Auth_Credentials, the fields every version has.
#[repr(C)]
pub struct AuthCredentials {
    pub ApiVersion: i32,
    pub Id: *const c_char,
    pub Token: *const c_char,
    /// EOS_ELoginCredentialType.
    pub Type: i32,
}

#[repr(C)]
pub struct AuthLoginInfo {
    pub ResultCode: i32,
    pub ClientData: usize,
    pub LocalUserId: Id,
    pub PinGrantInfo: usize,
    pub ContinuanceToken: usize,
    pub AccountFeatureRestrictedInfo: usize,
    pub SelectedAccountId: Id,
}

#[repr(C)]
pub struct AuthLoginStatusChangedInfo {
    pub ClientData: usize,
    pub LocalUserId: Id,
    pub PrevStatus: i32,
    pub CurrentStatus: i32,
}

#[repr(C)]
pub struct AuthToken {
    pub ApiVersion: i32,
    pub App: *const c_char,
    pub ClientId: *const c_char,
    pub AccountId: Id,
    pub AccessToken: *const c_char,
    pub ExpiresIn: f64,
    pub ExpiresAt: *const c_char,
    pub AuthType: i32,
    pub RefreshToken: *const c_char,
    pub RefreshExpiresIn: f64,
    pub RefreshExpiresAt: *const c_char,
}

#[repr(C)]
pub struct AuthIdToken {
    pub ApiVersion: i32,
    pub AccountId: Id,
    pub JsonWebToken: *const c_char,
}

#[repr(C)]
pub struct AuthCopyIdTokenOptions {
    pub ApiVersion: i32,
    pub AccountId: Id,
}

#[repr(C)]
pub struct UserInfoTargetOptions {
    pub ApiVersion: i32,
    pub LocalUserId: Id,
    pub TargetUserId: Id,
}

#[repr(C)]
pub struct UserInfo {
    pub ApiVersion: i32,
    pub UserId: Id,
    pub Country: *const c_char,
    pub DisplayName: *const c_char,
    pub PreferredLanguage: *const c_char,
    pub Nickname: *const c_char,
    pub DisplayNameSanitized: *const c_char,
}

// ---- Lobby ----

#[repr(C)]
pub struct CreateLobbyOptions {
    pub ApiVersion: i32,
    pub LocalUserId: Id,
    pub MaxLobbyMembers: u32,
    pub PermissionLevel: i32,
    pub bPresenceEnabled: Bool,
    pub bAllowInvites: Bool,
    pub BucketId: *const c_char,
    pub bDisableHostMigration: Bool,
    pub bEnableRTCRoom: Bool,
    pub LocalRTCOptions: usize,
    pub LobbyId: *const c_char,
    pub bEnableJoinById: Bool,
    pub bRejoinAfterKickRequiresInvite: Bool,
}

#[repr(C)]
pub struct LobbyIdOptions {
    pub ApiVersion: i32,
    pub LocalUserId: Id,
    pub LobbyId: *const c_char,
}

#[repr(C)]
pub struct JoinLobbyOptions {
    pub ApiVersion: i32,
    pub LobbyDetailsHandle: Handle,
    pub LocalUserId: Id,
}

#[repr(C)]
pub struct JoinLobbyByIdOptions {
    pub ApiVersion: i32,
    pub LobbyId: *const c_char,
    pub LocalUserId: Id,
}

#[repr(C)]
pub struct MemberActionOptions {
    pub ApiVersion: i32,
    pub LobbyId: *const c_char,
    pub LocalUserId: Id,
    pub TargetUserId: Id,
}

#[repr(C)]
pub union AttrValue {
    pub AsInt64: i64,
    pub AsDouble: f64,
    pub AsBool: Bool,
    pub AsUtf8: *const c_char,
}

#[repr(C)]
pub struct AttributeData {
    pub ApiVersion: i32,
    pub Key: *const c_char,
    pub Value: AttrValue,
    pub ValueType: i32,
}

#[repr(C)]
pub struct Attribute {
    pub ApiVersion: i32,
    pub Data: *mut AttributeData,
    pub Visibility: i32,
}

#[repr(C)]
pub struct AddAttributeOptions {
    pub ApiVersion: i32,
    pub Attribute: *const AttributeData,
    pub Visibility: i32,
}

#[repr(C)]
pub struct KeyOptions {
    pub ApiVersion: i32,
    pub Key: *const c_char,
}

#[repr(C)]
pub struct U32Options {
    pub ApiVersion: i32,
    pub Value: u32,
}

#[repr(C)]
pub struct PtrOptions {
    pub ApiVersion: i32,
    pub Value: usize,
}

#[repr(C)]
pub struct UpdateLobbyOptions {
    pub ApiVersion: i32,
    pub LobbyModificationHandle: Handle,
}

#[repr(C)]
pub struct SetParameterOptions {
    pub ApiVersion: i32,
    pub Parameter: *const AttributeData,
    pub ComparisonOp: i32,
}

#[repr(C)]
pub struct RemoveParameterOptions {
    pub ApiVersion: i32,
    pub Key: *const c_char,
    pub ComparisonOp: i32,
}

#[repr(C)]
pub struct TargetAttrIndexOptions {
    pub ApiVersion: i32,
    pub TargetUserId: Id,
    pub AttrIndex: u32,
}

#[repr(C)]
pub struct TargetAttrKeyOptions {
    pub ApiVersion: i32,
    pub TargetUserId: Id,
    pub AttrKey: *const c_char,
}

#[repr(C)]
pub struct LobbyDetailsInfo {
    pub ApiVersion: i32,
    pub LobbyId: *const c_char,
    pub LobbyOwnerUserId: Id,
    pub PermissionLevel: i32,
    pub AvailableSlots: u32,
    pub MaxMembers: u32,
    pub bAllowInvites: Bool,
    pub BucketId: *const c_char,
    pub bAllowHostMigration: Bool,
    pub bRTCRoomEnabled: Bool,
    pub bAllowJoinById: Bool,
    pub bRejoinAfterKickRequiresInvite: Bool,
    pub bPresenceEnabled: Bool,
    pub AllowedPlatformIds: usize,
    pub AllowedPlatformIdsCount: u32,
}

#[repr(C)]
pub struct LobbyMemberInfo {
    pub ApiVersion: i32,
    pub UserId: Id,
    pub Platform: u32,
    pub bAllowsCrossplay: Bool,
}

#[repr(C)]
pub struct LobbyUpdateInfo {
    pub ClientData: usize,
    pub LobbyId: *const c_char,
}

#[repr(C)]
pub struct LobbyMemberUpdateInfo {
    pub ClientData: usize,
    pub LobbyId: *const c_char,
    pub TargetUserId: Id,
}

#[repr(C)]
pub struct LobbyMemberStatusInfo {
    pub ClientData: usize,
    pub LobbyId: *const c_char,
    pub TargetUserId: Id,
    pub CurrentStatus: i32,
}

#[repr(C)]
pub struct SendInviteOptions {
    pub ApiVersion: i32,
    pub LobbyId: *const c_char,
    pub LocalUserId: Id,
    pub TargetUserId: Id,
}

#[repr(C)]
pub struct UiEventIdOptions {
    pub ApiVersion: i32,
    pub UiEventId: u64,
}

#[repr(C)]
pub struct IndexOptions {
    pub ApiVersion: i32,
    pub LocalUserId: Id,
    pub Index: u32,
}

#[repr(C)]
pub struct RejectInviteOptions {
    pub ApiVersion: i32,
    pub InviteId: *const c_char,
    pub LocalUserId: Id,
}

#[repr(C)]
pub struct LobbyInviteReceivedInfo {
    pub ClientData: usize,
    pub InviteId: *const c_char,
    pub LocalUserId: Id,
    pub TargetUserId: Id,
}

#[repr(C)]
pub struct LobbyInviteAcceptedInfo {
    pub ClientData: usize,
    pub InviteId: *const c_char,
    pub LocalUserId: Id,
    pub TargetUserId: Id,
    pub LobbyId: *const c_char,
}

#[repr(C)]
pub struct JoinLobbyAcceptedInfo {
    pub ClientData: usize,
    pub LocalUserId: Id,
    pub UiEventId: u64,
}

// ---- Friends ----

#[repr(C)]
pub struct FriendStatusOptions {
    pub ApiVersion: i32,
    pub LocalUserId: Id,
    pub TargetUserId: Id,
}

pub const FRIENDS_STATUS_FRIENDS: i32 = 3;

// ---- P2P ----

#[repr(C)]
pub struct SocketId {
    pub ApiVersion: i32,
    pub SocketName: [u8; SOCKET_NAME_SIZE],
}

#[repr(C)]
pub struct SendPacketOptions {
    pub ApiVersion: i32,
    pub LocalUserId: Id,
    pub RemoteUserId: Id,
    pub SocketId: *const SocketId,
    pub Channel: u8,
    pub DataLengthBytes: u32,
    pub Data: *const u8,
    pub bAllowDelayedDelivery: Bool,
    pub Reliability: i32,
    pub bDisableAutoAcceptConnection: Bool,
}

#[repr(C)]
pub struct ReceivePacketOptions {
    pub ApiVersion: i32,
    pub LocalUserId: Id,
    pub MaxDataSizeBytes: u32,
    pub RequestedChannel: *const u8,
}

#[repr(C)]
pub struct PacketSizeOptions {
    pub ApiVersion: i32,
    pub LocalUserId: Id,
    pub RequestedChannel: *const u8,
}

/// Accept / CloseConnection / ClearPacketQueue.
#[repr(C)]
pub struct PeerSocketOptions {
    pub ApiVersion: i32,
    pub LocalUserId: Id,
    pub RemoteUserId: Id,
    pub SocketId: *const SocketId,
}

/// CloseConnections and the connection AddNotify options.
#[repr(C)]
pub struct LocalSocketOptions {
    pub ApiVersion: i32,
    pub LocalUserId: Id,
    pub SocketId: *const SocketId,
}

#[repr(C)]
pub struct I32Options {
    pub ApiVersion: i32,
    pub Value: i32,
}

#[repr(C)]
pub struct PortRangeOptions {
    pub ApiVersion: i32,
    pub Port: u16,
    pub MaxAdditionalPortsToTry: u16,
}

#[repr(C)]
pub struct PacketQueueSizeOptions {
    pub ApiVersion: i32,
    pub IncomingPacketQueueMaxSizeBytes: u64,
    pub OutgoingPacketQueueMaxSizeBytes: u64,
}

#[repr(C)]
pub struct PacketQueueInfo {
    pub IncomingPacketQueueMaxSizeBytes: u64,
    pub IncomingPacketQueueCurrentSizeBytes: u64,
    pub IncomingPacketQueueCurrentPacketCount: u64,
    pub OutgoingPacketQueueMaxSizeBytes: u64,
    pub OutgoingPacketQueueCurrentSizeBytes: u64,
    pub OutgoingPacketQueueCurrentPacketCount: u64,
}

#[repr(C)]
pub struct ConnectionInfo {
    pub ClientData: usize,
    pub LocalUserId: Id,
    pub RemoteUserId: Id,
    pub SocketId: *const SocketId,
}

#[repr(C)]
pub struct ConnectionEstablishedInfo {
    pub ClientData: usize,
    pub LocalUserId: Id,
    pub RemoteUserId: Id,
    pub SocketId: *const SocketId,
    pub ConnectionType: i32,
    pub NetworkType: i32,
}

#[repr(C)]
pub struct ConnectionClosedInfo {
    pub ClientData: usize,
    pub LocalUserId: Id,
    pub RemoteUserId: Id,
    pub SocketId: *const SocketId,
    pub Reason: i32,
}

#[repr(C)]
pub struct NatTypeInfo {
    pub ResultCode: i32,
    pub ClientData: usize,
    pub NATType: i32,
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use std::mem::offset_of;

    /// Offsets checked against the struct layouts in the SDK's headers (x64).
    #[test]
    fn x64_offsets_match_the_sdk() {
        assert_eq!(offset_of!(CreateLobbyOptions, BucketId), 32);
        assert_eq!(offset_of!(CreateLobbyOptions, LobbyId), 56);
        assert_eq!(offset_of!(CreateLobbyOptions, bEnableJoinById), 64);
        assert_eq!(offset_of!(AttributeData, Value), 16);
        assert_eq!(offset_of!(AttributeData, ValueType), 24);
        assert_eq!(offset_of!(LobbyDetailsInfo, BucketId), 40);
        assert_eq!(offset_of!(LobbyDetailsInfo, AllowedPlatformIdsCount), 80);
        assert_eq!(offset_of!(SendPacketOptions, Channel), 32);
        assert_eq!(offset_of!(SendPacketOptions, DataLengthBytes), 36);
        assert_eq!(offset_of!(SendPacketOptions, Reliability), 52);
        assert_eq!(
            offset_of!(SendPacketOptions, bDisableAutoAcceptConnection),
            56
        );
        assert_eq!(offset_of!(ReceivePacketOptions, RequestedChannel), 24);
        assert_eq!(offset_of!(ConnectionEstablishedInfo, NetworkType), 36);
        assert_eq!(offset_of!(ConnectionClosedInfo, Reason), 32);
        assert_eq!(offset_of!(LobbyMemberStatusInfo, CurrentStatus), 24);
        assert_eq!(offset_of!(NatTypeInfo, NATType), 16);
        assert_eq!(offset_of!(PlatformOptions, ProductId), 16);
        assert_eq!(
            offset_of!(PacketQueueSizeOptions, IncomingPacketQueueMaxSizeBytes),
            8
        );
        assert_eq!(offset_of!(PortRangeOptions, MaxAdditionalPortsToTry), 6);
        assert_eq!(std::mem::size_of::<SocketId>(), 40);
    }
}
