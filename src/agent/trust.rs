//! Project trust: whether the `.yak/extensions` of the working project may
//! load. A project directory is untrusted by default — its plugin files run
//! with the user's full permissions the moment they are spawned, so "cd into
//! a cloned repo" must never silently become "execute whatever that repo
//! ships". The user-level homes (`~/.yak/extensions`, installed packages)
//! are always trusted: the user put them there.
//!
//! The trust table is one JSON object in the user dir, `trusted-projects.json`
//! (0600), mapping an absolute project root to the moment it was trusted.
//! Entries are keyed by the project root (the parent of the `.yak`
//! directory extension discovery walks), so cwd inside the project resolves
//! to the same key from any depth. A renamed/moved project is simply
//! untrusted again; nothing fingerprints file contents — the decision is
//! about the *user vouching for a directory*, not about tracking what it
//! contains.
//!
//! Failure discipline: a corrupt table is a loud error, never a silently
//! wiped or freshly trusted one (the same rule config.json follows).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The trust table file: `<user_dir>/trusted-projects.json`.
fn table_path() -> PathBuf {
    crate::core::config::user_dir().join("trusted-projects.json")
}

/// The key a project is stored under: its root directory, absolute. None
/// when the path is not absolute (a relative cwd cannot be vouched for
/// coherently across runs).
fn key_of(project: &Path) -> Option<String> {
    project.is_absolute().then(|| project.display().to_string())
}

/// The `.yak` (or legacy `.llm`) directory of `cwd`'s project when one
/// exists walking up — the same walk extension discovery uses, so trust
/// checks and discovery can never disagree about what "this project" is.
pub fn project_of(cwd: &Path) -> Option<PathBuf> {
    crate::core::paths::nearest_dir_up(cwd, ".yak", true)
        .or_else(|| crate::core::paths::nearest_dir_up(cwd, ".llm", true))
}

/// The read table, parsed loudly: a corrupt file is an error naming the
/// path, an absent file is an empty table.
fn read_table_at(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    serde_json::from_str(&raw).map_err(|e| {
        format!(
            "{} does not hold a valid trust table ({{\"<project path>\": \"<date>\"}}): {e}",
            path.display()
        )
    })
}

/// Write the table back: atomic, 0600 (it names where this user runs code
/// from — as private as the config that holds keys).
fn write_table_at(path: &Path, table: &BTreeMap<String, String>) -> Result<(), String> {
    let body =
        crate::jsonfmt::dumps_indent(&serde_json::to_value(table).expect("map serializes"), 2)
            + "\n";
    if let Some(dir) = path.parent() {
        crate::core::fsx::create_private_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    crate::core::fsx::write_atomic(path, body.as_bytes(), Some(0o600))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Does `cwd`'s project have plugin homes, and may they load? None = no
/// project homes exist at all (nothing to gate). The check reads the table
/// loudly: a corrupt table refuses the load rather than guessing.
pub(crate) fn trusted_at(table: &Path, cwd: &Path) -> Result<Option<bool>, String> {
    let Some(project) = project_of(cwd) else {
        return Ok(None);
    };
    // no extensions home: the question is moot, and the user must not be
    // asked to trust a directory that ships nothing
    if !project.join("extensions").is_dir() && !project.join("pkg").is_dir() {
        return Ok(None);
    }
    let Some(key) = key_of(project.parent().unwrap_or(&project)) else {
        return Ok(Some(false));
    };
    let table = read_table_at(table)?;
    Ok(Some(table.contains_key(&key)))
}

/// Is this project's plugin surface trusted to load? None = no project
/// homes exist at all (nothing to gate).
pub fn is_trusted(cwd: &Path) -> Result<Option<bool>, String> {
    trusted_at(&table_path(), cwd)
}

/// Record the user's trust for `cwd`'s project. Fails loudly on a corrupt
/// table or an unwritable user dir — a trust grant that silently failed is
/// the user believing code is vetted when it is not.
pub fn trust(cwd: &Path) -> Result<String, String> {
    let project = project_of(cwd).ok_or("no project directory (.yak) here to trust")?;
    let root = project.parent().unwrap_or(&project).to_path_buf();
    let key = key_of(&root).ok_or("the project path is not absolute")?;
    let path = table_path();
    let mut table = read_table_at(&path)?;
    table.insert(key.clone(), crate::core::db::now_turn_datetime());
    write_table_at(&path, &table)?;
    Ok(key)
}

/// Drop the recorded trust for `cwd`'s project (None when it was never
/// trusted). Idempotent.
pub fn untrust(cwd: &Path) -> Result<Option<String>, String> {
    let Some(project) = project_of(cwd) else {
        return Ok(None);
    };
    let root = project.parent().unwrap_or(&project).to_path_buf();
    let Some(key) = key_of(&root) else {
        return Ok(None);
    };
    let path = table_path();
    let mut table = read_table_at(&path)?;
    Ok(table.remove(&key).map(|_| {
        let _ = write_table_at(&path, &table);
        key
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key is the project root (the `.yak` parent), absolute only.
    #[test]
    fn keys_name_the_project_root() {
        let root = crate::core::testutil::scratch_dir("trust-keys");
        let yak = root.join(".yak");
        std::fs::create_dir_all(&yak).unwrap();
        assert_eq!(key_of(&root).as_deref(), Some(root.to_str().unwrap()));
        assert_eq!(key_of(Path::new("relative/.yak")), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A cwd inside the project resolves to the project's `.yak` walking up,
    /// stopping at the git root like discovery does.
    #[test]
    fn project_of_walks_up_to_the_yak_dir() {
        let root = crate::core::testutil::scratch_dir("trust-project");
        let yak = root.join(".yak");
        std::fs::create_dir_all(yak.join("deep/inside")).unwrap();
        assert_eq!(
            project_of(&root.join(".yak/deep/inside")),
            Some(yak.clone())
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The gate: no homes → None (nothing to ask about); homes and no table
    /// entry → false; the entry → true; a corrupt table is a loud error.
    #[test]
    fn trusted_at_reads_the_three_states_loudly() {
        let root = crate::core::testutil::scratch_dir("trust-states");
        let yak = root.join(".yak");
        std::fs::create_dir_all(yak.join("extensions")).unwrap();
        let table = root.join("table.json");

        // homes exist, nothing recorded: untrusted
        assert_eq!(trusted_at(&table, &yak).unwrap(), Some(false));
        // deeper cwd resolves to the same project
        std::fs::create_dir_all(root.join("src/bin")).unwrap();
        assert_eq!(
            trusted_at(&table, &root.join("src/bin")).unwrap(),
            Some(false)
        );

        // recorded: trusted
        std::fs::write(&table, format!("{{\"{}\": \"today\"}}", root.display())).unwrap();
        assert_eq!(
            trusted_at(&table, &root.join("src/bin")).unwrap(),
            Some(true)
        );

        // corrupt: an error, never a guess
        std::fs::write(&table, "not json").unwrap();
        assert!(trusted_at(&table, &yak).is_err());

        // a project with no plugin homes: nothing to gate
        let bare = crate::core::testutil::scratch_dir("trust-bare");
        std::fs::create_dir_all(bare.join("src")).unwrap();
        assert_eq!(trusted_at(&table, &bare.join("src")).unwrap(), None);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&bare);
    }
}
