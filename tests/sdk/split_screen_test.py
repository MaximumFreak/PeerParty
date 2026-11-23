"""A two-player split-screen session from start to Close, with a starter in each copy:
run as `run_pair.ps1 split_screen_test.py -Roles A -Direct` (PPTEST_PEERPARTY = the
peerparty.exe to test). Needs no SDK DLL.

Role A is the driver, run without PeerParty: it starts `peerparty --split-screen
--instances 2` on this script (role S) with the session's window hidden
(PEERPARTY_SESSION_HIDDEN=1), so nothing covers the screen. Each copy is a starter (S)
that starts this script again as its game (G) and ends at once, as a game's starter
does. Each game says which player it is (the session's PEERPARTY_INSTANCE), and stays
until it's ended.

The driver checks that both players' games started and outlive their starters (the
session keeps a pane whose starter has handed over: each copy is a job of its own), then
asks the session to close as the launcher's Close all does (its close event) and checks
that it ends both games and exits soon after: each game is asked, then ended once
CLOSE_WAIT has passed, as a game that doesn't close itself is."""
import ctypes, glob, json, os, subprocess, sys, time

DIR = os.environ["PPTEST_DIR"]
ROLE = sys.argv[1]
HERE = os.path.abspath(__file__)
NO_WINDOW = 0x08000000

def interpreter():
    """pythonw beside python: no console window of its own on screen."""
    w = os.path.join(os.path.dirname(sys.executable), "pythonw.exe")
    return w if os.path.exists(w) else sys.executable

if ROLE == "S":
    spec = json.loads(os.environ.get("PEERPARTY_INSTANCE") or "{}")
    slot = spec.get("slot", -1)
    game = subprocess.Popen([interpreter(), HERE, "G"], creationflags=NO_WINDOW)
    with open(os.path.join(DIR, f"starter-{slot}.txt"), "w") as fh:
        fh.write(f"{os.getpid()} {game.pid}")
    sys.exit(0)

if ROLE == "G":
    spec = json.loads(os.environ.get("PEERPARTY_INSTANCE") or "{}")
    with open(os.path.join(DIR, f"game-{spec.get('slot', -1)}.txt"), "w") as fh:
        fh.write(f"{os.getpid()} {json.dumps(spec)}")
    # Until the session ends it (or the driver's deadline has long passed).
    time.sleep(120)
    sys.exit(0)

RESULTS = []

def log(*a):
    with open(os.path.join(DIR, f"{ROLE}.log"), "a", encoding="utf-8") as fh:
        fh.write(" ".join(str(x) for x in a) + "\n")

def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok)))
    log("PASS" if ok else "FAIL", name, detail)

def finish():
    bad = [n for n, ok in RESULTS if not ok]
    log("RESULT", "ok" if not bad else "failed: " + ", ".join(bad))
    with open(os.path.join(DIR, f"{ROLE}.result"), "w") as fh:
        fh.write("ok" if not bad else "failed: " + ", ".join(bad))
    sys.exit(0 if not bad else 1)

k32 = ctypes.WinDLL("kernel32", use_last_error=True)
k32.OpenProcess.restype = ctypes.c_void_p
k32.OpenEventW.restype = ctypes.c_void_p

def alive(pid):
    h = k32.OpenProcess(0x1000, False, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
    if not h:
        return False
    code = ctypes.c_ulong()
    ok = k32.GetExitCodeProcess(ctypes.c_void_p(h), ctypes.byref(code))
    k32.CloseHandle(ctypes.c_void_p(h))
    return bool(ok) and code.value == 259  # STILL_ACTIVE

def read_pair(name):
    try:
        with open(os.path.join(DIR, name)) as fh:
            return fh.read().split(" ", 1)
    except OSError:
        return None

def wait_for(what, until):
    while time.time() < until:
        if what():
            return True
        time.sleep(0.2)
    return what()

pp = os.environ.get("PPTEST_PEERPARTY")
if not pp or not os.path.exists(pp):
    log("SKIP no peerparty.exe to test (PPTEST_PEERPARTY)")
    finish()

env = dict(os.environ)
env["PEERPARTY_SESSION_HIDDEN"] = "1"
# The copy is the programs started from the game's folder (tree.rs). Both the starter
# and the game it starts are this interpreter, so that folder is the interpreter's:
# wherever Python is installed, the session keeps the game after the starter ends.
env["PEERPARTY_GAME_ROOT"] = os.path.dirname(interpreter())
logs = os.path.join(DIR, "logs")
args = [pp, "--split-screen", "--instances", "2", "--set", "overlay=off", "--set", "toasts=off", "--set", "voice=off",
        "--log-dir", logs, interpreter(), HERE, "S"]
err = open(os.path.join(DIR, "peerparty.err"), "w")
session = subprocess.Popen(args, env=env, stderr=err, stdout=subprocess.DEVNULL, creationflags=NO_WINDOW, cwd=os.path.dirname(HERE))
log("session pid", session.pid)

started = wait_for(lambda: all(read_pair(f"game-{s}.txt") for s in (0, 1)), time.time() + 60)
check("both players' games started", started, sorted(os.listdir(DIR)))
games = {}
for s in (0, 1):
    got = read_pair(f"game-{s}.txt")
    if got:
        games[s] = int(got[0])
        spec = json.loads(got[1])
        pane = spec.get("pane") or {}
        check(f"player {s + 1}'s game has its pane", pane.get("width", 0) > 0 and spec.get("compositor_capture") is not None, got[1])
starters = {}
for s in (0, 1):
    got = read_pair(f"starter-{s}.txt")
    if got:
        starters[s] = int(got[0])
check("each copy started from its own starter", len(starters) == 2, starters)
handed = wait_for(lambda: not any(alive(p) for p in starters.values()), time.time() + 15)
check("the starters ended", len(starters) == 2 and handed)
# The session keeps going with the games alone.
time.sleep(2)
check("the session outlives the starters", session.poll() is None, session.poll())
check("both games still run", len(games) == 2 and all(alive(p) for p in games.values()), {s: alive(p) for s, p in games.items()})

# Close all, as the launcher's window does it.
ev = k32.OpenEventW(0x0002, False, f"Local\\peerparty-close-{session.pid}")  # EVENT_MODIFY_STATE
check("the session listens for Close all", bool(ev))
asked = time.time()
if ev:
    k32.SetEvent(ctypes.c_void_p(ev))
    k32.CloseHandle(ctypes.c_void_p(ev))
try:
    code = session.wait(timeout=30)
except subprocess.TimeoutExpired:
    code = None
took = time.time() - asked
check("the session ends after Close all", code is not None, f"{took:.1f} s")
check("within the close wait and a little", took < 20, f"{took:.1f} s")
check("both games are ended", not any(alive(p) for p in games.values()), {s: alive(p) for s, p in games.items()})
err.close()
out = open(os.path.join(DIR, "peerparty.err"), encoding="utf-8", errors="replace").read()
check("peerparty started both players", "started player 1" in out and "started player 2" in out)
check("and says it hid the session's window", "its window is hidden" in out)
check("and ended each game it had asked to close", "player 1's game is ended" in out and "player 2's game is ended" in out)
notes = "".join(open(f, encoding="utf-8", errors="replace").read() for f in glob.glob(os.path.join(logs, "*.log")))
check("each copy's log says the session closed it", notes.count("session: closing") >= 2)
if session.poll() is None:
    session.kill()
finish()
