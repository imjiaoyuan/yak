//! System prompt assembly: the built-in instructions (the shape), project
//! context discovery and the trailing cwd line.

use std::path::Path;

/// First AGENTS.override.md/AGENTS.md/CLAUDE.md walking up from cwd, stopping
/// at the git root. `AGENTS.override.md` wins (a local override), then
/// `AGENTS.md`, then `CLAUDE.md`; both title-cases are tried for each (the
/// candidate list). Returns the path with the file's text — the read the
/// emptiness check already paid, so the caller never reads it twice.
fn find_project_doc(cwd: &Path) -> Option<(std::path::PathBuf, String)> {
    for d in crate::core::paths::ancestors(cwd) {
        for name in [
            "AGENTS.override.md",
            "AGENTS.md",
            "AGENTS.MD",
            "CLAUDE.md",
            "CLAUDE.MD",
        ] {
            let p = d.join(name);
            if p.is_file()
                && let Ok(text) = std::fs::read_to_string(&p)
                && !text.trim().is_empty()
            {
                return Some((p, text));
            }
        }
        if d.join(".git").exists() {
            return None;
        }
    }
    None
}

pub fn project_context(cwd: &Path) -> Option<String> {
    let (p, text) = find_project_doc(cwd)?;
    Some(format!(
        "<project_context>\n\nProject-specific instructions and guidelines:\n\n<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n</project_context>",
        p.display(),
        text
    ))
}

/// Assemble the agent system prompt: the reference-shaped built-ins, project
/// context discovery, skills, and the trailing cwd line. Continuations keep
/// the stored prompt verbatim so the provider prefix cache survives a resume.
pub fn build_system_prompt(
    cwd: &Path,
    replace: Option<&str>,
    append: Option<&str>,
    stored: Option<&str>,
    skills: &[crate::agent::skills::SkillDef],
) -> Option<String> {
    let continuation = replace.is_none() && stored.is_some();
    let base = match replace {
        Some(r) => r.to_string(),
        None => match stored {
            // a continuation keeps the stored assembled prompt verbatim: it
            // already embeds project context and skills from when the
            // session started. Only the trailing cwd line refreshes.
            Some(s) => strip_cwd_line(s),
            None => {
                let base = "You are an expert coding assistant operating inside yak, a plugin-based harness for LLMs. \
                           You help users by reading files, executing commands, editing code, and writing new files.\n\
                           \n\
                           Available tools:\n\
                           - read: Read file contents\n\
                           - write: Create or overwrite files\n\
                           - edit: Make precise file edits with exact text replacement\n\
                           - bash: Execute a shell command\n\
                           \n\
                           In addition to the tools above, you may have access to other custom tools \
                           depending on the project.\n\
                           \n\
                           Guidelines:\n\
                           - Use read to examine files instead of cat or sed.\n\
                           - Use bash for file operations like ls, rg, find.\n\
                           - Use edit for precise changes (edits[].oldText must match exactly).\n\
                           - When changing multiple separate locations in one file, use one edit call \
                             with multiple entries in edits[] instead of multiple edit calls.\n\
                           - Use write only for new files or complete rewrites.\n\
                           - Be concise in your responses.\n\
                           - Show file paths clearly when working with files.";
                base.to_string()
            }
        },
    };
    let mut out = base;
    if !continuation {
        out.push_str("\n\n");
        out.push_str(&crate::agent::memory::section());
        if let Some(pm) = crate::agent::memory::project_section(cwd) {
            out.push_str("\n\n");
            out.push_str(&pm);
        }
        if let Some(ctx) = project_context(cwd) {
            out.push_str("\n\n");
            out.push_str(&ctx);
        }
        if let Some(block) = crate::agent::skills::skills_block(skills) {
            out.push_str("\n\n");
            out.push_str(&block);
        }
    }
    if let Some(extra) = append {
        out.push_str("\n\n");
        out.push_str(extra);
    }
    out.push_str(&format!("\n\nCurrent working directory: {}", cwd.display()));
    Some(out)
}

/// Drop the trailing cwd line a previously assembled prompt ends with, so a
/// continuation refreshes it (the session may resume from another directory).
fn strip_cwd_line(s: &str) -> String {
    match s.rfind("\n\nCurrent working directory: ") {
        Some(i) => s[..i].to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuation_keeps_stored_prompt_and_refreshes_only_cwd() {
        let cwd = std::path::Path::new("/tmp/proj");
        let stored = "base\n\n<project_instructions path=\"/x\">\nnotes\n</project_instructions>\
                      \n\nCurrent working directory: /old/dir";
        let out = build_system_prompt(cwd, None, None, Some(stored), &[]).unwrap();
        assert_eq!(
            out,
            "base\n\n<project_instructions path=\"/x\">\nnotes\n</project_instructions>\
             \n\nCurrent working directory: /tmp/proj"
        );
        // resuming the resumed prompt must be a fixed point: no compounding
        let again = build_system_prompt(cwd, None, None, Some(&out), &[]).unwrap();
        assert_eq!(again, out);
    }

    #[test]
    fn fresh_prompt_is_shaped_and_carries_yak_additions() {
        let dir = crate::core::testutil::scratch_dir("spfresh");
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        let out = build_system_prompt(&dir, None, None, None, &[]).unwrap();
        for needle in [
            "You are an expert coding assistant operating inside yak",
            "Available tools:",
            "- read: Read file contents",
            "In addition to the tools above",
            "Guidelines:",
            "Use bash for file operations like ls, rg, find",
            "Be concise in your responses",
            "Show file paths clearly when working with files",
        ] {
            assert!(out.contains(needle), "missing {needle}: {out}");
        }
        // the slimmed base no longer carries the self-extension or
        // reference-doc blocks: the yak repo's own AGENTS.md is their home
        for gone in [
            "Extending yourself",
            "Reference docs",
            "ordinary extensions",
        ] {
            assert!(!out.contains(gone), "unexpected {gone}");
        }
        // the memory block still rides under the slimmed base
        assert!(out.contains("<user_memory"), "{out}");
        assert!(!out.contains("Today's date"), "{out}");
        assert!(!out.contains("Environment: "), "{out}");
        // stable across builds in the same directory (the prefix cache)
        let again = build_system_prompt(&dir, None, None, None, &[]).unwrap();
        assert_eq!(out, again);
        // a scratch dir without .yak/YAK.md injects no project memory; one
        // with the file does, wrapped in its own tag (the guideline text
        // mentions the tag name, so assert on the block opening itself)
        assert!(!out.contains("<project_memory path="), "{out}");
        std::fs::create_dir_all(dir.join(".yak")).unwrap();
        std::fs::write(dir.join(".yak/YAK.md"), "- never commit target/\n").unwrap();
        let with = build_system_prompt(&dir, None, None, None, &[]).unwrap();
        assert!(with.contains("<project_memory path="), "{with}");
        assert!(with.contains("never commit target/"), "{with}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_context_is_wrapped_and_override_wins() {
        let dir = crate::core::testutil::scratch_dir("spctx");
        std::fs::write(dir.join("AGENTS.md"), "# base\n").unwrap();
        std::fs::write(dir.join("AGENTS.override.md"), "# override\n").unwrap();
        let p = find_project_doc(&dir).map(|(p, _)| p).unwrap();
        assert_eq!(p.file_name().unwrap(), "AGENTS.override.md");
        let ctx = project_context(&dir).unwrap();
        assert!(ctx.starts_with("<project_context>"), "{ctx}");
        assert!(ctx.contains("Project-specific instructions and guidelines"));
        assert!(ctx.contains("# override"), "{ctx}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Project instructions ride the prompt whole, however long the file is.
    #[test]
    fn project_instructions_are_embedded_whole_at_any_length() {
        let dir = crate::core::testutil::scratch_dir("whole");
        let mut file = String::from("# Project\n\n");
        for i in 0..800 {
            file.push_str(&format!("- rule {i}: keep this\n"));
        }
        file.push_str("\n## Project state\n\n- do not reintroduce the logs.db store\n");
        std::fs::write(dir.join("AGENTS.md"), &file).unwrap();

        let out = build_system_prompt(&dir, None, None, None, &[]).unwrap();
        assert!(out.contains("<project_instructions path="));
        assert!(out.contains("- rule 0: keep this"));
        assert!(out.contains("- rule 799: keep this"));
        assert!(out.contains("do not reintroduce the logs.db store"));
        assert!(!out.contains("[Truncated"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
