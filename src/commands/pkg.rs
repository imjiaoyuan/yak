//! `llm install|remove|list` — the reference packages, git-only. A package is a git
//! repository cloned under `~/.llm/pkg/<name>` (`-l`: `.llm/pkg/<name>`,
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
        "Install project-local (.llm/pkg/ instead of ~/.llm/pkg/)"
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
    let root = pkg_root(args.flag(&["local"]));
    let target = root.join(&name);
    if target.exists() {
        // refresh, unless the install is pinned to a ref the caller did
        // not repeat (a pinned clone moves only via install @new-ref)
        let pinned = git(&["config", "llm.pinned"], &target)
            .ok()
            .is_some_and(|p| !p.trim().is_empty() && ref_.as_deref() != Some(p.trim()));
        if pinned && ref_.is_none() {
            eprintln!("\x1b[2m{name} is pinned — re-run with @ref to move it\x1b[0m");
            return 0;
        }
        if let Err(e) = git(&["fetch", "--tags", "origin"], &target).and_then(|_| {
            let to = ref_.clone().unwrap_or_else(|| "origin/HEAD".to_string());
            git(&["reset", "--hard", &to], &target).map(|_| ())
        }) {
            eprintln!("Error: refresh failed: {e}");
            return 1;
        }
        eprintln!("\x1b[2mupdated {name}\x1b[0m");
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
        eprintln!("\x1b[2minstalled {name} → {}\x1b[0m", target.display());
    }
    for (dir, label) in [
        ("skills", "skill(s)"),
        ("extensions", "extension(s)"),
        ("commands", "prompt(s)"),
    ] {
        let count = std::fs::read_dir(target.join(dir))
            .map(|rd| rd.flatten().count())
            .unwrap_or(0);
        if count > 0 {
            eprintln!("\x1b[2m  {count} {label}\x1b[0m");
        }
    }
    0
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
                "\x1b[2mremoved {name} ({})\x1b[0m",
                pkg_root(local).display()
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
                "\x1b[1m{name}\x1b[0m \x1b[2m({scope} · {})\x1b[0m",
                path.display()
            );
            for (dir, label) in [
                ("skills", "skills"),
                ("extensions", "extensions"),
                ("commands", "prompts"),
            ] {
                let items = std::fs::read_dir(path.join(dir))
                    .map(|rd| {
                        rd.flatten()
                            .filter_map(|e| e.file_name().into_string().ok())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if !items.is_empty() {
                    eprintln!("  \x1b[2m{label}: {}\x1b[0m", items.join(", "));
                }
            }
        }
    }
    if !any {
        eprintln!("\x1b[2mno packages — llm install git:github.com/user/repo\x1b[0m");
    }
    0
}

/// Package-carried extension directories, nearest-first roots for the
/// discovery walks (project pkg wins over user pkg via ordering at the
/// call site).
pub fn extension_dirs(local: bool) -> Vec<PathBuf> {
    pkg_dirs(local, "extensions")
}

pub fn skill_dirs(local: bool) -> Vec<PathBuf> {
    pkg_dirs(local, "skills")
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
    use super::parse_source;

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
