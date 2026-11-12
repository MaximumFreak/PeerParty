"""Two copies in one Vivox positional channel through PeerParty's stand-in (hook/src/vivox.rs):
run as `run_pair.ps1 vivox_test.py -Env PEERPARTY_VIVOX=1,PEERPARTY_VOICE=0`.

Drives a game's real VivoxNative.dll (Unity's Vivox package, its SWIG wrappers) the way
the package's C# does (com.unity.services.vivox: VxClient, Client, LoginSession,
ChannelSession, AudioInputDevices): string fields through the wrappers' setters, messages
polled with vx_get_message, response fields and event fields read back through the
wrappers' getters. So this checks the struct offsets in vivox.rs against VivoxNative's
own accessors, and that VivoxNative's imports of vivoxsdk.dll reach PeerParty (the real
SDK would refuse everything here: it is never initialized).

Both log in, join one positional channel, and see their own participant and then each
other's (with the URI Unity's package builds for an account); both set a 3D position;
devices list; B leaves and A sees it go; both log out. PPTEST_VIVOX_DIR: a folder with a
game's VivoxNative.dll and vivoxsdk.dll (64-bit)."""
import ctypes as C, os, sys, time

DIR = os.environ["PPTEST_DIR"]
ROLE = sys.argv[1]
OTHER = "B" if ROLE == "A" else "A"
RESULTS = []

def log(*a):
    with open(os.path.join(DIR, f"{ROLE}.log"), "a", encoding="utf-8") as fh:
        fh.write(" ".join(str(x) for x in a) + "\n")

def check(name, ok, detail=""):
    RESULTS.append(bool(ok)); log("PASS" if ok else "FAIL", name, detail)
    return ok

def finish():
    res = "ok" if RESULTS and all(RESULTS) else "failed"
    log("RESULT", res)
    open(os.path.join(DIR, f"{ROLE}.result"), "w").write(res)
    os._exit(0)

def _crash(kind, value, tb):
    import traceback
    log("FAIL exception", "".join(traceback.format_exception(kind, value, tb)))
    open(os.path.join(DIR, f"{ROLE}.result"), "w").write("failed")
    os._exit(1)
sys.excepthook = _crash

here = os.environ["PPTEST_VIVOX_DIR"]
os.add_dll_directory(here)
native = C.CDLL(os.path.join(here, "VivoxNative.dll"))

def w(name, res, *args):
    full = name if name.startswith("SWIG") else f"CSharp_UnityfServicesfVivox_{name}___"
    f = getattr(native, full)
    f.restype, f.argtypes = res, list(args)
    return f

# The wrappers hand strings back through C#'s string helper: here it returns the pointer.
STRCB = C.CFUNCTYPE(C.c_void_p, C.c_void_p)
_keep = STRCB(lambda p: p)
w("SWIGRegisterStringCallback_VivoxCoreInstance", None, STRCB)(_keep)

P, I, S = C.c_void_p, C.c_int, C.c_char_p
def text(p):
    return C.string_at(p).decode() if p else None

def setter(struct, field, kind=S):
    f = w(f"{struct}_{field}_set", None, P, kind)
    return lambda obj, v: f(obj, v.encode() if isinstance(v, str) else v)

def getter(struct, field, kind=I):
    f = w(f"{struct}_{field}_get", P if kind is S else kind, P)
    return (lambda obj: text(f(obj))) if kind is S else f

def new(struct):
    return w(f"new_{struct}", P)()

base_of = lambda struct: w(f"{struct}_base__get", P, P)
cookie_set = setter("vx_req_base_t", "cookie")
issue = w("IssueRequest", I, P)
next_cookie = [0]

def send(struct, **fields):
    req = new(struct)
    for k, v in fields.items():
        setter(struct, k, I if isinstance(v, int) else S)(req, v)
    b = base_of(struct)(req)
    next_cookie[0] += 1
    cookie = str(next_cookie[0])
    cookie_set(b, cookie)
    r = issue(b)
    return cookie, r

msg_type = getter("vx_message_base_t", "type")
as_resp = w("vx_message_base_t_as_resp_base", P, P)
as_evt = w("vx_message_base_t_as_evt_base", P, P)
resp_type, resp_rc = getter("vx_resp_base_t", "type"), getter("vx_resp_base_t", "return_code")
resp_req = w("vx_resp_base_t_request_get", P, P)
req_cookie = getter("vx_req_base_t", "cookie", S)
evt_type = getter("vx_evt_base_t", "type")
get_message = w("vx_get_message__SWIG_0", P)

responses, events = {}, []
def pump(seconds=0.05):
    end = time.time() + seconds
    while True:
        m = get_message()
        while m:
            t = msg_type(m)
            if t == 2:
                r = as_resp(m)
                responses[req_cookie(resp_req(r))] = (resp_type(r), resp_rc(r), r)
            elif t == 3:
                e = as_evt(m)
                events.append((evt_type(e), e))
            m = get_message()
        if time.time() >= end:
            return
        time.sleep(0.02)

def wait(pred, seconds, what):
    end = time.time() + seconds
    while time.time() < end:
        pump(0.05)
        got = pred()
        if got:
            return got
    log("timed out waiting for", what)
    return None

def answered(cookie, seconds=5):
    return wait(lambda: responses.get(cookie), seconds, f"response {cookie}")

def event(kind, test, seconds=10, what=""):
    def find():
        for k, e in events:
            if k == kind and test(e):
                return e
    return wait(find, seconds, what or f"event {kind}")

# -- start (VxClient.Start: vx_get_default_config3, then vx_initialize3) --
config = new("vx_sdk_config_t")
check("initialize", w("Initialize", I, P)(config) == 0)
check("initialized", w("vx_is_initialized", I)() == 1)

# -- connector (Client.BeginGetConnectorHandle) --
c, r = send("vx_req_connector_create_t", connector_handle="C1", acct_mgmt_server="https://unity.vivox.com/appconfig/pptest")
resp = answered(c)
check("connector created", r == 0 and resp and resp[0] == 1 and resp[1] == 0, (r, resp and resp[:2]))
if resp:
    conn = w("vx_resp_base_t_as_vx_resp_connector_create", P, P)(resp[2])
    check("no default realm", getter("vx_resp_connector_create_t", "default_realm", S)(conn) is None)

# -- login (LoginSession.Login) --
ACCT = f".pptest-dev.player{ROLE}.0f7c3c43-0d1d-4b6e-9a3e-1f2b3c4d5e6f."
c, r = send("vx_req_account_anonymous_login_t", account_handle="A1", connector_handle="C1", acct_name=ACCT,
            displayname=f"Player {ROLE}", access_token="peerparty-token", participant_property_frequency=100)
check("login answered", answered(c) and responses[c][1] == 0)
login_state = getter("vx_evt_account_login_state_change_t", "state")
login_acct = getter("vx_evt_account_login_state_change_t", "account_handle", S)
check("logged in", event(2, lambda e: login_state(e) == 1 and login_acct(e) == "A1", 5, "logged in"))
c, r = send("vx_req_sessiongroup_set_tx_all_sessions_t", sessiongroup_handle="")
check("transmit to all", answered(c) and responses[c][1] == 0)

# -- join a positional channel (ChannelSession.BeginConnect) --
DOMAIN = "mtu1xp.vivox.com"
CHANNEL = f"sip:confctl-d-pptest-dev.dungeon.0f7c3c43-0d1d-4b6e-9a3e-1f2b3c4d5e6f!p-32-1-1.000-1@{DOMAIN}"
c, r = send("vx_req_sessiongroup_add_session_t", account_handle="A1", uri=CHANNEL, session_handle="S1",
            sessiongroup_handle="G1", connect_audio=1, connect_text=1, access_token="peerparty-token")
check("join answered", r == 0 and answered(c) and responses[c][1] == 0)
media_state = getter("vx_evt_media_stream_updated_t", "state")
media_session = getter("vx_evt_media_stream_updated_t", "session_handle", S)
text_state = getter("vx_evt_text_stream_updated_t", "state")
check("audio connected", event(20, lambda e: media_session(e) == "S1" and media_state(e) == 2, 5, "audio connected"))
check("text connected", event(21, lambda e: text_state(e) == 1, 5, "text connected"))
p_uri = getter("vx_evt_participant_added_t", "participant_uri", S)
p_self = getter("vx_evt_participant_added_t", "is_current_user")
p_name = getter("vx_evt_participant_added_t", "displayname", S)
p_session = getter("vx_evt_participant_added_t", "session_handle", S)
ME = f"sip:{ACCT}@{DOMAIN}"
THEM = f"sip:.pptest-dev.player{OTHER}.0f7c3c43-0d1d-4b6e-9a3e-1f2b3c4d5e6f.@{DOMAIN}"
me = event(26, lambda e: p_self(e) == 1, 5, "own participant")
check("own participant, Unity's account URI", me and p_uri(me) == ME and p_session(me) == "S1", me and (p_uri(me), p_session(me)))
them = event(26, lambda e: p_self(e) == 0, 30, "the other copy's participant")
check("the other copy's participant", them and p_uri(them) == THEM and p_name(them) == f"Player {OTHER}", them and (p_uri(them), p_name(them)))

# -- a 3D position (ChannelSession.Set3DPosition) --
F3 = C.c_float * 3
req = new("vx_req_session_set_3d_position_t")
setter("vx_req_session_set_3d_position_t", "session_handle")(req, "S1")
w("vx_req_session_set_3d_position_t_Set3DPosition", None, P, F3, F3, F3, F3)(
    req, F3(1.0 if ROLE == "A" else 4.0, 0, 2), F3(1.0 if ROLE == "A" else 4.0, 0, 2), F3(0, 0, 1), F3(0, 1, 0))
b = base_of("vx_req_session_set_3d_position_t")(req)
cookie_set(b, "pos")
check("position set", issue(b) == 0 and answered("pos") and responses["pos"][1] == 0)

# -- devices (AudioInputDevices.BeginRefresh) --
c, r = send("vx_req_aux_get_capture_devices_t")
resp = answered(c)
if check("capture devices", resp and resp[1] == 0):
    caps = w("vx_resp_base_t_as_vx_resp_aux_get_capture_devices", P, P)(resp[2])
    n = getter("vx_resp_aux_get_capture_devices_t", "count")(caps)
    devs = w("vx_resp_aux_get_capture_devices_t_capture_devices_get", P, P)(caps)
    key = getter("vx_device_t", "device", S)
    keys = [key(w("get_device", P, I, P)(i, devs)) for i in range(n)]
    cur = w("vx_resp_aux_get_capture_devices_t_current_capture_device_get", P, P)(caps)
    eff = w("vx_resp_aux_get_capture_devices_t_effective_capture_device_get", P, P)(caps)
    check("Vivox's two defaults, then Windows' microphones",
          keys[:2] == ["Default System Device", "Default Communication Device"], keys)
    check("the system default is current, a listed device effective",
          key(cur) == "Default System Device" and (key(eff) in keys[2:] or n == 2), (key(cur), key(eff)))
    if n > 2:
        # Choosing a device by its key (AudioInputDevices.BeginSetActiveDevice).
        c, r = send("vx_req_aux_set_capture_device_t", account_handle="A1", capture_device_specifier=keys[-1])
        resp = answered(c)
        opened = resp and getter("vx_resp_aux_set_capture_device_t", "open_capture_device_guid", S)(
            w("vx_resp_base_t_as_vx_resp_aux_set_capture_device", P, P)(resp[2]))
        check("a device chosen and opened", resp and resp[1] == 0 and opened == keys[-1], opened)
        c, r = send("vx_req_aux_get_capture_devices_t")
        resp = answered(c)
        caps = resp and w("vx_resp_base_t_as_vx_resp_aux_get_capture_devices", P, P)(resp[2])
        check("then listed as current", caps and key(w("vx_resp_aux_get_capture_devices_t_current_capture_device_get", P, P)(caps)) == keys[-1])
        c, r = send("vx_req_aux_set_capture_device_t", account_handle="A1", capture_device_specifier="Default System Device")
        check("back to the system default", answered(c) and responses[c][1] == 0)
c, r = send("vx_req_connector_mute_local_mic_t", account_handle="A1", mute_level=1)
check("microphone muted", answered(c) and responses[c][1] == 0)
c, r = send("vx_req_connector_mute_local_mic_t", account_handle="A1", mute_level=0)

# -- B leaves (ChannelSession.Disconnect); A sees it --
removed_uri = getter("vx_evt_participant_removed_t", "participant_uri", S)
if ROLE == "B":
    pump(2.0)
    c, r = send("vx_req_sessiongroup_remove_session_t", session_handle="S1", sessiongroup_handle="G1")
    check("leave answered", answered(c) and responses[c][1] == 0)
    check("session removed", event(25, lambda e: True, 5, "session removed"))
    check("audio disconnected", event(20, lambda e: media_state(e) == 1, 5, "audio disconnected"))
else:
    check("B's departure seen", event(27, lambda e: removed_uri(e) == THEM, 30, "B leaving"))

# -- log out (LoginSession.Logout) --
c, r = send("vx_req_account_logout_t", account_handle="A1")
check("logout answered", answered(c) and responses[c][1] == 0)
check("logged out", event(2, lambda e: login_state(e) == 0, 5, "logged out"))
check("uninitialize", w("Uninitialize", None)() is None)
finish()
