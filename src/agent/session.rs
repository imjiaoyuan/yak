//! The agent session: one model + tools + accumulated history, driving
//! tasks and persisting turns. Shared by the one-shot CLI and the REPL.

use std::path::PathBuf;

use crate::agent::approval::{self, ApprovalConfig};
use crate::agent::compact::CompactConfig;
use crate::agent::{
    AgentOptions, AgentUpdate, ApprovalRequest, ApprovalResponse, RunCallbacks, RunRequest,
    run_agent,
};
use crate::core::threads::{self, StoredTurn};
use crate::providers::{Msg, ResolvedModel};

/// Everything one agent task needs; the interactive REPL reuses this across
/// tasks, evolving `seed`/`conversation_id`/`approval` as it goes.
pub struct Session {
    pub model: ResolvedModel,
    pub tools: Vec<Box<dyn crate::agent::tools::Tool>>,
    pub system: Option<String>,
    pub cwd: PathBuf,
    pub max_turns: usize,
    /// cumulative input-token budget per task; 0 = unlimited
    pub token_budget: u64,
    /// ceiling on one serialized request body; a gateway in front of the
    /// model may refuse far less than the provider documents
    pub max_request_bytes: usize,
    pub stream: bool,
    pub compact: CompactConfig,
    pub no_session: bool,
    pub store: Option<threads::Store>,
    pub approval: ApprovalConfig,
    pub conversation_id: Option<String>,
    /// opaque id sent as `prompt_cache_key` so a gateway keeps this
    /// conversation on one cache replica; distinct from `conversation_id`,
    /// which is a thread filename and only exists once a turn is persisted
    pub cache_key: String,
    pub seed: Vec<Msg>,
    /// reasoning effort level; None sends no parameter
    pub thinking: Option<String>,
    /// steering lines typed mid-run; shared with the KeyWatcher, drained by
    /// the agent loop at tool-round boundaries and by the REPL afterwards
    pub steer_queue: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// the extension host: user executables registering tools (and, later,
    /// commands and event hooks); re-mounted by [`Session::rebuild_tools`]
    pub extensions: crate::agent::ext::Extensions,
    /// cumulative input/output tokens across the session (for the status line)
    pub tokens: (u64, u64),
    /// cumulative input tokens served from the provider prompt cache
    pub tokens_cached: u64,
    /// the latest model round's usage: a per-turn cache view the cumulative
    /// totals cannot show (a compaction or prefix change makes one round a
    /// full miss while the session average stays high)
    pub last_usage: Option<crate::core::http::Usage>,
    /// `--json`: write the task as a line-delimited JSON event stream instead
    /// of driving the terminal UI (one-shot mode only)
    pub json: bool,
    /// the last persistence failure, kept sticky: the run continues in
    /// memory, but the banner and /status must say the transcript is not
    /// being written — a disk-full session that looks healthy is lost work
    /// nobody noticed until /resume
    pub persist_error: Option<String>,
}

impl Session {
    /// A resumed thread can already be over the window. Project its
    /// oversized tool results down before the first request — the very cut
    /// the loop's own prune would make at the first turn end, only silent, so
    /// resuming a big thread does not repeat that notice turn after turn. The
    /// archive is content-addressed, so this rewrites nothing it did last
    /// time, and a thread that fits costs one estimate.
    pub fn prune_seed_to_fit(&mut self) {
        self.prune_seed_to_fit_at(&crate::agent::compact::observation_dir());
    }

    fn prune_seed_to_fit_at(&mut self, archive_dir: &std::path::Path) {
        if self.seed.is_empty() {
            return;
        }
        let estimate = crate::agent::compact::estimate_tokens(&self.seed, None);
        if crate::agent::compact::should_compact(estimate, &self.compact) {
            crate::agent::compact::prune_tool_results(&mut self.seed, archive_dir);
        }
    }

    /// Reset the in-memory session: drop history, forget the conversation id
    /// and token counters. The stored log is untouched.
    pub fn clear(&mut self) {
        self.seed.clear();
        self.conversation_id = None;
        self.tokens = (0, 0);
        self.tokens_cached = 0;
        self.last_usage = None;
    }

    /// Switch the session's model (the `/model` command): re-resolve,
    /// keep history, rebuild the tool registry.
    pub fn switch_model(&mut self, qualified: &str) -> Result<(), String> {
        let model = crate::providers::resolve_model_by_id(qualified)?;
        self.model = model;
        self.rebuild_tools();
        Ok(())
    }

    /// Rebuild the tool registry: built-ins plus extension-registered
    /// tools; called once at startup, and again on a model switch or
    /// `/reload`.
    pub fn rebuild_tools(&mut self) {
        let mut tools = crate::agent::tools::builtin_tools();
        self.extensions.mount_tools(&mut tools);
        self.tools = tools;
    }

    /// Run one task against the accumulated history. Attachments ride the
    /// task's user message (multimodal input).
    pub fn run_task(
        &mut self,
        prompt: &str,
        attachments: Vec<crate::providers::Attachment>,
    ) -> Result<(crate::agent::AgentOutcome, String), String> {
        if self.json {
            return self.run_task_json(prompt, attachments);
        }
        let opts = AgentOptions {
            max_request_bytes: self.max_request_bytes,
            system: self.system.as_deref(),
            cwd: self.cwd.clone(),
            max_turns: self.max_turns,
            token_budget: self.token_budget,
            stream: self.stream,
            compact: Some(self.compact.clone()),
            reasoning: self.thinking.clone(),
            hooks: &self.extensions,
            cache_key: Some(self.cache_key.as_str()),
        };
        let model_id = self.model.model_id.clone();
        // the shared TaskView owns the answer stream, spinner, thinking
        // trace and footer (indent 2); tool chrome stays local
        // shared behind a RefCell so the approval callback can pause the
        // spinner before printing its banner (otherwise they race mid-line)
        let view = std::cell::RefCell::new(crate::term::render::TaskView::new(2, &model_id, true));
        view.borrow_mut().terminal_md(2);
        let task_start = std::time::Instant::now();
        // an approval prompt already echoed the command; the matching
        // ToolStart must not print it a second time
        let approved_echo = std::cell::RefCell::new(None::<(String, String)>);
        // live-streamed tool output: ToolEnd must not print it again
        let streamed = std::cell::Cell::new(false);
        // only the first lines stream verbatim; the rest collapse into a
        // live-updating counter line
        let logged = std::cell::Cell::new(0usize);
        const LOG_HEAD: usize = 5;
        let mut total = crate::core::http::Usage::default();
        let mut last_usage: Option<crate::core::http::Usage> = None;
        let mut on_update = |u: AgentUpdate| {
            match u {
                AgentUpdate::Delta(text) => {
                    view.borrow_mut().delta(&text);
                }
                AgentUpdate::ReasoningDelta(text) => {
                    view.borrow_mut().reasoning_delta(&text);
                }
                AgentUpdate::ToolStart {
                    name,
                    preview,
                    diff,
                } => {
                    view.borrow_mut().tool_started();
                    streamed.set(false);
                    logged.set(0);
                    // the approval prompt already echoed this exact call
                    let dup = approved_echo
                        .borrow()
                        .as_ref()
                        .is_some_and(|(n, p)| n == &name && p == &preview);
                    approved_echo.borrow_mut().take();
                    if !dup {
                        crate::agent::tools::print_action_line(
                            crate::agent::tools::display_verb(&name),
                            &preview,
                            diff.as_deref(),
                        );
                    }
                    view.borrow_mut().resume_running();
                }
                AgentUpdate::ToolReceiving => {
                    // show a plain "running" status; the live argument
                    // size was confusing and the `$ run <cmd>` chrome
                    // line already shows the command
                    view.borrow_mut().receiving("running");
                }
                AgentUpdate::ToolLog(line) => {
                    {
                        streamed.set(true);
                        let n = logged.get() + 1;
                        logged.set(n);
                        if n <= LOG_HEAD {
                            // once output starts streaming, drop the spinner:
                            // its redraw frame would collide with the lines
                            // being printed on the same row
                            if n == 1 {
                                view.borrow_mut().spin_pause();
                            }
                            eprint!("\r\x1b[2K");
                            crate::agent::tools::print_output_block(&line);
                        } else {
                            // beyond the head: one line, rewritten in place
                            let p = crate::theme::err();
                            eprint!("\r\x1b[2K{}  … +{n} lines{}      ", p.gray, p.reset);
                            use std::io::Write;
                            let _ = std::io::stderr().flush();
                        }
                    }
                }
                AgentUpdate::ToolEnd { summary, is_error } => {
                    {
                        view.borrow_mut().pause();
                        if streamed.get() && !is_error {
                            // close the live counter line, if one is open
                            if logged.get() > LOG_HEAD {
                                eprintln!();
                            }
                        } else if is_error {
                            let p = crate::theme::err();
                            for (i, line) in summary.lines().enumerate() {
                                if i == 0 {
                                    eprintln!("{}{}  ✗ {line}{}", p.dim, p.red, p.reset);
                                } else {
                                    eprintln!("{}  {line}{}", p.red, p.reset);
                                }
                            }
                        } else {
                            crate::agent::tools::print_output_block(&summary);
                        }
                        // the next model round is awaited right after: spin,
                        // or the time-to-first-token reads as a hang
                        view.borrow_mut().resume_wait();
                    }
                }
                AgentUpdate::TurnEnd { usage, .. } => {
                    if let Some(u) = usage {
                        total.add(u);
                        last_usage = Some(u);
                    }
                    view.borrow_mut().turn_end(usage);
                }
                AgentUpdate::Compacted { .. } => {
                    // settle the streaming partial line; auto-compaction is
                    // silent on the terminal
                    view.borrow_mut().pause();
                }
                AgentUpdate::StreamRecovered { chars, error } => {
                    // settle the partial answer, then say why the wait
                    // continues; the model picks up from the partial text
                    view.borrow_mut().pause();
                    let p = crate::theme::err();
                    let what = if chars == 0 {
                        "nothing received yet, resending".to_string()
                    } else {
                        format!("keeping the partial answer ({chars} chars), continuing")
                    };
                    eprintln!("{}stream dropped ({error}) — {what}{}", p.dim, p.reset);
                    view.borrow_mut().resume_wait();
                }
                AgentUpdate::ToolResultsPruned { count } => {
                    // settle the row, then say why the next request got
                    // smaller; dim like the other automatic maintenance
                    view.borrow_mut().pause();
                    let p = crate::theme::err();
                    let s = if count == 1 { "" } else { "s" };
                    let dir = crate::agent::compact::observation_dir();
                    eprintln!(
                        "{}pruned {count} oversized tool result{s} from context (full text archived under {}){}",
                        p.dim,
                        dir.display(),
                        p.reset
                    );
                    view.borrow_mut().resume_wait();
                }
            }
        };
        // esc or ctrl-c during a running task requests a cooperative
        // interrupt; the watcher is paused around approval prompts, which
        // read the same stdin
        let watcher_queue = self.steer_queue.clone();
        let mut watcher = crate::term::lineedit::KeyWatcher::start_with(watcher_queue.clone());
        let mut on_approval = |req: ApprovalRequest| {
            // keystrokes typed in the stop window ride along so an eager
            // y/n answer is not swallowed by the dying watcher thread
            let pre = watcher.stop();
            // silence the spinner and close the thinking trace so the
            // banner lands on a clean line
            view.borrow_mut().pause();
            let answer = approval::prompt_approval(&req, pre);
            if !matches!(answer, ApprovalResponse::Deny) {
                *approved_echo.borrow_mut() = Some((req.tool.to_string(), req.preview.to_string()));
            }
            watcher = crate::term::lineedit::KeyWatcher::start_with(watcher_queue.clone());
            answer
        };
        let steer_queue = self.steer_queue.clone();
        let mut steer = move || {
            steer_queue
                .lock()
                .map(|mut q| q.drain(..).collect())
                .unwrap_or_default()
        };
        let result = run_agent(
            RunRequest {
                model: &self.model,
                tools: &self.tools,
                prompt,
                attachments,
                seed: std::mem::take(&mut self.seed),
                opts: &opts,
            },
            &mut self.approval,
            RunCallbacks {
                on_update: &mut on_update,
                on_approval: &mut on_approval,
                steer: &mut steer,
            },
        );
        watcher.stop();
        // an interrupted task may never see TurnEnd: flush any partial
        // markdown line here too (abort is idempotent)
        view.borrow_mut().abort();
        // the interrupt flag must not leak into the next prompt or task
        crate::core::http::clear_interrupt();
        self.tokens.0 += total.input;
        self.tokens.1 += total.output;
        self.tokens_cached += total.cached;
        self.last_usage = last_usage;
        match result {
            Ok(mut outcome) => {
                // one footer per completed task: totals across rounds and
                // wall time, right before the prompt returns. A user-initiated
                // interrupt already shows its own "interrupted" line and
                // should not dump a long-running elapsed/footer after it.
                if !outcome.interrupted {
                    view.borrow_mut().footer(task_start.elapsed().as_secs_f64());
                }
                // a budget stop is deliberate and quiet otherwise; name it
                // so "why did it stop" never needs archeology
                if outcome.budget_exhausted {
                    let p = crate::theme::err();
                    eprintln!(
                        "{}  token budget reached — follow up to continue{}",
                        p.gray, p.reset
                    );
                }
                // the history moves into the seed (no clone of the whole
                // conversation per task); persistence reads it first
                let history = std::mem::take(&mut outcome.history);
                let reasoning = view.into_inner().into_renderer().reasoning;
                self.persist_turn(
                    outcome.seed_boundary,
                    &history,
                    &outcome.final_text,
                    outcome.usage,
                    &reasoning,
                    task_start,
                );
                self.seed = history;
                Ok((outcome, reasoning))
            }
            // the failure carries what was already sent: the session
            // survives without a defensive clone taken up front. A failed
            // round still saw real work — completed tool rounds, maybe a
            // partial answer — so it persists too; without this a dropped
            // connection late in a long task would erase the transcript
            // from /resume while the in-memory seed kept it.
            Err(failure) => {
                let reasoning = view.into_inner().into_renderer().reasoning;
                self.persist_turn(
                    failure.seed_boundary,
                    &failure.history,
                    &failure.final_text,
                    None,
                    &reasoning,
                    task_start,
                );
                self.seed = failure.history;
                Err(failure.message)
            }
        }
    }

    /// `--json`: the same task, with a line-delimited JSON event stream as
    /// the output surface instead of the terminal UI. Every [`AgentUpdate`]
    /// becomes one object and the closing `result` object carries the final
    /// answer — the shape a supervising process parses (the `subagent`
    /// example extension, an editor plugin, CI). Approvals still work: their
    /// prompt goes to stderr, so stdout stays parseable.
    fn run_task_json(
        &mut self,
        prompt: &str,
        attachments: Vec<crate::providers::Attachment>,
    ) -> Result<(crate::agent::AgentOutcome, String), String> {
        let opts = AgentOptions {
            max_request_bytes: self.max_request_bytes,
            system: self.system.as_deref(),
            cwd: self.cwd.clone(),
            max_turns: self.max_turns,
            token_budget: self.token_budget,
            stream: self.stream,
            compact: Some(self.compact.clone()),
            reasoning: self.thinking.clone(),
            hooks: &self.extensions,
            cache_key: Some(self.cache_key.as_str()),
        };
        let task_start = std::time::Instant::now();
        // the terminal path renders the reasoning trace through TaskView;
        // here it is only carried into the stored turn
        let mut reasoning = String::new();
        let mut total = crate::core::http::Usage::default();
        let mut last_usage: Option<crate::core::http::Usage> = None;
        let mut on_update = |u: AgentUpdate| {
            match &u {
                AgentUpdate::ReasoningDelta(text) => reasoning.push_str(text),
                // same accounting as the terminal path, so the session totals
                // and /status agree in both modes
                AgentUpdate::TurnEnd { usage: Some(usage) } => {
                    total.add(*usage);
                    last_usage = Some(*usage);
                }
                _ => {}
            }
            emit_event(&event_json(&u));
        };
        let mut on_approval = |req: ApprovalRequest| approval::prompt_approval(&req, Vec::new());
        // no KeyWatcher here: stdin belongs to the caller (usually a pipe),
        // and ctrl-c kills this process like any other child
        let steer_queue = self.steer_queue.clone();
        let mut steer = move || {
            steer_queue
                .lock()
                .map(|mut q| q.drain(..).collect())
                .unwrap_or_default()
        };
        let result = run_agent(
            RunRequest {
                model: &self.model,
                tools: &self.tools,
                prompt,
                attachments,
                seed: std::mem::take(&mut self.seed),
                opts: &opts,
            },
            &mut self.approval,
            RunCallbacks {
                on_update: &mut on_update,
                on_approval: &mut on_approval,
                steer: &mut steer,
            },
        );
        crate::core::http::clear_interrupt();
        self.tokens.0 += total.input;
        self.tokens.1 += total.output;
        self.tokens_cached += total.cached;
        self.last_usage = last_usage;
        match result {
            Ok(mut outcome) => {
                emit_event(&serde_json::json!({
                    "type": "result",
                    "text": &outcome.final_text,
                    "usage": usage_json(outcome.usage.as_ref()),
                    "interrupted": outcome.interrupted,
                    "budget_exhausted": outcome.budget_exhausted,
                }));
                // the history moves into the seed (no clone of the whole
                // conversation per task); persistence reads it first
                let history = std::mem::take(&mut outcome.history);
                self.persist_turn(
                    outcome.seed_boundary,
                    &history,
                    &outcome.final_text,
                    outcome.usage,
                    &reasoning,
                    task_start,
                );
                self.seed = history;
                Ok((outcome, reasoning))
            }
            // a failed round still saw real work: the stream reports it and
            // the turn is persisted, exactly as in the terminal path
            Err(failure) => {
                emit_event(&serde_json::json!({
                    "type": "error",
                    "message": &failure.message,
                    "text": &failure.final_text,
                }));
                self.persist_turn(
                    failure.seed_boundary,
                    &failure.history,
                    &failure.final_text,
                    None,
                    &reasoning,
                    task_start,
                );
                self.seed = failure.history;
                Err(failure.message)
            }
        }
    }

    /// Steering lines that outlived the last run (typed after the final
    /// model call). The REPL submits each as the next task, the reference style.
    pub fn take_steer_leftover(&self) -> Vec<String> {
        self.steer_queue
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Persist the turn: the wire-level messages so `-c` can replay it.
    /// Runs inside `run_task` (the callers never see the history). Skipped
    /// with --no-session, or when nothing was added to the history — a
    /// completed round and a failed/interrupted one persist alike, so
    /// `/resume` sees the work either way.
    fn persist_turn(
        &mut self,
        boundary: usize,
        history: &[Msg],
        response: &str,
        usage: Option<crate::core::http::Usage>,
        reasoning: &str,
        start: std::time::Instant,
    ) {
        if self.no_session || history.len() <= boundary.min(history.len()) {
            return;
        }
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let mut new_messages: Vec<Msg> = history[boundary.min(history.len())..].to_vec();
        // the final no-tool assistant message doubles as the turn response
        let ends_plain = matches!(
            history.last(),
            Some(Msg::Assistant { tool_calls, .. }) if tool_calls.is_empty()
        );
        if ends_plain {
            new_messages.pop();
        }
        // cwd rides in turn options as provenance so `llm logs` can show
        // and filter conversations by project directory
        let mut turn_options = self.model.options.clone();
        turn_options.push(("cwd".to_string(), self.cwd.display().to_string()));
        let attached: Vec<String> = history[boundary.min(history.len())..]
            .iter()
            .filter_map(|m| match m {
                Msg::User { attachments, .. } if !attachments.is_empty() => Some(
                    attachments
                        .iter()
                        .map(|a| a.filename.clone().unwrap_or_else(|| a.mime_type.clone()))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                _ => None,
            })
            .collect();
        if !attached.is_empty() {
            turn_options.push(("attachments".to_string(), attached.join("; ")));
        }
        // the first user text of the round is the prompt preview
        let prompt = history[boundary.min(history.len())..]
            .iter()
            .find_map(|m| match m {
                Msg::User { text, .. } if !text.is_empty() => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let turn = StoredTurn {
            v: crate::core::threads::THREAD_FORMAT_VERSION,
            id: crate::core::db::ulid(),
            ts: crate::core::db::now_turn_datetime(),
            mode: "agent".to_string(),
            model: self.model.qualified_id(),
            cwd: Some(self.cwd.display().to_string()),
            system: self.system.clone(),
            prompt,
            response: response.to_string(),
            reasoning: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning.to_string())
            },
            usage: usage.map(|u| (u.input, u.output)),
            duration_ms: Some(start.elapsed().as_millis() as i64),
            options: turn_options,
            messages: stored_messages(&new_messages),
        };
        let thread_id = match store.append_turn(self.conversation_id.as_deref(), &turn) {
            Ok(id) => id,
            Err(e) => {
                // sticky: the run continues in memory, but the banner and
                // /status keep saying the transcript is not being written
                eprintln!("Warning: {e}");
                self.persist_error = Some(e);
                String::new()
            }
        };
        if self.conversation_id.is_none() && !thread_id.is_empty() {
            self.conversation_id = Some(thread_id);
        }
    }
}
/// One event as the `--json` stream writes it. The mapping is a contract:
/// the field names are what supervising processes parse, so a new
/// [`AgentUpdate`] variant gets a new `type` rather than a reshuffle.
fn event_json(u: &AgentUpdate) -> serde_json::Value {
    match u {
        AgentUpdate::Delta(text) => serde_json::json!({"type": "text", "text": text}),
        AgentUpdate::ReasoningDelta(text) => {
            serde_json::json!({"type": "reasoning", "text": text})
        }
        AgentUpdate::ToolStart {
            name,
            preview,
            diff,
        } => {
            let mut v = serde_json::json!({"type": "tool_start", "name": name, "preview": preview});
            if let Some(diff) = diff {
                v["diff"] = serde_json::json!(diff);
            }
            v
        }
        AgentUpdate::ToolLog(line) => serde_json::json!({"type": "tool_log", "line": line}),
        AgentUpdate::ToolReceiving => serde_json::json!({"type": "tool_receiving"}),
        AgentUpdate::ToolEnd { summary, is_error } => {
            serde_json::json!({"type": "tool_end", "summary": summary, "is_error": is_error})
        }
        AgentUpdate::TurnEnd { usage } => {
            serde_json::json!({"type": "turn_end", "usage": usage_json(usage.as_ref())})
        }
        AgentUpdate::Compacted { removed } => {
            serde_json::json!({"type": "compacted", "removed": removed})
        }
        AgentUpdate::StreamRecovered { chars, error } => {
            serde_json::json!({"type": "stream_recovered", "chars": chars, "error": error})
        }
        AgentUpdate::ToolResultsPruned { count } => {
            serde_json::json!({"type": "tool_results_pruned", "count": count})
        }
    }
}

/// Token usage in the stream's shape; `null` when the provider reported none
/// (the field stays present so consumers never branch on its absence).
fn usage_json(usage: Option<&crate::core::http::Usage>) -> serde_json::Value {
    match usage {
        Some(u) => serde_json::json!({"input": u.input, "output": u.output, "cached": u.cached}),
        None => serde_json::Value::Null,
    }
}

/// Write one event line and flush: consumers read the stream live, so a
/// buffered event would stall their progress display.
fn emit_event(event: &serde_json::Value) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{event}");
    let _ = out.flush();
}

/// Stored turns keep provenance, not pixels: `threads/<id>.jsonl` would
/// otherwise hold every attachment of every round again — a handful of
/// screenshots grows a thread file into tens of MB — and `rehydrated` puts the
/// bytes back from their sources on resume.
fn stored_messages(msgs: &[Msg]) -> Vec<Msg> {
    msgs.iter()
        .map(|m| match m {
            Msg::User { text, attachments } => Msg::User {
                text: text.clone(),
                attachments: attachments.iter().map(|a| a.without_payload()).collect(),
            },
            Msg::ToolResult {
                call_id,
                name,
                content,
                error,
                attachments,
            } => Msg::ToolResult {
                call_id: call_id.clone(),
                name: name.clone(),
                content: content.clone(),
                error: *error,
                attachments: attachments.iter().map(|a| a.without_payload()).collect(),
            },
            other => other.clone(),
        })
        .collect()
}

/// The reverse for a resumed turn: reload payloads from their local sources,
/// keeping the record as stored when the bytes are gone — the adapters then
/// report the missing attachment to the model instead of sending an empty
/// block.
fn rehydrated(m: &Msg) -> Msg {
    let restore = |attachments: &[crate::providers::Attachment]| {
        attachments
            .iter()
            .map(|a| crate::core::attachments::reload(a).unwrap_or_else(|| a.clone()))
            .collect::<Vec<_>>()
    };
    match m {
        Msg::User { text, attachments } => Msg::User {
            text: text.clone(),
            attachments: restore(attachments),
        },
        Msg::ToolResult {
            call_id,
            name,
            content,
            error,
            attachments,
        } => Msg::ToolResult {
            call_id: call_id.clone(),
            name: name.clone(),
            content: content.clone(),
            error: *error,
            attachments: restore(attachments),
        },
        other => other.clone(),
    }
}

/// Rebuild a wire-level history (plus the original system prompt) from a
/// thread's stored turns. The messages *are* `Msg` — the thread stores the
/// same struct the request carries — so the walk only filters empty user
/// turns and restores the final assistant answer, which the store pops out
/// of `messages` and keeps as the turn's `response` field. The first turn's
/// system is the prompt; later `Summary` messages are compaction summaries.
pub fn rebuild_turns(turns: &[StoredTurn]) -> (Vec<Msg>, Option<String>) {
    let mut msgs: Vec<Msg> = Vec::new();
    let mut system: Option<String> = None;
    for turn in turns {
        if system.is_none() {
            system = turn.system.clone();
        }
        for m in &turn.messages {
            // a user turn with neither text nor attachments carries nothing
            // into the request, so it never enters the replay
            if let Msg::User { text, attachments } = m
                && text.is_empty()
                && attachments.is_empty()
            {
                continue;
            }
            msgs.push(m.clone());
        }
        // the final plain assistant was popped out of `messages` (it doubles
        // as the turn response); ride it back in so a resume sees the answer
        if !turn.response.is_empty() {
            msgs.push(Msg::assistant(turn.response.clone()));
        }
    }
    // Only the kept window is worth reading off disk: an older image rides as
    // provenance and the adapters render the missing payload as a note.
    let start = image_window_start(&msgs).unwrap_or(0);
    for m in &mut msgs[start..] {
        let restored = rehydrated(m);
        *m = restored;
    }
    (msgs, system)
}

/// How many of the newest image-carrying user turns keep their pixels on a
/// replay. Every image in the request is billed on every request, so a thread
/// that once carried screenshots would keep paying for them; older turns keep
/// their provenance and the adapters render the dropped payload as a note.
const IMAGE_TURNS_KEPT: usize = 2;

/// Drop the pixels from images older than the newest `IMAGE_TURNS_KEPT` user
/// turns that carry any — from tool results too, since they belong to the turn
/// they ran in.
pub(crate) fn budget_images(msgs: &mut [Msg]) {
    let Some(start) = image_window_start(msgs) else {
        return;
    };
    for m in &mut msgs[..start] {
        drop_images(m);
    }
}

/// Where the kept image window starts: the index of the `IMAGE_TURNS_KEPT`-th
/// newest image-carrying user turn. `None` when the history holds fewer image
/// turns than the cap, so nothing is taken away.
fn image_window_start(msgs: &[Msg]) -> Option<usize> {
    let mut seen = 0;
    for (i, m) in msgs.iter().enumerate().rev() {
        if let Msg::User { attachments, .. } = m
            && attachments.iter().any(is_image)
        {
            seen += 1;
            if seen == IMAGE_TURNS_KEPT {
                return Some(i);
            }
        }
    }
    None
}

/// Leave an attachment's provenance and take only its pixels.
fn drop_images(m: &mut Msg) {
    let attachments = match m {
        Msg::User { attachments, .. } => attachments,
        Msg::ToolResult { attachments, .. } => attachments,
        _ => return,
    };
    for a in attachments.iter_mut().filter(|a| is_image(a)) {
        a.base64_data.clear();
    }
}

fn is_image(a: &crate::providers::Attachment) -> bool {
    a.mime_type.starts_with("image/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session with a tiny window, so a single stored result is over.
    fn tight_session(seed: Vec<Msg>) -> Session {
        Session {
            max_request_bytes: crate::core::http::MAX_REQUEST_BYTES,
            compact: CompactConfig {
                context_window: 4_000,
                reserve_tokens: 100,
                keep_recent_tokens: 100,
            },
            model: crate::providers::ResolvedModel {
                provider_name: "mock".into(),
                kind: "openai-compat".into(),
                base_url: "http://127.0.0.1:9/v1".into(),
                api_key: None,
                model_id: "m".into(),
                options: vec![],
            },
            tools: Vec::new(),
            system: None,
            cwd: std::env::temp_dir(),
            max_turns: 4,
            token_budget: 0,
            stream: true,
            no_session: true,
            store: None,
            approval: crate::agent::approval::ApprovalConfig::default(),
            conversation_id: None,
            cache_key: "cache-test".to_string(),
            seed,
            thinking: None,
            steer_queue: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            extensions: crate::agent::ext::Extensions::connect(std::path::Path::new(
                "/nonexistent",
            )),
            tokens: (0, 0),
            tokens_cached: 0,
            last_usage: None,
            json: false,
            persist_error: None,
        }
    }

    /// Resuming a thread that no longer fits projects its oversized results
    /// down once, before the first request — the loop must not have to cut
    /// them again on the first turn end (and say so every turn).
    #[test]
    fn a_resumed_seed_over_the_window_is_projected_down_once() {
        let dir = crate::core::testutil::scratch_dir("seed-prune");
        let big = "z".repeat(40_000);
        let mut session = tight_session(vec![
            Msg::user("read the log"),
            Msg::tool_result("1", "read", big.clone()),
        ]);
        session.prune_seed_to_fit_at(&dir);
        let content = match &session.seed[1] {
            Msg::ToolResult { content, .. } => content.clone(),
            _ => panic!("expected the tool result"),
        };
        assert!(
            content.contains("chars of the middle were cut"),
            "{content}"
        );
        assert!(
            content.starts_with(&"z".repeat(64)),
            "the head of the result is kept in context"
        );
        // the untouched original is on disk under the id the marker names
        let id = content
            .split("observation ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .expect("marker carries an observation id")
            .to_string();
        assert_eq!(
            std::fs::read_to_string(dir.join(format!("{id}.txt"))).unwrap(),
            big
        );
        // already-projected and under the window: a second pass changes
        // nothing at all, so no new file either
        let before = format!("{:?}", session.seed);
        session.prune_seed_to_fit_at(&dir);
        assert_eq!(format!("{:?}", session.seed), before);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    /// A thread that fits is left exactly as loaded — the projection is a
    /// pressure measure, not a property of every resume.
    #[test]
    fn a_resumed_seed_under_the_window_is_left_alone() {
        // the seed fits, so nothing is archived and this tree must stay absent
        let dir = crate::core::testutil::scratch_path("seed-keep");
        let seed = vec![
            Msg::user("read the log"),
            Msg::tool_result("1", "read", "q".repeat(40_000)),
        ];
        let mut session = tight_session(seed.clone());
        session.compact = CompactConfig {
            context_window: 128_000,
            reserve_tokens: 16_384,
            keep_recent_tokens: 32_000,
        };
        session.prune_seed_to_fit_at(&dir);
        assert_eq!(session.seed, seed);
        assert!(!dir.exists(), "nothing archived when nothing was cut");
    }

    /// A round that failed late (stream drop after tool rounds) must still
    /// reach the thread file: `/resume` sees the work either way.
    #[test]
    fn a_failed_round_persists_its_tool_rounds() {
        let dir = crate::core::testutil::scratch_path("persist-fail");
        let store = threads::Store::open_path(&dir).unwrap();
        let cwd = dir.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut session = Session {
            max_request_bytes: crate::core::http::MAX_REQUEST_BYTES,
            compact: CompactConfig::default(),
            model: crate::providers::ResolvedModel {
                provider_name: "mock".into(),
                kind: "openai-compat".into(),
                base_url: "http://127.0.0.1:9/v1".into(),
                api_key: None,
                model_id: "m".into(),
                options: vec![],
            },
            tools: Vec::new(),
            system: None,
            cwd: cwd.clone(),
            max_turns: 4,
            token_budget: 0,
            stream: true,
            no_session: false,
            store: Some(store),
            approval: crate::agent::approval::ApprovalConfig::default(),
            conversation_id: None,
            cache_key: "cache-test".to_string(),
            seed: Vec::new(),
            thinking: None,
            steer_queue: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            extensions: crate::agent::ext::Extensions::connect(&cwd),
            tokens: (0, 0),
            tokens_cached: 0,
            last_usage: None,
            json: false,
            persist_error: None,
        };
        let history = vec![
            Msg::user("write the docs"),
            Msg::Assistant {
                text: String::new(),
                tool_calls: vec![crate::providers::ToolCall {
                    id: "c1".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({"path": "a.md", "content": "x"}),
                }],
                reasoning: None,
                reasoning_meta: None,
            },
            Msg::ToolResult {
                call_id: "c1".into(),
                name: "write".into(),
                content: "wrote 1 bytes to a.md".into(),
                error: None,
                attachments: Vec::new(),
            },
        ];
        // the round died here: no final answer, no usage — still persisted
        session.persist_turn(0, &history, "", None, "", std::time::Instant::now());
        let cid = session.conversation_id.clone().expect("thread created");
        let turns = session.store.as_ref().unwrap().read_thread(&cid).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].messages.len(), 3, "prompt, tool call, result");
        assert_eq!(turns[0].response, "");

        // nothing new since the seed: no second turn
        session.persist_turn(3, &history, "", None, "", std::time::Instant::now());
        let turns = session.store.as_ref().unwrap().read_thread(&cid).unwrap();
        assert_eq!(turns.len(), 1, "an unchanged history persists nothing");
        assert!(session.persist_error.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_persist_failure_is_kept_visible() {
        // an unreadable store location: the run continues, but the sticky
        // flag must say the transcript is not being written
        let dir = crate::core::testutil::scratch_path("persist-err");
        let store = threads::Store::open_path(&dir).unwrap();
        let cwd = dir.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut session = Session {
            max_request_bytes: crate::core::http::MAX_REQUEST_BYTES,
            compact: CompactConfig::default(),
            model: crate::providers::ResolvedModel {
                provider_name: "mock".into(),
                kind: "openai-compat".into(),
                base_url: "http://127.0.0.1:9/v1".into(),
                api_key: None,
                model_id: "m".into(),
                options: vec![],
            },
            tools: Vec::new(),
            system: None,
            cwd: cwd.clone(),
            max_turns: 4,
            token_budget: 0,
            stream: true,
            no_session: false,
            store: Some(store),
            approval: crate::agent::approval::ApprovalConfig::default(),
            conversation_id: None,
            cache_key: "cache-test".to_string(),
            seed: Vec::new(),
            thinking: None,
            steer_queue: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            extensions: crate::agent::ext::Extensions::connect(&cwd),
            tokens: (0, 0),
            tokens_cached: 0,
            last_usage: None,
            json: false,
            persist_error: None,
        };
        // a store whose directory vanishes after opening: append_turn cannot
        // create the thread file (ENOENT), which is exactly the shape of a
        // disk-full / removed-mount failure at write time
        let bad = threads::Store::open_path(&cwd.join("blocked")).unwrap();
        std::fs::remove_dir(cwd.join("blocked")).unwrap();
        session.store = Some(bad);
        let history = vec![Msg::user("task")];
        session.persist_turn(0, &history, "answer", None, "", std::time::Instant::now());
        let e = session.persist_error.clone().expect("failure recorded");
        assert!(e.contains("cannot write"), "{e}");
        assert!(session.conversation_id.is_none(), "no thread was created");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn attachments_keep_their_provenance_through_a_serde_round_trip() {
        // the stored shape *is* the wire shape: a thread file round-trips an
        // attachment's path/url (never the bare display filename as a path),
        // and the base64 payload rides along so a resume rebuilds the request
        for (path, url) in [
            (Some("/tmp/cam/2026/shot.png"), None),
            (None, Some("https://example.com/shot.png?token=1")),
        ] {
            let msg = crate::providers::Msg::user_with(
                "look",
                vec![crate::providers::Attachment {
                    mime_type: "image/png".into(),
                    base64_data: crate::b64::encode(b"pngbytes"),
                    filename: Some("shot.png".into()),
                    path: path.map(str::to_string),
                    url: url.map(str::to_string),
                }],
            );
            let line = serde_json::to_string(&msg).unwrap();
            let back: Msg = serde_json::from_str(&line).unwrap();
            assert_eq!(back, msg, "{line}");
        }
    }

    /// The stored line keeps the wire `role` tags and omits empty payload
    /// fields, so a thread file stays small and readable.
    #[test]
    fn a_stored_message_omits_empty_attachment_fields() {
        let msg = Msg::user("plain");
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"role":"user","text":"plain","attachments":[]}"#
        );
        let msg = Msg::ToolResult {
            call_id: "c1".into(),
            name: "read".into(),
            content: "bytes".into(),
            error: None,
            attachments: Vec::new(),
        };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"role":"tool_result","call_id":"c1","name":"read","content":"bytes","error":null,"attachments":[]}"#
        );
    }

    #[test]
    fn a_failure_kind_round_trips_and_the_old_bool_still_reads() {
        let msg = Msg::ToolResult {
            call_id: "c1".into(),
            name: "bash".into(),
            content: "boom".into(),
            error: Some(crate::providers::ToolError::Denied),
            attachments: Vec::new(),
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(line.contains(r#""error":"denied""#), "{line}");
        assert_eq!(serde_json::from_str::<Msg>(&line).unwrap(), msg);

        // threads written before the kinds carried a plain bool
        let legacy = r#"{"role":"tool_result","call_id":"c9","name":"bash",
            "content":"boom","is_error":true,"attachments":[]}"#;
        match serde_json::from_str::<Msg>(legacy).unwrap() {
            Msg::ToolResult { error, .. } => {
                assert_eq!(error, Some(crate::providers::ToolError::Failed))
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    /// A stored turn keeps provenance, not pixels: a thread file would
    /// otherwise hold every attachment of every round again.
    #[test]
    fn a_stored_turn_drops_payloads_and_keeps_their_source() {
        let msg = Msg::user_with(
            "look",
            vec![crate::providers::Attachment {
                mime_type: "image/png".into(),
                base64_data: "aGk=".into(),
                filename: Some("m05.png".into()),
                path: Some("/tmp/mg/m05.png".into()),
                url: None,
            }],
        );
        let line = serde_json::to_string(&stored_messages(&[msg])).unwrap();
        assert!(!line.contains("aGk="), "{line}");
        assert!(line.contains("m05.png"), "{line}");
        assert!(line.contains("/tmp/mg/m05.png"), "{line}");
    }

    /// Resume puts the payloads back from their local files; a source that is
    /// gone stays a record, which the adapters report to the model as a note.
    #[test]
    fn only_the_newest_image_turns_keep_their_pixels() {
        let image_turn = |text: &str| Msg::User {
            text: text.to_string(),
            attachments: vec![crate::core::attachments::from_bytes(
                Some("image/png"),
                vec![1, 2, 3],
            )],
        };
        let mut msgs = vec![
            image_turn("one"),
            Msg::assistant("ok".to_string()),
            image_turn("two"),
            image_turn("three"),
        ];
        budget_images(&mut msgs);
        let dropped = |m: &Msg| match m {
            Msg::User { attachments, .. } => attachments[0].base64_data.is_empty(),
            _ => panic!("not a user turn"),
        };
        assert!(dropped(&msgs[0]), "the oldest image rides as a note");
        assert!(!dropped(&msgs[2]), "the newest two keep their pixels");
        assert!(!dropped(&msgs[3]));
    }

    #[test]
    fn an_old_tool_results_images_are_budgeted_too() {
        let shot = || {
            vec![crate::core::attachments::from_bytes(
                Some("image/png"),
                vec![1, 2, 3],
            )]
        };
        let mut msgs = vec![
            Msg::User {
                text: "old".into(),
                attachments: shot(),
            },
            Msg::ToolResult {
                call_id: "1".into(),
                name: "read".into(),
                content: "shot".into(),
                error: None,
                attachments: shot(),
            },
            Msg::User {
                text: "middle".into(),
                attachments: shot(),
            },
            Msg::User {
                text: "new".into(),
                attachments: shot(),
            },
        ];
        budget_images(&mut msgs);
        let dropped = |m: &Msg| match m {
            Msg::User { attachments, .. } | Msg::ToolResult { attachments, .. } => {
                attachments[0].base64_data.is_empty()
            }
            _ => panic!("not an image carrier"),
        };
        assert!(dropped(&msgs[0]), "the old turn's screenshot goes");
        assert!(dropped(&msgs[1]), "and the tool result that ran in it");
        assert!(!dropped(&msgs[2]), "the newest two keep their pixels");
        assert!(!dropped(&msgs[3]));
    }

    #[test]
    fn a_resume_only_reads_the_images_it_keeps() {
        let dir = crate::core::testutil::scratch_dir("rehydrate-window");
        let turn = |n: usize| {
            let file = dir.join(format!("shot{n}.png"));
            std::fs::write(&file, format!("bytes{n}")).unwrap();
            StoredTurn {
                v: crate::core::threads::THREAD_FORMAT_VERSION,
                id: format!("t{n}"),
                ts: "2026-08-23T01:00:00+00:00".into(),
                mode: "agent".into(),
                model: "prov/m".into(),
                cwd: None,
                system: None,
                prompt: format!("look {n}"),
                response: String::new(),
                reasoning: None,
                usage: None,
                duration_ms: None,
                options: Vec::new(),
                messages: vec![Msg::user_with(
                    format!("look {n}").as_str(),
                    vec![crate::providers::Attachment {
                        mime_type: "image/png".into(),
                        base64_data: String::new(),
                        filename: None,
                        path: Some(file.display().to_string()),
                        url: None,
                    }],
                )],
            }
        };
        let (msgs, _) = rebuild_turns(&[turn(0), turn(1), turn(2)]);
        let payload = |i: usize| match &msgs[i] {
            Msg::User { attachments, .. } => attachments[0].base64_data.clone(),
            _ => panic!("expected the stored user message"),
        };
        assert!(
            payload(0).is_empty(),
            "the oldest image is not even read off disk"
        );
        assert_eq!(payload(1), crate::b64::encode(b"bytes1"));
        assert_eq!(payload(2), crate::b64::encode(b"bytes2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_reloads_payloads_from_their_local_file() {
        let dir = crate::core::testutil::scratch_dir("rehydrate");
        let file = dir.join("m05.png");
        std::fs::write(&file, b"pngbytes").unwrap();
        let record = |path: Option<String>| crate::providers::Attachment {
            mime_type: "image/png".into(),
            base64_data: String::new(),
            filename: Some("m05.png".into()),
            path,
            url: None,
        };
        let turn = StoredTurn {
            v: crate::core::threads::THREAD_FORMAT_VERSION,
            id: "t1".into(),
            ts: "2026-08-23T01:00:00+00:00".into(),
            mode: "agent".into(),
            model: "prov/m".into(),
            cwd: None,
            system: None,
            prompt: "look".into(),
            response: String::new(),
            reasoning: None,
            usage: None,
            duration_ms: None,
            options: Vec::new(),
            messages: vec![Msg::user_with(
                "look",
                vec![
                    record(Some(file.display().to_string())),
                    record(Some(dir.join("gone.png").display().to_string())),
                    record(None),
                ],
            )],
        };
        let (msgs, _) = rebuild_turns(&[turn]);
        let Msg::User { attachments, .. } = &msgs[0] else {
            panic!("expected the stored user message")
        };
        assert_eq!(attachments[0].base64_data, crate::b64::encode(b"pngbytes"));
        assert!(attachments[1].base64_data.is_empty());
        assert!(attachments[2].base64_data.is_empty());
    }

    #[test]
    fn rebuild_thread_restores_final_response_and_attachments() {
        let dir = crate::core::testutil::scratch_path("rebuild");
        let store = threads::Store::open_path(&dir).unwrap();

        // user (with attachment) → assistant tool_call → tool result (with
        // attachment) → final plain assistant, stored only as `response`
        let turn = StoredTurn {
            v: crate::core::threads::THREAD_FORMAT_VERSION,
            id: "t1".into(),
            ts: "2026-08-23T01:00:00+00:00".into(),
            mode: "agent".into(),
            model: "prov/m".into(),
            cwd: None,
            system: Some("sys".into()),
            prompt: "look at this".into(),
            response: "it is a cat".into(),
            reasoning: None,
            usage: None,
            duration_ms: None,
            options: Vec::new(),
            messages: vec![
                Msg::user_with(
                    "look at this",
                    vec![crate::providers::Attachment {
                        mime_type: "image/png".into(),
                        base64_data: "aGk=".into(),
                        filename: None,
                        path: None,
                        url: None,
                    }],
                ),
                Msg::Assistant {
                    text: String::new(),
                    tool_calls: vec![crate::providers::ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "a.png"}),
                    }],
                    reasoning: None,
                    reasoning_meta: None,
                },
                Msg::ToolResult {
                    call_id: "c1".into(),
                    name: "read".into(),
                    content: "bytes".into(),
                    error: None,
                    attachments: vec![crate::providers::Attachment {
                        mime_type: "image/png".into(),
                        base64_data: "aGk=".into(),
                        filename: None,
                        path: Some("a.png".into()),
                        url: None,
                    }],
                },
            ],
        };
        store.append_turn(Some("th1"), &turn).unwrap();

        let turns = store.read_thread("th1").unwrap();
        let (msgs, system) = rebuild_turns(&turns);
        assert_eq!(system.as_deref(), Some("sys"));
        assert_eq!(msgs.len(), 4);
        match &msgs[0] {
            Msg::User { text, attachments } => {
                assert_eq!(text, "look at this");
                assert_eq!(attachments[0].base64_data, "aGk=");
            }
            _ => panic!("expected user"),
        }
        match &msgs[2] {
            Msg::ToolResult {
                call_id,
                attachments,
                ..
            } => {
                assert_eq!(call_id, "c1");
                assert_eq!(attachments[0].base64_data, "aGk=");
            }
            _ => panic!("expected tool result"),
        }
        match &msgs[3] {
            Msg::Assistant {
                text, tool_calls, ..
            } => {
                assert_eq!(text, "it is a cat");
                assert!(tool_calls.is_empty());
            }
            _ => panic!("expected final assistant"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A damaged thread must fail the resume loudly (the caller propagates
    /// `read_thread`'s error) instead of silently rebuilding an empty
    /// history and continuing as if nothing was lost.
    #[test]
    fn a_damaged_thread_fails_the_rebuild_instead_of_emptying_it() {
        let dir = crate::core::testutil::scratch_path("damaged");
        let store = threads::Store::open_path(&dir).unwrap();
        let turn = StoredTurn {
            v: crate::core::threads::THREAD_FORMAT_VERSION,
            id: "t1".into(),
            ts: "2026-08-23T01:00:00+00:00".into(),
            mode: "agent".into(),
            model: "prov/m".into(),
            cwd: None,
            system: None,
            prompt: "hello".into(),
            response: "hi".into(),
            reasoning: None,
            usage: None,
            duration_ms: None,
            options: Vec::new(),
            messages: vec![],
        };
        store.append_turn(Some("th1"), &turn).unwrap();
        // corrupt the middle: a non-JSON line with a valid one after it
        // (a lone bad tail is the bounded torn-tail repair, not damage)
        let path = dir.join("th1.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("not json at all\n");
        text.push_str(&serde_json::to_string(&turn).unwrap());
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        let err = store.read_thread("th1").unwrap_err();
        assert!(err.contains("corrupt turn"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_events_name_every_update() {
        use serde_json::json;
        let cases: Vec<(AgentUpdate, serde_json::Value)> = vec![
            (
                AgentUpdate::Delta("hi".into()),
                json!({"type": "text", "text": "hi"}),
            ),
            (
                AgentUpdate::ReasoningDelta("why".into()),
                json!({"type": "reasoning", "text": "why"}),
            ),
            (
                AgentUpdate::ToolStart {
                    name: "bash".into(),
                    preview: "$ ls".into(),
                    diff: None,
                },
                json!({"type": "tool_start", "name": "bash", "preview": "$ ls"}),
            ),
            (
                AgentUpdate::ToolStart {
                    name: "edit".into(),
                    preview: "src/x.rs".into(),
                    diff: Some("-a\n+b".into()),
                },
                json!({"type": "tool_start", "name": "edit", "preview": "src/x.rs", "diff": "-a\n+b"}),
            ),
            (
                AgentUpdate::ToolLog("[exit 1]".into()),
                json!({"type": "tool_log", "line": "[exit 1]"}),
            ),
            (
                AgentUpdate::ToolReceiving,
                json!({"type": "tool_receiving"}),
            ),
            (
                AgentUpdate::ToolEnd {
                    summary: "ok".into(),
                    is_error: false,
                },
                json!({"type": "tool_end", "summary": "ok", "is_error": false}),
            ),
            (
                AgentUpdate::TurnEnd {
                    usage: Some(crate::core::http::Usage {
                        input: 10,
                        output: 2,
                        cached: 8,
                    }),
                },
                json!({"type": "turn_end", "usage": {"input": 10, "output": 2, "cached": 8}}),
            ),
            // a round without a usage report keeps the key, as null
            (
                AgentUpdate::TurnEnd { usage: None },
                json!({"type": "turn_end", "usage": null}),
            ),
            (
                AgentUpdate::Compacted { removed: 12 },
                json!({"type": "compacted", "removed": 12}),
            ),
            (
                AgentUpdate::StreamRecovered {
                    chars: 40,
                    error: "closed".into(),
                },
                json!({"type": "stream_recovered", "chars": 40, "error": "closed"}),
            ),
            (
                AgentUpdate::ToolResultsPruned { count: 2 },
                json!({"type": "tool_results_pruned", "count": 2}),
            ),
        ];
        for (update, want) in cases {
            assert_eq!(event_json(&update), want, "for {want}");
        }
    }

    #[test]
    fn every_event_line_is_one_json_object() {
        // the stream's whole contract: no embedded newlines, so a consumer
        // can split stdout on '\n' and parse each line
        let line = event_json(&AgentUpdate::Delta("a\nb".into())).to_string();
        assert_eq!(line.matches('\n').count(), 0);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["text"],
            "a\nb"
        );
    }
}
