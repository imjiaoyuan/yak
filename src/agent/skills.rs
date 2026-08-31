//! Skills: SKILL.md packs discovered from the user dir and the project,
//! surfaced to the model as a name+description list (progressive disclosure
//! — the model reads the full file with the read tool when it decides to
//! use one). Interop: the agentskills-standard `.agents/skills` locations
//! are read too, at lower priority than our own `.llm/skills`, so skills
//! installed by other tools (npx skills, editors) work unmodified.

use std::path::{Path, PathBuf};

use crate::yaml;

/// Cap on the skill list injected into the system prompt; overflow drops
/// whole entries with a count note (matching the reference's context budget).
const LIST_CHAR_CAP: usize = 8000;

#[derive(Clone)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// false = excluded from the system-prompt list; only /skill:name works
    pub model_invocation: bool,
}

/// Parse a SKILL.md: `---` yaml frontmatter `---` then the instruction
/// body (read on demand, never stored here). A missing/never-closed
/// frontmatter means "not a skill" (None), but a frontmatter the YAML
/// subset cannot parse still loads with the fallback name — metadata must
/// never lose a usable skill.
pub fn parse_skill_md(text: &str, fallback_name: &str, path: PathBuf) -> Option<SkillDef> {
    let (fm, _) = crate::yaml::split_frontmatter(text)?;
    let map = yaml::parse(fm).ok().and_then(|y| y.as_map());
    let name = map
        .as_ref()
        .and_then(|m| m.get("name").cloned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| fallback_name.to_string());
    if name.is_empty() {
        return None;
    }
    let description = map
        .as_ref()
        .and_then(|m| m.get("description"))
        .cloned()
        .unwrap_or_default();
    let model_invocation = map
        .as_ref()
        .and_then(|m| m.get("disable_model_invocation"))
        .map(|v| v != "true")
        .unwrap_or(true);
    Some(SkillDef {
        name,
        description,
        path,
        model_invocation,
    })
}

/// Load every skill in one directory: `<dir>/<name>/SKILL.md` (the standard
/// layout) plus flat `<dir>/<name>.md` files.
fn load_dir(dir: &Path, out: &mut Vec<SkillDef>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            let skill = path.join("SKILL.md");
            let Some(fallback) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Ok(text) = std::fs::read_to_string(&skill)
                && let Some(def) = parse_skill_md(&text, fallback, skill)
            {
                out.push(def);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let Some(fallback) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Some(def) = parse_skill_md(&text, fallback, path.clone())
            {
                out.push(def);
            }
        }
    }
}

/// Nearest `suffix` directory walking up from cwd, stopping at the git root.
fn nearest_dir(cwd: &Path, suffix: &str) -> Option<PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let candidate = d.join(suffix);
        if candidate.is_dir() {
            return Some(candidate);
        }
        if d.join(".git").exists() {
            return None;
        }
        dir = d.parent();
    }
    None
}

/// Discover skills, lowest priority first so later entries override earlier
/// by name: `~/.agents/skills`, `~/.llm/skills`, then the nearest project
/// `.agents/skills`, then the nearest `.llm/skills` (project beats user, our
/// dirs beat the interop dirs). `disabled` drops entries by name.
pub fn discover(user_dir: &Path, cwd: &Path, disabled: &[String]) -> Vec<SkillDef> {
    let mut defs: Vec<SkillDef> = Vec::new();
    load_dir(
        &user_dir.parent().unwrap_or(user_dir).join(".agents/skills"),
        &mut defs,
    );
    load_dir(&user_dir.join("skills"), &mut defs);
    if let Some(d) = nearest_dir(cwd, ".agents/skills") {
        load_dir(&d, &mut defs);
    }
    if let Some(d) = nearest_dir(cwd, ".llm/skills") {
        load_dir(&d, &mut defs);
    }
    let mut merged: Vec<SkillDef> = Vec::new();
    for def in defs {
        if disabled.iter().any(|d| d == &def.name) {
            continue;
        }
        match merged.iter().position(|d| d.name == def.name) {
            Some(i) => merged[i] = def,
            None => merged.push(def),
        }
    }
    merged
}

/// The system-prompt section: one line per skill that the model may pick up
/// on its own. Capped at LIST_CHAR_CAP; entries that no longer fit are
/// dropped with a count note.
pub fn skills_block(skills: &[SkillDef]) -> Option<String> {
    let visible: Vec<&SkillDef> = skills.iter().filter(|s| s.model_invocation).collect();
    if visible.is_empty() {
        return None;
    }
    let mut out =
        String::from("Available skills (to use one, read its file first, then follow it):\n");
    let mut added = 0usize;
    for s in &visible {
        let line = if s.description.is_empty() {
            format!("- {} ({})\n", s.name, s.path.display())
        } else {
            format!("- {}: {} ({})\n", s.name, s.description, s.path.display())
        };
        if out.len() + line.len() > LIST_CHAR_CAP {
            break;
        }
        out.push_str(&line);
        added += 1;
    }
    let dropped = visible.len() - added;
    if dropped > 0 {
        out.push_str(&format!(
            "- … and {dropped} more (omitted to save context)\n"
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_dir(root: &Path, name: &str, fm: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("---\n{fm}\n---\nbody")).unwrap();
    }

    #[test]
    fn parses_frontmatter_with_defaults() {
        let def = parse_skill_md(
            "---\nname: pdf\ndescription: extract tables\n---\nbody",
            "fallback",
            PathBuf::from("/s/pdf/SKILL.md"),
        )
        .unwrap();
        assert_eq!(def.name, "pdf");
        assert_eq!(def.description, "extract tables");
        assert!(def.model_invocation);
    }

    #[test]
    fn falls_back_to_dir_name_and_honors_disable() {
        let def = parse_skill_md(
            "---\ndescription: x\ndisable_model_invocation: true\n---\nbody",
            "dirskill",
            PathBuf::from("/s/dirskill/SKILL.md"),
        )
        .unwrap();
        assert_eq!(def.name, "dirskill");
        assert!(!def.model_invocation);
    }

    #[test]
    fn unparseable_frontmatter_still_loads_with_fallback_name() {
        // the YAML subset chokes on the bare text line; the skill must
        // survive with the directory name instead of being dropped
        let def = parse_skill_md(
            "---\nname: x\njust some text without a colon\n---\nbody",
            "dirskill",
            PathBuf::from("/s/dirskill/SKILL.md"),
        )
        .unwrap();
        assert_eq!(def.name, "dirskill");
        assert!(def.description.is_empty());
        assert!(def.model_invocation);
        // truly absent frontmatter is still not a skill
        assert!(
            parse_skill_md("# plain notes\nbody", "notes", PathBuf::from("/s/notes.md")).is_none()
        );
    }

    #[test]
    fn project_overrides_user_and_llm_beats_agents() {
        let tmp = std::env::temp_dir().join(format!("llm-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let user = tmp.join("userdir"); // plays ~/.llm
        let proj = tmp.join("proj");
        std::fs::create_dir_all(user.join("skills")).unwrap();
        // interop user dir: ~/.agents/skills sits next to ~/.llm
        std::fs::create_dir_all(tmp.join("userdir/.agents/skills").parent().unwrap()).unwrap();
        std::fs::create_dir_all(tmp.join(".agents/skills")).unwrap();
        std::fs::create_dir_all(proj.join(".llm/skills")).unwrap();

        skill_dir(
            &tmp.join(".agents/skills"),
            "interop",
            "description: agents copy",
        );
        skill_dir(&user.join("skills"), "shared", "description: user copy");
        skill_dir(&user.join("skills"), "only-user", "description: u");
        skill_dir(
            &proj.join(".llm/skills"),
            "shared",
            "description: project copy",
        );

        let mut found = discover(&user, &proj, &[]);
        found.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(found.len(), 3);
        let shared = found.iter().find(|s| s.name == "shared").unwrap();
        assert_eq!(shared.description, "project copy");
        assert!(found.iter().any(|s| s.name == "interop"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn disabled_names_are_dropped() {
        let tmp = std::env::temp_dir().join(format!("llm-skills-d-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let user = tmp.join("userdir");
        std::fs::create_dir_all(user.join("skills")).unwrap();
        skill_dir(&user.join("skills"), "nope", "description: x");
        assert!(discover(&user, &tmp, &["nope".to_string()]).is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn block_caps_and_counts() {
        let mk = |i: usize| SkillDef {
            name: format!("skill{i}"),
            description: "d".repeat(900),
            path: PathBuf::from("/s"),
            model_invocation: true,
        };
        let skills: Vec<SkillDef> = (0..40).map(mk).collect();
        let block = skills_block(&skills).unwrap();
        assert!(block.len() <= LIST_CHAR_CAP + 200);
        assert!(block.contains("more (omitted"));
        // hidden skills never appear
        let mut hidden = mk(0);
        hidden.model_invocation = false;
        assert!(skills_block(&[hidden]).is_none());
    }
}
