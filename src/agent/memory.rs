//! Global user memory: a single hand-editable `~/.llm/LLM.md` injected into
//! the system prompt (agent). One manual region only — the user writes it
//! by hand, `/memory add` appends a line. (The agent `remember` tool was
//! removed on purpose: memory is manual, not agent-written.)

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

// The agent-facing `remember` tool was removed: memory is hand-edited only.
#[cfg(test)]
mod tests {
    use super::*;

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
}
