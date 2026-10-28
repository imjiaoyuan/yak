"""Hermetic native-provider regression: real CLI and tools against local mocks."""

import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading


def check_responses(binary):
    failures = []
    requests = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            try:
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                requests.append(body)
                assert self.path == "/v1/responses", self.path
                assert self.headers["Authorization"] == "Bearer local-test-key"
                assert self.headers.get("chatgpt-account-id") is None
                assert self.headers.get("originator") is None
                assert "reasoning" not in body and "text" not in body
                assert body["store"] is False
                assert all(t["strict"] is False for t in body["tools"])
                outputs = [i for i in body["input"] if i.get("type") == "function_call_output"]
                if not outputs:
                    output = [
                        {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "opaque"},
                        {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": json.dumps({"path": "sample.txt"})},
                    ]
                else:
                    assert "fixture contents" in outputs[-1]["output"], outputs
                    assert any(i.get("encrypted_content") == "opaque" for i in body["input"])
                    output = [{"type": "message", "content": [{"type": "output_text", "text": "responses final"}]}]
                response = {"id": "resp_1", "status": "completed", "output": output,
                            "usage": {"input_tokens": 20, "output_tokens": 4, "input_tokens_details": {"cached_tokens": 8}}}
                if body["stream"]:
                    frames = []
                    if outputs:
                        frames.append({"type": "response.output_text.delta", "output_index": 0, "delta": "responses "})
                    frames.append({"type": "response.completed", "response": response})
                    payload = "".join("data: " + json.dumps(f) + "\n\n" for f in frames).encode()
                    content_type = "text/event-stream"
                else:
                    payload = json.dumps(response).encode()
                    content_type = "application/json"
                self.send_response(200)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
            except Exception as error:
                failures.append(repr(error))
                self.send_error(400, str(error))

    server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="yak-responses-ci-") as tmp:
            with open(os.path.join(tmp, "sample.txt"), "w") as f:
                f.write("fixture contents\n")
            with open(os.path.join(tmp, "config.json"), "w") as f:
                json.dump({"providers": {"mock": {"kind": "responses", "base_url": f"http://127.0.0.1:{server.server_port}/v1", "api_key": "local-test-key", "models": ["local-model"]}}, "models": {"default": "mock/local-model"}}, f)
            for flags in ([], ["--no-stream"]):
                before = len(requests)
                result = subprocess.run([binary, "--json", "--no-session", *flags, "read sample.txt"],
                                        cwd=tmp, env=dict(os.environ, YAK_USER_PATH=tmp), stdin=subprocess.DEVNULL,
                                        capture_output=True, text=True, timeout=20)
                assert result.returncode == 0, (result.stdout, result.stderr, failures)
                events = [json.loads(line) for line in result.stdout.splitlines()]
                assert events[-1]["text"] == "responses final", events
                assert "".join(e["text"] for e in events if e["type"] == "text") == "responses final"
                assert len(requests) == before + 2, requests
                assert all(r["stream"] == (not flags) for r in requests[before:])
            assert not failures, failures
    finally:
        server.shutdown()
        server.server_close()
    print("responses provider passed (API key, streaming, JSON, tools and encrypted reasoning replay)")


def check_gemini(binary):
    failures = []
    requests = []
    signed_call = {"functionCall": {"name": "read", "args": {"path": "sample.txt"}}, "thoughtSignature": "call-signature"}
    signed_tail = {"text": "", "thoughtSignature": "tail-signature"}

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            try:
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                requests.append(body)
                streaming = self.path.endswith(":streamGenerateContent?alt=sse")
                assert self.path in ("/v1beta/models/gemini-local:generateContent", "/v1beta/models/gemini-local:streamGenerateContent?alt=sse")
                assert self.headers["x-goog-api-key"] == "local-test-key"
                assert self.headers.get("Authorization") is None
                assert "parametersJsonSchema" in body["tools"][0]["functionDeclarations"][0]
                assert body["generationConfig"]["candidateCount"] == 1
                parts = [p for c in body["contents"] for p in c["parts"]]
                results = [p["functionResponse"] for p in parts if "functionResponse" in p]
                if not results:
                    output = [signed_call]
                else:
                    assert signed_call in parts, parts
                    assert "id" not in results[-1], results
                    assert "fixture contents" in results[-1]["response"]["output"]
                    output = [{"text": "gemini final"}, signed_tail]
                usage = {"promptTokenCount": 20, "candidatesTokenCount": 4, "thoughtsTokenCount": 3, "cachedContentTokenCount": 8}
                if streaming:
                    frames = [{"candidates": [{"content": {"parts": [p]}}]} for p in output]
                    frames.append({"candidates": [{"finishReason": "STOP"}], "usageMetadata": usage})
                    payload = "".join("data: " + json.dumps(f) + "\n\n" for f in frames).encode()
                    content_type = "text/event-stream"
                else:
                    payload = json.dumps({"candidates": [{"content": {"parts": output}, "finishReason": "STOP"}], "usageMetadata": usage}).encode()
                    content_type = "application/json"
                self.send_response(200)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
            except Exception as error:
                failures.append(repr(error))
                self.send_error(400, str(error))

    server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="yak-gemini-ci-") as tmp:
            with open(os.path.join(tmp, "sample.txt"), "w") as f:
                f.write("fixture contents\n")
            with open(os.path.join(tmp, "config.json"), "w") as f:
                json.dump({"providers": {"mock": {"kind": "gemini", "base_url": f"http://127.0.0.1:{server.server_port}/v1beta", "api_key": "local-test-key", "models": ["gemini-local"]}}, "models": {"default": "mock/gemini-local"}}, f)
            env = dict(os.environ, YAK_USER_PATH=tmp)
            for flags in ([], ["--no-stream"]):
                before = len(requests)
                result = subprocess.run([binary, "--json", *flags, "read sample.txt"], cwd=tmp, env=env,
                                        stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=20)
                assert result.returncode == 0, (result.stdout, result.stderr, failures)
                events = [json.loads(line) for line in result.stdout.splitlines()]
                assert events[-1]["text"] == "gemini final", events
                assert "".join(e["text"] for e in events if e["type"] == "text") == "gemini final"
                assert len(requests) == before + 2, requests
                turn_events = [e for e in events if e["type"] == "turn_end"]
                assert all([e["usage"][k] for k in ("input", "output", "cached")] == [20, 7, 8] for e in turn_events), turn_events
                resumed = subprocess.run([binary, "--json", "-c", *flags, "continue"], cwd=tmp, env=env,
                                         stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=20)
                assert resumed.returncode == 0, (resumed.stdout, resumed.stderr, failures)
                assert signed_tail in [p for c in requests[-1]["contents"] for p in c["parts"]]
            assert not failures, failures
    finally:
        server.shutdown()
        server.server_close()
    print("gemini provider passed (native auth, SSE, JSON, tools, usage and signed history resume)")


if __name__ == "__main__":
    binary = os.path.abspath(sys.argv[1])
    check_responses(binary)
    check_gemini(binary)
