use super::*;

/// The extension homes, nearest-first within a root: project (walking up
/// from cwd) then user. Same name in both → project wins.
///
/// Nothing fingerprints these directories anymore: plugin files are read
/// at startup and re-read only on `/reload` (or a restart) — the per-task
/// fingerprint probe walked every root twice each round and cost more
/// than the plugins it watched.
pub fn discover_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // packages: project pkg first, then user pkg (the same nearest-wins
    // ordering the home directories below use)
    dirs.extend(crate::commands::pkg::extension_dirs(true));
    dirs.extend(crate::commands::pkg::extension_dirs(false));
    if let Some(d) = crate::core::paths::nearest_dir_up(cwd, ".llm/extensions", true) {
        dirs.push(d);
    }
    dirs.push(crate::core::config::user_dir().join("extensions"));
    dirs
}
