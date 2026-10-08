//! The session browser behind `yak -r` and `/resume`: one filterable list of
//! recent threads (the fuzzy-filter shape). Enter opens the transcript, then one key
//! resumes it in the agent session.

use std::path::Path;

use crate::core::threads::{Store, ThreadSummary};

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

/// One picker row: id, turn count, preview, age, and — when the list
/// spans directories — the directory it ran in.
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
    format!(
        "{} · {}{} turn{} · \"{}\" · {}{}",
        &t.id[..t.id.len().min(6)],
        t.turns,
        // the summary scans a thread's tail, so a huge conversation's count
        // is the bound it saw, not the file's whole line count
        if t.turns_exact { "" } else { "+" },
        if t.turns == 1 { "" } else { "s" },
        preview,
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
