#!/usr/bin/env python3
"""mcp-bridge — Model Context Protocol servers as yak tools (resident extension).

The host speaks its own newline-delimited protocol; MCP servers speak
JSON-RPC 2.0 over stdio or streamable HTTP. This extension sits between
them and translates, so an MCP server needs no host-side support: it is
started once, its tools/list is forwarded as the extension's own tool
list (each exposed as `<server>__<tool>`, the MCP convention), and every
call_tool is forwarded to the matching server's tools/call with the text
content pieces joined.

Configuration lives next to this file in `mcp.json` (same directory), a
flat map of server entries — the usual `{"mcpServers": {...}}` wrapper is
unwrapped when present:

    {
      "fetch":  {"command": "uvx", "args": ["mcp-server-fetch"]},
      "memory": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-memory"],
                 "env": {"MEMORY_FILE": "/tmp/mem.json"}},
      "cloudflare": {"type": "streamable-http",
                     "url": "https://mcp.cloudflare.com/mcp",
                     "headers": {"Authorization": "Bearer ${CLOUDFLARE_MCP_TOKEN}"}}
    }

`command` picks the stdio transport, `url` the streamable-HTTP one.
Relative `command` values resolve on PATH; `env` rides along with (not
replacing) the inherited environment, and `headers` adds to the HTTP
request's own. `${VAR}` in any string value expands from the environment;
a variable that is not set is an error, not a silently empty value. A
server that fails to start or to finish its initialize handshake is
skipped with a dim warning on stderr — the bridge and the remaining
servers stay up.

Startup is lazy: a server's tool listing is cached in `mcp_cache.json`,
keyed by a hash of its entry, and a warm cache mounts the tools instantly
without spawning the server — the child starts on its first tools/call
instead, so a cold host start touches no MCP child. An entry whose hash
misses (first run, or a changed config) starts right away as before, and
a lazily started server refreshes its cached listing whenever it has
drifted from a fresh tools/list.

Honest limits, by design:
  - HTTP means the streamable-HTTP transport: one POST per JSON-RPC
    message, the reply either a JSON body or an SSE stream carrying it,
    and the server's `Mcp-Session-Id` echoed on every later request. A
    server that answers a request with 202 and expects the client to
    listen on a separate GET stream is not bridged, nor is the older
    two-endpoint SSE transport. Auth is a static header, so an OAuth-only
    server needs a token obtained out of band (no browser flow here).
  - MCP resources, prompts and sampling are not translated — tools are the
    one MCP surface with a clean host-side counterpart.
  - Tool schemas pass through as-is (inputSchema → parameters); a server
    shipping a non-object schema is exposed as a no-argument tool rather
    than dropping the server.
  - `tools` in an entry filters its tools before they mount:
    `{"only": ["search*"], "except": ["*_admin"]}` (fnmatch against the
    MCP-side name). A tool the filter dropped errors at call time like any
    unknown tool; schemas cost tokens every round, so the cut belongs at
    the mount.
  - `defer: true` in an entry replaces the server's mounted tools with two:
    `<server>__search` (ranked name+description match over the cached
    tools/list) and `<server>__call` (forwards to the named MCP tool), so
    a server shipping forty tools costs two schemas instead of forty. The
    call validates against the filtered listing and forwards exactly like
    a mounted tool; the search runs off the warm cache and starts no
    child. The tool list still refreshes on drift whenever a call does
    start the server.
  - `tool_timeout` is not per tool: the bridge asks for none, so the
    config `extensions.tool_timeout` (120s default) bounds every call.

Deadlines: initialize a server waits 10s; a tools/call waits 30s and then
errors the call, sending `notifications/cancelled` for the id (a stdio
server may ignore it — the id has left the pending map either way, so a
late reply is dropped as unknown).
"""

import fnmatch
import hashlib
import json
import os
import re
import subprocess
import sys
import threading
import urllib.error
import urllib.request

INIT_TIMEOUT = 10.0     # seconds: a server's initialize handshake
CALL_TIMEOUT = 30.0     # seconds: one tools/call round
CONFIG_NAME = "mcp.json"
CACHE_NAME = "mcp_cache.json"
PROTOCOL_VERSION = "2025-06-18"
PROTOCOL_VERSION_HEADER = "MCP-Protocol-Version"
# CDN firewalls (Cloudflare's own, error 1010) drop the python-urllib
# default user agent, so every request carries a real one.
USER_AGENT = "yak-mcp-bridge/1 (https://github.com/imjiaoyuan/yak)"


def log(msg):
    """Diagnostics ride stderr; the host dims it into the tool log."""
    sys.stderr.write(f"mcp-bridge: {msg}\n")
    sys.stderr.flush()


def expand_env(value):
    """`${VAR}` in a config string resolves from the environment. An unset
    variable raises: a missing token must not become an empty header."""
    def sub(m):
        name = m.group(1)
        if name not in os.environ:
            raise RuntimeError(f"${{{name}}} is not set in the environment")
        return os.environ[name]
    return re.sub(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}", sub, value)


class StdioTransport:
    """One MCP server child: a locked request pipe and a reader thread
    that routes replies by JSON-RPC id to the waiting caller."""

    def __init__(self, name, spec):
        self.name = name
        self.spec = spec
        self.proc = None
        self.wlock = threading.Lock()
        self.next_id = 0
        self.pending = {}          # jsonrpc id -> (Event, reply-slot)
        self.plock = threading.Lock()

    # lifecycle

    def open(self):
        cmd = expand_env(str(self.spec["command"]))
        argv = [cmd] + [expand_env(str(a)) for a in (self.spec.get("args") or [])]
        env = dict(os.environ)
        for k, v in (self.spec.get("env") or {}).items():
            env[str(k)] = expand_env(str(v))
        try:
            self.proc = subprocess.Popen(
                argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, env=env)
        except FileNotFoundError:
            raise RuntimeError(f"'{cmd}' not found")
        except OSError as e:
            raise RuntimeError(f"spawn failed: {e}")
        threading.Thread(target=self._read_loop, daemon=True).start()

    def close(self):
        if self.proc and self.proc.poll() is None:
            try:
                self.proc.stdin.close()
            except OSError:
                pass
            try:
                self.proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self._fail_pending()

    def _fail_pending(self):
        """Release everyone still waiting: stdout closed or the child is
        gone, so no reply is ever coming."""
        with self.plock:
            waiters = list(self.pending.values())
            self.pending.clear()
        for _, slot, ready in waiters:
            slot.append(None)
            ready.set()

    # jsonrpc plumbing

    def _read_loop(self):
        for raw in self.proc.stdout:
            try:
                msg = json.loads(raw)
            except (json.JSONDecodeError, UnicodeDecodeError):
                continue
            rid = msg.get("id")
            if rid is None:            # a notification from the server; ignored
                continue
            with self.plock:
                waiter = self.pending.pop(rid, None)
            if waiter:
                waiter[1].append(msg.get("result") if "result" in msg
                                 else {"__error__": msg.get("error")})
                waiter[2].set()
        # stdout closed: fail everything still waiting
        self._fail_pending()

    def request(self, method, params, timeout):
        """One JSON-RPC round. Returns the result object, or None on
        timeout/death/error (the caller decides how loud that gets)."""
        if not self.proc or self.proc.poll() is not None:
            return None
        self.next_id += 1
        rid = self.next_id
        ready = threading.Event()
        slot = []
        with self.plock:
            self.pending[rid] = (rid, slot, ready)
        frame = {"jsonrpc": "2.0", "id": rid, "method": method,
                 "params": params}
        try:
            with self.wlock:
                self.proc.stdin.write((json.dumps(frame) + "\n").encode())
                self.proc.stdin.flush()
        except (BrokenPipeError, OSError):
            with self.plock:
                self.pending.pop(rid, None)
            return None
        if not ready.wait(timeout):
            with self.plock:
                self.pending.pop(rid, None)
            self.notify("notifications/cancelled",
                        {"requestId": rid, "reason": "timeout"})
            return None
        result = slot[0] if slot else None
        if isinstance(result, dict) and "__error__" in result:
            log(f"{self.name}: {method} error: {result['__error__']}")
            return None
        return result

    def notify(self, method, params):
        frame = {"jsonrpc": "2.0", "method": method, "params": params}
        try:
            with self.wlock:
                self.proc.stdin.write((json.dumps(frame) + "\n").encode())
                self.proc.stdin.flush()
        except (BrokenPipeError, OSError):
            pass


class HttpTransport:
    """MCP over streamable HTTP: one POST per JSON-RPC message, the reply
    either a JSON body or an SSE stream carrying it. The `Mcp-Session-Id`
    a server hands out at initialize rides every later request."""

    def __init__(self, name, spec):
        self.name = name
        self.spec = spec
        self.url = ""
        self.headers = {}
        self.session_id = None
        self.next_id = 0

    # lifecycle

    def open(self):
        self.url = expand_env(str(self.spec["url"]))
        if not self.url.startswith(("http://", "https://")):
            raise RuntimeError(f"'{self.url}' is not an http(s) url")
        for k, v in (self.spec.get("headers") or {}).items():
            self.headers[str(k)] = expand_env(str(v))

    def close(self):
        """DELETE ends the session, as the spec asks; a server that does
        not implement it is not an error worth surfacing."""
        if not self.session_id:
            return
        req = urllib.request.Request(
            self.url, method="DELETE",
            headers={**self.headers, "Mcp-Session-Id": self.session_id,
                     "User-Agent": USER_AGENT})
        try:
            urllib.request.urlopen(req, timeout=5).close()
        except (urllib.error.URLError, OSError):
            pass

    # jsonrpc plumbing

    def _post(self, frame, timeout, expect_reply):
        headers = dict(self.headers)
        headers["Content-Type"] = "application/json"
        headers["Accept"] = "application/json, text/event-stream"
        headers["User-Agent"] = USER_AGENT
        headers[PROTOCOL_VERSION_HEADER] = PROTOCOL_VERSION
        if self.session_id:
            headers["Mcp-Session-Id"] = self.session_id
        req = urllib.request.Request(
            self.url, data=json.dumps(frame).encode(), headers=headers,
            method="POST")
        with urllib.request.urlopen(req, timeout=timeout) as r:
            sid = r.headers.get("Mcp-Session-Id")
            if sid:
                self.session_id = sid
            if not expect_reply:
                return None
            ctype = (r.headers.get("Content-Type") or "").split(";")[0].strip().lower()
            if ctype == "text/event-stream":
                return self._sse_reply(r, frame.get("id"))
            body = r.read(8 * 1024 * 1024)
        return json.loads(body) if body.strip() else None

    def _sse_reply(self, stream, rid):
        """Read the response stream until the frame carrying our id shows
        up. Server-initiated messages on the way are ignored."""
        data = []
        for line in stream:
            line = line.decode("utf-8", "replace").rstrip("\r\n")
            if not line:
                if data:
                    msg = self._parse("\n".join(data))
                    data = []
                    if isinstance(msg, dict) and msg.get("id") == rid:
                        return msg
                continue
            if line.startswith("data:"):
                data.append(line[5:].lstrip())
        return None

    @staticmethod
    def _parse(text):
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            return None

    def request(self, method, params, timeout):
        """One JSON-RPC round over HTTP. Same contract as the stdio
        transport: the result object, or None (logged) on any failure."""
        self.next_id += 1
        rid = self.next_id
        frame = {"jsonrpc": "2.0", "id": rid, "method": method,
                 "params": params}
        try:
            reply = self._post(frame, timeout, True)
        except urllib.error.HTTPError as e:
            log(f"{self.name}: {method} failed: HTTP {e.code} {e.reason}")
            return None
        except (urllib.error.URLError, TimeoutError, OSError,
                json.JSONDecodeError, UnicodeDecodeError) as e:
            log(f"{self.name}: {method} failed: {e}")
            return None
        if not isinstance(reply, dict):
            log(f"{self.name}: {method} returned no reply")
            return None
        if "error" in reply:
            log(f"{self.name}: {method} error: {reply['error']}")
            return None
        return reply.get("result")

    def notify(self, method, params):
        frame = {"jsonrpc": "2.0", "method": method, "params": params}
        try:
            self._post(frame, 10.0, False)
        except (urllib.error.URLError, TimeoutError, OSError,
                json.JSONDecodeError, UnicodeDecodeError):
            pass


class Server:
    """One MCP server of either transport: open it, run the MCP handshake,
    then forward requests to it. The transport is picked from the entry —
    `command` spawns a child, `url` speaks HTTP."""

    def __init__(self, name, spec):
        self.name = name
        self.spec = spec
        self.transport = None
        self.slock = threading.Lock()
        self.listing = None      # fresh tools/list of the latest lazy start

    def ensure(self):
        """Spawn and handshake a lazily mounted server on its first call.
        True means requests can go through; a failure is logged here and
        answered as a call error by the caller."""
        if self.transport is not None:
            return True
        with self.slock:
            if self.transport is not None:
                return True
            try:
                self.listing = self.start()
                return True
            except RuntimeError as e:
                log(f"server '{self.name}' failed to start: {e}")
                self.stop()
                return False

    def start(self):
        """Connect, initialize, tools/list. Returns the tool list (may be
        empty) or raises with a reason the caller dims."""
        kind = str(self.spec.get("type") or "").lower()
        has_url = self.spec.get("url") is not None
        has_cmd = self.spec.get("command") is not None
        if has_url or kind in ("http", "streamable-http", "streamable_http"):
            if not has_url:
                raise RuntimeError(f"transport '{kind}' needs a url")
            self.transport = HttpTransport(self.name, self.spec)
        elif has_cmd:
            cmd = self.spec.get("command")
            if not isinstance(cmd, str) or not cmd:
                raise RuntimeError("entry has no command")
            self.transport = StdioTransport(self.name, self.spec)
        else:
            raise RuntimeError("entry needs 'command' (stdio) or 'url' (http)")
        self.transport.open()
        params = {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "yak-mcp-bridge", "version": "1"},
        }
        result = self.transport.request("initialize", params, INIT_TIMEOUT)
        if result is None:
            raise RuntimeError("initialize handshake timed out")
        self.transport.notify("notifications/initialized", {})
        return self.list_tools()

    def list_tools(self):
        """tools/list following the pagination cursor: a server with more
        tools than fit one page advertises the rest under `nextCursor`, and
        a page that stops early would silently hide them."""
        tools, cursor = [], None
        for _ in range(1000):
            params = {} if cursor is None else {"cursor": cursor}
            page = self.transport.request("tools/list", params, INIT_TIMEOUT)
            if not isinstance(page, dict) or not isinstance(page.get("tools"), list):
                raise RuntimeError("tools/list returned a malformed page")
            tools.extend(page["tools"])
            cursor = page.get("nextCursor")
            if cursor is not None and (not isinstance(cursor, str) or not cursor):
                raise RuntimeError("tools/list returned a malformed nextCursor")
            if not cursor:
                return tools
        raise RuntimeError(f"{self.name}: tools/list pagination did not end")

    def request(self, method, params, timeout):
        if self.transport is None:
            return None
        return self.transport.request(method, params, timeout)

    def notify(self, method, params):
        if self.transport is not None:
            self.transport.notify(method, params)

    def stop(self):
        if self.transport is not None:
            self.transport.close()


def text_of(result):
    """Join an MCP tools/call result's text pieces; non-text content is
    represented by its type so nothing silently vanishes."""
    out = []
    for block in (result or {}).get("content") or []:
        if not isinstance(block, dict):
            continue
        if block.get("type") == "text":
            out.append(block.get("text", ""))
        elif block.get("type") == "image":
            out.append("[image omitted]")
        else:
            out.append(f"[{block.get('type', 'content')}]")
    if (result or {}).get("isError"):
        return None, "\n".join(out) or "tool reported an error"
    return "\n".join(out), None


def load_config():
    """mcp.json beside this script. Missing file: no servers, no tools —
    the bridge mounts nothing and stays quiet. An `{"mcpServers": {...}}`
    wrapper (the agent-plugins.org shape MCP repos ship) is unwrapped."""
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), CONFIG_NAME)
    try:
        with open(path, encoding="utf-8") as f:
            cfg = json.load(f)
    except (OSError, json.JSONDecodeError) as e:
        log(f"cannot read {CONFIG_NAME}: {e}")
        return {}
    if not isinstance(cfg, dict):
        log(f"{CONFIG_NAME} must be an object of server entries")
        return {}
    inner = cfg.get("mcpServers")
    return inner if isinstance(inner, dict) else cfg


def spec_key(spec):
    """Identity of a server entry: any config change — command, args,
    env, url, tools filter — produces a new hash and invalidates the
    cached listing for that server."""
    blob = json.dumps(spec, sort_keys=True)
    return hashlib.sha256(blob.encode()).hexdigest()


def load_cache():
    """Cached tool listings beside this script. Missing or unreadable
    cache costs nothing: every server just starts eagerly once."""
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), CACHE_NAME)
    try:
        with open(path, encoding="utf-8") as f:
            cache = json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}
    return cache if isinstance(cache, dict) else {}


def save_cache(cache):
    """Write-through with an atomic rename, so a killed bridge never
    leaves a half-written cache behind."""
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), CACHE_NAME)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(cache, f)
    os.replace(tmp, path)


def tool_filter(spec, name):
    """The per-server include/exclude from its entry, as a predicate.
    `"tools": {"only": [...], "except": [...]}` — `only` admits the
    named tools (fnmatch patterns against the MCP-side name), `except`
    drops them, both at once meaning only-minus-except. A server that
    ships forty tools costs a schema each round whether or not they are
    wanted, so the cut happens before the mount, not at call time."""
    conf = spec.get("tools") or {}
    only = [p for p in conf.get("only") or [] if isinstance(p, str)]
    except_ = [p for p in conf.get("except") or [] if isinstance(p, str)]

    def admit(tname):
        if only and not any(fnmatch.fnmatch(tname, p) for p in only):
            return False
        return not any(fnmatch.fnmatch(tname, p) for p in except_)

    return admit if (only or except_) else (lambda _: True)


def search_rank(tool, query):
    """Score one listed tool against the query tokens: name hits outweigh
    description hits, an exact name match tops everything. Zero means the
    tool does not match at all."""
    tokens = [t for t in re.split(r"[^\w-]+", query.lower()) if t]
    if not tokens:
        return 1
    tname = (tool.get("name") or "").lower()
    tdesc = (tool.get("description") or "").lower()
    score = 0
    for t in tokens:
        if t == tname:
            score += 8
        elif t in tname:
            score += 4
        elif t in tdesc:
            score += 1
    return score


def deferred_tools(server):
    """The two schemas a deferred server mounts instead of its real list."""
    return [
        {
            "name": f"{server}__search",
            "description": (
                f"search the {server} MCP server's tools by name or purpose; "
                "run this first, then call what it lists"
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "words to match against tool names and descriptions; empty lists everything",
                    },
                    "limit": {
                        "type": "number",
                        "description": "max results (default 8, max 25)",
                    },
                },
            },
            "tier": "exec",
            "server": server,
            "deferred": "search",
        },
        {
            "name": f"{server}__call",
            "description": (
                f"run one {server} MCP tool by name — pass a name from "
                f"{server}__search results and its arguments"
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "the MCP-side tool name"},
                    "arguments": {
                        "type": "object",
                        "description": "the tool's input schema fields",
                        "additionalProperties": True,
                    },
                },
                "required": ["name"],
            },
            "tier": "exec",
            "server": server,
            "deferred": "call",
        },
    ]


def main():
    cfg = load_config()
    cache = load_cache()
    servers = {}
    tools = []
    dirty = False
    cfg_admits = {}
    for name, spec in cfg.items():
        if not isinstance(spec, dict):
            log(f"server '{name}': entry must be an object, skipped")
            continue
        admit = tool_filter(spec, name)
        cfg_admits[name] = admit
        srv = Server(name, spec)
        key = spec_key(spec)
        entry = cache.get(name)
        listing = (entry.get("tools")
                   if isinstance(entry, dict) and entry.get("key") == key
                   else None)
        if listing is None:
            try:
                listing = srv.start()
            except RuntimeError as e:
                log(f"server '{name}' skipped: {e}")
                srv.stop()
                continue
            cache[name] = {"key": key, "tools": listing}
            dirty = True
        if spec.get("defer") is True:
            tools.extend(deferred_tools(name))
            servers[name] = srv
            continue
        for t in listing or []:
            tname = t.get("name")
            if not isinstance(tname, str) or not tname:
                continue
            if not admit(tname):
                continue
            schema = t.get("inputSchema")
            if not (isinstance(schema, dict) and schema.get("type") == "object"):
                schema = {"type": "object", "properties": {},
                          "additionalProperties": True}
            tools.append({
                "name": f"{name}__{tname}",
                "description": (t.get("description") or f"{name} tool {tname}"),
                "parameters": schema,
                # MCP tools run arbitrary code out of process: exec tier,
                # gated only by the blacklist and per-tool policies.
                "tier": "exec",
                "server": name,
                "tool": tname,
            })
        servers[name] = srv
    if dirty:
        save_cache(cache)

    by_tool = {t["name"]: t for t in tools}

    def listed_tools(sname):
        """A deferred server's filtered listing: the live refresh when the
        server happens to be up, else the warm cache. Empty when neither
        has the server at all."""
        admit = cfg_admits.get(sname, lambda _: True)
        srv = servers.get(sname)
        live = srv.listing if srv is not None else None
        entry = cache.get(sname)
        cached = entry.get("tools") if isinstance(entry, dict) else None
        return [t for t in (live or cached or []) if admit(t.get("name"))]

    def reply(obj):
        sys.stdout.write(json.dumps(obj) + "\n")
        sys.stdout.flush()

    # the host's handshake arrives first; answer it with the merged list
    # from the already-started servers (tool_timeout omitted: the config
    # `extensions.tool_timeout` bounds every call)
    initialized = False
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        mid, kind = msg.get("id"), msg.get("type")
        if not initialized:
            if kind == "initialize":
                reply({"id": mid, "result": {
                    "tools": [{k: v for k, v in t.items()
                               if k not in ("server", "tool")} for t in tools],
                    "commands": [],
                    "events": [],
                }})
                initialized = True
            elif kind == "shutdown":
                break
            elif mid is not None:
                reply({"id": mid, "result": None})
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        mid, kind = msg.get("id"), msg.get("type")
        if kind == "shutdown":
            break
        elif kind == "call_tool":
            spec = by_tool.get(msg.get("name"))
            if not spec:
                reply({"id": mid, "error": f"unknown tool {msg.get('name')}"})
                continue
            sname = spec["server"]
            deferred = spec.get("deferred")
            if deferred == "search":
                args = msg.get("args") or {}
                query = args.get("query") if isinstance(args.get("query"), str) else ""
                limit = args.get("limit") if isinstance(args.get("limit"), (int, float)) else 8
                limit = max(1, min(int(limit), 25))
                listing = listed_tools(sname)
                if query:
                    ranked = sorted(
                        (t for t in listing if search_rank(t, query) > 0),
                        key=lambda t: search_rank(t, query),
                        reverse=True,
                    )
                else:
                    ranked = listing
                shown = ranked[:limit]
                if not shown:
                    reply({"id": mid, "result": (
                        f"no {sname} tool matches {query!r} — search a different word, "
                        "or an empty query lists everything" if query else
                        f"the {sname} server exposes no tools")})
                    continue
                lines = [f"{t.get('name')} — {(t.get('description') or '').splitlines()[0][:160]}"
                         for t in shown]
                more = len(ranked) - len(shown)
                reply({"id": mid, "result": "\n".join(lines) + (
                    f"\n…+{more} more (raise limit or narrow the query)" if more > 0 else "")})
                continue
            if deferred == "call":
                args = msg.get("args") or {}
                tname = args.get("name")
                if not isinstance(tname, str) or not tname:
                    reply({"id": mid, "error": "call needs the tool's name (from search results)"})
                    continue
                prefix = f"{sname}__"
                if tname.startswith(prefix):
                    tname = tname[len(prefix):]
                known = {t.get("name") for t in listed_tools(sname)}
                if tname not in known:
                    reply({"id": mid, "error": (
                        f"unknown {sname} tool {tname!r} — run {prefix}search first; "
                        "the list drifts with the server")})
                    continue
                call_args = args.get("arguments") if isinstance(args.get("arguments"), dict) else {}
            else:
                tname, call_args = spec["tool"], msg.get("args") or {}
            srv = servers.get(sname)
            if not srv:
                reply({"id": mid, "error": f"server '{sname}' is down"})
                continue
            if not srv.ensure():
                reply({"id": mid,
                       "error": f"mcp server '{sname}' failed to start"})
                continue
            if srv.listing is not None:
                entry = cache.get(srv.name)
                if isinstance(entry, dict) and entry.get("tools") != srv.listing:
                    entry["tools"] = srv.listing
                    save_cache(cache)
                srv.listing = None
            result = srv.request(
                "tools/call",
                {"name": tname, "arguments": call_args},
                CALL_TIMEOUT)
            if result is None:
                reply({"id": mid,
                       "error": f"mcp server '{sname}' did not answer"})
                continue
            text, err = text_of(result)
            if err is not None:
                reply({"id": mid, "error": err})
            else:
                reply({"id": mid, "result": text})
        elif kind == "interrupt":
            # the host abandoned a call: the transport has already sent
            # notifications/cancelled on its own timeout, so all that is
            # left here is answering the frame and keeping the protocol tidy
            if mid is not None:
                reply({"id": mid, "result": None})
        elif mid is not None:
            reply({"id": mid, "result": None})

    for srv in servers.values():
        srv.stop()


if __name__ == "__main__":
    main()
