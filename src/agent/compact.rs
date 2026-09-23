//! Context compaction: estimate tokens (last usage + chars/4 tail), cut at a
//! turn boundary keeping a recent window, and summarize the dropped prefix
//! with one tool-free LLM call (the reference's single-strategy approach).

use crate::core::http::{Event, Usage};
use crate::providers::Msg;
use crate::providers::PromptInput;

#[derive(Clone)]
pub struct CompactConfig {
    /// The auto-compaction trigger in tokens (`agent.compact_at_tokens`), used
    /// only when the model's context window is unknown: a known window anchors
    /// the trigger at `window - reserve` (the reference's rule) and this value is ignored.
    /// `0` switches automatic compaction off; a provider that refuses a prompt
    /// still forces one, so a session that ran into a wall is not left there.
    pub trigger_tokens: u64,
    /// The tail compaction keeps (the reference's default is 20k).
    pub keep_recent_tokens: u64,
}

impl Default for CompactConfig {
    fn default() -> CompactConfig {
        CompactConfig {
            trigger_tokens: 64_000,
            keep_recent_tokens: 20_000,
        }
    }
}

impl CompactConfig {
    /// The keep-recent window this trigger can honor. A fixed
    /// `keep_recent_tokens` is only meaningful against the trigger it was
    /// chosen for: at a small one a 32k tail is most of the budget, so
    /// `find_cut` finds no boundary that holds it and returns `None` —
    /// compaction then silently never runs and the run dies on the provider's
    /// context error instead. Half the trigger is the ceiling: enough that a
    /// cut can always be found once pressure is real, small enough that the
    /// kept tail does not crowd out the summary that replaces everything before
    /// it.
    pub fn effective_keep_recent(&self, trigger: u64) -> u64 {
        if trigger == 0 {
            // nothing to scale against: keep what the config asks for
            return self.keep_recent_tokens;
        }
        self.keep_recent_tokens.min(trigger / 2)
    }
}

/// Token cost of text: ASCII ≈ 1 token per 4 chars, CJK/fullwidth ≈ 1 token
/// per char (a space-free Chinese sentence would otherwise be undercounted
/// 3-4x, making `/status` and compaction trigger too late).
pub(crate) fn text_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut wide = 0u64;
    for ch in text.chars() {
        if crate::core::render_md::char_width(ch) >= 2 {
            wide += 1;
        } else {
            ascii += 1;
        }
    }
    ascii.div_ceil(4) + wide
}

/// Rough token cost of one message (text estimate plus a per-message overhead).
fn msg_tokens(msg: &Msg) -> u64 {
    let text: &str = match msg {
        Msg::User { text, .. } | Msg::Summary { text } => text,
        Msg::Assistant {
            text, tool_calls, ..
        } => {
            let calls: u64 = tool_calls
                .iter()
                .map(|c| {
                    text_tokens(&c.name) + text_tokens(&c.arguments.to_string()) + 8 // tool-call overhead
                })
                .sum();
            return text_tokens(text) + calls + 16;
        }
        Msg::ToolResult { content, .. } => content,
    };
    text_tokens(text) + 16
}

/// Estimate the context size: the last reported usage covers the first
/// `covered` messages; everything after is estimated at chars/4.
pub fn estimate_tokens(history: &[Msg], usage_marker: Option<(usize, Usage)>) -> u64 {
    match usage_marker {
        Some((covered, u)) => {
            let trailing: u64 = history[covered.min(history.len())..]
                .iter()
                .map(msg_tokens)
                .sum();
            u.input.saturating_add(u.output).saturating_add(trailing)
        }
        None => history.iter().map(msg_tokens).sum(),
    }
}

/// Does this much context cross the trigger? `0` means automatic compaction is
/// off — there is no line to cross, and this side does not invent one; a
/// provider that refuses a prompt is what still stops a run that went too far.
pub fn should_compact(estimate: u64, trigger: u64) -> bool {
    trigger > 0 && estimate >= trigger
}

/// The room every auto-compaction leaves below the model's real window for the
/// next answer and the summarizer call itself — the reference's reserve
/// (the compaction module).
pub const RESERVE_TOKENS: u64 = 16_384;

/// The auto-compaction trigger: `window - reserve` when the model's window is
/// known (the reference's rule, the compaction module), else the configured fallback. There
/// is no ladder — the trigger is anchored to the window, not a running count.
pub fn effective_trigger(configured: u64, window: Option<u64>) -> u64 {
    window.map_or(configured, |w| w.saturating_sub(RESERVE_TOKENS))
}

/// Should this round rewrite the conversation prefix (trim old attachments,
/// project stale tool results)? Those passes save input tokens on every
/// later request, but a mid-history edit invalidates the provider's cached
/// prefix from the change point on: below real pressure the cache they break
/// is worth more than the tokens they would save, so they wait. The gate is
/// half the trigger — comfortably before compaction (`should_compact`), so
/// there is room to relieve pressure without ever calling the summarizer. With
/// automatic compaction off nothing else guards the request, so the passes run
/// every round.
pub fn rewrite_prefix(estimate: u64, trigger: u64) -> bool {
    trigger == 0 || estimate >= trigger / 2
}

/// Find the cut point: the latest turn boundary whose kept tail still holds
/// at least `keep_recent_tokens` (the minimal cut preserving the window).
/// Boundaries are User, Summary and Assistant messages — an assistant stays
/// glued to the tool results that follow it, and a cut never lands ON a
/// ToolResult (that would orphan it from its assistant). Returns None when
/// nothing can be dropped.
pub fn find_cut(history: &[Msg], keep_recent: u64) -> Option<usize> {
    let n = history.len();
    let mut suffix = vec![0u64; n + 1];
    for i in (0..n).rev() {
        suffix[i] = suffix[i + 1] + msg_tokens(&history[i]);
    }
    for i in (0..n).rev() {
        let boundary = matches!(
            history[i],
            Msg::User { .. } | Msg::Summary { .. } | Msg::Assistant { .. }
        );
        if boundary && suffix[i] >= keep_recent && i > 0 {
            return Some(i);
        }
    }
    None
}

const SUMMARIZER_SYSTEM: &str = "You are a context summarization assistant. Your task is to read a \
                                 conversation between a user and an AI assistant, then produce a \
                                 structured summary following the exact format specified. Do NOT \
                                 continue the conversation. Do NOT respond to any questions in the \
                                 conversation. ONLY output the structured summary.";

const SECTIONS: &str = "## Goal\n\
                         [What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\
                         \n\
                         ## Constraints & Preferences\n\
                         - [Any constraints, preferences, or requirements mentioned by user]\n\
                         - [Or \"(none)\" if none were mentioned]\n\
                         \n\
                         ## Progress\n\
                         ### Done\n\
                         - [x] [Completed tasks/changes]\n\
                         \n\
                         ### In Progress\n\
                         - [ ] [Current work]\n\
                         \n\
                         ### Blocked\n\
                         - [Issues preventing progress, if any]\n\
                         \n\
                         ## Key Decisions\n\
                         - **[Decision]**: [Brief rationale]\n\
                         \n\
                         ## Next Steps\n\
                         1. [Ordered list of what should happen next]\n\
                         \n\
                         ## Critical Context\n\
                         - [Any data, examples, or references needed to continue]\n\
                         - [Or \"(none)\" if not applicable]\n\
                         \n\
                         Keep each section concise. Preserve exact file paths, function names, and \
                         error messages.";

const TEMPLATE: &str = "The messages above are a conversation to summarize. Create a structured \
                        context checkpoint summary that another LLM will use to continue the work.\n\
                        \n\
                        Use this EXACT format:\n\
                        \n";

const UPDATE_TEMPLATE: &str = "The messages above are NEW conversation messages to incorporate into \
                              the existing summary provided in <previous-summary> tags.\n\
                              \n\
                              Update the existing structured summary with new information. RULES:\n\
                              - PRESERVE all existing information from the previous summary\n\
                              - ADD new progress, decisions, and context from the new messages\n\
                              - UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n\
                              - UPDATE \"Next Steps\" based on what was accomplished\n\
                              - PRESERVE exact file paths, function names, and error messages\n\
                              - If something is no longer relevant, you may remove it\n\
                              \n\
                              Use this EXACT format:\n\
                              \n";

/// Serialize a message prefix for the summarizer, truncating each entry.
fn serialize_prefix(prefix: &[Msg]) -> String {
    let mut out = String::new();
    for msg in prefix {
        let mut entry = match msg {
            Msg::User { text, .. } => format!("user: {text}"),
            Msg::Summary { text } => format!("prior summary: {text}"),
            Msg::Assistant {
                text, tool_calls, ..
            } => {
                let calls: Vec<String> = tool_calls
                    .iter()
                    .map(|c| format!("{}({})", c.name, c.arguments))
                    .collect();
                let mut s = format!("assistant: {text}");
                if !calls.is_empty() {
                    s.push_str(&format!("\n  tool calls: {}", calls.join("; ")));
                }
                s
            }
            Msg::ToolResult {
                name,
                content,
                error,
                ..
            } => {
                let flag = if error.is_some() { " (error)" } else { "" };
                format!("tool result {name}{flag}: {content}")
            }
        };
        crate::core::text::truncate_ellipsis(&mut entry, 2000);
        out.push_str(&entry);
        out.push_str("\n\n");
    }
    out
}

/// Run the summarization call: streaming, no tools. A prefix that already
/// starts with a summary is an incremental update (the reference's
/// `UPDATE_SUMMARIZATION_PROMPT`), not a from-scratch re-summary.
pub fn summarize(
    model: &crate::providers::ResolvedModel,
    prefix: &[Msg],
) -> Result<String, String> {
    let (template, conversation) = match prefix.first() {
        Some(Msg::Summary { text }) => (
            UPDATE_TEMPLATE,
            format!(
                "<previous-summary>\n{text}\n</previous-summary>\n\n<conversation>\n{}</conversation>",
                serialize_prefix(&prefix[1..])
            ),
        ),
        _ => (
            TEMPLATE,
            format!(
                "<conversation>\n{}</conversation>",
                serialize_prefix(prefix)
            ),
        ),
    };
    let prompt = format!("{template}\n{SECTIONS}\n\n{conversation}");
    let input = PromptInput {
        max_request_bytes: crate::core::http::MAX_REQUEST_BYTES,
        system: Some(SUMMARIZER_SYSTEM),
        history: &[],
        prompt: &prompt,
        attachments: &[],
        tools: &[],
        reasoning: None,
        cache_anchor: None,
        cache_key: None,
        cache_ttl: None,
    };
    let mut text = String::new();
    model.stream(&input, true, &mut |event| {
        if let Event::Delta(t) = event {
            text.push_str(&t);
        }
    })?;
    Ok(text.trim().to_string())
}

/// How many of the most recent attachment-bearing messages keep their
/// images/documents. Multimodal blocks are the most expensive context
/// there is and the worst-cached; old screenshots rarely matter to the
/// current work, so everything older than the last few becomes a note.
pub const KEEP_ATTACHMENT_MESSAGES: usize = 2;

/// Replace attachments older than the last [`KEEP_ATTACHMENT_MESSAGES`]
/// attachment-bearing messages with a textual note (name + mime). The
/// saved input tokens apply to every subsequent request; the one-time
/// prefix shift costs a single cache miss. Idempotent: trimmed messages
/// carry no attachments, so a second pass changes nothing.
pub fn trim_old_attachments(history: &mut [Msg]) {
    let with_attachments: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| match m {
            Msg::User { attachments, .. } | Msg::ToolResult { attachments, .. } => {
                !attachments.is_empty()
            }
            _ => false,
        })
        .map(|(i, _)| i)
        .collect();
    let keep: std::collections::HashSet<usize> = with_attachments[with_attachments
        .len()
        .saturating_sub(KEEP_ATTACHMENT_MESSAGES)..]
        .iter()
        .copied()
        .collect();
    for (i, msg) in history.iter_mut().enumerate() {
        if keep.contains(&i) {
            continue;
        }
        let (text, attachments): (&mut String, &mut Vec<crate::providers::Attachment>) = match msg {
            Msg::User { text, attachments } => (text, attachments),
            Msg::ToolResult {
                content,
                attachments,
                ..
            } => (content, attachments),
            _ => continue,
        };
        if attachments.is_empty() {
            continue;
        }
        let names: Vec<String> = attachments
            .iter()
            .map(|a| {
                let name = a
                    .filename
                    .clone()
                    .or_else(|| a.path.clone())
                    .or_else(|| a.url.clone())
                    .unwrap_or_else(|| "unnamed".to_string());
                format!("{name} ({})", a.mime_type)
            })
            .collect();
        attachments.clear();
        text.push_str(&format!(
            "\n\n[earlier attachments dropped from context: {}]",
            names.join(", ")
        ));
    }
}

/// Pruning sizes (the compaction-tool-result-pruner defaults, in chars
/// not bytes): a tool result over the threshold becomes its head, a marker
/// naming what was cut, and its tail. Head + tail stay under the threshold,
/// so a pruned result never re-qualifies and a second pass is a no-op. The
/// cut middle is dropped — the reference's compaction is lossy, and the untouched
/// original still lives in the thread file and the session log.
pub const PRUNE_THRESHOLD_CHARS: usize = 8192;
pub const PRUNE_HEAD_CHARS: usize = 4096;
pub const PRUNE_TAIL_CHARS: usize = 1024;

/// The stale-prefix pass only touches outright dumps: 32k chars is ~8k
/// tokens of ASCII (more of CJK) — a result that big re-sent every round is
/// the single most expensive thing a session can carry, while results below
/// it may still be read verbatim by the model and are left alone until
/// compaction pressure lowers the bar to [`PRUNE_THRESHOLD_CHARS`].
pub const STALE_PRUNE_THRESHOLD_CHARS: usize = 32_768;
/// How many trailing messages count as fresh (never stale-pruned): the
/// current task's recent tool results, which the model is most likely to
/// still need verbatim.
pub const KEEP_FRESH_MESSAGES: usize = 6;

/// Project one over-budget tool result down to head + marker + tail. `None`
/// when the result fits. Returns the projected text and the tokens it keeps
/// out of the next request.
fn prune_one(content: &str, threshold: usize) -> Option<(String, u64)> {
    let total = content.chars().count();
    if total <= threshold {
        return None;
    }
    // char-indexed cuts: byte offsets would split CJK text mid-codepoint
    let head_end = content
        .char_indices()
        .nth(PRUNE_HEAD_CHARS)
        .map(|(i, _)| i)
        .unwrap_or(content.len());
    let tail_start = content
        .char_indices()
        .nth_back(PRUNE_TAIL_CHARS)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let head = &content[..head_end];
    let tail = &content[tail_start..];
    let dropped = total - PRUNE_HEAD_CHARS - PRUNE_TAIL_CHARS;
    let projected = format!(
        "{head}\n[... {dropped} chars of the middle were cut to fit the context window; \
         re-run the command or re-read the file if you need them ...]\n{tail}"
    );
    let freed = text_tokens(content).saturating_sub(text_tokens(&projected));
    Some((projected, freed))
}

/// What one pruning pass did: how many results were projected down, and the
/// tokens that frees in the next request (the provider usage figure the
/// estimate rests on still covers the un-pruned prefix).
#[derive(Debug, PartialEq, Eq)]
pub struct PrunedResults {
    pub count: usize,
    pub freed_tokens: u64,
}

/// Replace over-budget tool-result text with head + marker + tail. Runs only
/// once compaction pressure is confirmed, before the summarizer picks its
/// cut — it costs no model call and may relieve enough to skip
/// summarization entirely (the caller subtracts `freed_tokens` to see that).
pub fn prune_tool_results(history: &mut [Msg]) -> PrunedResults {
    let len = history.len();
    prune_matching(history, PRUNE_THRESHOLD_CHARS, len)
}

/// The every-round pass, run before each request regardless of compaction
/// pressure: results over [`STALE_PRUNE_THRESHOLD_CHARS`] in the stale
/// prefix (everything before the last [`KEEP_FRESH_MESSAGES`] messages) are
/// projected down too. Recent results stay verbatim — the model usually
/// needs those — while a huge one from earlier in the task stops being
/// re-sent every round.
pub fn prune_stale_tool_results(history: &mut [Msg]) -> PrunedResults {
    let stale_up_to = history.len().saturating_sub(KEEP_FRESH_MESSAGES);
    prune_matching(history, STALE_PRUNE_THRESHOLD_CHARS, stale_up_to)
}

fn prune_matching(history: &mut [Msg], threshold: usize, up_to: usize) -> PrunedResults {
    let mut out = PrunedResults {
        count: 0,
        freed_tokens: 0,
    };
    for msg in history[..up_to].iter_mut() {
        let Msg::ToolResult { content, .. } = msg else {
            continue;
        };
        let Some((projected, freed)) = prune_one(content, threshold) else {
            continue;
        };
        *content = projected;
        out.count += 1;
        out.freed_tokens += freed;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(s: &str) -> Msg {
        Msg::user(s)
    }

    /// The prefix-rewrite gate must sit strictly below the compaction gate:
    /// otherwise history gets rewritten every round for nothing (breaking
    /// the cache), or pressure only appears once summarization is already
    /// the only option left.
    #[test]
    fn rewrite_gate_fires_before_compaction() {
        let cfg = CompactConfig {
            trigger_tokens: 100_000,
            keep_recent_tokens: 0,
        };
        let trigger = cfg.trigger_tokens;
        assert!(
            !rewrite_prefix(49_999, trigger),
            "just under half: no rewrite"
        );
        assert!(rewrite_prefix(50_000, trigger), "at half: rewrite");
        assert!(rewrite_prefix(60_000, trigger));
        // and it must have fired well before the summarizer is due
        assert!(
            !should_compact(50_000, trigger),
            "at the rewrite gate compaction must still have room"
        );
        assert!(should_compact(100_000, trigger), "at the trigger: compact");
        // with compaction off there is no gate to sit under: nothing else
        // guards the request, so the cheap passes run every round
        assert!(rewrite_prefix(0, 0));
        assert!(!should_compact(u64::MAX, 0));
    }

    #[test]
    fn cjk_text_is_not_undercounted() {
        // 16 ASCII -> ~4 tokens; 16 Chinese chars -> ~16 tokens (not ~4),
        // so a Chinese-heavy conversation triggers compaction at the right time
        let ascii = text_tokens(&"a".repeat(16));
        let cjk = text_tokens(&"中".repeat(16));
        assert_eq!(ascii, 4);
        assert_eq!(cjk, 16, "CJK must count ~1 token/char, not chars/4");
        assert!(cjk >= ascii * 3, "CJK must not be undercounted 3-4x");
    }

    #[test]
    fn cut_lands_on_a_boundary_never_a_tool_result() {
        let call = |id: &str| super::super::ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: json!({"command": "ls"}),
        };
        // a single-task history: [user, asst+call, result, user, asst]
        let history = vec![
            user(&"x".repeat(400)),
            Msg::Assistant {
                text: String::new(),
                tool_calls: vec![call("1")],
                reasoning: None,
                reasoning_meta: None,
            },
            Msg::tool_result("1", "bash", "a\nb\nc"),
            user(&"y".repeat(400)),
            Msg::assistant("z".repeat(400)),
        ];
        let cut = find_cut(&history, 1).unwrap();
        // minimal cut keeps the tail from the last assistant (index 4)
        assert_eq!(cut, 4);

        // keep window forces the cut before an assistant whose big result
        // must travel with it: cutting at the ToolResult is forbidden
        let history = vec![
            user(&"x".repeat(400)),
            Msg::Assistant {
                text: String::new(),
                tool_calls: vec![call("1")],
                reasoning: None,
                reasoning_meta: None,
            },
            Msg::tool_result("1", "bash", "r".repeat(4000)),
            Msg::Assistant {
                text: String::new(),
                tool_calls: vec![call("2")],
                reasoning: None,
                reasoning_meta: None,
            },
            Msg::tool_result("2", "bash", "r".repeat(4000)),
        ];
        // keep everything from the second assistant onward (~1100 tokens)
        let cut = find_cut(&history, 1000).unwrap();
        assert_eq!(cut, 3);
        assert!(matches!(history[cut], Msg::Assistant { .. }));
        // its result at index 4 stays in the kept tail
        assert!(matches!(history[cut + 1], Msg::ToolResult { .. }));
    }

    #[test]
    fn cut_returns_none_without_a_droppable_prefix() {
        // only boundary is index 0 — nothing to cut
        assert_eq!(find_cut(&[user("hi")], 1), None);
        // an assistant is a boundary, so cutting to it is valid
        let history = vec![user("hi"), Msg::assistant("ho")];
        assert_eq!(find_cut(&history, 1), Some(1));
    }

    #[test]
    fn estimate_uses_marker_plus_tail() {
        let history = vec![user(&"a".repeat(4000)), user(&"b".repeat(400))];
        // marker covers 1 message with 100+50 tokens
        let est = estimate_tokens(
            &history,
            Some((
                1,
                Usage {
                    input: 100,
                    output: 50,
                    cached: 0,
                    cached_write: 0,
                },
            )),
        );
        assert!(est >= 150);
        let all = estimate_tokens(&history, None);
        assert!(all > est);
    }

    #[test]
    fn keep_recent_never_outgrows_the_trigger_it_guards() {
        // the default trigger honors the full configured tail (the reference's 20k)
        let big = CompactConfig::default();
        assert_eq!(big.effective_keep_recent(big.trigger_tokens), 20_000);
        // a small trigger clamps it to half, so a cut always exists once
        // compaction is due (before the clamp, find_cut returned None forever
        // and the run died on a provider context error)
        let small = CompactConfig {
            trigger_tokens: 12_000,
            keep_recent_tokens: 32_000,
        };
        assert_eq!(small.effective_keep_recent(12_000), 6_000);
        // a small configured value is left alone
        let tiny = CompactConfig {
            trigger_tokens: 16_000,
            keep_recent_tokens: 1_000,
        };
        assert_eq!(tiny.effective_keep_recent(16_000), 1_000);
        // OFF keeps what the config asks for: there is no trigger to scale to
        assert_eq!(small.effective_keep_recent(0), 32_000);
        // the clamped value really does let find_cut succeed: the kept tail
        // must itself hold the window, so the last message is the big one
        let history = vec![Msg::user("x".repeat(4_000)), Msg::user("y".repeat(28_000))];
        assert!(find_cut(&history, small.effective_keep_recent(12_000)).is_some());
        // and the unclamped 32k would not have found that cut
        assert_eq!(find_cut(&history, 32_000), None);
    }

    #[test]
    fn should_compact_uses_the_configured_trigger() {
        assert!(!should_compact(63_999, 64_000));
        assert!(should_compact(64_000, 64_000));
        // off: there is no line to cross
        assert!(!should_compact(u64::MAX, 0));
    }

    /// A known window anchors the trigger at `window - reserve` (the reference's rule);
    /// the configured value is only the fallback for an unknown window.
    #[test]
    fn a_known_window_anchors_the_trigger() {
        // no window: the configured fallback stands
        assert_eq!(effective_trigger(64_000, None), 64_000);
        // a known window wins outright
        let w = Some(200_000u64);
        assert_eq!(effective_trigger(64_000, w), 200_000 - RESERVE_TOKENS);
        assert_eq!(effective_trigger(20_000, w), 200_000 - RESERVE_TOKENS);
        // a small window saturates at 0 rather than underflowing
        assert_eq!(effective_trigger(64_000, Some(8_000)), 0);
    }

    #[test]
    fn old_attachments_become_notes_and_it_stays_done() {
        let att = || crate::providers::Attachment {
            mime_type: "image/png".to_string(),
            base64_data: String::new(),
            filename: Some("shot.png".to_string()),
            path: None,
            url: None,
        };
        let mut history = vec![
            Msg::user_with("first", vec![att()]),
            Msg::assistant("a"),
            Msg::user_with("second", vec![att()]),
            Msg::assistant("b"),
            Msg::user_with("third", vec![att()]),
            Msg::user_with("fourth", vec![att()]),
        ];
        trim_old_attachments(&mut history);
        let count = |h: &[Msg]| {
            h.iter()
                .filter(|m| match m {
                    Msg::User { attachments, .. } | Msg::ToolResult { attachments, .. } => {
                        !attachments.is_empty()
                    }
                    _ => false,
                })
                .count()
        };
        assert_eq!(
            count(&history),
            KEEP_ATTACHMENT_MESSAGES,
            "only the tail keeps images"
        );
        match &history[0] {
            Msg::User { text, attachments } => {
                assert!(attachments.is_empty());
                assert!(
                    text.contains("shot.png (image/png)"),
                    "the note names what was dropped"
                );
            }
            _ => panic!("expected a user message"),
        }
        // idempotent: a second pass changes nothing (the note is not doubled)
        let before = format!("{:?}", history);
        trim_old_attachments(&mut history);
        assert_eq!(before, format!("{:?}", history));
    }

    #[test]
    fn serialization_truncates_and_marks_errors() {
        let prefix = vec![
            user("hello"),
            Msg::ToolResult {
                call_id: "9".into(),
                name: "bash".into(),
                content: "e".repeat(3000),
                error: Some(crate::providers::ToolError::Failed),
                attachments: Vec::new(),
            },
        ];
        let s = serialize_prefix(&prefix);
        assert!(s.contains("user: hello"));
        assert!(s.contains("tool result bash (error):"));
        assert!(s.contains('…'));
        assert!(s.len() < 6000);
    }

    #[test]
    fn oversized_tool_results_prune_to_head_marker_tail() {
        let mut history = vec![
            Msg::ToolResult {
                call_id: "1".into(),
                name: "bash".into(),
                content: "x".repeat(20_000),
                error: None,
                attachments: Vec::new(),
            },
            Msg::tool_result("2", "bash", "small"),
        ];
        let before = estimate_tokens(&history, None);
        let pruned = prune_tool_results(&mut history);
        assert_eq!(pruned.count, 1);
        let content = match &history[0] {
            Msg::ToolResult { content, .. } => content,
            _ => unreachable!(),
        };
        assert!(content.starts_with("xxxx"), "the head survives");
        assert!(content.ends_with("xxxx"), "the tail survives");
        assert!(
            content.contains("chars of the middle were cut"),
            "{content}"
        );
        assert!(
            content.len() < PRUNE_HEAD_CHARS + PRUNE_TAIL_CHARS + 300,
            "pruned content is bounded"
        );
        // pruning tells the caller what it frees, so a pressure check that
        // rests on the provider's usage figure (which still covers the
        // un-pruned prefix) can see the relief
        let after = estimate_tokens(&history, None);
        assert_eq!(before - after, pruned.freed_tokens);
        assert!(pruned.freed_tokens > 3_000, "{}", pruned.freed_tokens);
        // a pruned result is under threshold: the second pass is a no-op
        let again = prune_tool_results(&mut history);
        assert_eq!((again.count, again.freed_tokens), (0, 0));
        // small results are untouched
        match &history[1] {
            Msg::ToolResult { content, .. } => assert_eq!(content, "small"),
            _ => unreachable!(),
        }
    }

    /// The stale pass is the pressure-independent one: a big dump older
    /// than the fresh window is projected down even when nothing is close
    /// to compacting, while a same-size result inside the window (and a
    /// small old one) survives verbatim.
    #[test]
    fn stale_pruning_takes_only_old_dumps() {
        let big = || Msg::tool_result("1", "bash", "x".repeat(40_000));
        let small = Msg::tool_result("2", "bash", "tiny");
        let filler = |n: usize| {
            (0..n)
                .map(|i| Msg::assistant(format!("step {i}")))
                .collect::<Vec<_>>()
        };
        // the dump sits well before the fresh window
        let mut history = vec![small.clone(), big()];
        history.extend(filler(KEEP_FRESH_MESSAGES + 2));
        let pruned = prune_stale_tool_results(&mut history);
        assert_eq!(pruned.count, 1, "only the old dump");
        assert!(pruned.freed_tokens > 8_000);
        match (&history[0], &history[1]) {
            (Msg::ToolResult { content: a, .. }, Msg::ToolResult { content: b, .. }) => {
                assert_eq!(a, "tiny", "a small old result is not worth cutting");
                assert!(b.contains("chars of the middle were cut"), "{b}");
            }
            _ => unreachable!(),
        }
        // idempotent: the next round changes nothing and reports nothing
        let again = prune_stale_tool_results(&mut history);
        assert_eq!((again.count, again.freed_tokens), (0, 0));

        // a result of the same size inside the fresh window stays whole
        let mut fresh = filler(1);
        fresh.push(big());
        let untouched = prune_stale_tool_results(&mut fresh);
        assert_eq!(untouched.count, 0, "recent results stay verbatim");
    }

    #[test]
    fn pruning_cuts_on_codepoint_boundaries() {
        // byte-indexed slicing would split a CJK char mid-codepoint
        let mut history = vec![Msg::tool_result("1", "read", "中".repeat(10_000))];
        assert_eq!(prune_tool_results(&mut history).count, 1);
        let content = match &history[0] {
            Msg::ToolResult { content, .. } => content,
            _ => unreachable!(),
        };
        assert!(content.starts_with("中中中"));
        assert!(content.ends_with("中中中"));
        assert!(
            content.chars().count() < PRUNE_HEAD_CHARS + PRUNE_TAIL_CHARS + 300,
            "char-count bounded: {}",
            content.chars().count()
        );
    }
}
