use super::*;

/// The extension homes, nearest-first within a root: project (walking up
/// from cwd) then user. Same name in both → project wins.
///
/// Nothing fingerprints these directories anymore: plugin files are read
/// at startup and re-read only on `/reload` (or a restart) — the per-task
/// fingerprint probe walked every root twice each round and cost more
/// than the plugins it watched.
pub fn discover_dirs(cwd: &Path) -> Result<Vec<PathBuf>, String> {
    let mut dirs = Vec::new();
    if let Some(d) = crate::core::paths::nearest_dir_up(cwd, ".yak/extensions", true)
        .or_else(|| crate::core::paths::nearest_dir_up(cwd, ".llm/extensions", true))
    {
        dirs.push(d);
    }
    // Packages follow project extensions, so an installed package cannot
    // shadow an extension supplied by the project.
    dirs.extend(crate::commands::pkg::extension_dirs(true)?);
    dirs.extend(crate::commands::pkg::extension_dirs(false)?);
    dirs.push(crate::core::config::user_dir().join("extensions"));
    Ok(dirs)
}
