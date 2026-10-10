#!/usr/bin/env python3
# A subscriber to the `approval` and `agent_end` events that raises a desktop
# notification, so a task can be left alone in a background terminal: you hear
# about the approval prompt (and the finished answer) wherever you are, not
# just in the window yak runs in.
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

# how much of the answer's first line rides the notification
SUMMARY_CHARS = 200


def first_line(text):
    """The answer's first non-empty line, capped."""
    for line in (text or "").splitlines():
        line = line.strip()
        if line:
            return line[:SUMMARY_CHARS]
    return "(no text)"


def notify(title, body):
    """Spawn the platform notifier, detached; return an error or None."""
    system = platform.system()
    try:
        if system == "Linux":
            if not shutil.which("notify-send"):
                return "notify-send not found"
            subprocess.Popen(
                ["notify-send", title, body],
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
        if msg.get("name") == "agent_end":
            if params.get("interrupted"):
                return
            error = notify(
                "yak",
                first_line(params.get("final_text") or ""),
            )
        elif msg.get("name") == "approval":
            error = notify(
                "yak — approval requested",
                "{}: {}".format(
                    params.get("tool", "tool"),
                    first_line(params.get("preview") or ""),
                ),
            )
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
