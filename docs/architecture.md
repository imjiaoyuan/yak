# Architecture

Deep implementation detail: read this when changing the request path, the agent loop, the tool
registry, the extension host or the terminal presentation. `docs/extensions.md` covers the plugin
surface; this file is where the *how* lives — request flow, loop discipline, tool and approval
rules, the extension host, the rendering contract.

Model resolution is one shared chain, `providers::resolve_run_model` (`-m` > `LLM_MODEL` > the
session's model > the stored default; a bare model name served by more than one provider errors
listing the `provider/model` candidates instead of silently taking the first in config order), and
every provider `/models` fetch plus the messages API build headers through
`providers::auth_headers(kind, key)` (Anthropic: `x-api-key` + `anthropic-version`; openai-compat:
Bearer).

Request flow: `main.rs::dispatch(argv)` routes to a command module → the command resolves a model
from config → `providers::ResolvedModel::stream()` dispatches on the config `kind` (`openai-compat`
or `anthropic`) → `http.rs` does sync POST + SSE parsing under a the reference shape, deliberately lean
error taxonomy (`HttpError::class()`: Connection/RateLimited/Server retry, Auth/InvalidRequest
fatal, ContextTooLarge sniffed from 400 bodies — OpenAI "maximum context length", Anthropic "prompt
is too long", Google "exceed context limit" — PayloadTooLarge for a 413 (whose body is usually a
gateway wrapper naming nothing, so the size we sent and the shrink-the-attachments remedy are
appended to it), Stream for post-start drops, Interrupted for esc); each adapter's `run` builds its
body and hands it to `providers::check_request_body` first, which refuses a body past
`input.max_request_bytes` — `http::MAX_REQUEST_BYTES` (32MB, the documented provider ceiling)
unless `agent.max_request_bytes` in config.json lowers it, since a gateway in front may refuse far
less — while it can still name the
attachments that filled it: compaction prunes tool results, never attachment payloads, and a resumed
thread reloads every attachment it stored, so the remedy has to reach the user;
retries resend with ±10%-jittered backoff (1s→30s for responses, a separate 5s→60s×6 budget for
connection failures — bounded for an attended terminal: ≈ three minutes of automatic fighting, then
the error surfaces), honoring a server Retry-After when sent, sleeping in 50ms interruptible slices
with a dim `retrying in Ns` notice, extracting `x-request-id`/`cf-ray` into the error text, and
never replaying after output was already handed out — a mid-stream drop instead keeps the partial
answer as a real assistant message and continues from it (assistant-last is a prefill for both wire
shapes), bounded at 5 recoveries per run before the error surfaces with the partial in history; a
drop that arrives before any output — no text, no reasoning, no tool call — is resent unchanged
(nothing was handed out, so nothing can duplicate), sharing the same 5-recovery budget; a
stream that ends without a completion marker (`[DONE]`/`message_stop`) is surfaced as truncation
rather than a clean turn (a clean FIN on a half-delivered answer must not be stored as a finished
round), and an Anthropic `message_start` seeds the input/cached token counts that the edge merges
with the `message_delta` output counts a stream silent past 300s is an idle error, not a hung task.
The body read runs on its own thread with 100ms `recv_timeout` slices checking the interrupt flag,
so esc works during silent thinking stretches — and the whole blocking request phase (DNS, TCP, TLS,
body upload, response headers, plus attachment GETs) runs on a worker polled the same way
(`send_raw_interruptible`/`get_with`), because a connect black-holed by a dead network offers no
other hook for the flag: esc used to sit ignored until OS defaults gave up (Linux SYN retransmits ≈
2min; a TLS handshake to a black-holed host, the 1800s global ceiling). The dial itself is bounded
too — `timeout_resolve(5s)`/`timeout_connect(10s)` on the shared agent (ureq leaves them unlimited)
so a dead endpoint fails into the retry path in seconds, while a real stream is untouched (those
bound dialing only, the global 1800s still covers generation) → events flow back as `http::Event`
(`Delta` / `ReasoningDelta` / `Done { usage }`), consumed by `term/render.rs::TaskView`, the
presentation shared by the REPL and the one-shot agent — spinner (the redraw thread is
`term/ticker.rs`) plus the typewriter heartbeat (`DrainTicker` in the same file): settled stream
text lands in a backlog and drains in one time-gated installment per 16ms tick — base 2 chars
(≈125/s, readable), one more per 24 waiting, capped at 24 per tick so no frame ever dumps a chunk;
bursts read as fast typing that decelerates into the base rate, a keeping-up stream never paces, and
everything stays write-once (no erase, no redraw — pacing only decides when bytes are written); any
non-interrupt keystroke sets `screen().flush_now` and the next tick prints the whole backlog at once
(the user is watching), a single dim `thinking ... end` trace line (reasoning is never streamed
anywhere), and the dim `secs · this task: input N · output N · cache N%` footer (the cache
share only when the provider reports prompt-cache hits; all three are the task's own
rounds summed — a re-sent prompt per round, so never a context size — while the session's
cumulative counters and the window's occupancy live in `/status`); answers stream character-immediately and styled the reference-like in a left-indent-2 block
on a TTY through `StyleStream` (`core/render_md.rs`), write-once — no erase, no redraw: each line
classifies from its first chars (heading, quote, list, fence, table, rule) and the settled prefix
streams, while an unclosed inline marker (`**`, `` ` ``, `~~`, `[`) holds only its own span until it
resolves or the line ends (then it flushes literally — the one physical cost of write-once styled
output); rows hard-wrap at the terminal width (re-read at every line start, so a resize applies from
the next row; unicode-width cells, tabs advancing to the next 8-column stop from the absolute
column; a space within the last few cells breaks early so word middles are usually spared), open SGR
spans re-open after each break, list/quote/fence continuation rows align under the content, `|`
table rows pass through verbatim (live and replay alike — the simple way: column widths need the
whole table and write-once output cannot restyle printed rows), and an unclosed inline marker holds
at most 80 bytes before degrading to literal text so a prose bracket or stray backtick never stalls
the stream so wrapped continuation rows keep the margin instead of soft-wrapping to column 0 (only
the `>` prompt and `$` tool chrome sit at column 0), and the spinner never starts over a dangling
partial row (`Renderer::has_dangling` guards it), paints its first frame only after one tick and
retracts only a row it painted — a wait that ends at once (the round after a settled answer) must
not blink a frame under printed text, and `turn_end` restarts it only when one is already up (the
tool cycle's `resume_running`/`resume_wait` cover the waits): everything the answer area shows is
append-only, which `ci_repl.py` asserts by rejecting any erase sequence after the answer's first
byte; replay (`MdStream` via `render_once`) renders the same style vocabulary over whole lines — h1
color+bold+underline, h3+ keep their `### ` prefix, visible ``` fences (the border always prints as
three backticks, and a fence may be indented — the common shape inside a list item — in which case
its closing run must still close the block: the live stream holds a line that is only whitespace and
backticks until it knows, or the whole rest of the answer would render as code), `│ ` quotes,
four-space list nesting, `[x]` task markers, setext headings, `min(80, width)` rules — and for
well-formed (single-blank-separated) markdown the streamed answer and the replay are byte-identical;
pipes stay raw and unstyled. The agent loop feeds TaskView `AgentUpdate`s and keeps its tool `$`
chrome and approvals locally. Every outbound request carries a real `user-agent` (`llm/<version>`,
never ureq's library default — `http::identity_headers()`, merged in `send_raw`/`get_with`); the
provider adapters add the vendor half on top (`providers::gateway_headers(url)`, also on the
catalog's /models fetch): for `opencode.ai` hosts the `x-opencode-session` conversation id OpenCode
Go/Zen demands with a 400 `MissingSessionID` when it is absent — one ulid per process, pinnable
across processes with `LLM_SESSION_ID`.

- The agent is the whole CLI: `main.rs::dispatch` routes only `--version`/`--help`; **everything
  else is `commands/agent.rs::run(argv)`** — bare `llm` on a terminal is the interactive REPL, text
  and piped stdin are one-shot tasks (the reference's shape; there is no typo guard, an unknown word is a
  task). Remaining subcommand modules are `pub fn run(argv: &[String]) -> i32` parsing their own
  flags via `args.rs` specs; only two are left: the package commands (`commands/pkg.rs`: `install
  git:github.com/user/repo[@ref]` clones into `~/.llm/pkg/<name>`, `-l` project-local into
  `.llm/pkg/` (`-g` the explicit default), re-running refreshes unless pinned, `remove`/`list`;
  `pkg::carried` classifies a clone by layout — `skills/`, `extensions/`, `commands/` and a root
  `SKILL.md` (a whole-repo skill, mounted by `skills::load_package`, which the bare `skills/` walk
  would miss), reported by `install`/`list`; on an attended terminal a bare `install` asks both
  questions — a `pick` for the scope, then a `pick_multi` checkbox list of everything the repo
  carries — while a piped/CI install (no tty) takes the defaults, global + everything; `-s NAME`
  (`'*'` = all) records a skill selection, and every keep list lives in the clone's git config
  (`llm.skills`, `llm.extensions`, `llm.prompts`; a name list, `*` = all, `-` = nothing), read by
  `pkg::selected` and applied at discovery — `skills::load_package` for skills,
  `pkg::extension_kept` in `ext/manifest.rs` for extensions, `pkg::prompt_kept` in `commands_md.rs`
  for prompts — so a plain refresh keeps it (alongside `llm.pinned`); in the skills walk user pkg
  loads before project pkg, so project wins) and `export` (`commands/export.rs`: `llm export [PATH]`
  writes the working directory's newest thread — else the newest anywhere, i.e. what `-c` would
  continue — as markdown through `core/export.rs`; the REPL's `/export [PATH]` shares
  `export_thread` for the live session id, defaulting to `llm-<id>.md`).
- `providers/message.rs` holds the unified conversation model — `Msg` (User messages carry
  `attachments: Vec<Attachment>`; `Msg::user_with` builds them), `ToolDef`, `ToolCall` — so the
  agent depends on providers, never the reverse. A tool result carries an `error: Option<ToolError>`
  (`Failed`/`Denied`/`UnknownTool`/`Interrupted`) rather than a bare bool — the wire shapes only
  send the current `is_error`, but a stored thread keeps the class, and the old `is_error` boolean
  still reads as `Failed`. An assistant message keeps `reasoning: Option<String>` beside the opaque
  `reasoning_meta` the provider needs replayed with it (Anthropic's thinking `signature`): the trace
  without its signature is a 400 on the next turn, so both persist even when the visible text is
  empty. Both provider adapters serialize attachments per mime (`attachment_block`): image blocks,
  OpenAI `file`/`input_audio` plus a text part for `text/*`, Anthropic `document` (base64 PDF or
  plaintext `text/plain`/`text/csv`); unsupported mimes error client-side before any request leaves,
  naming the accepted set (`build_body` returns `Result` for exactly that); a record stored without
  its bytes — a resumed attachment whose source is gone — rides as a text note naming it, never an
  empty data URI.
- `core/attachments.rs` is the shared loader: `load_args()` runs the `-a`/`--at` entry-flag loop for
  prompt and agent; `Loaded` keeps path/url/mime/bytes provenance so one load feeds both the wire
  (`request()`) and the log store (`stored()`); magic-byte `sniff_mime` covers stdin and clipboard
  bytes; `wants_stdin()` lets `-a -` claim stdin away from the prompt text.
- The agent (`src/agent/`, the whole binary): sync loop over the same `Event` stream — `tools/` (ten
  handwritten tools, one file per tool behind the shared `mod.rs`: `update_plan` (the reference shape
  checklist — a `{step, status}` list with at most one `in_progress`; it touches nothing, so it is
  Read-tier and never prompts, and the plan lives in the model's own tool call so no harness-side
  state is needed), read (one `path` or a `paths` batch of up to 5, `read_one` shared by both
  shapes), write, edit with exact-match spans (both landing through a same-dir temp-file + rename so
  a crash mid-write cannot truncate the target, the existing mode carried over) plus a
  whitespace-flexible fallback (per-line trailing whitespace, CRLF, smart quotes/dashes — the reference's fuzzy
  matching) so a near-miss oldText still applies, bash streaming live through
  `platform::run_shell_stream` in a new session/process group (a timed-out kill keeps the partial
  output it already printed — the deadline and the output are independent facts — and reports
  `Command timed out after Ns (process killed)`), grep (literal by default; `regex: true` passes the
  pattern to ripgrep, so a regex lookup stays inside the tool instead of becoming a bash pipeline),
  glob, ls, webfetch, recall (`observation_path` accepts only a plain alphanumeric id — a
  model-supplied `/` or `..` is refused, not cleaned, so it can never leave `~/.llm/observations/`;
  char-offset paging, `next_offset` continues); write and edit declare an optional `then_run`
  command whose execution lives in the loop, not the tool — `fuse_then_run` in `mod.rs` runs it as
  an ordinary `bash` call through the same gate/approval/blacklist path and folds the output into
  the mutation's result, so action fusion never widens the trust surface (skipped when the mutation
  failed; a failing follow-up is reported without turning the applied edit into an error); bash
  previews show as `run` via `display_verb`), `approval.rs` (read/write/exec tiers + a
  readonly-command whitelist — `readonly_command`: every command position on the list, git/cargo by
  read-only subcommand, redirection, command substitution (`$(..)`/backticks) and flag-writes (`sort
  -o`, `date -s`) rejected, git `branch`/`remote`/`config` read-only only in their bare or
  one-argument read forms — so ask mode only prompts for state-changing work: writes, non-read-only
  commands, out-of-cwd reads; symlink-aware `escapes_cwd`; an extension's `tool_call` reply may
  deny, rewrite args, or `decision: "allow"` to skip the ask (a blacklist ask still prompts — the
  ask-list is extension-proof); yolo is the default mode (`--approval-mode ask` / `[agent]
  approval_mode = "always-ask"` restores prompting; the legacy `~/.llm/trust.json` is ignored — its
  trusted-project flip became dead weight once everything started in yolo), `/yolo` toggles the
  mode; the shell command gates are two layers in either mode: the hardcoded `FORBIDDEN_COMMANDS`
  (`approval.rs`: `sudo`/`su`/`doas`, `mkfs*`/`mkswap`, `dd`/`shred`/`wipefs`, `fdisk`/`parted`,
  `shutdown`/`reboot`/`poweroff`/`halt`/`init`) plus `forbidden_command`'s shape checks (fork bomb,
  redirects into real device nodes — `/dev/null` and other sinks are exempt, `2>/dev/null` is stream
  hygiene — and `rm` whose target word is `/`, `/*`, `~` or `$HOME`, matched on the whole word so
  `rm -rf /tmp/build` stays ordinary cleanup) are outright `Deny`, none of which any file, flag or
  `!` line can switch off; then the user-editable ask-list (`blacklist.rs`: `~/.llm/blacklist` plus
  the nearest `.llm/blacklist`, project lines winning by last match; word patterns hit a command
  position anywhere in the line, segment patterns match the whole segment, globs work, `!`
  re-allows), seeded with `rm` and `git push --force*`: a hit is an `Ask` in either mode — immune to
  allow policies — with the matched pattern highlighted in the prompt, and `a` spares the pattern
  for the session (`blacklist_session_allows`, never persisted), `session.rs` (the Session: one
  model + tools + accumulated history, thinking level, steer queue shared with the KeyWatcher, turn
  persistence (failed and interrupted rounds too, so a late stream drop cannot erase the transcript
  from /resume), the wire messages the thread file stores verbatim; `rebuild_tools` is the single
  registry-build path — built-ins + extension tools — shared by the CLI entry, `switch_model` and
  `/reload`), `ext/` (the extension host — `mod.rs` plus `manifest.rs` for script manifests,
  `proto.rs` for the stdio loops and `roots.rs` for discovery; the reference's extensions done out-of-process:
  user executables in `~/.llm/extensions/` plus the nearest `.llm/extensions/`, project winning by
  name (a package's extensions dir is filtered by that clone's `llm.extensions` keep list, so
  `install`'s checkbox can leave one dormant), in two forms — **manifest script tools** (a `# ---
  llm-tool: name` comment header on any script in any language; the host spawns per call, single
  argument rides as `argv[1]`, `run_with_progress` owns timeout/stdin/stdout/exit-code; no exec bit
  needed so Windows works too) and **resident extensions** (spawned once, speaking newline-delimited
  JSON — the `initialize` handshake (carries `"v": 1`) advertises tools/commands/events, `call_tool`
  runs a mounted tool, `run_command` answers a slash command, `event` fires
  `agent_start/input/turn_start/turn_end/tool_call/tool_result/agent_end` (5s deadline; a
  `tool_call` reply denies or rewrites args and gates before the approval matrix, a `tool_result`
  reply's `{"content": ..}` replaces the model-visible result — the event carries the full content,
  so subscribing is the opt-in; `Extensions::subscribes` skips building that payload when nobody
  listens, the loop re-caps the replacement through `tools::truncate_marked` (2000 lines/50 KB),
  last rewrite wins and every failure mode is fail-open, leaving the tool's own result; failures
  land in the extension's diagnostics tail, never aborting a run); writer/reader threads with an
  id-correlated pending map, parallel connect (started on a background thread at CLI startup so
  handshakes overlap the thread store, attachments and system-prompt reads; the CLI joins before
  building the tool registry) that degrades per extension, `/reload` respawns, `extensions.disabled`
  + `extensions.tool_timeout` in config, with a resident extension allowed to ask for a longer
  `tool_timeout` in its `initialize` reply (clamped to an hour — a tool that runs a build or another
  agent needs it) and ctrl+c sending the busy extension an `interrupt` frame naming the abandoned
  call so it can stop its own children; an extension's stderr is streamed into the running call's
  tool log as live progress (dim, never part of the result, so a long tool can report without
  polluting what the model reads); extension tools are Exec-tier; the wire protocol is fully
  documented in `docs/extensions.md` with runnable examples in `examples/extensions/` —
  `repeat_guard.py` gates `tool_call`, `fold_repeats.py` rewrites `tool_result`, `subagent.py`
  mounts a `subagent` tool that runs a child `llm --json` with its own tools and system prompt from
  `~/.llm/agents/*.md` / `.llm/agents/*.md` (defined and discovered entirely inside the extension;
  `examples/agents/` ships four)), `repl.rs` (slash commands `/help /model /thinking /login /logout
  /resume /tree /export /clear /skill:<name> /yolo /status /reload /exit` — /memory /init /settings
  /tools /skills /compact were removed: memory is hand-edited, config is hand-edited, compaction is
  automatic (token-threshold in the loop; `Session::compact_prefix` went with the command), and
  extension state shows in /status + /reload; `run_skill` submits a `<skill name dir>` task — the
  dir is what lets a slash-run skill read its own `references/`), `!cmd` shell passthrough with
  bash-style tab completion — command positions (first word, or right after `|`/`&`/`;`) complete
  executable names from `$PATH`, later words complete filesystem paths as typed, dirs getting a
  trailing `/`, hidden entries only after a dot prefix — unknown `/name` first asks the extension
  host (`Extensions::command_owner` → `run_command`, reply printed dim), then the commands dir, then
  plain task text; extension commands join the `/`-completions, a near miss of a known command only
  prints a did-you-mean and sends nothing; slash and `!` input echoes bold in the line editor;
  `/resume` loads a picked past conversation into the live session (rebuild_thread), `/tree` rewinds
  to a picked turn (seed and thread file truncate just before it —
  `threads::Store::truncate_thread`), `/reload` re-discovers skills + extension tools + settings
  (plugin files are otherwise read at startup only; restart or `/reload` picks up a mid-session
  change — no fingerprint probe runs per task anymore); the resume replay clips every row to the
  terminal width (`history_rows`, one line per entry, `truncate_cells` charging the ellipsis and CJK
  its two cells) so a long tool payload cannot bury the next row, and the message cap counts
  messages, not rows; the startup banner is the bold identity line plus dim label rows — work ·
  context, session, and the live plugin surface: `plugins` lists resident extensions and manifest
  script tools by name (`Extensions::plugin_names`, a resident one that never connected reads
  `(failed)`), `skills` the discovered skills; either row disappears when its list is empty, six
  names is the cap (`+N more`), and the ctrl+o page reprints the same rows; `--fork` branches the
  loaded session onto a new thread id sharing its turns so far via `threads::Store::fork_thread`),
  `compact.rs` (cut at a turn boundary keeping a 32k-token recent window; the stored Summary
  composes the original task verbatim above the compressed sections — re-compaction recovers it from
  the previous summary — and `trim_old_attachments` swaps attachments older than the last two
  attachment-bearing messages for a name+mime note every round, before the request is built; a
  compaction that cannot run — summarizer error, empty summary, no cut point — is reported as a
  `compact_stalled` notice naming why, once per run, because a window quietly left over its limit is
  the one failure compaction exists to prevent; the window itself is never guessed either: config
  may name it, and an unset one is learned from the provider's own refusal (the refused size
  becomes the session's window, the history is compacted below it, and the round is retried,
  bounded); a round the provider reported no usage for is priced from the text instead, so a gateway
  that omits the counts cannot switch the gate off), Every
  request closes with a request-only `<context>N tokens left in this context window</context>` user
  turn (`context_note` in `mod.rs`: the context-window room — only once the window is known, since
  an unknown one has no room to report — plus the task's input-token room when
  `--token-budget` is set) — the reference style budget awareness so the model can choose to wrap up; it is
  never persisted and rides after the Anthropic cache tip, so the prompt-cache prefix is untouched.
  Read-only calls from one assistant message (every tool at `Tier::Read`) run concurrently on scope
  threads after a serial gating pass — approvals, extension `tool_call` hooks and the ToolStart
  chrome stay ordered, and results, tool logs and history are replayed in call order — while
  mutating and exec calls stay strictly serial; `prepare_call`/`finish_call` in `mod.rs` are shared
  by both paths so they gate and finish identically, and the provider adapters ask for
  `parallel_tool_calls` so the model may batch independent reads into that one message, a
  loop-hygiene guard in the loop (`RepeatGuard`: the model repeating the exact same tool call — same
  tool, arguments compared modulo key order — gets an advisory `[System]` note riding the result at
  its 3rd/5th/8th consecutive repeat, naming the arguments on the detailed ones; a fresh user
  message clears the streak; a deny-flavored plugin twin lives at
`examples/extensions/repeat_guard.py`), and a tool-result pruner at compaction pressure
(`prune_tool_results`: once `should_compact` fires, results over 8192 chars become head 4096 + a
middle marker + tail 1024 before the summarizer runs, and the pass reports `freed_tokens` — the loop
subtracts them from the usage-derived estimate, because re-estimating over the same usage marker
reports the identical number and would let the summarizer run anyway; a resumed thread that no
longer fits is projected down the same way before its first request, silently
(`Session::prune_seed_to_fit`), so the notice cannot repeat turn after turn); lossless, not lossy:
the untouched original is archived to `observation_dir()` (`~/.llm/observations/<content-id>.txt`,
the 64-bit FNV-1a of the text, so pruning the same result again after a resume rewrites one file
rather than piling up copies) and the marker carries that id so the `recall` built-in can page the
cut middle back — per-result fail-open (`create_dir_all`/`write` failure keeps the whole text rather
than leave a marker pointing at nothing), and char-indexed cuts keep CJK on codepoint boundaries,
`memory.rs` (`~/.llm/LLM.md`, one manual region; `section()` injects into the agent system prompt
and names the path even when the file is absent or empty — the block is what tells the agent where
durable preferences go, so it must never be conditional; there is no `/memory` command and no
`remember` tool, memory is manual-only, the built-in prompt only points at the file), `skills.rs`
(SKILL.md discovery, `skills_block()` caps at 2000 chars with per-entry trigger lines capped at 160
— the list rides every request, the full file is one `read` away), `settings.rs`, `system_prompt.rs`
(built-ins + memory + AGENTS.md/CLAUDE.md project-instruction discovery + an environment line — OS ·
shell · git repo · project type with its verify command — + skills + a self-extension block naming
the extensions/skills dirs and the manifest-header form; a continuation from a stored prompt keeps
it verbatim and refreshes only the trailing cwd line). The system prompt stays byte-identical across
every round: providers cache by request prefix (DeepSeek context caching, Anthropic prompt caching),
so any per-turn mutation re-bills the whole history at cache-miss price. Both adapters engage the
cache deliberately: the anthropic one sends explicit `cache_control` breakpoints (on the system text
block, caching tools+system together, falling back to the last tool when there is no system; on the
last message the previous request carried — the stable anchor `PromptInput::cache_anchor` names,
resolved at assembly so a trailing tool-result run lands inside the marked block — plus on the
current conversation tip; colliding onto one block when a tool round adds no user turn, and skipping
the never-resent one-shot prompt) because Messages API caching is opt-in and a marker on the moving
tip alone would rewrite the whole history at write price every round and read none of it back, and
it folds `cache_read`+`cache_creation` into `Usage.input` to match openai-compat's whole-prompt
`prompt_tokens`; the openai-compat side is automatic server-side, with a `prompt_cache_key` (the
session's `cache_key`) pinning one conversation to one replica — automatic caching is per-replica,
so a round-robin hop serves the next round cold — and its hit count parsed from all three usage
shapes (`providers::cache_hit_tokens`: DeepSeek, OpenAI, OpenRouter). Anything that rewrites history
mid-conversation invalidates the cached prefix from the change point, so the two pruning passes are
gated on `compact::rewrite_prefix` (half the window, well below the compaction gate) instead of
running every round, and the typewriter's settle wait is bounded by `SETTLE_GRACE` because it runs
on the agent thread — a long answer tail must not hold the next tool round hostage.
`commands/agent.rs` is CLI glue only. While a task runs, KeyWatcher buffers typed lines into the
steering queue and the loop drains them at the next tool boundary; leftovers become new tasks. When
the answer owns the current row (`term::screen`'s dangling flag), the watcher defers its `queued:`
notice to the render thread, which prints it at the next settle point (`TaskView::flush_notices`) —
erasing the row in place tore the streamed text apart and dropped the continuation at column 0.
- Plugins ride one plumbing: every extension tool is Exec-tier, so ask mode prompts for every
  extension call (yolo, the default, runs them free) and per-tool policies still win; commands-dir
  prompts (`core/commands_md.rs`: `~/.llm/commands/*.md` plus the nearest `.llm/commands/`, project
  wins) have no CLI dispatch — the REPL's unknown `/name` falls back to them, expanding `$input`
  through `core/templates.rs` (the internal substitution engine).
- `llm -r` (`commands/threads.rs`, the browser): one filterable list of recent threads through the
  shared picker — the list is scoped to the current directory (the reference-shaped:
  `threads::Store::recent_threads(limit, Some(cwd))` filters on the last turn's `cwd`, oldest
  spelling variants included via `Path` comparison), falling back to every directory when this one
  has no history, each row then tagging its directory; selecting shows the transcript, then one
  Y-key question continues that conversation in the agent session via `run`. `/resume` in the REPL
  shares `threads::pick_thread`. `--session`/`--cid` accept unambiguous id prefixes via
  `threads::Store::resolve_thread(prefix, Some(cwd))` — a prefix matching several threads resolves
  to the local one — and `-c` takes `latest_thread(Some(cwd))` before the global newest. The `llm
  logs` CLI (list/full/on/off/status) is gone.
- The provider/model lifecycle is REPL-internal (`commands/models.rs` and `commands/login.rs` are
  library-only now, no CLI): `/model` runs the provider→model→thinking cascade
  (`cascade_model_picker`), switches the live session via `Session::switch_model`
  (`providers::resolve_model_by_id`) and saves the choice as the shared default; `/thinking` adjusts
  the depth alone; `/login` runs the wizard (catalog + Cloudflare/Azure URL templates + custom;
  hidden-input key capture, an omitted key falls back to the catalog entry's env var as a `${VAR}`
  reference), `/logout` the removal picker (clearing the default when it pointed at the removed
  provider). Pickers build from `providers/catalog.rs` (the reference's provider registry, 40 entries incl.
  opencode-go in both wire kinds and four local runtimes: ollama, lm-studio, llama.cpp, vllm;
  OAuth-only and cloud-signature providers deliberately absent). A fresh session still starts on the
  stored default.
- `src/read/` is the text-file reading module behind the agent's read tool: streaming line windows
  (`window()`: BufReader, offsets skip without keeping, the agent tool clamps to a 2000-line window
  under the shared 50KB cap — the reference's values, so one read covers a typical source file), per-line
  2000-char cap, one lookahead line decides `Exact` vs `AtLeast` totals), the `BINARY_EXTS` gate +
  NUL sniff, and `binary_hint` mapping binary formats to local tooling (pdf→pdftotext,
  bam/cram→samtools, parquet/hdf5→duckdb, office→libreoffice). Memory stays bounded by the window;
  nothing but text is ever parsed.
- `src/core/threads.rs` is the conversation store: one JSONL file per thread under
  `user_dir/threads/<ulid>.jsonl`, one `StoredTurn` object per line (a `v` format stamp first —
  `THREAD_FORMAT_VERSION`, absent on pre-versioning lines — then id, ts, mode, model, cwd, system,
  prompt, response, reasoning, usage, options, and the round's wire `messages` as the same
  `providers::Msg` values the request carries), `append_turn` appends a line (a None thread id
  starts a fresh thread), `read_thread` returns turns oldest-first under two strict
  disciplines — a corrupt line mid-file fails loudly (refusing a damaged thread beats silently
  resuming without a turn) while a torn final line (a crash mid-append) is the one bounded repair:
  dropped with a warning — and a turn stamped with a future format is refused with the upgrade path
  named; `latest_thread`/`resolve_thread`/`fork_thread`/`recent_threads` serve the resume and list
  surfaces (the list falls back to the last parseable line, so a torn tail never hides a thread).
  Resume is the reference shape: a thread id reopens its file — no SQL, no FTS, no b2 content addressing.
- `term/lineedit.rs`: raw-mode line editor with completion (VMIN=0/VTIME=1 polled reads where
  `read()==0` means timeout, never EOF — EOF is the 0x04 byte), the KeyWatcher steering buffer, and
  the one picker `pick(title, items, echo)` used by every list in the tool — arrow-move +
  type-to-filter in the fuzzy-filter shape (UTF-8 accumulates across bytes so CJK filters work,
  space-separated terms AND-match case-insensitively, enter returns the original index);
  `pick_multi(title, items)` is the same menu in checkbox mode (one shared `pick_impl`, space
  toggles instead of filtering, everything starts checked) and exists only for `llm install`'s item
  selection. The buffer is multiline (the reference shape): `\r` submits while ctrl+j (`\n`), alt+enter,
  shift/ctrl+enter, a lone `\` before enter (a doubled `\\` submits literally) and bracketed paste
  insert real newlines — the platform clears ICRNL so `\r`/`\n` stay distinct, and the kitty
  keyboard protocol's disambiguate flag (`ESC[>1u`) is pushed only for the read's duration so
  modified enter arrives as CSI-u keys; kitty terminals that re-encode plain ctrl+letter as CSI-u
  (ctrl+c = `CSI 99;5u`) are folded back to the C0 byte so every ctrl binding keeps working, and a
  lone or CSI-27u ESC interrupts like ctrl-c (one press clears the line, two exit; the watcher and
  approval prompts keep plain-ESC semantics); ↑/↓ recall history only while browsing, from an empty
  buffer, or parked at the very start of the top line — the working draft is saved on first recall
  and restored on the way back past the newest entry — and otherwise walk logical lines with a
  sticky preferred column; readline keys round it out (ctrl-a/e cross lines at boundaries,
  ctrl-k/u/w + alt-d feed a single-entry kill buffer that survives submit, ctrl-y yanks, alt-b/f and
  ctrl-arrows move by word); ctrl+g round-trips the draft through `$VISUAL`/`$EDITOR` (guards
  dropped, cooked mode, then re-acquired); pastes over 10 lines or 1000 chars become an atomic
  `[paste #N +L lines]` token whose payload expands at submit; history persists to `history.jsonl`
  (below).
- Model ids are namespaced `provider/model`; the `aliases` object in config.json maps friendly names
  on top (a legacy standalone aliases.json folds in once and is ignored).
