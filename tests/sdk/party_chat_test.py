"""PlayFab Party chat between two copies, on a copy of a real Party DLL (PartyWin7.dll
or PartyWin.dll, copied from an installed game): run with
PPTEST_PARTY_DLL = that copy and PEERPARTY_VOICE=0 (no sound devices are opened), under
`peerparty --multi-instance`. The DLL is loaded with ctypes, so every Party lookup
goes through PeerParty's swapped GetProcAddress, as a game's would.

A hosts a network, B joins it; both authenticate and make an endpoint. Before any chat
control exists nothing chat-related happens (the old behaviour). Then each makes a chat
control (audio input and output set to none, and once to the system default to see it
report "not found" with devices off), connects it, sees the other's chat control
arrive, grants permissions, and they swap text (with attached data), including muted
and unmuted text and a send to a local target that must fail. Voice goes without a
microphone: A feeds a capture stream (24 kHz float, as Party's docs fix it) and B takes
A's voice out of a voice stream configured on A's remote chat control, with talking
indicators on both sides. Finally A disconnects and destroys its chat control and B
sees it leave and be destroyed.

API and layouts: Party_c.h from the PlayFab Party C++ SDK for Windows
(nuget.org/packages/Microsoft.PlayFab.PlayFabParty.Cpp.Windows), the reference docs
(learn.microsoft.com/gaming/playfab/features/multiplayer/networking/reference) and the
call order of PlayFab's Unity plugin (github.com/PlayFab/PlayFabPartyUnity)."""
import ctypes as C, math, os, struct, sys, time

DLL = os.environ["PPTEST_PARTY_DLL"]
DIR = os.environ["PPTEST_DIR"]
ROLE = sys.argv[1]
OTHER = "B" if ROLE == "A" else "A"
party = C.CDLL(DLL)

def log(*a):
    with open(os.path.join(DIR, f"{ROLE}.log"), "a", encoding="utf-8") as fh:
        fh.write(" ".join(str(x) for x in a) + "\n")

RESULTS = []
def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok)))
    log("PASS" if ok else "FAIL", name, detail)

def finish():
    bad = [n for n, ok in RESULTS if not ok]
    log("RESULT", "ok" if not bad else "failed: " + ", ".join(bad))
    with open(os.path.join(DIR, f"{ROLE}.result"), "w") as fh:
        fh.write("ok" if not bad else "failed: " + ", ".join(bad))
    sys.exit(0 if not bad else 1)

def flag(name):
    open(os.path.join(DIR, f"{ROLE}.{name}"), "w").close()

P, U32, U8, F32 = C.c_void_p, C.c_uint32, C.c_uint8, C.c_float
def fn(name, *args):
    f = getattr(party, name)
    f.restype = C.c_uint32
    f.argtypes = list(args)
    return f

class NetConfig(C.Structure):
    _fields_ = [("maxUserCount", U32), ("maxDeviceCount", U32), ("maxUsersPerDevice", U32),
                ("maxDevicesPerUser", U32), ("maxEndpointsPerDevice", U32), ("directPeer", U32)]
class DataBuffer(C.Structure):
    _fields_ = [("buffer", P), ("size", U32)]
class AudioFormat(C.Structure):
    _fields_ = [("rate", U32), ("mask", U32), ("channels", C.c_uint16), ("bits", C.c_uint16),
                ("sampleType", U32), ("interleaved", U8)]
class StreamConfig(C.Structure):
    _fields_ = [("format", C.POINTER(AudioFormat)), ("maxMs", U32)]

h = P()
check("PartyInitialize", fn("PartyInitialize", C.c_char_p, C.POINTER(P))(b"61FB1", C.byref(h)) == 0)
H = h.value
user = P()
entity = f"PPTEST-{ROLE}-ENTITY".encode()
check("PartyCreateLocalUser", fn("PartyCreateLocalUser", P, C.c_char_p, C.c_char_p, C.POINTER(P))(
    H, entity, b"token", C.byref(user)) == 0)
USER = user.value

# ---- state changes ----
EVENTS = []   # (type, dict) in arrival order (cleared between phases)
ALL = []      # every one, never cleared
def rd(ptr, off, fmt):
    return struct.unpack_from(fmt, C.string_at(ptr + off, struct.calcsize(fmt)))[0]
def cstr(p):
    return C.string_at(p).decode("utf-8", "replace") if p else None

def parse(ptr):
    t = rd(ptr, 0, "<I")
    q = lambda off: rd(ptr, off, "<Q")
    u = lambda off: rd(ptr, off, "<I")
    d = {}
    if t == 3: d = dict(result=u(4), network=q(384))
    elif t == 4: d = dict(result=u(4), network=q(16))
    elif t == 12: d = dict(network=q(8), endpoint=q(16))
    elif t in (14, 15): d = dict(device=q(8))
    elif t in (16,): d = dict(device=q(8), network=q(16))
    elif t == 31: d = dict(result=u(4), device=q(16), user=q(24), language=cstr(q(32)), chat=q(48))
    elif t == 32: d = dict(result=u(4), chat=q(24))
    elif t == 33: d = dict(chat=q(8))
    elif t == 34: d = dict(chat=q(8), reason=u(16))
    elif t == 36:
        n, recv, size, data = u(16), q(24), u(48), q(56)
        d = dict(sender=q(8), receivers=[rd(recv, 8 * i, "<Q") for i in range(n)], language=cstr(q(32)),
                 text=cstr(q(40)), data=C.string_at(data, size) if size else b"", translations=u(64))
    elif t in (38, 39): d = dict(result=u(4), chat=q(16), selection=u(24), context=cstr(q(32)))
    elif t in (40, 41): d = dict(chat=q(8), state=u(16))
    elif t == 46: d = dict(network=q(8), chat=q(16))
    elif t == 47: d = dict(reason=u(4), network=q(16), chat=q(24))
    elif t in (48, 49): d = dict(result=u(4), network=q(16), chat=q(24))
    elif t in (58, 59, 60): d = dict(result=u(4), chat=q(16), config=q(24))
    return t, d

START = fn("PartyStartProcessingStateChanges", P, C.POINTER(U32), C.POINTER(P))
FINISH = fn("PartyFinishProcessingStateChanges", P, U32, P)
def pump():
    n, arr = U32(), P()
    START(H, C.byref(n), C.byref(arr))
    for i in range(n.value):
        e = parse(rd(arr.value, 8 * i, "<Q"))
        EVENTS.append(e)
        ALL.append(e)
    FINISH(H, n, arr)

def wait(pred, secs, what):
    end = time.time() + secs
    while time.time() < end:
        pump()
        r = pred()
        if r:
            return r
        time.sleep(0.02)
    log("TIMEOUT waiting for", what)
    return None

def find(t, among=None, **kw):
    for ty, d in (EVENTS if among is None else among):
        if ty == t and all(d.get(k) == v for k, v in kw.items()):
            return d
    return None

def wait_flag(name, secs=30):
    path = os.path.join(DIR, f"{OTHER}.{name}")
    return wait(lambda: os.path.exists(path), secs, f"{OTHER}'s {name}")

def types():
    return [t for t, _ in EVENTS]

# ---- network and endpoint (the parts that existed before chat) ----
desc = (C.c_ubyte * 358)()
net = P()
if ROLE == "A":
    cfg = NetConfig(8, 8, 1, 1, 1, 0)
    applied = C.create_string_buffer(128)
    r = fn("PartyCreateNewNetwork", P, P, C.POINTER(NetConfig), U32, P, P, P, P, C.c_char_p)(
        H, USER, C.byref(cfg), 0, None, None, None, C.byref(desc), applied)
    check("PartyCreateNewNetwork", r == 0, r)
    text = C.create_string_buffer(449)
    fn("PartySerializeNetworkDescriptor", P, C.c_char_p)(C.byref(desc), text)
    with open(os.path.join(DIR, "network.txt"), "w") as fh:
        fh.write(text.value.decode())
else:
    path = os.path.join(DIR, "network.txt")
    wait(lambda: os.path.exists(path) and os.path.getsize(path) > 0, 30, "the network descriptor")
    r = fn("PartyDeserializeNetworkDescriptor", C.c_char_p, P)(open(path).read().strip().encode(), C.byref(desc))
    check("PartyDeserializeNetworkDescriptor", r == 0, r)
r = fn("PartyConnectToNetwork", P, P, P, C.POINTER(P))(H, C.byref(desc), None, C.byref(net))
check("PartyConnectToNetwork", r == 0, r)
NET = net.value
check("network connects", wait(lambda: find(3, network=NET, result=0), 30, "ConnectToNetworkCompleted"))
fn("PartyNetworkAuthenticateLocalUser", P, P, C.c_char_p, P)(NET, USER, b"", None)
check("user authenticates", wait(lambda: find(4, network=NET, result=0), 10, "AuthenticateLocalUserCompleted"))
ep = P()
r = fn("PartyNetworkCreateEndpoint", P, P, U32, P, P, P, C.POINTER(P))(NET, USER, 0, None, None, None, C.byref(ep))
check("PartyNetworkCreateEndpoint", r == 0, r)
remote_ep = wait(lambda: next((d for t, d in EVENTS if t == 12 and d["endpoint"] != ep.value), None), 30, "the other endpoint")
check("the other copy's endpoint arrives", remote_ep)
seq = types()
i12 = max(i for i, t in enumerate(seq) if t == 12)
check("remote device created and joined before its endpoint (old order)",
      14 in seq[:i12] and 16 in seq[:i12] and seq.index(14) < seq.index(16), seq)
count, arr = U32(99), P(1)
GET_ALL = fn("PartyGetChatControls", P, C.POINTER(U32), C.POINTER(P))
check("no chat controls before the game makes one", GET_ALL(H, C.byref(count), C.byref(arr)) == 0 and count.value == 0, count.value)
time.sleep(0.5)
pump()
check("no chat state changes without chat", not any(30 <= t <= 60 for t in types()), types())
flag("endpoint")
wait_flag("endpoint")

# ---- chat control ----
dev = P()
fn("PartyGetLocalDevice", P, C.POINTER(P))(H, C.byref(dev))
chat = P()
r = fn("PartyDeviceCreateChatControl", P, P, C.c_char_p, P, C.POINTER(P))(dev.value, USER, None, P(31), C.byref(chat))
check("PartyDeviceCreateChatControl", r == 0 and chat.value, r)
CHAT = chat.value
done = wait(lambda: find(31, chat=CHAT, result=0), 5, "CreateChatControlCompleted")
check("CreateChatControlCompleted with the user and a language", done and done["user"] == USER and done["language"], done)
check("ChatControlCreated for ours", wait(lambda: find(33, chat=CHAT), 5, "ChatControlCreated"))
lang = C.c_char_p()
fn("PartyChatControlGetLanguage", P, C.POINTER(C.c_char_p))(CHAT, C.byref(lang))
check("the language is Windows' (BCP 47)", lang.value and b"-" in lang.value, lang.value)
is_local = U8(9)
fn("PartyChatControlIsLocal", P, C.POINTER(U8))(CHAT, C.byref(is_local))
check("ours is local", is_local.value == 1)
fn("PartyDeviceGetChatControls", P, C.POINTER(U32), C.POINTER(P))(dev.value, C.byref(count), C.byref(arr))
check("the local device lists it", count.value == 1 and rd(arr.value, 0, "<Q") == CHAT, count.value)

SET_IN = fn("PartyChatControlSetAudioInput", P, U32, C.c_char_p, P)
SET_OUT = fn("PartyChatControlSetAudioOutput", P, U32, C.c_char_p, P)
# System default with PEERPARTY_VOICE=0: reported as not found, nothing opened.
check("SetAudioInput(system default)", SET_IN(CHAT, 1, None, P(1)) == 0)
st = wait(lambda: find(40, chat=CHAT), 5, "LocalChatAudioInputChanged")
check("input 'not found' with devices off", st and st["state"] == 2, st)
check("SetChatAudioInputCompleted", wait(lambda: find(38, chat=CHAT, selection=1, result=0), 5, "SetChatAudioInputCompleted"))
EVENTS.clear()
check("SetAudioInput(none)", SET_IN(CHAT, 0, None, P(2)) == 0)
check("SetAudioOutput(none)", SET_OUT(CHAT, 0, None, P(3)) == 0)
st = wait(lambda: find(40, chat=CHAT) and find(41, chat=CHAT), 5, "audio changed")
check("input and output: none", st and find(40, chat=CHAT)["state"] == 0 and find(41, chat=CHAT)["state"] == 0, EVENTS)
sel, ctx, devid = U32(9), C.c_char_p(), C.c_char_p()
fn("PartyChatControlGetAudioInput", P, C.POINTER(U32), C.POINTER(C.c_char_p), C.POINTER(C.c_char_p))(
    CHAT, C.byref(sel), C.byref(ctx), C.byref(devid))
check("GetAudioInput: none, no device", sel.value == 0 and devid.value == b"", (sel.value, devid.value))
ind = U32(9)
fn("PartyChatControlGetLocalChatIndicator", P, C.POINTER(U32))(CHAT, C.byref(ind))
check("local indicator: no audio input", ind.value == 3, ind.value)

check("PartyNetworkConnectChatControl", fn("PartyNetworkConnectChatControl", P, P, P)(NET, CHAT, P(48)) == 0)
check("ConnectChatControlCompleted", wait(lambda: find(48, network=NET, chat=CHAT, result=0), 5, "ConnectChatControlCompleted"))
check("ChatControlJoinedNetwork for ours", wait(lambda: find(46, network=NET, chat=CHAT), 5, "joined"))

# The other copy's chat control shows up (maybe already: it's shown once we have a
# chat control and are authenticated, connected or not).
def remote_created():
    for t, d in ALL:
        if t == 33 and d["chat"] != CHAT:
            return d["chat"]
RCHAT = wait(remote_created, 30, "the other copy's chat control")
check("the other copy's chat control arrives", RCHAT)
check("and joins our network", wait(lambda: find(46, ALL, network=NET, chat=RCHAT), 5, "remote joined"))
seq = [t for t, d in ALL if d.get("chat") == RCHAT]
check("created before joined", seq[:2] == [33, 46], seq)
eid = C.c_char_p()
fn("PartyChatControlGetEntityId", P, C.POINTER(C.c_char_p))(RCHAT, C.byref(eid))
check("it is the other copy's user", eid.value == f"PPTEST-{OTHER}-ENTITY".encode(), eid.value)
fn("PartyChatControlIsLocal", P, C.POINTER(U8))(RCHAT, C.byref(is_local))
check("it is remote", is_local.value == 0)
rdev = P()
fn("PartyChatControlGetDevice", P, C.POINTER(P))(RCHAT, C.byref(rdev))
check("on the other copy's device", rdev.value and rdev.value != dev.value)
fn("PartyNetworkGetChatControls", P, C.POINTER(U32), C.POINTER(P))(NET, C.byref(count), C.byref(arr))
check("the network lists both", count.value == 2, count.value)

SEND_TEXT = fn("PartyChatControlSendText", P, U32, C.POINTER(P), C.c_char_p, U32, C.POINTER(DataBuffer))
targets = (P * 1)(RCHAT)
# No permissions yet: text to us isn't delivered (checked by the other side), and a
# local target fails right away.
mine = (P * 1)(CHAT)
check("SendText to a local chat control fails", SEND_TEXT(CHAT, 1, mine, b"x", 0, None) != 0)
perm = U32(9)
fn("PartyChatControlGetPermissions", P, P, C.POINTER(U32))(CHAT, RCHAT, C.byref(perm))
check("permissions start at none", perm.value == 0, perm.value)
SET_PERM = fn("PartyChatControlSetPermissions", P, P, U32)
check("SetPermissions(all)", SET_PERM(CHAT, RCHAT, 0x1F) == 0)
flag("perm")
wait_flag("perm")

def texts():
    return [d for t, d in EVENTS if t == 36]

if ROLE == "A":
    payload = (C.c_ubyte * 3)(1, 2, 3)
    bufs = (DataBuffer * 1)(DataBuffer(C.cast(payload, P), 3))
    check("SendText", SEND_TEXT(CHAT, 1, targets, "hello from A \u2713".encode(), 1, bufs) == 0)
    got = wait(lambda: texts(), 15, "B's reply")
    check("B's reply arrives", got and got[0]["text"] == "hi from B" and got[0]["sender"] == RCHAT
          and got[0]["receivers"] == [CHAT] and got[0]["data"] == b"", got)
    wait_flag("muted")
    check("SendText while B mutes us", SEND_TEXT(CHAT, 1, targets, b"you should not see this", 0, None) == 0)
    flag("sent-muted")
    wait_flag("unmuted")
    check("SendText after B unmutes", SEND_TEXT(CHAT, 1, targets, b"after unmute", 0, None) == 0)
else:
    got = wait(lambda: texts(), 15, "A's text")
    ok = got and got[0]["text"] == "hello from A \u2713" and got[0]["sender"] == RCHAT and got[0]["receivers"] == [CHAT] \
        and got[0]["data"] == b"\x01\x02\x03" and got[0]["language"] and got[0]["translations"] == 0
    check("A's text arrives with its data, sender and language", ok, got)
    check("SendText reply", SEND_TEXT(CHAT, 1, targets, b"hi from B", 0, None) == 0)
    MUTE_TEXT = fn("PartyChatControlSetIncomingTextMuted", P, P, U8)
    check("SetIncomingTextMuted", MUTE_TEXT(CHAT, RCHAT, 1) == 0)
    EVENTS.clear()
    flag("muted")
    wait_flag("sent-muted")
    wait(lambda: False, 1.5, "(muted text, expected to stay away)")
    check("muted text isn't delivered", not texts(), texts())
    MUTE_TEXT(CHAT, RCHAT, 0)
    flag("unmuted")
    got = wait(lambda: texts(), 15, "text after unmuting")
    check("text after unmuting arrives", got and got[0]["text"] == "after unmute", got)

# ---- voice without a microphone: A's capture stream -> B's voice stream ----
ind_remote = U32(9)
GET_IND = fn("PartyChatControlGetChatIndicator", P, P, C.POINTER(U32))
if ROLE == "A":
    wait_flag("voice-ready")
    cfg = StreamConfig(None, 200)
    check("ConfigureAudioManipulationCaptureStream", fn("PartyChatControlConfigureAudioManipulationCaptureStream",
          P, C.POINTER(StreamConfig), P)(CHAT, C.byref(cfg), P(59)) == 0)
    check("its completion", wait(lambda: find(59, chat=CHAT, result=0), 5, "capture configured"))
    stream = P()
    fn("PartyChatControlGetAudioManipulationCaptureStream", P, C.POINTER(P))(CHAT, C.byref(stream))
    fmt = AudioFormat()
    fn("PartyAudioManipulationSinkStreamGetFormat", P, C.POINTER(AudioFormat))(stream.value, C.byref(fmt))
    check("capture format is 24 kHz mono float", (fmt.rate, fmt.channels, fmt.bits, fmt.sampleType, fmt.interleaved) == (24000, 1, 32, 1, 0),
          (fmt.rate, fmt.channels, fmt.bits, fmt.sampleType, fmt.interleaved))
    SUBMIT = fn("PartyAudioManipulationSinkStreamSubmitBuffer", P, C.POINTER(DataBuffer))
    samples = (C.c_float * 960)(*[0.3 * math.sin(2 * math.pi * 440 * i / 24000) for i in range(960)])
    talking = False
    for _ in range(40):
        r = SUBMIT(stream.value, C.byref(DataBuffer(C.cast(samples, P), 960 * 4)))
        fn("PartyChatControlGetLocalChatIndicator", P, C.POINTER(U32))(CHAT, C.byref(ind))
        talking |= ind.value == 1
        pump()
        time.sleep(0.04)
    check("SubmitBuffer", r == 0, r)
    check("local indicator: talking while sending", talking)
    flag("voice-sent")
else:
    cfg = StreamConfig(None, 2000)
    check("ConfigureAudioManipulationVoiceStream on A's chat control", fn("PartyChatControlConfigureAudioManipulationVoiceStream",
          P, C.POINTER(StreamConfig), P)(RCHAT, C.byref(cfg), P(58)) == 0)
    check("its completion", wait(lambda: find(58, chat=RCHAT, result=0), 5, "voice configured"))
    stream = P()
    fn("PartyChatControlGetAudioManipulationVoiceStream", P, C.POINTER(P))(RCHAT, C.byref(stream))
    fmt = AudioFormat()
    fn("PartyAudioManipulationSourceStreamGetFormat", P, C.POINTER(AudioFormat))(stream.value, C.byref(fmt))
    check("voice format is 24 kHz mono 16-bit", (fmt.rate, fmt.channels, fmt.bits, fmt.sampleType) == (24000, 1, 16, 0),
          (fmt.rate, fmt.channels, fmt.bits, fmt.sampleType))
    flag("voice-ready")
    COUNT = fn("PartyAudioManipulationSourceStreamGetAvailableBufferCount", P, C.POINTER(U32))
    NEXT = fn("PartyAudioManipulationSourceStreamGetNextBuffer", P, C.POINTER(DataBuffer))
    RETURN = fn("PartyAudioManipulationSourceStreamReturnBuffer", P, P)
    got, sizes, peak, talking = 0, set(), 0, False
    end = time.time() + 20
    while time.time() < end and got < 20:
        pump()
        GET_IND(CHAT, RCHAT, C.byref(ind_remote))
        talking |= ind_remote.value == 1
        n = U32()
        COUNT(stream.value, C.byref(n))
        for _ in range(n.value):
            b = DataBuffer()
            if NEXT(stream.value, C.byref(b)) == 0:
                got += 1
                sizes.add(b.size)
                pcm = struct.unpack(f"<{b.size // 2}h", C.string_at(b.buffer, b.size))
                peak = max(peak, max(abs(x) for x in pcm))
                RETURN(stream.value, b.buffer)
        time.sleep(0.02)
    check("A's voice comes out of the voice stream", got >= 10 and sizes == {1920} and 8000 < peak < 11000, (got, sizes, peak))
    check("A shows as talking", talking)
    b = DataBuffer()
    wait_flag("voice-sent")
    time.sleep(0.5)
    pump()
    while NEXT(stream.value, C.byref(b)) == 0:
        RETURN(stream.value, b.buffer)
    check("no buffer once A stops", NEXT(stream.value, C.byref(b)) != 0 and not b.buffer)
    # Muting A's audio flips the indicator.
    fn("PartyChatControlSetIncomingAudioMuted", P, P, U8)(CHAT, RCHAT, 1)
    GET_IND(CHAT, RCHAT, C.byref(ind_remote))
    check("indicator: muted", ind_remote.value == 3, ind_remote.value)
    fn("PartyChatControlSetIncomingAudioMuted", P, P, U8)(CHAT, RCHAT, 0)
    SET_PERM(CHAT, RCHAT, 0x10)
    GET_IND(CHAT, RCHAT, C.byref(ind_remote))
    check("indicator: voice disabled without receive permission", ind_remote.value == 2, ind_remote.value)

# ---- A's chat control goes away ----
flag("voice-done")
wait_flag("voice-done")
EVENTS.clear()
if ROLE == "A":
    check("PartyNetworkDisconnectChatControl", fn("PartyNetworkDisconnectChatControl", P, P, P)(NET, CHAT, P(49)) == 0)
    check("left, then disconnect completed", wait(lambda: find(49, chat=CHAT, result=0), 5, "disconnected") and
          types().index(47) < types().index(49), types())
    check("PartyDeviceDestroyChatControl", fn("PartyDeviceDestroyChatControl", P, P, P)(dev.value, CHAT, P(32)) == 0)
    check("destroy completed, then destroyed", wait(lambda: find(32, chat=CHAT, result=0) and find(34, chat=CHAT), 5, "destroyed"))
    check("GetChatControls: only B's remains", GET_ALL(H, C.byref(count), C.byref(arr)) == 0 and count.value == 1
          and rd(arr.value, 0, "<Q") == RCHAT, count.value)
    flag("chat-gone")
else:
    check("A's chat control leaves the network", wait(lambda: find(47, chat=RCHAT, network=NET), 15, "A's chat left"))
    check("and is destroyed (disconnected)", wait(lambda: find(34, chat=RCHAT), 5, "A's chat destroyed") and find(34, chat=RCHAT)["reason"] == 1)
    check("A's endpoint is still here", not any(t == 13 for t in types()), types())
    wait_flag("chat-gone")

flag("done")
wait_flag("done")
fn("PartyNetworkLeaveNetwork", P, P)(NET, None)
check("network left", wait(lambda: find(3) is None and any(t == 20 for t in types()), 10, "NetworkDestroyed"))
fn("PartyCleanup", P)(H)
finish()
