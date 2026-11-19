"""A game from before the flat API, through a steam_api64.dll from an older SDK under
`peerparty`: it fetches ISteamMatchmaking with the old `SteamMatchmaking()` export, calls
the object's vtable, and takes its callbacks with SteamAPI_RunCallbacks (classic
dispatch). Run with PPTEST_STEAM_LEGACY_DLL = a copy of such a steam_api64.dll (with
whatever files it reads kept beside it); SteamAppId=480, which run_all.ps1 sets. One
copy, role A.

Checks:

- PeerParty wraps the object `SteamMatchmaking()` hands out (whether or not the DLL
  names its interface versions) and says which version it took it as.
- CreateLobby on that object is PeerParty's: its log has the lobby.
- The LobbyCreated_t call result reaches the CCallResult the game registered, through
  SteamAPI_RunCallbacks, naming that lobby, and LobbyEnter_t reaches the registered
  callback: a game waiting on them ("Creating session...") goes on.
- PeerParty's log says the game dispatches classically, and nothing waits undelivered."""
import ctypes as C, glob, os, re, sys, time

DIR = os.environ["PPTEST_DIR"]
ROLE = sys.argv[1]
VP = C.c_void_p
U64 = C.c_uint64
LOBBY_ENTER = 504
LOBBY_CREATED = 513
# ISteamMatchmaking's CreateLobby slot in the versions where it takes (type, max members).
CREATE_LOBBY = {"SteamMatchMaking007": 10, "SteamMatchMaking008": 12, "SteamMatchMaking009": 13}


def log(*a):
    with open(os.path.join(DIR, f"{ROLE}.log"), "a", encoding="utf-8") as fh:
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


def peerparty_log():
    out = ""
    for path in glob.glob(os.path.join(DIR, "logs", "**", "*.log"), recursive=True):
        try:
            out += open(path, encoding="utf-8", errors="replace").read()
        except OSError:
            pass
    return out


def from_log(pattern, wait=5.0):
    """`pattern`'s match in PeerParty's log, once it's there."""
    end = time.time() + wait
    while True:
        m = re.search(pattern, peerparty_log())
        if m or time.time() >= end:
            return m
        time.sleep(0.1)


api = C.CDLL(os.environ["PPTEST_STEAM_LEGACY_DLL"])


def fn(name, restype, *argtypes):
    f = getattr(api, name, None)
    if f is not None:
        f.restype, f.argtypes = restype, list(argtypes)
    return f


needed = ["SteamAPI_Init", "SteamMatchmaking", "SteamAPI_RunCallbacks", "SteamAPI_RegisterCallback", "SteamAPI_RegisterCallResult"]
missing = [n for n in needed if getattr(api, n, None) is None]
if missing:
    log("SKIP not an older SDK's steam_api:", ", ".join(missing))
    done()

check("init", fn("SteamAPI_Init", C.c_bool)())
mm = fn("SteamMatchmaking", VP)()
check("SteamMatchmaking() hands out an object", bool(mm))
if not mm:
    done()
taken = from_log(r"steam-lan SteamMatchmaking: .*taken as (SteamMatchMaking\d+)|steam-lan (SteamMatchMaking\d+): peerparty answers")
version = (taken.group(1) or taken.group(2)) if taken else None
check("peerparty wraps it", taken is not None, version or peerparty_log()[-2000:])
if version not in CREATE_LOBBY:
    log("SKIP CreateLobby isn't (type, max members) in", version)
    done()

RUN = C.CFUNCTYPE(None, VP, VP)
RUN_RESULT = C.CFUNCTYPE(None, VP, VP, C.c_bool, U64)
SIZE = C.CFUNCTYPE(C.c_int, VP)


class Obj(C.Structure):
    _fields_ = [("vtable", VP), ("flags", C.c_uint8), ("callback", C.c_int32)]


seen = {}
keep = []


def listener(how, size, cb_id):
    def run(this, param):
        seen.setdefault(how, bytes(C.cast(param, C.POINTER(C.c_uint8))[:size]))

    def run_result(this, param, failed, call):
        if not failed:
            seen.setdefault(how, bytes(C.cast(param, C.POINTER(C.c_uint8))[:size]))

    fns = [RUN_RESULT(run_result), RUN(run), SIZE(lambda this: size)]
    vt = (VP * 3)(*[C.cast(f, VP) for f in fns])
    o = Obj(C.addressof(vt), 0, cb_id)
    keep.extend([fns, vt, o])
    return o


enter = listener("enter", 24, LOBBY_ENTER)
fn("SteamAPI_RegisterCallback", None, VP, C.c_int)(C.addressof(enter), LOBBY_ENTER)

vtable = C.cast(C.cast(mm, C.POINTER(VP))[0], C.POINTER(VP))
create = C.CFUNCTYPE(U64, VP, C.c_int, C.c_int)(vtable[CREATE_LOBBY[version]])
call = create(mm, 2, 2)
check("CreateLobby is a call", call != 0, hex(call))
made = from_log(r"steam-lan CreateLobby type=2 max=2 -> (\d+) \(true\)")
check("the lobby is peerparty's", made is not None)
lobby = int(made.group(1)) if made else 0

created = listener("created", 16, LOBBY_CREATED)
fn("SteamAPI_RegisterCallResult", None, VP, U64)(C.addressof(created), call)
run_callbacks = fn("SteamAPI_RunCallbacks", None)
end = time.time() + 15
while time.time() < end and not {"created", "enter"} <= set(seen):
    run_callbacks()
    time.sleep(0.05)

got = seen.get("created")
check("LobbyCreated_t reaches the game's CCallResult", got is not None)
if got:
    result, lobby_id = int.from_bytes(got[0:4], "little"), int.from_bytes(got[8:16], "little")
    check("it names peerparty's lobby", result == 1 and lobby_id == lobby, f"result {result} lobby {lobby_id}, peerparty's {lobby}")
got = seen.get("enter")
check("LobbyEnter_t reaches the registered callback", got is not None and int.from_bytes(got[0:8], "little") == lobby)
check("peerparty sees classic dispatch", from_log(r"steam: the game dispatches Steam callbacks with SteamAPI_RunCallbacks") is not None)
check("nothing waits undelivered", "wait undelivered" not in peerparty_log())
done()
