# Repository Guidelines

Contributor guide for `llm`, a single-binary, terminal-first AI hub written in Rust (edition 2024). Everything is synchronous and dependencies are intentionally limited to four crates.

## Project Structure & Module Organization

- `src/main.rs` — entry point; `dispatch` routes `argv` to a command module. Unknown words that aren't subcommands are treated as a prompt, except near-miss typos of a subcommand which exit with a did-you-mean hint.
- `src/commands/` — one file per CLI subcommand (`prompt.rs`, `agent.rs`, `logs.rs`, ...); each exposes `pub fn run(argv: &[String]) -> i32` and parses its own flags via `core::args` specs. `llm chat` is a tool-less preset implemented in `agent.rs` (`run_chat`).
- `src/core/` — shared services/kernel: databases, templates, schemas, attachment loading, markdown rendering, HTTP, config, args, custom commands.
- `src/providers/` — the unified conversation model (`message.rs`: `Msg`/`ToolDef`/`ToolCall`) and HTTP streaming (OpenAI-compatible and Anthropic kinds), plus the provider catalog.
- `src/agent/` — the agent loop, tools (eight built-ins plus the `task` sub-agent tool and the config plugin tables: MCP servers, script tools), approvals, session, memory, skills, compaction, and the one shared REPL (chat is a preset of it).
- `src/read/` — text file reading behind the agent's read tool: streaming line windows, binary gate with tooling hints.
- `src/platform/` — cross-platform terminal, shell, editor and clipboard primitives (Linux/macOS/Windows), including the clipboard image read behind ctrl+v in the REPL.
- `src/term/` — terminal primitives (raw-mode line editor with bracketed paste, the shared picker, ticker).
- plugins elsewhere: `src/core/commands_md.rs` (`~/.llm/commands/*.md` subcommands).
- `.github/workflows/` — CI and release pipelines.
- `.reference/` — read-only reference implementation; never edit or build it.

`LLM_SHELL` is an explicit override honored on all platforms; without it the platform default is used (`sh` on Unix, PowerShell on Windows).

State lives under the user directory (`~/.llm` by default, overridable with `LLM_USER_PATH`).

## Build, Test, and Development Commands

- `cargo build` — compile the debug binary.
- `cargo build --release` — produce the release binary at `target/release/llm`.
- `cargo test` — run all inline unit tests; append a name to filter (`cargo test <name>`).
- `LLM_USER_PATH=/tmp/x cargo run -- "prompt"` — smoke-test a prompt against a hermetic, temporary user directory.

`README.md` embeds every command's full `-h` output; refresh it when flags or help text change.

## Coding Style & Naming Conventions

- Run `cargo fmt` before committing; keep `cargo clippy` clean (one behavior per test, descriptive names).
- Use 4-space indentation and idiomatic Rust naming: `snake_case` for items, `CamelCase` for types.
- Follow existing patterns: synchronous code only (no async runtime), no new crates — extend the existing four (ureq, rusqlite, serde/serde_json) and the handwritten helpers in-place, one command per file.

## Testing Guidelines

- Unit tests live inline as `#[cfg(test)]` modules next to the code they cover.
- Name tests for the behavior they cover (`fn boundaries_clamp_to_char_edges`) and keep each one focused on a single behavior.
- CI runs `cargo test` on Ubuntu, macOS, and Windows for every push to `main` and each pull request.

## Commit & Pull Request Guidelines

- Commit messages are imperative, lowercase, and prefix-free: "add interactive agent repl with slash commands" or "fix yaml parser panic on multibyte keys".
- Keep commits focused on one change with a short subject line.
- PRs target `main` and describe the change plus any behavior or output differences. Releases are cut from `v*` tags by `.github/workflows/release.yml`.

## Agent-Specific Instructions

`CLAUDE.md` is the authoritative architecture reference — consult it before structural changes. Check `.reference/` when behavior is unclear.
