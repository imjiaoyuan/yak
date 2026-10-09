# Usage

Everything beyond the quick start: tools, approvals, the session store, models and config, plugins, attachments, the `--json` machine interface. Nothing here is needed to run `yak "fix the failing test"`; that much is on the front page. This page is for the second day, when you want to tune the agent, write an extension, or understand where your conversations live.

## Table of contents

- [The interactive session](#the-interactive-session)
- [Per-run flags](#per-run-flags)
- [Approvals and the blacklist](#approvals-and-the-blacklist)
- [Tools](#tools)
- [Attachments](#attachments)
- [Sessions](#sessions)
- [Skills](#skills)
- [Prompt templates](#prompt-templates)
- [Providers and models](#providers-and-models)
- [Tuning the agent](#tuning-the-agent)
- [The --json interface](#the---json-interface)
- [Plugins](#plugins)
- [Semantics](#semantics)

## The interactive session

Bare `yak` opens the interactive session. Slash commands cover the model and the session: `/model`, `/thinking`, `/login`, `/logout`, `/clear`, `/resume`, `/tree`, `/status`, `/reload`, … `/help` lists every one, including your skills as `/skill:<name>`. `!cmd` runs a shell command directly, and tab completes command names and paths.

Ctrl-c or esc interrupts a running task. There is no input surface while it works: keystrokes other than the interrupt are swallowed, and the next message is composed in the editor once the task ends.

A project instructions file (`AGENTS.md` / `AGENTS.override.md` / `CLAUDE.md`, the nearest walking up from the working directory, `AGENTS.override.md` winning) rides the system prompt whole; the banner shows which one is in play.

## Per-run flags

```bash
yak -m deepseek/deepseek-chat "..."        # pick a model for this run
yak -o temperature=0.2 -o top_p=0.9 "..."  # extra model options
yak --thinking high "..."                  # off | minimal | low | medium | high | xhigh
yak -s "you are a Rust reviewer" "..."     # replace the system prompt
yak --append-system-prompt "be terse" "..."
yak --tools read,grep "..."                # limit the toolbox (names or * patterns; +name/-name edits the default set)
yak --json "..."                           # line-delimited events instead of the UI
```

`--thinking` maps to `reasoning_effort` on OpenAI-compatible endpoints and to a thinking budget on Anthropic ones. `--max-request-bytes` caps one request body in bytes (32MB by default): lower it when a gateway in front of the model refuses less than the provider documents.

## Approvals and the blacklist

The agent runs automatically: it does whatever it needs without asking. What stops it is a hard refusal or a blacklist ask, never a broad ask-first mode, and it comes in two layers.

**Hardcoded: nothing can switch these off.** Privilege escalation (`sudo`, `su`, `doas`), filesystem creation and destruction (`mkfs*`, `mkswap`, `fdisk`, `parted`, `dd`, `shred`, `wipefs`), machine control (`shutdown`, `reboot`, `poweroff`, `halt`, `init`), a fork bomb, a write into a real device node (`> /dev/sda`; `2>/dev/null` is fine), and `rm` aimed at `/` or `~`. These are refused outright, and no config or file edit can re-enable them.

**Your blacklist file: remove or add freely.** `~/.yak/blacklist` for every project, plus `.yak/blacklist` in one repo (its lines win). This layer only *adds* refusals on top of the hardcoded ones. Ordinary `rm` is **not** refused by default: deleting files is normal work. Each line is a command word (`deploy` stops `deploy x` and `echo hi | deploy`), a whole segment (`git push --force origin main`), a glob (`mkfs*`), or `!pattern` to re-allow. The file is seeded with `rm` and `git push --force*` plus the syntax as comments; deleting it just resets it to those rules. It is a prompt, not a fence: the check is lexical, so a command a shell builds at runtime is not seen, and neither is a command an extension tool runs for itself.

A blacklist ask shows a prompt with the matched pattern highlighted. Type `a` to spare that pattern for the rest of the session. In a non-interactive run (`--json`) there is no terminal to answer at, so an ask fails closed and the call is denied. `--approval allow` answers every ask-list prompt with allow instead — meant for unattended children (a subagent driving a child `yak`): the hardcoded refusals above still deny whatever the flag says, and the interactive session never needs it.

## Tools

Eight built-ins: `update_plan`, `read`, `write`, `edit`, `bash`, `grep`, `glob`, `webfetch`, `remember`. The agent picks them itself; `--tools` narrows the set: plain names or `*` patterns (`read,grep`, `re*`) replace the default set wholesale, while `+name`/`-name` entries edit it in place (`-bash,-edit` runs with the write tools off; `+read` adds `read` back). A pattern must match a whole tool name, mixing the two forms is refused, and an unknown plain name is an error that lists what exists. A `ls` listing tool ships as the [`examples/extensions/ls`](https://github.com/imjiaoyuan/yak/tree/main/examples/extensions) extension — copy it into `~/.yak/extensions/` (or the project's `.yak/extensions/`) and it mounts read-tier like the built-ins.

Tool calls are timed: the whole call's clock runs in the spinner row while it executes (streamed output beyond the first lines folds into that row as a `+N lines` phase, so the clock stays visible through chatty tools), and `--json` carries `durationMs` on each `tool_end`. Calls in the read-only batch share one clock: every concurrent call reports the batch's span.

`update_plan` is the agent's own checklist for multi-step work: a list of steps, each `pending`, `in_progress` or `completed`, with at most one in progress. Marking a step done as it finishes keeps a long task from losing track of what is left; it touches nothing, so it never asks for approval.

The `read` tool pages through large files instead of loading them whole: `offset` and `limit` walk through them in 2000-line windows (50 KB per call, single lines capped at 2000 characters so a minified bundle cannot flood the context). Text returns bare, with a `[Showing lines a-b of N. Use offset=... to continue.]` note when more remains. Binary formats are refused with a hint at the right local tool: `pdftotext` for PDFs, `samtools` for BAM/CRAM, `duckdb` for Parquet/HDF5, `libreoffice --headless --convert-to csv` for old Office files. Images are read as vision attachments.

For JSON and JSONL, append `?q=<filter>` to the path and the file is queried instead of read whole: `data.json?q=.items[0].name`, `log.jsonl?q=.[] | select(.level == "err")`. The filter language is a practical jq subset — key paths (`.a.b`), iteration (`.[]`), indexing (`.[2]`), pipes (`|`), collection (`[...]`), `select()`, `keys`, `length`, `type` and a trailing `?` — evaluated in-process, no `jq` needed. A JSONL file streams one line at a time, so a multi-gigabyte log never loads whole. Results show up to 100 values (compact JSON, one per line) with an `offset=N` continuation note when more remain, files cap at 5 MB, and an unsupported filter fails loudly with the supported list rather than guessing.

`webfetch <url>` grabs a page and returns it as text (HTML stripped, http(s) only, proxies honoured) so the agent can read docs without a shell.

`edit` applies one or more exact-match replacements in one call; `write` creates or overwrites a file. Under context pressure an oversized tool result is cut to its head and tail with a note naming what was dropped: the full text stays in the session log, and the model can re-run the command or re-read the file when it needs the middle back.

`remember` saves one durable fact to `~/.yak/YAK.md` (`- [date] one line`): ask \"记住我喜欢简洁回复\" and the agent calls it. The line is deduped (a fact already noted, either way round, is not repeated) and injected into every future session's system prompt; a hand edit to the file works the same way. Project-scoped rules belong in an `AGENTS.md` in the repo, not here.

## Attachments

```bash
yak -a shot.png "what is wrong here?"      # attach a file
yak -a https://example.com/page "summarise this"
yak -a - "what is this?" < shot.png        # stdin as the attachment
```

Images, PDFs, wav/mp3 clips and plain text (.txt, .md, .csv, source files) are sent as native content blocks. Text becomes a document block on Anthropic models and an extra text part elsewhere. Anything the chosen model cannot accept is refused before the request leaves your machine.

In a session, ctrl+v pastes the clipboard image as a short `[paste #N image]` token (the temp-file path rides underneath and attaches on submit) (whatever image type the clipboard offers) a copied image *file* as its own path, and any local image path you type attaches itself. A clipboard that carries no image says what it does hold instead. Long conversations keep only the newest image attachments; older ones collapse into short text notes.

## Sessions

Every conversation is saved, so you can always come back to it:

```bash
yak -c "and in python?"                    # continue the newest session here
yak -r                                     # browse and resume past sessions
yak --session 01ABC... "..."               # pick an exact one (a short prefix works)
yak --no-session "..."                     # this run only, don't save it
yak --fork "..."                           # branch this session onto a new thread
yak export notes.md                        # write the newest session here as markdown
```

`-c` looks in the current directory first and falls back to the newest session anywhere, telling you which directory it used. `-r` opens one filterable list, newest first; typing filters across the preview and the id. `/resume` inside the session opens the same list. `/export [PATH]` writes the conversation you are in (tool calls and results included) as markdown, `yak-<id>.md` in the working directory by default (the same renderer backs `yak export`).

## Session tree

`/tree` opens the session as a tree: every turn of every branch, indented under its parent, the active branch marked. Picking a turn jumps there — the session replays that turn's branch back to the root, and your next message continues from it. Nothing is deleted: the turns after the jump point stay in the file as a sibling branch, and a later `/tree` can jump right back onto them. The export and log surfaces always show the active branch (the conversation as it stands, not the abandoned siblings).

The rewind is transcript-only: files the agent wrote stay written. To take the workspace back too, install the [`workspace_checkpoint.py`](https://github.com/imjiaoyuan/yak/blob/main/examples/extensions/workspace_checkpoint.py) extension — every turn starts from a shadow snapshot of the tree (no git required), and a jump folds the dropped turns' snapshots back into the working tree before your next message lands.

## Skills

Skills are `SKILL.md` folders, discovered from `~/.yak/skills`, `~/.agents/skills` and the nearest `.yak/skills`/`.agents/skills` walking up from where you are (later wins by name). A skill needs a `description`: that is what the model matches a task against. The agent lists them via `/help`, you run one with `/skill:<name>`, and it can pick them itself from the system prompt. A run gets the skill's own directory, so the `references/`, `scripts/` and assets a skill points at resolve wherever you started the session. Turn one off with `disable-model-invocation`, or all of them with `[agent] disabled_skills`.

## Prompt templates

Prompt templates turn a prompt you keep retyping into a slash command. Drop a `.md` file in `~/.yak/commands/` (or the nearest `.yak/commands/`: the project copy wins) and `/name` runs it: the body is the prompt, optional frontmatter can add a `system` prompt on top of the agent's own, and `$input` receives everything after the command name. Both are substituted, so `$input` works in the `system` line too. So `/review src/main.rs` runs your template on `src/main.rs` as one task.

## Providers and models

The easy path: run `yak`, type `/login`, pick a provider, paste your API key (hidden), pick the default model from the provider's live model list. That's it: only the model you picked is stored (the provider's full list is fetched live whenever `/model` runs), and the first provider's first model becomes your default, so a fresh install is ready to run. esc cancels at any step without writing anything. The catalog ships 42 providers, including Anthropic, OpenAI, DeepSeek, Google, Groq, Mistral, xAI, OpenRouter, and the local runtimes Ollama, LM Studio, llama.cpp and vLLM. `/logout` removes a provider and clears the default if it pointed there.

Two accounts on the same provider (say two OpenCode Go subscriptions) are two provider entries: run `/login` for the second one and accept the suggested `NAME-2`, then give it its own key. Models are then picked by the qualified `provider/model` id, and a bare model name served by both accounts is refused as ambiguous rather than guessed.

If you prefer to edit config by hand, add a block like this to `config.json`:

```json
{
  "providers": {
    "deepseek": {
      "kind": "openai-compat",
      "base_url": "https://api.deepseek.com",
      "api_key": "${DEEPSEEK_API_KEY}",
      "models": ["deepseek-chat", "deepseek-reasoner"]
    }
  }
}
```

`kind` is `openai-compat` or `anthropic`. `api_key` can hold the key itself or `${ENV_VAR}` to read it from the environment at request time: either works.

`/model` picks the model (live list, with your saved ones pinned on top of it) and its thinking depth, saved for future sessions; `/thinking` changes the depth alone. Both live in the `models` object of config.json. `-m` and `YAK_MODEL` override per run, and `--thinking` beats the stored depth. If the saved model no longer resolves, you get a warning and a fallback.

## Tuning the agent

Under the `"agent"` key of `config.json`:

```json
{
  "agent": {
    "max_request_bytes": 8000000,
    "tools": {"bash": "prompt"}
  }
}
```

`tools` maps a tool to `allow`, `deny` or `prompt`. `max_request_bytes` caps one request body in bytes (32MB by default): lower it when a gateway in front of the model refuses less than the provider documents, and the run refuses the oversized body locally, naming the attachments that filled it, instead of coming back as an opaque 413.

`cache_ttl` picks how long the provider should hold this conversation's prompt-cache entry: `5m` (the default, what every provider gives) or `1h`. Only the Anthropic Messages API takes a lifetime: the OpenAI-compatible wire caches automatically. The long entry is billed at a higher write rate, and it pays for itself as soon as one gap lapses a five-minute one: an approval prompt, a long test run or a coffee break otherwise leaves the next round re-writing the whole conversation instead of reading it back.

Compaction runs when the priced context passes `window - 16384` (the model's real context window minus a reserve) or `compact_at_tokens` (64k by default) when the window is unknown (a per-model `context_window` option in `config.json` records one). It keeps the most recent `keep_recent_tokens` (20k by default) and summarizes the dropped prefix. A request that comes back saying the prompt does not fit still compacts the conversation at once and retries, so the session does not walk into that wall again: in memory only: no file records it and nothing reports it.

Model traffic goes through the proxies in `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY` (and `NO_PROXY`) automatically.

## The --json interface

`--json` replaces the terminal UI with one JSON object per line (`text`, `reasoning`, `tool_start`, `tool_log`, `tool_end`, `turn_end` and a closing `result`) for a supervising process: an editor, a CI lane, or another agent driving a child `yak`. The task is the same task (pass one as an argument), approvals and diagnostics stay on stderr, and stdout is nothing but events. Sessions, usage accounting and persistence are identical to a normal run (a round's `turn_end` carries its `usage` (`input`, `output`, `cached`, `cached_write`; `input` counts the cached tokens in, so the two cache numbers are what a supervisor prices); the interactive session is what `--json` is *not*) it wants a task and exits.

## Plugins

Extensions are how you add things the core does not ship. Put a file in `~/.yak/extensions/` (or in the project's `.yak/extensions/`, which wins by name) then restart, or type `/reload`. There are two shapes, and the shape is chosen by the file itself.

### A script tool: a script with a header

Add a few comment lines at the top and any script becomes a tool. The host runs it per call, passes the arguments, and takes stdout as the result: Python, shell, R, whatever you have:

```python
#!/usr/bin/env python3
# --- yak-tool: wordcount
# description: count characters in a text
# args: text (string) the text
# arg-mode: argv
import sys
print(len(sys.argv[1]))
```

A tool with one declared argument gets it as a plain command-line argument: no JSON to parse.

### A resident extension: a program that stays running

Without a header, the file is started once per session and you talk to it in one-JSON-per-line over stdio:

```text
→ {"id":1,"type":"initialize","params":{"version":..,"cwd":..}}
← {"id":1,"result":{"tools":[..],"commands":[..],"events":[..]}}
→ {"id":2,"type":"call_tool","name":..,"args":{..}}    ← {"id":2,"result":..}
→ {"id":3,"type":"run_command","name":..,"args":".."}  ← {"id":3,"result":".."}
→ {"id":4,"type":"event","name":..,"params":{..}}      ← {"id":4,"result":{..}}
```

At startup the host asks what the extension offers: tools (with JSON Schema parameters), slash commands, and events it wants to hear about. After that it calls back when the model uses a tool, when you type a matching `/command`, and at turn and tool boundaries. The `tool_call` event is the useful one for gating: your extension can deny a call or rewrite its arguments.

Extension tools run freely like `bash`; a `[agent] tools` policy or the blacklist still gates them.

Anything an extension prints to stderr is a human channel: it lands in the diagnostics tail, and while a call is in flight it streams into that call's tool log line by line, so a long tool can report progress without polluting its own result. A resident extension can also stream **lane frames** on stdout while a call runs — id-less `{"type":"lane","lane":..,"text":..}` lines, each rendered as a status row rewritten in place, one per lane name, so parallel work (two subagents, one row each) reads at a glance; the rows live only on screen, never in the transcript, and `--json` carries them as `lane` events. Lane frames are an extension protocol feature: see [`extensions.md`](extensions.md#lane-frames).

A slow or broken extension prints a dim warning and mounts nothing; it never blocks the session. Tool calls time out after 120s (`extensions.tool_timeout` in config) unless the extension asks for its own deadline at `initialize` (an extension that runs a build or another agent needs that) and events after 5s. ctrl+c abandons a call and tells a busy extension `interrupt` so it can stop its own child processes. `extensions.disabled` skips one by file stem (or by a script tool's declared manifest name) and `/reload` restarts them all.

Two self-contained templates ship in `examples/extensions/`: `template.js` (a JavaScript runtime whose user section uses a familiar extension API (`registerTool` / `registerCommand` / `on("tool_call", ...)`) so most existing tool/command/hook extensions paste straight in; APIs that need the process, like UI, editors and hotkeys, raise with a clear message) and `template.py` (the same shape in Python). `template.rs` is the script-tool shape in Rust: single-file source with a manifest header, compiled by the host on first call (needs rustc installed; without it the tool call returns install instructions). Copy one into the extensions directory and edit its user section.

The full reference is [`extensions.md`](extensions.md) (manifest fields, every message and event, the `tool_call` gate, timeouts and config keys. `examples/extensions/` has runnable examples to copy: `wordcount` (a script tool), `template.rs` (a Rust script tool: the host compiles it with rustc, content-hash cached), `websearch` (a resident extension offering `web_search` plus a `/web` command) uses `BRAVE_API_KEY` when set, otherwise keyless DuckDuckGo/Wikipedia), `repeat_guard.py` (denies a `tool_call` loop), `fold_repeats.py` (folds repeated lines in a tool result), `mcp_bridge.py` (mounts MCP servers as `server__tool` tools from an `mcp.json` beside the script: stdio or streamable HTTP; this is the MCP support) and `subagent.py` (below).

Here is a whole resident extension: a tool that shells out to `deploy.sh`:

```python
#!/usr/bin/env python3
# ~/.yak/extensions/deploy
import json, sys

def reply(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    req = json.loads(line)
    if req.get("type") == "initialize":
        reply({"id": req["id"], "result": {"tools": [
            {"name": "deploy", "description": "Deploy the current tree",
             "parameters": {"type": "object", "properties": {}}}],
            "commands": [], "events": []}})
    elif req.get("type") == "call_tool":
        import subprocess
        out = subprocess.run(["deploy.sh"], capture_output=True, text=True)
        reply({"id": req["id"], "result": out.stdout or out.stderr})
    elif req.get("type") == "shutdown":
        break
```

### Delegating: a subagent extension

[`examples/extensions/subagent.py`](../examples/extensions/subagent.py) mounts a `subagent` tool that runs another `yak` in its own context window and returns only its conclusion:

```bash
cp examples/extensions/subagent.py ~/.yak/extensions/subagent && chmod +x ~/.yak/extensions/subagent
```

Agent definitions are markdown with frontmatter (`~/.yak/agents/scout.md`, or the project's `.yak/agents/scout.md` where the nearest wins), with `tools`, `model` and `thinking` optional; four ship in [`examples/agents/`](../examples/agents/). The tool takes `task` (+ `agent`), a parallel `tasks` batch, or a `chain` where each step gets the previous answer, and the child's tool calls show up in your session as it works (each child owns one rewritten status row while it runs, so a parallel batch reads as N ticking lanes). The child is a plain `yak --json` process: its own tools, its own system prompt, its own budget (the extension asks the host for a longer deadline), stopped if you press ctrl+c and unable to spawn further subagents. Copy the file or don't; nothing in the core knows subagents exist.

A subagent whose definition names a mutating tool (`write`, `edit`, `bash`) runs in a **git worktree** when your cwd is a git repo: its changes are committed to a `yak/subagent-*` branch and merged back when it finishes. Parallel writers merge one at a time; a conflicting merge is aborted clean and reported with the branch name left for you to resolve — nothing a writer did is dropped. `YAK_SUBAGENT_WORKTREES=0` runs writers in place instead.

## Semantics

Models are named `provider/model`. Short aliases can be mapped in the `aliases` object of config.json.

Rendering is on only for a real terminal: answers stream as markdown with a left margin, and piped output is the raw text. Reasoning is never printed: a dim `thinking ... end` line just marks that it happened.

Approval tiers are read, write and exec. Yolo mode (the default) auto-approves everything; the hardcoded refusals (privilege escalation, filesystem/machine destruction, a fork bomb, a write into a device node, `rm` at `/` or `~`) apply in either mode, and your blacklist file only adds to them. Ask mode prompts for writes and exec-tier calls with `y/n/a`, where `a` allows that tool for the rest of the session. Session ids are ULIDs.
