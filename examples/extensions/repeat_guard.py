#!/usr/bin/env python3
"""repeat-guard — loop hygiene as a resident yak extension (a loop guard).

Copy to ~/.yak/extensions/repeat-guard.py, chmod +x, /reload. It mounts no
tools and no commands; it subscribes to two events and denies the call that
keeps a stuck loop alive:

  input      — a fresh user message resets the repeat streak (a new
               instruction is never treated as a loop)
  tool_call  — counted per (tool, arguments) with arguments compared modulo
               key order; from the REMIND_AT-th consecutive identical call
               the call is denied and the reason (the reminder) reaches the
               model as the tool's error result

The core loop runs unbounded (the reference's shape) and ships no repeat guard of its
own: a stuck loop is compaction's business, and a model that genuinely needs
a hard stop gets one here — as an extension, where the threshold and the
remedy are yours to edit.

One honest limitation: the protocol has no config channel for resident
extensions, so the threshold lives in this file. Edit REMIND_AT and /reload.
"""

import json
import sys

REMIND_AT = 3   # deny from the Nth consecutive identical call
PREVIEW = 200   # chars of the repeated arguments echoed in the denial


def reply(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def canon(v):
    """A key-order-insensitive fingerprint: dict keys sorted recursively."""
    if isinstance(v, dict):
        return {k: canon(v[k]) for k in sorted(v)}
    if isinstance(v, list):
        return [canon(x) for x in v]
    return v


def main():
    last, count = None, 0
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        mid, kind = msg.get("id"), msg.get("type")
        if kind == "initialize":
            # no tools, no commands — pure event gate
            reply({"id": mid, "result": {"tools": [], "commands": [],
                                         "events": ["input", "tool_call"]}})
        elif kind == "event" and msg.get("name") == "input":
            last, count = None, 0
            reply({"id": mid, "result": None})
        elif kind == "event" and msg.get("name") == "tool_call":
            params = msg.get("params") or {}
            tool = str(params.get("tool") or "?")
            key = (tool, canon(params.get("args") or {}))
            if key == last:
                count += 1
            else:
                last, count = key, 1
            if count >= REMIND_AT:
                preview = json.dumps(params.get("args") or {})
                if len(preview) > PREVIEW:
                    preview = preview[:PREVIEW] + "…"
                reply({"id": mid, "result": {
                    "decision": "deny",
                    "reason": f"identical {tool} call repeated {count} times in a row "
                              f"({preview}) — the result will not change: analyze what you "
                              f"already have, change approach, or finish (repeat-guard)",
                }})
            else:
                reply({"id": mid, "result": None})
        elif kind == "shutdown":
            return
        elif mid is not None:
            # every request gets exactly one reply (the host correlates by id)
            reply({"id": mid, "result": None})


if __name__ == "__main__":
    main()
