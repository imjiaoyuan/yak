//! Skills: SKILL.md packs discovered from the user dir and the project,
//! surfaced to the model as a name+description list (progressive disclosure
//! — the model reads the full file with the read tool when it decides to
//! use one). Interop: the agentskills-standard `.agents/skills` locations
//! are read too, at lower priority than our own `.llm/skills`, so skills
//! installed by other tools (npx skills, editors) work unmodified.

use std::path::{Path, PathBuf};

use crate::yaml;

/// Cap on the skill list injected into the system prompt; overflow drops
/// whole entries with a count note. 2000 chars ≈ 500 tokens — the list rides
/// every request, and the model reads the full SKILL.md on use anyway, so
/// only the trigger line earns its tokens here.
const LIST_CHAR_CAP: usize = 2000;

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
    let map = yaml::parse(fm).ok().unwrap_or_default();
    let name = map
        .get("name")
        .cloned()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| fallback_name.to_string());
    if name.is_empty() {
        return None;
    }
    let description = map.get("description").cloned().unwrap_or_default();
    let model_invocation = map
        .get("disable-model-invocation")
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
/// One installed package's skills: its `skills/` entries plus the whole-repo
/// skill when `SKILL.md` sits at the package root. That root shape — a
/// `SKILL.md` with its `references/` beside it — is how most standalone skill
/// repos ship, and the plain `skills/` walk alone would mount nothing from
/// them.
pub(crate) fn load_package(pkg: &Path, out: &mut Vec<SkillDef>) {
    if let Some(def) = pack_root_skill(pkg) {
        out.push(def);
    }
    load_dir(&pkg.join("skills"), out);
}

/// `SKILL.md` at the package root: the repository itself is one skill, named
/// by its frontmatter (the directory name is the fallback).
pub(crate) fn pack_root_skill(pkg: &Path) -> Option<SkillDef> {
    let path = pkg.join("SKILL.md");
    let text = std::fs::read_to_string(&path).ok()?;
    let fallback = pkg.file_name()?.to_str()?;
    parse_skill_md(&text, fallback, path)
}

pub(crate) fn load_dir(dir: &Path, out: &mut Vec<SkillDef>) {
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

/// Discover skills, lowest priority first so later entries override earlier
/// by name: installed packages, `~/.agents/skills`, `~/.llm/skills`, then the
/// nearest project `.agents/skills`, then the nearest `.llm/skills` (project
/// beats user, our dirs beat the interop dirs). `disabled` drops entries by
/// name.
pub fn discover(user_dir: &Path, cwd: &Path, disabled: &[String]) -> Vec<SkillDef> {
    let mut defs: Vec<SkillDef> = Vec::new();
    // packages first (lowest priority): user then project, so a
    // project-local install shadows the user one
    for root in [user_dir.join("pkg"), cwd.join(".llm/pkg")] {
        for pkg in crate::commands::pkg::packages_in(&root) {
            load_package(&pkg, &mut defs);
        }
    }
    load_dir(
        &user_dir.parent().unwrap_or(user_dir).join(".agents/skills"),
        &mut defs,
    );
    load_dir(&user_dir.join("skills"), &mut defs);
    if let Some(d) = crate::core::paths::nearest_dir_up(cwd, ".agents/skills", true) {
        load_dir(&d, &mut defs);
    }
    if let Some(d) = crate::core::paths::nearest_dir_up(cwd, ".llm/skills", true) {
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
/// dropped with a count note. Each line carries the skill's whole trigger
/// (the description is what the model matches a task against, so it is not
/// cut to the first line), whitespace collapsed and capped.
pub fn skills_block(skills: &[SkillDef]) -> Option<String> {
    let visible: Vec<&SkillDef> = skills.iter().filter(|s| s.model_invocation).collect();
    if visible.is_empty() {
        return None;
    }
    let mut out = String::from(
        "Available skills (read a skill's file before following it; resolve any relative path it \
         mentions against the skill's directory):\n",
    );
    let mut added = 0usize;
    for s in &visible {
        let summary = trigger(&s.description);
        let line = if summary.is_empty() {
            format!("- {} ({})\n", s.name, s.path.display())
        } else {
            format!("- {}: {} ({})\n", s.name, summary, s.path.display())
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

/// The skill's trigger text for the list. The description is what the model
/// matches a task against, so the whole thing is kept (whitespace collapsed
/// onto one line) — but the list rides every request, so a generous cap
/// trims it; the full text is one `read` away when the skill is picked up.
fn trigger(description: &str) -> String {
    const MAX_CHARS: usize = 160;
    let collapsed: String = description.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::core::text::truncate_chars(&collapsed, MAX_CHARS)
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
            "---\ndescription: x\ndisable-model-invocation: true\n---\nbody",
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

    /// A package installed by `llm install`: the repo root carries SKILL.md
    /// (a standalone skill repo) and `skills/` holds more of them.
    #[test]
    fn package_mounts_a_root_skill_and_its_skills_dir() {
        let pkg = crate::core::testutil::scratch_dir("pkgskill");
        skill_dir(&pkg, "a", "name: a");
        skill_dir(&pkg.join("skills"), "b", "name: b");
        std::fs::write(pkg.join("SKILL.md"), "---\nname: wholegit\n---\nbody").unwrap();
        // a stray repo without SKILL.md at the root mounts nothing
        let empty = pkg.join("not-a-skill");
        std::fs::create_dir_all(&empty).unwrap();

        let mut found = Vec::new();
        load_package(&pkg, &mut found);
        let names: Vec<&str> = found.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["wholegit", "b"]);
        assert_eq!(pack_root_skill(&pkg).unwrap().name, "wholegit");
        assert!(pack_root_skill(&empty).is_none());
        let _ = std::fs::remove_dir_all(&pkg);
    }

    #[test]
    fn project_overrides_user_and_llm_beats_agents() {
        let tmp = crate::core::testutil::scratch_path("skills");
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
        let tmp = crate::core::testutil::scratch_path("skills-d");
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

    #[test]
    fn skill_line_keeps_the_whole_trigger_and_teaches_path_resolution() {
        // a multi-line description keeps every line in one collapsed run:
        // the first line alone may not name what the skill does
        let def = SkillDef {
            name: "pdf".into(),
            description: "Extract tables\nfrom scanned PDFs and CSV exports".into(),
            path: PathBuf::from("/s/pdf/SKILL.md"),
            model_invocation: true,
        };
        let block = skills_block(&[def]).unwrap();
        assert!(
            block.contains("Extract tables from scanned PDFs and CSV exports"),
            "{block}"
        );
        assert!(
            block.contains("resolve any relative path"),
            "a skill that references its own files must be told where to resolve them: {block}"
        );
    }
}
