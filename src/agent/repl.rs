//! The interactive REPL: prompt loop, slash commands, banners, model
//! switching, session resume and shell passthrough.

use crate::agent::approval;
use crate::agent::session::Session;
use std::io::Write;

use crate::core::render::humanize_tokens;

pub fn repl(
    mut session: crate::agent::session::Session,
    agents: Vec<crate::agent::task::AgentDef>,
    mut skills: Vec<crate::agent::skills::SkillDef>,
    mut attachments: Vec<crate::providers::Attachment>,
) -> Result<i32, String> {
    let mut settings = crate::agent::settings::load();
    let mut editor = crate::term::lineedit::LineEditor::new();

    // ctrl-c during a running task interrupts it instead of killing the REPL
    crate::term::install_sigint_handler();

    if session.chat_mode {
        eprintln!("\x1b[1mllm chat\x1b[0m · {}", session.model.qualified_id());
    } else {
        print_banner(&session, &agents);
    }
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
        let prompt = "\x1b[1m>\x1b[0m ";
        let help = repl_help(&session, &agents);
        let skill_names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
        let mode = session.approval.mode;
        let completer = move |buf: &str| completions(buf, &skill_names, mode);
        let line = match editor.read_line(prompt, &help, &completer) {
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
                        eprintln!("\x1b[2m(exited {})\x1b[0m", s.code().unwrap_or(-1));
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
                eprintln!("\x1b[2m→ {line}\x1b[0m");
                run_task_logged(&mut session, &line, &mut Vec::new());
            }
        }
    }
    // [agent] memory = "auto": extract once on the way out, long sessions only
    if settings.memory.as_deref() == Some("auto") {
        let turns = session
            .seed
            .iter()
            .filter(|m| matches!(m, crate::providers::Msg::User { .. }))
            .count();
        if turns > 5 {
            eprintln!("\x1b[2mextracting memories …\x1b[0m");
            let _ = crate::agent::memory::update(&session.model, &session.seed);
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
            eprintln!("\x1b[2m→ attached {} ({})\x1b[0m", token, req.mime_type);
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
            "\x1b[2m→ {} attachment{} ride this task\x1b[0m",
            attachments.len(),
            if attachments.len() == 1 { "" } else { "s" }
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
                eprintln!("\x1b[2minterrupted\x1b[0m");
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
        eprintln!("\x1b[2munknown skill '{name}' (try /skills)\x1b[0m");
        return;
    };
    let Ok(body) = std::fs::read_to_string(&skill.path) else {
        eprintln!("\x1b[2mcannot read {}\x1b[0m", skill.path.display());
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

/// Compose a prompt in $EDITOR; returns None when the editor fails or
/// closes with an empty buffer.
fn editor_prompt() -> Option<String> {
    let path = std::env::temp_dir().join(format!("llm-{}.md", crate::core::db::ulid()));
    std::fs::write(&path, "").ok()?;
    let editor =
        std::env::var("EDITOR").unwrap_or_else(|_| crate::platform::default_editor().to_string());
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    Some(content)
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
fn repl_help(session: &Session, agents: &[crate::agent::task::AgentDef]) -> String {
    let mut h = String::from(
        "\x1b[2mkeys      enter submit · alt+enter or trailing \\ = newline · tab complete",
    );
    h.push_str("\n           ↑/↓ history · ctrl+o this help · esc/ctrl+c interrupt · ctrl+c×2 exit · ctrl+d exit");
    h.push_str("\ncommands   /help /clear /init /mcp /edit /exit · !cmd runs shell");
    // only the switch away from the current mode is worth showing
    let switches: Vec<&str> = [
        ("/ask", approval::Mode::AlwaysAsk),
        ("/yolo", approval::Mode::Yolo),
    ]
    .into_iter()
    .filter(|(_, m)| *m != session.approval.mode)
    .map(|(cmd, _)| cmd)
    .collect();
    h.push_str(&format!(
        "\nmodes     {} (current: {})\n",
        switches.join(" "),
        session.approval.mode.label()
    ));
    h.push_str(&info_rows(session, agents, "    "));
    let model = match &session.thinking {
        Some(level) => format!("{} {level}", session.model.qualified_id()),
        None => session.model.qualified_id(),
    };
    h.push_str(&format!("model      {model}\n"));
    h.push_str("\x1b[0m");
    h
}

/// The context/agents/session rows shared by the banner and the ctrl+o page.
/// `pad` widens the label to match the surrounding layout.
fn info_rows(session: &Session, agents: &[crate::agent::task::AgentDef], pad: &str) -> String {
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
    if !agents.is_empty() {
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        rows.push_str(&format!("agents {pad} {}\n", names.join(", ")));
    }
    if let Some(cid) = &session.conversation_id {
        rows.push_str(&format!("session{pad}{cid}\n"));
    }
    rows
}

const SLASH_COMMANDS: &[&str] = &[
    "/help", "/clear", "/ask", "/yolo", "/skills", "/memory", "/compact", "/init", "/status", "/mcp",
    "/edit", "/resume", "/fork", "/name", "/undo", "/exit",
];

/// Startup banner: bold identity line, then dim label-aligned rows.
fn print_banner(session: &Session, agents: &[crate::agent::task::AgentDef]) {
    let thinking = session
        .thinking
        .as_deref()
        .map(|l| format!(" {l}"))
        .unwrap_or_default();
    eprintln!(
        "\x1b[1mllm agent\x1b[0m \x1b[2mv{} ·\x1b[0m \x1b[1m{}{}\x1b[0m \x1b[2m· {}\x1b[0m",
        crate::VERSION,
        session.model.qualified_id(),
        thinking,
        session.approval.mode.label()
    );
    eprint!("\x1b[2m{}\x1b[0m", info_rows(session, agents, " "));
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
        eprintln!("\x1b[2m── {skip} earlier message(s) omitted ──\x1b[0m");
    }
    for m in &seed[skip..] {
        match m {
            Msg::User { text, .. } => {
                for line in text.split('\n') {
                    eprintln!("\x1b[1m>\x1b[0m {line}");
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
                        "\x1b[2m  [tool: {}]\x1b[0m {}",
                        c.name,
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
                    "\x1b[2m  [result{flag} · {name}]\x1b[0m {}",
                    crate::core::text::truncate_chars(first, 100)
                );
            }
            Msg::Summary { text } => {
                eprintln!(
                    "\x1b[2m  <summary>\x1b[0m {}",
                    crate::core::text::truncate_chars(text, 120)
                );
            }
        }
    }
}

const INIT_TASK: &str = "Create or update AGENTS.md at the repository root, quickly — the \
                         AGENTS.md is this project's agent convention file (write CLAUDE.md only \
                         if the repo already has one and no AGENTS.md). Do one bounded scan and \
                         stop — do NOT read source files or work through a checklist. Run at \
                         most: `ls` the repo root, then read up to three of README* / AGENTS.md / \
                         CLAUDE.md / Cargo.toml / package.json / pyproject.toml / Makefile / \
                         go.mod (the ones that exist). Write it in the repository's existing doc \
                         language and shape. Keep it to a few short sections: build/test/run \
                         commands, a one-line architecture note, and conventions. Do not try to \
                         verify every claim; be brief and factual.";

fn completions(buf: &str, skill_names: &[String], mode: approval::Mode) -> Vec<String> {
    if let Some(arg) = buf.strip_prefix("/skill:") {
        return skill_names
            .iter()
            .filter(|n| n.starts_with(arg))
            .take(12)
            .map(|n| format!("/skill:{n}"))
            .collect();
    }
    if let Some(arg) = buf.strip_prefix("/memory ") {
        let subs = ["add", "update", "clean", "edit"];
        return subs
            .iter()
            .filter(|s| s.starts_with(arg.trim_start()))
            .map(|s| format!("/memory {s}"))
            .collect();
    }
    if buf.starts_with('/') && !buf.contains(' ') {
        // the mode you are already in is not a completion target
        return SLASH_COMMANDS
            .iter()
            .filter(|c| c.starts_with(buf))
            .filter(|c| match mode {
                approval::Mode::AlwaysAsk => **c != "/ask",
                approval::Mode::Yolo => **c != "/yolo",
            })
            .map(|c| c.to_string())
            .collect();
    }
    Vec::new()
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
            eprintln!("\x1b[2m  /clear        fresh session       /skills  list skills");
            eprintln!("  /skill:name   run one             /memory  global memory");
            eprintln!("  /yolo         toggle approvals    /status  usage stats");
            eprintln!("  /init         write an AGENTS.md   /mcp     mcp server status");
            eprintln!("  /resume       reopen a session     /fork   fork this session");
            eprintln!("  /name         rename this session  /undo   drop the last round");
            eprintln!("  /edit         compose in $EDITOR /exit    quit");
            eprintln!("  paste an image with ctrl+v, or just type its path");
            eprintln!("  models live in `llm models` (set agent defaults there)\x1b[0m");
        }
        "/ask" | "/yolo" => {
            let mode = if text == "/ask" {
                approval::Mode::AlwaysAsk
            } else {
                approval::Mode::Yolo
            };
            session.approval.mode = mode;
            let note = match mode {
                approval::Mode::Yolo => " · everything auto-approved",
                approval::Mode::AlwaysAsk => " · only in-directory reads auto",
            };
            eprintln!("\x1b[2mapproval → {}{note}\x1b[0m", mode.label());
        }
        "/clear" => {
            session.seed.clear();
            session.conversation_id = None;
            session.tokens = (0, 0);
            eprint!("\x1b[2J\x1b[H");
            let _ = std::io::stderr().flush();
            let agents =
                crate::agent::task::discover(&crate::core::config::user_dir(), &session.cwd);
            *skills = crate::agent::skills::discover(
                &crate::core::config::user_dir(),
                &session.cwd,
                &settings.disabled_skills,
            );
            print_banner(session, &agents);
        }
        "/undo" => {
            let last_user = session
                .seed
                .iter()
                .rposition(|m| matches!(m, crate::providers::Msg::User { .. }));
            let Some(idx) = last_user else {
                eprintln!("\x1b[2mnothing to undo\x1b[0m");
                return false;
            };
            session.seed.truncate(idx);
            if let (Some(db), Some(cid)) = (&session.db, &session.conversation_id)
                && let Err(e) = crate::core::logstore::undo_thread(db, cid)
            {
                eprintln!("Warning: {e}");
            }
            eprintln!("\x1b[2mundo — dropped the last round\x1b[0m");
        }
        "/status" => {
            let used = crate::agent::compact::estimate_tokens(&session.seed, None);
            let window = session.compact.context_window;
            let pct = (used * 100).checked_div(window).unwrap_or(0);
            eprintln!(
                "  \x1b[2mmodel   \x1b[0m\x1b[1m{}\x1b[0m",
                session.model.qualified_id()
            );
            eprintln!(
                "  \x1b[2msession \x1b[0m{}",
                session
                    .conversation_id
                    .as_deref()
                    .unwrap_or("new (not logged yet)")
            );
            eprintln!(
                "  \x1b[2mcontext \x1b[0m{} / {} ({}%) · {} messages",
                humanize_tokens(used),
                humanize_tokens(window),
                pct,
                session.seed.len()
            );
            eprintln!(
                "  \x1b[2mtokens  \x1b[0m↑{} ↓{} · approval {} · tools {} · thinking {}",
                humanize_tokens(session.tokens.0),
                humanize_tokens(session.tokens.1),
                session.approval.mode.label(),
                session.tools.len(),
                session.thinking.as_deref().unwrap_or("(model default)")
            );
        }
        "/skills" => {
            if skills.is_empty() {
                eprintln!(
                    "\x1b[2m  no skills found — drop SKILL.md folders into ~/.llm/skills/ or .llm/skills/\x1b[0m"
                );
                return false;
            }
            for s in skills.iter() {
                let hidden = if s.model_invocation { "" } else { " · hidden" };
                let desc = crate::core::text::truncate_chars(&s.description, 72);
                eprintln!(
                    "  \x1b[2m/skill:{}{hidden}\x1b[0m\x1b[2m — {desc}\x1b[0m",
                    s.name
                );
            }
            eprintln!(
                "  \x1b[2mskills live in ~/.llm/skills, ~/.agents/skills (shared) and .llm/skills (project);\x1b[0m"
            );
            eprintln!(
                "  \x1b[2mdelete a folder to remove one, or list it under [agent] disabled_skills in config.json\x1b[0m"
            );
        }

        "/mcp" => {
            let registry = &session.mcp;
            let rows = registry.rows();
            if rows.is_empty() {
                eprintln!(
                    "\x1b[2mno mcp servers configured — add them to config.json under \"mcpServers\"\x1b[0m"
                );
                return false;
            }
            for row in rows {
                if row.ready {
                    eprintln!(
                        "\x1b[2m{} — {} · ready ({} tools)\x1b[0m",
                        row.name, row.target, row.tools
                    );
                } else {
                    eprintln!(
                        "\x1b[0m{}\x1b[2m — {} · failed: {}\x1b[0m",
                        row.name, row.target, row.reason
                    );
                    let tail = registry.tail_lines(&row.name);
                    for line in tail.iter().skip(tail.len().saturating_sub(3)) {
                        eprintln!("\x1b[2m  {line}\x1b[0m");
                    }
                }
            }
        }
        "/memory" => {
            let path = crate::agent::memory::memory_path();
            let (sub, rest) = match arg.split_once(' ') {
                Some((s, r)) => (s, r.trim()),
                None => (arg, ""),
            };
            match sub {
                "" => {
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    let doc = crate::agent::memory::parse(&text);
                    let manual_lines = doc.manual.lines().filter(|l| !l.trim().is_empty()).count();
                    eprintln!(
                        "\x1b[2m{} · {} manual lines · {} auto memories\x1b[0m",
                        path.display(),
                        manual_lines,
                        doc.auto.len()
                    );
                    for l in doc.manual.lines().filter(|l| !l.trim().is_empty()).take(3) {
                        eprintln!("\x1b[2m  {l}\x1b[0m");
                    }
                }
                "add" => {
                    if rest.is_empty() {
                        eprintln!("usage: /memory add <one line>");
                    } else if let Err(e) = crate::agent::memory::add_manual_line(rest) {
                        eprintln!("Error: {e}");
                    } else {
                        eprintln!("\x1b[2mnoted\x1b[0m");
                    }
                }
                "update" => {
                    eprintln!("\x1b[2mextracting memories from this session …\x1b[0m");
                    match crate::agent::memory::update(&session.model, &session.seed) {
                        Ok(added) if added.is_empty() => {
                            eprintln!("\x1b[2mnothing new worth remembering\x1b[0m")
                        }
                        Ok(added) => {
                            for a in &added {
                                eprintln!("\x1b[2m+ {a}\x1b[0m");
                            }
                        }
                        Err(e) => eprintln!("Error: {e}"),
                    }
                }
                "clean" => match crate::agent::memory::clean(&session.model) {
                    Ok(()) => eprintln!("\x1b[2mauto memories consolidated\x1b[0m"),
                    Err(e) => eprintln!("Error: {e}"),
                },
                "edit" => {
                    let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
                    if !path.exists() {
                        let _ = std::fs::write(&path, "# Global memory\n");
                    }
                    let editor = std::env::var("EDITOR")
                        .unwrap_or_else(|_| crate::platform::default_editor().to_string());
                    let _ = std::process::Command::new(editor).arg(&path).status();
                }
                other => eprintln!("unknown subcommand '{other}' (add, update, clean, edit)"),
            }
        }
        "/compact" => {
            // manual compaction: summarize everything older than the
            // keep-recent window, same as the automatic path
            let cfg = session.compact.clone();
            let estimate = crate::agent::compact::estimate_tokens(&session.seed, None);
            match crate::agent::compact::find_cut(&session.seed, cfg.keep_recent_tokens) {
                Some(cut) => {
                    eprintln!("  \x1b[2mcompacting …\x1b[0m");
                    match crate::agent::compact::summarize(&session.model, &session.seed[..cut]) {
                        Ok(summary) if !summary.is_empty() => {
                            let tail = session.seed.split_off(cut);
                            session.seed.clear();
                            session
                                .seed
                                .push(crate::providers::Msg::Summary { text: summary });
                            session.seed.extend(tail);
                            let now = crate::agent::compact::estimate_tokens(&session.seed, None);
                            eprintln!(
                                "  \x1b[2mcompacted {} → {}\x1b[0m",
                                crate::core::render::humanize_tokens(estimate),
                                crate::core::render::humanize_tokens(now)
                            );
                        }
                        _ => eprintln!("  \x1b[2mcompaction failed; history kept as-is\x1b[0m"),
                    }
                }
                None => eprintln!("  \x1b[2mnothing to compact yet\x1b[0m"),
            }
        }
        "/init" => {
            if let Err(e) = session.run_task(INIT_TASK, Vec::new()) {
                eprintln!("Error: {e}");
            }
        }
        "/edit" => {
            // compose in $EDITOR, submit as one task
            match editor_prompt() {
                Some(text) if !text.trim().is_empty() => {
                    run_task_logged(session, text.trim(), &mut Vec::new());
                }
                Some(_) => {}
                None => eprintln!("\x1b[2meditor closed without a prompt\x1b[0m"),
            }
        }
        "/exit" | "/quit" => return true,
        other => {
            // unknown /name falls back to the commands dir: the file's body
            // (plus any trailing words) becomes one agent task
            if let Some(name) = other.strip_prefix('/')
                && let Some(cmd) = crate::core::commands_md::find(name)
            {
                let input = arg.trim();
                let prompt = if input.is_empty() {
                    cmd.body.clone()
                } else {
                    crate::core::commands_md::expand(&cmd, input)
                };
                run_task_logged(session, &prompt, &mut Vec::new());
                return false;
            }
            if other.len() > 1 && other[1..].contains('/') {
                // a path-looking word ("/home/me/shot.jpg 看看?") is task
                // text, not a command typo
                run_task_logged(session, text, &mut Vec::new());
                return false;
            }
            eprintln!("unknown command {other} (try /help)")
        }
    }
    false
}
