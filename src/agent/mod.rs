//! Agent primitives: the built-in tools, approval, and the agent loop
//! driving `llm agent`. The conversation model it speaks (`Msg`, `ToolDef`,
//! `ToolCall`) lives in `providers` — see `providers/mod.rs`.

pub mod approval;
pub mod blacklist;
pub mod compact;
pub mod ext;
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
    /// one agent round is complete: everything it added to the history is in
    /// place (the pending prompt, the assistant message, the tool results).
    /// The session persists the slice here — at each round boundary, not at
    /// task end — so a crash loses only the round in flight. Internal:
    /// the terminal UI renders nothing for it and the `--json` stream does
    /// not emit it.
    RoundEnd {
        messages: Vec<Msg>,
        usage: Option<Usage>,
    },
    /// compaction was due and could not be applied: the session keeps growing
    /// over the window, and that must not happen silently
    CompactStalled {
        reason: String,
    },
    /// a dropped stream was recovered: the partial answer is kept as a real
    /// assistant message and the model continues from it (bounded per run)
    StreamRecovered {
        chars: usize,
        error: String,
    },
    /// oversized tool results were replaced by head+marker+tail views to
    /// relieve context pressure (the full text stays in the session log)
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
    /// ceiling on one serialized request body, in bytes
    /// (`agent.max_request_bytes`); a gateway in front of the model may
    /// refuse far less than the provider itself documents
    pub max_request_bytes: usize,
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
    /// how much of `seed` the previous request in this conversation already
    /// carried, as a prefix length (see `PromptInput::cache_anchor`). None
    /// opens a conversation. A run seeded from a resumed thread or from the
    /// previous task of a REPL session otherwise carries only the moving-tip
    /// breakpoint on its first request, and a provider that reads its cache
    /// back by prefix finds nothing above it — the whole history is written
    /// at cache-write price at the start of every task instead of read back.
    pub cache_anchor: Option<usize>,
    /// how long the provider should hold its cache entry, for the wires that
    /// take one (`agent.cache_ttl`); None leaves the provider's own default
    pub cache_ttl: Option<crate::providers::CacheTtl>,
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
/// `final_text` is the last completed answer (or the partial one a dropped
/// stream left behind), kept for the persisted turn.
#[derive(Debug)]
pub struct AgentFailure {
    pub message: String,
    pub history: Vec<Msg>,
    pub final_text: String,
    /// where the failed round's own messages start: every earlier round was
    /// already persisted at its [`AgentUpdate::RoundEnd`], so this is the
    /// only slice a failure still has to write
    pub round_start: usize,
}

/// One run's inputs: the model, the tool set, the prompt and the seed
/// history. Grouped so a call site reads as what the run *is*, and the loop
/// keeps a short signature.
pub struct RunRequest<'a> {
    pub model: &'a crate::providers::ResolvedModel,
    pub tools: &'a [Box<dyn tools::Tool>],
    pub prompt: &'a str,
    pub attachments: Vec<crate::providers::Attachment>,
    pub seed: Vec<Msg>,
    pub opts: &'a AgentOptions<'a>,
}

/// The two callbacks the loop reports through: chrome updates and approval
/// questions.
pub struct RunCallbacks<'a> {
    pub on_update: &'a mut dyn FnMut(AgentUpdate),
    pub on_approval: &'a mut dyn FnMut(ApprovalRequest) -> ApprovalResponse,
}

/// The cache anchor a run starts its first round from: the caller's, when it
/// names a prefix this history actually has. A caller counts the seed before
/// the loop projects it down (a resume prunes oversized results), so the
/// number it holds can be out of range — dropped rather than trusted, since a
/// marker past the tip would only be a breakpoint nothing can match.
fn usable_anchor(anchor: Option<usize>, history_len: usize) -> Option<usize> {
    anchor.filter(|n| *n > 0 && *n <= history_len)
}

/// Run the agent loop: stream an assistant response, execute its tool calls,
/// feed results back, repeat until the model stops calling tools or the turn
/// budget hits. Tool errors become error results (data, not failure); only
/// provider errors abort with Err, carrying the partial history so the
/// session survives without a defensive clone.
pub fn run_agent(
    req: RunRequest<'_>,
    approval: &mut approval::ApprovalConfig,
    cb: RunCallbacks<'_>,
) -> Result<AgentOutcome, AgentFailure> {
    let RunRequest {
        model,
        tools,
        prompt,
        attachments,
        seed,
        opts,
    } = req;
    let RunCallbacks {
        on_update,
        on_approval,
    } = cb;
    let tool_defs: Vec<ToolDef> = tools
        .iter()
        .map(|t| ToolDef {
            name: t.name().to_string(),
            description: t.description().to_string(),
            parameters: t.parameters(),
        })
        .collect();

    let mut history: Vec<Msg> = seed;
    // where the round in flight starts: everything from here on is the round
    // the next `RoundEnd` reports. Assigned fresh after the loop-top
    // compaction check (a rebuild shifts every index); the recovery and
    // overflow paths keep their pushes inside the round they belong to.
    let mut round_start;
    let mut pending: Option<Msg> = Some(Msg::user_with(prompt, attachments));
    let mut last_usage = None;
    // the usage marker carried across turns: the provider's reported count
    // covers `history[..covered]`, so per-round estimates (the compaction
    // gate) only price the tail instead of rescanning the whole history each
    // round (O(n²) over a long task). Reset to None wherever history is
    // rebuilt (a compaction) so indices stay honest.
    let mut usage_marker: Option<(usize, Usage)> = None;
    // one compaction-stalled notice per run (see `maybe_compact`)
    let mut compact_stalled = false;
    // how many times a refused request was answered with a fresh compaction:
    // bounded, so a provider that refuses everything ends the run
    const MAX_OVERFLOW_COMPACTIONS: usize = 2;
    let mut overflow_compactions = 0usize;
    // The auto-compaction trigger: `window - reserve` when the model's window
    // is known (the reference's rule), else the configured fallback. It never changes
    // across the run. 0 switches it off; a provider that refuses a prompt
    // still forces one.
    let mut compact_trigger = opts
        .compact
        .as_ref()
        .map(|c| compact::effective_trigger(c.trigger_tokens, model.context_window))
        .unwrap_or(0);
    // a caller continuing a conversation (a resumed thread, the next task of a
    // REPL session) names the seed here, so the first request of the task
    // carries a second breakpoint on a prefix the provider still holds;
    // without it the moving tip alone re-writes the whole history at
    // cache-write price. Set after each completed round (the request's whole
    // conversation is what the next one extends) and cleared wherever history
    // is edited in place, so a provider reading its own cache back never sees
    // a prefix that was rewritten under it.
    let mut cache_stable: Option<usize> = usable_anchor(opts.cache_anchor, history.len());
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
        // Context pressure, priced once per round to serve the compaction
        // check below; under real pressure the stale prefix is
        // rewritten here too (see `price_and_rewrite`).
        // Every image in the request is billed every round, so only the newest
        // image turns keep their pixels — including anything a tool result
        // added mid-run.
        crate::agent::session::budget_images(&mut history);
        // the compaction this round runs under: the window-anchored trigger
        // (or the configured fallback), unchanged across the run
        let cfg_now = opts.compact.as_ref().map(|c| compact::CompactConfig {
            trigger_tokens: compact_trigger,
            ..c.clone()
        });
        price_and_rewrite(&mut history, usage_marker, cfg_now.as_ref(), on_update);
        // the reference's pre-request compaction check (the reference session): a session
        // that was interrupted — or resumed — over its window is compacted
        // before this request goes out, so ctrl-c cannot skip the gate the way
        // a check that only runs after a completed turn can.
        {
            let mut sink = StallSink::new(&mut compact_stalled, on_update);
            let after = maybe_compact(
                model,
                &mut history,
                usage_marker,
                cfg_now.as_ref(),
                cache_stable,
                &mut sink,
            );
            usage_marker = after.usage_marker;
            cache_stable = after.cache_stable;
            compact_trigger = after.trigger;
        }
        // the round boundary: a compaction above may have rebuilt the
        // history, so the slice the next `RoundEnd` reports starts here
        round_start = history.len();
        // the system prompt stays byte-identical every round: it is the head
        // of the request, and providers cache by input prefix (DeepSeek
        // context caching, Anthropic prompt caching), so any per-turn suffix
        // here re-bills the whole history at cache-miss price
        let input = PromptInput {
            max_request_bytes: opts.max_request_bytes,
            system: opts.system,
            history: &history,
            prompt: pending_prompt,
            attachments: pending_attachments,
            tools: &tool_defs,
            reasoning: opts.reasoning.as_deref(),
            cache_anchor: cache_stable,
            cache_key: opts.cache_key,
            cache_ttl: opts.cache_ttl,
        };

        let mut round = stream_round(model, &input, opts.stream, on_update);
        if let Some(e) = round.error.take() {
            if crate::core::http::interrupted() {
                interrupted = true;
                // keep the interrupted round's own messages: the pending
                // prompt and any partial answer already streamed, so /resume
                // starts from what was actually said instead of a gap the
                // transcript never recorded (the network-drop recovery below
                // keeps partial output the same way).
                if has_pending {
                    history.push(pending.take().expect("checked above"));
                }
                if !round.text.is_empty() {
                    final_text = round.text.clone();
                    history.push(Msg::Assistant {
                        text: round.text,
                        tool_calls: vec![],
                        reasoning: None,
                        reasoning_meta: None,
                    });
                }
                on_update(AgentUpdate::RoundEnd {
                    messages: history[round_start..].to_vec(),
                    usage: None,
                });
                break;
            }
            // The provider refusing a prompt that does not fit its window is
            // the one authoritative statement about it there is: compact the
            // history now, whatever the trigger says, and retry the round.
            // Bounded: a provider that refuses everything ends the run
            // instead of looping.
            if round.text.is_empty()
                && overflow_compactions < MAX_OVERFLOW_COMPACTIONS
                && crate::core::http::context_overflow(&e)
                && let Some(cfg) = cfg_now.as_ref()
            {
                let mut sink = StallSink::new(&mut compact_stalled, on_update);
                if force_compact(
                    model,
                    &mut history,
                    cfg,
                    &mut usage_marker,
                    &mut cache_stable,
                    &mut sink,
                ) {
                    overflow_compactions += 1;
                    continue;
                }
            }
            // a drop after output was handed out is never replayed — the
            // answer on screen would duplicate — but the run need not die
            // either: the partial text becomes a real assistant message and
            // the model continues from it (assistant-last is a prefill for
            // both wire shapes). Bounded: a link that drops every few KB
            // would otherwise recover forever, each attempt with a fresh
            // retry budget. The pending user message lands first so the
            // order stays what happened: prompt, partial answer.
            if !round.text.is_empty() {
                if has_pending {
                    history.push(pending.take().expect("checked above"));
                }
                history.push(Msg::Assistant {
                    text: round.text.clone(),
                    tool_calls: vec![],
                    reasoning: None,
                    reasoning_meta: None,
                });
                recoveries += 1;
                if recoveries <= MAX_STREAM_RECOVERIES {
                    on_update(AgentUpdate::StreamRecovered {
                        chars: round.text.chars().count(),
                        error: e,
                    });
                    // the partial answer is a real round record: the retry
                    // continues from it, but the transcript keeps what the
                    // model actually said first
                    on_update(AgentUpdate::RoundEnd {
                        messages: history[round_start..].to_vec(),
                        usage: None,
                    });
                    continue;
                }
            } else if round.text.is_empty()
                && round.reasoning.is_none()
                && round.tool_calls.is_empty()
                && recoveries < MAX_STREAM_RECOVERIES
            {
                // nothing was handed out — not one delta of text, reasoning
                // or a tool call — so replaying the request cannot duplicate
                // anything on screen. This is the shape of a gateway cutting
                // a long-lived SSE connection before the first byte of its
                // answer.
                recoveries += 1;
                on_update(AgentUpdate::StreamRecovered { chars: 0, error: e });
                continue;
            }
            return Err(AgentFailure {
                message: e,
                history,
                final_text: final_text.clone(),
                round_start,
            });
        }

        if has_pending {
            history.push(pending.take().expect("checked above"));
        }
        let text = round.text;
        let tool_calls = round.tool_calls;
        let usage = round.usage;
        let stop = round.stop;
        history.push(Msg::Assistant {
            text: text.clone(),
            tool_calls: tool_calls.clone(),
            reasoning: round.reasoning,
            reasoning_meta: round.reasoning_meta,
        });
        final_text = text;
        last_usage = usage;
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

        // A gateway can overflow silently — no 400, just a usage report past
        // the window, or a length stop that consumed the whole window and
        // produced nothing (the reference's the overflow module cases 2 and 3). The window must
        // be known to see it; then it is the same forced-compact-and-retry as
        // an outright refusal, bounded the same way.
        if let (Some(u), Some(w)) = (usage, model.context_window)
            && overflow_compactions < MAX_OVERFLOW_COMPACTIONS
            && let Some(cfg) = cfg_now.as_ref()
        {
            let silent = u.input > w;
            let truncated = stop == StopReason::Length && u.output == 0 && u.input >= w * 99 / 100;
            if silent || truncated {
                // the round's own record is on disk before the rewrite: the
                // transcript keeps what actually happened, compaction only
                // reshapes the in-memory context
                on_update(AgentUpdate::RoundEnd {
                    messages: history[round_start..].to_vec(),
                    usage,
                });
                let mut sink = StallSink::new(&mut compact_stalled, on_update);
                if force_compact(
                    model,
                    &mut history,
                    cfg,
                    &mut usage_marker,
                    &mut cache_stable,
                    &mut sink,
                ) {
                    overflow_compactions += 1;
                    continue;
                }
            }
        }

        if stop == StopReason::Length {
            // truncated output: don't act on possibly-mangled calls, let the
            // model re-issue them. the whole batch fails: streamed
            // arguments are finalized with a salvage parser, so any call in
            // the message may carry silently incomplete arguments.
            for call in &tool_calls {
                history.push(Msg::ToolResult {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    content: format!(
                        "Tool call \"{}\" was not executed: the response hit the output token \
                         limit, so its arguments may be truncated. Re-issue the tool call with \
                         complete arguments.",
                        call.name
                    ),
                    error: Some(crate::providers::ToolError::Failed),
                    attachments: Vec::new(),
                });
            }
            on_update(AgentUpdate::RoundEnd {
                messages: history[round_start..].to_vec(),
                usage,
            });
            continue;
        }
        if stop != StopReason::ToolUse || tool_calls.is_empty() {
            on_update(AgentUpdate::RoundEnd {
                messages: history[round_start..].to_vec(),
                usage,
            });
            break;
        }
        // Read-only calls from one assistant message have no ordering
        // dependency on each other, so they run concurrently; mutating and exec
        // calls stay strictly serial (see `run_tool_calls`).
        interrupted = run_tool_calls(
            tool_calls,
            tools,
            opts,
            CallSinks {
                approval: &mut *approval,
                on_approval: &mut *on_approval,
                on_update: &mut *on_update,
            },
            &mut history,
        );
        on_update(AgentUpdate::RoundEnd {
            messages: history[round_start..].to_vec(),
            usage,
        });
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
    })
}

/// One model round: everything that streamed back, plus the transport error if
/// the request broke mid-flight (`text` then holds the partial answer already
/// handed to the screen).
struct Round {
    text: String,
    reasoning: Option<String>,
    reasoning_meta: Option<serde_json::Value>,
    tool_calls: Vec<ToolCall>,
    usage: Option<Usage>,
    stop: StopReason,
    error: Option<String>,
}

/// Run one model round, reporting the stream as chrome updates.
fn stream_round(
    model: &crate::providers::ResolvedModel,
    input: &PromptInput<'_>,
    stream: bool,
    on_update: &mut dyn FnMut(AgentUpdate),
) -> Round {
    let mut text = String::new();
    let mut reasoning_text = String::new();
    let mut reasoning_meta: Option<serde_json::Value> = None;
    let mut acc = ToolCallAccumulator::default();
    let mut usage = None;
    let mut stop = StopReason::default();
    let error = model
        .stream(input, stream, &mut |event| match event {
            crate::core::http::Event::Delta(t) => {
                text.push_str(&t);
                on_update(AgentUpdate::Delta(t));
            }
            crate::core::http::Event::ReasoningDelta { text: t, meta } => {
                reasoning_text.push_str(&t);
                if let Some(meta) = meta {
                    reasoning_meta = Some(meta);
                }
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
        })
        .err();
    Round {
        text,
        reasoning: (!reasoning_text.is_empty()).then_some(reasoning_text),
        reasoning_meta,
        tool_calls: acc.finish(),
        usage,
        stop,
        error,
    }
}

/// Price the conversation once per round and, under real pressure, rewrite its
/// stale prefix: attachment blocks older than the last few messages become
/// notes, and an oversized tool result far from the tail is projected down
/// rather than waited for until compaction. Rewriting history in the middle
/// invalidates the provider's cached prefix from that point on, so both passes
/// run only past the rewrite gate — below it the tokens they would save cost
/// less than the cache they would break — and stay unconditional when
/// compaction is off, since then nothing else guards the window. Returns the
/// estimate the compaction gate price, or None when no
/// compaction is configured.
fn price_and_rewrite(
    history: &mut [Msg],
    usage_marker: Option<(usize, Usage)>,
    cfg: Option<&compact::CompactConfig>,
    on_update: &mut dyn FnMut(AgentUpdate),
) -> Option<u64> {
    let mut used_tokens = cfg.map(|_| compact::estimate_tokens(history, usage_marker));
    let rewrite = match (cfg, used_tokens) {
        (Some(c), Some(used)) => compact::rewrite_prefix(used, c.trigger_tokens),
        (None, _) | (Some(_), None) => true,
    };
    if rewrite {
        compact::trim_old_attachments(history);
        // the pass is idempotent, so its notice fires at most once per result
        let stale = compact::prune_stale_tool_results(history);
        if stale.count > 0 {
            on_update(AgentUpdate::ToolResultsPruned { count: stale.count });
        }
        // the passes edited the prefix: re-price rather than hand the pre-trim
        // number to the compaction gate
        used_tokens = cfg.map(|_| compact::estimate_tokens(history, usage_marker));
    }
    used_tokens
}

/// What a compaction check reports back: a rebuild moves every index, so the
/// two bookkeeping markers either follow it or are dropped.
struct AfterTurn {
    /// covered prefix of the history, or None after a rebuild
    usage_marker: Option<(usize, Usage)>,
    /// stable cache prefix, or None once the rebuild rewrote below the summary
    cache_stable: Option<usize>,
    /// the window-anchored trigger for this run (unchanged by a compaction)
    trigger: u64,
}

/// Compact when the estimate says the window is under pressure: prune
/// oversized tool results first (no model call, and it may relieve enough to
/// skip summarizing at all), then summarize at a turn boundary. Runs before a
/// request goes out (the reference's pre-request check), so an interrupted or resumed
/// session cannot skip it. A compaction that cannot run reports why through
/// the sink instead of leaving the session quietly over its window.
fn maybe_compact(
    model: &crate::providers::ResolvedModel,
    history: &mut Vec<Msg>,
    usage_marker: Option<(usize, Usage)>,
    cfg: Option<&compact::CompactConfig>,
    cache_stable: Option<usize>,
    sink: &mut StallSink<'_>,
) -> AfterTurn {
    let mut after = AfterTurn {
        usage_marker,
        cache_stable,
        trigger: cfg.map_or(0, |c| c.trigger_tokens),
    };
    let Some(cfg) = cfg else {
        return after;
    };
    // The usage report covered everything except the assistant we just pushed.
    // A round whose counts never arrived is priced from the text instead (the
    // same chars/4 math `prune_seed_to_fit` runs), because a gateway that omits
    // usage must not silently switch the window gate off.
    let mut estimate = compact::estimate_tokens(history, usage_marker);
    if compact::should_compact(estimate, cfg.trigger_tokens) {
        // pressure confirmed: prune oversized tool results first — it costs no
        // model call and may relieve enough to skip summarization entirely.
        // The usage marker covers the un-pruned prefix, so re-estimating over
        // it would report the identical number; subtract what the projection
        // frees.
        let pruned = compact::prune_tool_results(history);
        if pruned.count > 0 {
            estimate = estimate.saturating_sub(pruned.freed_tokens);
            (sink.on_update)(AgentUpdate::ToolResultsPruned {
                count: pruned.count,
            });
        }
    }
    if compact::should_compact(estimate, cfg.trigger_tokens) {
        // the trigger is window-anchored, not a running count: it stays where
        // it is after a compaction (the reference's rule)
        match compact_now(model, history, cfg, &mut after, &mut *sink.on_update) {
            Ok(()) => {}
            // The history is untouched either way: the run continues, but a
            // session that keeps growing past the trigger has to say why
            Err(reason) => sink.stalled(reason),
        }
    }
    after
}

/// Where a stalled compaction reports through. The notice is said once per run
/// — a summarizer that keeps failing would otherwise repeat itself every round
/// — while the attempt itself keeps happening every round.
pub struct StallSink<'a> {
    warned: &'a mut bool,
    on_update: &'a mut dyn FnMut(AgentUpdate),
}

impl StallSink<'_> {
    pub fn new<'a>(
        warned: &'a mut bool,
        on_update: &'a mut dyn FnMut(AgentUpdate),
    ) -> StallSink<'a> {
        StallSink { warned, on_update }
    }

    fn stalled(&mut self, reason: String) {
        if !*self.warned {
            *self.warned = true;
            (self.on_update)(AgentUpdate::CompactStalled { reason });
        }
    }
}

/// Replace the prefix below the cut with a summary of it. `Err` names why the
/// rebuild did not happen — the caller reports it, because a window that
/// silently stays over its limit is the one failure compaction exists to
/// prevent.
fn compact_now(
    model: &crate::providers::ResolvedModel,
    history: &mut Vec<Msg>,
    cfg: &compact::CompactConfig,
    after: &mut AfterTurn,
    on_update: &mut dyn FnMut(AgentUpdate),
) -> Result<(), String> {
    let Some(cut) = compact::find_cut(history, cfg.effective_keep_recent(cfg.trigger_tokens))
    else {
        return Err("no turn boundary can be dropped without orphaning a tool result".to_string());
    };
    let summary = compact::summarize(model, &history[..cut])?;
    if summary.trim().is_empty() {
        return Err("the summarizer returned nothing".to_string());
    }
    let tail = history.split_off(cut);
    history.clear();
    history.push(Msg::Summary { text: summary });
    history.extend(tail);
    on_update(AgentUpdate::Compacted { removed: cut });
    // the rebuild moved every index: the marker's covered length no longer
    // names anything real, so drop it until the next usage report
    // re-establishes one, and treat the prefix below the summary as rewritten
    after.usage_marker = None;
    after.cache_stable = None;
    Ok(())
}

/// Force a compaction regardless of the trigger: a provider refused the prompt
/// (400 overflow), or reported usage past the known window without one (silent
/// overflow). The caller retries the round on success, bounded by
/// `MAX_OVERFLOW_COMPACTIONS`. A failure reports through the sink and the run
/// carries on — over its window, but loudly.
fn force_compact(
    model: &crate::providers::ResolvedModel,
    history: &mut Vec<Msg>,
    cfg: &compact::CompactConfig,
    usage_marker: &mut Option<(usize, Usage)>,
    cache_stable: &mut Option<usize>,
    sink: &mut StallSink<'_>,
) -> bool {
    let mut after = AfterTurn {
        usage_marker: *usage_marker,
        cache_stable: *cache_stable,
        trigger: cfg.trigger_tokens,
    };
    match compact_now(model, history, cfg, &mut after, &mut *sink.on_update) {
        Ok(()) => {
            *usage_marker = after.usage_marker;
            *cache_stable = after.cache_stable;
            true
        }
        Err(reason) => {
            sink.stalled(reason);
            false
        }
    }
}

/// The mutable sinks a tool round reports through. Bundled so the call runner
/// takes one context instead of a handful of mutably borrowed arguments.
struct CallSinks<'a> {
    approval: &'a mut approval::ApprovalConfig,
    on_approval: &'a mut dyn FnMut(ApprovalRequest) -> ApprovalResponse,
    on_update: &'a mut dyn FnMut(AgentUpdate),
}

/// Execute one assistant message's tool calls: gate them first (extension
/// `tool_call` hooks, the approval matrix, ToolStart chrome), run them — a batch
/// of read-only calls concurrently, anything else strictly serially — then
/// finish them in call order so history and tool results stay deterministic. A
/// batched turn therefore emits its `$` action lines before the results; the
/// renderer's per-tool log state is unaffected because no built-in read tool
/// streams. Returns whether the user interrupted mid-batch.
fn run_tool_calls(
    tool_calls: Vec<ToolCall>,
    tools: &[Box<dyn tools::Tool>],
    opts: &AgentOptions<'_>,
    sinks: CallSinks<'_>,
    history: &mut Vec<Msg>,
) -> bool {
    let CallSinks {
        approval,
        on_approval,
        on_update,
    } = sinks;
    let mut interrupted = false;
    // other agents run a batch of read-only calls together; anything that
    // mutates or executes stays serial
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
            let mut ctx = call_ctx(
                tools,
                &opts.cwd,
                approval,
                on_approval,
                on_update,
                opts.hooks,
            );
            let cleared = prepare_call(&mut call, &mut ctx);
            prepared.push((call, cleared));
        }
        let outs: Vec<(tools::ToolOutput, Vec<String>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = prepared
                .iter()
                .map(|(call, cleared)| match cleared {
                    Ok(cleared) => {
                        let tool = cleared.tool;
                        let cwd = opts.cwd.as_path();
                        Some(scope.spawn(move || {
                            // read-only tools do not stream: buffer any lines and
                            // replay them in order below
                            let mut logs: Vec<String> = Vec::new();
                            let out = tool
                                .execute_call(call, cwd, &mut |l: &str| logs.push(l.to_string()));
                            (out, logs)
                        }))
                    }
                    Err(_) => None,
                })
                .collect();
            handles
                .into_iter()
                .map(|h| match h {
                    Some(h) => h
                        .join()
                        .unwrap_or_else(|_| (tools::ToolOutput::err("tool panicked"), Vec::new())),
                    None => (tools::ToolOutput::err(String::new()), Vec::new()),
                })
                .collect()
        });
        for ((call, cleared), (out, logs)) in prepared.into_iter().zip(outs) {
            for line in &logs {
                on_update(AgentUpdate::ToolLog(crate::core::text::strip_ansi(line)));
            }
            let out = match cleared {
                Err(denied) => denied.into(),
                Ok(_) => out,
            };
            let mut ctx = call_ctx(
                tools,
                &opts.cwd,
                approval,
                on_approval,
                on_update,
                opts.hooks,
            );
            finish_call(&call, out, &mut ctx, history);
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
            let mut ctx = call_ctx(
                tools,
                &opts.cwd,
                approval,
                on_approval,
                on_update,
                opts.hooks,
            );
            let out = match prepare_call(&mut call, &mut ctx) {
                Err(denied) => denied.into(),
                Ok(cleared) => {
                    let mut log = |line: &str| {
                        on_update(AgentUpdate::ToolLog(crate::core::text::strip_ansi(line)))
                    };
                    cleared.tool.execute_call(&call, &opts.cwd, &mut log)
                }
            };
            let mut ctx = call_ctx(
                tools,
                &opts.cwd,
                approval,
                on_approval,
                on_update,
                opts.hooks,
            );
            finish_call(&call, out, &mut ctx, history);
        }
    }
    interrupted
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
    call: &mut ToolCall,
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
    if let Err(e) = tools::validate(&tool.parameters(), &mut call.arguments) {
        return Err(denial(format!("invalid arguments: {e}")));
    }

    let bash_command = if tool.name() == "bash" {
        Some(call.arguments["command"].as_str().unwrap_or(""))
    } else {
        None
    };
    // one ask-list lookup for the whole call: resolve() needs it to decide
    // and the prompt needs the pattern to highlight and to spare on `a`
    let hit = bash_command.and_then(|cmd| approval::blacklist_hit(approval, cmd));
    let (ask, reason) = match approval::resolve_with_hit(
        tool.name(),
        tool.tier(),
        approval,
        bash_command,
        hit.clone(),
    ) {
        approval::Decision::Deny(r) => return Err(denial(format!("denied: {r}"))),
        approval::Decision::Ask(r) => (true, r),
        approval::Decision::Auto => (false, String::new()),
    };
    let preview = tool.preview(&call.arguments);
    let diff = tool.diff(&call.arguments, cwd).filter(|d| !d.is_empty());
    // a bash command that hit the ask-list carries its pattern down to the
    // prompt (highlighted there) and to the `a` answer, which spares the
    // pattern — not the whole bash tool — for the session.
    let matched_pattern = hit;
    // a blacklist ask prompts even when an extension said allow:
    // gating every run of the pattern is the file's whole point
    if ask && (!extension_allowed || matched_pattern.is_some()) {
        let answer = on_approval(ApprovalRequest {
            tool: tool.name(),
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

/// The per-round context both call helpers thread through: the tool set and
/// cwd they gate against, the live approval state, and the callbacks the loop
/// reports through. One carrier instead of six parameters each.
struct CallCtx<'a, 'b> {
    tools: &'a [Box<dyn tools::Tool>],
    cwd: &'b std::path::Path,
    approval: &'b mut approval::ApprovalConfig,
    on_approval: &'b mut dyn FnMut(ApprovalRequest) -> ApprovalResponse,
    on_update: &'b mut dyn FnMut(AgentUpdate),
    hooks: &'b crate::agent::ext::Extensions,
}

/// Borrow one round's helpers as a carrier. `tools` keeps its own lifetime so
/// a `ClearedCall` may outlive the borrow of the mutable helpers.
fn call_ctx<'a, 'b>(
    tools: &'a [Box<dyn tools::Tool>],
    cwd: &'b std::path::Path,
    approval: &'b mut approval::ApprovalConfig,
    on_approval: &'b mut dyn FnMut(ApprovalRequest) -> ApprovalResponse,
    on_update: &'b mut dyn FnMut(AgentUpdate),
    hooks: &'b crate::agent::ext::Extensions,
) -> CallCtx<'a, 'b> {
    CallCtx {
        tools,
        cwd,
        approval,
        on_approval,
        on_update,
        hooks,
    }
}

/// Gate one call before execution: fire the extension `tool_call` hook (it
/// may deny or rewrite the arguments), run the approval matrix, and emit the
/// ToolStart chrome. `Err` carries the denial/validation text that becomes
/// an error tool result. Shared by the serial path and the read-only parallel
/// batch so both gate identically.
fn prepare_call<'a>(
    call: &mut ToolCall,
    ctx: &mut CallCtx<'a, '_>,
) -> Result<ClearedCall<'a>, CallRefusal> {
    let tools = ctx.tools;
    let cwd = ctx.cwd;
    let hooks = ctx.hooks;
    let approval = &mut *ctx.approval;
    let on_approval = &mut *ctx.on_approval;
    let on_update = &mut *ctx.on_update;
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
    // the tool may salvage the shape it was sent (the reference's prepareArguments,
    // which runs ahead of validation): the edit tool parses a stringified
    // `edits` array and the legacy flat single-edit shape
    if let Some(tool) = tools.iter().find(|t| t.name() == call.name) {
        call.arguments = tool.prepare_arguments(&call.arguments);
    }
    let cleared = gate_call(call, tools, cwd, approval, on_approval, extension_allowed)?;
    on_update(AgentUpdate::ToolStart {
        name: call.name.clone(),
        preview: cleared.preview.clone(),
        diff: cleared.diff.clone(),
    });
    Ok(cleared)
}

/// Finish one call after execution: let extensions rewrite the model-visible
/// result, emit ToolEnd, and push the result onto the history. Shared by both
/// paths so ordering is identical.
fn finish_call(
    call: &ToolCall,
    out: tools::ToolOutput,
    ctx: &mut CallCtx<'_, '_>,
    history: &mut Vec<Msg>,
) {
    let hooks = ctx.hooks;
    let on_update = &mut *ctx.on_update;
    // extensions may replace the model-visible result (the enabler for
    // log-reducer/redactor plugins — see `docs/extensions.md`); the rewrite is
    // re-capped and fail-open, so a broken plugin costs its own rewrite and
    // never the tool's result
    let out = rewrite_tool_result(out, hooks, call);
    on_update(AgentUpdate::ToolEnd {
        summary: summarize(&out.content),
        is_error: out.is_error(),
    });
    history.push(Msg::ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        content: out.content,
        error: out.error,
        attachments: out.attachments,
    });
}

#[cfg(test)]
mod tests;
