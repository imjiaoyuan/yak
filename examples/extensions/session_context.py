#!/usr/bin/env python3
# Session-start context injection: a small, factual section appended to the
# agent system prompt, so the model starts knowing where it stands instead
# of spending a turn on `git status`. What rides in:
#
#   <session-context>
#   project: yak · git: main, 2 files changed · last commit "…" (3h ago)
#   rules/commit-hygiene.md: <the rule text>
#   </session-context>
#
# The `system` reply key is computed once at session start (that is when the
# host spawns this extension and asks for sections), so the git state is the
# startup snapshot, not a live feed — exactly what a session-start hook in
# other harnesses provides. Project rules come from `.yak/rules/*.md` beside
# the working directory, smallest-first and hard-capped: the section rides
# every request of the session, so a rule file that reads like docs belongs
# in a skill, not here.
#
# Everything degrades silently: not a git repo, no rules dir, or a slow git
# each just shrink the section (a `git` probe is bounded at one second).
# Install into ~/.yak/extensions/ or the project's .yak/extensions/, /reload;
# watch the section with /status.
import json
import os
import subprocess
import sys

# the whole section rides every request: keep it a rounding error next to
# the prompt, not a second context window
MAX_RULES = 3
MAX_RULE_CHARS = 1200
MAX_SECTION_CHARS = 4000
GIT_TIMEOUT = 1.0


def git(args, cwd):
    try:
        out = subprocess.run(
            ["git", *args],
            cwd=cwd, capture_output=True, text=True, timeout=GIT_TIMEOUT,
            stdin=subprocess.DEVNULL,
        )
        if out.returncode != 0:
            return None
        return out.stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return None


def git_line(cwd):
    """`main, 2 files changed · last commit "subject" (age)` — or nothing."""
    branch = git(["rev-parse", "--abbrev-ref", "HEAD"], cwd)
    if branch is None:
        return None
    parts = [branch]
    status = git(["status", "--porcelain"], cwd)
    if status:
        parts[-1] += f", {len(status.splitlines())} files changed"
    else:
        parts[-1] += ", clean"
    commit = git(["log", "-1", "--format=%s"], cwd)
    if commit:
        when = git(["log", "-1", "--format=%cr"], cwd) or ""
        subject = commit.splitlines()[0][:80]
        parts.append(f'last commit "{subject}"' + (f" ({when})" if when else ""))
    return " · ".join(parts)


def rules(cwd):
    """The project's `.yak/rules/*.md`, smallest first, hard-capped."""
    home = os.path.join(cwd, ".yak", "rules")
    try:
        names = sorted(
            (n for n in os.listdir(home) if n.endswith(".md")),
            key=lambda n: os.path.getsize(os.path.join(home, n)),
        )[:MAX_RULES]
    except OSError:
        return []
    out = []
    total = 0
    for name in names:
        path = os.path.join(home, name)
        try:
            with open(path, encoding="utf-8") as f:
                body = f.read(MAX_RULE_CHARS).strip()
        except OSError:
            continue
        if not body:
            continue
        total += len(body)
        if total > MAX_SECTION_CHARS:
            break
        out.append(f"{name}: {body}")
    return out


def section(cwd):
    """The system section, or None when there is nothing to say."""
    lines = []
    git_status = git_line(cwd)
    if git_status:
        lines.append(f"git: {git_status}")
    lines.extend(rules(cwd))
    if not lines:
        return None
    body = "\n".join(lines)
    if len(body) > MAX_SECTION_CHARS:
        body = body[:MAX_SECTION_CHARS] + "\n(context section truncated)"
    return "<session-context>\n" + body + "\n</session-context>"


def handle(msg):
    if msg.get("type") != "initialize":
        return None
    cwd = (msg.get("params") or {}).get("cwd") or os.getcwd()
    system = section(cwd)
    reply = {"events": []}
    if system:
        reply["system"] = system
    else:
        sys.stderr.write("session_context: nothing to inject (no git, no rules)\n")
    return reply


def main():
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("type") == "shutdown":
            return
        if msg.get("type") == "initialize":
            try:
                reply = handle(msg) or {"events": []}
            except Exception as e:
                sys.stderr.write(f"session_context: {e}\n")
                reply = {"events": []}
            print(json.dumps({"id": msg.get("id"), "result": reply}), flush=True)


if __name__ == "__main__":
    main()
