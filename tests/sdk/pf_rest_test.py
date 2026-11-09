"""PlayFab's web API between two copies, through PeerParty's local PlayFab service: run as
`run_pair.ps1 pf_rest_test.py -Env PEERPARTY_PLAYFAB=1` (A makes a lobby, B finds and
joins it). No SDK: the calls and JSON are PlayFab's REST API (learn.microsoft.com
rest/api/playfab: Client, Lobby). Python's DNS and TLS go through PeerParty's redirect
and certificate, as in ugs_test.py (PPTEST_UNVERIFIED=1 skips the certificate check).

A: signs in, creates a lobby (search, lobby and member data), sees B join and B's
member data change, keeps statistics and user data (on disk), sees B in its friends
list and its own score on a leaderboard, and deletes the lobby once B has left.
B: signs in, finds the lobby with a filter, joins it by connection string, changes its
member data, then leaves."""
import http.client, json, os, socket, ssl, sys, time

DIR = os.environ["PPTEST_DIR"]
ROLE = sys.argv[1]
os.environ["PEERPARTY_PLAYFAB_DIR"] = os.path.join(DIR, f"playfab-{ROLE}")
HOST = "lbt01.playfabapi.com"
# The first lookup starts PeerParty's PlayFab service, which lists its certificate in the
# ROOT store this process reads from then on; Python reads that store when the context
# is made, so look the host up first (as a game's first PlayFab call does).
socket.getaddrinfo(HOST, 443)
if os.environ.get("PPTEST_UNVERIFIED") == "1":
    CTX = ssl._create_unverified_context()
else:
    CTX = ssl.create_default_context()
    CTX.verify_flags &= ~getattr(ssl, "VERIFY_X509_STRICT", 0)
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


TOKEN = None


def call(path, body):
    c = http.client.HTTPSConnection(HOST, 443, context=CTX, timeout=30)
    h = {"Content-Type": "application/json"}
    if TOKEN:
        h["X-EntityToken"] = TOKEN
    c.request("POST", path, json.dumps(body), h)
    r = c.getresponse()
    raw = r.read()
    c.close()
    j = json.loads(raw) if raw else {}
    return r.status, j.get("data", j)


def wait_for(pred, secs=30):
    end = time.time() + secs
    while time.time() < end:
        got = pred()
        if got:
            return got
        time.sleep(0.3)
    return None


def put_file(name, text):
    p = os.path.join(DIR, name)
    open(p + ".tmp", "w").write(text)
    os.replace(p + ".tmp", p)


def read_file(name):
    p = os.path.join(DIR, name)
    return open(p).read() if os.path.exists(p) else None


st, login = call("/Client/LoginWithCustomID", {"TitleId": "LBT01", "CustomId": f"pptest-{ROLE}", "CreateAccount": True})
check("login", st == 200 and login.get("EntityToken"), st)
TOKEN = login["EntityToken"]["EntityToken"]
me = login["EntityToken"]["Entity"]
put_file(f"{ROLE}.entity", me["Id"])

if ROLE == "A":
    st, made = call("/Lobby/CreateLobby", {
        "AccessPolicy": "Public", "MaxPlayers": 4, "OwnerMigrationPolicy": "Automatic", "UseConnections": True,
        "Owner": me, "SearchData": {"string_key1": "pptest"}, "LobbyData": {"map": "caves"},
        "Members": [{"MemberEntity": me, "MemberData": {"ready": "0"}}],
    })
    check("CreateLobby", st == 200 and made.get("LobbyId") and made.get("ConnectionString"), (st, made))
    lobby = made["LobbyId"]
    put_file("lobby", lobby)

    def members():
        st, got = call("/Lobby/GetLobby", {"LobbyId": lobby})
        return got.get("Lobby", {}).get("Members", []) if st == 200 else []

    other = wait_for(lambda: next((m for m in members() if m["MemberEntity"]["Id"] != me["Id"]), None), 60)
    check("B joined", other is not None)
    st, got = call("/Lobby/GetLobby", {"LobbyId": lobby})
    lob = got.get("Lobby", {})
    check("lobby data and owner", lob.get("LobbyData", {}).get("map") == "caves" and lob.get("Owner", {}).get("Id") == me["Id"], lob)
    ready = wait_for(lambda: next((m for m in members() if m["MemberEntity"]["Id"] != me["Id"] and m.get("MemberData", {}).get("ready") == "1"), None))
    check("B's member data came through", ready is not None)
    put_file("a_saw_ready", "1")

    # Statistics, leaderboard, user data, friends.
    st, _ = call("/Client/UpdatePlayerStatistics", {"Statistics": [{"StatisticName": "score", "Value": 42}]})
    st2, stats = call("/Client/GetPlayerStatistics", {"StatisticNames": ["score"]})
    check("statistics kept", st == 200 and stats.get("Statistics", [{}])[0].get("Value") == 42, stats)
    st, board = call("/Client/GetLeaderboard", {"StatisticName": "score", "StartPosition": 0, "MaxResultsCount": 10})
    check("leaderboard has our score", st == 200 and board.get("Leaderboard", [{}])[0].get("StatValue") == 42, board)
    st, _ = call("/Client/UpdateUserData", {"Data": {"level": "3", "gold": "100"}})
    st2, data = call("/Client/GetUserData", {"Keys": ["level"]})
    check("user data kept", data.get("Data", {}).get("level", {}).get("Value") == "3" and "gold" not in data.get("Data", {}), data)
    saved = os.path.join(os.environ["PEERPARTY_PLAYFAB_DIR"], me["Id"] + ".json")
    check("player data on disk", os.path.isfile(saved) and '"score"' in open(saved).read())
    st, fr = call("/Client/GetFriendsList", {})
    check("B is a friend", st == 200 and len(fr.get("Friends", [])) >= 1, fr)

    gone = wait_for(lambda: len(members()) == 1, 60)
    check("B left", gone)
    st, _ = call("/Lobby/DeleteLobby", {"LobbyId": lobby})
    check("DeleteLobby", st == 200, st)
    st, _ = call("/Lobby/GetLobby", {"LobbyId": lobby})
    check("lobby gone", st == 404, st)
else:
    want = wait_for(lambda: read_file("lobby"), 60)
    found = wait_for(lambda: next((l for l in call("/Lobby/FindLobbies", {"Filter": "string_key1 eq 'pptest'"})[1].get("Lobbies", [])
                                   if l["LobbyId"] == want), None), 30)
    check("FindLobbies with a filter", found is not None, found)
    if not found:
        finish()
    st, joined = call("/Lobby/JoinLobby", {"ConnectionString": found["ConnectionString"], "MemberEntity": me, "MemberData": {"ready": "0"}})
    check("JoinLobby", st == 200 and joined.get("LobbyId") == want, (st, joined))
    st, got = call("/Lobby/GetLobby", {"LobbyId": want})
    check("two members", len(got.get("Lobby", {}).get("Members", [])) == 2, got)
    st, _ = call("/Lobby/UpdateLobby", {"LobbyId": want, "MemberEntity": me, "MemberData": {"ready": "1"}})
    check("UpdateLobby member data", st == 200, st)
    st, _ = call("/Lobby/UpdateLobby", {"LobbyId": want, "LobbyData": {"map": "mine"}})
    check("only the owner changes lobby data", st == 400, st)
    check("A saw it", wait_for(lambda: read_file("a_saw_ready"), 30))
    st, _ = call("/Lobby/LeaveLobby", {"LobbyId": want, "MemberEntity": me})
    check("LeaveLobby", st == 200, st)
    time.sleep(3)
finish()
