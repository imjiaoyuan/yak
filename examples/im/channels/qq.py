#!/usr/bin/env python3
"""QQ channel for the yak IM bridge: the official QQ bot platform.

Binding: `yak-im login` with channels.qq present — a QR appears, scan it
with the QQ app and confirm; the platform hands back appId/appSecret and
the scanner becomes the owner. Credentials land in im.json:

    "channels": {"qq": {"app_id": "…", "app_secret": "…"}}

Inbound: WebSocket gateway with identify/heartbeat; group chats answer
@mentions, private chats answer directly. Outbound is a passive reply
(must quote the triggering msg_id, msg_seq increments per id) — the
platform restricts proactive messages, so the bot only speaks after being
spoken to. No typing indicator exists on this API.
"""
import base64
import json
import os
import re
import socket
import ssl
import sys
import threading
import time
import urllib.error
import urllib.request

from channels.wsclient import WebSocket, WebSocketClosed

REST_BASE = "https://api.sgroup.qq.com"
TOKEN_URL = "https://bots.qq.com/app/getAppAccessToken"
GATEWAY_URL = "wss://api.sgroup.qq.com/websocket"
BIND_BASE = "https://q.qq.com"
INTENTS = 1 << 25  # GROUP_AND_C2C_SERVER_EVENTS
GROUP_AT = "GROUP_AT_MESSAGE_CREATE"
C2C_AT = "C2C_MESSAGE_CREATE"
MENTION_PREFIX = re.compile(r"^<@!\d+>\s*")


class QQError(Exception):
    pass


def _post(url, body, headers=None, timeout=10.0):
    req = urllib.request.Request(url, data=json.dumps(body).encode(), method="POST")
    req.add_header("content-type", "application/json")
    req.add_header("Accept", "application/json")
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8", "replace"))


# --- binding (QR) -------------------------------------------------------------

def create_bind_task():
    key = base64.b64encode(os.urandom(32)).decode()
    data = _post(f"{BIND_BASE}/lite/create_bind_task", {"key": key})
    if data.get("retcode") != 0 or not (data.get("data") or {}).get("task_id"):
        raise QQError(data.get("msg") or "create_bind_task failed")
    return data["data"]["task_id"], key


def connect_url(task_id, source="yak-im"):
    return (f"{BIND_BASE}/qqbot/openclaw/connect.html?task_id={task_id}"
            f"&source={source}&_wv=2")


def poll_bind_result(task_id, key):
    data = _post(f"{BIND_BASE}/lite/poll_bind_result", {"task_id": task_id})
    if data.get("retcode") != 0:
        raise QQError(data.get("msg") or "poll_bind_result failed")
    payload = data.get("data") or {}
    return {
        "status": payload.get("status") or 0,
        "app_id": str(payload.get("bot_appid") or ""),
        "secret_b64": str(payload.get("bot_encrypt_secret") or ""),
        "owner": payload.get("user_openid") or None,
    }


def decrypt_secret(key_b64, secret_b64):
    """AES-256-GCM: base64 blob = 12-byte nonce | ciphertext | 16-byte tag,
    keyed by the random key the bind task was created with."""
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    key = base64.b64decode(key_b64)
    blob = base64.b64decode(secret_b64)
    nonce, body = blob[:12], blob[12:]
    return AESGCM(key).decrypt(nonce, body, None).decode("utf-8")


def qq_login():
    """The QR binding loop: returns credentials or None. Renders the
    connect URL as a terminal QR via the `qrencode` CLI."""
    import subprocess
    task_id, key = create_bind_task()
    url = connect_url(task_id)
    try:
        out = subprocess.run(["qrencode", "-t", "ANSIUTF8", url],
                             capture_output=True, text=True, timeout=10)
        if out.returncode == 0:
            print(out.stdout, flush=True)
        else:
            print(f"scan URL with the QQ app: {url}", flush=True)
    except (OSError, subprocess.SubprocessError):
        print(f"install `qrencode` for a terminal QR, or open the URL with the QQ app: {url}")
    print("scan with the QQ app and confirm the bot binding…", flush=True)
    deadline = time.time() + 180
    while time.time() < deadline:
        try:
            out = poll_bind_result(task_id, key)
        except QQError as e:
            print(f"bind poll failed: {e}")
            return None
        status = out["status"]
        if status == 2:  # COMPLETED
            try:
                app_secret = decrypt_secret(key, out["secret_b64"])
            except Exception as e:
                print(f"secret decrypt failed: {e}", flush=True)
                return None
            return {"app_id": out["app_id"], "app_secret": app_secret,
                    "owner": out.get("owner")}
        if status == 3:  # EXPIRED
            print("QR expired — rerun `yak-im login`.", flush=True)
            return None
        if status == 1:
            print("scanned — confirm on the phone…", flush=True)
        time.sleep(2.0)
    print("login timed out.", flush=True)
    return None


# --- runtime ------------------------------------------------------------------

def _get_access_token(app_id, app_secret):
    data = _post(TOKEN_URL, {"appId": app_id, "clientSecret": app_secret})
    token = data.get("access_token")
    if not token:
        raise QQError(f"access token refused: {data.get('msg') or data}")
    expires = float(data.get("expires_in") or 7200)
    return token, time.time() + expires - 300


class QQChannel:
    name = "qq"

    def __init__(self, app_id, app_secret, owner=None, on_event=None):
        self.app_id = app_id
        self.app_secret = app_secret
        self.owner = owner
        self.on_event = on_event
        self.bridge = None
        self.token = None
        self.token_until = 0
        self.seq = 0
        self.ws = None
        self.msg_seq = {}
        self.heartbeat_interval = None
        self.last_seq = 0

    def _token(self, force=False):
        if force or self.token is None or time.time() >= self.token_until:
            self.token, self.token_until = _get_access_token(self.app_id, self.app_secret)
        return self.token

    def _rest(self, method, path, body, retry_auth=True):
        headers = {"Authorization": f"QQBot {self._token()}"}
        url = REST_BASE + path
        req = urllib.request.Request(url, data=json.dumps(body).encode(), method=method)
        for k, v in headers.items():
            req.add_header(k, v)
        try:
            with urllib.request.urlopen(req, timeout=15.0) as resp:
                return json.loads(resp.read().decode("utf-8", "replace"))
        except urllib.error.HTTPError as e:
            body_text = e.read().decode("utf-8", "replace")[:300]
            if e.code in (401, 403) and retry_auth:
                self._token(force=True)
                return self._rest(method, path, body, retry_auth=False)
            raise QQError(f"{method} {path} → HTTP {e.code}: {body_text}") from None

    # -- lifecycle -------------------------------------------------------------

    def loop(self, inbox, stop):
        """Connect → identify → heartbeat + dispatch loop. On a drop, the
        token refreshes and the whole gateway reconnects."""
        while not stop.is_set():
            try:
                self._run_connection(inbox, stop)
            except (WebSocketClosed, QQError, OSError, ssl.SSLError) as e:
                if stop.is_set():
                    return
                print(f"qq gateway dropped ({e}) — reconnecting", file=sys.stderr)
                stop.wait(3.0)
            except Exception as e:
                if stop.is_set():
                    return
                print(f"qq gateway crashed: {e!r} — reconnecting", file=sys.stderr)
                stop.wait(3.0)
        if self.ws is not None:
            self.ws.close()

    def _run_connection(self, inbox, stop):
        self._token()
        self.ws = WebSocket(GATEWAY_URL, timeout=40.0)
        hello = json.loads(self.ws.recv())
        if hello.get("op") != 10:
            raise QQError(f"expected hello, got {hello}")
        interval = max(5.0, hello["d"]["heartbeat_interval"] / 1000.0 - 1.0)
        self.ws.send_text(json.dumps({
            "op": 2,
            "d": {"token": f"QQBot {self._token()}", "intents": INTENTS,
                  "shard": [0, 1]},
        }))
        print("qq channel up")
        heartbeat_stop = threading.Event()
        threading.Thread(target=self._heartbeat, daemon=True,
                         args=(heartbeat_stop, interval)).start()
        try:
            while not stop.is_set():
                frame = json.loads(self.ws.recv())
                op = frame.get("op")
                if op == 11:
                    continue  # heartbeat ack
                if op == 0:
                    self.last_seq = frame.get("s") or self.last_seq
                    self._dispatch(frame.get("t"), frame.get("d") or {}, inbox)
        finally:
            heartbeat_stop.set()
            self.ws.close()
            self.ws = None

    def _heartbeat(self, stop, interval):
        while not stop.wait(interval):
            try:
                self.ws.send_text(json.dumps({"op": 1, "d": self.last_seq or None}))
            except Exception as e:
                print(f"qq heartbeat failed: {e}", file=sys.stderr)
                return

    def _dispatch(self, event_type, payload, inbox):
        if event_type not in (GROUP_AT, C2C_AT):
            return
        content = MENTION_PREFIX.sub("", payload.get("content") or "").strip()
        msg_id = payload.get("msg_id") or ""
        if event_type == GROUP_AT:
            sender = payload.get("group_openid") or ""
            meta = {"chat_type": "group", "chat_id": payload.get("group_openid"),
                    "msg_id": msg_id,
                    "author": (payload.get("author") or {}).get("id")}
        else:
            sender = payload.get("user_openid") or (payload.get("author") or {}).get("id")
            meta = {"chat_type": "c2c", "chat_id": sender, "msg_id": msg_id,
                    "author": (payload.get("author") or {}).get("id")}
        if not sender:
            return
        if self.on_event:
            try:
                self.on_event({"channel": self.name, "sender": sender,
                               "text": content, "attachments": [], "meta": meta})
            except Exception as e:
                print(f"qq dispatch failed: {e}", file=sys.stderr)

    # -- outbound ---------------------------------------------------------------

    def send(self, sender, text):
        """A passive reply quoting the chat's last inbound msg_id — proactive
        messages are restricted by the platform."""
        chat = self.bridge.chat(f"qq:{sender}") if self.bridge else None
        meta = getattr(chat, "channel_meta", None) or {}
        msg_id = meta.get("msg_id")
        chat_type = meta.get("chat_type")
        if not msg_id or not chat_type or not chat_type in ("group", "c2c"):
            raise QQError(
                f"no recent message to reply to (open id {sender}) — "
                "QQ only allows passive replies; say something in the chat first")
        seq = self.msg_seq.get(msg_id, 0) + 1
        self.msg_seq[msg_id] = seq
        path = (f"/v2/groups/{sender}/messages" if chat_type == "group"
                else f"/v2/users/{sender}/messages")
        for chunk in self._chunks(text):
            self._rest("POST", path,
                       {"content": chunk, "msg_id": msg_id, "msg_seq": seq})
            seq += 1

    @staticmethod
    def _chunks(text, limit=1800):
        text = (text or "").strip()
        if not text:
            return []
        chunks = []
        while len(text) > limit:
            cut = text.rfind("\n", 0, limit)
            if cut < limit // 2:
                cut = limit
            chunks.append(text[:cut].rstrip())
            text = text[cut:].lstrip()
        chunks.append(text)
        return chunks

    def typing(self, sender, running):
        pass
