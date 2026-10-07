# Extension API

Extensions are the plugin system: anything the core skips, you build yourself as an executable dropped into `~/.yak/extensions/` or the project's `.yak/extensions/`; drop a file in, restart or `/reload`. This page is the full reference; runnable examples live in [`examples/extensions/`](../examples/extensions/) (`wordcount`, `websearch`, `subagent.py` a tool that runs another `yak` in its own context window, `repeat_guard.py` a `tool_call` deny gate for stuck loops, `fold_repeats.py` a `tool_result` rewriter that folds repeated log lines, `workspace_checkpoint.py` a `turn_start`/`session_before_tree` pair that snapshots the workspace and offers to roll it back on `/tree`, plus the `template.js`/`template.py` starter templates). `template.js` runs as-is under node >= 23.6 (native type stripping); bun and deno work too.

Extensions run with your full user permissions and inherit your environment. Only install or write code you would run yourself.

## Discovery

| Home | Notes |
|---|---|
| `.yak/extensions/` (nearest walking up from cwd) | project copy, wins by name |
| `<user_dir>/pkg/<pkg>/extensions/` (project-local, then user pkgs) | from `yak install` packages |
| `~/.yak/extensions/` | user-global |

Every file (not directory) in these homes is scanned. Same stem name in two homes: the project copy wins. A file is either a **script tool** (it carries a manifest header, see below) or a **resident extension** (anything else, but it must be executable: the exec bit on unix, one of `.exe`/`.bat`/`.cmd`/`.ps1` on Windows). Config keys:

```json
{
  "extensions": {
    "disabled": ["websearch"],
    "tool_timeout": 120
  }
}
```

`disabled` matches a file's stem or a script tool's declared manifest name, either spelling disables it.

`/reload` re-discovers and respawns everything. A broken or slow extension degrades alone: it warns dimly and mounts nothing; `/status` shows the failure reason. Extension stderr is kept in a 20-line diagnostics tail: stray stdout that is not valid protocol lands there too.

## Script tools

A comment header on any script makes it a tool. The host spawns it **per call**, feeds the arguments, collects stdout as the result and owns timeout and size caps. No exec bit needed: declare an interpreter and the host runs the script through it (this is what makes the form work on Windows too).

```python
#!/usr/bin/env python3
# --- yak-tool: wordcount
# description: count characters, words and lines of a text
# args: text (string) the text to measure
# arg-mode: argv
# interpreter: python3
# timeout: 10
import sys
text = sys.argv[1] if len(sys.argv) > 1 else ""
print(f"{len(text)} chars · {len(text.split())} words")
```

Manifest fields (lines of `#` or `//` comments after the `--- yak-tool:` marker, until the first non-comment line):

| Field | Meaning |
|---|---|
| `--- yak-tool: <name>` | the marker + tool name (required) |
| `description: <text>` | shown to the model in the tool list |
| `args: <name> (<type>) <desc>` | one schema property; repeatable. Types: `string`, `int`/`integer`, `number`/`float`, `bool`/`boolean`, `list`/`array`. Every declared arg is required |
| `arg-mode: argv` | single declared argument arrives as plain `argv[1]` instead of stdin JSON |
| `interpreter: <prog>` | run the file through this program (`${ENV}` expanded); without it the file runs itself |
| `timeout: <secs>` | per-call timeout; default `extensions.tool_timeout` (120s) |
| `tier: read\|write\|exec` | trust tier the approval matrix sees; default `exec` |

Invocation contract:

- default mode: the arguments object arrives as **one JSON line on stdin** (`{"text": "..."}`)
- `arg-mode: argv`: the single declared argument arrives as `argv[1]` as plain text: shell scripts never parse JSON
- stdout is the tool result (stderr is appended when non-empty; both are capped)
- exit code `0` → normal result, anything else → error result (the exit code is appended)
- timeout → the call is interrupted and returned as an error result
- cwd is the agent's working directory
- script tools are exec-tier unless the manifest declares otherwise: the approval matrix applies (see below)

## Resident extensions

For hooks, commands and multi-call tools: the process is spawned **once** per session and speaks one JSON message per line over stdio. stdout carries only the protocol: anything else goes to the diagnostics tail. stderr is the human channel, and while a call is in flight it is also **live progress**: every line the extension prints is shown in the session's tool log for that call (dim, same as a tool's own output) and none of it reaches the model: the tool result stays exactly the string your reply carries. Use it for "found 3 of 50 files" reporting from a long tool; the buffer is bounded (the last 64 lines per call) and drained when the next call starts, so stderr is progress, never a result channel.

### Lifecycle

1. Host spawns the file (cwd = agent working directory).
2. Host sends `initialize`; the extension must reply within **10s**.
3. Requests flow: `call_tool`, `run_command`, `event`: each answered by id.
4. On exit or `/reload`: host sends `shutdown` and waits up to 0.5s for a clean exit, then kills the process. A dead extension is respawned lazily on its next use (tool call, command or hook): a crash costs one call, not the rest of the session. `/reload` still re-reads the discovery dirs.

### Messages

Every request carries `id` (and `"v": 1`), every reply echoes it. Requests you do not understand may be ignored silently: the host times them out.

**`initialize`**: advertise what you provide:

```json
→ {"id": 1, "type": "initialize", "v": 1, "params": {"version": "0.1.9", "cwd": "/home/me/proj"}}
← {"id": 1, "result": {
     "tools":    [{"name": "deploy", "description": "Deploy the current tree",
                    "parameters": {"type": "object", "properties": {}, "required": []}}],
     "commands": ["guard"],
     "events":   ["tool_call", "turn_end"],
     "tool_timeout": 900
   }}
```

`parameters` is JSON Schema; `commands` and `events` may be empty lists or omitted. Each tool may carry an optional `"tier": "read" | "write" | "exec"` (default `exec`).

`tool_timeout` (seconds, optional) is your own deadline for `call_tool`: ask for it when your tool legitimately runs for minutes (spawning a build, running another agent). It replaces the config default for this extension and is clamped to one hour; omit it and `extensions.tool_timeout` applies. The wait stays interruptible either way.

Tool names are registry-wide: a name that collides with a built-in or another extension is exposed as `<extension-stem>__<name>` (the wire protocol keeps the original name).

**`call_tool`**: the model invoked one of your tools (deadline: the `tool_timeout` you asked for at `initialize`, else `extensions.tool_timeout`, 120s default):

```json
→ {"id": 2, "type": "call_tool", "v": 1, "name": "deploy", "args": {}, "tool_call_id": "call_7f3"}
← {"id": 2, "result": "deployed 3 services"}
← {"id": 2, "error": "deploy script not found"}        // error tool result
```

`tool_call_id` is the model's own id for this call: key per-call state on it when several calls of your tool can be in flight (the read-only batch runs them concurrently). A script tool (`# --- yak-tool:`) does not see it: the host feeds those the arguments only.

The reply's `result` should be a string; a list lays out as **segments** — strings are text items, `{"text": .., "console": true}` items are console lines — with more than one text item each headed `==> text N/M <==` and console lines following in one `<console_output>` block. Any other JSON value is serialized as its pretty form.

**`run_command`**: the user typed your `/command` (deadline 120s):

```json
→ {"id": 3, "type": "run_command", "v": 1, "name": "web", "args": "rust ureq"}
← {"id": 3, "result": "via duckduckgo\n1. ureq - docs.rs\n   https://docs.rs/ureq …"}
```

`args` is everything after the command name, as one string. The reply prints dim in the session.

**`event`**: see the table below (deadline 5s). Only `tool_call` and `tool_result` replies are interpreted; other events are fire-and-forget (reply `null` or nothing).

```json
→ {"id": 4, "type": "event", "v": 1, "name": "tool_call",
    "params": {"tool": "bash", "args": {"command": "rm -rf build"}}}
← {"id": 4, "result": {"decision": "deny", "reason": "build dir is mounted"}}
```

**`interrupt`**: the user pressed ctrl+c while your tool call was running, so the host has stopped waiting and gone on with the turn:

```json
→ {"id": 6, "type": "interrupt", "v": 1, "cancelled": 5}
```

No reply is expected (the request is already abandoned; answer if you like, the reply is dropped as unknown). Most extensions can ignore it: your call's deadline is what ends it. It exists for tools that own work of their own: stop child processes, release locks, then return early. An extension that only learns about it from its main loop hears it late, because that loop is blocked inside the call; `subagent.py` reads stdin on a second thread for exactly this reason.

**`shutdown`**: `{"type": "shutdown"}` with no id; clean up and exit.

### Events

| Event | Fired | `params` |
|---|---|---|
| `agent_start` | once per task, before the first model call | `{cwd, task}` |
| `input` | the pending user message, before each model call | `{text, attachments: [{path, url, mime_type}]}` |
| `turn_start` | each agent loop turn | `{turn}` |
| `turn_end` | each completed model call | `{turn, usage: [in, out, cached] or null}` |
| `tool_call` | **before** each tool runs: see gate semantics | `{tool, args}` |
| `tool_result` | after each tool call (once), before it enters the transcript | `{tool, args, tool_call_id, summary, is_error, error, content}`: reply `{"content": ..}` to replace it, see below |
| `agent_end` | task finished or interrupted | `{final_text, interrupted}` |
| `session_before_tree` | `/tree` picked a turn, before the transcript is cut | `{thread, kept_turns, dropped_turns, dropped_ids}`: snapshot or restore the workspace here (see the checkpoint example); fire-and-forget, the jump proceeds either way |

### The `tool_call` gate

A `tool_call` reply may do nothing, deny, rewrite, or pre-allow:

| Reply `result` | Effect |
|---|---|
| `null` / no reply | no opinion; the built-in approval matrix decides |
| `{"decision": "deny", "reason": ".."}` | the call does not run; the model receives the reason as an error result |
| `{"args": {..}}` | the call runs with your rewritten arguments |
| `{"decision": "allow"}` | skips the approval prompt for this one call |

Multiple subscribed extensions fire in order; the first deny wins, and argument rewrites compose (last write wins). The gate runs **before** the approval matrix: denials never prompt.

### Replacing a tool result

A `tool_result` subscriber is handed the tool's **full content** and may rewrite what the model reads: the seam a log-reducer or redactor plugs into. Subscribing is the opt-in, because the payload is the whole result:

```json
→ {"id": 5, "type": "event", "v": 1, "name": "tool_result",
    "params": {"tool": "bash", "args": {"command": "cargo test"}, "tool_call_id": "c1",
               "summary": "…", "is_error": false, "error": null, "content": "<the full result text>"}}
← {"id": 5, "result": {"content": "<what the model should read instead>"}}
```

`error` is the refusal class when the call did not run as an ordinary failing tool: `"failed"` (ran and failed), `"denied"` (approval, a gate, or bad arguments), `"unknown_tool"`, `"interrupted"`: `null` on success, and always consistent with `is_error`.

Reply without a `content` field (or `null`) to observe only. The replacement is re-capped like any tool output (2000 lines / 50 KB), the last rewrite wins when several extensions reply, and the whole path is **fail-open**: a timeout, a crash or a malformed reply leaves the tool's own result exactly as produced. The thread file still keeps the original, so a bad rewrite cannot erase evidence from the session log: the model can re-run the command or re-read the file.

## Approval, tiers and policies

Every extension tool is **exec-tier** by default and runs free like `bash`. A tool may declare a lower tier (`# tier: write` in a script manifest, or `"tier": "read"` on a tool in the `initialize` reply) which groups it into the read-only parallel batch. There is no ask mode: a call runs unless a per-tool policy says otherwise, the hardcoded core refuses it, or the blacklist asks. A command in the hardcoded core (privilege escalation, filesystem or machine destruction, a fork bomb, a write into a device node) or one you added to `~/.yak/blacklist` or `.yak/blacklist` is refused or asks; the blacklist is matched against the `bash` tool's command line only: an extension tool is whatever its script does, so gate it with `tool_call` if it can do damage. The blacklist file only *adds* refusals; the core cannot be edited away. Explicit per-tool policies in config.json win over everything, in either direction:

```json
{"agent": {"tools": {"deploy": "allow", "git_push": "deny"}}}
```

`--tools name1,name2` selects a subset of the mounted registry at startup. For project-specific guardrails regardless of mode, write a `tool_call` gate.

## Timeouts and limits

| Phase | Limit | Override |
|---|---|---|
| spawn + initialize | 10s |; |
| tool call (resident) | 120s | the extension's `tool_timeout` in its `initialize` reply (≤ 1h), else `extensions.tool_timeout` |
| script-tool call | `timeout:` manifest field | defaults to the same config value |
| event hook | 5s |; |
| tool output | capped like the bash tool (lines + 50KB) |; |

## Host API compatibility

`examples/extensions/template.js` embeds a shim that accepts the host API surface (`host.registerTool`, `host.registerCommand`, `host.on(...)`) and maps it onto this stdio protocol, so most tool/command/hook extensions written against the shim paste straight in. APIs that need the host process (UI, editors, hotkeys) raise with a clear message instead of silently no-oping.

`examples/extensions/websearch` is trimmed to `web_search` + `/web`, since page fetching is the built-in `webfetch`. Extension *logic* is portable; extension *chrome* belongs to whichever host renders it, and a capability the host already offers belongs in the host.

## Mounting MCP servers: the `mcp_bridge.py` example

The MCP support lives here rather than in the binary: `examples/extensions/mcp_bridge.py` is a resident extension that starts every server in an `mcp.json` beside itself and mounts each of their tools as `<server>__<tool>`, exec-tier, so the approval matrix applies as usual. Entries pick their transport (`command` (stdio, `args`/`env`) or `url` (streamable HTTP, `headers`)) and `${VAR}` in any string value expands from the environment, so a token never has to sit in the file:

```json
{
  "cloudflare": {
    "type": "streamable-http",
    "url": "https://mcp.cloudflare.com/mcp",
    "headers": { "Authorization": "Bearer ${CLOUDFLARE_MCP_TOKEN}" }
  },
  "fetch": { "command": "uvx", "args": ["mcp-server-fetch"] }
}
```

The HTTP transport is streamable HTTP: one POST per JSON-RPC message, a JSON body or an SSE stream back, and the server's `Mcp-Session-Id` echoed once it hands one out (DELETE ends the session). Servers that only do the older two-endpoint SSE dance, or OAuth without a static token, are out of scope: a server that fails its handshake is skipped with a dim warning on stderr and the rest stay up.

## Running another yak: the `subagent.py` example

The one example that exercises every part of this page at once: `subagent.py` mounts a `subagent` tool that spawns a real `yak` child in its own context window and hands the conclusion back as a tool result.

```bash
yak --json --no-session --tools read,grep \
    -m <model> --thinking <level> --append-system-prompt "<agent body>" "Task: ..."
```

- The child's *agent definition* is markdown with frontmatter, from `~/.yak/agents/<name>.md` or the project's `.yak/agents/<name>.md` (nearest wins): `name`, `description`, `tools`, `model`, `thinking`, and the body becomes the appended system prompt. Discovery, parsing and the depth guard live in the extension: the core knows nothing about agents.
- `--json` is what makes the child observable: one JSON object per line (`text`, `reasoning`, `tool_start`, `tool_log`, `tool_end`, `turn_end`, `result`). The extension turns those into the parent's live tool log and reads the final answer off the `result` object, which is why it needs no parsing of human-facing text.
- The child is stopped on the host's `interrupt` and on its own deadline, `YAK_SUBAGENT_DEPTH` refuses nesting, and the `--tools` whitelist keeps it from calling `subagent` at all.
- The `subagent` call runs free like any exec tool (there is no ask mode); the definition's `tools:` line is what bounds the child.

`examples/agents/` ships four definitions to copy: `scout` and `reviewer` (read-only), `planner` (think-only) and `worker` (can edit and run commands).

## Debugging checklist

- `/status` lists every extension with state, tool/command counts or the failure reason
- `/reload` respawns everything after edits
- print to **stderr** for humans: it lands in the diagnostics tail, and during a call it streams into that call's tool log line by line
- a reply that never comes is a timeout: check you `flush()` stdout
- name the file without an extension for a clean tool/command stem
