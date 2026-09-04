//! File checkpoints for two-way undo: before the first write/edit touch of
//! a round, the target's bytes are copied under the session's checkpoint
//! root, so `/undo` restores files together with the conversation round.
//! bash-modified files are outside this net (its approval prompt is the
//! gate); snapshots are plain copies under the user directory — a per-round
//! rewind, not a VCS.

use std::path::{Path, PathBuf};

/// rounds kept before the oldest snapshot dirs are trimmed away
const KEEP_ROUNDS: usize = 10;

pub struct CheckpointState {
    /// `<user_dir>/checkpoints/<ulid>`: one tree per interactive session
    root: PathBuf,
    /// the round in progress (begin_round..end_round, one per run_task)
    current: Option<RoundUndo>,
    /// completed rounds, oldest first; file-less rounds stay in the stack
    /// so `/undo` pops the round the user actually just saw
    rounds: Vec<RoundUndo>,
    /// monotonic dir counter — never reused, so trims cannot collide
    next_dir: usize,
}

struct RoundUndo {
    /// seed length when the round began: `/undo` truncates back to it
    seed_len: usize,
    dir_no: usize,
    files: Vec<FileUndo>,
}

struct FileUndo {
    /// the original location, already resolved
    path: PathBuf,
    /// the pre-round copy; empty when the file did not exist (undo deletes)
    snapshot: PathBuf,
}

impl CheckpointState {
    pub fn new(root: PathBuf) -> CheckpointState {
        CheckpointState {
            root,
            current: None,
            rounds: Vec::new(),
            next_dir: 0,
        }
    }

    /// A round begins at conversation length `seed_len`.
    pub fn begin_round(&mut self, seed_len: usize) {
        let dir_no = self.next_dir;
        self.next_dir += 1;
        self.current = Some(RoundUndo {
            seed_len,
            dir_no,
            files: Vec::new(),
        });
    }

    /// First-touch snapshot: copies the pre-round bytes; a second touch of
    /// the same path in one round is ignored (the first snapshot is the
    /// state `/undo` restores to). Best-effort — on any io failure the path
    /// is skipped and the round stays conversation-undoable only.
    pub fn snapshot(&mut self, path: &Path) {
        let Some(r) = self.current.as_ref() else {
            return;
        };
        if r.files.iter().any(|f| f.path == path) || path.is_dir() {
            return;
        }
        let dir = self.round_dir(r.dir_no);
        let file_no = r.files.len();
        let Some(round) = self.current.as_mut() else {
            return;
        };
        if path.is_file() {
            let _ = std::fs::create_dir_all(&dir);
            let snap = dir.join(format!("f{file_no}"));
            if std::fs::copy(path, &snap).is_err() {
                return;
            }
            round.files.push(FileUndo {
                path: path.to_path_buf(),
                snapshot: snap,
            });
        } else {
            round.files.push(FileUndo {
                path: path.to_path_buf(),
                snapshot: PathBuf::new(),
            });
        }
    }

    /// Close the round; it joins the stack even when nothing was touched,
    /// keeping the stack aligned with the conversation rounds.
    pub fn end_round(&mut self) {
        if let Some(round) = self.current.take() {
            if round.files.is_empty() {
                let _ = std::fs::remove_dir_all(self.round_dir(round.dir_no));
            }
            self.rounds.push(round);
            self.trim();
        }
    }

    /// Pop the newest round, restore its files, drop its snapshots.
    /// Returns the conversation length to truncate to and how many files
    /// were restored.
    pub fn undo(&mut self) -> Option<(usize, usize)> {
        let round = self.rounds.pop()?;
        for f in round.files.iter().rev() {
            if f.snapshot.as_os_str().is_empty() {
                let _ = std::fs::remove_file(&f.path);
            } else {
                let _ = std::fs::copy(&f.snapshot, &f.path);
            }
        }
        let n = round.files.len();
        let _ = std::fs::remove_dir_all(self.round_dir(round.dir_no));
        Some((round.seed_len, n))
    }

    /// Drop every snapshot and reset the stack (`/clear`, session end).
    pub fn clear(&mut self) {
        self.current = None;
        self.rounds.clear();
        self.next_dir = 0;
        let _ = std::fs::remove_dir_all(&self.root);
    }

    fn round_dir(&self, no: usize) -> PathBuf {
        self.root.join(format!("r{no}"))
    }

    fn trim(&mut self) {
        while self.rounds.len() > KEEP_ROUNDS {
            let old = self.rounds.remove(0);
            let _ = std::fs::remove_dir_all(self.round_dir(old.dir_no));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("llm-ck-{}", crate::core::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn first_touch_wins_and_undo_restores_bytes() {
        let files = scratch();
        let root = scratch();
        let a = files.join("a.txt");
        std::fs::write(&a, "one").unwrap();

        let mut ck = CheckpointState::new(root.clone());
        ck.begin_round(0);
        ck.snapshot(&a);
        std::fs::write(&a, "two").unwrap();
        ck.snapshot(&a); // second touch: the pre-round "one" must survive
        ck.end_round();

        ck.begin_round(3);
        ck.end_round(); // a file-less round still occupies the stack

        let (len, files_restored) = ck.undo().unwrap();
        assert_eq!((len, files_restored), (3, 0)); // pops the empty round
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "two"); // untouched

        let (len, files_restored) = ck.undo().unwrap();
        assert_eq!((len, files_restored), (0, 1));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "one"); // rewound
        assert!(ck.undo().is_none());
        let _ = std::fs::remove_dir_all(&files);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn new_file_marker_deletes_on_undo() {
        let files = scratch();
        let root = scratch();
        let b = files.join("new.txt");

        let mut ck = CheckpointState::new(root);
        ck.begin_round(2);
        ck.snapshot(&b); // does not exist yet
        ck.end_round();
        std::fs::write(&b, "created by the round").unwrap();

        let (len, n) = ck.undo().unwrap();
        assert_eq!((len, n), (2, 1));
        assert!(!b.exists(), "undo must delete a round-created file");
        let _ = std::fs::remove_dir_all(&files);
    }

    #[test]
    fn clear_wipes_the_tree() {
        let files = scratch();
        let root = scratch();
        let c = files.join("c.txt");
        std::fs::write(&c, "x").unwrap();
        let mut ck = CheckpointState::new(root.clone());
        ck.begin_round(0);
        ck.snapshot(&c);
        ck.end_round();
        assert!(root.exists());
        ck.clear();
        assert!(!root.exists());
        assert!(ck.undo().is_none());
        let _ = std::fs::remove_dir_all(&files);
    }

    #[test]
    fn trim_bounds_the_stack() {
        let files = scratch();
        let root = scratch();
        let f = files.join("f.txt");
        std::fs::write(&f, "v").unwrap();
        let mut ck = CheckpointState::new(root.clone());
        for i in 0..(KEEP_ROUNDS + 3) {
            ck.begin_round(i);
            ck.snapshot(&f);
            std::fs::write(&f, format!("v{i}")).unwrap();
            ck.end_round();
        }
        assert_eq!(ck.rounds.len(), KEEP_ROUNDS);
        assert_eq!(ck.rounds[0].seed_len, 3); // the oldest three were trimmed
        let _ = std::fs::remove_dir_all(&files);
        let _ = std::fs::remove_dir_all(&root);
    }
}
