# Repository Guidelines

`llm` is a single-binary, terminal-first coding agent in Rust (edition 2024), the reference-shaped. `CLAUDE.md` is the
authoritative architecture reference — read the matching paragraph before touching a layer.

## Commands

```bash
cargo build / cargo build --release          # target/(release/)llm
cargo test <name>                            # inline #[cfg(test)] modules only
cargo fmt && cargo clippy --all-targets      # both must be clean before committing
LLM_USER_PATH=/tmp/x cargo run -- "task"     # hermetic smoke test
python .github/ci_e2e.py target/debug/llm    # CI end-to-end (mock SSE server)
python .github/ci_repl.py target/debug/llm   # CI interactive REPL over a real pty
```

- CI (`.github/workflows/ci.yml`) runs test → build → `ci_e2e.py` → `ci_repl.py` on ubuntu, macOS
  and windows-latest; the two Python scripts are CI-local harnesses, not shipped code. `ci_repl.py`
  skips itself on Windows, so run it on Unix to exercise the terminal paths.
- `README.md` embeds every command's full `-h` output **byte-identically**; refresh the block when
  flags or help text change.

## Architecture that is not obvious from the tree

- `src/main.rs` dispatches argv to the agent by default (`llm` bare = the REPL, text = a one-shot
  task) and to one file per subcommand in `src/commands/` (only the package commands remain:
  `pub fn run(argv: &[String]) -> i32`); `src/core/` (config.json, thread-file store, http,
  rendering), `src/providers/` (unified `Msg` + one adapter per wire protocol + the provider
  catalog), `src/agent/` (loop, tools, approvals, extension host, REPL), `src/read/`,
  `src/platform/` + `src/term/` (raw-mode line editor, picker, spinner).
- Hand-rolled instead of crate-ified, at `src/` root: `yaml.rs`, `b64.rs`, `gitignore.rs`,
  `jsonfmt.rs`. Deps are deliberately four (ureq, serde, serde_json, unicode-width) and the code is
  synchronous — no async runtime. Add a crate only when it buys real correctness or speed;
  otherwise extend the in-tree helper.
- Everything HTTP goes through `src/core/http.rs` `send_raw`/`get_with`. Gateway- or
  provider-required headers belong there via `identity_headers(url)`, never in an adapter: it sends
  a real `user-agent` (`llm/<version>`) and, for `opencode.ai` hosts, the `x-opencode-session`
  conversation id that OpenCode Go/Zen otherwise rejects with `400 MissingSessionID` (one ulid per
  process; `LLM_SESSION_ID` pins it across invocations).
- `LLM_USER_PATH` relocates `~/.llm` (also `LLM_SHELL` for the shell). Session ids are ULIDs from
  `core::db::ulid()`; never hand-roll another id or RNG.

## Workflow

- Commits: imperative, lowercase, no prefix ("add interactive agent repl with slash commands"), one
  focused change each. PRs target `main`; releases are cut from `v*` tags by
  `.github/workflows/release.yml`.
- 