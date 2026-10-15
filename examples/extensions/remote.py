#!/usr/bin/env python3
# The phone bridge: keeps a yak session left on an unattended machine
# reachable from a phone. Three jobs, all outbound-only connections so the
# machine needs no public IP and no forwarded port:
#
#   push     approval prompts and finished answers leave as notifications.
#            Every configured channel fires: Telegram, email (SMTP),
#            a WeCom group robot, ServerChan (WeChat) and Bark (iOS).
#   listen   the Telegram bot or the mailbox is watched on a background
#            thread (getUpdates long-poll / IMAP IDLE), so a reply typed
#            on the phone lands here within seconds.
#   deliver  inside tmux the reply is typed straight into this session's
#            own pane ($TMUX_PANE): plain text becomes the next prompt,
#            y/n/a answers a blocking approval prompt, /stop sends ctrl-c,
#            /status pushes a report back. Every routed reply is answered
#            with a ✓ ack quoting the sender's message. With several yak
#            sessions running, notifications carry a [project] tag and a
#            reply may open with "[project]" to route to that session's
#            pane; only one bridge polls Telegram (a flock), so concurrent
#            pollers never swallow each other's updates. Outside tmux (or
#            with delivery disabled) replies wait in an inbox that is typed
#            in when a session is free.
#
# While it lives the extension also holds the machine awake: systemd-inhibit
# (sleep:idle plus the lid switch) on Linux, caffeinate anchored to the host
# process on macOS, SetThreadExecutionState on Windows. It is a hold, not a
# settings change — it ends with the session. Set awake:false to opt out.
#
# The intended shape is one yak running inside tmux (the session survives a
# dropped ssh), this extension installed, and the phone on the other end:
#
#   tmux new -s yak
#   yak
#   ... work, approval prompt pushes to the phone, reply "/y" ...
#   ... answer pushes to the phone, reply "next: run the tests" ...
#
# Configuration lives in a JSON file — `remote.json` beside this script,
# or `<user_dir>/remote.json` (~/.yak, YAK_USER_PATH honored). The same
# scalar keys may ride env vars that override the file (yak's own override
# order). Run this file directly to create the template and to check the
# state:
#
#   ./remote init      write <user_dir>/remote.json (0600, random marker)
#   ./remote status    print the resolved channels and what is missing
#
# Channels, push fires on every one that is configured, listening uses
# telegram when its token is set, else the mailbox:
#
#   tg_token + tg_chat   Telegram bot from @BotFather + your numeric chat
#                        id (send the bot one message, then read the id off
#                        .../getUpdates) — push and listen. Messages from
#                        other chats are refused with a note.
#   tg_api               alternate API base in front of api.telegram.org
#                        (the usual https_proxy env is honored too)
#   email                one object, SMTP push + IMAP listen:
#                          {"from": "you@qq.com", "to": "you@qq.com",
#                           "password": "auth code",
#                           "smtp_host": "", "smtp_port": 465,
#                           "imap_host": "", "imap_port": 993,
#                           "from_filter": "", "marker": "yak-..."}
#                        Hosts default to smtp./imap. + the from domain, so
#                        qq.com and 163.com need only from + password
#                        (an app auth code, not the login password); gmail
#                        needs an app password. `to` defaults to `from` and
#                        `user` too. Every push carries the marker in its
#                        subject; a reply keeps it (Re: ...), mail without
#                        the marker is ignored, and only the first
#                        paragraph of a reply is read — the quoted original
#                        below the first blank line never becomes a
#                        command. `from_filter` (an address substring)
#                        pins replies to one sender.
#   wecom_webhook        WeCom group-robot webhook url — push only
#   serverchan           ServerChan sendkey — push to WeChat, push only
#   bark                 Bark key or full server url — push to iOS, push only
#   awake (bool)         keep-awake hold, default true
#   deliver (bool)       type replies into tmux, default true
#   notify               "always" or "unfocused" (default): the same focus
#                        gate as notify.py, quiet while you are looking
#
# Finished answers ride whole (Telegram chunks them at 4096 chars), and
# every prompt typed at the terminal is mirrored to the phone so the
# conversation never goes dark on one side (the bridge's own typed-in
# replies are not mirrored back — the ✓ ack already answered them).
#
# Security: the only thing a reply can do is type into this session's own
# tmux pane — no shell, no files. Telegram replies are filtered to one chat
# id; email replies must carry the per-install marker (generated by
# `remote init`) and may be pinned to one sender. The extension runs with
# your permissions like every extension; credentials sit in remote.json
# (0600), never in this code file.
#
# Install into ~/.yak/extensions/ (or the project's .yak/extensions/),
# /reload, and check /status. Without a channel configured the tools still
# mount and remote_status says what is missing.

import email as email_lib
import imaplib
import json
import os
import platform
import queue
import re
import secrets
import shutil
import smtplib
import socket
import subprocess
import sys
import threading
import time
import urllib.request

if platform.system() == "Windows":
    import msvcrt
else:
    import fcntl
from email.header import decode_header, make_header
from email.mime.text import MIMEText


def note(text):
    """stderr is the human channel; it lands in the diagnostics tail."""
    sys.stderr.write(f"remote: {text}\n")
    sys.stderr.flush()


def user_dir():
    return os.environ.get("YAK_USER_PATH") or os.path.join(
        os.path.expanduser("~"), ".yak")


def _load_cfg():
    """remote.json beside this script, else <user_dir>/remote.json."""
    paths = [os.path.join(
        os.path.dirname(os.path.realpath(__file__)), "remote.json"),
        os.path.join(user_dir(), "remote.json")]
    for path in paths:
        try:
            with open(path, encoding="utf-8") as f:
                return json.load(f), path
        except FileNotFoundError:
            continue
        except (OSError, json.JSONDecodeError) as e:
            note(f"config {path} unreadable: {e}")  # loud, not fatal
    return {}, None


CFG, CFG_PATH = _load_cfg()


def setting(key, env=None, default=""):
    """env wins when set (yak's own override order), else the file, else
    the default; booleans in the file become "1"/"0" so they compare the
    same way as flag-style env values. An empty value counts as unset:
    the init template ships empty strings for every optional key."""
    if env:
        raw = os.environ.get(env)
        if raw is not None and raw.strip():
            return raw.strip()
    v = CFG.get(key, default)
    if isinstance(v, bool):
        return "1" if v else "0"
    v = "" if v is None else str(v).strip()
    return v or default


TG_TOKEN = setting("tg_token", "YAK_REMOTE_TG_TOKEN")
TG_CHAT = setting("tg_chat", "YAK_REMOTE_TG_CHAT")
TG_API = setting("tg_api", "YAK_REMOTE_TG_API",
                 "https://api.telegram.org").rstrip("/")
WECOM = setting("wecom_webhook", "YAK_REMOTE_WECOM")
SERVERCHAN = setting("serverchan", "YAK_REMOTE_SERVERCHAN")
BARK = setting("bark", "YAK_REMOTE_BARK")
AWAKE = setting("awake", "YAK_REMOTE_AWAKE", "1") != "0"
TYPING = setting("deliver", "YAK_REMOTE_DELIVER", "1") != "0"
NOTIFY_WHEN = setting("notify", "YAK_REMOTE_NOTIFY", "unfocused")
TMUX_PANE = os.environ.get("TMUX_PANE", "")
# a subagent child (subagent.py's `yak --json`) inherits the user extension
# dir and so this bridge: its events belong to a child agent, not the user's
# session — pushing them would double every notification and every input
# mirror, and the child's cwd is a temp worktree no tag resolves. It carries
# YAK_SUBAGENT_DEPTH; a hand-run yak has none.
_CHILD = bool(os.environ.get("YAK_SUBAGENT_DEPTH"))


def _email_cfg():
    """The email channel: hosts default to smtp./imap. + the from domain,
    identity defaults to the from address, so one address + one auth code
    is a complete configuration."""
    raw = CFG.get("email")
    raw = raw if isinstance(raw, dict) else {}
    frm = str(raw.get("from", "")).strip()
    domain = frm.split("@", 1)[1] if "@" in frm else ""
    cfg = {
        "from": frm,
        "to": str(raw.get("to", "")).strip() or frm,
        "user": str(raw.get("user", "")).strip() or frm,
        "password": str(raw.get("password", "")).strip(),
        "smtp_host": str(raw.get("smtp_host", "")).strip()
        or (f"smtp.{domain}" if domain else ""),
        "smtp_port": int(raw.get("smtp_port", 0) or 465),
        "imap_host": str(raw.get("imap_host", "")).strip()
        or (f"imap.{domain}" if domain else ""),
        "imap_port": int(raw.get("imap_port", 0) or 993),
        "from_filter": str(raw.get("from_filter", "")).strip(),
        "marker": str(raw.get("marker", "")).strip(),
    }
    cfg["ready"] = bool(frm and cfg["password"] and cfg["to"]
                        and cfg["smtp_host"] and cfg["imap_host"])
    return cfg


EMAIL = _email_cfg()

# how much of an answer rides the notification body, past which an explicit
# ellipsis replaces the cut
SUMMARY_CHARS = 200
# Telegram caps one message at 4096 chars: a long answer rides as
# numbered chunks (the notification preview shows the first, tapping the
# conversation shows them all)
TG_CHARS = 3900
# WeCom text messages cap at 2048 bytes; stay under in characters
WECOM_CHARS = 1400
# IMAP IDLE has to be re-issued now and then; 29 minutes is inside every
# server's limit
IDLE_MAX = 29 * 60
# the title every push carries
PUSH_TITLE = "yak"

_lock = threading.Lock()
_busy = False              # between agent_start and agent_end
_pending_approval = None   # the approval prompt currently blocking, if any
_inbox = []                # replies that could not be delivered yet
_awake_child = None
_awake_how = "not started"
_session_cwd = ""
_event_cwd = ""            # the cwd the current events carry, for the tag
_own_input = None          # the reply this bridge itself typed/held, so the
                           # input mirror never echoes it back to the phone
_poll_lock_file = None     # held open while this bridge polls Telegram
_started = [False]
_seen_uids = set()         # mails already routed, so a read-on-the-phone
                           # reply still arrives exactly once


def http_post_json(url, payload, timeout=15):
    req = urllib.request.Request(
        url, data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        r.read()


def http_get_text(url, timeout=30):
    with urllib.request.urlopen(url, timeout=timeout) as r:
        return r.read().decode("utf-8", "replace")


def tg_push(body, reply_to=None, silent=False):
    """Send one Telegram message; returns the message id, or an error
    string (sendMessage's response carries the id the ack replies to)."""
    try:
        payload = {"chat_id": TG_CHAT, "text": body}
        if reply_to is not None:
            payload["reply_to_message_id"] = reply_to
        if silent:
            payload["disable_notification"] = True
        req = urllib.request.Request(
            f"{TG_API}/bot{TG_TOKEN}/sendMessage",
            data=json.dumps(payload).encode("utf-8"),
            headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=15) as r:
            data = json.loads(r.read())
        return data.get("result", {}).get("message_id")
    except Exception as e:
        return f"telegram: {e}"


def tg_send(text, silent=False):
    """One logical message to Telegram: full text, chunked at the limit.
    Returns the id of the first chunk, or an error string."""
    chunks = [text[i:i + TG_CHARS] for i in range(0, len(text), TG_CHARS)] or [""]
    first_id = None
    for i, chunk in enumerate(chunks):
        r = tg_push(chunk if len(chunks) == 1
                    else f"[{i + 1}/{len(chunks)}] {chunk}", silent=silent)
        if isinstance(r, str):
            return r
        if i == 0:
            first_id = r
    return first_id


def email_push(body):
    subject = f"{PUSH_TITLE} {EMAIL['marker']}".strip()
    msg = MIMEText(body, "plain", "utf-8")
    msg["Subject"] = subject
    msg["From"] = EMAIL["from"]
    msg["To"] = EMAIL["to"]
    try:
        with smtplib.SMTP_SSL(EMAIL["smtp_host"], EMAIL["smtp_port"],
                              timeout=20) as s:
            s.login(EMAIL["user"], EMAIL["password"])
            s.send_message(msg)
    except Exception as e:
        return f"email: {e}"


def wecom_push(body):
    try:
        http_post_json(WECOM, {"msgtype": "text", "text": {
            "content": f"{PUSH_TITLE}\n{body[:WECOM_CHARS]}"
            + ("…" if len(body) > WECOM_CHARS else "")}})
    except Exception as e:
        return f"wecom: {e}"


def serverchan_push(body):
    try:
        http_post_json(f"https://sctapi.ftqq.com/{SERVERCHAN}.send",
                       {"title": PUSH_TITLE, "desp": body})
    except Exception as e:
        return f"serverchan: {e}"


def bark_push(body):
    base = BARK if BARK.startswith("http") else f"https://api.day.app/{BARK}"
    try:
        http_post_json(base, {"title": PUSH_TITLE, "body": body, "group": "yak"})
    except Exception as e:
        return f"bark: {e}"


def _push_sync(body, silent=False):
    """Fire every configured channel; errors go to the diagnostics tail.
    A silent push carries no alert (mirror noise); Telegram takes the flag,
    the other channels have no such notion and simply skip."""
    errors = []
    if TG_TOKEN and TG_CHAT:
        r = tg_send(body, silent=silent)
        if isinstance(r, str):
            errors.append(r)
    if not silent:
        if EMAIL["ready"]:
            errors.append(email_push(body))
        if WECOM:
            errors.append(wecom_push(body))
        if SERVERCHAN:
            errors.append(serverchan_push(body))
        if BARK:
            errors.append(bark_push(body))
    if not any([TG_TOKEN and TG_CHAT, EMAIL["ready"], WECOM,
                SERVERCHAN, BARK]):
        note("no push channel configured — edit remote.json "
             "(tg_token + tg_chat, email, wecom_webhook, serverchan or bark)")
    for e in errors:
        if e:
            note(f"push failed: {e}")


def push(body, silent=False):
    """Push off the protocol thread: a slow channel must never hold back
    the 5s event-hook reply (the host times that out)."""
    threading.Thread(target=_push_sync, args=(body, silent), daemon=True).start()


def should_notify(params):
    """The focus gate: quiet while the user is looking at the terminal."""
    if NOTIFY_WHEN == "always":
        return True
    return not params.get("focused", False)


def first_line(text):
    for line in (text or "").splitlines():
        line = line.strip()
        if line:
            return line[:SUMMARY_CHARS] + ("…" if len(line) > SUMMARY_CHARS else "")
    return "(no text)"


def project_tag(cwd=None):
    """[basename] — tells two yak sessions' notifications apart."""
    return f"[{project_name(cwd)}] " if project_name(cwd) else ""


def project_name(cwd=None):
    """The bare tag: the cwd's basename, no brackets."""
    name = os.path.basename((cwd or _event_cwd or _session_cwd or "").rstrip("/"))
    return name


def pane_keys(text, key=None, pane=None):
    """Type into this session's tmux pane (or an explicit one); None on
    success, error string else."""
    pane = pane or TMUX_PANE
    if not pane:
        return "no tmux pane ($TMUX_PANE unset)"
    try:
        if key:
            subprocess.run(["tmux", "send-keys", "-t", pane, key],
                           check=True, stdin=subprocess.DEVNULL,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        else:
            subprocess.run(["tmux", "send-keys", "-t", pane, "-l", "--", text],
                           check=True, stdin=subprocess.DEVNULL,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            subprocess.run(["tmux", "send-keys", "-t", pane, "Enter"],
                           check=True, stdin=subprocess.DEVNULL,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    except (OSError, subprocess.CalledProcessError) as e:
        return f"tmux send-keys failed: {e}"
    return None


def _reg_path():
    return os.path.join(user_dir(), "remote-panes.json")


def reg_refresh():
    """Rewrite the pane registry: every live bridge lists its pane, project
    name, busy flag and pending-approval flag so one listener can route a
    reply to the right session; panes that no longer exist drop out."""
    if not TMUX_PANE:
        return
    try:
        live = subprocess.run(["tmux", "list-panes", "-a", "-F", "#{pane_id}"],
                              capture_output=True, text=True,
                              stdin=subprocess.DEVNULL).stdout.split()
    except Exception:
        return
    try:
        with open(_reg_path(), encoding="utf-8") as f:
            reg = json.load(f)
    except Exception:
        reg = {}
    if TMUX_PANE in live:
        prev = reg.get(TMUX_PANE)
        held = prev.get("inbox", []) if isinstance(prev, dict) else []
        reg[TMUX_PANE] = {"tag": project_name(), "busy": bool(_busy),
                          "ap": bool(_pending_approval), "inbox": held}
    reg = {p: e for p, e in reg.items()
           if p in live and isinstance(e, dict)}
    _reg_write(reg)


def reg_entry():
    try:
        with open(_reg_path(), encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return {}


def _reg_write(reg):
    try:
        tmp = _reg_path() + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(reg, f)
        os.replace(tmp, _reg_path())
    except OSError as e:
        note(f"pane registry write failed: {e}")


def reg_queue(pane, text):
    """Hold one instruction for a busy session's own bridge to type when
    its answer finishes — the registry is the only state two bridge
    processes share."""
    reg = reg_entry()
    e = reg.get(pane)
    if not isinstance(e, dict):
        return False
    e["inbox"] = (e.get("inbox") or []) + [text]
    _reg_write(reg)
    return True


def reg_pop_inbox(pane):
    """Take the oldest held instruction for this pane, if any."""
    reg = reg_entry()
    e = reg.get(pane)
    if not isinstance(e, dict) or not e.get("inbox"):
        return None
    text = e["inbox"][0]
    e["inbox"] = e["inbox"][1:]
    _reg_write(reg)
    return text


def reg_lookup(tag):
    """The pane a [tag]-prefixed reply targets, or this pane when no tag
    matches: untagged replies keep going to the pane they always went to."""
    if not tag:
        return TMUX_PANE
    for pane, entry in reg_entry().items():
        if entry.get("tag") == tag:
            return pane
    return TMUX_PANE


def reg_approval_pane():
    """The pane with a pending approval, if any — /y from the phone must
    reach the session that asked, not become a stray prompt elsewhere."""
    for pane, entry in reg_entry().items():
        if entry.get("ap"):
            return pane
    return None


ANSWERS = {"y": "y", "yes": "y", "allow": "y",
           "n": "n", "no": "n", "deny": "n",
           "a": "a", "always": "a"}


def deliver(text):
    """Route one phone reply: approval answer, typed prompt, or inbox
    hold. Returns a one-line outcome for the sender's ack."""
    global _pending_approval
    text = text.strip()
    if not text:
        return None
    cmd = text[1:].strip().lower() if text.startswith("/") else ""
    ap_pane = reg_approval_pane() or (TMUX_PANE if _pending_approval else None)
    if cmd in ("y", "n", "a") and ap_pane:
        letter = cmd
        _pending_approval = None
        err = pane_keys(None, key=letter, pane=ap_pane)
        note(err or f"answered the approval prompt with '{letter}'")
        return err or f"✓ approval answered with {letter}"
    if _pending_approval and not cmd and text.lower() in ANSWERS:
        letter = ANSWERS[text.lower()]
        _pending_approval = None
        err = pane_keys(None, key=letter, pane=ap_pane or TMUX_PANE)
        note(err or f"answered the approval prompt with '{letter}'")
        return err or f"✓ approval answered with {letter}"
    if cmd == "stop":
        err = pane_keys(None, key="C-c")
        note(err or "sent ctrl-c to the session")
        return err or "✓ sent ctrl-c to the session"
    if cmd == "status":
        push(status_text())
        return "✓ status pushed"
    if cmd:
        push("unknown command '{0}' — plain text is typed as the next "
             "instruction; /y /n /a answer an approval, /stop interrupts, "
             "/status reports".format(text))
        return None
    flat = " ".join(text.split())  # the line editor takes one line per submit
    tag = ""
    m = re.match(r"\[([^\[\]]+)\]\s*(.*)", flat, re.S)
    if m and reg_lookup(m.group(1)) != TMUX_PANE:
        tag, flat = m.group(1), m.group(2).strip()
    pane = reg_lookup(tag)
    entry = reg_entry().get(pane) or {}
    # the target session's own busy flag, not ours; when an old bridge
    # wrote the entry without one, assume busy — queueing is safe, typing
    # into a live editor is not
    busy = entry.get("busy", _busy if pane == TMUX_PANE else True)
    if not busy and TYPING:
        global _own_input
        _own_input = flat
        err = pane_keys(flat, pane=pane)
        if err is None:
            note(f"typed into {pane}: {flat[:80]}")
            whose = "this session" if pane == TMUX_PANE else f"session [{tag}]"
            return f"✓ typed into {whose}"
        note(err)
    if pane == TMUX_PANE:
        with _lock:
            _inbox.append(flat)
        note(f"held for the session: {flat[:80]}")
        return "✓ queued (busy — typed in when the answer finishes)"
    if reg_queue(pane, flat):
        note(f"held for [{tag or entry.get('tag')}] {pane}: {flat[:80]}")
        return f"✓ queued (busy — typed in when the answer finishes)"
    note(f"could not queue for {pane}: no registry entry")
    return "✗ target session unreachable (not in the pane registry)"


def flush_one():
    """One queued instruction per finished answer: typing more would stack
    submissions into a live editor, and each typed instruction starts a run
    whose agent_end flushes the next."""
    global _own_input
    text = None
    with _lock:
        if _inbox:
            text = _inbox.pop(0)
    if text is None:
        text = reg_pop_inbox(TMUX_PANE)
    if text is None:
        return
    _own_input = text
    err = pane_keys(text)
    note(err or f"typed a held instruction: {text[:80]}")


def tg_listen():
    base = f"{TG_API}/bot{TG_TOKEN}"
    offset = None
    while True:
        params = {"timeout": "25", "allowed_updates": json.dumps(["message"])}
        if offset is not None:
            params["offset"] = str(offset)
        url = base + "/getUpdates?" + "&".join(
            f"{k}={v}" for k, v in params.items())
        try:
            data = json.loads(http_get_text(url, timeout=35))
        except Exception as e:
            note(f"telegram poll failed: {e}")
            time.sleep(5)
            continue
        for upd in data.get("result", []):
            offset = max(offset or 0, upd["update_id"] + 1)
            msg = upd.get("message") or upd.get("edited_message") or {}
            chat = str((msg.get("chat") or {}).get("id", ""))
            text = (msg.get("text") or "").strip()
            if not text:
                continue
            if TG_CHAT and chat != TG_CHAT:
                push(f"refused a message from chat {chat}: tg_chat "
                     f"pins this bridge to one conversation")
                continue
            outcome = deliver(text)
            if outcome:
                tg_push(outcome, reply_to=msg.get("message_id"))


def decode_hdr(value):
    try:
        return str(make_header(decode_header(value or "")))
    except Exception:
        return value or ""


def email_body_text(msg):
    """The reply's fresh text: the first paragraph of the first text/plain
    part, stopping at a quoted original — the phone mail app's quote of our
    own push never becomes a command."""
    part = None
    if msg.is_multipart():
        for p in msg.walk():
            if p.get_content_type() == "text/plain" and not p.get_filename():
                part = p
                break
    elif msg.get_content_type() == "text/plain":
        part = msg
    if part is None:
        return ""
    charset = part.get_content_charset() or "utf-8"
    try:
        text = (part.get_payload(decode=True) or b"").decode(charset, "replace")
    except (LookupError, Exception):
        try:
            text = (part.get_payload(decode=True) or b"").decode("utf-8", "replace")
        except Exception:
            return ""
    out = []
    for line in text.splitlines():
        s = line.strip()
        if not s:
            if out:
                break  # the first paragraph is the user's text; stop there
            continue
        if s.startswith(">") or s.startswith("----"):
            break  # a quoted original below
        out.append(s)
    return " ".join(out)


def imap_idle(conn, tag_seq):
    """Hold one IMAP IDLE until the server reports new mail (or the
    periodic wake); raises on servers without IDLE so the caller can fall
    back to polling."""
    tag_seq[0] += 1
    tag = f"Y{tag_seq[0]}"
    conn.send(f"{tag} IDLE\r\n".encode())
    line = conn.readline()
    if not line.startswith(b"+"):
        raise RuntimeError(line.strip().decode("utf-8", "replace")[:60])
    conn.sock.settimeout(IDLE_MAX)
    try:
        while True:
            line = conn.readline()
            if not line:
                raise OSError("server closed the idle connection")
            upper = line.upper()
            if b"EXISTS" in upper or b"RECENT" in upper or b"FETCH" in upper:
                break  # something new arrived
    except (TimeoutError, socket.timeout):
        pass  # the periodic wake: re-issue, nothing arrived
    finally:
        conn.sock.settimeout(30)
    conn.send(b"DONE\r\n")
    while True:
        line = conn.readline()
        if line.startswith(tag.encode()):
            break


def email_listen():
    host, port = EMAIL["imap_host"], EMAIL["imap_port"]
    marker = EMAIL["marker"]
    idle_ok = True
    tag_seq = [0]
    while True:
        conn = None
        try:
            try:
                conn = imaplib.IMAP4_SSL(host, port, timeout=30)
            except TypeError:  # an older python without the timeout kw
                conn = imaplib.IMAP4_SSL(host, port)
            conn.login(EMAIL["user"], EMAIL["password"])
            conn.select("INBOX")
            # search by the subject marker, not by unread state: a reply
            # already read on the phone must still arrive, and anything
            # without the marker must never be routed
            query = f'(HEADER SUBJECT "{marker}")' if marker else "UNSEEN"
            while True:
                typ, data = conn.uid("search", None, query)
                for uid in (data[0].split() if typ == "OK" and data and
                            data[0] else []):
                    if uid in _seen_uids:
                        continue
                    typ, d = conn.uid("fetch", uid, "(BODY.PEEK[])")
                    if typ != "OK" or not d or not d[0]:
                        continue
                    msg = email_lib.message_from_bytes(d[0][1])
                    subject = decode_hdr(msg.get("Subject"))
                    sender = decode_hdr(msg.get("From"))
                    _seen_uids.add(uid)
                    if marker and marker not in subject:
                        continue
                    if EMAIL["from_filter"] and EMAIL["from_filter"] not in sender:
                        note(f"refused mail from {sender[:60]}: from_filter "
                             f"pins replies to one sender")
                        continue
                    try:
                        conn.uid("store", uid, "+FLAGS", "\\Seen")
                    except Exception:
                        pass
                    text = email_body_text(msg)
                    if text:
                        deliver(text)  # no ack: email replies are not quotable
                if idle_ok:
                    try:
                        imap_idle(conn, tag_seq)
                        continue
                    except Exception as e:
                        idle_ok = False
                        note(f"imap idle unavailable ({e}), polling every 25s")
                conn.noop()
                time.sleep(25)
        except Exception as e:
            note(f"imap failed: {e}")
            try:
                if conn is not None:
                    conn.logout()
            except Exception:
                pass
            time.sleep(15)


def listener_plan():
    """What the listener would be — shown before initialize starts it."""
    if TG_TOKEN:
        return f"telegram getUpdates ({TG_API})"
    if EMAIL["ready"]:
        return "email imap {} as {}{}".format(
            EMAIL["imap_host"], EMAIL["user"],
            f", marker {EMAIL['marker']}" if EMAIL["marker"] else "")
    return ("no listen channel — edit remote.json "
            "(tg_token + tg_chat, or the email object)")


def start_listener():
    global _listener_how, _poll_lock_file
    _started[0] = True
    solo = ""
    running = False
    if TG_TOKEN:
        # single-flight: with several yak sessions each running this
        # extension, only the lock holder polls Telegram — concurrent
        # getUpdates pollers swallow each other's updates; the lock is
        # taken the platform's way so it holds on windows too
        _poll_lock_file = open(
            os.path.join(user_dir(), "remote-poll.lock"), "a+b")
        locked = True
        try:
            if platform.system() == "Windows":
                msvcrt.locking(_poll_lock_file.fileno(),
                               msvcrt.LK_NBLCK, 1)
            else:
                fcntl.flock(_poll_lock_file,
                            fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            locked = False
        if locked:
            threading.Thread(target=tg_listen, daemon=True).start()
            solo = " (solo poller)"
        else:
            _poll_lock_file.close()
            _poll_lock_file = None
            solo = " (another bridge holds the poll lock)"
        running = True
    elif EMAIL["ready"]:
        threading.Thread(target=email_listen, daemon=True).start()
        running = True
    _listener_how = listener_plan() + (" (running)" if running else "") + solo


def start_awake():
    global _awake_child, _awake_how
    if not AWAKE:
        _awake_how = "disabled (awake: false)"
        return
    system = platform.system()
    devnull = subprocess.DEVNULL
    try:
        if system == "Linux" and shutil.which("systemd-inhibit"):
            _awake_child = subprocess.Popen(
                ["systemd-inhibit", "--what", "sleep:idle:handle-lid-switch",
                 "--who", "yak", "--why", "yak remote session",
                 "--mode", "block", "sleep", "infinity"],
                stdin=devnull, stdout=devnull, stderr=devnull,
                start_new_session=True)
            _awake_how = "systemd-inhibit sleep:idle:handle-lid-switch"
        elif system == "Darwin" and shutil.which("caffeinate"):
            # anchored to the host process: the hold ends with the session
            # even if this extension is killed without its shutdown
            _awake_child = subprocess.Popen(
                ["caffeinate", "-is", "-w", str(os.getppid())],
                stdin=devnull, stdout=devnull, stderr=devnull,
                start_new_session=True)
            _awake_how = "caffeinate -isw <host>"
        elif system == "Windows":
            import ctypes
            # ES_CONTINUOUS | ES_SYSTEM_REQUIRED, held by this thread until
            # the process exits or stop_awake resets it
            ctypes.windll.kernel32.SetThreadExecutionState(0x80000001)
            _awake_how = "SetThreadExecutionState(ES_SYSTEM_REQUIRED)"
        else:
            _awake_how = f"no keep-awake method on {system}"
    except OSError as e:
        _awake_how = f"keep-awake failed: {e}"
    note(f"keep-awake: {_awake_how}")


def stop_awake():
    global _awake_child
    if _awake_child is not None:
        _awake_child.terminate()
        _awake_child = None
    if platform.system() == "Windows" and AWAKE:
        try:
            import ctypes
            ctypes.windll.kernel32.SetThreadExecutionState(0x80000000)
        except Exception:
            pass


def status_text():
    with _lock:
        inbox = list(_inbox)
    channels = []
    if TG_TOKEN and TG_CHAT:
        channels.append("telegram push+listen")
    if EMAIL["ready"]:
        channels.append(f"email push+listen ({EMAIL['from']} → {EMAIL['to']})")
    if WECOM:
        channels.append("wecom push")
    if SERVERCHAN:
        channels.append("serverchan push")
    if BARK:
        channels.append("bark push")
    return "\n".join([
        f"config: {CFG_PATH or 'none found (run: remote init)'}",
        f"channels: {', '.join(channels) or 'none configured'}",
        f"listener: {_listener_how if _started[0] else listener_plan()}",
        f"keep-awake: {_awake_how}",
        f"tmux pane: {TMUX_PANE or 'not under tmux'}",
        f"session: {_session_cwd or '?'}, {'busy' if _busy else 'idle'}, "
        f"approval {'pending' if _pending_approval else 'clear'}",
        f"held replies: {len(inbox)}",
    ] + ([f"held: {t[:60]}" for t in inbox] or []))


TOOLS = [
    {"name": "remote_notify", "tier": "read",
     "description": "Push a message to the user's phone through the "
                    "configured notification channels (remote.json). Use it "
                    "to report a milestone without stopping work.",
     "parameters": {"type": "object",
                    "properties": {"body": {"type": "string"}},
                    "required": ["body"]}},
    {"name": "remote_status", "tier": "read",
     "description": "Report the phone bridge state: channels, listener, "
                    "keep-awake hold, pending replies.",
     "parameters": {"type": "object", "properties": {}, "required": []}},
]


def handle_call(msg):
    name = msg.get("name")
    args = msg.get("args") or {}
    if name == "remote_notify":
        push(str(args.get("body", "")))
        return "sent to every configured channel"
    if name == "remote_status":
        return status_text()
    return f"unknown tool {name}"


def handle_event(msg):
    global _busy, _pending_approval, _event_cwd, _own_input
    params = msg.get("params") or {}
    name = msg.get("name")
    if _CHILD:
        return None
    if params.get("cwd"):
        _event_cwd = params["cwd"]
    if name == "agent_start":
        _busy = True
        reg_refresh()
    elif name == "input":
        # mirror what was typed at the terminal, so the phone side never
        # misses half the conversation; the bridge's own deliveries are
        # skipped (_own_input) — the ack already answered them
        text = (params.get("text") or "").strip()
        if not text or text == _own_input:
            _own_input = None
            return None
        push("PC input {}› {}".format(project_tag(), text), silent=True)
    elif name == "agent_end":
        _busy = False
        _pending_approval = None
        reg_refresh()
        if not params.get("interrupted") and should_notify(params):
            answer = (params.get("final_text") or "").strip() or "(no text)"
            push("{}done — reply with the next instruction\n\n{}".format(
                project_tag(params.get("cwd")), answer))
        flush_one()
    elif name == "approval":
        _pending_approval = "{}: {}".format(
            params.get("tool", "tool"), first_line(params.get("preview")))
        reg_refresh()
        # an approval always blocks the session waiting on a human — the
        # focus gate must never swallow it (focused reports the pane the
        # user last typed in, not whether they see *this* prompt)
        push("{}approval requested\n{}\nreply /y /n /a".format(
            project_tag(params.get("cwd")), _pending_approval), silent=True)
    return None


def main():
    global _session_cwd
    start_awake()
    inbox = queue.Queue()

    def stdin_reader():
        for line in sys.stdin:
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            inbox.put(msg)
        inbox.put(None)

    threading.Thread(target=stdin_reader, daemon=True).start()
    while True:
        msg = inbox.get()
        if msg is None:
            break
        kind, mid = msg.get("type"), msg.get("id")
        if kind == "initialize":
            _session_cwd = (msg.get("params") or {}).get("cwd", "") or ""
            start_listener()
            reg_refresh()
            reply = {"tools": TOOLS, "commands": ["remote"],
                     "tool_timeout": 3600,
                     "events": ["agent_start", "input", "agent_end",
                                "approval"]}
            print(json.dumps({"id": mid, "result": reply}), flush=True)
        elif kind == "call_tool":
            try:
                result = handle_call(msg)
            except Exception as e:
                result = f"remote: {e}"
            print(json.dumps({"id": mid, "result": result}), flush=True)
        elif kind == "run_command":
            print(json.dumps({"id": mid, "result": status_text()}), flush=True)
        elif kind == "event":
            try:
                handle_event(msg)
            except Exception as e:
                note(f"event {msg.get('name')}: {e}")
            print(json.dumps({"id": mid, "result": None}), flush=True)
        elif kind == "shutdown":
            break
    stop_awake()


def cli(argv):
    """Direct run: `remote init` writes the config template, `remote
    status` prints the resolved state — setup happens outside the host."""
    cmd = argv[1] if len(argv) > 1 else ""
    if cmd == "init":
        path = os.path.join(user_dir(), "remote.json")
        if os.path.exists(path):
            print(f"{path} already exists — edit it instead")
            return 1
        os.makedirs(user_dir(), exist_ok=True)
        template = {
            "tg_token": "", "tg_chat": "", "tg_api": "",
            "email": {
                "from": "", "to": "", "user": "", "password": "",
                "smtp_host": "", "smtp_port": 465,
                "imap_host": "", "imap_port": 993,
                "from_filter": "",
                "marker": "yak-" + secrets.token_urlsafe(4),
            },
            "wecom_webhook": "",
            "serverchan": "", "bark": "",
            "awake": True, "deliver": True, "notify": "unfocused",
        }
        with open(path, "w", encoding="utf-8") as f:
            json.dump(template, f, indent=2, ensure_ascii=False)
            f.write("\n")
        os.chmod(path, 0o600)  # credentials live here
        print(f"wrote {path} (0600)")
        print("fastest: fill email.from + email.password (a qq.com or 163.com "
              "auth code) and reply to the pushed mails from your phone; "
              "or tg_token + tg_chat for telegram")
        return 0
    if cmd == "status":
        print(status_text())
        return 0
    if not cmd:
        main()
        return 0
    print("usage: remote [init|status]   (no argument = run as a yak extension)")
    return 2


if __name__ == "__main__":
    sys.exit(cli(sys.argv))
