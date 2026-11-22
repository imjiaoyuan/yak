#!/usr/bin/env python3
# Automatic formatting feedback: after the model writes or edits a file, run
# the file's formatter/linter and append its diagnostics to the tool result
# the model reads — the PostToolUse-additionalContext shape from other
# harnesses, on yak's `tool_result` seam. The model wrote the file; telling
# it what `rustfmt` thinks one tool call later is the cheapest moment to fix
# style, before the drift compounds across the session.
#
# AUTO_FIX = False (default): check-only, never touches the file — the
# diagnostics land in the result and the model decides. Set it to True to
# run the formatter's write mode first; the tool result then says so.
# Either way the thread file keeps the tool's own result untouched: the
# rewrite lives in the transcript the model reads, not in the session log.
#
# Formatters are chosen per extension and skipped silently when the binary
# is not installed. Every probe is bounded at four seconds — the host gives
# a `tool_result` subscriber five — and a timeout or crash leaves the tool
# result exactly as produced (fail-open, like the seam itself).
#
# Install into ~/.yak/extensions/ or the project's .yak/extensions/, /reload.
import json
import os
import shutil
import subprocess
import sys

TOOLS = {"write", "edit"}
AUTO_FIX = False
TIMEOUT = 4.0
MAX_DIAG_CHARS = 1500

# extension → formatters to try, in order; the first installed binary wins
FORMATTERS = {
    ".rs": ["rustfmt"],
    ".py": ["ruff", "black"],
    ".js": ["prettier"],
    ".ts": ["prettier"],
    ".json": ["prettier"],
    ".md": ["prettier"],
    ".css": ["prettier"],
}


def argv_for(tool, path, fix):
    if tool == "rustfmt":
        argv = ["rustfmt", "--edition", "2021"]
        if not fix:
            argv.append("--check")
        return argv + [path]
    if tool == "ruff":
        argv = ["ruff", "format"]
        return argv + (["--check"] if not fix else []) + [path]
    if tool == "black":
        # black writes in place by default; --check only reports
        return ["black", "--quiet" if fix else "--check", path]
    if tool == "prettier":
        return ["prettier", "--write" if fix else "--check", path]
    return None


def lint(path):
    """The note to append to the tool result, or None when all is clean."""
    ext = os.path.splitext(path)[1].lower()
    for tool in FORMATTERS.get(ext, []):
        argv = argv_for(tool, path, AUTO_FIX)
        if not argv or not shutil.which(tool):
            continue
        try:
            out = subprocess.run(
                argv, capture_output=True, text=True, timeout=TIMEOUT,
                stdin=subprocess.DEVNULL,
            )
        except (OSError, subprocess.SubprocessError) as e:
            return f"{tool} failed: {e}"
        diag = (out.stderr or "").strip() or (out.stdout or "").strip()
        if out.returncode != 0 and diag:
            if AUTO_FIX:
                return f"{tool} failed:\n{diag[:MAX_DIAG_CHARS]}"
            return f"{tool} would reformat — diagnostics:\n{diag[:MAX_DIAG_CHARS]}"
        if AUTO_FIX:
            if diag:
                return f"{tool} reformatted {path}; notes:\n{diag[:MAX_DIAG_CHARS]}"
            return f"{tool} reformatted {path}"
        if diag:
            return f"{tool} notes:\n{diag[:MAX_DIAG_CHARS]}"
        return None  # the first installed formatter is authoritative
    return None


def handle(msg):
    if msg.get("type") != "event" or msg.get("name") != "tool_result":
        return None
    params = msg.get("params") or {}
    if params.get("tool") not in TOOLS or params.get("is_error"):
        return None
    path = (params.get("args") or {}).get("path")
    if not path or not os.path.isfile(path):
        return None
    note = lint(path)
    if not note:
        return None
    content = params.get("content") or ""
    return {"content": f"{content}\n\n[autolint] {note}"}


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
        except Exception as e:  # fail-open: the tool result survives untouched
            sys.stderr.write(f"autolint: {e}\n")
        if kind == "initialize":
            print(
                json.dumps({"id": mid, "result": {"events": ["tool_result"]}}),
                flush=True,
            )
        elif kind == "event":
            print(json.dumps({"id": mid, "result": reply}), flush=True)


if __name__ == "__main__":
    main()
