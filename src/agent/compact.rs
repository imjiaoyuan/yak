//! Context compaction: estimate tokens (last usage + chars/4 tail), cut at a
//! turn boundary keeping a recent window, and summarize the dropped prefix
//! with one tool-free LLM call (the reference's single-strategy approach).

use std::path::{Path, PathBuf};

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
                is_error,
                ..
            } => {
                let flag = if *is_error { " (error)" } else { "" };
                format!("tool result {name}{flag}: {content}")
            }
        };
        crate::core::text::truncate_ellipsis(&mut entry, 2000);
        out.push_str(&entry);
        out.push_str("\n\n");
    }
    out
}

/// Run the summarization call: streaming, no tools.
pub fn summarize(
    model: &crate::providers::ResolvedModel,
    prefix: &[Msg],
) -> Result<String, String> {
    let prompt = format!(
        "{TEMPLATE}\n\n<conversation>\n{}</conversation>",
        serialize_prefix(prefix)
    );
    let input = PromptInput {
        system: Some(SUMMARIZER_SYSTEM),
        history: &[],
        prompt: &prompt,
        attachments: &[],
        tools: &[],
        reasoning: None,
        note: None,
        cache_anchor: None,
        cache_key: None,
    };
    let mut text = String::new();
    model.stream(&input, true, &mut |event| {
        if let Event::Delta(t) = event {
            text.push_str(&t);
        }
    })?;
    Ok(text.trim().to_string())
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

/// Pruning sizes (the compaction-tool-result-pruner defaults, in chars
/// not bytes): a tool result over the threshold becomes its head, a marker
/// naming what was cut, and its tail. Head + tail stay under the threshold,
/// so a pruned result never re-qualifies and a second pass is a no-op.
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

/// Where pruned tool-result originals live, so `recall` can page them back.
/// Beside the thread store (`~/.llm/observations/`), not inside a project.
pub fn observation_dir() -> PathBuf {
    crate::core::config::user_dir().join("observations")
}

/// Archive key for a pruned result, derived from its content rather than
/// minted per cut. Pruning is part of the load path too (a resumed thread
/// that no longer fits is projected down again), so the same bytes must land
/// in the same file: re-pruning rewrites nothing and no two copies of one
/// result pile up. Not an id or an RNG — a 64-bit content fingerprint, which
/// is all `recall` needs to find the file back.
fn content_id(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a
    for b in text.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Project one over-budget tool result down to head + marker + tail,
/// archiving the whole original under its content id. `None` when the result
/// fits (or the archive cannot be written: a marker pointing at nothing
/// would be worse than the full text). Returns the projected text and the
/// tokens it keeps out of the next request.
fn prune_one(content: &str, archive_dir: &Path, threshold: usize) -> Option<(String, u64)> {
    let total = content.chars().count();
    if total <= threshold {
        return None;
    }
    let id = content_id(content);
    let path = archive_dir.join(format!("{id}.txt"));
    if !path.exists()
        && (std::fs::create_dir_all(archive_dir).is_err()
            || std::fs::write(&path, content).is_err())
    {
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
         the full result is archived as observation {id} — call recall with \
         id=\"{id}\" and offset={PRUNE_HEAD_CHARS} to page the missing middle back, or \
         re-read the file if that is cheaper ...]\n{tail}"
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

/// Resolve an observation id to its archive file. The id comes from the
/// model (a marker in the transcript), so anything that is not a plain
/// alphanumeric token — a `/`, a `..`, an empty string — is not an id and
/// must never reach the filesystem.
pub fn observation_path(dir: &Path, id: &str) -> Option<PathBuf> {
    if id.is_empty() || id.len() > 32 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    Some(dir.join(format!("{id}.txt")))
}

/// Replace over-budget tool-result text with head + marker + tail. Runs only
/// once compaction pressure is confirmed, before the summarizer picks its
/// cut — it costs no model call and may relieve enough to skip
/// summarization entirely (the caller subtracts `freed_tokens` to see that).
///
/// The cut middle is not lost: the full original is archived under
/// `archive_dir` and the marker carries its observation id, so the model can
/// `recall` the exact bytes back instead of re-running the command.
///
/// Fail open per result: if the archive cannot be written, that result keeps
/// its full text rather than leave a marker pointing at nothing.
pub fn prune_tool_results(history: &mut [Msg], archive_dir: &Path) -> PrunedResults {
    let len = history.len();
    prune_matching(history, archive_dir, PRUNE_THRESHOLD_CHARS, len)
}

/// The every-round pass, run before each request regardless of compaction
/// pressure: results over [`STALE_PRUNE_THRESHOLD_CHARS`] in the stale
/// prefix (everything before the last [`KEEP_FRESH_MESSAGES`] messages) are
/// projected down too. Recent results stay verbatim — the model usually
/// needs those — while a huge one from earlier in the task stops being
/// re-sent every round. Same archive, same idempotence as the pressure
/// pass, so the notice fires once per result.
pub fn prune_stale_tool_results(history: &mut [Msg], archive_dir: &Path) -> PrunedResults {
    let stale_up_to = history.len().saturating_sub(KEEP_FRESH_MESSAGES);
    prune_matching(
        history,
        archive_dir,
        STALE_PRUNE_THRESHOLD_CHARS,
        stale_up_to,
    )
}

fn prune_matching(
    history: &mut [Msg],
    archive_dir: &Path,
    threshold: usize,
    up_to: usize,
) -> PrunedResults {
    let mut out = PrunedResults {
        count: 0,
        freed_tokens: 0,
    };
    for msg in history[..up_to].iter_mut() {
        let Msg::ToolResult { content, .. } = msg else {
            continue;
        };
        let Some((projected, freed)) = prune_one(content, archive_dir, threshold) else {
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

    /// A throwaway archive dir; each test gets its own so parallel runs do
    /// not race on the same observation files.
    fn obs_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("llm-obs-test-{}", crate::core::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
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

    #[test]
    fn oversized_tool_results_prune_to_head_marker_tail() {
        let original = "x".repeat(20_000);
        let mut history = vec![
            Msg::ToolResult {
                call_id: "1".into(),
                name: "bash".into(),
                content: original.clone(),
                is_error: false,
                attachments: Vec::new(),
            },
            Msg::tool_result("2", "bash", "small"),
        ];
        let dir = obs_dir();
        let before = estimate_tokens(&history, None);
        let pruned = prune_tool_results(&mut history, &dir);
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
        // the cut middle is archived and the marker names its id, so recall
        // can page the exact original back instead of re-running the command
        let id = observation_id_from_marker(content).expect("marker carries an observation id");
        let archived = std::fs::read_to_string(observation_path(&dir, &id).unwrap()).unwrap();
        assert_eq!(
            archived, original,
            "the archive holds the untouched original"
        );
        // pruning tells the caller what it frees, so a pressure check that
        // rests on the provider's usage figure (which still covers the
        // un-pruned prefix) can see the relief
        let after = estimate_tokens(&history, None);
        assert_eq!(before - after, pruned.freed_tokens);
        assert!(pruned.freed_tokens > 3_000, "{}", pruned.freed_tokens);
        // a pruned result is under threshold: the second pass is a no-op
        let again = prune_tool_results(&mut history, &dir);
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
        let dir = obs_dir();
        let pruned = prune_stale_tool_results(&mut history, &dir);
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
        let again = prune_stale_tool_results(&mut history, &dir);
        assert_eq!((again.count, again.freed_tokens), (0, 0));

        // a result of the same size inside the fresh window stays whole
        let mut fresh = filler(1);
        fresh.push(big());
        let untouched = prune_stale_tool_results(&mut fresh, &obs_dir());
        assert_eq!(untouched.count, 0, "recent results stay verbatim");
    }

    /// Pull the id back out the way a reader (or the recall tool) would.
    fn observation_id_from_marker(content: &str) -> Option<String> {
        let rest = content.split("observation ").nth(1)?;
        Some(rest.split_whitespace().next()?.to_string())
    }

    #[test]
    fn a_failed_archive_keeps_the_full_text() {
        // a marker must never point at a file that was not written: if the
        // archive dir cannot be created, pruning leaves the result alone
        let mut history = vec![Msg::tool_result("1", "bash", "x".repeat(20_000))];
        // a regular file where the archive directory should be: create_dir_all fails
        let blocker =
            std::env::temp_dir().join(format!("llm-obs-file-{}", crate::core::db::ulid()));
        std::fs::write(&blocker, "not a directory").unwrap();
        let blocked = blocker.join("observations");
        assert_eq!(prune_tool_results(&mut history, &blocked).count, 0);
        match &history[0] {
            Msg::ToolResult { content, .. } => assert_eq!(content.chars().count(), 20_000),
            _ => unreachable!(),
        }
    }

    /// The archive is keyed by content, so pruning the same bytes twice (one
    /// thread resumed under pressure over and over) touches one file and the
    /// marker stays the same.
    #[test]
    fn the_archive_is_content_addressed() {
        let dir = obs_dir();
        let big = Msg::tool_result("1", "read", "y".repeat(30_000));
        let mut first = vec![big.clone()];
        prune_tool_results(&mut first, &dir);
        let id1 = match &first[0] {
            Msg::ToolResult { content, .. } => {
                observation_id_from_marker(content).expect("marker id")
            }
            _ => unreachable!(),
        };
        // a second, independent view of the same result (a re-read thread):
        // same archive, same id, no second file
        let mut second = vec![big];
        prune_tool_results(&mut second, &dir);
        assert_eq!(first, second, "identical input, identical projection");
        let files: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            files,
            vec![format!("{id1}.txt")],
            "one file per distinct content"
        );
    }

    #[test]
    fn observation_ids_cannot_escape_their_directory() {
        let dir = std::path::Path::new("/tmp/observations");
        assert!(observation_path(dir, "01jz5k9h7q2m3n4p5r6s7t8v9w").is_some());
        // traversal, separators and empty ids are refused, not cleaned up
        assert_eq!(observation_path(dir, "../../etc/passwd"), None);
        assert_eq!(observation_path(dir, "a/b"), None);
        assert_eq!(observation_path(dir, ""), None);
    }

    #[test]
    fn pruning_cuts_on_codepoint_boundaries() {
        // byte-indexed slicing would split a CJK char mid-codepoint
        let mut history = vec![Msg::tool_result("1", "read", "中".repeat(10_000))];
        assert_eq!(prune_tool_results(&mut history, &obs_dir()).count, 1);
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
