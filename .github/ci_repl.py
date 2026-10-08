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
import shutil
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
            # the full wire history of the latest request, so a lane can
            # assert what a rebuild kept and dropped
            seen["last_msgs"] = msgs
            # the assembled system prompt, so a lane can assert a
            # commands-dir `system:` really shipped (and did not replace the
            # agent's own guidance)
            for m in msgs:
                if m.get("role") == "system":
                    c = m.get("content")
                    seen["system"] = c if isinstance(c, str) else json.dumps(c)
                    break
        if body.get("model") == "m-ckpt":
            # checkpoint lane: every round writes hello.txt, its content
            # naming the round, so /tree's restore has real bytes to undo.
            # the answer turn is recognized by the conversation ending on
            # this round's tool result (earlier rounds' tool messages stay
            # in the history, so "any tool message" would misfire); the
            # volatile <context> note rides last and is skipped first
            hist = list(msgs)
            if hist and hist[-1].get("role") == "user" \
                    and isinstance(hist[-1].get("content"), str) \
                    and hist[-1]["content"].startswith("<context>"):
                hist = hist[:-1]
            if hist and hist[-1].get("role") == "tool":
                chunks = [
                    {"choices": [{"index": 0, "delta": {"content": "checkpoint round done"}}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                ]
            else:
                prompt = ""
                for m in reversed(hist):
                    c = m.get("content")
                    if m.get("role") == "user" and isinstance(c, str) \
                            and not c.startswith("<context>"):
                        prompt = c
                        break
                chunks = [
                    {"choices": [{"index": 0, "delta": {"tool_calls": [
                        {"index": 0, "id": "call_ckpt", "type": "function",
                         "function": {"name": "write",
                                      "arguments": json.dumps({
                                          "path": "hello.txt",
                                          "content": f"written during: {prompt}\n"})}}]}}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
                ]
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for chunk in chunks:
                self.wfile.write(b"data: " + json.dumps(chunk).encode() + b"\n\n")
            self.wfile.write(b"data: [DONE]\n\n")
            return
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


def install_lane(binary, work, env):
    """Drive `yak install` through both of its pickers on a pty: the scope
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
                f"# --- yak-tool: {stem}\n"
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
        os.execve(binary, [binary, "install", "-l", pkg], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    threading.Thread(target=drain_reader, args=(fd,), daemon=True).start()

    # install mounts everything a package carries: no picker, nothing to
    # answer on the terminal, and -l still decides where the clone lands
    read_until(fd, rb"skills: ")
    time.sleep(0.3)
    screen = out_bytes()
    _, status = os.waitpid(pid, 0)
    assert os.waitstatus_to_exitcode(status) == 0, f"install exited {status}"

    clone = os.path.join(work, ".yak", "pkg", name)
    assert os.path.isdir(clone), f"install did not land in the project (screen {screen[-400:]!r})"
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
                    },
                    "mock-ckpt": {
                        "kind": "openai-compat",
                        "base_url": f"http://127.0.0.1:{PORT}/v1",
                        "api_key": "sk-x",
                        "models": ["m-ckpt"],
                    }
                },
                "models": {"default": "mock/m-a"},
            },
            f,
        )
    env = dict(os.environ, YAK_USER_PATH=user, TERM="xterm-256color")
    # a stub $EDITOR so the ctrl+g round-trip is scriptable
    stub = os.path.join(tempfile.gettempdir(), "yak-ci-editor-stub.sh")
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
    skill_dir = os.path.join(work, ".yak", "skills", "probe")
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
        os.execve(binary, [binary, "--no-session"], env)

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

    # -- /tree branches instead of truncating ------------------------------
    # a second child with a real store (the first runs --no-session),
    # resuming the same thread: two rounds, a /tree jump back to the first
    # turn, then a third. the file must hold the abandoned sibling, the
    # new branch's line must carry the jumped-to turn as its parent, and
    # the wire history after the jump must be the branch's chain only.
    # the model behind this child is m-ckpt: every round writes hello.txt
    # with the round's prompt in it, and the checkpoint extension (the
    # shipped example, installed into the probe user dir with its shadow
    # root pinned under /tmp) must restore the pre-jump bytes when the
    # jump lands — the write lane and the tree lane in one child
    OUT.clear()
    seen.clear()
    OUT_DONE.clear()
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    ckpt = os.path.join(user, "extensions", "workspace_checkpoint.py")
    os.makedirs(os.path.join(user, "extensions"), exist_ok=True)
    with open(os.path.join(here, "examples", "extensions",
                           "workspace_checkpoint.py")) as fsrc, open(ckpt, "w") as fdst:
        fdst.write(fsrc.read())
    os.chmod(ckpt, 0o755)
    ckpt_shadow = tempfile.mkdtemp()
    tree_env = dict(env, YAK_CHECKPOINT_DIR=ckpt_shadow)
    # the resumed root turn predates this lane; its own snapshot is whatever
    # hello.txt held before the child's first round rewrites it
    hello = os.path.join(work, "hello.txt")
    with open(hello, "w") as f:
        f.write("from smoke\n")
    pid2, fd2 = pty.fork()
    if pid2 == 0:
        os.chdir(work)
        os.execve(binary, [binary, "--session", "01localresumeprobe0000000",
                           "-m", "mock-ckpt/m-ckpt"], tree_env)
    fcntl.ioctl(fd2, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    threading.Thread(target=drain_reader, args=(fd2,), daemon=True).start()
    read_until(fd2, rb"local session marker")
    # the resumed history echoes the stored "ok from mock" and a "> " row
    # of its own, so the boot is settled by the real prompt glyph (bold >
    # followed by reset+space; the history row's ">" carries no reset)
    # the key watcher keeps draining stdin while the task's trailing UI
    # (footer, prompt redraw) lands; its stop() joins within ~100ms, so the
    # lane holds a beat after each round's prompt before typing the next
    # line — otherwise the first keystroke is swallowed by design
    read_until(fd2, rb"\x1b\[1m>\x1b\[0m ")
    OUT.clear()
    for label in (b"second round on the tree", b"third round on the tree"):
        time.sleep(0.2)
        send(fd2, label + b"\r")
        read_until(fd2, rb"checkpoint round done")
        read_until(fd2, rb"\x1b\[1m>\x1b\[0m ")
        OUT.clear()
    # the checkpoint extension must have snapshotted before each round; the
    # mock writes are real, so the file now holds the newest round's bytes
    with open(hello) as f:
        assert f.read() == "written during: third round on the tree\n", \
            f"m-ckpt never wrote the round: {open(hello).read()!r}"
    time.sleep(0.2)
    send(fd2, b"/tree\r")
    read_until(fd2, rb"jump to turn")
    # the picker lists every turn in file order, oldest first: the opening
    # selection is the root — the turn this child resumed from
    send(fd2, b"\r")
    read_until(fd2, rb"jumped to an earlier turn")
    read_until(fd2, rb"\x1b\[1m>\x1b\[0m ")
    # the checkpoint restore: two dropped rounds each rewrote hello.txt, so
    # by the time the prompt returns the workspace must hold the bytes the
    # first dropped round found — the pre-lane "from smoke" state, not any
    # dropped round's write. session_before_tree is fire-and-forget: the
    # event was sent, but the extension's writes may still be in flight, so
    # the check holds a short window rather than racing the subprocess
    restored = None
    for _ in range(50):
        with open(hello) as f:
            restored = f.read()
        if restored == "from smoke\n":
            break
        time.sleep(0.1)
    assert restored == "from smoke\n", \
        f"the checkpoint extension did not restore the pre-jump workspace: {restored!r}"
    # the shadow root only ever holds this project's snapshots, each named
    # by a ULID-ordering stamp
    shadow_project = os.listdir(ckpt_shadow)[0]
    assert all(len(s) == 10 for s in
               os.listdir(os.path.join(ckpt_shadow, shadow_project))), \
        "snapshot dirs must be stamp-named"
    OUT.clear()
    time.sleep(0.2)
    send(fd2, b"after the jump\r")
    read_until(fd2, rb"checkpoint round done")
    # the rebuilt wire history: the abandoned siblings must be gone and
    # the root chain intact
    wire = json.dumps(seen.get("last_msgs") or [])
    for gone in ("second round on the tree", "third round on the tree"):
        assert gone not in wire, "the abandoned branch reached the wire"
    assert "local session marker" in wire, "the root left the rebuilt history"
    # the thread file: five lines (the root, two tool rounds of two
    # lines each… the mock's rounds are tool-then-answer, so each user
    # turn persists two lines, plus the branch's own round) — assert the
    # shape by parent edges instead of a brittle count
    lines_f = [json.loads(l) for l in
               open(os.path.join(user, "threads",
                                 "01localresumeprobe0000000.jsonl"))
               if l.strip()]
    assert len(lines_f) == 7, \
        f"the file must hold both branches: {len(lines_f)} lines"
    assert all(t.get("parent") is None for t in lines_f[:-2]), \
        f"pre-jump lines carry no edge: {[t.get('id') for t in lines_f]}"
    # the jump's rounds branch off the root: the first post-jump line
    # carries the root's id as its parent
    assert lines_f[-2].get("parent") == lines_f[0]["id"], \
        f"the jump's round branches off the first turn: {lines_f[-2].get('parent')!r}"
    # and the branch's own round wrote through the restored workspace
    with open(hello) as f:
        assert f.read() == "written during: after the jump\n", \
            "the post-jump round never wrote the file"
    send(fd2, b"\x03")
    time.sleep(0.2)
    send(fd2, b"\x03")
    time.sleep(0.5)
    # the double ctrl-c must take the second child down too; a live one is
    # SIGKILLed so the lane's own exit state stays the assertion
    _, st2 = os.waitpid(pid2, os.WNOHANG)
    if st2 == 0 and not OUT_DONE.wait(3.0):
        os.kill(pid2, 9)
        os.waitpid(pid2, 0)

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
    screen = install_lane(binary, work, env)
    assert b"skills: " in screen, f"layout not recapped: {screen[-500:]!r}"

    print("repl pty smoke passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
