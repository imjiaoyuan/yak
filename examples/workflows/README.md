# workflows

JSON graphs for the `workflow` extension (`examples/extensions/workflow.py`).
Drop them into `.yak/workflows/` (project, nearest up wins) or
`~/.yak/workflows/` and drive them from a session: `/workflow run <name>
{json input}`.

The layout, the event vocabulary and the routing grammar below are the
compatibility surface: a graph, an event log and a resume keep working
across extension reloads and replacements.

## Graph file

```jsonc
{
  "name": "review",              // required, unique per home; project shadows user by name
  "version": 1,
  "entry": "review",             // a node id
  "defaults": {"model": "@smart", "thinking": "medium"},
  "models": {                    // roles a node names by string
    "smart": {"model": "@smart", "thinking": "high"},
    "fast":  {"model": "@fast"}
  },
  "state": {                     // declared fields; a node writes only these
    "task": "string",            // string | number | boolean | object | array
    "notes": {"type": "array", "reducer": "append"}   // replace | append | merge_object
  },
  "nodes": {
    "review": {
      "kind": "agent",           // agent | tool | router | parallel | human | subgraph
      "model": "smart",          // role, provider/model, or @alias; else defaults.model
      "input": {"task": "$.task"},   // state reads; a missing path is an error
      "prompt": "...",
      "tools": ["read", "grep"],     // optional tool subset for this node
      "thinking": "high",
      "append_prompt": "extra system prompt text",
      "approval": "allow",           // optional: answer the child's ask-list prompts
      "writes": ["review"]       // the patch fields this node may write
    }
  },
  "edges": [{"from": "review", "to": "summarize"}]
}
```

- **agent**: runs `yak --json --no-session` with the node's model, thinking
  and tool subset. The reply's last fenced ```json block (or a whole-body
  JSON object) is the state patch; fields outside `writes` are an error.
- **tool**: `{"kind": "tool", "tool": "bash", "args": {"command": "ls"}, "writes": ["out"]}`
  — args values map from state via `$.path` or stay literal; with `writes`
  the tool's raw output becomes that field.
- **router**: `routes: [{"when": "<expr>", "to": "node"}]`, first match
  wins, `default` the fallback; a router on a cycle needs `max_iterations`
  (cap 100).
- **parallel**: `branches: [{"node": "a"}, {"node": "b"}]`; the branches
  run concurrently and merge in node order. Branch writers on one field
  need the `append` or `merge_object` reducer; the next node is the
  branches' `join` or the parallel node's own edge.
- **human**: parks the run (`prompt`, `choices`); continue with
  `/workflow answer <run> <value>` (or `/workflow resume <run> <value>`).
  With `writes`, the answer lands in that field — `boolean` answers map
  approve/yes/true &c.
- **subgraph**: `{"graph": "other", "input": {"k": "$.path"}, "output": {"field": "$.sub"}}`
  — runs another graph as a child run and maps its final state back.

## Routing expressions

`$.path` reads, literals (`"str"`, `123`, `true`, `null`), comparisons
(`==`, `!=`, `<`, `<=`, `>`, `>=` — ordering needs numbers), `and`, `or`,
`not` and parentheses. Nothing else executes.

## Runs and events

`~/.yak/workflows/<run_id>.jsonl`, one event per line, append-only:
`graph_start` (with the frozen model snapshot and initial state),
`node_start` / `node_end` (with `state_patch`, `usage`, `next`),
`node_error`, `human_interrupt`, `graph_wait`, `graph_resume`,
`graph_cancel`, `graph_end`. Mid-file corruption fails the run loudly; a
torn final line is dropped with a warning; a future format version is
refused with the upgrade path named. A node interrupted mid-flight resumes
as `interrupted` and only an explicit `retry` or `skip` clears it.

Models resolve once at start (aliases through `config.json`'s `aliases`)
and are frozen into the run's first event; a resume never re-resolves.

## Commands

```
/workflow graphs
/workflow run <name> [json input]
/workflow list
/workflow inspect <run-id>
/workflow resume <run-id> [retry|skip|<answer>]
/workflow answer <run-id> <value>
/workflow cancel <run-id>
```

`workflow_run` (the tool) starts a run and waits; its tier is exec, so the
session's approval policy gates who may start one.
