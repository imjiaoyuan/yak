#!/usr/bin/env python3
"""llm extension template (python) — copy to ~/.llm/extensions/<name> and
edit the USER SECTION below. The protocol loop above talks to the agent
host; nothing below needs to know about it."""

import json
import os
import sys

# ---------------------------------------------------------------- protocol --
TOOLS = {}       # name -> (description, schema, handler(args_dict) -> str)
COMMANDS = {}    # name -> handler(arg_text) -> str
EVENTS = {}      # event name -> [handler(params_dict) -> reply-or-None]


def reply(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def tool(name, description, parameters=None):
    """Register a tool; the handler receives the arguments dict and
    returns the result text."""
    def wrap(handler):
        TOOLS[name] = (
            description,
            parameters or {"type": "object", "properties": {}},
            handler,
        )
        return handler

    return wrap


def on(event):
    """Subscribe to an agent event. A tool_call handler may return
    {"decision": "deny", "reason": ..} or {"args": {...}} to rewrite."""
    def wrap(handler):
        EVENTS.setdefault(event, []).append(handler)
        return handler

    return wrap


def command(name):
    """Register a /slash command; the handler receives the argument text
    and returns the reply text."""
    def wrap(handler):
        COMMANDS[name] = handler
        return handler

    return wrap


def run():
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except ValueError:
            continue
        kind = req.get("type")
        if kind == "initialize":
            reply({
                "id": req["id"],
                "result": {
                    "tools": [
                        {"name": n, "description": d, "parameters": s}
                        for n, (d, s, _) in TOOLS.items()
                    ],
                    "commands": list(COMMANDS),
                    "events": list(EVENTS),
                },
            })
        elif kind == "call_tool":
            try:
                handler = TOOLS[req["name"]][2]
                out = handler(req.get("args") or {})
                reply({"id": req["id"], "result": str(out)})
            except Exception as e:  # tool errors are results, not crashes
                reply({"id": req["id"], "result": f"error: {e}"})
        elif kind == "run_command":
            try:
                out = COMMANDS[req["name"]](req.get("args") or "")
                reply({"id": req["id"], "result": str(out)})
            except Exception as e:
                reply({"id": req["id"], "result": f"error: {e}"})
        elif kind == "event":
            result = None
            for handler in EVENTS.get(req["name"], []):
                try:
                    r = handler(req.get("params") or {})
                    if result is None and isinstance(r, dict):
                        result = r
                except Exception as e:
                    sys.stderr.write(f"{req['name']} handler failed: {e}\n")
            reply({"id": req["id"], "result": result})
        elif kind == "shutdown":
            break


# ----------------------------- USER SECTION ---------------------------------
# Register tools, commands and event hooks here. Examples:

@tool("now", "Current local time")
def now(args):
    import time
    return time.strftime("%Y-%m-%d %H:%M:%S")


@on("tool_call")
def gate(params):
    # return {"decision": "deny", "reason": "no"} to block a call, or
    # {"args": {...}} to rewrite its arguments
    return None


run()
