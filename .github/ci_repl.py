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
import select
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


def read_until(fd, pattern, timeout=15.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if re.search(pattern, bytes(OUT)):
            return bytes(OUT)
        r, _, _ = select.select([fd], [], [], 0.2)
        if r:
            try:
                OUT.extend(os.read(fd, 4096))
            except OSError:
                break
    raise AssertionError(f"pattern {pattern!r} not seen; tail: {bytes(OUT[-400:])!r}")


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
        os.execve(binary, [binary, "agent", "--yolo", "--no-session"], env)

    # a bare pty.fork pty reports a 0x0 window; give it a real one
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))

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
    time.sleep(1.0)
    prompts = seen.get("prompts") or []
    if not (prompts and prompts[0] == "line one\nline two\nline three"):
        # drain and show what the REPL actually drew, for cross-platform diagnosis
        time.sleep(0.5)
        r, _, _ = select.select([fd], [], [], 0.5)
        if r:
            try:
                OUT.extend(os.read(fd, 8192))
            except OSError:
                pass
        raise AssertionError(
            f"prompt on the wire: {prompts!r}; screen tail: {bytes(OUT[-600:])!r}"
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
    try:
        while True:
            r, _, _ = select.select([fd], [], [], 0.3)
            if not r:
                break
            if not os.read(fd, 4096):
                break
    except OSError:
        pass
    read_until(fd, rb"\x1b\[<u")  # popped on exit
    _, status = os.waitpid(pid, 0)
    assert os.waitstatus_to_exitcode(status) == 0, f"exit code {status}"

    print("repl pty smoke passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
