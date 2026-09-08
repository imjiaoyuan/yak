"""Cross-platform end-to-end smoke for CI: an OpenAI-style SSE mock runs
in-process, the freshly built binary resolves a provider from a scratch
config.json, runs one-shot agent tasks, then exercises the extension host
(a user extension registering a tool) and the package commands.
Exit code is nonzero on any assertion failure."""

import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading

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

def write_extension(dir_path, name):
    """Write one extension entry into dir_path. Windows has no shebang
    execution or exec bits, so the entry is a .cmd shim over the python
    script there; the stem stays the extension's name either way."""
    if sys.platform == "win32":
        script = os.path.join(dir_path, name + ".py")
        with open(script, "w") as f:
            f.write(ECHO_EXT)
        exe = os.path.join(dir_path, name + ".cmd")
        with open(exe, "w") as f:
            f.write(f'@"{sys.executable}" "{script}" %*\r\n')
    else:
        exe = os.path.join(dir_path, name)
        with open(exe, "w") as f:
            f.write(ECHO_EXT)
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
            seen["last_prompt"] = messages[-1].get("content", "")
            seen.setdefault("prompts", []).append(messages[-1].get("content", ""))
        if body.get("model") == "m-trunc":
            # a stream that closes without [DONE]: a truncation, not a success
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(b"data: " + json.dumps(
                {"choices": [{"index": 0, "delta": {"content": "half an ans"}}]}
            ).encode() + b"\n\n")
            return
        if body.get("model") == "m-write":
            self.handle_write_tool(body)
            return
        if body.get("tools") and not any(m.get("role") == "tool" for m in messages):
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


def run(cmd, env, cwd=None, stdin=None):
    return subprocess.run(
        cmd, capture_output=True, text=True, env=env, cwd=cwd,
        stdin=stdin, timeout=120,
    )


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
                },
                "models": {"prompt": {"model": "mock/m-a"}, "agent": {"model": "mock/m-a"}},
            },
            f,
        )
    os.makedirs(os.path.join(user, "extensions"))
    write_extension(os.path.join(user, "extensions"), "echo_ext")

    env = dict(os.environ, LLM_USER_PATH=user, ECHO_LOG=fake_log)

    p = run([binary, "hi"], env, stdin=subprocess.DEVNULL)
    assert p.returncode == 0, f"prompt rc={p.returncode} err={p.stderr[-500:]}"
    assert "final answer after tool" in p.stdout, f"unexpected stdout: {p.stdout!r}"
    assert seen.get("auth") == "Bearer sk-ci", f"auth header: {seen.get('auth')!r}"
    assert (seen.get("ua") or "").startswith("llm/"), \
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

    # the stored default is config: the REPL /model writes the same shape
    cfgpath = os.path.join(user, "config.json")
    cfg = json.load(open(cfgpath))
    cfg["models"] = {"default": "mock/m-b", "thinking": "high"}
    json.dump(cfg, open(cfgpath, "w"))

    # extension lane: the host spawns the extension, mounts its tool, the
    # model calls it, and the second round returns the final answer
    a = run([binary, "--yolo", "--no-session",
             "use the echo tool with text 'hi from model'"], env, cwd=work,
            stdin=subprocess.DEVNULL)
    assert a.returncode == 0, f"agent rc={a.returncode} err={a.stderr[-800:]}"
    assert "echo" in (seen.get("tools") or []), f"extension tool not mounted: {seen.get('tools')}"
    assert os.path.exists(fake_log) and "hi from model" in open(fake_log).read(), \
        f"extension call never ran: {open(fake_log).read() if os.path.exists(fake_log) else 'no log'}"
    assert "final answer after tool" in a.stdout + a.stderr, \
        f"final answer missing: {(a.stdout + a.stderr)[-300:]!r}"

    # package lane: a local git repo installs into .llm/pkg (project), its
    # extension mounts, list/remove work
    pkgrepo = os.path.join(work, "pkgrepo")
    os.makedirs(os.path.join(pkgrepo, "extensions"))
    write_extension(os.path.join(pkgrepo, "extensions"), "hello")
    subprocess.run(["git", "init", "-q", pkgrepo], check=True)
    subprocess.run(["git", "-C", pkgrepo, "add", "-A"], check=True)
    subprocess.run(["git", "-C", pkgrepo, "-c", "user.email=t@t", "-c",
                    "user.name=t", "commit", "-qm", "pkg"], check=True)
    p = run([binary, "install", "-l", pkgrepo], env, cwd=work,
            stdin=subprocess.DEVNULL)
    assert p.returncode == 0, f"install rc={p.returncode} err={p.stderr[-400:]}"
    assert os.path.exists(os.path.join(work, ".llm", "pkg", "pkgrepo", "extensions", "hello"))
    ls = run([binary, "list"], env, cwd=work, stdin=subprocess.DEVNULL)
    assert "pkgrepo" in ls.stdout + ls.stderr, f"list: {(ls.stdout + ls.stderr)[-300:]!r}"
    p = run([binary, "remove", "pkgrepo"], env, cwd=work, stdin=subprocess.DEVNULL)
    assert p.returncode == 0 and not os.path.exists(os.path.join(work, ".llm", "pkg", "pkgrepo")), \
        f"remove rc={p.returncode}"

    # builtin-tool lane: the model writes a real file through the write
    # tool; round 2 must carry the tool result back as a role:"tool"
    # message paired with the assistant tool_calls turn
    w = run([binary, "--yolo", "--no-session", "-m", "mock-write/m-write",
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
    an = run([binary, "--yolo", "--no-session", "-m", "mock-ant/m-ant",
              "hi"], env, stdin=subprocess.DEVNULL)
    assert an.returncode == 0 and "ant ok" in an.stdout + an.stderr, \
        f"anthropic agent rc={an.returncode} err={an.stderr[-300:]!r}"
    bodies = seen.get("ant_bodies") or []
    assert bodies, "anthropic request never arrived"
    b_agent = bodies[0]
    sys_blocks = b_agent.get("system")
    assert isinstance(sys_blocks, list) and len(sys_blocks) == 1 \
        and sys_blocks[0].get("cache_control", {}).get("type") == "ephemeral", \
        f"system breakpoint missing: {json.dumps(b_agent.get('system'))[:200]}"
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
    assert isinstance(msgs[-1]["content"], list) \
        and msgs[-1]["content"][-1].get("cache_control", {}).get("type") == "ephemeral", \
        f"conversation tip unmarked: {json.dumps(msgs[-1])[:300]}"
    assert not any("cache_control" in json.dumps(m) for m in msgs[:-1]), \
        f"marker leaked onto earlier turns: {json.dumps(msgs)[:300]}"

    # unknown words are agent tasks now, and the agent always sends tools
    ch = run([binary, "chat", "hi"], env, stdin=subprocess.DEVNULL)
    assert ch.returncode == 0 and "final answer after tool" in ch.stdout + ch.stderr, \
        f"chat rc={ch.returncode} err={ch.stderr[-300:]!r}"
    assert "read" in (seen.get("tools") or []), f"agent must send tools: {seen.get('tools')}"

    # sessions land as thread files in the store
    assert any(f.endswith(".jsonl") for f in os.listdir(os.path.join(user, "threads"))), \
        f"no thread files: {os.listdir(os.path.join(user, 'threads'))}"

    # piped stdin is the task; the REPL is never entered without a tty
    import io
    piped = subprocess.run([binary, "--yolo", "--no-session"],
                           input="piped task text", capture_output=True, text=True,
                           env=env, timeout=120)
    assert piped.returncode == 0 and "final answer after tool" in piped.stdout, \
        f"piped agent rc={piped.returncode} out={piped.stdout[-200:]!r} err={piped.stderr[-200:]!r}"

    print("e2e smoke passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
