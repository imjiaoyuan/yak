//! Global user memory: a single hand-editable `~/.llm/LLM.md` injected into
//! the system prompt (agent and chat). One manual region only — the user
//! writes it by hand, `/memory add` appends a line, and the agent's
//! `remember` tool appends dated lines when asked to note something down.

use std::path::PathBuf;

/// cap on the injected section (bytes), char-boundary safe
const SECTION_CAP: usize = 16 * 1024;

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

/// Append one line to the memory file (creating it when absent).
pub fn add_manual_line(line: &str) -> Result<(), String> {
    add_manual_line_at(&memory_path(), line)
}

fn add_manual_line_at(path: &std::path::Path, line: &str) -> Result<(), String> {
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    // a legacy auto block from older versions rides below the manual text
    // and is left untouched
    let line = line.trim_end();
    if !line.is_empty() {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::create_dir_all(path.parent().unwrap_or(path)).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

/// The agent-facing remember: one dated line, deduped against what is
/// already noted (containment either way, case-insensitive).
pub fn remember(line: &str) -> Result<bool, String> {
    remember_at(&memory_path(), line)
}

fn remember_at(path: &std::path::Path, line: &str) -> Result<bool, String> {
    let line = line.trim().trim_matches(['"', '.']);
    if line.is_empty() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lower = line.to_lowercase();
    let dup = text
        .lines()
        .map(|l| l.to_lowercase())
        .any(|l| l.contains(&lower) || lower.contains(&l));
    if dup {
        return Ok(false);
    }
    add_manual_line_at(path, &format!("- [{}] {}", crate::core::db::today(), line))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_dedups_and_dates() {
        let tmp = std::env::temp_dir().join(format!("llm-mem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("LLM.md");
        std::fs::write(&file, "- [2026-01-01] likes concise replies\n").unwrap();
        // new fact lands with today's date
        assert!(remember_at(&file, "prefers vim over emacs").unwrap());
        let out = std::fs::read_to_string(&file).unwrap();
        assert!(out.contains(&format!(
            "- [{}] prefers vim over emacs",
            crate::core::db::today()
        )));
        // containment either way is a duplicate
        assert!(!remember_at(&file, "prefers vim").unwrap());
        assert!(!remember_at(&file, "LIKES CONCISE REPLIES").unwrap());
        // empty/whitespace is a no-op
        assert!(!remember_at(&file, "  ").unwrap());
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
        assert_eq!(inject_section(None, None), None);
    }
}
