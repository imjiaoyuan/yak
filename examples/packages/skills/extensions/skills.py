#!/usr/bin/env python3
"""skills — on-demand skill loading as a yak package.

The built-in discovery stays in core (the system prompt is built once per
session and needs the skill list at that moment), but the full file has
always been read with the read tool by convention. This package makes the
surface explicit and searchable: `discover_skills` lists what is installed
(name, description, path) and `read_skill` returns one skill's full
instruction body, resolving the name case-insensitively against the same
discovery order core uses. Both are read-tier.

Install: yak install git:github.com/imjiaoyuan/yak-skills
(or a path: yak install file:///path/to/examples/packages/skills)

Protocol: newline-delimited JSON on stdio (docs/extensions.md).
"""

import json
import os
import re
import sys

from pathlib import Path


def reply(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


FRONT = re.compile(r"\A---\s*\n(.*?)\n---\s*\n?", re.S)


def parse_skill_md(text, fallback, path):
    m = FRONT.match(text)
    if not m:
        return None
    meta = {}
    key = None
    for line in m.group(1).splitlines():
        # the flat subset core parses: `key: value` and `|`/`>` block
        # scalars; nested yaml is nobody's skill file in practice
        kv = re.match(r"(\w[\w-]*):\s*(.*)", line)
        if kv:
            key = kv.group(1)
            meta[key] = kv.group(2)
        elif key and line.startswith((" ", "\t")) and isinstance(meta.get(key), str):
            meta[key] += " " + line.strip()
    description = meta.get("description", "").strip()
    if not description:
        return None
    name = meta.get("name", "").strip() or fallback
    hidden = meta.get("disable-model-invocation", "").strip().lower() == "true"
    return {"name": name, "description": description, "path": str(path),
            "model_invocation": not hidden}


def load_dir(d):
    out = []
    try:
        entries = sorted(Path(d).iterdir())
    except OSError:
        return out
    for p in entries:
        if p.is_dir():
            f = p / "SKILL.md"
            if f.is_file():
                try:
                    defn = parse_skill_md(f.read_text(encoding="utf-8", errors="replace"),
                                          p.name, f)
                except OSError:
                    defn = None
                if defn:
                    out.append(defn)
        elif p.suffix == ".md":
            try:
                defn = parse_skill_md(p.read_text(encoding="utf-8", errors="replace"),
                                      p.stem, p)
            except OSError:
                defn = None
            if defn:
                out.append(defn)
    return out


def package_skills():
    out = []
    for root in [Path.home() / ".yak" / "pkg", Path.cwd() / ".yak" / "pkg"]:
        try:
            pkgs = sorted(root.iterdir())
        except OSError:
            continue
        for pkg in pkgs:
            if not pkg.is_dir():
                continue
            f = pkg / "SKILL.md"
            if f.is_file():
                try:
                    defn = parse_skill_md(f.read_text(encoding="utf-8", errors="replace"),
                                          pkg.name, f)
                except OSError:
                    defn = None
                if defn:
                    out.append(defn)
            out.extend(load_dir(pkg / "skills"))
    return out


def discover():
    # lowest priority first, so a later (project) copy shadows the earlier
    # (user) one by name — the same merge order core discovery uses
    defs = package_skills()
    defs += load_dir(Path.home() / ".agents" / "skills")
    defs += load_dir(Path.home() / ".yak" / "skills")
    defs += load_dir(Path.cwd() / ".agents" / "skills")
    defs += load_dir(Path.cwd() / ".yak" / "skills")
    merged = {}
    for defn in defs:
        merged[defn["name"]] = defn
    return list(merged.values())


def find(name):
    want = name.strip().lower()
    for defn in discover():
        if defn["name"].lower() == want:
            return defn
    return None


def skill_root(defn):
    # a skill references relative paths against its directory: the parent
    # of SKILL.md, or dirname of a flat file
    return str(Path(defn["path"]).parent)


def discover_skills(args):
    defs = discover()
    if not defs:
        return {"result": "No skills installed."}
    query = (args.get("query") or "").strip().lower()
    lines = []
    for defn in defs:
        if query and query not in defn["name"].lower() \
                and query not in defn["description"].lower():
            continue
        lines.append(f"{defn['name']}: {defn['description']}\n  {defn['path']}")
    if not lines:
        return {"result": f"No skill matches {query!r}."}
    return {"result": "\n".join(lines) + "\n"}


def read_skill(args):
    name = args.get("name") or ""
    if not name.strip():
        return {"error": "name is required"}
    defn = find(name)
    if defn is None:
        names = ", ".join(sorted(d["name"] for d in discover()))
        return {"error": f"no skill named {name!r} (installed: {names or 'none'})"}
    try:
        text = Path(defn["path"]).read_text(encoding="utf-8", errors="replace")
    except OSError as e:
        return {"error": f"cannot read {defn['path']}: {e}"}
    body = FRONT.sub("", text, count=1) if FRONT.match(text) else text
    return {"result": body.strip() + "\n", "skill_dir": skill_root(defn)}


TOOLS = [{
    "name": "discover_skills",
    "description": "List installed skills (name, description, file path) so "
    "you can pick one to load. Optional query filters by substring.",
    "parameters": {
        "type": "object",
        "properties": {
            "query": {"type": "string",
                      "description": "Optional substring filter over names and descriptions"},
        },
    },
    "tier": "read",
}, {
    "name": "read_skill",
    "description": "Read one skill's full instructions by name. Use after "
    "discover_skills when a task matches a skill's description; the reply "
    "also carries skill_dir, the directory relative paths in the body "
    "resolve against.",
    "parameters": {
        "type": "object",
        "properties": {
            "name": {"type": "string", "description": "Skill name from discover_skills"},
        },
        "required": ["name"],
    },
    "tier": "read",
}]


def main():
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind, mid = msg.get("type"), msg.get("id")
        if kind == "initialize":
            reply({"id": mid, "result": {"tools": TOOLS, "commands": [], "events": []}})
        elif kind == "call_tool":
            name = msg.get("name")
            if name == "discover_skills":
                out = discover_skills(msg.get("args") or {})
            elif name == "read_skill":
                out = read_skill(msg.get("args") or {})
            else:
                out = {"error": f"unknown tool {name}"}
            reply({"id": mid, **out})
        elif kind == "shutdown":
            return


if __name__ == "__main__":
    main()
