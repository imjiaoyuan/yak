//! Shared directory-walk helpers: the `.llm/<name>` discovery walks that
//! skills, prompts/commands and drop-in tools all share.

use std::path::{Path, PathBuf};

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

/// Every directory named `<dir>` from `cwd` upward, nearest first — the
/// base for precedence rules where nearer entries override farther ones.
pub fn dirs_up(cwd: &Path, dir: &str) -> Vec<PathBuf> {
    ancestors(cwd)
        .map(|d| d.join(dir))
        .filter(|p| p.is_dir())
        .collect()
}
