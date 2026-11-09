"""Lobby voice: A creates a lobby with bEnableRTCRoom (manual audio, so no devices), B
joins it by id. Both get the lobby's RTC room name, see RTCRoomConnectionChanged
(connected) and IsRTCRoomConnected, and meet in that room (ParticipantStatusChanged)."""
import ctypes as C, os, time
from eos_common import *

plat = start()
me = login(plat)
other = "B" if ROLE == "A" else "A"
lobby = fn("EOS_Platform_GetLobbyInterface", C.c_void_p, C.c_void_p)(plat)
rtc = fn("EOS_Platform_GetRTCInterface", C.c_void_p, C.c_void_p)(plat) or 1
LOBBY = b"pptestvoicelobby"

class LocalRtc(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("Flags", C.c_uint32), ("bUseManualAudioInput", C.c_int32),
                ("bUseManualAudioOutput", C.c_int32), ("bStartsMuted", C.c_int32), ("Reserved", C.c_void_p)]
class Create(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("LocalUserId", C.c_void_p), ("MaxLobbyMembers", C.c_uint32),
                ("PermissionLevel", C.c_int32), ("bPresenceEnabled", C.c_int32), ("bAllowInvites", C.c_int32),
                ("BucketId", C.c_char_p), ("bDisableHostMigration", C.c_int32), ("bEnableRTCRoom", C.c_int32),
                ("LocalRTCOptions", C.POINTER(LocalRtc)), ("LobbyId", C.c_char_p), ("bEnableJoinById", C.c_int32),
                ("bRejoin", C.c_int32)]
class JoinById(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("LobbyId", C.c_char_p), ("LocalUserId", C.c_void_p)]
class IdDone(C.Structure):
    _fields_ = [("ResultCode", C.c_int32), ("ClientData", C.c_void_p), ("LobbyId", C.c_char_p)]
class LobbyOpts(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("LobbyId", C.c_char_p), ("LocalUserId", C.c_void_p)]
class Changed(C.Structure):
    _fields_ = [("ClientData", C.c_void_p), ("LobbyId", C.c_char_p), ("LocalUserId", C.c_void_p),
                ("bIsConnected", C.c_int32), ("DisconnectReason", C.c_int32)]
class RoomOpts(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("LocalUserId", C.c_void_p), ("RoomName", C.c_char_p)]
class Status(C.Structure):
    _fields_ = [("ClientData", C.c_void_p), ("LocalUserId", C.c_void_p), ("RoomName", C.c_char_p), ("ParticipantId", C.c_void_p),
                ("ParticipantStatus", C.c_int32)]

def text_id(p):
    buf = C.create_string_buffer(64); n = C.c_int32(64)
    fn("EOS_ProductUserId_ToString", C.c_int32, C.c_void_p, C.c_char_p, C.POINTER(C.c_int32))(p, buf, C.byref(n))
    return buf.value

ev = {"changed": [], "status": []}
done = {}
# Games register for the room's connection before creating or joining, as here.
fn("EOS_Lobby_AddNotifyRTCRoomConnectionChanged", C.c_uint64, C.c_void_p, C.c_void_p, C.c_void_p, CB)(
    lobby, C.byref(C.c_int32(2)), None, cb(Changed, lambda i: ev["changed"].append((i.LobbyId, i.bIsConnected))))
if ROLE == "A":
    lr = LocalRtc(2, 0, 1, 1, 0, None)
    co = Create(8, me, 4, 0, 0, 1, b"bucket", 0, 1, C.pointer(lr), LOBBY, 1, 0)
    fn("EOS_Lobby_CreateLobby", None, C.c_void_p, C.c_void_p, C.c_void_p, CB)(lobby, C.byref(co), None,
        cb(IdDone, lambda i: done.update(lobby=(i.ResultCode, i.LobbyId))))
    tick_until(plat, lambda: "lobby" in done, 5, "CreateLobby")
    check("lobby created", done.get("lobby") == (0, LOBBY), done.get("lobby"))
    open(os.path.join(DIR, "lobby.ready"), "w").write("1")
else:
    tick_until(plat, lambda: os.path.exists(os.path.join(DIR, "lobby.ready")), 20, "A's lobby")
    jo = JoinById(1, LOBBY, me)
    end = time.time() + 15
    while time.time() < end and done.get("join", (1,))[0] != 0:
        done.pop("join", None)
        fn("EOS_Lobby_JoinLobbyById", None, C.c_void_p, C.c_void_p, C.c_void_p, CB)(lobby, C.byref(jo), None,
            cb(IdDone, lambda i: done.update(join=(i.ResultCode, i.LobbyId))))
        tick_until(plat, lambda: "join" in done, 10, "JoinLobbyById")
        if done.get("join", (1,))[0] != 0:
            tick_until(plat, lambda: False, 1, "")
    check("joined A's lobby", done.get("join") == (0, LOBBY), done.get("join"))

buf = C.create_string_buffer(256); n = C.c_uint32(256)
r = fn("EOS_Lobby_GetRTCRoomName", C.c_int32, C.c_void_p, C.c_void_p, C.c_char_p, C.POINTER(C.c_uint32))(
    lobby, C.byref(LobbyOpts(1, LOBBY, me)), buf, C.byref(n))
room = buf.value
check("lobby room name", r == 0 and room == LOBBY + b":peerparty-voice" and n.value == len(room) + 1, (r, room, n.value))
small = C.create_string_buffer(4); n2 = C.c_uint32(4)
r = fn("EOS_Lobby_GetRTCRoomName", C.c_int32, C.c_void_p, C.c_void_p, C.c_char_p, C.POINTER(C.c_uint32))(
    lobby, C.byref(LobbyOpts(1, LOBBY, me)), small, C.byref(n2))
check("short buffer is LimitExceeded with the size needed", r == 22 and n2.value == len(room) + 1, (r, n2.value))
fn("EOS_RTC_AddNotifyParticipantStatusChanged", C.c_uint64, C.c_void_p, C.c_void_p, C.c_void_p, CB)(
    rtc, C.byref(RoomOpts(1, me, room)), None,
    cb(Status, lambda i: ev["status"].append((text_id(i.ParticipantId), i.ParticipantStatus, i.RoomName))))
tick_until(plat, lambda: (LOBBY, 1) in ev["changed"], 10, "RTCRoomConnectionChanged")
check("RTC room connected notification", (LOBBY, 1) in ev["changed"], ev["changed"])
conn = C.c_int32(0)
r = fn("EOS_Lobby_IsRTCRoomConnected", C.c_int32, C.c_void_p, C.c_void_p, C.POINTER(C.c_int32))(
    lobby, C.byref(LobbyOpts(1, LOBBY, me)), C.byref(conn))
check("IsRTCRoomConnected", r == 0 and conn.value == 1, (r, conn.value))
oid = text_id(other_id(other, plat))
tick_until(plat, lambda: (oid, 0, room) in ev["status"], 15, "the other copy in the lobby room")
check("the other copy is in the lobby's room", (oid, 0, room) in ev["status"], ev["status"])
open(os.path.join(DIR, f"{ROLE}.got"), "w").write("1")
tick_until(plat, lambda: os.path.exists(os.path.join(DIR, f"{other}.got")), 10, "the other copy to finish")
tick_until(plat, lambda: False, 1, "")
finish()
os._exit(0)
