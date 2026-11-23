#!/usr/bin/env python3
"""Feishu channel for the yak IM bridge.

Requires the official SDK (`pip install lark-oapi`) — Feishu's long
connection speaks a protobuf-framed protocol that is not worth re-implementing
for one channel, so this is the one place the bridge allows a dependency, and
only when the feishu channel is actually enabled:

    "channels": {"feishu": {"app_id": "cli_…", "app_secret": "…",
                            "owner": "<open-id>"}}

Events arrive over the SDK's WebSocket client (no public URL needed);
replies go through the im/v1 message API to the chat that spoke. As with
Telegram, the first chat to message the bot becomes the owner when `owner`
is unset.
"""
import json


class FeishuChannel:
    name = "feishu"

    def __init__(self, app_id, app_secret, on_event=None):
        try:
            import lark_oapi as lark
        except ImportError as e:
            raise RuntimeError(
                "the feishu channel needs the official SDK: pip install lark-oapi") from e
        self.name = "feishu"
        self.on_event = on_event
        self.lark = lark
        self.client = (lark.Client.builder()
                       .app_id(app_id).app_secret(app_secret).build())
        self.ws = lark.ws.Client(app_id, app_secret,
                                 event_handler=self._event_dispatch,
                                 log_level=lark.LogLevel.WARNING)

    def _event_dispatch(self, data):
        """P2ImMessageReceiveV1 → the shared inbox tuple."""
        try:
            message = data.event.message
            sender = message.chat_id
            content = json.loads(message.content or "{}")
            text = content.get("text") or ""
            kind = message.message_type
            if self.on_event:
                attachments = [] if kind == "text" else [f"{kind}"]
                self.on_event({"channel": self.name, "sender": sender,
                               "text": text.strip(), "attachments": attachments,
                               "meta": {}})
        except Exception as e:
            print(f"feishu dispatch failed: {e}")

    def loop(self, inbox, stop):
        """The SDK owns its own reconnect loop; `.start()` blocks this thread
        until the connection dies, and `stop` cannot interrupt it — stopping
        the feishu channel means restarting the daemon."""
        self._inbox = inbox
        self.ws.start()

    def _reply(self, chat_id, text):
        lark = self.lark
        from lark_oapi.api.im.v1 import (CreateMessageRequest,
                                         CreateMessageRequestBody)
        request = (CreateMessageRequest.builder()
                   .receive_id_type("chat_id")
                   .request_body(CreateMessageRequestBody.builder()
                                 .receive_id(chat_id)
                                 .content(json.dumps({"text": text}))
                                 .build())
                   .build())
        response = self.client.im.v1.message.create(request)
        if not response.success():
            print(f"feishu send failed: {response.code} {response.msg}")

    def send(self, sender, text):
        # feishu caps a text message at ~150 KB — chunking at the same 4000
        # keeps parity with the other channels without any real risk
        chunks = []
        body = (text or "").strip()
        while len(body) > 4000:
            chunks.append(body[:4000])
            body = body[4000:]
        chunks.append(body)
        for chunk in chunks:
            self._reply(sender, chunk)

    def typing(self, sender, running=True):
        pass  # feishu has no typing indicator on the bot API
