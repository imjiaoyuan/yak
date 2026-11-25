import contextlib
import hashlib
import http.server
import importlib.util
import json
import math
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import threading
import unittest


ROOT = Path(__file__).resolve().parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "examples" / "extensions" / name / "main.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


SESSION = load("session_search")
VECTOR = load("vector_search")


def turn(turn_id, body, parent=None, cwd="/project", date="2026-01-01"):
    value = {"v": 2, "id": turn_id, "ts": date + "T12:00:00+00:00", "mode": "agent", "model": "mock/model", "cwd": cwd, "prompt": body, "response": "answer " + body, "messages": [{"role": "user", "text": body}, {"role": "assistant", "text": "answer " + body, "tool_calls": []}]}
    if parent is not None:
        value["parent"] = parent
    return value


class SessionTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        (self.root / "threads").mkdir()
        self.path = self.root / "threads" / "thread-a.jsonl"
        self.rows = [turn("root", "common root"), turn("abandoned", "orphanword branch"), turn("active", "currentword branch", "root", date="2026-02-02")]
        self.save()
        self.plugin = SESSION.SessionSearch(self.root)
        self.addCleanup(self.plugin.db.close)

    def save(self):
        self.path.write_text("".join(json.dumps(row) + "\n" for row in self.rows), encoding="utf-8")

    def search(self, query, **kwargs):
        return self.plugin.handle(dict(action="search", query=query, **kwargs))

    def test_active_parent_walk_excludes_abandoned_branch(self):
        self.assertEqual(self.search("orphanword")["results"], [])
        results = self.search("common")["results"]
        self.assertEqual({row["turn"] for row in results}, {"root"})
        current = self.search("currentword")["results"][0]
        self.assertEqual(current["parent"], "root")
        self.assertEqual(current["effective_parent"], "root")
        self.assertEqual(current["thread"], "thread-a")
        self.assertEqual(current["source"], str(self.path))

    def test_all_branches_preserves_identity(self):
        rows = self.search("orphanword", all_branches=True)["results"]
        self.assertEqual({row["turn"] for row in rows}, {"abandoned"})
        self.assertTrue(all(row["active"] == 0 for row in rows))

    def test_prompt_and_signed_response_are_not_duplicated(self):
        rows = self.search("currentword")["results"]
        self.assertEqual(len(rows), 2)
        self.assertEqual({row["role"] for row in rows}, {"user", "assistant"})

    def test_tool_content_and_calls_are_searchable(self):
        self.rows[-1]["messages"].insert(1, {"role": "assistant", "text": "", "tool_calls": [{"name": "read", "id": "call", "arguments": {"path": "uniquepath"}}]})
        self.rows[-1]["messages"].insert(2, {"role": "tool_result", "name": "read", "content": "uniqueresult"})
        self.save()
        self.assertEqual(self.search("uniquepath")["results"][0]["role"], "tool_call")
        self.assertEqual(self.search("uniqueresult")["results"][0]["role"], "tool_result")

    def test_cwd_and_inclusive_date_filters(self):
        self.assertTrue(self.search("currentword", cwd="/project", after="2026-02-02", before="2026-02-02")["results"])
        self.assertEqual(self.search("currentword", cwd="/project/child")["results"], [])
        self.assertEqual(self.search("currentword", before="2026-02-01")["results"], [])
        with self.assertRaises(ValueError):
            self.search("currentword", after="bad-date")

    def test_exact_read_is_capped_and_has_provenance(self):
        result = self.plugin.handle({"action": "read", "thread": "thread-a", "turn": "active", "cap": 100})
        self.assertEqual(result["text"], self.path.read_text(encoding="utf-8").splitlines()[2][:100])
        self.assertTrue(result["truncated"])
        self.assertEqual(result["provenance"]["position"], 3)
        self.assertEqual(result["provenance"]["parent"], "root")

    def test_unsealed_multibyte_tail_warns_without_mutation(self):
        with self.path.open("ab") as stream:
            stream.write(b'{"id":"broken\xe4\xb8')
        original = self.path.read_bytes()
        first = self.search("currentword")
        second = self.search("currentword")
        self.assertTrue(first["warnings"])
        self.assertEqual(first["warnings"], second["warnings"])
        self.assertEqual(self.path.read_bytes(), original)

    def test_valid_json_without_newline_is_not_indexed(self):
        with self.path.open("ab") as stream:
            stream.write(json.dumps(turn("unsealed", "unsealedword")).encode())
        result = self.search("unsealedword")
        self.assertFalse(result["results"])
        self.assertTrue(result["warnings"])

    def test_complete_corrupt_tail_fails_loudly(self):
        with self.path.open("ab") as stream:
            stream.write(b'{broken}\n')
        with self.assertRaisesRegex(ValueError, "corrupt turn"):
            self.search("currentword")

    def test_interior_corruption_rolls_back_index_transaction(self):
        self.search("currentword")
        self.path.write_bytes(self.path.read_bytes().splitlines(keepends=True)[0] + b'{broken}\n' + json.dumps(self.rows[-1]).encode() + b"\n")
        with self.assertRaisesRegex(ValueError, "line 2"):
            self.search("currentword")
        self.assertEqual(self.plugin.db.execute("SELECT COUNT(*) FROM turns").fetchone()[0], 3)

    def test_future_version_fails_loudly(self):
        self.rows[-1]["v"] = 3
        self.save()
        with self.assertRaisesRegex(ValueError, "future format"):
            self.search("common")

    def test_duplicate_and_dangling_parent_are_rejected(self):
        for change in ({"id": "root"}, {"parent": "missing"}, {"parent": "active"}):
            with self.subTest(change=change):
                self.rows[-1] = dict(turn("active", "currentword", "root"), **change)
                self.save()
                with self.assertRaises(ValueError):
                    self.search("common")

    def test_legacy_version_and_first_external_parent(self):
        self.rows[0].pop("v")
        self.rows[0]["parent"] = "outside-fork"
        self.save()
        self.assertTrue(self.search("common")["results"])

    def test_incremental_append_rebuild_and_removed_source(self):
        self.plugin.handle({"action": "reindex"})
        self.assertEqual(self.plugin.sync()[0], 0)
        with self.path.open("a", encoding="utf-8") as stream:
            stream.write(json.dumps(turn("next", "appendedword")) + "\n")
        self.assertTrue(self.search("appendedword")["results"])
        self.assertEqual(self.plugin.sync()[0], 0)
        self.path.unlink()
        self.assertFalse(self.search("appendedword")["results"])
        self.assertEqual(self.plugin.db.execute("SELECT COUNT(*) FROM turns").fetchone()[0], 0)

    def test_same_size_rewrite_invalidates_rows(self):
        self.search("currentword")
        self.rows[-1]["prompt"] = "changedword branch"
        self.rows[-1]["messages"][0]["text"] = "changedword branch"
        self.save()
        self.assertTrue(self.search("changedword")["results"])

    def test_deterministic_ranking_and_excerpt_bounds(self):
        self.rows[-1] = turn("active", "x " * 2000 + "needle " + "z " * 2000, "root")
        self.save()
        first = self.search("needle", limit=1)
        self.assertEqual(first, self.search("needle", limit=1))
        self.assertLessEqual(len(first["results"][0]["excerpt"]), 600)
        self.assertIn("needle", first["results"][0]["excerpt"])
        self.assertTrue(first["results"][0]["truncated"])

    @unittest.skipIf(os.name == "nt", "Unix permissions")
    def test_index_and_parent_are_private(self):
        self.search("currentword")
        self.assertEqual(stat.S_IMODE(self.plugin.home.stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE((self.plugin.home / "index.sqlite").stat().st_mode), 0o600)


class EmbeddingServer(http.server.BaseHTTPRequestHandler):
    mode = "normal"
    requests = []

    def log_message(self, *args):
        pass

    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        type(self).requests.append((payload, self.headers.get("Authorization")))
        vectors = {"east": [1, 0], "north": [0, 1], "west": [-1, 0], "query": [1, 0.2], "tie": [1, 0]}
        data = [{"index": index, "embedding": vectors.get(body, [1, 0])} for index, body in enumerate(payload["input"])]
        result = {"model": payload["model"], "data": list(reversed(data))}
        mode = type(self).mode
        if mode == "dimension":
            for item in data:
                item["embedding"] = [1, 0, 0]
        elif mode == "mixed_dimension" and len(data) > 1:
            data[1]["embedding"] = [1, 0, 0]
        elif mode == "model":
            result["model"] = "wrong-model"
        elif mode == "nonfinite":
            data[0]["embedding"] = [float("nan"), 1]
        elif mode == "infinite":
            data[0]["embedding"] = [float("inf"), 1]
        elif mode == "zero":
            data[0]["embedding"] = [0, 0]
        elif mode == "boolean":
            data[0]["embedding"] = [True, 0]
        elif mode == "duplicate" and len(data) > 1:
            data[1]["index"] = 0
        elif mode == "missing":
            result["data"] = []
        elif mode == "error":
            self.send_response(401)
            self.end_headers()
            self.wfile.write(b"super-secret-key")
            return
        elif mode == "redirect":
            self.send_response(302)
            self.send_header("Location", "/elsewhere")
            self.end_headers()
            return
        raw = json.dumps(result).encode()
        if mode == "malformed":
            raw = b"not-json super-secret-key"
        elif mode == "oversized":
            raw = b" " * 2097153
        self.send_response(200)
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        with contextlib.suppress(BrokenPipeError, ConnectionResetError):
            self.wfile.write(raw)


class VectorTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), EmbeddingServer)
        cls.worker = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.worker.start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.worker.join()

    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.plugin = VECTOR.VectorSearch(self.scratch.name)
        self.addCleanup(self.plugin.db.close)
        self.config = {"endpoint": f"http://127.0.0.1:{self.server.server_port}/v1/embeddings", "api_key": "${YAK_RETRIEVAL_TEST_KEY}", "model": "test-model"}
        self.previous_key = os.environ.get("YAK_RETRIEVAL_TEST_KEY")
        os.environ["YAK_RETRIEVAL_TEST_KEY"] = "super-secret-key"
        self.addCleanup(self.restore_key)
        self.write_config()
        EmbeddingServer.mode = "normal"
        EmbeddingServer.requests = []

    def restore_key(self):
        if self.previous_key is None:
            os.environ.pop("YAK_RETRIEVAL_TEST_KEY", None)
        else:
            os.environ["YAK_RETRIEVAL_TEST_KEY"] = self.previous_key

    def write_config(self):
        (self.plugin.home / "config.json").write_text(json.dumps(self.config), encoding="utf-8")

    def add(self, names=("east", "north", "west")):
        return self.plugin.handle({"action": "add", "collection": "test", "items": [{"id": name, "text": name, "source": f"manual://{name}", "metadata": {"tag": name}} for name in names]})

    def search(self):
        return self.plugin.handle({"action": "search", "collection": "test", "query": "query", "limit": 10})

    def test_cosine_ranking_with_provenance_and_env_key(self):
        self.add()
        result = self.search()
        self.assertEqual([row["id"] for row in result["results"]], ["east", "north", "west"])
        self.assertAlmostEqual(result["results"][0]["score"], 1 / math.sqrt(1.04))
        self.assertEqual(result["results"][0]["source"], "manual://east")
        self.assertEqual(result["results"][0]["metadata"], {"tag": "east"})
        self.assertEqual(EmbeddingServer.requests[0][1], "Bearer super-secret-key")
        self.assertNotIn("super-secret-key", json.dumps(result))

    def test_equal_scores_sort_by_id(self):
        self.add(("tie", "east"))
        self.assertEqual([row["id"] for row in self.search()["results"]], ["east", "tie"])

    def test_delete_and_list_are_local_and_identity_remains(self):
        self.add()
        requests = len(EmbeddingServer.requests)
        result = self.plugin.handle({"action": "delete", "collection": "test", "ids": ["east", "absent"]})
        self.assertEqual(result["deleted"], 1)
        listing = self.plugin.handle({"action": "list", "collection": "test", "limit": 1})
        self.assertTrue(listing["more"])
        self.assertEqual(listing["items"][0]["id"], "north")
        self.assertEqual(self.plugin.handle({"action": "list"})["collections"][0]["count"], 2)
        self.assertEqual(len(EmbeddingServer.requests), requests)
        self.assertEqual(self.plugin.db.execute("SELECT COUNT(*) FROM collections").fetchone()[0], 1)

    def test_config_model_and_endpoint_mismatch_fail_before_http(self):
        self.add()
        requests = len(EmbeddingServer.requests)
        for key, value in (("model", "different"), ("endpoint", self.config["endpoint"] + "/other")):
            old = self.config[key]
            self.config[key] = value
            self.write_config()
            with self.assertRaisesRegex(ValueError, "mismatch"):
                self.search()
            self.config[key] = old
        self.assertEqual(len(EmbeddingServer.requests), requests)

    def test_dimension_mismatch_does_not_mutate_collection(self):
        self.add()
        EmbeddingServer.mode = "dimension"
        with self.assertRaisesRegex(ValueError, "dimension mismatch"):
            self.search()
        with self.assertRaisesRegex(ValueError, "dimension mismatch"):
            self.add(("new",))
        self.assertEqual(self.plugin.db.execute("SELECT COUNT(*) FROM items").fetchone()[0], 3)

    def test_optional_dimensions_enforced(self):
        self.config["dimensions"] = 3
        self.write_config()
        with self.assertRaisesRegex(ValueError, "dimension mismatch"):
            self.add()
        self.assertEqual(self.plugin.db.execute("SELECT COUNT(*) FROM collections").fetchone()[0], 0)

    def test_bad_embedding_responses_are_rejected_atomically(self):
        for mode in ("malformed", "nonfinite", "infinite", "zero", "boolean", "duplicate", "missing", "model", "mixed_dimension", "oversized", "error", "redirect"):
            with self.subTest(mode=mode):
                EmbeddingServer.mode = mode
                with self.assertRaises(ValueError) as error:
                    self.add()
                self.assertNotIn("super-secret-key", str(error.exception))
                self.assertEqual(self.plugin.db.execute("SELECT COUNT(*) FROM items").fetchone()[0], 0)
                self.assertEqual(self.plugin.db.execute("SELECT COUNT(*) FROM collections").fetchone()[0], 0)

    def test_ingestion_requires_source_and_respects_batch_bounds(self):
        for items in ([{"id": "a", "text": "east"}], [{"id": "a", "text": "x" * 16001, "source": "manual"}], [{"id": str(i), "text": "east", "source": "manual"} for i in range(17)]):
            with self.subTest(items=len(items)):
                with self.assertRaises(ValueError):
                    self.plugin.handle({"action": "add", "collection": "test", "items": items})
        self.assertFalse(EmbeddingServer.requests)

    def test_missing_required_config_never_uses_defaults(self):
        for key in ("api_key", "model", "endpoint"):
            value = self.config.pop(key)
            self.write_config()
            with self.assertRaisesRegex(ValueError, "config.json"):
                self.add()
            self.config[key] = value
        self.assertFalse(EmbeddingServer.requests)

    def test_unset_env_fails_without_key_disclosure(self):
        os.environ.pop("YAK_RETRIEVAL_TEST_KEY")
        with self.assertRaisesRegex(ValueError, "config.json"):
            self.add()
        self.assertFalse(EmbeddingServer.requests)

    def test_upsert_replaces_without_duplicate_rows(self):
        self.add()
        self.add(("east",))
        self.assertEqual(self.plugin.db.execute("SELECT COUNT(*) FROM items").fetchone()[0], 3)

    def test_huge_finite_vectors_normalize_without_overflow(self):
        vector = VECTOR.normalize([1e308, 1e308])
        self.assertAlmostEqual(sum(value * value for value in vector), 1)

    @unittest.skipIf(os.name == "nt", "Unix permissions")
    def test_db_config_and_parent_are_private(self):
        self.add()
        self.assertEqual(stat.S_IMODE(self.plugin.home.stat().st_mode), 0o700)
        for path in (self.plugin.home / "index.sqlite", self.plugin.home / "config.json"):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)


class ProtocolTests(unittest.TestCase):
    def test_resident_initialize_call_and_shutdown(self):
        for name in ("session_search", "vector_search"):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as root:
                frames = [{"v": 1, "id": 1, "type": "initialize", "params": {"user_dir": root}}, {"v": 1, "id": 2, "type": "call_tool", "name": name, "args": {"action": "reindex" if name == "session_search" else "list"}}, {"v": 1, "id": 3, "type": "call_tool", "name": name, "args": {"action": "invalid"}}, {"v": 1, "id": 4, "type": "shutdown"}]
                run = subprocess.run([sys.executable, str(ROOT / "examples" / "extensions" / name / "main.py")], input="".join(json.dumps(frame) + "\n" for frame in frames), capture_output=True, text=True, timeout=10)
                self.assertEqual(run.returncode, 0, run.stderr)
                replies = [json.loads(line) for line in run.stdout.splitlines()]
                self.assertEqual(len(replies), 3)
                self.assertEqual(replies[0]["result"]["tools"][0]["tier"], "exec")
                self.assertIsInstance(json.loads(replies[1]["result"]), dict)
                self.assertIn("error", replies[2])
                self.assertEqual([reply["id"] for reply in replies], [1, 2, 3])


if __name__ == "__main__":
    unittest.main(verbosity=2)
