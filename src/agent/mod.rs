//! Agent primitives: the built-in tools, approval, and the agent loop
//! driving `llm agent`. The conversation model it speaks (`Msg`, `ToolDef`,
//! `ToolCall`) lives in `providers` — see `providers/mod.rs`.

pub mod approval;
pub mod blacklist;
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
/// or JSONL (with --json) depending on the entry point.
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
    /// a dropped stream was recovered: the partial answer is kept as a real
    /// assistant message and the model continues from it (bounded per run)
    StreamRecovered {
        chars: usize,
        error: String,
    },
    /// oversized tool results were replaced by head+marker+tail views to
    /// relieve context pressure (the full text stays in the session log and
    /// is archived for the `recall` tool under its placeholder's id)
    ToolResultsPruned {
        count: usize,
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
    /// the blacklist pattern the command matched, when it did — highlighted
    /// in the prompt so the dangerous word is the first thing read
    pub pattern: Option<&'a str>,
}

pub struct AgentOptions<'a> {
    pub system: Option<&'a str>,
    pub cwd: std::path::PathBuf,
    /// maximum agent turns per task; 0 = unlimited (compaction and the
    /// token budget are the guardrails, not a turn cap)
    pub max_turns: usize,
    /// cumulative input-token budget for one task: past the soft share a
    /// wrap-up note is injected, at the cap the run stops with a visible
    /// line. 0 disables (the reference style rollout budget over turns alone)
    pub token_budget: u64,
    pub stream: bool,
    /// enable compaction with this configuration; None disables it
    pub compact: Option<compact::CompactConfig>,
    /// reasoning effort level; None sends no parameter
    pub reasoning: Option<String>,
    /// the extension host; event hooks fire at turn and tool boundaries.
    /// An empty host (nothing discovered) fires none and mounts nothing.
    pub hooks: &'a crate::agent::ext::Extensions,
    /// opaque id sent with every request so a multi-replica gateway routes
    /// one conversation to the same backend; automatic prefix caching is
    /// per-replica, so without it a round-robin hop re-bills the whole
    /// prompt. None for a one-shot call with no stable conversation.
    pub cache_key: Option<&'a str>,
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
    /// the token budget stopped the run; a follow-up continues seamlessly
    pub budget_exhausted: bool,
    /// index in `history` where this run's own messages start. Not the
    /// caller's seed length: compaction rewrites the prefix mid-run and
    /// shifts every index after the cut (see [`advance_seed_boundary`]).
    pub seed_boundary: usize,
}

/// A provider-level failure: the error plus everything already sent, so the
/// caller's session survives without having cloned the history up front.
/// `final_text` is the last completed answer (or the partial one a dropped
/// stream left behind), kept for the persisted turn.
#[derive(Debug)]
pub struct AgentFailure {
    pub message: String,
    pub history: Vec<Msg>,
    pub final_text: String,
    /// see [`AgentOutcome::seed_boundary`]
    pub seed_boundary: usize,
}

/// Where a run's own messages start after compaction replaced `cut` prefix
/// messages with one summary: everything after the cut shifts down by
/// `cut - 1`, and a boundary inside the dropped prefix lands right after
/// the summary — every remaining message is then this run's own.
pub(crate) fn advance_seed_boundary(boundary: usize, cut: usize) -> usize {
    if boundary > cut {
        boundary - cut + 1
    } else {
        1
    }
}

const WRAP_UP_NOTE: &str = "[System] The turn budget is almost exhausted. Finish your current \
                            work and produce a final answer now; do not start new tool calls.";

const BUDGET_NOTE: &str = "[System] The token budget for this task is almost exhausted. Finish your \
                            current work and produce a final answer now; do not start new tool calls.";

/// Consecutive identical-call counts that trigger an advisory reminder.
const REMIND_AT: [u32; 3] = [3, 5, 8];

/// A key-order-insensitive copy of `v`: object keys sorted recursively, so
/// two spellings of one arguments object compare equal.
fn canon_json(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = map
                .iter()
                .map(|(k, x)| (k.clone(), canon_json(x)))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(entries.into_iter().collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canon_json).collect())
        }
        other => other.clone(),
    }
}

/// Loop hygiene (a repeat-tool-reminder, in-tree): a model repeating
/// the exact same tool call cannot make progress, so at escalating repeat
/// counts an advisory note rides the tool result, asking it to analyze what
/// it has and change approach or finish. The count covers consecutive
/// identical calls (same tool, arguments compared modulo key order) and
/// clears when a new user message lands — a fresh instruction is never a
/// loop. The reminder is advisory: it never blocks a legitimate repeat. A
/// deny-flavored plugin twin lives at `examples/extensions/repeat_guard.py`
/// (load either one, not both — a denial there means this guard never sees
/// a third identical call).
struct RepeatGuard {
    last: Option<(String, serde_json::Value)>,
    count: u32,
}

impl RepeatGuard {
    fn observe(&mut self, tool: &str, args: &serde_json::Value) -> Option<String> {
        let key = (tool.to_string(), canon_json(args));
        let same = self.last.as_ref() == Some(&key);
        self.last = Some(key);
        self.count = if same { self.count + 1 } else { 1 };
        let n = self.count;
        if n == REMIND_AT[0] {
            return Some(format!(
                "\n\n[System] This is the {n}rd identical {tool} call in a row. The result will \
                 not change: analyze what you already have and either change approach or finish."
            ));
        }
        if REMIND_AT[1..].contains(&n) {
            let mut preview = args.to_string();
            crate::core::text::truncate_ellipsis(&mut preview, 500);
            return Some(format!(
                "\n\n[System] This is the {n}th identical {tool} call in a row. Repeating it \
                 verbatim cannot make progress: decide from the results already in hand — \
                 change approach, gather different evidence, or finish. Repeated arguments: {preview}"
            ));
        }
        None
    }

    fn reset(&mut self) {
        self.last = None;
        self.count = 0;
    }
}

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

    // turn cap: 0 = unlimited. other agents run unbounded loops guarded by
    // compaction and a token budget instead — a turn cap kills legitimate
    // large refactors whose context is nowhere near the window. kept as an
    // explicit escape hatch (--max-turns N).
    let max_turns = opts.max_turns;
    let soft_limit = if max_turns > 0 { max_turns * 4 / 5 } else { 0 };
    // token budget: cumulative input tokens across the task's model rounds.
    // Each round resends the full context, so the sum grows quadratically —
    // a runaway loop becomes visible long before the context window does.
    let token_budget = opts.token_budget;
    let soft_budget = if token_budget > 0 {
        token_budget * 4 / 5
    } else {
        0
    };
    let mut history: Vec<Msg> = seed;
    // persistence slices the turn out of `history`; compaction moves the goal
    // posts underneath that slice, so track the boundary as history changes
    let mut seed_boundary = history.len();
    let mut pending: Option<Msg> = Some(Msg::user_with(prompt, attachments));
    let mut repeats = RepeatGuard {
        last: None,
        count: 0,
    };
    let mut warned = false;
    let mut budget_warned = false;
    let mut spent_input = 0u64;
    let mut budget_exhausted = false;
    let mut last_usage = None;
    // the usage marker carried across turns: the provider's reported count
    // covers `history[..covered]`, so per-round estimates (the context note
    // and the compaction gate) only price the tail instead of rescanning
    // the whole history each round (O(n²) over a long task). Reset to None
    // wherever history is rebuilt (a compaction) so indices stay honest.
    let mut usage_marker: Option<(usize, Usage)> = None;
    // prefix length that has stopped changing (see PromptInput::cache_anchor):
    // set after each completed round, cleared wherever the loop edits history
    // in place, so a provider reading its own cache back never sees a prefix
    // that was rewritten under it.
    let mut cache_stable: Option<usize> = None;
    let mut final_text = String::new();
    let mut interrupted = false;
    // mid-stream drops recovered so far; the cap keeps a link that drops
    // every few KB from looping forever (each recovery re-enters the model
    // call with a fresh transport-level retry budget)
    let mut recoveries = 0;
    const MAX_STREAM_RECOVERIES: usize = 5;

    opts.hooks.fire(
        "agent_start",
        &json!({"cwd": opts.cwd.display().to_string(), "task": prompt}),
    );
    let mut turn = 0;
    loop {
        turn += 1;
        if max_turns > 0 && turn > max_turns {
            break;
        }
        if max_turns > 0 && turn == soft_limit && !warned {
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
        if token_budget > 0 && spent_input >= token_budget {
            budget_exhausted = true;
            break;
        }
        if token_budget > 0 && !budget_warned && spent_input >= soft_budget {
            budget_warned = true;
            let note = match pending.take() {
                Some(Msg::User { text, attachments }) => Msg::User {
                    text: format!("{text}\n\n{BUDGET_NOTE}"),
                    attachments,
                },
                _ => Msg::user(BUDGET_NOTE),
            };
            pending = Some(note);
        }

        // steering: queued mid-run input lands before the next model call
        pending = merge_steering(pending.take(), steer());
        // a fresh user message clears the repeat tracker: a new instruction
        // resets what counts as "the same call again"
        if pending.is_some() {
            repeats.reset();
        }
        opts.hooks.fire("turn_start", &json!({"turn": turn}));
        if let Some(Msg::User { text, attachments }) = pending.as_ref() {
            opts.hooks.fire(
                "input",
                &json!({
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
        // Context pressure, priced once per round to serve the rewrite gate,
        // the note and the compaction check below. Rewriting history in the
        // middle invalidates the provider's cached prefix from that point on,
        // so the two passes that rewrite it run only under real pressure:
        // below the gate the tokens they would save cost less than the cache
        // they would break (they stay unconditional when compaction is off,
        // since then nothing else guards the window).
        let cfg = opts.compact.as_ref();
        let mut used_tokens = cfg.map(|_| compact::estimate_tokens(&history, usage_marker));
        let rewrite = match (cfg, used_tokens) {
            (Some(c), Some(used)) => compact::rewrite_prefix(used, c),
            // no compaction configured: nothing else guards the window, so
            // the passes stay unconditional
            (None, _) | (Some(_), None) => true,
        };
        if rewrite {
            // multimodal blocks are the most expensive, worst-cached context:
            // attachments older than the last few messages become notes
            compact::trim_old_attachments(&mut history);
            // the same reasoning one step further: a huge tool result from
            // earlier in the task is re-sent every single round, so an
            // outright dump in the stale prefix is projected down here rather
            // than waited for until compaction pressure. Recent results stay
            // verbatim, and the pass is idempotent, so the notice fires at
            // most once per result (a resume replays the same archived id)
            let stale =
                compact::prune_stale_tool_results(&mut history, &compact::observation_dir());
            if stale.count > 0 {
                on_update(AgentUpdate::ToolResultsPruned { count: stale.count });
            }
            // the passes edited the prefix: re-price rather than hand the
            // pre-trim number to the note and the compaction gate
            used_tokens = cfg.map(|_| compact::estimate_tokens(&history, usage_marker));
        }
        // the system prompt stays byte-identical every round: it is the head
        // of the request, and providers cache by input prefix (DeepSeek
        // context caching, Anthropic prompt caching), so any per-turn suffix
        // here re-bills the whole history at cache-miss price
        // the reference style budget awareness: a terse note on how much room is
        // left rides the end of every request, so the model can decide to
        // wrap up instead of exploring indefinitely. Request-only: it never
        // enters the history or the prompt-cache prefix.
        let note = context_note(used_tokens, opts, spent_input);
        let input = PromptInput {
            system: opts.system,
            history: &history,
            prompt: pending_prompt,
            attachments: pending_attachments,
            tools: &tool_defs,
            reasoning: opts.reasoning.as_deref(),
            note: note.as_deref(),
            cache_anchor: cache_stable,
            cache_key: opts.cache_key,
        };

        let mut text = String::new();
        let mut reasoning_text = String::new();
        let mut acc = ToolCallAccumulator::default();
        let mut usage = None;
        let mut stop = StopReason::default();
        let stream_result = model.stream(&input, opts.stream, &mut |event| match event {
            crate::core::http::Event::Delta(t) => {
                text.push_str(&t);
                on_update(AgentUpdate::Delta(t));
            }
            crate::core::http::Event::ReasoningDelta(t) => {
                reasoning_text.push_str(&t);
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
            // a drop after output was handed out is never replayed — the
            // answer on screen would duplicate — but the run need not die
            // either: the partial text becomes a real assistant message and
            // the model continues from it (assistant-last is a prefill for
            // both wire shapes). Bounded: a link that drops every few KB
            // would otherwise recover forever, each attempt with a fresh
            // retry budget. The pending user message lands first so the
            // order stays what happened: prompt, partial answer.
            if !text.is_empty() {
                if has_pending {
                    history.push(pending.take().expect("checked above"));
                }
                history.push(Msg::Assistant {
                    text: text.clone(),
                    tool_calls: vec![],
                    reasoning: None,
                });
                recoveries += 1;
                if recoveries <= MAX_STREAM_RECOVERIES {
                    on_update(AgentUpdate::StreamRecovered {
                        chars: text.chars().count(),
                        error: e,
                    });
                    continue;
                }
            }
            return Err(AgentFailure {
                message: e,
                history,
                final_text: final_text.clone(),
                seed_boundary,
            });
        }

        if has_pending {
            history.push(pending.take().expect("checked above"));
        }
        let tool_calls = acc.finish();
        let reasoning = (!reasoning_text.is_empty()).then(|| reasoning_text.clone());
        history.push(Msg::Assistant {
            text: text.clone(),
            tool_calls: tool_calls.clone(),
            reasoning,
        });
        final_text = text;
        last_usage = usage;
        if let Some(u) = usage {
            spent_input += u.input;
        }
        // the assistant just pushed is the only message the report does not
        // name as input: mark everything up to and including it as covered
        // by u.input + u.output (the tail estimate prices it again at
        // chars/4, matching the compaction gate's conservative math)
        usage_marker = usage.map(|u| (history.len().saturating_sub(1), u));
        // the round is complete: this request's whole conversation is what
        // the next one will extend, so it is the next request's cache anchor
        cache_stable = Some(history.len());
        on_update(AgentUpdate::TurnEnd { usage });
        opts.hooks.fire(
            "turn_end",
            &json!({
                "turn": turn,
                "usage": usage.map(|u| json!([u.input, u.output, u.cached])),
            }),
        );

        // compaction check after each completed turn; the usage report
        // covered everything except the assistant we just pushed
        if let (Some(marker), Some(cfg)) = (usage_marker, opts.compact.as_ref()) {
            let mut estimate = compact::estimate_tokens(&history, Some(marker));
            if compact::should_compact(estimate, cfg) {
                // pressure confirmed: prune oversized tool results first —
                // it costs no model call and may relieve enough to skip
                // summarization entirely. The usage marker covers the
                // un-pruned prefix, so re-estimating over it would report the
                // identical number; subtract what the projection frees.
                let pruned = compact::prune_tool_results(&mut history, &compact::observation_dir());
                if pruned.count > 0 {
                    estimate = estimate.saturating_sub(pruned.freed_tokens);
                    on_update(AgentUpdate::ToolResultsPruned {
                        count: pruned.count,
                    });
                }
            }
            if compact::should_compact(estimate, cfg)
                && let Some(cut) = compact::find_cut(&history, cfg.effective_keep_recent())
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
                let task = task.map(|mut t| {
                    crate::core::text::truncate_ellipsis(&mut t, 4000);
                    t
                });
                let tail = history.split_off(cut);
                history.clear();
                history.push(Msg::Summary {
                    text: compact::compose_summary(task.as_deref(), &s),
                });
                history.extend(tail);
                seed_boundary = advance_seed_boundary(seed_boundary, cut);
                on_update(AgentUpdate::Compacted { removed: cut });
                // the rebuild moved every index: the marker's covered length
                // no longer names anything real, so drop it until the next
                // usage report re-establishes one
                usage_marker = None;
                // the rebuild rewrote everything below the summary too
                cache_stable = None;
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
                    error: Some(crate::providers::ToolError::Failed),
                    attachments: Vec::new(),
                });
            }
            continue;
        }
        if stop != StopReason::ToolUse || tool_calls.is_empty() {
            break;
        }
        // Read-only calls from one assistant message have no ordering
        // dependency on each other, so they run concurrently (other agents
        // do the same); mutating and exec calls stay strictly serial. The
        // batch is gated first (approvals prompt, extension hooks rewrite
        // args, ToolStart prints), executed in parallel, then finished in
        // the original order so history and tool results stay deterministic.
        // A batched turn therefore emits its `$` action lines before the
        // results; the renderer's per-tool log state is unaffected because
        // no built-in read tool streams, and the results themselves still
        // arrive in call order.
        let readonly_batch = tool_calls.len() > 1
            && !crate::core::http::interrupted()
            && tool_calls.iter().all(|c| {
                tools
                    .iter()
                    .find(|t| t.name() == c.name)
                    .is_some_and(|t| t.tier() == approval::Tier::Read)
            });
        if readonly_batch {
            let mut prepared: Vec<(ToolCall, Result<ClearedCall, CallRefusal>)> = Vec::new();
            for mut call in tool_calls {
                let cleared = prepare_call(
                    &mut call,
                    tools,
                    &opts.cwd,
                    approval,
                    on_approval,
                    on_update,
                    opts.hooks,
                );
                prepared.push((call, cleared));
            }
            let outs: Vec<(tools::ToolOutput, Vec<String>)> = std::thread::scope(|scope| {
                let handles: Vec<_> = prepared
                    .iter()
                    .map(|(call, cleared)| match cleared {
                        Ok(cleared) => {
                            let tool = cleared.tool;
                            let args = &call.arguments;
                            let cwd = opts.cwd.as_path();
                            Some(scope.spawn(move || {
                                // read-only tools do not stream: buffer any
                                // lines and replay them in order below
                                let mut logs: Vec<String> = Vec::new();
                                let out = tool
                                    .execute(args, cwd, &mut |l: &str| logs.push(l.to_string()));
                                (out, logs)
                            }))
                        }
                        Err(_) => None,
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| match h {
                        Some(h) => h.join().unwrap_or_else(|_| {
                            (tools::ToolOutput::err("tool panicked"), Vec::new())
                        }),
                        None => (tools::ToolOutput::err(String::new()), Vec::new()),
                    })
                    .collect()
            });
            for ((call, cleared), (out, logs)) in prepared.into_iter().zip(outs) {
                for line in &logs {
                    on_update(AgentUpdate::ToolLog(line.clone()));
                }
                let (out, repeat_note) = match cleared {
                    Err(denied) => (denied.into(), None),
                    Ok(_) => (out, repeats.observe(&call.name, &call.arguments)),
                };
                finish_call(
                    &call,
                    out,
                    tools,
                    &opts.cwd,
                    approval,
                    on_approval,
                    on_update,
                    opts.hooks,
                    repeat_note,
                    &mut history,
                );
            }
        } else {
            for mut call in tool_calls {
                if crate::core::http::interrupted() {
                    interrupted = true;
                    history.push(Msg::ToolResult {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        content: "interrupted by user".to_string(),
                        error: Some(crate::providers::ToolError::Interrupted),
                        attachments: Vec::new(),
                    });
                    continue;
                }
                let (out, repeat_note) = match prepare_call(
                    &mut call,
                    tools,
                    &opts.cwd,
                    approval,
                    on_approval,
                    on_update,
                    opts.hooks,
                ) {
                    Err(denied) => (denied.into(), None),
                    Ok(cleared) => {
                        let mut log =
                            |line: &str| on_update(AgentUpdate::ToolLog(line.to_string()));
                        let out = cleared.tool.execute(&call.arguments, &opts.cwd, &mut log);
                        // tool_result fires once, in finish_call, so denied
                        // and executed calls notify hooks identically
                        (out, repeats.observe(&call.name, &call.arguments))
                    }
                };
                finish_call(
                    &call,
                    out,
                    tools,
                    &opts.cwd,
                    approval,
                    on_approval,
                    on_update,
                    opts.hooks,
                    repeat_note,
                    &mut history,
                );
            }
        }
        if interrupted {
            break;
        }
    }

    opts.hooks.fire(
        "agent_end",
        &json!({"final_text": final_text, "interrupted": interrupted}),
    );
    Ok(AgentOutcome {
        history,
        final_text,
        usage: last_usage,
        interrupted,
        budget_exhausted,
        seed_boundary,
    })
}

/// Terminal preview of a tool result: the first ten non-empty lines, each
/// truncated, with a count of the lines that did not fit.
/// Budget awareness: a terse note reporting how much room the
/// task has left, in context-window tokens and (when a task budget is set)
/// input tokens. `None` when neither is known. Kept short and factual — it
/// exists so the model can choose to wrap up, not to make it narrate.
fn context_note(used_tokens: Option<u64>, opts: &AgentOptions, spent_input: u64) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let (Some(cfg), Some(used)) = (opts.compact.as_ref(), used_tokens) {
        // priced once by the caller: the same number gates the prefix
        // rewrites and (via the loop) the compaction check, so nothing
        // rescans the history here
        let left = cfg.context_window.saturating_sub(used);
        parts.push(format!("{left} tokens left in this context window"));
    }
    if opts.token_budget > 0 {
        let left = opts.token_budget.saturating_sub(spent_input);
        parts.push(format!("{left} of this task's input-token budget left"));
    }
    (!parts.is_empty()).then(|| format!("<context>{}</context>", parts.join("; ")))
}

fn summarize(content: &str) -> String {
    /// lines shown in the user-facing tool-result preview (matches the reference's
    /// collapsed default); the model still receives the full output
    const SHOWN: usize = 10;
    let mut lines: Vec<String> = Vec::new();
    let mut more = 0usize;
    for l in content.lines().filter(|l| !l.trim().is_empty()) {
        if lines.len() < SHOWN {
            let mut s = l.to_string();
            crate::core::text::truncate_ellipsis(&mut s, 200);
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

/// Let a `tool_result` subscriber replace the model-visible tool result
/// before it enters the transcript. The event carries the full content (the
/// opt-in), the replacement is re-capped like any tool output so a bad
/// extension cannot flood the context, and every failure mode — no reply,
/// timeout, a dead process — leaves the tool's own result untouched.
fn rewrite_tool_result(
    mut out: tools::ToolOutput,
    hooks: &crate::agent::ext::Extensions,
    call: &ToolCall,
) -> tools::ToolOutput {
    if !hooks.subscribes("tool_result") {
        return out;
    }
    let summary = summarize(&out.content);
    let params = json!({
        "tool": call.name,
        "args": call.arguments.clone(),
        "tool_call_id": call.id,
        "summary": summary,
        "is_error": out.is_error(),
        // the refusal class, for an extension that wants more than the bool
        "error": out.error.map(|e| json!(e)),
        "content": out.content.clone(),
    });
    if let Some(replacement) = hooks.rewrite_tool_result(&params) {
        out.content = tools::truncate_marked(&replacement, tools::MAX_LINES, tools::MAX_BYTES);
    }
    out
}

/// Action fusion: fold an `edit`/`write` call's optional `then_run` command
/// into its result. The model's edit-then-validate pattern is one intent, so
/// running the follow-up here saves a whole model round-trip; the command is
/// executed as an ordinary `bash` call (same lookup, approval and blacklist),
/// and a failure is reported in the text without turning the applied edit
/// into an error.
fn fuse_then_run(
    mut out: tools::ToolOutput,
    call: &ToolCall,
    tools: &[Box<dyn tools::Tool>],
    cwd: &std::path::Path,
    approval: &mut approval::ApprovalConfig,
    on_approval: &mut dyn FnMut(ApprovalRequest) -> ApprovalResponse,
) -> tools::ToolOutput {
    if out.is_error() || !matches!(call.name.as_str(), "edit" | "write") {
        return out;
    }
    let Some(command) = call
        .arguments
        .get("then_run")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
    else {
        return out;
    };
    let bash = ToolCall {
        id: format!("{}__then_run", call.id),
        name: "bash".to_string(),
        arguments: json!({"command": command}),
    };
    let body = match gate_call(&bash, tools, cwd, approval, on_approval, false) {
        Err(denied) => format!("not run: {}", denied.message),
        Ok(cleared) => {
            cleared
                .tool
                .execute(&bash.arguments, cwd, &mut |_: &str| {})
                .content
        }
    };
    out.content
        .push_str(&format!("\n\n[then_run] $ {command}\n{body}"));
    out
}

/// A cleared tool call: the tool plus the preview and diff already computed
/// for the approval prompt, so the ToolStart chrome does not re-run them
/// (an edit's read+diff is real work).
struct ClearedCall<'a> {
    tool: &'a dyn tools::Tool,
    preview: String,
    diff: Option<String>,
}

/// A call refused before it ran; `kind` is the `UnknownTool`/`Denied`
/// distinction the transcript keeps and `message` is what the model reads.
struct CallRefusal {
    kind: crate::providers::ToolError,
    message: String,
}

impl From<CallRefusal> for tools::ToolOutput {
    fn from(r: CallRefusal) -> tools::ToolOutput {
        tools::ToolOutput::err_kind(r.kind, r.message)
    }
}

/// Validate arguments and clear the approval gate. Ok(ClearedCall) means
/// cleared for execution; Err carries the refusal that is fed back to the
/// model as an error result.
fn gate_call<'a>(
    call: &ToolCall,
    tools: &'a [Box<dyn tools::Tool>],
    cwd: &std::path::Path,
    approval: &mut approval::ApprovalConfig,
    on_approval: &mut dyn FnMut(ApprovalRequest) -> ApprovalResponse,
    extension_allowed: bool,
) -> Result<ClearedCall<'a>, CallRefusal> {
    let denial = |message: String| CallRefusal {
        kind: crate::providers::ToolError::Denied,
        message,
    };
    let Some(tool) = tools.iter().find(|t| t.name() == call.name) else {
        return Err(CallRefusal {
            kind: crate::providers::ToolError::UnknownTool,
            message: format!("tool '{}' not found", call.name),
        });
    };
    if let Err(e) = tools::validate(&tool.parameters(), &call.arguments) {
        return Err(denial(format!("invalid arguments: {e}")));
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
            approval::Decision::Deny(r) => return Err(denial(format!("denied: {r}"))),
            approval::Decision::Ask(r) => (true, r),
            approval::Decision::Auto => (false, String::new()),
        };
    let preview = tool.preview(&call.arguments);
    let diff = tool.diff(&call.arguments, cwd).filter(|d| !d.is_empty());
    // a bash command that hit the ask-list carries its pattern down to the
    // prompt (highlighted there) and to the `a` answer, which spares the
    // pattern — not the whole bash tool — for the session
    let matched_pattern = bash_command.and_then(|cmd| approval::blacklist_hit(approval, cmd));
    // a blacklist ask prompts even when an extension said allow:
    // gating every run of the pattern is the file's whole point
    if ask && (!extension_allowed || matched_pattern.is_some()) {
        let answer = on_approval(ApprovalRequest {
            tool: tool.name(),
            tier: tool.tier(),
            preview: &preview,
            diff: diff.as_deref(),
            reason: &reason,
            pattern: matched_pattern.as_deref(),
        });
        match answer {
            ApprovalResponse::Allow => {}
            ApprovalResponse::AllowSession => {
                if let Some(pattern) = matched_pattern {
                    approval.blacklist_session_allows.push(pattern);
                } else {
                    approval
                        .tool_policies
                        .insert(tool.name().to_string(), approval::Policy::Allow);
                }
            }
            ApprovalResponse::Deny => {
                return Err(denial(format!("denied by user: {preview}")));
            }
        }
    }
    Ok(ClearedCall {
        tool: tool.as_ref(),
        preview,
        diff,
    })
}

/// Gate one call before execution: fire the extension `tool_call` hook (it
/// may deny or rewrite the arguments), run the approval matrix, and emit the
/// ToolStart chrome. `Err` carries the denial/validation text that becomes
/// an error tool result. Shared by the serial path and the read-only parallel
/// batch so both gate identically.
#[allow(clippy::too_many_arguments)]
fn prepare_call<'a>(
    call: &mut ToolCall,
    tools: &'a [Box<dyn tools::Tool>],
    cwd: &std::path::Path,
    approval: &mut approval::ApprovalConfig,
    on_approval: &mut dyn FnMut(ApprovalRequest) -> ApprovalResponse,
    on_update: &mut dyn FnMut(AgentUpdate),
    hooks: &crate::agent::ext::Extensions,
) -> Result<ClearedCall<'a>, CallRefusal> {
    // extension gate: a subscribed tool_call may deny, rewrite the
    // arguments, or allow (skip the built-in approval) before the matrix
    // even sees them; a deny is the Err that becomes an error tool result
    let denied = |reason: String| CallRefusal {
        kind: crate::providers::ToolError::Denied,
        message: reason,
    };
    let (extension_allowed, rewritten) = hooks
        .gate_tool_call(&json!({"tool": call.name, "args": call.arguments}))
        .map_err(denied)?;
    if let Some(rewritten) = rewritten {
        call.arguments = rewritten;
    }
    let cleared = gate_call(call, tools, cwd, approval, on_approval, extension_allowed)?;
    on_update(AgentUpdate::ToolStart {
        name: call.name.clone(),
        preview: cleared.preview.clone(),
        diff: cleared.diff.clone(),
    });
    Ok(cleared)
}

/// Finish one call after execution: fuse an edit/write's `then_run`, let
/// extensions rewrite the model-visible result, emit ToolEnd, and push the
/// result onto the history. Shared by both paths so ordering is identical.
#[allow(clippy::too_many_arguments)]
fn finish_call(
    call: &ToolCall,
    out: tools::ToolOutput,
    tools: &[Box<dyn tools::Tool>],
    cwd: &std::path::Path,
    approval: &mut approval::ApprovalConfig,
    on_approval: &mut dyn FnMut(ApprovalRequest) -> ApprovalResponse,
    on_update: &mut dyn FnMut(AgentUpdate),
    hooks: &crate::agent::ext::Extensions,
    repeat_note: Option<String>,
    history: &mut Vec<Msg>,
) {
    // action fusion: an edit/write may fuse its follow-up validation command
    // into the same result, which removes the extra model round-trip (the
    // command still passes the normal bash gate, so approval and the
    // blacklist apply to it)
    let out = fuse_then_run(out, call, tools, cwd, approval, on_approval);
    // extensions may replace the model-visible result (the enabler for
    // reducer/observation plugins — see `docs/extensions.md`); the rewrite is
    // re-capped and fail-open, so a broken plugin costs its own rewrite and
    // never the tool's result
    let out = rewrite_tool_result(out, hooks, call);
    on_update(AgentUpdate::ToolEnd {
        summary: summarize(&out.content),
        is_error: out.is_error(),
    });
    // the repeat reminder rides the result the model is about to read; the
    // terminal summary above stays the tool's own output
    let mut content = out.content;
    if let Some(note) = repeat_note {
        content.push_str(&note);
    }
    history.push(Msg::ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        content,
        error: out.error,
        attachments: out.attachments,
    });
}

#[cfg(test)]
mod tests {
    use super::advance_seed_boundary;
    use serde_json::json;

    /// Compaction drops `cut` messages and inserts one summary, so the run's
    /// own region moves; a boundary consumed by the cut lands on the summary.
    #[test]
    fn compaction_shifts_the_seed_boundary() {
        assert_eq!(advance_seed_boundary(10, 4), 7); // 6 seed messages survive
        assert_eq!(advance_seed_boundary(4, 4), 1); // the whole seed was cut
        assert_eq!(advance_seed_boundary(2, 9), 1);
        assert_eq!(advance_seed_boundary(0, 3), 1);
    }

    /// A throwaway cwd for the fusion tests (they never touch the repo).
    fn fuse_dir() -> std::path::PathBuf {
        let dir = crate::core::testutil::scratch_dir("fuse");
        std::fs::write(dir.join("x.txt"), "hello\n").unwrap();
        dir
    }

    fn edit_call(then_run: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "edit".into(),
            arguments: json!({
                "path": "x.txt",
                "edits": [{"oldText": "hello", "newText": "world"}],
                "then_run": then_run
            }),
        }
    }

    #[test]
    fn then_run_fuses_the_command_into_the_mutation_result() {
        let dir = fuse_dir();
        let tools = tools::builtin_tools();
        let mut approval = approval::ApprovalConfig::default();
        let out = fuse_then_run(
            tools::ToolOutput::ok("applied 1 edit"),
            &edit_call("cat x.txt"),
            &tools,
            &dir,
            &mut approval,
            &mut |_| panic!("yolo mode must not prompt for the fused command"),
        );
        assert!(!out.is_error(), "the applied edit keeps its status");
        assert!(out.content.contains("applied 1 edit"));
        assert!(
            out.content.contains("[then_run] $ cat x.txt"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("hello"),
            "the command output rides the same result: {}",
            out.content
        );
    }

    #[test]
    fn then_run_never_fires_on_a_failed_mutation_or_a_plain_call() {
        let dir = fuse_dir();
        let tools = tools::builtin_tools();
        let mut approval = approval::ApprovalConfig::default();
        // the edit failed (bad match): running its validation would be noise
        let out = fuse_then_run(
            tools::ToolOutput::err("oldText did not match"),
            &edit_call("touch ran.txt"),
            &tools,
            &dir,
            &mut approval,
            &mut |_| panic!("a failed mutation must not run its follow-up"),
        );
        assert!(out.is_error());
        assert!(!out.content.contains("[then_run]"), "{}", out.content);
        assert!(!dir.join("ran.txt").exists(), "nothing ran");
        // a bash call carrying then_run is not recursively fused
        let bash = ToolCall {
            id: "2".into(),
            name: "bash".into(),
            arguments: json!({"command": "echo hi", "then_run": "touch ran.txt"}),
        };
        let out = fuse_then_run(
            tools::ToolOutput::ok("hi\n"),
            &bash,
            &tools,
            &dir,
            &mut approval,
            &mut |_| panic!("only edit/write fuse"),
        );
        assert_eq!(out.content, "hi\n");
    }

    #[test]
    fn then_run_still_passes_the_approval_gate() {
        // ask mode: the fused command is gated like any other exec call, and
        // a denial is reported without failing the applied edit
        let dir = fuse_dir();
        let tools = tools::builtin_tools();
        let mut approval = approval::ApprovalConfig::default();
        approval.mode = approval::Mode::AlwaysAsk;
        let out = fuse_then_run(
            tools::ToolOutput::ok("applied 1 edit"),
            &edit_call("touch ran.txt"),
            &tools,
            &dir,
            &mut approval,
            &mut |_| ApprovalResponse::Deny,
        );
        assert!(!out.is_error(), "the edit still landed");
        assert!(out.content.contains("not run: denied"), "{}", out.content);
        assert!(
            !dir.join("ran.txt").exists(),
            "a denied follow-up did not run"
        );
    }

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

    #[test]
    fn identical_repeats_remind_at_escalating_counts() {
        let mut g = RepeatGuard {
            last: None,
            count: 0,
        };
        let args = json!({"command": "ls -la", "path": "."});
        assert!(
            g.observe("bash", &args).is_none(),
            "the first call is not a repeat"
        );
        assert!(
            g.observe("bash", &args).is_none(),
            "the second is still quiet"
        );
        let note = g.observe("bash", &args).expect("the third repeat reminds");
        assert!(note.contains("3rd identical bash call"), "{note}");
        assert!(
            g.observe("bash", &args).is_none(),
            "quiet between thresholds"
        );
        let note = g.observe("bash", &args).expect("the fifth repeat reminds");
        assert!(note.contains("5th identical bash call"), "{note}");
        assert!(
            note.contains("ls -la"),
            "the detailed note names the repeated arguments: {note}"
        );
    }

    #[test]
    fn repeat_tracking_ignores_key_order_and_resets_on_change() {
        let mut g = RepeatGuard {
            last: None,
            count: 0,
        };
        assert!(g.observe("bash", &json!({"a": 1, "b": 2})).is_none());
        assert!(
            g.observe("bash", &json!({"b": 2, "a": 1})).is_none(),
            "same arguments in a different key order are the same call"
        );
        let note = g
            .observe("bash", &json!({"a": 1, "b": 2}))
            .expect("third identical call");
        assert!(note.contains("3rd"));
        // a different call restarts the streak from one
        assert!(g.observe("bash", &json!({"a": 9})).is_none());
        // a new user message clears the tracker entirely
        g.reset();
        assert!(g.observe("bash", &json!({"a": 9})).is_none());
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

    /// Read one mock-server request: past the request head plus its
    /// content-length body. Shared by the inline SSE servers below.
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

    /// A mock SSE server that drops the first connection after one delta
    /// (no [DONE]) and completes the second: the agent loop must keep the
    /// partial answer as a real assistant message and finish the task on
    /// the continued request — the order stays prompt, partial, continuation.
    #[test]
    fn a_dropped_stream_continues_from_its_partial_answer() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits2 = hits.clone();
        let server = std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let n = hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut c = conn;
                // read past the request head + body (content-length)
                read_request(&mut c);
                if n == 0 {
                    // one delta, then the connection dies mid-answer
                    let _ = c.write_all(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n".as_bytes(),
                    );
                    let _ = c.write_all(
                        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial ans\"}}]}\n\n",
                    );
                    drop(c); // no [DONE]: the stream is cut
                } else {
                    let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ued cleanly\"}}]}\n\n\
                                data: [DONE]\n\n";
                    let _ = c.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    );
                    break;
                }
            }
        });
        use std::io::Write as _;

        let model = crate::providers::ResolvedModel {
            provider_name: "mock".into(),
            kind: "openai-compat".into(),
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: Some("sk-x".into()),
            model_id: "m".into(),
            options: vec![],
        };
        let tools: Vec<Box<dyn tools::Tool>> = vec![];
        let opts = AgentOptions {
            system: None,
            cwd: std::env::temp_dir(),
            max_turns: 4,
            token_budget: 0,
            stream: true,
            compact: None,
            reasoning: None,
            hooks: &crate::agent::ext::Extensions::empty(),
            cache_key: None,
        };
        let mut approval = approval::ApprovalConfig::default();
        let outcome = run_agent(
            &model,
            &tools,
            "go",
            vec![],
            vec![],
            &opts,
            &mut approval,
            &mut |_| {},
            &mut |_| ApprovalResponse::Deny,
            &mut || vec![],
        )
        .expect("the run must recover from the dropped stream");
        server.join().unwrap();
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "exactly one recovery request"
        );
        assert!(!outcome.interrupted);
        assert!(!outcome.budget_exhausted);
        assert_eq!(outcome.final_text, "ued cleanly");
        assert_eq!(outcome.history.len(), 3, "prompt, partial, continuation");
        assert!(
            matches!(&outcome.history[1], Msg::Assistant { text, tool_calls, .. } if text == "partial ans" && tool_calls.is_empty()),
            "the partial answer rides the history as a real assistant message"
        );
    }

    /// The token budget stops a task like a turn cap would, but on the
    /// metric that actually prices a runaway loop: cumulative input tokens.
    /// Past 80% a wrap-up note rides the pending prompt; at 100% the loop
    /// breaks and the outcome says so. A plain answer (no tool calls) ends
    /// the run normally, so the server needs one tool call per round to
    /// keep the loop alive until the budget bites.
    #[test]
    fn token_budget_warns_then_stops_the_run() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits2 = hits.clone();
        let server = std::thread::spawn(move || {
            let mut n = 0usize;
            for conn in listener.incoming().flatten() {
                if n >= 10 {
                    break;
                }
                let mut c = conn;
                read_request(&mut c);
                let _ = hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                n += 1;
                // each round reports 1000 input tokens and asks for a tool;
                // budget 2500: warn after round 2 (2000 ≥ 80%), stop before 4
                let body = format!(
                    "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c{n}\",\"function\":{{\"name\":\"echo\",\"arguments\":\"{{}}\"}}}}]}}}}]}}\n\n\
                     data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
                     data: {{\"usage\":{{\"prompt_tokens\":1000,\"completion_tokens\":5}}}}\n\n\
                     data: [DONE]\n\n"
                );
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        use std::io::Write as _;

        let model = crate::providers::ResolvedModel {
            provider_name: "mock".into(),
            kind: "openai-compat".into(),
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: Some("sk-x".into()),
            model_id: "m".into(),
            options: vec![],
        };
        let tools: Vec<Box<dyn tools::Tool>> = vec![Box::new(EchoTool)];
        let opts = AgentOptions {
            system: None,
            cwd: std::env::temp_dir(),
            max_turns: 0,
            token_budget: 2500,
            stream: true,
            compact: None,
            reasoning: None,
            hooks: &crate::agent::ext::Extensions::empty(),
            cache_key: None,
        };
        let mut approval = approval::ApprovalConfig::default();
        let outcome = run_agent(
            &model,
            &tools,
            "go",
            vec![],
            vec![],
            &opts,
            &mut approval,
            &mut |_| {},
            &mut |_| ApprovalResponse::Deny,
            &mut || vec![],
        )
        .expect("a budget stop is a normal outcome, not a failure");
        // let a hypothetical erroneous 4th request land before counting
        std::thread::sleep(std::time::Duration::from_millis(150));
        drop(server);
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "round 4 never starts: 3000 spent ≥ 2500 budget"
        );
        assert!(outcome.budget_exhausted);
        // the wrap-up note rode the round-3 prompt (2400 ≥ 2400 soft line)
        let warned = outcome
            .history
            .iter()
            .any(|m| matches!(m, Msg::User { text, .. } if text.contains("token budget")));
        assert!(warned, "the soft-line note must be in the history");
    }

    struct EchoTool;
    impl tools::Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn tier(&self) -> super::approval::Tier {
            super::approval::Tier::Read
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }
        fn preview(&self, _args: &serde_json::Value) -> String {
            "echo".into()
        }
        fn execute(
            &self,
            _args: &serde_json::Value,
            _cwd: &std::path::Path,
            _log: &mut dyn FnMut(&str),
        ) -> tools::ToolOutput {
            tools::ToolOutput::ok("echoed")
        }
    }

    /// Loop hygiene end to end: three identical echo calls in a row, and
    /// the third result carries the advisory reminder; a plain answer then
    /// ends the run normally.
    #[test]
    fn a_third_identical_tool_call_reminds_the_model_to_change_approach() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits2 = hits.clone();
        let server = std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let n = hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut c = conn;
                read_request(&mut c);
                let body = if n < 3 {
                    format!(
                        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c{n}\",\"function\":{{\"name\":\"echo\",\"arguments\":\"{{}}\"}}}}]}}}}]}}\n\n\
                         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
                         data: [DONE]\n\n"
                    )
                } else {
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"}}]}\n\n\
                     data: [DONE]\n\n"
                        .to_string()
                };
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                if n >= 3 {
                    break;
                }
            }
        });
        use std::io::Write as _;

        let model = crate::providers::ResolvedModel {
            provider_name: "mock".into(),
            kind: "openai-compat".into(),
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: Some("sk-x".into()),
            model_id: "m".into(),
            options: vec![],
        };
        let tools: Vec<Box<dyn tools::Tool>> = vec![Box::new(EchoTool)];
        let opts = AgentOptions {
            system: None,
            cwd: std::env::temp_dir(),
            max_turns: 0,
            token_budget: 0,
            stream: true,
            compact: None,
            reasoning: None,
            hooks: &crate::agent::ext::Extensions::empty(),
            cache_key: None,
        };
        let mut approval = approval::ApprovalConfig::default();
        let outcome = run_agent(
            &model,
            &tools,
            "go",
            vec![],
            vec![],
            &opts,
            &mut approval,
            &mut |_| {},
            &mut |_| ApprovalResponse::Deny,
            &mut || vec![],
        )
        .expect("identical echoes are not a failure");
        server.join().unwrap();
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "three tool rounds plus the plain answer"
        );
        assert_eq!(outcome.final_text, "done");
        let results: Vec<&str> = outcome
            .history
            .iter()
            .filter_map(|m| match m {
                Msg::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 3);
        assert!(!results[0].contains("[System]"));
        assert!(!results[1].contains("[System]"));
        assert!(
            results[2].contains("3rd identical echo call"),
            "the reminder rides the third result: {}",
            results[2]
        );
    }

    /// A read-tier probe that records the thread it ran on, so a batched
    /// read-only turn can be shown to execute off the calling thread.
    struct ProbeTool(std::sync::Arc<std::sync::Mutex<Vec<std::thread::ThreadId>>>);
    impl tools::Tool for ProbeTool {
        fn name(&self) -> &str {
            "probe"
        }
        fn tier(&self) -> super::approval::Tier {
            super::approval::Tier::Read
        }
        fn description(&self) -> &str {
            "probe"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }
        fn preview(&self, _args: &serde_json::Value) -> String {
            "probe".into()
        }
        fn execute(
            &self,
            _args: &serde_json::Value,
            _cwd: &std::path::Path,
            _log: &mut dyn FnMut(&str),
        ) -> tools::ToolOutput {
            self.0.lock().unwrap().push(std::thread::current().id());
            tools::ToolOutput::ok("probed")
        }
    }

    /// Two read-only calls in one assistant message run on scope threads
    /// (not the caller's) and their results still land in call order.
    #[test]
    fn batched_readonly_calls_run_off_the_calling_thread_in_order() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for (n, conn) in listener.incoming().flatten().enumerate() {
                let mut c = conn;
                read_request(&mut c);
                let body = if n == 0 {
                    let first = serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[
                        {"index":0,"id":"c1","function":{"name":"probe","arguments":"{}"}},
                        {"index":1,"id":"c2","function":{"name":"probe","arguments":"{}"}}
                    ]}}]})
                    .to_string();
                    format!(
                        "data: {first}\n\n\
                         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
                         data: [DONE]\n\n"
                    )
                } else {
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"}}]}\n\n\
                     data: [DONE]\n\n"
                        .to_string()
                };
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                if n >= 1 {
                    break;
                }
            }
        });
        use std::io::Write as _;

        let model = crate::providers::ResolvedModel {
            provider_name: "mock".into(),
            kind: "openai-compat".into(),
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: Some("sk-x".into()),
            model_id: "m".into(),
            options: vec![],
        };
        let threads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let tools: Vec<Box<dyn tools::Tool>> = vec![Box::new(ProbeTool(threads.clone()))];
        let opts = AgentOptions {
            system: None,
            cwd: std::env::temp_dir(),
            max_turns: 0,
            token_budget: 0,
            stream: true,
            compact: None,
            reasoning: None,
            hooks: &crate::agent::ext::Extensions::empty(),
            cache_key: None,
        };
        let caller = std::thread::current().id();
        let mut approval = approval::ApprovalConfig::default();
        let outcome = run_agent(
            &model,
            &tools,
            "go",
            vec![],
            vec![],
            &opts,
            &mut approval,
            &mut |_| {},
            &mut |_| ApprovalResponse::Deny,
            &mut || vec![],
        )
        .expect("a batched read-only turn is not a failure");
        server.join().unwrap();
        assert_eq!(outcome.final_text, "done");
        // both calls ran, each on its own scope thread — never the caller's
        let ids = threads.lock().unwrap().clone();
        assert_eq!(ids.len(), 2, "both calls executed");
        assert!(
            ids.iter().all(|id| *id != caller),
            "read-only calls must not run on the calling thread"
        );
        assert_ne!(ids[0], ids[1], "the two calls ran concurrently");
        // results stay in call order
        let results: Vec<(&str, &str)> = outcome
            .history
            .iter()
            .filter_map(|m| match m {
                Msg::ToolResult {
                    call_id, content, ..
                } => Some((call_id.as_str(), content.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(results, [("c1", "probed"), ("c2", "probed")]);
    }

    /// Budget awareness: the note reports the context-window room
    /// and (when set) the task's input-token room, wrapped so the model reads
    /// it as a system note rather than a user turn.
    #[test]
    fn context_note_reports_the_room_left() {
        let mut opts = AgentOptions {
            system: None,
            cwd: std::env::temp_dir(),
            max_turns: 0,
            token_budget: 0,
            stream: false,
            compact: Some(compact::CompactConfig {
                context_window: 100_000,
                reserve_tokens: 0,
                keep_recent_tokens: 0,
            }),
            reasoning: None,
            hooks: &crate::agent::ext::Extensions::empty(),
            cache_key: None,
        };
        let note = context_note(Some(0), &opts, 0).unwrap();
        assert!(
            note.starts_with("<context>") && note.ends_with("</context>"),
            "{note}"
        );
        assert!(
            note.contains("tokens left in this context window"),
            "{note}"
        );
        // a task budget adds its own clause
        opts.token_budget = 10_000;
        let note = context_note(Some(0), &opts, 3_000).unwrap();
        assert!(
            note.contains("7000 of this task's input-token budget left"),
            "{note}"
        );
        // neither a window nor a budget: no note at all
        opts.compact = None;
        opts.token_budget = 0;
        assert!(context_note(Some(0), &opts, 0).is_none());
    }

    /// The marker prices the covered prefix at the provider's reported count
    /// and only the tail at the chars/4 estimate: with a marker naming the
    /// whole history, the note must carry exactly the reported total, and
    /// the None path keeps its whole-history estimate.
    #[test]
    fn context_note_uses_the_usage_marker_for_the_covered_prefix() {
        let opts = AgentOptions {
            system: None,
            cwd: std::env::temp_dir(),
            max_turns: 0,
            token_budget: 0,
            stream: false,
            compact: Some(compact::CompactConfig {
                context_window: 100_000,
                reserve_tokens: 0,
                keep_recent_tokens: 0,
            }),
            reasoning: None,
            hooks: &crate::agent::ext::Extensions::empty(),
            cache_key: None,
        };
        let history = vec![Msg::user("hi"), Msg::user("there")];
        let marker = Some((
            history.len(),
            Usage {
                input: 7_000,
                output: 0,
                cached: 0,
            },
        ));
        // the loop prices the context once per round and hands the number
        // down, so the marker's reported total must survive that step
        let used = compact::estimate_tokens(&history, marker);
        let note = context_note(Some(used), &opts, 0).unwrap();
        assert!(
            note.contains("93000 tokens left"),
            "the marker total must flow through: {note}"
        );
    }
}
