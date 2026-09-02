# Repository Guidelines

Contributor guide for `llm`, a single-binary, terminal-first AI hub written in Rust (edition 2024).

## Build, Test, Run

- `cargo build` — debug binary; `cargo build --release` for `target/release/llm`.
- `cargo test [name]` — run inline unit tests (optionally filtered).
- `LLM_USER_PATH=/tmp/x cargo run -- "prompt"` — smoke-test against a hermetic user directory.
- `cargo fmt` before committing; keep `cargo clippy` clean.
- `README.md` embeds every command's full `-h` output; refresh it when flags or help text change.

## Architecture (one line)

`src/main.rs` dispatches argv to one file per subcommand in `src/commands/` on top of shared kernels: `src/core/` (databases, templates, config, HTTP), `src/providers/` (unified `Msg` model and streaming), `src/agent/` (agent loop, tools, REPL), plus `src/read/`, `src/platform/`, `src/term/`; `.reference/` is a read-only reference — never edit or build it.

State lives under `~/.llm` (override with `LLM_USER_PATH`); `LLM_SHELL` overrides the shell on all platforms.

## Conventions

- Synchronous code only (no async runtime); no new crates — the four are ureq, rusqlite, serde/serde_json; extend existing helpers in place, one command per file.
- Idiomatic Rust naming (`snake_case` items, `CamelCase` types), 4-space indentation.
- Unit tests are inline `#[cfg(test)]` modules next to the code they cover; one behavior per test, descriptive names.
- Commits: imperative, lowercase, prefix-free ("add interactive agent repl with slash commands"); one focused change per commit. PRs target `main`; releases are cut from `v*` tags by `.github/workflows/release.yml`.
- Consult `CLAUDE.md` (authoritative architecture reference) before structural changes; check `.reference/` when behavior is unclear.
