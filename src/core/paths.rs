//! Shared directory-walk helpers: the `.llm/<name>` discovery walks that
//! skills, prompts/commands and drop-in tools all share, plus the one home
//! lookup every `~` consumer reads.

use std::path::{Path, PathBuf};

/// The platform home directory: `HOME` everywhere, falling back to
/// `USERPROFILE` on Windows. The single home lookup — path resolution,
/// display abbreviation and the user directory all read it, so they can
/// never disagree about where `~` points.
pub fn home_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| {
        if cfg!(windows) {
            std::env::var_os("USERPROFILE")
        } else {
            None
        }
    });
    home.map(PathBuf::from)
}

/// `cwd` and every ancestor, nearest first, down to the filesystem root.
/// The primitive the directory/file discovery walks share; callers that
/// stop at the git root (skills, project docs) check `.git` themselves.
pub fn ancestors(cwd: &Path) -> impl Iterator<Item = &Path> {
    std::iter::successors(Some(cwd), |p| p.parent())
}

/// The nearest directory named `<dir>` walking up from `cwd`. With
/// `stop_at_git_root` the search never leaves the repository (skills and
/// project docs stop there); without it the walk reaches the filesystem
/// root (agent definitions and command files do not stop).
pub fn nearest_dir_up(cwd: &Path, dir: &str, stop_at_git_root: bool) -> Option<PathBuf> {
    for d in ancestors(cwd) {
        let candidate = d.join(dir);
        if candidate.is_dir() {
            return Some(candidate);
        }
        if stop_at_git_root && d.join(".git").exists() {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rooted(paths: &[&str]) -> std::path::PathBuf {
        let mut p = std::path::PathBuf::new();
        for seg in paths {
            p.push(seg);
        }
        p
    }

    #[test]
    fn ancestors_walk_from_the_directory_to_its_root() {
        let cwd = rooted(&["a", "b", "c"]);
        let got: Vec<_> = ancestors(&cwd).collect();
        assert_eq!(
            got,
            vec![
                rooted(&["a", "b", "c"]),
                rooted(&["a", "b"]),
                rooted(&["a"]),
                rooted(&[])
            ]
        );
    }

    #[test]
    fn nearest_dir_up_prefers_the_closest_match() {
        let dir = std::env::temp_dir().join(format!("llm-paths-{}", crate::core::db::ulid()));
        let deep = dir.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(dir.join("a").join("mark")).unwrap();
        assert_eq!(
            nearest_dir_up(&deep, "mark", false),
            Some(dir.join("a").join("mark"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_git_root_stops_the_upward_search() {
        let dir = std::env::temp_dir().join(format!("llm-paths-git-{}", crate::core::db::ulid()));
        let deep = dir.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        // the marker sits above the repo boundary: only the stop matters
        std::fs::create_dir_all(dir.join("mark")).unwrap();
        std::fs::create_dir_all(dir.join("a").join(".git")).unwrap();
        assert_eq!(nearest_dir_up(&deep, "mark", true), None);
        assert_eq!(nearest_dir_up(&deep, "mark", false), Some(dir.join("mark")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
