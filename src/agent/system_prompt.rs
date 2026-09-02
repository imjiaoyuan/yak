//! System prompt assembly: the built-in instructions, project
//! CLAUDE.md/AGENTS.md discovery and today's date.

use std::path::Path;

/// First AGENTS.md/CLAUDE.md walking up from cwd, stopping at the git root.
/// AGENTS.md wins when both exist (the other agents default).
pub fn find_project_file(cwd: &Path) -> Option<std::path::PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
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
        dir = d.parent();
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

pub fn build_system_prompt(
    cwd: &Path,
    replace: Option<&str>,
    append: Option<&str>,
    stored: Option<&str>,
    agents: &[crate::agent::task::AgentDef],
    skills: &[crate::agent::skills::SkillDef],
) -> Option<String> {
    let base = match replace {
        Some(r) => r.to_string(),
        None => match stored {
            // continuation keeps the original prompt; project context may
            // have shifted with cwd, so re-derive everything below it
            Some(s) => s.to_string(),
            None => {
                let date = today();
                format!(
                    "You are llm agent, a terminal coding assistant. Lead with the outcome: act, \
                     verify, then answer concisely. Be efficient — use the fewest commands to get \
                     the job done and do not keep exploring once the task is clear.\n\
                     \n\
                     Search & reading\n\
                     - When searching text or files, prefer `rg` and `rg --files` (via the bash tool) \
                       over grep: ripgrep is far faster and supports regex. Fall back to the grep \
                       tool only when rg is unavailable.\n\
                     - Target searches before reading: glob/ls to see what exists, grep/rg for the \
                       specific symbol or string, then read only the relevant files or ranges — do \
                       not dump whole files or walk the whole tree.\n\
                     - Do not re-read files or re-run commands you have already run this task.\n\
                     \n\
                     Planning\n\
                     - For a multi-step task, call `update_plan` with a short list of steps (each a \
                       few words) and a status (pending / in_progress / completed); update it after \
                       each step you finish. Skip planning for trivial one-liners.\n\
                     \n\
                     Changing files\n\
                     - For changing files, prefer the edit tool (a small, surgical diff) over the \
                       write tool (which replaces a whole file) — never rewrite an entire file \
                       when a targeted hunk will do, and avoid shell pipelines for edits.\n\
                     - Never revert unrelated changes you did not make; work around them.\n\
                     \n\
                     Final answer\n\
                     - Be concise and friendly; mirror the user's language and tone.\n\
                     - Reference files with paths and line numbers, not by dumping their contents.\n\
                     - When the task is done, stop and give the result — do not run more commands to \
                       \"confirm\".\n\
                     \n\
                     Today's date: {date}"
                )
            }
        },
    };
    let mut out = base;
    if let Some(mem) = crate::agent::memory::section() {
        out.push_str("\n\n");
        out.push_str(&mem);
    }
    if let Some(ctx) = project_context(cwd) {
        out.push_str("\n\n");
        out.push_str(&ctx);
    }
    if !agents.is_empty() {
        out.push_str("\n\nAvailable sub-agents (delegate via the task tool):");
        for a in agents {
            if a.description.is_empty() {
                out.push_str(&format!("\n- {}", a.name));
            } else {
                out.push_str(&format!("\n- {}: {}", a.name, a.description));
            }
        }
    }
    if let Some(block) = crate::agent::skills::skills_block(skills) {
        out.push_str("\n\n");
        out.push_str(&block);
    }
    if let Some(extra) = append {
        out.push_str("\n\n");
        out.push_str(extra);
    }
    out.push_str(&format!("\n\nCurrent working directory: {}", cwd.display()));
    Some(out)
}

fn today() -> String {
    crate::core::db::now_turn_datetime()
        .split('T')
        .next()
        .unwrap_or("")
        .to_string()
}
