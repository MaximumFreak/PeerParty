"""Two copies of a "game" using Unity Gaming Services through PeerParty's stand-in
(hook/src/ugs): run as `run_pair.ps1 ugs_test.py -Env PEERPARTY_UGS=1` (A hosts, B joins).

No Unity SDK here: the calls are the ones com.unity.services.authentication and
com.unity.services.multiplayer make (routes and JSON per Unity's REST references,
services.docs.unity.com player-auth/v1, lobby/v1, relay-allocations/v1), Wire's
Centrifuge JSON over a WebSocket (github.com/centrifugal/protocol), and the Relay UDP
messages of com.unity.transport (Runtime/Relay/RelayMessage*.cs). Python's own DNS and
TLS go through PeerParty's redirect. Certificates are verified the way UnityTls does it:
Python's ssl reads the Windows ROOT store (ssl.enum_certificates, crypt32's
CertOpenStore) and verifies the chain with its own library (OpenSSL; Unity's is
MbedTLS), so this passes only if the hook lists PeerParty's certificate in the ROOT store
this process sees. Set PPTEST_UNVERIFIED=1 to skip that check."""
import base64, http.client, json, os, socket, ssl, struct, sys, time

DIR = os.environ["PPTEST_DIR"]
ROLE = sys.argv[1]
if os.environ.get("PPTEST_UNVERIFIED") == "1":
    CTX = ssl._create_unverified_context()
else:
    CTX = ssl.create_default_context()  # Windows' ROOT store, as this process sees it
    # MbedTLS doesn't apply OpenSSL's extra X509_STRICT rules (newer Pythons set them).
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
    RESULTS.append(bool(ok)); log("PASS" if ok else "FAIL", name, detail)
    return ok

def finish():
    res = "ok" if RESULTS and all(RESULTS) else "failed"
    log("RESULT", res)
    open(os.path.join(DIR, f"{ROLE}.result"), "w").write(res)
    os._exit(0)

TOKEN = None
def call(host, method, path, body=None):
    c = http.client.HTTPSConnection(host, 443, context=CTX, timeout=20)
    h = {"ProjectId": "00000000-0000-0000-0000-000000000001", "Content-Type": "application/json"}
    if TOKEN:
        h["Authorization"] = "Bearer " + TOKEN
    c.request(method, path, None if body is None else json.dumps(body), h)
    r = c.getresponse()
    raw = r.read()
    c.close()
    return r.status, (json.loads(raw) if raw else None)

AUTH, LOBBY = "player-auth.services.api.unity.com", "lobby.services.api.unity.com"
RELAY, SOCIAL, WIRE = "relay-allocations.services.api.unity.com", "social.services.api.unity.com", "wire.unity3d.com"

def wait_file(name, secs=60):
    p = os.path.join(DIR, name)
    end = time.time() + secs
    while time.time() < end:
        if os.path.exists(p):
            time.sleep(0.1)
            return open(p).read()
        time.sleep(0.2)
    return None

def put_file(name, text):
    p = os.path.join(DIR, name)
    open(p + ".tmp", "w").write(text)
    os.replace(p + ".tmp", p)

# ---- Wire: a minimal WebSocket client (RFC 6455) speaking Centrifuge JSON.
class Wire:
    def __init__(self):
        raw = socket.create_connection((WIRE, 443), timeout=20)
        self.s = CTX.wrap_socket(raw, server_hostname=WIRE)
        key = base64.b64encode(os.urandom(16)).decode()
        self.s.sendall((f"GET /v2/ws HTTP/1.1\r\nHost: {WIRE}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        head = b""
        while b"\r\n\r\n" not in head:
            head += self.s.recv(1)
        self.status = head.split(b"\r\n")[0].decode()
        self.buf = b""
        self.pushes = []
        self.next_id = 1

    def send(self, obj):
        data = json.dumps(obj).encode()
        mask = os.urandom(4)
        n = len(data)
        hdr = bytes([0x81]) + (bytes([0x80 | n]) if n < 126 else bytes([0x80 | 126]) + struct.pack(">H", n))
        self.s.sendall(hdr + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

    def _frame(self, timeout):
        self.s.settimeout(timeout)
        def need(k):
            while len(self.buf) < k:
                chunk = self.s.recv(65536)
                if not chunk:
                    raise EOFError
                self.buf += chunk
        need(2)
        op, n = self.buf[0] & 0x0F, self.buf[1] & 0x7F
        at = 2
        if n == 126:
            need(4); n = struct.unpack(">H", self.buf[2:4])[0]; at = 4
        elif n == 127:
            need(10); n = struct.unpack(">Q", self.buf[2:10])[0]; at = 10
        need(at + n)
        data, self.buf = self.buf[at:at + n], self.buf[at + n:]
        return op, data

    def command(self, kind, body):
        cid = self.next_id; self.next_id += 1
        self.send({"id": cid, kind: body})
        end = time.time() + 10
        while time.time() < end:
            for msg in self.read(end - time.time()):
                if msg.get("id") == cid:
                    return msg
        return None

    def read(self, timeout):
        """Messages in one frame (Centrifuge batches them one JSON per line)."""
        try:
            op, data = self._frame(max(timeout, 0.05))
        except (socket.timeout, TimeoutError, ssl.SSLError):
            return []
        if op != 1:
            return []
        out = []
        for line in data.decode().split("\n"):
            if line.strip():
                m = json.loads(line)
                if "push" in m:
                    self.pushes.append(m["push"])
                out.append(m)
        return out

    def wait_patch(self, pred, secs=30):
        end = time.time() + secs
        while time.time() < end:
            for p in list(self.pushes):
                pub = p.get("pub")
                if pub:
                    patches = json.loads(pub["data"]["payload"])
                    if pred(patches):
                        return patches
                elif "unsubscribe" in p and pred(None):
                    return p
            self.pushes.clear()
            self.read(min(1.0, end - time.time()))
        return None

# ---- Relay: com.unity.transport's messages.
SIG = struct.pack("<H", 29402)
def hdr(kind): return SIG + bytes([0, kind])
def bind_msg(data, key):
    # RelayServerData.ComputeBindHMAC: HMAC-SHA256 with the allocation's key over the first 263 bytes.
    import hashlib, hmac
    m = hdr(0) + bytes([0]) + struct.pack("<H", 0) + bytes([255]) + data
    return m + hmac.new(key, m, hashlib.sha256).digest()
def connect_msg(me, host_data): return hdr(3) + me + bytes([255]) + host_data
def relay_msg(frm, to, payload): return hdr(10) + frm + to + struct.pack(">H", len(payload)) + payload
def disconnect_msg(frm, to): return hdr(9) + frm + to

def udp_wait(sock, kind, secs=10):
    end = time.time() + secs
    while time.time() < end:
        sock.settimeout(max(0.05, end - time.time()))
        try:
            pkt, _ = sock.recvfrom(2048)
        except (socket.timeout, TimeoutError, ConnectionResetError):
            continue
        if pkt[:3] == SIG + b"\0" and pkt[3] == kind:
            return pkt
        log("udp other", pkt[:4].hex(), len(pkt))
    return None

def endpoint(alloc):
    ep = next(e for e in alloc["serverEndpoints"] if e["connectionType"] == "udp")
    return ep["host"], ep["port"]

# ---- Both: sign in.
st, si = call(AUTH, "POST", "/v1/authentication/anonymous", {})
check("anonymous sign-in 200", st == 200, st)
TOKEN = si["idToken"]
PID = si["userId"]
claims = json.loads(base64.urlsafe_b64decode(TOKEN.split(".")[1] + "=="))
check("token sub is the player id", claims.get("sub") == PID and len(PID) == 28, (PID, claims.get("sub")))
check("token audience names the environment", any(a.startswith("envId:") for a in claims.get("aud", [])), claims.get("aud"))
st, nm = call(SOCIAL, "GET", f"/v1/names/{PID}")
check("player name", st == 200 and "#" in nm.get("name", ""), nm)

if ROLE == "A":
    st, lob = call(LOBBY, "POST", "/v1/create", {
        "name": "peerparty ugs test", "maxPlayers": 4, "isPrivate": False,
        "data": {"mode": {"value": "coop", "visibility": "public", "index": "S1"},
                 "secret": {"value": "host-only", "visibility": "private"}},
        "player": {"id": PID, "data": {"ready": {"value": "yes", "visibility": "member"}}}})
    check("create 201", st == 201, (st, lob))
    LID, CODE = lob["id"], lob["lobbyCode"]
    check("lobby code and host", len(CODE) == 6 and lob["hostId"] == PID and len(lob["players"]) == 1, (CODE, lob.get("hostId")))
    check("host sees its private data", lob["data"].get("secret", {}).get("value") == "host-only", lob["data"])
    st, toks = call(LOBBY, "POST", f"/v1/{LID}/tokens", [{"tokenType": "wireJoin"}])
    check("wire token", st == 200 and toks["wireJoin"]["uri"], (st, toks))
    w = Wire()
    check("wire websocket upgrade", " 101 " in w.status + " ", w.status)
    r = w.command("connect", {"token": toks["wireJoin"]["tokenValue"], "subs": {}})
    check("wire connect reply", r and "connect" in r, r)
    r = w.command("subscribe", {"channel": toks["wireJoin"]["uri"], "token": toks["wireJoin"]["tokenValue"]})
    check("wire subscribe reply", r and "subscribe" in r and "error" not in r, r)

    st, al = call(RELAY, "POST", "/v1/allocate", {"maxConnections": 3})
    check("relay allocate 201", st == 201, (st, al))
    alloc = al["data"]["allocation"]
    st, jc = call(RELAY, "POST", "/v1/joincode", {"allocationId": alloc["allocationId"]})
    check("relay join code", st in (200, 201) and len(jc["data"]["joinCode"]) == 6, (st, jc))
    rcode = jc["data"]["joinCode"]
    st, up = call(LOBBY, "POST", f"/v1/{LID}", {"data": {"relay": {"value": rcode, "visibility": "member"}}})
    check("lobby update with the relay code", st == 200 and up["data"]["relay"]["value"] == rcode, st)

    my_id = base64.b64decode(alloc["allocationIdBytes"])
    u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    u.bind(("0.0.0.0", 0))
    rhost = endpoint(alloc)
    u.sendto(bind_msg(base64.b64decode(alloc["connectionData"]), base64.b64decode(alloc["key"])), rhost)
    check("host bound to the relay", udp_wait(u, 1) is not None, rhost)
    put_file("code", CODE)

    def patches(ps):
        return (ps.get("Patches") or ps.get("patches") or []) if isinstance(ps, dict) else []
    p = w.wait_patch(lambda ps: any(x.get("op") == "add" and x.get("path", "").startswith("/players/") for x in patches(ps)), 60)
    check("wire: player joined patch", p is not None, json.dumps(p)[:300] if p else None)

    # B's relay data, echoed back.
    pkt = udp_wait(u, 10, 40)
    ok = pkt is not None and pkt[36:38] == struct.pack(">H", len(pkt) - 38)
    check("host got relayed data", ok and pkt[38:] == b"hello host", pkt[38:] if pkt else None)
    if ok:
        u.sendto(relay_msg(my_id, pkt[4:20], b"hello client"), rhost)
    check("host got the client's disconnect", udp_wait(u, 9, 30) is not None)

    p = w.wait_patch(lambda ps: any(x.get("op") == "remove" and x.get("path", "").startswith("/players/") for x in patches(ps)), 40)
    check("wire: player left patch", p is not None, json.dumps(p)[:300] if p else None)
    st, g = call(LOBBY, "GET", f"/v1/{LID}")
    check("lobby back to one player", st == 200 and len(g["players"]) == 1, (st, g and len(g.get("players", []))))
    st, _ = call(LOBBY, "DELETE", f"/v1/{LID}")
    check("delete 204", st == 204, st)
    put_file("a-done", "1")
    finish()
else:
    code = wait_file("code", 60)
    check("host's code arrived", code is not None)
    if code is None:
        finish()
    found = None
    for _ in range(20):
        st, q = call(LOBBY, "POST", "/v1/query", {"count": 10, "filter": [{"field": "S1", "op": "EQ", "value": "coop"}]})
        # Lobby codes are for members only; the listing finds it by its name.
        found = [l for l in q.get("results", []) if l.get("name") == "peerparty ugs test"] if st == 200 else []
        if found:
            break
        time.sleep(0.5)
    check("query finds the host's lobby by S1", bool(found), (st, q))
    if found:
        check("listing has no lobby code", "lobbyCode" not in found[0], found[0].get("lobbyCode"))
        check("query hides private data", "secret" not in found[0].get("data", {}) and "relay" not in found[0].get("data", {}), found[0].get("data"))
    st, bad = call(LOBBY, "POST", "/v1/joinbycode", {"lobbyCode": "ZZZZZZ"})
    check("bad code is 404", st == 404 and isinstance(bad, dict), (st, bad))
    st, lob = call(LOBBY, "POST", "/v1/joinbycode", {"lobbyCode": code, "player": {"id": PID}})
    check("join by code 200", st == 200 and len(lob["players"]) == 2, (st, lob and len(lob.get("players", []))))
    LID = lob["id"]
    rcode = lob.get("data", {}).get("relay", {}).get("value")
    check("member sees member data (relay code)", bool(rcode), lob.get("data"))
    check("member doesn't see private data", "secret" not in lob.get("data", {}))

    st, jn = call(RELAY, "POST", "/v1/join", {"joinCode": rcode})
    check("relay join 200", st == 200 and jn["data"]["allocation"].get("hostConnectionData"), (st, jn))
    alloc = jn["data"]["allocation"]
    my_id = base64.b64decode(alloc["allocationIdBytes"])
    host_data = base64.b64decode(alloc["hostConnectionData"])
    host_id = host_data[6:22]
    u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    u.bind(("0.0.0.0", 0))
    rhost = endpoint(alloc)
    u.sendto(bind_msg(base64.b64decode(alloc["connectionData"]), base64.b64decode(alloc["key"])), rhost)
    check("client bound to the host's relay", udp_wait(u, 1) is not None, rhost)
    u.sendto(connect_msg(my_id, host_data), rhost)
    acc = udp_wait(u, 6)
    check("relay accepted the connection", acc is not None and acc[4:20] == host_id and acc[20:36] == my_id)
    u.sendto(relay_msg(my_id, host_id, b"hello host"), rhost)
    back = udp_wait(u, 10, 20)
    check("client got the host's reply", back is not None and back[38:] == b"hello client", back[38:] if back else None)
    u.sendto(disconnect_msg(my_id, host_id), rhost)
    time.sleep(1)
    st, _ = call(LOBBY, "DELETE", f"/v1/{LID}/players/{PID}")
    check("leave 204", st == 204, st)
    st, j = call(LOBBY, "GET", "/v1/joined")
    check("no longer joined", st == 200 and LID not in (j if isinstance(j, list) else j.get("data", j)), (st, j))
    wait_file("a-done", 60)
    finish()
