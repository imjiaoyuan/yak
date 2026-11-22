#!/usr/bin/env python3
# A goal tracker with an explicit continue loop. `/goal <text>` records the
# task's success condition; `/go [note]` submits the goal back to the model
# as a continuation task — the goal text, the previous answer, and the
# instruction to keep working or declare `DONE:` — so a long objective is
# advanced one deliberate round at a time instead of hoping one task
# finishes it.
#
# Why a command, not an automatic Stop hook: yak's `agent_end` is a
# fire-and-forget notification — nothing an extension replies with can hold
# a finished task open, and an extension deciding to spawn unattended `yak`
# runs behind the user's back is exactly the kind of surprise this harness
# avoids. `/go` is the same continuation, one keystroke away, with the
# human watching each round. (`workflow.py` is the automatic shape, when a
# graph is what you want.)
#
# State lives in `~/.yak/goals.json` (0600), keyed by working directory, so
# a goal survives restarts and two projects never see each other's goal.
# Subagent children are ignored: their agent_end is a child's machinery,
# not progress on your goal.
#
# Install into ~/.yak/extensions/ or the project's .yak/extensions/, /reload.
import json
import os
import sys

CONTINUE_PROMPT = """Continue the task toward this goal:

  goal: {goal}

Previous round's answer:

{last}

{note}Keep working on what remains. If — and only if — the goal is fully \
met, reply with `DONE:` and a one-paragraph summary of what was accomplished.\
"""


def state_path(user_dir):
    return os.path.join(user_dir or os.path.expanduser("~/.yak"), "goals.json")


def load(user_dir):
    try:
        with open(state_path(user_dir), encoding="utf-8") as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}


def save(user_dir, state):
    """One atomic write, 0600: a half-written goal file is worse than none."""
    path = state_path(user_dir)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        os.write(fd, json.dumps(state, indent=1).encode("utf-8"))
    finally:
        os.close(fd)
    os.replace(tmp, path)


def excerpt(text, chars=4000):
    text = (text or "").strip()
    if len(text) <= chars:
        return text or "(no answer recorded)"
    return text[:chars] + "\n… (truncated)"


def handle(msg, state):
    kind = msg.get("type")
    if kind == "initialize":
        state["user_dir"] = (msg.get("params") or {}).get("user_dir")
        return {"events": ["agent_end"], "commands": ["goal", "go"]}
    if kind == "event" and msg.get("name") == "agent_end":
        params = msg.get("params") or {}
        if params.get("interrupted") or os.environ.get("YAK_SUBAGENT_DEPTH"):
            return
        cwd = params.get("cwd") or os.getcwd()
        entry = state.setdefault("by_cwd", {}).get(cwd)
        if entry and entry.get("goal"):
            entry["last"] = params.get("final_text") or ""
            try:
                save(state.get("user_dir"), state)
            except OSError as e:
                sys.stderr.write(f"stop_guard: {e}\n")
        return
    if kind == "run_command":
        name = msg.get("name")
        cwd = (msg.get("context") or {}).get("cwd") or os.getcwd()
        args = (msg.get("args") or "").strip()
        goals = state.setdefault("by_cwd", {})
        if name == "goal":
            if not args:
                entry = goals.get(cwd)
                if not entry or not entry.get("goal"):
                    return "no goal set — /goal <text> sets one, /goal clear drops it"
                lines = [f"goal: {entry['goal']}"]
                if entry.get("last"):
                    lines.append("last answer: recorded — /go continues from it")
                return "\n".join(lines)
            if args == "clear":
                goals.pop(cwd, None)
                try:
                    save(state.get("user_dir"), state)
                except OSError as e:
                    sys.stderr.write(f"stop_guard: {e}\n")
                return "goal cleared"
            goals[cwd] = {"goal": args, "last": goals.get(cwd, {}).get("last", "")}
            try:
                save(state.get("user_dir"), state)
            except OSError as e:
                sys.stderr.write(f"stop_guard: {e}\n")
                return f"goal noted, but saving failed: {e}"
            return f"goal set — /go starts (or continues) work toward it"
        if name == "go":
            entry = goals.get(cwd)
            if not entry or not entry.get("goal"):
                return "no goal set for this directory — /goal <text> first"
            note = f"{args}\n" if args else ""
            task = CONTINUE_PROMPT.format(
                goal=entry["goal"], last=excerpt(entry.get("last")), note=note
            )
            return {"task": task}
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
            sys.stderr.write(f"stop_guard: {e}\n")
            reply = None
        if kind == "run_command":
            print(json.dumps({"id": mid, "result": reply}), flush=True)
        elif kind == "initialize":
            print(json.dumps({"id": mid, "result": reply or {}}), flush=True)
        elif kind == "event":
            print(json.dumps({"id": mid, "result": None}), flush=True)


if __name__ == "__main__":
    main()
