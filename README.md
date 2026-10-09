# yak

A coding agent that lives in your terminal: `yak "fix the failing test"` runs one task with tools (reading, editing, searching, running commands) and bare `yak` opens an interactive session. Sessions are stored as thread files and can be resumed anytime.

One binary, ~3 dependencies (`ureq`, `serde_json`, `unicode-width`), no async runtime, every other piece handwritten in-tree. Linux, macOS and Windows.

## Install

Linux and macOS download the prebuilt binary, verify its sha256 and install it with:

```bash
curl -fsSL https://jiaoyuan.org/yak/install.sh | sh
```

It lands in `~/.local/bin`, no root needed, and adds that directory to your `PATH` when missing. Windows does the same from PowerShell:

```powershell
irm https://jiaoyuan.org/yak/install.ps1 | iex
```

It lands in `%USERPROFILE%\.local\bin`, no admin needed, and appends that directory to the user `Path`. Re-running either line is the updater: it checks the latest GitHub release, prints `updating 0.2.1 -> 0.2.2` when it moves and leaves an unchanged version alone. `YAK_FORCE=1` reinstalls anyway; `YAK_VERSION` pins a release tag, `YAK_REPO` installs from a fork and `YAK_INSTALL_DIR` picks a different directory. `YAK_GH_PROXY=1` routes every download through `https://gh-proxy.com/` for hosts where GitHub is slow or unreachable — any prefix-style proxy URL works as the value too, and behind a proxy the latest-version lookup switches from the releases redirect to the GitHub API, which such proxies pass through. On Linux the static musl build is used, so the same binary runs on any distribution; prebuilt targets today are x86_64 and aarch64 Linux, x86_64 and aarch64 macOS, and x86_64 Windows.

Building from source works the same everywhere:

```bash
git clone https://github.com/imjiaoyuan/yak
cd yak
cargo build --release
```

You need a Rust toolchain (install one with [rustup](https://rustup.rs)). The binary lands in `target/release/yak` (`target\release\yak.exe` on Windows): put it on your `PATH`. State lives under `~/.yak` (`%USERPROFILE%\.yak` on Windows); set `YAK_USER_PATH` to relocate it. Requests to OpenCode's Go/Zen gateway carry a per-conversation `x-opencode-session` id; set `YAK_SESSION_ID` to pin one id across several `yak` invocations of the same conversation.

### Platform notes

Linux, macOS and Windows all use the native platform implementation for the same terminal experience: raw-mode line editing, arrow-key pickers, hidden key input, and esc/ctrl-c interrupt while an agent task runs. Shell commands use `sh` on Linux/macOS and PowerShell on Windows by default; set `YAK_SHELL` to override the program (for example `cmd`, `powershell`, `pwsh`, `bash`, `zsh`, or any other shell on `PATH`). The interactive features need a real terminal; with piped stdin the CLI reads plain input.

## Usage

The shortest version:

```bash
yak                            # open the interactive session
yak "fix the failing test"      # run one task, then exit
cat error.log | yak "what broke?"   # the prompt can come from stdin
```

Piped output is plain text (no colours, no control codes) so it drops straight into a file or another command.

The first run needs a model: type `/login`, pick a provider from the shipped catalog of 42 (Anthropic, OpenAI, DeepSeek, Google, Groq, Mistral, xAI, OpenRouter, plus the local Ollama, LM Studio, llama.cpp and vLLM), paste the API key, pick the default model. Or write the provider block into `config.json` by hand; see [Providers and models](docs/usage.md#providers-and-models).

Attachments ride along natively (images, PDFs, audio, text) and in a session ctrl+v pastes the clipboard image straight into the prompt. Ask "remember this preference: ..." and the `remember` tool files it in `~/.yak/YAK.md`, injected into every future session.

```bash
yak -a shot.png "what is wrong here?"
yak -a https://example.com/page "summarise this"
```

### Going deeper

The front page ends here. Everything else lives in [`docs/usage.md`](docs/usage.md):

- [The interactive session](docs/usage.md#the-interactive-session): slash commands, `!cmd`, interrupting a running task
- [Approvals and the blacklist](docs/usage.md#approvals-and-the-blacklist): what is hardcoded, what you configure
- [Tools](docs/usage.md#tools): the nine built-ins and how they behave
- [Attachments](docs/usage.md#attachments): files, URLs, stdin, clipboard pastes
- [Sessions](docs/usage.md#sessions): resume, fork, export, `--no-session`
- [Session tree](docs/usage.md#session-tree): `/tree` jumps that branch instead of truncating
- [Skills](docs/usage.md#skills) and [prompt templates](docs/usage.md#prompt-templates)
- [Providers and models](docs/usage.md#providers-and-models): `/login`, hand-edited config, aliases
- [Tuning the agent](docs/usage.md#tuning-the-agent): tool policies, request caps, cache TTL, compaction
- [The --json interface](docs/usage.md#the---json-interface): drive `yak` from an editor, CI lane or another agent
- [Plugins](docs/usage.md#plugins): script tools (Python, shell, Rust — `.rs` sources compile on first call), resident extensions, subagents, MCP

The plugin wire protocol has its own full reference: [`docs/extensions.md`](docs/extensions.md).

## Packages

Packages bundle extensions, skills and prompt templates into one git repository and share it as a unit: `yak install git:github.com/user/repo[@ref]` clones into `~/.yak/pkg/<name>` (`-l` installs project-local into `.yak/pkg/`, project winning over user; `-g` is the explicit default), and its `extensions/`, `skills/` and `commands/` directories mount into the normal discovery walks. A `SKILL.md` at the repository root counts too, as a single whole-repo skill: the shape most standalone skill repos ship (`SKILL.md` + `references/` at the top), so `yak install https://github.com/user/my-skill` lands `/skill:my-skill` with no extra step. `install` reports what it recognized (skills, extensions, prompts); a repo carrying none of them is called out instead of mounting nothing silently.

Where the package goes is the only question: `-l` puts the clone in the project's `.yak/pkg/`, `-g` (the default) in `~/.yak/pkg/`: a piped or CI install takes the same path, with nothing to prompt.

Re-running `install` refreshes a clone (`git fetch` + reset); a pinned `@ref` clone moves only via `install repo@new-ref`. `yak list` shows what each package carries, `yak remove NAME` deletes it. There is no npm lane: git only. Review any third-party package before installing: extensions run with full system access.

```bash
yak install git:github.com/user/yak-deploy    # → ~/.yak/pkg/yak-deploy
yak install git:github.com/user/yak-deploy@v2 # pinned
yak install -l https://github.com/user/my-skill        # project-local skill
yak list
yak remove yak-deploy
```

## Help

```
Access Large Language Models from the command-line

Usage:
  yak [flags] [PROMPT]

Bare `yak` opens an interactive agent session; `yak "task"` runs the
agent once with tools.

Available commands:
  export     Export a conversation as markdown (also: /export)
  install    Install a git package (also: remove, list)

Flags:
  -h, --help      Show this message and exit
  -v, --version   Show the version number
```

```
Clone a package into the pkg directory (re-run to refresh)

Usage: yak install git:github.com/user/repo[@ref] [OPTIONS] SOURCE

Options:
  -l, --local           Install project-local (.yak/pkg/ instead of ~/.yak/pkg/)
  -g, --global          Install into the user directory (default)
  -h, --help            Show this message and exit
```

```
Export a conversation as a markdown file

Usage: yak export [OPTIONS] PATH

Options:
  -h, --help            Show this message and exit
```

```
Delete an installed package

Usage: yak remove NAME [OPTIONS] 

Options:
  -h, --help            Show this message and exit
```

```
List installed packages and what they carry

Usage: yak list [OPTIONS] 

Options:
  -h, --help            Show this message and exit
```

## Development

```bash
cargo build            # debug build
cargo build --release
cargo test             # inline #[cfg(test)] modules across the tree
YAK_USER_PATH=/tmp/x cargo run -- "smoke test prompt"
```

The source is organized by role: `src/commands/` holds one file per subcommand (flags, help, wiring), `src/core/` the shared kernel (config, the thread store, http, rendering), `src/providers/` one adapter per protocol plus the shared message model and the provider catalog, and the domains live top-level as `agent/` and `term/` (line editing, pickers, the spinner, terminal size). Tests are inline per module; run one with `cargo test <name>`.

Internal architecture (request path, agent loop, extension host, rendering) is documented in [`docs/architecture.md`](docs/architecture.md); the contributor guide is `AGENTS.md`.
