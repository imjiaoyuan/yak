//! Global user memory: a single hand-editable `~/.llm/LLM.md` injected into
//! the system prompt (agent and chat). The file has a manual region the
//! automation never touches and a marker-delimited `## Auto memories` region
//! that `/memory update` appends extracted facts to (capped, deduped,
//! secret-redacted).

use std::path::PathBuf;

use crate::providers::{Msg, PromptInput};

const AUTO_HEADING: &str = "## Auto memories";
const AUTO_BEGIN: &str = "<!-- auto:begin -->";
const AUTO_END: &str = "<!-- auto:end -->";
/// cap on the injected section (bytes), char-boundary safe
const SECTION_CAP: usize = 16 * 1024;
/// hard cap on auto entries; the oldest are dropped first
const AUTO_LIMIT: usize = 50;

pub fn memory_path() -> PathBuf {
    crate::core::config::user_dir().join("LLM.md")
}

/// The `<user_memory>` system-prompt section, None when the file is absent
/// or empty.
pub fn section() -> Option<String> {
    section_at(&memory_path())
}

/// Append the global memory section to a chat system prompt; used at
/// startup, so every conversation sees the same memory.
pub fn inject_system(system: Option<String>) -> Option<String> {
    inject_section(system, section())
}

fn inject_section(system: Option<String>, mem: Option<String>) -> Option<String> {
    match mem {
        Some(mem) => Some(match system {
            Some(s) => format!("{s}\n\n{mem}"),
            None => mem,
        }),
        None => system,
    }
}

fn section_at(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    let mut body = text;
    if body.len() > SECTION_CAP {
        let end = crate::core::text::floor_boundary(&body, SECTION_CAP);
        body.truncate(end);
        body.push_str("\n[truncated]");
    }
    Some(format!(
        "<user_memory path=\"{}\">\n{body}\n</user_memory>",
        path.display()
    ))
}

/// A parsed memory file: everything before the auto heading (verbatim,
/// possibly the whole file) plus the auto entries between the markers.
pub struct MemoryDoc {
    pub manual: String,
    pub auto: Vec<String>,
    /// the file already carried an auto region (keep the block on render)
    pub has_auto_block: bool,
}

/// Entry text without the leading `- [date] ` decoration.
fn entry_text(line: &str) -> &str {
    let rest = line.trim_start_matches("- ").trim_start_matches("* ");
    match rest.find("] ") {
        Some(i) => &rest[i + 2..],
        None => rest,
    }
}

pub fn parse(text: &str) -> MemoryDoc {
    let Some(heading_pos) = text.find(AUTO_HEADING) else {
        return MemoryDoc {
            manual: text.to_string(),
            auto: Vec::new(),
            has_auto_block: false,
        };
    };
    let manual = text[..heading_pos].to_string();
    let after = &text[heading_pos + AUTO_HEADING.len()..];
    let begin = after
        .find(AUTO_BEGIN)
        .map(|i| i + AUTO_BEGIN.len())
        .unwrap_or(0);
    let end = after.find(AUTO_END).unwrap_or(after.len());
    let auto = after[begin..end]
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("<!--"))
        .map(str::to_string)
        .collect();
    MemoryDoc {
        manual,
        auto,
        has_auto_block: true,
    }
}

/// Render back: the manual region byte-for-byte, then the auto block. An
/// empty auto list keeps the block only when the file already had one.
pub fn render(doc: &MemoryDoc) -> String {
    let mut out = doc.manual.clone();
    if doc.auto.is_empty() && !doc.has_auto_block {
        return out;
    }
    out.push_str(AUTO_HEADING);
    out.push('\n');
    out.push_str(AUTO_BEGIN);
    out.push('\n');
    for entry in &doc.auto {
        out.push_str(entry);
        out.push('\n');
    }
    out.push_str(AUTO_END);
    out.push('\n');
    out
}

fn today() -> String {
    crate::core::db::now_turn_datetime()
        .split('T')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Append one line to the manual region (before any auto block).
pub fn add_manual_line(line: &str) -> Result<(), String> {
    add_manual_line_at(&memory_path(), line)
}

fn add_manual_line_at(path: &std::path::Path, line: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc = parse(&text);
    let line = line.trim_end();
    if !line.is_empty() {
        doc.manual.push_str(line);
        doc.manual.push('\n');
    }
    std::fs::create_dir_all(path.parent().unwrap_or(path)).map_err(|e| e.to_string())?;
    std::fs::write(path, render(&doc)).map_err(|e| e.to_string())
}

/// True when the text smells like a secret; such facts are dropped.
fn looks_secret(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "sk-",
        "ghp_",
        "gho_",
        "github_pat_",
        "bearer ",
        "-----begin",
        "api_key",
        "password",
    ]
    .iter()
    .any(|pat| t.contains(pat))
}

/// Case-insensitive containment either way against the existing entries.
fn is_duplicate(text: &str, existing: &[String]) -> bool {
    let t = text.to_lowercase();
    existing.iter().any(|e| {
        let e = entry_text(e).to_lowercase();
        e.contains(&t) || t.contains(&e)
    })
}

/// Merge new facts into the doc's auto region: redact, dedup, cap. Returns
/// the entries actually added.
pub fn merge_auto(doc: &mut MemoryDoc, facts: Vec<String>) -> Vec<String> {
    let mut added = Vec::new();
    for fact in facts {
        let fact = fact.trim().trim_matches(['"', '.']).to_string();
        if fact.is_empty() || looks_secret(&fact) || is_duplicate(&fact, &doc.auto) {
            continue;
        }
        let entry = format!("- [{}] {}", today(), fact);
        doc.auto.push(entry.clone());
        added.push(fact);
        while doc.auto.len() > AUTO_LIMIT {
            doc.auto.remove(0);
        }
    }
    added
}

/// Pull the first balanced JSON object out of a reply (the model may wrap it
/// in prose or fences).
pub(crate) fn extract_json_object(text: &str) -> Option<serde_json::Value> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&text[start..=i]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

/// A compact transcript of the session for extraction: user and assistant
/// text, tool calls as one-liners.
fn transcript(history: &[Msg]) -> String {
    let mut out = String::new();
    for msg in history {
        match msg {
            Msg::User { text, .. } => {
                out.push_str("user: ");
                out.push_str(text);
                out.push('\n');
            }
            Msg::Assistant { text, tool_calls } => {
                if !text.is_empty() {
                    out.push_str("assistant: ");
                    out.push_str(text);
                    out.push('\n');
                }
                for c in tool_calls {
                    out.push_str(&format!("assistant used tool: {}\n", c.name));
                }
            }
            _ => {}
        }
    }
    if out.len() > 24 * 1024 {
        let end = crate::core::text::floor_boundary(&out, 24 * 1024);
        out.truncate(end);
        out.push_str("\n[truncated]\n");
    }
    out
}

const EXTRACT_SYSTEM: &str = "You extract durable personal facts from a coding-agent session. \
Only facts useful in FUTURE sessions on OTHER tasks: user preferences, environment details, \
long-term decisions. Never project-specific details (those belong in the project's AGENTS.md). Reply with \
ONLY a JSON object: {\"facts\": [{\"text\": \"...\"}]}, an empty list if nothing qualifies.";

const CONSOLIDATE_SYSTEM: &str = "You consolidate a list of memory entries: merge duplicates, \
drop contradictions keeping the newest, drop anything project-specific or no longer relevant. \
Reply with ONLY a JSON object: {\"facts\": [{\"text\": \"...\"}]}.";

fn ask_model(
    model: &crate::providers::ResolvedModel,
    system: &str,
    prompt: &str,
) -> Result<Vec<String>, String> {
    let input = PromptInput {
        system: Some(system),
        history: &[],
        prompt,
        attachments: &[],
        tools: &[],
        reasoning: None,
    };
    let mut text = String::new();
    model.stream(&input, false, &mut |event| {
        if let crate::core::http::Event::Delta(t) = event {
            text.push_str(&t);
        }
    })?;
    let value = extract_json_object(&text).ok_or_else(|| "model reply was not JSON".to_string())?;
    Ok(value["facts"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| f["text"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

/// `/memory update`: extract facts from the session into the auto region.
/// Returns the added facts (for display).
pub fn update(
    model: &crate::providers::ResolvedModel,
    history: &[Msg],
) -> Result<Vec<String>, String> {
    let path = memory_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc = parse(&text);
    let existing: Vec<String> = doc.auto.iter().map(|e| entry_text(e).to_string()).collect();
    let prompt = format!(
        "Existing memories (do not repeat them):\n{}\n\nSession transcript:\n{}",
        if existing.is_empty() {
            "(none)".to_string()
        } else {
            existing.join("\n")
        },
        transcript(history)
    );
    let facts = ask_model(model, EXTRACT_SYSTEM, &prompt)?;
    let added = merge_auto(&mut doc, facts);
    std::fs::create_dir_all(path.parent().unwrap_or(&path)).map_err(|e| e.to_string())?;
    std::fs::write(&path, render(&doc)).map_err(|e| e.to_string())?;
    Ok(added)
}

/// `/memory clean`: rewrite the auto region via the model; manual untouched.
pub fn clean(model: &crate::providers::ResolvedModel) -> Result<(), String> {
    let path = memory_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc = parse(&text);
    if doc.auto.is_empty() {
        return Ok(());
    }
    let entries: Vec<&str> = doc.auto.iter().map(|e| entry_text(e)).collect();
    let facts = ask_model(model, CONSOLIDATE_SYSTEM, &entries.join("\n"))?;
    if !facts.is_empty() {
        doc.auto.clear();
        merge_auto(&mut doc, facts);
    }
    std::fs::write(&path, render(&doc)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "手写区第一行\n\n## Auto memories\n<!-- auto:begin -->\n- [2026-01-01] old fact\n<!-- auto:end -->\n";

    #[test]
    fn parse_splits_regions_and_strips_dates() {
        let doc = parse(SAMPLE);
        assert_eq!(doc.manual, "手写区第一行\n\n");
        assert_eq!(doc.auto, vec!["- [2026-01-01] old fact".to_string()]);
        assert!(doc.has_auto_block);
        // no markers → everything is manual
        let doc = parse("just manual\n");
        assert_eq!(doc.manual, "just manual\n");
        assert!(doc.auto.is_empty());
        assert!(!doc.has_auto_block);
    }

    #[test]
    fn render_keeps_manual_byte_for_byte() {
        let mut doc = parse(SAMPLE);
        let before = doc.manual.clone();
        let added = merge_auto(&mut doc, vec!["new fact".into(), "old fact".into()]);
        assert_eq!(added, vec!["new fact".to_string()]); // duplicate skipped
        let out = render(&doc);
        assert!(out.starts_with(&before)); // manual region untouched
        assert!(out.contains("new fact"));
        assert_eq!(out.matches("old fact").count(), 1);
    }

    #[test]
    fn auto_cap_drops_oldest() {
        let mut doc = parse("");
        doc.has_auto_block = true;
        // texts chosen so none is a substring of another (dedup is
        // containment-based and would otherwise eat the numeric suffixes)
        let facts: Vec<String> = (0..80).map(|i| format!("item number {i} end")).collect();
        merge_auto(&mut doc, facts);
        assert_eq!(doc.auto.len(), AUTO_LIMIT);
        assert!(doc.auto.last().unwrap().contains("item number 79"));
        assert!(!doc.auto[0].contains("item number 0"));
    }

    #[test]
    fn secrets_are_dropped() {
        let mut doc = parse("");
        let added = merge_auto(
            &mut doc,
            vec!["my key is sk-abc123".into(), "safe fact".into()],
        );
        assert_eq!(added, vec!["safe fact".to_string()]);
    }

    #[test]
    fn json_extraction_tolerates_prose_and_fences() {
        let v = extract_json_object("Here you go:\n```json\n{\"facts\": [{\"text\": \"x\"}]}\n```")
            .unwrap();
        assert_eq!(v["facts"][0]["text"], "x");
        // nested braces inside strings don't confuse the scan
        let v =
            extract_json_object("noise {\"a\": \"literal } brace\", \"facts\": []} tail").unwrap();
        assert!(v["facts"].as_array().unwrap().is_empty());
        assert!(extract_json_object("no json here").is_none());
    }

    #[test]
    fn add_manual_line_lands_before_auto_block() {
        let tmp = std::env::temp_dir().join(format!("llm-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("LLM.md");
        std::fs::write(&file, SAMPLE).unwrap();
        add_manual_line_at(&file, "新的手写行").unwrap();
        let doc = parse(&std::fs::read_to_string(&file).unwrap());
        assert!(doc.manual.contains("新的手写行"));
        assert!(doc.manual.contains("手写区第一行"));
        assert_eq!(doc.auto.len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn section_wraps_and_truncates() {
        let tmp = std::env::temp_dir().join(format!("llm-memory-s-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("LLM.md");
        std::fs::write(&file, "x".repeat(20 * 1024)).unwrap();
        let s = section_at(&file).unwrap();
        assert!(s.starts_with("<user_memory path="));
        assert!(s.contains("[truncated]"));
        assert!(s.len() < 20 * 1024 + 200);
        std::fs::write(&file, "   \n").unwrap();
        assert!(section_at(&file).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn inject_keeps_existing_system_and_skips_absent_memory() {
        let mem = Some("<user_memory>…</user_memory>".to_string());
        assert_eq!(
            inject_section(Some("base".into()), mem.clone()),
            Some("base\n\n<user_memory>…</user_memory>".to_string())
        );
        assert_eq!(
            inject_section(None, mem),
            Some("<user_memory>…</user_memory>".to_string())
        );
        // no memory file → system untouched
        assert_eq!(
            inject_section(Some("base".into()), None),
            Some("base".to_string())
        );
        assert_eq!(inject_section(None, None), None);
    }
}
