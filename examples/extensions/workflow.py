#!/usr/bin/env python3
"""workflow — resumable graph orchestration as a resident extension.

Copy to `~/.yak/extensions/workflow` (chmod +x) and `/reload`. A workflow is
a JSON graph of nodes (agent / tool / router / parallel / human / subgraph)
connected by edges; each agent node names its own model, thinking level and
tool subset, and runs as a real `yak --json` child — so model resolution,
approvals, retries, usage and the blacklist stay the core's, never a
re-implementation. The graph shape, state mapping, routing and persistence
live here:

    ~/.yak/workflows/<run_id>.jsonl     one append-only event per line

State is declarative: the graph declares its fields and types, a node reads
them through `$.path` inputs, writes only fields it declared in `writes`,
and the output reducer merges each patch (`replace` default, `append`,
`merge_object` for parallel writers). Routers evaluate a comparison-only
expression grammar (`$.review.ok == true and $.attempts < 3`) — never a
script. Every cycle needs a router with `max_iterations`; a parallel node
fans its branches out on threads and merges them in node order.

Models are chosen by role: the graph's `models` table maps a role to
`{"model": "@alias", "thinking": "high"}`; a node names the role, and
resolution is node.model > role > defaults.model > the core's default. The
resolved set is frozen into the run's first event — a resume replays it and
never re-resolves, so editing an alias never mutates a half-run workflow.

Commands: `/workflow list`, `/workflow graphs`, `/workflow run <name>
[input json]`, `/workflow inspect <id>`, `/workflow resume <id>
[retry|skip]`, `/workflow answer <id> <value>`, `/workflow cancel <id>`.
The `workflow_run` tool exposes run to the model. Live progress rides lane
frames, one row per node, into the session's tool log — a node's transcript
never enters the parent model's context.

Durability follows the thread store's disciplines: mid-file corruption
fails the run loudly, a torn final line is the one bounded repair, a future
format version is refused with the upgrade path named. A node interrupted
mid-flight resumes as `interrupted` and only an explicit `retry` or `skip`
clears it — an indeterminate node is never silently re-run.

The persistence layout, the event vocabulary and the routing grammar are
frozen by `examples/workflows/README.md`: graphs, event logs and resume
semantics keep working across reloads and replacement of this file.
"""

import json
import os
import queue
import re
import shutil
import subprocess
import sys
import threading
import time

EVENT_FORMAT_VERSION = 1
NODE_KINDS = ("agent", "tool", "router", "parallel", "human", "subgraph")
STATE_TYPES = ("string", "number", "boolean", "object", "array")
REDUCERS = ("replace", "append", "merge_object")
MAX_ITERATIONS_CAP = 100
TOTAL_STEPS_CAP = 500
STEP_TIMEOUT = 1800
TOOL_TIMEOUT = STEP_TIMEOUT + 60


class WorkflowError(Exception):
    pass


class _Interrupted(Exception):
    pass


# stdout is the protocol channel: replies and lane frames must never
# interleave mid-line, so every writer holds one reentrant lock.
_OUT_LOCK = threading.RLock()
# live node children, so an interrupt from the host can stop them: the
# protocol loop is blocked inside the call when it arrives, and only reader
# threads are awake to hear it
_CHILDREN = set()
_CHILDREN_LOCK = threading.Lock()
_ABANDONED = threading.Event()


def reply(obj):
    with _OUT_LOCK:
        sys.stdout.write(json.dumps(obj) + "\n")
        sys.stdout.flush()


def send_lane(lane, text):
    text = text.rstrip("\n")[:240]
    with _OUT_LOCK:
        try:
            sys.stdout.write(json.dumps({"type": "lane", "lane": lane, "text": text}) + "\n")
            sys.stdout.flush()
        except (OSError, ValueError):
            pass  # progress is best-effort, never worth a crash


def note(line):
    """stderr is the live progress channel: while a call is in flight every
    line lands in the session's tool log, dim, and none of it reaches the
    model."""
    line = line.rstrip("\n")
    try:
        sys.stderr.write(line + "\n")
        sys.stderr.flush()
    except UnicodeError:
        try:
            sys.stderr.write(line.encode("ascii", "replace").decode("ascii") + "\n")
            sys.stderr.flush()
        except OSError:
            pass


# paths and identity


def user_dir():
    return os.environ.get("YAK_USER_PATH") or os.path.join(os.path.expanduser("~"), ".yak")


def graph_homes():
    """Project `.yak/workflows` walking up from cwd (nearest last), then the
    user-global home: same layering as agents and extensions, so a project
    shadows a user graph by name."""
    homes = []
    path = os.path.abspath(os.getcwd())
    homes.append(os.path.join(path, ".yak", "workflows"))
    while True:
        parent = os.path.dirname(path)
        if parent == path:
            break
        path = parent
        homes.append(os.path.join(path, ".yak", "workflows"))
    homes.append(os.path.join(user_dir(), "workflows"))
    return homes


def runs_dir():
    return os.path.join(user_dir(), "workflows")


def run_path(run_id):
    return os.path.join(runs_dir(), run_id + ".jsonl")


def new_ulid():
    """Run ids share the thread store's ULID shape (Crockford 32,
    time-ordered): a workflows/ listing reads like a threads/ listing."""
    ms = int(time.time() * 1000)
    rand = int.from_bytes(os.urandom(10), "big")
    alphabet = "0123456789abcdefghjkmnpqrstvwxyz"
    out = []
    for shift in range(40, -1, -5):
        out.append(alphabet[(ms >> shift) & 31])
    for shift in range(75, -1, -5):
        out.append(alphabet[(rand >> shift) & 31])
    return "".join(out[:26])


def now_iso():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def thousands(n):
    return "{:,}".format(int(n or 0))


def yak_binary():
    return os.environ.get("YAK_BIN") or shutil.which("yak") or shutil.which("yak.exe")


# state access: $.path reads, the routing-expression grammar


_MISSING = object()


def json_get(state, path):
    if path == "$":
        return state
    if not path.startswith("$."):
        return _MISSING
    cur = state
    for part in path[2:].split("."):
        if not isinstance(cur, dict):
            return _MISSING
        cur = cur.get(part, _MISSING)
        if cur is _MISSING:
            return _MISSING
    return cur


_TOKEN_RE = re.compile(
    r"""\s*(?:
        (?P<op>==|!=|<=|>=|<|>)
      | (?P<lp>\()
      | (?P<rp>\))
      | (?P<kw>and|or|not)(?![\w$.])
      | (?P<path>\$[A-Za-z0-9_.]*)
      | (?P<num>-?\d+(?:\.\d+)?)
      | (?P<str>"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')
      | (?P<lbrack>\[)
      | (?P<rbrack>\])
      | (?P<bool>true|false)(?![\w$.])
      | (?P<null>null)(?![\w$.])
    )""",
    re.VERBOSE,
)


def _tokenize(src):
    toks, pos = [], 0
    while pos < len(src):
        if src[pos].isspace():
            pos += 1
            continue
        m = _TOKEN_RE.match(src, pos)
        if not m:
            raise WorkflowError("invalid routing expression: %s (at %r)" % (src, src[pos:pos + 12]))
        pos = m.end()
        for kind in ("op", "lp", "rp", "kw", "path", "num", "str", "lbrack", "rbrack", "bool", "null"):
            if m.group(kind) is not None:
                toks.append((kind, m.group(kind)))
                break
    return toks


def _unescape(text):
    quote = text[0]
    body = text[1:-1]
    return body.replace("\\" + quote, quote).replace("\\\\", "\\")


class _ExprParser:
    """or > and > not > comparison > (expr) / literal / $.path. The grammar
    is frozen by examples/workflows/README.md."""

    def __init__(self, toks, state):
        self.toks = toks
        self.pos = 0
        self.state = state

    def peek(self):
        return self.toks[self.pos] if self.pos < len(self.toks) else (None, None)

    def next(self):
        tok = self.peek()
        self.pos += 1
        return tok

    def parse(self):
        v = self.or_expr()
        if self.pos != len(self.toks):
            raise WorkflowError("unexpected token in routing expression: %r" % (self.peek()[1],))
        return v

    def or_expr(self):
        v = self.and_expr()
        while self.peek() == ("kw", "or"):
            self.next()
            rhs = self.and_expr()
            v = bool(v) or bool(rhs)
        return v

    def and_expr(self):
        v = self.cmp_expr()
        while self.peek() == ("kw", "and"):
            self.next()
            rhs = self.cmp_expr()
            v = bool(v) and bool(rhs)
        return v

    def cmp_expr(self):
        if self.peek() == ("kw", "not"):
            self.next()
            return not bool(self.cmp_expr())
        v = self.atom()
        kind, text = self.peek()
        if kind == "op" and text in ("==", "!=", "<", "<=", ">", ">="):
            self.next()
            return _compare(text, v, self.atom())
        return v

    def atom(self):
        kind, text = self.next()
        if (kind, text) == ("lp", "("):
            v = self.or_expr()
            k2, t2 = self.next()
            if (k2, t2) != ("rp", ")"):
                raise WorkflowError("missing ) in routing expression")
            return v
        if kind == "kw" and text == "not":
            return not bool(self.atom())
        if kind == "lbrack":
            k2, t2 = self.next()
            if (k2, t2) != ("rbrack", "]"):
                raise WorkflowError("only the empty list literal [] is supported")
            return []
        if kind == "path":
            v = json_get(self.state, text)
            if v is _MISSING:
                raise WorkflowError("state path %s does not exist" % text)
            return v
        if kind == "num":
            return float(text) if "." in text else int(text)
        if kind == "str":
            return _unescape(text)
        if kind == "bool":
            return text == "true"
        if kind == "null":
            return None
        raise WorkflowError("unexpected token in routing expression: %r" % (text,))


def _is_number(v):
    return isinstance(v, (int, float)) and not isinstance(v, bool)


def _compare(op, a, b):
    if op == "==":
        return a == b
    if op == "!=":
        return a != b
    if not (_is_number(a) and _is_number(b)):
        raise WorkflowError("ordering %s needs numbers (%r %s %r)" % (op, a, op, b))
    return {"<": a < b, "<=": a <= b, ">": a > b, ">=": a >= b}[op]


def eval_route(src, state):
    return bool(_ExprParser(_tokenize(src), state).parse())


# graph loading and validation


def load_graphs():
    """Every discoverable graph keyed by name. A malformed file surfaces with
    its path; same-name graphs collapse outermost-in, so the nearest project
    dir wins."""
    graphs = {}
    for home in graph_homes():
        if not os.path.isdir(home):
            continue
        for entry in sorted(os.listdir(home)):
            if not entry.endswith(".json"):
                continue
            path = os.path.join(home, entry)
            try:
                with open(path, encoding="utf-8") as f:
                    graph = json.load(f)
            except (OSError, ValueError) as e:
                raise WorkflowError("cannot load workflow graph %s: %s" % (path, e))
            name = graph.get("name") if isinstance(graph, dict) else None
            if not isinstance(name, str) or not name:
                raise WorkflowError("%s: the graph needs a string name" % path)
            graphs[name] = graph
    return graphs


def state_decl(state):
    """name -> {type, reducer}: a bare type string or an object with `type`
    and an optional `reducer`."""
    out = {}
    for name, decl in (state or {}).items():
        if isinstance(decl, str):
            out[name] = {"type": decl, "reducer": "replace"}
        elif isinstance(decl, dict):
            out[name] = {"type": decl.get("type", "string"), "reducer": decl.get("reducer", "replace")}
        else:
            raise WorkflowError("state field %s: declare a type string or an object" % name)
    return out


def check_type(where, decl, value):
    t = decl["type"]
    checks = {
        "string": lambda v: isinstance(v, str),
        "number": _is_number,
        "boolean": lambda v: isinstance(v, bool),
        "object": lambda v: isinstance(v, dict),
        "array": lambda v: isinstance(v, list),
    }
    if t not in checks:
        raise WorkflowError("%s: unknown state type %s (one of %s)" % (where, t, ", ".join(STATE_TYPES)))
    if not checks[t](value):
        raise WorkflowError("%s must be a %s, got %s" % (where, t, type(value).__name__))


def apply_patch(state, decl, patch, origin):
    """Merge one node patch under the declared reducers. An undeclared field
    or a type mismatch fails loudly — never silently dropped."""
    for key in sorted(patch):
        value = patch[key]
        d = decl.get(key)
        if d is None:
            raise WorkflowError("%s writes %s, which the graph state does not declare" % (origin, key))
        where = "%s.%s" % (origin, key)
        check_type(where, d, value)
        if d["reducer"] == "append":
            cur = state.get(key) or []
            state[key] = cur + (value if isinstance(value, list) else [value])
        elif d["reducer"] == "merge_object":
            if not isinstance(state.get(key) or {}, dict) or not isinstance(value, dict):
                raise WorkflowError("%s: merge_object needs object values" % where)
            state.setdefault(key, {})
            state[key].update(value)
        else:
            state[key] = value


def successors(graph, node_id):
    """The full successor set of a node: declared edges plus router route
    targets — what cycle detection walks."""
    node = graph["nodes"][node_id]
    out = set()
    for e in graph.get("edges") or []:
        if e["from"] == node_id:
            out.add(e["to"])
    if node.get("kind") == "router":
        for r in node.get("routes") or []:
            out.add(r["to"])
        if node.get("default"):
            out.add(node["default"])
    if node.get("kind") == "parallel":
        for b in node.get("branches") or []:
            out.add(b["node"])
    return out


def cyclic_nodes(graph):
    """Nodes on any cycle, via iterative DFS with a path stack."""
    on_cycle = set()
    for start in graph["nodes"]:
        stack, seen_on_path = [(start, iter(sorted(successors(graph, start))))], {start}
        while stack:
            node, it = stack[-1]
            advanced = False
            for nxt in it:
                if nxt == start:
                    on_cycle.update(seen_on_path)
                elif nxt not in seen_on_path and nxt in graph["nodes"]:
                    seen_on_path.add(nxt)
                    stack.append((nxt, iter(sorted(successors(graph, nxt)))))
                    advanced = True
                    break
            if not advanced:
                stack.pop()
                seen_on_path.discard(node)
    return on_cycle


def validate_graph(graph, graphs=None):
    """The full pre-run check list; every failure names the graph and node.
    Returns the parsed state declaration."""
    name = graph.get("name")
    if not isinstance(name, str) or not name:
        raise WorkflowError("the graph needs a string name")
    version = graph.get("version", 1)
    if not isinstance(version, int) or version < 1:
        raise WorkflowError("%s: version must be a positive integer" % name)
    nodes = graph.get("nodes") or {}
    if not isinstance(nodes, dict) or not nodes:
        raise WorkflowError("%s: needs a non-empty nodes object" % name)
    entry = graph.get("entry")
    if entry not in nodes:
        raise WorkflowError("%s: entry node %r not in nodes" % (name, entry))

    decl = state_decl(graph.get("state"))
    for key, d in decl.items():
        if d["type"] not in STATE_TYPES:
            raise WorkflowError("%s: state.%s has unknown type %s" % (name, key, d["type"]))
        if d["reducer"] not in REDUCERS:
            raise WorkflowError("%s: state.%s has unknown reducer %s" % (name, key, d["reducer"]))

    for e in graph.get("edges") or []:
        if not isinstance(e, dict) or "from" not in e or "to" not in e:
            raise WorkflowError("%s: each edge needs from and to" % name)
        if e["from"] not in nodes or e["to"] not in nodes:
            raise WorkflowError("%s: edge %r -> %r names an unknown node" % (name, e.get("from"), e.get("to")))
        if nodes[e["from"]].get("kind") == "router":
            raise WorkflowError(
                "%s: edge %s -> %s races the router's routes; routing belongs in routes/default"
                % (name, e["from"], e["to"])
            )

    roles = graph.get("models") or {}
    for role, spec in roles.items():
        if not isinstance(spec, dict) or not isinstance(spec.get("model"), str):
            raise WorkflowError("%s: models.%s needs a model string" % (name, role))

    for nid, node in nodes.items():
        where = "%s.%s" % (name, nid)
        if not isinstance(node, dict):
            raise WorkflowError("%s: the node must be an object" % where)
        kind = node.get("kind")
        if kind not in NODE_KINDS:
            raise WorkflowError("%s: unknown kind %r (one of %s)" % (where, kind, ", ".join(NODE_KINDS)))
        for w in node.get("writes") or []:
            if w not in decl:
                raise WorkflowError("%s writes %s, which the state does not declare" % (where, w))
        for key, p in (node.get("input") or {}).items():
            if not isinstance(p, str) or not p.startswith("$."):
                raise WorkflowError("%s input.%s must be a $.path" % (where, key))
        it = node.get("max_iterations")
        if it is not None and not (isinstance(it, int) and 1 <= it <= MAX_ITERATIONS_CAP):
            raise WorkflowError("%s: max_iterations must be 1..%d" % (where, MAX_ITERATIONS_CAP))
        if kind == "router":
            routes = node.get("routes") or []
            if not routes:
                raise WorkflowError("%s: a router needs routes" % where)
            for r in routes:
                if not isinstance(r, dict) or "when" not in r or r.get("to") not in nodes:
                    raise WorkflowError("%s: each route needs when and a known to" % where)
            if node.get("default") and node["default"] not in nodes:
                raise WorkflowError("%s: default route %r is not a node" % (where, node["default"]))
        elif kind == "tool":
            if not isinstance(node.get("tool"), str) or not node["tool"]:
                raise WorkflowError("%s: a tool node needs a tool name" % where)
        elif kind == "agent":
            tools = node.get("tools")
            if tools is not None:
                ok = isinstance(tools, str) and tools.strip() or isinstance(tools, list) and tools
                if not ok:
                    raise WorkflowError('%s: tools must be a non-empty list or string (omit for the default set)' % where)
            if node.get("model") and node["model"] not in roles and "/" not in node["model"] and not node["model"].startswith("@"):
                raise WorkflowError("%s: model %r is neither a role in models, provider/model nor @alias" % (where, node["model"]))
        elif kind == "parallel":
            branches = node.get("branches") or []
            if not isinstance(branches, list) or not branches:
                raise WorkflowError("%s: a parallel node needs a non-empty branches list" % where)
            for b in branches:
                if not isinstance(b, dict) or b.get("node") not in nodes:
                    raise WorkflowError("%s: each branch names a known node" % where)
                tgt = nodes[b["node"]]
                if tgt.get("kind") in ("parallel",):
                    raise WorkflowError("%s: nested parallel is not supported" % where)
                for w in tgt.get("writes") or []:
                    d = decl.get(w) or {}
                    if d.get("reducer") not in ("append", "merge_object"):
                        raise WorkflowError(
                            "%s: branch %s writes %s without a reducer (append/merge_object required for parallel writers)"
                            % (where, b["node"], w)
                        )
        elif kind == "human":
            if len(node.get("writes") or []) > 1:
                raise WorkflowError("%s: a human node writes at most one field (the answer)" % where)
        elif kind == "subgraph":
            graphs = graphs if graphs is not None else load_graphs()
            sub = graphs.get(node.get("graph"))
            if sub is None:
                raise WorkflowError("%s: subgraph %r not found" % (where, node.get("graph")))
            for key, p in (node.get("input") or {}).items():
                if not isinstance(p, str) or not p.startswith("$."):
                    raise WorkflowError("%s input.%s must be a $.path" % (where, key))
            for field in (node.get("output") or {}):
                if field not in decl:
                    raise WorkflowError("%s outputs %s, which the state does not declare" % (where, field))

    # every cycle must pass a bounded router: no implicit infinite loops
    for nid in cyclic_nodes(graph):
        node = nodes[nid]
        if node.get("kind") == "router" and node.get("max_iterations") is None:
            raise WorkflowError("%s.%s routes a cycle without max_iterations" % (name, nid))
    if cyclic_nodes(graph) and not any(
        nodes[n].get("kind") == "router" for n in cyclic_nodes(graph)
    ):
        raise WorkflowError("%s: a cycle carries no router with max_iterations" % name)
    return decl


# the event log: append-only, the thread store's corruption disciplines


def append_event(run_id, event):
    event = dict(event)
    event.setdefault("v", EVENT_FORMAT_VERSION)
    event.setdefault("ts", now_iso())
    os.makedirs(runs_dir(), exist_ok=True)
    with open(run_path(run_id), "a", encoding="utf-8") as f:
        f.write(json.dumps(event, ensure_ascii=False) + "\n")
        f.flush()
        os.fsync(f.fileno())


class Replay:
    """One run's event log replayed into the last stable state. Corrupt
    mid-file lines fail the run; a torn final line is dropped with a warning;
    a future format version is refused with the upgrade path named."""

    def __init__(self, run_id):
        self.run_id = run_id
        self.graph = None
        self.graph_version = 1
        self.input = {}
        self.models = {}
        self.state = {}
        self.seq = 0
        self.status = "created"
        self.pending = None      # a node_start with no node_end: indeterminate
        self.cursor = None       # the next node to run (last node_end's next)
        self.waiting = None      # the human node the run stopped on
        self.answer = None       # a replayed graph_resume answer
        self.steps = 0
        self.iterations = {}
        self.usage = {"input": 0, "output": 0, "cached": 0, "cached_write": 0}
        self.subgraphs = {}   # node -> child run id (a subgraph parked on a human node)
        self._decl = None
        self._graphs = None

    def load(self):
        try:
            with open(run_path(self.run_id), encoding="utf-8") as f:
                lines = f.read().splitlines()
        except FileNotFoundError:
            raise WorkflowError("run %s not found" % self.run_id)
        events = []
        for i, line in enumerate(lines):
            if not line.strip():
                continue
            try:
                events.append(json.loads(line))
            except ValueError:
                if i == len(lines) - 1:
                    note("run %s: torn final line dropped (a crash mid-append)" % self.run_id)
                else:
                    raise WorkflowError("run %s: corrupt line %d; refusing to guess" % (self.run_id, i + 1))
        if not events:
            raise WorkflowError("run %s: no events survived" % self.run_id)
        if events[0].get("v", 1) > EVENT_FORMAT_VERSION:
            raise WorkflowError(
                "run %s was written by a newer format (v%s > v%d): upgrade the workflow extension to resume it"
                % (self.run_id, events[0].get("v"), EVENT_FORMAT_VERSION)
            )
        for ev in events:
            self.apply(ev)
        return self

    def decl(self):
        if self._decl is None:
            graph = self.graphs().get(self.graph)
            if graph is None:
                raise WorkflowError("graph %s referenced by run %s is gone" % (self.graph, self.run_id))
            self._decl = state_decl(graph.get("state"))
        return self._decl

    def graphs(self):
        if self._graphs is None:
            self._graphs = load_graphs()
        return self._graphs

    def apply(self, ev):
        self.seq = max(self.seq, int(ev.get("seq") or 0))
        t = ev.get("type")
        if t == "graph_start":
            self.graph = ev.get("graph")
            self.graph_version = ev.get("graph_version", 1)
            self.input = ev.get("input") or {}
            self.models = ev.get("models") or {}
            self.state = dict(self.input)
            self.status = "running"
            self.cursor = ev.get("entry")
        elif t == "node_start":
            self.pending = ev.get("node")
            self.iterations[ev.get("node")] = self.iterations.get(ev.get("node"), 0) + 1
            self.steps += 1
        elif t == "node_end":
            node = ev.get("node")
            patch = ev.get("state_patch") or {}
            if patch:
                apply_patch(self.state, self.decl(), patch, node)
            u = ev.get("usage") or {}
            for k in self.usage:
                self.usage[k] += int(u.get(k) or 0)
            if self.pending == node:
                self.pending = None
            self.cursor = ev.get("next")
        elif t == "node_error":
            if self.pending == ev.get("node"):
                self.pending = None
            self.status = ev.get("run_status", "failed")
        elif t == "subgraph_start":
            self.subgraphs[ev.get("node")] = ev.get("sub_run")
        elif t == "human_interrupt":
            self.status = "waiting"
            self.waiting = ev.get("node")
            if self.pending == ev.get("node"):
                self.pending = None
        elif t == "graph_wait":
            self.status = "waiting"
        elif t == "graph_resume":
            self.status = "running"
            if ev.get("answer") is not None:
                self.answer = ev.get("answer")
        elif t == "graph_cancel":
            self.status = "cancelled"
            self.pending = None
        elif t == "graph_end":
            self.status = ev.get("status", "completed")
            self.pending = None
            self.waiting = None

    def next_seq(self):
        self.seq += 1
        return self.seq

    def summary_line(self):
        return "%s  %-12s %-18s %s" % (self.run_id, self.status, self.graph or "?",
                                       self.waiting or (self.pending or self.cursor or "-"))


# model snapshots


def resolve_models(graph):
    """Resolve every agent node's model once, into the run's frozen
    snapshot. Aliases (`@fast`) resolve through config.json's aliases at
    start time only; an unresolvable alias fails the run before it starts."""
    aliases = {}
    cfg = os.path.join(user_dir(), "config.json")
    if os.path.exists(cfg):
        try:
            with open(cfg, encoding="utf-8") as f:
                aliases = json.load(f).get("aliases") or {}
        except (OSError, ValueError):
            aliases = {}
    roles = graph.get("models") or {}
    base = graph.get("defaults") or {}
    snapshot = {}
    for nid, node in (graph.get("nodes") or {}).items():
        if node.get("kind") != "agent":
            continue
        ref = node.get("model")
        spec = None
        if ref and ref in roles:
            spec = roles[ref]
        elif ref:
            spec = {"model": ref}
        elif base.get("model"):
            spec = {"model": base["model"]}
        if not spec:
            continue  # the child resolves the core default itself
        spec = dict(spec)
        mid = spec.get("model") or ""
        if mid.startswith("@"):
            alias = mid[1:]
            if alias not in aliases:
                raise WorkflowError(
                    "alias @%s does not resolve (config.json aliases); the run refuses to start" % alias
                )
            spec["model"] = aliases[alias]
        snapshot[nid] = spec
    return snapshot


# node execution


def build_node_prompt(node, state):
    parts = []
    if node.get("prompt"):
        parts.append(node["prompt"])
    inputs, missing = {}, []
    for key, p in (node.get("input") or {}).items():
        v = json_get(state, p)
        if v is _MISSING:
            missing.append("%s (%s)" % (key, p))
        else:
            inputs[key] = v
    if missing:
        raise WorkflowError("node input path(s) missing in state: %s" % ", ".join(missing))
    if inputs:
        parts.append("Input (JSON):\n" + json.dumps(inputs, ensure_ascii=False, indent=2))
    return "\n\n".join(parts)


def route_target(node, state):
    for r in node.get("routes") or []:
        if eval_route(r["when"], state):
            return r["to"]
    if node.get("default"):
        return node["default"]
    raise WorkflowError("no route matched and the router declares no default")


def parse_output_patch(text, node, where):
    """The node's last fenced ```json block (or a whole-body JSON object) is
    its state_patch. A patch naming an undeclared field is an error; no
    patch with declared writes writes nothing (noted, not fatal)."""
    writes = set(node.get("writes") or [])
    body = (text or "").strip()
    patch = None
    blocks = re.findall(r"```(?:json)?\s*(\{.*?\})\s*```", body, re.DOTALL)
    if blocks:
        try:
            patch = json.loads(blocks[-1])
        except ValueError:
            patch = None
    else:
        try:
            maybe = json.loads(body)
            if isinstance(maybe, dict):
                patch = maybe
        except ValueError:
            patch = None
    if patch is None:
        if writes:
            note("%s produced no JSON patch; writing nothing (declared: %s)" % (where, ", ".join(sorted(writes))))
        return {}
    for key in patch:
        if key not in writes:
            raise WorkflowError(
                "%s patched %r, which is not in its declared writes %s" % (where, key, sorted(writes))
            )
    return patch


def run_child(run, node_id, node, state):
    """One `yak --json --no-session` child for an agent or tool step. The
    child owns model resolution, approvals, retries and usage; the closing
    `result` text becomes the state patch. Returns (text, usage)."""
    where = node_id
    model = run.models.get(node_id) or {}
    cmd = [yak_binary(), "--json", "--no-session"]
    if model.get("model"):
        cmd += ["-m", model["model"]]
    if model.get("thinking"):
        cmd += ["--thinking", model["thinking"]]
    tools = node.get("tools")
    if isinstance(tools, list):
        cmd += ["--tools", ",".join(tools)]
    elif isinstance(tools, str):
        cmd += ["--tools", tools]
    if node.get("append_prompt"):
        cmd += ["--append-system-prompt", node["append_prompt"]]
    if node.get("approval") == "allow":
        # explicit opt-in in the graph (the graph's author is the user who
        # installed it): the child has no terminal to answer an ask at, so
        # without this a writer node fails closed on its first approval
        cmd += ["--approval", "allow"]
    cmd.append(build_node_prompt(node, state))

    usage = {"input": 0, "output": 0, "cached": 0, "cached_write": 0}
    tail = []
    send_lane(node_id, "%s · %s" % (node_id, model.get("model") or "default model"))
    try:
        child = subprocess.Popen(
            cmd,
            stdin=subprocess.DEVNULL,  # never the host pipe: a non-tty stdin would join the prompt
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            cwd=os.getcwd(),
        )
    except OSError as e:
        raise WorkflowError("cannot start the node child (%s); is yak on PATH or YAK_BIN set?" % e)
    with _CHILDREN_LOCK:
        _CHILDREN.add(child)
        abandoned = _ABANDONED.is_set()
    if abandoned:
        child.kill()

    def pump_stderr():
        for line in child.stderr:
            line = line.rstrip("\n")
            if line.strip():
                tail.append(line)
                send_lane(node_id, line[:200])

    threading.Thread(target=pump_stderr, daemon=True).start()

    def deadline_kill():
        time.sleep(STEP_TIMEOUT)
        with _CHILDREN_LOCK:
            gone = child not in _CHILDREN
        if not gone and child.poll() is None:
            note("%s timed out after %ds: killing the child" % (node_id, STEP_TIMEOUT))
            try:
                child.kill()
            except OSError:
                pass

    watchdog = threading.Timer(STEP_TIMEOUT, deadline_kill)
    watchdog.daemon = True
    watchdog.start()

    error = None
    final_text = ""
    try:
        for line in child.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except ValueError:
                continue
            kind = ev.get("type")
            if kind == "tool_start":
                send_lane(node_id, "%s: %s" % (ev.get("name"), (ev.get("preview") or "")[:110]))
            elif kind == "tool_end" and ev.get("is_error"):
                send_lane(node_id, "✗ %s" % (ev.get("summary") or "")[:110])
            elif kind == "turn_end" and ev.get("usage"):
                for k in usage:
                    usage[k] += int(ev["usage"].get(k) or 0)
            elif kind == "result":
                final_text = ev.get("text") or ""
            elif kind == "error":
                error = ev.get("message") or "the node child failed"
        try:
            child.wait(timeout=30)
        except subprocess.TimeoutExpired:
            child.kill()
    finally:
        watchdog.cancel()
        with _CHILDREN_LOCK:
            _CHILDREN.discard(child)
    if _ABANDONED.is_set():
        raise _Interrupted()
    if error is not None:
        raise WorkflowError(error)
    if child.returncode not in (0, None):
        detail = "the child exited with status %s" % child.returncode
        if tail:
            detail += "\n" + "\n".join(tail[-6:])
        raise WorkflowError(detail)
    return final_text, usage


def run_agent_step(run, node_id, node, state):
    text, usage = run_child(run, node_id, node, state)
    return parse_output_patch(text, node, node_id), usage


def run_tool_step(run, node_id, node, state):
    """A tool node rides the same child: a minimal agent with only the named
    tool mounted, instructed to call it with the mapped arguments verbatim.
    Deterministic in its mapping; the approval point stays the core's."""
    args = {}
    for key, p in (node.get("args") or {}).items():
        if isinstance(p, str) and p.startswith("$."):
            v = json_get(state, p)
            if v is _MISSING:
                raise WorkflowError("tool arg %s path %s missing in state" % (key, p))
            args[key] = v
        else:
            args[key] = p
    agent_node = {
        "kind": "agent",
        "prompt": "Call the %s tool with exactly these arguments and reply with its raw output only, no commentary:\n%s"
        % (node["tool"], json.dumps(args, ensure_ascii=False)),
        "tools": [node["tool"]],
        "append_prompt": node.get("append_prompt"),
        "approval": node.get("approval"),
    }
    text, usage = run_child(run, node_id, agent_node, state)
    writes = node.get("writes") or []
    if not writes:
        return {}, usage
    return {writes[0]: text}, usage


# the scheduler


PARK = "__park__"  # a subgraph waiting on a human node: not a terminal next


class Engine:
    """Walks the graph from the replayed position. Every decision lands in
    the event log before it happens, so any crash replays to the exact
    boundary: a node with node_start but no node_end is indeterminate and
    resumes as `interrupted`."""

    def __init__(self, run, graph, graphs, parent_event=None):
        self.replay = run
        self.graph = graph
        self.graphs = graphs
        self.parent_event = parent_event
        self.steps = 0

    def emit(self, event_type, **fields):
        ev = dict(fields)
        ev["type"] = event_type
        ev["run_id"] = self.replay.run_id
        ev["seq"] = self.replay.next_seq()
        append_event(self.replay.run_id, ev)

    def edge_target(self, node_id):
        # a router's own edges are a second routing table racing its
        # routes; the routes win and an edge from a router is refused at
        # validation, so here only non-router edges count
        if self.graph["nodes"][node_id].get("kind") == "router":
            return None
        for e in self.graph.get("edges") or []:
            if e["from"] == node_id:
                return e["to"]
        return None

    def check_iteration(self, node_id):
        limit = self.graph["nodes"][node_id].get("max_iterations") or MAX_ITERATIONS_CAP
        n = self.replay.iterations.get(node_id, 0) + 1
        if n > limit:
            raise WorkflowError("node %s exceeded max_iterations=%d" % (node_id, limit))

    def start_position(self):
        if self.replay.waiting:
            return self.replay.waiting, "resume"
        if self.replay.pending:
            return self.replay.pending, "resume"
        return self.replay.cursor or self.graph["entry"], "start"

    def bridge_gaps(self, node_id):
        """After a skip, drive routing-only nodes (routers whose inputs were
        never written cannot evaluate; a skip means the graph author chose
        to jump past the wreckage) straight to the next executable node."""
        while node_id in self.graph["nodes"] \
                and self.graph["nodes"][node_id].get("kind") == "router":
            node = self.graph["nodes"][node_id]
            self.emit("node_start", node=node_id)
            try:
                node_id = route_target(node, self.replay.state)
            except WorkflowError:
                node_id = node.get("default") \
                    or (node.get("routes") or [{}])[0].get("to")
            self.emit("node_end", node=node_id, status="completed", state_patch={}, next=node_id)
        return node_id

    def run(self, skip_indeterminate=False):
        node_id, phase = self.start_position()
        if phase == "resume" and self.replay.pending:
            # resume_run already gated and recorded the explicit choice
            # (retry re-runs; skip routes past without executing)
            if skip_indeterminate:
                nxt = self.successor_of(node_id)
                self.emit("node_end", node=node_id, status="skipped", state_patch={}, next=nxt)
                node_id = nxt
                if node_id is None:
                    self.emit("graph_end", status="completed")
                    return {"status": "completed", "state": self.replay.state}
                node_id = self.bridge_gaps(node_id)
            # retry: fall through and execute the pending node again
        while True:
            if _ABANDONED.is_set():
                raise _Interrupted()
            self.steps += 1
            if self.steps > TOTAL_STEPS_CAP:
                raise WorkflowError("the run exceeded %d steps; refusing to loop forever" % TOTAL_STEPS_CAP)
            nodes = self.graph["nodes"]
            if node_id not in nodes:
                raise WorkflowError("node %r is not in the graph" % node_id)
            node = nodes[node_id]
            kind = node.get("kind")

            # a resumed human node (or a parked subgraph) consumes the
            # replayed or fresh answer
            if phase == "resume" and kind in ("human", "subgraph"):
                if kind == "subgraph":
                    nxt, patch, usage = self.execute_subgraph(node_id, node, answer=self.replay.answer)
                    if nxt is PARK:
                        return {"status": "waiting", "node": node_id}
                    if patch:
                        apply_patch(self.replay.state, state_decl(self.graph.get("state")), patch, node_id)
                else:
                    patch = self.human_patch(node, node_id)
                    self.replay.state.update(patch)
                    nxt = self.successor_of(node_id)
                    self.emit("node_end", node=node_id, status="completed", state_patch=patch, next=nxt)
                if nxt is None:
                    self.emit("graph_end", status="completed")
                    return {"status": "completed", "state": self.replay.state}
                node_id, phase = nxt, "start"
                continue

            if kind == "human":
                # first arrival: park the run on the answer. No node_end is
                # written until the answer arrives, so a crash here replays
                # to the same waiting point.
                self.emit("node_start", node=node_id)
                self.emit("human_interrupt", node=node_id, prompt=node.get("prompt", ""))
                self.emit("graph_wait", node=node_id, reason="human")
                return {"status": "waiting", "node": node_id}

            if kind == "router":
                self.check_iteration(node_id)
            self.emit("node_start", node=node_id)
            try:
                nxt, patch, usage = self.execute(node_id, node, kind)
            except _Interrupted:
                self.emit("node_error", node=node_id, error="interrupted", run_status="interrupted")
                self.emit("graph_end", status="interrupted", error="interrupted")
                return {"status": "interrupted", "error": "interrupted", "node": node_id}
            except WorkflowError as e:
                self.emit("node_error", node=node_id, error=str(e))
                self.emit("graph_end", status="failed", error=str(e))
                return {"status": "failed", "error": str(e), "node": node_id}
            except Exception as e:  # never leave a run looking alive
                self.emit("node_error", node=node_id, error="internal error: %s" % e)
                self.emit("graph_end", status="failed", error=str(e))
                return {"status": "failed", "error": str(e), "node": node_id}

            if nxt is PARK:
                return {"status": "waiting", "node": node_id}
            if patch:
                apply_patch(self.replay.state, state_decl(self.graph.get("state")), patch, node_id)
            self.emit("node_end", node=node_id, status="completed", state_patch=patch,
                      usage=usage or {}, next=nxt)
            if nxt is None:
                self.emit("graph_end", status="completed")
                return {"status": "completed", "state": self.replay.state}
            node_id, phase = nxt, "start"

    def successor_of(self, node_id):
        node = self.graph["nodes"][node_id]
        if node.get("kind") == "router":
            return route_target(node, self.replay.state)
        if node.get("kind") == "parallel":
            join = next((b.get("join") for b in node.get("branches") or [] if b.get("join")), None)
            return join or self.edge_target(node_id)
        return self.edge_target(node_id)

    def human_patch(self, node, node_id):
        answer = self.replay.answer
        if answer is None:
            raise WorkflowError("node %s is waiting; answer it first (/workflow answer)" % node_id)
        writes = node.get("writes") or []
        if not writes:
            return {}
        decl = state_decl(self.graph.get("state")).get(writes[0]) or {"type": "string"}
        choices = [str(c).lower() for c in (node.get("choices") or [])]
        if choices and str(answer).lower() not in choices:
            raise WorkflowError("answer %r is not one of the declared choices %s" % (answer, node.get("choices")))
        t = decl["type"]
        if t == "boolean":
            value = str(answer).strip().lower() in ("true", "yes", "y", "approve", "approved", "allow", "ok")
        elif t == "number":
            try:
                value = float(answer) if "." in str(answer) else int(answer)
            except ValueError:
                raise WorkflowError("answer %r is not a number" % answer)
        else:
            value = str(answer)
        return {writes[0]: value}

    def execute(self, node_id, node, kind):
        """One non-human node; returns (next, patch, usage)."""
        if kind == "router":
            return route_target(node, self.replay.state), {}, None
        if kind == "parallel":
            return self.execute_parallel(node_id, node)
        if kind == "subgraph":
            return self.execute_subgraph(node_id, node)
        if kind == "tool":
            patch, usage = run_tool_step(self.replay, node_id, node, self.replay.state)
            return self.edge_target(node_id), patch, usage
        # agent
        patch, usage = run_agent_step(self.replay, node_id, node, self.replay.state)
        return self.edge_target(node_id), patch, usage

    def execute_parallel(self, node_id, node):
        branches = [b["node"] for b in node.get("branches") or []]
        for b in branches:
            self.emit("node_start", node=b)
            self.replay.iterations[b] = self.replay.iterations.get(b, 0) + 1
        results, errors = {}, {}

        def work(bname):
            try:
                bnode = self.graph["nodes"][bname]
                if bnode.get("kind") == "tool":
                    results[bname] = run_tool_step(self.replay, bname, bnode, self.replay.state)
                else:
                    results[bname] = run_agent_step(self.replay, bname, bnode, self.replay.state)
            except WorkflowError as e:
                errors[bname] = str(e)
            except _Interrupted:
                errors[bname] = "interrupted"

        threads = [threading.Thread(target=work, args=(b,), daemon=True) for b in branches]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        if _ABANDONED.is_set():
            raise _Interrupted()
        if errors:
            for b in branches:  # a failed fan still closes its started nodes
                if b in errors:
                    self.emit("node_error", node=b, error=errors[b])
                elif b in results:
                    self.emit("node_end", node=b, status="completed", state_patch=results[b][0],
                              usage=results[b][1], next=None)
            raise WorkflowError("parallel branch failure(s): " + "; ".join(
                "%s: %s" % (b, errors[b]) for b in branches if b in errors))
        # merge in stable node order, each branch's patch under its reducers
        for b in branches:
            patch, usage = results[b]
            if patch:
                apply_patch(self.replay.state, state_decl(self.graph.get("state")), patch, b)
            self.emit("node_end", node=b, status="completed", state_patch=patch, usage=usage, next=None)
        return self.successor_of(node_id), {}, None

    def execute_subgraph(self, node_id, node, answer=None):
        sub = self.graphs.get(node.get("graph"))
        if sub is None:
            raise WorkflowError("subgraph %r not found" % node.get("graph"))
        validate_graph(sub, self.graphs)
        sub_rid = self.replay.subgraphs.get(node_id)
        if sub_rid is None:
            sub_state = {}
            for key, p in (node.get("input") or {}).items():
                v = json_get(self.replay.state, p)
                if v is _MISSING:
                    raise WorkflowError("subgraph input %s path %s missing in state" % (key, p))
                sub_state[key] = v
            sub_rid = start_run(sub, sub_state, graphs=self.graphs)["run_id"]
            self.emit("subgraph_start", node=node_id, sub_run=sub_rid)
        sub_run = Replay(sub_rid).load()
        if answer is not None:
            append_event(sub_rid, {"type": "graph_resume", "seq": sub_run.next_seq(), "answer": answer})
            sub_run = Replay(sub_rid).load()
        outcome = Engine(sub_run, sub, self.graphs, parent_event=self.parent_event).run()
        if outcome.get("status") == "waiting":
            # a human node inside the subgraph parks the parent too: the
            # answer rides the next resume of this run
            self.replay.subgraphs[node_id] = sub_rid
            self.emit("human_interrupt", node=node_id, sub_run=sub_rid)
            self.emit("graph_wait", node=node_id, reason="human")
            return PARK, {}, None
        if outcome.get("status") != "completed":
            raise WorkflowError("subgraph %s finished as %s" % (node.get("graph"), outcome.get("status")))
        sub_final = Replay(sub_rid).load()
        patch = {}
        for field, p in (node.get("output") or {}).items():
            v = json_get(sub_final.state, p)
            if v is _MISSING:
                raise WorkflowError("subgraph output %s missing in the sub-state" % p)
            patch[field] = v
        return self.edge_target(node_id), patch, None


# run lifecycle


def start_run(graph, input_data, graphs=None):
    graphs = graphs if graphs is not None else load_graphs()
    decl = validate_graph(graph, graphs)
    run_id = new_ulid()
    models = resolve_models(graph)
    state = {}
    for key, value in (input_data or {}).items():
        if key not in decl:
            raise WorkflowError("input field %r is not declared in the graph state" % key)
        check_type("input.%s" % key, decl[key], value)
        state[key] = value
    append_event(run_id, {
        "type": "graph_start", "seq": 1, "graph": graph["name"],
        "graph_version": graph.get("version", 1), "entry": graph["entry"],
        "models": models, "input": input_data or {},
    })
    return {"run_id": run_id, "status": "running"}


def run_to_completion(graph, graphs, input_data):
    started = start_run(graph, input_data, graphs=graphs)
    run_id = started["run_id"]
    run = Replay(run_id).load()
    outcome = Engine(run, graph, graphs).run()
    outcome["run_id"] = run_id
    if outcome.get("status") == "waiting":
        outcome["hint"] = "answer with /workflow answer %s <value>" % run_id
    return outcome


def require_resumable(run, run_id, choice, answer=None):
    if run.status == "waiting" and run.waiting:
        if run.answer is None and answer is None:
            raise WorkflowError(
                "run %s waits on node %s; answer it first (/workflow answer %s <value>)"
                % (run_id, run.waiting, run_id)
            )
        return None
    if run.pending:
        if choice not in ("retry", "skip"):
            raise WorkflowError(
                "run %s stopped mid-node %s (indeterminate); resume with retry or skip"
                % (run_id, run.pending)
            )
        return choice
    if run.status in ("failed", "interrupted"):
        if choice not in ("retry", "skip"):
            raise WorkflowError("run %s is %s; resume with retry or skip" % (run_id, run.status))
        return choice
    if run.status in ("completed", "cancelled"):
        raise WorkflowError("run %s is %s; nothing to resume" % (run_id, run.status))
    return None


def resume_run(run_id, choice=None, answer=None):
    graphs = load_graphs()
    run = Replay(run_id).load()
    graph = graphs.get(run.graph)
    if graph is None:
        raise WorkflowError("graph %s for run %s not found" % (run.graph, run_id))
    # an explicit answer rides the resume; otherwise a waiting run needs
    # one already recorded by /workflow answer, and a crashed run needs an
    # explicit retry/skip choice — nothing resumes implicitly
    if answer is None and run.status == "waiting" and run.answer is None and choice is not None \
            and choice not in ("retry", "skip"):
        answer = choice
        choice = None
    mode = require_resumable(run, run_id, choice, answer)
    if answer is not None:
        append_event(run_id, {"type": "graph_resume", "seq": run.next_seq(), "answer": answer})
        run = Replay(run_id).load()
    elif mode:
        append_event(run_id, {"type": "graph_resume", "seq": run.next_seq(), "choice": mode})
        run = Replay(run_id).load()
    outcome = Engine(run, graph, graphs).run(skip_indeterminate=(mode == "skip"))
    outcome["run_id"] = run_id
    return outcome


def cancel_run(run_id):
    run = Replay(run_id).load()
    append_event(run_id, {"type": "graph_cancel", "seq": run.next_seq()})
    return {"status": "cancelled", "run_id": run_id}


# inspection


def graph_report(graphs):
    if not graphs:
        return "no workflow graphs (drop .json files into .yak/workflows/ or %s)" % os.path.join(user_dir(), "workflows")
    rows = ["graphs:"]
    for name in sorted(graphs):
        g = graphs[name]
        rows.append("  %s v%s · %d node(s) · entry %s" %
                    (name, g.get("version", 1), len(g.get("nodes") or {}), g.get("entry")))
    return "\n".join(rows)


def run_report(run_id):
    run = Replay(run_id).load()
    graph = run.graphs().get(run.graph) or {}
    u = run.usage
    rows = [
        "workflow: %s v%s" % (run.graph, graph.get("version", run.graph_version)),
        "run: %s" % run.run_id,
        "status: %s" % run.status,
    ]
    if run.pending:
        rows.append("stopped mid-node: %s (indeterminate; resume retry|skip)" % run.pending)
    if run.waiting:
        rows.append("waiting on: %s" % run.waiting)
    elif run.cursor:
        rows.append("next node: %s" % run.cursor)
    rows += [
        "steps: %d" % run.steps,
        "input tokens: %s · output %s · cached %s" % (thousands(u["input"]), thousands(u["output"]), thousands(u["cached"])),
        "state: " + json.dumps(run.state, ensure_ascii=False),
    ]
    return "\n".join(rows)


def list_runs():
    d = runs_dir()
    if not os.path.isdir(d):
        return []
    out = []
    for entry in sorted(os.listdir(d)):
        if not entry.endswith(".jsonl"):
            continue
        try:
            out.append(Replay(entry[:-6]).load())
        except WorkflowError:
            continue  # a broken foreign file never hides the runnable runs
    return out


# the tool and the commands


TOOL_RUN = {
    "name": "workflow_run",
    "description": "Start a workflow graph by name and wait for it to finish. "
                   "Returns the final state, or the waiting point when a human node stops it.",
    "parameters": {
        "type": "object",
        "properties": {
            "name": {"type": "string", "description": "graph name; /workflow graphs lists them"},
            "input": {"type": "object", "description": "initial state fields"},
        },
        "required": ["name"],
    },
}


def handle_tool(args):
    name = args.get("name")
    graphs = load_graphs()
    graph = graphs.get(name)
    if graph is None:
        return "graph %r not found; the graphs command lists them" % name
    outcome = run_to_completion(graph, graphs, args.get("input") or {})
    return json.dumps(outcome, ensure_ascii=False, indent=2)


def handle_command(argstr):
    parts = argstr.split()
    sub = parts[0] if parts else "list"
    rest = parts[1:]
    graphs = load_graphs()
    if sub == "list":
        rows = list_runs()
        if not rows:
            return "no runs yet (start one: /workflow run <name> [input json])"
        out = ["%-28s %-12s %-18s %s" % ("run", "status", "graph", "node")]
        out += [r.summary_line() for r in rows]
        return "\n".join(out)
    if sub == "graphs":
        return graph_report(graphs)
    if sub == "run":
        if not rest:
            return "usage: /workflow run <name> [input json]"
        graph = graphs.get(rest[0])
        if graph is None:
            return "graph %r not found; /workflow graphs lists them" % rest[0]
        input_data = {}
        if len(rest) > 1:
            try:
                input_data = json.loads(" ".join(rest[1:]))
            except ValueError as e:
                return "input is not valid JSON: %s" % e
        outcome = run_to_completion(graph, graphs, input_data)
        run_id = outcome["run_id"]
        if outcome.get("status") == "completed":
            return "run %s completed\n%s" % (run_id, json.dumps(outcome.get("state") or {}, ensure_ascii=False, indent=2))
        if outcome.get("status") == "waiting":
            return "run %s waits on node %s\n%s" % (run_id, outcome.get("node"), outcome.get("hint", ""))
        return "run %s %s: %s" % (run_id, outcome.get("status"), outcome.get("error"))
    if sub == "inspect":
        if not rest:
            return "usage: /workflow inspect <run-id>"
        return run_report(rest[0])
    if sub == "resume":
        if not rest:
            return "usage: /workflow resume <run-id> [retry|skip|answer]"
        outcome = resume_run(rest[0], " ".join(rest[1:]) or None)
        if outcome.get("status") == "completed":
            return "run %s completed\n%s" % (outcome["run_id"], json.dumps(outcome.get("state") or {}, ensure_ascii=False, indent=2))
        return json.dumps(outcome, ensure_ascii=False, indent=2)
    if sub == "cancel":
        if not rest:
            return "usage: /workflow cancel <run-id>"
        cancel_run(rest[0])
        return "run %s cancelled" % rest[0]
    if sub == "answer":
        if len(rest) < 2:
            return "usage: /workflow answer <run-id> <value>"
        outcome = resume_run(rest[0], answer=" ".join(rest[1:]))
        if outcome.get("status") == "completed":
            return "run %s completed\n%s" % (outcome["run_id"], json.dumps(outcome.get("state") or {}, ensure_ascii=False, indent=2))
        return json.dumps(outcome, ensure_ascii=False, indent=2)
    return "unknown workflow subcommand %r (list, graphs, run, inspect, resume, cancel, answer)" % sub


# protocol loop


def read_stdin(q):
    try:
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                req = json.loads(line)
            except ValueError:
                continue
            q.put(req)
            if req.get("type") == "shutdown":
                return
    finally:
        q.put(None)


def main():
    requests = queue.Queue()
    threading.Thread(target=read_stdin, args=(requests,), daemon=True).start()
    while True:
        req = requests.get()
        if req is None:
            break
        kind = req.get("type")
        if kind == "initialize":
            reply({
                "id": req["id"],
                "result": {
                    "tools": [TOOL_RUN],
                    "commands": ["workflow"],
                    "events": [],
                    # a workflow runs for minutes; the host otherwise applies
                    # extensions.tool_timeout (120s by default)
                    "tool_timeout": TOOL_TIMEOUT,
                },
            })
        elif kind == "call_tool":
            try:
                out = handle_tool(req.get("args") or {})
            except _Interrupted:
                out = "error: interrupted"
            except WorkflowError as e:
                out = "error: %s" % e
            except Exception as e:  # a tool error is a result, never a crash
                out = "error: workflow extension failed: %s" % e
            reply({"id": req["id"], "result": out})
        elif kind == "run_command":
            try:
                out = handle_command(req.get("args") or "")
            except _Interrupted:
                out = "error: interrupted"
            except WorkflowError as e:
                out = "error: %s" % e
            except Exception as e:
                out = "error: %s" % e
            reply({"id": req["id"], "result": out})
        elif kind == "interrupt":
            _ABANDONED.set()
            with _CHILDREN_LOCK:
                running = list(_CHILDREN)
            for child in running:
                try:
                    child.kill()
                except OSError:
                    pass
        elif kind == "shutdown":
            break


if __name__ == "__main__":
    main()
