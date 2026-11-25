import json
import math
import os
from pathlib import Path
import re
import sqlite3
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


TOOL = {
    "name": "vector_search",
    "tier": "exec",
    "description": "Explicit text ingestion and exact cosine retrieval, no filesystem scanning. add/search send caller text to the configured OpenAI-compatible embeddings endpoint. Configure extensions/vector_search/config.json with required endpoint (full HTTP URL), api_key (supports ${ENV}), model, optional dimensions. Collections pin endpoint/model/dimension. add upserts up to 16 items {id,text,source,metadata?}; source provenance is required. Text max 16000 chars/item, 64000/batch; at most 10000 items/collection. delete takes ids; list lists collections or bounded item metadata, never sends HTTP.",
    "parameters": {
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": ["add", "search", "delete", "list"]},
            "collection": {"type": "string"},
            "items": {"type": "array", "maxItems": 16, "items": {"type": "object", "properties": {"id": {"type": "string"}, "text": {"type": "string"}, "source": {"type": "string"}, "metadata": {"type": "object"}}, "required": ["id", "text", "source"], "additionalProperties": False}},
            "query": {"type": "string"},
            "ids": {"type": "array", "maxItems": 16, "items": {"type": "string"}},
            "limit": {"type": "integer", "minimum": 1, "maximum": 10},
            "offset": {"type": "integer", "minimum": 0, "maximum": 10000},
        },
        "required": ["action"],
        "additionalProperties": False,
    },
}


def reject_constant(value):
    raise ValueError("nonfinite JSON value")


def string(value, name, cap):
    if not isinstance(value, str) or not value.strip() or len(value) > cap:
        raise ValueError(f"{name} must be a nonempty string of at most {cap} characters")
    return value


def integer(value, name, low, high):
    if type(value) is not int or not low <= value <= high:
        raise ValueError(f"{name} must be an integer in {low}..{high}")
    return value


def expand(value):
    def replace(match):
        if not os.environ.get(match[1]):
            raise ValueError("embedding config references an unset or empty environment variable")
        return os.environ[match[1]]
    return re.sub(r"\$\{([^}]+)\}", replace, value)


def normalize(vector, dimension=None):
    if not isinstance(vector, list) or not 1 <= len(vector) <= 4096:
        raise ValueError("embedding must be an array of 1..4096 numbers")
    if dimension is not None and len(vector) != dimension:
        raise ValueError("embedding dimension mismatch")
    if any(type(value) not in (int, float) for value in vector):
        raise ValueError("embedding values must be finite numbers")
    try:
        if not all(math.isfinite(value) for value in vector):
            raise ValueError("embedding values must be finite numbers")
        scale = max(abs(value) for value in vector)
        if scale == 0:
            raise ValueError("embedding must be nonzero")
        scaled = [value / scale for value in vector]
        norm = math.sqrt(math.fsum(value * value for value in scaled))
        return [value / norm for value in scaled]
    except OverflowError:
        raise ValueError("embedding values must be finite numbers") from None


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError("embedding endpoint redirects are refused")


class VectorSearch:
    def __init__(self, user_dir):
        self.home = Path(user_dir) / "extensions" / "vector_search"
        self.home.mkdir(parents=True, exist_ok=True, mode=0o700)
        if os.name != "nt":
            self.home.chmod(0o700)
        path = self.home / "index.sqlite"
        descriptor = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
        os.close(descriptor)
        if os.name != "nt":
            path.chmod(0o600)
        self.db = sqlite3.connect(path)
        self.db.row_factory = sqlite3.Row
        self.db.executescript("""
            CREATE TABLE IF NOT EXISTS collections(name TEXT PRIMARY KEY, endpoint TEXT NOT NULL, model TEXT NOT NULL, dimension INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS items(collection TEXT, id TEXT, text TEXT NOT NULL, source TEXT NOT NULL, metadata TEXT NOT NULL, vector TEXT NOT NULL, PRIMARY KEY(collection,id));
        """)

    def config(self):
        path = self.home / "config.json"
        if os.name != "nt" and path.exists():
            path.chmod(0o600)
        try:
            with path.open(encoding="utf-8") as stream:
                config = json.load(stream, parse_constant=reject_constant)
            if not isinstance(config, dict):
                raise ValueError()
            endpoint = expand(string(config.get("endpoint"), "endpoint", 2048))
            key = expand(string(config.get("api_key"), "api_key", 4096))
            model = expand(string(config.get("model"), "model", 256))
            url = urllib.parse.urlsplit(endpoint)
            if url.scheme not in ("http", "https") or not url.hostname or url.username or url.password or url.fragment or url.query:
                raise ValueError()
            if any(c in key for c in "\r\n") or not key.strip() or not model.strip():
                raise ValueError()
            dimension = config.get("dimensions")
            if dimension is not None:
                integer(dimension, "dimensions", 1, 4096)
        except (ValueError, TypeError, OSError):
            raise ValueError("invalid or missing vector_search/config.json: require endpoint (full HTTP(S) URL without credentials/query/fragment), api_key, model; optional dimensions 1..4096; ${ENV} must resolve") from None
        return endpoint, key, model, dimension

    def embed(self, texts, config):
        endpoint, key, model, dimension = config
        payload = {"model": model, "input": texts, "encoding_format": "float"}
        if dimension is not None:
            payload["dimensions"] = dimension
        request = urllib.request.Request(endpoint, data=json.dumps(payload, allow_nan=False).encode(), headers={"Content-Type": "application/json", "Authorization": "Bearer " + key}, method="POST")
        deadline = time.monotonic() + 15
        try:
            with urllib.request.build_opener(NoRedirect()).open(request, timeout=10) as response:
                if response.status != 200:
                    raise ValueError("embedding endpoint must return HTTP 200")
                raw = bytearray()
                while True:
                    if time.monotonic() >= deadline:
                        raise ValueError("embedding response exceeded time limit")
                    chunk = response.read1(min(65536, 2097153 - len(raw)))
                    raw.extend(chunk)
                    if len(raw) > 2097152:
                        raise ValueError("embedding response exceeds 2 MiB")
                    if not chunk:
                        break
        except urllib.error.HTTPError as exc:
            status = exc.code
            exc.close()
            raise ValueError(f"embedding HTTP error {status}; response body withheld") from None
        except (urllib.error.URLError, OSError, TimeoutError):
            raise ValueError("embedding request failed or timed out; endpoint details withheld") from None
        try:
            result = json.loads(raw.decode("utf-8"), parse_constant=reject_constant)
            if not isinstance(result, dict) or not isinstance(result.get("data"), list) or len(result["data"]) != len(texts):
                raise ValueError("embedding data count mismatch")
            if "model" in result and result["model"] != model:
                raise ValueError("embedding response model mismatch")
            vectors = [None] * len(texts)
            for item in result["data"]:
                if not isinstance(item, dict):
                    raise ValueError("embedding data entries must be objects")
                index = integer(item.get("index"), "embedding index", 0, len(texts) - 1)
                if vectors[index] is not None:
                    raise ValueError("duplicate embedding index")
                vectors[index] = normalize(item.get("embedding"), dimension)
                dimension = len(vectors[index])
            return vectors
        except (UnicodeError, json.JSONDecodeError):
            raise ValueError("embedding response is not valid UTF-8 JSON") from None

    def identity(self, collection, config):
        row = self.db.execute("SELECT * FROM collections WHERE name=?", (collection,)).fetchone()
        if row and (row["endpoint"] != config[0] or row["model"] != config[2] or (config[3] is not None and row["dimension"] != config[3])):
            raise ValueError("collection endpoint/model/dimension mismatch; use the original config or a new collection")
        return row

    def handle(self, args):
        action = args.get("action")
        if action not in ("add", "search", "delete", "list"):
            raise ValueError("action must be add, search, delete or list")
        limit = integer(args.get("limit", 5), "limit", 1, 10)
        offset = integer(args.get("offset", 0), "offset", 0, 10000)
        if action == "list" and "collection" not in args:
            rows = self.db.execute("SELECT c.name,c.model,c.dimension,COUNT(i.id) AS count FROM collections c LEFT JOIN items i ON i.collection=c.name GROUP BY c.name ORDER BY c.name LIMIT ? OFFSET ?", (limit + 1, offset)).fetchall()
            return {"collections": [dict(row) for row in rows[:limit]], "more": len(rows) > limit, "offset": offset}
        collection = string(args.get("collection"), "collection", 128)
        if not re.fullmatch(r"[A-Za-z0-9_-]+", collection):
            raise ValueError("collection must contain only ASCII letters, digits, underscore or hyphen")
        if action == "list":
            rows = self.db.execute("SELECT id,source,length(text) AS text_chars FROM items WHERE collection=? ORDER BY id LIMIT ? OFFSET ?", (collection, limit + 1, offset)).fetchall()
            return {"items": [dict(row) for row in rows[:limit]], "more": len(rows) > limit, "offset": offset}
        if action == "delete":
            ids = args.get("ids")
            if not isinstance(ids, list) or not 1 <= len(ids) <= 16:
                raise ValueError("ids must contain 1..16 ids")
            ids = [string(value, "id", 256) for value in ids]
            with self.db:
                deleted = 0
                for item_id in sorted(set(ids)):
                    deleted += self.db.execute("DELETE FROM items WHERE collection=? AND id=?", (collection, item_id)).rowcount
            return {"deleted": deleted, "identity_retained": True}
        config = self.config()
        identity = self.identity(collection, config)
        if action == "add":
            items = args.get("items")
            if not isinstance(items, list) or not 1 <= len(items) <= 16:
                raise ValueError("items must contain 1..16 items")
            prepared = []
            seen = set()
            for item in items:
                if not isinstance(item, dict):
                    raise ValueError("each item must be an object")
                item_id = string(item.get("id"), "id", 256)
                body = string(item.get("text"), "text", 16000)
                source = string(item.get("source"), "source", 2000)
                metadata = item.get("metadata", {})
                if not isinstance(metadata, dict):
                    raise ValueError("metadata must be an object")
                encoded = json.dumps(metadata, ensure_ascii=False, allow_nan=False)
                if len(encoded) > 4000:
                    raise ValueError("metadata exceeds 4000 characters")
                if item_id in seen:
                    raise ValueError("duplicate id in batch")
                seen.add(item_id)
                prepared.append((item_id, body, source, encoded))
            if sum(len(item[1]) for item in prepared) > 64000:
                raise ValueError("batch text exceeds 64000 characters")
            vectors = self.embed([item[1] for item in prepared], config)
            dimension = len(vectors[0])
            with self.db:
                identity = self.identity(collection, config)
                if identity and identity["dimension"] != dimension:
                    raise ValueError("collection dimension mismatch")
                if identity is None:
                    self.db.execute("INSERT INTO collections VALUES(?,?,?,?)", (collection, config[0], config[2], dimension))
                count = self.db.execute("SELECT COUNT(*) FROM items WHERE collection=?", (collection,)).fetchone()[0]
                existing = {row[0] for row in self.db.execute("SELECT id FROM items WHERE collection=?", (collection,))}
                if count + len(seen - existing) > 10000:
                    raise ValueError("collection exceeds 10000 items; delete items or use another collection")
                for item, vector in zip(prepared, vectors):
                    self.db.execute("INSERT OR REPLACE INTO items VALUES(?,?,?,?,?,?)", (collection, *item, json.dumps(vector, allow_nan=False)))
            return {"added": len(prepared), "collection": collection, "model": config[2], "dimension": dimension}
        query = string(args.get("query"), "query", 16000)
        if identity is None:
            raise ValueError("collection not found")
        vector = self.embed([query], config)[0]
        if len(vector) != identity["dimension"]:
            raise ValueError("collection dimension mismatch")
        count = self.db.execute("SELECT COUNT(*) FROM items WHERE collection=?", (collection,)).fetchone()[0]
        if count > 10000:
            raise ValueError("collection exceeds exact-search bound of 10000 items")
        ranked = []
        for row in self.db.execute("SELECT * FROM items WHERE collection=?", (collection,)):
            stored = normalize(json.loads(row["vector"], parse_constant=reject_constant), identity["dimension"])
            score = max(-1.0, min(1.0, math.fsum(a * b for a, b in zip(vector, stored))))
            ranked.append((score, row))
        ranked.sort(key=lambda entry: (-entry[0], entry[1]["id"]))
        return {"collection": collection, "model": identity["model"], "dimension": identity["dimension"], "results": [{"id": row["id"], "score": score, "source": row["source"], "metadata": json.loads(row["metadata"], parse_constant=reject_constant), "text": row["text"][:1000], "text_chars": len(row["text"]), "truncated": len(row["text"]) > 1000} for score, row in ranked[:limit]]}


def main():
    plugin = None
    for line in sys.stdin:
        frame = json.loads(line)
        try:
            if frame.get("type") == "shutdown":
                break
            if frame.get("type") == "initialize":
                plugin = VectorSearch(frame["params"]["user_dir"])
                result = {"tools": [TOOL]}
            elif frame.get("type") == "call_tool":
                if plugin is None or frame.get("name") != TOOL["name"]:
                    raise ValueError("tool not initialized or unknown")
                result = json.dumps(plugin.handle(frame.get("args", {})), ensure_ascii=False, allow_nan=False)
            else:
                raise ValueError("unsupported request")
            reply = {"id": frame["id"], "result": result}
        except (ValueError, OSError, sqlite3.Error, KeyError, TypeError) as exc:
            reply = {"id": frame["id"], "error": str(exc)}
        print(json.dumps(reply, ensure_ascii=False, allow_nan=False), flush=True)
    if plugin is not None:
        plugin.db.close()


if __name__ == "__main__":
    main()
