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
#   session_before_tree  — /tree picked a jump point: merge the snapshots
#                          the dropped turns began with, oldest winning,
#                          back into the working tree
#
# The merge is additive by design: files the dropped turns edited come back
# as they were at the oldest selected snapshot (the moment the first dropped
# turn began), files untouched since the cut point stay as they are now, and
# nothing is ever deleted — files the dropped turns created stay behind. One
# assumption is stated rather than hidden: the snapshots of a project's
# dropped turns are the newest ones on disk (a second yak session in the
# same directory interleaves its own; close it before you jump).
#
# Install into ~/.yak/extensions/ (or .yak/extensions/), chmod +x, /reload.
# The shadow root lives in the system temp dir; override with
# YAK_CHECKPOINT_DIR. Turn it off for a session by removing the file.
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
# files over this many bytes are referenced, not copied (symlink)
BIG_FILE = 1 << 20
KEEP_DAYS = 7

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
    """Copy src's tree into dst. Every file is an independent copy (or a
    symlink for oversized ones): a hardlink would alias the live file, so
    restoring it would "restore" whatever the working tree holds now."""
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
            try:
                st = os.stat(s)
                if st.st_size > BIG_FILE:
                    os.symlink(os.path.abspath(s), d)
                else:
                    shutil.copy2(s, d)  # an independent copy: a hardlink
                    # would alias the live file and break the snapshot
            except OSError:
                try:
                    shutil.copy2(s, d)
                except OSError:
                    pass  # vanished mid-walk: it will not be in the snapshot


def take_snapshot(cwd, stamp):
    root = project_root(cwd)
    prune(root)
    mirror(cwd, snapshot_dir(root, stamp))


def restore(cwd, dropped_count):
    """Fold the last `dropped_count` snapshots back into the working tree,
    oldest winning per file. The dropped turns are the active branch's tail,
    so their begin-of-turn snapshots are the newest ones on disk for the
    project; the oldest of them is the state the first dropped turn found,
    which is the state the jump returns the workspace to."""
    root = project_root(cwd)
    stamps = sorted(s for s in os.listdir(root) if is_stamp(s))
    selected = stamps[-dropped_count:] if dropped_count > 0 else []
    merged = set()
    for stamp in selected:
        snap = snapshot_dir(root, stamp)
        for dirpath, _, filenames in os.walk(snap):
            rel = os.path.relpath(dirpath, snap)
            target = cwd if rel == "." else os.path.join(cwd, rel)
            os.makedirs(target, exist_ok=True)
            for name in filenames:
                src = os.path.join(dirpath, name)
                dst = os.path.join(target, name)
                if dst in merged or os.path.islink(src):
                    continue
                merged.add(dst)
                if os.path.islink(dst):
                    os.remove(dst)
                try:
                    shutil.copy2(src, dst)
                except OSError:
                    pass


def handle(msg):
    kind = msg.get("type")
    if kind == "initialize":
        return {"events": ["turn_start", "session_before_tree"]}
    if kind != "event":
        return None
    name = msg.get("name")
    params = msg.get("params", {})
    cwd = os.getcwd()
    if name == "turn_start":
        # turn_start carries only the round number, so the snapshot is keyed
        # on the wall clock; ordering is what matters, and the stamp sorts
        # the way turn ids do
        take_snapshot(cwd, stamp_now())
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
    print(json.dumps({"id": msg.get("id"), "result": handle(msg)}), flush=True)
