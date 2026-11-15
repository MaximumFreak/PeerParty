"""A starter that hands the game to a program it starts, with internet play set up (a
room code): run as `run_pair.ps1 starter_test.py -Roles A` with PPTEST_STARTER_DLL = a
copy of steam_api64.dll and PEERPARTY_ROOM set, under `peerparty --multi-instance`.

Role A is the starter: it opens a socket (so Winsock and PeerParty's forwarder load in
it, as in a real starter), starts this script again as the game, and waits for it. The
game loads the SDK DLL with ctypes (only loading it, so no Steam client is needed),
which is what tells PeerParty it is the game. Then the starter reads both processes'
PeerParty logs: the game must hold this PC's first player slot (its name without
" (2)"), and the starter no LAN at all. Before the fix, internet play's eager start
gave the starter the first slot, and the game came up as "Name (2)".

With PPTEST_STARTER_MENU = seconds, the starter waits that long before starting the
game (a menu): long enough to take itself for the game and start its LAN. Then it
must let its LAN and slot go once the game says it is the game, and the game must
still be this PC's first player."""
import ctypes, glob, os, re, socket, subprocess, sys, time

DIR = os.environ["PPTEST_DIR"]
ROLE = sys.argv[1]

if ROLE == "game":
    ctypes.CDLL(os.environ["PPTEST_STARTER_DLL"])
    time.sleep(6)
    sys.exit(0)

RESULTS = []

def log(*a):
    with open(os.path.join(DIR, f"{ROLE}.log"), "a", encoding="utf-8") as fh:
        fh.write(" ".join(str(x) for x in a) + "\n")

def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok)))
    log("PASS" if ok else "FAIL", name, detail)

MENU = float(os.environ.get("PPTEST_STARTER_MENU") or 0)
socket.socket().close()
time.sleep(0.3 + MENU)
game = subprocess.Popen([sys.executable, os.path.abspath(__file__), "game"], creationflags=0x08000000)
game.wait()
time.sleep(0.5)

def log_of(pid):
    found = glob.glob(os.path.join(DIR, "logs", f"*-{pid}.log"))
    return open(found[0], encoding="utf-8", errors="replace").read() if found else None

mine, theirs = log_of(os.getpid()), log_of(game.pid)
check("both processes have a PeerParty log", mine is not None and theirs is not None, (os.getpid(), game.pid))
mine, theirs = mine or "", theirs or ""
check("the game is the game", "is a game SDK, so this process is the game" in theirs)
check("the starter hands the launcher to it", "is the game: the launcher goes to it" in mine)
if MENU:
    check("the starter, after its menu, starts its LAN", "lan started" in mine)
    check("and lets it go once the game is the game", "this starter's LAN goes" in mine)
else:
    check("the starter starts no LAN", "lan started" not in mine, re.findall(r"lan started.*", mine))
    check("and says why", "no LAN in this one" in mine)
started = re.search(r"lan started id=\S+ name=(.*) game=", theirs)
check("the game's LAN started", started is not None)
check("with this PC's first player's name", started and not re.search(r" \(\d+\)$", started.group(1)), started and started.group(1))

bad = [n for n, ok in RESULTS if not ok]
log("RESULT", "ok" if not bad else "failed: " + ", ".join(bad))
with open(os.path.join(DIR, f"{ROLE}.result"), "w") as fh:
    fh.write("ok" if not bad else "failed: " + ", ".join(bad))
sys.exit(0 if not bad else 1)
