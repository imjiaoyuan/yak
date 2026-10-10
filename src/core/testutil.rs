//! Test scratch directories. Every test that touches the filesystem needs one,
//! and hand-rolling the path at each site produced two recurring defects: the
//! same `~/.yak`-shaped layout reinvented dozens of times, and names keyed on
//! `std::process::id()` — which does not distinguish the tests running in
//! parallel threads of one test binary, so a reused prefix meant a collision.
//! These two helpers own the naming (a monotonic ULID, unique under any
//! parallelism) and the cleanup, so a site only names its own tag.

use std::path::PathBuf;

/// A fresh, empty scratch directory. Any stale directory under the same name
/// is cleared first (a previous run's leftovers must never satisfy a test that
/// expects an empty tree), then it is created. The caller owns removal; a
/// failed cleanup leaks a directory under the system temp dir, which is what
/// the operating system's temp reaper is for.
pub fn scratch_dir(tag: &str) -> PathBuf {
    let dir = scratch_path(tag);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A scratch *path* that does not exist yet: cleared like [`scratch_dir`] but
/// not created, for the tests that assert on what a missing directory does
/// (a first `Store::open_path`, a write into a tree the code is meant to
/// build itself).
pub fn scratch_path(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yak-{tag}-{}", crate::core::db::ulid()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Read one mock-server request: past the request head plus its
/// content-length body. The shared reader for every inline HTTP mock in
/// the tree (the agent-loop SSE servers, the session persistence tests).
#[cfg(test)]
pub fn read_request(c: &mut std::net::TcpStream) {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        use std::io::Read;
        if c.read(&mut byte).unwrap_or(0) == 0 {
            break;
        }
        buf.push(byte[0]);
        let head_end = buf.windows(4).rposition(|w| w == b"\r\n\r\n");
        if let Some(i) = head_end {
            let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() - i - 4 >= len {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_calls_never_share_a_directory() {
        let a = scratch_dir("same-tag");
        let b = scratch_dir("same-tag");
        assert_ne!(a, b, "the tag alone must not decide the path");
        assert!(a.is_dir() && b.is_dir());
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn scratch_path_is_not_created() {
        let dir = scratch_path("absent");
        assert!(!dir.exists(), "the caller decides when this tree exists");
    }

    #[test]
    fn the_path_carries_the_tag() {
        let dir = scratch_path("my-tag");
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("yak-my-tag-"), "{name}");
    }
}
