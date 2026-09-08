# llm

A single-binary, terminal-first coding agent in Rust, the reference-shaped. One executable is the whole loop: bare `llm` opens an interactive agent session, `llm "task"` runs the agent once with tools, and a thread-file conversation store keeps every session resumable. Everything is synchronous, the whole dependency set is four crates, and all state lives under one user directory.

## Install

Linux and macOS download the prebuilt binary, verify its sha256 and install it with:

```bash
curl -fsSL https://jiaoyuan.org/llm/install.sh | sh
```

It lands in `~/.local/bin` as a plain user install, no root needed, and the script adds that
directory to your `PATH` when it is not there yet.

Windows does the same from PowerShell:

```powershell
irm https://jiaoyuan.org/llm/install.ps1 | iex
```

It lands in `%USERPROFILE%\.local\bin`, no admin needed, and the script appends that directory
to the user `Path`.

Both scripts are also the updater: run the same line again and it checks the latest GitHub
release, compares versions with the installed binary, prints `updating 0.1.0 -> 0.1.2` when it
moves and leaves an unchanged version alone (`LLM_FORCE=1` reinstalls anyway). They honor
`LLM_VERSION` (pin a release tag instead of latest), `LLM_REPO` (install from a fork) and
`LLM_INSTALL_DIR` (a different install directory). On Linux the static musl build is used, so the
same binary runs on any distribution; prebuilt targets today are x86_64 and aarch64 Linux, x86_64
and aarch64 macOS, and x86_64 Windows. Building from source works the same everywhere:

```bash
git clone https://github.com/imjiaoyuan/llm
cd llm
cargo build --release
```

You need a Rust toolchain (install one with [rustup](https://rustup.rs)). CI runs the same tests on
Linux, macOS and Windows. The binary lands in:

- Linux / macOS: `target/release/llm`
- Windows: `target\release\llm.exe`

Put it on your `PATH` (e.g. `cp target/release/llm ~/.local/bin/` on Unix, or add the
`target\release` directory to `Path` on Windows). State lives under `~/.llm` (Linux/macOS) or
`%USERPROFILE%\.llm` (Windows); set `LLM_USER_PATH` to relocate it, handy for trying the tool out
without touching the real user directory. Requests to OpenCode's Go/Zen gateway carry a
per-conversation `x-opencode-session` id; set `LLM_SESSION_ID` to pin one id across several `llm`
invocations of the same conversation.

### Platform notes

Linux, macOS and Windows all use the native platform implementation for the same terminal
experience: raw-mode line editing, arrow-key pickers, hidden key input, and steering while an
agent task runs. Shell commands use `sh` on Linux/macOS and PowerShell on Windows by default; set
`LLM_SHELL` to override the program (for example `cmd`, `powershell`, `pwsh`, `bash`, `zsh`, or
any other shell on `PATH`). The interactive features still require a real terminal; with piped
stdin the CLI reads plain input as before.

## Config

### Set an API key (interactive)

The quickest way to get going is the interactive provider wizard — no need to hand-edit any file:

```bash
llm login                     # the wizard
llm login deepseek sk-xxx     # direct form: catalog name + key, no prompts
llm login deepseek            # keyless catalog form: ${DEEPSEEK_API_KEY} backs it
llm login my-proxy --base-url https://proxy.internal/v1 --kind openai-compat
```

The wizard opens a picker over the built-in provider catalog (Anthropic, OpenAI, DeepSeek, Google,
Groq, Ollama, ...), asks for your API key with hidden input, and writes the provider into
`config.json`; the first provider's first model automatically becomes the shared default, so a
fresh install is ready to run. Every configuration command is dual-form like this: bare on a
terminal opens the interactive flow, full arguments run directly. `llm logout` (or
`llm logout deepseek`) removes a provider and clears the default if it pointed there. You can also
inspect or replace a single key:

```bash
llm models key               # providers, with key status
llm models key deepseek      # print the key, ${VARS} expanded (use with care)
llm models key deepseek sk-2 # set it directly (or --set for hidden input)
```

Or skip the interactive commands entirely: put the provider block in `config.json` with an
environment-variable key (see below).

Data lives under the user directory, `~/.llm` by default: `threads/` holds every conversation as JSONL thread files, `config.json` every setting (providers with their API keys, the `models` family, the `agent` section, the plugin tables `tools`/`mcpServers`), and `commands/` the custom subcommands.

Providers are registered in `config.json` in that directory, alongside any other settings:

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

`kind` is one of `openai-compat`, `anthropic`, `image`, `tts`, and `api_key` expands `${ENV_VAR}`
references at request time, so literal secrets and environment indirection live in the same field.

`llm models` picks the default model and its thinking depth — one default shared by every mode
(bare `llm` and the agent REPL both start on it). Bare on a terminal it walks you through
provider → model → thinking depth; the same subcommands take direct arguments:

```bash
llm models                      # the wizard (or: llm models set)
llm models set zai/glm-5.2 --thinking high
llm models set deepseek/deepseek-chat           # bare model names and aliases resolve too
llm models get                  # the current default
llm models unset
```

It is all one `models` object in config.json (distinct from the nested `agent.models`
context-window table): `default` is the shared model every mode starts on, `thinking` the
reasoning depth riding it, and `options` per-model default options
(`llm models options set MODEL KEY VALUE`). `-m` and `LLM_MODEL` stay per-invocation; the stored
`thinking` loses only to `--thinking`. Legacy per-mode entries (`prompt`/`agent` keys from
older versions) migrate on first read: the prompt entry wins, and the next `models set` collapses
the file onto the new shape. When the stored default no longer resolves (its provider was
removed), commands warn and fall back.

A built-in catalog of 30+ providers (Anthropic, OpenAI, DeepSeek, Google, Groq, Mistral, Cerebras, NVIDIA, Hugging Face, Together, Baseten, Fireworks, xAI, OpenRouter, Moonshot, Kimi, Z.ai, Qwen token plans, Xiaomi MiMo, MiniMax, Vercel AI Gateway, SiliconFlow, Zhipu, and the local runtimes Ollama, LM Studio, llama.cpp, vLLM) carries canonical endpoints and env var names, and `llm login` builds its wizard picker straight from it:

```bash
llm login                                  # provider wizard (catalog + templates)
llm models key deepseek sk-xxx             # set the provider's key directly
llm models set deepseek/deepseek-chat
```

## Plugins

The quickest plugin is a drop-in tool: any executable with a manifest header, dropped into `~/.llm/tools/` or the project's `.llm/tools/` (the project copy wins by name). One file is one tool, any language — the header declares it, the file itself runs, receiving the tool arguments as one JSON line on stdin and answering on stdout:

```bash
#!/usr/bin/env python3
# --- llm-tool: wordcount ---
# description: count characters in the text
# args: text (string) the text to count
import json, sys
args = json.loads(sys.stdin.readline())
print(len(args["text"]))
```

`args:` lines build the input schema (string, int, float, bool); the tool mounts automatically at startup, `/tools` in the agent REPL lists everything (drop-ins, config-table tools and MCP servers together). A `name.json` manifest beside a script points at an external command instead — same fields as the config `tools` table, as a file. Everything runs at the exec approval tier, so a plugin asks before it runs.

The extension surfaces follow one rule: files declare, processes compute, the binary itself never
recompiles. Aliases, commands and skills were always files; what follows adds the three
code-bearing surfaces, all declared in config.json or dropped in as files.

When you want a tool that needs no shebang of its own, declare it in config instead — same spawn
model (one process per call, arguments as a JSON line on stdin, stdout is the result, nonzero exit
reports as an error). Say you keep tickets in a file and want the agent to look them up:

```python
# ~/.llm/scripts/ticket.py
import json, sys
args = json.loads(sys.stdin.readline())
print(f"ticket {args['id']}: see ~/.llm/notes")
```

```json
"tools": {
  "ticket": {
    "description": "Look up a ticket by id",
    "command": "python3",
    "args": ["~/.llm/scripts/ticket.py"],
    "schema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
    "timeout": 30
  }
}
```

That whole entry is the plugin. `description` tells the model what the tool does, `schema` is a
JSON Schema for the arguments (default `{"type": "object"}`), `timeout` bounds each call in seconds
(default 60), and `${ENV_VAR}` expands in `command` and `args`. Both kinds — drop-in and declared —
mount under their own name next to the built-ins, ask approval like any exec-tier tool
(`Allow? [Y/n/a]`, remembered per session with `a`), obey `[agent] tools` policies, and can be
picked with `--tools ticket,bash`.
Sub-agents default to the built-in tool set (`read,grep,glob,ls`); name plugin tools in an agent
definition's `tools:` list to hand them through.

For the wider ecosystem the same config holds MCP servers, the standard tool protocol of 2026. Any
stdio MCP server mounts its tools as `mcp__<server>__<tool>`, which usually means one line of
config and nothing to install:

```json
"mcpServers": {
  "fetch":  {"command": "uvx", "args": ["mcp-server-fetch"]},
  "github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
             "env": {"GITHUB_TOKEN": "${GITHUB_TOKEN}"}}
}
```

Servers connect in parallel when a session starts (a slow or broken one warns dimly and mounts
nothing, it never blocks), children die with the session, and `env` entries expand `${ENV_VAR}`
over the inherited environment. The agent REPL's `/mcp` lists every server with its health and
tool count. On Windows `npx` is a `.cmd` shim, so spell it
`{"command": "cmd", "args": ["/c", "npx", ...]}` until a PATHEXT probe lands.

Writing a server for yourself needs no SDK, the protocol is one JSON-RPC message per line over
stdio:

```python
#!/usr/bin/env python3
# a complete MCP server: initialize, tools/list, tools/call
import json, sys
for line in sys.stdin:
    req = json.loads(line)
    rid = req.get("id")
    if req.get("method") == "initialize":
        result = {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "mine"}}
    elif req.get("method") == "tools/list":
        result = {"tools": [{"name": "hello", "description": "Say hello",
                             "inputSchema": {"type": "object", "properties": {"who": {"type": "string"}}}}]}
    elif req.get("method") == "tools/call":
        result = {"content": [{"type": "text", "text": "hello " + req["params"]["arguments"].get("who", "")}]}
    else:
        continue
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "result": result}) + "\n")
    sys.stdout.flush()
```

The official TypeScript and Python SDKs (`@modelcontextprotocol/sdk`, the `mcp` package with
FastMCP) wrap all of this if you would rather not handle the loop yourself.

Finally, `~/.llm/commands/*.md` turns a prompt you keep retyping into a subcommand, with the
nearest `.llm/commands/` winning for project-specific variants. The body is the prompt,
frontmatter may pin `model` and `system`, and `$input` receives everything after the name, so
`llm review src/main.rs` runs the template with `src/main.rs` as input while every prompt flag
(`-m`, `-o`, `-p`, `-a`, ...) still applies:

```markdown
---
model: zai/glm-5.2
system: You are a meticulous code reviewer.
---
Review $input for correctness bugs and suggest minimal fixes.
```

Inside the agent REPL the same file answers to `/review src/main.rs`, submitted as one task in the
agent. Names that collide with built-in commands keep the built-in, and a
word that merely looks like a typo still gets the did-you-mean guard before anything is looked up.

## Usage

The top-level help:

```
Access Large Language Models from the command-line

Usage:
  llm [flags] [PROMPT]

Bare `llm` opens an interactive agent session; `llm "task"` runs the
agent once with tools.

Available commands:
  models     Pick the default model and its thinking level
  login      Add a provider
  logout     Remove a provider

Use "llm [command] --help" for more information about a command.

Flags:
  -h, --help      Show this message and exit
  -v, --version   Show the version number
```

```
Access Large Language Models from the command-line

Usage:
  llm [flags] [PROMPT]

Bare `llm` opens an interactive agent session; `llm "task"` runs the
agent once with tools.

Available commands:
  models     Pick the default model and its thinking level
  login      Add a provider
  logout     Remove a provider

Use "llm [command] --help" for more information about a command.

Flags:
  -h, --help      Show this message and exit
  -v, --version   Show the version number
```

### Configuration

```
Add a provider (bare: the interactive wizard)

Usage: llm login [NAME [KEY]] [OPTIONS] NAME

Options:
      --base-url URL    Custom endpoint for a non-catalog provider
      --kind KIND       Wire kind: openai-compat or anthropic
  -h, --help            Show this message and exit
```

```
Remove a provider (bare: the picker)

Usage: llm logout [NAME] [OPTIONS] 

Options:
  -h, --help            Show this message and exit
```

```
Manage the default model and per-model options

Commands:
  set       Set the default model (bare: interactive wizard)
  get       Show the default
  unset     Clear the default
  key       Show or set a provider's API key
  list      List available models
  options   Per-model default options

Providers are added and removed with `llm login` and `llm logout`.

Usage: llm models [COMMAND] [ARGS]... [OPTIONS] 

Options:
  -h, --help            Show this message and exit
```


## Examples

`llm` is the agent, the reference-shaped: bare `llm` on a terminal opens the interactive REPL, `llm "fix the failing test"` runs the task once with tools and exits, and piped stdin is the task text (`git diff | llm "review this change" > review.md` — pipes stay plain, never ANSI codes). The loop can read, edit, search and run commands, pausing for approval on writes, commands and reads outside the working directory unless you pass `--yolo` or set `approval_mode = "yolo"` in config; file edits and writes show a unified-diff preview (context, `-` and `+` rows, capped) right above the approval question, so you decide with the actual change in view. The `read` tool streams text files a window at a time instead of loading them: each answer opens with a metadata header naming the file, its size and the shown range, `offset` and `limit` page through 500-line windows (50KB byte cap, single lines capped at 2000 characters so a minified bundle cannot eat the context), and binary formats are refused with a hint at the right local tooling rather than garbage bytes. `webfetch <url>` fetches web pages and returns plain text (HTML stripped, 256KB cap, http(s) only, proxies inherited from the environment) so the agent can consult docs and articles without a shell. The REPL carries slash commands (`/clear`, `/skills`, `/memory`, `/compact`, `/status`, `/init`, `/tools`, ...), shell passthrough via `!cmd`, and ctrl-c or esc to interrupt a running task (esc takes effect within a tenth of a second, even mid-reasoning). While a task runs you can keep typing; the queued lines are delivered to the model at the next tool boundary and any that outlive the task run as the next prompt. Branching is cheap: `llm --fork` continues the most recent session on a fresh branch (the original keeps its own history from that point), and `--fork --session ID` branches a specific one.

Pick a model per call with `-m deepseek/deepseek-chat`. Model options ride along as `-o temperature=0.2 -o top_p=0.9`. Attach files or URLs with `-a shot.png`, force a mimetype with `--at image.png image/png`, and add a system prompt with `-s`; piped stdin can feed an attachment instead of the prompt, so `llm -a - "what is this" < shot.png` sends the image and the words together, images, PDFs, wav/mp3 clips and plain-text files (.txt, .md, .csv, source code) ride the same request as native content blocks: text attaches as a document block on anthropic models and as an extra text part elsewhere, and anything a model family cannot accept is refused before a request leaves the machine with the supported list named in the error.

Conversations continue with `llm -c "and in python?"` or `llm --session 01ABC... "..."` (a short unambiguous prefix like `01m13d` works too), and every session lands in `~/.llm/threads/` unless you pass `--no-session`. `llm -r` is the way back in: one filterable list of recent conversations (typing filters across preview and id, fuzzy-filter style); enter opens the transcript and then offers to jump straight into the conversation (answering Y resumes it in the agent session). Inside the REPL, slash commands fill the same roles; when a prompt is worth keeping, drop it in `~/.llm/commands/review.md`: the body is the prompt, `$input` receives whatever follows the command name, and unknown `/review` runs it as a template. Frontmatter can pin `system:`.

Multimodal input reaches every mode the same way: `-a screenshot.png` rides the task's first message, and inside a session ctrl+v pastes the clipboard image as a temp-file path you can see and edit, while any local image path typed into a message attaches itself automatically (a dim note confirms each one). Limit the toolbox with `--tools read,grep`, the turn budget with `--max-turns`, and swap the system prompt with `-s` or `--append-system-prompt`. Sessions persist, so `llm -c "now run it"` picks up where the last one ended, `--no-session` opts out.

Binary formats stay outside the binary on purpose: the agent's read tool answers with a hint at local tooling instead of garbage bytes — `pdftotext` for PDFs, `samtools` for BAM and CRAM, `duckdb` for Parquet and HDF5, `libreoffice --headless --convert-to csv` for the legacy Office formats. Convert first, then feed the `.txt` to any command.

Reasoning effort is a first-class dial on every entry point: `llm --thinking high` maps to `reasoning_effort` on OpenAI-compatible endpoints and a thinking budget on Anthropic ones; the default depth lives next to the default model in config.json (`llm models set MODEL --thinking LEVEL`; `off` omits the parameter entirely).

Skills and memory live under the user directory. Skills are SKILL.md folders discovered from `~/.llm/skills`, `~/.agents/skills` and the nearest `.llm/skills`/`.agents/skills` walking up from the working directory (later wins by name, so packs installed by other tools keep working); clone or copy a folder into one of those and the agent picks it up, listing them via `/skills` and running one with `/skill:<name>`, and the model can pick skills itself from the system-prompt list (disabled per skill with `disable_model_invocation` or globally via `[agent] disabled_skills`). Global memory is a hand-editable `~/.llm/LLM.md` injected into the agent system prompt: `/memory add` appends a line by hand, and the agent reads it on the next session. (The agent `remember` tool was removed on purpose — memory is manual, not agent-written.) Model traffic goes through HTTP proxies from `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY` (and `NO_PROXY`) automatically

Agent behavior is tuned under the `"agent"` key of `config.json`:

```json
{
  "agent": {
    "approval_mode": "always-ask",
    "context_window": 128000,
    "tools": {"bash": "prompt"}
  }
}
```

`approval_mode` is `always-ask` (default) or `yolo`; `context_window` is where compaction
kicks in; `tools` maps each tool to `allow`, `deny` or `prompt`.

## Outputs

Every prompt and agent session is written to `~/.llm/threads/` as JSONL thread files: reasoning parts are stored next to the responses, tool calls and results ride along in agent sessions, and turns carry their model, options and token usage. A `logs-off` marker file in the user directory turns prompt logging off entirely.

## Semantics

Model ids are `provider/model` everywhere, with the `aliases` object in config.json mapping short names on top; `models set` and `models options` manage the mapping, and the `aliases` object is hand-edited config. Terminal rendering is enabled only on a TTY: prompts and agent answers stream as markdown with a two-column margin, blank lines are dropped except around headings and code blocks, and piped output is the raw text. Reasoning is never dumped to the screen in any mode, one gray `thinking ... end` line records that it happened and `-R` hides even that. Approval tiers split agent tools into read, write and exec: reads run freely in ask mode, writes and exec-tier calls prompt with y/n/a (`a` allows the tool for the rest of the session). Session ids are ULIDs, `-c` continues the newest session and `--cid` picks an exact one. Long lists in any picker scroll inside a bounded window instead of flooding the screen.

## Development

```bash
cargo build            # debug build
cargo build --release
cargo test             # inline #[cfg(test)] modules across the tree
LLM_USER_PATH=/tmp/x cargo run -- "smoke test prompt"
```

The source is organized by role: `src/commands/` holds one file per subcommand (flags, help, wiring), `src/core/` the shared kernel (config, the sqlite stores and read model, http, rendering), `src/providers/` one adapter per protocol plus the shared message model and the provider catalog, and the domains live top-level as `agent/` and `term/` (line editing, pickers, the spinner, terminal size). Tests are inline per module; run one with `cargo test <name>`.

## License

MIT
