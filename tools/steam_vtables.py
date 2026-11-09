"""Generates hook/src/steamvt.rs: the vtable layout of every version of the Steam
interfaces peerparty wraps, from a folder of the Steamworks SDK's interface headers in
every historical version, one file per version (e.g. isteammatchmaking008.h).

    python tools/steam_vtables.py <headers folder> > hook/src/steamvt.rs

Each version lists its methods in vtable order. MSVC lays out a class's virtuals in
declaration order, except that overloads are grouped at the first one's place in
reverse declaration order. A method whose parameters differ from the newest version's
gets a `~<argument count>` suffix ("SendMessages~3"): peerparty looks methods up by
name and implements the newest signature, so it leaves that slot alone unless it has
an implementation for that older shape too (on x86 the argument count decides how
much stack the method pops).
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
}
DEFINED = {"STEAM_WIN32", "_WIN32", "WIN32"}


def preprocess(text):
    text = re.sub(r"/\*.*?\*/", " ", text, flags=re.S)
    text = re.sub(r"//[^\n]*", "", text)
    out, stack = [], []
    for line in text.split("\n"):
        s = line.strip()
        m = re.match(r"#\s*(ifdef|ifndef|if|elif|else|endif)\b\s*(.*)", s)
        if m:
            kind, arg = m.group(1), m.group(2).strip()
            if kind == "ifdef":
                stack.append([arg in DEFINED, arg in DEFINED])
            elif kind == "ifndef":
                stack.append([arg not in DEFINED, arg not in DEFINED])
            elif kind == "if":
                # Only "#if 0"-style and platform checks appear in these headers.
                stack.append([False, False])
            elif kind == "elif":
                stack[-1][0] = False
            elif kind == "else":
                stack[-1][0] = not stack[-1][1]
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
            found.append((name, types))
        else:
            found.append((m.group(4), norm(m.group(5))))
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
    print("//! Generated by tools/steam_vtables.py from the Steamworks SDK interface headers, every")
    print("//! historical version. Don't edit.")
    print("//!")
    print("//! Each Steam interface version's methods in vtable order. `~N` marks a slot whose")
    print("//! signature differs from the newest version's (N arguments).")
    print()
    print("pub const TABLES: &[(&str, &[&str])] = &[")
    for version, names in tables:
        print('    (\n        "%s",\n        &[' % version)
        for n in names:
            print('            "%s",' % n)
        print("        ],\n    ),")
    print("];")


if __name__ == "__main__":
    main(sys.argv[1])
