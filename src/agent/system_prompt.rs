//! System prompt assembly: the built-in instructions (the shape), project
//! context discovery and the trailing cwd line.

use std::path::Path;

/// First AGENTS.override.md/AGENTS.md/CLAUDE.md walking up from cwd, stopping
/// at the git root. `AGENTS.override.md` wins (a local override), then
/// `AGENTS.md`, then `CLAUDE.md`; both title-cases are tried for each (the
/// candidate list).
pub fn find_project_file(cwd: &Path) -> Option<std::path::PathBuf> {
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
                return Some(p);
            }
        }
        if d.join(".git").exists() {
            return None;
        }
    }
    None
}

pub fn project_context(cwd: &Path) -> Option<String> {
    let p = find_project_file(cwd)?;
    let text = std::fs::read_to_string(&p).ok()?;
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
                let ext_dir = crate::core::config::user_dir()
                    .join("extensions")
                    .display()
                    .to_string();
                let skills_dir = crate::core::config::user_dir()
                    .join("skills")
                    .display()
                    .to_string();
                format!(
                    "You are an expert coding assistant operating inside yak, a coding agent harness. \
                     You help users by reading files, executing commands, editing code, and writing new files.\n\
                     \n\
                     Available tools:\n\
                     - read: Read file contents\n\
                     - write: Create or overwrite files\n\
                     - edit: Make precise file edits with exact text replacement\n\
                     - bash: Execute a shell command\n\
                     - grep: Search file contents for patterns (respects .gitignore)\n\
                     - glob: Find files by glob pattern (respects .gitignore)\n\
                     - ls: List directory contents\n\
                     - webfetch: Fetch a URL and return its text content\n\
                     - update_plan: Track a multi-step plan\n\
                     - remember: Save one durable fact to the user's global memory\n\
                     \n\
                     In addition to the tools above, you may have access to other custom tools \
                     depending on the project.\n\
                     \n\
                     Guidelines:\n\
                     - Use read to examine files instead of cat or sed.\n\
                     - Use edit for precise changes (edits[].oldText must match exactly).\n\
                     - When changing multiple separate locations in one file, use one edit call \
                       with multiple entries in edits[] instead of multiple edit calls.\n\
                     - Use write only for new files or complete rewrites.\n\
                     - Multi-step work (past the simplest 25%): track steps with `update_plan`, one \
                       `in_progress` at a time.\n\
                     - Keep the plan current; it persists across turns. After `update_plan`, NEVER \
                       restate the plan — the UI shows it.\n\
                     - Be concise in your responses.\n\
                     - Show file paths clearly when working with files.\n\
                     \n\
                     Memory (user preferences)\n\
                     - When the user asks you to remember or note a durable preference \n\
                       (an environment detail, a long-term decision, a way of working), call \n\
                       the `remember` tool with one concise line. It lands in the user memory \n\
                       file named in the `<user_memory>` block below and applies from the \n\
                       next session on; this session's prompt stays as it is.\n\
                     - Repo- or directory-scoped rules belong in a project AGENTS.md/CLAUDE.md \n\
                       instead; do not put project facts in the global memory file.\n\
                     - Never record secrets, tokens or credentials.\n\
                     \n\
                     Extending yourself\n\
                     - Add tools by writing into {ext_dir} (or the project's nearest \
                       .yak/extensions): a `# --- yak-tool: <name>` header (`description:`, \
                       `args: name (type) desc`, `interpreter: python3`) makes any script a tool; \
                       an executable speaking line-delimited JSON on stdio becomes a resident \
                       extension (tools, commands, event hooks). Restart yak or type /reload \
                       to pick a new one up.\n\
                     - A directory with SKILL.md under {skills_dir} (or the project's .yak/skills) \
                       publishes /skill:<name>.\n\
                     - MCP servers are not built in: mount them with a resident extension that \
                       speaks MCP (JSON-RPC 2.0 over stdio or streamable HTTP) and adds one \
                       `<server>__<tool>` tool per MCP tool, reading a config file beside itself; \
                       write it yourself, or start from the bridge named under Reference docs.\n\
                     \n\
                     Reference docs (yak's own; base raw.githubusercontent.com/imjiaoyuan/yak/main/)\n\
                     - Read these only when the user asks about yak itself (extensions, MCP, \
                       skills, config, internals); in a checkout of the yak repo read the local \
                       files instead.\n\
                     - docs/extensions.md — the extension protocol: manifest fields, every message \
                       and event, the `tool_call` gate, timeouts.\n\
                     - examples/extensions/mcp_bridge.py — MCP servers mounted as \
                       `<server>__<tool>` tools.\n\
                     - docs/architecture.md — request flow, the agent loop, tool and approval \
                       rules, the rendering contract."
                )
            }
        },
    };
    let mut out = base;
    if !continuation {
        out.push_str("\n\n");
        out.push_str(&crate::agent::memory::section());
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
            "Be concise in your responses",
            "Show file paths clearly when working with files",
            "Extending yourself",
            "Reference docs",
            "raw.githubusercontent.com/imjiaoyuan/yak/main/",
            "docs/extensions.md",
            "docs/architecture.md",
            "update_plan",
            "remember",
        ] {
            assert!(out.contains(needle), "missing {needle}: {out}");
        }
        // the prompt shape, plus the memory block yak keeps
        assert!(out.contains("<user_memory"), "{out}");
        assert!(!out.contains("Today's date"), "{out}");
        assert!(!out.contains("Environment: "), "{out}");
        // stable across builds in the same directory (the prefix cache)
        let again = build_system_prompt(&dir, None, None, None, &[]).unwrap();
        assert_eq!(out, again);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_context_is_wrapped_and_override_wins() {
        let dir = crate::core::testutil::scratch_dir("spctx");
        std::fs::write(dir.join("AGENTS.md"), "# base\n").unwrap();
        std::fs::write(dir.join("AGENTS.override.md"), "# override\n").unwrap();
        let p = find_project_file(&dir).unwrap();
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
