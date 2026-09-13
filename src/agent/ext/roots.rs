use super::*;

/// The extension homes, nearest-first within a root: project (walking up
/// from cwd) then user. Same name in both → project wins.
/// The extension homes, nearest-first within a root: project (walking up
/// from cwd) then user. Same name in both → project wins.
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

/// The plugin-surface roots live-reload watches: every extension root
/// ([`discover_dirs`]) and skill root (the same walk `skills::discover`
/// takes), plus config.json (settings, disabled extensions, tool policies).
pub fn fingerprint_roots(cwd: &Path) -> (Vec<PathBuf>, PathBuf) {
    let mut roots = discover_dirs(cwd);
    roots.extend(crate::commands::pkg::skill_dirs(true));
    roots.extend(crate::commands::pkg::skill_dirs(false));
    let user = crate::core::config::user_dir();
    roots.push(user.join(".agents/skills"));
    roots.push(user.join("skills"));
    if let Some(d) = crate::core::paths::nearest_dir_up(cwd, ".agents/skills", true) {
        roots.push(d);
    }
    if let Some(d) = crate::core::paths::nearest_dir_up(cwd, ".llm/skills", true) {
        roots.push(d);
    }
    (roots, crate::core::config::config_path())
}

/// A cheap change probe over the plugin surfaces. The REPL snapshots this
/// at startup and re-runs the /reload path at a task boundary whenever the
/// value moves, so a file the agent (or the user) drops into an extensions
/// or skills dir mid-session is live on the next task — the reference's
/// reload-runtime without the manual step.
pub fn plugin_fingerprint(cwd: &Path) -> u64 {
    let (roots, config) = fingerprint_roots(cwd);
    hash_roots(&roots, &config)
}

/// One root's fingerprint input: its name, mtime, length and the name/mtime
/// of every child one level down (skills live in subdirectories).
pub(super) type RootEntry = (String, u64, u64, Vec<(String, u64)>);

pub(super) fn hash_roots(roots: &[PathBuf], config: &Path) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let stamp = |p: &Path| -> (u64, u64) {
        match std::fs::metadata(p) {
            Ok(m) => (
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                m.len(),
            ),
            Err(_) => (0, 0),
        }
    };
    let mut h = DefaultHasher::new();
    let mut roots: Vec<&PathBuf> = roots.iter().collect();
    roots.sort();
    for root in roots {
        h.write(root.as_os_str().as_encoded_bytes());
        let Ok(rd) = std::fs::read_dir(root) else {
            h.write_u8(0); // absent root: a stable placeholder
            continue;
        };
        // every entry of the root, and — skills live one level down — each
        // subdirectory's immediate children; deeper trees stop mattering
        let mut entries: Vec<RootEntry> = Vec::new();
        for e in rd.flatten() {
            let (m, len) = stamp(&e.path());
            let mut children = Vec::new();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && let Ok(sub) = std::fs::read_dir(e.path())
            {
                for c in sub.flatten() {
                    let (cm, _) = stamp(&c.path());
                    children.push((c.file_name().to_string_lossy().into_owned(), cm));
                }
                children.sort();
            }
            entries.push((
                e.file_name().to_string_lossy().into_owned(),
                m,
                len,
                children,
            ));
        }
        entries.sort();
        for (name, m, len, children) in entries {
            h.write(name.as_bytes());
            h.write_u64(m);
            h.write_u64(len);
            for (cn, cm) in children {
                h.write(cn.as_bytes());
                h.write_u64(cm);
            }
        }
    }
    let (cm, clen) = stamp(config);
    h.write(config.as_os_str().as_encoded_bytes());
    h.write_u64(cm);
    h.write_u64(clen);
    h.finish()
}
