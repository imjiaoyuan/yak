//! The agent session: one model + tools + accumulated history, driving
//! tasks and persisting turns. Shared by the one-shot CLI and the REPL.

use std::path::PathBuf;

use crate::agent::approval::{self, ApprovalConfig};
use crate::agent::compact::CompactConfig;
use crate::agent::{AgentOptions, AgentUpdate, ApprovalRequest, ApprovalResponse, run_agent};
use crate::core::db::Db;
use crate::core::logstore::{self, Message, Part, ReadPart, StoredAttachment};
use crate::providers::{Msg, ResolvedModel, ToolCall};

/// Everything one agent task needs; the interactive REPL reuses this across
/// tasks, evolving `seed`/`conversation_id`/`approval` as it goes.
pub struct Session {
    pub model: ResolvedModel,
    pub tools: Vec<Box<dyn crate::agent::tools::Tool>>,
    pub system: Option<String>,
    pub cwd: PathBuf,
    pub max_turns: usize,
    pub stream: bool,
    pub compact: CompactConfig,
    pub json_mode: bool,
    pub no_session: bool,
    pub db: Option<Db>,
    pub approval: ApprovalConfig,
    pub conversation_id: Option<String>,
    pub seed: Vec<Msg>,
    /// reasoning effort level; None sends no parameter
    pub thinking: Option<String>,
    /// steering lines typed mid-run; shared with the KeyWatcher, drained by
    /// the agent loop at tool-round boundaries and by the REPL afterwards
    pub steer_queue: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// chat mode: the tool-less conversational preset (`llm chat`) —
    /// no tools mount and turns are stamped mode "chat"
    pub chat_mode: bool,
    /// script tools from the config `tools` table; re-mounted by
    /// [`Session::rebuild_tools`] on every registry rebuild
    pub script_tools: Vec<crate::agent::script_tool::ScriptToolSpec>,
    /// live MCP clients; held in an Arc so the registry outlives rebuilds
    /// without orphaning the child processes (they die with the Session,
    /// RAII kill on drop); an empty registry when none are configured
    pub mcp: std::sync::Arc<crate::agent::mcp::McpRegistry>,
    /// cumulative input/output tokens across the session (for the status line)
    pub tokens: (u64, u64),
    /// cumulative input tokens served from the provider prompt cache
    pub tokens_cached: u64,
    /// first-touch write/edit snapshots for two-way `/undo`; enabled by the
    /// interactive REPL, None in one-shot and child-agent runs
    pub checkpoints:
        Option<std::sync::Arc<std::sync::Mutex<crate::agent::checkpoint::CheckpointState>>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // snapshots live and die with the interactive session
        if let Some(ck) = &self.checkpoints
            && let Ok(mut ck) = ck.lock()
        {
            ck.clear();
        }
    }
}

impl Session {
    /// The turn-provenance mode stamp ("agent" or "chat").
    pub fn mode_label(&self) -> &str {
        if self.chat_mode { "chat" } else { "agent" }
    }

    /// Rebuild the tool registry: built-ins plus the session's plugin tools
    /// (script tools and mounted MCP tools); called once at startup, and
    /// chat mode mounts none.
    pub fn rebuild_tools(&mut self, roles: &std::collections::BTreeMap<String, String>) {
        if self.chat_mode {
            self.tools = Vec::new();
            return;
        }
        let mut tools =
            crate::agent::tools::builtin_tools_configured(Some(&self.model.qualified_id()), roles);
        // drop-ins win over same-named config-table tools (the nearer the
        // home, the more specific), and a name never mounts twice
        let mut specs = crate::agent::user_tools::discover(&self.cwd);
        for cfg in &self.script_tools {
            match specs.iter_mut().find(|s| s.name == cfg.name) {
                Some(hit) => *hit = cfg.clone(),
                None => specs.push(cfg.clone()),
            }
        }
        tools.extend(specs.iter().map(crate::agent::script_tool::mount));
        self.mcp.mount_tools(&mut tools);
        self.tools = tools;
    }

    /// Run one task against the accumulated history. Attachments ride the
    /// task's user message (multimodal input).
    pub fn run_task(
        &mut self,
        prompt: &str,
        attachments: Vec<crate::providers::Attachment>,
    ) -> Result<(crate::agent::AgentOutcome, String), String> {
        let opts = AgentOptions {
            system: self.system.as_deref(),
            cwd: self.cwd.clone(),
            max_turns: self.max_turns,
            stream: self.stream,
            compact: Some(self.compact.clone()),
            reasoning: self.thinking.clone(),
            checkpoints: self.checkpoints.clone(),
        };
        let json_mode = self.json_mode;
        let model_id = self.model.model_id.clone();
        // the shared TaskView owns the answer stream, spinner, thinking
        // trace and footer (indent 2); tool chrome stays local
        // shared behind a RefCell so the approval callback can pause the
        // spinner before printing its banner (otherwise they race mid-line)
        let view =
            std::cell::RefCell::new(crate::core::render::TaskView::new(2, &model_id, !json_mode));
        view.borrow_mut().renderer_mut().terminal_md(2);
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
        let mut total_in = 0u64;
        let mut total_out = 0u64;
        let mut total_cached = 0u64;
        let mut on_update = |u: AgentUpdate| {
            match u {
                AgentUpdate::Delta(text) => {
                    if json_mode {
                        crate::agent::emit_json(
                            &serde_json::json!({"type": "delta", "text": text}),
                        );
                    } else {
                        view.borrow_mut().delta(&text);
                    }
                }
                AgentUpdate::ReasoningDelta(text) => {
                    if json_mode {
                        view.borrow_mut()
                            .renderer_mut()
                            .push_reasoning_buffered(&text);
                        crate::agent::emit_json(
                            &serde_json::json!({"type": "reasoning_delta", "text": text}),
                        );
                    } else {
                        view.borrow_mut().reasoning_delta(&text);
                    }
                }
                AgentUpdate::ToolStart {
                    name,
                    preview,
                    diff,
                } => {
                    if json_mode {
                        crate::agent::emit_json(
                            &serde_json::json!({"type": "tool_start", "name": name, "preview": preview, "diff": diff}),
                        );
                    } else {
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
                }
                AgentUpdate::ToolReceiving => {
                    if !json_mode {
                        // show a plain "running" status; the live argument
                        // size was confusing and the `$ run <cmd>` chrome
                        // line already shows the command
                        view.borrow_mut().receiving("running");
                    }
                }
                AgentUpdate::ToolLog(line) => {
                    if !json_mode {
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
                            let width = crate::term::columns().max(20);
                            let wrapped = crate::core::render_md::wrap_block(&line, width, 2);
                            eprintln!("\x1b[90m{wrapped}\x1b[0m");
                        } else {
                            // beyond the head: one line, rewritten in place
                            eprint!("\r\x1b[2K\x1b[90m  … +{n} lines\x1b[0m      ");
                            use std::io::Write;
                            let _ = std::io::stderr().flush();
                        }
                    }
                }
                AgentUpdate::ToolEnd {
                    name,
                    summary,
                    is_error,
                } => {
                    if json_mode {
                        crate::agent::emit_json(
                            &serde_json::json!({"type": "tool_end", "name": name, "summary": summary, "is_error": is_error}),
                        );
                    } else {
                        view.borrow_mut().pause();
                        if streamed.get() && !is_error {
                            // close the live counter line, if one is open
                            if logged.get() > LOG_HEAD {
                                eprintln!();
                            }
                        } else if is_error {
                            for (i, line) in summary.lines().enumerate() {
                                if i == 0 {
                                    eprintln!("\x1b[2m\x1b[31m  ✗ {line}\x1b[0m");
                                } else {
                                    eprintln!("\x1b[31m  {line}\x1b[0m");
                                }
                            }
                        } else {
                            let width = crate::term::columns().max(20);
                            for line in summary.lines() {
                                let wrapped = crate::core::render_md::wrap_block(line, width, 2);
                                eprintln!("\x1b[90m{wrapped}\x1b[0m");
                            }
                        }
                        // the next model round is awaited right after: spin,
                        // or the time-to-first-token reads as a hang
                        view.borrow_mut().resume_wait();
                    }
                }
                AgentUpdate::TurnEnd {
                    usage, elapsed_ms, ..
                } => {
                    if let Some(u) = usage {
                        total_in += u.input;
                        total_out += u.output;
                        total_cached += u.cached;
                    }
                    if json_mode {
                        crate::agent::emit_json(
                            &serde_json::json!({"type": "turn_end", "usage": usage.map(|u| serde_json::json!([u.input, u.output, u.cached])), "elapsed_ms": elapsed_ms}),
                        );
                    } else {
                        view.borrow_mut().turn_end(usage);
                    }
                }
                AgentUpdate::Compacted { removed } => {
                    if json_mode {
                        crate::agent::emit_json(
                            &serde_json::json!({"type": "compacted", "removed": removed}),
                        );
                    } else {
                        // settle the streaming partial line; auto-compaction is
                        // silent on the terminal (only json_mode reports it)
                        view.borrow_mut().pause();
                    }
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
            let answer = approval::prompt_approval(&req, json_mode, pre);
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
        let seed_len = self.seed.len();
        if let Some(ck) = &self.checkpoints
            && let Ok(mut ck) = ck.lock()
        {
            ck.begin_round(seed_len);
        }
        let result = run_agent(
            &self.model,
            &self.tools,
            prompt,
            attachments,
            std::mem::take(&mut self.seed),
            &opts,
            &mut self.approval,
            &mut on_update,
            &mut on_approval,
            &mut steer,
        );
        watcher.stop();
        // an interrupted task may never see TurnEnd: flush any partial
        // markdown line here too (abort is idempotent)
        view.borrow_mut().abort();
        // the interrupt flag must not leak into the next prompt or task
        crate::core::http::clear_interrupt();
        // close the checkpoint round on every exit path so the undo stack
        // stays aligned with the conversation rounds
        if let Some(ck) = &self.checkpoints
            && let Ok(mut ck) = ck.lock()
        {
            ck.end_round();
        }
        self.tokens.0 += total_in;
        self.tokens.1 += total_out;
        self.tokens_cached += total_cached;
        match result {
            Ok(mut outcome) => {
                // one footer per completed task: totals across rounds and
                // wall time, right before the prompt returns. A user-initiated
                // interrupt already shows its own "interrupted" line and
                // should not dump a long-running elapsed/footer after it.
                if !json_mode && !outcome.interrupted {
                    view.borrow_mut().footer(task_start.elapsed().as_secs_f64());
                }
                // the history moves into the seed (no clone of the whole
                // conversation per task); persistence reads it first
                let history = std::mem::take(&mut outcome.history);
                let reasoning = view.into_inner().into_renderer().reasoning;
                self.persist_turn(seed_len, &history, &outcome, &reasoning, task_start);
                self.seed = history;
                Ok((outcome, reasoning))
            }
            // the failure carries what was already sent: the session
            // survives without a defensive clone taken up front
            Err(failure) => {
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

    /// Persist the turn: the wire-level chain (system + messages with tool
    /// parts) so `-c` can replay it. Runs inside `run_task` (the callers
    /// never see the history). Skipped with --no-session.
    fn persist_turn(
        &mut self,
        seed_len: usize,
        history: &[Msg],
        outcome: &crate::agent::AgentOutcome,
        reasoning: &str,
        start: std::time::Instant,
    ) {
        if self.no_session || outcome.final_text.is_empty() {
            return;
        }
        let Some(db) = self.db.as_ref() else { return };
        let mut new_messages: Vec<Message> = history[seed_len.min(history.len())..]
            .iter()
            .map(msg_to_message)
            .collect();
        // the final no-tool assistant message doubles as the turn response
        let ends_plain = matches!(history.last(), Some(Msg::Assistant { tool_calls, .. }) if tool_calls.is_empty());
        if ends_plain {
            new_messages.pop();
        }
        // cwd rides in turn options as provenance so `llm logs` can show
        // and filter conversations by project directory
        let mut turn_options = self.model.options.clone();
        turn_options.push(("cwd".to_string(), self.cwd.display().to_string()));
        turn_options.push(("mode".to_string(), self.mode_label().to_string()));
        let attached: Vec<String> = history[seed_len.min(history.len())..]
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
        logstore::log_completed_turn(
            db,
            &logstore::CompletedTurn {
                conversation_id: self.conversation_id.as_deref(),
                system: self.system.as_deref(),
                input_messages: &new_messages,
                reasoning: if reasoning.is_empty() {
                    None
                } else {
                    Some(reasoning)
                },
                response_text: &outcome.final_text,
                model: &self.model.qualified_id(),
                options: &turn_options,
                schema: None,
                usage: outcome.usage.map(|u| (u.input, u.output)),
                duration_ms: start.elapsed().as_millis() as i64,
            },
        );
        if self.conversation_id.is_none() {
            self.conversation_id = logstore::latest_conversation_id(db);
        }
    }
}

pub fn msg_to_message(m: &Msg) -> Message {
    match m {
        Msg::User { text, attachments } => {
            if attachments.is_empty() {
                return Message::text("user", text.clone());
            }
            let mut parts: Vec<Part> = Vec::new();
            if !text.is_empty() {
                parts.push(Part::Text(text.clone()));
            }
            for a in attachments {
                let content = match crate::b64::decode(&a.base64_data) {
                    Some(bytes) => bytes,
                    None => {
                        eprintln!(
                            "Warning: attachment {} failed to decode for storage; stored empty",
                            a.filename.as_deref().unwrap_or("attachment")
                        );
                        Vec::new()
                    }
                };
                parts.push(Part::Attachment(StoredAttachment {
                    path: a.filename.clone(),
                    url: None,
                    mime_type: Some(a.mime_type.clone()),
                    content,
                }));
            }
            Message {
                role: "user".into(),
                parts,
            }
        }
        Msg::Summary { text } => Message::text("system", text.clone()),
        Msg::Assistant { text, tool_calls } => {
            let mut parts: Vec<Part> = Vec::new();
            if !text.is_empty() {
                parts.push(Part::Text(text.clone()));
            }
            for c in tool_calls {
                parts.push(Part::ToolCall {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    arguments: c.arguments.clone(),
                });
            }
            Message {
                role: "assistant".into(),
                parts,
            }
        }
        Msg::ToolResult {
            call_id,
            name,
            content,
            is_error,
            ..
        } => Message {
            role: "tool".into(),
            parts: vec![Part::ToolResult {
                call_id: call_id.clone(),
                name: name.clone(),
                content: content.clone(),
                is_error: *is_error,
            }],
        },
    }
}

/// Rebuild a wire-level history (plus the original system prompt) from the
/// stored chain of a thread. The leading system message is the prompt; any
/// later system message is a compaction summary.
pub fn rebuild_thread(db: &Db, cid: &str) -> (Vec<Msg>, Option<String>) {
    let chain = logstore::thread_chain(db, cid);
    let mut msgs: Vec<Msg> = Vec::new();
    let mut system: Option<String> = None;
    let mut seen_first_system = false;
    for (role, parts) in &chain {
        let text_of = |parts: &[ReadPart]| -> String {
            parts
                .iter()
                .filter(|p| p.part_type == "text")
                .filter_map(|p| p.text.clone())
                .collect::<Vec<_>>()
                .join("")
        };
        match role.as_str() {
            "system" => {
                let text = text_of(parts);
                if !seen_first_system {
                    system = Some(text);
                    seen_first_system = true;
                } else {
                    msgs.push(Msg::Summary { text });
                }
            }
            "user" => {
                let text = text_of(parts);
                if !text.is_empty() {
                    msgs.push(Msg::user(text));
                }
            }
            "assistant" => {
                let mut text = String::new();
                let mut calls: Vec<ToolCall> = Vec::new();
                for p in parts {
                    match p.part_type.as_str() {
                        "text" => {
                            if let Some(t) = &p.text {
                                text.push_str(t);
                            }
                        }
                        "tool_call" => {
                            if let Some(payload) = &p.payload
                                && let Ok(v) = serde_json::from_str::<serde_json::Value>(payload)
                            {
                                calls.push(ToolCall {
                                    id: v["id"].as_str().unwrap_or_default().to_string(),
                                    name: v["name"].as_str().unwrap_or_default().to_string(),
                                    arguments: v["arguments"].clone(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
                msgs.push(Msg::Assistant {
                    text,
                    tool_calls: calls,
                });
            }
            "tool" => {
                for p in parts {
                    if p.part_type != "tool_result" {
                        continue;
                    }
                    let payload = p
                        .payload
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                        .unwrap_or(serde_json::json!({}));
                    msgs.push(Msg::ToolResult {
                        call_id: payload["call_id"].as_str().unwrap_or_default().to_string(),
                        name: payload["name"].as_str().unwrap_or_default().to_string(),
                        content: p.text.clone().unwrap_or_default(),
                        is_error: payload["is_error"].as_bool().unwrap_or(false),
                        attachments: Vec::new(),
                    });
                }
            }
            _ => {}
        }
    }
    (msgs, system)
}
