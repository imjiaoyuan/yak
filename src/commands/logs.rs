//! `llm logs` — list past conversations from the JSONL thread store and
//! resume them. The FTS search, per-turn SQL reports and `backup` were
//! removed with the SQLite store; this is the reference shape resume surface.

use std::io::IsTerminal;

use crate::core::args::{OptSpec, ParsedArgs, render_help, split_subcommand};
use crate::core::config;
use crate::core::threads::{Store, StoredTurn, ThreadSummary};
use crate::{flag_spec, value_spec};

const LIST_SPECS: &[OptSpec] = &[
    value_spec!("count", Some('n'), "Number of threads to show", "INTEGER"),
    value_spec!("database", Some('d'), "Path to thread directory", "PATH"),
    value_spec!(
        "model",
        Some('m'),
        "Filter by model or model alias",
        "MODEL"
    ),
    flag_spec!("current", Some('c'), "Show the most recent thread"),
    value_spec!(
        "conversation",
        None,
        "Show the conversation with this ID",
        "ID"
    ),
    value_spec!("cid", None, "(alias of --conversation)", "ID"),
    flag_spec!(
        "full",
        None,
        "Show the full per-turn report (default is a compact list)"
    ),
    flag_spec!("truncate", Some('t'), "Truncate long strings in output"),
    flag_spec!("usage", Some('u'), "Include token usage"),
    flag_spec!("response", Some('r'), "Just output the last response"),
    flag_spec!("extract", Some('x'), "Extract first fenced code block"),
    flag_spec!("extract-last", None, "Extract last fenced code block"),
    flag_spec!("xl", None, "(alias of --extract-last)"),
    flag_spec!("json", None, "Output as JSON"),
    flag_spec!("help", Some('h'), "Show this message and exit"),
];

const LOG_MODES: [&str; 3] = ["agent", "prompt", "all"];

pub fn run(argv: &[String]) -> i32 {
    let mode_filter: Option<String> = argv
        .first()
        .filter(|a| LOG_MODES.contains(&a.as_str()))
        .cloned();
    let effective: Vec<String> = if mode_filter.is_some() {
        argv[1..].to_vec()
    } else {
        argv.to_vec()
    };
    if effective.is_empty() && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let store = match Store::open() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        };
        return interactive(&store, mode_filter.as_deref());
    }
    let (sub, rest) = if effective
        .first()
        .is_some_and(|f| matches!(f.as_str(), "--help" | "-h" | "help"))
    {
        ("help", &effective[1..])
    } else {
        split_subcommand(&effective, "list")
    };
    match sub {
        "list" => list(rest, mode_filter.as_deref()),
        "path" => {
            println!("{}", config::threads_dir().display());
            0
        }
        "on" => {
            config::set_logs_enabled(true);
            0
        }
        "off" => {
            config::set_logs_enabled(false);
            0
        }
        "status" => status(),
        "--help" | "-h" | "help" => {
            print!(
                "{}",
                render_help(
                    "llm logs [OPTIONS] COMMAND [ARGS]...",
                    "Show past conversations\n\nCommands:\n  list, path, status, on, off\n  or a mode: agent, prompt, all",
                    LIST_SPECS,
                    &[],
                )
            );
            0
        }
        other => {
            eprintln!("Error: No such command 'logs {other}'.");
            2
        }
    }
}

/// Bare-terminal `llm logs`: one filterable list of recent threads (each row
/// tagged with its mode) — the fuzzy-filter shape. Enter opens the transcript, then
/// one key resumes it.
fn interactive(store: &Store, only: Option<&str>) -> i32 {
    let threads = store.recent_threads(30);
    if threads.is_empty() {
        eprintln!("\x1b[2mno conversations yet\x1b[0m");
        return 0;
    }
    let shown: Vec<&ThreadSummary> = threads
        .iter()
        .filter(|t| only.is_none_or(|f| f == "all" || f == t.mode))
        .collect();
    if shown.is_empty() {
        eprintln!("\x1b[2mno conversations yet\x1b[0m");
        return 0;
    }
    let now = crate::core::db::now_turn_datetime();
    let items: Vec<String> = shown
        .iter()
        .map(|t| {
            let preview: String = t.last_prompt.chars().take(40).collect::<String>();
            let preview = preview.replace('\n', " ");
            format!(
                "{:<6} {} · \"{}\" · {}",
                t.mode,
                &t.id[..t.id.len().min(6)],
                preview,
                crate::core::db::short_time(&now, &t.last)
            )
        })
        .collect();
    let Some(i) = crate::term::lineedit::pick("conversations:", &items, false) else {
        return 0;
    };
    let t = shown[i];
    show_transcript(store, &t.id);
    eprint!("\x1b[2menter this conversation? [Y/n]\x1b[0m ");
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
fn show_transcript(store: &Store, cid: &str) -> i32 {
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
        println!("\x1b[2m{} \x1b[0m", turn.ts);
        if !turn.prompt.is_empty() {
            for line in turn.prompt.lines() {
                println!("\x1b[1m>\x1b[0m {line}");
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

fn open_store(args: &ParsedArgs) -> Result<Store, String> {
    if args.opt(&["database"]).is_none() && !config::threads_dir().exists() {
        return Err(format!(
            "No thread store found at {}",
            config::threads_dir().display()
        ));
    }
    Store::open_from_arg(args.opt(&["database"]))
}

fn list(argv: &[String], mode_filter: Option<&str>) -> i32 {
    let (args, code) = crate::core::args::parse_with_help(argv, LIST_SPECS, || {
        render_help(
            "llm logs list [OPTIONS]",
            "Show recent conversations",
            LIST_SPECS,
            &[],
        )
    });
    let Some(args) = args else { return code };
    let store = match open_store(&args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    // conversation selection: --cid > -c
    let conversation: Option<String> = if let Some(cid) = args.opt(&["conversation", "cid"]) {
        Some(cid.to_string())
    } else if args.flag(&["current"]) || args.flag(&["response"]) {
        match store.latest_thread() {
            Ok(Some(id)) => Some(id),
            _ => {
                eprintln!("Error: No conversations found");
                return 1;
            }
        }
    } else {
        None
    };

    if let Some(raw) = conversation.as_deref() {
        let cid = match store.resolve_thread(raw) {
            Ok(Some(cid)) => cid,
            Ok(None) => {
                eprintln!(
                    "Error: conversation id or prefix '{raw}' matches nothing (or is ambiguous)"
                );
                return 1;
            }
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        };
        return show_one(&store, &cid, &args);
    }

    // model filter: alias-expanded to its target id
    let model_filter: Option<String> = args.opt(&["model"]).map(|m| {
        let cfg = config::load();
        match cfg.resolve_model(m) {
            Ok(Some((n, _, mid))) => format!("{n}/{mid}"),
            _ => m.to_string(),
        }
    });

    let count = args
        .opt(&["count"])
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(3);

    let mut threads = store.recent_threads(usize::MAX);
    threads.retain(|t| {
        mode_filter.is_none_or(|f| f == "all" || f == t.mode)
            && model_filter.as_ref().is_none_or(|m| t.model == *m)
    });
    threads.truncate(count);

    if threads.is_empty() {
        return 0;
    }

    if args.flag(&["json"]) {
        let vals: Vec<serde_json::Value> = threads
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "turns": t.turns,
                    "datetime_utc": t.last,
                    "model": t.model,
                    "mode": t.mode,
                    "prompt": t.last_prompt,
                })
            })
            .collect();
        println!(
            "{}",
            crate::jsonfmt::dumps_indent(&serde_json::Value::Array(vals), 2)
        );
        return 0;
    }

    compact_output(&threads, mode_filter);
    0
}

/// A full report or an extraction for one conversation (`--cid`).
fn show_one(store: &Store, cid: &str, args: &ParsedArgs) -> i32 {
    let turns = match store.read_thread(cid) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    if args.flag(&["response"]) {
        if let Some(last) = turns.last() {
            let response = &last.response;
            if args.flag(&["extract", "extract-last", "xl"]) {
                let block = crate::term::render::extract_fenced(
                    response,
                    args.flag(&["extract-last", "xl"]),
                )
                .unwrap_or_else(|| response.clone());
                println!("{block}");
            } else {
                println!("{response}");
            }
        }
        return 0;
    }
    if args.flag(&["json"]) {
        let vals: Vec<serde_json::Value> = turns.iter().map(turn_json).collect();
        println!(
            "{}",
            crate::jsonfmt::dumps_indent(&serde_json::Value::Array(vals), 2)
        );
        return 0;
    }
    if args.flag(&["full"]) {
        markdown_output(&turns, args);
        return 0;
    }
    show_transcript(store, cid)
}

fn turn_json(t: &StoredTurn) -> serde_json::Value {
    serde_json::json!({
        "id": t.id,
        "datetime_utc": t.ts,
        "model": t.model,
        "mode": t.mode,
        "prompt": t.prompt,
        "response": t.response,
        "reasoning": t.reasoning,
        "input_tokens": t.usage.map(|(i, _)| i),
        "output_tokens": t.usage.map(|(_, o)| o),
    })
}

/// Compact index view: one line per thread under a dim mode header.
fn compact_output(threads: &[ThreadSummary], filter: Option<&str>) {
    let now = crate::core::db::now_turn_datetime();
    print!(
        "{}",
        compact_lines(threads, &now, std::io::stdout().is_terminal(), filter)
    );
}

fn compact_lines(threads: &[ThreadSummary], now: &str, tty: bool, filter: Option<&str>) -> String {
    let dim = |s: &str| {
        if tty {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    let bold = |s: &str| {
        if tty {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    let width = if tty {
        crate::term::columns().max(60)
    } else {
        80
    };

    let mut out = String::new();
    for mode in ["agent", "prompt"] {
        if let Some(f) = filter
            && mode != f
            && f != "all"
        {
            continue;
        }
        let selected: Vec<&ThreadSummary> = threads.iter().filter(|t| t.mode == mode).collect();
        if selected.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&bold(mode));
        out.push('\n');
        for t in selected {
            let shown_cid = t.id[..t.id.len().min(6)].to_string();
            let turns = if t.turns == 1 { "turn" } else { "turns" };
            out.push_str(&dim(&format!(
                "{shown_cid} · {} · {} {} · {}",
                t.model,
                t.turns,
                turns,
                crate::core::db::short_time(now, &t.last)
            )));
            out.push('\n');
            let preview = t.last_prompt.replace('\n', " ");
            let preview = if preview.trim().is_empty() {
                "--".to_string()
            } else {
                crate::core::render_md::truncate_cells(preview.trim(), width.saturating_sub(4))
            };
            out.push_str(&dim(&format!("  {preview}")));
            out.push('\n');
        }
    }
    out
}

fn markdown_output(turns: &[StoredTurn], args: &ParsedArgs) {
    let truncate = args.flag(&["truncate"]);
    let cut = |s: &str, n: usize| {
        if truncate {
            crate::core::text::truncate_chars(s, n)
        } else {
            s.to_string()
        }
    };
    let mut previous_system: Option<&str> = None;
    for turn in turns {
        println!("# {}", turn.ts);
        if !turn.prompt.is_empty() {
            println!("\n## Prompt\n\n{}", cut(&turn.prompt, 100));
        }
        if !turn.options.is_empty() {
            println!("\n## Options\n");
            for (k, v) in &turn.options {
                println!("- {k}: {v}");
            }
        }
        if let Some(system) = turn.system.as_deref() {
            if !system.is_empty() && previous_system != Some(system) {
                println!("\n## System\n\n{}", cut(system, 100));
            }
            previous_system = Some(system);
        }
        if let Some(reasoning) = turn.reasoning.as_deref().filter(|r| !r.is_empty()) {
            println!("\n## Reasoning\n\n{}", cut(reasoning, 100));
        }
        println!("\n## Response\n");
        if truncate {
            println!("{}", cut(&turn.response, 100));
        } else if std::io::stdout().is_terminal() {
            let shown = crate::core::render_md::render_once(&turn.response, 2);
            print!("{shown}");
            if !shown.ends_with('\n') {
                println!();
            }
        } else {
            println!("{}", turn.response);
        }
        if args.flag(&["usage"])
            && let Some((input, output)) = turn.usage
        {
            println!(
                "\n## Token usage\n\n{}: {input} input, {output} output",
                turn.model
            );
        }
        println!();
    }
}

fn status() -> i32 {
    let dir = config::threads_dir();
    if !dir.exists() {
        println!("No thread store found at {}", dir.display());
        return 0;
    }
    let on = config::logs_on();
    println!(
        "Logging is {} for all prompts",
        if on { "ON" } else { "OFF" }
    );
    println!("Found thread store at {}", dir.display());
    let store = match Store::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let (threads, turns) = store.counts();
    println!("Number of threads logged:\t{threads}");
    println!("Number of turns logged:\t\t{turns}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str, mode: &str, turns: usize, last: &str, prompt: &str) -> ThreadSummary {
        ThreadSummary {
            id: id.to_string(),
            turns,
            last: last.to_string(),
            model: "prov/m".to_string(),
            mode: mode.to_string(),
            last_prompt: prompt.to_string(),
        }
    }

    #[test]
    fn compact_groups_threads_by_mode() {
        let threads = vec![
            summary(
                "01CCCCCCCCCCCCCCCCCCCCCCCC",
                "agent",
                2,
                "2026-08-23T02:00:00+00:00",
                "second",
            ),
            summary(
                "01EEEEEEEEEEEEEEEEEEEEEEEE",
                "prompt",
                1,
                "2026-08-22T02:00:00+00:00",
                "other",
            ),
        ];
        let out = compact_lines(&threads, "2026-08-23T12:00:00+00:00", false, None);
        assert!(
            out.starts_with("agent\n01CCCC · prov/m · 2 turns · 02:00"),
            "got {out:?}"
        );
        assert!(
            out.contains("\nprompt\n01EEEE · prov/m · 1 turn · 08/22"),
            "got {out:?}"
        );
        assert!(out.contains("second"), "got {out:?}");
        assert!(!out.contains('\x1b'), "got {out:?}");
    }

    #[test]
    fn compact_styles_on_tty_and_shows_placeholder() {
        let threads = vec![summary(
            "01AAAAAAAAAAAAAAAAAAAAAAAA",
            "agent",
            1,
            "2026-08-23T01:00:00+00:00",
            "",
        )];
        assert!(compact_lines(&threads, "2026-08-23T12:00:00+00:00", true, None).contains('\x1b'));
        let out = compact_lines(&threads, "2026-08-23T12:00:00+00:00", false, None);
        assert!(out.contains("--"), "got {out:?}");
    }
}
