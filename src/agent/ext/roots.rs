use super::*;

/// The extension homes, nearest-first within a root: project (walking up
/// from cwd) then user. Same name in both → project wins.
///
/// The project's homes load only when the user has vouched for the project
/// (see `agent::trust`): plugin files execute with this process's
/// permissions, so a cloned repository must not turn into code execution by
/// a mere `cd`. Untrusted, the homes are skipped and the refusal is reported
/// back so the entry point can say how to trust the project; a corrupt
/// trust table is a loud error, never a quietly empty project surface.
///
/// Nothing fingerprints these directories anymore: plugin files are read
/// at startup and re-read only on `/reload` (or a restart) — the per-task
/// fingerprint probe walked every root twice each round and cost more
/// than the plugins it watched.
pub fn discover_dirs(cwd: &Path) -> Result<Vec<PathBuf>, String> {
    let mut dirs = Vec::new();
    let project_untrusted = match crate::agent::trust::is_trusted(cwd)? {
        // no project homes exist: nothing to gate
        None => false,
        Some(true) => false,
        Some(false) => true,
    };
    if project_untrusted {
        eprintln!(
            "{}project extensions not loaded — this project is not trusted; run /trust (or remove its .yak/extensions){}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
    } else if let Some(d) = crate::core::paths::nearest_dir_up(cwd, ".yak/extensions", true)
        .or_else(|| crate::core::paths::nearest_dir_up(cwd, ".llm/extensions", true))
    {
        dirs.push(d);
        // project-local packages ride the same trust decision: they are
        // cloned code under the project's own `.yak`
        dirs.extend(crate::commands::pkg::extension_dirs(true)?);
    }
    // Packages follow project extensions, so an installed package cannot
    // shadow an extension supplied by the project.
    dirs.extend(crate::commands::pkg::extension_dirs(false)?);
    dirs.push(crate::core::config::user_dir().join("extensions"));
    Ok(dirs)
}
