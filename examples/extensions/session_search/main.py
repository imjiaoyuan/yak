import datetime
import json
import os
from pathlib import Path
import re
import sqlite3
import sys


TOOL = {
    "name": "session_search",
    "tier": "exec",
    "description": "Search a disposable local FTS5 index of persisted transcripts. Active branch only by default; never modifies transcripts. read returns a capped exact JSON turn. Warnings identify ignored unsealed tails. cwd is an exact recorded path; dates are inclusive YYYY-MM-DD UTC date prefixes.",
    "parameters": {
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": ["search", "read", "reindex"]},
            "query": {"type": "string", "description": "Literal words, AND matched; not FTS syntax"},
            "thread": {"type": "string"},
            "turn": {"type": "string"},
            "all_branches": {"type": "boolean"},
            "cwd": {"type": "string", "description": "Exact recorded cwd"},
            "after": {"type": "string", "description": "Inclusive YYYY-MM-DD"},
            "before": {"type": "string", "description": "Inclusive YYYY-MM-DD"},
            "limit": {"type": "integer", "minimum": 1, "maximum": 10},
            "cap": {"type": "integer", "minimum": 100, "maximum": 10000},
        },
        "required": ["action"],
        "additionalProperties": False,
    },
}


def bounded(value, low, high, name):
    if type(value) is not int or not low <= value <= high:
        raise ValueError(f"{name} must be an integer in {low}..{high}")
    return value


def private_home(user_dir):
    home = Path(user_dir) / "extensions" / "session_search"
    home.mkdir(parents=True, exist_ok=True, mode=0o700)
    if os.name != "nt":
        home.chmod(0o700)
    return home


def signature(path):
    stat = path.stat()
    return json.dumps([stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns, stat.st_ino])


def text(value, name):
    if not isinstance(value, str):
        raise ValueError(f"{name} must be a string")
    return value


def load_source(path):
    before = signature(path)
    turns = []
    warnings = []
    seen = {}
    with path.open("rb") as stream:
        for line_number, raw in enumerate(stream, 1):
            if not raw.endswith(b"\n"):
                warnings.append(f"{path}: ignored unsealed tail at line {line_number}; transcript unchanged")
                break
            if not raw.strip():
                continue
            try:
                turn = json.loads(raw.decode("utf-8"), parse_constant=reject_constant)
                if not isinstance(turn, dict):
                    raise ValueError("turn must be an object")
                version = turn.get("v", 0)
                if type(version) is not int or version < 0:
                    raise ValueError("invalid format version")
                if version > 2:
                    raise ValueError(f"future format v{version}; upgrade this extension")
                for key in ("id", "ts", "mode", "model"):
                    text(turn.get(key), key)
                if not turn["id"] or turn["id"] in seen:
                    raise ValueError("empty or duplicate turn id")
                datetime.datetime.fromisoformat(turn["ts"])
                if turn.get("cwd") is not None:
                    text(turn["cwd"], "cwd")
                for key in ("prompt", "response"):
                    text(turn.get(key, ""), key)
                if turn.get("reasoning") is not None:
                    text(turn["reasoning"], "reasoning")
                parent = turn.get("parent")
                if parent is not None:
                    text(parent, "parent")
                    if turns and parent not in seen:
                        raise ValueError("parent must name an earlier turn")
                if not isinstance(turn.get("messages"), list):
                    raise ValueError("messages must be an array")
                list(passages(turn))
            except (ValueError, TypeError, KeyError) as exc:
                raise ValueError(f"{path}: corrupt turn on line {line_number}: {exc}") from None
            seen[turn["id"]] = len(turns)
            turns.append((turn, line_number, raw.decode("utf-8").rstrip("\r\n")))
    if signature(path) != before:
        raise ValueError(f"{path}: changed while reading; retry")
    active = set()
    cursor = len(turns) - 1
    while cursor >= 0:
        turn = turns[cursor][0]
        active.add(turn["id"])
        if cursor == 0:
            break
        cursor = seen[turn["parent"]] if turn.get("parent") is not None else cursor - 1
    return turns, active, warnings, before


def reject_constant(value):
    raise ValueError(f"invalid JSON constant {value}")


def passages(turn):
    messages = turn["messages"]
    for message in messages:
        if not isinstance(message, dict):
            raise ValueError("message must be an object")
        role = message.get("role")
        if role not in ("user", "assistant", "tool_result", "summary"):
            raise ValueError(f"unsupported message role {role}")
        body = text(message.get("content" if role == "tool_result" else "text", ""), "message text")
        if body:
            yield role, body
        if message.get("reasoning") is not None:
            reasoning = text(message["reasoning"], "message reasoning")
            if reasoning:
                yield "reasoning", reasoning
        if message.get("tool_calls"):
            if not isinstance(message["tool_calls"], list):
                raise ValueError("tool_calls must be an array")
            for call in message["tool_calls"]:
                yield "tool_call", json.dumps(call, ensure_ascii=False, allow_nan=False)
    prompt = turn.get("prompt", "")
    if prompt and not any(m.get("role") == "user" and m.get("text") == prompt for m in messages):
        yield "user", prompt
    response = turn.get("response", "")
    if response and not (messages and messages[-1].get("role") == "assistant" and not messages[-1].get("tool_calls") and messages[-1].get("text") == response):
        yield "assistant", response
    reasoning = turn.get("reasoning")
    if reasoning and not any(m.get("reasoning") == reasoning for m in messages):
        yield "reasoning", reasoning


class SessionSearch:
    def __init__(self, user_dir):
        self.threads = Path(user_dir) / "threads"
        self.home = private_home(user_dir)
        path = self.home / "index.sqlite"
        descriptor = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
        os.close(descriptor)
        if os.name != "nt":
            path.chmod(0o600)
        self.db = sqlite3.connect(path)
        self.db.row_factory = sqlite3.Row
        self.db.executescript("""
            CREATE TABLE IF NOT EXISTS sources(thread TEXT PRIMARY KEY, signature TEXT NOT NULL, warnings TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS turns(thread TEXT, turn TEXT, parent TEXT, effective_parent TEXT, active INTEGER, position INTEGER, cwd TEXT, ts TEXT, PRIMARY KEY(thread, turn));
            CREATE VIRTUAL TABLE IF NOT EXISTS docs USING fts5(text, thread UNINDEXED, turn UNINDEXED, role UNINDEXED, part UNINDEXED);
        """)

    def sync(self, force=False):
        warnings = []
        paths = sorted(self.threads.glob("*.jsonl"))
        current = {p.stem for p in paths}
        changed = 0
        with self.db:
            for row in self.db.execute("SELECT thread FROM sources").fetchall():
                if row[0] not in current:
                    self.remove(row[0])
            for path in paths:
                old = self.db.execute("SELECT signature, warnings FROM sources WHERE thread=?", (path.stem,)).fetchone()
                if not force and old and old[0] == signature(path):
                    warnings.extend(json.loads(old[1]))
                    continue
                turns, active, notes, stamp = load_source(path)
                self.remove(path.stem)
                previous = None
                for turn, position, raw in turns:
                    self.db.execute("INSERT INTO turns VALUES(?,?,?,?,?,?,?,?)", (path.stem, turn["id"], turn.get("parent"), turn.get("parent") if turn.get("parent") is not None else previous, int(turn["id"] in active), position, turn.get("cwd"), turn["ts"]))
                    for part, (role, body) in enumerate(passages(turn)):
                        self.db.execute("INSERT INTO docs(text,thread,turn,role,part) VALUES(?,?,?,?,?)", (body, path.stem, turn["id"], role, part))
                    previous = turn["id"]
                self.db.execute("INSERT INTO sources VALUES(?,?,?)", (path.stem, stamp, json.dumps(notes)))
                warnings.extend(notes)
                changed += 1
        return changed, warnings

    def remove(self, thread):
        for table in ("docs", "turns", "sources"):
            self.db.execute(f"DELETE FROM {table} WHERE thread=?", (thread,))

    def handle(self, args):
        action = args.get("action")
        if action not in ("search", "read", "reindex"):
            raise ValueError("action must be search, read or reindex")
        if "all_branches" in args and type(args["all_branches"]) is not bool:
            raise ValueError("all_branches must be boolean")
        changed, warnings = self.sync(action == "reindex")
        if action == "reindex":
            return {"rebuilt_sources": changed, "warnings": warnings}
        if action == "read":
            thread = text(args.get("thread"), "thread")
            turn_id = text(args.get("turn"), "turn")
            row = self.db.execute("SELECT * FROM turns WHERE thread=? AND turn=?", (thread, turn_id)).fetchone()
            if row is None:
                raise ValueError("thread/turn not found")
            cap = bounded(args.get("cap", 6000), 100, 10000, "cap")
            path = self.threads / f"{thread}.jsonl"
            turns, active, notes, stamp = load_source(path)
            for turn, position, raw in turns:
                if turn["id"] == turn_id:
                    return {"provenance": dict(row), "source": str(path), "format": "exact JSON line (capped characters)", "text": raw[:cap], "total_chars": len(raw), "truncated": len(raw) > cap, "warnings": sorted(set(warnings + notes))}
            raise ValueError("source changed since indexing; retry")
        query = text(args.get("query"), "query")
        if not query.strip() or len(query) > 512:
            raise ValueError("query must contain 1..512 characters")
        words = re.findall(r"\w+", query, re.UNICODE)
        if not words:
            raise ValueError("query must contain searchable words")
        limit = bounded(args.get("limit", 5), 1, 10, "limit")
        where = ["docs MATCH ?"]
        values = [" AND ".join('"' + word + '"' for word in words)]
        if not args.get("all_branches", False):
            where.append("t.active=1")
        for key in ("thread", "cwd"):
            if key in args:
                where.append(f"t.{key}=?")
                values.append(text(args[key], key))
        for key, operator in (("after", ">="), ("before", "<=")):
            if key in args:
                value = text(args[key], key)
                if datetime.date.fromisoformat(value).isoformat() != value:
                    raise ValueError(f"{key} must be YYYY-MM-DD")
                where.append(f"substr(t.ts,1,10){operator}?")
                values.append(value)
        if args.get("after", "") > args.get("before", "9999-12-31"):
            raise ValueError("after must not be later than before")
        rows = self.db.execute("SELECT t.*, docs.role, docs.part, docs.text, bm25(docs) AS rank FROM docs JOIN turns t ON t.thread=docs.thread AND t.turn=docs.turn WHERE " + " AND ".join(where) + " ORDER BY rank,t.thread,t.position,CAST(docs.part AS INTEGER) LIMIT ?", values + [limit]).fetchall()
        results = []
        for row in rows:
            item = dict(row)
            body = item.pop("text")
            match = re.search("|".join(re.escape(w) for w in words), body, re.IGNORECASE)
            start = max(0, (match.start() if match else 0) - 120)
            item.update(source=str(self.threads / f"{item['thread']}.jsonl"), excerpt=body[start:start + 600], excerpt_offset=start, total_chars=len(body), truncated=start > 0 or len(body) > start + 600)
            results.append(item)
        return {"results": results, "branch_scope": "all" if args.get("all_branches") else "active", "warnings": warnings}


def main():
    plugin = None
    for line in sys.stdin:
        frame = json.loads(line)
        try:
            if frame.get("type") == "shutdown":
                break
            if frame.get("type") == "initialize":
                plugin = SessionSearch(frame["params"]["user_dir"])
                result = {"tools": [TOOL]}
            elif frame.get("type") == "call_tool":
                if plugin is None or frame.get("name") != TOOL["name"]:
                    raise ValueError("tool not initialized or unknown")
                result = json.dumps(plugin.handle(frame.get("args", {})), ensure_ascii=False, allow_nan=False)
            else:
                raise ValueError("unsupported request")
            reply = {"id": frame["id"], "result": result}
        except (ValueError, OSError, sqlite3.Error, KeyError, TypeError) as exc:
            reply = {"id": frame["id"], "error": str(exc)}
        print(json.dumps(reply, ensure_ascii=False, allow_nan=False), flush=True)
    if plugin is not None:
        plugin.db.close()


if __name__ == "__main__":
    main()
