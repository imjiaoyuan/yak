#!/usr/bin/env python3
# A usage ledger: every `turn_end` appends one line to `~/.yak/usage.jsonl`,
# and `/usage [today|month|all]` totals the ledger — tokens by model and
# (when prices are known) an estimated cost. The shape every provider
# reports rides the event as `usage: [input, output, cached, cached_write?]`
# (the cached-write slot may be absent on older wires).
#
# Costs are estimates and only when told: drop a `prices.json` beside this
# script mapping a model-name substring to USD per million tokens, e.g.
#   {"deepseek-chat": {"input": 0.27, "cached": 0.07, "output": 1.1}}
# Unmatched models report tokens only, never a made-up price.
#
# Subagent children (subagent.py spawns `yak --json` with the same user
# extension dir) are recorded too, flagged `subagent: true`: their API
# usage is real spend the parent never sees in its own numbers. Reading the
# ledger stays cheap: `/usage` streams the file once, same as any append
# log; there is no index to invalidate.
#
# Install into ~/.yak/extensions/, /reload; watch it with /status.
import json
import os
import sys
import threading
from datetime import datetime, timezone

LEDGER = "usage.jsonl"

# USD per million tokens by model-name substring; absent keys mean "no
# price known — report tokens only". Or drop a prices.json beside the script.
PRICES = {}

_lock = threading.Lock()


def ledger_path(user_dir):
    return os.path.join(user_dir or os.path.expanduser("~/.yak"), LEDGER)


def prices_path():
    return os.path.join(os.path.dirname(os.path.abspath(__file__)), "prices.json")


def load_prices():
    """Header table first, then an optional prices.json beside the script."""
    table = dict(PRICES)
    try:
        with open(prices_path(), encoding="utf-8") as f:
            table.update(json.load(f))
    except (OSError, json.JSONDecodeError):
        pass
    return table


def append_record(user_dir, record):
    """One single-write append, 0600: the ledger is the user's spend data."""
    line = json.dumps(record) + "\n"
    directory = os.path.dirname(ledger_path(user_dir))
    os.makedirs(directory, exist_ok=True)
    fd = os.open(
        ledger_path(user_dir), os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600
    )
    try:
        os.write(fd, line.encode("utf-8"))
    finally:
        os.close(fd)


def read_records(user_dir):
    """A torn final line (a crash mid-append) is skipped, not fatal."""
    path = ledger_path(user_dir)
    records = []
    try:
        with open(path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    records.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
    except OSError:
        pass
    return records


def cost_of(record, prices):
    """Estimated USD, or None when this model has no known price."""
    model = record.get("model") or ""
    for key, price in prices.items():
        if key in model:
            rate = lambda slot: float(price.get(slot, 0.0))  # noqa: E731
            return (
                record.get("input", 0) * rate("input")
                + record.get("output", 0) * rate("output")
                + record.get("cached", 0) * rate("cached")
                + record.get("cached_write", 0) * rate("cached_write")
            ) / 1e6
    return None


def summarize(records, prices):
    """(models, tokens, cost) over the selected records."""
    models = {}
    tokens = 0
    cost = 0.0
    priced = False
    for r in records:
        used = (
            r.get("input", 0) + r.get("output", 0) + r.get("cached", 0)
            + r.get("cached_write", 0)
        )
        tokens += used
        slot = models.setdefault(r.get("model") or "unknown", [0, 0.0, False])
        slot[0] += used
        c = cost_of(r, prices)
        if c is not None:
            priced = True
            cost += c
            slot[1] += c
            slot[2] = True
    return models, tokens, cost, priced


def fmt_tokens(n):
    if n >= 1_000_000:
        return f"{n / 1_000_000:.2f}M"
    if n >= 1_000:
        return f"{n / 1_000:.1f}k"
    return str(n)


def report(records, prices, scope):
    models, tokens, cost, priced = summarize(records, prices)
    if not records:
        return f"no usage recorded ({scope})"
    lines = [f"yak usage — {scope}: {len(records)} turns, {fmt_tokens(tokens)} tokens"]
    for model, (used, mcost, priced_model) in sorted(
        models.items(), key=lambda kv: -kv[1][0]
    ):
        row = f"  {fmt_tokens(used):>8}  {model}"
        if priced_model:
            row += f"  (${mcost:.4f})"
        lines.append(row)
    if priced:
        lines.append(f"  estimated cost: ${cost:.4f}")
    else:
        lines.append("  (no prices known — drop prices.json beside the script for $)")
    return "\n".join(lines)


def select(records, scope, now):
    if scope.startswith("all"):
        return records, "all time"
    today = now.strftime("%Y-%m-%d")
    if scope.startswith("month"):
        prefix = now.strftime("%Y-%m")
    else:
        prefix = today
    chosen = [r for r in records if (r.get("ts") or "").startswith(prefix)]
    label = "today" if prefix == today else prefix
    return chosen, label


def handle(msg, state):
    kind = msg.get("type")
    if kind == "initialize":
        state["user_dir"] = (msg.get("params") or {}).get("user_dir")
        return {"events": ["agent_start", "turn_end"], "commands": ["usage"]}
    if kind == "event":
        params = msg.get("params") or {}
        name = msg.get("name")
        if name == "agent_start":
            state["model"] = params.get("model")
            state["cwd"] = params.get("cwd")
        elif name == "turn_end":
            usage = params.get("usage") or []
            if not usage:
                return
            slots = list(usage) + [0] * (4 - len(usage))
            try:
                with _lock:
                    append_record(
                        state.get("user_dir"),
                        {
                            "ts": datetime.now(timezone.utc)
                            .isoformat(timespec="seconds"),
                            "cwd": state.get("cwd"),
                            "model": state.get("model"),
                            "input": int(slots[0]),
                            "output": int(slots[1]),
                            "cached": int(slots[2]),
                            "cached_write": int(slots[3]),
                            "subagent": bool(os.environ.get("YAK_SUBAGENT_DEPTH")),
                        },
                    )
            except OSError as e:
                sys.stderr.write(f"usage_tracker: {e}\n")
        return
    if kind == "run_command" and msg.get("name") == "usage":
        prices = load_prices()
        scope = (msg.get("args") or "").strip() or "today"
        records = read_records(state.get("user_dir"))
        chosen, label = select(records, scope, datetime.now())
        return report(chosen, prices, label)


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
        except Exception as e:  # accounting must never break a session
            sys.stderr.write(f"usage_tracker: {e}\n")
            reply = None
        if kind == "run_command":
            print(json.dumps({"id": mid, "result": reply}), flush=True)
        elif kind == "initialize":
            print(json.dumps({"id": mid, "result": reply or {}}), flush=True)
        elif kind == "event":
            print(json.dumps({"id": mid, "result": None}), flush=True)


if __name__ == "__main__":
    main()
