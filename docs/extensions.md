# Extension API

Extensions are the plugin system: anything the core skips, you build yourself as an executable dropped into `~/.yak/extensions/` or the project's `.yak/extensions/`; drop a file in, restart or `/reload`. This page is the full reference; runnable examples live in [`examples/extensions/`](../examples/extensions/), one row each in the table below. Copy what you need; nothing mounts unless you install it.

Extensions run with your full user permissions and inherit your environment. Resident extensions also receive `YAK_BIN` pointing to the running yak executable, so child agents use the same build even when PATH contains an older installation; an explicit `YAK_BIN` override is preserved. Only install or write code you would run yourself.

## The examples

Every runnable example in [`examples/extensions/`](../examples/extensions/), one row each. The six tools migrated off the built-in registry (`ls`, `plan` (the former `update_plan`), `grep`+`glob` riding the one `search` extension, `memory` (the former `remember`), `webfetch`) are byte-compatible with their built-in former selves.

| Example | Kind | What it does |
|---|---|---|
| `ls` | script tool | directory listing; the reference tool the host used to carry built in, kept byte-compatible |
| `plan` | script tool | the checklist (renamed from `update_plan`): a `{step, status}` list with at most one `in_progress`; the plan lives in the model's own tool call, so no harness-side state |
| `search` | script tool | the `grep`/`glob` pair over ripgrep, read-only over the tree; read-tier, never prompts |
| `memory` | script tool | memory (renamed from `remember`): one dated deduped line into `~/.yak/YAK.md`, or with `project: true` into `<project>/.yak/YAK.md`; write-tier |
| `webfetch` | resident | the page fetcher; the one way off the machine; exec-tier |
| `wordcount` | script tool | the minimal `# --- yak-tool` starter, one stdin/stdout round |
| `template.py` / `template.rs` / `template.js` | templates | starter templates in the three languages; `template.js` runs as-is under node >= 23.6 (native type stripping), bun and deno work too; `template.rs` is compiled by the host with rustc, content-hash cached |
| `mcp_bridge.py` | resident | mounts an MCP server (stdio or streamable HTTP) as one `<server>__<tool>` per MCP tool; config in `mcp.json` |
| `mcp_notion.py` | resident | Notion's remote MCP endpoint, OAuth-only; the login runs by the human, not the agent (§ Notion below) |
| `subagent.py` | resident | runs another `yak` in its own context window against an agent definition from `~/.yak/agents/` or `.yak/agents/` |
| `skills` | resident | SKILL.md discovery as a plugin: the `<available_skills>` system-prompt section, the banner/`/help` skill list and every `/skill:<name>` command — global (`~/.yak/skills`, `~/.agents/skills`, `~/.yak/pkg`) and project-level (nearest `.yak/skills`/`.agents/skills`, `.yak/pkg`) homes, project shadowing user by name |
| `repeat_guard.py` | resident | a `tool_call` deny gate for stuck loops |
| `fold_repeats.py` | resident | a `tool_result` rewriter that folds repeated log lines |
| `workspace_checkpoint.py` | resident | a `turn_start`/`session_before_tree` pair that snapshots the workspace and offers to roll it back on `/tree` |
| `notify.py` | resident | an `approval`/`agent_end` subscriber that raises a desktop notification; approval prompts and finished answers reach you outside the terminal |
| `remote.py` | resident | the phone bridge: notifications out to Telegram/email/WeCom/ServerChan/Bark, instructions back in over Telegram or mailbox polling, typed into the session's tmux pane or held in an inbox until the session is free; keep-awake hold on the machine while it lives; config in `remote.json`, written by `remote init` |
| `jev.js` | resident | TypeSafe's System One model as a `jev` tool: typed choice/noul/score questions about a text, answered with probabilities; `TYPESAFE_API_KEY`, no keyless fallback |
| `model_here` | resident | the project-model pin: `/model-here` reads and writes `models.default` in `<project>/.yak/config.json`, honored inside the project |
| `model_pin` | resident | the per-directory model pin: `/model-pin` reads and writes `<user_dir>/model-pins.json`, the nearest pinned ancestor of cwd wins at startup |
| `websearch` | resident | a `/web` command plus `web_search` tool; Brave's Search API when `BRAVE_API_KEY` is set, else a keyless DuckDuckGo→Wikipedia chain |
| `ssh.py` | resident | remote access over plain `ssh` subprocesses with hardened arguments (BatchMode, no forwarding, host keys pinned in the extension's own known_hosts); `ssh_exec`/`ssh_read`/`ssh_write` and a `/ssh` status command; hosts in `ssh.json` |

## Discovery

| Home | Notes |
|---|---|
| `.yak/extensions/` (nearest walking up from cwd) | project copy, wins by name |
| `<user_dir>/pkg/<pkg>/extensions/` (project-local, then user pkgs) | from `yak install` packages |
| `~/.yak/extensions/` | user-global |

Every file (not directory) in these homes is scanned. Project extensions take precedence over installed packages, then project packages over user packages and user extensions. Package directories at the same level are scanned by name. An absent home is ignored; an unreadable home or invalid manifest fails discovery with its path. A file is either a **script tool** (it carries a manifest header, see below) or a **resident extension** (anything else, but it must be executable: the exec bit on unix, one of `.exe`/`.bat`/`.cmd`/`.ps1` on Windows). Config keys:

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
| `arg-mode: argv` | exactly one declared argument (required); its value arrives as plain `argv[1]` instead of stdin JSON |
| `interpreter: <prog>` | run the file through this program (`${ENV}` expanded); without it the file runs itself |
| `timeout: <secs>` | per-call timeout; default `extensions.tool_timeout` (120s) |
| `tier: read\|write\|exec` | trust tier the approval matrix sees; default `exec` |

Invocation contract:

- default mode: the arguments object arrives as **one JSON line on stdin** (`{"text": "..."}`)
- `arg-mode: argv`: requires exactly one declared argument, delivered as `argv[1]` as plain text; an absent value fails the call rather than being replaced with an empty string
- stdout is the tool result (stderr is appended when non-empty; both are capped)
- exit code `0` → normal result, anything else → error result (the exit code is appended)
- timeout → the call is interrupted and returned as an error result
- cwd is the agent's working directory
- script tools are exec-tier unless the manifest declares otherwise: the approval matrix applies (see below)

### Rust script tools

A `.rs` file with the same header is a script tool too — Rust has no interpreter, so the host compiles it instead: first call runs `rustc --edition 2021 -O` and caches the binary under `~/.yak/tmp/rs-cache/<stem>-<content-hash>` (rebuilt only when the source changes; the cache rides the weekly tmp sweep). The compile shares the tool's timeout and rustc's stderr reaches the tool log, so a broken tool fails with its diagnostics.

```rust
// --- yak-tool: rustgreet
// description: greet a name
// args: name (string) the name to greet
// arg-mode: argv
fn main() {
    let name = std::env::args().nth(1).unwrap_or_default();
    println!("hello, {name}");
}
```

The stdlib is what a bare `rustc` gives you: no crates, no Cargo project — single-file sources only (the manifest header lines are comments, so they compile as-is). Crates need a real build; for that build a binary the normal way (`cargo build --release`) and drop the executable in as a resident extension instead. A missing rustc is an error with install instructions (rustup or a system package), never a silent fallback.

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
→ {"id": 1, "type": "initialize", "v": 1, "params": {"version": "0.1.9", "cwd": "/home/me/proj", "user_dir": "/home/me/.yak"}}
← {"id": 1, "result": {
     "tools":    [{"name": "deploy", "description": "Deploy the current tree",
                    "parameters": {"type": "object", "properties": {}, "required": []}}],
     "commands": ["guard"],
     "events":   ["tool_call", "turn_end"],
     "system":   "a section appended to the agent system prompt",
     "skills":   [{"name": "pdf", "description": "extract tables", "model_invocation": true}],
     "tool_timeout": 900
   }}
```

`parameters` is JSON Schema; `commands` and `events` may be empty lists or omitted. Each tool may carry an optional `"tier": "read" | "write" | "exec"` (default `exec").

`params.user_dir` is the resolved yak user directory (honors `YAK_USER_PATH`), so an extension can find its global homes without guessing at `~`.

Three more reply keys mount surfaces beyond tools and commands:

- **`system`** (string): appended to the agent system prompt for the session — the seam a skill index, a project glossary or house rules ride on (the `skills` example builds its `<available_skills>` block here). Continuations keep the stored prompt verbatim, sections included.
- **`skills`** (list of `{name, description, model_invocation?}`): the skills your extension mounts; the banner, `/help` and `/skill:` completion list them. `model_invocation` defaults true (listed to the model in whatever `system` section you contribute).
- **`task`** in a `run_command` reply (see below).

`tool_timeout` (seconds, optional) is your own deadline for `call_tool`: ask for it when your tool legitimately runs for minutes (spawning a build, running another agent). It replaces the config default for this extension and is clamped to one hour; omit it and `extensions.tool_timeout` applies. The wait stays interruptible either way.

Tool names are registry-wide: a name that collides with a built-in or another extension is exposed as `<extension-stem>__<name>` (the wire protocol keeps the original name).

**`call_tool`**: the model invoked one of your tools (deadline: the `tool_timeout` you asked for at `initialize`, else `extensions.tool_timeout`, 120s default):

```json
→ {"id": 2, "type": "call_tool", "v": 1, "name": "deploy", "args": {}, "tool_call_id": "call_7f3"}
← {"id": 2, "result": "deployed 3 services"}
← {"id": 2, "error": "deploy script not found"}        // error tool result
```

`tool_call_id` is the model's own id for this call: key per-call state on it when several calls of your tool can be in flight (the read-only batch runs them concurrently). A script tool (`# --- yak-tool:`) does not see it: the host feeds those the arguments only.

The reply's `result` should be a string; a list lays out as **segments** — strings are text items, `{"text": .., "console": true}` items are console lines — with more than one text item each headed `==> text N/M <==` and console lines following in one `<console_output>` block. Any other JSON value is serialized as its pretty form. A string over the shared caps (2000 lines / 50 KB / a 12k-token estimate) is cut to the tail and the cut **marked in the text** (`[output truncated to fit the context window]`), exactly as the builtin tools mark theirs: the model must be able to tell a complete answer from a truncated one.

**`run_command`**: the user typed your `/command` (deadline 120s):

```json
→ {"id": 3, "type": "run_command", "v": 1, "name": "web", "args": "rust ureq"}
← {"id": 3, "result": "via duckduckgo\n1. ureq - docs.rs\n   https://docs.rs/ureq …"}
```

`args` is everything after the command name, as one string. The reply prints dim in the session. A command whose answer is *work for the model* replies with a task instead of text — `{"result": {"task": "…"}}` — and the session submits that string as one agent task (the `/skill:<name>` shape: the `skills` example reads the SKILL.md off disk and hands back a `<skill name dir>` prompt). `task` wins over a printed `result` when both ride the reply.

**`event`**: see the table below (deadline 5s). Only `tool_call`, `tool_result` and `compact` replies are interpreted; other events are fire-and-forget (reply `null` or nothing).

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

### Lane frames

While a call is in flight an extension may stream **lane frames** on stdout — id-less lines the terminal renders as one status row per lane, rewritten in place as work moves (stderr stays the fire-and-forget diagnostics channel):

```json
→ {"type": "lane", "v": 1, "lane": "1/2 coder", "text": "bash: cargo test"}
```

- one lane name = one row on screen; an update with a known name rewrites that row, an unknown name appends a row below the block
- `text` is one line (overlong rows wrap to the terminal); the whole block retracts before any other output prints and repaints below it, so rows never interleave with streamed tool output
- scoped to the running call: the block is gone at the next `tool_start`, and lanes never enter the transcript — the tool result is the report
- `--json` surfaces each frame as a `{"type": "lane", "lane": .., "text": ..}` event, so a wrapper can mirror the parallel view
- a lane frame missing `lane` or `text` is not an error: it lands in the diagnostics tail like any other id-less line

`subagent.py` drives its whole live view this way: every child's tool activity, worktree verdict and final state update that child's row, so two parallel writers read as two rows each ticking on their own.

### Events

| Event | Fired | `params` |
|---|---|---|
| `agent_start` | once per task, before the first model call | `{cwd, task}` |
| `input` | the pending user message, before each model call | `{text, attachments: [{path, url, mime_type}], cwd, focused?}`: same `cwd`/`focused` semantics as `agent_end` |
| `turn_start` | each agent loop turn | `{turn}` |
| `turn_end` | each completed model call | `{turn, usage: [in, out, cached] or null}` |
| `tool_call` | **before** each tool runs: see gate semantics | `{tool, args}` |
| `tool_result` | after each tool call (once), before it enters the transcript | `{tool, args, tool_call_id, summary, is_error, error, content}`: reply `{"content": ..}` to replace it, see below |
| `compact` | a compaction is due — the pre-request check, an overflow refusal, a silent overflow, or the manual `/compact` — before the built-in summarizer runs | `{reason: "trigger"\|"overflow"\|"stalled"\|"manual", trigger_tokens, keep_recent_tokens, instructions?}`: `instructions` rides only a manual `/compact [text]`; reply `{"decision": "compact", "summary": .., "keep_recent_tokens": n}` to replace it, see below |
| `approval` | an approval prompt is about to surface, before it blocks | `{cwd, tool, preview, pattern, focused?}`: `pattern` is the matched ask-list line for a bash ask, else null — the desktop-notification seam (see `notify.py`); fire-and-forget. Auto-allowed calls (read-tier, allow policies, `tool_call` pre-allow without a blacklist hit) never fire it |
| `agent_end` | task finished or interrupted | `{cwd, final_text, interrupted, focused?}`: `cwd` (on both events, and on `agent_start`) is the session's working directory — with more than one yak running it is what tells the notifications apart. `focused` (on both events) is present only when the terminal reports focus (`ESC[I`/`ESC[O`, the focus-report mode held while yak reads input): true means the window is focused, so the user is already looking and a notification is noise — `notify.py` stays quiet then (its `NOTIFY_WHEN = "always"` opts out); absent means focus is unknown, and a gate must not guess |
| `session_before_tree` | `/tree` picked a turn, before the session moves to it | `{thread, kept_turns, dropped_turns, dropped_ids}`: the ids leaving the active branch (the active chain minus the chain the jump lands on; the turns stay in the file as a sibling branch — nothing is deleted) — restore the workspace here (see the checkpoint example); a hook error or timeout aborts the jump before the transcript moves |

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

### Replacing a compaction

A `compact` subscriber sees that a compaction is due and may replace the built-in summarize-the-prefix step with its own summary — the seam a privacy filter, a domain-specific distiller or a side-model call plugs into. The event tells you why it fired: `trigger` (the token estimate crossed the trigger), `overflow` (the provider refused the prompt as too large), `stalled` (a previous attempt failed; this is the retry), `manual` (the user ran `/compact`; `instructions` carries its optional focus text):

```json
→ {"id": 7, "type": "event", "v": 1, "name": "compact",
    "params": {"reason": "trigger", "trigger_tokens": 64000, "keep_recent_tokens": 20000}}
← {"id": 7, "result": {"decision": "compact", "summary": "<the new context prefix>", "keep_recent_tokens": 8000}}
```

`trigger_tokens` is the resolved trigger (0 when automatic compaction is off — a `manual` event can still arrive then, because `/compact` is never gated on it).

A reply carrying `decision: "compact"` and a non-empty `summary` replaces the whole compaction: your summary becomes the context prefix (a `Summary` message the model reads as ``<summary>``-wrapped history). During a task the replacement **ends** the task — the summary is a verdict, not a recap, so no model round follows it and the summary text is the task's final answer; a manual `/compact` folds the idle session and returns to the prompt instead. `keep_recent_tokens` is optional: it re-derives how much recent history stays readable beside the summary (omit it to keep the configured cut). Replying anything else observes only, and the built-in summarizer runs. Like every hook the path is **fail-open**: a timeout, a crash or a malformed reply falls back to the built-in compaction, never to a dropped compaction. With no subscriber at all the event costs nothing.

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
| tool call (resident) | 120s | the extension's `tool_timeout` in its `initialize` reply (≤ 1h), else `extensions.tool_timeout`; timeout kills the resident process before another call may respawn it |
| script-tool call | `timeout:` manifest field | defaults to the same config value |
| event hook | 5s |; |
| resident stdout/stderr frame | 1 MiB per newline-delimited line; an oversized stdout frame closes the connection and fails pending calls |; |
| tool output | capped like the bash tool (lines + 50KB + a 12k-token estimate), the cut marked in the text |; |

## Host API compatibility

`examples/extensions/template.js` embeds a shim that accepts the host API surface (`host.registerTool`, `host.registerCommand`, `host.on(...)`) and maps it onto this stdio protocol, so most tool/command/hook extensions written against the shim paste straight in. APIs that need the host process (UI, editors, hotkeys) raise with a clear message instead of silently no-oping.

`examples/extensions/websearch` is trimmed to `web_search` + `/web`, since page fetching is `examples/extensions/webfetch`. Extension *logic* is portable; extension *chrome* belongs to whichever host renders it, and a capability the host already offers belongs in the host.

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

The HTTP transport is streamable HTTP: one POST per JSON-RPC message, a JSON body or an SSE stream back, and the server's `Mcp-Session-Id` echoed once it hands one out (DELETE ends the session). Requests carry a real `User-Agent` (CDN firewalls drop the python-urllib default — Cloudflare's own edge answers it with error 1010). `tools/list` follows the server's `nextCursor` pagination, so a server with more tools than fit one page still mounts them all. An entry may carry a `"tools": {"only": [...], "except": [...]}` filter (fnmatch against the MCP-side names) that cuts tools at the mount: schemas ride every request, so a tool the session never wants costs tokens whether or not it is called. Servers that only do the older two-endpoint SSE dance, or OAuth without a static token, are out of scope: a server that fails its handshake is skipped with a dim warning on stderr and the rest stay up.

The token-cost shape of a server is the server's own choice. Notion's endpoint advertises every workspace action with a long usage guide per tool; Cloudflare went the other way: its API server (2,500+ endpoints behind OAuth) exposes two tools — `search` and `execute` (the search-and-execute Code Mode pattern, ~1k tokens where full schemas would cost ~1.17M) — and its docs server at `https://docs.mcp.cloudflare.com/mcp` needs no auth at all, two tools, under 1k tokens of schema. Pick servers that respect the injection; disable the rest per session with `extensions.disabled`.

## Notion in one file: the `mcp_notion.py` example

Where the bridge stops — an OAuth-only server with no static token — `examples/extensions/mcp_notion.py` picks up: Notion's remote MCP endpoint, with the login run by the human instead of the agent. Copy it into `~/.yak/extensions/` (executable, no `mcp.json` needed) and log in once from a terminal:

```bash
python3 ~/.yak/extensions/mcp_notion login    # prints the URL, opens the browser
python3 ~/.yak/extensions/mcp_notion status   # token left? tools cached?
```

No tool call ever opens a browser: the extension answers the host's `initialize` purely from disk (`token.json` + `tools.json` beside the file, 0600, both written by `login`), so startup is network-free and cannot blow the host's 10s window. With a token cached it mounts the full `notion__*` tool list (search, fetch, create-pages, comments, ...); without one it mounts nothing and every yak start says how to log in. The MCP session itself opens lazily on the first tool call. A token lives ~8h; when it expires every call says so, and another `login` + `/reload` fixes it. The login round is the same spec chain (RFC 9728 discovery → RFC 7591 dynamic registration → PKCE S256 → form-encoded exchange) the bridge declines to run.

## Running another yak: the `subagent.py` example

The one example that exercises every part of this page at once: `subagent.py` mounts a `subagent` tool that spawns a real `yak` child in its own context window and hands the conclusion back as a tool result.

```bash
yak --json --no-session --tools read,grep \
    -m <model> --thinking <level> --append-system-prompt "<agent body>" "Task: ..."
```

- The child's *agent definition* is markdown with frontmatter, from `~/.yak/agents/<name>.md` or the project's `.yak/agents/<name>.md` (nearest wins): `name`, `description`, `tools`, `model`, `thinking`, and the body becomes the appended system prompt. Discovery, parsing and the depth guard live in the extension: the core knows nothing about agents.
- `--json` is what makes the child observable: one JSON object per line (`text`, `reasoning`, `tool_start`, `tool_log`, `tool_end`, `turn_end`, `result`). The extension turns those into the parent's live tool log and reads the final answer off the `result` object, which is why it needs no parsing of human-facing text.
- The child is stopped on the host's `interrupt` and on its own deadline, `YAK_SUBAGENT_DEPTH` refuses nesting, and the `--tools` whitelist keeps it from calling `subagent` at all. The env var doubles as a child marker for user extensions: a subagent child inherits `~/.yak/extensions/`, so `notify.py` and `remote.py` read `YAK_SUBAGENT_DEPTH` and stay silent inside one (a child's `agent_end` is subagent machinery, not work you submitted — without the gate every child in a `$TMPDIR` worktree would raise its own notification).
- The `subagent` call runs free like any exec tool (there is no ask mode); the definition's `tools:` line is what bounds the child.
- **Worktree isolation**: when the child is a *writer* — its definition's `tools:` line names a mutating tool (`write`, `edit` or `bash`) and the parent's cwd is a git repo with at least one commit — it does not run in place. The extension forks a fresh `git worktree` (`$TMPDIR/yak-wt-<pid>/wt-<n>`, branch `yak/subagent-<pid>-<n>`), runs the child there, commits the diff, and merges the branch back into the parent repo under a lock (parallel writers merge serially; their runs stay parallel). The verdict rides the tool result — `merge merged: <git line>`, `merge conflict: <CONFLICT line> — branch <name> kept; resolve with git merge <name>`, or `merge none: no changes`. A conflict is aborted (`merge --abort`) so the parent tree is never left mid-merge; the branch and its worktree are **kept** for the human, as is anything from a failed run (“kept for inspection”). Nothing a writer did is ever silently dropped. Set `YAK_SUBAGENT_WORKTREES=0` to disable and run writers in place; readers and non-repo cwds always run in place.

`examples/agents/` ships four definitions to copy: `scout` and `reviewer` (read-only), `planner` (think-only) and `worker` (can edit and run commands).

## Debugging checklist

- `/status` lists every extension with state, tool/command counts or the failure reason
- `/reload` respawns everything after edits
- print to **stderr** for humans: it lands in the diagnostics tail, and during a call it streams into that call's tool log line by line
- a reply that never comes is a timeout: check you `flush()` stdout
- name the file without an extension for a clean tool/command stem
