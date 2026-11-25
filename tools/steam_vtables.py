"""Generates hook/src/steam/vt.rs: the vtable layout of every version of the Steam
interfaces peerparty wraps, from a folder of the Steamworks SDK's interface headers in
every historical version, one file per version (e.g. isteammatchmaking008.h).

    python tools/steam_vtables.py <headers folder> > hook/src/steam/vt.rs

Each version lists its methods in vtable order. MSVC lays out a class's virtuals in
declaration order, except that overloads are grouped at the first one's place in
reverse declaration order. A method whose parameters differ from the newest version's
gets a `~<argument count>` suffix ("SendMessages~3"): peerparty looks methods up by
name and implements the newest signature, so it leaves that slot alone unless it has
an implementation for that older shape too (on x86 the argument count decides how
much stack the method pops).

Valve sometimes added methods at the end of an interface without a new version string
(SteamUtils005 has 19, 20 and 23 methods across SDKs 1.07 to 1.23a). Steam's client
implements each version once, in its longest form, so where a version appears more than
once the longest shape is kept (each shorter one must be a prefix of it).

The SteamUtils, SteamUserStats, SteamApps, SteamUGC, SteamController, SteamInput and
SteamRemoteStorage tables were generated from Valve's SDK headers as Valve publishes them in its Proton
repository (lsteamclient/steamworks_sdk_*/isteam*.h, one copy per SDK release, put in
one folder with the SDK's name in front of each file's). That set starts at SDK 0.99,
so it lacks some older versions the other families' tables have: generate only the
families it adds, and keep the rest.
"""

import os
import re
import sys

FAMILIES = {
    # class prefix: (version string prefix, newest version's #define)
    "ISteamClient": ("SteamClient", "STEAMCLIENT_INTERFACE_VERSION"),
    "ISteamUser": ("SteamUser", "STEAMUSER_INTERFACE_VERSION"),
    "ISteamFriends": ("SteamFriends", "STEAMFRIENDS_INTERFACE_VERSION"),
    "ISteamMatchmaking": ("SteamMatchMaking", "STEAMMATCHMAKING_INTERFACE_VERSION"),
    "ISteamNetworking": ("SteamNetworking", "STEAMNETWORKING_INTERFACE_VERSION"),
    "ISteamNetworkingSockets": ("SteamNetworkingSockets", "STEAMNETWORKINGSOCKETS_INTERFACE_VERSION"),
    "ISteamNetworkingMessages": ("SteamNetworkingMessages", "STEAMNETWORKINGMESSAGES_INTERFACE_VERSION"),
    "ISteamNetworkingUtils": ("SteamNetworkingUtils", "STEAMNETWORKINGUTILS_INTERFACE_VERSION"),
    "ISteamGameServer": ("SteamGameServer", "STEAMGAMESERVER_INTERFACE_VERSION"),
    "ISteamMatchmakingServers": ("SteamMatchMakingServers", "STEAMMATCHMAKINGSERVERS_INTERFACE_VERSION"),
    "ISteamUtils": ("SteamUtils", "STEAMUTILS_INTERFACE_VERSION"),
    "ISteamUserStats": ("STEAMUSERSTATS_INTERFACE_VERSION", "STEAMUSERSTATS_INTERFACE_VERSION"),
    "ISteamApps": ("STEAMAPPS_INTERFACE_VERSION", "STEAMAPPS_INTERFACE_VERSION"),
    "ISteamUGC": ("STEAMUGC_INTERFACE_VERSION", "STEAMUGC_INTERFACE_VERSION"),
    "ISteamController": ("SteamController", "STEAMCONTROLLER_INTERFACE_VERSION"),
    "ISteamInput": ("SteamInput", "STEAMINPUT_INTERFACE_VERSION"),
    "ISteamRemoteStorage": ("STEAMREMOTESTORAGE_INTERFACE_VERSION", "STEAMREMOTESTORAGE_INTERFACE_VERSION"),
}
# The Steamworks SDK defines STEAMNETWORKINGSOCKETS_ENABLE_SDR (steamnetworkingtypes.h):
# the relay and ping methods are in Steam's networking interfaces.
DEFINED = {"STEAM_WIN32", "_WIN32", "WIN32", "STEAMNETWORKINGSOCKETS_ENABLE_SDR"}


def condition(expr):
    """An #if / #elif expression, as the C preprocessor reads it: defined(X) and
    defined X against DEFINED, numbers, !, &&, ||, comparisons and parentheses; any
    other name is 0."""
    e = re.sub(r"\bdefined\s*\(\s*(\w+)\s*\)|\bdefined\s+(\w+)", lambda m: "1" if (m.group(1) or m.group(2)) in DEFINED else "0", expr)
    e = e.replace("&&", " and ").replace("||", " or ")
    e = re.sub(r"!(?!=)", " not ", e)
    e = re.sub(r"\b[A-Za-z_]\w*\b", lambda m: m.group(0) if m.group(0) in ("and", "or", "not") else "0", e)
    e = re.sub(r"\b(\d+)[uUlL]+\b", r"\1", e)
    if not re.fullmatch(r"[\d\s()<>=!andortn]*", e):
        return False
    try:
        return bool(eval(e, {"__builtins__": {}}, {}))
    except Exception:
        return False


def preprocess(text):
    text = re.sub(r"/\*.*?\*/", " ", text, flags=re.S)
    text = re.sub(r"//[^\n]*", "", text)
    # Each open #if: [this branch on, some branch of it already taken].
    out, stack = [], []
    for line in text.split("\n"):
        s = line.strip()
        m = re.match(r"#\s*(ifdef|ifndef|if|elif|else|endif)\b\s*(.*)", s)
        if m:
            kind, arg = m.group(1), m.group(2).strip()
            if kind == "ifdef":
                on = arg in DEFINED
                stack.append([on, on])
            elif kind == "ifndef":
                on = arg not in DEFINED
                stack.append([on, on])
            elif kind == "if":
                on = condition(arg)
                stack.append([on, on])
            elif kind == "elif":
                on = not stack[-1][1] and condition(arg)
                stack[-1] = [on, stack[-1][1] or on]
            elif kind == "else":
                stack[-1] = [not stack[-1][1], True]
            elif kind == "endif":
                stack.pop()
            continue
        if all(on for on, _ in stack):
            out.append(line)
    return "\n".join(out)


def class_body(text, name):
    m = re.search(r"\bclass\s+%s\b[^;{]*\{" % re.escape(name), text)
    if not m:
        return None
    depth, i = 1, m.end()
    while depth:
        c = text[i]
        depth += c == "{"
        depth -= c == "}"
        i += 1
    return text[m.end() : i - 1]


def norm(params):
    params = re.sub(r"\bSTEAM_\w+\s*\([^()]*\)", "", params)
    parts = [p.strip() for p in params.split(",") if p.strip() and p.strip() != "void"]
    types = []
    for p in parts:
        p = re.sub(r"=.*", "", p).strip()
        p = re.sub(r"\s*\b\w+$", "", p) if re.search(r"[\w\*&\]]\s+\w+$", p) else p
        types.append(re.sub(r"\s+", " ", p.replace("*", " * ").replace("&", " & ")).strip())
    return tuple(types)


class Sig(tuple):
    """A method's signature: (parameter types, return type, const). len() is the
    parameter count (what `~N` names say)."""

    def __new__(cls, params, ret, const):
        t = super().__new__(cls, (tuple(params), ret, const))
        return t

    def __len__(self):
        return len(self[0])

    def __eq__(self, other):
        return tuple.__eq__(self, other)

    def __hash__(self):
        return tuple.__hash__(self)


def methods(body):
    # Strip nested braces (inline helpers) so only declarations remain.
    flat, depth = [], 0
    for c in body:
        if c == "{":
            depth += 1
            continue
        if c == "}":
            depth -= 1
            flat.append(";")
            continue
        if depth == 0:
            flat.append(c)
    flat = "".join(flat)
    found = []
    pat = re.compile(
        r"STEAMWORKS_STRUCT_RETURN_(\d)\s*\(([^)]*)\)"
        r"|virtual\s+([^;()]*?)\b(\w+)\s*\(([^()]*(?:\([^()]*\)[^()]*)*)\)\s*(?:const)?\s*=\s*0\s*;",
        re.S,
    )
    for m in pat.finditer(flat):
        if m.group(1):
            args = [a.strip() for a in m.group(2).split(",")]
            name = args[1]
            types = tuple(args[2::2])
            found.append((name, Sig(types, args[0], False)))
        else:
            # The signature: the parameters' types, the return type and constness
            # (a method whose return type changed is another method in that slot).
            ret = re.sub(r"\s+", " ", m.group(3).replace("*", " * ").replace("&", " & ")).strip()
            const = bool(re.search(r"\)\s*const\s*=\s*0\s*;$", m.group(0)))
            found.append((m.group(4), Sig(norm(m.group(5)), ret, const)))
    # MSVC: overloads grouped at the first one's slot, in reverse declaration order.
    order, seen = [], {}
    for i, (name, sig) in enumerate(found):
        seen.setdefault(name, []).append((name, sig))
    placed = set()
    for name, _ in found:
        if name in placed:
            continue
        placed.add(name)
        order.extend(reversed(seen[name]))
    return order


def main(sdk):
    tables = []
    for prefix, (vprefix, newest_define) in FAMILIES.items():
        versions = []
        for f in sorted(os.listdir(sdk)):
            path = os.path.join(sdk, f)
            text = preprocess(open(path, encoding="utf-8", errors="replace").read())
            for cls in re.findall(r"\bclass\s+(%s(\d{3})?)\s*(?::[^{]*)?\{" % prefix, text):
                name, num = cls
                body = class_body(text, name)
                if body is None:
                    continue
                if num:
                    version = vprefix + num
                else:
                    d = re.search(r'#define\s+%s\s+"(\w+)"' % newest_define, text)
                    if not d:
                        continue
                    version = d.group(1)
                versions.append((version, methods(body)))
        versions.sort(key=lambda v: v[0])
        if not versions:
            continue
        # One shape per version: the longest, which every other must start like.
        longest = {}
        for version, ms in versions:
            kept = longest.get(version)
            if kept is None or len(ms) > len(kept):
                short, longer = (kept or []), ms
                longest[version] = ms
            else:
                short, longer = ms, kept
            if [n for n, _ in short] != [n for n, _ in longer[: len(short)]]:
                sys.stderr.write("%s: shapes differ, not just added methods\n" % version)
        versions = sorted(longest.items())
        newest = dict(versions[-1][1])
        for version, ms in versions:
            if any(v == version for v, _ in tables):
                continue
            names = []
            overloaded = {n for n, _ in ms if sum(1 for m, _ in ms if m == n) > 1}
            for n, sig in ms:
                if n in overloaded or newest.get(n) != sig:
                    names.append("%s~%d" % (n, len(sig)))
                else:
                    names.append(n)
            tables.append((version, names))
    emit(tables)


def emit(tables):
    """Prints vt.rs: each name once in NAMES, each version's slots as indices into it
    (a table of `&str`s costs 16 bytes a slot on x64 and a relocation each on x86)."""
    pool, index = [], {}
    for _, names in tables:
        for n in names:
            if n not in index:
                index[n] = len(pool)
                pool.append(n)
    assert len(pool) < 1 << 16
    print("//! Generated by tools/steam_vtables.py from the Steamworks SDK interface headers, every")
    print("//! historical version. Don't edit.")
    print("//!")
    print("//! Each Steam interface version's methods in vtable order, as indices into NAMES.")
    print("//! `~N` marks a slot whose signature differs from the newest version's (N arguments).")
    print()
    print("pub const NAMES: &[&str] = &[")
    for n in pool:
        print('    "%s",' % n)
    print("];")
    print()
    print("pub const TABLES: &[(&str, &[u16])] = &[")
    for version, names in tables:
        print('    ("%s", &[%s]),' % (version, ", ".join(str(index[n]) for n in names)))
    print("];")

if __name__ == "__main__":
    main(sys.argv[1])
