//! `llm install|remove|list` — the reference packages, git-only. A package is a git
//! repository cloned under `~/.llm/pkg/<name>` (`-l`: `.llm/pkg/<name>`,
//! project-local); its `skills/`, `extensions/` and `commands/` directories
//! mount into the normal discovery walks. No update command: re-running
//! `install` refreshes (pinned `@ref` clones are skipped by the refresh).

use std::path::{Path, PathBuf};

use crate::core::args::{OptSpec, ParsedArgs, render_help};
use crate::{flag_spec, multi_spec};

const INSTALL_SPECS: &[OptSpec] = &[
    flag_spec!(
        "local",
        Some('l'),
        "Install project-local (.llm/pkg/ instead of ~/.llm/pkg/)"
    ),
    flag_spec!(
        "global",
        Some('g'),
        "Install into the user directory (default)"
    ),
    multi_spec!(
        "skill",
        Some('s'),
        "Keep only these skills by name; '*' keeps all (repeatable)",
        "NAME"
    ),
    flag_spec!("help", Some('h'), "Show this message and exit"),
];

const REMOVE_SPECS: &[OptSpec] = &[flag_spec!("help", Some('h'), "Show this message and exit")];

const LIST_SPECS: &[OptSpec] = &[flag_spec!("help", Some('h'), "Show this message and exit")];

pub fn run(argv: &[String]) -> i32 {
    let Some(first) = argv.first().map(String::as_str) else {
        print!(
            "{}",
            render_help(
                "llm install|remove|list",
                "Manage git packages (the reference packages, git-only)\n\nCommands:\n  install git:github.com/user/repo[@ref]\n  remove NAME\n  list",
                LIST_SPECS,
                &[],
            )
        );
        return 0;
    };
    let rest: Vec<String> = argv[1..].to_vec();
    match first {
        "install" => install(&rest),
        "remove" | "uninstall" => remove(&rest),
        "list" => list(&rest),
        "--help" | "-h" | "help" => {
            print!(
                "{}",
                render_help(
                    "llm install|remove|list",
                    "Manage git packages (the reference packages, git-only)",
                    LIST_SPECS,
                    &[],
                )
            );
            0
        }
        other => {
            eprintln!("Error: No such command '{other}' (install, remove, list).");
            2
        }
    }
}

fn parse_install_args(argv: &[String]) -> (Option<ParsedArgs>, i32) {
    crate::core::args::parse_with_help(argv, INSTALL_SPECS, || {
        render_help(
            "llm install git:github.com/user/repo[@ref]",
            "Clone a package into the pkg directory (re-run to refresh)",
            INSTALL_SPECS,
            &[("SOURCE", "git:github.com/user/repo, with an optional @ref")],
        )
    })
}

/// `git:github.com/user/repo`, `git:git@github.com:user/repo` or a plain
/// https URL, with an optional `@ref` pinned on the end.
fn parse_source(raw: &str) -> Option<(String, Option<String>)> {
    let (url_part, ref_) = match raw.rsplit_once('@') {
        // only a trailing `@ref` after a real path splits (the `@` in
        // `git@github.com:` stays part of the URL)
        Some((u, r)) if !r.is_empty() && u.contains('/') => (u.to_string(), Some(r.to_string())),
        _ => (raw.to_string(), None),
    };
    let url = if let Some(rest) = url_part.strip_prefix("git:github.com/") {
        format!("https://github.com/{rest}")
    } else if let Some(rest) = url_part.strip_prefix("git:git@github.com:") {
        format!("https://github.com/{rest}")
    } else if let Some(rest) = url_part.strip_prefix("file://") {
        rest.to_string()
    } else if url_part.starts_with("https://")
        || url_part.starts_with("http://")
        || Path::new(&url_part).is_dir()
    {
        // an https remote or a plain local path (self-hosted, testing)
        url_part.clone()
    } else {
        return None;
    };
    Some((url, ref_))
}

fn pkg_root(local: bool) -> PathBuf {
    if local {
        std::env::current_dir()
            .unwrap_or_else(|_| Path::new(".").to_path_buf())
            .join(".llm/pkg")
    } else {
        crate::core::config::user_dir().join("pkg")
    }
}

fn git(args: &[&str], cwd: &Path) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn install(argv: &[String]) -> i32 {
    let (args, code) = parse_install_args(argv);
    let Some(args) = args else { return code };
    let Some(raw) = args.positionals.first() else {
        eprintln!("Error: Usage: llm install git:github.com/user/repo[@ref]");
        return 2;
    };
    let Some((url, ref_)) = parse_source(raw) else {
        eprintln!(
            "Error: '{raw}' is not a git source (git:github.com/user/repo[@ref] or an https URL)"
        );
        return 2;
    };
    let name = url
        .trim_end_matches('/')
        .rsplit(['/', '\\'])
        .next()
        .map(|n| n.trim_end_matches(".git"))
        .unwrap_or("package")
        .to_string();
    if !crate::core::text::valid_plugin_name(&name, 64) {
        eprintln!("Error: cannot derive a package name from '{url}'");
        return 2;
    }
    if args.flag(&["local"]) && args.flag(&["global"]) {
        eprintln!("Error: --local and --global are mutually exclusive");
        return 2;
    }
    let p = crate::theme::err();
    // scope first (the clone path depends on it): an explicit flag wins,
    // otherwise an attended terminal gets asked, pipes/CI keep the user dir
    let mut local = args.flag(&["local"]);
    let scope_given = local || args.flag(&["global"]);
    if !scope_given && interactive() {
        let items = [
            "project-local  (.llm/pkg/ — this project only)".to_string(),
            format!("global  ({} · every project)", pkg_root(false).display()),
        ];
        match crate::term::lineedit::pick("install where:", &items, true) {
            Some(0) => local = true,
            Some(_) => local = false,
            None => {
                eprintln!("{}install cancelled{}", p.dim, p.reset);
                return 1;
            }
        }
    }
    let root = pkg_root(local);
    let target = root.join(&name);
    if target.exists() {
        // refresh, unless the install is pinned to a ref the caller did
        // not repeat (a pinned clone moves only via install @new-ref)
        let pinned = git(&["config", "llm.pinned"], &target)
            .ok()
            .is_some_and(|p| !p.trim().is_empty() && ref_.as_deref() != Some(p.trim()));
        if pinned && ref_.is_none() {
            eprintln!(
                "{} {name} is pinned — re-run with @ref to move it{}",
                p.dim, p.reset
            );
        } else if let Err(e) = git(&["fetch", "--tags", "origin"], &target).and_then(|_| {
            let to = ref_.clone().unwrap_or_else(|| "origin/HEAD".to_string());
            git(&["reset", "--hard", &to], &target).map(|_| ())
        }) {
            eprintln!("Error: refresh failed: {e}");
            return 1;
        } else {
            eprintln!("{}updated {name}{}", p.dim, p.reset);
        }
    } else {
        let _ = std::fs::create_dir_all(&root);
        let mut clone = vec!["clone".to_string(), "--depth".to_string(), "1".to_string()];
        if let Some(ref_) = &ref_ {
            clone.push("--branch".to_string());
            clone.push(ref_.clone());
        }
        clone.push(url.clone());
        clone.push(target.display().to_string());
        let clone_args: Vec<&str> = clone.iter().map(String::as_str).collect();
        if let Err(e) = git(&clone_args, &root) {
            eprintln!("Error: clone failed: {e}");
            return 1;
        }
        if ref_.is_none() {
            let _ = git(&["config", "llm.pinned", ""], &target);
        } else {
            let _ = git(
                &["config", "llm.pinned", ref_.unwrap_or_default().as_str()],
                &target,
            );
        }
        eprintln!(
            "{}installed {name} → {}{}",
            p.dim,
            target.display(),
            p.reset
        );
    }
    // a package is whatever its layout says it is: recognize the skills,
    // extensions and prompts it carries and say so. A repo that mounts
    // nothing (no hooks dir, no root SKILL.md) is otherwise silent — you
    // only notice on the next `/help` that nothing showed up
    let found = carried(&target);
    let wanted = args.multi(&["skill"]);
    if wanted.iter().any(|w| w == "*") {
        clear_keep_list(&target, "llm.skills");
        clear_keep_list(&target, "llm.extensions");
        clear_keep_list(&target, "llm.prompts");
    } else if !wanted.is_empty() {
        let available: Vec<&str> = found
            .root_skill
            .iter()
            .map(String::as_str)
            .chain(found.skills.iter().map(String::as_str))
            .collect();
        if let Some(missing) = wanted.iter().find(|w| !available.contains(&w.as_str())) {
            let carries = if available.is_empty() {
                "no skills".to_string()
            } else {
                available.join(", ")
            };
            eprintln!(
                "Error: no skill '{missing}' in {name} (carries: {carries}) — \
                 the clone is installed; llm remove {name} to drop it"
            );
            return 2;
        }
        set_keep_list(&target, "llm.skills", &wanted);
    } else if interactive() && found.total() > 1 {
        // no --skill, and a terminal to ask in: show everything the repo
        // carries and let the caller uncheck what should stay dormant.
        // A cancel keeps the whole package rather than aborting the clone.
        match choose_items(&found) {
            Some(keep) => {
                for (key, names) in [
                    ("llm.skills", &keep.skills),
                    ("llm.extensions", &keep.extensions),
                    ("llm.prompts", &keep.prompts),
                ] {
                    match names {
                        Some(names) => set_keep_list(&target, key, names),
                        None => clear_keep_list(&target, key),
                    }
                }
            }
            None => eprintln!("{}keeping everything (cancelled){}", p.dim, p.reset),
        }
    }
    for line in notes(&found, &selected(&target)) {
        eprintln!("{}  {line}{}", p.dim, p.reset);
    }
    0
}

/// Is there a human at the keyboard? A piped install (CI, scripts, the
/// e2e harness with stdin=DEVNULL) must never block on a menu.
fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// What the multi-select left checked, per group, in the names the keep
/// lists store.
/// What the menu changed, per group: `None` = the group stayed entirely
/// checked (nothing to record), `Some(names)` = narrowed to those names
/// (`Some(vec![])` = the whole group off).
#[derive(Default)]
struct Keep {
    skills: Option<Vec<String>>,
    extensions: Option<Vec<String>>,
    prompts: Option<Vec<String>>,
}

/// One flat menu of every mountable item, tagged by kind: skills first (the
/// repo-root one included), then extensions, then prompts. Returns None when
/// cancelled. Nothing chosen = nothing mounted, which is a legal answer.
fn choose_items(found: &Carried) -> Option<Keep> {
    let kinds = menu_items(found);
    let items: Vec<String> = kinds
        .iter()
        .map(|(kind, name)| format!("{kind:<9} {name}"))
        .collect();
    let chosen = crate::term::lineedit::pick_multi(
        "install which of these? (space toggles, enter installs):",
        &items,
    )?;
    Some(keep_from(&kinds, &chosen))
}

/// Every mountable item as (kind, name): skills first (the repo-root one
/// included), then extensions, then prompts — the order the menu shows.
fn menu_items(found: &Carried) -> Vec<(&'static str, String)> {
    let mut kinds: Vec<(&'static str, String)> = Vec::new();
    for name in found.root_skill.iter().chain(found.skills.iter()) {
        kinds.push(("skill", name.clone()));
    }
    for name in &found.extensions {
        kinds.push(("extension", name.clone()));
    }
    for name in &found.prompts {
        kinds.push(("prompt", name.clone()));
    }
    kinds
}

/// Fold the checked menu indexes back into per-group keep lists. A group
/// left entirely checked records nothing, so the package keeps following the
/// repo — a skill or extension added upstream is live without re-installing.
fn keep_from(kinds: &[(&str, String)], chosen: &[usize]) -> Keep {
    let mut keep = Keep::default();
    let kept = |kind: &str| -> Vec<String> {
        kinds
            .iter()
            .enumerate()
            .filter(|(i, (k, _))| *k == kind && chosen.contains(i))
            .map(|(_, (_, name))| name.clone())
            .collect()
    };
    let all = |kind: &str| -> usize { kinds.iter().filter(|(k, _)| *k == kind).count() };
    for kind in ["skill", "extension", "prompt"] {
        let total = all(kind);
        let names = kept(kind);
        if total == 0 || names.len() == total {
            continue;
        }
        match kind {
            "skill" => keep.skills = Some(names),
            "extension" => keep.extensions = Some(names),
            _ => keep.prompts = Some(names),
        }
    }
    keep
}

/// What an installed package carries, by layout — the same rules the
/// discovery walks apply: `skills/`, `extensions/`, `commands/`, plus a
/// `SKILL.md` at the package root (a whole-repo skill, the shape most
/// standalone skill repos ship: `SKILL.md` + `references/` at the top).
#[derive(Default)]
pub struct Carried {
    /// name of the root `SKILL.md` skill, when the repo itself is one skill
    pub root_skill: Option<String>,
    pub skills: Vec<String>,
    pub extensions: Vec<String>,
    pub prompts: Vec<String>,
}

impl Carried {
    /// Everything the package could mount, across the groups.
    fn total(&self) -> usize {
        self.root_skill.iter().count()
            + self.skills.len()
            + self.extensions.len()
            + self.prompts.len()
    }
}

/// A package's recorded keep lists, one per group: `None` = everything live.
#[derive(Default, Clone)]
pub struct Selected {
    pub skills: Option<Vec<String>>,
    pub extensions: Option<Vec<String>>,
    pub prompts: Option<Vec<String>>,
}

pub fn carried(pkg: &Path) -> Carried {
    let mut out = Carried::default();
    if let Some(def) = crate::agent::skills::pack_root_skill(pkg) {
        out.root_skill = Some(def.name);
    }
    let mut defs = Vec::new();
    crate::agent::skills::load_dir(&pkg.join("skills"), &mut defs);
    out.skills = defs.into_iter().map(|d| d.name).collect();
    out.extensions = entry_names(&pkg.join("extensions"));
    out.prompts = file_stems(&pkg.join("commands"), "md");
    out
}

/// The package's selections behind one call.
pub fn selected(pkg: &Path) -> Selected {
    Selected {
        skills: keep_list(pkg, "llm.skills"),
        extensions: keep_list(pkg, "llm.extensions"),
        prompts: keep_list(pkg, "llm.prompts"),
    }
}

fn keep_list(pkg: &Path, key: &str) -> Option<Vec<String>> {
    let raw = git(&["config", "--get", key], pkg).ok()?;
    let raw = raw.trim();
    if raw.is_empty() || raw == "*" {
        return None;
    }
    // "-" is the recorded "none of this group": the clone stays for the
    // other groups, but nothing of this one mounts
    if raw == "-" {
        return Some(Vec::new());
    }
    let names: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    (!names.is_empty()).then_some(names)
}

/// Record a keep list: `-` for "none of this group", else the names.
fn set_keep_list(pkg: &Path, key: &str, names: &[String]) {
    let value = if names.is_empty() {
        "-".to_string()
    } else {
        names.join(",")
    };
    let _ = git(&["config", key, &value], pkg);
}

/// Forget a keep list (`*`): the whole group mounts again.
fn clear_keep_list(pkg: &Path, key: &str) {
    let _ = git(&["config", key, "*"], pkg);
}

/// Is `file` inside a package's `extensions/` directory switched on? The
/// keep list rides the clone's git config, so only package-carried files
/// can be off — `~/.llm/extensions/` is never filtered.
pub fn extension_kept(dir: &Path, file: &str) -> bool {
    dir_kept(dir, file, "llm.extensions")
}

/// The same for a `commands/` prompt, keyed by stem (the `NAME` in
/// `llm NAME`), which is how [`file_stems`] records them.
pub fn prompt_kept(dir: &Path, stem: &str) -> bool {
    dir_kept(dir, stem, "llm.prompts")
}

fn dir_kept(dir: &Path, name: &str, key: &str) -> bool {
    let Some(pkg) = dir.parent() else {
        return true;
    };
    match keep_list(pkg, key) {
        None => true,
        Some(keep) => keep.iter().any(|k| k == name),
    }
}

/// The dim lines `install` and `list` print for one package: what it
/// carries, what is live of that, or why nothing is.
fn notes(found: &Carried, selected: &Selected) -> Vec<String> {
    let mut skills: Vec<String> = found
        .root_skill
        .iter()
        .map(|s| format!("{s} (repo root)"))
        .collect();
    skills.extend(found.skills.iter().cloned());
    let mut out = Vec::new();
    for (label, items) in [
        ("skills", &skills),
        ("extensions", &found.extensions),
        ("prompts", &found.prompts),
    ] {
        if !items.is_empty() {
            out.push(format!("{label}: {}", items.join(", ")));
        }
    }
    for (label, keep) in [
        ("skills", &selected.skills),
        ("extensions", &selected.extensions),
        ("prompts", &selected.prompts),
    ] {
        if let Some(keep) = keep {
            out.push(format!("only {label}: {}", keep.join(", ")));
        }
    }
    if out.is_empty() {
        out.push(
            "nothing llm can mount — needs skills/, extensions/, commands/ or a root SKILL.md"
                .to_string(),
        );
    }
    out
}

fn entry_names(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    out.sort();
    out
}

fn file_stems(dir: &Path, ext: &str) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(ext))
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .collect();
    out.sort();
    out
}

fn remove(argv: &[String]) -> i32 {
    let (args, code) = crate::core::args::parse_with_help(argv, REMOVE_SPECS, || {
        render_help(
            "llm remove NAME",
            "Delete an installed package",
            REMOVE_SPECS,
            &[],
        )
    });
    let Some(args) = args else { return code };
    let Some(name) = args.positionals.first() else {
        eprintln!("Error: Usage: llm remove NAME");
        return 2;
    };
    let mut removed = false;
    for local in [true, false] {
        let target = pkg_root(local).join(name);
        if target.exists() {
            std::fs::remove_dir_all(&target)
                .map_err(|e| eprintln!("Error: cannot remove {}: {e}", target.display()))
                .unwrap_or(());
            eprintln!(
                "{}removed {name} ({}){}",
                crate::theme::err().dim,
                pkg_root(local).display(),
                crate::theme::err().reset
            );
            removed = true;
        }
    }
    if !removed {
        eprintln!("Error: no package '{name}' installed");
        return 1;
    }
    0
}

fn list(argv: &[String]) -> i32 {
    let (args, code) = crate::core::args::parse_with_help(argv, LIST_SPECS, || {
        render_help(
            "llm list",
            "List installed packages and what they carry",
            LIST_SPECS,
            &[],
        )
    });
    let Some(_) = args else { return code };
    let mut any = false;
    for (local, root) in [(false, pkg_root(false)), (true, pkg_root(true))] {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            any = true;
            let scope = if local { "project" } else { "user" };
            eprintln!(
                "{b}{name}{r} {d}({scope} · {}){r}",
                path.display(),
                b = crate::theme::err().bold,
                r = crate::theme::err().reset,
                d = crate::theme::err().dim
            );
            let found = carried(&path);
            for line in notes(&found, &selected(&path)) {
                eprintln!(
                    "{}  {line}{}",
                    crate::theme::err().dim,
                    crate::theme::err().reset
                );
            }
        }
    }
    if !any {
        eprintln!(
            "{}no packages — llm install git:github.com/user/repo{}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
    }
    0
}

/// Package-carried extension directories, nearest-first roots for the
/// discovery walks (project pkg wins over user pkg via ordering at the
/// call site).
/// The package clones rooted anywhere: the skills walk passes the user
/// dir it was handed (so discovery stays hermetic in tests) instead of the
/// ambient one.
pub fn packages_in(root: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    out.sort();
    out
}

pub fn extension_dirs(local: bool) -> Vec<PathBuf> {
    pkg_dirs(local, "extensions")
}

pub fn command_dirs(local: bool) -> Vec<PathBuf> {
    pkg_dirs(local, "commands")
}

fn pkg_dirs(local: bool, sub: &str) -> Vec<PathBuf> {
    let root = pkg_root(local);
    let Ok(rd) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let dir = entry.path().join(sub);
        if dir.is_dir() {
            out.push(dir);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Carried, Selected, carried, keep_from, menu_items, notes, parse_source};

    #[test]
    fn carried_recognizes_every_layout() {
        let dir = crate::core::testutil::scratch_dir("carried");
        std::fs::create_dir_all(dir.join("skills/demo")).unwrap();
        std::fs::write(dir.join("skills/demo/SKILL.md"), "---\nname: demo\n---\nb").unwrap();
        std::fs::write(dir.join("skills/flat.md"), "---\nname: flat\n---\nb").unwrap();
        std::fs::create_dir_all(dir.join("extensions")).unwrap();
        std::fs::write(dir.join("extensions/wordcount.py"), "x").unwrap();
        std::fs::create_dir_all(dir.join("commands")).unwrap();
        std::fs::write(dir.join("commands/review.md"), "x").unwrap();

        let found = carried(&dir);
        assert_eq!(found.root_skill, None);
        assert_eq!(found.skills, vec!["demo", "flat"]);
        assert_eq!(found.extensions, vec!["wordcount.py"]);
        assert_eq!(found.prompts, vec!["review"]);
        assert!(
            notes(&found, &Selected::default())
                .iter()
                .any(|l| l == "skills: demo, flat")
        );

        // the repo itself becomes one skill once SKILL.md sits at the root
        std::fs::write(dir.join("SKILL.md"), "---\nname: wholegit\n---\nb").unwrap();
        let found = carried(&dir);
        assert_eq!(found.root_skill.as_deref(), Some("wholegit"));
        let narrowed = Selected {
            skills: Some(vec![String::from("wholegit")]),
            ..Default::default()
        };
        let lines = notes(&found, &narrowed);
        assert!(lines.contains(&"skills: wholegit (repo root), demo, flat".to_string()));
        assert!(lines.contains(&"only skills: wholegit".to_string()));
        // a clone with no hooks at all says why it is silent
        let empty = notes(&Carried::default(), &Selected::default());
        assert!(empty[0].contains("nothing llm can mount"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The interactive menu: one flat list in group order, and the checked
    /// indexes fold back into per-group keep lists — a group left entirely
    /// checked records nothing (the repo stays the source of truth).
    #[test]
    fn menu_and_keep_lists_fold_by_group() {
        let found = Carried {
            root_skill: Some("wholegit".to_string()),
            skills: vec!["demo".to_string()],
            extensions: vec!["hello.py".to_string(), "other.py".to_string()],
            prompts: vec!["review".to_string()],
        };
        let kinds = menu_items(&found);
        let labels: Vec<String> = kinds.iter().map(|(k, n)| format!("{k:<9} {n}")).collect();
        assert_eq!(
            labels,
            vec![
                "skill     wholegit",
                "skill     demo",
                "extension hello.py",
                "extension other.py",
                "prompt    review",
            ]
        );
        // everything checked: nothing recorded, install follows the repo
        let all: Vec<usize> = (0..kinds.len()).collect();
        let keep = keep_from(&kinds, &all);
        assert!(keep.skills.is_none() && keep.extensions.is_none() && keep.prompts.is_none());
        // unchecking one extension narrows that group; a group left fully
        // checked records nothing
        let keep = keep_from(&kinds, &[0, 1, 2, 4]);
        assert_eq!(
            keep.extensions.as_deref(),
            Some(&["hello.py".to_string()][..])
        );
        assert!(keep.skills.is_none() && keep.prompts.is_none());
        // unchecking everything records an empty keep list per group (the
        // clone stays for the other groups, this one mounts nothing)
        let keep = keep_from(&kinds, &[]);
        assert_eq!(keep.skills.as_deref(), Some(&[][..]));
        assert_eq!(keep.extensions.as_deref(), Some(&[][..]));
    }

    #[test]
    fn github_forms_map_to_https() {
        let (url, ref_) = parse_source("git:github.com/user/repo").unwrap();
        assert_eq!(url, "https://github.com/user/repo");
        assert_eq!(ref_, None);
        let (url, ref_) = parse_source("git:github.com/user/repo@v1.2").unwrap();
        assert_eq!(url, "https://github.com/user/repo");
        assert_eq!(ref_.as_deref(), Some("v1.2"));
        let (url, _) = parse_source("git:git@github.com:user/repo").unwrap();
        assert_eq!(url, "https://github.com/user/repo");
    }

    #[test]
    fn ref_split_takes_the_last_at() {
        // an @ inside the path (rare) stays in the URL
        let (url, ref_) = parse_source("git:github.com/us@er/repo").unwrap();
        assert_eq!(url, "https://github.com/us");
        assert_eq!(ref_.as_deref(), Some("er/repo"));
    }

    #[test]
    fn unknown_schemes_are_rejected() {
        assert!(parse_source("npm:@foo/the reference-tools").is_none());
        assert!(parse_source("ssh://git@github.com/u/r").is_none());
    }
}
