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
        clip_project_text(&text)
    ))
}

/// Cap on the embedded project instructions, in bytes of file text. A repo's
/// AGENTS.md/CLAUDE.md rides the system prompt verbatim, which makes it the
/// largest fixed cost of a session: in this repo the file is 14.5KB of a 17KB
/// assembled prompt, and the whole thing is re-sent (or cache-read) on every
/// round. Past the cap the head is kept and the rest stays on disk, named by
/// the block's own `path`, so the model reads the range it needs instead of
/// paying for every line up front.
const PROJECT_INSTRUCTIONS_MAX_BYTES: usize = 8 * 1024;

/// Clip file text to [`PROJECT_INSTRUCTIONS_MAX_BYTES`] on a line boundary
/// (so a markdown section is never cut mid-sentence) and name the shortfall.
/// Whole lines also keep the cut from orphaning a heading from its body.
fn clip_project_text(text: &str) -> String {
    if text.len() <= PROJECT_INSTRUCTIONS_MAX_BYTES {
        return text.to_string();
    }
    let cap = crate::core::text::floor_boundary(text, PROJECT_INSTRUCTIONS_MAX_BYTES);
    // back up to the last newline so the cut lands between lines; a file with
    // no newline in the whole head just cuts at the char boundary
    let end = match text[..cap].rfind('\n') {
        Some(i) if i > 0 => i,
        _ => cap,
    };
    format!(
        "{}\n\n[Truncated: showing the first {} of {} bytes. Use the `read` tool on the file \
         named in this block's `path` for the rest.]",
        &text[..end],
        end,
        text.len()
    )
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
    fn project_instructions_are_capped_on_a_line_boundary() {
        // small enough: untouched, no marker
        let small = "# Notes\n\nshort";
        assert_eq!(clip_project_text(small), small);
        // exactly at the cap: still untouched
        let exact = "x".repeat(PROJECT_INSTRUCTIONS_MAX_BYTES);
        assert_eq!(clip_project_text(&exact), exact);

        // oversize: the head survives, the cut lands between lines, and the
        // model is told how to get the rest
        let mut big = String::new();
        for i in 0..2_000 {
            big.push_str(&format!("## section {i}\nbody line for {i}\n"));
        }
        assert!(big.len() > PROJECT_INSTRUCTIONS_MAX_BYTES);
        let clipped = clip_project_text(&big);
        assert!(clipped.starts_with("## section 0\nbody line for 0\n"));
        assert!(clipped.len() < big.len());
        assert!(clipped.contains("[Truncated: showing the first"));
        assert!(clipped.contains(&format!("of {} bytes", big.len())));
        assert!(clipped.contains("`read` tool"));
        // a section heading is never separated from nothing: the kept part
        // ends on a complete line
        let kept = clipped.split("\n\n[Truncated").next().unwrap();
        assert!(
            !kept.ends_with('\n'),
            "the trailing newline is the boundary"
        );
        assert!(kept.lines().all(|l| !l.is_empty()));

        // CJK: the cut is a byte cap but must never split a codepoint
        let cjk = "中".repeat(PROJECT_INSTRUCTIONS_MAX_BYTES);
        let clipped = clip_project_text(&cjk);
        let kept = clipped.split("\n\n[Truncated").next().unwrap();
        assert!(kept.chars().all(|c| c == '中'));
        // no newline anywhere: falls back to the char boundary
        assert!(kept.len() <= PROJECT_INSTRUCTIONS_MAX_BYTES);
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
}
