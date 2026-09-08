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

### Providers

Run `llm` and use `/login`: the wizard opens a picker over the built-in provider catalog
(Anthropic, OpenAI, DeepSeek, Google, Groq, Ollama, ...), asks for your API key with hidden
input, and writes the provider into `config.json`; the first provider's first model
automatically becomes the shared default, so a fresh install is ready to run. `/logout` removes
a provider and clears the default if it pointed there.

Or skip the wizard entirely: put the provider block in `config.json` with an
environment-variable key (see below).

Data lives under the user directory, `~/.llm` by default: `threads/` holds every conversation as JSONL thread files, `config.json` every setting (providers with their API keys, the `models` family, the `agent` section, the `extensions` table), `extensions/` the code-bearing plugins, `pkg/` the installed packages, and `commands/` the prompt templates.

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

`kind` is `openai-compat` or `anthropic`, and `api_key` expands `${ENV_VAR}`
references at request time, so literal secrets and environment indirection live in the same field.

### The default model

`/model` in the REPL picks the default model and its thinking depth — one default the REPL
always starts on. The picker walks provider → model → thinking depth and saves the choice;
`/thinking` adjusts the depth alone. It is all one `models` object in config.json:
`default` is the startup model, `thinking` the reasoning depth riding it, and `options`
per-model default options (hand-edited). `-m` and `LLM_MODEL` stay per-invocation; the stored
`thinking` loses only to `--thinking`. Legacy per-mode entries (`prompt`/`agent` keys from
older versions) migrate on first read: the prompt entry wins. When the stored default no longer
resolves (its provider was removed), runs warn and fall back.

A built-in catalog of 30+ providers (Anthropic, OpenAI, DeepSeek, Google, Groq, Mistral, Cerebras, NVIDIA, Hugging Face, Together, Baseten, Fireworks, xAI, OpenRouter, Moonshot, Kimi, Z.ai, Qwen token plans, Xiaomi MiMo, MiniMax, Vercel AI Gateway, SiliconFlow, Zhipu, and the local runtimes Ollama, LM Studio, llama.cpp, vLLM) carries canonical endpoints and env var names; the `/login` wizard is built straight from it.

## Plugins

Extensions are the plugin system, the reference-shaped: anything the core skips, you build yourself as an
extension — an executable in `~/.llm/extensions/` or the project's `.llm/extensions/` (the project
copy wins by name), in any language the shebang decides. One file is one extension, and it stays
running for the whole session, speaking one JSON message per line over stdio:

```text
→ {"id":1,"type":"initialize","params":{"version":..,"cwd":..}}
← {"id":1,"result":{"tools":[..],"commands":[..],"events":[..]}}
→ {"id":2,"type":"call_tool","name":..,"args":{..}}    ← {"id":2,"result":..}
→ {"id":3,"type":"run_command","name":..,"args":".."}  ← {"id":3,"result":".."}
→ {"id":4,"type":"event","name":..,"params":{..}}      ← {"id":4,"result":{..}}
```

The `initialize` handshake advertises the extension's tools (JSON Schema parameters), slash
commands and event subscriptions; the host then routes `call_tool` when the model invokes one,
`run_command` when the user types a matching `/command`, and `event` at turn and tool boundaries
(`tool_call` may deny or rewrite a call — permission gates and path protection live here).
Extension tools are exec-tier: the approval matrix asks by default (`Allow? [Y/n/a]`, remembered
per session with `a`), `[agent] tools` policies still win, and `--tools` picks a subset.
`extensions.disabled` in config.json skips one by name, `/reload` respawns everything, and a
slow or broken extension warns dimly and mounts nothing — it never blocks a session. Tool calls
time out after 120s (config `extensions.tool_timeout`), events after 5s.

A minimal tool extension, complete in thirty lines of Python:

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

Prompt templates are the data-only surface: `~/.llm/commands/*.md` (or the nearest
`.llm/commands/`, project wins) turns a prompt you keep retyping into `/name`. The body is the
prompt, frontmatter may pin `system`, and `$input` receives everything after the command name,
so `/review src/main.rs` runs the template with `src/main.rs` as input, submitted as one task
in the agent session:

```markdown
---
system: You are a meticulous code reviewer.
---
Review $input for correctness bugs and suggest minimal fixes.
```

Packages bundle all three (extensions, skills, prompt templates) into one git repository and
share it as a unit: `llm install git:github.com/user/repo[@ref]` clones into `~/.llm/pkg/<name>`
(`-l` installs project-local into `.llm/pkg/`, project winning over user), and its
`extensions/`, `skills/` and `commands/` directories mount into the normal discovery walks.
Re-running `install` refreshes a clone (`git fetch` + reset); a pinned `@ref` clone moves only
via `install repo@new-ref`. `llm list` shows what each package carries, `llm remove NAME`
deletes it. There is no npm lane — git only. Review any third-party package before installing:
extensions run with full system access.

```bash
llm install git:github.com/user/llm-deploy    # → ~/.llm/pkg/llm-deploy
llm install git:github.com/user/llm-deploy@v2 # pinned
llm list
llm remove llm-deploy
```

## Usage

The top-level help:

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

## Examples

`llm` is the agent, the reference-shaped: bare `llm` on a terminal opens the interactive REPL, `llm "fix the failing test"` runs the task once with tools and exits, and piped stdin is the task text (`git diff | llm "review this change" > review.md` — pipes stay plain, never ANSI codes). The loop can read, edit, search and run commands, pausing for approval on writes, commands and reads outside the working directory unless you pass `--yolo` or set `approval_mode = "yolo"` in config; file edits and writes show a unified-diff preview (context, `-` and `+` rows, capped) right above the approval question, so you decide with the actual change in view. The `read` tool streams text files a window at a time instead of loading them: each answer opens with a metadata header naming the file, its size and the shown range, `offset` and `limit` page through 500-line windows (50KB byte cap, single lines capped at 2000 characters so a minified bundle cannot eat the context), and binary formats are refused with a hint at the right local tooling rather than garbage bytes. `webfetch <url>` fetches web pages and returns plain text (HTML stripped, 256KB cap, http(s) only, proxies inherited from the environment) so the agent can consult docs and articles without a shell. The REPL carries slash commands (`/model`, `/thinking`, `/login`, `/logout`, `/clear`, `/skills`, `/memory`, `/compact`, `/status`, `/init`, `/tools`, ...), shell passthrough via `!cmd`, and ctrl-c or esc to interrupt a running task (esc takes effect within a tenth of a second, even mid-reasoning). While a task runs you can keep typing; the queued lines are delivered to the model at the next tool boundary and any that outlive the task run as the next prompt. Branching is cheap: `llm --fork` continues the most recent session on a fresh branch (the original keeps its own history from that point), and `--fork --session ID` branches a specific one.

Pick a model per call with `-m deepseek/deepseek-chat`. Model options ride along as `-o temperature=0.2 -o top_p=0.9`. Attach files or URLs with `-a shot.png`, force a mimetype with `--at image.png image/png`, and add a system prompt with `-s`; piped stdin can feed an attachment instead of the prompt, so `llm -a - "what is this" < shot.png` sends the image and the words together, images, PDFs, wav/mp3 clips and plain-text files (.txt, .md, .csv, source code) ride the same request as native content blocks: text attaches as a document block on anthropic models and as an extra text part elsewhere, and anything a model family cannot accept is refused before a request leaves the machine with the supported list named in the error.

Conversations continue with `llm -c "and in python?"` or `llm --session 01ABC... "..."` (a short unambiguous prefix like `01m13d` works too), and every session lands in `~/.llm/threads/` unless you pass `--no-session`. `llm -r` is the way back in: one filterable list of recent conversations (typing filters across preview and id, fuzzy-filter style); enter opens the transcript and then offers to jump straight into the conversation (answering Y resumes it in the agent session). Inside the REPL, slash commands fill the same roles; when a prompt is worth keeping, drop it in `~/.llm/commands/review.md`: the body is the prompt, `$input` receives whatever follows the command name, and unknown `/review` runs it as a template. Frontmatter can pin `system:`.

Multimodal input reaches every mode the same way: `-a screenshot.png` rides the task's first message, and inside a session ctrl+v pastes the clipboard image as a temp-file path you can see and edit, while any local image path typed into a message attaches itself automatically (a dim note confirms each one). Limit the toolbox with `--tools read,grep`, the turn budget with `--max-turns`, and swap the system prompt with `-s` or `--append-system-prompt`. Sessions persist, so `llm -c "now run it"` picks up where the last one ended, `--no-session` opts out.

Binary formats stay outside the binary on purpose: the agent's read tool answers with a hint at local tooling instead of garbage bytes — `pdftotext` for PDFs, `samtools` for BAM and CRAM, `duckdb` for Parquet and HDF5, `libreoffice --headless --convert-to csv` for the legacy Office formats. Convert first, then feed the `.txt` to any command.

Reasoning effort is a first-class dial on every entry point: `llm --thinking high` maps to `reasoning_effort` on OpenAI-compatible endpoints and a thinking budget on Anthropic ones; the default depth lives next to the default model in config.json (`/thinking` in the REPL; `off` omits the parameter entirely).

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
