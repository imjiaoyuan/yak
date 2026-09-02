//! `llm logs` — view past conversations, merged across the content-addressed
//! store (threads/turns) and the legacy responses table.

use std::io::IsTerminal;

use crate::core::args::{OptSpec, ParsedArgs, parse, render_help, split_subcommand};
use crate::core::config;
use crate::core::db::Db;
use crate::core::logstore::truncate_string;
use crate::{flag_spec, value_spec};

const LIST_SPECS: &[OptSpec] = &[
    value_spec!("count", Some('n'), "Number of entries to show", "INTEGER"),
    value_spec!("database", Some('d'), "Path to log database", "PATH"),
    value_spec!(
        "model",
        Some('m'),
        "Filter by model or model alias",
        "MODEL"
    ),
    value_spec!(
        "query",
        Some('q'),
        "Search for logs matching this string",
        "QUERY"
    ),
    value_spec!("schema", None, "Show logs that use this schema", "SCHEMA"),
    flag_spec!("current", Some('c'), "Show the current conversation"),
    value_spec!(
        "conversation",
        None,
        "Show the conversation with this ID",
        "ID"
    ),
    value_spec!("cid", None, "(alias of --conversation)", "ID"),
    value_spec!("id-gt", None, "Return responses with ID > this", "ID"),
    value_spec!("id-gte", None, "Return responses with ID >= this", "ID"),
    flag_spec!(
        "latest",
        Some('l'),
        "Sort by time (newest first), not relevance"
    ),
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

const LOG_MODES: [&str; 4] = ["agent", "chat", "prompt", "all"];

pub fn run(argv: &[String]) -> i32 {
    // `llm logs <mode>` filters to one mode everywhere: the interactive
    // picker starts pre-filtered on a terminal, printing shows only that
    // section. Bare `llm logs` on a terminal is interactive (like
    // `llm models`); piped or flagged input keeps the plain list and
    // `llm logs list` always prints.
    use std::io::IsTerminal;
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
        let db = match Db::open() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        };
        return interactive(&db, mode_filter.as_deref());
    }
    let (sub, rest) = split_subcommand(&effective, "list");
    match sub {
        "list" => list(rest, mode_filter.as_deref()),
        "path" => {
            println!("{}", config::logs_db_path().display());
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
        "backup" => backup(rest),
        "--help" | "-h" | "help" => {
            print!(
                "{}",
                render_help(
                    "llm logs [OPTIONS] COMMAND [ARGS]...",
                    "Show past conversations\n\nCommands:\n  list, path, status, backup, on, off\n  or a mode: agent, chat, prompt, all",
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

/// Bare-terminal `llm logs`: one filterable list of recent conversations
/// (each row tagged with its mode) — the fuzzy-filter shape, not a two-step picker.
/// Type to filter across mode, preview and id; enter opens the report.
fn interactive(db: &Db, only: Option<&str>) -> i32 {
    let threads = crate::core::logstore::recent_threads(db, 30);
    if threads.is_empty() {
        eprintln!("\x1b[2mno conversations yet\x1b[0m");
        return 0;
    }
    let now = crate::core::db::now_turn_datetime();
    // resolve each conversation's mode once (one options query per thread);
    // `shown` stays index-aligned with the picker items, so a mode filter
    // can never open the wrong conversation
    let mut modes: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut shown: Vec<&crate::core::logstore::ThreadSummary> = Vec::new();
    for t in &threads {
        let mode = conversation_mode(Some(db), &t.id, t.turns, &modes);
        if only.is_none_or(|f| f == "all" || f == mode) {
            shown.push(t);
        }
        modes.insert(t.id.clone(), mode);
    }
    let items: Vec<String> = shown
        .iter()
        .map(|t| {
            let mode = modes[&t.id].as_str();
            let preview: String = crate::core::logstore::thread_last_prompt(db, &t.id)
                .chars()
                .take(40)
                .collect::<String>()
                .replace('\n', " ");
            format!(
                "{mode:<6} {} · \"{}\" · {}",
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
    show_conversation_full(db, &t.id);
    // one key continues the conversation where it left off: chat threads
    // reopen the tool-less session, everything else the agent session
    eprint!("\x1b[2menter this conversation? [Y/n]\x1b[0m ");
    match crate::term::lineedit::read_approval_key() {
        Some(crate::term::lineedit::ApprovalKey::Yes)
        | Some(crate::term::lineedit::ApprovalKey::Always) => {
            let argv: Vec<String> = vec!["--session".into(), t.id.clone()];
            if modes[&t.id] == "chat" {
                crate::commands::agent::run_chat(&argv)
            } else {
                crate::commands::agent::run(&argv)
            }
        }
        _ => 0,
    }
}

/// One conversation's full per-turn report — the same rendering `--cid --full`
/// produces, minus the flags.
fn show_conversation_full(db: &Db, cid: &str) -> i32 {
    let mut rows = crate::core::logstore::collect_rows(
        db,
        &crate::core::logstore::RowFilters {
            conversation: Some(cid),
            model: None,
            query: None,
            schema_id: None,
            id_gt: None,
            id_gte: None,
            count: None,
            search: false,
        },
    );
    rows.reverse();
    let annotated = crate::core::logstore::annotate(db, rows, false);
    render_browse_conversation(&annotated);
    0
}

/// A clean conversation view for interactive `llm logs`: just the user prompt
/// and the assistant response per turn, indented in the answer block, no
/// internal system/reasoning/options noise (those stay in `--full`).
fn render_browse_conversation(rows: &[serde_json::Value]) {
    if let Some(first) = rows.first() {
        println!(
            "conversation: {} · {}",
            first["conversation_id"].as_str().unwrap_or(""),
            first["model"].as_str().unwrap_or("")
        );
    }
    for row in rows {
        let datetime = row["datetime_utc"].as_str().unwrap_or("");
        println!("\x1b[2m{} \x1b[0m", datetime);
        let prompt = row["prompt"].as_str().unwrap_or("");
        if !prompt.is_empty() {
            for line in prompt.lines() {
                println!("\x1b[1m>\x1b[0m {line}");
            }
        }
        let response = row["response"].as_str().unwrap_or("");
        if !response.is_empty() {
            let shown = crate::core::render_md::render_once(response, 2);
            print!("{shown}");
            if !shown.ends_with('\n') {
                println!();
            }
        }
        println!();
    }
}

fn open_db(args: &ParsedArgs) -> Result<Db, String> {
    match args.opt(&["database"]) {
        Some(p) => Db::open_path(std::path::Path::new(p)).map_err(|e| e.to_string()),
        None => {
            let path = config::logs_db_path();
            if !path.exists() {
                return Err(format!("No log database found at {}", path.display()));
            }
            Db::open().map_err(|e| e.to_string())
        }
    }
}

fn list(argv: &[String], mode_filter: Option<&str>) -> i32 {
    let args = match parse(argv, LIST_SPECS) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if args.flag(&["help"]) {
        print!(
            "{}",
            render_help(
                "llm logs list [OPTIONS]",
                "Show recent logged prompts",
                LIST_SPECS,
                &[]
            )
        );
        return 0;
    }
    let db = match open_db(&args) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    // conversation selection: --cid > -c > latest; -r without cid implies current
    let conversation: Option<String> = if let Some(cid) = args.opt(&["conversation", "cid"]) {
        Some(cid.to_string())
    } else if args.flag(&["current"]) || args.flag(&["response"]) {
        match crate::core::logstore::latest_conversation(&db) {
            Some(id) => Some(id),
            None => {
                eprintln!("Error: No conversations found");
                return 1;
            }
        }
    } else {
        None
    };

    let count: i64 = if conversation.is_some() {
        0
    } else {
        args.opt(&["count"])
            .and_then(|v| v.parse().ok())
            .unwrap_or(3)
    };

    // schema id for --schema / --schema-multi
    let schema_id: Option<String> = if let Some(s) = args.opt(&["schema"]) {
        match crate::core::schemas::resolve_schema(s) {
            Ok(v) => Some(crate::core::logstore::make_schema_id(&v).0),
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        }
    } else {
        None
    };

    // model filter: alias-expanded to its target id
    let model_filter: Option<String> = args.opt(&["model"]).map(|m| {
        let cfg = config::load();
        match cfg.resolve_model(m) {
            Some((n, _, mid)) => format!("{n}/{mid}"),
            None => m.to_string(),
        }
    });

    let query = args.opt(&["query"]).map(|q| q.to_string());
    let search = query.is_some() && !args.flag(&["latest"]);

    let mut rows = crate::core::logstore::collect_rows(
        &db,
        &crate::core::logstore::RowFilters {
            conversation: conversation.as_deref(),
            model: model_filter.as_deref(),
            query: query.as_deref(),
            schema_id: schema_id.as_deref(),
            id_gt: args.opt(&["id-gt"]),
            id_gte: args.opt(&["id-gte"]),
            count: if count > 0 { Some(count) } else { None },
            search,
        },
    );

    // display order is chronological
    rows.reverse();

    if rows.is_empty() {
        return 0;
    }

    if args.flag(&["json"]) {
        let annotated = crate::core::logstore::annotate(&db, rows, args.flag(&["truncate"]));
        println!(
            "{}",
            crate::jsonfmt::dumps_indent(&serde_json::Value::Array(annotated), 2)
        );
        return 0;
    }
    if args.flag(&["response"]) {
        // the most recent row's response
        if let Some(last) = rows.last() {
            let response = last["response"].as_str().unwrap_or_default();
            if args.flag(&["extract", "extract-last", "xl"]) {
                let block = crate::core::render::extract_fenced(
                    response,
                    args.flag(&["extract-last", "xl"]),
                )
                .unwrap_or_else(|| response.to_string());
                println!("{block}");
            } else {
                println!("{response}");
            }
        }
        return 0;
    }
    if args.flag(&["extract"]) || args.flag(&["extract-last", "xl"]) {
        for row in &rows {
            let response = row["response"].as_str().unwrap_or_default();
            if let Some(block) =
                crate::core::render::extract_fenced(response, args.flag(&["extract-last", "xl"]))
            {
                println!("{block}");
                return 0;
            }
        }
        return 0;
    }

    if args.flag(&["full"]) {
        let annotated = crate::core::logstore::annotate(&db, rows, args.flag(&["truncate"]));
        markdown_output(&annotated, &args, conversation.is_some());
        return 0;
    }

    // mode labels resolved once per conversation (db provenance first)
    let mut modes: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for row in &rows {
        let cid = row["conversation_id"].as_str().unwrap_or("");
        if !cid.is_empty() && !modes.contains_key(cid) {
            modes.insert(
                cid.to_string(),
                conversation_mode(Some(&db), cid, 0, &modes),
            );
        }
    }
    compact_output(&rows, &modes, mode_filter);
    0
}

/// Compact index view (the default): one line per turn under a dim
/// conversation header, mirroring the agent `/resume` list.
fn compact_output(
    rows: &[serde_json::Value],
    modes: &std::collections::HashMap<String, String>,
    filter: Option<&str>,
) {
    let now = crate::core::db::now_turn_datetime();
    print!(
        "{}",
        compact_lines(rows, &now, std::io::stdout().is_terminal(), modes, filter)
    );
}

/// The conversation's mode label: explicit per-turn provenance wins, then
/// the agent's cwd marker; pre-provenance sessions
/// fall back to a turn-count guess (a lone turn is a prompt, a thread is a
/// chat).
fn conversation_mode(
    db: Option<&Db>,
    cid: &str,
    turns: usize,
    known: &std::collections::HashMap<String, String>,
) -> String {
    if let Some(m) = known.get(cid) {
        return m.clone();
    }
    if let Some(db) = db
        && let Some(opts) = crate::core::logstore::newest_turn_options(db, cid)
    {
        if let Some(m) = opts.get("mode").and_then(|v| v.as_str()) {
            return m.to_string();
        }
        if opts.get("cwd").is_some() {
            return "agent".to_string();
        }
    }
    if turns == 1 {
        "prompt".to_string()
    } else {
        "chat".to_string()
    }
}

fn compact_lines(
    rows: &[serde_json::Value],
    now: &str,
    tty: bool,
    modes: &std::collections::HashMap<String, String>,
    filter: Option<&str>,
) -> String {
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
    // display-width-aware truncation: CJK glyphs take two cells, so a
    // char-count cap alone lets Chinese prompts run far past the margin
    let cut = |text: &str, max: usize| {
        let mut width = 0usize;
        for (i, c) in text.chars().enumerate() {
            width += crate::core::render_md::char_width(c);
            if width > max.saturating_sub(3) {
                let head: String = text.chars().take(i).collect();
                return format!("{head}...");
            }
        }
        text.to_string()
    };
    let width = if tty {
        crate::term::columns().max(60)
    } else {
        80
    };

    // conversations in row order (newest first), each with its mode label
    struct Group<'a> {
        cid: &'a str,
        rows: &'a [serde_json::Value],
        mode: String,
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let cid = rows[i]["conversation_id"].as_str().unwrap_or("");
        let mut j = i;
        while j < rows.len() && rows[j]["conversation_id"].as_str().unwrap_or("") == cid {
            j += 1;
        }
        groups.push(Group {
            cid,
            rows: &rows[i..j],
            mode: conversation_mode(None, cid, j - i, modes),
        });
        i = j;
    }

    let mut out = String::new();
    for mode in ["agent", "chat", "prompt"] {
        if let Some(f) = filter
            && mode != f
            && f != "all"
        {
            continue;
        }
        let selected: Vec<&Group> = groups.iter().filter(|g| g.mode == mode).collect();
        if selected.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&bold(mode));
        out.push('\n');
        for (gi, g) in selected.iter().enumerate() {
            if gi > 0 {
                out.push('\n');
            }
            let model = g.rows[0]["conversation_model"]
                .as_str()
                .filter(|m| !m.is_empty())
                .or_else(|| g.rows[0]["model"].as_str())
                .unwrap_or("--");
            let shown_cid = if g.cid.is_empty() {
                "--".to_string()
            } else {
                g.cid[..g.cid.len().min(6)].to_string()
            };
            let n = g.rows.len();
            let turns = if n == 1 { "turn" } else { "turns" };
            let last_dt = g.rows[n - 1]["datetime_utc"].as_str().unwrap_or("");
            out.push_str(&dim(&format!(
                "{shown_cid} · {model} · {n} {turns} · {}",
                crate::core::db::short_time(now, last_dt)
            )));
            out.push('\n');
            for (k, row) in g.rows.iter().enumerate() {
                let dt = row["datetime_utc"].as_str().unwrap_or("");
                let prompt = row["prompt"].as_str().unwrap_or("").replace('\n', " ");
                let preview = if prompt.trim().is_empty() {
                    "--".to_string()
                } else {
                    cut(prompt.trim(), width.saturating_sub(12))
                };
                out.push_str(&dim(&format!(
                    "  {}. {}",
                    k + 1,
                    crate::core::db::short_time(now, dt)
                )));
                out.push(' ');
                out.push_str(&preview);
                out.push('\n');
            }
        }
    }
    out
}

fn markdown_output(rows: &[serde_json::Value], args: &ParsedArgs, filtered_by_conversation: bool) {
    let truncate = args.flag(&["truncate"]);
    let mut previous_system: Option<String> = None;
    for (index, row) in rows.iter().enumerate() {
        let datetime = row["datetime_utc"].as_str().unwrap_or("");
        println!("# {datetime}");
        if index == 0 && filtered_by_conversation {
            println!(
                "    conversation: {} id: {}",
                row["conversation_id"].as_str().unwrap_or(""),
                row["id"].as_str().unwrap_or("")
            );
            println!("Model: **{}**", row["model"].as_str().unwrap_or(""));
        }
        let prompt = row["prompt"].as_str().unwrap_or("");
        println!("\n## Prompt\n");
        if prompt.is_empty() {
            println!("-- none --");
        } else {
            let shown = if truncate {
                truncate_string(prompt, 100)
            } else {
                prompt.to_string()
            };
            println!("{shown}");
        }
        let options = row["options_json"].as_object();
        if options.is_some_and(|o| !o.is_empty()) {
            println!("\n## Options\n");
            for (k, v) in options.into_iter().flatten() {
                println!("- {k}: {v}");
            }
        }
        let system = row["system"].as_str().unwrap_or("");
        if !system.is_empty() && previous_system.as_deref() != Some(system) {
            println!("\n## System\n");
            let shown = if truncate {
                truncate_string(system, 100)
            } else {
                system.to_string()
            };
            println!("{shown}");
        }
        previous_system = Some(system.to_string());
        if !row["schema_json"].is_null() {
            println!("\n## Schema\n");
            println!(
                "```json\n{}\n```",
                crate::jsonfmt::dumps_indent(&row["schema_json"], 2)
            );
        }
        let reasoning = row["reasoning"].as_str().unwrap_or("");
        if !reasoning.is_empty() {
            println!("\n## Reasoning\n");
            let shown = if truncate {
                truncate_string(reasoning, 100)
            } else {
                reasoning.to_string()
            };
            println!("{shown}");
        }
        println!("\n## Response\n");
        let response = row["response"].as_str().unwrap_or("");
        let mut pretty = None;
        if !row["schema_json"].is_null()
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(response)
        {
            pretty = Some(crate::jsonfmt::dumps_indent(&parsed, 2));
        }
        match pretty {
            Some(json) => println!("```json\n{json}\n```"),
            None => {
                if truncate {
                    println!("{}", truncate_string(response, 100));
                } else if std::io::stdout().is_terminal() {
                    // styled like a live answer on a terminal; pipes keep raw text
                    let shown = crate::core::render_md::render_once(response, 2);
                    print!("{shown}");
                    if !shown.ends_with('\n') {
                        println!();
                    }
                } else {
                    println!("{response}");
                }
            }
        }
        let attachments = row["attachments"].as_array().cloned().unwrap_or_default();
        if !attachments.is_empty() {
            println!("\n### Attachments\n");
            for (i, att) in attachments.iter().enumerate() {
                let path = att["path"].as_str().unwrap_or("");
                let url = att["url"].as_str().unwrap_or("");
                let type_ = att["type"].as_str().unwrap_or("");
                let target = if !path.is_empty() {
                    path.to_string()
                } else if !url.is_empty() {
                    url.to_string()
                } else {
                    format!("<{} bytes>", att["content_length"].as_i64().unwrap_or(0))
                };
                println!("{}. {}: {}", i + 1, type_, target);
            }
        }
        if args.flag(&["usage"])
            && (!row["input_tokens"].is_null() || !row["output_tokens"].is_null())
        {
            println!("\n## Token usage\n");
            let input = row["input_tokens"].as_i64().unwrap_or(0);
            let output = row["output_tokens"].as_i64().unwrap_or(0);
            println!(
                "{}: {} input, {} output",
                row["model"].as_str().unwrap_or(""),
                input,
                output
            );
        }
        println!();
    }
}

fn status() -> i32 {
    let path = config::logs_db_path();
    if !path.exists() {
        println!("No log database found at {}", path.display());
        return 0;
    }
    let on = config::logs_on();
    println!(
        "Logging is {} for all prompts",
        if on { "ON" } else { "OFF" }
    );
    println!("Found log database at {}", path.display());
    let db = match Db::open() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let counts = crate::core::logstore::store_counts(&db);
    println!("Number of threads logged:\t{}", counts.threads);
    println!("Number of turns logged:\t\t{}", counts.turns);
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!("Database file size: \t\t{}", human_size(size));
    0
}

fn human_size(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < units.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.2}{}", units[unit])
}

fn backup(rest: &[String]) -> i32 {
    let Some(path) = rest.first().cloned() else {
        eprintln!("Error: Missing argument 'PATH'.");
        return 2;
    };
    let db = match Db::open() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    match db.backup_to(&path) {
        Ok(_) => {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            println!("Backed up {} to {}", human_size(size), path);
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::compact_lines;
    use serde_json::json;

    fn row(id: &str, cid: &str, dt: &str, prompt: &str) -> serde_json::Value {
        json!({
            "id": id,
            "conversation_id": cid,
            "model": "prov/m",
            "conversation_model": "prov/m",
            "conversation_name": "",
            "datetime_utc": dt,
            "prompt": prompt,
        })
    }

    #[test]
    fn compact_groups_turns_by_conversation() {
        let rows = vec![
            row(
                "01AAAAAAAAAAAAAAAAAAAAAAAA",
                "01CCCCCCCCCCCCCCCCCCCCCCCC",
                "2026-08-23T01:00:00+00:00",
                "first",
            ),
            row(
                "01BBBBBBBBBBBBBBBBBBBBBBBB",
                "01CCCCCCCCCCCCCCCCCCCCCCCC",
                "2026-08-23T02:00:00+00:00",
                "second",
            ),
            row(
                "01DDDDDDDDDDDDDDDDDDDDDDDD",
                "01EEEEEEEEEEEEEEEEEEEEEEEE",
                "2026-08-22T02:00:00+00:00",
                "other",
            ),
        ];
        let out = compact_lines(
            &rows,
            "2026-08-23T12:00:00+00:00",
            false,
            &std::collections::HashMap::new(),
            None,
        );
        assert!(
            out.starts_with("chat\n01CCCC · prov/m · 2 turns · 02:00"),
            "got {out:?}"
        );
        // singular, and the group recency rides the header
        assert!(
            out.contains("\nprompt\n01EEEE · prov/m · 1 turn · 08/22"),
            "got {out:?}"
        );
        // numbered, indented turn rows; the turn ULID is noise and stays out
        assert!(out.contains("  2. 02:00 second"), "got {out:?}");
        assert!(out.contains("  1. 08/22 other"), "got {out:?}");
        assert!(!out.contains("01BBBBBB"), "got {out:?}");
        assert!(!out.contains('\x1b'), "got {out:?}");
    }

    #[test]
    fn compact_flattens_prompts_and_styles_on_tty() {
        let rows = vec![row(
            "01AAAAAAAAAAAAAAAAAAAAAAAA",
            "01CCCCCCCCCCCCCCCCCCCCCCCC",
            "2026-08-23T01:00:00+00:00",
            "line one\nline two",
        )];
        let piped = compact_lines(
            &rows,
            "2026-08-23T12:00:00+00:00",
            false,
            &std::collections::HashMap::new(),
            None,
        );
        assert!(
            piped.contains("  1. 01:00 line one line two"),
            "got {piped:?}"
        );
        assert!(
            compact_lines(
                &rows,
                "2026-08-23T12:00:00+00:00",
                true,
                &std::collections::HashMap::new(),
                None
            )
            .contains('\x1b')
        );
        let empty = vec![row(
            "01AAAAAAAAAAAAAAAAAAAAAAAA",
            "01CCCCCCCCCCCCCCCCCCCCCCCC",
            "2026-08-23T01:00:00+00:00",
            "",
        )];
        assert!(
            compact_lines(
                &empty,
                "2026-08-23T12:00:00+00:00",
                false,
                &std::collections::HashMap::new(),
                None
            )
            .contains("--")
        );
    }

    #[test]
    fn compact_truncates_cjk_by_display_width() {
        // 40 CJK glyphs are 80 cells wide; a char-count cap would let them
        // run past the margin, the width cap must cut them
        let long = "汉".repeat(40);
        let rows = vec![row(
            "01AAAAAAAAAAAAAAAAAAAAAAAA",
            "01CCCCCCCCCCCCCCCCCCCCCCCC",
            "2026-08-23T01:00:00+00:00",
            &long,
        )];
        let out = compact_lines(
            &rows,
            "2026-08-23T12:00:00+00:00",
            false,
            &std::collections::HashMap::new(),
            None,
        );
        // line 0 is the mode section header, line 1 the conversation header
        let line = out.lines().nth(2).unwrap();
        let cells: usize = line.chars().map(crate::core::render_md::char_width).sum();
        assert!(cells <= 80, "line is {cells} cells: {line:?}");
        assert!(line.ends_with("..."), "got {line:?}");
        // short prompts pass through untouched
        let rows = vec![row(
            "01AAAAAAAAAAAAAAAAAAAAAAAA",
            "01CCCCCCCCCCCCCCCCCCCCCCCC",
            "2026-08-23T01:00:00+00:00",
            "更新Claudemd和agentsmd",
        )];
        let out = compact_lines(
            &rows,
            "2026-08-23T12:00:00+00:00",
            false,
            &std::collections::HashMap::new(),
            None,
        );
        assert!(out.contains("更新Claudemd和agentsmd"), "got {out:?}");
    }
}
