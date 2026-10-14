#!/usr/bin/env python3
"""ssh — run commands and read files on remote machines.

Remote access without a resident daemon: plain `ssh` subprocesses
with hardened arguments (BatchMode, no forwarding, no locale, strict host
keys against this extension's own known_hosts), one call = one ssh run.
No persistent connection and nothing installed on the remote side.

Hosts live in `ssh.json` beside this script or in `<user_dir>/ssh.json`
(YAK_USER_PATH honored):

    {
      "dev": {"host": "10.0.0.5", "user": "root", "port": 22},
      "gpu": {"host": "gpu.lab", "user": "me", "identity_file": "~/.ssh/id_ed25519",
              "known_hosts": "accept-new"}
    }

A host entry may set `host`, `user`, `port`, `identity_file`,
`config_file` (an ssh -F config), and `known_hosts`: `"strict"` (default)
pins the key on first connect and refuses any later change, `"accept-new"`
pins on first connect and trusts ssh's own TOFU default, or a path to a
shared known_hosts file. `strict` and `accept-new` use this extension's
own file (`<user_dir>/ssh_known_hosts`), never `~/.ssh/known_hosts`, and a
changed host key is always an error naming the host.

Tools:

    ssh_exec  run a shell command on a host; stdout+stderr (merged) and the
              exit status come back as the result. Optional `timeout`
              bounds the run in seconds (default 300, 0 = host default).
    ssh_read  read a remote file: `path`, optional `offset`/`limit` in
              lines (same semantics as the built-in read tool).
    ssh_write write a remote file: `path` plus `content` (created or
              overwritten; parent directories are not created).

Every call is exec-tier like the built-in bash tool: the approval
matrix and the ask-list apply to it. A prompt (BatchMode=yes) cannot
happen: an unauthenticated host fails fast instead of hanging.

Run this file directly to check the setup:

    ./ssh.py status    list hosts and probe each with a short command
"""

import json
import os
import subprocess
import sys
import threading
import time

# Live calls, so an interrupt from the host can stop them: the protocol
# loop is blocked inside the call when it arrives, and only a reader
# thread is awake to hear it.
_live = set()
_live_lock = threading.Lock()
_abandoned = threading.Event()
_out_lock = threading.RLock()


def note(text):
    sys.stderr.write(f"ssh: {text}\n")
    sys.stderr.flush()


def reply(obj):
    with _out_lock:
        sys.stdout.write(json.dumps(obj) + "\n")
        sys.stdout.flush()


def user_dir():
    return os.environ.get("YAK_USER_PATH") or os.path.join(
        os.path.expanduser("~"), ".yak")


def _load_hosts():
    paths = [os.path.join(os.path.dirname(os.path.realpath(__file__)),
                          "ssh.json"),
             os.path.join(user_dir(), "ssh.json")]
    for path in paths:
        try:
            with open(path, encoding="utf-8") as f:
                raw = json.load(f)
        except FileNotFoundError:
            continue
        except (OSError, json.JSONDecodeError) as e:
            note(f"config {path} unreadable: {e}")
            return {}
        hosts = {}
        for name, entry in (raw.get("hosts") if isinstance(raw, dict) else raw).items():
            if isinstance(entry, dict):
                hosts[name] = entry
        return hosts
    return {}


HOSTS = _load_hosts()


def _check_field(name, value):
    """No leading `-` (it would be read as an ssh option), no whitespace,
    no control characters — a host field must not smuggle arguments."""
    if not isinstance(value, str) or not value or value.startswith("-") \
            or any(c.isspace() or ord(c) < 32 or ord(c) == 127 for c in value):
        raise ValueError(f"invalid {name}: {value!r}")


def _opt_path(name, value):
    """A path handed to an ssh option: no quotes, no control characters."""
    if not isinstance(value, str) or not value or \
            any(ord(c) < 32 or ord(c) == 127 or c in '"' for c in value):
        raise ValueError(f"invalid {name}: {value!r}")
    return value.replace("%", "%%")


def ssh_argv(entry):
    """Hardened ssh command line up to the host, a strict list: no prompts,
    no forwarding of any kind, no shared connections, no commands from
    the config, no locale forwarding — and strict host keys against this
    extension's own file under a fixed alias."""
    host = entry.get("host", "")
    _check_field("host", host)
    user = entry.get("user")
    if user:
        _check_field("user", user)
    port = entry.get("port")
    if port is not None and (not isinstance(port, int) or port < 1 or port > 65535):
        raise ValueError(f"invalid port: {port}")
    mode = entry.get("known_hosts", "strict")
    opts = ["-T", "-a", "-x",
            "-o", "BatchMode=yes",
            "-o", "ClearAllForwardings=yes",
            "-o", "ForwardAgent=no",
            "-o", "ForwardX11=no",
            "-o", "ControlMaster=no",
            "-o", "ControlPath=none",
            "-o", "RemoteCommand=none",
            "-o", "PermitLocalCommand=no",
            "-o", "SendEnv=-*",
            "-o", "ServerAliveInterval=15",
            "-o", "GlobalKnownHostsFile=none",
            "-o", "HashKnownHosts=no"]
    if entry.get("config_file"):
        opts += ["-F", _opt_path("config_file", entry["config_file"])]
    if mode == "strict" or mode == "accept-new":
        opts += ["-o", "StrictHostKeyChecking=" +
                 ("yes" if mode == "strict" else "accept-new"),
                 "-o", "UserKnownHostsFile=" + _opt_path(
                     "known hosts file", os.path.join(user_dir(), "ssh_known_hosts")),
                 "-o", "HostKeyAlias=" + host]
    elif isinstance(mode, str) and mode and not mode.startswith("-") \
            and not any(c.isspace() or ord(c) < 32 or ord(c) == 127
                        or c == '%' for c in mode):
        opts += ["-o", "StrictHostKeyChecking=yes",
                 "-o", "UserKnownHostsFile=" + _opt_path("known hosts file", mode)]
    else:
        raise ValueError(f"invalid known_hosts mode: {mode!r}")
    if user:
        opts += ["-l", user]
    if port:
        opts += ["-p", str(port)]
    ident = entry.get("identity_file")
    if ident:
        ident = os.path.expanduser(ident) if ident.startswith("~") else ident
        opts += ["-o", "IdentityFile=" + _opt_path("identity file", ident),
                 "-o", "IdentitiesOnly=yes"]
    opts += ["--", host]
    return opts


def run_ssh(name, remote_command, timeout=0, stdin_text=None):
    """One ssh run: returns (exit_status, merged_output, timed_out). The
    remote command is ONE string — ssh hands the joined arguments to the
    remote login shell for another round of parsing, so pieces would be
    re-spliced and misread; a single pre-quoted string survives it. The
    process is registered so an interrupt kills it; a timeout kill keeps
    the output already collected."""
    entry = HOSTS.get(name)
    if entry is None:
        known = ", ".join(sorted(HOSTS)) or "none configured"
        return 1, f"unknown host {name!r} (ssh.json hosts: {known})", False
    argv = ["ssh"] + ssh_argv(entry) + [remote_command]
    proc = subprocess.Popen(argv, stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT)
    with _live_lock:
        _live.add(proc)
    out = []

    def pump():
        for chunk in iter(lambda: proc.stdout.read(65536), b""):
            out.append(chunk)

    reader = threading.Thread(target=pump, daemon=True)
    reader.start()
    if stdin_text is not None:
        try:
            proc.stdin.write(stdin_text.encode("utf-8", "replace"))
        except OSError:
            pass
    proc.stdin.close()
    deadline = time.monotonic() + (timeout if timeout and timeout > 0 else 600)
    while proc.poll() is None:
        if _abandoned.is_set():
            proc.kill()
            reader.join(2)
            with _live_lock:
                _live.discard(proc)
            return 130, b"".join(out).decode("utf-8", "replace") + "\n[interrupted]", True
        if time.monotonic() >= deadline:
            proc.kill()
            reader.join(2)
            with _live_lock:
                _live.discard(proc)
            return 124, b"".join(out).decode("utf-8", "replace") + \
                f"\n[remote command timed out after {timeout}s (process killed)]", True
        time.sleep(0.05)
    reader.join(2)
    with _live_lock:
        _live.discard(proc)
    text = b"".join(out).decode("utf-8", "replace")
    entry = HOSTS.get(name) or {}
    if proc.returncode != 0 and "host key verification failed" in text.lower() \
            and "REMOTE HOST IDENTIFICATION HAS CHANGED" not in text \
            and entry.get("known_hosts", "strict") != "accept-new":
        # a genuinely new host key (not a changed one): name the two ways to pin it
        text += ("\n[first connect? the host key is not pinned yet; add it: "
                 f"ssh-keyscan -p {entry.get('port', 22)} "
                 f"{entry.get('host', '')} >> {os.path.join(user_dir(), 'ssh_known_hosts')} "
                 "(then verify the fingerprint out of band), or set "
                 f'"known_hosts": "accept-new" for this host]')
    return proc.returncode, text, False


def sh_quote(text):
    """Single-quote for the remote shell; embedded quotes close, escape
    and reopen — the only safe splice into a shell string."""
    return "'" + text.replace("'", "'\\''") + "'"


def do_exec(args):
    command = args.get("command", "")
    if not command.strip():
        return "error: empty command"
    timeout = int(args.get("timeout", 300) or 0)
    status, text, _ = run_ssh(args.get("host", ""), command, timeout)
    return text.rstrip("\n") + f"\n[exit {status}]"


def do_read(args):
    path = args.get("path", "")
    if not path:
        return "error: path is required"
    offset = int(args.get("offset", 1) or 1)
    limit = int(args.get("limit", 0) or 0)
    if offset > 1 and limit > 0:
        sed = f"{offset},{offset + limit - 1}"
    elif offset > 1:
        sed = f"{offset},$"
    elif limit > 0:
        sed = f"1,{limit}"
    else:
        sed = ""
    command = (f"sed -n {sh_quote(sed + 'p')} " if sed else "cat ") + sh_quote(path)
    status, text, timed_out = run_ssh(args.get("host", ""), command, 60)
    if timed_out:
        return "error: " + text
    if status == 0:
        return text
    return f"error: read {path}: exit {status}\n{text}"


def do_write(args):
    path = args.get("path", "")
    content = args.get("content", "")
    if not path:
        return "error: path is required"
    status, text, _ = run_ssh(
        args.get("host", ""),
        "cat > " + sh_quote(path), 60, stdin_text=content)
    if status == 0:
        return f"wrote {len(content.encode('utf-8'))} bytes to {path}"
    return f"error: write {path}: exit {status}\n{text}"


TOOLS = [
    {"name": "ssh_exec",
     "description": "Run a shell command on a remote host over ssh. "
                    "Merged stdout/stderr and the exit status come back. "
                    "Hosts are named in ssh.json; ask the user before "
                    "adding one.",
     "parameters": {"type": "object",
                    "properties": {
                        "host": {"type": "string", "description": "host name from ssh.json"},
                        "command": {"type": "string", "description": "shell command to run"},
                        "timeout": {"type": "integer", "description": "seconds, default 300"}},
                    "required": ["host", "command"]}},
    {"name": "ssh_read",
     "description": "Read a remote file by path, with optional line "
                    "offset/limit like the built-in read tool.",
     "parameters": {"type": "object",
                    "properties": {
                        "host": {"type": "string", "description": "host name from ssh.json"},
                        "path": {"type": "string", "description": "absolute path on the remote"},
                        "offset": {"type": "integer", "description": "1-based first line"},
                        "limit": {"type": "integer", "description": "max lines to return"}},
                    "required": ["host", "path"]}},
    {"name": "ssh_write",
     "description": "Write a remote file (created or overwritten; parent "
                    "directories are not created).",
     "parameters": {"type": "object",
                    "properties": {
                        "host": {"type": "string", "description": "host name from ssh.json"},
                        "path": {"type": "string", "description": "absolute path on the remote"},
                        "content": {"type": "string", "description": "full file content"}},
                    "required": ["host", "path", "content"]}},
]


def status():
    if not HOSTS:
        return "no hosts configured: write ssh.json (beside this script or in " \
               + os.path.join(user_dir(), "") + ") — see the header of ssh.py"
    rows = []
    for name in sorted(HOSTS):
        entry = HOSTS[name]
        where = f"{entry.get('user', '')}@{entry.get('host', '')}" \
            if entry.get("user") else entry.get("host", "")
        code, text, _ = run_ssh(name, "true", 10)
        lines = text.strip().splitlines()
        state = "ok" if code == 0 else "unreachable: " + (lines[-1][:80] if lines else f"exit {code}")
        rows.append(f"  {name:12} {where:24} {state}")
    return "hosts:\n" + "\n".join(rows)


def main():
    import queue
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
            reply({"id": mid, "result": {
                "tools": TOOLS,
                "commands": ["ssh"],
                "tool_timeout": 3600}})
        elif kind == "call_tool":
            _abandoned.clear()
            try:
                fn = {"ssh_exec": do_exec, "ssh_read": do_read,
                      "ssh_write": do_write}.get(msg.get("name"))
                result = fn(msg.get("args") or {}) if fn else f"unknown tool {msg.get('name')}"
            except Exception as e:
                result = f"ssh: {e}"
            reply({"id": mid, "result": result})
        elif kind == "run_command" and msg.get("name") == "ssh":
            _abandoned.clear()
            try:
                result = status()
            except Exception as e:
                result = f"ssh: {e}"
            reply({"id": mid, "result": result})
        elif kind == "interrupt":
            _abandoned.set()
            with _live_lock:
                for proc in list(_live):
                    try:
                        proc.kill()
                    except OSError:
                        pass
        elif kind == "shutdown":
            break


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "status":
        print(status())
    else:
        main()
