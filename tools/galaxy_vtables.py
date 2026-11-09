"""Generates hook/src/galaxyvt.rs: the vtable layout of the GOG Galaxy interfaces
peerparty wraps, for every Galaxy SDK version, from the SDK's headers (one
folder per SDK version, <version>/galaxy/*.h).

    python tools/galaxy_vtables.py <a folder with one subfolder of headers per SDK version> > hook/src/galaxyvt.rs

Each class lists its virtual methods in vtable order, its virtual destructor ("~")
first. MSVC lays out virtuals in declaration order, except that overloads are grouped at
the first one's place in reverse declaration order. A method whose parameters differ
from the newest version's gets a `~<argument count>` suffix, plus `@<index>` when one
of them is a listener: peerparty implements the newest signatures, and the older shapes
it knows by that suffix (old SDKs had no per-call listeners; mid ones overloaded
`SignIn`), and leaves any other such slot to the real Galaxy.
The ListenerType enum is listed per version too, since listeners are registered by it.
"""

import os
import re
import sys

CLASSES = {
    "IListenerRegistrar": "IListenerRegistrar.h",
    "IUser": "IUser.h",
    "IFriends": "IFriends.h",
    "IMatchmaking": "IMatchmaking.h",
    "INetworking": "INetworking.h",
    "IStats": "IStats.h",
    "IUtils": "IUtils.h",
    "IApps": "IApps.h",
    "IStorage": "IStorage.h",
    "ICloudStorage": "ICloudStorage.h",
    "ITelemetry": "ITelemetry.h",
    "IChat": "IChat.h",
    "ICustomNetworking": "ICustomNetworking.h",
}

# Classes added after the first five: overloads whose suffixed names would still be the
# same (ICloudStorage's two GetFile) get "#<n>" in vtable order. The first five keep the
# names PeerParty's old-shape table matches.
NUMBERED = set(list(CLASSES)[5:])


def strip_comments(text):
    text = re.sub(r"/\*.*?\*/", " ", text, flags=re.S)
    return re.sub(r"//[^\n]*", "", text)


def class_body(text, name):
    m = re.search(r"\bclass\s+(?:GALAXY_DLL_EXPORT\s+)?%s\b[^;{]*\{" % re.escape(name), text)
    if not m:
        return None
    depth, i = 1, m.end()
    while depth:
        c = text[i]
        depth += c == "{"
        depth -= c == "}"
        i += 1
    return text[m.end() : i - 1]


def listener_at(params):
    """The index of the first listener parameter, or None."""
    parts = [p for p in re.split(r",(?![^()]*\))", params) if p.strip() and p.strip() != "void"]
    for i, p in enumerate(parts):
        if "Listener" in p:
            return i
    return None


def norm(params):
    parts, depth, cur = [], 0, ""
    for c in params:
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
        if c == "," and depth == 0:
            parts.append(cur)
            cur = ""
        else:
            cur += c
    parts.append(cur)
    types = []
    for p in parts:
        p = re.sub(r"=.*", "", p, flags=re.S).strip()
        if not p or p == "void":
            continue
        p = re.sub(r"\s*\b\w+$", "", p) if re.search(r"[\w\*&\]]\s+\w+$", p) else p
        types.append(re.sub(r"\s+", " ", p.replace("*", " * ").replace("&", " & ")).strip())
    return tuple(types)


def methods(body):
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
    for m in re.finditer(r"virtual\s+([^;()]*?)(~?\w+)\s*\(((?:[^()]|\([^()]*\))*)\)\s*(?:const)?\s*(?:=\s*0)?\s*;", flat, re.S):
        name = m.group(2)
        found.append(("~" if name.startswith("~") else name, norm(m.group(3)), listener_at(m.group(3))))
    order, seen, placed = [], {}, set()
    for name, sig, at in found:
        seen.setdefault(name, []).append((name, sig, at))
    for name, _, _ in found:
        if name in placed:
            continue
        placed.add(name)
        order.extend(reversed(seen[name]))
    return order


def listener_types(text):
    """ListenerType names in value order (LISTENER_TYPE_BEGIN/END left out)."""
    m = re.search(r"enum\s+ListenerType\s*\{(.*?)\}", text, re.S)
    if not m:
        return []
    values, value = {}, -1
    for item in m.group(1).split(","):
        item = item.strip()
        if not item:
            continue
        name, _, expr = (x.strip() for x in item.partition("="))
        if expr:
            value = int(expr, 0) if re.fullmatch(r"-?\w+", expr) and expr[0].isdigit() else values.get(expr, value + 1)
        else:
            value += 1
        values[name] = value
    named = {n: v for n, v in values.items() if n not in ("LISTENER_TYPE_BEGIN", "LISTENER_TYPE_END")}
    out = [None] * (max(named.values()) + 1 if named else 0)
    for n, v in named.items():
        if out[v] is None:
            out[v] = n
    return [n or "" for n in out]


def vkey(v):
    return [int(x) for x in v.split(".")]


def main(root):
    versions = sorted(os.listdir(root), key=vkey)
    raw = {}
    enums = {}
    for v in versions:
        raw[v] = {}
        for cls, header in CLASSES.items():
            path = os.path.join(root, v, header)
            if not os.path.exists(path):
                path = os.path.join(root, v, "galaxy", header)
            if not os.path.exists(path):
                continue
            text = strip_comments(open(path, encoding="utf-8", errors="replace").read())
            body = class_body(text, cls)
            if body is not None:
                raw[v][cls] = methods(body)
            if cls == "IListenerRegistrar":
                enums[v] = listener_types(text)
    newest = versions[-1]
    layouts = {cls: [] for cls in CLASSES}
    table = []
    for v in versions:
        row = []
        for cls in CLASSES:
            ms = raw[v].get(cls)
            if ms is None:
                row.append(None)
                continue
            ref = {n: sig for n, sig, _ in raw[newest][cls]}
            overloaded = {n for n, _, _ in ms if sum(1 for m, _, _ in ms if m == n) > 1}
            names = tuple(
                n
                if n == "~" or (n not in overloaded and ref.get(n) == sig)
                else "%s~%d%s" % (n, len(sig), "" if at is None else "@%d" % at)
                for n, sig, at in ms
            )
            if cls in NUMBERED:
                seen, out = {}, []
                for n in names:
                    if names.count(n) > 1:
                        seen[n] = seen.get(n, 0) + 1
                        n = "%s#%d" % (n, seen[n])
                    out.append(n)
                names = tuple(out)
            if names not in layouts[cls]:
                layouts[cls].append(names)
            row.append(layouts[cls].index(names))
        table.append((v, row))
    enum_layouts = []
    enum_row = {}
    for v in versions:
        e = tuple(enums.get(v, []))
        if e not in enum_layouts:
            enum_layouts.append(e)
        enum_row[v] = enum_layouts.index(e)
    p = print
    p("//! Generated by tools/galaxy_vtables.py from the Galaxy SDK headers")
    p("//! (one folder per SDK version). Don't edit.")
    p("//!")
    p("//! Each interface's methods in vtable order, the virtual destructor (\"~\") first. `~N`")
    p("//! marks a slot whose signature differs from the newest version's (N arguments).")
    p()
    p("pub const CLASSES: &[&str] = &[%s];" % ", ".join('"%s"' % c for c in CLASSES))
    p()
    for cls in CLASSES:
        p("pub const %s: &[&[&str]] = &[" % cls.upper())
        for names in layouts[cls]:
            p("    &[%s]," % ", ".join('"%s"' % n for n in names))
        p("];")
        p()
    p("/// ListenerType values in order, per distinct enum.")
    p("pub const LISTENER_TYPES: &[&[&str]] = &[")
    for e in enum_layouts:
        p("    &[%s]," % ", ".join('"%s"' % n for n in e))
    p("];")
    p()
    p("/// (SDK version, layout index per class in CLASSES order (-1: not in that SDK), ListenerType enum index).")
    p("pub const VERSIONS: &[((u32, u32, u32), [i8; %d], u8)] = &[" % len(CLASSES))
    for v, row in table:
        a, b, c = (vkey(v) + [0, 0])[:3]
        p("    ((%d, %d, %d), [%s], %d)," % (a, b, c, ", ".join(str(-1 if x is None else x) for x in row), enum_row[v]))
    p("];")


if __name__ == "__main__":
    main(sys.argv[1])
