#!/usr/bin/env python3
"""subagent — delegate a task to a fresh yak in its own context window.

Copy to `~/.yak/extensions/subagent` (chmod +x) and `/reload`. The tool it
mounts is the reference's subagent idea in this repo's shape: the child is a real `yak`
process with its own window, its own tool subset and its own system prompt,
so a long reconnaissance burns the *child's* context and you receive the
conclusion instead of the transcript.

Agent definitions are markdown with frontmatter, one file per agent:

    ~/.yak/agents/scout.md          user-level
    <project>/.yak/agents/scout.md  project-level, walks up from cwd and
                                    wins by name (nearest last)

    ---
    name: scout
    description: fast reconnaissance; returns compressed findings
    tools: read, grep, glob, ls, webfetch
    model: openai/gpt-5-mini    # optional, else the child resolves like any yak
    thinking: low               # off|minimal|low|medium|high|xhigh, optional
    ---
    You are a scout. Report file:line facts, not prose. Never edit anything.

Four sample definitions ship in `examples/agents/` — scout and reviewer
(read-only), planner (think-only), worker (edits and runs commands) — and
two built-ins of the same names cover the case where no file exists at all.

How a child runs
----------------
    yak --json --no-session --tools <definition> \
        [--model M] [--thinking L] --append-system-prompt <body> "Task: ..."

`--json` is the line-delimited event stream this extension parses: tool calls
become progress lines, the closing `result` object is the answer. Because the
child is a plain yak, everything the CLI can do (models, thinking levels,
tool subsets) is available to a definition, and nothing here can outlive a
`/reload`.

*Progress.* The child's stderr is streamed, and so is ours: every line this
extension writes to stderr while a call is in flight reaches the parent's
live tool log (`docs/extensions.md`). None of it enters either model's
context — the tool result is exactly the string we return.
*Budget.* A subagent takes minutes, so the initialize reply asks the host
for a matching `tool_timeout`; each child is also killed on its own deadline
below rather than leaving a runaway process behind.
*Depth.* A child cannot spawn a child: we pass YAK_SUBAGENT_DEPTH down and
refuse when it is at the limit. The child's `--tools` whitelist excludes this
extension anyway, so the guard only has to catch a hand-run child.
*Abort.* ctrl+c in the parent abandons the call; the host tells us with an
`interrupt` message, which a reader thread picks up while the main thread is
busy — every running child is killed on the spot (this is why stdin is read
by a thread rather than by the protocol loop).
*Trust.* The child runs without approvals: there is nobody at its terminal
to answer a prompt. The parent call is the approval point — this is an
ordinary exec-tier extension tool, and the definition's `tools:` line is
what the child may then touch. Read-only agents (scout, reviewer) therefore
stay read-only.
"""

import json
import os
import queue
import shutil
import subprocess
import sys
import threading
import time

# A Windows console (and a CI runner with a legacy codepage) defaults to
# cp437/cp1252, where the progress glyphs below raise UnicodeEncodeError —
# inside a worker thread that costs the child's whole answer. Pin both streams
# to UTF-8; the host reads them as UTF-8 either way.
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, OSError, ValueError):
        pass

# Children allowed to run at once for a `tasks` batch.
MAX_PARALLEL = 4
# Nesting limit: a child may not delegate further.
MAX_DEPTH = 1
# Wall-clock deadline per child (the host's own cap is asked for below).
CHILD_TIMEOUT = int(os.environ.get("YAK_SUBAGENT_TIMEOUT") or 1500)
# The answer we hand back is a conclusion, not a transcript.
MAX_RESULT = 20000

BUILTIN_AGENTS = {
    "scout": {
        "description": "fast reconnaissance: returns compressed file:line facts",
        "tools": "read,grep,glob,ls,webfetch",
        "prompt": (
            "You are a scout. Survey what was asked and report only what you "
            "found, as file:line facts and short quotes rather than prose. Say "
            "plainly what you did not find. Never edit anything."
        ),
    },
    "worker": {
        "description": "does the work: edits files, runs commands, verifies",
        "tools": "read,write,edit,bash,grep,glob,ls",
        "prompt": (
            "You are a worker. Do the task, then verify it with the narrowest "
            "command that proves it. Report what changed and what you ran. "
            "Leave nothing half-applied."
        ),
    },
}

TOOL = {
    "name": "subagent",
    "description": (
        "Delegate to a subagent: a fresh yak with its own context window, its "
        "own tool subset and its own system prompt. Use it for reconnaissance "
        "that would flood this context, or to run one task several ways at "
        "once. Modes (pass exactly one): `task` plus optional `agent` for a "
        "single subagent; `tasks` for a parallel batch; `chain` for a "
        "pipeline where each step receives the previous answer. A subagent "
        "cannot ask questions, cannot spawn another subagent, and its tool "
        "access comes from its agent definition (read-only unless the "
        "definition says otherwise). You get its final answer, not its "
        "transcript."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "task": {"type": "string", "description": "the instruction for one subagent"},
            "agent": {
                "type": "string",
                "description": "agent definition name (scout, worker, or a *.md in ~/.yak/agents)",
            },
            "tasks": {
                "type": "array",
                "description": "parallel batch",
                "items": {
                    "type": "object",
                    "properties": {
                        "agent": {"type": "string"},
                        "task": {"type": "string"},
                    },
                },
            },
            "chain": {
                "type": "array",
                "description": "sequential pipeline; each step sees the previous answer",
                "items": {
                    "type": "object",
                    "properties": {
                        "agent": {"type": "string"},
                        "task": {"type": "string"},
                    },
                },
            },
        },
    },
}

# --------------------------------------------------------------- progress --
# One lock: the stdout parser and the stderr pump both write progress, and a
# half-written line would reach the parent as garbage.
_note_lock = threading.Lock()

# Live children, so an interrupt from the host can stop them: the protocol
# loop is blocked inside the call when it arrives, and only a reader thread
# is awake to hear it.
_children = set()
_children_lock = threading.Lock()
_abandoned = threading.Event()


def note(line):
    line = line.rstrip("\n")
    with _note_lock:
        try:
            sys.stderr.write(line + "\n")
        except UnicodeError:
            # progress is best-effort: never let a glyph take down a run
            sys.stderr.write(line.encode("ascii", "replace").decode("ascii") + "\n")
        sys.stderr.flush()


def stop_children():
    _abandoned.set()
    with _children_lock:
        running = list(_children)
    if running:
        note("[subagent] interrupted: stopping %d child agent(s)" % len(running))
    for child in running:
        try:
            child.kill()
        except OSError:
            pass


def reply(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


# ---------------------------------------------------------------- agents --
def user_dir():
    return os.environ.get("YAK_USER_PATH") or os.path.join(os.path.expanduser("~"), ".yak")


def project_agent_dirs(start):
    """`.yak/agents` from the filesystem root down to cwd: the nearest one is
    last, and a later definition replaces an earlier one by name."""
    dirs, path = [], os.path.abspath(start)
    while True:
        dirs.append(os.path.join(path, ".yak", "agents"))
        parent = os.path.dirname(path)
        if parent == path:
            return list(reversed(dirs))
        path = parent


def parse_definition(path):
    """Frontmatter + body; anything unreadable is skipped, never fatal."""
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            text = f.read()
    except OSError:
        return None
    fields, body = {}, text
    if text.startswith("---"):
        end = text.find("\n---", 3)
        if end != -1:
            fields = {}
            for line in text[3:end].splitlines():
                key, _, value = line.partition(":")
                if value.strip():
                    fields[key.strip().lower()] = value.strip()
            body = text[end + 4:]
    name = fields.get("name") or os.path.splitext(os.path.basename(path))[0]
    return {
        "name": name,
        "description": fields.get("description", ""),
        "tools": fields.get("tools", ""),
        "model": fields.get("model", ""),
        "thinking": fields.get("thinking", ""),
        "prompt": body.strip(),
        "source": path,
    }


def load_agents():
    """Built-ins first, then user definitions, then project ones: discovery
    from the outside in, each layer overriding the one before it."""
    agents = {}
    for name, spec in BUILTIN_AGENTS.items():
        agents[name] = dict(spec, name=name, model="", thinking="", source="built-in")
    homes = [os.path.join(user_dir(), "agents")]
    homes += project_agent_dirs(os.getcwd())
    for home in homes:
        if not os.path.isdir(home):
            continue
        for entry in sorted(os.listdir(home)):
            if not entry.endswith(".md"):
                continue
            spec = parse_definition(os.path.join(home, entry))
            if spec and spec["prompt"]:
                agents[spec["name"]] = spec
    return agents


def child_tools(agent):
    """The definition's whitelist, minus this extension: a child that cannot
    name the tool cannot call it, whatever the depth guard says."""
    names = [t.strip() for t in agent.get("tools", "").split(",")]
    return [t for t in names if t and t not in ("subagent", "task")]


def yak_binary():
    return os.environ.get("YAK_BIN") or shutil.which("yak") or shutil.which("yak.exe")


# --------------------------------------------------------------- one child --
def run_child(binary, agent, prompt, label, depth):
    """Run one child to completion. Returns (ok, text)."""
    tools = child_tools(agent)
    cmd = [binary, "--json", "--no-session"]
    if tools:
        cmd += ["--tools", ",".join(tools)]
    if agent.get("model"):
        cmd += ["-m", agent["model"]]
    if agent.get("thinking"):
        cmd += ["--thinking", agent["thinking"]]
    if agent.get("prompt"):
        cmd += ["--append-system-prompt", agent["prompt"]]
    cmd.append(prompt)
    env = dict(os.environ, YAK_SUBAGENT_DEPTH=str(depth + 1))
    note("[%s] %s · tools: %s" % (label, agent["name"], ",".join(tools) or "none"))
    try:
        child = subprocess.Popen(
            cmd,
            # stdin is explicitly empty: the child must never read the pipe
            # this extension talks to the host over (a non-tty stdin makes
            # yak append it to the prompt, and it would block forever)
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
        )
    except OSError as e:
        return False, "cannot start the subagent (%s); set YAK_BIN to the yak binary" % e

    tail = []

    def pump_stderr():
        for line in child.stderr:
            line = line.rstrip("\n")
            if len(tail) == 8:
                tail.pop(0)
            tail.append(line)
            note("[%s] %s" % (label, line))

    pump = threading.Thread(target=pump_stderr, daemon=True)
    pump.start()
    with _children_lock:
        _children.add(child)
        abandoned = _abandoned.is_set()
    if abandoned:  # the interrupt landed between spawn and registration
        child.kill()

    started = time.time()
    final, error, calls, usage = None, None, 0, None
    try:
        for line in child.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
            except ValueError:
                # not our protocol (a stray banner?): stderr's diagnostics
                # tail already has the reason, skip it rather than dying
                continue
            kind = event.get("type")
            if kind == "tool_start":
                calls += 1
                note("[%s] %s: %s" % (label, event.get("name"), event.get("preview", "")[:120]))
            elif kind == "tool_end" and event.get("is_error"):
                note("[%s] ✗ %s" % (label, event.get("summary", "")[:120]))
            elif kind == "tool_results_pruned":
                note("[%s] pruned %s tool results from its context" % (label, event.get("count")))
            elif kind == "compacted":
                note("[%s] compacted its context (%s messages)" % (label, event.get("removed")))
            elif kind == "stream_recovered":
                note("[%s] stream dropped, recovered %s chars" % (label, event.get("chars")))
            elif kind == "result":
                final = event.get("text") or ""
                usage = event.get("usage") or {}
            elif kind == "error":
                error = event.get("message") or "the subagent failed"
        try:
            status = child.wait(timeout=max(1, CHILD_TIMEOUT - int(time.time() - started)))
        except subprocess.TimeoutExpired:
            child.kill()
            status = -9
            error = "timed out after %ds" % CHILD_TIMEOUT
        pump.join(timeout=1)
    finally:
        with _children_lock:
            _children.discard(child)

    secs = time.time() - started
    if _abandoned.is_set():
        return False, "[%s] interrupted" % label
    if status != 0 or error:
        detail = error or ("the subagent exited with status %d" % status)
        if tail:
            detail += "\n" + "\n".join(tail)
        return False, "[%s] %s" % (label, detail)
    spent = ""
    if usage:
        spent = " · %s in / %s out" % (thousands(usage.get("input")), thousands(usage.get("output")))
    note("[%s] done: %d tool calls · %.0fs" % (label, calls, secs))
    header = "[%s] %d tool calls · %.0fs%s\n" % (label, calls, secs, spent)
    return True, header + (final if final is not None else "(no answer)")


def thousands(value):
    if not isinstance(value, (int, float)):
        return "?"
    return "%.1fk" % (value / 1000.0) if value >= 1000 else "%d" % value


def clamp(text):
    if len(text) <= MAX_RESULT:
        return text
    return text[:MAX_RESULT] + "\n…[%d more chars truncated]" % (len(text) - MAX_RESULT)


# ------------------------------------------------------------------ modes --
def steps_of(raw, default_agent):
    """Normalize a batch entry list into [{agent, task}]; malformed entries
    become errors the model can fix rather than a crash."""
    steps = []
    for i, item in enumerate(raw):
        if not isinstance(item, dict) or not (item.get("task") or "").strip():
            return None, "entry %d needs a `task` string" % (i + 1)
        steps.append(
            {"agent": (item.get("agent") or default_agent).strip(), "task": item["task"].strip()}
        )
    return steps, None


def unknown(agents, name):
    known = ", ".join(sorted(agents))
    return "error: no agent named '%s'; available: %s" % (name, known)


def handle(args):
    _abandoned.clear()
    depth = int(os.environ.get("YAK_SUBAGENT_DEPTH") or 0)
    if depth >= MAX_DEPTH:
        return (
            "error: nested subagents are disabled (YAK_SUBAGENT_DEPTH=%d) — "
            "do this work here instead." % depth
        )
    binary = yak_binary()
    if not binary:
        return (
            "error: cannot find the yak binary; set YAK_BIN to its path "
            "(e.g. YAK_BIN=/path/to/target/release/yak)"
        )
    agents = load_agents()
    default_agent = (args.get("agent") or "worker").strip()

    chain, batch = args.get("chain") or [], args.get("tasks") or []
    if chain and batch:
        return "error: pass either `tasks` (parallel) or `chain` (sequential), not both"
    if chain or batch:
        raw, mode = (chain, "chain") if chain else (batch, "parallel")
        steps, problem = steps_of(raw, default_agent)
        if problem:
            return "error: %s" % problem
    elif (args.get("task") or "").strip():
        steps, mode = [{"agent": default_agent, "task": args["task"].strip()}], "single"
    else:
        return (
            "error: pass `task` (+ optional `agent`), or a `tasks` batch, or a "
            "`chain`; run the `agents` command to list definitions"
        )
    for step in steps:
        if step["agent"] not in agents:
            return unknown(agents, step["agent"])

    if mode != "chain":
        return run_batch(binary, agents, steps, mode, depth)
    return run_chain(binary, agents, steps, depth)


def run_batch(binary, agents, steps, mode, depth):
    results = [None] * len(steps)
    gate = threading.Semaphore(MAX_PARALLEL)

    def one(index, step):
        label = step["agent"] if mode == "single" else "%d/%d %s" % (index + 1, len(steps), step["agent"])
        with gate:
            try:
                results[index] = run_child(
                    binary, agents[step["agent"]], "Task: " + step["task"], label, depth
                )
            except Exception as e:  # a crashed thread must not read as "never ran"
                results[index] = (False, "[%s] the subagent runner crashed: %r" % (label, e))

    threads = [threading.Thread(target=one, args=(i, s)) for i, s in enumerate(steps)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    blocks, failed = [], []
    for step, result in zip(steps, results):
        ok, text = result or (False, "the subagent never ran")
        blocks.append(text)
        if not ok:
            failed.append(step["agent"])
    out = clamp("\n\n".join(blocks))
    if failed:
        return "error: %s failed\n\n%s" % (", ".join(failed), out)
    return out


def run_chain(binary, agents, steps, depth):
    blocks, prior = [], None
    for index, step in enumerate(steps):
        if _abandoned.is_set():
            break
        label = "%d/%d %s" % (index + 1, len(steps), step["agent"])
        prompt = "Task: " + step["task"]
        if prior is not None:
            prompt += "\n\nWork from the previous subagent's result:\n\n" + prior
        ok, text = run_child(binary, agents[step["agent"]], prompt, label, depth)
        if not ok:
            return "error: %s\n\n%s" % (text, clamp("\n\n".join(blocks)))
        blocks.append(text)
        prior = text
    return clamp("\n\n".join(blocks[-1:]))


# --------------------------------------------------------------- protocol --
def agents_report():
    agents = load_agents()
    lines = []
    for name in sorted(agents):
        spec = agents[name]
        where = spec["source"] if spec["source"] == "built-in" else os.path.relpath(spec["source"])
        tools = spec.get("tools") or "all"
        lines.append(
            "%s (%s)\n    tools: %s\n    %s" % (name, where, tools, spec.get("description", ""))
        )
    return "agent definitions:\n\n" + "\n".join(lines)


def read_stdin(requests):
    """Everything arrives on stdin, including the interrupt that says the
    parent walked away from the call we are in the middle of."""
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except ValueError:
            continue
        if request.get("type") in ("interrupt", "shutdown"):
            stop_children()
        if request.get("type") == "interrupt":
            continue  # nothing is waiting for a reply
        requests.put(request)
    requests.put(None)


def main():
    requests = queue.Queue()
    threading.Thread(target=read_stdin, args=(requests,), daemon=True).start()
    while True:
        request = requests.get()
        if request is None:
            break
        kind = request.get("type")
        if kind == "initialize":
            reply(
                {
                    "id": request["id"],
                    "result": {
                        "tools": [TOOL],
                        "commands": ["agents"],
                        "events": [],
                        # a subagent runs for minutes; the host otherwise
                        # applies extensions.tool_timeout (120s by default)
                        "tool_timeout": CHILD_TIMEOUT + 60,
                    },
                }
            )
        elif kind == "call_tool":
            try:
                out = handle(request.get("args") or {})
            except Exception as e:  # a tool error is a result, never a crash
                out = "error: subagent extension failed: %s" % e
            reply({"id": request["id"], "result": out})
        elif kind == "run_command":
            try:
                out = agents_report() if request.get("name") == "agents" else "unknown command"
            except Exception as e:
                out = "error: %s" % e
            reply({"id": request["id"], "result": out})
        elif kind == "shutdown":
            break


if __name__ == "__main__":
    main()
