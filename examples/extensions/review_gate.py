#!/usr/bin/env python3
# A second pair of eyes: `/review [focus]` spawns a real yak child over the
# working tree's uncommitted diff — the reviewer shape from other harnesses'
# end-of-turn code review — and returns its verdict as the command result.
#
#   /review                review the whole uncommitted diff
#   /review auth paths     review with a focus note for the child
#
# The child runs `yak --json --no-session --tools read` (read-only: it can
# look at the code around a hunk, never touch anything) using this same
# build via the YAK_BIN the host hands us. Its model is $YAK_REVIEW_MODEL,
# else the session's own model (learned from `agent_start`), so a cheap
# second model can be pinned per project with the env var. The command's
# 120-second deadline bounds the child; the diff itself is capped at
# MAX_DIFF_CHARS and the cap is stated in the prompt, so the review is of
# what the child actually saw.
#
# Verdicts are not persisted or enforced — this is an on-demand review you
# read in the session. For automatic end-of-turn runs, point a workflow
# graph (examples/workflows/review.json) at the same child shape.
#
# Install into ~/.yak/extensions/ or the project's .yak/extensions/, /reload.
import json
import os
import subprocess
import sys

MAX_DIFF_CHARS = 40_000
MAX_DIFF_LINES = 1_500
REVIEW_TIMEOUT = 115  # the host gives a run_command 120s

PROMPT = """Review the following uncommitted diff as a code reviewer. Report:
1. one-line verdict (ship / needs changes),
2. concrete problems, each with the file and hunk it is in,
3. anything questionable worth a human's look.

Be specific and brief; do not restate the diff. Only the following diff is
in scope (it may be truncated — review only what is shown):

```diff
{diff}
```
{focus}"""


def git_diff(cwd):
    """The uncommitted diff, capped; the cap is stated, not silent."""
    try:
        out = subprocess.run(
            ["git", "diff", "HEAD"], cwd=cwd, capture_output=True, text=True,
            timeout=10, stdin=subprocess.DEVNULL,
        )
    except (OSError, subprocess.SubprocessError) as e:
        return None, f"git diff failed: {e}"
    diff = out.stdout
    if not diff.strip():
        return None, "no uncommitted changes to review (working tree is clean)"
    lines = diff.splitlines()
    if len(lines) > MAX_DIFF_LINES:
        diff = "\n".join(lines[:MAX_DIFF_LINES])
    if len(diff) > MAX_DIFF_CHARS:
        diff = diff[:MAX_DIFF_CHARS]
    note = ""
    if len(lines) > MAX_DIFF_LINES or len(out.stdout) > MAX_DIFF_CHARS:
        note = "\n(the diff was truncated — only the shown part is reviewed)"
    return diff + note, None


def review_child(diff, focus, model):
    """One `yak --json` child; the final text of its result is the verdict."""
    argv = [os.environ.get("YAK_BIN") or "yak", "--json", "--no-session", "--tools", "read"]
    if model:
        argv += ["-m", model]
    prompt = PROMPT.format(
        diff=diff,
        focus=f"\nReview focus from the user: {focus}\n" if focus else "",
    )
    argv.append(prompt)
    try:
        out = subprocess.run(
            argv, capture_output=True, text=True, timeout=REVIEW_TIMEOUT,
            stdin=subprocess.DEVNULL,
        )
    except subprocess.TimeoutExpired:
        return "review timed out after {0}s — try a narrower /review focus".format(REVIEW_TIMEOUT)
    except OSError as e:
        return f"cannot run the yak child: {e}"
    # --json prints line-delimited events; the verdict is the result's text
    for line in out.stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "result":
            text = (event.get("text") or "").strip()
            if text:
                return text
    err = (out.stderr or "").strip()
    return "review child produced no result" + (f": {err[:300]}" if err else "")


def handle(msg, state):
    kind = msg.get("type")
    if kind == "initialize":
        return {"events": ["agent_start"], "commands": ["review"]}
    if kind == "event" and msg.get("name") == "agent_start":
        # the session's own model is the child's fallback; a project can pin
        # a cheaper reviewer with YAK_REVIEW_MODEL
        state["model"] = os.environ.get("YAK_REVIEW_MODEL") or (msg.get("params") or {}).get("model")
        return
    if kind == "run_command" and msg.get("name") == "review":
        cwd = (msg.get("context") or {}).get("cwd") or os.getcwd()
        focus = (msg.get("args") or "").strip()
        diff, error = git_diff(cwd)
        if error:
            return error
        sys.stderr.write("review_gate: reviewer child running (up to {0}s)\n".format(REVIEW_TIMEOUT))
        return review_child(diff, focus, state.get("model"))
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
            sys.stderr.write(f"review_gate: {e}\n")
            reply = None
        if kind in ("run_command", "initialize"):
            print(json.dumps({"id": mid, "result": reply}), flush=True)
        elif kind == "event":
            print(json.dumps({"id": mid, "result": None}), flush=True)


if __name__ == "__main__":
    main()
