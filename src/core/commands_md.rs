//! User commands: `~/.yak/commands/*.md` (plus the nearest project
//! `.yak/commands/`, which wins) expanding a REPL `/name [args...]` into a
//! prompt-template invocation — a declarative subcommand with no code.
//! Markdown frontmatter carries `system` (the body is the prompt, and
//! `$input` receives the trailing args).

use crate::core::config::user_dir;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct CommandMd {
    pub system: Option<String>,
    pub body: String,
}

/// Parse one command file. A file without frontmatter is still a command
/// (whole body is the prompt); frontmatter the YAML subset cannot parse
/// degrades to empty metadata — never lose a usable command.
pub fn parse(text: &str) -> CommandMd {
    let mut system = None;
    let mut body = text.trim().to_string();
    if let Some((fm, after)) = crate::yaml::split_frontmatter(text) {
        if let Ok(map) = crate::yaml::parse(fm) {
            system = map.get("system").filter(|s| !s.is_empty()).cloned();
        }
        body = after.trim_start_matches('\n').trim().to_string();
    }
    CommandMd { system, body }
}

/// Command names share the plugin-name rules; rejecting anything else is
/// also the traversal guard (no `/`, no `..`, no leading `-`).
pub fn valid_name(name: &str) -> bool {
    crate::core::text::valid_plugin_name(name, 64) && !name.starts_with('-')
}

/// Every directory a command file can live in, in lookup order: package
/// command dirs (project pkg before user pkg), the nearest
/// `.yak/commands`/`.llm/commands` walking up from `cwd`, then the user
/// dir. Shared by `find` (a probe) and `names` (an enumeration) so the two
/// can never disagree about where a command lives.
fn search_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    out.extend(crate::commands::pkg::command_dirs(true));
    out.extend(crate::commands::pkg::command_dirs(false));
    if let Ok(cwd) = std::env::current_dir() {
        for d in crate::core::paths::ancestors(&cwd) {
            for proj in [".yak/commands", ".llm/commands"] {
                out.push(d.join(proj));
            }
        }
    }
    out.push(user_dir().join("commands"));
    out
}

/// Look up one command by name: the first `search_dirs()` entry holding
/// `<name>.md`. A path probe, not a directory walk, so the bare-prompt fast
/// path never pays for it.
pub fn find(name: &str) -> Option<CommandMd> {
    if !valid_name(name) {
        return None;
    }
    let file = format!("{name}.md");
    for dir in search_dirs() {
        if let Ok(text) = std::fs::read_to_string(dir.join(&file)) {
            return Some(parse(&text));
        }
    }
    None
}

/// Every command name visible from `cwd`, deduped and sorted. The REPL
/// discovers a command file lazily at submit time, but the *name* has to be
/// known up front or the editor cannot highlight or complete it.
pub fn names() -> Vec<String> {
    names_from(&search_dirs())
}

fn names_from(dirs: &[PathBuf]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for dir in dirs {
        for name in dir_names(dir) {
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

/// The valid `.md` stems in one directory, name rules applied (so a stray
/// `../` or a leading `-` never reaches the completer).
fn dir_names(dir: &std::path::Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if valid_name(stem) {
            out.push(stem.to_string());
        }
    }
    out
}

/// Build the substitution template from a command file: the body is the
/// prompt, frontmatter supplies the system prompt.
pub fn template(cmd: &CommandMd) -> crate::core::templates::Template {
    crate::core::templates::Template {
        prompt: Some(cmd.body.clone()),
        system: cmd.system.clone(),
    }
}

/// Expand a command into a ready prompt and its system prompt (used by the
/// REPL): `$input` substitution, args appended when the body has no
/// `$input`. The system prompt is substituted too, so it may reference
/// `$input` as well.
pub fn expand(cmd: &CommandMd, input: &str) -> (String, Option<String>) {
    // an empty input sends both the body and the system verbatim rather
    // than substituting an `$input` that was never supplied
    if input.trim().is_empty() {
        return (cmd.body.clone(), cmd.system.clone());
    }
    let (prompt, system) =
        crate::core::templates::apply(&template(cmd), input, &std::collections::BTreeMap::new());
    (prompt.unwrap_or_else(|| cmd.body.clone()), system)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let text = "---\nmodel: mock/big\nsystem: Be terse\n---\nReview this: $input";
        let cmd = parse(text);
        assert_eq!(cmd.system.as_deref(), Some("Be terse"));
        assert_eq!(cmd.body, "Review this: $input");
    }

    #[test]
    fn body_without_frontmatter_is_still_a_command() {
        let cmd = parse("Just a prompt body");
        assert_eq!(cmd.body, "Just a prompt body");
        assert!(cmd.system.is_none());
    }

    #[test]
    fn unparseable_frontmatter_degrades_to_empty_metadata() {
        let cmd = parse("---\n: : bad yaml [\n---\nbody here");
        assert_eq!(cmd.body, "body here");
        assert!(cmd.system.is_none());
    }

    #[test]
    fn valid_name_rejects_traversal_and_flags() {
        assert!(valid_name("review"));
        assert!(valid_name("code_review-2"));
        assert!(!valid_name("../etc/passwd"));
        assert!(!valid_name("-m"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name(""));
    }

    #[test]
    fn expand_substitutes_appends_or_uses_bare_body() {
        let cmd = parse("Review: $input");
        assert_eq!(expand(&cmd, "the diff").0, "Review: the diff");
        let plain = parse("Summarize the code");
        // apply() appends the input when the body lacks $input
        assert!(expand(&plain, "now").0.contains("Summarize the code"));
        // empty input sends the body as-is
        assert_eq!(expand(&cmd, "").0, "Review: $input");
    }

    #[test]
    fn a_stray_variable_does_not_abandon_the_input_substitution() {
        // a body quoting the shell must still expand its `$input`; the
        // unbound name is what it is, not a hard error
        let cmd = parse("Explain $HOME and then: $input");
        let (prompt, _) = expand(&cmd, "the diff");
        assert!(prompt.ends_with("the diff"), "{prompt}");
        assert!(prompt.contains("$HOME"), "{prompt}");
    }

    #[test]
    fn dir_names_lists_only_valid_markdown_stems() {
        let dir = crate::core::testutil::scratch_dir("cmd-names");
        std::fs::write(dir.join("review.md"), "x").unwrap();
        std::fs::write(dir.join("init.md"), "x").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::fs::write(dir.join("-flag.md"), "x").unwrap();
        std::fs::create_dir(dir.join("sub.md")).unwrap();
        let mut got = dir_names(&dir);
        got.sort();
        assert_eq!(got, vec!["init", "review"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_collects_and_dedupes_across_dirs() {
        let root = crate::core::testutil::scratch_dir("cmd-visible");
        // the local project dir wins by search order, so its copy shadows the
        // user one; the union still surfaces both distinct names
        let project = root.join("proj/commands");
        let user = root.join("user/commands");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(project.join("init.md"), "x").unwrap();
        std::fs::write(project.join("review.md"), "x").unwrap();
        std::fs::write(user.join("init.md"), "x").unwrap();
        std::fs::write(user.join("deploy.md"), "x").unwrap();
        let got = names_from(&[project, user]);
        assert_eq!(got, vec!["deploy", "init", "review"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn frontmatter_system_reaches_the_expansion() {
        let cmd = parse("---\nsystem: Be terse about $input\n---\nReview $input");
        let (prompt, system) = expand(&cmd, "the diff");
        assert_eq!(prompt, "Review the diff");
        assert_eq!(system.as_deref(), Some("Be terse about the diff"));
        // and it survives an empty input too
        assert_eq!(expand(&cmd, "").1.as_deref(), Some("Be terse about $input"));
    }
}
