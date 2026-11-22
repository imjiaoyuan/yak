#!/usr/bin/env python3
# Structured compaction support: when a compaction is due, distill the
# tool activity that is about to leave the context into labeled preserve
# blocks, which the host appends to the built-in summary. The `compact`
# seam's first example — and deliberately the `preserved` shape, not the
# `decision: "compact"` replacement: this event's payload carries the
# *dropped tool results*, never the whole history, so an extension that
# claimed the whole summary would be inventing context it never saw.
# `preserved` adds what only the extension knows (the mechanical record of
# what was done) and lets the model call summarize the conversation.
#
# What survives here is the activity digest other harnesses' PostCompact
# summaries carry: which tools ran, how often, and the first line of each
# dropped result (a written path, a command's verdict, a search's hit).
# That line is usually the one fact the summary would otherwise lose.
#
# The path is fail-open like every hook: a slow or dead extension costs
# the host nothing and the built-in summary runs alone.
#
# Install into ~/.yak/extensions/ or the project's .yak/extensions/, /reload.
import json
import sys

# the preserved block rides future requests: a digest, not an archive
MAX_ENTRIES = 40
DIGEST_CHARS = 120


def digest(content):
    """The result's first line, cut with an explicit ellipsis."""
    for line in (content or "").splitlines():
        line = line.strip()
        if line:
            return line[:DIGEST_CHARS] + ("…" if len(line) > DIGEST_CHARS else "")
    return "(empty)"


def preserved_blocks(params):
    dropped = params.get("dropped_tool_results") or []
    if not dropped:
        return []
    lines = []
    counts = {}
    for entry in dropped[:MAX_ENTRIES]:
        tool = entry.get("tool") or "unknown"
        counts[tool] = counts.get(tool, 0) + 1
        lines.append(f"- {tool}: {digest(entry.get('content'))}")
    if len(dropped) > MAX_ENTRIES:
        lines.append(f"- … and {len(dropped) - MAX_ENTRIES} more dropped results")
    counts_line = ", ".join(f"{tool}×{n}" for tool, n in sorted(counts.items()))
    header = (
        f"Tool activity from the compacted prefix ({counts_line}); "
        "first line of each dropped result:"
    )
    return [
        {
            "label": "dropped tool activity (digest)",
            "content": f"{header}\n" + "\n".join(lines),
        }
    ]


def handle(msg):
    if msg.get("type") != "event" or msg.get("name") != "compact":
        return None
    return {"preserved": preserved_blocks(msg.get("params") or {})}


def main():
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind, mid = msg.get("type"), msg.get("id")
        if kind == "shutdown":
            return
        reply = None
        try:
            reply = handle(msg)
        except Exception as e:
            sys.stderr.write(f"compact_summary: {e}\n")
        if kind == "initialize":
            print(
                json.dumps({"id": mid, "result": {"events": ["compact"]}}),
                flush=True,
            )
        elif kind == "event":
            print(json.dumps({"id": mid, "result": reply}), flush=True)


if __name__ == "__main__":
    main()
