//! Global user memory: a single hand-editable `~/.yak/YAK.md` injected into
//! the agent system prompt. The `remember` tool appends one dated line when
//! the user asks to note something down; everything else is hand-edited by
//! the user who owns the file.

use std::path::PathBuf;

/// cap on the injected section (bytes), char-boundary safe
const SECTION_CAP: usize = 16 * 1024;

pub fn memory_path() -> PathBuf {
    crate::core::config::user_dir().join("YAK.md")
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
    format!("<{tag} path=\"{}\">\n{body}\n</{tag}>", path.display())
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

/// The agent-facing `remember`: one dated line appended to the memory file,
/// deduped against what is already noted (containment either way,
/// case-insensitive). `Ok(true)` means the line landed, `Ok(false)` that it
/// was already there or empty.
#[cfg(test)]
fn remember_at(path: &std::path::Path, line: &str) -> Result<bool, String> {
    let line = line.trim().trim_matches(['"', '.']);
    if line.is_empty() {
        return Ok(false);
    }
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    let lower = line.to_lowercase();
    let dup = text
        .lines()
        .map(str::to_lowercase)
        .any(|l| l.contains(&lower) || lower.contains(&l));
    if dup {
        return Ok(false);
    }
    // a hand-edited file may lack the trailing newline; never glue lines
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    let today = crate::core::db::now_turn_datetime();
    let today = today.get(..10).unwrap_or("");
    text.push_str(&format!("- [{today}] {line}\n"));
    std::fs::create_dir_all(path.parent().unwrap_or(path)).map_err(|e| e.to_string())?;
    std::fs::write(path, text)
        .map_err(|e| e.to_string())
        .map(|_| true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_appends_one_dated_line() {
        let dir = crate::core::testutil::scratch_dir("memory-add");
        let file = dir.join("YAK.md");
        // no trailing newline: the new line must not glue onto it
        std::fs::write(&file, "- [2026-01-01] likes concise replies").unwrap();
        assert!(remember_at(&file, "prefers vim over emacs").unwrap());
        let out = std::fs::read_to_string(&file).unwrap();
        let today = crate::core::db::now_turn_datetime();
        let today = &today[..10];
        assert_eq!(
            out,
            format!("- [2026-01-01] likes concise replies\n- [{today}] prefers vim over emacs\n")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remember_dedups_by_containment_either_way() {
        let dir = crate::core::testutil::scratch_dir("memory-dup");
        let file = dir.join("YAK.md");
        std::fs::write(&file, "- [2026-01-01] likes concise replies\n").unwrap();
        // a noted line contains the new one (case-insensitive)
        assert!(!remember_at(&file, "LIKES CONCISE REPLIES").unwrap());
        // the new one contains a noted line
        assert!(!remember_at(&file, "likes concise").unwrap());
        // whitespace-only is a no-op, quotes and dots trimmed
        assert!(!remember_at(&file, "  ").unwrap());
        let before = std::fs::read_to_string(&file).unwrap();
        assert!(remember_at(&file, "\"prefers teal.\"\n").unwrap());
        let after = std::fs::read_to_string(&file).unwrap();
        let today = &crate::core::db::now_turn_datetime()[..10];
        assert!(
            after.contains(&format!("- [{today}] prefers teal\n")),
            "{after}"
        );
        assert!(after.len() > before.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        // and remember bootstraps the file itself
        assert!(remember_at(&file, "first fact").unwrap());
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
        assert!(s.starts_with("<user_memory path="));
        assert!(s.contains("[truncated]"));
        assert!(s.len() < 16 * 1024 + 200);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
