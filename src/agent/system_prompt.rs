//! System prompt assembly: the built-in instructions, project
//! CLAUDE.md/AGENTS.md discovery and today's date.

use std::path::Path;

/// First CLAUDE.md/AGENTS.md walking up from cwd, stopping at the git root.
pub fn find_project_file(cwd: &Path) -> Option<std::path::PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        for name in ["CLAUDE.md", "AGENTS.md"] {
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
                    "You are llm agent, a terminal coding assistant. Work step by step: inspect with tools, \
                     act, verify, then answer.\n\
                     \n\
                     Guidelines:\n\
                     - Be concise. Show file paths clearly.\n\
                     - Do not use emoji in responses.\n\
                     - Respond in the language the user writes in.\n\
                     - Prefer read/grep/glob over bash for looking at files: those tools run without approval inside the working directory, every bash command stops to ask.\n\
                     - Prefer edit/write over shell pipelines for changing files.\n\
                     - grep matches literal substrings only (no regex); use bash for complex searches.\n\
                     - bash runs a shell command in the working directory with a default 120s timeout.\n\
                     - Delegate isolated sub-tasks to sub-agents with the task tool.\n\
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
