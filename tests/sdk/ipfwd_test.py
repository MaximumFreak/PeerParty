"""Typed-IP games on async sockets, between two copies joined by a room code over
loopback (LAN discovery off, so each sees the other only at its stand-in address,
198.18.x.y). Run as

    run_pair.ps1 ipfwd_test.py -Env PEERPARTY_ROOM=<code>,PEERPARTY_LAN_DISCOVERY=0,
        PEERPARTY_INET_DHT=0,PEERPARTY_INET_STUN=0,PEERPARTY_INET_PORTMAP=0,PEERPARTY_IPV6=0
        -RoleEnv A:PEERPARTY_INET_PORT=<p1>,B:PEERPARTY_INET_PORT=<p2>,
        B:PEERPARTY_INET_PEERS=127.0.0.1:<p1>

(tests/run_all.ps1 does). No SDK: Winsock and the thread pool through ctypes, as a .NET
or C++ game calls them.

A: a plain UDP socket on 127.0.0.1:PORT. Every datagram must come from B's stand-in;
it answers each one back to that sender.
B: reads A's stand-in from its own peerparty log, then for each way a game can collect
an overlapped WSARecvFrom (a completion routine, a completion port with
GetQueuedCompletionStatus and its Ex form, WSAGetOverlappedResult, GetOverlappedResult,
a thread-pool IO object, BindIoCompletionCallback) posts the receive first, sends to A's
stand-in, collects the answer that way, and checks it came from A's stand-in, not from
a loopback address."""
import ctypes, glob, os, re, socket, sys, time
from ctypes import wintypes

DIR = os.environ["PPTEST_DIR"]
ROLE = sys.argv[1]
PORT = 47913
RESULTS = []


def _crash(kind, value, tb):
    import traceback

    log("FAIL exception", "".join(traceback.format_exception(kind, value, tb)))
    open(os.path.join(DIR, f"{ROLE}.result"), "w").write("failed")
    os._exit(1)


sys.excepthook = _crash


def log(*a):
    with open(os.path.join(DIR, f"{ROLE}.log"), "a", encoding="utf-8") as fh:
        fh.write(" ".join(str(x) for x in a) + "\n")


def check(name, ok, detail=""):
    RESULTS.append(bool(ok))
    log("PASS" if ok else "FAIL", name, detail)
    return ok


def finish():
    res = "ok" if RESULTS and all(RESULTS) else "failed"
    log("RESULT", res)
    open(os.path.join(DIR, f"{ROLE}.result"), "w").write(res)
    os._exit(0)


def is_stand_in(ip):
    a, b = (int(x) for x in ip.split(".")[:2])
    return a == 198 and b & 0xFE == 18


# ---- A: answers whatever arrives ----

if ROLE == "A":
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind(("127.0.0.1", PORT))
    s.settimeout(90)
    for i in range(7):
        try:
            data, sender = s.recvfrom(2048)
        except socket.timeout:
            check(f"datagram {i} arrived", False, "timed out")
            finish()
        check(f"datagram {i} came from B's stand-in", is_stand_in(sender[0]), f"{sender} {data!r}")
        s.sendto(b"reply " + data, sender)
    time.sleep(2)
    finish()

# ---- B: every way to collect an overlapped receive ----

ws2 = ctypes.WinDLL("ws2_32", use_last_error=True)
k32 = ctypes.WinDLL("kernel32", use_last_error=True)
SOCKET = ctypes.c_size_t
HANDLE = ctypes.c_void_p


class OVERLAPPED(ctypes.Structure):
    _fields_ = [("Internal", ctypes.c_size_t), ("InternalHigh", ctypes.c_size_t), ("Offset", wintypes.DWORD), ("OffsetHigh", wintypes.DWORD), ("hEvent", HANDLE)]


class WSABUF(ctypes.Structure):
    _fields_ = [("len", wintypes.ULONG), ("buf", ctypes.c_char_p)]


class SOCKADDR_IN(ctypes.Structure):
    _fields_ = [("family", ctypes.c_short), ("port", ctypes.c_ushort), ("addr", ctypes.c_ubyte * 4), ("zero", ctypes.c_char * 8)]


class OVERLAPPED_ENTRY(ctypes.Structure):
    _fields_ = [("key", ctypes.c_size_t), ("ov", ctypes.POINTER(OVERLAPPED)), ("internal", ctypes.c_size_t), ("bytes", wintypes.DWORD)]


ROUTINE = ctypes.WINFUNCTYPE(None, wintypes.DWORD, wintypes.DWORD, ctypes.POINTER(OVERLAPPED), wintypes.DWORD)
BIND_ROUTINE = ctypes.WINFUNCTYPE(None, wintypes.DWORD, wintypes.DWORD, ctypes.POINTER(OVERLAPPED))
TP_IO_CALLBACK = ctypes.WINFUNCTYPE(None, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p, wintypes.ULONG, ctypes.c_size_t, ctypes.c_void_p)

ws2.WSASocketW.restype = SOCKET
ws2.WSASocketW.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD]
ws2.bind.argtypes = [SOCKET, ctypes.c_void_p, ctypes.c_int]
ws2.sendto.argtypes = [SOCKET, ctypes.c_char_p, ctypes.c_int, ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
ws2.WSARecvFrom.argtypes = [SOCKET, ctypes.POINTER(WSABUF), wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p, ctypes.POINTER(ctypes.c_int), ctypes.POINTER(OVERLAPPED), ctypes.c_void_p]
ws2.WSAGetOverlappedResult.argtypes = [SOCKET, ctypes.POINTER(OVERLAPPED), ctypes.POINTER(wintypes.DWORD), wintypes.BOOL, ctypes.POINTER(wintypes.DWORD)]
ws2.closesocket.argtypes = [SOCKET]
k32.CreateEventW.restype = HANDLE
k32.CreateEventW.argtypes = [ctypes.c_void_p, wintypes.BOOL, wintypes.BOOL, ctypes.c_wchar_p]
k32.SetEvent.argtypes = [HANDLE]
k32.WaitForSingleObject.argtypes = [HANDLE, wintypes.DWORD]
k32.SleepEx.argtypes = [wintypes.DWORD, wintypes.BOOL]
k32.CreateIoCompletionPort.restype = HANDLE
k32.CreateIoCompletionPort.argtypes = [HANDLE, HANDLE, ctypes.c_size_t, wintypes.DWORD]
k32.GetQueuedCompletionStatus.argtypes = [HANDLE, ctypes.POINTER(wintypes.DWORD), ctypes.POINTER(ctypes.c_size_t), ctypes.POINTER(ctypes.POINTER(OVERLAPPED)), wintypes.DWORD]
k32.GetQueuedCompletionStatusEx.argtypes = [HANDLE, ctypes.POINTER(OVERLAPPED_ENTRY), wintypes.ULONG, ctypes.POINTER(wintypes.ULONG), wintypes.DWORD, wintypes.BOOL]
k32.GetOverlappedResult.argtypes = [HANDLE, ctypes.POINTER(OVERLAPPED), ctypes.POINTER(wintypes.DWORD), wintypes.BOOL]
k32.CreateThreadpoolIo.restype = ctypes.c_void_p
k32.CreateThreadpoolIo.argtypes = [HANDLE, TP_IO_CALLBACK, ctypes.c_void_p, ctypes.c_void_p]
k32.StartThreadpoolIo.argtypes = [ctypes.c_void_p]
k32.CloseThreadpoolIo.argtypes = [ctypes.c_void_p]
k32.BindIoCompletionCallback.argtypes = [HANDLE, BIND_ROUTINE, wintypes.ULONG]
k32.CloseHandle.argtypes = [HANDLE]


def sockaddr(ip, port):
    sa = SOCKADDR_IN()
    sa.family = socket.AF_INET
    sa.port = socket.htons(port)
    sa.addr[:] = list(socket.inet_aton(ip))
    return sa


def stand_in_of_a():
    # The hook logs each internet peer's stand-in as it arrives; this copy's own log
    # names only A.
    deadline = time.time() + 60
    while time.time() < deadline:
        for path in glob.glob(os.path.join(DIR, "logs", f"*-{os.getpid()}.log")):
            m = re.search(r"ipfwd: .* is at (198\.1[89]\.\d+\.\d+)", open(path, encoding="utf-8", errors="replace").read())
            if m:
                return m.group(1)
        time.sleep(0.25)
    return None


class Recv:
    """A UDP socket with one overlapped WSARecvFrom posted."""

    def __init__(self):
        self.s = ws2.WSASocketW(socket.AF_INET, socket.SOCK_DGRAM, 17, None, 0, 0x01)  # WSA_FLAG_OVERLAPPED
        local = sockaddr("127.0.0.1", 0)
        assert ws2.bind(self.s, ctypes.byref(local), ctypes.sizeof(local)) == 0, ctypes.get_last_error()
        self.data = ctypes.create_string_buffer(2048)
        self.buf = WSABUF(2048, ctypes.cast(self.data, ctypes.c_char_p))
        self.frm = SOCKADDR_IN()
        self.frmlen = ctypes.c_int(ctypes.sizeof(self.frm))
        self.flags = wintypes.DWORD(0)
        self.got = wintypes.DWORD(0)
        self.ov = OVERLAPPED()

    def post(self, routine=None):
        r = ws2.WSARecvFrom(self.s, ctypes.byref(self.buf), 1, ctypes.byref(self.got), ctypes.byref(self.flags), ctypes.byref(self.frm), ctypes.byref(self.frmlen), ctypes.byref(self.ov), routine)
        err = ctypes.get_last_error()
        return r != 0 and err == 997  # pending: the path under test

    def sender(self):
        return socket.inet_ntoa(bytes(self.frm.addr)), socket.ntohs(self.frm.port)

    def close(self):
        ws2.closesocket(self.s)


def send_to_a(r, a_ip, what):
    dest = sockaddr(a_ip, PORT)
    n = ws2.sendto(r.s, what, len(what), 0, ctypes.byref(dest), ctypes.sizeof(dest))
    return n == len(what)


def run(name, a_ip, setup, wait):
    r = Recv()
    ctx = setup(r)
    pending = r.post(ctx.get("routine"))
    check(f"{name}: the receive is pending", pending)
    check(f"{name}: sent", send_to_a(r, a_ip, name.encode()))
    ok = wait(r, ctx)
    check(f"{name}: completed", ok)
    ip, port = r.sender()
    check(f"{name}: came from A's stand-in", ok and (ip, port) == (a_ip, PORT), f"{ip}:{port} {r.data.value!r}")
    r.close()


def by_routine(a_ip):
    done = []
    cb = ROUTINE(lambda err, n, ov, fl: done.append(err))

    def wait(r, ctx):
        deadline = time.time() + 15
        while not done and time.time() < deadline:
            k32.SleepEx(200, True)
        return done == [0]

    run("completion routine", a_ip, lambda r: {"routine": ctypes.cast(cb, ctypes.c_void_p)}, wait)


def by_port(a_ip, ex):
    def setup(r):
        return {"port": k32.CreateIoCompletionPort(HANDLE(r.s), None, 7, 0)}

    def wait(r, ctx):
        if ex:
            entries = (OVERLAPPED_ENTRY * 4)()
            n = wintypes.ULONG(0)
            ok = k32.GetQueuedCompletionStatusEx(ctx["port"], entries, 4, ctypes.byref(n), 15000, False)
            result = bool(ok) and n.value == 1 and entries[0].internal == 0
        else:
            n, key, ov = wintypes.DWORD(0), ctypes.c_size_t(0), ctypes.POINTER(OVERLAPPED)()
            result = bool(k32.GetQueuedCompletionStatus(ctx["port"], ctypes.byref(n), ctypes.byref(key), ctypes.byref(ov), 15000))
        k32.CloseHandle(ctx["port"])
        return result

    run("GetQueuedCompletionStatusEx" if ex else "GetQueuedCompletionStatus", a_ip, setup, wait)


def by_result(a_ip, wsa):
    def setup(r):
        r.ov.hEvent = k32.CreateEventW(None, True, False, None)
        return {}

    def wait(r, ctx):
        n = wintypes.DWORD(0)
        if wsa:
            flags = wintypes.DWORD(0)
            return bool(ws2.WSAGetOverlappedResult(r.s, ctypes.byref(r.ov), ctypes.byref(n), True, ctypes.byref(flags)))
        return bool(k32.GetOverlappedResult(HANDLE(r.s), ctypes.byref(r.ov), ctypes.byref(n), True))

    run("WSAGetOverlappedResult" if wsa else "GetOverlappedResult", a_ip, setup, wait)


def by_threadpool(a_ip):
    done = k32.CreateEventW(None, True, False, None)
    results = []

    def callback(instance, context, ov, result, n, io):
        results.append(result)
        k32.SetEvent(done)

    cb = TP_IO_CALLBACK(callback)

    def setup(r):
        io = k32.CreateThreadpoolIo(HANDLE(r.s), cb, None, None)
        k32.StartThreadpoolIo(io)
        return {"io": io}

    def wait(r, ctx):
        ok = k32.WaitForSingleObject(done, 15000) == 0 and results == [0]
        k32.CloseThreadpoolIo(ctx["io"])
        return ok

    run("CreateThreadpoolIo", a_ip, setup, wait)


def by_bind(a_ip):
    done = k32.CreateEventW(None, True, False, None)
    results = []

    def routine(err, n, ov):
        results.append(err)
        k32.SetEvent(done)

    cb = BIND_ROUTINE(routine)
    BY_BIND_KEEP.append(cb)

    def setup(r):
        check("BindIoCompletionCallback: bound", k32.BindIoCompletionCallback(HANDLE(r.s), cb, 0))
        return {}

    run("BindIoCompletionCallback", a_ip, setup, lambda r, ctx: k32.WaitForSingleObject(done, 15000) == 0 and results == [0])


BY_BIND_KEEP = []
a_ip = stand_in_of_a()
if not check("A's stand-in is in our log", a_ip is not None):
    finish()
log("A is at", a_ip)
time.sleep(1)
by_routine(a_ip)
by_port(a_ip, ex=False)
by_port(a_ip, ex=True)
by_result(a_ip, wsa=True)
by_result(a_ip, wsa=False)
by_threadpool(a_ip)
by_bind(a_ip)
finish()
