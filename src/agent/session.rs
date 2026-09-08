//! The agent session: one model + tools + accumulated history, driving
//! tasks and persisting turns. Shared by the one-shot CLI and the REPL.

use std::path::PathBuf;

use crate::agent::approval::{self, ApprovalConfig};
use crate::agent::compact::CompactConfig;
use crate::agent::{AgentOptions, AgentUpdate, ApprovalRequest, ApprovalResponse, run_agent};
use crate::core::threads::{self, StoredAttachment, StoredMsg, StoredToolCall, StoredTurn};
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
    pub no_session: bool,
    pub store: Option<threads::Store>,
    pub approval: ApprovalConfig,
    pub conversation_id: Option<String>,
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
}

impl Session {
    /// The turn-provenance mode stamp (always "agent").
    pub fn mode_label(&self) -> &str {
        "agent"
    }

    /// Reset the in-memory session: drop history, forget the conversation id
    /// and token counters. The stored log is untouched.
    pub fn clear(&mut self) {
        self.seed.clear();
        self.conversation_id = None;
        self.tokens = (0, 0);
        self.tokens_cached = 0;
    }

    /// Switch the session's model (the `/model` command): re-resolve,
    /// keep history, rebuild the tool registry.
    pub fn switch_model(&mut self, qualified: &str) -> Result<(), String> {
        let model = crate::providers::resolve_model_by_id(qualified)?;
        self.model = model;
        self.rebuild_tools();
        Ok(())
    }

    /// Replace the oldest `cut` messages with one summary, keeping the tail
    /// and turning the conversation into `[summary, ...tail]`.
    pub fn compact_prefix(&mut self, summary: String, cut: usize) {
        let tail = self.seed.split_off(cut);
        self.seed.clear();
        self.seed
            .push(crate::providers::Msg::Summary { text: summary });
        self.seed.extend(tail);
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
        let opts = AgentOptions {
            system: self.system.as_deref(),
            cwd: self.cwd.clone(),
            max_turns: self.max_turns,
            stream: self.stream,
            compact: Some(self.compact.clone()),
            reasoning: self.thinking.clone(),
        };
        let model_id = self.model.model_id.clone();
        // the shared TaskView owns the answer stream, spinner, thinking
        // trace and footer (indent 2); tool chrome stays local
        // shared behind a RefCell so the approval callback can pause the
        // spinner before printing its banner (otherwise they race mid-line)
        let view = std::cell::RefCell::new(crate::term::render::TaskView::new(2, &model_id, true));
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
                AgentUpdate::ToolEnd { summary, is_error } => {
                    {
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
                AgentUpdate::TurnEnd { usage, .. } => {
                    if let Some(u) = usage {
                        total_in += u.input;
                        total_out += u.output;
                        total_cached += u.cached;
                    }
                    view.borrow_mut().turn_end(usage);
                }
                AgentUpdate::Compacted { removed } => {
                    let _ = removed;
                    // settle the streaming partial line; auto-compaction is
                    // silent on the terminal
                    view.borrow_mut().pause();
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
        let seed_len = self.seed.len();
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
        self.tokens.0 += total_in;
        self.tokens.1 += total_out;
        self.tokens_cached += total_cached;
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

    /// Persist the turn: the wire-level messages so `-c` can replay it.
    /// Runs inside `run_task` (the callers never see the history). Skipped
    /// with --no-session.
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
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let mut new_messages: Vec<StoredMsg> = history[seed_len.min(history.len())..]
            .iter()
            .map(msg_to_stored)
            .collect();
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
        // the first user text of the round is the prompt preview
        let prompt = history[seed_len.min(history.len())..]
            .iter()
            .find_map(|m| match m {
                Msg::User { text, .. } if !text.is_empty() => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let turn = StoredTurn {
            id: crate::core::db::ulid(),
            ts: crate::core::db::now_turn_datetime(),
            mode: self.mode_label().to_string(),
            model: self.model.qualified_id(),
            cwd: Some(self.cwd.display().to_string()),
            system: self.system.clone(),
            prompt,
            response: outcome.final_text.clone(),
            reasoning: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning.to_string())
            },
            usage: outcome.usage.map(|u| (u.input, u.output)),
            duration_ms: Some(start.elapsed().as_millis() as i64),
            options: turn_options,
            messages: new_messages,
        };
        let thread_id = store
            .append_turn(self.conversation_id.as_deref(), &turn)
            .map_err(|e| eprintln!("Warning: {e}"))
            .unwrap_or_default();
        if self.conversation_id.is_none() && !thread_id.is_empty() {
            self.conversation_id = Some(thread_id);
        }
    }
}

pub fn msg_to_stored(m: &Msg) -> StoredMsg {
    match m {
        Msg::User { text, attachments } => StoredMsg::User {
            text: text.clone(),
            attachments: attachments.iter().map(attachment_to_stored).collect(),
        },
        Msg::Summary { text } => StoredMsg::Summary { text: text.clone() },
        Msg::Assistant { text, tool_calls } => StoredMsg::Assistant {
            text: text.clone(),
            tool_calls: tool_calls
                .iter()
                .map(|c| StoredToolCall {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    arguments: c.arguments.clone(),
                })
                .collect(),
        },
        Msg::ToolResult {
            call_id,
            name,
            content,
            is_error,
            attachments,
        } => StoredMsg::Tool {
            call_id: call_id.clone(),
            name: name.clone(),
            content: content.clone(),
            is_error: *is_error,
            attachments: attachments.iter().map(attachment_to_stored).collect(),
        },
    }
}

fn attachment_to_stored(a: &crate::providers::Attachment) -> StoredAttachment {
    StoredAttachment {
        path: a.path.clone(),
        url: a.url.clone(),
        mime_type: Some(a.mime_type.clone()),
        base64: (!a.base64_data.is_empty()).then(|| a.base64_data.clone()),
    }
}

fn attachment_from_stored(a: &StoredAttachment) -> crate::providers::Attachment {
    crate::providers::Attachment {
        mime_type: a
            .mime_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".into()),
        base64_data: a.base64.clone().unwrap_or_default(),
        filename: None,
        path: a.path.clone(),
        url: a.url.clone(),
    }
}

/// Rebuild a wire-level history (plus the original system prompt) from a
/// thread's stored turns. The first turn's system is the prompt; later
/// `Summary` messages are compaction summaries.
pub fn rebuild_thread(store: &threads::Store, cid: &str) -> (Vec<Msg>, Option<String>) {
    let turns = match store.read_thread(cid) {
        Ok(t) => t,
        Err(_) => return (Vec::new(), None),
    };
    let mut msgs: Vec<Msg> = Vec::new();
    let mut system: Option<String> = None;
    for turn in &turns {
        if system.is_none() {
            system = turn.system.clone();
        }
        for m in &turn.messages {
            match m {
                StoredMsg::User { text, attachments } => {
                    if !text.is_empty() || !attachments.is_empty() {
                        msgs.push(Msg::user_with(
                            text.clone(),
                            attachments.iter().map(attachment_from_stored).collect(),
                        ));
                    }
                }
                StoredMsg::Assistant { text, tool_calls } => {
                    msgs.push(Msg::Assistant {
                        text: text.clone(),
                        tool_calls: tool_calls
                            .iter()
                            .map(|c| ToolCall {
                                id: c.id.clone(),
                                name: c.name.clone(),
                                arguments: c.arguments.clone(),
                            })
                            .collect(),
                    });
                }
                StoredMsg::Tool {
                    call_id,
                    name,
                    content,
                    is_error,
                    attachments,
                } => msgs.push(Msg::ToolResult {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                    attachments: attachments.iter().map(attachment_from_stored).collect(),
                }),
                StoredMsg::Summary { text } => msgs.push(Msg::Summary { text: text.clone() }),
            }
        }
        // the final plain assistant was popped out of `messages` (it doubles
        // as the turn response); ride it back in so a resume sees the answer
        if !turn.response.is_empty() {
            msgs.push(Msg::assistant(turn.response.clone()));
        }
    }
    (msgs, system)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_attachments_keep_their_real_provenance_in_storage() {
        // a wire attachment carrying loader provenance stores the path/url
        // it came from — never the bare display filename as a path
        let msg = crate::providers::Msg::user_with(
            "look",
            vec![crate::providers::Attachment {
                mime_type: "image/png".into(),
                base64_data: crate::b64::encode(b"pngbytes"),
                filename: Some("shot.png".into()),
                path: Some("/tmp/cam/2026/shot.png".into()),
                url: None,
            }],
        );
        let stored = msg_to_stored(&msg);
        let StoredMsg::User { attachments, .. } = &stored else {
            panic!("expected a user message");
        };
        assert_eq!(
            attachments[0].path.as_deref(),
            Some("/tmp/cam/2026/shot.png")
        );
        assert_eq!(attachments[0].url, None);

        // a URL attachment stores the URL
        let msg = crate::providers::Msg::user_with(
            "look",
            vec![crate::providers::Attachment {
                mime_type: "image/png".into(),
                base64_data: crate::b64::encode(b"pngbytes"),
                filename: Some("shot.png".into()),
                path: None,
                url: Some("https://example.com/shot.png?token=1".into()),
            }],
        );
        let stored = msg_to_stored(&msg);
        let StoredMsg::User { attachments, .. } = &stored else {
            panic!("expected a user message");
        };
        assert_eq!(
            attachments[0].url.as_deref(),
            Some("https://example.com/shot.png?token=1")
        );
        assert_eq!(attachments[0].path, None);
    }

    #[test]
    fn rebuild_thread_restores_final_response_and_attachments() {
        let dir = std::env::temp_dir().join(format!("llm-rebuild-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = threads::Store::open_path(&dir).unwrap();

        // user (with attachment) → assistant tool_call → tool result (with
        // attachment) → final plain assistant, stored only as `response`
        let turn = StoredTurn {
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
                StoredMsg::User {
                    text: "look at this".into(),
                    attachments: vec![StoredAttachment {
                        path: None,
                        url: None,
                        mime_type: Some("image/png".into()),
                        base64: Some("aGk=".into()),
                    }],
                },
                StoredMsg::Assistant {
                    text: String::new(),
                    tool_calls: vec![StoredToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "a.png"}),
                    }],
                },
                StoredMsg::Tool {
                    call_id: "c1".into(),
                    name: "read".into(),
                    content: "bytes".into(),
                    is_error: false,
                    attachments: vec![StoredAttachment {
                        path: Some("a.png".into()),
                        url: None,
                        mime_type: Some("image/png".into()),
                        base64: Some("aGk=".into()),
                    }],
                },
            ],
        };
        store.append_turn(Some("th1"), &turn).unwrap();

        let (msgs, system) = rebuild_thread(&store, "th1");
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
            Msg::Assistant { text, tool_calls } => {
                assert_eq!(text, "it is a cat");
                assert!(tool_calls.is_empty());
            }
            _ => panic!("expected final assistant"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
