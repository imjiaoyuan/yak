//! The session browser behind `yak threads`, `yak -r` and `/resume`: a
//! filterable list of recent threads (the fuzzy-filter shape), a non-interactive
//! list for scripts and remote clients, and the id/prefix resolver behind
//! `/resume <id>`.

use std::path::Path;

use crate::core::args::{OptSpec, render_help};
use crate::core::threads::{Store, ThreadSummary};
use crate::{flag_spec, value_spec};

const SPECS: &[OptSpec] = &[
    flag_spec!("help", Some('h'), "Show this message and exit"),
    flag_spec!("all", Some('a'), "Every directory, not only this one"),
    flag_spec!("json", None, "One JSON array of thread objects on stdout"),
    value_spec!("limit", Some('n'), "How many rows (default 20)", "N"),
];

fn help() -> String {
    render_help("yak threads", "List recent conversations", SPECS, &[])
}

/// `yak threads`: the same summaries `/resume` picks from, printed — the
/// non-interactive surface a script, editor or phone bridge addresses a
/// session through.
pub fn run(argv: &[String]) -> i32 {
    let rest: Vec<String> = argv.get(1..).unwrap_or_default().to_vec();
    let (args, code) = crate::core::args::parse_with_help(&rest, SPECS, help);
    let Some(args) = args else { return code };
    match list(&args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

fn list(args: &crate::core::args::ParsedArgs) -> Result<(), String> {
    let limit = match args.opt(&["limit", "n"]) {
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| format!("invalid --limit '{raw}' (want a number)"))?,
        None => 20,
    };
    if limit == 0 {
        return Err("--limit must be at least 1".into());
    }
    let all_dirs = args.flag(&["all", "a"]);
    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot read the working directory: {e}"))?;
    let store = Store::open()?;
    render(&store, &cwd, all_dirs, limit, args.flag(&["json"]))
}

/// The list itself, with the store injected: the path the tests drive.
fn render(
    store: &Store,
    cwd: &Path,
    all_dirs: bool,
    limit: usize,
    json: bool,
) -> Result<(), String> {
    let dir = cwd.display().to_string();
    let scope = if all_dirs { None } else { Some(dir.as_str()) };
    let rows = store.recent_threads(limit, scope);
    if json {
        let value = serde_json::Value::Array(rows.iter().map(summary_json).collect());
        println!("{value}");
        return Ok(());
    }
    if rows.is_empty() {
        let p = crate::theme::err();
        eprintln!(
            "{}no conversations {}{}",
            p.dim,
            if all_dirs {
                "yet"
            } else {
                "in this directory — -a lists every directory"
            },
            p.reset
        );
        return Ok(());
    }
    let now = crate::core::db::now_turn_datetime();
    for t in &rows {
        println!("{}", thread_item(t, &now, all_dirs));
    }
    Ok(())
}

/// One row as a machine reads it: the summary verbatim, full id included.
/// `turns_exact` says whether the count covers the whole file (the summary
/// reads a bounded tail) so a consumer need not guess what `turns` means.
fn summary_json(t: &ThreadSummary) -> serde_json::Value {
    let mut v = serde_json::json!({
        "id": t.id,
        "turns": t.turns,
        "turns_exact": t.turns_exact,
        "last": t.last,
        "preview": t.preview,
        "cwd": t.cwd,
    });
    if let Some(name) = &t.name {
        v["name"] = serde_json::json!(name);
    }
    v
}

/// `/resume <id|prefix>`: the id itself, or the unambiguous prefix — with
/// the same this-directory preference the picker has. An ambiguous prefix is
/// an error naming the candidates, never a guess.
pub fn resolve_prefix(cwd: &Path, raw: &str) -> Result<String, String> {
    let store = Store::open()?;
    resolve_in(&store, cwd, raw)
}

fn resolve_in(store: &Store, cwd: &Path, raw: &str) -> Result<String, String> {
    let dir = cwd.display().to_string();
    if let Some(cid) = store.resolve_thread(raw, Some(&dir))? {
        return Ok(cid);
    }
    let hits = store.threads_matching(raw);
    if hits.is_empty() {
        return Err(format!("no session matches '{raw}'"));
    }
    let now = crate::core::db::now_turn_datetime();
    let mut rows: Vec<String> = hits
        .iter()
        .take(5)
        .map(|t| thread_item(t, &now, true))
        .collect();
    if hits.len() > rows.len() {
        rows.push(format!("… and {} more", hits.len() - rows.len()));
    }
    Err(format!(
        "'{raw}' matches {} sessions — use more of the id:\n  {}",
        hits.len(),
        rows.join("\n  ")
    ))
}

/// The conversations a picker offers: those that ran in `cwd`, newest first —
/// a resume list is this project's history. A directory with none of
/// its own falls back to every directory's (the items then carry a directory
/// tag), so a fresh project is never a dead end. None = nothing to show.
pub fn pick_thread(cwd: &Path, title: &str) -> Result<Option<String>, String> {
    let store = Store::open()?;
    let dir = cwd.display().to_string();
    let local = store.recent_threads(30, Some(&dir));
    let (threads, all_dirs) = if local.is_empty() {
        (store.recent_threads(30, None), true)
    } else {
        (local, false)
    };
    if threads.is_empty() {
        eprintln!(
            "{}no conversations yet{}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
        return Ok(None);
    }
    let p = crate::theme::err();
    if all_dirs {
        eprintln!(
            "{}{dir} has no conversations — showing every directory{}",
            p.dim, p.reset
        );
    }
    let now = crate::core::db::now_turn_datetime();
    let items: Vec<String> = threads
        .iter()
        .map(|t| thread_item(t, &now, all_dirs))
        .collect();
    let Some(i) = crate::term::lineedit::pick(title, &items, false) else {
        return Ok(None);
    };
    Ok(Some(threads[i].id.clone()))
}

/// One picker row: name or id, turn count, preview, age, and — when the
/// list spans directories — the directory it ran in.
fn thread_item(t: &ThreadSummary, now: &str, with_dir: bool) -> String {
    let preview: String = t.preview.chars().take(40).collect();
    let preview = preview.replace('\n', " ");
    let dir = match t.cwd.as_deref() {
        Some(cwd) => Path::new(cwd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(cwd),
        None => "--",
    };
    // a hand-set name replaces the preview quote; the raw id stays in front
    // so a resume-by-prefix still reads against the row
    let quote = match &t.name {
        Some(name) => format!("\"{name}\""),
        None => format!("\"{preview}\""),
    };
    format!(
        "{} · {}{} turn{} · {quote} · {}{}",
        &t.id[..t.id.len().min(6)],
        t.turns,
        // the summary scans a thread's tail, so a huge conversation's count
        // is the bound it saw, not the file's whole line count
        if t.turns_exact { "" } else { "+" },
        if t.turns == 1 { "" } else { "s" },
        crate::core::db::short_time(now, &t.last),
        if with_dir {
            format!(" · {dir}")
        } else {
            String::new()
        }
    )
}

/// Pick from recent conversations; `Y` on the enter question continues the
/// chosen one by re-entering the agent with `--session <id>`.
pub fn browse() -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: cannot read the working directory: {e}");
            return 1;
        }
    };
    let cid = match pick_thread(&cwd, "conversations:") {
        Ok(Some(id)) => id,
        Ok(None) => return 0,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    show_transcript(&cid);
    eprint!(
        "{}enter this conversation? [Y/n]{} ",
        crate::theme::err().dim,
        crate::theme::err().reset
    );
    match crate::term::lineedit::read_approval_key() {
        Some(crate::term::lineedit::ApprovalKey::Yes)
        | Some(crate::term::lineedit::ApprovalKey::Always) => {
            let argv: Vec<String> = vec!["--session".into(), cid];
            crate::commands::agent::run(&argv)
        }
        _ => 0,
    }
}

/// A clean conversation view: just the user prompt and the assistant
/// response per turn, indented in the answer block.
pub fn show_transcript(cid: &str) -> i32 {
    let store = match Store::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let turns = match store.read_thread(cid) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    // the preview is the conversation as it stands — the active branch,
    // not the sibling branches a /tree jump left in the file
    let chain = match crate::core::threads::chain_indices(&turns, turns.len().saturating_sub(1)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let active: Vec<&crate::core::threads::StoredTurn> = chain.iter().map(|&i| &turns[i]).collect();
    let p = crate::theme::out();
    if let Some(first) = active.first() {
        let where_ = match first.cwd.as_deref() {
            Some(cwd) => format!(" · {cwd}"),
            None => String::new(),
        };
        println!(
            "{}conversation: {cid} · {}{where_}{}",
            p.dim, first.model, p.reset
        );
    }
    for turn in &active {
        let p = crate::theme::out();
        println!("{}{} {}", p.dim, turn.ts, p.reset);
        if !turn.prompt.is_empty() {
            for line in turn.prompt.lines() {
                println!("{}>{} {line}", p.bold, p.reset);
            }
        }
        if !turn.response.is_empty() {
            let shown = crate::core::render_md::render_once(&turn.response, 2);
            print!("{shown}");
            if !shown.ends_with('\n') {
                println!();
            }
        }
        println!();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::{resolve_in, summary_json};
    use crate::core::testutil::scratch_dir;
    use crate::core::threads::Store;
    use std::path::Path;

    /// A thread file with only the fields the store requires: the reader
    /// defaults the rest, so the fixture stays about the listing.
    fn write_thread(dir: &Path, id: &str, cwd: &str, ts: &str, prompt: &str) {
        let line = serde_json::json!({
            "id": "t1",
            "ts": ts,
            "mode": "agent",
            "model": "mock/m",
            "cwd": cwd,
            "prompt": prompt,
            "response": "ok",
            "messages": [],
        });
        std::fs::write(dir.join(format!("{id}.jsonl")), format!("{line}\n")).unwrap();
    }

    #[test]
    fn a_resume_prefix_prefers_this_directory_and_refuses_ambiguity() {
        let dir = scratch_dir("resume-prefix");
        write_thread(&dir, "01abcA", "/p/a", "2026-10-20T10:00:00+00:00", "first");
        write_thread(
            &dir,
            "01abcB",
            "/p/b",
            "2026-10-20T11:00:00+00:00",
            "second",
        );
        let store = Store::open_path(&dir).unwrap();
        // a local match wins over a newer sibling in another directory
        assert_eq!(
            resolve_in(&store, Path::new("/p/a"), "01abc").unwrap(),
            "01abcA"
        );
        // nothing local: the prefix is ambiguous, and the error names both
        let err = resolve_in(&store, Path::new("/p/c"), "01abc").unwrap_err();
        assert!(err.contains("matches 2 sessions"), "{err}");
        assert!(err.contains("01abcA") && err.contains("01abcB"), "{err}");
        // a unique prefix resolves from any directory
        assert_eq!(
            resolve_in(&store, Path::new("/p/c"), "01abcB").unwrap(),
            "01abcB"
        );
        let err = resolve_in(&store, Path::new("/p/c"), "zzz").unwrap_err();
        assert!(err.contains("no session matches 'zzz'"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_json_row_carries_the_full_id_and_the_count_meaning() {
        let dir = scratch_dir("threads-json");
        write_thread(
            &dir,
            "01abcA",
            "/p/a",
            "2026-10-20T10:00:00+00:00",
            "hello there",
        );
        let store = Store::open_path(&dir).unwrap();
        let rows = store.recent_threads(20, None);
        let v = summary_json(&rows[0]);
        assert_eq!(v["id"], "01abcA");
        assert_eq!(v["turns"], 1);
        assert_eq!(v["turns_exact"], true);
        assert_eq!(v["preview"], "hello there");
        assert_eq!(v["cwd"], "/p/a");
        assert_eq!(v["last"], "2026-10-20T10:00:00+00:00");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
