"""Cross-platform end-to-end smoke for CI: an OpenAI-style SSE mock runs
in-process, the freshly built binary resolves a provider from a scratch
config.json, runs one-shot agent tasks, then exercises the extension host
(a user extension registering a tool) and the package commands.
Exit code is nonzero on any assertion failure."""

import http.server
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time

PORT = 8123
seen = {}

# one extension: the host protocol (initialize handshake, one tool)
ECHO_EXT = r"""#!/usr/bin/env python3
import json, sys, os
log = os.environ["ECHO_LOG"]

def reply(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    if req.get("type") == "initialize":
        reply({"id": req["id"], "result": {"tools": [
            {"name": "echo", "description": "Echo the given text",
             "parameters": {"type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"]}}],
            "commands": [], "events": []}})
    elif req.get("type") == "call_tool":
        with open(log, "a") as f:
            f.write(json.dumps(req["args"]) + "\n")
        reply({"id": req["id"], "result": "echo: " + req["args"].get("text", "")})
    elif req.get("type") == "shutdown":
        break
"""

# one extension: a tool_result subscriber that replaces the model-visible
# result (the reducer/redactor seam)
REWRITE_EXT = r"""#!/usr/bin/env python3
import json, sys

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    kind = req.get("type")
    if kind == "initialize":
        reply = {"events": ["tool_result"]}
    elif kind == "shutdown":
        break
    elif kind == "event":
        content = req.get("params", {}).get("content", "")
        reply = {"content": "FOLDED-BY-EXTENSION"} if "NOISE" in content else None
    else:
        reply = None
    sys.stdout.write(json.dumps({"id": req.get("id"), "result": reply}) + "\n")
    sys.stdout.flush()
"""


def write_extension(dir_path, name, body=ECHO_EXT):
    """Write one extension entry into dir_path. Windows has no shebang
    execution or exec bits, so the entry is a .cmd shim over the python
    script there; the stem stays the extension's name either way."""
    if sys.platform == "win32":
        script = os.path.join(dir_path, name + ".py")
        with open(script, "w") as f:
            f.write(body)
        exe = os.path.join(dir_path, name + ".cmd")
        with open(exe, "w") as f:
            f.write(f'@"{sys.executable}" "{script}" %*\r\n')
    else:
        exe = os.path.join(dir_path, name)
        with open(exe, "w") as f:
            f.write(body)
        os.chmod(exe, 0o755)


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.endswith("/models"):
            body = {"data": [{"id": "m-a"}, {"id": "m-b"}]}
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(body).encode())
            return
        self.send_response(404)
        self.end_headers()

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        if self.path.endswith("/v1/messages"):
            self.handle_anthropic(body)
            return
        messages = body.get("messages", [])
        seen["auth"] = self.headers.get("Authorization")
        seen["ua"] = self.headers.get("User-Agent")
        seen["model"] = body.get("model")
        seen["tools"] = [t["function"]["name"] for t in body.get("tools", [])]
        if messages:
            # the agent appends a volatile `<context>…</context>` budget note
            # as the final turn; the real prompt is the message before it
            def prompt_of(msgs):
                last = msgs[-1]
                if isinstance(last.get("content"), str) and last["content"].startswith(
                    "<context>"
                ):
                    last = msgs[-2] if len(msgs) > 1 else last
                return last.get("content", "")

            seen["last_prompt"] = prompt_of(messages)
            seen["last_messages"] = messages
            seen.setdefault("prompts", []).append(prompt_of(messages))
        if body.get("model") == "m-wtw":
            # worktree-writer child: one write whose content comes from the
            # task text ("write: X" → the file says "from X"), so two
            # parallel writers fork the same blob and the second merge
            # must conflict; the answer round rides the tool result back
            msgs = body.get("messages", [])
            hist = list(msgs)
            if hist and hist[-1].get("role") == "tool":
                chunks = [
                    {"choices": [{"index": 0, "delta": {"content": "writer done"}}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                ]
            else:
                prompt = ""
                for m in reversed(hist):
                    c = m.get("content")
                    if m.get("role") == "user" and isinstance(c, str):
                        prompt = c
                        break
                tag = prompt.split("write:")[-1].strip() if "write:" in prompt else "worker"
                args = json.dumps(
                    {"path": "worker.txt", "content": "from %s\n" % tag})
                chunks = [
                    {"choices": [{"index": 0, "delta": {"tool_calls": [
                        {"index": 0, "id": "call_wtw", "type": "function",
                         "function": {"name": "write",
                                      "arguments": args}}]}}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
                ]
            self.sse(chunks)
            return
        if body.get("model") == "m-wt":
            # worktree parent: one subagent call batching two writers over
            # the same file, then the answer round
            if any(m.get("role") == "tool" for m in body.get("messages", [])):
                seen["wt_round2"] = body.get("messages", [])
                chunks = [
                    {"choices": [{"index": 0, "delta": {"content": "parent saw the writers"}}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                ]
            else:
                args = json.dumps({"tasks": [
                    {"agent": "writer", "task": "write: writer one"},
                    {"agent": "writer", "task": "write: writer two"}]})
                chunks = [
                    {"choices": [{"index": 0, "delta": {"tool_calls": [
                        {"index": 0, "id": "call_wt", "type": "function",
                         "function": {"name": "subagent",
                                      "arguments": args}}]}}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
                ]
            self.sse(chunks)
            return
        if seen.get("tools") and any(
            t.get("function", {}).get("name") == "wordcount"
            for t in body.get("tools", [])
        ) and body.get("model") == "m-wc":
            messages = body.get("messages", [])
            if any(m.get("role") == "tool" for m in messages):
                seen["wc_result"] = messages
                chunks = [
                    {"choices": [{"index": 0, "delta": {"content": "counted"}}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                ]
            else:
                chunks = [
                    {"choices": [{"index": 0, "delta": {"tool_calls": [
                        {"index": 0, "id": "call_wc", "type": "function",
                         "function": {"name": "wordcount",
                                      "arguments": '{"text": "four"}'}}]}}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
                ]
            self.sse(chunks)
            return
        if body.get("model") == "m-trunc":
            # a stream that closes without [DONE]: a truncation, not a success
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(b"data: " + json.dumps(
                {"choices": [{"index": 0, "delta": {"content": "half an ans"}}]}
            ).encode() + b"\n\n")
            return
        if body.get("model") == "m-413":
            # a gateway's opaque refusal: nothing in it names the size, so the
            # CLI has to explain it itself — and re-sending the same body
            # cannot help, so it must not retry
            self.send_response(413)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"error": {
                "type": "server_error",
                "code": "server_error",
                "message": "Upstream request failed: response was not valid JSON",
            }}).encode())
            return
        if body.get("model") == "m-429":
            n = seen.get("m429", 0)
            seen["m429"] = n + 1
            if n == 0:
                self.send_response(429)
                self.send_header("Retry-After", "1")
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b'{"error": {"message": "slow down"}}')
                return
            # the second attempt falls through to the normal reply
        if body.get("model") == "m-write":
            self.handle_write_tool(body)
            return
        if body.get("model") == "m-rw":
            # rewrite lane: round 1 reads a noisy file, round 2 (recognized
            # by the role:"tool" result riding back) is where the driver
            # asserts the extension's replacement reached the transcript
            if any(m.get("role") == "tool" for m in messages):
                seen["rw_round2"] = messages
                chunks = [
                    {"choices": [{"index": 0, "delta": {"content": "read the log"}}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                ]
            else:
                chunks = [
                    {"choices": [{"index": 0, "delta": {"tool_calls": [
                        {"index": 0, "id": "call_rw", "type": "function",
                         "function": {"name": "read",
                                      "arguments": '{"path": "big.log"}'}}]}}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
                ]
            self.sse(chunks)
            return
        if body.get("model") == "m-sub":
            # subagent lane: round 1 asks for the subagent tool, round 2 rides
            # back with whatever the child agent answered
            if any(m.get("role") == "tool" for m in messages):
                seen["sub_round2"] = messages
                chunks = [
                    {"choices": [{"index": 0, "delta": {"content": "parent saw the subagent"}}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                ]
            else:
                chunks = [
                    {"choices": [{"index": 0, "delta": {"tool_calls": [
                        {"index": 0, "id": "call_sub", "type": "function",
                         "function": {"name": "subagent",
                                      "arguments": '{"agent": "scout", "task": "count the mocks"}'}}]}}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
                ]
            self.sse(chunks)
            return
        if body.get("tools") and not any(m.get("role") == "tool" for m in messages):
            # each first round's tool list: the subagent lane asserts the
            # child ran with its definition's subset
            seen.setdefault("generic_tools", []).append(
                [t["function"]["name"] for t in body.get("tools", [])])
            chunks = [
                {"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "call_1", "type": "function",
                     "function": {"name": "echo",
                                  "arguments": '{"text": "hi from model"}'}}]}}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
            ]
        elif body.get("tools"):
            chunks = [
                {"choices": [{"index": 0, "delta": {"content": "final answer after tool"}}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
        else:
            chunks = [
                {"choices": [{"index": 0, "delta": {"content": "ok from mock"}}],
                 "usage": {"prompt_tokens": 2, "completion_tokens": 3}},
            ]
        self.sse(chunks)

    def log_message(self, *a):
        pass

    def sse(self, chunks):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for chunk in chunks:
            self.wfile.write(b"data: " + json.dumps(chunk).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")

    def sse_events(self, events):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for name, payload in events:
            self.wfile.write(b"event: " + name.encode() + b"\n")
            self.wfile.write(b"data: " + json.dumps(payload).encode() + b"\n\n")

    def handle_anthropic(self, body):
        """Anthropic Messages lane: a plain-text answer, with every request
        body recorded so the wire shape of the cache breakpoints can be
        asserted from the test driver."""
        seen.setdefault("ant_bodies", []).append(body)
        self.sse_events([
            ("message_start", {"message": {"usage": {"input_tokens": 7}}}),
            ("content_block_start", {"index": 0, "content_block": {"type": "text"}}),
            ("content_block_delta", {"index": 0, "delta": {"type": "text_delta", "text": "ant ok"}}),
            ("message_delta", {"delta": {"stop_reason": "end_turn"},
                               "usage": {"output_tokens": 2}}),
            ("message_stop", {}),
        ])

    def handle_write_tool(self, body):
        """Built-in-tool lane: round 1 asks the model to call the write
        tool; round 2 (recognized by the role:"tool" result riding back in
        the messages) answers, and the driver asserts the pairing."""
        messages = body.get("messages", [])
        if any(m.get("role") == "tool" for m in messages):
            seen["write_round2"] = messages
            self.sse([
                {"choices": [{"index": 0, "delta": {"content": "wrote hello for you"}}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ])
        else:
            seen["write_round1"] = messages
            self.sse([
                {"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "call_w", "type": "function",
                     "function": {"name": "write",
                                  "arguments": '{"path": "hello.txt", "content": "from agent\\n"}'}}]}}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
            ])


def pin_thread_mtimes(user_dir, offsets):
    """Age each thread file by cwd: offsets maps a cwd to seconds relative to
    now, so "newest" is explicit instead of filesystem-granular. Both sides
    are resolved, because a temp path can be a symlink (/var on macOS) while
    the child records `getcwd()`'s resolved spelling."""
    now = time.time()
    offsets = {os.path.realpath(k): v for k, v in offsets.items()}
    thread_dir = os.path.join(user_dir, "threads")
    for name in os.listdir(thread_dir):
        path = os.path.join(thread_dir, name)
        with open(path) as f:
            lines = [ln for ln in f.read().splitlines() if ln.strip()]
        cwd = json.loads(lines[-1]).get("cwd") if lines else None
        key = os.path.realpath(cwd) if cwd else None
        if key in offsets:
            stamp = now + offsets[key]
            os.utime(path, (stamp, stamp))


def run(cmd, env, cwd=None, stdin=None, timeout=120):
    # the CLI writes UTF-8 whatever the platform default is, so the decode is
    # pinned: Windows would otherwise read it through the console code page
    # (cp1252) and kill the reader thread on a multi-byte glyph's byte, which
    # surfaces as a None stdout rather than the actual failure
    #
    # the child gets its own process group (unix) so a timeout can take down
    # the whole tree: the worktree lane spawns grandchildren (a subagent
    # extension, child yaks), and a lone kill() would orphan them holding
    # the very pipes the hung parent was waiting on
    if sys.platform == "win32":
        popen = lambda: subprocess.Popen(
            cmd, stdin=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, encoding="utf-8", errors="replace", env=env, cwd=cwd)
    else:
        popen = lambda: subprocess.Popen(
            cmd, stdin=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, encoding="utf-8", errors="replace", env=env, cwd=cwd,
            start_new_session=True)
    proc = popen()
    try:
        out, err = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        # communicate() discards what the child already wrote, so the timeout
        # path kills first, then drains: a hang leaves a trail in the --json
        # event stream (or stderr) saying where it stopped
        proc.kill()
        try:
            out, err = proc.communicate(timeout=5)  # drains what a kill leaves behind
        except subprocess.TimeoutExpired:
            out, err = None, None
        print("== TIMEOUT after %ss ==\ncmd: %r\n-- stdout tail --\n%s\n-- stderr tail --\n%s"
              % (timeout, cmd, (out or "")[-4000:], (err or "")[-4000:],),
              file=sys.stderr, flush=True)
        # tree take-down: taskkill /T walks the spawn chain on windows, the
        # process group gets the whole session on unix
        try:
            if sys.platform == "win32":
                subprocess.run(["taskkill", "/F", "/T", "/PID", str(proc.pid)],
                               capture_output=True)
            else:
                import signal
                os.killpg(proc.pid, signal.SIGKILL)
        except OSError:
            pass
        raise subprocess.TimeoutExpired(cmd, timeout, output=out, stderr=err)
    return subprocess.CompletedProcess(cmd, proc.returncode, out, err)


HANDSHAKE = '{"id":1,"type":"initialize","params":{}}\n'


def assert_example_handshake(name, out):
    """One line of `initialize` must come back as a well-formed result."""
    lines = [ln for ln in out.stdout.splitlines() if ln.strip()]
    assert lines, f"{name} answered nothing to initialize: {out.stderr[-300:]!r}"
    obj = json.loads(lines[0])
    result = obj.get("result")
    assert obj.get("id") == 1 and isinstance(result, dict), \
        f"{name} handshake is malformed: {lines[0][:200]!r}"
    assert any(k in result for k in ("tools", "commands", "events")), \
        f"{name} advertises nothing: {result!r}"


def probe_examples(ex_dir, env):
    """Every shipped example has to parse and, if it speaks the host
    protocol, answer the handshake. The deeper lanes drive three of them;
    this is the cheap net over all of them, on every platform. A node-backed
    example is skipped when node is missing (or too old for `*.ts`)."""
    node = shutil.which("node")
    node_ts = False
    if node:
        ver = subprocess.run([node, "--version"], capture_output=True, text=True).stdout
        try:
            major, minor = (int(x) for x in ver.strip().lstrip("v").split(".")[:2])
            node_ts = (major, minor) >= (23, 6)  # native type stripping
        except ValueError:
            node_ts = False
    ran, skipped = [], []
    for name in sorted(os.listdir(ex_dir)):
        path = os.path.join(ex_dir, name)
        if not os.path.isfile(path):
            continue
        with open(path, encoding="utf-8") as f:
            body = f.read()
        shebang = body.split("\n", 1)[0]
        if "yak-tool:" in body[:400]:
            # a manifest script tool: argv in, stdout out, no protocol
            out = subprocess.run(
                [sys.executable, "-c",
                 "import sys; compile(open(sys.argv[1], encoding='utf-8').read(),"
                 " sys.argv[1], 'exec')", path],
                capture_output=True, text=True)
            assert out.returncode == 0, \
                f"manifest tool {name} does not parse: {out.stderr[-300:]!r}"
            ran.append(name)
            continue
        if "python" in shebang or name.endswith(".py"):
            argv, why = [sys.executable, path], None
        elif ("node" in shebang or name.endswith(".js")):
            argv, why = ([node, path] if node else None), "node is missing"
        elif name.endswith(".ts"):
            argv, why = ([node, path] if node_ts else None), "node >= 23.6 is missing"
        else:
            argv, why = None, "unknown interpreter"
        if argv is None:
            skipped.append(f"{name} ({why})")
            continue
        out = subprocess.run(argv, input=HANDSHAKE, capture_output=True,
                             text=True, timeout=60, env=env)
        assert_example_handshake(name, out)
        ran.append(name)
    return ran, skipped


def main():
    # absolute: some scenarios run with cwd=work, and a relative binary
    # path would resolve against the child's cwd on posix and vanish
    binary = os.path.abspath(sys.argv[1])
    srv = http.server.HTTPServer(("127.0.0.1", PORT), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()

    user = tempfile.mkdtemp()
    work = tempfile.mkdtemp()
    fake_log = os.path.join(work, "fake.log")

    with open(os.path.join(user, "config.json"), "w") as f:
        json.dump(
            {
                "providers": {
                    "mock": {
                        "kind": "openai-compat",
                        "base_url": f"http://127.0.0.1:{PORT}/v1",
                        "api_key": "sk-ci",
                        "models": ["m-a", "m-b"],
                    },
                    "mock-write": {
                        "kind": "openai-compat",
                        "base_url": f"http://127.0.0.1:{PORT}/v1",
                        "api_key": "sk-ci",
                        "models": ["m-write"],
                    },
                    "mock-ant": {
                        "kind": "anthropic",
                        "base_url": f"http://127.0.0.1:{PORT}",
                        "api_key": "sk-ci",
                        "models": ["m-ant"],
                    },
                    "mock-wc": {
                        "kind": "openai-compat",
                        "base_url": f"http://127.0.0.1:{PORT}/v1",
                        "api_key": "sk-ci",
                        "models": ["m-wc"],
                    },
                    "mock-rw": {
                        "kind": "openai-compat",
                        "base_url": f"http://127.0.0.1:{PORT}/v1",
                        "api_key": "sk-ci",
                        "models": ["m-rw"],
                    },
                    "mock-sub": {
                        "kind": "openai-compat",
                        "base_url": f"http://127.0.0.1:{PORT}/v1",
                        "api_key": "sk-ci",
                        "models": ["m-sub"],
                    },
                    "mock-wt": {
                        "kind": "openai-compat",
                        "base_url": f"http://127.0.0.1:{PORT}/v1",
                        "api_key": "sk-ci",
                        "models": ["m-wt"],
                    },
                    "mock-wtw": {
                        "kind": "openai-compat",
                        "base_url": f"http://127.0.0.1:{PORT}/v1",
                        "api_key": "sk-ci",
                        "models": ["m-wtw"]
                    },
                },
                "models": {"default": "mock/m-a"},
                # the anthropic lane asserts the long prompt-cache lifetime
                # reaches the wire; no other provider takes one
                "agent": {"cache_ttl": "1h"},
            },
            f,
        )
    os.makedirs(os.path.join(user, "extensions"))
    write_extension(os.path.join(user, "extensions"), "echo_ext")
    write_extension(os.path.join(user, "extensions"), "rewriter", REWRITE_EXT)
    # manifest script tool: a plain python file with a comment header; the
    # host spawns it per call and feeds the single argument as argv[1]
    wc = os.path.join(user, "extensions", "wordcount")
    # unix runs the file through its shebang; windows has no shebang
    # execution, so the manifest declares the interpreter explicitly
    interp = "# interpreter: python\n" if sys.platform == "win32" else ""
    with open(wc, "w") as f:
        f.write(
            "#!/usr/bin/env python3\n"
            "# --- yak-tool: wordcount\n"
            "# description: count characters in a text\n"
            "# args: text (string) the text\n"
            "# arg-mode: argv\n"
            + interp +
            "import sys\n"
            "print(len(sys.argv[1]) if len(sys.argv) > 1 else 0)\n"
        )
    if sys.platform != "win32":
        os.chmod(wc, 0o755)

    env = dict(os.environ, YAK_USER_PATH=user, ECHO_LOG=fake_log)

    p = run([binary, "hi"], env, stdin=subprocess.DEVNULL)
    assert p.returncode == 0, f"prompt rc={p.returncode} err={p.stderr[-500:]}"
    assert "final answer after tool" in p.stdout, f"unexpected stdout: {p.stdout!r}"
    assert seen.get("auth") == "Bearer sk-ci", f"auth header: {seen.get('auth')!r}"
    assert (seen.get("ua") or "").startswith("yak/"), \
        f"user-agent must name this client, not ureq: {seen.get('ua')!r}"
    assert seen.get("model") == "m-a", f"model: {seen.get('model')!r}"

    # a bare multi-word prompt joins into one sentence, no word is dropped
    mw = run([binary, "two", "words", "here"], env, stdin=subprocess.DEVNULL)
    assert mw.returncode == 0, f"multi-word rc={mw.returncode} err={mw.stderr[-300:]}"
    assert "two words here" in (seen.get("prompts") or []), \
        f"joined prompt: {seen.get('prompts')!r}"

    # resuming under --no-session is refused, never silently logged
    nl = run([binary, "--no-session", "--session", "x", "hi"], env, stdin=subprocess.DEVNULL)
    out = nl.stdout + nl.stderr
    assert nl.returncode == 1 and "requires a store" in out, \
        f"--no-session --session refusal: rc={nl.returncode} out={out[-300:]!r}"

    # a stream that closes without [DONE] is a truncation error, never a
    # silently completed turn
    tr = run([binary, "-m", "mock/m-trunc", "x"], env, stdin=subprocess.DEVNULL)
    tout = tr.stdout + tr.stderr
    assert tr.returncode == 1 and "completion marker" in tout, \
        f"truncated stream: rc={tr.returncode} out={tout[-300:]!r}"

    # a 413 body names nothing (a gateway wrapper), so the CLI states the body
    # it sent plus the remedy, and never replays it
    o413 = run([binary, "-m", "mock/m-413", "x"], env, stdin=subprocess.DEVNULL)
    t413 = o413.stdout + o413.stderr
    assert o413.returncode == 1 and "HTTP 413" in t413 \
        and "request body was" in t413 and "shrink or drop attachments" in t413, \
        f"413 explanation: rc={o413.returncode} out={t413[-300:]!r}"

    # a 429 with Retry-After is waited out and the turn still completes
    o429 = run([binary, "-m", "mock/m-429", "hello"], env, stdin=subprocess.DEVNULL)
    t429 = o429.stdout + o429.stderr
    assert o429.returncode == 0 and seen.get("m429", 0) >= 2 \
        and "final answer after tool" in t429, \
        f"429 retry: rc={o429.returncode} seen={seen.get('m429')} out={t429[-300:]!r}"

    # the stored default is config: the REPL /model writes the same shape
    cfgpath = os.path.join(user, "config.json")
    cfg = json.load(open(cfgpath))
    cfg["models"] = {"default": "mock/m-b", "thinking": "high"}
    json.dump(cfg, open(cfgpath, "w"))

    # extension lane: the host spawns the extension, mounts its tool, the
    # model calls it, and the second round returns the final answer
    a = run([binary, "--no-session",
             "use the echo tool with text 'hi from model'"], env, cwd=work,
            stdin=subprocess.DEVNULL)
    assert a.returncode == 0, f"agent rc={a.returncode} err={a.stderr[-800:]}"
    assert "echo" in (seen.get("tools") or []), f"extension tool not mounted: {seen.get('tools')}"
    assert os.path.exists(fake_log) and "hi from model" in open(fake_log).read(), \
        f"extension call never ran: {open(fake_log).read() if os.path.exists(fake_log) else 'no log'}"
    assert "final answer after tool" in a.stdout + a.stderr, \
        f"final answer missing: {(a.stdout + a.stderr)[-300:]!r}"

    # tool_result rewrite lane: a subscriber gets the tool's full content and
    # replaces what the model reads (the reducer/redactor seam)
    with open(os.path.join(work, "big.log"), "w") as f:
        f.write("NOISE line\n" * 500)
    rw = run([binary, "--no-session", "-m", "mock-rw/m-rw", "read the log"],
             env, cwd=work, stdin=subprocess.DEVNULL)
    assert rw.returncode == 0, f"rewrite lane rc={rw.returncode} err={rw.stderr[-800:]}"
    tool_msgs = [m for m in (seen.get("rw_round2") or []) if m.get("role") == "tool"]
    assert tool_msgs, f"the tool result never rode back: {seen.get('rw_round2')}"
    assert tool_msgs[0].get("content") == "FOLDED-BY-EXTENSION", \
        f"the extension's replacement must be what the model reads: {tool_msgs[0].get('content')!r}"

    # manifest script tool lane: the host mounts the header-declared tool
    # and runs the script per call (single argument rides as argv[1])
    w = run([binary, "--no-session", "-m", "mock-wc/m-wc",
             "count the text"], env, cwd=work, stdin=subprocess.DEVNULL)
    assert w.returncode == 0, f"wc agent rc={w.returncode} err={w.stderr[-800:]}"
    assert "wordcount" in (seen.get("tools") or []), \
        f"manifest tool not mounted: {seen.get('tools')}"
    round2 = seen.get("wc_result") or []
    tool_msgs = [m for m in round2 if m.get("role") == "tool"]
    assert tool_msgs and "4" in tool_msgs[0].get("content", ""), \
        f"script tool result missing: {json.dumps(round2)[-300:]!r}"
    assert "counted" in w.stdout + w.stderr, \
        f"final answer missing: {(w.stdout + w.stderr)[-300:]!r}"

    # the reference-template lane: the node template in examples/extensions speaks the
    # protocol natively (initialize -> advertised tools), so the reference-style
    # extensions can ride the same host
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    tpl = os.path.join(root, "examples", "extensions", "template.js")
    if shutil.which("node"):
        out = subprocess.run(
            ["node", tpl],
            input='{"id":1,"type":"initialize","params":{}}\n',
            capture_output=True, text=True, timeout=30,
        )
        assert '"tools"' in out.stdout and '"now"' in out.stdout, \
            f"the reference template handshake broke: {out.stdout[:200]!r} {out.stderr[:200]!r}"

    # examples lane: every shipped example parses and every protocol
    # extension answers `initialize` — the net under the per-example lanes
    ex_dir = os.path.join(root, "examples", "extensions")
    ran, skipped = probe_examples(ex_dir, env)
    assert "subagent.py" in ran and "fold_repeats.py" in ran, \
        f"the examples lane lost coverage: ran={ran} skipped={skipped}"
    print(f"examples: {len(ran)} probed, {len(skipped)} skipped {skipped}")

    # package lane: a local git repo installs into .yak/pkg (project), its
    # extension mounts, list/remove work
    pkgrepo = os.path.join(work, "pkgrepo")
    os.makedirs(os.path.join(pkgrepo, "extensions"))
    os.makedirs(os.path.join(pkgrepo, "skills", "demo"))
    write_extension(os.path.join(pkgrepo, "extensions"), "hello")
    # a repo root SKILL.md is itself one skill (the shape standalone skill
    # repos ship: SKILL.md + references/ at the top)
    open(os.path.join(pkgrepo, "SKILL.md"), "w").write(
        "---\nname: wholegit\ndescription: whole repo skill\n---\nbody\n")
    open(os.path.join(pkgrepo, "skills", "demo", "SKILL.md"), "w").write(
        "---\nname: demo\ndescription: skills dir skill\n---\nbody\n")
    subprocess.run(["git", "init", "-q", pkgrepo], check=True)
    subprocess.run(["git", "-C", pkgrepo, "add", "-A"], check=True)
    subprocess.run(["git", "-C", pkgrepo, "-c", "user.email=t@t", "-c",
                    "user.name=t", "commit", "-qm", "pkg"], check=True)
    p = run([binary, "install", "-l", pkgrepo], env, cwd=work,
            stdin=subprocess.DEVNULL)
    assert p.returncode == 0, f"install rc={p.returncode} err={p.stderr[-400:]}"
    out = p.stdout + p.stderr
    assert "skills: wholegit (repo root), demo" in out and "extensions:" in out, \
        f"install did not report the package layout: {out[-400:]!r}"
    ext_name = "hello.cmd" if sys.platform == "win32" else "hello"
    assert os.path.exists(os.path.join(work, ".yak", "pkg", "pkgrepo", "extensions", ext_name))
    ls = run([binary, "list"], env, cwd=work, stdin=subprocess.DEVNULL)
    lsout = ls.stdout + ls.stderr
    assert "pkgrepo" in lsout and "wholegit (repo root)" in lsout, f"list: {lsout[-300:]!r}"
    # a plain re-install refreshes the same clone; the scope flags are the
    # only thing that decides where it lands, so they are exclusive
    p = run([binary, "install", "-l", pkgrepo], env, cwd=work,
            stdin=subprocess.DEVNULL)
    assert p.returncode == 0, f"refresh rc={p.returncode} err={p.stderr[-300:]}"
    assert "skills: wholegit (repo root), demo" in p.stdout + p.stderr, \
        f"refresh lost the layout: {(p.stdout + p.stderr)[-300:]!r}"
    p = run([binary, "install", "-l", "-g", pkgrepo], env, cwd=work,
            stdin=subprocess.DEVNULL)
    assert p.returncode == 2, f"-l and -g must conflict: rc={p.returncode}"
    p = run([binary, "remove", "pkgrepo"], env, cwd=work, stdin=subprocess.DEVNULL)
    assert p.returncode == 0 and not os.path.exists(os.path.join(work, ".yak", "pkg", "pkgrepo")), \
        f"remove rc={p.returncode}"

    # builtin-tool lane: the model writes a real file through the write
    # tool; round 2 must carry the tool result back as a role:"tool"
    # message paired with the assistant tool_calls turn
    w = run([binary, "--no-session", "-m", "mock-write/m-write",
             "create hello.txt"], env, cwd=work, stdin=subprocess.DEVNULL)
    assert w.returncode == 0, f"write agent rc={w.returncode} err={w.stderr[-800:]}"
    hello = os.path.join(work, "hello.txt")
    assert os.path.exists(hello) and open(hello).read() == "from agent\n", \
        f"write tool never landed: {sorted(os.listdir(work))}"
    round2 = seen.get("write_round2") or []
    tool_msgs = [m for m in round2 if m.get("role") == "tool"]
    assert len(tool_msgs) == 1 and "hello.txt" in tool_msgs[0].get("content", ""), \
        f"tool result pairing broke: {json.dumps(round2)[:400]}"
    calls = [m for m in round2 if m.get("role") == "assistant" and m.get("tool_calls")]
    assert calls and calls[-1]["tool_calls"][0]["id"] == "call_w", \
        f"assistant tool_calls turn missing: {json.dumps(round2)[:400]}"
    assert "wrote hello for you" in w.stdout + w.stderr, \
        f"final answer missing: {(w.stdout + w.stderr)[-300:]!r}"

    # anthropic lane: the wire shape of the prompt-cache breakpoints — the
    # agent round pins tools+system behind one cache_control marker and
    # leaves a first-round prompt unmarked; the continued prompt marks the
    # conversation tip once history exists
    an = run([binary, "--no-session", "-m", "mock-ant/m-ant",
              "hi"], env, stdin=subprocess.DEVNULL)
    assert an.returncode == 0 and "ant ok" in an.stdout + an.stderr, \
        f"anthropic agent rc={an.returncode} err={an.stderr[-300:]!r}"
    bodies = seen.get("ant_bodies") or []
    assert bodies, "anthropic request never arrived"
    b_agent = bodies[0]
    sys_blocks = b_agent.get("system")
    assert isinstance(sys_blocks, list) and len(sys_blocks) == 1 \
        and sys_blocks[0].get("cache_control") == {"type": "ephemeral", "ttl": "1h"}, \
        f"system breakpoint missing its lifetime: {json.dumps(b_agent.get('system'))[:200]}"
    assert "write" in [t.get("name") for t in b_agent.get("tools", [])], \
        f"builtin tools missing: {b_agent.get('tools')}"
    assert not any("cache_control" in json.dumps(m) for m in b_agent.get("messages", [])), \
        f"first-round prompt must stay unmarked: {json.dumps(b_agent['messages'])[:300]}"

    p1 = run([binary, "-m", "mock-ant/m-ant", "one"], env, stdin=subprocess.DEVNULL)
    assert p1.returncode == 0, f"anthropic prompt rc={p1.returncode} err={p1.stderr[-300:]!r}"
    assert not any("cache_control" in json.dumps(m)
                   for m in seen["ant_bodies"][1]["messages"]), \
        "one-shot prompt must stay unmarked"

    p2 = run([binary, "-c", "-m", "mock-ant/m-ant", "two"], env, stdin=subprocess.DEVNULL)
    assert p2.returncode == 0, f"anthropic continue rc={p2.returncode} err={p2.stderr[-300:]!r}"
    msgs = seen["ant_bodies"][2]["messages"]
    # the final turn is the volatile budget note; the tip breakpoint sits on
    # the real prompt just before it
    tip = msgs[-2] if len(msgs) > 1 and "<context>" in str(msgs[-1].get("content")) else msgs[-1]
    assert isinstance(tip["content"], list) \
        and tip["content"][-1].get("cache_control", {}).get("type") == "ephemeral", \
        f"conversation tip unmarked: {json.dumps(tip)[:300]}"
    # a task opened on a carried history marks two breakpoints: the tip above,
    # and the anchor naming the prefix the previous request already wrote —
    # the first request of a task would otherwise re-write the whole history
    # at write price instead of reading it back
    marked = [i for i, m in enumerate(msgs) if "cache_control" in json.dumps(m)]
    tip_index = msgs.index(tip)
    assert marked == [tip_index - 1, tip_index], \
        f"the anchor must name the carried prefix: {json.dumps(msgs)[:300]}"
    assert msgs[tip_index - 1]["content"][-1]["cache_control"] == \
        {"type": "ephemeral", "ttl": "1h"}, f"anchor lifetime: {json.dumps(msgs[tip_index - 1])[:300]}"

    # unknown words are agent tasks now, and the agent always sends tools
    ch = run([binary, "chat", "hi"], env, stdin=subprocess.DEVNULL)
    assert ch.returncode == 0 and "final answer after tool" in ch.stdout + ch.stderr, \
        f"chat rc={ch.returncode} err={ch.stderr[-300:]!r}"
    assert "read" in (seen.get("tools") or []), f"agent must send tools: {seen.get('tools')}"

    # --json lane: the same task writes a line-delimited event stream instead
    # of the terminal UI — the contract the subagent example extension parses
    js = run([binary, "--json", "--no-session", "-m", "mock/m-a", "hi"],
             env, stdin=subprocess.DEVNULL)
    assert js.returncode == 0, f"json lane rc={js.returncode} err={js.stderr[-500:]}"
    events = []
    for line in js.stdout.splitlines():
        assert line.startswith("{") and line.endswith("}"), \
            f"the JSON stream must stay parseable, got {line!r}"
        events.append(json.loads(line))
    kinds = [e["type"] for e in events]
    assert "tool_start" in kinds and "tool_end" in kinds, f"no tool events: {kinds}"
    assert "text" in kinds, f"no answer deltas: {kinds}"
    assert kinds[-1] == "result", f"the stream must end with the result: {kinds[-5:]}"
    assert events[-1]["text"] == "final answer after tool", f"result text: {events[-1]!r}"
    assert "\x1b[" not in js.stdout, "no terminal escapes may ride the stream"
    assert all("usage" in e for e in events if e["type"] == "turn_end"), \
        "every turn_end carries a usage key (null when the provider reports none)"
    start = next(e for e in events if e["type"] == "tool_start")
    assert start["name"] == "echo" and "preview" in start, \
        f"tool_start names the tool: {start!r}"
    # the flag replaces the interactive UI, so it needs a task
    nj = run([binary, "--json"], env, stdin=subprocess.DEVNULL)
    assert nj.returncode == 1 and "needs a task" in nj.stdout + nj.stderr, \
        f"--json without a task: rc={nj.returncode} out={(nj.stdout + nj.stderr)[-200:]!r}"

    # subagent lane: examples/extensions/subagent.py mounts a tool that spawns
    # a real child yak (--json, its own tools and system prompt), reads its
    # event stream and hands the conclusion back as the tool result
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    sub_dst = os.path.join(user, "extensions", "subagent")
    if sys.platform == "win32":
        shutil.copyfile(os.path.join(here, "examples", "extensions", "subagent.py"),
                        sub_dst + ".py")
        with open(sub_dst + ".cmd", "w") as f:
            f.write('@"%s" "%s" %%*\r\n' % (sys.executable, sub_dst + ".py"))
    else:
        shutil.copyfile(os.path.join(here, "examples", "extensions", "subagent.py"), sub_dst)
        os.chmod(sub_dst, 0o755)
    os.makedirs(os.path.join(user, "agents"), exist_ok=True)
    with open(os.path.join(user, "agents", "scout.md"), "w") as f:
        f.write("---\nname: scout\ndescription: ci scout\ntools: read, grep\n---\n"
                "You are the CI scout.\n")
    # a legacy codepage is what Windows CI actually runs under; pinning it on
    # unix too makes the lane prove the example survives one everywhere
    sub_env = dict(env, YAK_BIN=os.path.abspath(binary), PYTHONIOENCODING="cp1252")
    sb = run([binary, "--no-session", "-m", "mock-sub/m-sub", "ask the scout"],
             sub_env, cwd=work, stdin=subprocess.DEVNULL)
    assert sb.returncode == 0, f"subagent lane rc={sb.returncode} err={sb.stderr[-800:]}"
    assert "parent saw the subagent" in sb.stdout + sb.stderr, \
        f"the parent never finished: {(sb.stdout + sb.stderr)[-300:]!r}"
    round2 = seen.get("sub_round2") or []
    tool_msgs = [m for m in round2 if m.get("role") == "tool"]
    assert len(tool_msgs) == 1, f"the tool result went missing: {json.dumps(round2)[:400]}"
    result_text = tool_msgs[0].get("content", "")
    assert "[scout]" in result_text and "final answer after tool" in result_text, \
        f"the child's answer must reach the parent: {result_text[:400]!r}"
    assert ["read", "grep"] in (seen.get("generic_tools") or []), \
        f"the child must run with its definition's tool subset: {seen.get('generic_tools')}"

    # worktree lane: the same subagent extension, but the parent runs in a
    # git repo and the child definition names a mutating tool, so the writer
    # must fork a worktree, write there, and merge back. Two parallel
    # writers over one file: the first merge lands, the second conflicts,
    # the conflict aborts clean and the branch survives for the human.
    # git may be absent on a CI box: the lane skips (the mock still sees
    # the subagent call, which the asserts above already cover)
    if shutil.which("git"):
        repo = tempfile.mkdtemp()

        def git(*a):
            return subprocess.run(["git", "-C", repo] + list(a),
                                  capture_output=True, text=True)

        git("init", "-q", "-b", "main")
        git("config", "user.email", "ci@example.invalid")
        git("config", "user.name", "ci")
        with open(os.path.join(repo, "seed.txt"), "w") as f:
            f.write("seed\n")
        git("add", "-A")
        git("commit", "-qm", "seed")
        # probe: the same spawn shape the subagent extension uses (a python
        # grandchild with PIPEs and a communicate deadline) — when the lane
        # degrades to "not a git repo" on windows, this says whether plain
        # git-from-python hangs there too or only the extension's copy does
        probe = subprocess.run(
            [sys.executable, "-c",
             "import subprocess, sys, time;"
             "t=time.time();"
             "p=subprocess.Popen([\"git\",\"-C\",sys.argv[1],\"rev-parse\",\"--show-toplevel\"],"
             "stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);"
             "out,err=p.communicate(timeout=30);"
             "print(\"probe rc=%d in %.1fs out=%r\" % (p.returncode, time.time()-t, out[:80]))",
             repo],
            capture_output=True, text=True, timeout=60)
        print("git-probe:", probe.stdout.strip(), probe.stderr.strip()[:200],
              flush=True)
        with open(os.path.join(user, "agents", "writer.md"), "w") as f:
            f.write("---\nname: writer\ndescription: ci writer\n"
                    "tools: read, write\nmodel: mock-wtw/m-wtw\n---\n"
                    "You are the CI writer.\n")
        wt = run([binary, "--no-session", "--json", "-m", "mock-wt/m-wt",
                  "delegate to the writers"],
                 dict(sub_env), cwd=repo, stdin=subprocess.DEVNULL)
        assert wt.returncode == 0, \
            f"worktree lane rc={wt.returncode} out={wt.stdout[-400:]!r} err={wt.stderr[-400:]!r}"
        summaries = [json.loads(l) for l in wt.stdout.splitlines() if l.strip()]
        ends = [e.get("summary", "") for e in summaries
                if e.get("type") == "tool_end"]
        assert len(ends) == 1, f"expected one subagent call: {len(ends)}"
        # lane frames: the extension streams one rewritten status row per
        # parallel writer while the call runs; the --json stream surfaces
        # each as a `lane` event (the terminal path renders them in place)
        lane_events = [(e.get("lane", ""), e.get("text", "")) for e in summaries
                       if e.get("type") == "lane"]
        lanes_seen = {name for name, _ in lane_events}
        assert len(lanes_seen) == 2, \
            f"two parallel writers must own two lanes: {lane_events!r}"
        assert any("starting" in t for _, t in lane_events), \
            f"a lane must report the spawn: {lane_events!r}"
        assert any("worktree merged" in t or "worktree conflict" in t
                   for _, t in lane_events), \
            f"a lane must report the merge verdict: {lane_events!r}"
        result = ends[0]
        assert "merge merged" in result and "merge conflict" in result, \
            f"one clean merge and one conflict must be reported: {result[:500]!r}"
        assert "CONFLICT (add/add): Merge conflict in worker.txt" in result, \
            f"the conflict must name the file: {result[:500]!r}"
        assert "resolve with git merge yak/subagent-" in result, \
            f"the conflict must leave the branch and name it: {result[:500]!r}"
        # the parent repo: the first writer's bytes landed, no merge state,
        # the conflicting branch kept, its worktree still there for scrutiny
        assert open(os.path.join(repo, "worker.txt")).read().startswith("from "), \
            "the clean merge never landed in the parent repo"
        assert git("rev-parse", "-q", "--verify", "MERGE_HEAD").returncode != 0, \
            "a conflicted merge was left mid-state instead of aborted"
        kept = [l for l in git("branch").stdout.splitlines() if "yak/subagent-" in l]
        assert kept, "the conflicting writer's branch was deleted"
        listed = git("worktree", "list").stdout
        assert "yak/subagent-" in listed, \
            f"the conflicting writer's worktree was deleted: {listed!r}"
        # the round-2 wire history carries the whole verdict: the parent
        # model reads the conflict, not just the conclusion
        wt_round2 = seen.get("wt_round2") or []
        wt_tool = [m for m in wt_round2 if m.get("role") == "tool"]
        assert wt_tool and "merge conflict" in wt_tool[0].get("content", ""), \
            "the merge verdict must reach the parent model"


    # sessions land as thread files in the store
    assert any(f.endswith(".jsonl") for f in os.listdir(os.path.join(user, "threads"))), \
        f"no thread files: {os.listdir(os.path.join(user, 'threads'))}"

    # piped stdin is the task; the REPL is never entered without a tty
    import io
    piped = subprocess.run([binary, "--no-session"],
                           input="piped task text", capture_output=True, text=True,
                           env=env, timeout=120)
    assert piped.returncode == 0 and "final answer after tool" in piped.stdout, \
        f"piped agent rc={piped.returncode} out={piped.stdout[-200:]!r} err={piped.stderr[-200:]!r}"

    # resume scoping: with no history in this directory `-c` falls back to the
    # newest anywhere and says which directory that was; with its own history
    # it stays local even though another directory's session is newer
    here = tempfile.mkdtemp()
    other = tempfile.mkdtemp()
    stray = tempfile.mkdtemp()
    for home, marker in ((here, "alpha marker"), (other, "beta marker")):
        r = run([binary, "-m", "mock/m-a", marker], env, cwd=home,
                stdin=subprocess.DEVNULL)
        assert r.returncode == 0, f"scoped run rc={r.returncode} err={r.stderr[-300:]!r}"
    # pin the thread files' ages so "newest" does not ride the filesystem's
    # mtime granularity: beta (other) is the newest anywhere
    pin_thread_mtimes(user, {here: -5, other: 100})

    fallback = run([binary, "-c", "-m", "mock/m-a", "stray turn"], env,
                   cwd=stray, stdin=subprocess.DEVNULL)
    body = json.dumps(seen.get("last_messages"))
    assert fallback.returncode == 0 and "continuing" in fallback.stderr, \
        f"cross-directory fallback must say so: err={fallback.stderr[-300:]!r}"
    assert "beta marker" in body and "alpha marker" not in body, \
        f"fallback did not take the newest thread: {body[-300:]!r}"

    local = run([binary, "-c", "-m", "mock/m-a", "local turn"], env,
                cwd=here, stdin=subprocess.DEVNULL)
    body = json.dumps(seen.get("last_messages"))
    assert local.returncode == 0 and "continuing" not in local.stderr, \
        f"local continue must stay local: err={local.stderr[-300:]!r}"
    assert "alpha marker" in body and "beta marker" not in body, \
        f"local continue picked the wrong thread: {body[-300:]!r}"

    # export: the working directory's newest thread as markdown — prose,
    # fenced tool calls and results; no flag picks the conversation
    md_path = os.path.join(stray, "exported.md")
    ex = run([binary, "export", md_path], env, cwd=here, stdin=subprocess.DEVNULL)
    assert ex.returncode == 0, \
        f"export rc={ex.returncode} err={(ex.stdout + ex.stderr)[-300:]!r}"
    md = open(md_path).read()
    assert md.startswith("# alpha marker"), f"export title: {md[:120]!r}"
    assert "**User**" in md and "**Assistant**" in md, f"export body: {md[:400]!r}"
    assert "final answer after tool" in md, "the stored answer must be exported"
    # no path: yak-<id>.md lands in the working directory
    dflt = run([binary, "export"], env, cwd=here, stdin=subprocess.DEVNULL)
    assert dflt.returncode == 0, \
        f"default export rc={dflt.returncode} err={dflt.stderr[-300:]!r}"
    named = [f for f in os.listdir(here) if f.startswith("yak-") and f.endswith(".md")]
    assert named, f"no default-named export in {os.listdir(here)}"

    print("e2e smoke passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
