//! The session browser behind `llm -r`: one filterable list of recent
//! threads (the fuzzy-filter shape). Enter opens the transcript, then one key
//! resumes it in the agent session.

use crate::core::threads::Store;

/// Pick from recent conversations; `Y` on the enter question continues the
/// chosen one by re-entering the agent with `--session <id>`.
pub fn browse() -> i32 {
    let store = match Store::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let threads = store.recent_threads(30);
    if threads.is_empty() {
        eprintln!(
            "{}no conversations yet{}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
        return 0;
    }
    let now = crate::core::db::now_turn_datetime();
    let items: Vec<String> = threads
        .iter()
        .map(|t| {
            let preview: String = t.last_prompt.chars().take(40).collect::<String>();
            let preview = preview.replace('\n', " ");
            format!(
                "{} · {} turn{} · \"{}\" · {}",
                &t.id[..t.id.len().min(6)],
                t.turns,
                if t.turns == 1 { "" } else { "s" },
                preview,
                crate::core::db::short_time(&now, &t.last)
            )
        })
        .collect();
    let Some(i) = crate::term::lineedit::pick("conversations:", &items, false) else {
        return 0;
    };
    let t = &threads[i];
    show_transcript(&store, &t.id);
    eprint!(
        "{}enter this conversation? [Y/n]{} ",
        crate::theme::err().dim,
        crate::theme::err().reset
    );
    match crate::term::lineedit::read_approval_key(Vec::new()) {
        Some(crate::term::lineedit::ApprovalKey::Yes)
        | Some(crate::term::lineedit::ApprovalKey::Always) => {
            let argv: Vec<String> = vec!["--session".into(), t.id.clone()];
            crate::commands::agent::run(&argv)
        }
        _ => 0,
    }
}

/// A clean conversation view: just the user prompt and the assistant
/// response per turn, indented in the answer block.
pub fn show_transcript(store: &Store, cid: &str) -> i32 {
    let turns = match store.read_thread(cid) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    if let Some(first) = turns.first() {
        println!("conversation: {} · {}", cid, first.model);
    }
    for turn in &turns {
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
