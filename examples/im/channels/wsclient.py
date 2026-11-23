#!/usr/bin/env python3
"""Minimal RFC 6455 WebSocket client (stdlib only): enough for the QQ bot
gateway — one text/binary channel, server pings answered with pongs,
close frames raise. Client frames are always masked, per the spec."""
import base64
import hashlib
import os
import socket
import ssl
import struct
import urllib.parse

GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


class WebSocketClosed(Exception):
    pass


class WebSocket:
    def __init__(self, url, timeout=10.0):
        u = urllib.parse.urlsplit(url)
        if u.scheme not in ("ws", "wss"):
            raise ValueError("ws/wss URL required")
        self.secure = u.scheme == "wss"
        self.host = u.hostname
        self.port = u.port or (443 if self.secure else 80)
        self.path = u.path + (f"?{u.query}" if u.query else "")
        self.timeout = timeout
        self.sock = None
        self._connect()

    def _connect(self):
        raw = socket.create_connection((self.host, self.port), timeout=self.timeout)
        if self.secure:
            ctx = ssl.create_default_context()
            raw = ctx.wrap_socket(raw, server_hostname=self.host)
        raw.settimeout(self.timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        req = (f"GET {self.path} HTTP/1.1\r\n"
               f"Host: {self.host}\r\n"
               "Upgrade: websocket\r\nConnection: Upgrade\r\n"
               f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")
        raw.sendall(req.encode())
        response = b""
        while b"\r\n\r\n" not in response:
            chunk = raw.recv(4096)
            if not chunk:
                raise WebSocketClosed("handshake: connection closed")
            response += chunk
            if len(response) > 65536:
                raise WebSocketClosed("handshake: response too large")
        head, _, rest = response.partition(b"\r\n\r\n")
        status = head.split(b"\r\n")[0].decode("latin-1")
        if " 101 " not in status + " ":
            raise WebSocketClosed(f"handshake rejected: {status}")
        expected = base64.b64encode(
            hashlib.sha1((key + GUID).encode()).digest()).decode()
        if expected.encode() not in head:
            raise WebSocketClosed("handshake: bad Sec-WebSocket-Accept")
        self.sock = raw
        self.buffer = rest

    def _read_exact(self, n):
        while len(self.buffer) < n:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise WebSocketClosed("connection closed")
            self.buffer += chunk
        out, self.buffer = self.buffer[:n], self.buffer[n:]
        return out

    def _recv_frame(self):
        b1, b2 = self._read_exact(2)
        fin, opcode = bool(b1 & 0x80), b1 & 0x0F
        masked, length = bool(b2 & 0x80), b2 & 0x7F
        if length == 126:
            (length,) = struct.unpack(">H", self._read_exact(2))
        elif length == 127:
            (length,) = struct.unpack(">Q", self._read_exact(8))
        mask = self._read_exact(4) if masked else None
        payload = self._read_exact(length)
        if mask:
            payload = bytes(p ^ mask[i % 4] for i, p in enumerate(payload))
        return fin, opcode, payload

    def recv(self):
        """The next text/binary message, reassembling continuation frames.
        Pings are answered with pongs, pongs dropped, close raises."""
        message = bytearray()
        while True:
            fin, opcode, payload = self._recv_frame()
            if opcode == 0x9:  # ping
                self._send_frame(0xA, payload)
                continue
            if opcode == 0xA:  # pong
                continue
            if opcode == 0x8:  # close
                self.close()
                raise WebSocketClosed("server sent close")
            if opcode in (0x1, 0x2, 0x0):
                message += payload
                if fin:
                    return bytes(message)

    def _send_frame(self, opcode, payload):
        if self.sock is None:
            raise WebSocketClosed("socket is closed")
        mask = os.urandom(4)
        header = bytes([0x80 | opcode])
        n = len(payload)
        if n < 126:
            header += bytes([0x80 | n])
        elif n < 65536:
            header += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            header += bytes([0x80 | 127]) + struct.pack(">Q", n)
        masked = bytes(p ^ mask[i % 4] for i, p in enumerate(payload))
        self.sock.sendall(header + mask + masked)

    def send_text(self, text):
        self._send_frame(0x1, text.encode("utf-8"))

    def send_pong(self, payload=b""):
        self._send_frame(0xA, payload)

    def close(self):
        if self.sock is not None:
            try:
                self._send_frame(0x8, b"")
                self.sock.close()
            except OSError:
                pass
            self.sock = None
