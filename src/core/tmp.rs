//! Housekeeping for `~/.llm/tmp`: the scratch dir the editor drops pasted
//! clipboard images and ctrl+g buffers into. Thread files keep an attachment's
//! path as provenance, never its bytes, so nothing else ever revisits these
//! files — without a prune the directory grows for the life of the install.
//! Each agent start sweeps what has aged past the retention window.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// A week: long enough that resuming a thread from earlier in the week still
/// finds the images it references, short enough that the dir stays bounded.
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Delete regular files in `dir` last modified more than `retention` before
/// `now`; returns how many went. A missing dir is simply nothing to do, and
/// an entry that cannot be stat'ed or removed is left alone rather than
/// failing the session that is only doing housekeeping.
pub fn prune_before(dir: &Path, now: SystemTime, retention: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let age = now.duration_since(modified).unwrap_or_default();
        if age > retention && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Prune the user's tmp dir against the wall clock.
pub fn prune_user_tmp() -> usize {
    prune_before(
        &crate::core::config::user_dir().join("tmp"),
        SystemTime::now(),
        RETENTION,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_drops_aged_files_and_keeps_fresh_ones() {
        let dir = crate::core::testutil::scratch_dir("tmp-prune");
        std::fs::write(dir.join("paste-fresh.png"), b"x").unwrap();
        std::fs::write(dir.join("paste-old.png"), b"x").unwrap();
        std::fs::create_dir(dir.join("inner")).unwrap();
        // the fresh file is created now, so a clock one retention past it is
        // exactly what "aged" means — no sleeping, no mtime surgery
        let now = SystemTime::now() + RETENTION + Duration::from_secs(60);
        assert_eq!(prune_before(&dir, now, RETENTION), 2);
        assert!(!dir.join("paste-fresh.png").exists());
        assert!(!dir.join("paste-old.png").exists());
        assert!(
            dir.join("inner").is_dir(),
            "directories are not ours to cut"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_to_prune_is_not_an_error() {
        let dir = crate::core::testutil::scratch_path("tmp-absent");
        assert_eq!(prune_before(&dir, SystemTime::now(), RETENTION), 0);
    }

    #[test]
    fn a_file_inside_the_window_survives() {
        let dir = crate::core::testutil::scratch_dir("tmp-keep");
        std::fs::write(dir.join("paste-now.png"), b"x").unwrap();
        assert_eq!(prune_before(&dir, SystemTime::now(), RETENTION), 0);
        assert!(dir.join("paste-now.png").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
