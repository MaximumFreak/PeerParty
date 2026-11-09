"""Steam licence checks between two copies, through a real steam_api64.dll under
`peerparty --multi-instance`: run with PPTEST_STEAM_DLL = a copy of a game's
steam_api64.dll (any SDK with the flat API; ISteamUser and ISteamFriends are got the way
that SDK's headers get them, newest interface version first) and SteamAppId = the id of
a game the signed-in Steam account owns. The Steam client must be running; it shows the
account as playing that game while the test runs.

peerparty never answers a licence question itself: ISteamUser::UserHasLicenseForApp goes
to the steam_api the game loaded, and only when that gives nothing usable does peerparty
say k_EUserHasLicenseResultNoAuth (2, can't verify). So:

- each copy signs in; A keeps the account's SteamID, B gets a LAN one;
- each finds the other among its Steam friends (PeerParty's LAN players);
- A asks about B's LAN SteamID: Steam has no auth session for it, so NoAuth (2);
- both ask about a SteamID nobody has: NoAuth (2);
- B asks about A (the signed-in account itself): whatever Steam says, as long as it is
  one of the three results.

The flat functions call the interface objects steam_api hands out, which are PeerParty's
stand-ins, so this is the path a game's own calls take.

Role `probe`, run without peerparty: only starts the SDK and gets the interfaces, to tell
a build that can't start on this PC at all from one peerparty breaks."""
import ctypes as C, os, re, sys, time

DIR = os.environ["PPTEST_DIR"]
ROLE = sys.argv[1]
OTHER = "B" if ROLE == "A" else "A"
APP = int(os.environ["SteamAppId"])
NO_AUTH = 2
NOBODY = 0x0110000100000000 | 0x7A5E1D  # an individual SteamID in the public universe


def log(*a):
    # peerparty starts the copy without inheriting handles, so output goes to a file.
    with open(os.path.join(DIR, f"{ROLE}.log"), "a") as fh:
        fh.write(" ".join(str(x) for x in a) + "\n")


RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append(bool(ok))
    log("PASS" if ok else "FAIL", name, detail)


def done(shutdown=False):
    res = "ok" if RESULTS and all(RESULTS) else "failed"
    log("RESULT", res)
    open(os.path.join(DIR, f"{ROLE}.result"), "w").write(res)
    if shutdown:
        # After the result: some builds end the process in SteamAPI_Shutdown.
        api.SteamAPI_Shutdown()
    os._exit(0)


def mark(name, value=""):
    open(os.path.join(DIR, f"{ROLE}.{name}"), "w").write(str(value))


def wait_mark(name, secs=30):
    path = os.path.join(DIR, f"{OTHER}.{name}")
    end = time.time() + secs
    while time.time() < end:
        if os.path.exists(path):
            text = open(path).read()
            if text or name != "steamid":
                return text
        time.sleep(0.1)
    return None


api = C.CDLL(os.environ["PPTEST_STEAM_DLL"])
VP = C.c_void_p


def fn(name, restype, *argtypes):
    f = getattr(api, name, None)
    if f is not None:
        f.restype, f.argtypes = restype, list(argtypes)
    return f


NEEDED = ["SteamAPI_ISteamUser_GetSteamID", "SteamAPI_ISteamUser_UserHasLicenseForApp",
          "SteamAPI_ISteamFriends_GetFriendCount", "SteamAPI_ISteamFriends_GetFriendByIndex"]
missing = [n for n in NEEDED if fn(n, None) is None]
if missing:
    # Before the flat API: games call the C++ interfaces only.
    log("SKIP no flat API:", ", ".join(missing))
    done()

if fn("SteamAPI_InitFlat", C.c_int, C.c_char_p):
    err = C.create_string_buffer(1024)
    rc = api.SteamAPI_InitFlat(err)
    check("SteamAPI_InitFlat", rc == 0, f"{rc} {err.value.decode(errors='replace')}")
elif fn("SteamAPI_Init", C.c_bool):
    rc = 0 if api.SteamAPI_Init() else 1
    check("SteamAPI_Init", rc == 0)
else:
    # The SDK's inline SteamAPI_InitEx, with no interface versions to check.
    init = fn("SteamInternal_SteamAPI_Init", C.c_int, C.c_char_p, C.c_char_p)
    err = C.create_string_buffer(1024)
    rc = init(b"\0", err) if init else -1
    check("SteamInternal_SteamAPI_Init", rc == 0, f"{rc} {err.value.decode(errors='replace')}")
if rc != 0:
    done()


def interface(kind, versions):
    """The interface the way the SDK's own headers get it, newest way first: the
    versioned flat accessor (SDK 1.49+), SteamInternal_FindOrCreateUserInterface
    (1.43+), the SteamUser()-style export, or ISteamClient::GetISteamUser (older)."""
    for v in versions:
        f = fn(f"SteamAPI_Steam{kind}_v{v:03}", VP)
        if f:
            return f(), f"SteamAPI_Steam{kind}_v{v:03}"
    user = fn("SteamAPI_GetHSteamUser", C.c_int32)()
    find = fn("SteamInternal_FindOrCreateUserInterface", VP, C.c_int32, C.c_char_p)
    for v in versions:
        name = f"Steam{kind}{v:03}".encode()
        if find and find(user, name):
            return find(user, name), f"FindOrCreateUserInterface {name.decode()}"
    legacy = fn(f"Steam{kind}", VP)
    if legacy and legacy():
        return legacy(), f"Steam{kind}()"
    client = fn("SteamClient", VP)
    get = fn(f"SteamAPI_ISteamClient_GetISteam{kind}", VP, VP, C.c_int32, C.c_int32, C.c_char_p)
    if client and get and client():
        pipe = fn("SteamAPI_GetHSteamPipe", C.c_int32)()
        for v in versions:
            obj = get(client(), user, pipe, f"Steam{kind}{v:03}".encode())
            if obj:
                return obj, f"ISteamClient::GetISteam{kind} Steam{kind}{v:03}"
    return None, None


def built_for(kind, versions):
    """The interface versions the DLL names (the ones its flat functions were compiled
    for, as a game's headers would ask), newest first, then the rest. Asking an older
    build for a newer version gets an object its flat functions don't match."""
    data = open(os.environ["PPTEST_STEAM_DLL"], "rb").read()
    named = sorted({int(v) for v in re.findall(rb"Steam%s0(\d\d)\x00" % kind.encode(), data)}, reverse=True)
    return named + [v for v in versions if v not in named]


user, user_how = interface("User", built_for("User", range(23, 9, -1)))
# SteamFriends018 last: older steam_api builds name no SteamFriends version, and their
# flat functions crash on an 018 object.
friends, friends_how = interface("Friends", built_for("Friends", [*range(17, 9, -1), 18]))
log("interfaces", user_how, "/", friends_how)
check("ISteamUser and ISteamFriends", user and friends)
if not (user and friends) or ROLE == "probe":
    done()
api.SteamAPI_ISteamUser_GetSteamID.restype = C.c_uint64
api.SteamAPI_ISteamUser_GetSteamID.argtypes = [C.c_void_p]
api.SteamAPI_ISteamUser_UserHasLicenseForApp.restype = C.c_int
api.SteamAPI_ISteamUser_UserHasLicenseForApp.argtypes = [C.c_void_p, C.c_uint64, C.c_uint32]
api.SteamAPI_ISteamFriends_GetFriendCount.restype = C.c_int
api.SteamAPI_ISteamFriends_GetFriendCount.argtypes = [C.c_void_p, C.c_int]
api.SteamAPI_ISteamFriends_GetFriendByIndex.restype = C.c_uint64
api.SteamAPI_ISteamFriends_GetFriendByIndex.argtypes = [C.c_void_p, C.c_int, C.c_int]

me =api.SteamAPI_ISteamUser_GetSteamID(user)
check("own SteamID", me != 0, hex(me))
mark("steamid", me)

other = wait_mark("steamid")
check("the other copy started", other is not None)
if other is None:
    done()
other = int(other)
check("the copies have different SteamIDs", other != me, f"{hex(me)} / {hex(other)}")

# The other copy shows up as a Steam friend once peerparty has met it on the LAN.
IMMEDIATE = 4
end, found, listed = time.time() + 30, False, []
while time.time() < end and not found:
    api.SteamAPI_RunCallbacks()
    n = api.SteamAPI_ISteamFriends_GetFriendCount(friends, IMMEDIATE)
    listed = [api.SteamAPI_ISteamFriends_GetFriendByIndex(friends, i, IMMEDIATE) for i in range(n)]
    found = other in listed
    time.sleep(0.1)
check("the other copy is a LAN friend", found, f"{hex(other)} among {[hex(x) for x in listed[-5:]]} ({len(listed)})")

r = api.SteamAPI_ISteamUser_UserHasLicenseForApp(user, NOBODY, APP)
check("a SteamID nobody has: NoAuth", r == NO_AUTH, r)

r = api.SteamAPI_ISteamUser_UserHasLicenseForApp(user, other, APP)
if ROLE == "A":
    # B's SteamID is a LAN one: Steam never authenticated it.
    check("the LAN player: NoAuth", r == NO_AUTH, r)
else:
    # A is the signed-in account itself: Steam's answer, whatever it is.
    check("the other copy: one of Steam's three results", r in (0, 1, 2), r)

r = api.SteamAPI_ISteamUser_UserHasLicenseForApp(user, me, APP)
check("this copy: one of Steam's three results", r in (0, 1, 2), r)

# Both stay signed in until both have asked.
mark("done")
check("the other copy finished", wait_mark("done") is not None)
done(shutdown=True)
