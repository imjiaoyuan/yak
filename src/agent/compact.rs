//! Context compaction: estimate tokens (last usage + chars/4 tail), cut at a
//! turn boundary keeping a recent window, and summarize the dropped prefix
//! with one tool-free LLM call (the reference's single-strategy approach).

use crate::core::http::{Event, Usage};
use crate::providers::Msg;
use crate::providers::PromptInput;

#[derive(Clone)]
pub struct CompactConfig {
    pub context_window: u64,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

impl Default for CompactConfig {
    fn default() -> CompactConfig {
        CompactConfig {
            context_window: 128_000,
            reserve_tokens: 16_384,
            keep_recent_tokens: 32_000,
        }
    }
}

/// Token cost of text: ASCII ≈ 1 token per 4 chars, CJK/fullwidth ≈ 1 token
/// per char (a space-free Chinese sentence would otherwise be undercounted
/// 3-4x, making `/status` and compaction trigger too late).
fn text_tokens(text: &str) -> u64 {
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

pub fn should_compact(estimate: u64, cfg: &CompactConfig) -> bool {
    estimate + cfg.reserve_tokens >= cfg.context_window
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

const SUMMARIZER_SYSTEM: &str = "You compress coding-agent conversations into a dense summary. \
                                 Keep every fact needed to continue the work. Do not add commentary.";

const TEMPLATE: &str = "Summarize this conversation for continuation. Use exactly these sections:\n\
                        ## Goal\n## Constraints\n## Progress\n## Key Decisions\n## Next Steps\n\
                        ## Critical Context\n\nThen append:\n<read-files>\n<modified-files>\n\
                        listing the file paths that were read and modified.";

/// Serialize a message prefix for the summarizer, truncating each entry.
fn serialize_prefix(prefix: &[Msg]) -> String {
    let mut out = String::new();
    for msg in prefix {
        let entry = match msg {
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
                is_error,
                ..
            } => {
                let flag = if *is_error { " (error)" } else { "" };
                format!("tool result {name}{flag}: {content}")
            }
        };
        let mut line = entry;
        if line.len() > 2000 {
            line.truncate(crate::core::text::floor_boundary(&line, 2000));
            line.push('…');
        }
        out.push_str(&line);
        out.push_str("\n\n");
    }
    out
}

/// Run the summarization call: streaming, no tools.
pub fn summarize(
    model: &crate::providers::ResolvedModel,
    prefix: &[Msg],
) -> Result<String, String> {
    summarize_with(model, prefix, "")
}

const TASK_MARK: &str = "# Original task\n";
const SUMMARY_MARK: &str = "\n# Summary\n";

/// Compose the stored summary text: the original task verbatim on top, the
/// compressive summary below. Long-running work must not drift from what
/// was actually asked — the summary compresses, the task never does.
pub fn compose_summary(task: Option<&str>, summary: &str) -> String {
    match task.map(str::trim).filter(|t| !t.is_empty()) {
        Some(task) => format!("{TASK_MARK}{task}{SUMMARY_MARK}\n{summary}"),
        None => summary.to_string(),
    }
}

/// Pull the preserved original task back out of a stored summary (the
/// re-compaction path: the first user message is long gone by then).
pub fn extract_original_task(summary: &str) -> Option<String> {
    let rest = summary.strip_prefix(TASK_MARK)?;
    let end = rest.find(SUMMARY_MARK)?;
    Some(rest[..end].to_string())
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

/// Summarize with the caller's extra instructions appended to the template
/// (the `/compact <prompt>` argument).
pub fn summarize_with(
    model: &crate::providers::ResolvedModel,
    prefix: &[Msg],
    extra: &str,
) -> Result<String, String> {
    let extra = if extra.trim().is_empty() {
        String::new()
    } else {
        format!("\n\nAdditional instructions: {}", extra.trim())
    };
    let prompt = format!(
        "{TEMPLATE}{extra}\n\n<conversation>\n{}</conversation>",
        serialize_prefix(prefix)
    );
    let input = PromptInput {
        system: Some(SUMMARIZER_SYSTEM),
        history: &[],
        prompt: &prompt,
        attachments: &[],
        tools: &[],
        reasoning: None,
    };
    let mut text = String::new();
    model.stream(&input, true, &mut |event| {
        if let Event::Delta(t) = event {
            text.push_str(&t);
        }
    })?;
    Ok(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(s: &str) -> Msg {
        Msg::user(s)
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
            },
            Msg::tool_result("1", "bash", "r".repeat(4000)),
            Msg::Assistant {
                text: String::new(),
                tool_calls: vec![call("2")],
                reasoning: None,
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
                },
            )),
        );
        assert!(est >= 150);
        let all = estimate_tokens(&history, None);
        assert!(all > est);
    }

    #[test]
    fn threshold_triggers() {
        let cfg = CompactConfig {
            context_window: 1000,
            reserve_tokens: 100,
            keep_recent_tokens: 1,
        };
        assert!(!should_compact(500, &cfg));
        assert!(should_compact(901, &cfg));
        assert!(should_compact(10_000, &cfg));
    }

    #[test]
    fn summary_composes_and_extracts_the_original_task() {
        let s = compose_summary(Some("fix the login bug"), "## Goal\nfix login");
        assert!(s.starts_with("# Original task\nfix the login bug\n# Summary\n"));
        assert_eq!(
            extract_original_task(&s).as_deref(),
            Some("fix the login bug"),
            "re-compaction recovers the task verbatim"
        );
        // no task → bare summary; garbage → None
        assert_eq!(compose_summary(None, "just summary"), "just summary");
        assert_eq!(compose_summary(Some("  "), "s"), "s");
        assert_eq!(extract_original_task("no marks here"), None);
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
                is_error: true,
                attachments: Vec::new(),
            },
        ];
        let s = serialize_prefix(&prefix);
        assert!(s.contains("user: hello"));
        assert!(s.contains("tool result bash (error):"));
        assert!(s.contains('…'));
        assert!(s.len() < 6000);
    }
}
