#!/usr/bin/env python3
"""Telegram channel for the yak IM bridge: the official Bot API over plain
HTTPS with a 25s `getUpdates` long poll. Unlike weixin (direct connection),
api.telegram.org rides whatever proxy the environment offers, which is how
a mainland machine reaches it.

Setup: create a bot with @BotFather, put the token in im.json:

    "channels": {"telegram": {"token": "123456:ABC…", "owner": "<chat-id>"}}

`owner` is optional: the first chat that messages the bot becomes the owner
and is saved back into im.json.
"""
import json
import sys
import time
import urllib.error
import urllib.request

API_BASE = "https://api.telegram.org/bot"
SEND_TIMEOUT_S = 15.0
LONG_POLL_S = 25
MAX_MESSAGE_CHARS = 4096

# urllib honors http_proxy/https_proxy: api.telegram.org needs that ride,
# unlike the domestic weixin endpoint which bypasses proxies entirely.


class TelegramError(Exception):
    pass


class TelegramChannel:
    name = "telegram"

    def __init__(self, token, on_event=None):
        self.token = token
        self.on_event = on_event  # event(event_dict) → dispatch path
        self.cursor = 0

    def _call(self, method, payload=None, timeout=SEND_TIMEOUT_S):
        url = f"{API_BASE}{self.token}/{method}"
        data = None if payload is None else json.dumps(payload).encode()
        req = urllib.request.Request(url, data=data, method="POST")
        req.add_header("content-type", "application/json")
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            out = json.loads(resp.read().decode("utf-8", "replace"))
        if not out.get("ok"):
            raise TelegramError(f"telegram {method} failed: {out.get('description')}")
        return out.get("result")

    def me(self):
        return self._call("getMe", {}, timeout=10.0)

    def loop(self, inbox, stop):
        """The long-poll loop: inbound messages land in the shared inbox as
        (channel, sender, text, attachments, meta) tuples."""
        while not stop.is_set():
            try:
                updates = self._call("getUpdates",
                                     {"offset": self.cursor, "timeout": LONG_POLL_S},
                                     timeout=LONG_POLL_S + 10.0)
            except TelegramError as e:
                if not stop.is_set():
                    print(f"telegram poll failed: {e}", file=sys.stderr)
                    stop.wait(3.0)
                continue
            for update in updates:
                self.cursor = max(self.cursor, update.get("update_id", 0) + 1)
                message = update.get("message") or {}
                chat = message.get("chat") or {}
                sender = chat.get("id")
                text = message.get("text") or message.get("caption")
                if sender is None or not (text or "").strip():
                    continue
                if self.on_event:
                    try:
                        self.on_event({"channel": self.name, "sender": str(sender),
                                       "text": text, "attachments": [],
                                       "meta": {}})
                    except Exception as e:
                        print(f"telegram dispatch failed: {e}", file=sys.stderr)

    def send(self, sender, text):
        for chunk in self._chunks(text):
            self._call("sendMessage", {"chat_id": sender, "text": chunk})

    def typing(self, sender, running=True):
        try:
            self._call("sendChatAction",
                       {"chat_id": sender, "action": "typing" if running else "cancel"},
                       timeout=8.0)
        except TelegramError:
            pass

    @staticmethod
    def _chunks(text, limit=MAX_MESSAGE_CHARS):
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
