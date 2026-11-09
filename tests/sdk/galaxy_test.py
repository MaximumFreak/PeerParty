"""GOG Galaxy's interfaces beyond lobbies and P2P, through a real Galaxy64.dll under
`peerparty`: run with PPTEST_GALAXY_DLL = a copy of a game's Galaxy64.dll (any SDK
version in galaxyvt.rs; the parts an older SDK doesn't have are skipped and logged).
Roles A and B are two copies on the LAN.

Galaxy is C++: the accessors (galaxy::api::Stats() and the rest) are the DLL's own
exports, and the methods are called through each object's vtable, in the order
hook/src/galaxyvt.rs gives for the DLL's version (checked against the DLL's RTTI by the
cargo tests). Listeners are objects whose vtables are ctypes callbacks. Without peerparty
these calls fail: the real Galaxy isn't signed in.

Both copies: sign in, stats and achievements (stored to disk, the achievement listener
fires), leaderboards fail as offline, IUtils, IStorage, ICloudStorage, telemetry, tokens
and tickets, custom networking fails through its listener. Then A and B find each other
(FindUser) and chat through IChat."""
import ctypes as C, os, re, sys, time

DIR = os.environ["PPTEST_DIR"]
ROLE = sys.argv[1]
HERE = os.path.dirname(os.path.abspath(__file__))
os.environ["PEERPARTY_GALAXY_DIR"] = os.path.join(DIR, f"galaxy-{ROLE}")


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


# ---- layouts from galaxyvt.rs, for the DLL's version ----


def file_version(path):
    ver = C.WinDLL("version")
    n = ver.GetFileVersionInfoSizeW(path, None)
    buf = C.create_string_buffer(n)
    ver.GetFileVersionInfoW(path, 0, n, buf)
    p, size = C.c_void_p(), C.c_uint()
    ver.VerQueryValueW(buf, "\\", C.byref(p), C.byref(size))
    ffi = C.cast(p, C.POINTER(C.c_uint32 * 4)).contents  # signature, struct version, MS, LS
    return (ffi[2] >> 16, ffi[2] & 0xFFFF, ffi[3] >> 16)


vt_src = open(os.path.join(HERE, "..", "..", "hook", "src", "galaxyvt.rs"), encoding="utf-8").read()
classes = re.findall(r'"(I\w+)"', re.search(r"pub const CLASSES: &\[&str\] = &\[(.*?)\];", vt_src).group(1))
VERSION = file_version(os.environ["PPTEST_GALAXY_DLL"])
# As the hook picks: the newest SDK in the table that isn't newer than the DLL.
rows = [((int(a), int(b), int(c)), [int(x) for x in r.split(",")], int(lt))
        for a, b, c, r, lt in re.findall(r"\(\((\d+), (\d+), (\d+)\), \[([-\d, ]+)\], (\d+)\)", vt_src)]
sdk, row, lt_row = [r for r in rows if r[0] <= VERSION][-1]
log("Galaxy", ".".join(map(str, VERSION)), "layouts of", ".".join(map(str, sdk)))


def table(const):
    body = re.search(r"pub const %s: &\[&\[&str\]\] = &\[\n(.*?)\n\];" % const, vt_src, re.S).group(1)
    return [re.findall(r'"([^"]*)"', line) for line in body.split("\n")]


dll = C.CDLL(os.environ["PPTEST_GALAXY_DLL"])


def rtti_slots(cls):
    """The slot count of the DLL's own vtable for galaxy::api::<cls>, through MSVC RTTI
    (x64), as the hook counts it."""
    base = dll._handle
    nt = base + C.c_uint32.from_address(base + 0x3C).value
    img = C.string_at(base, C.c_uint32.from_address(nt + 0x18 + 0x38).value)
    count, opt = C.c_uint16.from_address(nt + 6).value, C.c_uint16.from_address(nt + 20).value
    secs = []
    for i in range(count):
        sec = nt + 24 + opt + i * 40
        secs.append((C.c_uint32.from_address(sec + 12).value, C.c_uint32.from_address(sec + 8).value, C.c_uint32.from_address(sec + 36).value))
    data = [(r, n) for r, n, ch in secs if ch & 0x40000000 and not ch & 0x20000000]
    code = [(r, n) for r, n, ch in secs if ch & 0x20000000]
    at = img.find(f".?AV{cls}@api@galaxy@@\0".encode())
    if at < 0:
        return None
    td = at - 16
    rd32 = lambda o: int.from_bytes(img[o:o + 4], "little")
    rdp = lambda o: int.from_bytes(img[o:o + 8], "little")
    col = next((o for r, n in data for o in range(r, min(r + n, len(img)) - 24, 4)
                if rd32(o) == 1 and rd32(o + 4) == 0 and rd32(o + 12) == td and rd32(o + 20) == o), None)
    if col is None:
        return None
    is_code = lambda va: any(base + r <= va < base + r + n for r, n in code)
    for r, n in data:
        for o in range(r, min(r + n, len(img)) - 16, 8):
            if rdp(o) == base + col:
                k = 0
                while is_code(rdp(o + 8 * (k + 1))):
                    k += 1
                return k
    return None


def slots(cls):
    """As the hook picks: the SDK's layout, or where the DLL's slot count says otherwise,
    a layout from the same x.y series with that count (newer first)."""
    at = row[classes.index(cls)]
    if at < 0:
        return {}
    layouts = table(cls.upper())
    names = layouts[at]
    n = rtti_slots(cls)
    if n is not None and n != len(names):
        i = rows.index((sdk, row, lt_row))
        same = [r for r in rows[i + 1:] + rows[:i][::-1] if r[0][:2] == sdk[:2]]
        near = [layouts[r[1][classes.index(cls)]] for r in same if r[1][classes.index(cls)] >= 0]
        names = next((l for l in near if len(l) == n), names)
        log(cls, f"has {n} slots, the headers {len(layouts[at])}:", "a nearby layout" if len(names) == n else "no layout fits")
    return {m: i for i, m in enumerate(names)}


SLOT = {c: slots(c) for c in classes}
LT = {n: i for i, n in enumerate(table("LISTENER_TYPES")[lt_row])}


def has(cls, *methods):
    """Whether this SDK has these methods; logs the part it skips when not."""
    missing = [m for m in methods if m not in SLOT[cls]]
    if missing:
        log("n/a in this SDK:", cls, ", ".join(missing))
    return not missing

FN = C.CFUNCTYPE
VP = C.c_void_p


def export(name):
    return C.cast(dll[name], VP).value


def accessor(name):
    try:
        return FN(VP)(export(f"?{name}@api@galaxy@@YAPEAVI{name}@12@XZ"))()
    except AttributeError:  # not in this SDK
        return None


def call(obj, cls, method, restype, *args):
    """args: (ctype, value) pairs."""
    vt = C.cast(obj, C.POINTER(C.POINTER(VP)))[0]
    f = FN(restype, VP, *[t for t, _ in args])(vt[SLOT[cls][method]])
    return f(obj, *[v for _, v in args])


def gid_arg(v):
    """A GalaxyID by value: MSVC x64 passes a pointer to a copy."""
    return (C.POINTER(C.c_uint64), C.pointer(C.c_uint64(v)))


class Listener:
    """A C++ listener: slot 0 the destructor, then the methods in order."""

    def __init__(self, *methods):
        self.fns = [FN(None, VP)(lambda this: None)] + [FN(None, VP, *types)(f) for types, f in methods]
        self.vt = (VP * len(self.fns))(*[C.cast(f, VP) for f in self.fns])
        self.obj = (VP * 1)(C.cast(self.vt, VP))
        self.ptr = C.cast(self.obj, VP)


EVENTS = []


def note(name):
    return lambda this, *a: EVENTS.append((name,) + tuple(a))


process = FN(None)(export("?ProcessData@api@galaxy@@YAXXZ"))


def wait_for(pred, secs=10):
    end = time.time() + secs
    while time.time() < end:
        process()
        got = pred()
        if got:
            return got
        time.sleep(0.05)
    return None


def event(name, secs=10):
    return wait_for(lambda: next((e for e in EVENTS if e[0] == name), None), secs)


class InitOptions(C.Structure):
    _fields_ = [("clientID", C.c_char_p), ("clientSecret", C.c_char_p), ("configFilePath", C.c_char_p), ("storagePath", C.c_char_p),
                ("galaxyAllocator", VP), ("galaxyThreadFactory", VP), ("host", C.c_char_p), ("port", C.c_uint16)]


opts = InitOptions(b"peerpartytest", b"secret", b".", None, None, None, None, 0)
FN(None, C.c_void_p)(export("?Init@api@galaxy@@YAXAEBUInitOptions@12@@Z"))(C.cast(C.pointer(opts), VP))

user, friends, stats = accessor("User"), accessor("Friends"), accessor("Stats")
utils, storage = accessor("Utils"), accessor("Storage")
cloud = accessor("CloudStorage") if SLOT["ICloudStorage"] else None
telemetry, chat, custom = accessor("Telemetry"), accessor("Chat"), accessor("CustomNetworking")
registrar = accessor("ListenerRegistrar")
check("accessors", all([user, friends, stats, utils, storage, telemetry, chat, custom, registrar]))

auth = Listener(([], note("auth")), ([C.c_uint32], note("auth_failed")), ([], note("auth_lost")))
if "SignInGalaxy" in SLOT["IUser"]:
    call(user, "IUser", "SignInGalaxy", None, (C.c_bool, False), (C.c_uint32, 15), (VP, auth.ptr))
else:  # before 1.152.9: no timeout argument
    call(user, "IUser", "SignInGalaxy~2@1", None, (C.c_bool, False), (VP, auth.ptr))
check("signed in", event("auth") is not None)
me = C.c_uint64()
call(user, "IUser", "GetGalaxyID", VP, (C.POINTER(C.c_uint64), C.byref(me)))
check("GalaxyID is a user", me.value >> 56 == 2, hex(me.value))

# Global listeners go in first, as games do: a chat message can arrive any time.
msgs = Listener(([C.c_uint64, C.c_uint32, C.c_uint32], note("messages")))
call(registrar, "IListenerRegistrar", "Register", None, (C.c_uint32, LT["CHAT_ROOM_MESSAGES_LISTENER"]), (VP, msgs.ptr))

# ---- IStats ----
ach = Listener(([C.c_char_p], note("achievement")))
call(registrar, "IListenerRegistrar", "Register", None, (C.c_uint32, LT["ACHIEVEMENT_CHANGE"]), (VP, ach.ptr))
retrieved = Listener(([C.POINTER(C.c_uint64)], note("stats")), ([C.POINTER(C.c_uint64), C.c_uint32], note("stats_failed")))
call(stats, "IStats", "RequestUserStatsAndAchievements", None, gid_arg(0), (VP, retrieved.ptr))
check("stats retrieved", event("stats") is not None)
call(stats, "IStats", "SetStatInt", None, (C.c_char_p, b"kills"), (C.c_int32, 5))
call(stats, "IStats", "SetStatFloat", None, (C.c_char_p, b"distance"), (C.c_float, 2.5))
call(stats, "IStats", "SetAchievement", None, (C.c_char_p, b"FIRST_WIN"))
stored = Listener(([], note("stored")), ([C.c_uint32], note("store_failed")))
call(stats, "IStats", "StoreStatsAndAchievements", None, (VP, stored.ptr))
check("stats stored", event("stored") is not None)
got = event("achievement")
check("achievement listener heard the unlock", got and got[1] == b"FIRST_WIN", got)
check("GetStatInt", call(stats, "IStats", "GetStatInt", C.c_int32, (C.c_char_p, b"kills"), gid_arg(0)) == 5)
check("GetStatFloat", call(stats, "IStats", "GetStatFloat", C.c_float, (C.c_char_p, b"distance"), gid_arg(0)) == 2.5)
unlocked, when = C.c_bool(), C.c_uint32()
call(stats, "IStats", "GetAchievement", None, (C.c_char_p, b"FIRST_WIN"), (C.POINTER(C.c_bool), C.byref(unlocked)),
     (C.POINTER(C.c_uint32), C.byref(when)), gid_arg(0))
check("GetAchievement", unlocked.value and when.value > 0, (unlocked.value, when.value))
if has("IStats", "GetAchievementsNumber"):
    check("achievement count", call(stats, "IStats", "GetAchievementsNumber", C.c_uint32) == 1)
tsv = os.path.join(os.environ["PEERPARTY_GALAXY_DIR"], "stats.tsv")
check("stats on disk", os.path.isfile(tsv) and "kills\t5" in open(tsv).read())
lb = Listener(([C.c_char_p], note("lb")), ([C.c_char_p, C.c_uint32], note("lb_failed")))
call(stats, "IStats", "FindLeaderboard", None, (C.c_char_p, b"best"), (VP, lb.ptr))
check("leaderboard fails as offline", event("lb_failed") is not None)

# ---- IUtils, IApps ----
if has("IUtils", "GetGogServicesConnectionState"):
    check("GOG services connected", call(utils, "IUtils", "GetGogServicesConnectionState", C.c_uint32) == 1)
check("overlay state answered", call(utils, "IUtils", "GetOverlayState", C.c_uint32) in (1, 4))
check("overlay not visible", not call(utils, "IUtils", "IsOverlayVisible", C.c_bool))

# ---- IStorage ----
data = b"save data " + ROLE.encode()
call(storage, "IStorage", "FileWrite", None, (C.c_char_p, b"slot1.sav"), (C.c_char_p, data), (C.c_uint32, len(data)))
check("FileExists", call(storage, "IStorage", "FileExists", C.c_bool, (C.c_char_p, b"slot1.sav")))
buf = C.create_string_buffer(64)
n = call(storage, "IStorage", "FileRead", C.c_uint32, (C.c_char_p, b"slot1.sav"), (VP, C.cast(buf, VP)), (C.c_uint32, 64))
check("FileRead", buf.raw[:n] == data, buf.raw[:n])
check("GetFileCount", call(storage, "IStorage", "GetFileCount", C.c_uint32) == 1)
check("GetFileNameByIndex", call(storage, "IStorage", "GetFileNameByIndex", C.c_char_p, (C.c_uint32, 0)) == b"slot1.sav")

# ---- ICloudStorage ----
if cloud and has("ICloudStorage", "PutFile~8@4", "GetFileList", "GetFileNameByIndex", "GetFileHashByIndex", "GetFile~5@4#1"):
    put = Listener(([C.c_char_p, C.c_char_p], note("put")), ([C.c_char_p, C.c_char_p, C.c_uint32], note("put_failed")))
    call(cloud, "ICloudStorage", "PutFile~8@4", None, (C.c_char_p, b"saves"), (C.c_char_p, b"cloud.sav"), (C.c_char_p, data),
         (C.c_uint32, len(data)), (VP, put.ptr), (C.c_uint32, 3), (C.c_uint32, 0), (C.c_char_p, None))
    check("cloud PutFile", event("put") is not None)
    lst = Listener(([C.c_uint32, C.c_uint32, C.c_uint32], note("list")), ([C.c_uint32], note("list_failed")))
    call(cloud, "ICloudStorage", "GetFileList", None, (C.c_char_p, b"saves"), (VP, lst.ptr))
    got = event("list")
    check("cloud GetFileList", got and got[1] == 1, got)
    check("cloud file name", call(cloud, "ICloudStorage", "GetFileNameByIndex", C.c_char_p, (C.c_uint32, 0)) == b"cloud.sav")
    check("cloud file hash is MD5", len(call(cloud, "ICloudStorage", "GetFileHashByIndex", C.c_char_p, (C.c_uint32, 0)) or b"") == 32)
    getl = Listener(([C.c_char_p, C.c_char_p, C.c_uint32, C.c_uint32, C.c_char_p], note("get")),
                    ([C.c_char_p, C.c_char_p, C.c_uint32], note("get_failed")))
    buf = C.create_string_buffer(64)
    call(cloud, "ICloudStorage", "GetFile~5@4#1", None, (C.c_char_p, b"saves"), (C.c_char_p, b"cloud.sav"), (VP, C.cast(buf, VP)),
         (C.c_uint32, 64), (VP, getl.ptr))
    got = event("get")
    check("cloud GetFile", got and buf.raw[:got[3]] == data, got)

# ---- ITelemetry, IUser tokens, ICustomNetworking ----
tel = Listener(([C.c_char_p, C.c_uint32], note("tel")), ([C.c_char_p, C.c_uint32, C.c_uint32], note("tel_failed")))
call(telemetry, "ITelemetry", "AddStringParam", None, (C.c_char_p, b"level"), (C.c_char_p, b"1"))
call(telemetry, "ITelemetry", "SendTelemetryEvent", C.c_uint32, (C.c_char_p, b"game_start"), (VP, tel.ptr))
check("telemetry sent", event("tel") is not None)
check("access token", (call(user, "IUser", "GetAccessToken", C.c_char_p) or b"").startswith(b"peerparty-"))
if has("IUser", "GetIDToken"):
    check("ID token is a JWT", (call(user, "IUser", "GetIDToken", C.c_char_p) or b"").count(b".") == 2)
tk = Listener(([], note("ticket")), ([C.c_uint32], note("ticket_failed")))
call(user, "IUser", "RequestEncryptedAppTicket", None, (VP, None), (C.c_uint32, 0), (VP, tk.ptr))
check("encrypted app ticket", event("ticket") is not None)
tbuf, tlen = C.create_string_buffer(256), C.c_uint32()
call(user, "IUser", "GetEncryptedAppTicket", None, (VP, C.cast(tbuf, VP)), (C.c_uint32, 256), (C.POINTER(C.c_uint32), C.byref(tlen)))
check("ticket bytes", tlen.value > 0, tbuf.raw[:tlen.value])
conn = Listener(([C.c_char_p, C.c_uint64], note("conn")), ([C.c_char_p, C.c_uint32], note("conn_failed")))
call(custom, "ICustomNetworking", "OpenConnection", None, (C.c_char_p, b"tcp://example.invalid:1"), (VP, conn.ptr))
check("custom networking fails through its listener", event("conn_failed") is not None)

# ---- A and B: FindUser and IChat ----
other = wait_for(lambda: call(friends, "IFriends", "GetFriendCount", C.c_uint32) >= 1, 30)
check("the other copy is a friend", other)
if not other:
    done()
oid = C.c_uint64()
call(friends, "IFriends", "GetFriendByIndex", VP, (C.POINTER(C.c_uint64), C.byref(oid)), (C.c_uint32, 0))
oname = call(friends, "IFriends", "GetFriendPersonaName", C.c_char_p, gid_arg(oid.value))
found = Listener(([C.c_char_p, C.POINTER(C.c_uint64)], lambda this, s, g: EVENTS.append(("found", s, g.contents.value))),
                 ([C.c_char_p, C.c_uint32], note("not_found")))
call(friends, "IFriends", "FindUser", None, (C.c_char_p, oname), (VP, found.ptr))
got = event("found")
check("FindUser by name", got and got[2] == oid.value, (oname, got))

room_l = Listener(([C.POINTER(C.c_uint64), C.c_uint64], lambda this, g, r: EVENTS.append(("room", r))),
                  ([C.POINTER(C.c_uint64), C.c_uint32], note("room_failed")))
call(chat, "IChat", "RequestChatRoomWithUser", None, gid_arg(oid.value), (VP, room_l.ptr))
got = event("room")
check("chat room with the other copy", got is not None, got)
room = got[1] if got else 0
sent = Listener(([C.c_uint64, C.c_uint32, C.c_uint64, C.c_uint32], note("sent")), ([C.c_uint64, C.c_uint32, C.c_uint32], note("send_failed")))
text = f"hello from {ROLE}".encode()
# Both copies send until the other's message arrives (the channel opens on first use).
heard = None
end = time.time() + 30
while time.time() < end and not heard:
    if not any(e[0] == "sent" for e in EVENTS):
        call(chat, "IChat", "SendChatRoomMessage", C.c_uint32, (C.c_uint64, room), (C.c_char_p, text), (VP, sent.ptr))
    heard = wait_for(lambda: next((e for e in EVENTS if e[0] == "messages"), None), 1)
check("message sent", any(e[0] == "sent" for e in EVENTS))
check("message from the other copy arrived", heard and heard[1] == room, heard)
if heard:
    mid, kind, sender, when = C.c_uint64(), C.c_uint32(), C.c_uint64(), C.c_uint32()
    mbuf = C.create_string_buffer(128)
    n = call(chat, "IChat", "GetChatRoomMessageByIndex", C.c_uint32, (C.c_uint32, 0), (C.POINTER(C.c_uint64), C.byref(mid)),
             (C.POINTER(C.c_uint32), C.byref(kind)), (C.POINTER(C.c_uint64), C.byref(sender)), (C.POINTER(C.c_uint32), C.byref(when)),
             (VP, C.cast(mbuf, VP)), (C.c_uint32, 128))
    want = b"hello from " + (b"B" if ROLE == "A" else b"A")
    check("message text and sender", mbuf.value == want and sender.value == oid.value and kind.value == 1, (mbuf.value, sender.value))
# Let the other copy's last message arrive before leaving.
wait_for(lambda: False, 2)
done()
