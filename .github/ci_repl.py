"""Interactive REPL smoke over a real pty (unix only; prints a skip on
Windows): an SSE mock answers the agent REPL while the script drives real
keystrokes and checks the wire, the history file and the drawn screen —
multiline keys, kitty CSI-u encoded keys, tab completion and the ctrl+g
editor round-trip. Exit code is nonzero on any assertion failure."""

import sys

if sys.platform == "win32":
    print("repl smoke: skipped (Windows)")
    sys.exit(0)

import fcntl
import http.server
import json
import os
import pty
import re
import struct
import tempfile
import termios
import threading
import time

PORT = 8199
seen = {}


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        msgs = body.get("messages", [])
        if msgs:
            seen.setdefault("prompts", []).append(msgs[-1].get("content", ""))
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        chunk = {"choices": [{"index": 0, "delta": {"content": "ok from mock"}}]}
        self.wfile.write(b"data: " + json.dumps(chunk).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")

    def log_message(self, *a):
        pass


OUT = bytearray()
OUT_lock = threading.Lock()
OUT_DONE = threading.Event()


def drain_reader(fd):
    """Read the pty master forever. The REPL redraws on every keystroke and
    macOS's small tty queues block a child whose reader (this script) falls
    behind — a background drain keeps the smoke's own reads from ever
    starving the child's writes."""
    while True:
        try:
            data = os.read(fd, 65536)
        except OSError:
            break
        if not data:
            break
        with OUT_lock:
            OUT.extend(data)
    OUT_DONE.set()


def out_bytes():
    with OUT_lock:
        return bytes(OUT)


def read_until(fd, pattern, timeout=15.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if re.search(pattern, out_bytes()):
            return out_bytes()
        if OUT_DONE.is_set():
            break
        time.sleep(0.05)
    raise AssertionError(f"pattern {pattern!r} not seen; tail: {out_bytes()[-400:]!r}")


def send(fd, data):
    os.write(fd, data)


def main():
    if not hasattr(termios, "TIOCSWINSZ"):
        print("repl smoke: skipped (no pty on this platform)")
        return 0

    binary = os.path.abspath(sys.argv[1])
    user = tempfile.mkdtemp()
    srv = http.server.HTTPServer(("127.0.0.1", PORT), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    with open(os.path.join(user, "config.json"), "w") as f:
        json.dump(
            {
                "providers": {
                    "mock": {
                        "kind": "openai-compat",
                        "base_url": f"http://127.0.0.1:{PORT}/v1",
                        "api_key": "sk-x",
                        "models": ["m-a"],
                    }
                },
                "models": {"agent": {"model": "mock/m-a"}},
            },
            f,
        )
    env = dict(os.environ, LLM_USER_PATH=user, TERM="xterm-256color")
    # a stub $EDITOR so the ctrl+g round-trip is scriptable
    stub = os.path.join(tempfile.gettempdir(), "llm-ci-editor-stub.sh")
    with open(stub, "w") as f:
        f.write('#!/bin/sh\nprintf \'from editor\\n\' > "$1"\n')
    os.chmod(stub, 0o755)
    env["EDITOR"] = stub

    # the completion target lives in the child's cwd
    work = tempfile.mkdtemp()
    with open(os.path.join(work, "hello.txt"), "w") as f:
        f.write("from smoke\n")

    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(work)
        os.execve(binary, [binary, "--yolo", "--no-session"], env)

    # a bare pty.fork pty reports a 0x0 window; give it a real one
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    threading.Thread(target=drain_reader, args=(fd,), daemon=True).start()

    # -- multiline: ctrl+j, shift+enter bytes, submit ----------------------
    read_until(fd, rb"\x1b\[>1u")  # the kitty push ships with the prompt
    read_until(fd, rb">")
    send(fd, b"line one")
    send(fd, b"\x0a")            # ctrl+j -> newline
    send(fd, b"line two")
    send(fd, b"\x1b[13;2u")      # shift+enter via kitty CSI-u -> newline
    send(fd, b"line three")
    time.sleep(0.3)
    send(fd, b"\r")              # enter submits
    # poll for the POST: pty delivery can lag, so give it a real window
    for _ in range(40):
        time.sleep(0.25)
        if seen.get("prompts"):
            break
    prompts = seen.get("prompts") or []
    if not (prompts and prompts[0] == "line one\nline two\nline three"):
        # the drain thread keeps the screen current; show what the REPL drew
        time.sleep(0.5)
        raise AssertionError(
            f"prompt on the wire: {prompts!r}; screen tail: {out_bytes()[-800:]!r}"
        )
    hist = open(os.path.join(user, "history.jsonl")).read()
    assert "line one\\nline two\\nline three" in hist, f"history: {hist!r}"

    # -- history recall keeps the draft ------------------------------------
    read_until(fd, rb">")
    send(fd, b"\x1b[A")          # up: empty buffer recalls history
    time.sleep(0.3)
    read_until(fd, rb"line three")
    send(fd, b"\x1b[B")          # down past newest: the draft returns
    time.sleep(0.3)
    send(fd, b"\r")
    time.sleep(0.3)

    # -- lone backslash + enter inserts a newline ---------------------------
    read_until(fd, rb">")
    send(fd, b"line four")
    send(fd, b"\\")
    send(fd, b"\r")              # enter after a lone \ -> newline, not submit
    time.sleep(0.3)
    read_until(fd, rb">")        # the continuation row's dim > marker
    send(fd, b"line five")
    send(fd, b"\r")
    time.sleep(1.0)
    prompts = seen.get("prompts") or []
    assert prompts and prompts[-1] == "line four\nline five", \
        f"prompt on the wire: {prompts!r}"
    hist = open(os.path.join(user, "history.jsonl")).read()
    assert "line four\\nline five" in hist, f"history: {hist!r}"

    # -- tab completion on ! shell lines ------------------------------------
    read_until(fd, rb">")
    send(fd, b"!he")
    time.sleep(0.2)
    send(fd, b"\t")
    # command position: a dim listing of bang-prefixed $PATH candidates
    read_until(fd, rb"\r\n\x1b\[2m  !h")
    send(fd, b"\x15")            # ctrl-u clears the line
    time.sleep(0.2)
    send(fd, b"!cat he")
    time.sleep(0.2)
    send(fd, b"\t")
    read_until(fd, rb"!cat hello\.txt")  # the bang survives completion
    send(fd, b"\x15")
    time.sleep(0.2)

    # -- kitty CSI-u encoded keys fold back ---------------------------------
    # esc first, while the double-press window is certainly closed (the
    # last interaction was a successful submit)
    send(fd, b"\x1b[27u")        # kitty plain esc -> interrupt
    read_until(fd, rb"ctrl-c again")
    read_until(fd, rb"\x1b\[\?2004h\x1b\[>1u")  # a fresh prompt cycle
    time.sleep(2.2)               # let the double-press window close
    send(fd, b"one")
    time.sleep(0.2)
    send(fd, b"\x1b[106;5u")     # kitty ctrl+j -> newline
    time.sleep(0.2)
    send(fd, b"two")
    time.sleep(0.2)
    read_until(fd, rb"two")
    send(fd, b"\x1b[99;5u")      # kitty ctrl+c -> interrupt
    read_until(fd, rb"ctrl-c again")
    read_until(fd, rb"\x1b\[\?2004h\x1b\[>1u")
    time.sleep(2.2)               # close the window before the editor lane
    OUT.clear()

    # -- ctrl+g editor round-trip -------------------------------------------
    send(fd, b"scratch")
    time.sleep(0.2)
    send(fd, b"\x07")            # ctrl+g: into $EDITOR and back
    time.sleep(1.0)
    read_until(fd, rb"from editor")
    send(fd, b"\r")
    time.sleep(1.0)
    prompts = seen.get("prompts") or []
    assert prompts and prompts[-1] == "from editor", \
        f"editor round-trip on the wire: {prompts!r}"

    # -- exit: the kitty stack is popped ------------------------------------
    send(fd, b"\x03")
    time.sleep(0.2)
    send(fd, b"\x03")
    time.sleep(0.5)
    if not OUT_DONE.wait(3.0):
        OUT_DONE.wait(2.0)
    read_until(fd, rb"\x1b\[<u")  # popped on exit
    _, status = os.waitpid(pid, 0)
    assert os.waitstatus_to_exitcode(status) == 0, f"exit code {status}"

    print("repl pty smoke passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
