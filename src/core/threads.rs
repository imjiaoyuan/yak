//! The conversation store: one JSONL file per thread under
//! `~/.yak/threads/<ulid>.jsonl`, one turn object per line. Resume is
//! a thread id reopens the file, nothing else.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::config;
use crate::providers::Msg;

/// The on-disk turn format this binary writes and reads. The stamp lives on
/// every stored line; a line without one (written before versioning
/// existed) reads as version 0. A higher number on read means a newer yak
/// wrote the thread — refused loudly, never guessed at. Version 2 added the
/// optional `parent` field that turns a thread into a tree (a `/tree` jump
/// appends a branched turn instead of rewriting the file).
pub const THREAD_FORMAT_VERSION: u32 = 2;

/// `skip_serializing_if` for the version stamp: version 0 stays absent so
/// pre-versioning lines keep their exact shape.
fn v_is_zero(v: &u32) -> bool {
    *v == 0
}

/// One round's token usage as the transcript keeps it, positional because a
/// thread file is one long line per turn: `[input, output]` on lines written
/// before the cache counts were kept, `[input, output, cached, cached_write]`
/// since. The short form still reads — a transcript must not fail loudly over
/// a field it predates — and the split is what a resumed session's cache
/// figure needs, `input` folding the cached reads in as the wire reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TurnUsage {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub cached_write: u64,
}

impl From<crate::core::http::Usage> for TurnUsage {
    fn from(u: crate::core::http::Usage) -> TurnUsage {
        TurnUsage {
            input: u.input,
            output: u.output,
            cached: u.cached,
            cached_write: u.cached_write,
        }
    }
}

impl From<TurnUsage> for crate::core::http::Usage {
    fn from(u: TurnUsage) -> crate::core::http::Usage {
        crate::core::http::Usage {
            input: u.input,
            output: u.output,
            cached: u.cached,
            cached_write: u.cached_write,
        }
    }
}

impl Serialize for TurnUsage {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        [self.input, self.output, self.cached, self.cached_write].serialize(s)
    }
}

impl<'de> Deserialize<'de> for TurnUsage {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<TurnUsage, D::Error> {
        let fields = Vec::<u64>::deserialize(d)?;
        match fields[..] {
            [input, output] => Ok(TurnUsage {
                input,
                output,
                ..TurnUsage::default()
            }),
            [input, output, cached, cached_write] => Ok(TurnUsage {
                input,
                output,
                cached,
                cached_write,
            }),
            _ => Err(serde::de::Error::invalid_length(
                fields.len(),
                &"[input, output] or [input, output, cached, cached_write]",
            )),
        }
    }
}

/// One completed turn, appended as a single JSON line.
#[derive(Serialize, Deserialize, Debug)]
pub struct StoredTurn {
    /// storage-format stamp: [`THREAD_FORMAT_VERSION`] at write time, 0 on
    /// lines written before versioning existed
    #[serde(default, skip_serializing_if = "v_is_zero")]
    pub v: u32,
    /// turn id (ulid)
    pub id: String,
    /// the turn this one continues, when it does not simply follow the
    /// previous line: the tree edge a `/tree` jump creates. Absent on every
    /// turn of a linear thread (and on the first turn of any thread — its
    /// parent would be nothing). The active branch of a thread is the last
    /// line walked back along these edges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
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
    pub usage: Option<TurnUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub options: Vec<(String, String)>,
    /// the full wire messages of the round (agent replay fidelity)
    pub messages: Vec<Msg>,
}

/// One row of the browser list.
#[derive(Clone)]
pub struct ThreadSummary {
    pub id: String,
    /// turn lines seen by the summary scan. Exact when [`Self::turns_exact`];
    /// a lower bound otherwise, because the scan reads only a thread's tail.
    pub turns: usize,
    /// the count covers the whole file — it was small enough to read entire
    pub turns_exact: bool,
    pub last: String,
    /// what the list row quotes: the newest prompt in the scanned tail, else
    /// the newest answer's first line
    pub preview: String,
    /// working directory of the thread's last turn, when recorded
    pub cwd: Option<String>,
}

/// True when two recorded working directories name the same place.
fn same_dir(a: &str, b: &str) -> bool {
    Path::new(a) == Path::new(b)
}

/// The tail one summary reads. A thread's rows all come off the end — the
/// newest timestamp, the last turn's cwd, the preview — so only the turn
/// count needs the whole file; past this window it becomes a lower bound
/// (the list shows `N+`) rather than a full read per row. No window growth
/// hunts an older prompt: a task buried under megabytes of tool rounds is
/// beyond any bounded tail, and the last answer identifies the conversation
/// just as well.
const SUMMARY_TAIL: u64 = 256 * 1024;

/// Scan summary bytes: the turn lines they hold, the newest turn that
/// parses, and the newest prompt-bearing turn. Pieces are split on the
/// newline byte, which UTF-8 never carries inside a character, so every
/// complete piece is valid JSON-carrying text; the first piece of a window
/// that starts mid-line is a fragment — counted (it is the tail of a real
/// turn) but never parseable. A torn final line fails to parse and the
/// scan falls back to the line before it, as `read_thread` does.
fn summary_from_tail(bytes: &[u8]) -> (usize, Option<StoredTurn>, Option<String>) {
    let mut turns = 0usize;
    let mut last: Option<StoredTurn> = None;
    let mut prompt: Option<String> = None;
    for line in bytes.split(|&b| b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        turns += 1;
        if let Ok(text) = std::str::from_utf8(line)
            && let Ok(turn) = serde_json::from_str::<StoredTurn>(text)
        {
            // the newest prompt wins; a tool round's empty prompt keeps the
            // older task text standing
            if !turn.prompt.is_empty() {
                prompt = Some(turn.prompt.clone());
            }
            last = Some(turn);
        }
    }
    (turns, last, prompt)
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
        let mut opts = fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // 0600 on creation: a conversation transcript is as private as
            // the config (keys) and the input history, and only exists once
            // created here
            opts.mode(0o600);
        }
        let mut f = opts
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

    /// Read every turn of a thread, oldest first. Two strict
    /// disciplines: a corrupt line mid-file fails loudly (silently dropping
    /// a turn would break tool-call pairing on resume), while a torn final
    /// line — an append interrupted by a crash — is the one bounded repair:
    /// dropped, with a warning. A format version newer than this binary
    /// writes is refused with the upgrade path named.
    pub fn read_thread(&self, id: &str) -> Result<Vec<StoredTurn>, String> {
        let path = self.thread_path(id);
        let text =
            fs::read_to_string(&path).map_err(|e| format!("cannot read thread {id}: {e}"))?;
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let mut turns = Vec::new();
        for (n, line) in lines.iter().enumerate() {
            let turn: StoredTurn = match serde_json::from_str(line) {
                Ok(t) => t,
                Err(e) if n + 1 == lines.len() => {
                    eprintln!(
                        "Warning: thread {id}: repaired unsealed tail — dropped the incomplete \
                         last line ({e})"
                    );
                    break;
                }
                Err(e) => {
                    return Err(format!(
                        "thread {id}: corrupt turn on line {} ({e}) — refusing a damaged thread",
                        n + 1
                    ));
                }
            };
            if turn.v > THREAD_FORMAT_VERSION {
                return Err(format!(
                    "thread {id}: format v{} is newer than this yak (writes \
                     v{THREAD_FORMAT_VERSION}); upgrade yak to resume it",
                    turn.v
                ));
            }
            turns.push(turn);
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
            let Ok(id) = entry_id(&entry) else { continue };
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
            let Ok(id) = entry_id(&entry) else { continue };
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

    /// Every thread, unsorted. Reads each file's tail only — the newest
    /// turn, its cwd and the preview prompt all sit at the end — so the
    /// lists stay cheap however long the conversations grow.
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
        self.summarize_tail(id, SUMMARY_TAIL)
    }

    /// Summarize from the file's tail: seek to `cap` bytes before the end
    /// and read forward. A file smaller than the window is read entire and
    /// its turn count is exact; a larger one gives a bound.
    fn summarize_tail(&self, id: &str, cap: u64) -> Option<ThreadSummary> {
        let path = self.thread_path(id);
        let mut file = fs::File::open(&path).ok()?;
        let total = file.metadata().ok()?.len();
        let cap = cap.min(total);
        let start = total - cap;
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut bytes = Vec::with_capacity(cap as usize);
        file.read_to_end(&mut bytes).ok()?;
        let (turns, last, prompt) = summary_from_tail(&bytes);
        let last = last?;
        // the preview the lists show: the newest prompt in the window, else
        // the newest answer's first line
        let preview = prompt.or_else(|| {
            last.response
                .trim()
                .lines()
                .next()
                .map(|line| line.trim().to_string())
        });
        Some(ThreadSummary {
            id: id.to_string(),
            turns,
            turns_exact: start == 0,
            last: last.ts,
            preview: preview.unwrap_or_default(),
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

/// Walk a thread's turns back from line `leaf` to the root along `parent`
/// edges, returning the chain's indices root-first. The thread's *active
/// branch* is this walk from its last line: the newest turn written is
/// where the conversation stands, and no separate leaf marker exists to
/// keep consistent (a crash mid-append leaves the previous chain fully
/// intact, the same torn-tail discipline `read_thread` keeps). A turn
/// without a `parent` continues the line before it (the linear-thread
/// rule, and the tree's way of saying "the previous line"), so a chain
/// through a turn whose parent is the line right before it reads exactly
/// as a linear prefix. A `parent` naming a missing id — a corrupted or
/// hand-edited edge — fails loudly: guessing at a chain would silently
/// resume the wrong branch, the same refusal `read_thread` gives a
/// corrupt line.
pub fn chain_indices(turns: &[StoredTurn], leaf: usize) -> Result<Vec<usize>, String> {
    if turns.is_empty() {
        return Ok(Vec::new());
    }
    if leaf >= turns.len() {
        return Err(format!(
            "chain leaf {leaf} is past the thread's {} turns",
            turns.len()
        ));
    }
    // line index by turn id; duplicate ids (a hand-edited file) make the
    // edge ambiguous, which is also refused
    let mut by_id: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::with_capacity(turns.len());
    for (i, t) in turns.iter().enumerate() {
        if by_id.insert(t.id.as_str(), i).is_some() {
            return Err(format!("duplicate turn id {} in thread", t.id));
        }
    }
    let mut chain = vec![leaf];
    let mut cursor = leaf;
    loop {
        let next = match &turns[cursor].parent {
            // the linear rule: no parent, continue the previous line
            None => cursor.checked_sub(1),
            // the first line names a parent only to point before the file's
            // start, which is nothing
            Some(_) if cursor == 0 => None,
            Some(parent) => match by_id.get(parent.as_str()) {
                Some(&i) if i < cursor => Some(i),
                Some(&i) => {
                    return Err(format!(
                        "turn {} branches off turn {} below it — the edge must point at an \
                         earlier line",
                        turns[cursor].id, turns[i].id
                    ));
                }
                None => {
                    return Err(format!(
                        "turn {} names parent {parent}, which no line of this thread carries",
                        turns[cursor].id
                    ));
                }
            },
        };
        match next {
            Some(i) => {
                chain.push(i);
                cursor = i;
            }
            None => break,
        }
    }
    chain.reverse();
    Ok(chain)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: &str, prompt: &str, response: &str, mode: &str) -> StoredTurn {
        StoredTurn {
            v: THREAD_FORMAT_VERSION,
            id: id.to_string(),
            parent: None,
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
            messages: vec![Msg::user(prompt.to_string())],
        }
    }

    /// The same turn, stamped with the directory it ran in.
    fn turn_in(id: &str, cwd: &str, prompt: &str) -> StoredTurn {
        let mut t = turn(id, prompt, "ans", "agent");
        t.cwd = Some(cwd.to_string());
        t
    }

    /// A turn branching off the named parent (the edge a `/tree` jump's
    /// next round writes).
    fn branch(id: &str, parent: &str, prompt: &str) -> StoredTurn {
        let mut t = turn(id, prompt, "ans", "agent");
        t.parent = Some(parent.to_string());
        t
    }

    /// The active chain of a linear thread is every line, in order: a
    /// thread that never jumped reads exactly as before the tree existed.
    #[test]
    fn a_linear_thread_is_its_own_chain() {
        let turns = vec![
            turn("1", "a", "b", "agent"),
            turn("2", "c", "d", "agent"),
            turn("3", "e", "f", "agent"),
        ];
        assert_eq!(chain_indices(&turns, 2).unwrap(), vec![0, 1, 2]);
    }

    /// A branched turn ends the active chain: the jump's rounds read back
    /// through it, and the abandoned sibling stays in the file, off the
    /// chain.
    #[test]
    fn a_branched_turn_ends_the_active_chain() {
        // 0 ← 1 ← 2, then a jump to 1 writes 3 with parent 1
        let turns = vec![
            turn("1", "a", "b", "agent"),
            turn("2", "c", "d", "agent"),
            turn("3", "e", "f", "agent"),
            branch("4", "2", "g"),
        ];
        assert_eq!(chain_indices(&turns, 3).unwrap(), vec![0, 1, 3]);
        // and the abandoned tip is still a walkable chain of its own
        assert_eq!(chain_indices(&turns, 2).unwrap(), vec![0, 1, 2]);
    }

    /// A parent naming a missing turn is a broken edge: the thread fails
    /// loudly instead of resuming some guessed branch.
    #[test]
    fn a_dangling_parent_fails_loudly() {
        let turns = vec![turn("1", "a", "b", "agent"), branch("2", "nope", "c")];
        let err = chain_indices(&turns, 1).unwrap_err();
        assert!(err.contains("nope") && err.contains("parent"), "{err}");
    }

    /// An edge pointing at a later line walks the chain forward, which is
    /// not a conversation — refused, the same way a dangling one is. The
    /// first line naming any parent at all stops at the root: nothing
    /// before it could be the parent, so it is one.
    #[test]
    fn a_forward_edge_is_refused() {
        let turns = vec![
            turn("1", "a", "b", "agent"),
            branch("2", "3", "c"),
            turn("3", "e", "f", "agent"),
        ];
        let err = chain_indices(&turns, 2).unwrap_err();
        assert!(err.contains("below it"), "{err}");
        // the first line with a parent stops at the root rather than error
        let solo = vec![branch("1", "ghost", "a")];
        assert_eq!(chain_indices(&solo, 0).unwrap(), vec![0]);
    }

    /// A duplicate id makes every edge to it ambiguous — also refused,
    /// the same way a corrupt line is.
    #[test]
    fn duplicate_ids_fail_loudly() {
        let turns = vec![turn("1", "a", "b", "agent"), turn("1", "c", "d", "agent")];
        let err = chain_indices(&turns, 1).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    /// The parent field rides through a store round-trip: what a jump's
    /// next round wrote reads back with its edge intact.
    #[test]
    fn the_parent_edge_survives_the_round_trip() {
        let dir = scratch("parent-edge");
        let store = Store::open_path(&dir).unwrap();
        let id = store
            .append_turn(None, &turn("1", "a", "b", "agent"))
            .unwrap();
        store
            .append_turn(Some(&id), &branch("2", "1", "c"))
            .unwrap();
        let turns = store.read_thread(&id).unwrap();
        assert_eq!(turns[0].parent, None, "the first line is the root");
        assert_eq!(
            turns[1].parent.as_deref(),
            Some("1"),
            "the branch edge round-trips"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Lines written before the `parent` field existed deserialize with it
    /// absent — the tree degrades to a linear chain on old threads.
    #[test]
    fn pre_parent_lines_read_as_linear() {
        let dir = scratch("pre-parent");
        let store = Store::open_path(&dir).unwrap();
        let id = store
            .append_turn(None, &turn("1", "a", "b", "agent"))
            .unwrap();
        let path = store.thread_path(&id);
        let text = fs::read_to_string(&path).unwrap();
        // hand-strip a parent-shaped line: an old writer would not have one
        let old = text.replace(",\"parent\":\"1\"", "");
        fs::write(&path, &old).unwrap();
        let turns = store.read_thread(&id).unwrap();
        assert_eq!(turns[0].parent, None);
        assert_eq!(chain_indices(&turns, 0).unwrap(), vec![0]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// What this binary writes round-trips whole, and the pair older files
    /// carry still reads — a transcript must not fail loudly over a field it
    /// predates. A length nothing wrote is refused rather than guessed at.
    #[test]
    fn a_usage_line_survives_both_its_shapes() {
        let mut t = turn("1", "p", "r", "agent");
        t.usage = Some(TurnUsage {
            input: 10,
            output: 2,
            cached: 8,
            cached_write: 3,
        });
        let line = serde_json::to_string(&t).unwrap();
        assert!(line.contains(r#""usage":[10,2,8,3]"#), "{line}");
        assert_eq!(
            serde_json::from_str::<StoredTurn>(&line).unwrap().usage,
            t.usage
        );

        // what a file written before the cache split holds
        let legacy = line.replace(r#""usage":[10,2,8,3]"#, r#""usage":[10,2]"#);
        assert_eq!(
            serde_json::from_str::<StoredTurn>(&legacy).unwrap().usage,
            Some(TurnUsage {
                input: 10,
                output: 2,
                cached: 0,
                cached_write: 0,
            })
        );

        // and a shape no version of this wrote is an error, not zeros
        let broken = line.replace(r#""usage":[10,2,8,3]"#, r#""usage":[10,2,3]"#);
        assert!(serde_json::from_str::<StoredTurn>(&broken).is_err());
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
        let dir = crate::core::testutil::scratch_dir(&format!("threads-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// A stray non-thread file in the store (a `.DS_Store` the desktop
    /// drops in, a `desktop.ini`) must not take the resume and export
    /// surfaces down with it — the listing walks skip it, same as
    /// `summaries` always has.
    #[test]
    fn a_stray_file_in_the_store_does_not_break_resume() {
        let dir = scratch("stray");
        let store = Store::open_path(&dir).unwrap();
        let id = seed(&store, &[turn("1", "task", "ans", "agent")], 0);
        fs::write(dir.join(".DS_Store"), b"junk").unwrap();
        assert_eq!(
            store.latest_thread(None).unwrap().as_deref(),
            Some(id.as_str())
        );
        assert_eq!(
            store.resolve_thread(&id[..8], None).unwrap().as_deref(),
            Some(id.as_str())
        );
        let _ = fs::remove_dir_all(&dir);
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
        // a fixture this small is read entire, so the count is the file's
        assert!(summaries[0].turns_exact);
        assert_eq!(summaries[0].preview, "moved");
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

    /// A line written before the message types were merged (attachment
    /// payloads as `base64`, a nullable `mime_type`) still resumes: the read
    /// side accepts both spellings, so the merge costs no old thread.
    #[test]
    fn a_thread_written_before_the_merge_still_reads() {
        let dir = scratch("legacy");
        let store = Store::open_path(&dir).unwrap();
        let id = store
            .append_turn(None, &turn("1", "a", "b", "agent"))
            .unwrap();
        let path = store.thread_path(&id);
        let legacy = serde_json::json!({
            "v": 1,
            "id": "t1",
            "ts": "2026-08-23T01:00:00+00:00",
            "mode": "agent",
            "model": "prov/m",
            "prompt": "look",
            "response": "ok",
            "options": [],
            "messages": [{
                "role": "user",
                "text": "look",
                "attachments": [{
                    "path": "/tmp/shot.png",
                    "mime_type": serde_json::Value::Null,
                    "base64": "aGk="
                }]
            }]
        });
        fs::write(&path, format!("{legacy}\n")).unwrap();
        let turns = store.read_thread(&id).unwrap();
        let Msg::User { attachments, .. } = &turns[0].messages[0] else {
            panic!("expected a user message");
        };
        assert_eq!(attachments[0].base64_data, "aGk=");
        assert_eq!(attachments[0].mime_type, "");
        assert_eq!(attachments[0].path.as_deref(), Some("/tmp/shot.png"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A turn stamped by a future format is refused with the upgrade path,
    /// never parsed best-effort into something wrong.
    #[test]
    fn a_future_format_version_is_refused_loudly() {
        let dir = scratch("future");
        let store = Store::open_path(&dir).unwrap();
        let id = store
            .append_turn(None, &turn("1", "a", "b", "agent"))
            .unwrap();
        let path = store.thread_path(&id);
        let text = fs::read_to_string(&path).unwrap();
        let bumped = text.replace("\"v\":2", "\"v\":99");
        assert_ne!(text, bumped, "the stamp must be present to bump");
        fs::write(&path, bumped).unwrap();
        let err = store.read_thread(&id).unwrap_err();
        assert!(err.contains("newer") && err.contains("upgrade"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A crash mid-append tears the final line: the bounded repair drops it
    /// with a warning instead of poisoning the whole read.
    #[test]
    fn a_torn_tail_is_repaired_not_fatal() {
        let dir = scratch("torn");
        let store = Store::open_path(&dir).unwrap();
        let id = store
            .append_turn(None, &turn("1", "a", "b", "agent"))
            .unwrap();
        let path = store.thread_path(&id);
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("{\"v\":1,\"id\":\"torn");
        fs::write(&path, text).unwrap();
        let turns = store.read_thread(&id).unwrap();
        assert_eq!(
            turns.len(),
            1,
            "the torn line is dropped, the rest survives"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Corruption mid-file is real damage: fail loudly instead of silently
    /// resuming a conversation with a turn missing.
    #[test]
    fn a_corrupt_middle_line_fails_loud() {
        let dir = scratch("middle");
        let store = Store::open_path(&dir).unwrap();
        let id = store
            .append_turn(None, &turn("1", "a", "b", "agent"))
            .unwrap();
        let path = store.thread_path(&id);
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("not json at all\n");
        text.push_str(&serde_json::to_string(&turn("2", "c", "d", "agent")).unwrap());
        text.push('\n');
        fs::write(&path, text).unwrap();
        let err = store.read_thread(&id).unwrap_err();
        assert!(err.contains("corrupt turn on line 2"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Lines written before versioning existed (no `v` field) still read;
    /// they carry version 0.
    #[test]
    fn pre_version_lines_still_read() {
        let dir = scratch("preversion");
        let store = Store::open_path(&dir).unwrap();
        let path = store.thread_path("oldthread");
        let text = serde_json::to_string(&turn("1", "a", "b", "agent")).unwrap();
        let old = text.replace("\"v\":2,", "");
        assert_ne!(text, old, "the stamp must be present to strip");
        fs::write(&path, old).unwrap();
        let turns = store.read_thread("oldthread").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].v, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A torn tail must not make the thread vanish from the lists: the
    /// summary falls back to the last line that parses.
    #[test]
    fn a_torn_tail_still_lists_the_thread() {
        let dir = scratch("tornlist");
        let store = Store::open_path(&dir).unwrap();
        let id = store
            .append_turn(None, &turn("1", "a", "b", "agent"))
            .unwrap();
        let path = store.thread_path(&id);
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("{\"v\":1,\"id\":\"torn");
        fs::write(&path, text).unwrap();
        let summaries = store.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].preview, "a");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The summary reads only a thread's tail: the newest turn, its cwd and
    /// preview prompt still come off the end, while the turn count — which
    /// only a whole-file read can know exactly — degrades to a marked bound.
    #[test]
    fn a_tail_window_reports_a_bound_count_with_exact_fields() {
        let dir = scratch("tailbound");
        let store = Store::open_path(&dir).unwrap();
        let id = "tailbound";
        let mut text = String::new();
        for n in 1..=4 {
            text.push_str(
                &serde_json::to_string(&turn_in(
                    &n.to_string(),
                    &format!("/p/{n}"),
                    &format!("task {n}"),
                ))
                .unwrap(),
            );
            text.push('\n');
        }
        fs::write(store.thread_path(id), &text).unwrap();
        // a window that starts exactly at line 3: two whole lines in, two
        // turns of the file never read
        let lines: Vec<&str> = text.lines().collect();
        let cap = (lines[2].len() + lines[3].len() + 2) as u64;
        let s = store.summarize_tail(id, cap).unwrap();
        assert_eq!(s.turns, 2, "only the windowed turns are counted");
        assert!(!s.turns_exact, "the window never reached the file start");
        assert_eq!(s.preview, "task 4");
        assert_eq!(s.cwd.as_deref(), Some("/p/4"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A prompt buried under big prompt-less tool rounds is beyond any
    /// bounded tail: the preview falls back to the newest answer's first
    /// line instead of growing the read without bound.
    #[test]
    fn a_prompt_beyond_the_tail_falls_back_to_the_last_answer() {
        let dir = scratch("tailgrow");
        let store = Store::open_path(&dir).unwrap();
        let id = "tailgrow";
        let big = "x".repeat(200);
        let mut text = serde_json::to_string(&turn_in("1", "/p/one", "the real task")).unwrap();
        text.push('\n');
        for n in 2..=4 {
            let mut t = turn(&n.to_string(), "", "the counted answer", "agent");
            t.cwd = Some("/p/four".to_string());
            t.messages = vec![Msg::user(big.clone())];
            text.push_str(&serde_json::to_string(&t).unwrap());
            text.push('\n');
        }
        fs::write(store.thread_path(id), &text).unwrap();
        let total = text.len() as u64;
        // a window over the last third cannot reach turn 1's prompt
        let s = store.summarize_tail(id, total - total / 3).unwrap();
        assert_eq!(s.preview, "the counted answer");
        assert!(!s.turns_exact, "the window never reached the file start");
        assert_eq!(s.cwd.as_deref(), Some("/p/four"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `read_to_string` refused a file whose torn tail cut a character in
    /// half; the byte tail scan only needs the newline positions, so the
    /// thread stays listed.
    #[test]
    fn a_torn_multibyte_tail_still_lists_the_thread() {
        let dir = scratch("tailutf");
        let store = Store::open_path(&dir).unwrap();
        let id = "tailutf";
        let mut bytes = serde_json::to_string(&turn("1", "中文任务", "", "agent"))
            .unwrap()
            .into_bytes();
        bytes.push(b'\n');
        // the torn second line ends mid-character (the first two bytes of a 3-byte char)
        bytes.extend_from_slice(b"{\"v\":1,\"prompt\":\"\xe4\xb8");
        fs::write(store.thread_path(id), &bytes).unwrap();
        let summaries = store.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].preview, "中文任务");
        let _ = fs::remove_dir_all(&dir);
    }
}
