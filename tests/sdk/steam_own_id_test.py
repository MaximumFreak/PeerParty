"""A SteamID of its own for this PC, through a real steam_api64.dll under `peerparty`:
run with PPTEST_STEAM_OWN_DLL = a copy of a game's steam_api64.dll and the Steam client
running (SteamAppId=480, PEERPARTY_MODE_STEAMID=lan, which run_all.ps1 sets). One copy,
role A; `PPTEST_DISPATCH=classic` for classic callback dispatch, else ManualDispatch.

Checks, with steam_api's SteamID read from PeerParty's log (`steamown:` line):

- GetSteamID is a LAN SteamID (instance 4, account with the top bit set), not
  steam_api's; GetAppOwner says the same.
- Saves under steam_api's SteamID, account id and hex SteamID are copied the first time
  a path with the new one is opened: a folder (with a subfolder) and a file token. The
  copy can be written without touching the original. A save that already exists under
  the new name is left as it is.
- RequestUserStats(our SteamID) reaches steam_api as steam_api's, and its
  UserStatsReceived_t (callback and call result) comes back naming ours, never
  steam_api's; classic dispatch's registered object answers the same way.

Role `probe`, run without peerparty: only starts the SDK and reads the SteamID."""
import ctypes as C, glob, os, re, sys, time

DIR = os.environ["PPTEST_DIR"]
ROLE = sys.argv[1]
CLASSIC = os.environ.get("PPTEST_DISPATCH") == "classic"
VP = C.c_void_p
U64 = C.c_uint64
USER_STATS_RECEIVED = 1101
CALL_COMPLETED = 703


def log(*a):
    with open(os.path.join(DIR, f"{ROLE}.log"), "a") as fh:
        fh.write(" ".join(str(x) for x in a) + "\n")


RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append(bool(ok))
    log("PASS" if ok else "FAIL", name, detail)


def done():
    res = "ok" if RESULTS and all(RESULTS) else "failed"
    log("RESULT", res)
    open(os.path.join(DIR, f"{ROLE}.result"), "w").write(res)
    os._exit(0)


api = C.CDLL(os.environ["PPTEST_STEAM_OWN_DLL"])


def fn(name, restype, *argtypes):
    f = getattr(api, name, None)
    if f is not None:
        f.restype, f.argtypes = restype, list(argtypes)
    return f


def accessor(kind, versions):
    for v in versions:
        f = fn(f"SteamAPI_Steam{kind}_v{v:03}", VP)
        if f:
            return f()
    return None


NEEDED = ["SteamAPI_ISteamUser_GetSteamID", "SteamAPI_ISteamUserStats_RequestUserStats", "SteamAPI_ISteamApps_GetAppOwner",
          "SteamAPI_ManualDispatch_Init", "SteamAPI_RegisterCallback", "SteamAPI_RegisterCallResult"]
missing = [n for n in NEEDED if getattr(api, n, None) is None]
if missing:
    log("SKIP no flat API:", ", ".join(missing))
    done()

if fn("SteamAPI_InitFlat", C.c_int, C.c_char_p):
    err = C.create_string_buffer(1024)
    rc = api.SteamAPI_InitFlat(err)
    detail = err.value.decode(errors="replace")
else:
    rc = 0 if fn("SteamAPI_Init", C.c_bool)() else 1
    detail = ""
check("init", rc == 0, f"{rc} {detail}")
if rc != 0:
    done()

user = accessor("User", range(23, 9, -1))
stats = accessor("UserStats", range(13, 5, -1))
apps = accessor("Apps", range(9, 5, -1))
check("interfaces", user and stats and apps)
if not (user and stats and apps):
    done()
me = fn("SteamAPI_ISteamUser_GetSteamID", U64, VP)(user)
log("GetSteamID", me, hex(me))
if ROLE == "probe":
    check("a SteamID", me != 0)
    done()


def from_log(pattern):
    """The first group of `pattern` in PeerParty's log, once it's there."""
    for _ in range(50):
        for path in glob.glob(os.path.join(DIR, "logs", "**", "*.log"), recursive=True):
            try:
                text = open(path, encoding="utf-8", errors="replace").read()
            except OSError:
                continue
            m = re.search(pattern, text)
            if m:
                return int(m.group(1))
        time.sleep(0.1)
    return None


old = from_log(rf"our SteamID is {me} \(this PC's own, account \d+; steam_api's is (\d+)\)")
check("the game is shown a SteamID of its own", old is not None and old != me, f"steam_api's {old}")
if old is None:
    done()
check("peerparty maps steam_api's SteamID to ours", from_log(rf"steamown: steam_api's SteamID {old} is ({me})") == me)
check("our SteamID is a LAN one", (me >> 32) & 0xFFFFF == 4 and me & 0x8000_0000, hex(me))
owner = fn("SteamAPI_ISteamApps_GetAppOwner", U64, VP)(apps)
check("GetAppOwner is ours", owner == me, f"{owner}")

# ---- saves ----

base = os.path.join(DIR, "saves")


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as fh:
        fh.write(text)


def read(path):
    try:
        with open(path) as fh:
            return fh.read()
    except OSError as e:
        return f"<{e.__class__.__name__}>"


acc_old, acc_me = old & 0xFFFFFFFF, me & 0xFFFFFFFF
hex_old, hex_me = f"{old:016x}", f"{me:016x}"
# Already the game's own under the new name: made under another name, then renamed, so
# nothing looks at the new path until the test does.
write(os.path.join(base, "mine-first", "x.sav"), "mine")
os.rename(os.path.join(base, "mine-first"), os.path.join(base, hex_me))
write(os.path.join(base, str(old), "a.sav"), "old-a")
write(os.path.join(base, str(old), "sub", "b.sav"), "old-b")
write(os.path.join(base, f"{acc_old}_profile.dat"), "old-p")
write(os.path.join(base, hex_old, "x.sav"), "old-x")

check("a folder save is copied", read(os.path.join(base, str(me), "a.sav")) == "old-a")
check("with its subfolders", read(os.path.join(base, str(me), "sub", "b.sav")) == "old-b")
write(os.path.join(base, str(me), "a.sav"), "new-a")
check("the copy is the game's own", read(os.path.join(base, str(me), "a.sav")) == "new-a")
check("the original is untouched", read(os.path.join(base, str(old), "a.sav")) == "old-a")
check("a file save named by account id is copied", read(os.path.join(base, f"{acc_me}_profile.dat")) == "old-p")
check("a save already under the new name stays", read(os.path.join(base, hex_me, "x.sav")) == "mine")
check("no half-done copy left", not glob.glob(os.path.join(base, "*.peerparty-copy")))

# ---- stats callbacks ----

class Msg(C.Structure):
    _fields_ = [("user", C.c_int32), ("callback", C.c_int32), ("param", C.POINTER(C.c_uint8)), ("size", C.c_int32)]


def ids_in(buf, n):
    raw = bytes(buf[:n])
    return {int.from_bytes(raw[i:i + 8], "little") for i in range(0, max(0, n - 7))} & {me, old}


seen = []  # (how, SteamIDs of ours/steam_api's found in the payload)
request = fn("SteamAPI_ISteamUserStats_RequestUserStats", U64, VP, U64)
current = fn("SteamAPI_ISteamUserStats_RequestCurrentStats", C.c_bool, VP)

if not CLASSIC:
    api.SteamAPI_ManualDispatch_Init()
    pipe = fn("SteamAPI_GetHSteamPipe", C.c_int32)()
    run_frame = fn("SteamAPI_ManualDispatch_RunFrame", None, C.c_int32)
    next_cb = fn("SteamAPI_ManualDispatch_GetNextCallback", C.c_bool, C.c_int32, C.POINTER(Msg))
    free_last = fn("SteamAPI_ManualDispatch_FreeLastCallback", None, C.c_int32)
    result = fn("SteamAPI_ManualDispatch_GetAPICallResult", C.c_bool, C.c_int32, U64, VP, C.c_int, C.c_int, C.POINTER(C.c_bool))
    if current:
        current(stats)
    call = request(stats, me)
    check("RequestUserStats(ours) is a call", call != 0, hex(call))
    end = time.time() + 10
    got_result = False
    while time.time() < end and not (got_result and any(h == "callback" for h, _ in seen)):
        run_frame(pipe)
        msg = Msg()
        while next_cb(pipe, C.byref(msg)):
            if msg.callback == USER_STATS_RECEIVED:
                seen.append(("callback", ids_in(msg.param, msg.size)))
            elif msg.callback == CALL_COMPLETED:
                done_call = int.from_bytes(bytes(msg.param[:8]), "little")
                if done_call == call:
                    buf = (C.c_uint8 * 64)()
                    failed = C.c_bool(True)
                    if result(pipe, call, buf, 64, USER_STATS_RECEIVED, C.byref(failed)) and not failed.value:
                        seen.append(("call result", ids_in(buf, 64)))
                        got_result = True
            free_last(pipe)
        time.sleep(0.05)
else:
    RUN = C.CFUNCTYPE(None, VP, VP)
    RUN_RESULT = C.CFUNCTYPE(None, VP, VP, C.c_bool, U64)
    SIZE = C.CFUNCTYPE(C.c_int, VP)

    class Obj(C.Structure):
        _fields_ = [("vtable", VP), ("flags", C.c_uint8), ("callback", C.c_int32)]

    def on_run(how):
        def run(this, param):
            seen.append((how, ids_in(C.cast(param, C.POINTER(C.c_uint8)), 24)))
        return run

    def on_result(how):
        def run(this, param, failed, call):
            if not failed:
                seen.append((how, ids_in(C.cast(param, C.POINTER(C.c_uint8)), 24)))
        return run

    keep = []

    def obj(how):
        fns = [RUN_RESULT(on_result(how)), RUN(on_run(how)), SIZE(lambda this: 24)]
        vt = (VP * 3)(*[C.cast(f, VP) for f in fns])
        o = Obj(C.addressof(vt), 0, USER_STATS_RECEIVED)
        keep.extend([fns, vt, o])
        return o, C.addressof(vt)

    cb, cb_vt = obj("callback")
    fn("SteamAPI_RegisterCallback", None, VP, C.c_int)(C.addressof(cb), USER_STATS_RECEIVED)
    check("classic: the registered object goes through peerparty", cb.vtable != cb_vt)
    if current:
        current(stats)
    call = request(stats, me)
    check("RequestUserStats(ours) is a call", call != 0, hex(call))
    cr, _ = obj("call result")
    fn("SteamAPI_RegisterCallResult", None, VP, U64)(C.addressof(cr), call)
    run_callbacks = fn("SteamAPI_RunCallbacks", None)
    end = time.time() + 10
    while time.time() < end and not ({"callback", "call result"} <= {h for h, _ in seen}):
        run_callbacks()
        time.sleep(0.05)

log("stats answers:", seen)
check("UserStatsReceived_t arrived", any(h == "callback" for h, _ in seen) or any(h == "call result" for h, _ in seen))
check("the call result for RequestUserStats(ours) arrived", any(h == "call result" for h, _ in seen))
check("every answer names our SteamID", seen and all(ids == {me} for _, ids in seen))
done()
