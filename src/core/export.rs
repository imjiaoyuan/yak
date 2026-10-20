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

/// This directory's newest conversation (the newest anywhere when it has
/// none) — the id both export formats write when `yak export` names no
/// thread. The REPL's `/export` passes the live session id instead.
pub fn latest_thread_id(cwd: &Path) -> Result<String, String> {
    let store = Store::open()?;
    let dir = cwd.display().to_string();
    match store.latest_thread(Some(&dir))? {
        Some(cid) => Ok(cid),
        None => store
            .latest_thread(None)?
            .ok_or_else(|| "no conversations to export yet".to_string()),
    }
}

/// Render thread `cid` to markdown and write it to `path` — a directory
/// takes the default name, a relative one resolves against `cwd`. The
/// export is the conversation as it stands: the active branch, not the
/// sibling branches a `/tree` jump left in the file.
pub fn export_thread(cid: &str, cwd: &Path, path: Option<&str>) -> Result<PathBuf, String> {
    let store = Store::open()?;
    let turns = store.read_thread(cid)?;
    let chain = crate::core::threads::chain_indices(&turns, turns.len().saturating_sub(1))?;
    let active: Vec<&StoredTurn> = chain.iter().map(|&i| &turns[i]).collect();
    let md = to_markdown(cid, &active);
    let target = output_path(cwd, path, cid);
    crate::core::fsx::write_atomic(&target, md.as_bytes(), None)
        .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    Ok(target)
}

/// `yak-<id>.md` unless a file name was given; a directory argument gets
/// that name inside it.
fn output_path(cwd: &Path, path: Option<&str>, cid: &str) -> PathBuf {
    let default = || cwd.join(format!("yak-{cid}.md"));
    match path {
        None => default(),
        Some(raw) => {
            let p = Path::new(raw);
            if p.is_dir() {
                p.join(format!("yak-{cid}.md"))
            } else if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            }
        }
    }
}

/// Render `turns` (the thread's active branch, oldest first) as markdown.
pub fn to_markdown(id: &str, turns: &[&StoredTurn]) -> String {
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
    out.push_str(&format!("{} — exported by yak\n", meta.join(" · ")));

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

/// Render thread `cid` to one self-contained HTML file — the export a
/// phone or a browser reads: the conversation, the branch tree in a
/// sidebar, inline images (base64, no external files), a wide-screen
/// two-column layout that collapses to one column under 48em. Returns the
/// path written.
pub fn export_html_thread(cid: &str, cwd: &Path, path: Option<&str>) -> Result<PathBuf, String> {
    let store = Store::open()?;
    let turns = store.read_thread(cid)?;
    let chain = crate::core::threads::chain_indices(&turns, turns.len().saturating_sub(1))?;
    let html = to_html(cid, &turns, &chain);
    let target = html_output_path(cwd, path, cid);
    crate::core::fsx::write_atomic(&target, html.as_bytes(), None)
        .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    Ok(target)
}

/// `yak-<id>.html` unless a name was given; same resolution rules as the
/// markdown export.
fn html_output_path(cwd: &Path, path: Option<&str>, cid: &str) -> PathBuf {
    let default = || cwd.join(format!("yak-{cid}.html"));
    match path {
        None => default(),
        Some(raw) => {
            let p = Path::new(raw);
            if p.is_dir() {
                p.join(format!("yak-{cid}.html"))
            } else if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            }
        }
    }
}

/// The minimal HTML-escaper: the five characters that change meaning.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// One attachment as an inline element: images embed their pixels when the
/// bytes load back (a stored record keeps only provenance); anything else
/// is a provenance note, exactly as the markdown export renders it.
fn html_attachment(a: &Attachment) -> String {
    if a.mime_type.starts_with("image/") {
        let payload = if a.base64_data.is_empty() {
            crate::core::attachments::reload(a)
                .map(|r| r.base64_data)
                .unwrap_or_default()
        } else {
            a.base64_data.clone()
        };
        if payload.is_empty() {
            let from = a.path.as_deref().or(a.url.as_deref()).unwrap_or("(inline)");
            return format!(
                "<p class=\"attachment\">image {} — <em>bytes gone (source: {})</em></p>\n",
                esc(&a.mime_type),
                esc(from)
            );
        }
        return format!(
            "<img src=\"data:{};base64,{}\" alt=\"attachment\" loading=\"lazy\">\n",
            esc(&a.mime_type),
            payload
        );
    }
    let from = match (a.path.as_deref(), a.url.as_deref()) {
        (Some(path), _) => format!("file {}", esc(path)),
        (None, Some(url)) => esc(url),
        (None, None) => "inline".to_string(),
    };
    format!(
        "<p class=\"attachment\">attachment {} — {}</p>\n",
        esc(&a.mime_type),
        from
    )
}

/// The whole document. Styles are inline in a <style> block — no external
/// asset, no network fetch: the file opens offline exactly as online.
pub fn to_html(id: &str, turns: &[StoredTurn], chain: &[usize]) -> String {
    let active: std::collections::HashSet<usize> = chain.iter().copied().collect();
    let mut title = format!("Conversation {id}");
    if let Some(first) = chain.first()
        && !turns[*first].prompt.trim().is_empty()
    {
        title = flatten(&turns[*first].prompt, 80);
    }
    // the sidebar: every turn of every branch, the active chain marked
    let mut nav = String::new();
    let depths = html_depths(turns);
    for (i, t) in turns.iter().enumerate() {
        let mark = if active.contains(&i) {
            " class=\"on\""
        } else {
            ""
        };
        nav.push_str(&format!(
            "<a{} href=\"#t{i}\" style=\"padding-left:{}em\"><span>{}</span> {}</a>\n",
            mark,
            depths[i],
            esc(&t.ts[..t.ts.len().min(10)]),
            esc(&flatten(&turn_label(t), 34)),
        ));
    }
    // the transcript: the active branch only
    let mut body = String::new();
    for &i in chain {
        let t = &turns[i];
        body.push_str(&format!(
            "<section id=\"t{}\">\n<h2>{} · {}{}</h2>\n",
            i,
            esc(&t.ts[..t.ts.len().min(19)]),
            esc(&t.model),
            t.usage
                .map(|u| format!(" <small>↑{} ↓{}</small>", u.input, u.output))
                .unwrap_or_default()
        ));
        for m in &t.messages {
            body.push_str(&html_message(m));
        }
        let answer = t.response.trim_end();
        if !answer.is_empty() {
            body.push_str(&format!("<div class=\"a\">{}</div>\n", esc(answer)));
        }
        if let Some(reasoning) = t
            .reasoning
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            body.push_str(&format!(
                "<details><summary>thinking</summary><pre>{}</pre></details>\n",
                esc(reasoning)
            ));
        }
        body.push_str("</section>\n");
    }
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{}</title>\n<style>\n:root {{ color-scheme: light dark; }}\n* {{ box-sizing: border-box; }}\nbody {{ margin: 0; font: 15px/1.6 ui-sans-serif, system-ui, sans-serif; }}\n.layout {{ display: grid; grid-template-columns: 18em 1fr; min-height: 100vh; }}\nnav {{ border-right: 1px solid #8883; padding: 1em; overflow-x: hidden; }}\nnav a {{ display: block; text-decoration: none; color: inherit; opacity: .75; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; padding: .15em .3em; border-radius: .3em; }}\nnav a.on {{ opacity: 1; font-weight: 600; background: #8882; }}\nnav span {{ opacity: .6; font-variant-numeric: tabular-nums; font-size: .85em; }}\nmain {{ padding: 1.5em; max-width: 46em; }}\nsection {{ border-top: 1px solid #8883; padding-top: 1em; margin-top: 1em; }}\nh2 {{ font-size: .8em; font-weight: 500; opacity: .6; margin: 0 0 .8em; }}\nh2 small {{ font-weight: 400; }}\n.u {{ font-weight: 600; white-space: pre-wrap; }}\n.a {{ white-space: pre-wrap; }}\npre {{ background: #8881; padding: .6em; overflow-x: auto; font-size: .85em; }}\ncode {{ font-family: ui-monospace, monospace; }}\ntool {{ opacity: .7; font-size: .85em; }}\n.attachment {{ opacity: .6; font-size: .85em; }}\nimg {{ max-width: 100%; }}\ndetails {{ margin: .4em 0; opacity: .8; }}\nsummary {{ cursor: pointer; font-size: .85em; }}\n@media (max-width: 48em) {{ .layout {{ grid-template-columns: 1fr; }} nav {{ border: 0; border-bottom: 1px solid #8883; }} }}\n</style>\n<div class=\"layout\">\n<nav>\n{}\n</nav>\n<main>\n<h1>{}</h1>\n<p class=\"attachment\">{} · {} turn{} on the active branch</p>\n{}\n</main>\n</div>\n",
        esc(&title),
        nav,
        esc(&title),
        esc(id),
        chain.len(),
        if chain.len() == 1 { "" } else { "s" },
        body
    )
}

/// One wire message as HTML.
fn html_message(m: &Msg) -> String {
    match m {
        Msg::User { text, attachments } => {
            let mut out = String::new();
            if !text.trim().is_empty() {
                out.push_str(&format!("<div class=\"u\">{}</div>\n", esc(text)));
            }
            for a in attachments {
                out.push_str(&html_attachment(a));
            }
            out
        }
        Msg::Assistant {
            text, tool_calls, ..
        } => {
            let mut out = String::new();
            if !text.trim().is_empty() {
                out.push_str(&format!("<div class=\"a\">{}</div>\n", esc(text)));
            }
            for call in tool_calls {
                let args = serde_json::to_string_pretty(&call.arguments).unwrap_or_default();
                out.push_str(&format!(
                    "<p><tool>{} <code>{}</code></p>\n<pre>{}</pre>\n",
                    esc(&call.name),
                    esc(&flatten(&call.arguments.to_string(), 100)),
                    esc(&args)
                ));
            }
            out
        }
        Msg::ToolResult {
            name,
            content,
            error,
            attachments,
            ..
        } => {
            let mut out = String::new();
            let label = if error.is_some() {
                "tool error"
            } else {
                "tool result"
            };
            out.push_str(&format!(
                "<p><tool>{} {}</tool></p>\n<pre>{}</pre>\n",
                label,
                esc(name),
                esc(content)
            ));
            for a in attachments {
                out.push_str(&html_attachment(a));
            }
            out
        }
        Msg::Summary { text } => {
            format!(
                "<p><em>compacted summary</em></p>\n<div class=\"a\">{}</div>\n",
                esc(text)
            )
        }
    }
}

/// Depth per turn for the sidebar indent, mirroring the /tree view.
fn html_depths(turns: &[StoredTurn]) -> Vec<usize> {
    let mut by_id = std::collections::HashMap::new();
    for (i, t) in turns.iter().enumerate() {
        by_id.insert(t.id.as_str(), i);
    }
    let mut depths = vec![0usize; turns.len()];
    for i in 0..turns.len() {
        depths[i] = match &turns[i].parent {
            None => i.checked_sub(1).map(|p| depths[p]).unwrap_or(0),
            Some(parent) => match by_id.get(parent.as_str()) {
                Some(&p) if p < i => depths[p] + 1,
                _ => 0,
            },
        };
    }
    depths
}

/// One sidebar label: prompt, actions, or the answer's head.
fn turn_label(t: &StoredTurn) -> String {
    if !t.prompt.trim().is_empty() {
        return t.prompt.clone();
    }
    if !t.response.trim().is_empty() {
        return t.response.clone();
    }
    "--".to_string()
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
        assert_eq!(output_path(&cwd, None, "01ABC"), cwd.join("yak-01ABC.md"));
        // a relative name resolves against cwd
        assert_eq!(
            output_path(&cwd, Some("notes.md"), "01ABC"),
            cwd.join("notes.md")
        );
        // an existing directory takes the default name inside it
        let as_dir = dir.display().to_string();
        assert_eq!(
            output_path(&cwd, Some(&as_dir), "01ABC"),
            dir.join("yak-01ABC.md")
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
            parent: None,
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
        let md = to_markdown("01THREAD", &[&t]);
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
        let md = to_markdown("01THREAD", &[&t]);
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
        let md = to_markdown("01THREAD", &[&t]);
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
        let md = to_markdown("01THREAD", &[&t]);
        assert_eq!(md.matches("the final answer").count(), 1);
    }

    #[test]
    fn a_prompt_only_thread_still_exports() {
        let mut t = turn(Vec::new());
        t.response = "done".into();
        let md = to_markdown("01THREAD", &[&t]);
        assert!(md.contains("**User**\n\nhello  world"));
        assert!(md.contains("**Assistant**\n\ndone"));
    }
}
