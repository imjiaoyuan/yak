# llm

A single-binary, terminal-first AI hub in Rust. One executable covers the whole loop: one-shot prompts, a conversational mode, an agent with tools and sub-agents, custom subcommands, a searchable conversation store, and multimodal input and output (images in, image/TTS out). Everything is synchronous, the whole dependency set is five crates, all state lives under one user directory, and both the chat and agent REPLs share the same line editor, pickers, spinner and key bindings.

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
without touching the real user directory.

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
llm models add                     # the wizard
llm models add deepseek sk-xxx     # direct form: catalog name + key, no prompts
```

The wizard opens a picker over the built-in provider catalog (Anthropic, OpenAI, DeepSeek, Google,
Groq, Ollama, ...), asks for your API key with hidden input, and writes the provider into
`config.json`; the first provider's first model automatically becomes the default for prompt,
agent and chat, so a fresh install is ready to run. Every configuration command is dual-form like
this: bare on a terminal opens the interactive flow, full arguments run directly. You can also
inspect or replace a single key:

```bash
llm models key               # providers, with key status
llm models key deepseek      # print the key, ${VARS} expanded (use with care)
llm models key deepseek sk-2 # set it directly (or --set for hidden input)
```

Or skip the interactive commands entirely: put the provider block in `config.json` with an
environment-variable key (see below).

Data lives under the user directory, `~/.llm` by default: `logs.db` holds every conversation as a chain of content-addressed messages, `config.json` every setting (providers with their API keys, the `models` family, the `agent` section, the plugin tables `tools`/`mcpServers`), and `commands/` the custom subcommands.

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

`llm models` is the configuration center for everything model-shaped. Bare on a terminal it walks
you through mode → provider → model → thinking depth; the same subcommands take direct arguments:

```bash
llm models                      # the wizard (or: llm models set)
llm models set chat zai/glm-5.2 --thinking high
llm models set deepseek/deepseek-chat           # a bare model targets the prompt mode
llm models get                  # the current setting of every mode
llm models unset chat
```

It is all one `models` object in config.json (distinct from the nested `agent.models`
context-window table): `prompt` is what a bare `llm "prompt"` uses, `agent`/`chat` are what
their REPLs start on, and `options` holds per-model default
options (`llm models options set MODEL KEY VALUE`). `-m` and `LLM_MODEL` stay per-invocation;
a mode's `thinking` loses only to `--thinking`; when a stored model no longer resolves (its
provider was removed), the mode warns and falls back to the prompt default.

A built-in catalog of 30+ providers (Anthropic, OpenAI, DeepSeek, Google, Groq, Mistral, Cerebras, NVIDIA, Hugging Face, Together, Baseten, Fireworks, xAI, OpenRouter, Moonshot, Kimi, Z.ai, Qwen token plans, Xiaomi MiMo, MiniMax, Vercel AI Gateway, SiliconFlow, Zhipu, and the local runtimes Ollama, LM Studio, llama.cpp, vLLM, plus `openai-image`/`openai-tts` entries whose `image` and `tts` kinds drive `--out`) carries canonical endpoints and env var names, and `llm models add` builds its wizard picker straight from it:

```bash
llm models add                             # provider wizard (catalog + templates)
llm models key deepseek sk-xxx             # set the provider's key directly
llm models set deepseek/deepseek-chat
```

## Plugins

The extension surfaces follow one rule: files declare, processes compute, the binary itself never
recompiles. Aliases, commands and skills were always files; what follows adds the three
code-bearing surfaces, all declared in config.json or dropped in as files.

When you want a tool the agent does not have, write a script and point config at it. A script tool
spawns once per call, receives the tool arguments as one JSON line on stdin, and whatever it prints
to stdout becomes the tool result; a nonzero exit reports as an error. Say you keep tickets in a
file and want the agent to look them up:

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
(default 60), and `${ENV_VAR}` expands in `command` and `args`. Script tools mount under their own
name next to the built-ins, ask approval like any exec-tier tool (`Allow? [Y/n/a]`, remembered per
session with `a`), obey `[agent] tools` policies, and can be picked with `--tools ticket,bash`.
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

Inside either REPL the same file answers to `/review src/main.rs`, submitted as one task in the
agent and as a message in chat. Names that collide with built-in commands keep the built-in, and a
word that merely looks like a typo still gets the did-you-mean guard before anything is looked up.

## Usage

The top-level help:

```
Access Large Language Models from the command-line

Usage: llm [OPTIONS] [PROMPT]...

  Running `llm` without a subcommand executes a prompt.

Commands:
  (default)  Execute a prompt
  prompt     Execute a prompt
  agent      Run an agentic task with tools
  chat       Hold an ongoing conversation (tool-less agent session)
  logs       Show past conversations
  models     Manage models, defaults and provider keys

Options:
  -h, --help     Show this message and exit
      --version  Show the version and exit
```

### Asking

```
Execute a prompt

Usage: llm prompt [OPTIONS] [PROMPT]

Options:
  -s, --system TEXT             System prompt to use
  -m, --model MODEL             Model to use
  -d, --database PATH           Path to log database
  -a, --attachment ATTACHMENT   Attachment path or URL or -
      --at PATH MIMETYPE        Attachment with explicit mimetype
  -o, --option KEY=VALUE        key/value options for the model
      --options                 Show options for the selected model
      --schema SCHEMA           JSON schema, filepath or the compact DSL
      --schema-multi SCHEMA     JSON schema for multiple results
  -p, --param KEY=VALUE         Parameters for a custom command's $variables
      --no-stream               Do not stream output
  -n, --no-log                  Don't log to database
      --log                     Log prompt and response to the database
  -R, --hide-reasoning          Hide reasoning output
      --thinking LEVEL          Reasoning effort: minimal, low, medium, high or xhigh
  -c, --continue                Continue the most recent conversation
      --conversation, --cid ID  Continue the conversation with the given ID
      --key KEY                 API key to use
  -x, --extract                 Extract first fenced code block
      --extract-last            Extract last fenced code block
      --json                    Output the response as JSON, same format as llm logs --json
      --out PATH                Write media (image/audio) output to this file
      --voice VOICE             Voice for TTS models
      --size SIZE               Image size for image models
  -h, --help                    Show this message and exit
```

```
Hold an ongoing conversation with a model (tool-less agent session)

Usage: llm chat [OPTIONS] [PROMPT]

Options:
  -m, --model MODEL            Model to use
  -s, --system-prompt TEXT     Replace the built-in system prompt
      --thinking LEVEL         Reasoning effort: off, minimal, low, medium, high or xhigh
  -c, --continue               Continue the most recent agent session
      --session, --cid ID      Continue the session with the given ID
  -a, --attachment ATTACHMENT  Attachment path or URL or -
      --at PATH MIMETYPE       Attachment with explicit mimetype
  -d, --database PATH          Path to log database
      --no-stream              Do not stream output
      --key KEY                API key to use
  -h, --help                   Show this message and exit
```

```
Run an agentic task with tools (bare invocation opens an interactive session)

Usage: llm agent [OPTIONS] [PROMPT]

Options:
  -m, --model MODEL                Model to use
  -o, --option KEY=VALUE           key/value options for the model
  -s, --system-prompt TEXT         Replace the built-in system prompt
      --append-system-prompt TEXT  Append to the system prompt
      --tools NAMES                Comma-separated tool subset (default: all)
      --approval-mode MODE         ask or yolo
      --thinking LEVEL             Reasoning effort: off, minimal, low, medium, high or xhigh
      --yolo                       Alias for --approval-mode yolo
      --max-turns N                Maximum agent turns per task (default 50)
      --mode MODE                  text (default) or json (one JSON event per line)
      --no-session                 Don't log the conversation to the database
  -c, --continue                   Continue the most recent agent session
      --session, --cid ID          Continue the session with the given ID
      --fork                       Continue a session on a new branch (original untouched; combine with --session)
  -a, --attachment ATTACHMENT      Attachment path or URL or -
      --at PATH MIMETYPE           Attachment with explicit mimetype
  -d, --database PATH              Path to log database
      --no-stream                  Do not stream output
      --key KEY                    API key to use
  -h, --help                       Show this message and exit
```

### History

```
Show recent logged prompts

Usage: llm logs list [OPTIONS] [OPTIONS] 

Options:
  -n, --count INTEGER           Number of entries to show
  -d, --database PATH           Path to log database
  -m, --model MODEL             Filter by model or model alias
  -q, --query QUERY             Search for logs matching this string
      --schema SCHEMA           Show logs that use this schema
  -c, --current                 Show the current conversation
      --conversation, --cid ID  Show the conversation with this ID
      --id-gt ID                Return responses with ID > this
      --id-gte ID               Return responses with ID >= this
  -l, --latest                  Sort by time (newest first), not relevance
      --full                    Show the full per-turn report (default is a compact list)
  -t, --truncate                Truncate long strings in output
  -u, --usage                   Include token usage
  -r, --response                Just output the last response
  -x, --extract                 Extract first fenced code block
      --extract-last, --xl      Extract last fenced code block
      --json                    Output as JSON
  -h, --help                    Show this message and exit
```

```
Show recent logged prompts

Usage: llm logs list [OPTIONS] [OPTIONS] 

Options:
  -n, --count INTEGER           Number of entries to show
  -d, --database PATH           Path to log database
  -m, --model MODEL             Filter by model or model alias
  -q, --query QUERY             Search for logs matching this string
      --schema SCHEMA           Show logs that use this schema
  -c, --current                 Show the current conversation
      --conversation, --cid ID  Show the conversation with this ID
      --id-gt ID                Return responses with ID > this
      --id-gte ID               Return responses with ID >= this
  -l, --latest                  Sort by time (newest first), not relevance
      --full                    Show the full per-turn report (default is a compact list)
  -t, --truncate                Truncate long strings in output
  -u, --usage                   Include token usage
  -r, --response                Just output the last response
  -x, --extract                 Extract first fenced code block
      --extract-last, --xl      Extract last fenced code block
      --json                    Output as JSON
  -h, --help                    Show this message and exit
```


## Examples

Asking is the default action, so `llm "explain ownership in rust"` streams an answer rendered as terminal markdown, with a spinner while the model thinks, a single gray trace line for reasoning, and a timing and token footer per prompt. Pipes stay plain: `git diff | llm "review this change" > review.md` writes the raw markdown to the file, never ANSI codes.

Pick a model per call with `-m deepseek/deepseek-chat`, or fuzzily with `-q claude` when you cannot remember the full id. Model options ride along as `-o temperature=0.2 -o top_p=0.9`, and `-u` adds the exact token counts. Attach files or URLs with `-a shot.png`, force a mimetype with `--at image.png image/png`, and add a system prompt with `-s`; piped stdin can feed an attachment instead of the prompt, so `llm -a - "what is this" < shot.png` sends the image and the words together, images, PDFs, wav/mp3 clips and plain-text files (.txt, .md, .csv, source code) ride the same request as native content blocks: text attaches as a document block on anthropic models and as an extra text part elsewhere, and anything a model family cannot accept is refused before a request leaves the machine with the supported list named in the error. Structured output comes from `--schema`: inline JSON, the compact DSL (`"name, age int: years"`) or a file. `--schema-multi` wraps it for lists of results.

Conversations continue with `llm -c "and in python?"` or `llm --cid 01ABC... "..."` (a short unambiguous prefix like `01m13d` works too), and every prompt lands in `logs.db` unless you pass `-n`. `llm logs list` groups conversations under mode sections (`agent`, `chat`, `prompt`), each row a six-character id, the model and the turn count, with previews truncated by display width so CJK prompts stay inside the margin. Bare `llm logs` on a terminal is interactive the way bare `llm models` is: one filterable list of recent conversations, each row tagged with its mode, and typing filters across mode, preview and id (fuzzy-filter style — `agent`, a model name or a phrase from the prompt all narrow it); enter opens the transcript and then offers to jump straight into the conversation (answering Y resumes it in the right session, chat threads in the chat preset, everything else in the agent); piped or flagged invocations keep printing the list. When a prompt is worth keeping, drop it in `~/.llm/commands/review.md`: the body is the prompt, `$input` receives whatever follows the command name, and `$name` / `${name}` variables fill from `-p lang=rust` on `llm review -p lang=rust`. Frontmatter can pin `model:` and `system:`, or declare `attachments` (a list of files, URLs or `-`) and `attachment_types` (a path to mimetype map) that ride along on every run, ahead of any `-a` entries.

Chat is the multi-turn form: `llm chat` opens the same line-editing REPL as the agent, minus the tools, starting on the model configured for the chat mode (`llm models set chat ...`). It shares everything with the agent session: `/clear` starts fresh, `/edit` composes a prompt in `$EDITOR`, `/mcp` reports servers, history and tab completion behave identically, and turns land in the same store tagged `chat`. Images join by pasting (ctrl+v pulls the clipboard image in as a file path) or by simply naming an image path in the message; both attach automatically. Bare `llm logs` on a terminal is the way back into any past conversation: pick one, read the transcript, answer the enter question and the session continues where it left off. Pressing ctrl+c once interrupts the running turn; a second press within two seconds leaves.

Agent mode adds tools: `llm agent "fix the failing test"` runs a loop that can read, edit, search and run commands, pausing for approval on writes, commands and reads outside the working directory unless you pass `--yolo` or set `approval_mode = "yolo"` in config; file edits and writes show a unified-diff preview (context, `-` and `+` rows, capped) right above the approval question, so you decide with the actual change in view. The `read` tool streams text files a window at a time instead of loading them: each answer opens with a metadata header naming the file, its size and the shown range, `offset` and `limit` page through 500-line windows (50KB byte cap, single lines capped at 2000 characters so a minified bundle cannot eat the context), and binary formats are refused with a hint at the right local tooling rather than garbage bytes. `webfetch <url>` fetches web pages and returns plain text (HTML stripped, 256KB cap, http(s) only, proxies inherited from the environment) so the agent can consult docs and articles without a shell. Bare `llm agent` on a terminal opens an interactive session on the agent mode's configured model (`llm models set agent ...`), with slash commands (`/skills`, `/memory`, `/compact`, `/status`, `/init`, `/mcp`, `/edit`, ...), shell passthrough via `!cmd`, and ctrl-c or esc to interrupt a running task (esc takes effect within a tenth of a second, even mid-reasoning). Past sessions reopen from `llm logs`: pick a conversation, answer the enter question. While a task runs you can keep typing; the queued lines are delivered to the model at the next tool boundary and any that outlive the task run as the next prompt. Branching is cheap: `llm agent --fork` continues the most recent session on a fresh branch (the original keeps its own history from that point), and `--fork --session ID` branches a specific one. Delegation covers the rest of the reference's shape: the `task` tool runs sub-agents defined as markdown files (`~/.llm/agents/*.md` or `.llm/agents/`), one at a time, chained with `{previous}`, or fanned out with `tasks: [...]` where every running sub-agent streams a live `name · progress` line; definitions can declare a single-line `output_schema:` (or a call can pass `outputSchema`) and the child's final answer comes back validated as JSON.

Multimodal input reaches every mode the same way: `-a screenshot.png` rides the task's first message, and inside a session ctrl+v pastes the clipboard image as a temp-file path you can see and edit, while any local image path typed into a message attaches itself automatically (a dim note confirms each one). Limit the toolbox with `--tools read,grep`, the turn budget with `--max-turns`, and swap the system prompt with `-s` or `--append-system-prompt`. Sessions persist, so `llm agent -c "now run it"` picks up where the last one ended, `--no-session` opts out, and `--mode json` emits one JSON event per line for driving the agent from other programs.

Binary formats stay outside the binary on purpose: the agent's read tool answers with a hint at local tooling instead of garbage bytes — `pdftotext` for PDFs, `samtools` for BAM and CRAM, `duckdb` for Parquet and HDF5, `libreoffice --headless --convert-to csv` for the legacy Office formats. Convert first, then feed the `.txt` to any command.

Reasoning effort is a first-class dial on every entry point: `llm prompt --thinking high`, `llm chat --thinking medium`, and `llm agent --thinking high` map to `reasoning_effort` on OpenAI-compatible endpoints and a thinking budget on Anthropic ones; each mode's default depth lives next to its model in config.json (`llm models set MODE MODEL --thinking LEVEL`, `off` omits the parameter entirely).

Skills and memory live under the user directory. Skills are SKILL.md folders discovered from `~/.llm/skills`, `~/.agents/skills` and the nearest `.llm/skills`/`.agents/skills` walking up from the working directory (later wins by name, so packs installed by other tools keep working); clone or copy a folder into one of those and the agent picks it up, listing them via `/skills` and running one with `/skill:<name>`, and the model can pick skills itself from the system-prompt list (disabled per skill with `disable_model_invocation` or globally via `[agent] disabled_skills`). Global memory is a hand-editable `~/.llm/LLM.md` injected into the agent and chat system prompts, with `/memory add` appending a manual line, `/memory update` extracting durable facts from the session into the marker-delimited auto region, `/memory clean` consolidating it, and `[agent] memory = "auto"` extracting automatically when the REPL exits. Model traffic goes through HTTP proxies from `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY` (and `NO_PROXY`) automatically

Sub-agents are markdown files with frontmatter, discovered in `~/.llm/agents/` or the nearest `.llm/agents/` walking up from the working directory (the project copy wins by name). The model delegates through the `task` tool and each sub-agent runs as a headless child process:

```markdown
---
name: researcher
description: finds things in the tree
tools: read, grep, glob      # default: read,grep,glob,ls
---
You are a researcher. Answer with file:line references.
```

History lives in `llm logs`. The default view is one line per turn grouped by conversation; `llm logs --full` prints the complete report with prompts, reasoning and responses, `llm logs -q "keyword"` searches full text with bm25 ranking, `-m` filters by model, `-c` selects the current conversation and `--cid` a specific one. Extraction shortcuts help scripting: `-r` prints just the last response, `-x` the first fenced code block, `--xl` the last, `--json` the stored row shape, and `--data` pulls rows out of a schema run.

For image and audio models, `--out` writes media files directly: `llm -m openai-image/gpt-image-1 "cyberpunk street" --out street.png` saves one image under the exact name, while a directory target (`--out shots/`, created when missing) auto-names `image.png` or `speech.mp3` inside it. Ask for several images with `-o n=3` and every one lands on disk: a directory numbers them `image-1.png`, `image-2.png`, ..., an explicit file name becomes `street-1.png`, `street-2.png`, ..., each with the extension sniffed from its own bytes. TTS output follows `-o response_format` (mp3 by default, also opus, wav, aac or flac) for the extension, `--voice` picks the speaker and `--size` the image size. `--out -` streams the single result to stdout as raw bytes, and existing files are never overwritten silently: any collision, checked across all targets before the first byte lands, is an error. Media turns are not logged to the database.

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

`approval_mode` is `always-ask` (default), `write` or `yolo`; `context_window` is where compaction
kicks in; `tools` maps each tool to `allow`, `deny` or `prompt`.

## Outputs

Every prompt, chat turn and agent session is written to `logs.db` as a chain of content-addressed messages: reasoning parts are stored next to the responses, tool calls and results ride along in agent sessions, and turns carry their options, schema and token usage. `llm logs backup /path/copy.db` takes a consistent snapshot, and a `logs-off` marker file in the user directory turns prompt logging off entirely.

## Semantics

Model ids are `provider/model` everywhere, with the `aliases` object in config.json mapping short names on top; `models set` and `models options` manage the mapping, and the `aliases` object is hand-edited config. Terminal rendering is enabled only on a TTY: prompts, chats and agent answers stream as markdown with a two-column margin, blank lines are dropped except around headings and code blocks, and piped output is the raw text. Reasoning is never dumped to the screen in any mode, one gray `thinking ... end` line records that it happened and `-R` hides even that. Approval tiers split agent tools into read, write and exec: reads run freely in ask mode, writes and exec-tier calls prompt with y/n/a, and root commands (sudo, doas, su, pkexec) are denied in every mode. Session ids are ULIDs, `-c` continues the newest session and `--cid` picks an exact one. Long lists in any picker scroll inside a bounded window instead of flooding the screen.

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
