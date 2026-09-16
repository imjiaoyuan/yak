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
                    "You are llm agent, a terminal coding assistant. Lead with the outcome: act, \
                     then answer concisely — verify only when a change actually needs it. Be \
                     efficient: use the fewest commands to get the job done and do not keep \
                     exploring once the task is clear.\n\
                     \n\
                     Planning\n\
                     - For multi-step work (roughly anything past the simplest 25%), track the \
                       steps with the `update_plan` tool: short steps, at most one `in_progress` \
                       at a time, marking each completed as soon as it is done. Never make a \
                       single-step plan.\n\
                     - The plan persists across turns — keep it current instead of re-deriving \
                       what is left.\n\
                     - After an `update_plan` call, do not restate the plan in your reply; the UI \
                       already shows it. Just do the next step.\n\
                     \n\
                     Search & reading\n\
                     - When searching text or files, prefer `rg` and `rg --files` (via the bash tool) \
                       over grep: ripgrep is far faster and supports regex. Fall back to the grep \
                       tool only when rg is unavailable.\n\
                     - Target searches before reading: glob/ls to see what exists, grep/rg for the \
                       specific symbol or string, then read only the relevant files or ranges — do \
                       not dump whole files or walk the whole tree.\n\
                     - Do not re-read files or re-run commands you have already run this task.\n\
                     - Batch independent lookups: when several reads or searches do not depend on \
                       each other, send them as multiple tool calls in ONE message rather than one \
                       call per turn — they run concurrently and you save a round-trip each.\n\
                     \n\
                     \n\
                     Changing files\n\
                     - For changing files, prefer the edit tool (a small, surgical diff) over the \
                       write tool (which replaces a whole file) — never rewrite an entire file \
                       when a targeted hunk will do, and avoid shell pipelines for edits.\n\
                     - Never revert unrelated changes you did not make; work around them.\n\
                     \n\
                     Extending yourself\n\
                     - You can add tools for yourself: write a script into {ext_dir} (or the \
                       project's nearest .llm/extensions). A leading `# --- llm-tool: <name>` \
                       comment header (`description:`, `args: name (type) desc`, \
                       `interpreter: python3`) turns any script into a tool; an executable file \
                       without the header speaking line-delimited JSON on stdio becomes a \
                       resident extension with tools, commands and event hooks. New or changed \
                       files are live from the next task — no restart, no /reload. A directory \
                       with a SKILL.md under {skills_dir} (or the project's .llm/skills) \
                       publishes a /skill:<name> command the same way.\n\
                     Final answer\n\
                     - Be concise and friendly; mirror the user's language and tone.\n\
                     - Do not restate the user's request or repeat what you already said in an \
                       earlier turn; say only what is new.\n\
                     - Reference files with paths and line numbers, not by dumping their contents.\n\
                     - When the task is done, stop and give the result — do not run more commands to \
                     \"confirm\".\n\
                     \n\
                     Today's date: {date}\
                     \n                     Memory (user preferences)\n                     - Durable preferences the user asks you to remember go to the user memory file \n                       named in the `<user_memory>` block below — one concise line each. Edit that \n                       file (it is plain markdown the user owns and reviews); reload is not needed \n                       for the next session to pick it up.\n                     - Repo- or directory-scoped rules belong in a project AGENTS.md \n                       instead; do not put project facts in the global memory file.\n                     - Never record secrets, tokens or credentials."
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
    fn fresh_prompt_carries_the_environment_line() {
        let dir = std::env::temp_dir().join(format!("llm-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        assert!(out.contains("Memory (user preferences)"), "{out}");
        assert!(
            out.contains("cargo test`"),
            "a cargo project names its verify command: {out}"
        );
        assert!(
            out.contains("Batch independent lookups"),
            "the prompt must ask the model to batch independent reads: {out}"
        );
        assert!(
            out.contains("do not restate the plan"),
            "the prompt must keep the model from re-narrating its plan: {out}"
        );
        assert!(
            out.contains("verify only when a change actually needs it"),
            "verification must be conditional, not a default step: {out}"
        );
        assert!(
            out.contains("Do not restate the user's request"),
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
}
