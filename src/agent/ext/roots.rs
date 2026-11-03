use super::*;

/// The extension homes, nearest-first within a root: project (walking up
/// from cwd) then user. Same name in both → project wins.
///
/// The project's homes load unconditionally: a `.yak/extensions` in the
/// tree is a plugin home the moment it exists. Plugin files are read at
/// startup and re-read only on `/reload` (or a restart) — the per-task
/// fingerprint probe walked every root twice each round and cost more than
/// the plugins it watched.
pub fn discover_dirs(cwd: &Path) -> Result<Vec<PathBuf>, String> {
    let mut dirs = Vec::new();
    if let Some(d) = crate::core::paths::nearest_dir_up(cwd, ".yak/extensions", true)
        .or_else(|| crate::core::paths::nearest_dir_up(cwd, ".llm/extensions", true))
    {
        dirs.push(d);
        // project-local packages ride the same root: they are code under the
        // project's own `.yak`
        dirs.extend(crate::commands::pkg::extension_dirs(true)?);
    }
    // Packages follow project extensions, so an installed package cannot
    // shadow an extension supplied by the project.
    dirs.extend(crate::commands::pkg::extension_dirs(false)?);
    dirs.push(crate::core::config::user_dir().join("extensions"));
    Ok(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project's `.yak/extensions` is a plugin home on its own: no trust
    /// record, no opt-in — existing is enough.
    #[test]
    fn project_extensions_load_without_a_trust_record() {
        let root = crate::core::testutil::scratch_dir("ext-roots-project");
        std::fs::create_dir_all(root.join(".yak/extensions")).unwrap();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        let dirs = discover_dirs(&root.join("src/deep")).unwrap();
        assert_eq!(dirs.first(), Some(&root.join(".yak/extensions")));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
