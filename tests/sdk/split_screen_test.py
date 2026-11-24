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
CLOSE_WAIT has passed, as a game that doesn't close itself is.

Saves: both games, at the same moment, open the same save in AppData/LocalLow (no
"save" in its path) without sharing, write their player into it and copy it with
CopyFile2 (.NET's File.Copy), and write the same HKCU/Software key; player 2 also writes
a file in the game's folder. Neither may meet the other: player 1's are the real ones,
player 2's are in its own folder (PEERPARTY_DATA_DIR/saves/.../player-2) and key. Then
`peerparty --player 2` alone (role P) reads player 2's save, and a second
`--player 2` meanwhile is turned away.

Input: each copy is told an input of its own (neither everything), and its game, left
idle, polls XInput and the keyboard as a game does: no copy sees a controller past its
own (XInput users 1-3 never connect, whatever is plugged in), the keyboard reaches one
copy at most, and each copy's log says its hook answered XInput from the session's
input block rather than passing it through. Needs no controller attached."""
import ctypes, glob, json, os, subprocess, sys, time, winreg

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

TAG = os.environ.get("PPTEST_SAVE_TAG", "x")
SAVE_DIR = os.path.join(os.environ.get("USERPROFILE", ""), "AppData", "LocalLow", f"PeerPartyTest{TAG}", "Game")
SAVE = os.path.join(SAVE_DIR, "data_full_-99.data")
KEY = rf"Software\PeerPartyTest{TAG}\Game"
GAME_FILE = os.path.join(os.path.dirname(sys.executable), f"PeerPartyTest{TAG}", "SteamData", "stats.ini")

def save_calls(slot):
    """The calls a game makes on its saves, as both players at once: what happened."""
    k = ctypes.WinDLL("kernel32", use_last_error=True)
    k.CreateFileW.restype = ctypes.c_void_p
    k.CreateFileW.argtypes = [ctypes.c_wchar_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_void_p]
    k.CopyFile2.restype = ctypes.c_long
    k.CopyFile2.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p, ctypes.c_void_p]
    out = {"slot": slot}
    # Together: each waits for the other before touching anything.
    open(os.path.join(DIR, f"ready-{slot}"), "w").close()
    deadline = time.time() + 30
    while time.time() < deadline and not all(os.path.exists(os.path.join(DIR, f"ready-{s}")) for s in (0, 1)):
        time.sleep(0.05)
    os.makedirs(SAVE_DIR, exist_ok=True)
    h = k.CreateFileW(SAVE, 0x40000000, 0, None, 2, 0x80, None)  # GENERIC_WRITE, no sharing, CREATE_ALWAYS
    out["open_error"] = 0 if h not in (None, ctypes.c_void_p(-1).value) else ctypes.get_last_error()
    if out["open_error"] == 0:
        data = f"player {slot + 1}".encode()
        written = ctypes.c_uint32()
        k.WriteFile(ctypes.c_void_p(h), data, len(data), ctypes.byref(written), None)
        time.sleep(1.5)
        k.CloseHandle(ctypes.c_void_p(h))
    out["copy_hr"] = k.CopyFile2(SAVE, SAVE + ".bak", None) & 0xFFFFFFFF
    with winreg.CreateKeyEx(winreg.HKEY_CURRENT_USER, KEY, 0, winreg.KEY_ALL_ACCESS) as key:
        winreg.SetValueEx(key, "Slot", 0, winreg.REG_DWORD, slot)
    if slot == 1:
        os.makedirs(os.path.dirname(GAME_FILE), exist_ok=True)
        with open(GAME_FILE, "w") as fh:
            fh.write("player 2")
    with open(SAVE, encoding="utf-8", errors="replace") as fh:
        out["reads"] = fh.read()
    return out

def input_calls(slot):
    """What an idle game reads of the controllers and keyboard, for 3 seconds: the
    return codes of XInputGetState for users 0-3, and the keys seen down."""
    u = ctypes.WinDLL("user32")
    u.GetAsyncKeyState.restype = ctypes.c_short
    u.GetAsyncKeyState.argtypes = [ctypes.c_int]
    try:
        xi = ctypes.WinDLL("xinput1_4")
    except OSError:
        xi = ctypes.WinDLL("xinput9_1_0")
    xi.XInputGetState.restype = ctypes.c_uint32
    xi.XInputGetState.argtypes = [ctypes.c_uint32, ctypes.c_void_p]
    codes = {i: set() for i in range(4)}
    keys = set()
    state = ctypes.create_string_buffer(16)
    end = time.time() + 3
    while time.time() < end:
        for i in range(4):
            codes[i].add(xi.XInputGetState(i, state))
        for vk in range(1, 255):
            if u.GetAsyncKeyState(vk) & 0x8000:
                keys.add(vk)
        time.sleep(0.05)
    return {"slot": slot, "xinput": {str(i): sorted(c) for i, c in codes.items()}, "keys": sorted(keys)}

if ROLE == "G":
    spec = json.loads(os.environ.get("PEERPARTY_INSTANCE") or "{}")
    with open(os.path.join(DIR, f"game-{spec.get('slot', -1)}.txt"), "w") as fh:
        fh.write(f"{os.getpid()} {json.dumps(spec)}")
    try:
        result = save_calls(spec.get("slot", -1))
    except Exception as e:  # said in the result, checked by the driver
        result = {"error": repr(e)}
    with open(os.path.join(DIR, f"save-{spec.get('slot', -1)}.json"), "w") as fh:
        json.dump(result, fh)
    try:
        result = input_calls(spec.get("slot", -1))
    except Exception as e:
        result = {"error": repr(e)}
    with open(os.path.join(DIR, f"input-{spec.get('slot', -1)}.json"), "w") as fh:
        json.dump(result, fh)
    # Until the session ends it (or the driver's deadline has long passed).
    time.sleep(120)
    sys.exit(0)

if ROLE == "P":
    # Alone as player 2: its save is the one it wrote in the session.
    spec = json.loads(os.environ.get("PEERPARTY_INSTANCE") or "{}")
    try:
        with open(SAVE, encoding="utf-8", errors="replace") as fh:
            reads = fh.read()
    except OSError as e:
        reads = repr(e)
    with open(os.path.join(DIR, "solo.json"), "w") as fh:
        json.dump({"reads": reads, "spec": spec, "slot": os.environ.get("PEERPARTY_PLAYER_SLOT")}, fh)
    time.sleep(8)
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
# Players 2-4's saves in this run's own folder, never the player's.
data = env.get("PEERPARTY_DATA_DIR") or os.path.join(DIR, "data")
env["PEERPARTY_DATA_DIR"] = data
env["PPTEST_SAVE_TAG"] = TAG = f"{os.getpid()}"
SAVE_DIR = os.path.join(os.environ.get("USERPROFILE", ""), "AppData", "LocalLow", f"PeerPartyTest{TAG}", "Game")
SAVE = os.path.join(SAVE_DIR, "data_full_-99.data")
KEY = rf"Software\PeerPartyTest{TAG}\Game"
GAME_FILE = os.path.join(os.path.dirname(interpreter()), f"PeerPartyTest{TAG}", "SteamData", "stats.ini")

def reg_slot(path):
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, path) as k:
            return winreg.QueryValueEx(k, "Slot")[0]
    except OSError:
        return None

def own_keys():
    """The games' keys under HKCU/Software/PeerParty/Saves (each player's are under them)."""
    found = []
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, r"Software\PeerParty\Saves") as saves:
            i = 0
            while True:
                try:
                    found.append(rf"Software\PeerParty\Saves\{winreg.EnumKey(saves, i)}")
                except OSError:
                    break
                i += 1
    except OSError:
        pass
    return found

def clean():
    import shutil
    shutil.rmtree(os.path.dirname(SAVE_DIR), ignore_errors=True)
    shutil.rmtree(os.path.dirname(os.path.dirname(GAME_FILE)), ignore_errors=True)
    for path in [rf"Software\PeerPartyTest{TAG}\Game", rf"Software\PeerPartyTest{TAG}"]:
        try:
            winreg.DeleteKey(winreg.HKEY_CURRENT_USER, path)
        except OSError:
            pass
    for game in own_keys():
        if os.path.basename(game).startswith("python"):
            subprocess.run(["reg", "delete", rf"HKCU\{game}", "/f"], creationflags=NO_WINDOW, capture_output=True)

# Whatever happens below, nothing of the test's stays in the player's folders or registry.
import atexit
atexit.register(clean)

# The copy is the programs started from the game's folder (tree.rs). Both the starter
# and the game it starts are this interpreter, so that folder is the interpreter's:
# wherever Python is installed, the session keeps the game after the starter ends.
env["PEERPARTY_GAME_ROOT"] = os.path.dirname(interpreter())
logs = os.path.join(DIR, "logs")
# The games run a copy of this script in the work folder: this one may be in Documents,
# which is a save folder, player 2's own (its copy wouldn't find the script there).
import shutil
GAME = os.path.join(DIR, "split_screen_game.py")
shutil.copyfile(HERE, GAME)
args = [pp, "--split-screen", "--instances", "2", "--set", "overlay=off", "--set", "toasts=off", "--set", "voice=off",
        "--log-dir", logs, interpreter(), GAME, "S"]
err = open(os.path.join(DIR, "peerparty.err"), "w")
session = subprocess.Popen(args, env=env, stderr=err, stdout=subprocess.DEVNULL, creationflags=NO_WINDOW, cwd=DIR)
log("session pid", session.pid)

started = wait_for(lambda: all(read_pair(f"game-{s}.txt") for s in (0, 1)), time.time() + 60)
check("both players' games started", started, sorted(os.listdir(DIR)))
games = {}
specs = {}
for s in (0, 1):
    got = read_pair(f"game-{s}.txt")
    if got:
        games[s] = int(got[0])
        spec = specs[s] = json.loads(got[1])
        pane = spec.get("pane") or {}
        check(f"player {s + 1}'s game has its pane", pane.get("width", 0) > 0 and spec.get("compositor_capture") is not None, got[1])
        check(f"player {s + 1}'s game is told the focus is its own", spec.get("focus_spoof") is True, got[1])
inputs = [specs.get(s, {}).get("input_assignment") or {} for s in (0, 1)]
check("each player is told an input of its own", inputs[0] != inputs[1] and all(a.get("kind") not in (None, "all", "none") for a in inputs), inputs)
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

# Saves: each player's own, made at the same moment.
done = wait_for(lambda: all(os.path.exists(os.path.join(DIR, f"save-{s}.json")) for s in (0, 1)), time.time() + 40)
check("both games made their save calls", done)
saved = {}
for s in (0, 1):
    try:
        with open(os.path.join(DIR, f"save-{s}.json")) as fh:
            saved[s] = json.load(fh)
    except (OSError, ValueError):
        saved[s] = {}
for s in (0, 1):
    r = saved.get(s, {})
    check(f"player {s + 1}'s exclusive open of its save worked (no sharing violation)", r.get("open_error") == 0, r)
    check(f"player {s + 1}'s CopyFile2 of its save worked", r.get("copy_hr", 1) < 0x80000000, r)
    check(f"player {s + 1} reads its own save", r.get("reads") == f"player {s + 1}", r)
real = open(SAVE, encoding="utf-8", errors="replace").read() if os.path.exists(SAVE) else None
check("player 1's save is the real one", real == "player 1", real)
mine = glob.glob(os.path.join(data, "saves", "*", "player-2", "fs", "LocalLow", f"PeerPartyTest{TAG}", "Game", "data_full_-99.data"))
check("player 2's save is in its own folder", len(mine) == 1 and open(mine[0]).read() == "player 2", mine)
check("and its CopyFile2 copy too", len(mine) == 1 and os.path.exists(mine[0] + ".bak"))
check("nothing of player 2's beside player 1's", not any(".peerparty-" in n for n in os.listdir(SAVE_DIR)) if os.path.isdir(SAVE_DIR) else False, os.listdir(SAVE_DIR) if os.path.isdir(SAVE_DIR) else None)
check("player 1's registry key is the real one", reg_slot(KEY) == 0, reg_slot(KEY))
theirs = [reg_slot(rf"{g}\p2\PeerPartyTest{TAG}\Game") for g in own_keys()]
check("player 2's registry key is its own", 1 in theirs, theirs)
check("the game's folder is never written", not os.path.exists(GAME_FILE), GAME_FILE)
check("player 2's write there is in its own folder", bool(glob.glob(os.path.join(data, "saves", "*", "player-2", "fs", "GameDir", f"PeerPartyTest{TAG}", "SteamData", "stats.ini"))))

# Input: what each idle game read (its log is checked once the session has ended).
done = wait_for(lambda: all(os.path.exists(os.path.join(DIR, f"input-{s}.json")) for s in (0, 1)), time.time() + 20)
check("both games polled their input", done)
polled = {}
for s in (0, 1):
    try:
        with open(os.path.join(DIR, f"input-{s}.json")) as fh:
            polled[s] = json.load(fh)
    except (OSError, ValueError):
        polled[s] = {}
NOT_CONNECTED = 1167
for s in (0, 1):
    r = polled.get(s, {})
    xi = r.get("xinput") or {}
    check(f"player {s + 1}'s game sees no controller past its own", len(xi) == 4 and all(xi.get(str(i)) == [NOT_CONNECTED] for i in (1, 2, 3)), r)
    check(f"player {s + 1}'s controller is there or not, nothing else", set(xi.get("0") or [-1]) <= {0, NOT_CONNECTED}, r)
check("the keyboard reaches one copy at most", not (polled.get(0, {}).get("keys") and polled.get(1, {}).get("keys")), polled)

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
check("player 2's log says its saves are its own", "saves: player 2's saves are its own" in notes)
for s, pid in games.items():
    own = "".join(open(f, encoding="utf-8", errors="replace").read() for f in glob.glob(os.path.join(logs, f"*-{pid}.log")))
    check(f"player {s + 1}'s hook reads the session's input block", "input: mapped virtual input block" in own, pid)
    check(f"player {s + 1}'s hook answers XInput from it, not the real controllers",
          "input: XInput controller 0 polled before a controller took this copy's seat" in own or "input: the game reads XInput (hook slot" in own, pid)
if session.poll() is None:
    session.kill()

# Alone as player 2: its saves; a second player 2 is turned away meanwhile.
solo_args = [pp, "--player", "2", "--set", "overlay=off", "--set", "toasts=off", "--log-dir", logs, interpreter(), GAME, "P"]
solo_err = open(os.path.join(DIR, "solo.err"), "w")
solo = subprocess.Popen(solo_args, env=env, stderr=solo_err, stdout=subprocess.DEVNULL, creationflags=NO_WINDOW, cwd=DIR)
got_solo = wait_for(lambda: os.path.exists(os.path.join(DIR, "solo.json")), time.time() + 40)
check("peerparty --player 2 started the game", got_solo)
if got_solo:
    with open(os.path.join(DIR, "solo.json")) as fh:
        r = json.load(fh)
    check("alone, player 2 reads player 2's save", r.get("reads") == "player 2", r)
    check("as player 2, not in a pane", r.get("slot") == "1" and not r.get("spec", {}).get("focus_spoof") and not r.get("spec", {}).get("pane"), r)
    second = subprocess.run(solo_args, env=env, capture_output=True, text=True, creationflags=NO_WINDOW, timeout=60, cwd=DIR)
    check("a second player 2 is turned away", second.returncode != 0 and "already playing" in second.stderr, (second.returncode, second.stderr[-300:]))
try:
    solo.wait(timeout=60)
except subprocess.TimeoutExpired:
    solo.kill()
solo_err.close()
finish()
