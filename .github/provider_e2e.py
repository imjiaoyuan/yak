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


if __name__ == "__main__":
    check_responses(os.path.abspath(sys.argv[1]))
