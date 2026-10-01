#!/usr/bin/env python3
# Workspace checkpoints for /tree — no git required.
#
# the reference's answer to "the agent edited files, then I jumped back in the
# conversation — what about my code?" is: the core rewinds only the
# transcript; an extension snapshots the workspace and restores it. This is
# that extension, portable to projects with no VCS at all.
#
#   turn_start           — mirror the working tree into a shadow dir (one per
#                          turn, hardlinks where possible so unchanged files
#                          cost nothing), after pruning checkpoints older
#                          than KEEP_DAYS
#   session_before_tree  — /tree picked a cut point: merge every snapshot
#                          from the dropped turns, newest first, back into
#                          the working tree
#
# The merge is additive by design: files the dropped turns created or edited
# come back as they were, files untouched since the cut point stay as they
# are now. Deletions are not resurrected by later turns' snapshots shadowing
# them — the newest snapshot that still holds the file wins. That heuristic
# is right far more often than it is wrong, and /tree's own warning
# ("everything after it is dropped") is the contract this extends to disk.
#
# Install into ~/.llm/extensions/ (or .llm/extensions/), chmod +x, /reload.
# The shadow root lives in the system temp dir; override with
# LLM_CHECKPOINT_DIR. Turn it off for a session by removing the file.
import json
import os
import shutil
import sys
import tempfile
import time

# don't mirror these anywhere they appear: VCS internals, dependency trees,
# the shadow dir itself if it sits under the project
SKIP_DIRS = {".git", ".hg", ".svn", "node_modules", "__pycache__",
             ".venv", "venv", "target", "dist", "build", ".llm"}
# files over this many bytes are referenced, not copied (symlink)
BIG_FILE = 1 << 20
KEEP_DAYS = 7


def shadow_root():
    root = os.environ.get("LLM_CHECKPOINT_DIR") or os.path.join(
        tempfile.gettempdir(), "llm-workspace-checkpoints")
    os.makedirs(root, exist_ok=True)
    return root


def project_key(cwd):
    # /home/me/work/foo -> home-me-work-foo
    return os.path.abspath(cwd).strip(os.sep).replace(os.sep, "-")


def project_root(cwd):
    root = os.path.join(shadow_root(), project_key(cwd))
    os.makedirs(root, exist_ok=True)
    return root


def turn_stamp(ts):
    # turn ids are ulids, and ulids sort by time — the stamp keeps that order
    return ts


def snapshot_dir(root, turn_id):
    return os.path.join(root, turn_stamp(turn_id))


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


def take_snapshot(cwd, turn_id):
    root = project_root(cwd)
    prune(root)
    mirror(cwd, snapshot_dir(root, turn_id))


def restore(cwd, dropped_ids):
    """Fold the dropped turns' snapshots back, newest first."""
    root = project_root(cwd)
    merged = set()
    for turn_id in sorted(dropped_ids, reverse=True):
        snap = snapshot_dir(root, turn_id)
        if not os.path.isdir(snap):
            continue
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
        # turn_start carries only the round number, so key the snapshot on
        # the wall clock; ulid ordering would be nicer but the id is not
        # known until the turn is persisted
        take_snapshot(cwd, time.strftime("%Y%m%dT%H%M%S"))
    elif name == "session_before_tree":
        restore(cwd, params.get("dropped_ids", []))
    return None


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    if msg.get("type") == "shutdown":
        break
    print(json.dumps({"id": msg.get("id"), "result": handle(msg)}), flush=True)
