//! Global user memory: a single hand-editable `~/.llm/LLM.md` injected into
//! the system prompt (agent). One manual region only — memory is
//! user-written, never agent-written.

use std::path::PathBuf;

/// cap on the injected section (bytes), char-boundary safe
const SECTION_CAP: usize = 16 * 1024;

pub fn memory_path() -> PathBuf {
    crate::core::config::user_dir().join("LLM.md")
}

/// The `<user_memory>` system-prompt section. Always present, even when the
/// file does not exist yet: the agent has to know *where* durable preferences
/// go before it can be asked to remember one (README/docs point users at the
/// path, but that is no use mid-task). The empty-file shell costs a few
/// tokens and keeps the prompt byte-identical across rounds (the prefix
/// cache depends on it).
pub fn section() -> String {
    section_at(&memory_path())
}

fn section_at(path: &std::path::Path) -> String {
    let body = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => "(empty — durable user preferences belong here, one line each)".to_string(),
    };
    let mut body = body;
    if body.len() > SECTION_CAP {
        let end = crate::core::text::floor_boundary(&body, SECTION_CAP);
        body.truncate(end);
        body.push_str("\n[truncated]");
    }
    format!(
        "<user_memory path=\"{}\">\n{body}\n</user_memory>",
        path.display()
    )
}

// The agent-facing `remember` tool and the `/memory` command were removed:
// memory is hand-edited only (~/.llm/LLM.md).
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
        let s = section_at(&file);
        assert!(s.starts_with("<user_memory path="));
        assert!(s.contains("[truncated]"));
        assert!(s.len() < 20 * 1024 + 200);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_missing_or_empty_file_still_names_the_path() {
        let tmp = std::env::temp_dir().join(format!("llm-memory-e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("LLM.md");
        for absent in [true, false] {
            if !absent {
                std::fs::write(&file, "   \n").unwrap();
            }
            let s = section_at(&file);
            assert!(
                s.contains(&file.display().to_string()),
                "the path must be discoverable even with no memory file: {s}"
            );
            assert!(s.contains("empty"), "{s}");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
