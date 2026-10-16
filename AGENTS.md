# AGENTS.md

Single contributor guide for `yak`: what the project is, how to build, test and release it, the module map, the storage layout and the workflow rules, plus the constraints a change must not break. Everything deeper has exactly one home:

| Topic | Home |
|---|---|
| request path, retries and the error taxonomy, provider adapters, prompt caching | `docs/architecture.md` |
| agent loop, tools, approval/blacklist, compaction, memory, skills, system prompt | `docs/architecture.md` |
| extension host, script tools, resident protocol, packages | `docs/architecture.md` |
| REPL and session surfaces, threads store, export | `docs/architecture.md` |
| plugin wire protocol and manifest fields | `docs/extensions.md` |
| the user-facing surface: flags, help text, semantics | `README.md` |
| the user manual absorbing the README deep content | `docs/usage.md` |

State a fact once, in its home. A `docs/` fact restated here is one that will drift out of sync.

## What this is

`yak` is a single-binary, plugin-based harness for LLMs in Rust, the reference-shaped: one executable, one agent loop (four built-in tools plus the `ls`, `plan`, `grep`/`glob` search, `memory`, `webfetch` and `skills` surfaces shipped as example extensions, approvals, compaction, memory, an interactive REPL), a thread-file session store, and an out-of-process extension host standing in for the reference's TypeScript extensions. On top sit kept extensions the reference lacks: multimodal input (`-a/--attachment` path/URL/stdin; in a session ctrl+v pastes the clipboard image (any image type the selection offers, else the path of a copied image *file*) as a temp-file path, and any local image path in a message auto-attaches).



- `the behavioral spec`: agent loop, tools, streaming REPL, extension/packages shape.
- `the approval reference`: the approval model.
- `~/work/references/yak/`: historical: only the JSONL thread-store shape and ULIDs survive from it.

## Commands

```bash
cargo build                  # debug
cargo build --release        # release profile: thin LTO, strip, codegen-units=1
cargo test                   # inline #[cfg(test)] modules across the tree
cargo test <name>            # one test by name
cargo fmt                    # before committing
cargo clippy --all-targets   # keep at zero warnings
```

- Tests are one behavior per test under a descriptive name (`boundaries_clamp_to_char_edges`, never `test_foo`). A test that needs a scratch directory takes one from `core/testutil` (`scratch_dir` creates it empty, `scratch_path` hands back an absent path for the tests that assert on what a missing tree does): never hand-roll `env::temp_dir().join(...)`, and never key a name on `process::id()` (it does not separate the parallel threads of one test binary).
- Smoke-test the binary hermetically: every path resolves under the override, so nothing touches the real user dir.

  ```bash
  YAK_USER_PATH=/tmp/x cargo run -- "task"
  ```

- End-to-end: a python `http.server` mock returning OpenAI-style SSE on `127.0.0.1` (or Anthropic-style `event:`-framed SSE for `/v1/messages`) plus a `config.json` pointing at it exercises any full path. Start server and client in the same script; backgrounded servers do not survive between tool calls, and pass `< /dev/null`: the CLI reads piped stdin as prompt text by design. `.github/ci_e2e.py` is the working instance; run it locally against a fresh build with `python .github/ci_e2e.py target/debug/yak`.
- CI (`.github/workflows/ci.yml`) runs test → release build → `ci_e2e.py` → `ci_repl.py` on ubuntu, macOS and windows-latest. The two Python scripts are CI-local harnesses, not shipped code; `ci_repl.py` covers the interactive pty paths (multiline keys, kitty CSI-u folding, `!` tab completion, the ctrl+g editor round-trip, and a `/tree` branch round-trip whose model rounds write real files so the `workspace_checkpoint.py` example's restore is exercised end to end) and skips itself on Windows, so run it on Unix to exercise the terminal.
- `README.md` embeds every command's full `-h` output verbatim. When flags or help text change, refresh the matching block there too and keep it byte-identical (`tests/readme_help.rs` enforces it).

### Environment overrides

- `YAK_USER_PATH`: relocate the whole user directory (state root, see Storage).
- `YAK_MODEL`: beats the stored default, loses only to `-m`.
- `YAK_SHELL`: the shell behind every spawned command (agent bash, `!cmd`); platform default is `sh`/PowerShell.
- `YAK_SESSION_ID`: pins the per-conversation `x-opencode-session` id across invocations.
- `COLUMNS`/`LINES`, `TERM`, `COLORTERM`, `NO_COLOR`, `CLICOLOR_FORCE`: terminal sizing and the color gate. `EDITOR`/`VISUAL`: the ctrl+g round-trip. `HOME`/`USERPROFILE`: user dir root. Model traffic honors the usual proxy variables through `ureq`.

### Releases

Remote is `github.com/imjiaoyuan/yak` (branch `main`); tagged `v*` cuts releases (`.github/workflows/release.yml`, six targets: aarch64/x86_64 Apple, x86_64 Windows, and x86_64-gnu plus x86_64/aarch64-musl Linux; sha256 checksums). The release body is generated, not written: the workflow lists every commit subject since the previous tag (version-bump commits filtered out) above the install one-liner, so commit subjects are the release notes; write each one as a user-facing sentence. There is no CHANGELOG file; the releases page is the changelog. Repo-root `install.sh`/`install.ps1` are the portable installer/updaters over those assets: re-running resolves the latest release and compares versions (`updating old -> new`; equal versions stay unless `YAK_FORCE=1`), with `YAK_VERSION`/`YAK_REPO`/`YAK_INSTALL_DIR` overrides and a user-level `~/.local/bin` everywhere. `YAK_GH_PROXY=1` routes all GitHub traffic through a reverse proxy (default `https://gh-proxy.com/`, or any prefix-style URL as the value) for hosts with unstable GitHub connectivity; behind a proxy the latest-tag lookup switches from the releases redirect to the API, which prefix proxies pass through. Keep them in sync with the release asset names (`yak-<target>.tar.gz` on Unix, `yak-<target>.zip` on Windows, each with a `.sha256`). The Pages site (`https://jiaoyuan.org/yak/`, source = main root) serves only the two installer scripts, never binaries.

## Hard constraint: minimal dependencies

Three crates total: `ureq` (sync HTTP+TLS), `serde`/`serde_json` (with the `preserve_order` feature (insertion order matters for JSON output parity), and `unicode-width` (real terminal cell widths; ) it hard-wraps the chrome: `$` action lines, tool summaries and the logs transcript replay, so those rows match the terminal; the live answer stream hard-wraps too so wrapped rows keep the left margin). No async runtime; the CLI is synchronous throughout: keep new code sync.

Everything else is handwritten in-tree:

| Handwritten | What it replaces |
|---|---|
| `core/args.rs` | argument parsing, reference-shaped help |
| `yaml.rs` | a YAML subset (frontmatter) |
| `b64.rs` | base64 |
| `core/sha256.rs` | SHA-256 (OAuth PKCE S256) |
| `core/db.rs` | monotonic ULIDs and the turn-timestamp format |
| `jsonfmt.rs` | reference-style JSON serialization |
| `core/text.rs` | string helpers (char boundaries, truncation, ANSI strip) |
| `core/paths.rs` | directory walks (`ancestors`/`nearest_dir_up`/`dirs_up`, shared by commands-dir and extension discovery) |
| `term/render.rs` | ANSI rendering + the shared TaskView |
| `core/render_md/` | terminal markdown incl. CJK cell widths (one streaming engine; replay resolves its setext lookahead up front) |
| `theme.rs` | the color theme (one cached `Palette` per stream) |
| `platform/` | terminal/shell/editor/pager/clipboard + `platform::interrupt` (the cooperative interrupt flag, re-exported by core); a PowerShell payload forces UTF-8 output and appends the explicit exit that makes a native command's code visible |
| `term/lineedit.rs` | the raw-mode line editor and picker |

Extend these instead of adding crates. When a new crate is genuinely justified (correctness or efficiency: `unicode-width` was), say so here and in the commit. Hand-rolled code mimicking the original's behavior is the norm here, not tech debt.

## Constraints a change must not break

- **Sync throughout.** No async runtime, no `block_on`; blocking work goes on a worker thread that polls `platform::interrupt` (the pattern `core/http.rs` uses), never on a second executor.
- **Never hand-write an escape sequence outside `theme.rs`.** Color, cursor control and the color-off paths all live there; a literal `\x1b[` elsewhere is a bug even when it looks right.
- **The store is append-only and lossless.** A thread file is the transcript: turns are appended, and anything compaction removes is archived (see `docs/architecture.md`): nothing is silently dropped, and a corrupt file fails loudly rather than being repaired by guesswork.
- **Fail loudly.** Prefer an error over a default-on-failure: an unresolvable model, a corrupt file or a refused action surfaces; fallbacks that hide a real problem are the thing to avoid.
- **`--json` is a contract.** The one-shot non-interactive mode prints line-delimited events on stdout and nothing else (the same usage accounting and persistence as the terminal path); approvals and diagnostics go to stderr, and the interactive session refuses the flag.

## Architecture

Module map:

| Path | Role |
|---|---|
| `main.rs` | entry: console init, the broken-pipe hook, `dispatch(argv)` |
| `providers/` | wire adapters (openai-compat, anthropic, codex + the ChatGPT OAuth login), the unified `Msg`/`ToolDef`/`ToolCall` model, the provider catalog |
| `core/http.rs` | sync POST + SSE, the retry/error taxonomy |
| `core/` | config, threads store, attachments, text, render_md, paths, templates, export |
| `term/` | line editor, picker, render/TaskView, ticker |
| `agent/` | the loop, tools, approval, session, compact, ext host, memory, system_prompt |
| `read/` | streaming text-file reading behind the read tool |
| `commands/` | CLI entries: agent, pkg, export, threads (plus library-only login/models) |

Deep detail on any of these (request flow, loop discipline, tool/approval rules, the extension host and the rendering contract) is in `docs/architecture.md`.

## Storage

All under `user_dir()`, overridable via `YAK_USER_PATH`; `~/.yak` on every platform.

- `threads/<ulid>.jsonl` (one file per conversation, one `StoredTurn` JSON object per line, one line per agent round appended at the round boundary (a `v` format stamp first) `THREAD_FORMAT_VERSION` (2; version 1 lines predate the `parent` edge), absent on pre-versioning lines: then id, `parent` (optional, present only on the round after a `/tree` jump: the turn it branches off; absent = the line before), ts, mode, model, cwd, system, prompt, response, reasoning, usage, options, and the round's wire `messages` as the same `providers::Msg` values the request carries). The file is a tree read through `chain_indices`: the active branch is the walk from the last line back to the root along `parent` edges, and a jump appends its branch rather than rewriting anything — the store is append-only, abandoned branches stay in the file. `usage` keeps the round's cache split (`[input, output, cached, cached_write]`; the `[input, output]` pair older lines hold still reads), and a resume seeds the session's totals from it — along the active chain only. `append_turn` appends a line (a None thread id starts a fresh thread); `read_thread` returns turns oldest-first under two disciplines: a corrupt line mid-file fails loudly (refusing a damaged thread beats silently resuming without a turn) while a torn final line (a crash mid-append) is the one bounded repair: dropped with a warning. A turn stamped with a future format is refused with the upgrade path named. `latest_thread`/`resolve_thread`/`fork_thread`/`recent_threads` serve the resume and list surfaces (the list falls back to the last parseable line, so a torn tail never hides a thread; each row is summarized from a bounded tail read: a count past the window is a shown lower bound, never a full-file read). Attachments persist as provenance, not pixels (path/url/mime: `stored_messages` strips the payload so a thread file stays small, however many screenshots ride it); `rebuild_turns` reloads local files on resume, and a record whose bytes are gone rides as a text note in place of its block. An attachment rides as the bytes the file holds: nothing is re-encoded, so an oversized screenshot costs what its pixels cost. Every image in a replayed history is re-billed on every request, so `session::budget_images` (run on every round, before pricing) drops the pixels from images outside the newest two user turns, tool results included (a screenshot can enter as a `read` result; provenance stays, so the adapters render the dropped payload as a note), and a resume rehydrates only from that window's start: an image out of the window is never decoded and never read off disk. Every agent session persists unless `--no-session`; there is no global logging switch.
- `config.json` (the single settings file (0600, `jsonfmt::dumps_indent(2)`, merge-preserving hand-added keys): `providers` with inline `api_key` supporting `${ENV_VAR}` expansion everywhere, the top-level `models` family (`default` + optional `thinking`) one shared model every mode starts on; the per-model `options` table (values string or number, a bare token count is legal), where `context_window` (an optional per-model token count) anchors auto-compaction to `window - 16384`, the reference's rule), the `agent` behavior section (approval/tools, `compact_at_tokens` (an optional tightening of that line to `min(configured, window - 16384)`, `0` off; with no window known automatic compaction is off) `keep_recent_tokens` (the tail each compaction keeps, 20k by default), `tools` policies, `cache_ttl`: how long a provider should hold this conversation's prompt-cache entry, `5m` (the API default) or `1h`, for the wires that take one), the `aliases` object (hand-edited; no CLI command edits it), and the two plugin tables (`extensions.disabled`/`tool_timeout`). This file deliberately deviates from the reference's config.toml + keys.json + default_model.txt + model_options.json split.
- `auth.json`: OAuth credentials for the subscription channels (`chatgpt` today), one entry per provider name — `access`/`refresh`/`expires`, 0600, written by `/login`'s browser flow and refreshed in place; `/logout` of the provider drops its entry.
- `history.jsonl`: the REPL input history: one `{"ts", "text"}` object per line, single-write appends (concurrent processes never interleave), 0600, adjacent submissions deduped at record time, rewritten down to a 1600-entry soft cap once past 2000; the in-memory window is the last 200.
- `blacklist`: the command blacklist file (plus a project `.yak/blacklist`).
- `YAK.md`: global user memory, hand-edited or appended by the `memory` tool when the user
  asks to note something durable; injected into the agent system prompt (`agent/memory.rs`).
- `extensions/`, `skills/`, `commands/`: user-side plugin, skill and prompt directories (skills are mounted by the `examples/extensions/skills` plugin — the core only knows the extension protocol).
- `pkg/`: packages installed with `yak install`.
- `tmp/`: the editor's scratch dir (pasted clipboard images, ctrl+g buffers) and the rustc compile cache for `.rs` script tools (`tmp/rs-cache/`), swept of anything older than a week at every agent start (`core/tmp.rs`).

## Workflow

- Commits: imperative, lowercase, no prefix ("add interactive agent repl with slash commands"), one focused change each, a subject line only.
- PRs target `main`; releases are cut from `v*` tags by `.github/workflows/release.yml`.
- Run `cargo fmt` before committing and keep `cargo clippy --all-targets` at zero warnings.
- When flags, help text or behavior change, update the matching README block (byte-identical `-h` output) and the relevant `docs/` file. `docs/architecture.md` owns the *how* of the request path, loop, tools, extension host and rendering; `docs/extensions.md` owns the plugin wire protocol and manifest. Keep this file to the map, the storage layout, the constraints and the workflow: new detail goes where its home is.

- Prefer failing loudly over defensive fallbacks: a corrupt file, an unresolvable model, or a refused action should surface, not be silently defaulted away.
