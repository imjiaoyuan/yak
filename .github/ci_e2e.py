"""Cross-platform end-to-end smoke for CI: an OpenAI-style SSE mock runs
in-process, the freshly built binary resolves a provider from a scratch
config.json, streams a prompt, switches a mode default and reads it back,
then exercises the plugin surfaces (script tool, MCP server over stdio,
a commands-dir subcommand). Exit code is nonzero on any assertion
failure."""

import base64
import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading

PORT = 8123
seen = {}

FAKE_MCP = r'''#!/usr/bin/env python3
import json, sys, os
log = os.environ["FAKE_LOG"]
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    rid = req.get("id")
    if req.get("method") == "initialize":
        result = {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "fake"}}
    elif req.get("method") == "tools/list":
        result = {"tools": [{"name": "echo", "description": "Echo text",
                             "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}]}
    elif req.get("method") == "tools/call":
        with open(log, "a") as f:
            f.write(json.dumps(req["params"]) + "\n")
        result = {"content": [{"type": "text", "text": "echo: " + req["params"]["arguments"]["text"]}]}
    else:
        continue
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "result": result}) + "\n")
    sys.stdout.flush()
'''

SCRIPT_TOOL = """import json, sys
args = json.loads(sys.stdin.readline())
print("script-tool saw: " + args.get("value", "?"))
"""

HOOK_APPEND = """import sys
with open(sys.argv[1], "a") as f:
    f.write(sys.stdin.read() + "\\n")
"""


# one 8x8 red-channel PNG used for every generated image
TINY_PNG = bytes.fromhex(
    "89504e470d0a1a0a0000000d49484452000000080000000808020000004b6d29dc"
    "0000001d4944415478da63f8cfc0f01f0005000106a2a261646265846261640000"
    "000049454e44ae426082"
)


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        if self.path.endswith("/v1/messages"):
            self.handle_anthropic(body)
            return
        if self.path.endswith("/images/generations"):
            seen["image_prompt"] = body.get("prompt")
            seen["image_n"] = body.get("n")
            n = int(body.get("n", 1))
            data = {"data": [{"b64_json": base64.b64encode(TINY_PNG).decode()} for _ in range(n)]}
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(data).encode())
            return
        if self.path.endswith("/audio/speech"):
            seen["speech_input"] = body.get("input")
            self.send_response(200)
            self.send_header("Content-Type", "audio/mpeg")
            self.end_headers()
            self.wfile.write(b"fake-speech-bytes")
            return
        messages = body.get("messages", [])
        seen["auth"] = self.headers.get("Authorization")
        seen["model"] = body.get("model")
        seen["tools"] = [t["function"]["name"] for t in body.get("tools", [])]
        if messages:
            seen["last_prompt"] = messages[-1].get("content", "")
        if body.get("model") == "m-write":
            self.handle_write_tool(body)
            return
        if body.get("tools") and not any(m.get("role") == "tool" for m in messages):
            chunks = [
                {"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "call_1", "type": "function",
                     "function": {"name": "mcp__fake__echo",
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
            ("content_block_start", {"index": 0, "content_block": {"type": "text"}}),
            ("content_block_delta", {"index": 0, "delta": {"type": "text_delta", "text": "ant ok"}}),
            ("message_delta", {"delta": {"stop_reason": "end_turn"},
                               "usage": {"input_tokens": 7, "output_tokens": 2}}),
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
    fake_mcp = os.path.join(work, "fake_mcp.py")
    with open(fake_mcp, "w") as f:
        f.write(FAKE_MCP)
    script_tool = os.path.join(work, "script_tool.py")
    with open(script_tool, "w") as f:
        f.write(SCRIPT_TOOL)
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
                    "openai-image": {
                        "kind": "image",
                        "base_url": f"http://127.0.0.1:{PORT}/v1",
                        "api_key": "sk-ci",
                        "models": ["img-1"],
                    },
                    "openai-tts": {
                        "kind": "tts",
                        "base_url": f"http://127.0.0.1:{PORT}/v1",
                        "api_key": "sk-ci",
                        "models": ["voice-1"],
                    },
                },
                "models": {"prompt": {"model": "mock/m-a"}, "agent": {"model": "mock/m-a"}},
                "tools": {
                    "shout": {
                        "description": "Shout a value",
                        "command": sys.executable,
                        "args": [script_tool],
                    }
                },
                "mcpServers": {
                    "fake": {
                        "command": sys.executable,
                        "args": [fake_mcp],
                        "env": {"FAKE_LOG": fake_log},
                    }
                },
            },
            f,
        )
    os.makedirs(os.path.join(user, "commands"))
    with open(os.path.join(user, "commands", "hello-cmd.md"), "w") as f:
        f.write("---\nmodel: mock/m-a\n---\nSay hello to $input")
    env = dict(os.environ, LLM_USER_PATH=user)

    p = run([binary, "hi"], env, stdin=subprocess.DEVNULL)
    assert p.returncode == 0, f"prompt rc={p.returncode} err={p.stderr[-500:]}"
    assert "ok from mock" in p.stdout, f"unexpected stdout: {p.stdout!r}"
    assert seen.get("auth") == "Bearer sk-ci", f"auth header: {seen.get('auth')!r}"
    assert seen.get("model") == "m-a", f"model: {seen.get('model')!r}"

    s = run([binary, "models", "set", "chat", "mock/m-b", "--thinking", "high"], env,
            stdin=subprocess.DEVNULL)
    assert s.returncode == 0, f"models set rc={s.returncode} err={s.stderr[-500:]}"

    g = run([binary, "models", "get", "chat"], env, stdin=subprocess.DEVNULL)
    assert "mock/m-b" in g.stdout and "high" in g.stdout, f"get: {g.stdout!r}"

    k = run([binary, "models", "key", "mock"], env, stdin=subprocess.DEVNULL)
    assert k.stdout.strip() == "sk-ci", f"key: {k.stdout!r}"

    # plugin lane: the agent mounts the script tool and the MCP server,
    # the model calls mcp__fake__echo, the fake server logs the call and
    # the second round returns the final answer
    a = run([binary, "agent", "--yolo", "--no-session",
             "use the echo tool with text 'hi from model'"], env,
            stdin=subprocess.DEVNULL)
    assert a.returncode == 0, f"agent rc={a.returncode} err={a.stderr[-800:]}"
    assert "shout" in (seen.get("tools") or []), f"script tool not mounted: {seen.get('tools')}"
    assert "mcp__fake__echo" in (seen.get("tools") or []), f"mcp tool not mounted: {seen.get('tools')}"
    assert os.path.exists(fake_log) and "hi from model" in open(fake_log).read(), \
        f"mcp call never reached the server: {open(fake_log).read() if os.path.exists(fake_log) else 'no log'}"
    assert "final answer after tool" in a.stdout + a.stderr, \
        f"final answer missing: {(a.stdout + a.stderr)[-300:]!r}"

    # builtin-tool lane: the model writes a real file through the write
    # tool; round 2 must carry the tool result back as a role:"tool"
    # message paired with the assistant tool_calls turn
    w = run([binary, "agent", "--yolo", "--no-session", "-m", "mock-write/m-write",
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
    an = run([binary, "agent", "--yolo", "--no-session", "-m", "mock-ant/m-ant",
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

    # commands dir: llm hello-cmd world expands $input through the prompt path
    c = run([binary, "hello-cmd", "world"], env, stdin=subprocess.DEVNULL)
    assert c.returncode == 0, f"command rc={c.returncode} err={c.stderr[-500:]}"
    assert seen.get("last_prompt") == "Say hello to world", f"expanded prompt: {seen.get('last_prompt')!r}"

    # chat preset: the tool-less session stamps mode "chat" and logs there
    ch = run([binary, "chat", "hi"], env, stdin=subprocess.DEVNULL)
    assert ch.returncode == 0 and "ok from mock" in ch.stdout + ch.stderr, \
        f"chat rc={ch.returncode} err={ch.stderr[-300:]!r}"
    assert seen.get("tools") == [], f"chat must not send tools: {seen.get('tools')}"
    lg = run([binary, "logs"], env, stdin=subprocess.DEVNULL)
    assert "chat" in lg.stdout + lg.stderr, f"chat section missing: {(lg.stdout + lg.stderr)[:300]!r}"

    # piped stdin is the task; the REPL is never entered without a tty
    import io
    piped = subprocess.run([binary, "agent", "--yolo", "--no-session"],
                           input="piped task text", capture_output=True, text=True,
                           env=env, timeout=120)
    assert piped.returncode == 0 and "final answer after tool" in piped.stdout, \
        f"piped agent rc={piped.returncode} out={piped.stdout[-200:]!r} err={piped.stderr[-200:]!r}"

    # media: images land as numbered files, tts as speech.mp3, overwrite is refused
    med = os.path.join(work, "media")
    img = run([binary, "-m", "openai-image/img-1", "-o", "n=2", "a cat", "--out", med + "/"],
              env, stdin=subprocess.DEVNULL)
    assert img.returncode == 0, f"image rc={img.returncode} err={img.stderr[-400:]}"
    assert os.path.exists(os.path.join(med, "image-1.png")) \
        and os.path.exists(os.path.join(med, "image-2.png")), f"dir images: {sorted(os.listdir(med))}"
    assert seen.get("image_n") == 2, f"n not sent: {seen.get('image_n')!r}"

    one = run([binary, "-m", "openai-image/img-1", "a cat", "--out", os.path.join(work, "cat.png")],
              env, stdin=subprocess.DEVNULL)
    assert one.returncode == 0 and os.path.exists(os.path.join(work, "cat.png"))
    dup = run([binary, "-m", "openai-image/img-1", "a cat", "--out", os.path.join(work, "cat.png")],
              env, stdin=subprocess.DEVNULL)
    assert dup.returncode == 1 and "refusing to overwrite" in dup.stderr, \
        f"overwrite not refused: rc={dup.returncode} err={dup.stderr[-200:]!r}"

    so = subprocess.run(
        [binary, "-m", "openai-image/img-1", "a cat", "--out", "-"],
        capture_output=True, env=env, stdin=subprocess.DEVNULL, timeout=120,
    )
    assert so.returncode == 0 and so.stdout == TINY_PNG, \
        f"stdout image bytes differ: {so.stdout[:20]!r}"

    sp = run([binary, "-m", "openai-tts/voice-1", "say it", "--out", med + "/"],
             env, stdin=subprocess.DEVNULL)
    assert sp.returncode == 0 and os.path.exists(os.path.join(med, "speech.mp3")), \
        f"tts rc={sp.returncode} err={sp.stderr[-300:]}"
    assert open(os.path.join(med, "speech.mp3"), "rb").read() == b"fake-speech-bytes"

    # the typo guard keeps precedence over command/prompt fallback
    t = run([binary, "lgos", "x"], env, stdin=subprocess.DEVNULL)
    assert t.returncode == 2 and "closest match" in t.stderr, f"typo guard: {t.returncode} {t.stderr!r}"

    print("e2e smoke passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
