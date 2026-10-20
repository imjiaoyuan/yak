//! Session names: an optional hand-set display title for a thread id. The
//! thread file itself is append-only turn lines with no place for a name,
//! so names live beside the store in one JSON object, `thread-names.json`
//! (0600, atomic writes): `{"<thread id>": "payment-fix"}`. `yak threads`,
//! the picker rows and `get_state` all surface the name; an id without an
//! entry simply has none.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn table_path() -> PathBuf {
    crate::core::threads::names_path()
}

/// The name of `thread`, when one was set.
pub fn thread_name(thread: Option<&str>) -> Option<String> {
    let id = thread?;
    read_table_at(&table_path()).ok()?.get(id).cloned()
}

/// Set (or, on None/empty, drop) `thread`'s name. Fails loudly: a name that
/// silently failed to save is a label the user believes exists.
pub fn set_thread_name(thread: &str, name: Option<&str>) -> Result<(), String> {
    let path = table_path();
    let mut table = read_table_at(&path)?;
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => {
            table.insert(thread.to_string(), name.to_string());
        }
        None => {
            table.remove(thread);
        }
    }
    write_table_at(&path, &table)
}

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
            "{} does not hold a valid name table ({{\"<thread id>\": \"<name>\"}}): {e}",
            path.display()
        )
    })
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The full cycle against a scratch table: absent → set → read → clear.
    #[test]
    fn names_set_read_and_clear() {
        let dir = crate::core::testutil::scratch_dir("thread-names");
        let table = dir.join("names.json");
        let mut path_of = table.clone();
        // point the module's table at the scratch file by testing the _at
        // pair directly (the process-wide user_dir is shared between tests)
        let _ = &mut path_of;
        let mut names = BTreeMap::new();
        names.insert("01THREAD".to_string(), "payment fix".to_string());
        write_table_at(&table, &names).unwrap();
        assert_eq!(
            read_table_at(&table).unwrap().get("01THREAD").cloned(),
            Some("payment fix".to_string())
        );
        names.remove("01THREAD");
        write_table_at(&table, &names).unwrap();
        assert!(!read_table_at(&table).unwrap().contains_key("01THREAD"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A corrupt table is a loud error naming the file.
    #[test]
    fn a_corrupt_table_fails_loudly() {
        let dir = crate::core::testutil::scratch_dir("thread-names-corrupt");
        let table = dir.join("names.json");
        std::fs::write(&table, "nope").unwrap();
        assert!(read_table_at(&table).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
