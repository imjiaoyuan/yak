#!/usr/bin/env python3
"""session_log — let the model read its own persisted transcript.

The thread file under `<user_dir>/threads/<id>.jsonl` is the durable session:
append-only, one line per agent round, compaction archives into it instead of
deleting. What the context window carries is a projection of that log —
summaries replace old rounds, oversized tool results are cut to head+tail.
This tool hands the model a bounded read over the log itself: `list` shows
one row per round, `show` replays full rounds, so anything the projection
dropped can be recovered instead of lost.

The thread id rides every `call_tool` frame's `context` (`thread`, null until
the first round persists); the extension never guesses it from the directory.
Install into `~/.yak/extensions/` or the project's `.yak/extensions/`,
chmod +x, `/reload`.
"""
import json
import os
import sys

MAX_LIST_ROWS = 200
MAX_SHOW_TURNS = 40
CAP_PROMPT = 6000
CAP_RESPONSE = 6000
CAP_MESSAGE = 4000
CAP_TOOL_ARGS = 1500
CAP_REASONING = 2000
KNOWN_VERSIONS = (1, 2)


def clip(text, cap):
    text = text or ""
    if len(text) <= cap:
        return text
    return text[:cap] + "\n…[+%d chars truncated — rerun show on this turn range to re-read]" % (
        len(text) - cap
    )


def first_line(text, width=90):
    line = (text or "").strip().splitlines()
    if not line:
        return ""
    out = line[0]
    return out if len(out) <= width else out[: width - 1] + "…"


def usage_cells(turn):
    usage = turn.get("usage")
    if not isinstance(usage, list) or not usage:
        return "-"
    if len(usage) >= 4:
        return "in=%dk out=%dk (cached %dk)" % (
            usage[0] // 1000,
            usage[1] // 1000,
            usage[2] // 1000,
        )
    return "in=%dk out=%dk" % (usage[0] // 1000, usage[1] // 1000)


def turn_calls(turn):
    names = []
    for msg in turn.get("messages") or []:
        if msg.get("role") == "assistant":
            for call in msg.get("tool_calls") or []:
                name = call.get("name") or "?"
                if not names or names[-1] != name:
                    names.append(name)
    return names


def list_row(pos, turn):
    cells = ["#%d" % pos]
    if turn.get("parent"):
        cells.append("[branch]")
    cells.append(turn.get("ts") or "-")
    cells.append(first_line(turn.get("prompt")) or "(tool round)")
    calls = turn_calls(turn)
    if calls:
        cells.append("calls: " + ",".join(calls[:6]) + ("…" if len(calls) > 6 else ""))
    answer = first_line(turn.get("response"))
    if answer:
        cells.append("→ " + answer)
    cells.append(usage_cells(turn))
    return " | ".join(cells)


def render_attachment(att):
    name = att.get("filename") or ""
    mime = att.get("mime_type") or "?"
    return "[attachment %s %s]" % (mime, name) if name else "[attachment %s]" % mime


def render_message(msg):
    role = msg.get("role")
    if role == "user":
        body = clip(msg.get("text"), CAP_MESSAGE)
        atts = msg.get("attachments") or []
        if atts:
            body += "\n" + "\n".join(render_attachment(a) for a in atts)
        return "[user] " + body if body else "[user] (empty)"
    if role == "assistant":
        parts = []
        if msg.get("reasoning"):
            parts.append("[thinking] " + clip(msg["reasoning"], CAP_REASONING))
        text = clip(msg.get("text"), CAP_MESSAGE)
        if text:
            parts.append("[assistant] " + text)
        for call in msg.get("tool_calls") or []:
            args = call.get("arguments")
            args_text = args if isinstance(args, str) else json.dumps(
                args, ensure_ascii=False
            )
            parts.append(
                "[tool_call %s %s] %s"
                % (call.get("name") or "?", call.get("id") or "", clip(args_text, CAP_TOOL_ARGS))
            )
        return "\n".join(parts) if parts else "[assistant] (empty)"
    if role == "tool_result":
        error = msg.get("error")
        marker = " ERROR(%s)" % error if error else ""
        return "[tool_result %s]%s %s" % (
            msg.get("name") or "?",
            marker,
            clip(msg.get("content"), CAP_MESSAGE),
        )
    if role == "summary":
        return "[summary] " + clip(msg.get("text"), CAP_MESSAGE)
    return "[%s] %s" % (role or "?", clip(json.dumps(msg, ensure_ascii=False), CAP_MESSAGE))


def render_turn(pos, turn):
    head = "== turn #%d  %s  model=%s  %s" % (
        pos,
        turn.get("ts") or "-",
        turn.get("model") or "-",
        usage_cells(turn),
    )
    lines = [head]
    prompt = turn.get("prompt")
    if prompt:
        lines.append("[prompt] " + clip(prompt, CAP_PROMPT))
    messages = turn.get("messages")
    if isinstance(messages, list):
        for msg in messages:
            try:
                lines.append(render_message(msg))
            except Exception as e:
                lines.append("[unrenderable message: %s]" % e)
    response = turn.get("response")
    if response:
        lines.append("[response] " + clip(response, CAP_RESPONSE))
    return "\n".join(lines)


def load_turns(path):
    turns = []
    with open(path, encoding="utf-8", errors="replace") as f:
        for pos, raw in enumerate(f, 1):
            if not raw.strip():
                continue
            try:
                turn = json.loads(raw)
            except ValueError as e:
                raise ValueError("corrupt turn at position %d of %s: %s" % (pos, path, e))
            if not isinstance(turn, dict):
                raise ValueError("turn at position %d of %s is not an object" % (pos, path))
            version = turn.get("v", 1)
            if version not in KNOWN_VERSIONS:
                raise ValueError(
                    "turn at position %d carries format v%s; this build reads v%s — "
                    "upgrade yak" % (pos, version, max(KNOWN_VERSIONS))
                )
            turns.append((pos, turn))
    return turns


def resolve_range(turns, args):
    total = len(turns)
    if total == 0:
        return None, "the thread file has no turns yet"
    start = args.get("from")
    end = args.get("to")
    if start is None:
        start = 1
    if end is None:
        end = start if args.get("action") == "show" else total
    if not isinstance(start, int) or not isinstance(end, int) or start < 1 or end < start:
        return None, "invalid range: from=%s to=%s (positions are 1-based, from ≤ to)" % (
            start,
            end,
        )
    if end > total:
        end = total
    if start > total:
        return None, "from=%s is past the newest turn (#%d)" % (start, total)
    return (start, end), None


def handle(args, thread, user_dir):
    action = args.get("action")
    if action not in ("list", "show"):
        return "error: action must be \"list\" or \"show\""
    if not thread:
        return (
            "error: no session thread yet — the first persisted round creates "
            "one (--no-session runs never have one)"
        )
    path = os.path.join(user_dir, "threads", "%s.jsonl" % thread)
    try:
        turns = load_turns(path)
    except OSError:
        return "error: thread %s has no file at %s (not persisted yet?)" % (thread, path)
    rng, problem = resolve_range(turns, dict(args, action=action))
    if problem:
        return "error: %s" % problem
    start, end = rng
    window = turns[start - 1 : end]
    if action == "list":
        rows = [list_row(pos, turn) for pos, turn in window]
        if len(rows) > MAX_LIST_ROWS:
            head = "turns #%d–#%d of #%d (showing newest %d — raise `from` for older ones)" % (
                start,
                end,
                len(turns),
                MAX_LIST_ROWS,
            )
            rows = rows[-MAX_LIST_ROWS:]
            return head + "\n" + "\n".join(rows)
        return "turns #%d–#%d of #%d\n%s" % (start, end, len(turns), "\n".join(rows))
    if end - start + 1 > MAX_SHOW_TURNS:
        return (
            "error: %d turns exceed the %d-turn show window — narrow `from`/`to` "
            "(list first to find the round you want)" % (end - start + 1, MAX_SHOW_TURNS)
        )
    return "\n\n".join(render_turn(pos, turn) for pos, turn in window)


def main():
    user_dir = None
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = msg.get("type")
        if kind == "shutdown":
            return
        if kind == "initialize":
            params = msg.get("params") or {}
            user_dir = params.get("user_dir") or os.path.join(
                os.path.expanduser("~"), ".yak"
            )
            print(
                json.dumps(
                    {
                        "id": msg.get("id"),
                        "result": {
                            "tools": [
                                {
                                    "name": "session_log",
                                    "description": (
                                        "read this conversation's persisted log — the "
                                        "durable record outside your context window. Use "
                                        "it to recover rounds or tool results lost to "
                                        "compaction or output truncation, or to re-read "
                                        "what happened earlier. `list` gives one row per "
                                        "round (positions to target with `show`); `show` "
                                        "replays full rounds from/to."
                                    ),
                                    "input_schema": {
                                        "type": "object",
                                        "properties": {
                                            "action": {
                                                "type": "string",
                                                "enum": ["list", "show"],
                                                "description": (
                                                    "list = one row per round; "
                                                    "show = full content of a round range"
                                                ),
                                            },
                                            "from": {
                                                "type": "number",
                                                "description": (
                                                    "first round position (1-based, file "
                                                    "order; list rows are numbered)"
                                                ),
                                            },
                                            "to": {
                                                "type": "number",
                                                "description": (
                                                    "last round position, inclusive "
                                                    "(show defaults to `from`; list "
                                                    "defaults to the newest)"
                                                ),
                                            },
                                        },
                                        "required": ["action"],
                                    },
                                }
                            ],
                            "events": [],
                        },
                    }
                ),
                flush=True,
            )
        elif kind == "call_tool":
            thread = (msg.get("context") or {}).get("thread")
            try:
                out = handle(msg.get("args") or {}, thread, user_dir)
            except Exception as e:
                out = "error: session_log failed: %s" % e
            print(json.dumps({"id": msg.get("id"), "result": out}), flush=True)


if __name__ == "__main__":
    main()
