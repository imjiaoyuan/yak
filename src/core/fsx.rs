//! Atomic file replacement: write a same-directory temp file, then rename it
//! over the target. A crash or power loss mid-write leaves the previous file
//! intact instead of a truncated one — which matters for `config.json` (it
//! holds API keys) and for thread files the model is told it can rely on.
//! Also the two private-write helpers: everything under the user directory
//! (transcripts, pasted images, spilled command output) is the user's alone.

use std::io;
use std::io::Write;
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

/// Create `path` and its parents with owner-only access (0700 on Unix).
/// `create_dir_all` leaves the umask's 0755, which on a shared machine lets
/// another local account list the directory and read what is in it — the
/// user dir holds API keys, conversation transcripts and pasted screenshots.
/// An existing directory is tightened too, so an install from before this
/// rule gets fixed on the next open rather than staying exposed.
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write `bytes` to a new (or truncated) private file: 0600 from the first
/// byte, so a scratch file is never briefly world-readable. Unlike
/// [`write_atomic`] this overwrites in place: the callers are throwaway
/// buffers whose loss on a crash costs nothing.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)
}

/// Give every regular file directly under `dir` owner-only permissions
/// (0600 on Unix), returning how many changed. Existing installs hold scratch
/// files older builds wrote world-readable; tightening them is what makes the
/// new rule retroactive instead of only applying to what we write from now on.
/// Best-effort: a file that cannot be chmod'ed is left alone.
#[cfg(unix)]
pub fn tighten_files(dir: &Path) -> usize {
    use std::os::unix::fs::PermissionsExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut changed = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || meta.permissions().mode() & 0o077 == 0 {
            continue;
        }
        if std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600)).is_ok() {
            changed += 1;
        }
    }
    changed
}

#[cfg(not(unix))]
pub fn tighten_files(_dir: &Path) -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        crate::core::testutil::scratch_dir(&format!("fsx-test-{tag}"))
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

    #[cfg(unix)]
    #[test]
    fn private_dirs_and_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let base = tmp_dir("private");
        let dir = base.join("nested");
        create_private_dir_all(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // an existing 0755 directory is tightened, not left as it was
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir_all(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);

        let path = dir.join("spill.log");
        write_private(&path, b"secret output").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        write_private(&path, b"again").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "again");

        // a file an older build wrote world-readable is tightened in place
        let loose = dir.join("old.log");
        std::fs::write(&loose, b"x").unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(tighten_files(&dir), 1);
        let mode = std::fs::metadata(&loose).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(tighten_files(&dir), 0, "already tight files are untouched");
        let _ = std::fs::remove_dir_all(&base);
    }
}
