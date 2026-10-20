#!/usr/bin/env python3
"""remote_rpc — the phone bridge in protocol form.

Where remote.py types replies into a tmux pane (a terminal is the
contract), this gateway owns a `yak --rpc` child and turns every phone
reply into a protocol command: plain text becomes a prompt (a steer while
a task runs), y/n/a answers an approval request the moment it appears,
/stop interrupts. The push channels and the listener are remote.py's own
(Telegram long-poll or IMAP idle), reused here through the same
remote.json — the notification side does not change, only the delivery.

Shape (nothing needs tmux, nothing needs a terminal):

    tmux new -s yak            # or plain ssh, or a systemd unit
    ./remote_rpc.py            # spawns `yak --rpc` with this cwd

Replies follow remote.py's grammar: plain text = the next instruction,
/y /n /a answer an approval, /stop interrupts, /status reports. The [tag]
prefix routes to one of several gateways only when they share a Telegram
bot; otherwise each gateway uses its own chat and needs no tag.

Configuration: remote.json as remote.py reads it (see its header for the
channel keys). The yak binary is `yak` on PATH or YAK_BIN; extra flags
ride YAK_RPC_FLAGS (one shell-word split, e.g. "--model mock/m").
"""

import json
import os
import queue
import shlex
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.realpath(__file__))
sys.path.insert(0, HERE)

# remote.py holds the channel stack (push + listen) as importable pieces;
# importing it runs only its module-level constants, not its main loop
import remote  # noqa: E402


def note(text):
    sys.stderr.write(f"remote_rpc: {text}\n")
    sys.stderr.flush()


def yak_argv():
    """The rpc child: `yak --rpc` with the environment's overrides."""
    binary = os.environ.get("YAK_BIN", "yak")
    argv = [binary, "--rpc"]
    flags = os.environ.get("YAK_RPC_FLAGS", "").strip()
    if flags:
        argv.extend(shlex.split(flags))
    return argv


class RpcChild:
    """One `yak --rpc` subprocess: writes commands, reads events, answers
    approvals. stdout lines land in one queue in arrival order."""

    def __init__(self):
        self.proc = subprocess.Popen(
            yak_argv(), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, encoding="utf-8",
            errors="replace", cwd=os.getcwd())
        self.events = queue.Queue()
        self.pending_approval = None   # the ask waiting on the phone
        self.thread = None            # busy between prompt and result
        self.lock = threading.Lock()
        self.seq = 0
        threading.Thread(target=self._read_stderr, daemon=True).start()
        threading.Thread(target=self._read_stdout, daemon=True).start()

    def _read_stderr(self):
        for line in self.proc.stderr:
            note(line.rstrip())

    def _read_stdout(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                note(f"unparsable event: {line[:120]}")
                continue
            with self.lock:
                kind = event.get("type")
                if kind == "approval_request":
                    self.pending_approval = event
                elif kind == "prompt" or kind == "session":
                    pass
            self.events.put(event)
        self.events.put(None)

    def send(self, command):
        """One protocol command; ids are sequential so acks correlate."""
        with self.lock:
            self.seq += 1
            command = dict(command, id=str(self.seq))
        self.proc.stdin.write(json.dumps(command) + "\n")
        self.proc.stdin.flush()

    def answer_approval(self, answer):
        with self.lock:
            ask = self.pending_approval
            self.pending_approval = None
        if not ask:
            return False
        self.send({"type": "approval_response",
                   "request": ask["id"], "answer": answer})
        return True

    def interrupt(self):
        self.send({"type": "interrupt"})

    def state(self):
        """Ask get_state and wait briefly for its response."""
        with self.lock:
            self.seq += 1
            rid = str(self.seq)
        self.proc.stdin.write(
            json.dumps({"id": rid, "type": "get_state"}) + "\n")
        self.proc.stdin.flush()
        deadline = time.time() + 5
        while time.time() < deadline:
            try:
                event = self.events.get(timeout=0.3)
            except queue.Empty:
                continue
            if event and event.get("id") == rid and event.get("success"):
                return event.get("data") or {}
            if event:
                self.events.put(event)   # not ours: back it goes
        return {}

    def alive(self):
        return self.proc.poll() is None


def phone_deliver(child, text):
    """Route one phone reply against the rpc child. Returns the ack line."""
    text = text.strip()
    if not text:
        return None
    cmd = text[1:].strip().lower() if text.startswith("/") else ""
    if cmd in ("y", "n", "a"):
        if child.answer_approval({"y": "allow", "n": "deny",
                                  "a": "allow_session"}[cmd]):
            return f"✓ approval answered {cmd}"
        return "no approval is waiting"
    if cmd == "stop":
        child.interrupt()
        return "✓ interrupt sent"
    if cmd == "status":
        state = child.state()
        remote.push("status: model {} · {} message(s) · queued {}".format(
            state.get("model") or "(none)",
            state.get("messages", "?"),
            state.get("queued", "?")))
        return "✓ status pushed"
    if cmd:
        return ("unknown command /{cmd} — plain text is the next "
                "instruction; /y /n /a answer an approval, /stop "
                "interrupts, /status reports")
    # plain text: a steer while a task runs, else the next prompt. The
    # protocol treats both the same from the client side; `steer` lands
    # between rounds, `prompt` starts or queues — steer is the intent a
    # mid-task reply carries, so send steer when the run is live. The
    # child answers either with an immediate ack.
    child.send({"type": "steer", "message": text})
    return "✓ queued for yak"


def main():
    child = _CHILD[0]
    if child is None:
        child = RpcChild()
        _CHILD[0] = child
    if not remote.TG_TOKEN and not remote.CFG.get("email"):
        print("remote_rpc needs a listening channel (tg_token or email in "
              "remote.json) — see remote.py's header for the keys")
        return 1
    note("spawned " + " ".join(yak_argv()))

    def push_events():
        """Events worth the phone: approval asks and settled results."""
        while True:
            event = child.events.get()
            if event is None:
                remote.push("yak rpc exited")
                return
            kind = event.get("type")
            if kind == "approval_request":
                remote.push("{}approval requested\n{}\nreply /y /n /a".format(
                    remote.project_tag(),
                    (event.get("preview") or "")[:200]))
            elif kind == "result":
                answer = (event.get("text") or "").strip() or "(no text)"
                if event.get("interrupted"):
                    answer += "\n(interrupted)"
                remote.push("{}done — reply with the next instruction\n\n{}".format(
                    remote.project_tag(), answer))

    threading.Thread(target=push_events, daemon=True).start()
    remote.start_listener()
    note("listening; replies drive the rpc child")
    try:
        while child.alive():
            time.sleep(0.2)
    except KeyboardInterrupt:
        pass
    return 0


# remote.py's listener calls its own deliver() for every reply; when run as
# a module it routes through ANSWERS and its tmux lane — here we replace
# the router wholesale before the listener starts
def _route_reply(text):
    child = _CHILD[0]
    if child is None:
        return "gateway not ready"
    return phone_deliver(child, text) or ""


_CHILD = [None]

_original_deliver = remote.deliver
remote.deliver = lambda text: _route_reply(text)

if __name__ == "__main__":
    # This file is a gateway, not an extension: when something speaks the
    # host protocol at it (the repo's example probe), answer honestly and
    # exit instead of starting the bridge.
    first = sys.stdin.readline().strip()
    if '"initialize"' in first or '"type"' in first:
        try:
            frame = json.loads(first)
            rid = frame.get("id")
        except json.JSONDecodeError:
            rid = None
        print(json.dumps({"id": rid, "result": {
            "tools": [], "commands": [], "events": [],
            "note": "remote_rpc is a standalone gateway: run it directly "
                    "(./remote_rpc.py), not as a yak extension"}}), flush=True)
        sys.exit(0)
    sys.exit(main())
