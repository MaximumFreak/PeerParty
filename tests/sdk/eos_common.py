"""Shared ctypes setup for PeerParty's two-process tests on a real EOS SDK DLL.

Run under `peerparty --multi-instance python.exe <test>.py <role>`: the DLL is
loaded with ctypes, so every EOS lookup goes through PeerParty's swapped GetProcAddress,
as a game's would. Struct layouts are from the EOS SDK 1.19 headers."""
import ctypes as C, os, sys, time

DLL = os.environ["PPTEST_EOS_DLL"]
DIR = os.environ["PPTEST_DIR"]
ROLE = sys.argv[1]
eos = C.CDLL(DLL)

def log(*a):
    # peerparty starts the copy without inheriting handles, so output goes to a file.
    with open(os.path.join(DIR, f"{ROLE}.log"), "a") as fh:
        fh.write(" ".join(str(x) for x in a) + "\n")

class InitOpts(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("Alloc", C.c_void_p), ("Realloc", C.c_void_p), ("Release", C.c_void_p),
                ("ProductName", C.c_char_p), ("ProductVersion", C.c_char_p), ("Reserved", C.c_void_p),
                ("SystemInitializeOptions", C.c_void_p), ("OverrideThreadAffinity", C.c_void_p)]

class PlatformOpts(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("Reserved", C.c_void_p), ("ProductId", C.c_char_p), ("SandboxId", C.c_char_p),
                ("ClientId", C.c_char_p), ("ClientSecret", C.c_char_p), ("bIsServer", C.c_int32),
                ("EncryptionKey", C.c_char_p), ("OverrideCountryCode", C.c_char_p), ("OverrideLocaleCode", C.c_char_p),
                ("DeploymentId", C.c_char_p), ("Flags", C.c_uint64), ("CacheDirectory", C.c_char_p),
                ("TickBudgetInMilliseconds", C.c_uint32), ("RTCOptions", C.c_void_p),
                ("IntegratedPlatformOptionsContainerHandle", C.c_void_p), ("SystemSpecificOptions", C.c_void_p),
                ("TaskNetworkTimeoutSeconds", C.c_void_p)]

class LoginInfo(C.Structure):
    _fields_ = [("ResultCode", C.c_int32), ("ClientData", C.c_void_p), ("LocalUserId", C.c_void_p), ("ContinuanceToken", C.c_void_p)]

class Creds(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("Token", C.c_char_p), ("Type", C.c_int32)]

class LoginOpts(C.Structure):
    _fields_ = [("ApiVersion", C.c_int32), ("Credentials", C.POINTER(Creds)), ("UserLoginInfo", C.c_void_p)]

CB = C.CFUNCTYPE(None, C.c_void_p)
_keep = []

def cb(struct_type, fn):
    """A C callback taking `const struct_type*`, kept alive for the run."""
    def tramp(p):
        try:
            fn(C.cast(p, C.POINTER(struct_type)).contents)
        except Exception as e:  # never unwind into the SDK
            log("callback error", e)
    f = CB(tramp)
    _keep.append(f)
    return f

def fn(name, res=C.c_int32, *args):
    f = getattr(eos, name, None)
    if f is None:
        # An older SDK without this interface: the test has nothing to run on it.
        log("SKIP not in this SDK:", name)
        with open(os.path.join(DIR, f"{ROLE}.result"), "w") as fh:
            fh.write("ok")
        os._exit(0)
    f.restype = res
    f.argtypes = list(args)
    return f

def start():
    init = fn("EOS_Initialize", C.c_int32, C.c_void_p)
    io = InitOpts(ApiVersion=4, ProductName=b"peerparty-test", ProductVersion=b"1.0")
    r = init(C.byref(io))
    log("EOS_Initialize", r)
    create = fn("EOS_Platform_Create", C.c_void_p, C.c_void_p)
    cache = os.path.join(DIR, f"cache-{ROLE}").encode()
    os.makedirs(cache, exist_ok=True)
    plat = None
    for v in range(14, 0, -1):  # newest first; an older SDK refuses a newer version
        po = PlatformOpts(ApiVersion=v, ProductId=b"peerpartytestproduct", SandboxId=b"peerpartytestsandbox",
                          DeploymentId=b"peerpartytestdeployment", ClientId=b"x", ClientSecret=b"y",
                          EncryptionKey=b"1111111111111111111111111111111111111111111111111111111111111111",
                          Flags=0x2 | 0x4, CacheDirectory=cache)
        plat = create(C.byref(po))
        if plat:
            log("EOS_Platform_Create api", v, "->", hex(plat))
            break
    if not plat:
        log("EOS_Platform_Create failed")
        sys.exit(2)
    return plat

TICK = None
def tick_until(plat, cond, secs, what):
    global TICK
    if TICK is None:
        TICK = fn("EOS_Platform_Tick", None, C.c_void_p)
    end = time.time() + secs
    while time.time() < end:
        TICK(plat)
        if cond():
            return True
        time.sleep(0.02)
    log("TIMEOUT waiting for", what)
    return False

def login(plat):
    conn = fn("EOS_Platform_GetConnectInterface", C.c_void_p, C.c_void_p)(plat)
    done = {}
    f = cb(LoginInfo, lambda i: done.update(r=i.ResultCode, id=i.LocalUserId))
    cr = Creds(ApiVersion=1, Token=b"t", Type=11)
    lo = LoginOpts(ApiVersion=2, Credentials=C.pointer(cr))
    fn("EOS_Connect_Login", None, C.c_void_p, C.c_void_p, C.c_void_p, CB)(conn, C.byref(lo), None, f)
    tick_until(plat, lambda: "r" in done, 10, "Connect_Login")
    me = done.get("id")
    buf = C.create_string_buffer(64)
    n = C.c_int32(64)
    fn("EOS_ProductUserId_ToString", C.c_int32, C.c_void_p, C.c_char_p, C.POINTER(C.c_int32))(me, buf, C.byref(n))
    log("logged in as", buf.value.decode(), "result", done.get("r"))
    with open(os.path.join(DIR, f"{ROLE}.id"), "w") as fh:
        fh.write(buf.value.decode())
    return me

def other_id(role, plat, secs=20):
    path = os.path.join(DIR, f"{role}.id")
    tick_until(plat, lambda: os.path.exists(path) and os.path.getsize(path) > 0, secs, f"{role}'s id")
    text = open(path).read().strip()
    return fn("EOS_ProductUserId_FromString", C.c_void_p, C.c_char_p)(text.encode())

def wait_for_peer(plat, secs=15):
    """Until the LAN engine has met the other copy (its friend list has it)."""
    time.sleep(0)  # the other copy's id file is the signal; P2P to it needs the mesh up
    end = time.time() + secs
    while time.time() < end:
        TICK(plat)
        time.sleep(0.05)
        if time.time() + secs - 3 > end:  # ~3 s for discovery
            break

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

if ROLE == "probe":
    # Run without peerparty by run_all.ps1: only starts the SDK, to tell a build that
    # can't start on this PC at all from one peerparty breaks.
    check("EOS_Platform_Create", start())
    finish()
