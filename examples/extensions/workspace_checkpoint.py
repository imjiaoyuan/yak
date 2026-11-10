#!/usr/bin/env python3
# Workspace checkpoints for /tree — no git required.
#
# the reference's answer to "the agent edited files, then I jumped back in the
# conversation — what about my code?" is: the core rewinds only the
# transcript; an extension snapshots the workspace and restores it. This is
# that extension, portable to projects with no VCS at all.
#
#   turn_start           — mirror the working tree into a shadow dir (one per
#                          turn, named for the wall-clock moment it was
#                          taken), after pruning checkpoints older
#                          than KEEP_DAYS
#   tool_call            — before the write / edit tool runs: copy the file
#                          it is about to touch into a pre-tool snapshot (a
#                          cheap, precise mirror of just the named targets,
#                          recording whether each existed). This is what
#                          `/undo` rolls back
#   session_before_tree  — /tree picked a jump point: merge the snapshots
#                          the dropped turns began with, oldest winning,
#                          back into the working tree
#
# The tree merge is additive by design: files the dropped turns edited come
# back as they were at the oldest selected snapshot (the moment the first
# dropped turn began), files untouched since the cut point stay as they are
# now, and nothing is ever deleted — files the dropped turns created stay
# behind. This example assumes the project's dropped turns own the newest
# snapshots; do not run two sessions against the same workspace if you need
# reliable /tree restore.
#
# /undo [N] restores the last N pre-tool snapshots (default 1), oldest of
# the N winning per file; files a snapshot recorded as not-yet-existing are
# removed again. Turn mirrors and pre-tool snapshots live in separate
# folders (`turns/`, `pre/`) under the project's shadow dir, so the two
# restore paths never pick up each other's snapshots.
#
# Install into ~/.yak/extensions/ (or .yak/extensions/), chmod +x, /reload.
# The shadow root lives in the system temp dir; override with
# YAK_CHECKPOINT_DIR. Set YAK_CHECKPOINT_BASH=1 to also mirror the whole
# tree before every bash call (faithful but heavy; the per-turn mirror
# already bounds what an undo-less bash can lose). Turn it off for a
# session by removing the file.
import json
import os
import shutil
import sys
import tempfile
import time

# don't mirror these anywhere they appear: VCS internals, dependency trees,
# the shadow dir itself if it sits under the project
SKIP_DIRS = {".git", ".hg", ".svn", "node_modules", "__pycache__",
             ".venv", "venv", "target", "dist", "build", ".yak"}
KEEP_DAYS = 7
MANIFEST = ".yak-checkpoint-files.json"

# a snapshot stamp is the wall clock in ULID time-ordering: 10 Crockford
# base32 chars encoding the millisecond (the first half of every ULID yak
# mints), so stamps sort exactly the way turn ids do
STAMP_LEN = 10
CROCKFORD = "0123456789abcdefghjkmnpqrstvwxyz"
CROCKFORD_ALPHABET = frozenset(CROCKFORD)


def shadow_root():
    root = os.environ.get("YAK_CHECKPOINT_DIR") or os.path.join(
        tempfile.gettempdir(), "yak-workspace-checkpoints")
    os.makedirs(root, exist_ok=True)
    return root


def project_key(cwd):
    # /home/me/work/foo -> home-me-work-foo
    return os.path.abspath(cwd).strip(os.sep).replace(os.sep, "-")


def project_root(cwd):
    root = os.path.join(shadow_root(), project_key(cwd))
    os.makedirs(root, exist_ok=True)
    return root


def turns_root(cwd):
    """Per-turn full mirrors; what a /tree restore folds back."""
    root = os.path.join(project_root(cwd), "turns")
    os.makedirs(root, exist_ok=True)
    return root


def pre_root(cwd):
    """Pre-tool snapshots (write/edit targets); what /undo rolls back."""
    root = os.path.join(project_root(cwd), "pre")
    os.makedirs(root, exist_ok=True)
    return root


def encode_stamp(ms):
    chars = ["0"] * STAMP_LEN
    for slot in reversed(range(STAMP_LEN)):
        chars[slot] = CROCKFORD[ms & 31]
        ms >>= 5
    return "".join(chars)


_LAST_STAMP_MS = [0]


def stamp_now():
    ms = int(time.time() * 1000)
    if ms <= _LAST_STAMP_MS[0]:
        # two turn_starts inside one wall-clock millisecond must not
        # collide, or the later snapshot silently overwrites the earlier
        # one — borrow the next millisecond, the way a monotonic ULID does
        ms = _LAST_STAMP_MS[0] + 1
    _LAST_STAMP_MS[0] = ms
    return encode_stamp(ms)


def is_stamp(name):
    return (len(name) == STAMP_LEN
            and all(c in CROCKFORD_ALPHABET for c in name))


def snapshot_dir(root, stamp):
    return os.path.join(root, stamp)


def prune(root, keep_seconds=KEEP_DAYS * 86400):
    now = time.time()
    try:
        stamps = sorted(os.listdir(root))
    except OSError:
        return
    for stamp in stamps:
        path = os.path.join(root, stamp)
        try:
            if now - os.path.getmtime(path) > keep_seconds:
                shutil.rmtree(path, ignore_errors=True)
        except OSError:
            pass


def mirror(src, dst):
    """Copy src's tree into dst. Every file is an independent copy; symlinks
    and hardlinks to the live file would not preserve its former contents."""
    os.makedirs(dst, exist_ok=True)
    for dirpath, dirnames, filenames in os.walk(src):
        dirnames[:] = [d for d in dirnames
                       if d not in SKIP_DIRS
                       and os.path.join(dirpath, d) != shadow_root()]
        rel = os.path.relpath(dirpath, src)
        target = dst if rel == "." else os.path.join(dst, rel)
        os.makedirs(target, exist_ok=True)
        for name in filenames:
            s = os.path.join(dirpath, name)
            d = os.path.join(target, name)
            if os.path.islink(s):
                continue  # never follow out-of-tree links
            shutil.copy2(s, d)

def take_snapshot(cwd, stamp):
    root = turns_root(cwd)
    prune(root)
    final = snapshot_dir(root, stamp)
    staging = tempfile.mkdtemp(prefix=".snapshot-", dir=root)
    try:
        mirror(cwd, staging)
        if os.path.exists(os.path.join(staging, MANIFEST)):
            raise ValueError(f"workspace contains reserved checkpoint name: {MANIFEST}")
        files = []
        for dirpath, _, filenames in os.walk(staging):
            for name in filenames:
                files.append(os.path.relpath(os.path.join(dirpath, name), staging))
        with open(os.path.join(staging, MANIFEST), "w", encoding="utf-8") as f:
            json.dump(sorted(files), f)
        os.replace(staging, final)
    except BaseException:
        shutil.rmtree(staging)
        raise


def restore(cwd, dropped_count):
    """Fold the last `dropped_count` snapshots back into the working tree,
    oldest winning per file. The dropped turns are the active branch's tail,
    so their begin-of-turn snapshots are the newest ones on disk for the
    project; the oldest of them is the state the first dropped turn found,
    which is the state the jump returns the workspace to."""
    root = turns_root(cwd)
    stamps = sorted(s for s in os.listdir(root) if is_stamp(s))
    if dropped_count > len(stamps):
        raise FileNotFoundError(
            f"cannot restore {dropped_count} turns: only {len(stamps)} checkpoints remain")
    selected = stamps[-dropped_count:] if dropped_count > 0 else []
    merged = {}
    for stamp in selected:
        snap = snapshot_dir(root, stamp)
        with open(os.path.join(snap, MANIFEST), encoding="utf-8") as f:
            files = json.load(f)
        if not isinstance(files, list) or any(not isinstance(p, str) for p in files):
            raise ValueError(f"invalid checkpoint manifest: {snap}")
        for rel in files:
            src = os.path.join(snap, rel)
            if not os.path.isfile(src):
                raise FileNotFoundError(f"checkpoint file missing: {src}")
            if (os.path.islink(src) or os.path.commonpath(
                    (os.path.realpath(src), os.path.realpath(snap))) != os.path.realpath(snap)):
                raise ValueError(f"checkpoint file is unsafe: {src}")
            dst = os.path.join(cwd, rel)
            if dst not in merged:
                merged[dst] = src
    staging = tempfile.mkdtemp(prefix=".restore-", dir=cwd)
    try:
        staged = {}
        for dst, src in merged.items():
            path = os.path.join(staging, str(len(staged)))
            shutil.copy2(src, path)
            staged[dst] = path
        for dst, path in staged.items():
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            if os.path.commonpath((os.path.realpath(os.path.dirname(dst)),
                                   os.path.realpath(cwd))) != os.path.realpath(cwd):
                raise ValueError(f"restore path escapes the workspace: {dst}")
            os.replace(path, dst)
    finally:
        shutil.rmtree(staging)


PRE_MANIFEST = ".yak-pretool.json"
SNAPSHOT_TOOLS = {"write", "edit"}  # tools whose named targets are snapshotted


def targets_of(tool, args, cwd):
    """The workspace files a tool call is about to touch: write and edit name
    a `path`; bash names nothing (a whole-tree mirror per call is opt-in via
    YAK_CHECKPOINT_BASH=1). Paths that escape the workspace or are symlinks
    are skipped — a snapshot must never follow a link out of the tree."""
    if tool in SNAPSHOT_TOOLS and isinstance(args, dict):
        raw = args.get("path")
        paths = [raw] if isinstance(raw, str) else []
    elif tool == "bash" and os.environ.get("YAK_CHECKPOINT_BASH") == "1":
        paths = ["*"]  # whole tree
    else:
        return []
    real_cwd = os.path.realpath(cwd)
    out = []
    for raw in paths:
        if raw == "*":
            out.append("*")
            continue
        abs_path = os.path.realpath(os.path.join(cwd, raw)) if not os.path.isabs(raw) \
            else os.path.realpath(raw)
        if os.path.commonpath((abs_path, real_cwd)) != real_cwd:
            continue  # outside the workspace: not ours to snapshot
        if os.path.islink(os.path.join(cwd, raw)):
            continue
        out.append(os.path.relpath(abs_path, real_cwd))
    return out


def pre_tool_snapshot(cwd, targets):
    """Copy the named files (or the whole tree for `*`) into a pre snapshot:
    one stamp-ordered folder per tool call, each with a manifest recording
    per file whether it existed — that bit is what lets /undo remove again
    a file the tool created. Nothing here can fail the tool call: a snapshot
    that cannot be taken is skipped with a stderr note (fail-open, like the
    host's hooks)."""
    if not targets:
        return
    root = pre_root(cwd)
    prune(root)
    final = snapshot_dir(root, stamp_now())
    staging = tempfile.mkdtemp(prefix=".pre-", dir=root)
    try:
        entries = []
        if targets == ["*"]:
            mirror(cwd, staging)
            if os.path.exists(os.path.join(staging, PRE_MANIFEST)):
                raise ValueError("reserved checkpoint name in workspace")
            for dirpath, _, filenames in os.walk(staging):
                for name in filenames:
                    rel = os.path.relpath(os.path.join(dirpath, name), staging)
                    entries.append({"path": rel, "existed": True})
        else:
            for rel in targets:
                src = os.path.join(cwd, rel)
                existed = os.path.isfile(src) and not os.path.islink(src)
                if existed:
                    dst = os.path.join(staging, rel)
                    os.makedirs(os.path.dirname(dst), exist_ok=True)
                    shutil.copy2(src, dst)
                entries.append({"path": rel, "existed": existed})
        with open(os.path.join(staging, PRE_MANIFEST), "w", encoding="utf-8") as f:
            json.dump(entries, f)
        os.replace(staging, final)
    except BaseException as e:
        shutil.rmtree(staging, ignore_errors=True)
        sys.stderr.write("checkpoint: skipped (%s)\n" % e)


def undo(cwd, count):
    """Fold the last `count` pre-tool snapshots back, oldest of them winning
    per file; a file the oldest snapshot recorded as not existing is removed.
    Returns the human summary."""
    root = pre_root(cwd)
    stamps = sorted(s for s in os.listdir(root) if is_stamp(s))
    if not stamps:
        return "nothing to undo: no pre-tool snapshots for this workspace"
    selected = stamps[-min(count, len(stamps)):]
    merged = {}  # rel -> (snapshot dir, existed); the oldest stamp wins
    for stamp in selected:
        snap = snapshot_dir(root, stamp)
        with open(os.path.join(snap, PRE_MANIFEST), encoding="utf-8") as f:
            entries = json.load(f)
        if not isinstance(entries, list):
            raise ValueError(f"invalid pre-tool manifest: {snap}")
        for entry in entries:
            rel, existed = entry.get("path"), entry.get("existed")
            if not isinstance(rel, str) or not isinstance(existed, bool):
                raise ValueError(f"invalid pre-tool manifest entry: {snap}")
            if rel not in merged:
                merged[rel] = (snap, existed)
    real_cwd = os.path.realpath(cwd)
    restored, removed = [], []
    staging = tempfile.mkdtemp(prefix=".undo-", dir=cwd)
    try:
        staged = {}
        for rel, (snap, existed) in merged.items():
            if os.path.islink(rel) or os.path.commonpath(
                    (os.path.realpath(os.path.join(cwd, rel)), real_cwd)) != real_cwd:
                raise ValueError(f"undo path escapes the workspace: {rel}")
            if existed:
                src = os.path.join(snap, rel)
                if not os.path.isfile(src):
                    raise FileNotFoundError(f"checkpoint file missing: {src}")
                path = os.path.join(staging, str(len(staged)))
                shutil.copy2(src, path)
                staged[os.path.join(cwd, rel)] = path
                restored.append(rel)
            else:
                removed.append(rel)
        for dst, path in staged.items():
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            os.replace(path, dst)
        for rel in removed:
            target = os.path.join(cwd, rel)
            if os.path.isfile(target):
                os.remove(target)
        # consumed: a second /undo walks one step further back, not the
        # same step again — the snapshots rolled back are gone
        for stamp in selected:
            shutil.rmtree(snapshot_dir(root, stamp), ignore_errors=True)
    finally:
        shutil.rmtree(staging, ignore_errors=True)
    lines = ["undid %d tool call(s): restored %s" % (len(selected),
                                                     ", ".join(restored) or "nothing")]
    if removed:
        lines.append("removed: %s" % ", ".join(removed))
    return "\n".join(lines)


def handle(msg):
    kind = msg.get("type")
    if kind == "initialize":
        return {"events": ["turn_start", "tool_call", "session_before_tree"],
                "commands": ["undo"]}
    if kind == "call_tool":
        return None  # no tools mounted; never happens
    if kind != "event" and kind != "run_command":
        return None
    name = msg.get("name")
    params = msg.get("params", {})
    cwd = os.getcwd()
    if kind == "run_command":
        if name != "undo":
            return None
        try:
            count = max(1, int((msg.get("args") or "").strip() or 1))
        except ValueError:
            return "usage: /undo [N] — how many tool calls to roll back (default 1)"
        return undo(cwd, count)
    if name == "turn_start":
        # turn_start carries only the round number, so the snapshot is keyed
        # on the wall clock; ordering is what matters, and the stamp sorts
        # the way turn ids do
        take_snapshot(cwd, stamp_now())
    elif name == "tool_call":
        # before the tool runs: snapshot what it names. Replying null leaves
        # the decision to the approval matrix — this is a snapshot, not a gate
        pre_tool_snapshot(cwd, targets_of(params.get("tool"),
                                         params.get("args"), cwd))
    elif name == "session_before_tree":
        # the turns leaving the active branch: restore the workspace to
        # where the oldest of them began. `dropped_turns` is the payload's
        # own count (the ids only identify the turns, and the snapshots are
        # keyed on time, not ids)
        restore(cwd, params.get("dropped_turns",
                                len(params.get("dropped_ids", []))))
    return None


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    if msg.get("type") == "shutdown":
        break
    try:
        print(json.dumps({"id": msg.get("id"), "result": handle(msg)}), flush=True)
    except Exception as e:
        print(json.dumps({"id": msg.get("id"), "error": str(e)}), flush=True)
