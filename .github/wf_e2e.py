"""Workflow extension e2e (unix): an SSE mock answers agent nodes while a
real pty REPL drives the workflow extension through the extension host —
`/workflow graphs`, a full two-model run, list/inspect, and the human gate
park + answer. Exit code is nonzero on any assertion failure."""

import sys

if sys.platform == "win32":
    print("workflow e2e: skipped (Windows; needs a pty)")
    sys.exit(0)

import fcntl
import http.server
import json
import os
import pty
import re
import struct
import subprocess
import tempfile
import termios
import threading
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXT = os.path.join(REPO, "examples", "extensions", "workflow.py")
GRAPHS = os.path.join(REPO, "examples", "workflows")
seen = {"models": [], "tools": [], "prompts": []}


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        model = body.get("model")
        seen["models"].append(model)
        seen["tools"].append([t["function"]["name"] for t in body.get("tools", [])])
        msgs = body.get("messages", [])
        prompt = ""
        for m in reversed(msgs):
            if m.get("role") == "user" and isinstance(m.get("content"), str):
                prompt = m["content"]
                break
        seen["prompts"].append(prompt)
        if model == "smart":
            if "Draft a short implementation plan" in prompt:
                text = '```json\n{"plan": "step one then step two"}\n```'
            elif "closing sentence" in prompt:
                text = '```json\n{"result": "declined without executing"}\n```'
            else:
                text = '```json\n{"review": {"ok": true, "issues": []}}\n```'
        elif "Execute the plan" in prompt:
            text = '```json\n{"result": "executed the plan"}\n```'
        else:
            text = '```json\n{"summary": "all good"}\n```'
        chunks = [
            {"choices": [{"index": 0, "delta": {"content": text}}]},
            {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
        ]
        payload = "".join("data: " + json.dumps(c) + "\n\n" for c in chunks).encode()
        payload += b"data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


OUT = bytearray()
OUT_lock = threading.Lock()


def drain_reader(fd):
    """Read the pty master forever (macOS's small tty queues block a child
    whose reader falls behind)."""
    while True:
        try:
            data = os.read(fd, 65536)
        except OSError:
            break
        if not data:
            break
        with OUT_lock:
            OUT.extend(data)


def out_bytes():
    with OUT_lock:
        return bytes(OUT)


def read_until(fd, pattern, timeout=90):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if re.search(pattern, out_bytes()):
            return out_bytes()
        time.sleep(0.05)
    raise AssertionError("timeout waiting for %r; screen tail: %r" % (pattern, out_bytes()[-600:]))


def send(fd, data):
    os.write(fd, data)


def main():
    binary = os.path.abspath(sys.argv[1])
    server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_port
    threading.Thread(target=server.serve_forever, daemon=True).start()
    tmp = tempfile.mkdtemp(prefix="wf-e2e-")
    user, work = os.path.join(tmp, "user"), os.path.join(tmp, "work")
    os.makedirs(os.path.join(user, "extensions"))
    os.makedirs(work)
    exe = os.path.join(user, "extensions", "workflow")
    with open(exe, "w", encoding="utf-8") as f:
        f.write(open(EXT, encoding="utf-8").read())
    os.chmod(exe, 0o755)
    os.makedirs(os.path.join(user, "workflows"))
    for name in os.listdir(GRAPHS):
        if name.endswith(".json"):
            with open(os.path.join(GRAPHS, name)) as src, \
                 open(os.path.join(user, "workflows", name), "w") as dst:
                dst.write(src.read())
    with open(os.path.join(user, "config.json"), "w") as f:
        json.dump({
            "providers": {
                "mock": {"kind": "openai-compat", "base_url": f"http://127.0.0.1:{port}/v1",
                         "api_key": "sk-wf", "models": ["smart", "fast"]},
            },
            "models": {"default": "mock/fast"},
            "aliases": {"smart": "mock/smart", "fast": "mock/fast"},
        }, f)
    env = {**os.environ, "YAK_USER_PATH": user}
    env.pop("YAK_BIN", None)

    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(work)
        os.execve(binary, [binary], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    threading.Thread(target=drain_reader, args=(fd,), daemon=True).start()

    def check(cond, msg, extra=b""):
        if not cond:
            print("FAIL:", msg, extra[-600:] if isinstance(extra, bytes) else extra)
            sys.exit(1)
        print("ok:", msg)

    try:
        read_until(fd, rb">")

        # graphs: the extension answered through the host
        send(fd, b"/workflow graphs\r")
        screen = read_until(fd, rb"review v1")
        check(b"gate" in screen and b"pipeline" in screen, "graphs lists the sample graphs", screen)

        # a full two-model run of the review graph
        seen["models"].clear()
        send(fd, b"/workflow run review {\"task\": \"check the login fix\"}\r")
        screen = read_until(fd, rb"run .{0,30}completed", timeout=300)
        check(seen["models"][:2] == ["smart", "fast"],
              "nodes used their role models in order", str(seen["models"]))
        check(seen["tools"][0] == ["read", "bash"],
              "agent node mounted its tool subset", str(seen["tools"]))

        # list + inspect
        send(fd, b"/workflow list\r")
        screen = read_until(fd, rb"completed\s+review")
        rid = re.search(rb"(01[0-9a-z]{20,26}|1m[0-9a-z]{20,26})", screen[screen.rfind(rb"workflow list"):])
        check(rid is not None, "parsed a run id from list", screen)
        run_id = rid.group(1).decode()
        send(fd, ("/workflow inspect %s\r" % run_id).encode())
        screen = read_until(fd, rb"input tokens")
        check(b"status: completed" in screen, "inspect reports the finished run", screen)

        # the event log is jsonl with the frozen model snapshot
        path = os.path.join(user, "workflows", run_id + ".jsonl")
        check(os.path.exists(path), "the run file exists at <user>/workflows/<id>.jsonl")
        events = [json.loads(l) for l in open(path)]
        check(events[0]["type"] == "graph_start" and events[0]["models"]["review"]["model"] == "mock/smart",
              "graph_start froze the model snapshot", open(path).read()[:300])

        # human gate: park, then answer through the resume
        send(fd, b"/workflow run gate {\"task\": \"ship\"}\r")
        screen = read_until(fd, rb"waits on node approve", timeout=300)
        send(fd, b"/workflow list\r")
        screen = read_until(fd, rb"waiting\s+gate")
        gate_id = re.search(rb"(01[0-9a-z]{20,26}|1m[0-9a-z]{20,26})\s+waiting\s+gate", screen[screen.rfind(rb"workflow list"):]).group(1).decode()
        send(fd, ("/workflow answer %s approve\r" % gate_id).encode())
        screen = read_until(fd, rb"executed the plan", timeout=300)
        check(b"completed" in screen, "the answered gate ran to completion", screen)

        print("workflow e2e passed")
    finally:
        try:
            os.kill(pid, 9)
        except OSError:
            pass
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
