#!/usr/bin/env python3
# A `tool_result` subscriber: it receives a tool's full content and may return
# {"content": ...} to replace what the model reads.
#
# This one folds runs of three or more identical consecutive lines in large
# bash/grep output into the line plus a count. Nothing unique is dropped — the
# fold is lossless except for the repetition it names — so a noisy log shrinks
# without hiding the one line that matters. Small results, other tools, and
# anything it cannot actually shrink pass through untouched.
#
# Install into ~/.llm/extensions/ (or .llm/extensions/) and /reload; watch it
# with /status. The thread file keeps every original result, so the model can
# re-run the command if it wants the raw text back.
import json
import sys

# only bother with results big enough for folding to matter
MIN_CHARS = 4000
# a run this long is noise; two identical lines can be meaningful
MIN_RUN = 3


def fold(content):
    """Return (folded_text, lines_folded)."""
    lines = content.split("\n")
    out = []
    folded = 0
    i = 0
    while i < len(lines):
        j = i
        while j + 1 < len(lines) and lines[j + 1] == lines[i]:
            j += 1
        run = j - i + 1
        if run >= MIN_RUN:
            out.append(lines[i])
            out.append(f"[... {run - 1} more identical lines folded ...]")
            folded += run - 1
        else:
            out.extend(lines[i : j + 1])
        i = j + 1
    return "\n".join(out), folded


def handle(msg):
    kind = msg.get("type")
    if kind == "initialize":
        return {"events": ["tool_result"]}
    if kind == "event":
        params = msg.get("params", {})
        content = params.get("content", "")
        if params.get("tool") not in ("bash", "grep") or len(content) < MIN_CHARS:
            return None
        folded_text, folded = fold(content)
        # never hand back something no smaller than what it replaces
        if folded == 0 or len(folded_text) >= len(content):
            return None
        return {"content": folded_text}
    return None


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    if msg.get("type") == "shutdown":
        break
    # every request is answered by id; a null result means "no opinion"
    print(json.dumps({"id": msg.get("id"), "result": handle(msg)}), flush=True)
