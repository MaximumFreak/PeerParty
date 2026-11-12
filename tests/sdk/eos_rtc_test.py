"""Two copies in one EOS RTC room: both join (manual audio, so no sound devices), see
each other join, A sends audio frames and room data, B gets them through
AudioBeforeRender / DataReceived with A as the participant and sees A speaking; then B
leaves and A sees it go. Also checks the device list and the completions."""
import ctypes as C, os, time
from eos_common import *

plat = start()
me = login(plat)
other = "B" if ROLE == "A" else "A"
rtc = fn("EOS_Platform_GetRTCInterface", C.c_void_p, C.c_void_p)(plat) or 1
audio = fn("EOS_RTC_GetAudioInterface", C.c_void_p, C.c_void_p)(rtc)
data = fn("EOS_RTC_GetDataInterface", C.c_void_p, C.c_void_p)(rtc)
check("audio and data interfaces", audio and data, (audio, data))
ROOM = b"pptest-room"

class RoomOpts(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("LocalUserId", C.c_void_p), ("RoomName", C.c_char_p)]
class JoinOpts(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("LocalUserId", C.c_void_p), ("RoomName", C.c_char_p),
                ("ClientBaseUrl", C.c_char_p), ("ParticipantToken", C.c_char_p), ("ParticipantId", C.c_void_p),
                ("Flags", C.c_uint32), ("bManualAudioInputEnabled", C.c_int32), ("bManualAudioOutputEnabled", C.c_int32)]
class RoomDone(C.Structure):
    _fields_ = [("ResultCode", C.c_int32), ("ClientData", C.c_void_p), ("LocalUserId", C.c_void_p), ("RoomName", C.c_char_p),
                ("Tail", C.c_int32)]
class Status(C.Structure):
    _fields_ = [("ClientData", C.c_void_p), ("LocalUserId", C.c_void_p), ("RoomName", C.c_char_p), ("ParticipantId", C.c_void_p),
                ("ParticipantStatus", C.c_int32), ("MetaCount", C.c_uint32), ("Meta", C.c_void_p), ("bInBlocklist", C.c_int32)]
class Speaking(C.Structure):
    _fields_ = [("ClientData", C.c_void_p), ("LocalUserId", C.c_void_p), ("RoomName", C.c_char_p), ("ParticipantId", C.c_void_p),
                ("bSpeaking", C.c_int32), ("AudioStatus", C.c_int32)]
class Buffer(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("Frames", C.POINTER(C.c_int16)), ("FramesCount", C.c_uint32),
                ("SampleRate", C.c_uint32), ("Channels", C.c_uint32)]
class Render(C.Structure):
    _fields_ = [("ClientData", C.c_void_p), ("LocalUserId", C.c_void_p), ("RoomName", C.c_char_p),
                ("Buffer", C.POINTER(Buffer)), ("ParticipantId", C.c_void_p)]
class SendAudio(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("LocalUserId", C.c_void_p), ("RoomName", C.c_char_p), ("Buffer", C.POINTER(Buffer))]
class DataGot(C.Structure):
    _fields_ = [("ClientData", C.c_void_p), ("LocalUserId", C.c_void_p), ("RoomName", C.c_char_p),
                ("DataLengthBytes", C.c_uint32), ("Data", C.c_void_p), ("ParticipantId", C.c_void_p)]
class SendData(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("LocalUserId", C.c_void_p), ("RoomName", C.c_char_p),
                ("DataLengthBytes", C.c_uint32), ("Data", C.c_void_p)]
class StatusOpts(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("LocalUserId", C.c_void_p), ("RoomName", C.c_char_p), ("Status", C.c_int32)]
class IndexOpts(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("DeviceIndex", C.c_uint32)]
class DeviceInfo(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("bDefaultDevice", C.c_int32), ("DeviceId", C.c_char_p), ("DeviceName", C.c_char_p)]

def text_id(p):
    buf = C.create_string_buffer(64); n = C.c_int32(64)
    fn("EOS_ProductUserId_ToString", C.c_int32, C.c_void_p, C.c_char_p, C.POINTER(C.c_int32))(p, buf, C.byref(n))
    return buf.value

add = lambda name: fn(name, C.c_uint64, C.c_void_p, C.c_void_p, C.c_void_p, CB)
ro = RoomOpts(1, me, ROOM)
ev = {"status": [], "speaking": [], "frames": [], "data": []}
add("EOS_RTC_AddNotifyParticipantStatusChanged")(rtc, C.byref(ro), None,
    cb(Status, lambda i: ev["status"].append((text_id(i.ParticipantId), i.ParticipantStatus, i.RoomName))))
add("EOS_RTCAudio_AddNotifyParticipantUpdated")(audio, C.byref(ro), None,
    cb(Speaking, lambda i: ev["speaking"].append((text_id(i.ParticipantId), i.bSpeaking))))
def on_render(i):
    b = i.Buffer.contents
    ev["frames"].append((text_id(i.ParticipantId), b.FramesCount, b.SampleRate, b.Channels, b.Frames[0], b.Frames[b.FramesCount - 1]))
add("EOS_RTCAudio_AddNotifyAudioBeforeRender")(audio, C.byref(ro), None, cb(Render, on_render))
add("EOS_RTCData_AddNotifyDataReceived")(data, C.byref(ro), None,
    cb(DataGot, lambda i: ev["data"].append((text_id(i.ParticipantId), C.string_at(i.Data, i.DataLengthBytes)))))

# devices
# Windows' own microphones, each with an id and a name, the system default marked.
n = fn("EOS_RTCAudio_GetInputDevicesCount", C.c_uint32, C.c_void_p, C.c_void_p)(audio, C.byref(IndexOpts(1, 0)))
listed = []
for i in range(n + 1):
    pinfo = C.POINTER(DeviceInfo)()
    r = fn("EOS_RTCAudio_CopyInputDeviceInformationByIndex", C.c_int32, C.c_void_p, C.c_void_p, C.c_void_p)(
        audio, C.byref(IndexOpts(1, i)), C.byref(pinfo))
    if r != 0:
        listed.append(r)
        break
    d = pinfo.contents
    listed.append((d.DeviceId, d.DeviceName, d.bDefaultDevice))
    fn("EOS_RTCAudio_InputDeviceInformation_Release", None, C.c_void_p)(pinfo)
devices, past_end = listed[:-1], listed[-1]
check("Windows' input devices, named, one default",
      len(devices) == n and past_end != 0 and all(i and nm for i, nm, _ in devices)
      and (n == 0 or sum(d for _, _, d in devices) == 1), (n, listed))

done = {}
jo = JoinOpts(1, me, ROOM, b"https://localhost", b"token", me, 0x4, 1, 1)
fn("EOS_RTC_JoinRoom", None, C.c_void_p, C.c_void_p, C.c_void_p, CB)(rtc, C.byref(jo), None,
    cb(RoomDone, lambda i: done.update(join=(i.ResultCode, i.RoomName))))
tick_until(plat, lambda: "join" in done, 5, "JoinRoom")
check("JoinRoom completes", done.get("join") == (0, ROOM), done.get("join"))

oid = text_id(other_id(other, plat))
tick_until(plat, lambda: (oid, 0, ROOM) in ev["status"], 20, "the other copy in the room")
check("the other copy joins the room", (oid, 0, ROOM) in ev["status"], ev["status"])

so = StatusOpts(1, me, ROOM, 1)
fn("EOS_RTCAudio_UpdateSending", None, C.c_void_p, C.c_void_p, C.c_void_p, CB)(audio, C.byref(so), None,
    cb(RoomDone, lambda i: done.update(sending=(i.ResultCode, i.Tail))))
tick_until(plat, lambda: "sending" in done, 5, "UpdateSending")
check("UpdateSending completes", done.get("sending") == (0, 1), done.get("sending"))

big = C.create_string_buffer(2000)
r = fn("EOS_RTCData_SendData", C.c_int32, C.c_void_p, C.c_void_p)(data, C.byref(SendData(1, me, ROOM, 1171, C.cast(big, C.c_void_p))))
check("SendData over 1170 bytes is refused", r == 22, r)

flag = lambda role: os.path.join(DIR, f"{role}.got")
if ROLE == "A":
    samples = (C.c_int16 * 480)(*[(i * 10) % 30000 for i in range(480)])
    buf = Buffer(1, samples, 480, 48000, 1)
    payload = b"hello-rtc"
    end = time.time() + 15
    while time.time() < end and not os.path.exists(flag("B")):
        r1 = fn("EOS_RTCAudio_SendAudio", C.c_int32, C.c_void_p, C.c_void_p)(audio, C.byref(SendAudio(1, me, ROOM, C.pointer(buf))))
        r2 = fn("EOS_RTCData_SendData", C.c_int32, C.c_void_p, C.c_void_p)(
            data, C.byref(SendData(1, me, ROOM, len(payload), C.cast(C.c_char_p(payload), C.c_void_p))))
        tick_until(plat, lambda: False, 0.1, "")
    check("SendAudio and SendData succeed", r1 == 0 and r2 == 0, (r1, r2))
    check("B got the audio and data", os.path.exists(flag("B")))
    tick_until(plat, lambda: (oid, 1, ROOM) in ev["status"], 15, "B leaving")
    check("B's leaving is seen", (oid, 1, ROOM) in ev["status"], ev["status"])
else:
    tick_until(plat, lambda: len(ev["frames"]) >= 3 and ev["data"], 15, "audio and data from A")
    f = ev["frames"][0] if ev["frames"] else None
    check("audio frames arrive from A", f and f[0] == oid and f[1:4] == (480, 48000, 1) and f[4] == 0 and f[5] == 4790, f)
    check("room data arrives from A", ev["data"] and ev["data"][0] == (oid, b"hello-rtc"), ev["data"][:1])
    check("A shows as speaking", (oid, 1) in ev["speaking"], ev["speaking"])
    open(flag("B"), "w").write("1")
    tick_until(plat, lambda: (oid, 0) in ev["speaking"], 5, "A to stop speaking")
    fn("EOS_RTC_LeaveRoom", None, C.c_void_p, C.c_void_p, C.c_void_p, CB)(rtc, C.byref(ro), None,
        cb(RoomDone, lambda i: done.update(leave=i.ResultCode)))
    tick_until(plat, lambda: "leave" in done, 5, "LeaveRoom")
    check("LeaveRoom completes", done.get("leave") == 0, done.get("leave"))
    check("A stops speaking after its last frame", (oid, 0) in ev["speaking"], ev["speaking"])
    tick_until(plat, lambda: False, 2, "")
finish()
os._exit(0)
