//! Atomic file replacement: write a same-directory temp file, then rename it
//! over the target. A crash or power loss mid-write leaves the previous file
//! intact instead of a truncated one — which matters for `config.json` (it
//! holds API keys) and for thread files the model is told it can rely on.

use std::io;
use std::path::Path;

/// Write `bytes` to `path` atomically. `mode` sets the new file's Unix
/// permissions (`Some(0o600)` for a secret-bearing config); `None` carries
/// over whatever the target already had, so the executable bit on an edited
/// script survives the fresh inode. The temp file is removed on failure.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".{}.tmp", crate::core::db::ulid()));
    let attempt = || -> io::Result<()> {
        std::fs::write(&tmp, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let bits = mode.or_else(|| {
                std::fs::metadata(path)
                    .ok()
                    .map(|m| m.permissions().mode() & 0o7777)
            });
            if let Some(bits) = bits {
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(bits));
            }
        }
        #[cfg(not(unix))]
        let _ = mode;
        std::fs::rename(&tmp, path)
    };
    match attempt() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fsx-test-{tag}-{}", crate::core::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn replaces_content_and_leaves_no_temp_behind() {
        let dir = tmp_dir("replace");
        let path = dir.join("target");
        write_atomic(&path, b"old", None).unwrap();
        write_atomic(&path, b"new", None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["target".to_string()], "stray files: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn carries_the_mode_over_and_honours_an_explicit_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("mode");
        let path = dir.join("target");
        write_atomic(&path, b"x", Some(0o600)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
        // an explicit mode wins on the next write too
        write_atomic(&path, b"y", Some(0o644)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o644);
        // None carries the existing mode over instead of resetting it
        write_atomic(&path, b"z", None).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o644);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
