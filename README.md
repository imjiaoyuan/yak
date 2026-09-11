# llm

A coding agent that lives in your terminal: `llm "fix the failing test"` runs one task with tools —
reading, editing, searching, running commands — and bare `llm` opens an interactive session.
Sessions are stored as thread files and can be resumed anytime.

## Install

Linux and macOS download the prebuilt binary, verify its sha256 and install it with:

```bash
curl -fsSL https://jiaoyuan.org/llm/install.sh | sh
```

It lands in `~/.local/bin`, no root needed, and adds that directory to your `PATH` when missing.
Windows does the same from PowerShell:

```powershell
irm https://jiaoyuan.org/llm/install.ps1 | iex
```

It lands in `%USERPROFILE%\.local\bin`, no admin needed, and appends that directory to the user
`Path`. Re-running either line is the updater: it checks the latest GitHub release, prints
`updating 0.1.2 -> 0.1.5` when it moves and leaves an unchanged version alone. `LLM_FORCE=1`
reinstalls anyway; `LLM_VERSION` pins a release tag, `LLM_REPO` installs from a fork and
`LLM_INSTALL_DIR` picks a different directory. On Linux the static musl build is used, so the same
binary runs on any distribution; prebuilt targets today are x86_64 and aarch64 Linux, x86_64 and
aarch64 macOS, and x86_64 Windows.

Building from source works the same everywhere:

```bash
git clone https://github.com/imjiaoyuan/llm
cd llm
cargo build --release
```

You need a Rust toolchain (install one with [rustup](https://rustup.rs)). The binary lands in
`target/release/llm` (`target\release\llm.exe` on Windows) — put it on your `PATH`. State lives
under `~/.llm` (`%USERPROFILE%\.llm` on Windows); set `LLM_USER_PATH` to relocate it. Requests to
OpenCode's Go/Zen gateway carry a per-conversation `x-opencode-session` id; set `LLM_SESSION_ID` to
pin one id across several `llm` invocations of the same conversation.

### Platform notes

Linux, macOS and Windows all use the native platform implementation for the same terminal
experience: raw-mode line editing, arrow-key pickers, hidden key input, and steering while an agent
task runs. Shell commands use `sh` on Linux/macOS and PowerShell on Windows by default; set
`LLM_SHELL` to override the program (for example `cmd`, `powershell`, `pwsh`, `bash`, `zsh`, or any
other shell on `PATH`). The interactive features need a real terminal; with piped stdin the CLI
reads plain input.

## Usage

Bare `llm` on a terminal opens the interactive REPL, `llm "fix the failing test"` runs the task
once with tools and exits, and piped stdin is the task text
(`git diff | llm "review this change" > review.md` — pipes stay plain, never ANSI codes). The loop
reads, edits, searches and runs commands; it runs in **yolo mode by default** — everything
auto-approved except a short list of destructive commands (`rm`, `sudo`, `dd`, `mkfs`, `shutdown`,
…) that keeps its one-shot `Allow? [Y/n/a]` prompt. Pass `--approval-mode ask` or set
`approval_mode = "always-ask"` to confirm every state change: file writes, deletions, `git push`
and unrecognized or non-read-only commands prompt, while reads inside the working directory and
read-only commands (`ls`, `git status`, `rg`, `cargo test`, ...) always run free; `/yolo` toggles
the mode for the session. File edits and writes show a unified-diff preview right above the
approval question.

The `read` tool streams text files a window at a time instead of loading them: each answer opens
with a metadata header naming the file, its size and the shown range, `offset` and `limit` page
through 2000-line windows (50KB byte cap, single lines capped at 2000 characters so a minified
bundle cannot eat the context), `paths` batches up to five files into one call, and binary formats
are refused with a hint at the right local tooling rather than garbage bytes — `pdftotext` for
PDFs, `samtools` for BAM/CRAM, `duckdb` for Parquet/HDF5, `libreoffice --headless --convert-to csv`
for legacy Office formats. `webfetch <url>` fetches web pages and returns plain text (HTML
stripped, 256KB cap, http(s) only, proxies inherited from the environment) so the agent can consult
docs without a shell.

The REPL carries slash commands (`/model`, `/thinking`, `/login`, `/logout`, `/clear`, `/resume`,
`/tree`, `/status`, `/reload`, ... — `/help` lists them one per line, skills included as
`/skill:<name>`) and shell passthrough via `!cmd`, with bash-style tab completion for command names
and paths. Ctrl-c or esc interrupts a running task; while it runs, typed lines are queued as
steering and delivered at the next tool boundary, and leftovers become new tasks.

Pick a model per call with `-m deepseek/deepseek-chat`. Model options ride along as
`-o temperature=0.2 -o top_p=0.9`, and `--thinking high` maps to `reasoning_effort` on
OpenAI-compatible endpoints and a thinking budget on Anthropic ones (`off` omits the parameter
entirely). Add a system prompt with `-s` or `--append-system-prompt`, limit the toolbox with
`--tools read,grep` and the turn budget with `--max-turns`.

Attach files or URLs with `-a shot.png` (`--at image.png image/png` forces a mimetype); images,
PDFs, wav/mp3 clips and plain-text files (.txt, .md, .csv, source code) ride the request as native
content blocks — text attaches as a document block on Anthropic models and as an extra text part
elsewhere, and anything the model family cannot accept is refused before a request leaves the
machine. Piped stdin can feed an attachment instead of the prompt, so
`llm -a - "what is this" < shot.png` sends the image and the words together. Inside a session
ctrl+v pastes the clipboard image as a temp-file path you can see and edit, and any local image path
typed into a message attaches itself automatically. Long conversations keep only the most recent
image attachments in context — older ones become short text notes.

Sessions persist: `llm -c "and in python?"` continues the newest conversation of this directory,
`llm --session 01ABC...` (`--cid`, or a short unambiguous prefix like `01m13d`) picks an exact
thread, and `--no-session` opts out. `llm -r` is the way back in: one filterable list of this
directory's conversations, newest first (typing filters across preview and id, fuzzy-filter style); enter
opens the transcript and offers to resume it. A directory with no history of its own falls back to
listing every directory, the directory tagged on each row. `/resume` inside the REPL opens the same
picker. `--fork` branches the loaded session onto a new thread id sharing its turns so far.

Skills and memory live under the user directory. Skills are SKILL.md folders discovered from
`~/.llm/skills`, `~/.agents/skills` and the nearest `.llm/skills`/`.agents/skills` walking up from
the working directory (later wins by name); the agent lists them via `/help`, runs one with
`/skill:<name>`, and can pick them itself from the system-prompt list (disable per skill with
`disable_model_invocation` or globally via `[agent] disabled_skills`). Global memory is a
hand-edited `~/.llm/LLM.md` injected into the agent system prompt, read on the next session — there
is no agent-written memory. Model traffic goes through the HTTP proxies in
`ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY` (and `NO_PROXY`) automatically.

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

`approval_mode` is `yolo` (default) or `always-ask`; `context_window` is where compaction kicks in;
`tools` maps each tool to `allow`, `deny` or `prompt`. Prompt templates turn a prompt you keep
retyping into a slash command: drop a `.md` file in `~/.llm/commands/` (or the nearest
`.llm/commands/`, project wins) and `/name` runs it — the body is the prompt, frontmatter may pin
`system`, and `$input` receives everything after the command name, so `/review src/main.rs` runs the
template with `src/main.rs` as input, submitted as one task in the agent session.

### Help

```
Access Large Language Models from the command-line

Usage:
  llm [flags] [PROMPT]

Bare `llm` opens an interactive agent session; `llm "task"` runs the
agent once with tools.

Available commands:
  install    Install a git package (also: remove, list)

Flags:
  -h, --help      Show this message and exit
  -v, --version   Show the version number
```

### Packages

Packages bundle extensions, skills and prompt templates into one git repository and share it as a
unit: `llm install git:github.com/user/repo[@ref]` clones into `~/.llm/pkg/<name>` (`-l` installs
project-local into `.llm/pkg/`, project winning over user), and its `extensions/`, `skills/` and
`commands/` directories mount into the normal discovery walks. Re-running `install` refreshes a
clone (`git fetch` + reset); a pinned `@ref` clone moves only via `install repo@new-ref`. `llm list`
shows what each package carries, `llm remove NAME` deletes it. There is no npm lane — git only.
Review any third-party package before installing: extensions run with full system access.

```bash
llm install git:github.com/user/llm-deploy    # → ~/.llm/pkg/llm-deploy
llm install git:github.com/user/llm-deploy@v2 # pinned
llm list
llm remove llm-deploy
```

```
Clone a package into the pkg directory (re-run to refresh)

Usage: llm install git:github.com/user/repo[@ref] [OPTIONS] SOURCE

Options:
  -l, --local           Install project-local (.llm/pkg/ instead of ~/.llm/pkg/)
  -h, --help            Show this message and exit
```

```
Delete an installed package

Usage: llm remove NAME [OPTIONS] 

Options:
  -h, --help            Show this message and exit
```

```
List installed packages and what they carry

Usage: llm list [OPTIONS] 

Options:
  -h, --help            Show this message and exit
```

## Providers and models

Run `llm` and use `/login`: the wizard opens a picker over the built-in provider catalog (38
providers — Anthropic, OpenAI, DeepSeek, Google, Groq, Mistral, Cerebras, NVIDIA, Hugging Face,
Together, Baseten, Fireworks, xAI, OpenRouter, Moonshot, Kimi, Z.ai, Qwen token plans, Xiaomi MiMo,
MiniMax, Vercel AI Gateway, SiliconFlow, Zhipu, and the local runtimes Ollama, LM Studio, llama.cpp
and vLLM), asks for your API key with hidden input, and writes the provider into `config.json`; the
first provider's first model automatically becomes the shared default, so a fresh install is ready
to run. `/logout` removes a provider and clears the default if it pointed there.

Or skip the wizard and put the provider block in `config.json` directly:

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

`kind` is `openai-compat` or `anthropic`, and `api_key` expands `${ENV_VAR}` references at request
time, so literal secrets and environment indirection live in the same field.

Data lives under the user directory, `~/.llm` by default: `threads/` holds every conversation as
JSONL thread files, `config.json` every setting (providers with their API keys, the `models` family,
the `agent` section, the `extensions` table), `extensions/` the code-bearing plugins, `pkg/` the
installed packages, and `commands/` the prompt templates.

`/model` picks the default model and its thinking depth — one default the REPL always starts on.
The picker walks provider → model → thinking depth and saves the choice; `/thinking` adjusts the
depth alone. It is all one `models` object in config.json: `default` is the startup model,
`thinking` the reasoning depth riding it, and `options` per-model default options (hand-edited).
`-m` and `LLM_MODEL` stay per-invocation; the stored `thinking` loses only to `--thinking`. Legacy
per-mode entries (`prompt`/`agent` keys from older versions) migrate on first read; when the stored
default no longer resolves, runs warn and fall back.

## Plugins

Extensions are the plugin system: anything the core skips, you build yourself as an extension in
`~/.llm/extensions/` or the project's `.llm/extensions/` (the project copy wins by name). One
directory, one mental model: drop an executable in, restart or `/reload`. Three forms, from
thinnest up:

**Script tools — any language, three comment lines.** A plain script with a manifest header is a
tool; the host spawns it per call, feeds the arguments (a single declared argument rides as plain
`argv[1]`, no JSON), collects stdout as the result, and owns timeout, size cap and approval. Python,
shell, R, anything with an interpreter:

```python
#!/usr/bin/env python3
# --- llm-tool: wordcount
# description: count characters in a text
# args: text (string) the text
# arg-mode: argv
import sys
print(len(sys.argv[1]))
```

**Resident extensions — tools, commands and event hooks.** A file without a manifest header is
spawned once per session and speaks one JSON message per line over stdio:

```text
→ {"id":1,"type":"initialize","params":{"version":..,"cwd":..}}
← {"id":1,"result":{"tools":[..],"commands":[..],"events":[..]}}
→ {"id":2,"type":"call_tool","name":..,"args":{..}}    ← {"id":2,"result":..}
→ {"id":3,"type":"run_command","name":..,"args":".."}  ← {"id":3,"result":".."}
→ {"id":4,"type":"event","name":..,"params":{..}}      ← {"id":4,"result":{..}}
```

The `initialize` handshake advertises the extension's tools (JSON Schema parameters), slash commands
and event subscriptions; the host then routes `call_tool` when the model invokes one, `run_command`
when the user types a matching `/command`, and `event` at turn and tool boundaries (`tool_call` may
deny or rewrite a call — permission gates and path protection live here). Extension tools are
exec-tier: the approval matrix treats them like `bash` — under the default yolo mode they run free,
and in ask mode every call prompts (`Allow? [Y/n/a]`, remembered per session with `a`).
`[agent] tools` policies still win in either mode, and `--tools` picks a subset.
`extensions.disabled` in config.json skips one by name, `/reload` respawns everything, and a slow or
broken extension warns dimly and mounts nothing — it never blocks a session. Tool calls time out
after 120s (config `extensions.tool_timeout`), events after 5s.

Two self-contained templates ship in `examples/extensions/`: `template.js` (a JavaScript runtime
whose user section uses a familiar extension API — `registerTool` / `registerCommand` /
`on("tool_call", ...)` — so most existing tool/command/hook extensions paste straight in; APIs that
need the process, like UI, editors and hotkeys, raise with a clear message) and `template.py` (the
same shape in Python). Copy one into the extensions directory and edit its user section.

The full reference is [`docs/extensions.md`](docs/extensions.md) — manifest fields, every protocol
message and event, the `tool_call` gate, timeouts and config keys — and `examples/extensions/` also
carries three runnable examples: `wordcount` (a script tool), `websearch` (a resident extension
mounting `web_search` + `web_fetch` tools and a `/web` command — Brave's Search API when
`BRAVE_API_KEY` is set, keyless DuckDuckGo/Wikipedia fallback otherwise), and `todo`, ready to copy.

A minimal resident extension, complete in thirty lines of Python:

```python
#!/usr/bin/env python3
# ~/.llm/extensions/deploy
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

## Threads

Every prompt and agent session is written to `~/.llm/threads/` as a JSONL thread file: reasoning
parts are stored next to the responses, tool calls and results ride along in agent sessions, and
turns carry their model, options and token usage. Sessions persist unless `--no-session` opts out;
there is no global logging switch.

## Semantics

Model ids are `provider/model` everywhere, with the `aliases` object in config.json mapping short
names on top (hand-edited config). Terminal rendering is enabled only on a TTY: prompts and agent
answers stream as markdown with a two-column margin, blank lines are dropped except around headings
and code blocks, and piped output is the raw text. Reasoning is never dumped to the screen in any
mode; one gray `thinking ... end` line records that it happened and `-R` hides even that. Approval
tiers split agent tools into read, write and exec: yolo is the default (everything auto except the
destructive list), while ask mode prompts for writes and exec-tier calls with y/n/a (`a` allows the
tool for the rest of the session). Session ids are ULIDs, `-c` continues the newest session and
`--cid` picks an exact one.

## Development

```bash
cargo build            # debug build
cargo build --release
cargo test             # inline #[cfg(test)] modules across the tree
LLM_USER_PATH=/tmp/x cargo run -- "smoke test prompt"
```

The source is organized by role: `src/commands/` holds one file per subcommand (flags, help,
wiring), `src/core/` the shared kernel (config, the thread store, http, rendering), `src/providers/`
one adapter per protocol plus the shared message model and the provider catalog, and the domains
live top-level as `agent/` and `term/` (line editing, pickers, the spinner, terminal size). Tests
are inline per module; run one with `cargo test <name>`.

## License

MIT
