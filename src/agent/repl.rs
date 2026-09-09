//! The interactive REPL: prompt loop, slash commands, banners, model
//! switching, session resume and shell passthrough.

use crate::agent::approval;
use crate::agent::session::Session;
use std::io::{IsTerminal, Write};

use crate::term::render::humanize_tokens;

pub fn repl(
    mut session: crate::agent::session::Session,
    mut skills: Vec<crate::agent::skills::SkillDef>,
    mut attachments: Vec<crate::providers::Attachment>,
) -> Result<i32, String> {
    let mut settings = crate::agent::settings::load();
    let mut editor = crate::term::lineedit::LineEditor::new();

    // ctrl-c during a running task interrupts it instead of killing the REPL
    crate::term::install_sigint_handler();

    print_banner(&session);
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
        let help = repl_help(&session);
        let skill_names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
        let cwd = session.cwd.display().to_string();
        let command_names = session.extensions.command_names();
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
        let line = match editor.read_line(&prompt, &help, &completer) {
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
        if text == "exit" || text == "quit" || text == "/exit" || text == "/quit" {
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

        attach_local_files(text, &mut attachments, session.model.kind == "anthropic");
        run_task_logged(&mut session, text, &mut attachments);

        // steering lines typed after the final model call become new tasks
        while !session
            .steer_queue
            .lock()
            .map(|q| q.is_empty())
            .unwrap_or(true)
        {
            let leftover = session.take_steer_leftover();
            if leftover.is_empty() {
                break;
            }
            for line in leftover {
                eprintln!(
                    "{}→ {line}{}",
                    crate::theme::err().dim,
                    crate::theme::err().reset
                );
                run_task_logged(&mut session, &line, &mut Vec::new());
            }
        }
    }
    restore_default_sigint();
    Ok(0)
}

/// Local file paths mentioned in a prompt ride the message automatically:
/// every whitespace token naming an existing image (always) or PDF
/// (anthropic models support document blocks) loads as an attachment with
/// a dim notice; ctrl+v pastes an image as exactly such a path.
fn attach_local_files(
    text: &str,
    queue: &mut Vec<crate::providers::Attachment>,
    supports_pdf: bool,
) {
    use std::io::Read;
    for token in text.split_whitespace() {
        let path = std::path::Path::new(token);
        if !path.is_file() {
            continue;
        }
        let Ok(mut file) = std::fs::File::open(path) else {
            continue;
        };
        let mut probe = [0u8; 64];
        let Ok(n) = file.read(&mut probe) else {
            continue;
        };
        let mime = match crate::core::attachments::sniff_mime(&probe[..n]) {
            Some(m) if m.starts_with("image/") => m,
            Some("application/pdf") if supports_pdf => "application/pdf",
            _ => continue,
        };
        if let Ok(loaded) = crate::core::attachments::load(token, Some(mime)) {
            let req = loaded.request();
            eprintln!(
                "{}→ attached {} ({}){}",
                crate::theme::err().dim,
                token,
                req.mime_type,
                crate::theme::err().reset
            );
            queue.push(req);
        }
    }
}

/// One agent task with interrupt chrome — the shared path for typed input,
/// /skill:name and steering leftovers. Persistence runs inside run_task.
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
    let prompt = format!(
        "<skill name=\"{}\">\n{}\n</skill>\n\nApply this skill.",
        skill.name,
        body.trim_end()
    );
    run_task_logged(session, &prompt, &mut Vec::new());
}

fn restore_default_sigint() {
    crate::term::restore_sigint_handler();
}

/// Home-directory abbreviation for the status line.
fn abbrev_home(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy().to_string();
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

/// Full help page shown on ctrl+o.
fn repl_help(session: &Session) -> String {
    let p = crate::theme::err();
    let mut h = format!(
        "{}keys      enter submit · ctrl+j, alt+enter or \\ at end = newline · tab complete · ctrl+g editor",
        p.dim
    );
    h.push_str("\n           ↑/↓ history (or move between lines) · ctrl+o this help · esc/ctrl+c interrupt · ctrl+c×2 exit · ctrl+d exit");
    h.push_str("\ncommands   /model /thinking /login /logout /resume /tree /clear /compact /status /yolo /reload /exit · !cmd runs shell · /help lists all");
    h.push_str(&info_rows(session, "    "));
    let model = match &session.thinking {
        Some(level) => format!("{} {level}", session.model.qualified_id()),
        None => session.model.qualified_id(),
    };
    h.push_str(&format!("model      {model}\n"));
    h.push_str(p.reset.as_str());
    h
}

/// The context/session rows shared by the banner and the ctrl+o page.
/// `pad` widens the label to match the surrounding layout.
fn info_rows(session: &Session, pad: &str) -> String {
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
    rows
}

const SLASH_COMMANDS: &[&str] = &[
    "/help",
    "/clear",
    "/yolo",
    "/compact",
    "/status",
    "/exit",
    "/model",
    "/thinking",
    "/login",
    "/logout",
    "/resume",
    "/tree",
    "/reload",
];

/// A near miss of a known slash command ("/clea"), mirroring main.rs's
/// command_hint thresholds so a typo hints instead of burning a model call.
fn slash_hint(word: &str) -> Option<String> {
    let word = word.strip_prefix('/')?;
    let names: Vec<&str> = SLASH_COMMANDS
        .iter()
        .map(|c| c.strip_prefix('/').unwrap_or(c))
        .collect();
    crate::core::text::closest_name(word, &names).map(|n| format!("/{n}"))
}

/// `/resume`: pick a past conversation and load it into this session —
/// history replays, the next task appends to the same thread.
fn resume_pick(session: &mut Session) -> Result<(), String> {
    let store = crate::core::threads::Store::open()?;
    let threads = store.recent_threads(30);
    if threads.is_empty() {
        eprintln!(
            "{}no conversations yet{}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
        return Ok(());
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
    let Some(i) = crate::term::lineedit::pick("resume:", &items, false) else {
        return Ok(());
    };
    let cid = threads[i].id.clone();
    let (msgs, system) = crate::agent::session::rebuild_thread(&store, &cid);
    session.seed = msgs;
    session.system = system;
    session.conversation_id = Some(cid);
    render_history(&session.seed);
    Ok(())
}

/// `/tree`: jump to any past turn of this session — the seed and the thread
/// file are truncated to just before the picked turn, and the next task
/// continues from there.
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
    let items: Vec<String> = turns
        .iter()
        .map(|t| {
            let preview: String = t.prompt.chars().take(50).collect::<String>();
            let preview = preview.replace('\n', " ");
            format!(
                "{} · \"{}\"",
                &t.ts[..t.ts.len().min(19)],
                if preview.trim().is_empty() {
                    "--"
                } else {
                    &preview
                }
            )
        })
        .collect();
    let Some(i) = crate::term::lineedit::pick(
        "jump to turn (everything after it is dropped):",
        &items,
        true,
    ) else {
        return Ok(());
    };
    // the wire messages of the kept turns are the new seed
    let cut: usize = turns[..i].iter().map(|t| t.messages.len()).sum();
    session.seed.truncate(cut);
    store.truncate_thread(&cid, i)?;
    eprintln!(
        "{}rewound to turn {} — type the next task{}",
        crate::theme::err().dim,
        i + 1,
        crate::theme::err().reset
    );
    Ok(())
}

/// Startup banner: bold identity line, then dim label-aligned rows.
fn print_banner(session: &Session) {
    let thinking = session
        .thinking
        .as_deref()
        .map(|l| format!(" {l}"))
        .unwrap_or_default();
    let p = crate::theme::err();
    eprintln!(
        "{p0}llm agent{r} {d}v{v} ·{r} {b}{m}{t}{r} {d}· {a}{r}",
        p0 = p.bold,
        r = p.reset,
        d = p.dim,
        b = p.bold,
        v = crate::VERSION,
        m = session.model.qualified_id(),
        t = thinking,
        a = session.approval.mode.label()
    );
    eprint!("{}{}{}", p.dim, info_rows(session, " "), p.reset);
}

/// Replay the loaded conversation on resume, so the user actually sees what
/// was there before the prompt (bounded to the most recent stretch).
fn render_history(seed: &[crate::providers::Msg]) {
    use crate::providers::Msg;
    const CAP: usize = 40; // messages shown on resume; older history is noted
    if seed.is_empty() {
        return;
    }
    let skip = seed.len().saturating_sub(CAP);
    if skip > 0 {
        eprintln!(
            "{}── {skip} earlier message(s) omitted ──{}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
    }
    for m in &seed[skip..] {
        match m {
            Msg::User { text, .. } => {
                for line in text.split('\n') {
                    eprintln!(
                        "{}>{} {line}",
                        crate::theme::err().bold,
                        crate::theme::err().reset
                    );
                }
            }
            Msg::Assistant { text, tool_calls } => {
                if !text.is_empty() {
                    for line in text.lines() {
                        eprintln!("  {line}");
                    }
                }
                for c in tool_calls {
                    eprintln!(
                        "{}  [tool: {}]{} {}",
                        crate::theme::err().dim,
                        c.name,
                        crate::theme::err().reset,
                        crate::core::text::truncate_chars(&c.arguments.to_string(), 80)
                    );
                }
            }
            Msg::ToolResult {
                name,
                content,
                is_error,
                ..
            } => {
                let first = content.lines().next().unwrap_or("");
                let flag = if *is_error { " ✗" } else { "" };
                eprintln!(
                    "{}  [result{flag} · {name}]{} {}",
                    crate::theme::err().dim,
                    crate::theme::err().reset,
                    crate::core::text::truncate_chars(first, 100)
                );
            }
            Msg::Summary { text } => {
                eprintln!(
                    "{}  <summary>{} {}",
                    crate::theme::err().dim,
                    crate::theme::err().reset,
                    crate::core::text::truncate_chars(text, 120)
                );
            }
        }
    }
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
    if let Some(arg) = buf.strip_prefix("/memory ") {
        // the editor replaces the current WORD, so candidates are bare
        let subs = ["add", "edit"];
        return subs
            .iter()
            .filter(|s| s.starts_with(arg.trim_start()))
            .map(|s| s.to_string())
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
                "/clear         fresh session",
                "/compact       condense history now (also runs automatically)",
                "/status        usage, context and plugins",
                "/yolo          toggle auto-approval",
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
            let current = session.model.qualified_id();
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
                        session.model.qualified_id(),
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
            let qualified = session.model.qualified_id();
            if crate::providers::resolve_model_by_id(&qualified).is_err() {
                eprintln!(
                    "{}current model {qualified} no longer resolves — run /model{}",
                    crate::theme::err().dim,
                    crate::theme::err().reset
                );
            }
        }
        "/yolo" => {
            // a toggle: the way back to ask mode is the same command
            let mode = match session.approval.mode {
                approval::Mode::Yolo => approval::Mode::AlwaysAsk,
                approval::Mode::AlwaysAsk => approval::Mode::Yolo,
            };
            session.approval.mode = mode;
            let note = match mode {
                approval::Mode::Yolo => " · everything auto-approved",
                approval::Mode::AlwaysAsk => " · only in-directory reads auto",
            };
            let p = crate::theme::err();
            eprintln!("{}approval → {}{note}{}", p.dim, mode.label(), p.reset);
        }
        "/clear" => {
            session.clear();
            eprint!("\x1b[2J\x1b[H");
            let _ = std::io::stderr().flush();
            *skills = crate::agent::skills::discover(
                &crate::core::config::user_dir(),
                &session.cwd,
                &settings.disabled_skills,
            );
            print_banner(session);
        }
        "/status" => {
            let p = crate::theme::err();
            let used = crate::agent::compact::estimate_tokens(&session.seed, None);
            let window = session.compact.context_window;
            let pct = (used * 100).checked_div(window).unwrap_or(0);
            eprintln!(
                "  {d}model   {r}{b}{}{r}",
                session.model.qualified_id(),
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
            eprintln!(
                "  {d}context {r}{} / {} ({}%) · {} messages",
                humanize_tokens(used),
                humanize_tokens(window),
                pct,
                session.seed.len(),
                d = p.dim,
                r = p.reset
            );
            eprintln!(
                "  {d}tokens  {r}↑{} ↓{}{} · approval {} · tools {} · thinking {}",
                humanize_tokens(session.tokens.0),
                humanize_tokens(session.tokens.1),
                if session.tokens_cached > 0 {
                    format!(
                        " · cache {}%",
                        crate::core::http::Usage {
                            input: session.tokens.0,
                            output: 0,
                            cached: session.tokens_cached
                        }
                        .cache_percent()
                    )
                } else {
                    String::new()
                },
                session.approval.mode.label(),
                session.tools.len(),
                session.thinking.as_deref().unwrap_or("(model default)"),
                d = p.dim,
                r = p.reset
            );
            let rows = session.extensions.rows().len();
            if rows > 0 {
                eprintln!(
                    "  {}plugins {}{rows} extension(s) · /reload re-reads them",
                    crate::theme::err().dim,
                    crate::theme::err().reset
                );
            }
        }
        "/compact" => {
            // manual compaction: summarize everything older than the
            // keep-recent window, same as the automatic path
            let cfg = session.compact.clone();
            let estimate = crate::agent::compact::estimate_tokens(&session.seed, None);
            match crate::agent::compact::find_cut(&session.seed, cfg.keep_recent_tokens) {
                Some(cut) => {
                    eprintln!(
                        "  {}compacting …{}",
                        crate::theme::err().dim,
                        crate::theme::err().reset
                    );
                    // the `/compact <prompt>` argument rides along as extra
                    // instructions for the summarizer
                    match crate::agent::compact::summarize_with(
                        &session.model,
                        &session.seed[..cut],
                        arg,
                    ) {
                        Ok(summary) if !summary.is_empty() => {
                            session.compact_prefix(summary, cut);
                            let now = crate::agent::compact::estimate_tokens(&session.seed, None);
                            eprintln!(
                                "  {}compacted {} → {}{}",
                                crate::theme::err().dim,
                                crate::term::render::humanize_tokens(estimate),
                                crate::term::render::humanize_tokens(now),
                                crate::theme::err().reset
                            );
                        }
                        _ => eprintln!(
                            "  {}compaction failed; history kept as-is{}",
                            crate::theme::err().dim,
                            crate::theme::err().reset
                        ),
                    }
                }
                None => eprintln!(
                    "  {}nothing to compact yet{}",
                    crate::theme::err().dim,
                    crate::theme::err().reset
                ),
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
                let prompt = if input.is_empty() {
                    cmd.body.clone()
                } else {
                    crate::core::commands_md::expand(&cmd, input)
                };
                run_task_logged(session, &prompt, &mut Vec::new());
                return false;
            }
            // not a builtin and not a commands-dir command: plain task text
            // (covers path-looking words like "/home/me/shot.jpg 看看?").
            // A near miss of a known command only hints — no model call.
            if let Some(name) = slash_hint(other) {
                eprintln!(
                    "{}did you mean {name}? (nothing was sent){}",
                    crate::theme::err().dim,
                    crate::theme::err().reset
                );
                return false;
            }
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
        let dir = std::env::temp_dir().join(format!("llm-tab-{}", crate::core::db::ulid()));
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
    fn slash_commands_are_the_pruned_set() {
        let names: Vec<&str> = SLASH_COMMANDS
            .iter()
            .map(|c| c.strip_prefix('/').unwrap())
            .collect();
        for gone in [
            "memory", "init", "export", "settings", "tools", "ask", "quit", "mcp", "trust",
            "skills",
        ] {
            assert!(!names.contains(&gone), "{gone} should be gone");
        }
        for kept in [
            "model", "thinking", "login", "logout", "resume", "clear", "compact", "status",
            "reload", "yolo", "tree", "help", "exit",
        ] {
            assert!(names.contains(&kept), "{kept} should be listed");
        }
    }

    #[test]
    fn slash_hint_catches_prefixes_and_near_misses() {
        assert_eq!(slash_hint("/cle"), Some("/clear".to_string()));
        assert_eq!(slash_hint("/statu"), Some("/status".to_string()));
        assert_eq!(slash_hint("/hlep"), Some("/help".to_string()));
        // an ordinary word or path-looking text is no typo of a command
        assert_eq!(slash_hint("/etc/passwd"), None);
        assert_eq!(slash_hint("/summarize this file"), None);
        assert_eq!(slash_hint("/x"), None);
    }
}
