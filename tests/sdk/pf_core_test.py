"""PlayFab's C SDK (PlayFabCore, as games ship it) signing in to PeerParty's
local PlayFab service: run with PPTEST_PFCORE_DIR = a folder holding copies of
libHttpClient.Win32.dll and PlayFabCore.Win32.dll, under `peerparty`.
The SDK's own HTTP stack (libHttpClient on WinHTTP) makes the calls; peerparty only
redirects the host and answers. API per github.com/PlayFab/PlayFabCSdk (PFCore.h,
PFAuthentication.h, PFEntity.h) and libHttpClient's XAsync.h."""
import ctypes as C, os, sys, time
DIR = os.environ["PPTEST_DIR"]
ROLE = sys.argv[1]
def log(*a):
    with open(os.path.join(DIR, f"{ROLE}.log"), "a") as fh:
        fh.write(" ".join(str(x) for x in a) + "\n")
RESULTS = []
def check(name, ok, detail=""):
    RESULTS.append(bool(ok)); log("PASS" if ok else "FAIL", name, detail)

d = os.environ["PPTEST_PFCORE_DIR"]
os.add_dll_directory(d)
lhc = C.CDLL(os.path.join(d, "libHttpClient.Win32.dll"))
core = C.CDLL(os.path.join(d, "PlayFabCore.Win32.dll"))
HR = C.c_int32

class XAsyncBlock(C.Structure):
    _fields_ = [("queue", C.c_void_p), ("context", C.c_void_p), ("callback", C.c_void_p), ("internal", C.c_ubyte * 32)]

class LoginReq(C.Structure):
    _fields_ = [("createAccount", C.c_bool), ("customId", C.c_char_p), ("customTags", C.c_void_p), ("customTagsCount", C.c_uint32),
                ("infoRequestParameters", C.c_void_p), ("playerSecret", C.c_char_p)]

def f(lib, name, *args):
    fn = getattr(lib, name); fn.restype = HR; fn.argtypes = list(args); return fn

hr = f(core, "PFInitialize", C.c_void_p)(None)
check("PFInitialize", hr == 0, hex(hr & 0xffffffff))
cfg = C.c_void_p()
title = os.environ.get("PPTEST_TITLE", "61FB1")
hr = f(core, "PFServiceConfigCreateHandle", C.c_char_p, C.c_char_p, C.POINTER(C.c_void_p))(
    f"https://{title}.playfabapi.com".encode(), title.encode(), C.byref(cfg))
check("PFServiceConfigCreateHandle", hr == 0, hex(hr & 0xffffffff))

ab = XAsyncBlock()
req = LoginReq(createAccount=True, customId=b"peerparty-test")
hr = f(core, "PFAuthenticationLoginWithCustomIDAsync", C.c_void_p, C.c_void_p, C.c_void_p)(cfg, C.byref(req), C.byref(ab))
check("LoginWithCustomIDAsync started", hr == 0, hex(hr & 0xffffffff))
status = f(lhc, "XAsyncGetStatus", C.c_void_p, C.c_bool)(C.byref(ab), True)
check("login completed with S_OK", status == 0, hex(status & 0xffffffff))
size = C.c_size_t()
f(core, "PFAuthenticationLoginWithCustomIDGetResultSize", C.c_void_p, C.POINTER(C.c_size_t))(C.byref(ab), C.byref(size))
buf = C.create_string_buffer(max(size.value, 1))
entity = C.c_void_p()
result = C.c_void_p()
hr = f(core, "PFAuthenticationLoginWithCustomIDGetResult", C.c_void_p, C.POINTER(C.c_void_p), C.c_size_t, C.c_void_p,
       C.POINTER(C.c_void_p), C.POINTER(C.c_size_t))(C.byref(ab), C.byref(entity), size, buf, C.byref(result), None)
check("GetResult gives an entity handle", hr == 0 and entity.value, f"hr={hex(hr & 0xffffffff)} size={size.value}")
if entity.value:
    ksize = C.c_size_t()
    f(core, "PFEntityGetEntityKeySize", C.c_void_p, C.POINTER(C.c_size_t))(entity, C.byref(ksize))
    kbuf = C.create_string_buffer(max(ksize.value, 1))
    class Key(C.Structure):
        _fields_ = [("id", C.c_char_p), ("type", C.c_char_p)]
    kp = C.POINTER(Key)()
    hr = f(core, "PFEntityGetEntityKey", C.c_void_p, C.c_size_t, C.c_void_p, C.POINTER(C.POINTER(Key)), C.POINTER(C.c_size_t))(
        entity, ksize, kbuf, C.byref(kp), None)
    ok = hr == 0 and bool(kp)
    check("entity key is a LAN title player", ok and kp.contents.type == b"title_player_account",
          (kp.contents.id, kp.contents.type) if ok else hex(hr & 0xffffffff))
    # An entity token for later calls (Party / Lobby take it).
    ab2 = XAsyncBlock()
    hr = f(core, "PFEntityGetEntityTokenAsync", C.c_void_p, C.c_void_p)(entity, C.byref(ab2))
    st = f(lhc, "XAsyncGetStatus", C.c_void_p, C.c_bool)(C.byref(ab2), True)
    tsize = C.c_size_t()
    f(core, "PFEntityGetEntityTokenResultSize", C.c_void_p, C.POINTER(C.c_size_t))(C.byref(ab2), C.byref(tsize))
    tbuf = C.create_string_buffer(max(tsize.value, 1))
    class Tok(C.Structure):
        _fields_ = [("token", C.c_char_p), ("expiration", C.c_void_p)]
    tp = C.POINTER(Tok)()
    hr2 = f(core, "PFEntityGetEntityTokenResult", C.c_void_p, C.c_size_t, C.c_void_p, C.POINTER(C.POINTER(Tok)), C.POINTER(C.c_size_t))(
        C.byref(ab2), tsize, tbuf, C.byref(tp), None)
    check("entity token", hr == 0 and st == 0 and hr2 == 0 and tp and tp.contents.token.startswith(b"peerparty-"),
          tp.contents.token if (hr2 == 0 and tp) else (hex(hr & 0xffffffff), hex(st & 0xffffffff), hex(hr2 & 0xffffffff)))
res = "ok" if all(RESULTS) else "failed"
log("RESULT", res)
open(os.path.join(DIR, f"{ROLE}.result"), "w").write(res)
os._exit(0)
