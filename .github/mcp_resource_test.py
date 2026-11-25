import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

BRIDGE = Path(__file__).resolve().parents[1] / "examples" / "extensions" / "mcp_bridge.py"


def fake_server(caps, pages, reads):
    return (
        "import json, sys\n"
        "caps = json.loads(%r)\n"
        "pages = json.loads(%r)\n"
        "reads = json.loads(%r)\n"
        "for line in sys.stdin:\n"
        "    msg = json.loads(line)\n"
        "    method = msg.get('method')\n"
        "    if 'id' not in msg:\n"
        "        continue\n"
        "    if method == 'initialize':\n"
        "        result = {'protocolVersion': '2025-06-18', 'capabilities': caps,\n"
        "                  'serverInfo': {'name': 'fake', 'version': '1'}}\n"
        "    elif method == 'tools/list':\n"
        "        result = {'tools': []}\n"
        "    elif method in pages:\n"
        "        cursor = (msg.get('params') or {}).get('cursor')\n"
        "        result = pages[method]['first' if cursor is None else 'next']\n"
        "    elif method == 'resources/read':\n"
        "        key = (msg.get('params') or {}).get('uri')\n"
        "        if key not in reads:\n"
        "            sys.stdout.write(json.dumps({'jsonrpc': '2.0', 'id': msg['id'],\n"
        "                'error': {'code': -32002, 'message': 'Resource not found'}}) + '\\n')\n"
        "            sys.stdout.flush()\n"
        "            continue\n"
        "        result = {'contents': reads[key]}\n"
        "    else:\n"
        "        continue\n"
        "    sys.stdout.write(json.dumps({'jsonrpc': '2.0', 'id': msg['id'], 'result': result}) + '\\n')\n"
        "    sys.stdout.flush()\n"
    ) % (json.dumps(caps), json.dumps(pages), json.dumps(reads))


class McpResourceTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="yak-mcp-res-")
        self.root = Path(self.scratch.name)
        shutil.copyfile(BRIDGE, self.root / "mcp_bridge.py")

    def tearDown(self):
        self.scratch.cleanup()

    def bridge(self, server):
        path = self.root / "fake_mcp.py"
        path.write_text(server)
        (self.root / "config.json").write_text(json.dumps({"fake": {"command": sys.executable, "args": [str(path)]}}))

    def drive(self, frames):
        proc = subprocess.Popen([sys.executable, str(self.root / "mcp_bridge.py")],
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.DEVNULL, text=True)
        replies = []
        try:
            for frame in frames:
                proc.stdin.write(json.dumps(frame) + "\n")
                proc.stdin.flush()
                replies.append(json.loads(proc.stdout.readline()))
            proc.stdin.write('{"type": "shutdown"}\n')
            proc.stdin.flush()
            proc.stdin.close()
            proc.wait(timeout=20)
        finally:
            for stream in (proc.stdin, proc.stdout):
                if stream is not None and not stream.closed:
                    stream.close()
            if proc.poll() is None:
                proc.kill()
                proc.wait()
        return replies

    def test_resources_mount_only_when_the_handshake_declares_them(self):
        self.bridge(fake_server({}, {"tools/list": {"first": {"tools": []}}}, {}))
        init = self.drive([{"type": "initialize", "id": 1, "v": 1}])[0]
        self.assertEqual([t["name"] for t in init["result"]["tools"]], [])

    def test_listing_follows_pagination_and_shows_templates(self):
        pages = {
            "resources/list": {"first": {"resources": [{"uri": "file:///a", "name": "A"}], "nextCursor": "c2"},
                               "next": {"resources": [{"uri": "file:///b", "name": "B"}]}},
            "resources/templates/list": {"first": {"resourceTemplates": [{"uriTemplate": "file:///{path}", "name": "any file"}]}},
        }
        self.bridge(fake_server({"resources": {}}, pages, {}))
        replies = self.drive([{"type": "initialize", "id": 1, "v": 1},
                              {"type": "call_tool", "id": 2, "v": 1, "name": "fake__resources", "args": {}}])
        self.assertEqual(sorted(t["name"] for t in replies[0]["result"]["tools"]),
                         ["fake__resource", "fake__resources"])
        text = replies[1]["result"]
        for expected in ("file:///a", "file:///b", "file:///{path} — template: any file"):
            self.assertIn(expected, text)

    def test_read_renders_text_and_summarizes_a_blob(self):
        reads = {"file:///a": [{"uri": "file:///a", "mimeType": "text/plain", "text": "hello 图"}],
                 "file:///bin": [{"uri": "file:///bin", "mimeType": "application/octet-stream", "blob": "AAAA"}]}
        self.bridge(fake_server({"resources": {}}, {"tools/list": {"first": {"tools": []}}}, reads))
        replies = self.drive([{"type": "initialize", "id": 1, "v": 1},
                              {"type": "call_tool", "id": 2, "v": 1, "name": "fake__resource", "args": {"uri": "file:///a"}},
                              {"type": "call_tool", "id": 3, "v": 1, "name": "fake__resource", "args": {"uri": "file:///bin"}},
                              {"type": "call_tool", "id": 4, "v": 1, "name": "fake__resource", "args": {"uri": "file:///nope"}},
                              {"type": "call_tool", "id": 5, "v": 1, "name": "fake__resource", "args": {}}])
        self.assertEqual(replies[1]["result"], "[uri file:///a] [type text/plain]\nhello 图")
        self.assertIn("[blob 4 base64 chars omitted]", replies[2]["result"])
        self.assertNotIn("AAAA", replies[2]["result"])
        self.assertIn("error", replies[3])
        self.assertIn("error", replies[4])

    def test_oversized_resource_is_cut_with_the_cut_marked(self):
        reads = {"file:///big": [{"uri": "file:///big", "text": "x" * 60000}]}
        self.bridge(fake_server({"resources": {}}, {"tools/list": {"first": {"tools": []}}}, reads))
        reply = self.drive([{"type": "initialize", "id": 1, "v": 1},
                            {"type": "call_tool", "id": 2, "v": 1, "name": "fake__resource", "args": {"uri": "file:///big"}}])[1]
        self.assertIn("[resource truncated to fit the context window]", reply["result"])
        self.assertLess(len(reply["result"]), 50000)

    def test_warm_cache_mounts_the_same_resource_tools_without_a_handshake(self):
        pages = {"resources/list": {"first": {"resources": []}}}
        self.bridge(fake_server({"resources": {}}, pages, {}))
        cold = self.drive([{"type": "initialize", "id": 1, "v": 1}])[0]
        self.assertTrue((self.root / "cache.json").exists())
        entry = json.loads((self.root / "cache.json").read_text())["fake"]
        self.assertIs(entry["resources"], True)
        (self.root / "fake_mcp.py").unlink()
        warm = self.drive([{"type": "initialize", "id": 1, "v": 1}])[0]
        self.assertEqual(sorted(t["name"] for t in warm["result"]["tools"]),
                         sorted(t["name"] for t in cold["result"]["tools"]))


if __name__ == "__main__":
    unittest.main()
