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
`updating 0.1.2 -> 0.1.8` when it moves and leaves an unchanged version alone. `LLM_FORCE=1`
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

The shortest version:

```bash
llm                            # open the interactive session
llm "fix the failing test"      # run one task, then exit
cat error.log | llm "what broke?"   # the prompt can come from stdin
```

Piped output is plain text — no colours, no control codes — so it drops straight into a file or
another command.

### Approvals

The agent runs in **yolo mode** by default: it does whatever it needs without asking. What stops it
is a hard refusal, never a prompt, and it comes in two layers.

**Hardcoded — nothing can switch these off.** Privilege escalation (`sudo`, `su`, `doas`),
filesystem creation and destruction (`mkfs*`, `mkswap`, `fdisk`, `parted`, `dd`, `shred`, `wipefs`),
machine control (`shutdown`, `reboot`, `poweroff`, `halt`, `init`), a fork bomb, a write into a real
device node (`> /dev/sda` — `2>/dev/null` is fine), and `rm` aimed at `/` or `~`. These are refused
in yolo *and* ask mode, and no config or file edit can re-enable them.

**Your blacklist file — remove or add freely.** `~/.llm/blacklist` for every project, plus
`.llm/blacklist` in one repo (its lines win). This layer only *adds* refusals on top of the
hardcoded ones. Ordinary `rm` is **not** refused by default: deleting files is normal work. Each line
is a command word (`deploy` stops `deploy x` and `echo hi | deploy`), a whole segment
(`git push --force origin main`), a glob (`mkfs*`), or `!pattern` to re-allow. The file is seeded
with the syntax as comments; deleting it just resets those comments.

Want to approve things yourself? Use `--approval-mode ask` for one run, or put
`"approval_mode": "always-ask"` in `config.json` to make it the default. In ask mode:

- **Free:** reading files in your project, and read-only commands like `ls`, `git status`, `rg`,
  `cargo test`.
- **Asks first:** file writes and edits, branch changes like `git push`, and anything it cannot
  recognise as safe.

File edits show a unified diff right above the question. Type `a` to allow that tool for the rest of
the session. `/yolo` flips the mode on and off mid-session.

### Tools

Eight built-ins: `read`, `write`, `edit`, `bash`, `grep`, `glob`, `ls`, `webfetch`. The agent picks
them itself; `--tools read,grep` narrows the set.

The `read` tool pages through large files instead of loading them whole. Every answer starts with a
header naming the file, its size and the range shown; `offset` and `limit` walk through it in
2000-line windows (50 KB per call, single lines capped at 2000 characters so a minified bundle
cannot flood the context). `paths` reads up to five files at once. Binary formats are refused with a
hint at the right local tool — `pdftotext` for PDFs, `samtools` for BAM/CRAM, `duckdb` for
Parquet/HDF5, `libreoffice --headless --convert-to csv` for old Office files.

`webfetch <url>` grabs a page and returns it as text (HTML stripped, 256 KB cap, http(s) only,
proxies honoured) so the agent can read docs without a shell.

### The interactive session

Slash commands cover the model and the session — `/model`, `/thinking`, `/login`, `/logout`,
`/clear`, `/resume`, `/tree`, `/status`, `/reload`, … `/help` lists every one, including your skills
as `/skill:<name>`. `!cmd` runs a shell command directly, and tab completes command names and paths.

Ctrl-c or esc interrupts a running task. Anything you type while it works is queued as steering and
delivered at the next tool boundary; whatever is left over becomes your next message.

### Per-run flags

```bash
llm -m deepseek/deepseek-chat "..."        # pick a model for this run
llm -o temperature=0.2 -o top_p=0.9 "..."  # extra model options
llm --thinking high "..."                  # off | minimal | low | medium | high | xhigh
llm -s "you are a Rust reviewer" "..."     # replace the system prompt
llm --append-system-prompt "be terse" "..."
llm --tools read,grep "..."                # limit the toolbox
llm --token-budget 500000 "..."            # stop after this many input tokens
```

`--thinking` maps to `reasoning_effort` on OpenAI-compatible endpoints and to a thinking budget on
Anthropic ones. `--token-budget` counts input tokens across the whole run — every round resends the
context, so the total is what a runaway task actually costs. It warns at 80% and stops cleanly at
100%. (`--max-turns` still exists as an explicit cap; `0` or unset means unlimited.)

### Attachments

```bash
llm -a shot.png "what is wrong here?"      # attach a file
llm -a https://example.com/page "summarise this"
llm --at image.png image/png "..."         # force the mimetype
llm -a - "what is this?" < shot.png        # stdin as the attachment
```

Images, PDFs, wav/mp3 clips and plain text (.txt, .md, .csv, source files) are sent as native
content blocks. Text becomes a document block on Anthropic models and an extra text part elsewhere.
Anything the chosen model cannot accept is refused before the request leaves your machine.

In a session, ctrl+v pastes the clipboard image as a temp-file path you can see and edit, and any
local image path you type attaches itself. Long conversations keep only the newest image
attachments; older ones collapse into short text notes.

### Sessions

Every conversation is saved, so you can always come back to it:

```bash
llm -c "and in python?"                    # continue the newest session here
llm -r                                     # browse and resume past sessions
llm --session 01ABC... "..."               # pick an exact one (a short prefix works)
llm --no-session "..."                     # this run only, don't save it
llm --fork "..."                           # branch this session onto a new thread
```

`-c` looks in the current directory first and falls back to the newest session anywhere, telling you
which directory it used. `-r` opens one filterable list, newest first; typing filters across the
preview and the id. `/resume` inside the session opens the same list.

### Skills and memory

Both live in your user directory.

**Skills** are `SKILL.md` folders, discovered from `~/.llm/skills`, `~/.agents/skills` and the nearest
`.llm/skills`/`.agents/skills` walking up from where you are (later wins by name). The agent lists
them via `/help`, you run one with `/skill:<name>`, and it can pick them itself from the system
prompt. Turn one off with `disable_model_invocation`, or all of them with `[agent] disabled_skills`.

**Memory** is a plain markdown file: `~/.llm/LLM.md`. The system prompt always names that path —
even before the file exists — so "remember this" has somewhere to go: ask the agent to remember a
preference and it edits that file for you. Durable preferences live there; repo-specific rules
belong in `AGENTS.md`/`CLAUDE.md` instead. Nothing is written behind your back — the edit is a normal
file change you can review, change or delete.

Model traffic goes through the proxies in `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY` (and `NO_PROXY`)
automatically.

### Tuning the agent

Under the `"agent"` key of `config.json`:

```json
{
  "agent": {
    "approval_mode": "always-ask",
    "context_window": 128000,
    "tools": {"bash": "prompt"}
  }
}
```

`approval_mode` is `yolo` (default) or `always-ask`. `context_window` is the point where long
conversations get compacted. `tools` maps a tool to `allow`, `deny` or `prompt`.

The command blacklist is a plain file, not a config key: `~/.llm/blacklist` for everything you run,
and `.llm/blacklist` for one project (its lines win; the two are concatenated). It only adds refusals
— privilege escalation and the other dangerous commands are hardcoded and always apply. See
[Approvals](#approvals) for the line syntax.

Prompt templates turn a prompt you keep retyping into a slash command. Drop a `.md` file in
`~/.llm/commands/` (or the nearest `.llm/commands/` — the project copy wins) and `/name` runs it: the
body is the prompt, optional frontmatter can pin `system`, and `$input` receives everything after the
command name. So `/review src/main.rs` runs your template on `src/main.rs` as one task.

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

The easy path: run `llm`, type `/login`, pick a provider, paste your API key (hidden). That's it —
the first provider's first model becomes your default, so a fresh install is ready to run. The
catalog ships 38 providers, including Anthropic, OpenAI, DeepSeek, Google, Groq, Mistral, xAI,
OpenRouter, and the local runtimes Ollama, LM Studio, llama.cpp and vLLM. `/logout` removes a provider
and clears the default if it pointed there.

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

`kind` is `openai-compat` or `anthropic`. `api_key` can hold the key itself or `${ENV_VAR}` to read it
from the environment at request time — either works.

Everything lives under `~/.llm`:

| | |
|---|---|
| `threads/` | every conversation, as JSONL files |
| `config.json` | all settings, including providers and their keys |
| `extensions/` | your plugins |
| `pkg/` | packages installed with `llm install` |
| `commands/` | prompt templates |
| `blacklist` | extra commands to refuse (yours; the core is built in) |

`/model` picks the model and its thinking depth, saved for future sessions; `/thinking` changes the
depth alone. Both live in the `models` object of config.json. `-m` and `LLM_MODEL` override per run,
and `--thinking` beats the stored depth. Configs from older versions migrate on first read; if the
saved model no longer resolves, you get a warning and a fallback.

## Plugins

Extensions are how you add things the core does not ship. Put a file in `~/.llm/extensions/` — or in
the project's `.llm/extensions/`, which wins by name — then restart, or type `/reload`. There are two
shapes, and the shape is chosen by the file itself.

### A script tool: a script with a header

Add a few comment lines at the top and any script becomes a tool. The host runs it per call, passes
the arguments, and takes stdout as the result — Python, shell, R, whatever you have:

```python
#!/usr/bin/env python3
# --- llm-tool: wordcount
# description: count characters in a text
# args: text (string) the text
# arg-mode: argv
import sys
print(len(sys.argv[1]))
```

A tool with one declared argument gets it as a plain command-line argument — no JSON to parse.

### A resident extension: a program that stays running

Without a header, the file is started once per session and you talk to it in one-JSON-per-line over
stdio:

```text
→ {"id":1,"type":"initialize","params":{"version":..,"cwd":..}}
← {"id":1,"result":{"tools":[..],"commands":[..],"events":[..]}}
→ {"id":2,"type":"call_tool","name":..,"args":{..}}    ← {"id":2,"result":..}
→ {"id":3,"type":"run_command","name":..,"args":".."}  ← {"id":3,"result":".."}
→ {"id":4,"type":"event","name":..,"params":{..}}      ← {"id":4,"result":{..}}
```

At startup the host asks what the extension offers: tools (with JSON Schema parameters), slash
commands, and events it wants to hear about. After that it calls back when the model uses a tool,
when you type a matching `/command`, and at turn and tool boundaries. The `tool_call` event is the
useful one for gating — your extension can deny a call or rewrite its arguments.

Extension tools start at the same trust level as `bash`: they run freely in yolo mode, and in ask
mode each call prompts. You can lower a tool's tier if you have reviewed it — see
[`docs/extensions.md`](docs/extensions.md). `[agent] tools` policies and `--tools` still apply.

A slow or broken extension prints a dim warning and mounts nothing; it never blocks the session.
Tool calls time out after 120s (`extensions.tool_timeout` in config), events after 5s.
`extensions.disabled` skips one by name, and `/reload` restarts them all.

Two self-contained templates ship in `examples/extensions/`: `template.js` (a JavaScript runtime
whose user section uses a familiar extension API — `registerTool` / `registerCommand` /
`on("tool_call", ...)` — so most existing tool/command/hook extensions paste straight in; APIs that
need the process, like UI, editors and hotkeys, raise with a clear message) and `template.py` (the
same shape in Python). Copy one into the extensions directory and edit its user section.

The full reference is [`docs/extensions.md`](docs/extensions.md) — manifest fields, every message and
event, the `tool_call` gate, timeouts and config keys. `examples/extensions/` has three runnable
examples to copy: `wordcount` (a script tool), `websearch` (a resident extension offering
`web_search` + `web_fetch` and a `/web` command — uses `BRAVE_API_KEY` when set, otherwise keyless
DuckDuckGo/Wikipedia) and `todo`.

Here is a whole resident extension — a tool that shells out to `deploy.sh`:

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

Every conversation is saved to `~/.llm/threads/` as a JSONL file, one per session. Each turn records
its model, options and token usage; in agent sessions the tool calls and results ride along, and
reasoning is stored beside the answer. Pass `--no-session` for a throwaway run — otherwise
everything is kept.

## Semantics

Models are named `provider/model`. Short aliases can be mapped in the `aliases` object of
config.json.

Rendering is on only for a real terminal: answers stream as markdown with a left margin, and piped
output is the raw text. Reasoning is never printed — a dim `thinking ... end` line just marks that it
happened.

Approval tiers are read, write and exec. Yolo mode (the default) auto-approves everything; the
hardcoded refusals (privilege escalation, filesystem/machine destruction, a fork bomb, a write into a
device node, `rm` at `/` or `~`) apply in either mode, and your blacklist file only adds to them.
Ask mode prompts for writes and exec-tier calls with `y/n/a`, where `a` allows that tool for the rest
of the session. Session ids are ULIDs.

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
