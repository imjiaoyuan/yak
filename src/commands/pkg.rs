//! `yak install|remove|list` — the reference packages, git-only. A package is a git
//! repository cloned under `~/.yak/pkg/<name>` (`-l`: `.yak/pkg/<name>`,
//! project-local); its `skills/`, `extensions/` and `commands/` directories
//! mount into the normal discovery walks. No update command: re-running
//! `install` refreshes (pinned `@ref` clones are skipped by the refresh).

use std::path::{Path, PathBuf};

use crate::core::args::{OptSpec, ParsedArgs, render_help};
use crate::flag_spec;

const INSTALL_SPECS: &[OptSpec] = &[
    flag_spec!(
        "local",
        Some('l'),
        "Install project-local (.yak/pkg/ instead of ~/.yak/pkg/)"
    ),
    flag_spec!(
        "global",
        Some('g'),
        "Install into the user directory (default)"
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
                "yak install|remove|list",
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
                    "yak install|remove|list",
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
            "yak install git:github.com/user/repo[@ref]",
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
        // the last `@` is the ref separator (the `@` in `git@github.com:`
        // stays part of the URL because the part before it holds no `/`)
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
            .join(".yak/pkg")
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
        eprintln!("Error: Usage: yak install git:github.com/user/repo[@ref]");
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
    // the scope flag is the only thing that decides where the clone lands:
    // a piped install and an attended one take the same path
    let local = args.flag(&["local"]);
    let root = pkg_root(local);
    let target = root.join(&name);
    if target.exists() {
        // refresh, unless the install is pinned to a ref the caller did
        // not repeat (a pinned clone moves only via install @new-ref)
        let pinned = git(&["config", "yak.pinned"], &target)
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
            // an explicit @ref re-pins: without this the marker goes stale
            // on a move (or stays empty on a first pin) and the next plain
            // `install NAME` would move a clone the user pinned
            if let Some(ref_) = &ref_
                && let Err(e) = git(&["config", "yak.pinned", ref_], &target)
            {
                eprintln!(
                    "{}Warning: updated {name} but could not record the new pin ({e}){}",
                    p.dim, p.reset
                );
            }
            eprintln!("{}updated {name}{}", p.dim, p.reset);
        }
    } else {
        if let Err(e) = std::fs::create_dir_all(&root) {
            eprintln!("Error: cannot create {}: {e}", root.display());
            return 1;
        }
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
        // the pin marker decides whether a later refresh may move the clone:
        // a lost marker reads as "unpinned", so a failed write must say so
        // — silence here would have the next install reset a pinned clone
        let pinned_value = ref_.as_deref().unwrap_or("");
        if let Err(e) = git(&["config", "yak.pinned", pinned_value], &target) {
            eprintln!(
                "{}Warning: installed {name} but could not record its pin ({e}) — \
                 a later `yak install {name}` will refresh it like an unpinned clone{}",
                p.dim, p.reset
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
    for line in notes(&found) {
        eprintln!("{}  {line}{}", p.dim, p.reset);
    }
    0
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

/// The dim lines `install` and `list` print for one package: what it
/// carries, or why nothing mounts.
fn notes(found: &Carried) -> Vec<String> {
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
    if out.is_empty() {
        out.push(
            "nothing yak can mount — needs skills/, extensions/, commands/ or a root SKILL.md"
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
            "yak remove NAME",
            "Delete an installed package",
            REMOVE_SPECS,
            &[],
        )
    });
    let Some(args) = args else { return code };
    let Some(name) = args.positionals.first() else {
        eprintln!("Error: Usage: yak remove NAME");
        return 2;
    };
    let mut removed = false;
    let mut failed = false;
    for local in [true, false] {
        let target = pkg_root(local).join(name);
        if !target.exists() {
            continue;
        }
        // a failed removal must not report success: the package is still
        // there, and a silent 0 would say otherwise
        match std::fs::remove_dir_all(&target) {
            Ok(()) => {
                eprintln!(
                    "{}removed {name} ({}){}",
                    crate::theme::err().dim,
                    pkg_root(local).display(),
                    crate::theme::err().reset
                );
                removed = true;
            }
            Err(e) => {
                eprintln!("Error: cannot remove {}: {e}", target.display());
                failed = true;
            }
        }
    }
    if failed {
        return 1;
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
            "yak list",
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
            for line in notes(&found) {
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
            "{}no packages — yak install git:github.com/user/repo{}",
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
    use super::{Carried, carried, notes, parse_source};

    #[test]
    fn carried_recognizes_every_layout() {
        let dir = crate::core::testutil::scratch_dir("carried");
        std::fs::create_dir_all(dir.join("skills/demo")).unwrap();
        std::fs::write(
            dir.join("skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: d\n---\nb",
        )
        .unwrap();
        std::fs::write(
            dir.join("skills/flat.md"),
            "---\nname: flat\ndescription: d\n---\nb",
        )
        .unwrap();
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
            notes(&found).iter().any(|l| l == "skills: demo, flat"),
            "{:?}",
            notes(&found)
        );

        // the repo itself becomes one skill once SKILL.md sits at the root
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: wholegit\ndescription: d\n---\nb",
        )
        .unwrap();
        let found = carried(&dir);
        assert_eq!(found.root_skill.as_deref(), Some("wholegit"));
        let lines = notes(&found);
        assert!(lines.contains(&"skills: wholegit (repo root), demo, flat".to_string()));
        // a clone with no hooks at all says why it is silent
        let empty = notes(&Carried::default());
        assert!(empty[0].contains("nothing yak can mount"));
        let _ = std::fs::remove_dir_all(&dir);
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
        // the last `@` wins even when one also sits inside the path
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
