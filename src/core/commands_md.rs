//! User commands: `~/.llm/commands/*.md` (plus the nearest project
//! `.llm/commands/`, which wins) turning `llm <name> [args...]` into a
//! prompt-template invocation — a declarative subcommand with no code.
//! Markdown frontmatter (`model`, `system`, `attachments`,
//! `attachment_types`) plus the body as the prompt; `$input` receives the
//! trailing args.

use crate::core::config::user_dir;

#[derive(Clone, Debug)]
pub struct CommandMd {
    pub model: Option<String>,
    pub system: Option<String>,
    /// files/URLs attached ahead of any -a entries on every run
    pub attachments: Vec<String>,
    /// explicit path → mimetype map for the attachments above
    pub attachment_types: std::collections::BTreeMap<String, String>,
    pub body: String,
}

/// Parse one command file. A file without frontmatter is still a command
/// (whole body is the prompt); frontmatter the YAML subset cannot parse
/// degrades to empty metadata — never lose a usable command.
pub fn parse(text: &str) -> CommandMd {
    let mut model = None;
    let mut system = None;
    let mut attachments = Vec::new();
    let mut attachment_types = std::collections::BTreeMap::new();
    let mut body = text.trim().to_string();
    if let Some((fm, after)) = crate::yaml::split_frontmatter(text) {
        if let Ok(y) = crate::yaml::parse(fm) {
            if let Some(map) = y.as_map() {
                model = map.get("model").filter(|m| !m.is_empty()).cloned();
                system = map.get("system").filter(|s| !s.is_empty()).cloned();
            }
            attachments = y
                .get("attachments")
                .and_then(|v| v.as_str_list())
                .unwrap_or_default();
            if let Some(types) = y.get("attachment_types").and_then(|v| v.as_map()) {
                attachment_types = types;
            }
        }
        body = after.trim_start_matches('\n').trim().to_string();
    }
    CommandMd {
        model,
        system,
        attachments,
        attachment_types,
        body,
    }
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
    let mut dir = Some(cwd.as_path());
    while let Some(d) = dir {
        let candidate = d.join(".llm/commands").join(&file);
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            return Some(parse(&text));
        }
        dir = d.parent();
    }
    std::fs::read_to_string(user_dir().join("commands").join(&file))
        .ok()
        .map(|text| parse(&text))
}

/// Build the agent-tool-style template from a command file: the body is the
/// prompt, frontmatter supplies model/system.
pub fn template(cmd: &CommandMd) -> crate::core::templates::Template {
    crate::core::templates::Template {
        prompt: Some(cmd.body.clone()),
        system: cmd.system.clone(),
        model: cmd.model.clone(),
        attachments: cmd.attachments.clone(),
        attachment_types: cmd
            .attachment_types
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        ..Default::default()
    }
}

/// Expand a command into a ready prompt (used by the REPLs): `$input`
/// substitution, args appended when the body has no `$input`.
pub fn expand(cmd: &CommandMd, input: &str) -> String {
    if input.trim().is_empty() {
        return cmd.body.clone();
    }
    match crate::core::templates::apply(&template(cmd), input, &std::collections::BTreeMap::new()) {
        Ok((Some(prompt), _)) => prompt,
        _ => cmd.body.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let text = "---\nmodel: mock/big\nsystem: Be terse\n---\nReview this: $input";
        let cmd = parse(text);
        assert_eq!(cmd.model.as_deref(), Some("mock/big"));
        assert_eq!(cmd.system.as_deref(), Some("Be terse"));
        assert_eq!(cmd.body, "Review this: $input");
    }

    #[test]
    fn body_without_frontmatter_is_still_a_command() {
        let cmd = parse("Just a prompt body");
        assert_eq!(cmd.body, "Just a prompt body");
        assert!(cmd.model.is_none());
        assert!(cmd.system.is_none());
    }

    #[test]
    fn unparseable_frontmatter_degrades_to_empty_metadata() {
        let cmd = parse("---\n: : bad yaml [\n---\nbody here");
        assert_eq!(cmd.body, "body here");
        assert!(cmd.model.is_none());
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
        assert_eq!(expand(&cmd, "the diff"), "Review: the diff");
        let plain = parse("Summarize the code");
        // apply() appends the input when the body lacks $input
        assert!(expand(&plain, "now").contains("Summarize the code"));
        // empty input sends the body as-is
        assert_eq!(expand(&cmd, ""), "Review: $input");
    }
}
