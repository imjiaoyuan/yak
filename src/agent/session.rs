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
    /// The active model. None until `/login` or `/model` sets one: a fresh
    /// install has no stored default, and the REPL still has to open so those
    /// commands are reachable. `run_task` fails loudly while it is None.
    pub model: Option<ResolvedModel>,
    pub tools: Vec<Box<dyn crate::agent::tools::Tool>>,
    pub system: Option<String>,
    pub cwd: PathBuf,
    /// ceiling on one serialized request body; a gateway in front of the
    /// model may refuse far less than the provider documents
    pub max_request_bytes: usize,
    pub stream: bool,
    pub compact: CompactConfig,
    /// how long to ask the provider to hold this conversation's prompt-cache
    /// entries (`agent.cache_ttl`); None uses the provider's own default
    pub cache_ttl: Option<crate::providers::CacheTtl>,
    /// None when --no-session was asked: the flag's whole effect is that no
    /// store exists to write to
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
    /// the extension host: user executables registering tools (and, later,
    /// commands and event hooks); re-mounted by [`Session::rebuild_tools`]
    pub extensions: crate::agent::ext::Extensions,
    /// cumulative usage across the session, for `/status`: every field a round
    /// reports, folded through `Usage::add` so the cache split survives to the
    /// end of the run instead of counting as one undifferentiated input total
    pub usage: crate::core::http::Usage,
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
    /// resuming a big thread does not repeat that notice turn after turn. A
    /// thread that fits costs one estimate.
    pub fn prune_seed_to_fit(&mut self) {
        if self.seed.is_empty() {
            return;
        }
        let estimate = crate::agent::compact::estimate_tokens(&self.seed, None);
        // a resumed thread is priced against the model's real window when one
        // is recorded, else the configured fallback
        let trigger = crate::agent::compact::effective_trigger(
            self.compact.trigger_tokens,
            self.model.as_ref().and_then(|m| m.context_window),
        );
        if crate::agent::compact::should_compact(estimate, trigger) {
            crate::agent::compact::prune_tool_results(&mut self.seed);
        }
    }

    /// How much of the carried history the previous request in this
    /// conversation already sent, as a prefix length — what the next run
    /// starts its cache breakpoints from. None on a session's first task:
    /// nothing was sent yet, and a marker on a prefix no request ever carried
    /// is a breakpoint the provider cannot match.
    pub fn cache_anchor(&self) -> Option<usize> {
        (!self.seed.is_empty()).then_some(self.seed.len())
    }

    /// Reset the in-memory session: drop history, forget the conversation id
    /// and token counters (picked up again from a thread on the next resume).
    /// The stored log is untouched.
    pub fn clear(&mut self) {
        self.seed.clear();
        self.conversation_id = None;
        self.usage = crate::core::http::Usage::default();
        self.last_usage = None;
    }

    /// Switch the session's model (the `/model` command): re-resolve,
    /// keep history, rebuild the tool registry.
    pub fn switch_model(&mut self, qualified: &str) -> Result<(), String> {
        let model = crate::providers::resolve_model_by_id(qualified)?;
        self.model = Some(model);
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
        // no model yet: /login and /model configure one, but a task cannot
        // run without it
        let Some(model) = self.model.as_ref() else {
            return Err(
                "No model configured. Run /login to add a provider, then /model to pick one."
                    .to_string(),
            );
        };
        // the same set `run_task_json` builds below, by hand because a `&self`
        // method would borrow the whole session and lock out the
        // `&mut self.seed` and `&mut self.approval` this same call needs:
        // a field added here has to be added there too
        let opts = AgentOptions {
            max_request_bytes: self.max_request_bytes,
            system: self.system.as_deref(),
            cwd: self.cwd.clone(),
            stream: self.stream,
            compact: Some(self.compact.clone()),
            reasoning: self.thinking.clone(),
            hooks: &self.extensions,
            cache_key: Some(self.cache_key.as_str()),
            cache_anchor: self.cache_anchor(),
            cache_ttl: self.cache_ttl,
        };
        let model_id = model.model_id.clone();
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
        // the round in flight: its reasoning deltas and its clock, flushed at
        // each `RoundEnd` and reset for the next round
        let mut round_reasoning = String::new();
        let mut round_started = std::time::Instant::now();
        let identity = TaskIdentity {
            model,
            cwd: &self.cwd,
            system: self.system.as_deref(),
        };
        let mut on_update = |u: AgentUpdate| {
            match u {
                AgentUpdate::Delta(text) => {
                    view.borrow_mut().delta(&text);
                }
                AgentUpdate::ReasoningDelta(text) => {
                    round_reasoning.push_str(&text);
                    view.borrow_mut().reasoning_delta(&text);
                }
                AgentUpdate::RoundEnd { messages, usage } => {
                    persist_round(
                        self.store.as_ref(),
                        &mut self.conversation_id,
                        &mut self.persist_error,
                        &identity,
                        &messages,
                        usage,
                        &round_reasoning,
                        round_started,
                    );
                    round_reasoning.clear();
                    round_started = std::time::Instant::now();
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
                            eprint!("{}", crate::theme::cursor().clear_line);
                            crate::agent::tools::print_output_block(&line);
                        } else {
                            // beyond the head: one line, rewritten in place
                            let p = crate::theme::err();
                            eprint!(
                                "{}{}  … +{n} lines{}      ",
                                crate::theme::cursor().clear_line,
                                p.gray,
                                p.reset
                            );
                            use std::io::Write;
                            let _ = std::io::stderr().flush();
                        }
                    }
                }
                AgentUpdate::ToolEnd {
                    summary,
                    is_error,
                    duration,
                } => {
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
                        // the reference's `Took 1.2s`: the measured duration of what just
                        // ran, on its own dim line — cheap to print, and it
                        // separates a slow call from a hung one at a glance
                        eprintln!(
                            "{}  Took {}{}",
                            crate::theme::err().dim,
                            crate::agent::tools::format_duration(duration),
                            crate::theme::err().reset
                        );
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
                AgentUpdate::CompactStalled { reason } => {
                    // compaction is due and cannot run: the session keeps
                    // growing over the window, so the reason is the one thing
                    // the user needs (and the only place it is said)
                    view.borrow_mut().pause();
                    let p = crate::theme::err();
                    eprintln!(
                        "{}! compaction stalled: {reason} — the context stays over the window{}",
                        p.red, p.reset
                    );
                    view.borrow_mut().resume_wait();
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
                    if crate::providers::is_truncation(&error) {
                        // the transport closed cleanly but the provider never
                        // said it was done: that is a warning about the answer
                        // on screen, not a failed link like a mid-stream drop
                        eprintln!("{}! {error}; {what}{}", p.red, p.reset);
                    } else {
                        eprintln!(
                            "{}{}stream dropped ({error}) — {what}{}",
                            p.dim,
                            crate::theme::NOTICE,
                            p.reset
                        );
                    }
                    view.borrow_mut().resume_wait();
                }
                AgentUpdate::ToolResultsPruned { count } => {
                    // settle the row, then say why the next request got
                    // smaller; dim like the other automatic maintenance.
                    // The stale-prefix pass keeps the head and tail, so
                    // "full text stays in the session log" was never the
                    // story — the middle was cut.
                    view.borrow_mut().pause();
                    let p = crate::theme::err();
                    let s = if count == 1 { "" } else { "s" };
                    eprintln!(
                        "{}{}pruned {count} oversized tool result{s} from context (kept the head and tail; re-run the command for the middle){}",
                        p.dim,
                        crate::theme::NOTICE,
                        p.reset
                    );
                    view.borrow_mut().resume_wait();
                }
            }
        };
        // esc or ctrl-c during a running task requests a cooperative
        // interrupt; the watcher is paused around approval prompts, which
        // read the same stdin
        let mut watcher = crate::term::lineedit::KeyWatcher::start();
        let mut on_approval = |req: ApprovalRequest| {
            // keystrokes have no meaning while the watcher runs, so stopping
            // it just hands the terminal back to the approval prompt
            watcher.stop();
            // silence the spinner and close the thinking trace so the
            // banner lands on a clean line
            view.borrow_mut().pause();
            let answer = approval::prompt_approval(&req);
            if !matches!(answer, ApprovalResponse::Deny) {
                *approved_echo.borrow_mut() = Some((req.tool.to_string(), req.preview.to_string()));
            }
            watcher = crate::term::lineedit::KeyWatcher::start();
            answer
        };
        let result = run_agent(
            RunRequest {
                model,
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
            },
        );
        watcher.stop();
        // an interrupted task may never see TurnEnd: flush any partial
        // markdown line here too (abort is idempotent)
        view.borrow_mut().abort();
        // the interrupt flag must not leak into the next prompt or task
        crate::core::http::clear_interrupt();
        self.usage.add(total);
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
                // the history moves into the seed (no clone of the whole
                // conversation per task); every round is already on disk
                let history = std::mem::take(&mut outcome.history);
                let reasoning = view.into_inner().into_renderer().reasoning;
                self.seed = history;
                Ok((outcome, reasoning))
            }
            // the failure carries what was already sent: the session
            // survives without a defensive clone taken up front. The round
            // in flight is the only thing not yet on disk — completed
            // rounds wrote themselves at their round boundaries — so only
            // the failed round's slice persists here.
            Err(failure) => {
                persist_round(
                    self.store.as_ref(),
                    &mut self.conversation_id,
                    &mut self.persist_error,
                    &TaskIdentity {
                        model,
                        cwd: &self.cwd,
                        system: self.system.as_deref(),
                    },
                    &failure.history[failure.round_start.min(failure.history.len())..],
                    None,
                    &round_reasoning,
                    round_started,
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
        let Some(model) = self.model.as_ref() else {
            return Err(
                "No model configured. Run /login to add a provider, then /model to pick one."
                    .to_string(),
            );
        };
        // the same set `run_task` builds above, by hand for the same borrow
        // reason: keep the two in step
        let opts = AgentOptions {
            max_request_bytes: self.max_request_bytes,
            system: self.system.as_deref(),
            cwd: self.cwd.clone(),
            stream: self.stream,
            compact: Some(self.compact.clone()),
            reasoning: self.thinking.clone(),
            hooks: &self.extensions,
            cache_key: Some(self.cache_key.as_str()),
            cache_anchor: self.cache_anchor(),
            cache_ttl: self.cache_ttl,
        };
        // the terminal path renders the reasoning trace through TaskView;
        // here it is only carried into the stored turn
        let mut reasoning = String::new();
        let mut round_reasoning = String::new();
        let mut round_started = std::time::Instant::now();
        let mut total = crate::core::http::Usage::default();
        let mut last_usage: Option<crate::core::http::Usage> = None;
        let identity = TaskIdentity {
            model,
            cwd: &self.cwd,
            system: self.system.as_deref(),
        };
        let mut on_update = |u: AgentUpdate| {
            match &u {
                AgentUpdate::ReasoningDelta(text) => {
                    reasoning.push_str(text);
                    round_reasoning.push_str(text);
                }
                // same accounting as the terminal path, so the session totals
                // and /status agree in both modes
                AgentUpdate::TurnEnd { usage: Some(usage) } => {
                    total.add(*usage);
                    last_usage = Some(*usage);
                }
                // the round lands in the thread file here and is not emitted:
                // the event stream stays the UI surface, persistence is not
                // part of the contract
                AgentUpdate::RoundEnd { messages, usage } => {
                    persist_round(
                        self.store.as_ref(),
                        &mut self.conversation_id,
                        &mut self.persist_error,
                        &identity,
                        messages,
                        *usage,
                        &round_reasoning,
                        round_started,
                    );
                    round_reasoning.clear();
                    round_started = std::time::Instant::now();
                    return;
                }
                _ => {}
            }
            emit_event(&event_json(&u));
        };
        let mut on_approval = |req: ApprovalRequest| approval::prompt_approval(&req);
        // no KeyWatcher here: stdin belongs to the caller (usually a pipe),
        // and ctrl-c kills this process like any other child
        let result = run_agent(
            RunRequest {
                model,
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
            },
        );
        crate::core::http::clear_interrupt();
        self.usage.add(total);
        self.last_usage = last_usage;
        match result {
            Ok(mut outcome) => {
                emit_event(&serde_json::json!({
                    "type": "result",
                    "text": &outcome.final_text,
                    "usage": usage_json(outcome.usage.as_ref()),
                    "interrupted": outcome.interrupted,
                }));
                // the history moves into the seed (no clone of the whole
                // conversation per task); every round is already on disk
                let history = std::mem::take(&mut outcome.history);
                self.seed = history;
                Ok((outcome, reasoning))
            }
            // a failed round still saw real work: the stream reports it and
            // the round in flight persists, exactly as in the terminal path
            Err(failure) => {
                emit_event(&serde_json::json!({
                    "type": "error",
                    "message": &failure.message,
                    "text": &failure.final_text,
                }));
                persist_round(
                    self.store.as_ref(),
                    &mut self.conversation_id,
                    &mut self.persist_error,
                    &TaskIdentity {
                        model,
                        cwd: &self.cwd,
                        system: self.system.as_deref(),
                    },
                    &failure.history[failure.round_start.min(failure.history.len())..],
                    None,
                    &round_reasoning,
                    round_started,
                );
                self.seed = failure.history;
                Err(failure.message)
            }
        }
    }
}

/// The per-task facts every round line carries.
struct TaskIdentity<'a> {
    model: &'a crate::providers::ResolvedModel,
    cwd: &'a std::path::Path,
    /// the assembled system prompt — stamped on the thread's first line only
    system: Option<&'a str>,
}

/// Append one agent round to the thread file. A round is the slice of
/// history one model call produced: the pending prompt, the assistant
/// message, its tool results. The trailing no-tool assistant doubles as the
/// line's `response`, exactly as the task-level turn did — a thread file is
/// still one `StoredTurn` JSON object per line, just at round granularity.
#[allow(clippy::too_many_arguments)]
fn persist_round(
    store: Option<&crate::core::threads::Store>,
    conversation_id: &mut Option<String>,
    persist_error: &mut Option<String>,
    identity: &TaskIdentity<'_>,
    messages: &[Msg],
    usage: Option<crate::core::http::Usage>,
    reasoning: &str,
    started: std::time::Instant,
) {
    let Some(store) = store else {
        return;
    };
    if messages.is_empty() {
        return;
    }
    let mut new_messages: Vec<Msg> = messages.to_vec();
    // a trailing no-tool assistant message doubles as the round response
    let ends_plain =
        matches!(messages.last(), Some(Msg::Assistant { tool_calls, .. }) if tool_calls.is_empty());
    let response = if ends_plain {
        match new_messages.pop() {
            Some(Msg::Assistant { text, .. }) => text,
            _ => String::new(),
        }
    } else {
        String::new()
    };
    // cwd rides in turn options as provenance so `yak logs` can show
    // and filter conversations by project directory
    let mut turn_options = identity.model.options.clone();
    turn_options.push(("cwd".to_string(), identity.cwd.display().to_string()));
    let attached: Vec<String> = messages
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
    // the system prompt is the thread's head: stamp it on the first line
    // only, or every round line carries several KB of cached-prefix copy
    let system = conversation_id
        .is_none()
        .then_some(identity.system)
        .flatten()
        .map(str::to_string);
    // the round's user text is the line's prompt preview (empty on pure
    // tool rounds; the thread list scans back for the newest non-empty one)
    let prompt = messages
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
        model: identity.model.qualified_id(),
        cwd: Some(identity.cwd.display().to_string()),
        system,
        prompt,
        response,
        reasoning: if reasoning.is_empty() {
            None
        } else {
            Some(reasoning.to_string())
        },
        usage: usage.map(crate::core::threads::TurnUsage::from),
        duration_ms: Some(started.elapsed().as_millis() as i64),
        options: turn_options,
        messages: stored_messages(&new_messages),
    };
    let thread_id = match store.append_turn(conversation_id.as_deref(), &turn) {
        Ok(id) => id,
        Err(e) => {
            // sticky: the run continues in memory, but the banner and
            // /status keep saying the transcript is not being written
            eprintln!("Warning: {e}");
            *persist_error = Some(e);
            String::new()
        }
    };
    if conversation_id.is_none() && !thread_id.is_empty() {
        *conversation_id = Some(thread_id);
    }
}
/// One event as the `--json` stream writes it. The mapping is a contract:
/// the field names are what supervising processes parse, so a new
/// [`AgentUpdate`] variant gets a new `type` rather than a reshuffle.
fn event_json(u: &AgentUpdate) -> serde_json::Value {
    match u {
        AgentUpdate::Delta(text) => serde_json::json!({"type": "text", "text": text}),
        // internal persistence marker: run_task_json intercepts it before
        // this map (it drives the thread file, not the event stream), so it
        // never reaches a consumer; the arm keeps the mapping total
        AgentUpdate::RoundEnd { .. } => serde_json::json!({"type": "round"}),
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
        AgentUpdate::ToolEnd {
            summary,
            is_error,
            duration,
        } => serde_json::json!({
            "type": "tool_end",
            "summary": summary,
            "is_error": is_error,
            "durationMs": duration.as_millis() as u64
        }),
        AgentUpdate::TurnEnd { usage } => {
            serde_json::json!({"type": "turn_end", "usage": usage_json(usage.as_ref())})
        }
        AgentUpdate::Compacted { removed } => {
            serde_json::json!({"type": "compacted", "removed": removed})
        }
        AgentUpdate::CompactStalled { reason } => {
            serde_json::json!({"type": "compact_stalled", "reason": reason})
        }
        AgentUpdate::StreamRecovered { chars, error } => {
            serde_json::json!({"type": "stream_recovered", "chars": chars, "error": error})
        }
        AgentUpdate::ToolResultsPruned { count } => {
            serde_json::json!({"type": "tool_results_pruned", "count": count})
        }
    }
}

/// One round's usage in the stream's shape; `null` when the provider reported
/// none (the field stays present so consumers never branch on its absence).
/// `input` is every prompt token the round sent, cached reads included, so the
/// two cache numbers are what a consumer prices: `cached` was read back,
/// `cached_write` was written into the provider's cache (Anthropic bills it;
/// the OpenAI-compatible wires keep no such count and report 0), and the rest
/// was billed at full price.
fn usage_json(usage: Option<&crate::core::http::Usage>) -> serde_json::Value {
    match usage {
        Some(u) => serde_json::json!({
            "input": u.input,
            "output": u.output,
            "cached": u.cached,
            "cached_write": u.cached_write,
        }),
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

/// A thread read back for a resume.
pub struct Rebuilt {
    /// the wire-level history, ready to be a seed
    pub messages: Vec<Msg>,
    /// the original system prompt, from the first turn
    pub system: Option<String>,
    /// what the conversation has spent so far, per the transcript: a resumed
    /// session starts its totals here, so `/status` describes the conversation
    /// rather than the process that happened to reopen it
    pub usage: crate::core::http::Usage,
}

/// Rebuild a wire-level history (plus the original system prompt) from a
/// thread's stored turns. The messages *are* `Msg` — the thread stores the
/// same struct the request carries — so the walk only filters empty user
/// turns and restores the final assistant answer, which the store pops out
/// of `messages` and keeps as the turn's `response` field. The first turn's
/// system is the prompt; later `Summary` messages are compaction summaries.
pub fn rebuild_turns(turns: &[StoredTurn]) -> Rebuilt {
    let mut msgs: Vec<Msg> = Vec::new();
    let mut system: Option<String> = None;
    let mut usage = crate::core::http::Usage::default();
    for turn in turns {
        if system.is_none() {
            system = turn.system.clone();
        }
        if let Some(u) = turn.usage {
            usage.add(u.into());
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
    Rebuilt {
        messages: msgs,
        system,
        usage,
    }
}

/// How many of the newest user turns keep their pixels on a replay. Every
/// image in the request is billed on every request, so a thread that once
/// carried screenshots would keep paying for them; older turns keep their
/// provenance and the adapters render the dropped payload as a note.
const IMAGE_TURNS_KEPT: usize = 2;

/// Drop the pixels from images older than the newest `IMAGE_TURNS_KEPT` user
/// turns — from tool results too, since a screenshot can enter as a `read`
/// result and belongs to the turn it ran in.
pub(crate) fn budget_images(msgs: &mut [Msg]) {
    let Some(start) = image_window_start(msgs) else {
        return;
    };
    for m in &mut msgs[..start] {
        drop_images(m);
    }
}

/// Where the kept image window starts: the index of the `IMAGE_TURNS_KEPT`-th
/// newest user turn. The window is anchored to turns, not to image-carrying
/// turns — a lone screenshot must age out as the conversation moves on, or a
/// thread that ever carried one pays for it forever. `None` when the history
/// holds fewer user turns than the cap, so nothing is taken away.
fn image_window_start(msgs: &[Msg]) -> Option<usize> {
    // walk from the end; the cap-th newest user turn is where the window
    // opens, and fewer than `IMAGE_TURNS_KEPT` user turns keeps everything
    let mut left = IMAGE_TURNS_KEPT;
    for (i, m) in msgs.iter().enumerate().rev() {
        if matches!(m, Msg::User { .. }) {
            left -= 1;
            if left == 0 {
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
    for a in attachments.iter_mut().filter(|a| a.is_image()) {
        a.base64_data.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive `persist_round` the way the loop's `RoundEnd` arm does.
    fn persist_test_round(session: &mut Session, messages: &[Msg]) {
        persist_round(
            session.store.as_ref(),
            &mut session.conversation_id,
            &mut session.persist_error,
            &TaskIdentity {
                model: session.model.as_ref().expect("test session has a model"),
                cwd: &session.cwd,
                system: session.system.as_deref(),
            },
            messages,
            None,
            "",
            std::time::Instant::now(),
        );
    }

    /// Read one mock-server request: past the request head plus its
    /// content-length body (the same reader `agent::tests` uses).
    fn read_request(c: &mut std::net::TcpStream) {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            use std::io::Read;
            if c.read(&mut byte).unwrap_or(0) == 0 {
                break;
            }
            buf.push(byte[0]);
            let head_end = buf.windows(4).rposition(|w| w == b"\r\n\r\n");
            if let Some(i) = head_end {
                let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if buf.len() - i - 4 >= len {
                    break;
                }
            }
        }
    }

    /// A session with a tiny window, so a single stored result is over.
    fn tight_session(seed: Vec<Msg>) -> Session {
        Session {
            max_request_bytes: crate::core::http::MAX_REQUEST_BYTES,
            compact: CompactConfig {
                trigger_tokens: 4_000,
                keep_recent_tokens: 100,
            },
            cache_ttl: None,
            model: Some(crate::providers::ResolvedModel {
                provider_name: "mock".into(),
                kind: "openai-compat".into(),
                base_url: "http://127.0.0.1:9/v1".into(),
                api_key: None,
                model_id: "m".into(),
                context_window: None,
                options: vec![],
            }),
            tools: Vec::new(),
            system: None,
            cwd: std::env::temp_dir(),
            stream: true,
            store: None,
            approval: crate::agent::approval::ApprovalConfig::default(),
            conversation_id: None,
            cache_key: "cache-test".to_string(),
            seed,
            thinking: None,
            extensions: crate::agent::ext::Extensions::connect(std::path::Path::new(
                "/nonexistent",
            )),
            usage: crate::core::http::Usage::default(),
            last_usage: None,
            json: false,
            persist_error: None,
        }
    }

    /// A session with no model — a bare interactive start on a fresh install —
    /// opens the REPL but cannot run a task: the failure names the commands
    /// that configure one instead of silently doing nothing.
    #[test]
    fn a_task_without_a_model_fails_loudly() {
        let mut session = tight_session(Vec::new());
        session.model = None;
        let err = match session.run_task("hello", Vec::new()) {
            Ok(_) => panic!("no model must refuse the task"),
            Err(e) => e,
        };
        assert!(err.contains("/login"), "the error points at /login: {err}");
    }

    /// A conversation carried into the next task names the cache anchor for
    /// it, so the first request of a REPL turn or a resume reads the history
    /// back instead of writing it again. A session with nothing carried has
    /// no anchor to name, and neither has one that just started over.
    #[test]
    fn a_carried_history_names_the_cache_anchor() {
        assert_eq!(tight_session(Vec::new()).cache_anchor(), None);
        let mut carried = tight_session(vec![Msg::user("hi"), Msg::assistant("ok")]);
        assert_eq!(carried.cache_anchor(), Some(2));
        carried.clear();
        assert_eq!(carried.cache_anchor(), None);
    }

    /// Resuming a thread that no longer fits projects its oversized results
    /// down once, before the first request — the loop must not have to cut
    /// them again on the first turn end (and say so every turn).
    #[test]
    fn a_resumed_seed_over_the_window_is_projected_down_once() {
        let mut session = tight_session(vec![
            Msg::user("read the log"),
            Msg::tool_result("1", "read", "z".repeat(40_000)),
        ]);
        session.prune_seed_to_fit();
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
        // already-projected: a second pass changes nothing at all
        let before = format!("{:?}", session.seed);
        session.prune_seed_to_fit();
        assert_eq!(format!("{:?}", session.seed), before);
    }

    /// A thread that fits is left exactly as loaded — the projection is a
    /// pressure measure, not a property of every resume.
    #[test]
    fn a_resumed_seed_under_the_window_is_left_alone() {
        let seed = vec![
            Msg::user("read the log"),
            Msg::tool_result("1", "read", "q".repeat(40_000)),
        ];
        let mut session = tight_session(seed.clone());
        session.compact = CompactConfig {
            trigger_tokens: 128_000,
            keep_recent_tokens: 32_000,
        };
        session.prune_seed_to_fit();
        assert_eq!(session.seed, seed);
    }

    /// A tool round followed by a plain answer writes one thread line per
    /// round, and the rebuilt history is the exact wire conversation — the
    /// crash-safety property: every completed round is on disk the moment it
    /// ends, so a kill mid-task loses only the round in flight.
    #[test]
    fn a_multi_round_task_persists_one_line_per_round_and_resumes_whole() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits2 = hits.clone();
        let server = std::thread::spawn(move || {
            use std::io::Write as _;
            for conn in listener.incoming().flatten() {
                let n = hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut c = conn;
                read_request(&mut c);
                let sse = if n == 0 {
                    // round 1: a tool call
                    let call = serde_json::json!({
                        "choices": [{
                            "index": 0,
                            "delta": {"tool_calls": [{
                                "index": 0,
                                "id": "c1",
                                "function": {"name": "ls", "arguments": "{\"path\": \".\"}"}
                            }]},
                            "finish_reason": null
                        }]
                    });
                    let done = serde_json::json!({
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
                    });
                    format!("data: {call}\n\ndata: {done}\n\ndata: [DONE]\n\n")
                } else {
                    // round 2: the plain answer ends the task
                    let done = serde_json::json!({
                        "choices": [{
                            "index": 0,
                            "delta": {"content": "all done"},
                            "finish_reason": "stop"
                        }]
                    });
                    format!("data: {done}\n\ndata: [DONE]\n\n")
                };
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{sse}",
                        sse.len()
                    )
                    .as_bytes(),
                );
                if n >= 1 {
                    break;
                }
            }
        });
        let dir = crate::core::testutil::scratch_path("round-persist");
        let store = threads::Store::open_path(&dir).unwrap();
        let mut session = Session {
            max_request_bytes: crate::core::http::MAX_REQUEST_BYTES,
            compact: CompactConfig::default(),
            cache_ttl: None,
            model: Some(crate::providers::ResolvedModel {
                provider_name: "mock".into(),
                kind: "openai-compat".into(),
                base_url: format!("http://127.0.0.1:{port}/v1"),
                api_key: None,
                model_id: "m".into(),
                context_window: None,
                options: vec![],
            }),
            tools: vec![],
            system: None,
            cwd: dir.clone(),
            stream: true,
            store: Some(store),
            approval: crate::agent::approval::ApprovalConfig::default(),
            conversation_id: None,
            cache_key: "round-persist".to_string(),
            seed: Vec::new(),
            thinking: None,
            extensions: crate::agent::ext::Extensions::connect(&dir),
            usage: crate::core::http::Usage::default(),
            last_usage: None,
            // the --json driver: identical loop and persistence, without the
            // KeyWatcher (whose blocking stdin read has no terminal here)
            json: true,
            persist_error: None,
        };
        let (_outcome, _) = session
            .run_task("list the files", vec![])
            .expect("the task completes");
        server.join().unwrap();

        // one line per round: the tool round and the answer round
        let cid = session.conversation_id.clone().expect("thread created");
        let turns = session.store.as_ref().unwrap().read_thread(&cid).unwrap();
        assert_eq!(turns.len(), 2, "one StoredTurn line per agent round");
        assert_eq!(turns[0].messages.len(), 3, "prompt, tool call, result");
        assert_eq!(turns[0].response, "", "the tool round has no answer");
        assert_eq!(
            turns[1].response, "all done",
            "the answer is the last line's response"
        );
        assert_eq!(
            turns[1].messages.len(),
            0,
            "the plain answer rides the response field"
        );

        // the rebuilt history is the exact wire conversation the seed now holds
        let rebuilt = rebuild_turns(&turns);
        assert_eq!(rebuilt.messages, session.seed);
        let _ = std::fs::remove_dir_all(&dir);
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
            cache_ttl: None,
            model: Some(crate::providers::ResolvedModel {
                provider_name: "mock".into(),
                kind: "openai-compat".into(),
                base_url: "http://127.0.0.1:9/v1".into(),
                api_key: None,
                model_id: "m".into(),
                context_window: None,
                options: vec![],
            }),
            tools: Vec::new(),
            system: None,
            cwd: cwd.clone(),
            stream: true,
            store: Some(store),
            approval: crate::agent::approval::ApprovalConfig::default(),
            conversation_id: None,
            cache_key: "cache-test".to_string(),
            seed: Vec::new(),
            thinking: None,
            extensions: crate::agent::ext::Extensions::connect(&cwd),
            usage: crate::core::http::Usage::default(),
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
        persist_test_round(&mut session, &history);
        let cid = session.conversation_id.clone().expect("thread created");
        let turns = session.store.as_ref().unwrap().read_thread(&cid).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].messages.len(), 3, "prompt, tool call, result");
        assert_eq!(turns[0].response, "");

        // an empty round records nothing
        persist_test_round(&mut session, &[]);
        let turns = session.store.as_ref().unwrap().read_thread(&cid).unwrap();
        assert_eq!(turns.len(), 1, "an empty round persists nothing");
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
            cache_ttl: None,
            model: Some(crate::providers::ResolvedModel {
                provider_name: "mock".into(),
                kind: "openai-compat".into(),
                base_url: "http://127.0.0.1:9/v1".into(),
                api_key: None,
                model_id: "m".into(),
                context_window: None,
                options: vec![],
            }),
            tools: Vec::new(),
            system: None,
            cwd: cwd.clone(),
            stream: true,
            store: Some(store),
            approval: crate::agent::approval::ApprovalConfig::default(),
            conversation_id: None,
            cache_key: "cache-test".to_string(),
            seed: Vec::new(),
            thinking: None,
            extensions: crate::agent::ext::Extensions::connect(&cwd),
            usage: crate::core::http::Usage::default(),
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
        persist_test_round(&mut session, &history);
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

    /// A screenshot that entered as a `read` result (auto-attach missed the
    /// path) must still age out; the window is anchored to user turns, not to
    /// turns that happen to carry an image.
    #[test]
    fn a_lone_tool_result_image_ages_out_with_the_turns() {
        let shot = || {
            vec![crate::core::attachments::from_bytes(
                Some("image/png"),
                vec![1, 2, 3],
            )]
        };
        let mut msgs = vec![
            Msg::User {
                text: "look at this".into(),
                attachments: Vec::new(),
            },
            Msg::assistant("reading".to_string()),
            Msg::ToolResult {
                call_id: "1".into(),
                name: "read".into(),
                content: "shot".into(),
                error: None,
                attachments: shot(),
            },
            Msg::assistant("there".to_string()),
            Msg::User {
                text: "first later turn".into(),
                attachments: Vec::new(),
            },
            Msg::User {
                text: "second later turn".into(),
                attachments: Vec::new(),
            },
        ];
        budget_images(&mut msgs);
        match &msgs[2] {
            Msg::ToolResult { attachments, .. } => assert!(
                attachments[0].base64_data.is_empty(),
                "the old tool-result screenshot goes"
            ),
            _ => panic!("not a tool result"),
        }
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
        let msgs = rebuild_turns(&[turn(0), turn(1), turn(2)]).messages;
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

    /// A resume starts the session's totals where the transcript left off, so
    /// the next `/status` describes the conversation rather than the process
    /// that happened to reopen it. Turns written before the cache split was
    /// kept contribute what they have and leave the cache at zero.
    #[test]
    fn a_replayed_thread_sums_the_usage_it_holds() {
        let turn = |id: &str, usage: Option<crate::core::threads::TurnUsage>| StoredTurn {
            v: crate::core::threads::THREAD_FORMAT_VERSION,
            id: id.into(),
            ts: "2026-08-23T01:00:00+00:00".into(),
            mode: "agent".into(),
            model: "prov/m".into(),
            cwd: None,
            system: Some("sys".into()),
            prompt: "p".into(),
            response: String::new(),
            reasoning: None,
            usage,
            duration_ms: None,
            options: Vec::new(),
            messages: vec![Msg::user("p")],
        };
        let rebuilt = rebuild_turns(&[
            turn(
                "t1",
                Some(crate::core::threads::TurnUsage {
                    input: 100,
                    output: 10,
                    cached: 80,
                    cached_write: 20,
                }),
            ),
            turn(
                "t2",
                Some(crate::core::threads::TurnUsage {
                    input: 50,
                    output: 5,
                    cached: 45,
                    cached_write: 0,
                }),
            ),
            // a turn with no usage report adds nothing at all
            turn("t3", None),
        ]);
        assert_eq!(
            rebuilt.usage,
            crate::core::http::Usage {
                input: 150,
                output: 15,
                cached: 125,
                cached_write: 20,
            }
        );
        assert_eq!(rebuilt.usage.cache_percent(), 83);
        assert_eq!(rebuilt.usage.write_percent(), 13);
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
        let msgs = rebuild_turns(&[turn]).messages;
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
        let rebuilt = rebuild_turns(&turns);
        let (msgs, system) = (rebuilt.messages, rebuilt.system);
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
                    duration: std::time::Duration::from_millis(1500),
                },
                json!({"type": "tool_end", "summary": "ok", "is_error": false, "durationMs": 1500}),
            ),
            (
                AgentUpdate::TurnEnd {
                    usage: Some(crate::core::http::Usage {
                        input: 10,
                        output: 2,
                        cached: 8,
                        cached_write: 1,
                    }),
                },
                json!({"type": "turn_end", "usage":
                    {"input": 10, "output": 2, "cached": 8, "cached_write": 1}}),
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
                AgentUpdate::CompactStalled {
                    reason: "the summarizer returned nothing".into(),
                },
                json!({"type": "compact_stalled", "reason": "the summarizer returned nothing"}),
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
