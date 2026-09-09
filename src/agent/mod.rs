//! Agent primitives: the built-in tools, approval, and the agent loop
//! driving `llm agent`. The conversation model it speaks (`Msg`, `ToolDef`,
//! `ToolCall`) lives in `providers` — see `providers/mod.rs`.

pub mod approval;
pub mod compact;
pub mod ext;
pub mod memory;
pub mod repl;
pub mod session;
pub mod settings;
pub mod skills;
pub mod system_prompt;
pub mod tools;

use crate::core::http::{StopReason, Usage};
use crate::providers::{Msg, PromptInput, ToolCall, ToolCallAccumulator, ToolDef};
use serde_json::json;

/// Progress events surfaced by the loop; `llm agent` renders these as text
/// or JSONL depending on --mode.
pub enum AgentUpdate {
    Delta(String),
    ReasoningDelta(String),
    ToolStart {
        name: String,
        preview: String,
        /// optional change preview (edit/write diffs) printed under the
        /// action line
        diff: Option<String>,
    },
    /// a live output line from the running tool (bash stdout)
    ToolLog(String),
    /// the model is streaming a tool call's arguments in; the spinner shows
    /// a plain "running" status while it arrives
    ToolReceiving,
    ToolEnd {
        summary: String,
        is_error: bool,
    },
    TurnEnd {
        usage: Option<Usage>,
    },
    /// history prefix was replaced by a compaction summary
    Compacted {
        removed: usize,
    },
}

/// What the UI answered to an approval request.
pub enum ApprovalResponse {
    Allow,
    /// allow this tool for the rest of the session
    AllowSession,
    Deny,
}

/// An approval prompt for one gated tool call.
pub struct ApprovalRequest<'a> {
    pub tool: &'a str,
    pub tier: approval::Tier,
    pub preview: &'a str,
    /// optional change preview (edit/write diffs) shown under the action line
    pub diff: Option<&'a str>,
    pub reason: &'a str,
}

pub struct AgentOptions<'a> {
    pub system: Option<&'a str>,
    pub cwd: std::path::PathBuf,
    pub max_turns: usize,
    pub stream: bool,
    /// enable compaction with this configuration; None disables it
    pub compact: Option<compact::CompactConfig>,
    /// reasoning effort level; None sends no parameter
    pub reasoning: Option<String>,
    /// the extension host, when extensions exist; event hooks fire at turn
    /// and tool boundaries (None when nothing was discovered)
    pub hooks: Option<&'a crate::agent::ext::Extensions>,
}

/// Fire an event on the host, swallowing failures into the extension's
/// diagnostics tail (events never abort a run; only `tool_call` denials
/// surface, as an error tool result).
fn fire(
    hooks: Option<&crate::agent::ext::Extensions>,
    name: &str,
    params: serde_json::Value,
) -> Result<Option<serde_json::Value>, String> {
    match hooks {
        Some(host) => host.fire(name, &params),
        None => Ok(None),
    }
}

pub struct AgentOutcome {
    /// full wire-level conversation (user, assistant with tool calls,
    /// results). `Session::run_task` takes this over into its seed — by the
    /// time callers see the outcome it reads empty.
    pub history: Vec<Msg>,
    /// final assistant text ("" if the run ended mid-tools)
    pub final_text: String,
    pub usage: Option<Usage>,
    /// the user interrupted the run (ctrl-c); partial history is kept
    pub interrupted: bool,
}

/// A provider-level failure: the error plus everything already sent, so the
/// caller's session survives without having cloned the history up front.
pub struct AgentFailure {
    pub message: String,
    pub history: Vec<Msg>,
}

const WRAP_UP_NOTE: &str = "[System] The turn budget is almost exhausted. Finish your current \
                            work and produce a final answer now; do not start new tool calls.";

/// Fold steering lines into the pending user message: multiple queued lines
/// join into one message, and an existing pending message keeps its text
/// first. Exposed for testing.
pub(crate) fn merge_steering(pending: Option<Msg>, queued: Vec<String>) -> Option<Msg> {
    if queued.is_empty() {
        return pending;
    }
    let joined = queued.join("\n\n");
    Some(match pending {
        Some(Msg::User { text, attachments }) => Msg::User {
            text: format!("{text}\n\n{joined}"),
            attachments,
        },
        _ => Msg::user(joined),
    })
}

/// Run the agent loop: stream an assistant response, execute its tool calls,
/// feed results back, repeat until the model stops calling tools or the turn
/// budget hits. Tool errors become error results (data, not failure); only
/// provider errors abort with Err, carrying the partial history so the
/// session survives without a defensive clone. `steer` is polled at every
/// tool-round boundary; lines the user typed mid-run are delivered as a user
/// message before the next model call.
#[allow(clippy::too_many_arguments)]
pub fn run_agent(
    model: &crate::providers::ResolvedModel,
    tools: &[Box<dyn tools::Tool>],
    prompt: &str,
    attachments: Vec<crate::providers::Attachment>,
    seed: Vec<Msg>,
    opts: &AgentOptions,
    approval: &mut approval::ApprovalConfig,
    on_update: &mut dyn FnMut(AgentUpdate),
    on_approval: &mut dyn FnMut(ApprovalRequest) -> ApprovalResponse,
    steer: &mut dyn FnMut() -> Vec<String>,
) -> Result<AgentOutcome, AgentFailure> {
    let tool_defs: Vec<ToolDef> = tools
        .iter()
        .map(|t| ToolDef {
            name: t.name().to_string(),
            description: t.description().to_string(),
            parameters: t.parameters(),
        })
        .collect();

    let max_turns = opts.max_turns.max(1);
    let soft_limit = (max_turns * 4 / 5).max(1);
    let mut history: Vec<Msg> = seed;
    let mut pending: Option<Msg> = Some(Msg::user_with(prompt, attachments));
    let mut warned = false;
    let mut last_usage = None;
    let mut final_text = String::new();
    let mut interrupted = false;

    let _ = fire(
        opts.hooks,
        "agent_start",
        json!({"cwd": opts.cwd.display().to_string(), "task": prompt}),
    );
    let mut turn = 0;
    loop {
        turn += 1;
        if turn > max_turns {
            break;
        }
        if turn == soft_limit && !warned {
            warned = true;
            let note = match pending.take() {
                Some(Msg::User { text, attachments }) => Msg::User {
                    text: format!("{text}\n\n{WRAP_UP_NOTE}"),
                    attachments,
                },
                _ => Msg::user(WRAP_UP_NOTE),
            };
            pending = Some(note);
        }

        // steering: queued mid-run input lands before the next model call
        pending = merge_steering(pending.take(), steer());
        let _ = fire(opts.hooks, "turn_start", json!({"turn": turn}));
        if let Some(Msg::User { text, attachments }) = pending.as_ref() {
            let _ = fire(
                opts.hooks,
                "input",
                json!({
                    "text": text,
                    "attachments": attachments.iter().map(|a| json!({
                        "path": a.path, "url": a.url, "mime_type": a.mime_type,
                    })).collect::<Vec<_>>(),
                }),
            );
        }

        let (pending_prompt, pending_attachments): (&str, &[crate::providers::Attachment]) =
            match pending.as_ref() {
                Some(Msg::User { text, attachments }) => (text.as_str(), attachments.as_slice()),
                _ => ("", &[]),
            };
        let has_pending = pending.is_some();
        // multimodal blocks are the most expensive, worst-cached context:
        // attachments older than the last few messages become notes now,
        // before the request is built (the idempotent pass is cheap)
        compact::trim_old_attachments(&mut history);
        // the system prompt stays byte-identical every round: it is the head
        // of the request, and providers cache by input prefix (DeepSeek
        // context caching, Anthropic prompt caching), so any per-turn suffix
        // here re-bills the whole history at cache-miss price
        let input = PromptInput {
            system: opts.system,
            history: &history,
            prompt: pending_prompt,
            attachments: pending_attachments,
            tools: &tool_defs,
            reasoning: opts.reasoning.as_deref(),
        };

        let mut text = String::new();
        let mut acc = ToolCallAccumulator::default();
        let mut usage = None;
        let mut stop = StopReason::default();
        let stream_result = model.stream(&input, opts.stream, &mut |event| match event {
            crate::core::http::Event::Delta(t) => {
                text.push_str(&t);
                on_update(AgentUpdate::Delta(t));
            }
            crate::core::http::Event::ReasoningDelta(t) => {
                on_update(AgentUpdate::ReasoningDelta(t));
            }
            crate::core::http::Event::ToolCallDelta {
                index,
                name,
                id,
                fragment,
            } => {
                acc.push(index, id.as_deref(), name.as_deref(), &fragment);
                // live size of the argument streaming in: a big write looks
                // dead otherwise, then dumps its whole diff at once
                if acc.name(index).is_some() {
                    on_update(AgentUpdate::ToolReceiving);
                }
            }
            crate::core::http::Event::Done { usage: u, stop: s } => {
                usage = u;
                stop = s;
            }
        });
        if let Err(e) = stream_result {
            if crate::core::http::interrupted() {
                interrupted = true;
                break;
            }
            return Err(AgentFailure {
                message: e,
                history,
            });
        }

        if has_pending {
            history.push(pending.take().expect("checked above"));
        }
        let tool_calls = acc.finish();
        history.push(Msg::Assistant {
            text: text.clone(),
            tool_calls: tool_calls.clone(),
        });
        final_text = text;
        last_usage = usage;
        on_update(AgentUpdate::TurnEnd { usage });
        let _ = fire(
            opts.hooks,
            "turn_end",
            json!({
                "turn": turn,
                "usage": usage.map(|u| json!([u.input, u.output, u.cached])),
            }),
        );

        // compaction check after each completed turn; the usage report
        // covered everything except the assistant we just pushed
        if let (Some(u), Some(cfg)) = (usage, opts.compact.as_ref()) {
            let estimate =
                compact::estimate_tokens(&history, Some((history.len().saturating_sub(1), u)));
            if compact::should_compact(estimate, cfg)
                && let Some(cut) = compact::find_cut(&history, cfg.keep_recent_tokens)
                && let Ok(s) = compact::summarize(model, &history[..cut])
                && !s.is_empty()
            {
                // the original task rides verbatim on top of the summary:
                // long-running work must not drift from what was asked. On
                // re-compaction it is recovered from the previous summary
                // (the first user message is long gone by then).
                let dropped = &history[..cut];
                let task = match dropped.first() {
                    Some(Msg::Summary { text }) => compact::extract_original_task(text),
                    _ => dropped.iter().find_map(|m| match m {
                        Msg::User { text, .. } => Some(text.clone()),
                        _ => None,
                    }),
                };
                let task = task.map(|t| {
                    if t.len() > 4000 {
                        let mut cut_text =
                            t[..crate::core::text::floor_boundary(&t, 4000)].to_string();
                        cut_text.push('…');
                        cut_text
                    } else {
                        t
                    }
                });
                let tail = history.split_off(cut);
                history.clear();
                history.push(Msg::Summary {
                    text: compact::compose_summary(task.as_deref(), &s),
                });
                history.extend(tail);
                on_update(AgentUpdate::Compacted { removed: cut });
            }
            // a failed summarization leaves the history untouched:
            // the run continues, possibly hitting the window later
        }

        if stop == StopReason::Length {
            // truncated output: don't act on possibly-mangled calls, let the
            // model re-issue them
            for call in &tool_calls {
                history.push(Msg::ToolResult {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    content: "The response was truncated before this tool call could run. \
                              Re-issue it with a shorter response."
                        .to_string(),
                    is_error: true,
                    attachments: Vec::new(),
                });
            }
            continue;
        }
        if stop != StopReason::ToolUse || tool_calls.is_empty() {
            break;
        }
        for mut call in tool_calls {
            if crate::core::http::interrupted() {
                interrupted = true;
                history.push(Msg::ToolResult {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    content: "interrupted by user".to_string(),
                    is_error: true,
                    attachments: Vec::new(),
                });
                continue;
            }
            // extension gate: a subscribed tool_call may deny, rewrite the
            // arguments, or allow (skip the built-in approval) before the
            // matrix even sees them
            let mut denied: Option<String> = None;
            let mut extension_allowed = false;
            match fire(
                opts.hooks,
                "tool_call",
                json!({"tool": call.name, "args": call.arguments}),
            ) {
                Err(reason) => denied = Some(reason),
                Ok(Some(reply)) => {
                    if reply.get("decision").and_then(serde_json::Value::as_str) == Some("allow") {
                        extension_allowed = true;
                    }
                    if let Some(rewritten) = reply.get("args") {
                        call.arguments = rewritten.clone();
                    }
                }
                Ok(None) => {}
            }
            let out = if let Some(reason) = denied {
                Err(reason)
            } else {
                match gate_call(
                    &call,
                    tools,
                    &opts.cwd,
                    approval,
                    on_approval,
                    extension_allowed,
                ) {
                    Err(denied) => Err(denied),
                    Ok(cleared) => {
                        on_update(AgentUpdate::ToolStart {
                            name: call.name.clone(),
                            preview: cleared.preview,
                            diff: cleared.diff,
                        });
                        let mut log =
                            |line: &str| on_update(AgentUpdate::ToolLog(line.to_string()));
                        let out = cleared.tool.execute(&call.arguments, &opts.cwd, &mut log);
                        // tool_result fires once, after the match below, so
                        // denied and executed calls notify hooks identically
                        Ok(out)
                    }
                }
            };
            let out = match out {
                Err(denied) => tools::ToolOutput::err(denied),
                Ok(out) => out,
            };
            let _ = fire(
                opts.hooks,
                "tool_result",
                json!({
                    "tool": call.name,
                    "summary": summarize(&out.content),
                    "is_error": out.is_error,
                }),
            );
            on_update(AgentUpdate::ToolEnd {
                summary: summarize(&out.content),
                is_error: out.is_error,
            });
            history.push(Msg::ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                content: out.content,
                is_error: out.is_error,
                attachments: out.attachments,
            });
        }
        if interrupted {
            break;
        }
    }

    let _ = fire(
        opts.hooks,
        "agent_end",
        json!({"final_text": final_text, "interrupted": interrupted}),
    );
    Ok(AgentOutcome {
        history,
        final_text,
        usage: last_usage,
        interrupted,
    })
}

/// Terminal preview of a tool result: the first ten non-empty lines, each
/// truncated, with a count of the lines that did not fit.
fn summarize(content: &str) -> String {
    /// lines shown in the user-facing tool-result preview (matches the reference's
    /// collapsed default); the model still receives the full output
    const SHOWN: usize = 10;
    let mut lines: Vec<String> = Vec::new();
    let mut more = 0usize;
    for l in content.lines().filter(|l| !l.trim().is_empty()) {
        if lines.len() < SHOWN {
            let mut s = l.to_string();
            if s.len() > 200 {
                s.truncate(crate::core::text::floor_boundary(&s, 200));
                s.push('…');
            }
            lines.push(s);
        } else {
            more += 1;
        }
    }
    if more > 0
        && let Some(last) = lines.last_mut()
    {
        last.push_str(&format!(" … +{more} lines"));
    }
    lines.join("\n")
}

/// A cleared tool call: the tool plus the preview and diff already computed
/// for the approval prompt, so the ToolStart chrome does not re-run them
/// (an edit's read+diff is real work).
struct ClearedCall<'a> {
    tool: &'a dyn tools::Tool,
    preview: String,
    diff: Option<String>,
}

/// Validate arguments and clear the approval gate. Ok(ClearedCall) means
/// cleared for execution; Err carries the denial/validation message that is
/// fed back to the model as an error result.
fn gate_call<'a>(
    call: &ToolCall,
    tools: &'a [Box<dyn tools::Tool>],
    cwd: &std::path::Path,
    approval: &mut approval::ApprovalConfig,
    on_approval: &mut dyn FnMut(ApprovalRequest) -> ApprovalResponse,
    extension_allowed: bool,
) -> Result<ClearedCall<'a>, String> {
    let Some(tool) = tools.iter().find(|t| t.name() == call.name) else {
        return Err(format!("tool '{}' not found", call.name));
    };
    if let Err(e) = tools::validate(&tool.parameters(), &call.arguments) {
        return Err(format!("invalid arguments: {e}"));
    }

    let escapes = call
        .arguments
        .get("path")
        .and_then(|p| p.as_str())
        .is_some_and(|p| approval::escapes_cwd(cwd, p));
    let bash_command = if tool.name() == "bash" {
        Some(call.arguments["command"].as_str().unwrap_or(""))
    } else {
        None
    };
    let (ask, reason) =
        match approval::resolve(tool.name(), tool.tier(), escapes, approval, bash_command) {
            approval::Decision::Deny(r) => return Err(format!("denied: {r}")),
            approval::Decision::Ask(r) => (true, r),
            approval::Decision::Auto => (false, String::new()),
        };
    let preview = tool.preview(&call.arguments);
    let diff = tool.diff(&call.arguments, cwd).filter(|d| !d.is_empty());
    if ask && !extension_allowed {
        let answer = on_approval(ApprovalRequest {
            tool: tool.name(),
            tier: tool.tier(),
            preview: &preview,
            diff: diff.as_deref(),
            reason: &reason,
        });
        match answer {
            ApprovalResponse::Allow => {}
            ApprovalResponse::AllowSession => {
                approval
                    .tool_policies
                    .insert(tool.name().to_string(), approval::Policy::Allow);
            }
            ApprovalResponse::Deny => {
                return Err(format!("denied by user: {preview}"));
            }
        }
    }
    Ok(ClearedCall {
        tool: tool.as_ref(),
        preview,
        diff,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn summarize_shows_ten_lines_plus_count() {
        assert_eq!(summarize("a\nb\nc\nd\ne\n"), "a\nb\nc\nd\ne");
        assert_eq!(
            summarize("a\n".repeat(12).as_str()),
            "a\na\na\na\na\na\na\na\na\na … +2 lines"
        );
        assert_eq!(summarize("only"), "only");
        assert_eq!(summarize(""), "");
        assert_eq!(summarize("\n\n1\n2\n"), "1\n2");
    }

    #[test]
    fn steering_joins_into_one_user_message() {
        let m = merge_steering(None, vec!["first".into(), "second".into()]).unwrap();
        assert!(matches!(m, Msg::User { ref text, .. } if text == "first\n\nsecond"));
        let m = merge_steering(Some(Msg::user("task")), vec!["steer".into()]).unwrap();
        assert!(matches!(m, Msg::User { ref text, .. } if text == "task\n\nsteer"));
        // nothing queued → pending untouched
        assert!(merge_steering(None, vec![]).is_none());
    }
    use super::*;

    #[test]
    fn accumulator_falls_back_on_bad_json() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(0, Some("c1"), Some("ls"), "not json at all");
        let calls = acc.finish();
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[0].arguments, json!({}));
    }

    #[test]
    fn accumulator_synthesizes_missing_ids_and_orders_by_index() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(2, None, Some("read"), "{}");
        acc.push(0, None, Some("ls"), "{}");
        let calls = acc.finish();
        assert_eq!(calls[0].name, "ls");
        assert_eq!(calls[0].id, "ls-0");
        assert_eq!(calls[1].id, "read-2");
    }

    #[test]
    fn later_id_and_name_do_not_clobber_earlier() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(0, Some("first"), Some("bash"), "{\"a\"");
        acc.push(0, Some(""), Some(""), ":1}");
        let calls = acc.finish();
        assert_eq!(calls[0].id, "first");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments, json!({"a": 1}));
    }
}
