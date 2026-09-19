//! System prompt assembly: the built-in instructions, project
//! CLAUDE.md/AGENTS.md discovery and today's date.

use std::path::Path;

/// First AGENTS.md/CLAUDE.md walking up from cwd, stopping at the git root.
/// AGENTS.md wins when both exist (the other agents default).
pub fn find_project_file(cwd: &Path) -> Option<std::path::PathBuf> {
    for d in crate::core::paths::ancestors(cwd) {
        for name in ["AGENTS.md", "CLAUDE.md"] {
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
        "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
        p.display(),
        text
    ))
}

/// Assemble the agent system prompt: built-ins plus project
/// CLAUDE.md/AGENTS.md discovery, skills, and global user memory.
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
            // already embeds memory/project/agents/skills from when the
            // session started, and re-appending them would grow the prompt
            // by a full copy on every resume (and shift it, busting the
            // provider prefix cache). Only the trailing cwd line refreshes.
            Some(s) => strip_cwd_line(s),
            None => {
                let date = crate::core::db::today();
                let ext_dir = crate::core::config::user_dir()
                    .join("extensions")
                    .display()
                    .to_string();
                let skills_dir = crate::core::config::user_dir()
                    .join("skills")
                    .display()
                    .to_string();
                format!(
                    "You are llm, a terminal coding agent. Lead with the outcome: act, then answer. \
                     NEVER keep exploring once the task is clear.\n\
                     \n\
                     Planning\n\
                     - Multi-step work (past the simplest 25%): track steps with `update_plan`, one \
                       `in_progress` at a time, and NEVER make a single-step plan.\n\
                     - Keep the plan current; it persists across turns. After `update_plan`, NEVER \
                       restate the plan — the UI shows it.\n\
                     \n\
                     Search & reading\n\
                     - Inspect with the tools: `grep` (text; `regex: true` for patterns), `glob` \
                       (files), `ls`, `read`. Keep `bash` for build, test and run.\n\
                     - Locate before reading: glob/ls → grep the symbol → read ONLY the relevant \
                       range. NEVER dump whole files or walk the tree.\n\
                     - Batch independent lookups as multiple tool calls in ONE message — they run \
                       concurrently. NEVER re-read or re-run what you already have this task.\n\
                     \n\
                     Changing files\n\
                     - Use `edit` (surgical diff) over `write`; NEVER rewrite a whole file for a \
                       targeted hunk; NEVER shell-pipeline an edit.\n\
                     - NEVER revert changes you did not make — work around them.\n\
                     \n\
                     Extending yourself\n\
                     - Add tools by writing into {ext_dir} (or the project's nearest \
                       .llm/extensions): a `# --- llm-tool: <name>` header (`description:`, \
                       `args: name (type) desc`, `interpreter: python3`) makes any script a tool; \
                       an executable speaking line-delimited JSON on stdio becomes a resident \
                       extension (tools, commands, event hooks). Restart llm or type /reload \
                       to pick a new one up.\n\
                     - A directory with SKILL.md under {skills_dir} (or the project's .llm/skills) \
                       publishes /skill:<name>.\n\
                     - MCP servers are not built in: mount them with a resident extension that \
                       speaks MCP (JSON-RPC 2.0 over stdio or streamable HTTP) and adds one \
                       `<server>__<tool>` tool per MCP tool, reading a config file beside itself; \
                       write it yourself, or start from the bridge named under Reference docs.\n\
                     Reference docs (llm's own; base \
                     raw.githubusercontent.com/imjiaoyuan/llm/main/)\n\
                     - Read these only when the user asks about llm itself (extensions, MCP, \
                       skills, memory, config, internals); in a checkout of the llm repo read the \
                       local files instead.\n\
                     - docs/extensions.md — the extension protocol: manifest fields, every message \
                       and event, the `tool_call` gate, timeouts.\n\
                     - examples/extensions/mcp_bridge.py — MCP servers mounted as \
                       `<server>__<tool>` tools.\n\
                     - docs/architecture.md — request flow, the agent loop, tool and approval \
                       rules, the rendering contract.\n\
                     \n\
                     Answering\n\
                     - Be concise; mirror the user's language and tone. NEVER restate the request \
                       or repeat an earlier turn — say only what is new.\n\
                     - Reference files by path:line, not by dumping contents.\n\
                     - When done, stop and give the result; NEVER run more commands to \"confirm\".\n\
                     \n\
                     Memory\n\
                     - Durable user preferences → the `<user_memory>` file below, one line each. It \
                       is plain markdown the user owns and reviews; the next session reads it.\n\
                     - Repo or directory rules → a project AGENTS.md, never global memory.\n\
                     - NEVER record secrets, tokens or credentials.\n\
                     \n\
                     Today's date: {date}"
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
        out.push_str("\n\n");
        out.push_str(&environment_line(cwd));
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

/// One stable line of environment facts: platform, shell, VCS, project
/// type with its verify command. Keeps the model from guessing
/// (PowerShell-isms on Linux, npm test in a cargo project, ...). Built
/// once per session, so the system prompt stays byte-identical across
/// rounds (the prefix cache depends on it).
fn environment_line(cwd: &Path) -> String {
    let mut parts = vec![std::env::consts::OS.to_string()];
    parts.push(crate::platform::shell_spec().program);
    if crate::core::paths::nearest_dir_up(cwd, ".git", true).is_some() {
        parts.push("git repository".to_string());
    }
    let hints: &[(&str, &str)] = &[
        ("Cargo.toml", "Rust project — verify with `cargo test`"),
        ("go.mod", "go project — verify with `go test ./...`"),
        ("package.json", "node project — verify with `npm test`"),
        ("pyproject.toml", "python project — verify with `pytest`"),
        ("requirements.txt", "python project — verify with `pytest`"),
    ];
    for (marker, hint) in hints {
        if cwd.join(marker).is_file() {
            parts.push((*hint).to_string());
            break;
        }
    }
    format!("Environment: {}.", parts.join(" · "))
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
    fn reference_docs_block_points_at_the_repo_files() {
        let dir = crate::core::testutil::scratch_dir("refdocs");
        let out = build_system_prompt(&dir, None, None, None, &[]).unwrap();
        for needle in [
            "Reference docs",
            "raw.githubusercontent.com/imjiaoyuan/llm/main/",
            "docs/extensions.md",
            "examples/extensions/mcp_bridge.py",
            "docs/architecture.md",
        ] {
            assert!(out.contains(needle), "missing {needle}: {out}");
        }
    }

    #[test]
    fn fresh_prompt_carries_the_environment_line() {
        let dir = crate::core::testutil::scratch_dir("env");
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        let out = build_system_prompt(&dir, None, None, None, &[]).unwrap();
        assert!(out.contains("Environment: "), "{}", out);
        // the memory block must be unconditional: it is the only place the
        // built-in prompt can point the agent at the file to edit
        assert!(
            out.contains("<user_memory path="),
            "the memory path must survive an empty/missing file: {out}"
        );
        assert!(out.contains("LLM.md"), "{out}");
        assert!(
            out.contains("\nMemory\n"),
            "the memory section must survive the compressed prompt: {out}"
        );
        assert!(
            out.contains("cargo test`"),
            "a cargo project names its verify command: {out}"
        );
        assert!(
            out.contains("Batch independent lookups"),
            "the prompt must ask the model to batch independent reads: {out}"
        );
        assert!(
            out.contains("restate the plan"),
            "the prompt must keep the model from re-narrating its plan: {out}"
        );
        assert!(
            out.contains("NEVER restate the request"),
            "the prompt must discourage repeating earlier turns: {out}"
        );
        assert!(
            !out.contains("act, verify, then"),
            "the old default-verify wording must be gone: {out}"
        );
        // stable across builds in the same directory (the prefix cache)
        let again = build_system_prompt(&dir, None, None, None, &[]).unwrap();
        assert_eq!(out, again);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Project instructions ride the prompt whole, however long the file is.
    /// Capping them was tried and reverted: the tail of a real AGENTS.md is
    /// where the "do not reintroduce X" / "do not fix Y" rules live, the
    /// model cannot know what it was not shown, and the "read the file for
    /// the rest" hint is no help against a gap it cannot perceive. The fixed
    /// prefix is also cache-read after the first round, so the saving was a
    /// fraction of what a naive all-tokens-priced estimate suggested.
    #[test]
    fn project_instructions_are_embedded_whole_at_any_length() {
        let dir = crate::core::testutil::scratch_dir("whole");
        // comfortably past any cap worth arguing about, with a marker sentence
        // at the very end: it is the part a head-cut would have eaten
        let mut file = String::from("# Project\n\n");
        for i in 0..800 {
            file.push_str(&format!("- rule {i}: keep this\n"));
        }
        file.push_str("\n## Project state\n\n- do not reintroduce the logs.db store\n");
        assert!(file.len() > 16 * 1024, "the fixture must be genuinely long");
        std::fs::write(dir.join("AGENTS.md"), &file).unwrap();

        let out = build_system_prompt(&dir, None, None, None, &[]).unwrap();
        assert!(
            out.contains("<project_instructions path="),
            "the block must still be there"
        );
        // every line survives, and the last one in particular
        assert!(out.contains("- rule 0: keep this"), "the head is dropped");
        assert!(
            out.contains("- rule 799: keep this"),
            "the middle is dropped"
        );
        assert!(
            out.contains("do not reintroduce the logs.db store"),
            "the tail is the part that matters most and must never be cut"
        );
        assert!(
            !out.contains("[Truncated"),
            "no truncation marker may reappear: this file is embedded whole"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
