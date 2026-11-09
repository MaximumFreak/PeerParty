"""Two copies: B sets a custom invite and sends it to A, then asks A to join it.
A gets Received + Accepted with B's payload, finalizes the invite, and accepts B's
request to join; B hears the accepted response."""
import ctypes as C, time
from eos_common import *

plat = start()
me = login(plat)
ci = fn("EOS_Platform_GetCustomInvitesInterface", C.c_void_p, C.c_void_p)(plat)

class InviteInfo(C.Structure):
    _fields_ = [("ClientData", C.c_void_p), ("TargetUserId", C.c_void_p), ("LocalUserId", C.c_void_p),
                ("CustomInviteId", C.c_char_p), ("Payload", C.c_char_p)]
class RtjInfo(C.Structure):
    _fields_ = [("ClientData", C.c_void_p), ("FromUserId", C.c_void_p), ("ToUserId", C.c_void_p)]
class RtjResponse(C.Structure):
    _fields_ = [("ClientData", C.c_void_p), ("FromUserId", C.c_void_p), ("ToUserId", C.c_void_p), ("Response", C.c_int32)]
class Done2(C.Structure):
    _fields_ = [("ResultCode", C.c_int32), ("ClientData", C.c_void_p), ("Local", C.c_void_p), ("Target", C.c_void_p)]
class SendDone(C.Structure):
    _fields_ = [("ResultCode", C.c_int32), ("ClientData", C.c_void_p), ("Local", C.c_void_p),
                ("Targets", C.c_void_p), ("Count", C.c_uint32)]
class AddOpts(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32)]

add = lambda name: fn(name, C.c_uint64, C.c_void_p, C.c_void_p, C.c_void_p, CB)
ao = AddOpts(1)
got = {}

if ROLE == "A":
    other = other_id("B", plat)
    add("EOS_CustomInvites_AddNotifyCustomInviteReceived")(ci, C.byref(ao), None,
        cb(InviteInfo, lambda i: got.update(received=(i.CustomInviteId.decode(), i.Payload.decode(), i.TargetUserId, i.LocalUserId))))
    add("EOS_CustomInvites_AddNotifyCustomInviteAccepted")(ci, C.byref(ao), None,
        cb(InviteInfo, lambda i: got.update(accepted=i.Payload.decode())))
    add("EOS_CustomInvites_AddNotifyRequestToJoinReceived")(ci, C.byref(ao), None,
        cb(RtjInfo, lambda i: got.update(rtj=(i.FromUserId, i.ToUserId))))
    open(os.path.join(DIR, "A.ready"), "w").close()
    tick_until(plat, lambda: "accepted" in got, 30, "custom invite")
    rid, payload, sender, local = got.get("received", ("", "", None, None))
    check("invite received with B's payload", payload == "join:lobby-42|pw=hunter2", payload)
    check("sender is B, recipient is me", sender == other and local == me)
    check("accepted after received (auto-accept)", got.get("accepted") == payload)

    class Fin(C.Structure):
        _fields_ = [("ApiVersion", C.c_int32), ("TargetUserId", C.c_void_p), ("LocalUserId", C.c_void_p),
                    ("CustomInviteId", C.c_char_p), ("ProcessingResult", C.c_int32)]
    fin = fn("EOS_CustomInvites_FinalizeInvite", C.c_int32, C.c_void_p, C.c_void_p)
    fo = Fin(1, sender, me, rid.encode(), 0)
    check("FinalizeInvite succeeds once", fin(ci, C.byref(fo)) == 0)
    check("FinalizeInvite again is NotFound", fin(ci, C.byref(fo)) == 18)

    tick_until(plat, lambda: "rtj" in got, 30, "request to join")
    check("request to join from B", got.get("rtj", (None,))[0] == other)
    class LT(C.Structure):
        _fields_ = [("ApiVersion", C.c_int32), ("LocalUserId", C.c_void_p), ("TargetUserId", C.c_void_p)]
    lt = LT(1, me, other)
    fn("EOS_CustomInvites_AcceptRequestToJoin", None, C.c_void_p, C.c_void_p, C.c_void_p, CB)(ci, C.byref(lt), None,
        cb(Done2, lambda i: got.update(acc=i.ResultCode)))
    tick_until(plat, lambda: "acc" in got, 5, "AcceptRequestToJoin")
    check("AcceptRequestToJoin completes", got.get("acc") == 0)
    tick_until(plat, lambda: os.path.exists(os.path.join(DIR, "B.result")), 20, "B to finish")
else:
    other = other_id("A", plat)
    tick_until(plat, lambda: os.path.exists(os.path.join(DIR, "A.ready")), 20, "A ready")
    tick_until(plat, lambda: False, 4, "discovery (expected)")
    add("EOS_CustomInvites_AddNotifyRequestToJoinResponseReceived")(ci, C.byref(ao), None,
        cb(RtjResponse, lambda i: got.update(resp=(i.Response, i.FromUserId, i.ToUserId))))
    class SendOpts(C.Structure):
        _fields_ = [("ApiVersion", C.c_int32), ("LocalUserId", C.c_void_p), ("TargetUserIds", C.POINTER(C.c_void_p)), ("Count", C.c_uint32)]
    targets = (C.c_void_p * 1)(other)
    so = SendOpts(1, me, targets, 1)
    send = fn("EOS_CustomInvites_SendCustomInvite", None, C.c_void_p, C.c_void_p, C.c_void_p, CB)
    send(ci, C.byref(so), None, cb(SendDone, lambda i: got.update(early=i.ResultCode)))
    tick_until(plat, lambda: "early" in got, 5, "send without payload")
    check("SendCustomInvite before SetCustomInvite is InvalidRequest", got.get("early") == 11, got.get("early"))
    class SetOpts(C.Structure):
        _fields_ = [("ApiVersion", C.c_int32), ("LocalUserId", C.c_void_p), ("Payload", C.c_char_p)]
    st = SetOpts(1, me, b"join:lobby-42|pw=hunter2")
    check("SetCustomInvite", fn("EOS_CustomInvites_SetCustomInvite", C.c_int32, C.c_void_p, C.c_void_p)(ci, C.byref(st)) == 0)
    send(ci, C.byref(so), None, cb(SendDone, lambda i: got.update(sent=(i.ResultCode, i.Count))))
    tick_until(plat, lambda: "sent" in got, 5, "SendCustomInvite")
    check("SendCustomInvite completes with Success for 1 target", got.get("sent") == (0, 1), got.get("sent"))
    tick_until(plat, lambda: False, 2, "A handling the invite (expected)")
    class LT(C.Structure):
        _fields_ = [("ApiVersion", C.c_int32), ("LocalUserId", C.c_void_p), ("TargetUserId", C.c_void_p)]
    lt = LT(1, me, other)
    fn("EOS_CustomInvites_SendRequestToJoin", None, C.c_void_p, C.c_void_p, C.c_void_p, CB)(ci, C.byref(lt), None,
        cb(Done2, lambda i: got.update(rtjsent=i.ResultCode)))
    tick_until(plat, lambda: "resp" in got, 30, "request-to-join response")
    check("SendRequestToJoin completes", got.get("rtjsent") == 0)
    check("A accepted the request", got.get("resp", (None,))[0] == 0 and got["resp"][1] == other and got["resp"][2] == me)
finish()
