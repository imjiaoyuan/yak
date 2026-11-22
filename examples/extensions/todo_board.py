#!/usr/bin/env python3
# A shared todo board the model keeps itself: `todo_read` returns the current
# list, `todo_write` replaces it — at most one `in_progress` step, same shape
# as the built-in `plan` — and while a write is in flight the board is
# rendered as a lane row ("2/5 done · next: …"), the same live surface
# subagent.py drives. Unlike `plan`, the board survives across tasks and
# sessions: state lives in `~/.yak/todos.json` keyed by working directory
# (0600, atomic replace), so `/todos` shows what was left open here.
#
# Commands:
#   /todos          the board as it stands (or "no open todos")
#   /todos clear    drop this directory's board
#
# The board is the model's own working memory, not a gate: nothing enforces
# that it updates the list (a gate needs a `tool_call` subscriber — see
# repeat_guard.py for that seam).
#
# Install into ~/.yak/extensions/ or the project's .yak/extensions/, /reload.
import json
import os
import sys


def state_path(user_dir):
    return os.path.join(user_dir or os.path.expanduser("~/.yak"), "todos.json")


def load(user_dir):
    try:
        with open(state_path(user_dir), encoding="utf-8") as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}


def save(user_dir, state):
    """One atomic write, 0600."""
    path = state_path(user_dir)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        os.write(fd, json.dumps(state, indent=1).encode("utf-8"))
    finally:
        os.close(fd)
    os.replace(tmp, path)


def validate(steps):
    """The plan shape: {step, status} entries, at most one in_progress."""
    if not isinstance(steps, list):
        return "todos must be a list of {step, status} entries"
    seen_progress = 0
    for entry in steps:
        if not isinstance(entry, dict) or not isinstance(entry.get("step"), str):
            return "every todo needs a step string"
        status = entry.get("status", "pending")
        if status not in ("pending", "in_progress", "done"):
            return f"unknown status {status!r} (pending, in_progress, done)"
        if status == "in_progress":
            seen_progress += 1
        entry["status"] = status
    if seen_progress > 1:
        return "at most one todo may be in_progress"
    return None


def lane_line(steps):
    """The live row: progress counts and what is next."""
    done = sum(1 for s in steps if s.get("status") == "done")
    current = next((s["step"] for s in steps if s.get("status") == "in_progress"), None)
    if current:
        return f"todos {done}/{len(steps)} · now: {current}"
    pending = next((s["step"] for s in reversed(steps) if s.get("status") == "pending"), None)
    if pending:
        return f"todos {done}/{len(steps)} · next: {pending}"
    return f"todos {done}/{len(steps)} · all done"


def handle(msg, state):
    kind = msg.get("type")
    if kind == "initialize":
        state["user_dir"] = (msg.get("params") or {}).get("user_dir")
        schema = {
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "step": {"type": "string"},
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "done"],
                            },
                        },
                        "required": ["step", "status"],
                    },
                }
            },
            "required": ["todos"],
        }
        return {
            "tools": [
                {
                    "name": "todo_read",
                    "description": (
                        "read the shared todo board for this working directory: "
                        "the persisted {step, status} list across tasks and sessions"
                    ),
                    "parameters": {"type": "object", "properties": {}, "required": []},
                    "tier": "read",
                },
                {
                    "name": "todo_write",
                    "description": (
                        "replace the shared todo board with this list "
                        "({step, status}, at most one in_progress)"
                    ),
                    "parameters": schema,
                    "tier": "write",
                },
            ],
            "commands": ["todos"],
            "events": [],
        }
    if kind == "call_tool":
        name = msg.get("name")
        cwd = os.getcwd()  # the host spawns us with the agent's cwd
        board = state.setdefault("by_cwd", {}).setdefault(cwd, [])
        if name == "todo_read":
            if not board:
                return "no todos for this directory"
            return json.dumps(board, indent=1)
        if name == "todo_write":
            todos = (msg.get("args") or {}).get("todos")
            error = validate(todos)
            if error:
                return {"error": error}
            state["by_cwd"][cwd] = todos
            try:
                save(state.get("user_dir"), state)
            except OSError as e:
                sys.stderr.write(f"todo_board: {e}\n")
            # the live row while the call runs: progress and what is next
            print(
                json.dumps({"type": "lane", "v": 1, "lane": "todos", "text": lane_line(todos)}),
                flush=True,
            )
            return f"board updated: {lane_line(todos)}"
        return {"error": f"unknown tool {name}"}
    if kind == "run_command" and msg.get("name") == "todos":
        cwd = (msg.get("context") or {}).get("cwd") or os.getcwd()
        board = state.setdefault("by_cwd", {}).setdefault(cwd, [])
        args = (msg.get("args") or "").strip()
        if args == "clear":
            state.setdefault("by_cwd", {}).pop(cwd, None)
            try:
                save(state.get("user_dir"), state)
            except OSError as e:
                sys.stderr.write(f"todo_board: {e}\n")
            return "todo board cleared"
        if not board:
            return "no todos for this directory (the model builds one with todo_write)"
        lines = [lane_line(board)]
        lines += [f"  [{s.get('status'):<11}] {s['step']}" for s in board]
        return "\n".join(lines)
    return None


def main():
    state = {}
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind, mid = msg.get("type"), msg.get("id")
        if kind == "shutdown":
            return
        try:
            reply = handle(msg, state)
        except Exception as e:
            sys.stderr.write(f"todo_board: {e}\n")
            reply = {"error": str(e)} if kind == "call_tool" else None
        if kind in ("call_tool", "run_command", "initialize"):
            print(json.dumps({"id": mid, "result": reply}), flush=True)


if __name__ == "__main__":
    main()
