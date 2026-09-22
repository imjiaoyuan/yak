//! Conversation export: a stored thread rendered as one markdown document.
//! The wire messages are the source of truth — the same ones a resume
//! replays — flattened for reading: user and assistant text as prose, tool
//! arguments and results in backtick fences long enough to survive any
//! fence already inside them. Attachment bytes stay out (a base64 payload
//! would bury the transcript); only their kind and where they came from are
//! recorded.

use std::path::{Path, PathBuf};

use crate::core::threads::{Store, StoredTurn};
use crate::providers::{Attachment, Msg};

/// Export this directory's newest conversation (the newest anywhere when it
/// has none), returning the session id and the file written. `llm export`
/// calls this with the CLI's PATH argument; the REPL's `/export` shares
/// [`export_thread`] with the live session id instead.
pub fn export_latest(cwd: &Path, path: Option<&str>) -> Result<(String, PathBuf), String> {
    let store = Store::open()?;
    let dir = cwd.display().to_string();
    let cid = match store.latest_thread(Some(&dir))? {
        Some(cid) => cid,
        None => store
            .latest_thread(None)?
            .ok_or_else(|| "no conversations to export yet".to_string())?,
    };
    let path = export_thread(&cid, cwd, path)?;
    Ok((cid, path))
}

/// Render thread `cid` to markdown and write it to `path` — a directory
/// takes the default name, a relative one resolves against `cwd`.
pub fn export_thread(cid: &str, cwd: &Path, path: Option<&str>) -> Result<PathBuf, String> {
    let store = Store::open()?;
    let turns = store.read_thread(cid)?;
    let md = to_markdown(cid, &turns);
    let target = output_path(cwd, path, cid);
    crate::core::fsx::write_atomic(&target, md.as_bytes(), None)
        .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    Ok(target)
}

/// `llm-<id>.md` unless a file name was given; a directory argument gets
/// that name inside it.
fn output_path(cwd: &Path, path: Option<&str>, cid: &str) -> PathBuf {
    let default = || cwd.join(format!("llm-{cid}.md"));
    match path {
        None => default(),
        Some(raw) => {
            let p = Path::new(raw);
            if p.is_dir() {
                p.join(format!("llm-{cid}.md"))
            } else if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            }
        }
    }
}

/// Render `turns` (thread `id`, oldest first) as markdown.
pub fn to_markdown(id: &str, turns: &[StoredTurn]) -> String {
    let mut out = String::new();

    // the first prompt makes the better title; the id is the fallback
    let title = turns
        .first()
        .map(|t| flatten(&t.prompt, 80))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| format!("Conversation {id}"));
    out.push_str(&format!("# {title}\n\n"));

    let first = turns.first();
    let mut meta = vec![format!("`{id}`")];
    if let Some(t) = first {
        meta.push(t.ts.clone());
        if let Some(cwd) = t.cwd.as_deref() {
            meta.push(format!("`{cwd}`"));
        }
        if !t.model.is_empty() {
            meta.push(format!("`{}`", t.model));
        }
    }
    meta.push(format!(
        "{} turn{}",
        turns.len(),
        if turns.len() == 1 { "" } else { "s" }
    ));
    out.push_str(&format!("{} — exported by llm\n", meta.join(" · ")));

    let mut last_model = first.map(|t| t.model.clone()).unwrap_or_default();
    for (n, turn) in turns.iter().enumerate() {
        out.push_str(&format!("\n---\n\n## {} · {}", n + 1, turn.ts));
        // the model only when the thread switched mid-way
        if turn.model != last_model {
            out.push_str(&format!(" · `{}`", turn.model));
            last_model = turn.model.clone();
        }
        if let Some(ms) = turn.duration_ms {
            out.push_str(&format!(" · {:.1}s", ms as f64 / 1000.0));
        }
        if let Some(usage) = turn.usage {
            out.push_str(&format!(" · ↑{} ↓{}", usage.input, usage.output));
        }
        out.push_str("\n\n");

        let mut last_answer: Option<String> = None;
        for m in &turn.messages {
            if let Msg::Assistant { text, .. } = m
                && !text.trim().is_empty()
            {
                last_answer = Some(text.trim_end().to_string());
            }
            render_message(&mut out, m);
        }
        if turn.messages.is_empty() {
            // a turn normally is its messages; a prompt-only round falls
            // back to the prompt
            if !turn.prompt.trim().is_empty() {
                out.push_str(&format!("**User**\n\n{}\n\n", turn.prompt.trim_end()));
            }
        }
        // the round's answer is stored on the turn, popped out of `messages`
        // — render it unless the last assistant message already was it
        let answer = turn.response.trim_end();
        if !answer.trim().is_empty() && last_answer.as_deref() != Some(answer) {
            out.push_str(&format!("**Assistant**\n\n{answer}\n\n"));
        }
        if let Some(reasoning) = turn
            .reasoning
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            out.push_str("**Thinking**\n\n");
            out.push_str(&fenced(reasoning, "text"));
            out.push('\n');
        }
    }

    if let Some(system) = first
        .and_then(|t| t.system.as_deref())
        .map(str::trim_end)
        .filter(|s| !s.trim().is_empty())
    {
        out.push_str("\n---\n\n## System prompt\n\n");
        out.push_str(&fenced(system, "text"));
    }
    out
}

fn render_message(out: &mut String, m: &Msg) {
    match m {
        Msg::User { text, attachments } => {
            if !text.trim().is_empty() {
                out.push_str(&format!("**User**\n\n{}\n\n", text.trim_end()));
            }
            render_attachments(out, attachments);
        }
        Msg::Assistant {
            text, tool_calls, ..
        } => {
            if !text.trim().is_empty() {
                out.push_str(&format!("**Assistant**\n\n{}\n\n", text.trim_end()));
            }
            for call in tool_calls {
                out.push_str(&format!("**Tool** `{}`\n\n", call.name));
                let args = serde_json::to_string_pretty(&call.arguments)
                    .unwrap_or_else(|_| call.arguments.to_string());
                out.push_str(&fenced(&args, "json"));
                out.push('\n');
            }
        }
        Msg::ToolResult {
            name,
            content,
            error,
            attachments,
            ..
        } => {
            let label = if error.is_some() {
                "Tool error"
            } else {
                "Tool result"
            };
            out.push_str(&format!("**{label}** `{name}`\n\n"));
            if !content.trim().is_empty() {
                out.push_str(&fenced(content, "text"));
            }
            render_attachments(out, attachments);
            out.push('\n');
        }
        Msg::Summary { text } => {
            if !text.trim().is_empty() {
                out.push_str(&format!("**Compacted summary**\n\n{}\n\n", text.trim_end()));
            }
        }
    }
}

/// A `*attachment*` line: kind plus provenance, never the payload.
fn render_attachments(out: &mut String, attachments: &[Attachment]) {
    for a in attachments {
        let kind = if a.mime_type.is_empty() {
            "application/octet-stream"
        } else {
            &a.mime_type
        };
        let from = match (a.path.as_deref(), a.url.as_deref()) {
            (Some(path), _) => format!("`{path}`"),
            (None, Some(url)) => url.to_string(),
            // inline-only (a pasted image): the bytes stay behind
            (None, None) => {
                let kb = a.base64_data.len() * 3 / 4 / 1024;
                format!("inline (~{kb} KB)")
            }
        };
        out.push_str(&format!("*attachment* `{kind}` — {from}\n\n"));
    }
}

/// A fence long enough that the body cannot close it: any run of backticks
/// inside (a tool result quoting markdown, say) stays literal, and a body
/// line always shorter than the closing bar never ends the block early.
fn fenced(body: &str, lang: &str) -> String {
    let body = body.trim_end_matches('\n');
    let longest = body
        .split(|c| c != '`')
        .map(|run| run.len())
        .max()
        .unwrap_or(0);
    let bar = "`".repeat(longest.max(2) + 1);
    format!("{bar}{lang}\n{body}\n{bar}\n")
}

/// The first line of `s`, whitespace-collapsed and clipped for a heading.
fn flatten(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_path_defaults_and_resolves() {
        // real dirs under the platform temp dir: a hardcoded `/tmp` is not an
        // absolute path on Windows, and whether it is a directory there is
        // an accident of the drive layout
        let root = crate::core::testutil::scratch_dir("export");
        let cwd = root.join("project");
        let dir = root.join("out");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&dir).unwrap();

        // no argument: the session-named default in cwd
        assert_eq!(output_path(&cwd, None, "01ABC"), cwd.join("llm-01ABC.md"));
        // a relative name resolves against cwd
        assert_eq!(
            output_path(&cwd, Some("notes.md"), "01ABC"),
            cwd.join("notes.md")
        );
        // an existing directory takes the default name inside it
        let as_dir = dir.display().to_string();
        assert_eq!(
            output_path(&cwd, Some(&as_dir), "01ABC"),
            dir.join("llm-01ABC.md")
        );
        // an absolute path to a file is used as given
        let abs = root.join("elsewhere.md");
        let as_abs = abs.display().to_string();
        assert_eq!(output_path(&cwd, Some(&as_abs), "01ABC"), abs);
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn turn(messages: Vec<Msg>) -> StoredTurn {
        StoredTurn {
            v: crate::core::threads::THREAD_FORMAT_VERSION,
            id: "01TESTTURN".into(),
            ts: "2026-09-13T10:00:00+00:00".into(),
            mode: "agent".into(),
            model: "openai/gpt-5".into(),
            cwd: Some("/tmp/project".into()),
            system: None,
            prompt: "hello  world".into(),
            response: String::new(),
            reasoning: None,
            usage: Some(crate::core::threads::TurnUsage {
                input: 1200,
                output: 345,
                ..Default::default()
            }),
            duration_ms: Some(4210),
            options: Vec::new(),
            messages,
        }
    }

    #[test]
    fn renders_the_thread_and_its_metadata() {
        let t = turn(vec![
            Msg::user("hello  world"),
            Msg::Assistant {
                text: "hi".into(),
                tool_calls: vec![crate::providers::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "src/main.rs"}),
                }],
                reasoning: None,
                reasoning_meta: None,
            },
            Msg::ToolResult {
                call_id: "c1".into(),
                name: "read".into(),
                content: "fn main() {}".into(),
                error: None,
                attachments: Vec::new(),
            },
        ]);
        let md = to_markdown("01THREAD", &[t]);
        assert!(md.starts_with("# hello world\n"), "{md}");
        assert!(md.contains("`01THREAD` · 2026-09-13T10:00:00+00:00 · `/tmp/project`"));
        assert!(md.contains("## 1 · 2026-09-13T10:00:00+00:00 · 4.2s · ↑1200 ↓345"));
        assert!(md.contains("**User**\n\nhello  world"));
        assert!(md.contains("**Tool** `read`"));
        assert!(md.contains("\"path\": \"src/main.rs\""));
        assert!(md.contains("**Tool result** `read`"));
        // the order of the transcript is the order of the export
        let user = md.find("**User**").unwrap();
        let call = md.find("**Tool** `read`").unwrap();
        let result = md.find("**Tool result**").unwrap();
        assert!(user < call && call < result);
    }

    #[test]
    fn a_fence_outruns_the_one_inside_it() {
        let body = "```\nstill content\n```";
        let out = fenced(body, "text");
        let bar = &out[..out.find('\n').unwrap()];
        assert_eq!(bar, "````text");
        assert!(out.trim_end().ends_with("````"));
        assert!(out.contains(body));

        // an empty body still gets a closed block
        let empty = fenced("", "json");
        assert_eq!(empty, "```json\n\n```\n");
    }

    #[test]
    fn attachment_payloads_never_reach_the_document() {
        let t = turn(vec![Msg::user_with(
            "look at this",
            vec![Attachment {
                path: Some("/tmp/shot.png".into()),
                url: None,
                mime_type: "image/png".into(),
                base64_data: "QUJDREVGRw".into(),
                filename: None,
            }],
        )]);
        let md = to_markdown("01THREAD", &[t]);
        assert!(md.contains("*attachment* `image/png` — `/tmp/shot.png`"));
        assert!(!md.contains("QUJDREVGRw"));
    }

    #[test]
    fn the_rounds_answer_never_goes_missing_or_doubles() {
        // the loop pops the final assistant message out of `messages` and
        // keeps it as the turn response — the export must not lose it
        let mut t = turn(vec![
            Msg::user("hello  world"),
            Msg::Assistant {
                text: "calling".into(),
                tool_calls: Vec::new(),
                reasoning: None,
                reasoning_meta: None,
            },
        ]);
        t.response = "the final answer".into();
        let md = to_markdown("01THREAD", &[t]);
        assert!(md.contains("**Assistant**\n\ncalling"));
        assert!(md.contains("**Assistant**\n\nthe final answer"));

        // when the message is still in the transcript it is not repeated
        let mut t = turn(vec![
            Msg::user("hello  world"),
            Msg::Assistant {
                text: "the final answer".into(),
                tool_calls: Vec::new(),
                reasoning: None,
                reasoning_meta: None,
            },
        ]);
        t.response = "the final answer".into();
        let md = to_markdown("01THREAD", &[t]);
        assert_eq!(md.matches("the final answer").count(), 1);
    }

    #[test]
    fn a_prompt_only_thread_still_exports() {
        let mut t = turn(Vec::new());
        t.response = "done".into();
        let md = to_markdown("01THREAD", &[t]);
        assert!(md.contains("**User**\n\nhello  world"));
        assert!(md.contains("**Assistant**\n\ndone"));
    }
}
