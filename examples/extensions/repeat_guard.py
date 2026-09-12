#!/usr/bin/env python3
"""repeat-guard — loop hygiene as a resident llm extension (a loop guard).

Copy to ~/.llm/extensions/repeat-guard.py, chmod +x, /reload. It mounts no
tools and no commands; it subscribes to two events and denies the call that
keeps a stuck loop alive:

  input      — a fresh user message resets the repeat streak (a new
               instruction is never treated as a loop)
  tool_call  — counted per (tool, arguments) with arguments compared modulo
               key order; from the REMIND_AT-th consecutive identical call
               the call is denied and the reason (the reminder) reaches the
               model as the tool's error result

This is the deny-flavored twin of the in-core advisory RepeatGuard (which
lets the call run and rides a [System] note on its result). Load either one,
not both: a denial here means the core guard never sees a third identical
call, so together they are redundant rather than harmful. What the event
protocol cannot express is the advisory middle ground — a deny gate can only
refuse — which is why the core keeps its own copy.

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
