//! Skills: SKILL.md packs discovered from the user dir and the project,
//! surfaced to the model as the reference's `<available_skills>` block (name +
//! description + location; progressive disclosure — the model reads the full
//! file with the read tool when it decides to use one). Interop: the
//! agentskills-standard `.agents/skills` locations are read too, at lower
//! priority than our own `.yak/skills`, so skills installed by other tools
//! (npx skills, editors) work unmodified.

use std::path::{Path, PathBuf};

use crate::yaml;

/// Name rules per the Agent Skills spec, enforced as they are enforced: a
/// violation is a warning, not a rejection.
const MAX_NAME_LENGTH: usize = 64;
/// Description length cap per the spec, also warning-only.
const MAX_DESCRIPTION_LENGTH: usize = 1024;

#[derive(Clone, Debug)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// false = excluded from the system-prompt list; only /skill:name works
    pub model_invocation: bool,
}

/// Parse a SKILL.md: `---` yaml frontmatter `---` then the instruction body
/// (read on demand, never stored here). Rules follow the reference's
/// `loadSkillFromFile`: frontmatter that cannot be parsed or a missing/
/// empty description skips the skill with a warning; an invalid name or an
/// over-long description only warns and the skill still loads.
pub fn parse_skill_md(text: &str, fallback_name: &str, path: &Path) -> Result<SkillDef, String> {
    let warn = |msg: &str| eprintln!("Warning: skill {}: {msg}", path.display());
    let Some((fm, _)) = crate::yaml::split_frontmatter(text) else {
        return Err("no frontmatter".to_string());
    };
    let map = match yaml::parse(fm) {
        Ok(map) => map,
        Err(e) => {
            return Err(format!("unparseable frontmatter ({e})"));
        }
    };
    let description = map.get("description").cloned().unwrap_or_default();
    if description.trim().is_empty() {
        warn("description is required");
        return Err("description is required".to_string());
    }
    if description.chars().count() > MAX_DESCRIPTION_LENGTH {
        warn(&format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} characters"
        ));
    }
    let name = map
        .get("name")
        .cloned()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| fallback_name.to_string());
    if name.chars().count() > MAX_NAME_LENGTH {
        warn(&format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.chars().count()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        warn("name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)");
    }
    if name.starts_with('-') || name.ends_with('-') {
        warn("name must not start or end with a hyphen");
    }
    if name.contains("--") {
        warn("name must not contain consecutive hyphens");
    }
    let model_invocation = map
        .get("disable-model-invocation")
        .map(|v| v != "true")
        .unwrap_or(true);
    Ok(SkillDef {
        name,
        description,
        path: path.to_path_buf(),
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
    parse_skill_md(&text, fallback, &path).ok()
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
                && let Ok(def) = parse_skill_md(&text, fallback, &skill)
            {
                out.push(def);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let Some(fallback) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(def) = parse_skill_md(&text, fallback, &path)
            {
                out.push(def);
            }
        }
    }
}

/// Discover skills, lowest priority first so later entries override earlier
/// by name: installed packages, `~/.agents/skills`, `~/.yak/skills`, then the
/// nearest project `.agents/skills`, then the nearest `.yak/skills` (project
/// beats user, our dirs beat the interop dirs). `disabled` drops entries by
/// name.
pub fn discover(user_dir: &Path, cwd: &Path, disabled: &[String]) -> Vec<SkillDef> {
    let mut defs: Vec<SkillDef> = Vec::new();
    // packages first (lowest priority): user then project, so a
    // project-local install shadows the user one
    for root in [
        user_dir.join("pkg"),
        crate::core::paths::nearest_project_dir(cwd)
            .map(|d| d.join("pkg"))
            .unwrap_or_else(|| cwd.join(".yak/pkg")),
    ] {
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
    if let Some(d) = crate::core::paths::nearest_dir_up(cwd, ".yak/skills", true)
        .or_else(|| crate::core::paths::nearest_dir_up(cwd, ".llm/skills", true))
    {
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

/// The system-prompt section, the reference's `formatSkillsForPrompt`: one XML entry
/// per skill the model may pick up on its own, with the file location for
/// progressive disclosure.
pub fn skills_block(skills: &[SkillDef]) -> Option<String> {
    let visible: Vec<&SkillDef> = skills.iter().filter(|s| s.model_invocation).collect();
    if visible.is_empty() {
        return None;
    }
    let mut out = String::from(
        "The following skills provide specialized instructions for specific tasks.\n\
         Use the read tool to load a skill's file when the task matches its description.\n\
         When a skill file references a relative path, resolve it against the skill directory \
         (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.\n\
         \n\
         <available_skills>\n",
    );
    for s in &visible {
        out.push_str("  <skill>\n");
        out.push_str(&format!("    <name>{}</name>\n", escape_xml(&s.name)));
        out.push_str(&format!(
            "    <description>{}</description>\n",
            escape_xml(&s.description)
        ));
        out.push_str(&format!(
            "    <location>{}</location>\n",
            escape_xml(&s.path.display().to_string())
        ));
        out.push_str("  </skill>\n");
    }
    out.push_str("</available_skills>");
    Some(out)
}

/// the reference's `escapeXml`, so a description containing angle brackets or quotes
/// cannot break the block's structure.
fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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
            Path::new("/s/pdf/SKILL.md"),
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
            Path::new("/s/dirskill/SKILL.md"),
        )
        .unwrap();
        assert_eq!(def.name, "dirskill");
        assert!(!def.model_invocation);
    }

    /// a SKILL is dropped.md without a usable description: the description is
    /// what the model matches a task against, so without one it can never
    /// fire.
    #[test]
    fn a_skill_without_a_description_is_skipped() {
        assert!(
            parse_skill_md(
                "---\nname: pdf\n---\nbody",
                "pdf",
                Path::new("/s/pdf/SKILL.md"),
            )
            .is_err()
        );
    }

    /// Frontmatter the YAML subset cannot parse means the metadata is not
    /// understood — a skill fired on guessed metadata is worse than a loud
    /// skip (skipped too).
    #[test]
    fn unparseable_frontmatter_is_skipped_loudly() {
        let err = parse_skill_md(
            "---\nname: x\njust some text without a colon\n---\nbody",
            "dirskill",
            Path::new("/s/dirskill/SKILL.md"),
        )
        .unwrap_err();
        assert!(err.contains("frontmatter"), "{err}");
        // truly absent frontmatter is still not a skill
        assert!(parse_skill_md("# plain notes\nbody", "notes", Path::new("/s/notes.md")).is_err());
    }

    /// Package installed by `yak install`: the repo root carries SKILL.md
    /// (a standalone skill repo) and `skills/` holds more of them.
    #[test]
    fn package_mounts_a_root_skill_and_its_skills_dir() {
        let pkg = crate::core::testutil::scratch_dir("pkgskill");
        skill_dir(&pkg, "a", "name: a\ndescription: d");
        skill_dir(&pkg.join("skills"), "b", "name: b\ndescription: d");
        std::fs::write(
            pkg.join("SKILL.md"),
            "---\nname: wholegit\ndescription: d\n---\nbody",
        )
        .unwrap();
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
    fn project_overrides_user_and_yak_beats_agents() {
        let tmp = crate::core::testutil::scratch_path("skills");
        let user = tmp.join("userdir"); // plays ~/.yak
        let proj = tmp.join("proj");
        std::fs::create_dir_all(user.join("skills")).unwrap();
        // interop user dir: ~/.agents/skills sits next to ~/.yak
        std::fs::create_dir_all(tmp.join("userdir/.agents/skills").parent().unwrap()).unwrap();
        std::fs::create_dir_all(tmp.join(".agents/skills")).unwrap();
        std::fs::create_dir_all(proj.join(".yak/skills")).unwrap();

        skill_dir(
            &tmp.join(".agents/skills"),
            "interop",
            "description: agents copy",
        );
        skill_dir(&user.join("skills"), "shared", "description: user copy");
        skill_dir(&user.join("skills"), "only-user", "description: u");
        skill_dir(
            &proj.join(".yak/skills"),
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
    fn the_skills_block_is_shaped_and_escapes_xml() {
        let def = SkillDef {
            name: "pdf".into(),
            description: "Extract <tables> & \"quotes\" from PDFs".into(),
            path: PathBuf::from("/s/pdf/SKILL.md"),
            model_invocation: true,
        };
        let block = skills_block(&[def]).unwrap();
        assert!(
            block.starts_with(
                "The following skills provide specialized instructions for specific tasks."
            ),
            "{block}"
        );
        assert!(
            block.contains("resolve it against the skill directory"),
            "{block}"
        );
        assert!(block.contains("<available_skills>"), "{block}");
        assert!(block.contains("<name>pdf</name>"), "{block}");
        assert!(
            block.contains(
                "<description>Extract &lt;tables&gt; &amp; &quot;quotes&quot; from PDFs</description>"
            ),
            "{block}"
        );
        assert!(
            block.contains("<location>/s/pdf/SKILL.md</location>"),
            "{block}"
        );
        // hidden skills never appear
        let mut hidden = SkillDef {
            name: "hidden".into(),
            description: "d".into(),
            path: PathBuf::from("/s/h"),
            model_invocation: false,
        };
        hidden.model_invocation = false;
        assert!(skills_block(&[hidden]).is_none());
    }
}
