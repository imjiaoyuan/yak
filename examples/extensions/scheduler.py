#!/usr/bin/env python3
# A cron for yak: schedules in `~/.yak/schedules.json` fire standalone
# `yak -p` runs in the background when due, each writing its transcript to
# `~/.yak/scheduler-logs/<name>-<stamp>.log` (0600, like everything else
# here). The shape other harnesses call scheduled tasks.
#
#   [
#     {"name": "nightly-standup", "at": "09:00", "prompt": "summarize yesterday's commits in this repo", "cwd": "~/work/yak"},
#     {"name": "repo-sweep", "every_minutes": 240, "prompt": "check for failing CI artifacts and stale branches"}
#   ]
#
# Two honest limits, both from the wire protocol rather than lack of
# trying: there is no host-ward seam for an extension to inject a prompt
# into a *running* session, so a firing schedule starts a fresh headless
# run instead of nudging the one on screen (make prompts self-contained);
# and schedules are read from the file once a minute, so editing it takes
# effect without a /reload but a running task cannot be recalled.
#
#   /schedule            the list, each with its next fire time
#   /schedule run NAME   fire one now, without waiting for its slot
#   /schedule on|off     pause and resume the whole scheduler
#
# Install into ~/.yak/extensions/ or the project's .yak/extensions/, /reload.
import json
import os
import subprocess
import sys
import threading
import time
from datetime import datetime, timedelta

POLL_SECS = 20
LOG_DIR = "scheduler-logs"


def state_path(user_dir):
    return os.path.join(user_dir or os.path.expanduser("~/.yak"), "schedules.json")


def load_schedules(user_dir):
    try:
        with open(state_path(user_dir), encoding="utf-8") as f:
            entries = json.load(f)
        return entries if isinstance(entries, list) else []
    except (OSError, json.JSONDecodeError):
        return []


def expand(path):
    return os.path.expanduser(path)


def first_due(entry, now):
    """When a schedule first fires after it is loaded."""
    if "every_minutes" in entry:
        return now + timedelta(minutes=float(entry["every_minutes"]))
    at = entry.get("at")  # "HH:MM"
    if at:
        fire = now.replace(hour=int(at[:2]), minute=int(at[3:5]), second=0)
        if fire <= now:
            fire += timedelta(days=1)
        return fire
    return None


def next_due(entry, previous):
    if "every_minutes" in entry:
        return previous + timedelta(minutes=float(entry["every_minutes"]))
    return first_due(entry, previous + timedelta(days=1))


class Scheduler(threading.Thread):
    def __init__(self, user_dir):
        super().__init__(daemon=True)
        self.user_dir = user_dir
        self.enabled = True
        self.next = {}      # name → datetime of the next fire
        self.lock = threading.Lock()
        self.run_now = []   # names requested via /schedule run

    def log(self, line):
        sys.stderr.write(f"scheduler: {line}\n")

    def fire(self, entry, reason):
        now = datetime.now()
        name = entry.get("name") or "unnamed"
        prompt = entry.get("prompt")
        if not prompt:
            self.log(f"{name}: no prompt, skipped")
            return
        cwd = expand(entry["cwd"]) if entry.get("cwd") else os.getcwd()
        log_dir = os.path.join(self.user_dir or os.path.expanduser("~/.yak"), LOG_DIR)
        os.makedirs(log_dir, exist_ok=True)
        stamp = now.strftime("%Y%m%d-%H%M%S")
        log_path = os.path.join(log_dir, f"{name}-{stamp}.log")
        fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        argv = [os.environ.get("YAK_BIN") or "yak", "-p", prompt, "--no-session"]
        if entry.get("model"):
            argv += ["-m", entry["model"]]
        try:
            subprocess.Popen(
                argv, cwd=cwd if os.path.isdir(cwd) else None,
                stdout=fd, stderr=fd, stdin=subprocess.DEVNULL,
                start_new_session=True,
            )
            self.log(f"{name} fired ({reason}); transcript → {log_path}")
        except OSError as e:
            self.log(f"{name} failed to start: {e}")
        finally:
            os.close(fd)

    def run(self):
        while True:
            time.sleep(POLL_SECS)
            now = datetime.now()
            entries = load_schedules(self.user_dir)
            if not self.enabled:
                continue
            with self.lock:
                due_now, self.run_now = self.run_now, []
            for name in due_now:
                for entry in entries:
                    if entry.get("name") == name:
                        self.fire(entry, "manual")
            for entry in entries:
                name = entry.get("name") or "unnamed"
                if name not in self.next:
                    self.next[name] = first_due(entry, now)
                    continue
                if self.next[name] <= now:
                    self.fire(entry, "scheduled")
                    self.next[name] = next_due(entry, self.next[name])

    def schedule_now(self, name):
        with self.lock:
            self.run_now.append(name)


def handle(msg, state):
    if msg.get("type") == "initialize":
        state["user_dir"] = (msg.get("params") or {}).get("user_dir")
        state["scheduler"] = Scheduler(state["user_dir"])
        state["scheduler"].start()
        return {"events": [], "commands": ["schedule"]}
    if msg.get("type") == "run_command" and msg.get("name") == "schedule":
        sched = state["scheduler"]
        entries = load_schedules(state.get("user_dir"))
        args = (msg.get("args") or "").strip()
        if args == "on":
            sched.enabled = True
            return "scheduler resumed"
        if args == "off":
            sched.enabled = False
            return "scheduler paused (running tasks continue; `schedule on` resumes)"
        if args.startswith("run "):
            name = args[4:].strip()
            if not any(e.get("name") == name for e in entries):
                return f"no schedule named {name!r} — edit ~/.yak/schedules.json"
            sched.schedule_now(name)
            return f"{name} firing now (transcript lands in ~/.yak/{LOG_DIR}/)"
        if not entries:
            return (
                "no schedules — add entries to ~/.yak/schedules.json:\n"
                '  [{"name": "daily", "at": "09:00", "prompt": "...", "cwd": "..."}]'
            )
        lines = ["schedules:" + ("" if sched.enabled else " (paused — /schedule on)")]
        for entry in entries:
            name = entry.get("name") or "unnamed"
            cadence = (
                f"every {entry['every_minutes']}m" if "every_minutes" in entry
                else f"daily at {entry.get('at')}"
            )
            nxt = sched.next.get(name)
            when = f", next {nxt:%Y-%m-%d %H:%M}" if nxt else ""
            lines.append(f"  {name} ({cadence}{when}): {str(entry.get('prompt'))[:80]}")
        return "\n".join(lines)
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
            sys.stderr.write(f"scheduler: {e}\n")
            reply = None
        if kind in ("run_command", "initialize"):
            print(json.dumps({"id": mid, "result": reply}), flush=True)


if __name__ == "__main__":
    main()
