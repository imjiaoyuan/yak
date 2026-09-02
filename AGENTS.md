# Repository Guidelines

Contributor guide for `llm`, a single-binary, terminal-first AI hub written in Rust (edition 2024).

## Build, Test, Run

- `cargo build` — debug binary; `cargo build --release` for `target/release/llm`.
- `cargo test [name]` — run inline unit tests (optionally filtered).
- `LLM_USER_PATH=/tmp/x cargo run -- "prompt"` — smoke-test against a hermetic user directory.
- `cargo fmt` before committing; keep `cargo clippy` clean.
- `README.md` embeds every command's full `-h` output byte-identically; refresh it when flags or help text change.
- Keep `cargo clippy --all-targets` at zero warnings before committing.

## Architecture (one line)

`src/main.rs` dispatches argv to one file per subcommand in `src/commands/`, each `pub fn run(argv: &[String]) -> i32`, on top of shared kernels: `src/core/` (logs.db, config.json, HTTP, rendering, templates, JSON/YAML helpers), `src/providers/` (unified `Msg` model + streaming adapters), `src/agent/` (agent loop, tools, sub-agents, approvals, REPL), `src/read/` (streaming text-file windows), `src/platform/` and `src/term/` (raw-mode line editor, picker, spinner). Handwritten helpers we refused to crate-ify live at `src/` root: `yaml.rs`, `b64.rs`, `hash.rs`, `blake2.rs`, `gitignore.rs`, `jsonfmt.rs`. `.reference/` is a read-only behavioral spec — never edit or build it; `CLAUDE.md` is the authoritative architecture reference.

State lives under `~/.llm` (override with `LLM_USER_PATH`); `LLM_SHELL` overrides the shell on all platforms.

## Conventions

- Synchronous code only (no async runtime); a small, deliberate dep set — five today: ureq, rusqlite, serde/serde_json, and unicode-width (real terminal cell widths). Add a crate only when it materially buys correctness or efficiency (unicode-width did); otherwise extend the in-tree helpers. One command per file.
- Idiomatic Rust naming (`snake_case` items, `CamelCase` types), 4-space indentation.
- Unit tests are inline `#[cfg(test)]` modules next to the code they cover; one behavior per test, descriptive names.
- Commits: imperative, lowercase, prefix-free ("add interactive agent repl with slash commands"); one focused change per commit. PRs target `main`; releases are cut from `v*` tags by `.github/workflows/release.yml`.
- Consult `CLAUDE.md` (authoritative architecture reference) before structural changes; check `.reference/` when behavior is unclear.
