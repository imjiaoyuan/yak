#!/usr/bin/env python3
"""mcp_notion — Notion's MCP server as yak tools, OAuth login included.

Notion's remote MCP endpoint (https://mcp.notion.com/mcp) is
streamable-HTTP and OAuth-only: the docs say "copy this configuration
and complete the OAuth flow when prompted" —

    {"mcpServers": {"notion": {"url": "https://mcp.notion.com/mcp"}}}

but a resident extension has no browser prompt. This one is the whole
journey in a single file: it mounts every tool Notion's server lists
(`notion__notion-search`, `notion__notion-fetch`, ...) and runs the
OAuth flow itself, on demand, through a `notion__login` tool.

First start (no token cached): the extension mounts only `notion__login`.
The model (or you, via a manual `notion__login {}` call) triggers it:

  1. POST the MCP endpoint once unauthenticated: the 401's
     WWW-Authenticate carries the RFC 9728 resource_metadata pointer.
  2. Register a throwaway public client at Notion's /register (RFC 7591),
     redirect http://localhost:8917/callback, no client secret.
  3. Print the authorize URL into the tool log (stderr streams to the
     terminal live) and open it in the default browser; log in, approve.
  4. The one-shot local listener catches the redirect, exchanges code +
     PKCE verifier (S256, form-encoded — Notion 400s a JSON token
     request), and the tokens land in `token.json` beside this file,
     0600. That cache is the whole persistence: re-login overwrites it.
  5. `/reload` (or the next yak start) remounts with the full tool list.

The access token lives ~8h. When it expires the server starts
answering 401 and this extension says so in every tool result: one
`notion__login` round refreshes it (a new token, not a refresh-token
grant — the flow is the same three clicks).

Everything is stdlib on purpose (the yak constraint), every HTTP round
retries transient TLS blips (Cloudflare cuts a handshake now and then),
and the User-Agent is a real name because Notion's CDN firewalls the
python-urllib default (error 1010).
"""

import base64
import hashlib
import http.server
import json
import os
import secrets
import socket
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

ENDPOINT = "https://mcp.notion.com/mcp"
CALLBACK_PORT = 8917     # must match the registered redirect_uri
PROTOCOL_VERSION = "2025-06-18"
UA = "yak-mcp-notion/1 (https://github.com/imjiaoyuan/yak)"
HTTP_TRIES = 3           # transient TLS blips get retried, verdicts do not
INIT_TIMEOUT = 10.0
CALL_TIMEOUT = 60.0
AUTH_WAIT = 300.0        # seconds the login listener waits for the browser
HERE = os.path.dirname(os.path.abspath(__file__))
TOKEN_FILE = os.path.join(HERE, "token.json")


def log(msg):
    """Diagnostics ride stderr; the host dims it into the tool log."""
    sys.stderr.write(f"mcp_notion: {msg}\n")
    sys.stderr.flush()


# -- the token cache --------------------------------------------------------

def load_token():
    """The cached access token, or None. A corrupt cache is deleted loudly
    (fail loudly, not half-work): re-login rebuilds it."""
    try:
        with open(TOKEN_FILE, encoding="utf-8") as f:
            doc = json.load(f)
        tok = doc.get("access_token")
        return tok if isinstance(tok, str) and tok else None
    except FileNotFoundError:
        return None
    except (OSError, ValueError) as e:
        log(f"token cache unreadable ({e}); removing it, re-login")
        try:
            os.unlink(TOKEN_FILE)
        except OSError:
            pass
        return None


def save_token(doc):
    tmp = TOKEN_FILE + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(doc, f)
    os.chmod(tmp, 0o600)
    os.replace(tmp, TOKEN_FILE)


# -- oauth: discovery, registration, the browser round ----------------------

def http_json(url, method="GET", body=None, headers=None, form=False,
              tries=HTTP_TRIES):
    """One HTTP round returning parsed JSON. OAuth token endpoints speak
    RFC 6749 form-encoding (`form`); HTTP errors surface as errors."""
    ctype = "application/x-www-form-urlencoded" if form else "application/json"
    data = (urllib.parse.urlencode(body).encode() if form else
            json.dumps(body).encode() if body is not None else None)
    req = urllib.request.Request(url, data=data, method=method, headers={
        "User-Agent": UA,
        "Accept": "application/json",
        **(headers or {}),
        **({"Content-Type": ctype} if data else {}),
    })
    last = None
    for attempt in range(tries):
        try:
            with urllib.request.urlopen(req, timeout=20) as r:
                raw = r.read()
            break
        except urllib.error.HTTPError:
            raise  # a real HTTP answer is a verdict, not a blip
        except urllib.error.URLError as e:
            last = e
            if attempt < tries - 1:
                time.sleep(1.5)
    else:
        raise RuntimeError(f"cannot reach {url}: {last}")
    return json.loads(raw) if raw.strip() else {}


def discover_auth_server():
    """The endpoint's 401 points at RFC 9728 protected-resource metadata;
    that names the authorization server whose metadata has every endpoint."""
    req = urllib.request.Request(
        ENDPOINT, method="POST",
        headers={"User-Agent": UA,
                 "Accept": "application/json, text/event-stream",
                 "Content-Type": "application/json"},
        data=json.dumps({"jsonrpc": "2.0", "id": 1,
                         "method": "initialize", "params": {}}).encode())
    try:
        urllib.request.urlopen(req, timeout=20)
    except urllib.error.HTTPError as e:
        if e.code != 401:
            raise RuntimeError(f"unexpected HTTP {e.code} from {ENDPOINT}")
        www = e.headers.get("WWW-Authenticate") or ""
    else:
        raise RuntimeError("the endpoint answered without auth")
    meta_url = next((p.split("=", 1)[1].strip('"')
                     for p in www.split(",")
                     if p.strip().startswith("resource_metadata=")), None)
    if not meta_url:
        raise RuntimeError("the 401 carries no resource_metadata pointer")
    resource = http_json(meta_url)
    servers = resource.get("authorization_servers") or []
    if not servers:
        raise RuntimeError("metadata lists no authorization_servers")
    issuer = servers[0]
    return http_json(issuer.rstrip("/") + "/.well-known/oauth-authorization-server")


def register_client(auth_meta):
    return http_json(auth_meta["registration_endpoint"], method="POST", body={
        "client_name": "yak mcp_notion",
        "redirect_uris": [f"http://localhost:{CALLBACK_PORT}/callback"],
        "grant_types": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_method": "none",   # public client: PKCE only
        "response_types": ["code"],
    })


def run_login():
    """The whole browser round. Returns a human-facing summary string."""
    auth_meta = discover_auth_server()
    log(f"authorization server: {auth_meta.get('issuer', 'unknown')}")
    client = register_client(auth_meta)
    redirect_uri = client["redirect_uris"][0]
    log(f"registered client {client['client_id']}")

    state = secrets.token_urlsafe(24)
    verifier = secrets.token_urlsafe(48)
    challenge = base64.urlsafe_b64encode(
        hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
    url = (f"{auth_meta['authorization_endpoint']}?" + urllib.parse.urlencode({
        "response_type": "code",
        "client_id": client["client_id"],
        "redirect_uri": redirect_uri,
        "scope": " ".join(auth_meta.get("scopes_supported") or ["default"]),
        "state": state,
        "code_challenge": challenge,
        "code_challenge_method": "S256",
    }))

    got = {}

    class CB(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            got.update({k: v[0] for k, v in
                        urllib.parse.parse_qs(
                            urllib.parse.urlparse(self.path).query).items()})
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            ok = "code" in got and got.get("state") == state
            self.wfile.write(
                f"<html><body><h3>OAuth "
                f"{'done — return to the terminal.' if ok else 'failed — check the terminal.'}"
                f"</h3></body></html>".encode())

        def log_message(self, *_):
            pass

    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            probe.bind(("127.0.0.1", CALLBACK_PORT))
        except OSError:
            return (f"port {CALLBACK_PORT} is busy — close whatever holds it "
                    f"(the OAuth redirect must land there)")
    server = http.server.HTTPServer(("127.0.0.1", CALLBACK_PORT), CB)
    threading.Thread(target=server.handle_request, daemon=True).start()

    log("open this URL in a browser, log into Notion, approve:")
    sys.stderr.write(f"\n  {url}\n\n")
    sys.stderr.flush()
    import webbrowser
    try:
        webbrowser.open(url)
    except Exception:
        pass  # headless: the printed URL is the manual path

    deadline = time.monotonic() + AUTH_WAIT
    while not got and time.monotonic() < deadline:
        time.sleep(0.2)
    server.server_close()
    if "error" in got:
        return f"authorization failed: {got.get('error')}"
    if not got:
        return (f"no redirect arrived within {AUTH_WAIT:.0f}s — "
                f"did the approval page open?")
    if got.get("state") != state:
        return "state mismatch at the callback — aborting"

    tokens = http_json(auth_meta["token_endpoint"], method="POST", form=True,
                       body={"grant_type": "authorization_code",
                             "code": got["code"],
                             "redirect_uri": redirect_uri,
                             "client_id": client["client_id"],
                             "code_verifier": verifier})
    if not tokens.get("access_token"):
        raise RuntimeError(f"token response held no access_token: {tokens}")
    save_token(tokens)
    ttl = tokens.get("expires_in")
    return ("login OK — token cached ("
            + (f"expires in {ttl}s" if ttl else "unknown ttl")
            + "). /reload now mounts the full tool list.")


# -- the mcp client ----------------------------------------------------------

class McpSession:
    """Streamable HTTP: one POST per JSON-RPC message, the reply either a
    JSON body or an SSE stream carrying it; Mcp-Session-Id rides along."""

    def __init__(self, token):
        self.token = token
        self.session_id = None
        self.next_id = 0

    def _post(self, frame, timeout):
        headers = {"Authorization": f"Bearer {self.token}",
                   "Content-Type": "application/json",
                   "Accept": "application/json, text/event-stream",
                   "MCP-Protocol-Version": PROTOCOL_VERSION,
                   "User-Agent": UA}
        if self.session_id:
            headers["Mcp-Session-Id"] = self.session_id
        req = urllib.request.Request(
            ENDPOINT, data=json.dumps(frame).encode(), headers=headers,
            method="POST")
        last = None
        for attempt in range(HTTP_TRIES):
            try:
                r = urllib.request.urlopen(req, timeout=timeout)
                break
            except urllib.error.HTTPError:
                raise  # 401/4xx are verdicts the caller names
            except urllib.error.URLError as e:
                last = e
                if attempt < HTTP_TRIES - 1:
                    time.sleep(1.5)
        else:
            raise RuntimeError(f"cannot reach the MCP endpoint: {last}")
        with r:
            sid = r.headers.get("Mcp-Session-Id")
            if sid:
                self.session_id = sid
            ctype = (r.headers.get("Content-Type") or
                     "").split(";")[0].strip().lower()
            if ctype == "text/event-stream":
                return self._sse(r, frame.get("id"))
            body = r.read(8 * 1024 * 1024)
        return json.loads(body) if body.strip() else None

    @staticmethod
    def _sse(stream, rid):
        data = []
        for line in stream:
            line = line.decode("utf-8", "replace").rstrip("\r\n")
            if not line:
                if data:
                    msg = json.loads("\n".join(data))
                    data = []
                    if msg.get("id") == rid:
                        return msg
                continue
            if line.startswith("data:"):
                data.append(line[5:].lstrip())
        return None

    def request(self, method, params, timeout):
        self.next_id += 1
        rid = self.next_id
        frame = {"jsonrpc": "2.0", "id": rid,
                 "method": method, "params": params}
        reply = self._post(frame, timeout)
        if reply is None:
            raise RuntimeError("the server did not answer")
        if "error" in reply:
            raise RuntimeError(f"server error: {reply['error'].get('message')}")
        return reply.get("result")


def text_of(result):
    """MCP tool content → plain text, plus a Notion citation url if any."""
    parts = []
    for c in (result or {}).get("content") or []:
        if isinstance(c, dict) and c.get("type") == "text" and c.get("text"):
            parts.append(c["text"])
    return "\n".join(parts) if parts else json.dumps(result, ensure_ascii=False)


def mount(session):
    """initialize + tools/list → the resident tool list for the host."""
    session.request("initialize", {
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "yak-mcp-notion", "version": "1"},
    }, INIT_TIMEOUT)
    listing = session.request("tools/list", {}, INIT_TIMEOUT)
    tools = []
    for t in listing.get("tools") or []:
        name = t.get("name")
        if not isinstance(name, str) or not name:
            continue
        schema = t.get("inputSchema")
        if not (isinstance(schema, dict) and schema.get("type") == "object"):
            schema = {"type": "object", "properties": {},
                      "additionalProperties": True}
        tools.append({"name": f"notion__{name}",
                      "description": t.get("description") or f"notion tool {name}",
                      "parameters": schema,
                      # MCP tools act on the user's Notion workspace out of
                      # process: exec tier, gated by the approval matrix
                      "tier": "exec"})
    return tools, session


# -- the resident protocol loop ----------------------------------------------

def main():
    token = load_token()
    login_only = {"name": "notion__login", "description":
                  "Run the Notion OAuth login: prints the authorize URL, "
                  "waits for the browser approval, caches the token. "
                  "Call with no arguments when tools report 'not logged in'.",
                  "parameters": {"type": "object", "properties": {},
                                 "additionalProperties": False},
                  "tier": "exec"}
    if not token:
        tools, session = [login_only], None
    else:
        session = McpSession(token)
        try:
            tools, session = mount(session)
        except Exception as e:
            # a dead/expired token still mounts the login tool (that is the
            # fix for it); the failure is loud, not papered over
            log(f"notion tools not mounted ({e}); only notion__login is up")
            tools, session = [login_only], None

    def reply(obj):
        sys.stdout.write(json.dumps(obj) + "\n")
        sys.stdout.flush()

    initialized = False
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        mid, kind = msg.get("id"), msg.get("type")
        if kind == "initialize":
            reply({"id": mid, "result": {"tools": tools, "commands": [],
                                         "events": []}})
            initialized = True
            continue
        if not initialized or kind != "call_tool":
            if mid is not None:
                reply({"id": mid, "result": None})
            continue
        tool = msg.get("name") or ""

        if tool == "notion__login":
            try:
                reply({"id": mid, "result": run_login()})
            except Exception as e:
                reply({"id": mid, "error": f"login failed: {e}"})
            continue
        if session is None:
            reply({"id": mid, "error":
                   "not logged into Notion — call notion__login first "
                   "(no arguments), then /reload"})
            continue
        mcp_name = tool[len("notion__"):] if tool.startswith("notion__") else tool
        try:
            result = session.request(
                "tools/call", {"name": mcp_name,
                               "arguments": msg.get("args") or {}},
                CALL_TIMEOUT)
            reply({"id": mid, "result": text_of(result)})
        except urllib.error.HTTPError as e:
            hint = (" — token expired? run notion__login, then /reload"
                    if e.code == 401 else "")
            reply({"id": mid, "error": f"notion http {e.code}{hint}"})
        except Exception as e:
            reply({"id": mid, "error": str(e)})


if __name__ == "__main__":
    main()
