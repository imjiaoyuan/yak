//! Global user memory: a single hand-editable `~/.yak/YAK.md` injected into
//! the agent system prompt. The `memory` extension appends one dated line when
//! the user asks to note something down; everything else is hand-edited by
//! the user who owns the file.

use std::path::PathBuf;

/// cap on the injected section (bytes), char-boundary safe
const SECTION_CAP: usize = 16 * 1024;

fn memory_path() -> PathBuf {
    crate::core::config::user_dir().join("YAK.md")
}

/// The `<user_memory>` system-prompt section: two rules — when to write and
/// where project facts go — and the file itself (placeholder when absent,
/// capped on a char boundary). Always present: the agent has to know *where*
/// durable preferences go before it can be asked to remember one. The
/// placeholder shell costs a few tokens and keeps the prompt byte-identical
/// across rounds (the prefix cache depends on it).
pub fn section() -> String {
    section_at(&memory_path())
}

fn section_at(path: &std::path::Path) -> String {
    section_tagged(path, "user_memory")
}

/// The shared body of a memory section: read, placeholder when empty, cap,
/// wrap in the named tag. `<user_memory>` and `<project_memory>` render
/// through the same rules so neither drifts from the other.
fn section_tagged(path: &std::path::Path, tag: &str) -> String {
    let body = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => format!("(empty — durable {tag} notes belong here, one line each)"),
    };
    let mut body = body;
    if body.len() > SECTION_CAP {
        let end = crate::core::text::floor_boundary(&body, SECTION_CAP);
        body.truncate(end);
        body.push_str("\n[truncated]");
    }
    format!(
        "Memory: durable, reusable facts about the user (preferences, environment, ways of working) live in the file below. Append with the `memory` tool only when the user asks or the fact is clearly durable — never secrets or one-off task details.\n<{tag} path=\"{}\">\n{body}\n</{tag}>",
        path.display()
    )
}

/// The `<project_memory>` section: the nearest `<project>/.yak/YAK.md`
/// walking up from `cwd` (bounded by the git root, like project docs),
/// absent entirely when the project carries none — a project without a
/// memory file spends no tokens on an empty shell.
pub fn project_section(cwd: &std::path::Path) -> Option<String> {
    let dir = crate::core::paths::nearest_dir_up(cwd, ".yak", true)?;
    let path = dir.join("YAK.md");
    if !path.is_file() {
        return None;
    }
    Some(section_tagged(&path, "project_memory"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_empty_file_still_names_the_path() {
        let dir = crate::core::testutil::scratch_dir("memory-empty");
        let file = dir.join("YAK.md");
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
        std::fs::write(&file, "first fact").unwrap();
        let s = section_at(&file);
        assert!(s.contains("first fact"), "{s}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn section_truncates_on_a_char_boundary() {
        let dir = crate::core::testutil::scratch_dir("memory-cap");
        let file = dir.join("YAK.md");
        std::fs::write(
            &file,
            format!("{}\u{4e2d}{}", "x".repeat(16 * 1024), "y".repeat(64)),
        )
        .unwrap();
        let s = section_at(&file);
        assert!(s.contains("<user_memory path="));
        assert!(s.contains("[truncated]"));
        assert!(s.len() < 16 * 1024 + 400);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
