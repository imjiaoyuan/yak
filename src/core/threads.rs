//! The conversation store: one JSONL file per thread under
//! `~/.llm/threads/<ulid>.jsonl`, one turn object per line. Resume is
//! the reference shape: a thread id reopens the file, nothing else.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::config;

/// Attachment provenance, metadata only — thread files store no bytes, and
/// resume replays text only.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct StoredAttachment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// base64 payload (present when the attachment's content was loaded)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct StoredToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// A stored wire-level message (user/assistant/tool/summary), mirroring
/// `providers::Msg` — attachment payloads ride as base64 on the user/tool
/// variants so a resume rebuilds the exact conversation.
#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum StoredMsg {
    User {
        text: String,
        #[serde(default)]
        attachments: Vec<StoredAttachment>,
    },
    Assistant {
        text: String,
        #[serde(default)]
        tool_calls: Vec<StoredToolCall>,
        /// reasoning trace (thinking models); replayed to gateways that
        /// require it back
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
    },
    Tool {
        call_id: String,
        name: String,
        content: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        attachments: Vec<StoredAttachment>,
    },
    Summary {
        text: String,
    },
}

/// One completed turn, appended as a single JSON line.
#[derive(Serialize, Deserialize)]
pub struct StoredTurn {
    /// turn id (ulid)
    pub id: String,
    /// `db::now_turn_datetime()`
    pub ts: String,
    /// "agent" or "prompt" — provenance for the logs list
    pub mode: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// system prompt, present on the first turn only
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// first user text of the round (for list previews and prompt resume)
    #[serde(default)]
    pub prompt: String,
    /// final assistant text (empty when the round ended mid-tools)
    #[serde(default)]
    pub response: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<(u64, u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub options: Vec<(String, String)>,
    /// the full wire messages of the round (agent replay fidelity)
    pub messages: Vec<StoredMsg>,
}

/// One row of the browser list.
#[derive(Clone)]
pub struct ThreadSummary {
    pub id: String,
    pub turns: usize,
    pub last: String,
    pub last_prompt: String,
    /// working directory of the thread's last turn, when recorded
    pub cwd: Option<String>,
}

/// True when two recorded working directories name the same place.
fn same_dir(a: &str, b: &str) -> bool {
    Path::new(a) == Path::new(b)
}

/// The thread-file store. A thin handle over a directory; every method
/// opens what it needs and lets errors surface as `Result<_, String>`.
#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// The default store under the user directory.
    pub fn open() -> Result<Store, String> {
        Store::open_path(&config::threads_dir())
    }

    pub fn open_path(path: &Path) -> Result<Store, String> {
        fs::create_dir_all(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        Ok(Store {
            dir: path.to_path_buf(),
        })
    }

    fn thread_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }

    /// Append a turn; a None/empty thread id starts a fresh thread. Returns
    /// the thread id the turn landed in.
    pub fn append_turn(
        &self,
        thread_id: Option<&str>,
        turn: &StoredTurn,
    ) -> Result<String, String> {
        let id = match thread_id {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => crate::core::db::ulid(),
        };
        let path = self.thread_path(&id);
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot write thread {id}: {e}"))?;
        let mut line = serde_json::to_string(turn).map_err(|e| e.to_string())?;
        line.push('\n');
        // one write() under O_APPEND is atomic: concurrent processes can
        // append to the same thread without interleaving a line
        let bytes = line.as_bytes();
        let written = f
            .write(bytes)
            .map_err(|e| format!("cannot write thread {id}: {e}"))?;
        if written != bytes.len() {
            return Err(format!("short write on thread {id}"));
        }
        Ok(id)
    }

    /// Read every turn of a thread, oldest first. Corrupt tail lines are
    /// skipped rather than aborting the whole read.
    pub fn read_thread(&self, id: &str) -> Result<Vec<StoredTurn>, String> {
        let path = self.thread_path(id);
        let text =
            fs::read_to_string(&path).map_err(|e| format!("cannot read thread {id}: {e}"))?;
        let mut turns = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(t) = serde_json::from_str::<StoredTurn>(line) {
                turns.push(t);
            }
        }
        Ok(turns)
    }

    /// The newest thread id by file mtime, restricted to `cwd` when given.
    /// The scoped walk reads each thread file once (for its directory) and
    /// then only stats them.
    pub fn latest_thread(&self, cwd: Option<&str>) -> Result<Option<String>, String> {
        let local: Option<HashSet<String>> = cwd.map(|cwd| {
            self.summaries()
                .into_iter()
                .filter(|t| t.cwd.as_deref().is_some_and(|c| same_dir(c, cwd)))
                .map(|t| t.id)
                .collect()
        });
        let mut best: Option<(std::time::SystemTime, String)> = None;
        for entry in self.entries()? {
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(m) = meta.modified() else { continue };
            if best.as_ref().is_some_and(|(t, _)| m <= *t) {
                continue;
            }
            let id = entry_id(&entry)?;
            if local.as_ref().is_some_and(|local| !local.contains(&id)) {
                continue;
            }
            best = Some((m, id));
        }
        Ok(best.map(|(_, id)| id))
    }

    /// Resolve a full id or an unambiguous prefix. When a prefix matches
    /// several threads, one that ran in `cwd` wins; None = no match or
    /// ambiguous (the caller prints the combined message).
    pub fn resolve_thread(
        &self,
        prefix: &str,
        cwd: Option<&str>,
    ) -> Result<Option<String>, String> {
        let mut hits: Vec<String> = Vec::new();
        for entry in self.entries()? {
            let id = entry_id(&entry)?;
            if id == prefix {
                return Ok(Some(id));
            }
            if id.starts_with(prefix) {
                hits.push(id);
            }
        }
        if hits.len() <= 1 {
            return Ok(hits.into_iter().next());
        }
        if let Some(cwd) = cwd {
            let local: Vec<String> = hits
                .iter()
                .filter(|id| self.ran_in_dir(id, cwd))
                .cloned()
                .collect();
            if local.len() == 1 {
                return Ok(local.into_iter().next());
            }
        }
        Ok(None)
    }

    /// True when the thread's last turn ran in `cwd`.
    fn ran_in_dir(&self, id: &str, cwd: &str) -> bool {
        self.summarize(id)
            .and_then(|t| t.cwd)
            .is_some_and(|c| same_dir(&c, cwd))
    }

    /// Recent threads, newest first (by last-turn timestamp), capped at
    /// `limit`. A `limit` of `usize::MAX` lists everything; `cwd` restricts
    /// the list to conversations that ran in that directory.
    pub fn recent_threads(&self, limit: usize, cwd: Option<&str>) -> Vec<ThreadSummary> {
        let mut all = self.summaries();
        all.retain(|t| match cwd {
            Some(cwd) => t.cwd.as_deref().is_some_and(|c| same_dir(c, cwd)),
            None => true,
        });
        all.sort_by(|a, b| b.last.cmp(&a.last));
        all.truncate(limit);
        all
    }

    /// Every thread, unsorted. Reads each file once and parses only its
    /// last line (the only turn the summary needs), so `llm logs` stays
    /// cheap however long the conversations grow.
    pub fn summaries(&self) -> Vec<ThreadSummary> {
        let Ok(entries) = self.entries() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries {
            let Ok(id) = entry_id(&entry) else { continue };
            if let Some(summary) = self.summarize(&id) {
                out.push(summary);
            }
        }
        out
    }

    fn summarize(&self, id: &str) -> Option<ThreadSummary> {
        let text = fs::read_to_string(self.thread_path(id)).ok()?;
        let mut turns = 0usize;
        let mut last_line: Option<&str> = None;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            turns += 1;
            last_line = Some(line);
        }
        if turns == 0 {
            return None;
        }
        let last: StoredTurn = serde_json::from_str(last_line?).ok()?;
        Some(ThreadSummary {
            id: id.to_string(),
            turns,
            last: last.ts,
            last_prompt: last.prompt,
            cwd: last.cwd,
        })
    }

    /// Branch a thread onto a fresh id sharing its turns so far.
    pub fn fork_thread(&self, source: &str) -> Result<Option<String>, String> {
        let src = self.thread_path(source);
        if !src.exists() {
            return Ok(None);
        }
        let new_id = crate::core::db::ulid();
        let dst = self.thread_path(&new_id);
        fs::copy(&src, &dst).map_err(|e| format!("cannot fork thread {source}: {e}"))?;
        Ok(Some(new_id))
    }

    /// Keep only the first `keep` turns of a thread (the `/tree` jump):
    /// the file is rewritten, so the dropped turns are gone for good.
    pub fn truncate_thread(&self, id: &str, keep: usize) -> Result<(), String> {
        let turns = self.read_thread(id)?;
        if keep >= turns.len() {
            return Ok(());
        }
        let mut out = String::new();
        for turn in &turns[..keep] {
            out.push_str(&serde_json::to_string(turn).map_err(|e| e.to_string())?);
            out.push('\n');
        }
        let path = self.thread_path(id);
        fs::write(&path, out).map_err(|e| format!("cannot rewrite thread {id}: {e}"))
    }

    fn entries(&self) -> Result<Vec<fs::DirEntry>, String> {
        let rd = fs::read_dir(&self.dir)
            .map_err(|e| format!("cannot list {}: {e}", self.dir.display()))?;
        let mut out = Vec::new();
        for entry in rd {
            match entry {
                Ok(e) => out.push(e),
                Err(_) => continue,
            }
        }
        Ok(out)
    }
}

fn entry_id(entry: &fs::DirEntry) -> Result<String, String> {
    let path = entry.path();
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return Err("not a thread file".to_string());
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .ok_or_else(|| "thread file without a stem".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: &str, prompt: &str, response: &str, mode: &str) -> StoredTurn {
        StoredTurn {
            id: id.to_string(),
            ts: format!("2026-08-23T0{}:00:00+00:00", id),
            mode: mode.to_string(),
            model: "prov/m".to_string(),
            cwd: None,
            system: None,
            prompt: prompt.to_string(),
            response: response.to_string(),
            reasoning: None,
            usage: None,
            duration_ms: None,
            options: Vec::new(),
            messages: vec![StoredMsg::User {
                text: prompt.to_string(),
                attachments: Vec::new(),
            }],
        }
    }

    /// The same turn, stamped with the directory it ran in.
    fn turn_in(id: &str, cwd: &str, prompt: &str) -> StoredTurn {
        let mut t = turn(id, prompt, "ans", "agent");
        t.cwd = Some(cwd.to_string());
        t
    }

    /// Append `turns` and pin the thread file's mtime `age_secs` into the
    /// past, so mtime ordering is explicit instead of filesystem-grained.
    fn seed(store: &Store, turns: &[StoredTurn], age_secs: u64) -> String {
        let id = store.append_turn(None, &turns[0]).unwrap();
        for t in &turns[1..] {
            store.append_turn(Some(&id), t).unwrap();
        }
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
        let file = fs::File::options()
            .write(true)
            .open(store.thread_path(&id))
            .unwrap();
        file.set_modified(old).unwrap();
        id
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("llm-threads-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn appends_and_reads_back_in_order() {
        let dir = scratch("order");
        let store = Store::open_path(&dir).unwrap();
        let id = store
            .append_turn(None, &turn("1", "one", "ans1", "prompt"))
            .unwrap();
        store
            .append_turn(Some(&id), &turn("2", "two", "ans2", "prompt"))
            .unwrap();
        let turns = store.read_thread(&id).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].prompt, "one");
        assert_eq!(turns[1].prompt, "two");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_exact_and_unique_prefix() {
        let dir = scratch("r");
        let store = Store::open_path(&dir).unwrap();
        let id = store
            .append_turn(None, &turn("1", "a", "b", "agent"))
            .unwrap();
        assert_eq!(
            store.resolve_thread(&id, None).unwrap().as_deref(),
            Some(id.as_str())
        );
        assert_eq!(
            store.resolve_thread(&id[..6], None).unwrap().as_deref(),
            Some(id.as_str())
        );
        assert_eq!(store.resolve_thread("zzz", None).unwrap(), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_prefix_matching_two_threads_prefers_this_directory() {
        let dir = scratch("prefix");
        let store = Store::open_path(&dir).unwrap();
        let mine = seed(&store, &[turn_in("1", "/p/mine", "a")], 0);
        let theirs = seed(&store, &[turn_in("2", "/p/theirs", "b")], 0);
        let shared = mine
            .chars()
            .zip(theirs.chars())
            .take_while(|(a, b)| a == b)
            .count();
        assert!(shared >= 4, "ids share no usable prefix: {mine} {theirs}");
        let prefix = &mine[..shared];
        assert_eq!(store.resolve_thread(prefix, None).unwrap(), None);
        assert_eq!(
            store
                .resolve_thread(prefix, Some("/p/mine"))
                .unwrap()
                .as_deref(),
            Some(mine.as_str())
        );
        assert_eq!(
            store
                .resolve_thread(prefix, Some("/p/theirs"))
                .unwrap()
                .as_deref(),
            Some(theirs.as_str())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_threads_are_scoped_to_one_directory() {
        let dir = scratch("scope");
        let store = Store::open_path(&dir).unwrap();
        let old = seed(&store, &[turn_in("1", "/p/a", "old")], 120);
        let here = seed(&store, &[turn_in("2", "/p/b", "here")], 60);
        let untagged = seed(&store, &[turn("3", "legacy", "", "agent")], 0);
        let ids =
            |ts: Vec<ThreadSummary>| -> Vec<String> { ts.into_iter().map(|t| t.id).collect() };
        assert_eq!(
            ids(store.recent_threads(30, None)),
            vec![untagged, here, old.clone()]
        );
        assert_eq!(
            ids(store.recent_threads(30, Some("/p/a"))),
            vec![old.clone()]
        );
        // a trailing slash or `.` component is the same directory
        assert_eq!(
            ids(store.recent_threads(30, Some("/p/a/"))),
            vec![old.clone()]
        );
        assert_eq!(
            ids(store.recent_threads(30, Some("/p/./a"))),
            vec![old.clone()]
        );
        // a different directory is filtered out
        assert!(store.recent_threads(30, Some("/p/c")).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_thread_can_be_scoped_to_one_directory() {
        let dir = scratch("latest");
        let store = Store::open_path(&dir).unwrap();
        let old = seed(&store, &[turn_in("1", "/p/a", "old")], 120);
        let here = seed(&store, &[turn_in("2", "/p/b", "here")], 60);
        assert_eq!(
            store.latest_thread(Some("/p/b")).unwrap().as_deref(),
            Some(here.as_str())
        );
        assert_eq!(
            store.latest_thread(Some("/p/a")).unwrap().as_deref(),
            Some(old.as_str())
        );
        assert_eq!(store.latest_thread(Some("/p/nowhere")).unwrap(), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn summaries_carry_the_last_turns_directory() {
        let dir = scratch("cwd");
        let store = Store::open_path(&dir).unwrap();
        let id = seed(
            &store,
            &[turn_in("1", "/p/a", "first"), turn_in("2", "/p/b", "moved")],
            0,
        );
        let summaries = store.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, id);
        assert_eq!(summaries[0].turns, 2);
        assert_eq!(summaries[0].last_prompt, "moved");
        assert_eq!(summaries[0].cwd.as_deref(), Some("/p/b"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fork_copies_the_thread() {
        let dir = scratch("f");
        let store = Store::open_path(&dir).unwrap();
        let id = store
            .append_turn(None, &turn("1", "a", "b", "agent"))
            .unwrap();
        let forked = store.fork_thread(&id).unwrap().unwrap();
        assert_ne!(forked, id);
        assert_eq!(store.read_thread(&forked).unwrap().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
