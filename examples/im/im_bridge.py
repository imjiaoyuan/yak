#!/usr/bin/env python3
"""im_bridge — control yak from IM chats (weixin today; telegram/feishu/qq planned).

A background daemon, independent of any interactive yak session. One
long-poll or socket per channel delivers inbound chats; each chat maps to
one `yak --rpc` child (headless, bound to its thread file, cwd = the chat's
workspace), so session state persists in the thread store and restarting
the daemon resumes everything. Install the package under
`<user_dir>/pkg/yak-im/` and link `bin/yak-im` onto PATH; drive it with
`yak-im login|start|stop|status|log` from any directory.

Configuration lives in `<user_dir>/im.json` (YAK_USER_PATH honored):

    {
      "yak_bin": "yak",
      "busy_mode": "steer",
      "approval_timeout": 120,
      "chats": {
        "<user-id>": {"cwd": "~/work/proj", "thinking": "low", "model": null}
      }
    }

`busy_mode` decides what a message does while the chat's session is
already running: `steer` injects it into the run, `queue` submits it as
the next task, `reject` answers with a busy note. Tool approvals surface
as chat messages answered with `y` / `n`; unanswered ones deny after
`approval_timeout` seconds, and a new addressed request first rejects
everything still pending, so a conversation never waits behind a decision
that is not coming.

Channels (im.json `channels` section): weixin binds via `yak-im login`;
telegram needs a @BotFather token; feishu needs app_id/app_secret plus the
optional `lark-oapi` package (its long connection is protobuf-framed — the
one place a dependency is allowed, only when feishu is enabled). Chat
commands (handled locally, never sent to the model): /new /clear /compact
/model /cwd /status /stop /steer /queue /reject /batch /send /cancel
/version /help, approvals answered y / n.
"""
import json
import os
import queue
import re
import signal
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from channels import feishu, qq, telegram, weixin  # noqa: E402

MAX_MESSAGE_CHARS = 1800
APPROVAL_TIMEOUT_DEFAULT = 120
BATCH_LIMIT = 10

HELP = """commands:
/new                 fresh session for this chat
/compact             fold the context now
/model NAME          switch the model (no name: show current)
/cwd PATH            switch this chat's workspace directory
/status              session state
/stop                interrupt the running task
/steer|/queue|/reject  busy-mode for the next message
/batch … /send … /cancel   collect up to 10 messages, submit as one
/help                this list"""


def user_dir():
    return os.environ.get("YAK_USER_PATH") or os.path.join(os.path.expanduser("~"), ".yak")


def load_json(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}


def save_json(path, data):
    tmp = path + ".tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, indent=1)
    os.replace(tmp, path)


def expand(path):
    return os.path.abspath(os.path.expanduser(path or "~"))


def split_reply(text, limit=MAX_MESSAGE_CHARS):
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


class RpcSession:
    """One `yak --rpc` child bound to a chat: line protocol on stdio, a
    reader thread, and id-correlated command responses."""

    def __init__(self, yak_bin, cwd, session=None, model=None, thinking=None,
                 on_event=lambda kind, data: None):
        cmd = [yak_bin, "--rpc"]
        if session:
            cmd += ["--session", session]
        if model:
            cmd += ["-m", model]
        if thinking:
            cmd += ["--thinking", thinking]
        self.proc = subprocess.Popen(
            cmd, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, bufsize=1)
        self.cwd = cwd
        self.thread = session
        self.on_event = on_event
        self.pending = {}
        self.lock = threading.Lock()
        self.next_id = 0
        self.dead = False
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            try:
                record = json.loads(line)
                rid = record.get("id")
                if rid is not None:
                    with self.lock:
                        waiter = self.pending.pop(str(rid), None)
                    if waiter is not None:
                        done, slot = waiter
                        slot[0] = record
                        done.set()
                    continue
                kind = record.get("type")
                if kind == "session":
                    self.thread = record.get("thread")
                elif kind == "tool_start" and self.on_event:
                    self.on_event("tool_start", record)
                if self.on_event:
                    self.on_event(kind, record)
            except Exception as e:
                # one bad frame must not kill the reader: every later
                # event (replies included) would silently never arrive
                print(f"rpc reader dropped a frame: {e}", file=sys.stderr)
        self.dead = True

    def request(self, kind, wait=5.0, **fields):
        if self.dead:
            return None
        with self.lock:
            self.next_id += 1
            rid = str(self.next_id)
            done, slot = threading.Event(), [None]
            self.pending[rid] = (done, slot)
        body = {"id": rid, "type": kind, **fields}
        try:
            self.proc.stdin.write(json.dumps(body) + "\n")
            self.proc.stdin.flush()
        except (OSError, ValueError):
            with self.lock:
                self.pending.pop(rid, None)
            return None
        if not done.wait(wait):
            with self.lock:
                self.pending.pop(rid, None)
            return None
        return slot[0]

    def prompt(self, text):
        return self.request("prompt", message=text)

    def steer(self, text):
        return self.request("steer", message=text)

    def follow_up(self, text):
        return self.request("follow_up", message=text)

    def approve(self, request_id, answer):
        return self.request("approval_response", wait=3.0,
                            request=request_id, answer=answer)

    def command(self, kind, wait=5.0, **fields):
        return self.request(kind, wait=wait, **fields)

    def close(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


class Chat:
    """One IM chat and the yak session behind it."""

    def __init__(self, bridge, sender, chat_cfg):
        self.bridge = bridge
        self.sender = sender
        self.cfg = chat_cfg
        self.rpc = None
        self.busy = False
        self.context_token = None
        self.batch = None
        self.pending_approvals = {}
        self.last_error = None
        self.sender_channel = None
        self.chat_sender = None
        self.channel_meta = {}

    def ensure_session(self):
        if self.rpc is not None and not self.rpc.dead:
            return self.rpc
        try:
            self.rpc = RpcSession(
                self.bridge.yak_bin, expand(self.cfg.get("cwd")),
                session=self.cfg.get("session"), model=self.cfg.get("model"),
                thinking=self.cfg.get("thinking"),
                on_event=lambda kind, data: self.on_event(kind, data))
        except (OSError, ValueError) as e:
            self.bridge.send(self.sender, f"cannot start yak: {e}")
            return None
        return self.rpc

    def on_event(self, kind, data):
        if kind == "session" and data.get("thread"):
            self.cfg["session"] = data["thread"]
            self.bridge.save_chats()
        elif kind == "approval_request":
            self.on_approval(data)
        elif kind == "tool_start":
            self.bridge.log("⚙", "33",
                            f"{data.get('tool') or '?'} · "
                            f"{(data.get('preview') or '')[:110]}")
        elif kind == "result":
            self.busy = False
            self.bridge.on_busy(self, False)
            reply = data.get("text") or "(empty reply)"
            first = reply.strip().splitlines()[0][:160] if reply.strip() else ""
            self.bridge.log("▶", "32",
                            first + (f"  (+{len(reply)} chars)" if len(reply) > 160 else ""))
            self.bridge.send(self.sender, reply, data.get("thread"))
        elif kind == "error":
            self.busy = False
            self.bridge.on_busy(self, False)
            self.bridge.log("✖", "31", f"error: {(data.get('message') or '')[:140]}")
            self.bridge.send(self.sender, f"error: {data.get('message')}",
                             data.get("thread"))

    def on_approval(self, data):
        request_id = data.get("id")
        timeout = self.bridge.approval_timeout
        tool = data.get("tool") or "?"
        preview = (data.get("preview") or "").strip().replace("\n", " ⏎ ")[:300]
        self.pending_approvals[request_id] = {"at": time.time()}
        self.bridge.log("⚠", "35", f"approval · {tool}: {preview[:100]}")
        self.bridge.send(self.sender,
                         f"[approval #{request_id}] {tool}: {preview}\n"
                         f"reply y / n ({timeout}s timeout = deny)")
        threading.Thread(target=self._approval_watchdog,
                         args=(request_id,), daemon=True).start()

    def _approval_watchdog(self, request_id):
        deadline = time.time() + self.bridge.approval_timeout
        while time.time() < deadline:
            if request_id not in self.pending_approvals:
                return
            time.sleep(1)
        if self.pending_approvals.pop(request_id, None) is not None:
            rpc = self.ensure_session()
            if rpc:
                rpc.approve(request_id, "deny")
            self.bridge.send(self.sender, f"[approval #{request_id}] timed out — denied")

    def decide_approval(self, answer, request_id=None):
        if request_id is None and self.pending_approvals:
            request_id = sorted(self.pending_approvals)[-1]
        if request_id is None or request_id not in self.pending_approvals:
            return "no pending approval"
        self.pending_approvals.pop(request_id, None)
        rpc = self.ensure_session()
        if rpc is not None:
            rpc.approve(request_id, "allow" if answer else "deny")
        return f"[approval #{request_id}] {'allowed' if answer else 'denied'}"

    def reject_pending(self):
        for request_id in list(self.pending_approvals):
            self.decide_approval(False, request_id)

    def submit(self, text):
        self.reject_pending()
        rpc = self.ensure_session()
        if rpc is None:
            return None
        if self.busy:
            mode = self.bridge.busy_mode
            if mode == "reject":
                return "still running — /stop to interrupt, or /queue"
            if mode == "queue":
                rpc.prompt(text)
                return "queued as the next task"
            rpc.steer(text)
            return None  # steered: the run's own reply follows
        self.busy = True
        self.bridge.on_busy(self, True)
        rpc.prompt(text)
        return None

    def running(self):
        return self.busy


class Bridge:
    def __init__(self):
        self.cfg_path = _cfg_path()
        self.cfg = load_json(self.cfg_path)
        if not isinstance(self.cfg, dict):
            self.cfg = {}
        self.cfg.setdefault("yak_bin", os.environ.get("YAK_BIN") or "yak")
        self.cfg.setdefault("busy_mode", "steer")
        self.cfg.setdefault("approval_timeout", APPROVAL_TIMEOUT_DEFAULT)
        chats = self.cfg.setdefault("chats", {})
        if not isinstance(chats, dict):
            self.cfg["chats"] = chats = {}
        self.yak_bin = self.cfg["yak_bin"]
        self.busy_mode = self.cfg["busy_mode"]
        self.approval_timeout = self.cfg["approval_timeout"]
        self.chats = {}
        self.batches = {}
        self.channels = {}
        self.owner_chats = {}

    def save_chats(self):
        save_json(self.cfg_path, self.cfg)

    def chat(self, sender):
        chat = self.chats.get(sender)
        if chat is None:
            chat = Chat(self, sender, self.cfg["chats"].setdefault(sender, {}))
            if not chat.cfg.get("cwd"):
                chat.cfg["cwd"] = os.getcwd()
            self.chats[sender] = chat
        return chat

    def send(self, key, text, thread=None):
        channel_name, _, sender = key.partition(":")
        channel = self.channels.get(channel_name)
        if channel:
            try:
                channel.send(sender, text)
            except Exception as e:
                print(f"send failed: {e}", file=sys.stderr)

    def on_busy(self, chat, running):
        channel = self.channels.get(chat.sender_channel)
        if channel and chat.chat_sender:
            channel.typing(chat.chat_sender, running)

    def dispatch(self, sender, text):
        """One inbound chat message: local command, batch collection, or a
        model turn. Returns an immediate reply string, or None when the
        answer arrives later as session events."""
        text = (text or "").strip()
        if not text:
            return None
        chat = self.chat(sender)
        if chat.pending_approvals and text.lower() in ("y", "n", "/y", "/n"):
            return chat.decide_approval(text.lower().lstrip("/") == "y")
        if text.startswith("/"):
            return self.command(chat, sender, text)
        if chat.batch is not None:
            if len(chat.batch) >= BATCH_LIMIT:
                return "batch is full — /send or /cancel"
            chat.batch.append(text)
            if len(chat.batch) >= BATCH_LIMIT:
                return self.run_batch(chat)
            return f"batch {len(chat.batch)}/{BATCH_LIMIT} — /send to submit, /cancel to drop"
        return chat.submit(text)

    def run_batch(self, chat):
        messages = chat.batch or []
        chat.batch = None
        if not messages:
            return "nothing batched"
        sections = [f"[消息 {i}] {m}" for i, m in enumerate(messages, 1)]
        prompt = "以下是批量提交的多条内容，请按顺序作为同一次输入统一处理。\n\n" + "\n\n".join(sections)
        return chat.submit(prompt)

    def command(self, chat, sender, text):
        parts = text.split(maxsplit=1)
        cmd, arg = parts[0].lower(), (parts[1].strip() if len(parts) > 1 else "")
        if cmd == "/version":
            out = subprocess.run([self.yak_bin, "--version"], capture_output=True,
                                 text=True, timeout=10)
            return (out.stdout or out.stderr).strip() or "unknown"
        if cmd == "/help":
            return HELP
        if cmd in ("/new", "/clear"):
            chat.reject_pending()
            if chat.rpc:
                chat.rpc.close()
            chat.rpc = None
            chat.cfg.pop("session", None)
            self.save_chats()
            return "fresh session (the next message creates the thread)"
        if cmd == "/compact":
            chat.ensure_session().command("compact")
            return "compacting…"
        if cmd == "/model":
            if not arg:
                return f"model: {chat.cfg.get('model') or '(session default)'}"
            chat.cfg["model"] = arg
            if chat.rpc:
                chat.rpc.command("set_model", model=arg)
            self.save_chats()
            return f"model → {arg}"
        if cmd == "/cwd":
            if not arg:
                return f"cwd: {chat.cfg.get('cwd')}"
            path = expand(arg)
            if not os.path.isdir(path):
                return f"not a directory: {path}"
            chat.cfg["cwd"] = path
            self.save_chats()
            return (f"cwd → {path} (takes effect on /new; the running "
                    "session keeps its directory)")
        if cmd == "/status":
            return (f"cwd {chat.cfg.get('cwd')} · session {chat.cfg.get('session') or '—'}"
                    f" · model {chat.cfg.get('model') or '—'}"
                    f" · {'running' if chat.running() else 'idle'}"
                    f" · busy_mode {self.busy_mode}")
        if cmd == "/stop":
            chat.reject_pending()
            chat.ensure_session().command("interrupt")
            return "interrupting…"
        if cmd in ("/steer", "/queue", "/reject"):
            self.busy_mode = cmd[1:]
            return f"busy mode → {self.busy_mode}"
        if cmd == "/batch":
            if chat.batch is not None:
                return f"batch already collecting {len(chat.batch)}/{BATCH_LIMIT} — /send or /cancel"
            chat.batch = []
            return f"batch 0/{BATCH_LIMIT} — send messages, then /send (/cancel drops)"
        if cmd == "/send":
            if chat.batch is None:
                return "no batch collecting"
            return self.run_batch(chat)
        if cmd == "/cancel":
            chat.batch = None
            return "batch dropped"
        if cmd == "/y":
            return chat.decide_approval(True)
        if cmd == "/n":
            return chat.decide_approval(False)
        if cmd in ("/yes", "/no"):
            return chat.decide_approval(cmd == "/yes")
        if text.lower() in ("y", "n"):
            return chat.decide_approval(text.lower() == "y")
        return None


def weixin_login(state_path):
    print("weixin binding: scan the QR with WeChat and confirm on the phone.")
    qrcode, image_url = weixin.begin_login()
    weixin.open_binding_page(image_url)
    verify = None
    deadline = time.time() + 180
    while time.time() < deadline:
        try:
            out = weixin.poll_login(qrcode, verify)
        except weixin.WeixinError as e:
            print(f"login poll failed: {e}")
            return None
        status = out["status"]
        if status == "wait":
            pass
        elif status == "scaned":
            print("scanned — confirm on the phone…")
        elif status in ("need_verifycode", "verify_code_blocked"):
            verify = input("platform verify code: ").strip()
        elif status == "expired":
            print("QR expired — rerun `login`.")
            return None
        elif status == "confirmed":
            state = {"token": out["token"], "bot_id": out["bot_id"],
                     "owner": out["owner"], "base_url": out["base_url"], "cursor": ""}
            save_json(state_path, state)
            owner = out["owner"]
            print(f"bound: bot {out['bot_id']} · owner {owner}")
            print(f"add this chat's workspace to im.json chats[\"{owner}\"] (or edit later).")
            return state
        else:
            print(f"status: {status}")
        time.sleep(1.0)
    print("login timed out.")
    return None


PID_DIR_NAME = "run"

def _state_dir():
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), PID_DIR_NAME)
    os.makedirs(path, exist_ok=True)
    return path


def _pid_path():
    return os.path.join(_state_dir(), "pid")


def _log_path():
    return os.path.join(_state_dir(), "log")


def _weixin_state_path():
    return os.path.join(os.path.dirname(os.path.abspath(__file__)), "weixin.json")


def _cfg_path():
    return os.path.join(os.path.dirname(os.path.abspath(__file__)), "config.json")


def _pid_alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True  # exists but owned by someone else


def _read_pid():
    data = load_json(_pid_path())
    pid = data.get("pid")
    if isinstance(pid, int) and pid > 0:
        return data
    return None


bridge = None

STOP_NOTICE = ("yak IM bridge has stopped — messages will not be answered "
               "until you run `yak-im start` again.")


class WeixinChannel:
    """The bound weixin bot: a 30s long poll pushing into the shared inbox,
    replies through sendmessage with the chat's last context token."""

    name = "weixin"

    def __init__(self, state):
        self.state = state
        self.token = state["token"]
        self.base_url = state.get("base_url") or weixin.BASE_URL
        self.cursor = state.get("cursor") or ""
        self.bridge = None
        self.owner = state.get("owner")

    def loop(self, inbox, stop):
        weixin.notify(self.token, start=True)
        print(f"\x1b[32m● weixin channel up\x1b[0m", flush=True)
        while not stop.is_set():
            try:
                msgs, self.cursor = weixin.get_updates(
                    self.token, self.cursor, timeout_s=30.0)
                self.state["cursor"] = self.cursor
                save_json(_weixin_state_path(), self.state)
                for message in msgs:
                    info = weixin.extract_message(message)
                    if info["echo"] or not info["sender"]:
                        continue
                    inbox.put((self.name, info["sender"], info["text"],
                               info["attachments"],
                               {"context_token": info["context_token"]}))
            except weixin.WeixinError as e:
                print(f"weixin poll failed: {e.code} — retrying", file=sys.stderr)
                stop.wait(3.0)

    def send(self, sender, text):
        chat = self.bridge.chat(f"weixin:{sender}") if self.bridge else None
        context_token = chat.context_token if chat else None
        for chunk in split_reply(text):
            weixin.send_text(self.token, sender, chunk, context_token=context_token)

    def typing(self, sender, running):
        try:
            ticket = weixin.get_config(self.token, sender)
            if ticket:
                weixin.send_typing(self.token, sender, ticket, 1 if running else 2)
        except weixin.WeixinError:
            pass


bridge = None


HELP_TEXT = """yak im — the IM bridge daemon

  login     bind channels (weixin: browser page; qq: QR via qrencode)
  start     start the daemon in the background (requires a completed login)
  stop      stop the daemon
  status    pid, uptime and binding state
  log       the daemon's recent log lines
  attach    watch the IM conversations live in this terminal
  run       run in the foreground (what `start` spawns)

channels (im.json `channels` section): weixin binds via `yak-im login`;
telegram needs a @BotFather token; feishu needs app_id/app_secret and the
optional `lark-oapi` package; qq binds via `yak-im login` (QR) — group
chats answer @mentions, private chats answer directly, replies are always
passive (platform rule). Chat commands: /new /clear /compact /model /cwd
/status /stop /steer /queue /reject /batch /send /cancel /version /help,
approvals answered y / n.
"""


def action_login(channel=None):
    """Bind whatever is not bound yet; naming a channel forces its
    re-binding even when credentials already exist."""
    results = []
    if channel in (None, "weixin"):
        bound = load_json(_weixin_state_path()).get("token")
        if bound and channel is None:
            print("weixin already bound — `yak im login weixin` re-binds.")
        else:
            results.append(action_login_weixin())
    if channel in (None, "qq"):
        qq_cfg = (load_json(_cfg_path()).get("channels") or {}).get("qq") or {}
        if qq_cfg.get("app_id") and channel is None:
            print("qq already bound — `yak im login qq` re-binds.")
        else:
            results.append(action_login_qq())
    if channel is None and not results:
        print("all channels bound.")
    return max(results) if results else 0


def action_login_weixin():
    state_path = _weixin_state_path()
    state = weixin_login(state_path)
    return 0 if state and state.get("token") else 1


def action_login_qq():
    """The QQ binding: QR rendered with `qrencode`, scanned with the QQ
    app; credentials land in im.json channels.qq."""
    import channels.qq as qq_channel
    creds = qq_channel.qq_login()
    if not creds:
        return 1
    cfg = load_json(_cfg_path())
    channels_cfg = cfg.setdefault("channels", {}).setdefault("qq", {})
    channels_cfg.update({"app_id": creds["app_id"], "app_secret": creds["app_secret"]})
    if creds.get("owner"):
        channels_cfg.setdefault("owner", creds["owner"])
    save_json(_cfg_path(), cfg)
    print(f"qq bound: app {creds['app_id']} · owner {creds.get('owner') or '(first chat)'}")
    return 0


def action_start():
    state = load_json(_weixin_state_path())
    if not state.get("token"):
        print("not bound yet — run `yak im login` first.")
        return 1
    if (pid_data := _read_pid()) and _pid_alive(pid_data["pid"]):
        print(f"already running (pid {pid_data['pid']}) — /status or `yak im log`.")
        return 0
    log_path = _log_path()
    log = open(log_path, "a", encoding="utf-8")
    log.write(f"\n── started {time.strftime('%Y-%m-%d %H:%M:%S')} ──\n")
    log.flush()
    proc = subprocess.Popen(
        [sys.executable, os.path.abspath(__file__), "run"],
        stdin=subprocess.DEVNULL, stdout=log, stderr=log,
        start_new_session=True)
    log.close()
    save_json(_pid_path(), {"pid": proc.pid, "started": time.time()})
    print(f"started (pid {proc.pid}) · log {_log_path()}")
    return 0


def action_stop():
    pid_data = _read_pid()
    if not pid_data or not _pid_alive(pid_data["pid"]):
        if pid_data:
            os.remove(_pid_path())
        print("not running.")
        return 0
    os.kill(pid_data["pid"], signal.SIGTERM)
    for _ in range(50):
        if not _pid_alive(pid_data["pid"]):
            break
        time.sleep(0.1)
    else:
        os.kill(pid_data["pid"], signal.SIGKILL)
    if os.path.exists(_pid_path()):
        os.remove(_pid_path())
    print(f"stopped (pid {pid_data['pid']}).")
    return 0


def action_status():
    pid_data = _read_pid()
    state = load_json(_weixin_state_path())
    bound = bool(state.get("token"))
    if not pid_data or not _pid_alive(pid_data["pid"]):
        print(f"daemon: not running · weixin: {'bound' if bound else 'not bound'}"
              + (" — `yak im login` first" if not bound else " — `yak im start`"))
        return 1 if bound else 1
    uptime = time.time() - pid_data.get("started", time.time())
    mins, secs = divmod(int(uptime), 60)
    hours, mins = divmod(mins, 60)
    print(f"daemon: running (pid {pid_data['pid']}, up {hours}h{mins:02d}m{secs:02d}s)"
          f" · weixin: bound · bot {state.get('bot_id') or '?'}"
          f" · owner {state.get('owner') or '?'}"
          + (f" · busy {bridge.busy_mode}" if bridge else ""))
    return 0


def action_log():
    try:
        with open(_log_path(), "rb") as f:
            f.seek(0, os.SEEK_END)
            size = f.tell()
            f.seek(max(0, size - 8192))
            lines = f.read().decode("utf-8", "replace").splitlines()
    except OSError:
        print("no log yet.")
        return 0
    for line in lines[-30:]:
        print(line)
    return 0


def action_run():
    state = load_json(_weixin_state_path())
    if not state.get("token"):
        print("not bound — run `yak im login` first.", file=sys.stderr)
        return 1
    save_json(_pid_path(), {"pid": os.getpid(), "started": time.time()})
    global bridge
    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    try:
        run_daemon(state, stop)
    finally:
        if os.path.exists(_pid_path()):
            os.remove(_pid_path())
    return 0


def run_daemon(state, stop):
    """Bring up every configured channel and route the shared inbox: owner
    allowlist per channel, first-sender-becomes-owner for channels without a
    configured owner, chats keyed `channel:sender`. On shutdown every
    channel tells its owner the bridge has stopped."""
    global bridge
    bridge = Bridge()

    def log(icon, color, line):
        # stdout rides ~/.yak/im/log when the daemon was started detached;
        # chat-shaped colored lines keep `attach` reading like a live session
        stamp = time.strftime("[%H:%M:%S]")
        print(f"\x1b[2m{stamp}\x1b[0m \x1b[{color}m{icon}\x1b[0m {line}", flush=True)
    bridge.log = log
    bridge.channels = {}
    inbox = queue.Queue()

    def register(channel, owner):
        channel.bridge = bridge
        bridge.channels[channel.name] = channel
        if owner:
            bridge.owner_chats[channel.name] = owner

    if state.get("token"):
        register(WeixinChannel(state), state.get("owner"))
    channels_cfg = bridge.cfg.get("channels") or {}
    tg = channels_cfg.get("telegram") or {}
    if isinstance(tg, dict) and tg.get("token"):
        register(telegram.TelegramChannel(tg["token"]), tg.get("owner"))
    fs = channels_cfg.get("feishu") or {}
    if isinstance(fs, dict) and fs.get("app_id") and fs.get("app_secret"):
        try:
            register(feishu.FeishuChannel(fs["app_id"], fs["app_secret"]), fs.get("owner"))
        except RuntimeError as e:
            print(f"feishu channel skipped: {e}", file=sys.stderr)
    qq_cfg = channels_cfg.get("qq") or {}
    if isinstance(qq_cfg, dict) and qq_cfg.get("app_id") and qq_cfg.get("app_secret"):
        register(qq.QQChannel(qq_cfg["app_id"], qq_cfg["app_secret"], owner=qq_cfg.get("owner")))

    if not bridge.channels:
        print("no channels configured — bind weixin (`yak-im login`) or add "
              "channels to im.json.", file=sys.stderr)
        return 1

    # legacy chat keys: the weixin chat was stored under the bare owner id
    owner = state.get("owner")
    if owner and owner in bridge.cfg.get("chats", {}):
        bridge.cfg["chats"][f"weixin:{owner}"] = bridge.cfg["chats"].pop(owner)
        bridge.save_chats()

    def push(item):
        channel_name, sender, text, attachments, meta = item
        chat = bridge.chat(f"{channel_name}:{sender}")
        chat.sender_channel = channel_name
        chat.chat_sender = sender
        chat.channel_meta = dict(meta)
        if "context_token" in meta:
            chat.context_token = meta["context_token"]
        channel = bridge.channels[channel_name]
        log("◀", "36", f"[{channel_name}] {(text or '(no text)')[:160]}")
        owner = bridge.owner_chats.get(channel_name)
        if owner is not None and sender != owner:
            if channel_name == "weixin":
                bridge.send(f"{channel_name}:{sender}",
                            "this bot answers its owner only.")
            return
        if owner is None and channel_name != "weixin":
            # first sender becomes the owner, saved so it sticks
            bridge.owner_chats[channel_name] = sender
            channel_cfg = bridge.cfg.setdefault("channels", {}).setdefault(channel_name, {})
            channel_cfg["owner"] = sender
            bridge.save_chats()
            bridge.send(f"{channel_name}:{sender}", "you are now this bot's owner.")
        if attachments:
            bridge.send(f"{channel_name}:{sender}",
                        f"[{' · '.join(attachments)} — binary transfers land in a later build]")
        if text:
            reply = bridge.dispatch(f"{channel_name}:{sender}", text)
            if reply:
                bridge.send(f"{channel_name}:{sender}", reply)
                first = reply.strip().splitlines()[0][:160] if reply.strip() else ""
                log("▶", "32", first + (f"  (+{len(reply)} chars)" if len(reply) > 160 else ""))

    threads = [threading.Thread(target=ch.loop, args=(inbox, stop), daemon=True)
               for ch in bridge.channels.values()]
    for t in threads:
        t.start()
    print(f"channels up: {', '.join(bridge.channels)} · busy {bridge.busy_mode}")
    try:
        while not stop.is_set():
            try:
                item = inbox.get(timeout=1.0)
            except queue.Empty:
                continue
            if item is None:
                break
            try:
                push(item)
            except Exception as e:
                print(f"message routing failed: {e}", file=sys.stderr)
    finally:
        # the stop notice goes to every bound owner in English, best effort
        for (channel_name, owner) in list(bridge.owner_chats.items()):
            if owner and bridge.channels.get(channel_name):
                try:
                    bridge.send(f"{channel_name}:{owner}", STOP_NOTICE)
                except Exception as e:
                    print(f"stop notice failed for {channel_name}: {e}",
                          file=sys.stderr)
    return 0


def action_attach():
    """Live view: follow the daemon's transcript in this terminal."""
    log_path = _log_path()
    if not os.path.exists(log_path):
        print("no transcript yet — start the daemon first.")
        return 1
    print(f"\x1b[2m── live · ctrl-c to stop watching (the daemon keeps running) ──\x1b[0m")
    os.execvp("tail", ["tail", "-n", "40", "-f", log_path])


ACTIONS = {"login": action_login, "start": action_start, "stop": action_stop,
           "status": action_status, "log": action_log, "attach": action_attach,
           "run": action_run, "help": lambda: (print(HELP_TEXT), 0)[1]}


def main():
    args = sys.argv[1:]
    if args and args[0] in ("login",) and len(args) > 1:
        channel = args[1]
        if channel not in ("weixin", "qq"):
            print(f"unknown channel {channel!r} (weixin, qq)")
            return 2
        return action_login(channel)
    action = args[0] if args else "help"
    fn = ACTIONS.get(action)
    if fn is None:
        print(f"unknown action {action!r}\n\n{HELP_TEXT}")
        return 2
    return fn()


if __name__ == "__main__":
    sys.exit(main())
