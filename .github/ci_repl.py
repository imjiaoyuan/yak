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
import subprocess
import tempfile
import termios
import threading
import time

PORT = 8199
seen = {}


def write_thread(user_dir, thread_id, cwd, prompt):
    """One stored turn, shaped like the agent writes it: the resume list
    reads the last line's cwd and prompt."""
    turn = {
        "id": thread_id,
        "ts": "2026-09-11T10:00:00+00:00",
        "mode": "agent",
        "model": "mock/m-a",
        "cwd": cwd,
        "prompt": prompt,
        "response": "ok from mock",
        "options": [],
        "messages": [
            {"role": "user", "text": prompt, "attachments": []},
            {"role": "assistant", "text": "ok from mock", "tool_calls": []},
        ],
    }
    path = os.path.join(user_dir, "threads", thread_id + ".jsonl")
    with open(path, "w") as f:
        f.write(json.dumps(turn) + "\n")


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        msgs = body.get("messages", [])
        if msgs:
            # the agent appends a volatile `<context>…</context>` budget note
            # as the final turn; the real prompt is the message before it
            last = msgs[-1]
            if isinstance(last.get("content"), str) and last["content"].startswith("<context>"):
                last = msgs[-2] if len(msgs) > 1 else last
            seen.setdefault("prompts", []).append(last.get("content", ""))
            # the assembled system prompt, so a lane can assert a
            # commands-dir `system:` really shipped (and did not replace the
            # agent's own guidance)
            for m in msgs:
                if m.get("role") == "system":
                    c = m.get("content")
                    seen["system"] = c if isinstance(c, str) else json.dumps(c)
                    break
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


def install_picker_lane(binary, work, env):
    """Drive `llm install` through both of its pickers on a pty: the scope
    menu and the checkbox selection. The e2e lane installs with flags (it has
    no tty), so the keys — and the keep lists they write — are only real
    here. Returns the recap the picker printed."""
    pkg = tempfile.mkdtemp()
    name = os.path.basename(pkg)
    os.makedirs(os.path.join(pkg, "skills", "demo"))
    with open(os.path.join(pkg, "SKILL.md"), "w") as f:
        f.write("---\nname: wholegit\ndescription: smoke\n---\nbody\n")
    with open(os.path.join(pkg, "skills", "demo", "SKILL.md"), "w") as f:
        f.write("---\nname: demo\ndescription: smoke\n---\nbody\n")
    os.makedirs(os.path.join(pkg, "extensions"))
    for stem in ("hello", "bye"):
        with open(os.path.join(pkg, "extensions", stem), "w") as f:
            f.write(
                "#!/usr/bin/env python3\n"
                f"# --- llm-tool: {stem}\n"
                "# description: smoke\n"
                "# args: text (string) the text\n"
                "# arg-mode: argv\n"
                "import sys\n"
                "print(1)\n"
            )
    for argv in (
        ["git", "init", "-q", "."],
        ["git", "add", "-A"],
        ["git", "-c", "user.email=ci@ci", "-c", "user.name=ci", "commit", "-qm", "pkg"],
    ):
        subprocess.run(argv, cwd=pkg, check=True, capture_output=True)

    OUT.clear()
    OUT_DONE.clear()
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(work)
        os.execve(binary, [binary, "install", pkg], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    threading.Thread(target=drain_reader, args=(fd,), daemon=True).start()

    # scope picker: enter takes the highlighted first row (project-local)
    read_until(fd, rb"install where")
    send(fd, b"\r")
    # item picker: uncheck the first row (the repo-root skill), keep the rest
    read_until(fd, rb"install which of these")
    send(fd, b" ")
    send(fd, b"\r")
    read_until(fd, rb"only skills")
    time.sleep(0.3)
    screen = out_bytes()
    _, status = os.waitpid(pid, 0)
    assert os.waitstatus_to_exitcode(status) == 0, f"install exited {status}"

    clone = os.path.join(work, ".llm", "pkg", name)
    assert os.path.isdir(clone), f"install did not land in the project (screen {screen[-400:]!r})"
    config = subprocess.run(
        ["git", "-C", clone, "config", "--get-regexp", "^llm"],
        capture_output=True, text=True,
    ).stdout
    assert "llm.skills demo" in config, f"unchecked skill not recorded: {config!r}"
    assert "llm.extensions *" in config, f"fully checked extensions must stay open: {config!r}"
    return screen


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
                "models": {"default": "mock/m-a"},
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

    # the completion target lives in the child's cwd. realpath: the child
    # records getcwd()'s resolved spelling (/private/var on macOS) and the
    # resume scoping matches on it
    work = os.path.realpath(tempfile.mkdtemp())
    with open(os.path.join(work, "hello.txt"), "w") as f:
        f.write("from smoke\n")
    # a skill for the /skill:<name> lane: its prompt must carry the skill's
    # own directory, or the references/ a skill points at resolve against cwd
    skill_dir = os.path.join(work, ".llm", "skills", "probe")
    os.makedirs(os.path.join(skill_dir, "references"))
    with open(os.path.join(skill_dir, "SKILL.md"), "w") as f:
        f.write("---\nname: probe\ndescription: smoke\n---\nRead references/x.md.\n")
    with open(os.path.join(skill_dir, "references", "x.md"), "w") as f:
        f.write("# x\n")

    # a commands-dir prompt carrying a frontmatter `system`: `/review <arg>`
    # must ship the substituted body *and* the system line, appended to the
    # agent's own prompt rather than replacing it
    cmds = os.path.join(user, "commands")
    os.makedirs(cmds)
    with open(os.path.join(cmds, "review.md"), "w") as f:
        f.write("---\nsystem: SYSTEM-MARKER about $input\n---\nReview $input\n")

    # two stored conversations: one from this directory, one from another.
    # `/resume` must offer only the first (the reference-shaped scoping).
    os.makedirs(os.path.join(user, "threads"), exist_ok=True)
    write_thread(user, "01localresumeprobe0000000", work, "local session marker")
    write_thread(
        user,
        "01foreignresumeprobe000000",
        os.path.realpath(tempfile.mkdtemp()),
        "foreign session marker",
    )

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

    # -- the answer area is append-only: printed once, never retracted -----
    # the wait spinner may own an empty row (its `\r\x1b[2K` frame lives and
    # dies before the first answer byte); from that byte on nothing may erase
    # and no printed text may be written twice. the stream is paced, so wait
    # for the last byte rather than reading a half-arrived answer
    for _ in range(100):
        if b"ok from mock" in out_bytes():
            break
        time.sleep(0.1)
    screen = out_bytes()
    i = screen.find(b"ok from mock")
    assert i != -1, f"no answer on screen: {screen[-400:]!r}"
    assert screen.count(b"ok from mock") == 1, "the answer was reprinted"
    for bad in (b"\x1b[2K", b"\x1b[1A", b"\x1b[J"):
        assert bad not in screen[i:], (
            f"the settled answer was redrawn ({bad!r}): {screen[i:][:400]!r}"
        )

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

    # -- /skill:<name> ships the skill's directory on the wire --------------
    OUT.clear()
    send(fd, b"/skill:probe\r")
    time.sleep(1.0)
    prompts = seen.get("prompts") or []
    want = f'<skill name="probe" dir="{skill_dir}">'
    assert prompts and want in prompts[-1], \
        f"skill prompt: {prompts[-1] if prompts else None!r} (wanted {want!r})"

    # -- a commands-dir prompt ships its body and frontmatter system --------
    OUT.clear()
    send(fd, b"/review the-diff-arg\r")
    time.sleep(1.0)
    prompts = seen.get("prompts") or []
    assert prompts and prompts[-1] == "Review the-diff-arg", \
        f"commands-dir prompt on the wire: {prompts[-1] if prompts else None!r}"
    system = seen.get("system") or ""
    assert "SYSTEM-MARKER about the-diff-arg" in system, \
        f"frontmatter system missing from the wire: {system[:300]!r}"
    assert "update_plan" in system, \
        "a command's system must append to the agent prompt, not replace it"

    # -- /resume lists this directory's conversations only -------------------
    OUT.clear()
    send(fd, b"/resume")
    time.sleep(0.3)
    send(fd, b"\r")
    read_until(fd, rb"local session marker")
    time.sleep(0.3)
    screen = out_bytes()
    assert b"foreign session marker" not in screen, \
        f"/resume leaked another directory's session: {screen[-800:]!r}"
    send(fd, b"\x1b")             # esc closes the picker
    time.sleep(0.3)
    read_until(fd, rb">")

    # -- /export writes the loaded conversation as markdown -----------------
    OUT.clear()
    send(fd, b"/resume\r")          # the picker comes up
    read_until(fd, rb"enter select")
    send(fd, b"\r")              # a second enter loads the local session
    time.sleep(0.5)
    send(fd, b"/status\r")
    read_until(fd, rb"01localresumeprobe0000000")  # the session is live now
    exported = os.path.join(user, "exported.md")
    send(fd, ("/export " + exported).encode() + b"\r")
    time.sleep(0.8)
    assert os.path.exists(exported), f"no export written; screen: {out_bytes()[-800:]!r}"
    md = open(exported).read()
    assert md.startswith("# local session marker"), f"export: {md[:200]!r}"
    assert "**Assistant**" in md, f"export body: {md[:400]!r}"

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

    # -- install pickers: scope menu, then the checkbox item list -----------
    screen = install_picker_lane(binary, work, env)
    assert b"only skills: demo" in screen, f"selection not recapped: {screen[-500:]!r}"

    print("repl pty smoke passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
