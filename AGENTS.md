# AGENTS.md

Single contributor guide and authoritative architecture reference for `llm`.

## What this is

`llm` is a single-binary, terminal-first coding agent in Rust, the reference-shaped: one executable, one
agent (ten built-in tools, approvals, skills, memory, compaction, an interactive REPL), a
thread-file session store, and an out-of-process extension host standing in for the reference's TypeScript
extensions. On top sit kept extensions the reference lacks: multimodal input (`-a/--attachment`
path/URL/stdin + `--at PATH MIMETYPE`; in a session ctrl+v pastes the clipboard image as a
temp-file path and any local image path in a message auto-attaches).



- `the behavioral spec` — agent loop, tools, streaming REPL, extension/packages shape.
- `the approval reference` — the approval model.
- `~/work/references/llm/` — historical: only the JSONL thread-store shape and ULIDs survive
  from it.

When command semantics, flags, storage or output formats are unclear, check the references
there. This file is the authoritative architecture reference.

## Commands

```bash
cargo build                  # debug
cargo build --release        # release profile: thin LTO, strip, codegen-units=1
cargo test                   # inline #[cfg(test)] modules across the tree
cargo test <name>            # one test by name
cargo fmt                    # before committing
cargo clippy --all-targets   # keep at zero warnings
```

- Tests are one behavior per test under a descriptive name (`boundaries_clamp_to_char_edges`,
  never `test_foo`). A test that needs a scratch directory takes one from `core/testutil`
  (`scratch_dir` creates it empty, `scratch_path` hands back an absent path for the tests that
  assert on what a missing tree does) — never hand-roll `env::temp_dir().join(...)`, and never
  key a name on `process::id()` (it does not separate the parallel threads of one test binary).
- Smoke-test the binary hermetically: every path resolves under the override, so nothing touches
  the real user dir.

  ```bash
  LLM_USER_PATH=/tmp/x cargo run -- "task"
  ```

- End-to-end: a python `http.server` mock returning OpenAI-style SSE on `127.0.0.1` (or
  Anthropic-style `event:`-framed SSE for `/v1/messages`) plus a `config.json` pointing at it
  exercises any full path. Start server and client in the same script; backgrounded servers do
  not survive between tool calls, and pass `< /dev/null` — the CLI reads piped stdin as prompt
  text by design. `.github/ci_e2e.py` is the working instance; run it locally against a fresh
  build with `python .github/ci_e2e.py target/debug/llm`.
- CI (`.github/workflows/ci.yml`) runs test → release build → `ci_e2e.py` → `ci_repl.py` on
  ubuntu, macOS and windows-latest. The two Python scripts are CI-local harnesses, not shipped
  code; `ci_repl.py` covers the interactive pty paths (multiline keys, kitty CSI-u folding,
  `!` tab completion, the ctrl+g editor round-trip) and skips itself on Windows, so run it on
  Unix to exercise the terminal.
- `README.md` embeds every command's full `-h` output verbatim. When flags or help text change,
  refresh the matching block there too and keep it byte-identical.

### Environment overrides

- `LLM_USER_PATH` — relocate the whole user directory (state root, see Storage).
- `LLM_MODEL` — beats the stored default, loses only to `-m`.
- `LLM_SHELL` — the shell behind every spawned command (agent bash, `!cmd`); platform default is
  `sh`/PowerShell.
- `LLM_SESSION_ID` — pins the per-conversation `x-opencode-session` id across invocations.
- `COLUMNS`/`LINES`, `TERM`, `COLORTERM`, `NO_COLOR`, `CLICOLOR_FORCE` — terminal sizing and the
  color gate. `EDITOR`/`VISUAL` — the ctrl+g round-trip. `HOME`/`USERPROFILE` — user dir root.
  Model traffic honors the usual proxy variables through `ureq`.

### Releases

Remote is `github.com/imjiaoyuan/llm` (branch `main`); tagged `v*` cuts releases
(`.github/workflows/release.yml`, six targets: aarch64/x86_64 Apple, x86_64 Windows, and
x86_64-gnu plus x86_64/aarch64-musl Linux; sha256 checksums). Repo-root `install.sh`/`install.ps1`
are the portable installer/updaters over those assets: re-running resolves the latest release and
compares versions (`updating old -> new`; equal versions stay unless `LLM_FORCE=1`), with
`LLM_VERSION`/`LLM_REPO`/`LLM_INSTALL_DIR` overrides and a user-level `~/.local/bin` everywhere.
Keep them in sync with the release asset names (`llm-<target>.tar.gz` + `.sha256`). The Pages
site (`https://jiaoyuan.org/llm/`, source = main root) serves only the two installer scripts,
never binaries.

## Hard constraint: minimal dependencies

Four crates total: `ureq` (sync HTTP+TLS), `serde`/`serde_json` (with the `preserve_order`
feature — insertion order matters for JSON output parity), and `unicode-width` (real terminal
cell widths; it hard-wraps the chrome: `$` action lines, tool
summaries and the logs transcript replay, so those rows match the terminal; the live answer
stream hard-wraps too so wrapped rows keep the left margin). No async runtime; the CLI is
synchronous throughout — keep new code sync.

Everything else is handwritten in-tree:

| Handwritten | What it replaces |
|---|---|
| `core/args.rs` | argument parsing, reference-shaped help |
| `yaml.rs` | a YAML subset (frontmatter) |
| `b64.rs` | base64 |
| `core/db.rs` | monotonic ULIDs and the turn-timestamp format |
| `jsonfmt.rs` | reference-style JSON serialization |
| `core/text.rs` | Damerau-Levenshtein and string helpers |
| `gitignore.rs` | gitignore matching (blacklist lines) |
| `core/paths.rs` | directory walks (`ancestors`/`nearest_dir_up`/`dirs_up`, shared by skills, commands-dir and extension discovery) |
| `term/render.rs` | ANSI rendering + the shared TaskView |
| `core/render_md.rs` | terminal markdown incl. CJK cell widths (`MdStream` for replay + `StyleStream` for live streaming) |
| `theme.rs` | the color theme (one cached `Palette` per stream) |
| `platform/` | terminal/shell/editor/pager/clipboard + `platform::interrupt` (the cooperative interrupt flag, re-exported by core) |
| `term/lineedit.rs` | the raw-mode line editor and picker |

Extend these instead of adding crates. When a new crate is genuinely justified (correctness or
efficiency — `unicode-width` was), say so here and in the commit. Hand-rolled code mimicking the
original's behavior is the norm here, not tech debt.

## Architecture

Module map:

| Path | Role |
|---|---|
| `main.rs` | entry: console init, the broken-pipe hook, `dispatch(argv)` |
| `providers/` | wire adapters, the unified `Msg`/`ToolDef`/`ToolCall` model, the provider catalog |
| `core/http.rs` | sync POST + SSE, the retry/error taxonomy |
| `core/` | config, threads store, attachments, text, render_md, paths, templates, export |
| `term/` | line editor, picker, render/TaskView, ticker |
| `agent/` | the loop, tools, approval, session, compact, ext host, skills, memory, system_prompt |
| `read/` | streaming text-file reading behind the read tool |
| `commands/` | CLI entries: agent, pkg, export, threads (plus library-only login/models) |

Full request flow, loop discipline, tool/approval rules, extension protocol and rendering
contract: `docs/architecture.md`. Extension wire protocol and manifest reference:
`docs/extensions.md`.

### Request flow

`main.rs::dispatch` routes only `--version`/`--help`, the `export` command, and the package
commands (`install`/`remove`/`uninstall`/`list`). **Everything else is
`commands/agent.rs::run(argv)`** — bare `llm` on a terminal is the interactive REPL, text and
piped stdin are one-shot tasks (the reference's shape; there is no typo guard, an unknown word is a task).
Remaining subcommand modules are `pub fn run(argv: &[String]) -> i32` parsing their own flags
through `args.rs` specs.

Model resolution is one shared chain, `providers::resolve_run_model` (`-m` > `LLM_MODEL` > the
session's model > the stored default; a bare model name served by more than one provider errors
listing the `provider/model` candidates instead of silently taking the first in config order).
Every provider `/models` fetch and messages API builds headers through
`providers::auth_headers(kind, key)` (Anthropic: `x-api-key` + `anthropic-version`;
openai-compat: Bearer).

A command resolves a model from config → `providers::ResolvedModel::stream()` dispatches on the
config `kind` (`openai-compat` or `anthropic`) → `core/http.rs` does sync POST + SSE parsing
under a the reference shape, deliberately lean error taxonomy (`HttpError::class()`:
Connection/RateLimited/Server retry, Auth/InvalidRequest fatal, ContextTooLarge sniffed from 400
bodies — OpenAI "maximum context length", Anthropic "prompt is too long", Google "exceed context
limit" — Stream for post-start drops, Interrupted for esc). Retries resend with ±10%-jittered
backoff (1s→30s for responses, a separate 5s→60s×6 budget for connection failures — bounded for
an attended terminal: ≈ three minutes of automatic fighting, then the error surfaces), honoring
a server `Retry-After`, sleeping in 50ms interruptible slices with a dim `retrying in Ns` notice,
extracting `x-request-id`/`cf-ray` into the error text, and never replaying after output was
already handed out. A mid-stream drop instead keeps the partial answer as a real assistant
message and continues from it (assistant-last is a prefill for both wire shapes), bounded at 5
recoveries per run; a stream that ends without a completion marker (`[DONE]`/`message_stop`) is
surfaced as truncation rather than a clean turn. An Anthropic `message_start` seeds the
input/cached token counts the edge merges with the `message_delta` output counts; a stream silent
past 300s is an idle error. The body read runs on its own thread with 100ms `recv_timeout` slices
checking the interrupt flag, so esc works during silent thinking stretches; the whole blocking
request phase (DNS, TCP, TLS, body upload, response headers, plus attachment GETs) runs on a
worker polled the same way (`send_raw_interruptible`/`get_with`).

### The agent

`src/agent/` is the whole binary: a sync loop over the same `Event` stream.

- **Tools** (`tools/`, ten handwritten, one file per tool behind the shared `mod.rs`):
  - `update_plan` — the reference shape checklist (`{step, status}`, at most one `in_progress`).
    It touches nothing, so it is Read-tier and never prompts, and the plan lives in the model's
    own tool call — no harness-side plan state.
  - `read` — one `path` or a `paths` batch of up to 5 (both share `read_one`).
  - `write`, `edit` — exact-match spans, both landing through a same-dir temp file + rename
    (`core/fsx.rs`) so a crash mid-write cannot truncate the target, carrying the existing mode
    over. `edit` has a whitespace-flexible fallback (per-line trailing whitespace, CRLF, smart
    quotes/dashes — the reference's fuzzy matching) so a near-miss oldText still applies.
  - `bash` — streams live through `platform::run_shell_stream` in a new session/process group; a
    timed-out kill keeps the partial output it already printed and reports
    `Command timed out after Ns (process killed)`.
  - `grep` — literal by default; `regex: true` passes the pattern to ripgrep, so a regex lookup
    stays inside the tool instead of becoming a bash pipeline.
  - `glob`, `ls`, `webfetch`.
  - `recall` — `observation_path` accepts only a plain alphanumeric id (a model-supplied `/` or
    `..` is refused, not cleaned, so it can never leave `~/.llm/observations/`); char-offset
    paging, `next_offset` continues.
  - `write` and `edit` declare an optional `then_run` whose execution lives in the loop, not the
    tool: `fuse_then_run` runs it as an ordinary `bash` call through the same
    gate/approval/blacklist path and folds the output into the mutation's result (skipped when
    the mutation failed; a failing follow-up is reported without turning the applied edit into an
    error). Action fusion never widens the trust surface. `bash` previews show as `run` via
    `display_verb`.
- **Approval** (`approval.rs`): read/write/exec tiers, a readonly-command whitelist, and the
  hardcoded refusal list that applies in either mode. Yolo is the default; `--approval-mode ask`
  or config `approval_mode = "always-ask"` restores prompting. Per-tool policies live in
  `[agent] tools` (`allow`/`deny`/`prompt`) and `--tools` narrows the set.
- **Blacklist** (`blacklist.rs`): gitignore-style files (`~/.llm/blacklist`, project
  `.llm/blacklist`, project lines win) that only *add* refusals on top of the hardcoded ones.
- **Compaction** (`compact.rs`): estimate tokens (last usage + chars/4 tail), cut at a
  boundary, summarize, plus a tool-result pruner at compaction pressure
  (`prune_tool_results`: results over 8192 chars become head 4096 + a middle marker + tail 1024
  before the summarizer runs, reporting `freed_tokens` — the loop subtracts them from the
  usage-derived estimate). Lossless, not lossy: the untouched original is archived to
  `observation_dir()` (`~/.llm/observations/<content-id>.txt`, the 64-bit FNV-1a of the text, so
  pruning the same result again after a resume rewrites one file rather than piling up copies)
  and the marker carries that id so `recall` pages the cut middle back. Per-result fail-open on
  archive write failure (keep the whole text rather than leave a marker pointing at nothing), and
  char-indexed cuts keep CJK on codepoint boundaries. A resumed thread that no longer fits is
  projected down the same way before its first request, silently (`Session::prune_seed_to_fit`),
  so the notice cannot repeat turn after turn.
- **Memory** (`memory.rs`): `~/.llm/LLM.md`, one manual region, 16KB cap. `section()` injects
  into the system prompt and names the path even when the file is absent or empty — the block is
  what tells the agent where durable preferences go, so it must never be conditional. There is no
  `/memory` command and no `remember` tool: memory is manual-only.
- **Skills** (`skills.rs`): `SKILL.md` discovery, `skills_block()` caps the list at 2000 chars
  with per-entry trigger lines capped at 160 — the list rides every request, the full file is one
  `read` away.
- **System prompt** (`system_prompt.rs`): built-ins + memory + `AGENTS.md`/`CLAUDE.md` project
  discovery (first walking up from cwd, stopping at the git root; `AGENTS.md` wins when both
  exist) + an environment line (OS · shell · git repo · project type with its verify command) +
  skills + a self-extension block naming the extensions/skills dirs and the manifest-header form.
  Project instructions are embedded whole at any length.
- **`--json`** replaces the terminal UI of a *one-shot* task with line-delimited events
  (`text`/`reasoning`/`tool_start`/`tool_log`/`tool_end`/`turn_end`/`result`) so an
  out-of-process consumer (CI, an editor, `examples/extensions/subagent.py`) can watch a run —
  the same usage accounting and persistence as the terminal path, approvals and diagnostics on
  stderr, stdout nothing but events. A missing task is a usage error, and the interactive
  session refuses it.

### Extensions

The plugin surface: out-of-process executables in `~/.llm/extensions/` or the project
`.llm/extensions/` (project wins by name), discovered through `agent/ext/roots.rs` (package
dirs first, then project, then user). Two shapes, chosen by the file itself:

- **Script tool** — a script with a `# --- llm-tool: <name>` header (`description:`, `args:`,
  `arg-mode:`, `interpreter:`, `timeout:`), run per call with stdout as the result.
- **Resident extension** — no header: started once per session, line-delimited JSON on stdio
  (`initialize` → tools/commands/events, then `call_tool`/`run_command`/`event`).

Every extension tool is Exec-tier, so ask mode prompts for every call (yolo runs them free) and
per-tool policies still win. A slow or broken extension mounts nothing and never blocks the
session; tool calls time out at 120s (`extensions.tool_timeout`) unless the extension asks for
its own deadline at `initialize`, events after 5s. `extensions.disabled` skips one by file stem
(or a script tool's manifest name), and `/reload` restarts them all. There is no trust store and
no restart backoff — consent-by-presence: a file dropped in an extensions dir may spawn,
inheriting the ambient env. The `tool_call` event is the gate: an extension can deny a call or
rewrite its arguments.

Commands-dir prompts (`core/commands_md.rs`: `~/.llm/commands/*.md` plus the nearest
`.llm/commands/`, project wins) have no CLI dispatch — the REPL's unknown `/name` falls back to
them, expanding `$input` through `core/templates.rs`.

Runnable examples live in `examples/extensions/`: `wordcount` (a script tool), `websearch` and
its TypeScript twin `websearch.ts` (a resident extension offering `web_search` plus a `/web`
command), `template.py`/`template.js` (self-contained starting points for a resident extension),
`repeat_guard.py` (a `tool_call` deny gate that breaks identical-call loops), `fold_repeats.py`
(folds repeated lines in a tool result), `subagent.py` (mounts a `subagent` tool that runs a
child `llm --json` in its own context window with a `*.md` agent definition from
`examples/agents/`) and `mcp_bridge.py` (a resident extension that mounts MCP servers as tools
from an `mcp.json` beside the script, stdio or streamable HTTP, one `<server>__<tool>` per tool —
this *is* the MCP support). Refresh `docs/extensions.md` and the examples together when the host
changes.

### Provider/model lifecycle

REPL-internal: `commands/models.rs` and `commands/login.rs` are library-only, no CLI. `/model`
runs the provider→model→thinking cascade (`cascade_model_picker`), switches the live session via
`Session::switch_model` (`providers::resolve_model_by_id`) and saves the choice as the shared
default; `/thinking` adjusts the depth alone; `/login` runs the wizard (catalog +
Cloudflare/Azure URL templates + custom; hidden-input key capture, an omitted key falls back to
the catalog entry's env var as a `${VAR}` reference); `/logout` the removal picker (clearing the
default when it pointed at the removed provider). Pickers build from `providers/catalog.rs` (38
entries incl. opencode-go in both wire kinds and four local runtimes: ollama, lm-studio,
llama.cpp, vllm; OAuth-only and cloud-signature providers deliberately absent). A fresh session
starts on the stored default.

### Reading and rendering

- `src/read/` is the text-file module behind the agent's read tool: streaming line windows
  (`window()`: BufReader, offsets skip without keeping; the tool clamps to a 2000-line window
  under the shared 50KB cap — the reference's values, so one read covers a typical source file), per-line
  2000-char cap, one lookahead line deciding `Exact` vs `AtLeast` totals, the `BINARY_EXTS` gate
  + NUL sniff, and `binary_hint` mapping binary formats to local tooling (pdf→pdftotext,
  bam/cram→samtools, parquet/hdf5→duckdb, office→libreoffice). Memory stays bounded by the
  window; nothing but text is ever parsed.
- Rendering is on only for a real terminal. The answer text (markdown streaming + replay)
  renders the reference's dark palette (`mdHeading` #f0c674, accent #8abeb7, code-block green #b5bd68, link
  #81a2be, gray quotes/rules — truecolor where `COLORTERM` says so, nearest-256 otherwise),
  while all chrome keeps the original near-monochrome set byte-for-byte — default terminal color
  for content, bold for emphasis, gray (90/2) for chrome and metadata, red reserved for errors.
  The only hues are the green preview text on `$` tool lines and the bold-cyan `Allow?` question
  (diffs: additions default, deletions gray, headers/context dim). `theme::out()`/`theme::err()`
  cache one `Palette` per stream and `NO_COLOR`, `TERM=dumb` and non-terminal streams strip every
  SGR byte so redirected output and logs stay escape-free (cursor control like the spinner's
  `\r\x1b[2K` is not color and stays). **Never hand-write an escape sequence outside
  `theme.rs`.**
- `term/lineedit.rs`: raw-mode line editor (VMIN=0/VTIME=1 polled reads where `read()==0` means
  timeout, never EOF — EOF is the 0x04 byte), the KeyWatcher steering buffer, and the one picker
  `pick(title, items, echo)` used by every list in the tool (arrow-move + type-to-filter in the
  fuzzy-filter shape: UTF-8 accumulates across bytes so CJK filters work, space-separated terms
  AND-match case-insensitively, enter returns the original index). `pick_multi` is the checkbox
  variant, used only by `llm install`'s item selection. The buffer is multiline: `\r` submits
  while ctrl+j, alt+enter, shift/ctrl+enter, a lone `\` before enter and bracketed paste insert
  real newlines; the kitty keyboard protocol's disambiguate flag is pushed only for the read's
  duration and kitty's CSI-u re-encoding of plain ctrl+letter is folded back to the C0 byte. A
  lone or CSI-27u ESC interrupts like ctrl-c. ↑/↓ recall history only while browsing, from an
  empty buffer, or parked at the very start of the top line (the working draft is saved on first
  recall and restored on the way back); otherwise they walk logical lines with a sticky preferred
  column. Readline keys round it out (ctrl-a/e, ctrl-k/u/w + alt-d into a single-entry kill
  buffer, ctrl-y, alt-b/f, ctrl-arrows); ctrl+g round-trips the draft through `$VISUAL`/`$EDITOR`;
  pastes over 10 lines or 1000 chars become an atomic `[paste #N +L lines]` token.

### The REPL and session surfaces

- The interactive REPL (`agent/repl.rs`): prompt loop, slash commands, banners, model pickers.
  ctrl-c or esc interrupts a run; typed input while it works is queued as steering and delivered
  at the next tool boundary. `!cmd` runs a shell command; tab completes command names and paths.
- `llm -r` (`commands/threads.rs`, the browser): one filterable list of recent threads through
  the shared picker — scoped to the current directory (`threads::Store::recent_threads(limit,
  Some(cwd))` filters on the last turn's `cwd`, oldest spelling variants included via `Path`
  comparison), falling back to every directory when this one has no history, each row then
  tagging its directory; selecting shows the transcript, then one question continues that
  conversation via `run`. `/resume` shares `threads::pick_thread`. `--session`/`--cid` accept
  unambiguous id prefixes via `threads::Store::resolve_thread(prefix, Some(cwd))` — a prefix
  matching several threads resolves to the local one — and `-c` takes `latest_thread(Some(cwd))`
  before the global newest.
- `llm export [PATH]` (`commands/export.rs` → `core/export.rs`) writes the working directory's
  newest thread — else the newest anywhere, i.e. what `-c` would continue — as markdown; the
  REPL's `/export [PATH]` shares `export_thread` for the live session id, defaulting to
  `llm-<id>.md`.

### Packages

`commands/pkg.rs`: `install git:github.com/user/repo[@ref]` clones into `~/.llm/pkg/<name>`,
`-l` project-local into `.llm/pkg/` (`-g` the explicit default), re-running refreshes unless
pinned, plus `remove`/`list`. `pkg::carried` classifies a clone by layout — `skills/`,
`extensions/`, `commands/` and a root `SKILL.md` (a whole-repo skill, mounted by
`skills::load_package`, which the bare `skills/` walk would miss). On an attended terminal a bare
`install` asks both questions — a `pick` for the scope, then a `pick_multi` checkbox list of
everything the repo carries — while a piped/CI install (no tty) takes the defaults, global +
everything. `-s NAME` (`'*'` = all) records a skill selection; every keep list lives in the
clone's git config (`llm.skills`, `llm.extensions`, `llm.prompts`; a name list, `*` = all, `-` =
nothing), read by `pkg::selected` and applied at discovery (`skills::load_package` for skills,
`pkg::extension_kept` in `ext/manifest.rs` for extensions, `pkg::prompt_kept` in
`commands_md.rs` for prompts), so a plain refresh keeps it (alongside `llm.pinned`).

## Storage

All under `user_dir()`, overridable via `LLM_USER_PATH`; `~/.llm` on every platform.

- `threads/<ulid>.jsonl` — one file per conversation, one `StoredTurn` JSON object per line (a
  `v` format stamp first — `THREAD_FORMAT_VERSION`, absent on pre-versioning lines — then id,
  ts, mode, model, cwd, system, prompt, response, reasoning, usage, options, and the round's wire
  `messages` as the same `providers::Msg` values the request carries). `append_turn` appends a
  line (a None thread id starts a fresh thread); `read_thread` returns turns oldest-first under
  two disciplines — a corrupt line mid-file fails loudly (refusing a damaged
  thread beats silently resuming without a turn) while a torn final line (a crash mid-append) is
  the one bounded repair: dropped with a warning. A turn stamped with a future format is refused
  with the upgrade path named. `latest_thread`/`resolve_thread`/`fork_thread`/`recent_threads`
  serve the resume and list surfaces (the list falls back to the last parseable line, so a torn
  tail never hides a thread). Attachments persist as metadata only (path/url/mime); resume
  replays text only. Every agent session persists unless `--no-session`; there is no global
  logging switch.
- `config.json` — the single settings file (0600, `jsonfmt::dumps_indent(2)`, merge-preserving
  hand-added keys): `providers` with inline `api_key` supporting `${ENV_VAR}` expansion
  everywhere, the top-level `models` family (`default` + optional `thinking` — one shared model
  every mode starts on; the per-model `options` table), the `agent` behavior section
  (approval/tools/skills, `context_window`, `reserve_tokens`, `keep_recent_tokens`,
  `tools` policies, `model_windows`, `disabled_skills`), the `aliases` object (hand-edited; no
  CLI command edits it), and the two plugin tables (`extensions.disabled`/`tool_timeout`).
  This file deliberately deviates from the reference's config.toml + keys.json +
  default_model.txt + model_options.json split.
- `history.jsonl` — the REPL input history: one `{"ts", "text"}` object per line, single-write
  appends (concurrent processes never interleave), 0600, adjacent submissions deduped at record
  time, rewritten down to a 1600-entry soft cap once past 2000; the in-memory window is the last
  200.
- `blacklist` — the command blacklist file (plus a project `.llm/blacklist`).
- `LLM.md` — hand-edited user memory, injected into the system prompt.
- `extensions/`, `skills/`, `commands/` — user-side plugin, skill and prompt directories.
- `pkg/` — packages installed with `llm install`.
- `observations/` — archived full tool results, keyed by content id, paged back by `recall`.

## Workflow

- Commits: imperative, lowercase, no prefix ("add interactive agent repl with slash commands"),
  one focused change each, a subject line only.
- PRs target `main`; releases are cut from `v*` tags by `.github/workflows/release.yml`.
- Run `cargo fmt` before committing and keep `cargo clippy --all-targets` at zero warnings.
- When flags, help text or behavior change, update the matching README block (byte-identical
  `-h` output) and the relevant `docs/` file. `docs/architecture.md` owns the *how* of the
  request path, loop, tools, extension host and rendering; `docs/extensions.md` owns the plugin
  wire protocol and manifest.

  or build them.
- Prefer failing loudly over defensive fallbacks: a corrupt file, an unresolvable model, or a
  refused action should surface, not be silently defaulted away.
