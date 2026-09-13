# Extension API

Extensions are the plugin system: anything the core skips, you build yourself as an executable
dropped into `~/.llm/extensions/` or the project's `.llm/extensions/`; drop a file in, restart or
`/reload`. This page is the full reference; runnable examples live in
[`examples/extensions/`](../examples/extensions/) (`wordcount`, `websearch`, `todo`, `repeat_guard.py` —
a `tool_call` deny gate for stuck loops, `fold_repeats.py` — a `tool_result` rewriter that folds
repeated log lines, plus the `template.js`/`template.py` starter templates).
`websearch.ts` is a TypeScript twin of the python `websearch` (node >= 23.6 runs it directly
through native type stripping; bun/deno also work) — install one of the two, not both: the
host dedups extension entries by file stem.

Extensions run with your full user permissions and inherit your environment. Only install or
write code you would run yourself.

## Discovery

| Home | Notes |
|---|---|
| `.llm/extensions/` (nearest walking up from cwd) | project copy, wins by name |
| `<user_dir>/pkg/<pkg>/extensions/` (project-local, then user pkgs) | from `llm install` packages |
| `~/.llm/extensions/` | user-global |

Every file (not directory) in these homes is scanned. Same stem name in two homes: the project
copy wins. A file is either a **script tool** (it carries a manifest header, see below) or a
**resident extension** (anything else, but it must be executable — the exec bit on unix, one of
`.exe`/`.bat`/`.cmd`/`.ps1` on Windows). Config keys:

```json
{
  "extensions": {
    "disabled": ["websearch"],
    "tool_timeout": 120
  }
}
```

`/reload` re-discovers and respawns everything. A broken or slow extension degrades alone: it
warns dimly and mounts nothing; `/status` shows the failure reason. Extension stderr is kept in
a 20-line diagnostics tail — stray stdout that is not valid protocol lands there too.

## Script tools

A comment header on any script makes it a tool. The host spawns it **per call**, feeds the
arguments, collects stdout as the result and owns timeout and size caps. No exec bit needed —
declare an interpreter and the host runs the script through it (this is what makes the form work
on Windows too).

```python
#!/usr/bin/env python3
# --- llm-tool: wordcount
# description: count characters, words and lines of a text
# args: text (string) the text to measure
# arg-mode: argv
# interpreter: python3
# timeout: 10
import sys
text = sys.argv[1] if len(sys.argv) > 1 else ""
print(f"{len(text)} chars · {len(text.split())} words")
```

Manifest fields (lines of `#` or `//` comments after the `--- llm-tool:` marker, until the first
non-comment line):

| Field | Meaning |
|---|---|
| `--- llm-tool: <name>` | the marker + tool name (required) |
| `description: <text>` | shown to the model in the tool list |
| `args: <name> (<type>) <desc>` | one schema property; repeatable. Types: `string`, `int`/`integer`, `number`/`float`, `bool`/`boolean`, `list`/`array`. Every declared arg is required |
| `arg-mode: argv` | single declared argument arrives as plain `argv[1]` instead of stdin JSON |
| `interpreter: <prog>` | run the file through this program (`${ENV}` expanded); without it the file runs itself |
| `timeout: <secs>` | per-call timeout; default `extensions.tool_timeout` (120s) |
| `tier: read\|write\|exec` | trust tier the approval matrix sees; default `exec` |

Invocation contract:

- default mode: the arguments object arrives as **one JSON line on stdin** (`{"text": "..."}`)
- `arg-mode: argv`: the single declared argument arrives as `argv[1]` as plain text — shell
  scripts never parse JSON
- stdout is the tool result (stderr is appended when non-empty; both are capped)
- exit code `0` → normal result, anything else → error result (the exit code is appended)
- timeout → the call is interrupted and returned as an error result
- cwd is the agent's working directory
- script tools are exec-tier unless the manifest declares otherwise: the approval matrix applies (see below)

## Resident extensions

For hooks, commands and multi-call tools: the process is spawned **once** per session and speaks
one JSON message per line over stdio. stdout carries only the protocol — anything else goes to
the diagnostics tail; stderr likewise.

### Lifecycle

1. Host spawns the file (cwd = agent working directory).
2. Host sends `initialize`; the extension must reply within **10s**.
3. Requests flow: `call_tool`, `run_command`, `event` — each answered by id.
4. On exit or `/reload`: host sends `shutdown`, closes stdin, then kills the process.
   A dead extension is respawned lazily on its next use (tool call, command or hook) — a crash
   costs one call, not the rest of the session. `/reload` still re-reads the discovery dirs.

### Messages

Every request carries `id` (and `"v": 1`), every reply echoes it. Requests you do not understand
may be ignored silently — the host times them out.

**`initialize`** — advertise what you provide:

```json
→ {"id": 1, "type": "initialize", "v": 1, "params": {"version": "0.1.9", "cwd": "/home/me/proj"}}
← {"id": 1, "result": {
     "tools":    [{"name": "deploy", "description": "Deploy the current tree",
                    "parameters": {"type": "object", "properties": {}, "required": []}}],
     "commands": ["guard"],
     "events":   ["tool_call", "turn_end"]
   }}
```

`parameters` is JSON Schema; `commands` and `events` may be empty lists or omitted. Each tool may
carry an optional `"tier": "read" | "write" | "exec"` (default `exec`).

Tool names are registry-wide: a name that collides with a built-in or another extension is
exposed as `<extension-stem>__<name>` (the wire protocol keeps the original name).

**`call_tool`** — the model invoked one of your tools (deadline `extensions.tool_timeout`, 120s
default):

```json
→ {"id": 2, "type": "call_tool", "v": 1, "name": "deploy", "args": {}}
← {"id": 2, "result": "deployed 3 services"}
← {"id": 2, "error": "deploy script not found"}        // error tool result
```

The reply's `result` should be a string; any other JSON value is serialized as its pretty form.

**`run_command`** — the user typed your `/command` (deadline 120s):

```json
→ {"id": 3, "type": "run_command", "v": 1, "name": "web", "args": "rust ureq"}
← {"id": 3, "result": "via duckduckgo\n1. ureq - docs.rs\n   https://docs.rs/ureq …"}
```

`args` is everything after the command name, as one string. The reply prints dim in the session.

**`event`** — see the table below (deadline 5s). Only `tool_call` and `tool_result` replies are
interpreted; other events are fire-and-forget (reply `null` or nothing).

```json
→ {"id": 4, "type": "event", "v": 1, "name": "tool_call",
    "params": {"tool": "bash", "args": {"command": "rm -rf build"}}}
← {"id": 4, "result": {"decision": "deny", "reason": "build dir is mounted"}}
```

**`shutdown`** — `{"type": "shutdown"}` with no id; clean up and exit.

### Events

| Event | Fired | `params` |
|---|---|---|
| `agent_start` | once per task, before the first model call | `{cwd, task}` |
| `input` | the pending user message, before each model call | `{text, attachments: [{path, url, mime_type}]}` |
| `turn_start` | each agent loop turn | `{turn}` |
| `turn_end` | each completed model call | `{turn, usage: [in, out, cached] or null}` |
| `tool_call` | **before** each tool runs — see gate semantics | `{tool, args}` |
| `tool_result` | after each tool call (once), before it enters the transcript | `{tool, args, tool_call_id, summary, is_error, content}` — reply `{"content": ..}` to replace it, see below |
| `agent_end` | task finished or interrupted | `{final_text, interrupted}` |

### The `tool_call` gate

A `tool_call` reply may do nothing, deny, rewrite, or pre-allow:

| Reply `result` | Effect |
|---|---|
| `null` / no reply | no opinion; the built-in approval matrix decides |
| `{"decision": "deny", "reason": ".."}` | the call does not run; the model receives the reason as an error result |
| `{"args": {..}}` | the call runs with your rewritten arguments |
| `{"decision": "allow"}` | skips the approval prompt for this one call |

Multiple subscribed extensions fire in order; the first deny wins, and argument rewrites compose
(last write wins). The gate runs **before** the approval matrix — denials never prompt.

### Replacing a tool result

A `tool_result` subscriber is handed the tool's **full content** and may rewrite what the model
reads — the seam a log-reducer or redactor plugs into. Subscribing is the opt-in, because the
payload is the whole result:

```json
→ {"id": 5, "type": "event", "v": 1, "name": "tool_result",
    "params": {"tool": "bash", "args": {"command": "cargo test"}, "tool_call_id": "c1",
               "summary": "…", "is_error": false, "content": "<the full result text>"}}
← {"id": 5, "result": {"content": "<what the model should read instead>"}}
```

Reply without a `content` field (or `null`) to observe only. The replacement is re-capped like any
tool output (2000 lines / 50 KB), the last rewrite wins when several extensions reply, and the
whole path is **fail-open**: a timeout, a crash or a malformed reply leaves the tool's own result
exactly as produced. The thread file still keeps the original, so a bad rewrite cannot erase
evidence from the session log — the model can re-run the command or page the result back with
`recall`.

## Approval, tiers and policies

Every extension tool is **exec-tier** by default: the approval matrix treats it exactly like
`bash`. A tool may declare a lower tier — `# tier: write` in a script manifest, or
`"tier": "read"` on a tool in the `initialize` reply — and then the matrix treats it like the
corresponding built-in (a `read` tool runs unprompted in ask mode). This is a trust decision:
lower it only for tools you would let run anyway, since a mislabeled tool bypasses the prompt. The agent's default mode is **yolo** — extension tools run free unless a
per-tool policy says otherwise. In ask mode (`--approval-mode ask`, or config
`approval_mode = "always-ask"`) every extension tool call prompts (`Allow? [Y/n/a]` — `a`
remembers for the session). A command in the hardcoded core (privilege escalation, filesystem or
machine destruction, a fork bomb, a write into a device node) or one you added to `~/.llm/blacklist`
or `.llm/blacklist` is refused outright in either mode; the blacklist is matched against the `bash`
tool's command line only — an extension tool is whatever its script does, so gate it with `tool_call`
if it can do damage. The blacklist file only *adds* refusals; the core cannot be edited away.
Explicit per-tool policies in config.json win over everything, in either direction:

```json
{"agent": {"tools": {"deploy": "allow", "git_push": "deny"}}}
```

`--tools name1,name2` selects a subset of the mounted registry at startup. For project-specific
guardrails regardless of mode, write a `tool_call` gate.

## Timeouts and limits

| Phase | Limit | Override |
|---|---|---|
| spawn + initialize | 10s | — |
| tool call (resident) | 120s | `extensions.tool_timeout` |
| script-tool call | `timeout:` manifest field | defaults to the same config value |
| event hook | 5s | — |
| tool output | capped like the bash tool (lines + 50KB) | — |

## Host API compatibility

`examples/extensions/template.js` embeds a shim that accepts the host API surface —
`host.registerTool`, `host.registerCommand`, `host.on(...)` — and maps it onto this stdio
protocol, so most tool/command/hook extensions written against the shim paste straight in. APIs that need
the host process (UI, editors, hotkeys) raise with a clear message instead of silently no-oping.

`examples/extensions/todo.js` is a port of the reference's official `todo.ts` example: the tool and command
logic transliterates unchanged in shape — same `todo` tool with `list`/`add`/`toggle`/`clear`, same
`/todos` command. Three things could not cross the process boundary and have substitutes:

| in-process | out-of-process |
|---|---|
| `renderCall`/`renderResult` TUI components | the host's own `$ todo` action line + result summary |
| `ctx.ui.custom` full-screen component for `/todos` | the command prints the list as text |
| `ctx.sessionManager` state in session entries (branching rewinds it) | `~/.llm/todo.json` (survives restarts, shared across branches) |

Extension *logic* is portable; extension *chrome* belongs to whichever host renders it.

## Debugging checklist

- `/status` lists every extension with state, tool/command counts or the failure reason
- `/reload` respawns everything after edits
- print to **stderr** for humans (it lands in the diagnostics tail); keep stdout protocol-only
- a reply that never comes is a timeout: check you `flush()` stdout
- name the file without an extension for a clean tool/command stem
