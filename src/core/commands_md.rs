//! User commands: `~/.llm/commands/*.md` (plus the nearest project
//! `.llm/commands/`, which wins) expanding a REPL `/name [args...]` into a
//! prompt-template invocation — a declarative subcommand with no code.
//! Markdown frontmatter carries `system` (the body is the prompt, and
//! `$input` receives the trailing args).

use crate::core::config::user_dir;

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

/// Look up one command by name: nearest `.llm/commands/<name>.md` walking
/// up from `cwd`, then `user_dir/commands/<name>.md`. A path probe, not a
/// directory walk, so the bare-prompt fast path never pays for it.
pub fn find(name: &str) -> Option<CommandMd> {
    if !valid_name(name) {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let file = format!("{name}.md");
    for dir in crate::commands::pkg::command_dirs(true)
        .into_iter()
        .chain(crate::commands::pkg::command_dirs(false))
    {
        if let Ok(text) = std::fs::read_to_string(dir.join(&file)) {
            return Some(parse(&text));
        }
    }
    for d in crate::core::paths::ancestors(&cwd) {
        let candidate = d.join(".llm/commands").join(&file);
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            return Some(parse(&text));
        }
    }
    std::fs::read_to_string(user_dir().join("commands").join(&file))
        .ok()
        .map(|text| parse(&text))
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
    fn frontmatter_system_reaches_the_expansion() {
        let cmd = parse("---\nsystem: Be terse about $input\n---\nReview $input");
        let (prompt, system) = expand(&cmd, "the diff");
        assert_eq!(prompt, "Review the diff");
        assert_eq!(system.as_deref(), Some("Be terse about the diff"));
        // and it survives an empty input too
        assert_eq!(expand(&cmd, "").1.as_deref(), Some("Be terse about $input"));
    }
}
