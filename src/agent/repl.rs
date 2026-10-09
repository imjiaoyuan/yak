//! The interactive REPL: prompt loop, slash commands, banners, model
//! switching, session resume and shell passthrough.

use crate::agent::session::Session;
use crate::core::threads::StoredTurn;
use std::io::{IsTerminal, Write};

use crate::term::render::{cache_note, humanize_tokens};

pub fn repl(
    mut session: crate::agent::session::Session,
    mut skills: Vec<crate::agent::skills::SkillDef>,
    mut attachments: Vec<crate::providers::Attachment>,
) -> Result<i32, String> {
    let mut settings = crate::agent::settings::load();
    let mut editor = crate::term::lineedit::LineEditor::new();

    // ctrl-c during a running task interrupts it instead of killing the REPL
    crate::term::install_sigint_handler();

    print_banner(&session, &skills);
    render_history(&session.seed);

    let mut exit_presses = crate::term::DoubleInterrupt::new();
    loop {
        // a ctrl-c landing in the cooked-mode window between prompts arrives
        // as SIGINT (flag already set) instead of a keystroke; count it too
        if crate::core::http::interrupted() {
            crate::core::http::clear_interrupt();
            if exit_presses.pressed() {
                break;
            }
            continue;
        }
        let p = crate::theme::err();
        let prompt = format!("{}>{} ", p.bold, p.reset);
        let help = repl_help(&session, &skills);
        let skill_names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
        let cwd = session.cwd.display().to_string();
        let command_names = session.extensions.command_names();
        // the echo highlight rides this predicate: registered names only, so
        // a `/`-prefixed path (`/home/you/shot.png`) stays plain task text
        let command_set = {
            let mut names: Vec<String> = SLASH_COMMANDS
                .iter()
                .map(|c| c.strip_prefix('/').unwrap_or(c).to_string())
                .collect();
            names.extend(command_names.clone());
            names.sort();
            names
        };
        let is_command = move |buf: &str| slash_command_line(buf, &command_set);
        let completer = move |buf: &str| {
            let mut out = completions(buf, &skill_names, &cwd);
            if let Some(arg) = buf.strip_prefix('/')
                && !arg.contains(' ')
            {
                for name in &command_names {
                    if name.starts_with(arg) {
                        out.push(format!("/{name}"));
                    }
                }
                out.sort();
                out.dedup();
            }
            out
        };
        let line = match editor.read_line(&prompt, &help, &completer, &is_command) {
            crate::term::lineedit::LineResult::Line(l) => l,
            crate::term::lineedit::LineResult::Eof => break,
            crate::term::lineedit::LineResult::Interrupt => {
                if exit_presses.pressed() {
                    break;
                }
                continue;
            }
        };
        exit_presses.reset();
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if text == "/exit" {
            break;
        }
        if let Some(rest) = text.strip_prefix('!') {
            let status = crate::platform::run_shell_interactive(rest, &session.cwd);
            match status {
                Ok(s) => {
                    if !s.success() {
                        eprintln!(
                            "{}(exited {}){}",
                            crate::theme::err().dim,
                            s.code().unwrap_or(-1),
                            crate::theme::err().reset
                        );
                    }
                }
                Err(e) => eprintln!("Error: {e}"),
            }
            continue;
        }
        if let Some(name) = text.strip_prefix("/skill:") {
            run_skill(&mut session, &skills, name);
            continue;
        }
        if text.starts_with('/') {
            if repl_command(&mut session, &mut settings, text, &mut skills) {
                break;
            }
            continue;
        }

        attach_local_files(
            text,
            &mut attachments,
            session
                .model
                .as_ref()
                .is_some_and(|m| m.kind == "anthropic"),
        );
        run_task_logged(&mut session, text, &mut attachments);
    }
    restore_default_sigint();
    Ok(0)
}

/// Local file paths mentioned in a prompt ride the message automatically:
/// a whitespace token naming an existing image (always) or PDF (anthropic
/// models support document blocks) loads as an attachment with a dim notice;
/// ctrl+v pastes an image as exactly such a path. A pasted path is often
/// glued to the words typed right after it with no space between (like
/// `…png make-it-this-big`), so the longest existing-file prefix of the
/// token is what counts, not the whole token.
fn attach_local_files(
    text: &str,
    queue: &mut Vec<crate::providers::Attachment>,
    supports_pdf: bool,
) {
    use std::io::Read;
    for token in text.split_whitespace() {
        let Some(path_str) = existing_file_prefix(token) else {
            continue;
        };
        let path = std::path::Path::new(path_str);
        let Ok(mut file) = std::fs::File::open(path) else {
            continue;
        };
        let mut probe = [0u8; 64];
        let Ok(n) = file.read(&mut probe) else {
            continue;
        };
        // the one classification point: anything the adapters would not
        // carry as a pixel or a page is not auto-attached
        let mime = match crate::core::attachments::sniff_mime(&probe[..n]) {
            Some(m)
                if crate::core::attachments::kind_of(m)
                    == Some(crate::core::attachments::Kind::Image) =>
            {
                m
            }
            Some("application/pdf") if supports_pdf => "application/pdf",
            _ => continue,
        };
        if let Ok(loaded) = crate::core::attachments::load(path_str, Some(mime)) {
            let req = loaded.request();
            eprintln!(
                "{}→ attached {} ({}){}",
                crate::theme::err().dim,
                path_str,
                req.mime_type,
                crate::theme::err().reset
            );
            queue.push(req);
        }
    }
}

/// The longest prefix of `token` at a char boundary that names an existing
/// file, so a path glued to trailing text still resolves. `None` when no
/// prefix is a file (the common case: a plain word).
fn existing_file_prefix(token: &str) -> Option<&str> {
    (1..=token.len())
        .rev()
        .find(|&end| token.is_char_boundary(end) && std::path::Path::new(&token[..end]).is_file())
        .map(|end| &token[..end])
}

/// One agent task with interrupt chrome — the shared path for typed input
/// and /skill:name. Persistence runs inside run_task.
fn run_task_logged(
    session: &mut Session,
    text: &str,
    attachments: &mut Vec<crate::providers::Attachment>,
) {
    crate::core::http::clear_interrupt();
    if !attachments.is_empty() {
        eprintln!(
            "{}→ {} attachment{} ride this task{}",
            crate::theme::err().dim,
            attachments.len(),
            if attachments.len() == 1 { "" } else { "s" },
            crate::theme::err().reset
        );
        // the queue can already crowd the request ceiling: the same heads-up
        // the CLI prints for -a, before the pre-flight refuses anything
        if let Some(warning) =
            crate::providers::attachment_weight(attachments, session.max_request_bytes)
        {
            eprintln!("Warning: {warning}");
        }
    }
    // cloned in, cleared on success: a failed task keeps them queued
    match session.run_task(text, attachments.clone()) {
        Ok((outcome, _reasoning)) => {
            attachments.clear();
            if outcome.interrupted {
                if !outcome.final_text.is_empty() && !outcome.final_text.ends_with('\n') {
                    println!();
                }
                eprintln!(
                    "{}interrupted{}",
                    crate::theme::err().dim,
                    crate::theme::err().reset
                );
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            // run_task restores the seed so the session survives errors
        }
    }
}

/// `/skill:name` — load the skill file and submit it as one task.
fn run_skill(session: &mut Session, skills: &[crate::agent::skills::SkillDef], name: &str) {
    let Some(skill) = skills.iter().find(|s| s.name == name) else {
        eprintln!(
            "{}unknown skill '{name}' (see /help){}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
        return;
    };
    let Ok(body) = std::fs::read_to_string(&skill.path) else {
        eprintln!(
            "{}cannot read {}{}",
            crate::theme::err().dim,
            skill.path.display(),
            crate::theme::err().reset
        );
        return;
    };
    let prompt = skill_prompt(&skill.name, &skill.path, &body);
    run_task_logged(session, &prompt, &mut Vec::new());
}

/// The task `/skill:name` submits: the SKILL.md body plus its directory, so
/// the `references/`, `scripts/` and assets a skill points at resolve from
/// anywhere in the repo. A skill body is written relative to its own home
/// (model-invoked skills get the path in the system prompt; a slash
/// invocation had no path at all, so `references/x.md` read against the cwd).
fn skill_prompt(name: &str, path: &std::path::Path, body: &str) -> String {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(path);
    format!(
        "<skill name=\"{name}\" dir=\"{}\">\n{}\n</skill>\n\nApply this skill. \
         The files it references (references/, scripts/) live in that dir.",
        dir.display(),
        body.trim_end()
    )
}

fn restore_default_sigint() {
    crate::term::restore_sigint_handler();
}

/// Home-directory abbreviation for the status line.
fn abbrev_home(path: &str) -> String {
    if let Some(home) = crate::core::paths::home_dir()
        && let Ok(rest) = std::path::Path::new(path).strip_prefix(&home)
    {
        // component-based, so `/home/ann` is not a prefix of
        // `/home/anna/proj` (string-stripping that would print `~a/proj`)
        let rest = rest.to_string_lossy();
        return if rest.is_empty() {
            "~".to_string()
        } else {
            format!("~{}{rest}", std::path::MAIN_SEPARATOR)
        };
    }
    path.to_string()
}

/// Full help page shown on ctrl+o.
fn repl_help(session: &Session, skills: &[crate::agent::skills::SkillDef]) -> String {
    let p = crate::theme::err();
    let mut h = format!(
        "{}keys      enter submit · ctrl+j, alt+enter or \\ at end = newline · tab complete · ctrl+g editor",
        p.dim
    );
    h.push_str("\n           ↑/↓ history (or move between lines) · ctrl+o this help · esc/ctrl+c interrupt · ctrl+c×2 exit · ctrl+d exit");
    h.push_str("\ncommands   /model /thinking /login /logout /resume /tree /clear /status /reload /exit · !cmd runs shell · /help lists all");
    h.push_str(&info_rows(session, skills, "    "));
    let model = match &session.thinking {
        Some(level) => format!("{} {level}", model_label(session)),
        None => model_label(session),
    };
    h.push_str(&format!("model      {model}\n"));
    h.push_str(p.reset.as_str());
    h
}

/// How many names a banner row lists before it says `+N more`: the rows
/// stay one line wide on a normal terminal.
const NAME_CAP: usize = 6;

/// The context/session rows shared by the banner and the ctrl+o page.
/// `pad` widens the label to match the surrounding layout.
fn info_rows(session: &Session, skills: &[crate::agent::skills::SkillDef], pad: &str) -> String {
    let mut rows = String::new();
    // project instructions directly in the working directory, file name only
    let context = ["CLAUDE.md", "AGENTS.md"]
        .iter()
        .find(|name| session.cwd.join(name).is_file())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "none".to_string());
    rows.push_str(&format!(
        "work{pad}{} · context {context}\n",
        abbrev_home(&session.cwd.display().to_string())
    ));
    if let Some(cid) = &session.conversation_id {
        rows.push_str(&format!("session{pad}{cid}\n"));
    }
    // the plugin surface: what is actually mounted right now, so a
    // skill or extension that never loaded is visible here and not only in
    // /help and /status (a label is 7 cells, matching `session`)
    let plugins = session.extensions.plugin_names();
    let skills: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
    rows.push_str(&name_row("plugins", pad, &plugins));
    rows.push_str(&name_row("skills ", pad, &skills));
    // sticky persistence failure: the run continues in memory only, and a
    // healthy-looking banner would hide lost work until /resume
    if let Some(e) = &session.persist_error {
        rows.push_str(&format!("persist{pad}NOT SAVED — {e}\n"));
    }
    rows
}

/// The model shown in the banner and /status: its qualified id, or a
/// pointer to the setup commands when the session has none yet.
fn model_label(session: &Session) -> String {
    match &session.model {
        Some(m) => m.qualified_id(),
        None => "(no model — /login or /model)".to_string(),
    }
}

/// One `names` row, or nothing at all when the list is empty (a bare banner
/// stays bare for someone who mounts neither).
fn name_row(label: &str, pad: &str, names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    let mut list = names
        .iter()
        .take(NAME_CAP)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > NAME_CAP {
        list.push_str(&format!(" +{} more", names.len() - NAME_CAP));
    }
    format!("{label}{pad}{list}\n")
}

const SLASH_COMMANDS: &[&str] = &[
    "/help",
    "/clear",
    "/status",
    "/exit",
    "/model",
    "/thinking",
    "/login",
    "/logout",
    "/resume",
    "/tree",
    "/export",
    "/reload",
];

/// Is this buffer a command line for the echo highlight? Only when its
/// first word is a registered command — exact (arguments allowed) or, still
/// being typed, a prefix of one. A `/`-prefixed path matches no name and
/// stays plain; a bare `/` is still deciding and counts as typing.
fn slash_command_line(buf: &str, names: &[String]) -> bool {
    let Some(rest) = buf.strip_prefix('!') else {
        return slash_command_word(buf, names);
    };
    !rest.trim_end().is_empty()
}

fn slash_command_word(buf: &str, names: &[String]) -> bool {
    let Some(rest) = buf.strip_prefix('/') else {
        return false;
    };
    let (word, has_space) = rest
        .split_once(' ')
        .map_or((rest, false), |(w, _)| (w, true));
    if word.is_empty() {
        return !has_space;
    }
    if rest.starts_with("skill:") && !has_space {
        return true;
    }
    names.iter().any(|n| n == word || n.starts_with(word))
}

/// `/resume`: pick a past conversation — this directory's, newest first —
/// and load it into this session (history replays, the next task appends to
/// the same thread).
fn resume_pick(session: &mut Session) -> Result<(), String> {
    let Some(cid) = crate::commands::threads::pick_thread(&session.cwd, "resume:")? else {
        return Ok(());
    };
    let store = crate::core::threads::Store::open()?;
    let turns = store.read_thread(&cid)?;
    // only the active branch replays, and the conversation continues
    // linearly from its end — jumping into a resumed thread is /tree's
    // business, not the resume's
    let rebuilt = crate::agent::session::rebuild_turns(&turns)?;
    session.seed = rebuilt.messages;
    session.system = rebuilt.system;
    session.usage = rebuilt.usage;
    session.conversation_id = Some(cid);
    session.branch_parent = None;
    render_history(&session.seed);
    // the replay above shows the thread as stored; the projection happens
    // before the next request, where it belongs
    session.prune_seed_to_fit();
    Ok(())
}

/// The action(s) a round ran, read off its assistant message's `tool_calls` —
/// the same `$ <verb> <preview>` the terminal printed. Empty for a round that
/// called no tool.
fn round_actions(messages: &[crate::providers::Msg]) -> Vec<String> {
    use crate::providers::Msg;
    let tools = crate::agent::tools::builtin_tools();
    let mut actions = Vec::new();
    for msg in messages {
        if let Msg::Assistant { tool_calls, .. } = msg {
            for call in tool_calls {
                let verb = crate::agent::tools::display_verb(&call.name);
                let preview = tools
                    .iter()
                    .find(|t| t.name() == call.name)
                    .map(|t| t.preview(&call.arguments))
                    .unwrap_or_else(|| call.name.clone());
                actions.push(format!("{verb} {preview}"));
            }
        }
    }
    actions
}

/// One `/tree` row label: the user's text when the round carried one,
/// otherwise the action(s) it ran, otherwise its answer's first line. A tool
/// round used to render as `--`, which made a long task's rounds
/// indistinguishable.
fn turn_label(t: &crate::core::threads::StoredTurn) -> String {
    let clip = |s: &str| -> String { s.replace('\n', " ").chars().take(50).collect() };
    if !t.prompt.trim().is_empty() {
        return clip(&t.prompt);
    }
    let actions = round_actions(&t.messages);
    if !actions.is_empty() {
        return clip(&actions.join(" · "));
    }
    let text = clip(&t.response);
    if text.trim().is_empty() {
        "--".to_string()
    } else {
        text
    }
}

/// Depth per turn: a turn without a `parent` (or whose parent is the line
/// before it) continues the previous line at its depth; a `parent` naming
/// an earlier line starts a sibling branch one level under that parent.
/// Every turn of the file gets a depth, so the whole tree renders.
fn tree_depths(turns: &[StoredTurn]) -> Vec<usize> {
    let mut by_id = std::collections::HashMap::new();
    for (i, t) in turns.iter().enumerate() {
        by_id.insert(t.id.as_str(), i);
    }
    let mut depths = vec![0usize; turns.len()];
    for i in 0..turns.len() {
        depths[i] = match &turns[i].parent {
            None => i.checked_sub(1).map(|p| depths[p]).unwrap_or(0),
            Some(parent) => match by_id.get(parent.as_str()) {
                Some(&p) if p < i => depths[p] + 1,
                // an edge chain_indices would refuse; the tree view is not
                // the place to fail, so a bad edge sits at the top level
                _ => 0,
            },
        };
    }
    depths
}

/// The picker rows for `/tree`: one per turn in file order, indented to its
/// branch depth, the active branch marked with `*`. Rows are plain text —
/// the picker filters on them, so the depth indent keeps a branch's rows
/// matching a query together.
fn tree_rows(turns: &[StoredTurn], chain: &[usize]) -> Vec<String> {
    let depths = tree_depths(turns);
    let on_chain: std::collections::HashSet<usize> = chain.iter().copied().collect();
    turns
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mark = if on_chain.contains(&i) { "* " } else { "  " };
            let indent = "  ".repeat(depths[i]);
            format!(
                "{mark}{indent}{} · \"{}\"",
                &t.ts[..t.ts.len().min(19)],
                turn_label(t)
            )
        })
        .collect()
}

/// `/tree`: pick any turn of this session and continue from there — the
/// next task branches the thread off that turn instead of extending it.
/// The old rounds are not removed: they stay in the file as a sibling
/// branch (the store is append-only; the transcript never loses a turn),
/// and the tree below shows every branch so the jump can land anywhere.
fn tree_jump(session: &mut Session) -> Result<(), String> {
    let Some(cid) = session.conversation_id.clone() else {
        eprintln!(
            "{}no session yet — /tree needs a saved conversation{}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
        return Ok(());
    };
    let store = crate::core::threads::Store::open()?;
    let turns = store.read_thread(&cid)?;
    if turns.is_empty() {
        eprintln!(
            "{}empty session{}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
        return Ok(());
    }
    // the active branch: last line walked back to the root. It matches the
    // seed the session already carries — landing anywhere else is the
    // jump, landing on it is a no-op that still shows the tree.
    let active = crate::core::threads::chain_indices(&turns, turns.len() - 1)?;
    let items: Vec<String> = tree_rows(&turns, &active);
    let Some(picked) =
        crate::term::lineedit::pick("jump to turn (the next task branches here):", &items, true)
    else {
        return Ok(());
    };
    // `picked` indexes the file-order rows, so it may name a turn on a
    // sibling branch — the chain that ends at that turn is its own walk
    // to the root, which is exactly the conversation the jump replays
    let landed = crate::core::threads::chain_indices(&turns, picked)?;
    // before the conversation moves, extensions get one chance to snapshot
    // or restore the workspace (the git-checkpoint shape): the event names
    // the turns leaving the active branch. Fail-open — a broken extension
    // never blocks the jump; its error lands in the diagnostics tail.
    session.extensions.fire(
        "session_before_tree",
        &tree_event_params(&cid, &turns, &active, &landed),
    );
    // rebuild the seed from the chain that ends at the picked turn, then
    // arm the branch: the next persisted round lands with the picked
    // turn's id as its parent and the thread grows a branch instead of a
    // continuation
    let rebuilt = crate::agent::session::rebuild_chain(&turns, &landed)?;
    session.seed = rebuilt.messages;
    session.usage = rebuilt.usage;
    session.branch_parent = Some(turns[picked].id.clone());
    let side = if picked + 1 == turns.len() {
        "on the active branch's newest turn"
    } else if active.contains(&picked) {
        "to an earlier turn of the active branch"
    } else {
        "onto another branch"
    };
    eprintln!(
        "{}jumped {side}; the next task branches off it (older rounds stay in the thread){}",
        crate::theme::err().dim,
        crate::theme::err().reset
    );
    Ok(())
}

/// The `session_before_tree` payload: which thread's active branch is
/// about to move, and which turns leave it — the active chain minus the
/// chain the jump landed on. Ids, not labels, because the extension keys
/// its snapshots on what the store persists. The turns stay in the file
/// (the thread only grows); what the extension restores is the workspace
/// those turns wrote.
fn tree_event_params(
    thread: &str,
    turns: &[StoredTurn],
    active: &[usize],
    landed: &[usize],
) -> serde_json::Value {
    let keep: std::collections::HashSet<usize> = landed.iter().copied().collect();
    let dropped: Vec<&str> = active
        .iter()
        .copied()
        .filter(|i| !keep.contains(i))
        .map(|i| turns[i].id.as_str())
        .collect();
    serde_json::json!({
        "thread": thread,
        "kept_turns": landed.len(),
        "dropped_turns": dropped.len(),
        "dropped_ids": dropped,
    })
}

/// Startup banner: bold identity line, then dim label-aligned rows.
fn print_banner(session: &Session, skills: &[crate::agent::skills::SkillDef]) {
    let thinking = session
        .thinking
        .as_deref()
        .map(|l| format!(" {l}"))
        .unwrap_or_default();
    let p = crate::theme::err();
    eprintln!(
        "{p0}yak agent{r} {d}v{v} ·{r} {b}{m}{t}{r}",
        p0 = p.bold,
        r = p.reset,
        d = p.dim,
        b = p.bold,
        v = crate::VERSION,
        m = model_label(session),
        t = thinking,
    );
    eprint!("{}{}{}", p.dim, info_rows(session, skills, " "), p.reset);
}

/// Replay the loaded conversation on resume, so the user actually sees what
/// was there before the prompt (bounded to the most recent stretch).
fn render_history(seed: &[crate::providers::Msg]) {
    let width = crate::term::columns().saturating_sub(1).max(20);
    let p = crate::theme::err();
    for row in history_rows(seed, width) {
        match row.style {
            RowStyle::User => eprintln!("{}{}{}", p.bold, row.text, p.reset),
            RowStyle::Plain => eprintln!("{}", row.text),
            RowStyle::Dim => eprintln!("{}{}{}", p.dim, row.text, p.reset),
        }
    }
}

/// How a replayed row is painted; the text itself is style-free so the
/// clipping can be tested without a terminal or a palette.
enum RowStyle {
    User,
    Plain,
    Dim,
}

struct Row {
    style: RowStyle,
    text: String,
}

/// The replay's rows, each clipped to `width` cells. History is a scan, not
/// an answer: a row that wraps would bury the next one under a long tool
/// payload, so every entry gets exactly one line and the payload is cut with
/// an ellipsis instead. `width` is a parameter (and callers pass one column
/// less than the terminal) because a row landing on the last cell can trip
/// the terminal's autowrap and double-space the whole replay.
fn history_rows(seed: &[crate::providers::Msg], width: usize) -> Vec<Row> {
    use crate::core::render_md::{cell_width, truncate_cells};
    use crate::providers::Msg;
    const CAP: usize = 40; // messages shown on resume; older history is noted
    let mut rows = Vec::new();
    if seed.is_empty() {
        return rows;
    }
    let clip = |payload: &str, used: usize| truncate_cells(payload, width.saturating_sub(used));
    let skip = seed.len().saturating_sub(CAP);
    if skip > 0 {
        rows.push(Row {
            style: RowStyle::Dim,
            text: format!("── {skip} earlier message(s) omitted ──"),
        });
    }
    for m in &seed[skip..] {
        match m {
            Msg::User { text, .. } => {
                for line in text.split('\n') {
                    rows.push(Row {
                        style: RowStyle::User,
                        text: format!("> {}", clip(line, 2)),
                    });
                }
            }
            Msg::Assistant {
                text, tool_calls, ..
            } => {
                for line in text.lines() {
                    rows.push(Row {
                        style: RowStyle::Plain,
                        text: format!("  {}", clip(line, 2)),
                    });
                }
                for c in tool_calls {
                    let head = format!("  [tool: {}] ", c.name);
                    let used = cell_width(&head);
                    rows.push(Row {
                        style: RowStyle::Dim,
                        text: format!("{head}{}", clip(&c.arguments.to_string(), used)),
                    });
                }
            }
            Msg::ToolResult {
                name,
                content,
                error,
                ..
            } => {
                let first = content.lines().next().unwrap_or("");
                let flag = if error.is_some() { " ✗" } else { "" };
                let head = format!("  [result{flag} · {name}] ");
                let used = cell_width(&head);
                rows.push(Row {
                    style: RowStyle::Dim,
                    text: format!("{head}{}", clip(first, used)),
                });
            }
            Msg::Summary { text } => {
                rows.push(Row {
                    style: RowStyle::Dim,
                    text: format!("  <summary> {}", clip(text, 12)),
                });
            }
        }
    }
    rows
}

fn completions(buf: &str, skill_names: &[String], cwd: &str) -> Vec<String> {
    if let Some(rest) = buf.strip_prefix('!') {
        return shell_completions(rest, cwd);
    }
    if let Some(arg) = buf.strip_prefix("/skill:") {
        return skill_names
            .iter()
            .filter(|n| n.starts_with(arg))
            .take(12)
            .map(|n| format!("/skill:{n}"))
            .collect();
    }
    if buf.starts_with('/') && !buf.contains(' ') {
        // the mode you are already in is not a completion target
        return SLASH_COMMANDS
            .iter()
            .filter(|c| c.starts_with(buf))
            .map(|c| c.to_string())
            .collect();
    }
    Vec::new()
}

/// Bash-style completion for a `!` shell line: the word at a command
/// position (first word, or right after `|`/`&`/`;`) completes executable
/// names from $PATH, everything else completes paths.
fn shell_completions(rest: &str, cwd: &str) -> Vec<String> {
    let (word, command_position) = shell_word_and_scope(rest);
    if command_position && !word.contains('/') {
        let mut out = path_commands(word);
        // a line-start command replaces the WHOLE word — column 0 — so the
        // candidates must carry the `!` back or the prefix is swallowed
        if bang_prefixed(rest) {
            out = out.into_iter().map(|c| format!("!{c}")).collect();
        }
        out
    } else {
        path_files(word, cwd)
    }
}

/// True when the shell line is just `!<word>`: the completion target is the
/// first word of the line and candidates need the bang.
fn bang_prefixed(rest: &str) -> bool {
    !rest.contains(char::is_whitespace)
}

/// The word being completed, and whether it sits at a command position.
fn shell_word_and_scope(rest: &str) -> (&str, bool) {
    let word = rest
        .rfind(char::is_whitespace)
        .map(|i| rest[i + 1..].trim_start())
        .unwrap_or(rest);
    let segment = rest
        .rfind(['|', '&', ';', '\n'])
        .map(|i| &rest[i + 1..])
        .unwrap_or(rest);
    let command_position = !segment.trim_start().contains(char::is_whitespace);
    (word, command_position)
}

/// Executables on $PATH whose name starts with `prefix`.
fn path_commands(prefix: &str) -> Vec<String> {
    if prefix.is_empty() {
        return Vec::new();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    let mut seen = std::collections::BTreeSet::new();
    for dir in std::env::split_paths(&path) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if seen.len() > 500 {
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(prefix) && is_executable(&entry) {
                seen.insert(name);
            }
        }
    }
    seen.into_iter().take(200).collect()
}

fn is_executable(entry: &std::fs::DirEntry) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        entry
            .metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        entry
            .path()
            .extension()
            .map(|x| x == "exe" || x == "bat" || x == "cmd")
            .unwrap_or(false)
    }
}

/// Filesystem entries matching the word's directory + prefix, as typed
/// (relative stays relative, `~` scans the home but echoes back as typed).
/// Directories complete with a trailing `/` so tabbing keeps walking.
fn path_files(word: &str, cwd: &str) -> Vec<String> {
    let (dir, base) = match word.rfind('/') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };
    let scan = crate::agent::tools::resolve_path(
        std::path::Path::new(cwd),
        if dir.is_empty() { "." } else { dir },
    );
    let Ok(entries) = std::fs::read_dir(&scan) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // hidden entries only when the prefix itself is hidden
            if !name.starts_with(base) || (name.starts_with('.') && !base.starts_with('.')) {
                return None;
            }
            let suffix = if e.path().is_dir() { "/" } else { "" };
            Some(format!("{dir}{name}{suffix}"))
        })
        .collect();
    out.sort();
    out.truncate(200);
    out
}

/// Handle a /command. Returns true when the REPL should exit.
fn repl_command(
    session: &mut Session,
    settings: &mut crate::agent::settings::AgentSettings,
    text: &str,
    skills: &mut Vec<crate::agent::skills::SkillDef>,
) -> bool {
    let (cmd, arg) = match text.split_once(' ') {
        Some((c, a)) => (c, a.trim()),
        None => (text, ""),
    };
    match cmd {
        "/help" => {
            // one command per line, scannable while the picker is open
            for line in [
                "/model         switch model",
                "/thinking      reasoning effort",
                "/login         add a provider",
                "/logout        remove one",
                "/resume        load a past session",
                "/tree          jump between branches of this session",
                "/export [PATH] write this conversation to markdown",
                "/clear         fresh session",
                "/status        usage, context and plugins",
                "/reload        reload skills/extensions",
                "/exit          quit",
                "paste an image with ctrl+v, or just type its path",
                "multi-line: ctrl+j / alt+enter / \\ at end newline · ctrl+g edits in $EDITOR",
            ] {
                eprintln!(
                    "{}  {line}{}",
                    crate::theme::err().dim,
                    crate::theme::err().reset
                );
            }
            // skills are commands (/skill:<name>): they live on the same
            // slash plane, so /help lists them where the commands are
            if !skills.is_empty() {
                let p = crate::theme::err();
                eprintln!(
                    "{}  skills — run one with /skill:<name> (tab completes):{}",
                    p.dim, p.reset
                );
                for s in skills.iter() {
                    let hidden = if s.model_invocation { "" } else { " · hidden" };
                    let desc = crate::core::text::truncate_chars(&s.description, 64);
                    eprintln!("{}  /skill:{}{hidden} — {desc}{}", p.dim, s.name, p.reset);
                }
            }
        }
        "/model" => {
            let current = session
                .model
                .as_ref()
                .map(|m| m.qualified_id())
                .unwrap_or_default();
            let Some(choice) = crate::commands::models::cascade_model_picker(
                &current,
                session.thinking.as_deref(),
            ) else {
                return false;
            };
            match session.switch_model(&choice.model) {
                Ok(()) => {
                    eprintln!(
                        "{}model → {}{}",
                        crate::theme::err().dim,
                        model_label(session),
                        crate::theme::err().reset
                    );
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    return false;
                }
            }
            if let Some(thinking) = choice.thinking {
                session.thinking = thinking.clone();
                if let Err(e) = crate::core::config::try_set_default_thinking(thinking.as_deref()) {
                    eprintln!("Warning: could not save thinking: {e}");
                }
            }
            // the picked model becomes the startup default (the old wizard
            // behavior): the REPL always starts on the stored default
            if let Err(e) = crate::core::config::try_set_default_model(&choice.model) {
                eprintln!("Warning: could not save the default: {e}");
            }
        }
        "/thinking" => {
            if let Some(level) =
                crate::commands::models::thinking_picker(session.thinking.as_deref())
            {
                session.thinking = level.clone();
                if let Err(e) = crate::core::config::try_set_default_thinking(level.as_deref()) {
                    eprintln!("Warning: could not save thinking: {e}");
                }
                eprintln!(
                    "{}thinking → {}{}",
                    crate::theme::err().dim,
                    session.thinking.as_deref().unwrap_or("(model default)"),
                    crate::theme::err().reset
                );
            }
        }
        "/login" => {
            if !std::io::stdin().is_terminal() {
                eprintln!(
                    "{}/login needs a terminal{}",
                    crate::theme::err().dim,
                    crate::theme::err().reset
                );
                return false;
            }
            match crate::commands::login::wizard() {
                Ok(()) => eprintln!(
                    "{}run /model to pick its models{}",
                    crate::theme::err().dim,
                    crate::theme::err().reset
                ),
                Err(e) => eprintln!("Error: {e}"),
            }
        }
        "/logout" => {
            if !std::io::stdin().is_terminal() {
                eprintln!(
                    "{}/logout needs a terminal{}",
                    crate::theme::err().dim,
                    crate::theme::err().reset
                );
                return false;
            }
            if let Err(e) = crate::commands::login::logout_picker() {
                eprintln!("Error: {e}");
            }
            // the default may have been cleared with it: re-resolve lazily on
            // the next task, but warn now if the session model is orphaned
            if let Some(qualified) = session.model.as_ref().map(|m| m.qualified_id())
                && crate::providers::resolve_model_by_id(&qualified).is_err()
            {
                eprintln!(
                    "{}current model {qualified} no longer resolves — run /model{}",
                    crate::theme::err().dim,
                    crate::theme::err().reset
                );
            }
        }
        "/clear" => {
            session.clear();
            eprint!("{}", crate::theme::cursor().clear_screen);
            let _ = std::io::stderr().flush();
            *skills = crate::agent::skills::discover(
                &crate::core::config::user_dir(),
                &session.cwd,
                &settings.disabled_skills,
            );
            print_banner(session, skills);
        }
        "/status" => {
            let p = crate::theme::err();
            // the last round's input covers the entire conversation (each
            // round resends it all) and its output the final answer: the
            // real usage is exact, not a chars/4 estimate
            let n = session.seed.len();
            let used = crate::agent::compact::estimate_tokens(
                &session.seed,
                session.last_usage.map(|u| (n, u)),
            );
            eprintln!(
                "  {d}model   {r}{b}{}{r}",
                model_label(session),
                d = p.dim,
                r = p.reset,
                b = p.bold
            );
            eprintln!(
                "  {d}session {r}{}",
                session
                    .conversation_id
                    .as_deref()
                    .unwrap_or("new (not logged yet)"),
                d = p.dim,
                r = p.reset
            );
            if let Some(e) = &session.persist_error {
                let red = crate::theme::err();
                eprintln!(
                    "  {d}persist {r}{}NOT SAVED — {e}{}",
                    red.bold,
                    red.reset,
                    d = p.dim,
                    r = p.reset
                );
            }
            eprintln!(
                "  {d}context {r}{} · {} messages",
                humanize_tokens(used),
                session.seed.len(),
                d = p.dim,
                r = p.reset
            );
            eprintln!(
                "  {d}tokens  {r}cumulative input {} · output {}{} · tools {} · thinking {}",
                humanize_tokens(session.usage.input),
                humanize_tokens(session.usage.output),
                cache_note(session.usage, session.last_usage),
                session.tools.len(),
                session.thinking.as_deref().unwrap_or("(model default)"),
                d = p.dim,
                r = p.reset
            );
            let rows = session.extensions.rows();
            if !rows.is_empty() {
                let total: usize = rows.iter().map(|r| r.2).sum();
                let commands: usize = rows.iter().map(|r| r.3).sum();
                eprintln!(
                    "  {d}plugins {r}{} extension(s) · {} tool(s){} · /reload re-reads them",
                    rows.len(),
                    total,
                    if commands > 0 {
                        format!(" · {} command(s)", commands)
                    } else {
                        String::new()
                    },
                    d = p.dim,
                    r = p.reset
                );
                for (name, _target, tools, cmds, reason) in &rows {
                    if !reason.is_empty() {
                        eprintln!("  {d}  {name}: {reason}{r}", d = p.dim, r = p.reset);
                    } else {
                        eprintln!(
                            "  {d}  {name}: {} tool(s){}{r}",
                            tools,
                            if *cmds > 0 {
                                format!(", {} command(s)", cmds)
                            } else {
                                String::new()
                            },
                            d = p.dim,
                            r = p.reset
                        );
                    }
                }
            }
        }
        "/resume" => {
            if let Err(e) = resume_pick(session) {
                eprintln!("Error: {e}");
            }
        }
        "/tree" => {
            if let Err(e) = tree_jump(session) {
                eprintln!("Error: {e}");
            }
        }
        "/export" => match session.conversation_id.clone() {
            Some(cid) => {
                let path = (!arg.is_empty()).then_some(arg);
                match crate::core::export::export_thread(&cid, &session.cwd, path) {
                    Ok(path) => eprintln!(
                        "{}exported {cid} → {}{}",
                        crate::theme::err().dim,
                        path.display(),
                        crate::theme::err().reset
                    ),
                    Err(e) => eprintln!("Error: {e}"),
                }
            }
            None => eprintln!(
                "{}no session yet — nothing to export{}",
                crate::theme::err().dim,
                crate::theme::err().reset
            ),
        },
        "/reload" => {
            *skills = crate::agent::skills::discover(
                &crate::core::config::user_dir(),
                &session.cwd,
                &settings.disabled_skills,
            );
            *settings = crate::agent::settings::load();
            session.extensions = crate::agent::ext::Extensions::connect(&session.cwd);
            session.rebuild_tools();
            eprintln!(
                "{}reloaded skills, plugin tools and settings{}",
                crate::theme::err().dim,
                crate::theme::err().reset
            );
        }
        "/exit" => return true,
        other => {
            let name = other.strip_prefix('/').unwrap_or(other);
            // an extension-registered command runs out-of-process and its
            // reply prints
            if other.starts_with('/')
                && let Some(ext) = session.extensions.command_owner(name)
            {
                match ext.run_command(name, arg) {
                    Ok(text) => {
                        for line in text.lines() {
                            eprintln!(
                                "  {}{line}{}",
                                crate::theme::err().dim,
                                crate::theme::err().reset
                            );
                        }
                    }
                    Err(e) => eprintln!("Error: {e}"),
                }
                return false;
            }
            // otherwise a commands-dir prompt template: the file's body
            // (plus any trailing words) becomes one agent task
            if let Some(cmd) = crate::core::commands_md::find(name) {
                let input = arg.trim();
                let (prompt, system) = crate::core::commands_md::expand(&cmd, input);
                // frontmatter `system` rides this task only, appended to the
                // session prompt (the `--append-system-prompt` semantic) so
                // the agent keeps its tool guidance; restored afterwards
                let base = session.system.clone();
                if let Some(s) = &system {
                    session.system = Some(match &base {
                        Some(b) => format!("{b}\n\n{s}"),
                        None => s.clone(),
                    });
                }
                run_task_logged(session, &prompt, &mut Vec::new());
                session.system = base;
                return false;
            }
            // not a builtin and not a commands-dir command: plain task text
            // (covers path-looking words like "/home/me/shot.jpg see?").
            run_task_logged(session, text, &mut Vec::new());
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_row_lists_the_plugin_surface_and_caps_it() {
        // neither surface mounted: no row at all
        assert_eq!(name_row("skills ", " ", &[]), "");
        // the labels stay 7 cells wide, so the banner is one column
        assert_eq!(
            name_row("skills ", " ", &["demo".into(), "probe".into()]),
            "skills  demo, probe\n"
        );
        assert_eq!(
            name_row("plugins", "    ", &[("tool (failed)").into()]),
            "plugins    tool (failed)\n"
        );
        // more than the cap: the extra count, never a second line
        let many: Vec<String> = (0..NAME_CAP + 3).map(|i| format!("s{i}")).collect();
        assert_eq!(
            name_row("skills ", " ", &many),
            "skills  s0, s1, s2, s3, s4, s5 +3 more\n"
        );
    }

    #[test]
    fn home_abbreviation_respects_path_boundaries() {
        let Some(home) = crate::core::paths::home_dir() else {
            return;
        };
        let inside = home.join("proj");
        assert!(
            abbrev_home(&inside.display().to_string()).starts_with('~'),
            "a path under home abbreviates"
        );
        // a sibling sharing only the string prefix of home is left alone
        let sibling = format!(
            "{home_s}x{sep}proj",
            home_s = home.display(),
            sep = std::path::MAIN_SEPARATOR
        );
        assert_eq!(abbrev_home(&sibling), sibling);
    }

    #[test]
    fn slash_skill_prompt_carries_the_skill_dir() {
        let path = std::path::Path::new("/pkg/shuorenhua/SKILL.md");
        let prompt = skill_prompt("shuorenhua", path, "body\n");
        assert!(prompt.starts_with("<skill name=\"shuorenhua\" dir=\"/pkg/shuorenhua\">"));
        assert!(prompt.contains("\nbody\n</skill>"));
        assert!(prompt.ends_with("live in that dir."));
        // a path with no parent still renders (never panic on the dir)
        assert!(
            skill_prompt("x", std::path::Path::new("SKILL.md"), "b").contains("dir=\"SKILL.md\"")
        );
    }

    #[test]
    fn history_replay_clips_every_row_to_the_terminal_width() {
        use crate::providers::{Msg, ToolCall};
        let long = "x".repeat(400);
        let seed = vec![
            Msg::user("a user line that is quite long indeed\nsecond line"),
            Msg::Assistant {
                text: long.clone(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({ "command": long.clone() }),
                }],
                reasoning: None,
                reasoning_meta: None,
            },
            Msg::ToolResult {
                call_id: "c1".into(),
                name: "bash".into(),
                content: format!("{long}\nmore"),
                error: Some(crate::providers::ToolError::Failed),
                attachments: Vec::new(),
            },
            Msg::Summary { text: long.clone() },
        ];
        let width = 40;
        let rows = history_rows(&seed, width);
        // every row fits, wide glyphs charged their real cells
        for r in &rows {
            assert!(
                crate::core::render_md::cell_width(&r.text) <= width,
                "row overflows {width} cells: {}",
                r.text
            );
        }
        assert_eq!(rows.len(), 6); // two user lines, assistant text, tool, result, summary
        assert!(rows[0].text.starts_with("> a user line"));
        assert!(rows[3].text.contains("[tool: bash]"));
        assert!(rows[4].text.contains("[result ✗ · bash]"));
        assert!(rows[5].text.starts_with("  <summary>"));
        // a short history is replayed whole, with no omission note
        assert!(history_rows(&[Msg::user("hi")], width)[0].text == "> hi");
        assert!(history_rows(&[], width).is_empty());
    }

    #[test]
    fn history_replay_notes_the_dropped_prefix() {
        use crate::providers::Msg;
        let seed: Vec<Msg> = (0..45).map(|i| Msg::user(format!("m{i}"))).collect();
        let rows = history_rows(&seed, 80);
        assert!(rows[0].text.contains("5 earlier message(s) omitted"));
        assert_eq!(rows.len(), 41);
    }

    #[test]
    fn line_start_command_candidates_carry_the_bang() {
        assert!(bang_prefixed("gi"));
        assert!(bang_prefixed(""));
        // after a space or a pipe the word is no longer the line start
        assert!(!bang_prefixed("cat he"));
        assert!(!bang_prefixed("ls | gr"));
    }

    #[test]
    fn shell_scope_detects_command_positions() {
        assert_eq!(shell_word_and_scope("gi"), ("gi", true));
        assert_eq!(shell_word_and_scope("git che"), ("che", false));
        // right after a pipe or separator is a command position again
        assert_eq!(shell_word_and_scope("ls | gr"), ("gr", true));
        assert_eq!(shell_word_and_scope("cd /tmp; mk"), ("mk", true));
        assert_eq!(shell_word_and_scope("cat sr"), ("sr", false));
        // an explicit path is a path even at a command position
        assert_eq!(shell_word_and_scope("./confi"), ("./confi", true));
        assert_eq!(shell_word_and_scope(""), ("", true));
    }

    #[test]
    fn path_files_complete_as_typed_with_dir_slashes() {
        let dir = crate::core::testutil::scratch_dir("tab");
        std::fs::create_dir_all(dir.join("gamma")).unwrap();
        std::fs::write(dir.join("alpha.txt"), "a").unwrap();
        std::fs::write(dir.join(".hid"), "h").unwrap();
        let cwd = dir.display().to_string();
        assert_eq!(path_files("al", &cwd), vec!["alpha.txt".to_string()]);
        assert_eq!(path_files("ga", &cwd), vec!["gamma/".to_string()]);
        assert_eq!(path_files(".h", &cwd), vec![".hid".to_string()]);
        assert_eq!(path_files("alpha.txt", &cwd), vec!["alpha.txt".to_string()]);
        // empty base lists every visible entry (gamma is empty here)
        assert_eq!(path_files("gamma/", &cwd), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pasted_path_glued_to_text_still_attaches() {
        let dir = crate::core::testutil::scratch_dir("autoattach");
        let shot = dir.join("shot.png");
        std::fs::write(&shot, [0x89, b'P', b'N', b'G', 0, 0]).unwrap();
        // ctrl+v drops the path in, then the user keeps typing with no space
        let text = format!("{}make-it-this-big", shot.display());
        let mut queue = Vec::new();
        attach_local_files(&text, &mut queue, false);
        assert_eq!(queue.len(), 1, "the glued path attaches");
        assert_eq!(queue[0].filename.as_deref(), Some("shot.png"));
        // a plain word names no file and attaches nothing
        let mut none = Vec::new();
        attach_local_files("no file here make-it-this-big", &mut none, false);
        assert!(none.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slash_commands_are_the_pruned_set() {
        let names: Vec<&str> = SLASH_COMMANDS
            .iter()
            .map(|c| c.strip_prefix('/').unwrap())
            .collect();
        for gone in [
            "memory", "init", "settings", "tools", "ask", "quit", "mcp", "trust", "skills",
            "compact",
        ] {
            assert!(!names.contains(&gone), "{gone} should be gone");
        }
        for kept in [
            "model", "thinking", "login", "logout", "resume", "clear", "status", "reload", "tree",
            "export", "help", "exit",
        ] {
            assert!(names.contains(&kept), "{kept} should be listed");
        }
    }

    #[test]
    fn slash_highlight_matches_registered_commands_only() {
        // the editor's echo bolds only what actually dispatches as a
        // command; a `/`-prefixed path is ordinary task text
        let names = [
            "clear".to_string(),
            "status".to_string(),
            "export".to_string(),
        ];
        assert!(slash_command_line("/clear", &names));
        assert!(slash_command_line("/c", &names));
        assert!(slash_command_line("/export /tmp/a.md", &names));
        assert!(slash_command_line("/", &names));
        assert!(slash_command_line("!ls", &names));
        assert!(!slash_command_line("!", &names));
        assert!(!slash_command_line("/home/you/paste.png", &names));
        assert!(!slash_command_line("/no-such-cmd", &names));
        assert!(!slash_command_line("/x /", &names));
        assert!(!slash_command_line("task", &names));
    }

    #[test]
    fn tree_rows_name_a_rounds_action_not_a_blank() {
        use crate::core::threads::StoredTurn;
        use crate::providers::{Msg, ToolCall};
        let call = |name: &str, arguments: serde_json::Value| ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments,
        };
        let stored = |prompt: &str, response: &str, messages: Vec<Msg>| StoredTurn {
            v: 0,
            id: "t".into(),
            parent: None,
            ts: "2026-09-24T11:14:07".into(),
            mode: "agent".into(),
            model: "m".into(),
            cwd: None,
            system: None,
            prompt: prompt.into(),
            response: response.into(),
            reasoning: None,
            usage: None,
            duration_ms: None,
            options: Vec::new(),
            messages,
        };
        // a tool round reads as the actions it ran, joined
        let tool_round = stored(
            "",
            "",
            vec![Msg::Assistant {
                text: String::new(),
                tool_calls: vec![
                    call("read", serde_json::json!({"path": "R/cli.R"})),
                    call("webfetch", serde_json::json!({"url": "https://x.dev"})),
                ],
                reasoning: None,
                reasoning_meta: None,
            }],
        );
        assert_eq!(
            turn_label(&tool_round),
            "read R/cli.R · webfetch https://x.dev"
        );
        // a user round shows the text, a plain answer its first line
        assert_eq!(
            turn_label(&stored("read cli.R", "", Vec::new())),
            "read cli.R"
        );
        assert_eq!(turn_label(&stored("", "all done", Vec::new())), "all done");
        // nothing to say stays the placeholder, not an empty quote
        assert_eq!(turn_label(&stored("", "", Vec::new())), "--");
    }

    /// The tree view indents a branch one level under its parent and
    /// marks only the active branch: the abandoned sibling sits unmarked
    /// at the same depth as the work that replaced it.
    #[test]
    fn tree_rows_indent_and_mark_the_active_branch() {
        use crate::core::threads::StoredTurn;
        use crate::providers::Msg;
        let turn = |id: &str, parent: Option<String>| StoredTurn {
            v: 0,
            id: id.into(),
            parent,
            ts: "2026-09-24T11:14:07".into(),
            mode: "agent".into(),
            model: "m".into(),
            cwd: None,
            system: None,
            prompt: id.into(),
            response: String::new(),
            reasoning: None,
            usage: None,
            duration_ms: None,
            options: Vec::new(),
            messages: vec![Msg::User {
                text: id.into(),
                attachments: Vec::new(),
            }],
        };
        // root, continuation, abandoned tip; then a branch off the root
        let turns = vec![
            turn("a", None),
            turn("b", None),
            turn("c", None),
            turn("d", Some("a".into())),
        ];
        assert_eq!(tree_depths(&turns), vec![0, 0, 0, 1]);
        // the active chain ends at the branch
        let chain = crate::core::threads::chain_indices(&turns, 3).unwrap();
        assert_eq!(chain, vec![0, 3]);
        let rows = tree_rows(&turns, &chain);
        assert_eq!(rows.len(), 4, "every turn of the file gets a row");
        assert!(
            rows[0].starts_with("* "),
            "the root is on the chain: {}",
            rows[0]
        );
        assert!(rows[1].starts_with("  "), "the sibling is off the chain");
        assert!(
            rows[3].starts_with("*   "),
            "the branch is marked and indented"
        );
        // a bad edge renders at the top level rather than failing the view
        let depths = tree_depths(&[turn("a", None), turn("e", Some("ghost".into()))]);
        assert_eq!(depths[1], 0, "a dangling edge sits at the top");
    }

    #[test]
    fn tree_jump_event_names_what_is_dropped() {
        use crate::providers::Msg;
        let turn = |id: &str| StoredTurn {
            v: 0,
            id: id.into(),
            parent: None,
            ts: "2026-09-24T11:14:07".into(),
            mode: "agent".into(),
            model: "m".into(),
            cwd: None,
            system: None,
            prompt: "p".into(),
            response: String::new(),
            reasoning: None,
            usage: None,
            duration_ms: None,
            options: Vec::new(),
            messages: vec![Msg::User {
                text: "p".into(),
                attachments: Vec::new(),
            }],
        };
        let turns = vec![turn("a"), turn("b"), turn("c"), turn("d")];
        let chain: Vec<usize> = (0..4).collect();
        let p = tree_event_params("thread-1", &turns, &chain, &chain[..2]);
        assert_eq!(p["thread"], "thread-1");
        assert_eq!(p["kept_turns"], 2);
        assert_eq!(p["dropped_turns"], 2);
        assert_eq!(p["dropped_ids"], serde_json::json!(["c", "d"]));
        // jumping to the last turn leaves nothing inactive, but the event
        // still fires: the extension learns the jump happened and that no
        // work is undone
        let none = tree_event_params("thread-1", &turns, &chain, &chain);
        assert_eq!(none["dropped_ids"], serde_json::json!([]));
        // landing on a sibling branch keeps only the shared prefix; the
        // old tip counts as dropped even though it stays in the file
        let sibling = vec![0, 1, 3];
        let crossed = tree_event_params("thread-1", &turns, &chain, &sibling);
        assert_eq!(crossed["dropped_ids"], serde_json::json!(["c"]));
    }
}
