#!/usr/bin/env python3
"""Weixin (personal-WeChat bot) channel for the yak IM bridge.

Protocol endpoints and message shapes adapted from xmanrui/dsh-im (MIT).
The transport is WeChat's ilink bot platform: QR-code binding issues a bot
token, inbound messages arrive on a 35s long poll, replies go out as
`sendmessage` posts with the chat's `context_token` from the last inbound
message. No public endpoint is involved anywhere.
"""
import base64
import json
import os
import random
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request

BASE_URL = "https://ilinkai.weixin.qq.com/"
PROTOCOL_VERSION = "2.4.6"
BOT_TYPE = "3"
LONG_POLL_TIMEOUT_MS = 35_000
SEND_TIMEOUT_MS = 15_000
LOGIN_TIMEOUT_MS = 35_000
TEXT_SEND_RETRIES = (0.4, 1.2)
TRUSTED_HOST = re.compile(r"(^|\.)weixin\.qq\.com$")
LOGIN_STATUSES = {
    "wait", "scaned", "confirmed", "expired", "scaned_but_redirect",
    "need_verifycode", "verify_code_blocked", "binded_redirect",
}


class WeixinError(Exception):
    def __init__(self, code, message, provider_code=None):
        super().__init__(message)
        self.code = code
        self.provider_code = provider_code


def _clean(value):
    return value.strip() if isinstance(value, str) and value.strip() else None


def _provider_code(value, keys=("errcode", "ret")):
    for k in keys:
        code = value.get(k) if isinstance(value, dict) else None
        if code is not None and code != 0:
            return str(code)
    return None


APP_ID = "bot"
CLIENT_VERSION = str((2 << 16) | (4 << 8) | 6)


def _headers(token=None, authenticated=True):
    """The platform routes on the two iLink headers — without them requests
    hang. Authenticated requests add the JSON content type, a random UIN
    (never interpreted) and the bearer token; the QR endpoints pre-auth go
    out bare, exactly as the reference client sends them."""
    headers = {"iLink-App-Id": APP_ID, "iLink-App-ClientVersion": CLIENT_VERSION}
    if not authenticated:
        return headers
    headers["content-type"] = "application/json"
    headers["AuthorizationType"] = "ilink_bot_token"
    headers["X-WECHAT-UIN"] = base64.b64encode(str(random.getrandbits(32)).encode()).decode()
    if _clean(token):
        headers["Authorization"] = f"Bearer {token.strip()}"
    return headers


def _base_info():
    return {"channel_version": PROTOCOL_VERSION, "bot_agent": "yak-im/0.1"}


# ilinkai is a domestic endpoint: bypass http_proxy/https_proxy entirely —
# a shell's international proxy is exactly what makes these requests hang
_opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def _request(method, endpoint, body=None, token=None, timeout_ms=15_000, base_url=BASE_URL,
             authenticated=True):
    url = base_url.rstrip("/") + "/" + endpoint.lstrip("/")
    if not TRUSTED_HOST.search(url.split("/", 3)[2]):
        raise WeixinError("untrusted-endpoint", f"refusing non-weixin endpoint {url}")
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method)
    for k, v in _headers(token if authenticated else None).items():
        req.add_header(k, v)
    try:
        with _opener.open(req, timeout=timeout_ms / 1000.0) as resp:
            return json.loads(resp.read().decode("utf-8", "replace"))
    except urllib.error.HTTPError as e:
        raise WeixinError("http-error", f"weixin service returned HTTP {e.code}",
                          provider_code=str(e.code)) from None
    except urllib.error.URLError as e:
        reason = str(getattr(e, "reason", ""))
        if "timed out" in reason.lower():
            raise WeixinError("timeout", f"weixin request timed out: {url}") from None
        raise WeixinError("network-error", "cannot reach the weixin service") from None
    except TimeoutError:
        raise WeixinError("timeout", f"weixin request timed out: {url}") from None


def request_json(method, endpoint, body=None, token=None, timeout_ms=15_000, base_url=BASE_URL,
                 authenticated=True, retry_network=False):
    """`_request`, optionally retrying bare network failures (a send that got
    no HTTP response at all is repeated; a timeout or HTTP error may already
    have been delivered, so only `network-error` retries)."""
    attempt = 0
    while True:
        try:
            return _request(method, endpoint, body, token, timeout_ms, base_url, authenticated)
        except WeixinError as e:
            if e.code != "network-error" or not retry_network \
                    or attempt >= len(TEXT_SEND_RETRIES):
                raise
            time.sleep(TEXT_SEND_RETRIES[attempt])
            attempt += 1


def begin_login():
    data = request_json("POST", f"ilink/bot/get_bot_qrcode?bot_type={BOT_TYPE}",
                        body={"local_token_list": []}, authenticated=False, timeout_ms=10_000)
    code = _provider_code(data)
    if code:
        raise WeixinError("qr-rejected", "weixin refused the QR request", provider_code=code)
    qrcode = _clean(data.get("qrcode"))
    if not qrcode:
        raise WeixinError("invalid-qr", "weixin returned no QR token")
    return qrcode, _clean(data.get("qrcode_img_content"))


def poll_login(qrcode, verify_code=None):
    endpoint = f"ilink/bot/get_qrcode_status?qrcode={qrcode}"
    if _clean(verify_code):
        endpoint += f"&verify_code={_clean(verify_code)}"
    data = request_json("GET", endpoint, timeout_ms=LOGIN_TIMEOUT_MS)
    status = data.get("status")
    if status not in LOGIN_STATUSES:
        raise WeixinError("invalid-login-status", "unrecognized QR login status")
    out = {"status": status}
    if status == "confirmed":
        out["token"] = _clean(data.get("bot_token"))
        out["bot_id"] = _clean(data.get("ilink_bot_id"))
        out["owner"] = _clean(data.get("ilink_user_id"))
        out["base_url"] = _clean(data.get("baseurl")) or BASE_URL
    return out


def get_updates(token, cursor, timeout_s=35.0):
    """One long poll. A server-side timeout is an empty page, not an error."""
    try:
        data = request_json("POST", "ilink/bot/getupdates", timeout_ms=timeout_s * 1000, token=token,
                            body={"get_updates_buf": cursor or "", "base_info": _base_info()})
    except WeixinError as e:
        if e.code == "timeout":
            return [], cursor
        raise
    if data.get("ret") not in (None, 0):
        raise WeixinError("updates-rejected", "weixin rejected the long poll",
                          provider_code=str(data.get("ret")))
    return data.get("msgs") or [], data.get("get_updates_buf") or cursor


def get_config(token, to_user):
    data = request_json("POST", "ilink/bot/getconfig", token=token, timeout_ms=10_000,
                        body={"ilink_user_id": to_user, "base_info": _base_info()})
    if data.get("ret") not in (None, 0):
        raise WeixinError("config-rejected", "weixin refused the bot config request")
    return _clean(data.get("typing_ticket"))


def send_typing(token, to_user, ticket, status):
    if status not in (1, 2):
        raise ValueError("typing status must be 1 or 2")
    data = request_json("POST", "ilink/bot/sendtyping", token=token, timeout_ms=10_000,
                        body={"ilink_user_id": to_user, "typing_ticket": ticket,
                              "status": status, "base_info": _base_info()})
    if data.get("ret") not in (None, 0):
        raise WeixinError("typing-rejected", "weixin refused the typing status")


def send_text(token, to_user, text, context_token=None, run_id=None):
    body = {
        "msg": {
            "from_user_id": "",
            "to_user_id": to_user,
            "client_id": f"yak-im-{int(time.time() * 1000)}-{random.randint(0, 1 << 30)}",
            "message_type": 2,
            "message_state": 2,
            "item_list": [{"type": 1, "text_item": {"text": text}}],
            **({"context_token": context_token} if context_token else {}),
            **({"run_id": run_id} if run_id else {}),
        },
        "base_info": _base_info(),
    }
    data = request_json("POST", "ilink/bot/sendmessage", body=body, token=token,
                        timeout_ms=SEND_TIMEOUT_MS, retry_network=True)
    code = _provider_code(data)
    if code:
        raise WeixinError("send-rejected", "weixin refused the reply message", provider_code=code)
    return data


def notify(token, start):
    endpoint = "ilink/bot/msg/notifystart" if start else "ilink/bot/msg/notifystop"
    data = request_json("POST", endpoint, token=token, timeout_ms=10_000,
                        body={"base_info": _base_info()})
    code = _provider_code(data)
    if code:
        raise WeixinError("stale-token" if code == "-14" else "start-rejected",
                          "weixin account connection failed", provider_code=code)
    return data


def extract_message(message):
    """The inbound fields the bridge routes on: sender, text (typed or
    server-transcribed voice), the chat's context token, and what non-text
    items the message carried (images/files are reported, not fetched)."""
    sender = _clean(message.get("from_user_id"))
    context_token = _clean(message.get("context_token"))
    msg_type = message.get("message_type")
    text = None
    attachments = []
    for item in message.get("item_list") or []:
        if item.get("type") == 1 and isinstance(item.get("text_item"), dict):
            text = _clean(item["text_item"].get("text")) or text
        elif item.get("type") == 3 and isinstance(item.get("voice_item"), dict):
            text = _clean(item["voice_item"].get("text")) or text
        elif item.get("type") == 2:
            attachments.append("image")
        elif item.get("type") == 4:
            name = _clean((item.get("file_item") or {}).get("file_name")) or "file"
            attachments.append(f"file {name}")
    return {
        "sender": sender,
        "text": text,
        "context_token": context_token,
        "echo": msg_type == 2,
        "attachments": attachments,
    }


def open_binding_page(image_url):
    """The binding QR lives on this page: open it in a desktop browser and
    scan the screen with WeChat — or open the URL in WeChat on the phone."""
    opener = ("open" if sys.platform == "darwin"
              else "start" if sys.platform == "win32" else "xdg-open")
    try:
        subprocess.Popen([opener, image_url],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        print(f"binding page opened in your browser — scan the QR with WeChat")
    except OSError:
        pass
    print(f"binding page URL (open in a browser, scan with WeChat; or open "
          f"it in WeChat on the phone): {image_url}")


def extract_message(message):
    """The inbound fields the bridge routes on: sender, text (typed or
    server-transcribed voice), the chat's context token, and what non-text
    items the message carried (images/files are reported, not fetched)."""
    sender = _clean(message.get("from_user_id"))
    context_token = _clean(message.get("context_token"))
    msg_type = message.get("message_type")
    text = None
    attachments = []
    for item in message.get("item_list") or []:
        if item.get("type") == 1 and isinstance(item.get("text_item"), dict):
            text = _clean(item["text_item"].get("text")) or text
        elif item.get("type") == 3 and isinstance(item.get("voice_item"), dict):
            text = _clean(item["voice_item"].get("text")) or text
        elif item.get("type") == 2:
            attachments.append("image")
        elif item.get("type") == 4:
            name = _clean((item.get("file_item") or {}).get("file_name")) or "file"
            attachments.append(f"file {name}")
    return {
        "sender": sender,
        "text": text,
        "context_token": context_token,
        "echo": msg_type == 2,
        "attachments": attachments,
    }


PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


def fetch_qr_image(image_url):
    """The service's QR bitmap bytes, capped at 5 MB. None when the URL is
    not a direct image (a web page falls back to browser display)."""
    try:
        with _opener.open(image_url, timeout=10) as resp:
            data = resp.read(5 * 1024 * 1024 + 1)
    except (urllib.error.URLError, OSError):
        return None
    if len(data) > 5 * 1024 * 1024:
        return None
    return data


def _png_chunks(data):
    pos = len(PNG_SIGNATURE)
    while pos + 8 <= len(data):
        length = int.from_bytes(data[pos:pos + 4], "big")
        ctype = data[pos + 4:pos + 8]
        chunk = data[pos + 8:pos + 8 + length]
        pos += 12 + length
        yield ctype, chunk


def decode_png(data):
    """Minimal PNG decode for QR display: 8-bit depth, non-interlaced, any
    color type, alpha composited onto white. Returns rows of booleans,
    True = dark module. None when the bytes are not such a PNG."""
    if not data.startswith(PNG_SIGNATURE):
        return None
    width = height = None
    bit_depth = color_type = None
    palette = None
    idat = bytearray()
    for ctype, chunk in _png_chunks(data):
        if ctype == b"IHDR":
            width, height = int.from_bytes(chunk[0:4], "big"), int.from_bytes(chunk[4:8], "big")
            bit_depth, color_type = chunk[8], chunk[9]
            if chunk[12] != 0:  # interlace
                return None
        elif ctype == b"PLTE":
            palette = [chunk[i:i + 3] for i in range(0, len(chunk), 3)]
        elif ctype == b"IDAT":
            idat.extend(chunk)
        elif ctype == b"IEND":
            break
    if not width or not height or bit_depth != 8:
        return None
    channels = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}.get(color_type)
    if channels is None:
        return None
    try:
        raw = __import__("zlib").decompress(bytes(idat))
    except __import__("zlib").error:
        return None
    stride = width * channels
    if len(raw) < (stride + 1) * height:
        return None

    def paeth(a, b, c):
        p = a + b - c
        pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
        return a if pa <= pb and pa <= pc else (b if pb <= pc else c)

    def pixel_gray(row, x):
        if color_type == 3:
            r, g, b = palette[row[x]] if palette and row[x] < len(palette) else (255, 255, 255)
        elif color_type == 0:
            return row[x]
        elif color_type == 4:
            return row[x] if row[x + 1] >= 128 else 255
        else:
            r, g, b = row[x], row[x + 1], row[x + 2]
            if color_type == 6 and row[x + 3] < 128:
                return 255
        return (r * 299 + g * 587 + b * 114) // 1000

    grid = []
    prev = bytearray(stride)
    for y in range(height):
        row = bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        ftype = raw[y * (stride + 1)]
        for x in range(stride):
            a = row[x - channels] if x >= channels else 0
            b = prev[x]
            c = prev[x - channels] if x >= channels else 0
            if ftype == 1:
                row[x] = (row[x] + a) & 0xFF
            elif ftype == 2:
                row[x] = (row[x] + b) & 0xFF
            elif ftype == 3:
                row[x] = (row[x] + (a + b) // 2) & 0xFF
            elif ftype == 4:
                row[x] = (row[x] + paeth(a, b, c)) & 0xFF
        grid.append([pixel_gray(row, x) < 128 for x in range(width)])
        prev = row
    return grid


def _module_size(grid):
    """The pixel run of one module: the finder pattern opens with a dark run
    of exactly 7 modules, so the first dark run of the first dark row is
    seven times the module size."""
    for row in grid:
        runs, current = [], 0
        for px in row:
            if px:
                current += 1
            elif current:
                runs.append(current)
                current = 0
        if current:
            runs.append(current)
        if runs:
            return max(1, runs[0] // 7)
    return None


def render_qr_terminal(grid):
    """The decoded matrix as ANSI half-blocks: one cell per module, square
    on screen, scannable straight from the terminal."""
    size = _module_size(grid)
    if not size or len(grid[0]) < size:
        return None
    dim = len(grid[0]) // size
    height = min(len(grid) // size, dim)
    lines = []
    for gy in range(0, height - 1, 2):
        line = []
        for gx in range(dim):
            cx = gx * size + size // 2
            top = grid[gy * size + size // 2][cx]
            bottom = grid[min((gy + 1) * size + size // 2, len(grid) - 1)][cx]
            fr, fgc, fb = (0, 0, 0) if top else (255, 255, 255)
            br, bgc, bb = (0, 0, 0) if bottom else (255, 255, 255)
            line.append(f"\x1b[38;2;{fr};{fgc};{fb};48;2;{br};{bgc};{bb}m▀")
        lines.append("\x1b[0m" + "".join(line) + "\x1b[0m")
    return "\n".join(lines) if lines else None


def show_qr(image_url, open_path=os.path.join(os.path.expanduser("~"), ".yak", "weixin-qr.png")):
    """Zero-dependency QR display: the official image is downloaded, decoded
    and drawn as ANSI half-blocks; the file is saved and handed to the
    desktop opener as a fallback, and the URL is printed last."""
    qr = encode_qr(image_url.encode())
    drawn = render_qr_matrix(qr) if qr else None
    if drawn:
        print(drawn)
        print("(scan with WeChat — it opens the binding page)")
    else:
        print(f"open this URL on the phone (in WeChat) to confirm: {image_url}")
