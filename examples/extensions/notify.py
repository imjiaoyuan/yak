#!/usr/bin/env python3
# A subscriber to the `approval` and `agent_end` events that raises a desktop
# notification, so a task can be left alone in a background terminal: you hear
# about the approval prompt (and the finished answer) wherever you are, not
# just in the window yak runs in.
#
# Every payload carries the working directory of the session that raised
# it, and the notification shows it underneath the answer's first line:
# with more than one yak running, that path is what tells the notifications
# apart (which project, which terminal to go back to). Both lines ride the
# body, never the summary: Plasma and friends render the summary bold and
# single-line, and a finished answer is not a headline. Anything past
# SUMMARY_CHARS is cut and marked with an ellipsis rather than clipped
# mid-glyph by the renderer.
#
# Approval pings stay quiet for PING_QUIET_SECS after any ping for the same
# directory: a run of prompts inside one task calls you over once, not once
# per prompt — the prompts themselves still block in the terminal either way.
#
# Both notifications respect the focus gate: while the terminal window is
# focused you are already looking at it, so a notification is noise — quiet
# then, loud once you look away. The payload's "focused" field carries the
# state (an interactive yak assumes focused until the terminal reports a
# change; an older yak or a non-interactive run omits the field, which
# counts as unfocused so notifications keep working). A terminal that never
# reports focus stays quiet — set NOTIFY_WHEN = "always" there.
#
# Payloads reuse the shapes other agents' `notify` wrappers already speak, so an
# existing notify-send/osascript script works unchanged:
#   {"type": "agent-turn-complete", "cwd": ..., "input-messages": [...],
#    "last-assistant-message": "..."}
#   {"type": "approval-requested", "cwd": ..., "tool": ..., "preview": ...}
#
# Install into ~/.yak/extensions/ (or the project's .yak/extensions/), /reload;
# watch it with /status. Delivery degrades silently: no notifier on $PATH, or
# a failed one, costs nothing but a line on the extension's stderr.
import json
import os
import platform
import shutil
import subprocess
import sys
import time

# how much of the answer's first line rides the notification; longer
# lines are cut and marked with an ellipsis
SUMMARY_CHARS = 120

# after an approval ping for one working directory, further approval pings
# for it stay quiet this long (the prompt still blocks in the terminal)
PING_QUIET_SECS = 120

# when notifications may fire: "unfocused" stays quiet while the terminal
# is focused (you are already looking at it), "always" fires regardless.
# The "focused" field rides the event payload; an older yak without it
# counts as unfocused, so notifications never silently stop. A terminal
# that never speaks focus reporting is assumed focused — use "always".
NOTIFY_WHEN = "unfocused"

_last_ping = {}

# a subagent child (`yak --json` spawned by subagent.py) inherits the user
# extension dir, so this extension runs inside it too: its events are a
# child agent's machinery, not work the user submitted, and notifying for
# them doubles every notification with one whose cwd is a temp worktree
# under /tmp. The child carries YAK_SUBAGENT_DEPTH; a hand-run yak has none.
_child = bool(os.environ.get("YAK_SUBAGENT_DEPTH"))


def should_notify(params):
    """The focus gate: quiet while the user is looking at the terminal."""
    if NOTIFY_WHEN == "always":
        return True
    return not params.get("focused", False)


def first_line(text):
    """The answer's first non-empty line, capped with an explicit ellipsis."""
    for line in (text or "").splitlines():
        line = line.strip()
        if line:
            return line[:SUMMARY_CHARS] + ("…" if len(line) > SUMMARY_CHARS else "")
    return "(no text)"


def escape(text):
    """Neutralize the limited markup notify-send bodies are parsed for."""
    return text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def notify(title, body):
    """Spawn the platform notifier, detached; return an error or None."""
    system = platform.system()
    try:
        if system == "Linux":
            if not shutil.which("notify-send"):
                return "notify-send not found"
            subprocess.Popen(
                ["notify-send", escape(title), escape(body)],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
        elif system == "Darwin":
            script = 'display notification {} with title "{}"'.format(
                json.dumps(body), title.replace('"', '\\"'))
            subprocess.Popen(
                ["osascript", "-e", script],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
        elif system == "Windows":
            ps = (
                "[Windows.UI.Notifications.ToastNotificationManager, "
                "Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; "
                "$t = [Windows.UI.Notifications.ToastNotificationManager]::GetDefaultUser()"
                ".GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); "
                "$x = $t.GetElementsByTagName('text'); "
                "$x.Item(0).AppendChild($t.CreateTextNode('{}')) | Out-Null; "
                "$x.Item(1).AppendChild($t.CreateTextNode('{}')) | Out-Null; "
                "[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier"
                "('yak').Show("
                "[Windows.UI.Notifications.ToastNotification]::new($t))"
            ).format(title.replace("'", "''"), body.replace("'", "''"))
            subprocess.Popen(
                ["powershell", "-NoProfile", "-Command", ps],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
                creationflags=0x08000000,  # CREATE_NO_WINDOW
            )
        else:
            return f"no notifier for {system}"
    except OSError as e:
        return str(e)
    return None


def handle(msg):
    kind = msg.get("type")
    if kind == "initialize":
        return {"events": ["agent_end", "approval"]}
    if kind == "event":
        params = msg.get("params") or {}
        # the session's working directory rides under the notification's
        # first line: the one field that says which conversation is talking
        cwd = (params.get("cwd") or "").strip()
        if _child or not should_notify(params):
            return
        now = time.monotonic()
        if msg.get("name") == "agent_end":
            if params.get("interrupted"):
                return
            # the answer's first line on top, the working path below it, both
            # in the body where the renderer keeps them plain-weight
            error = notify(
                "yak",
                "{}\n{}".format(
                    first_line(params.get("final_text") or ""),
                    cwd or "yak",
                ),
            )
        elif msg.get("name") == "approval":
            if now - _last_ping.get(cwd, float("-inf")) < PING_QUIET_SECS:
                return
            error = notify(
                "yak — approval requested",
                "{}\n{}".format(
                    "{}: {}".format(
                        params.get("tool", "tool"),
                        first_line(params.get("preview") or ""),
                    ),
                    cwd or "yak",
                ),
            )
            _last_ping[cwd] = now
        else:
            return
        if error:
            sys.stderr.write(f"notify: {error}\n")


def main():
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind, mid = msg.get("type"), msg.get("id")
        if kind == "initialize":
            reply = handle(msg) or {}
            print(json.dumps({"id": mid, "result": reply}), flush=True)
        elif kind == "event":
            try:
                handle(msg)
            except Exception as e:  # a notifier hiccup must never kill the host
                sys.stderr.write(f"notify: {e}\n")
            print(json.dumps({"id": mid, "result": None}), flush=True)
        elif kind == "shutdown":
            return


if __name__ == "__main__":
    main()
