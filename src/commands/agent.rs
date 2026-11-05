//! `yak agent` — the interactive CLI session plus its one-shot
//! (`yak agent "task"`) form.

use crate::core::args::{OptSpec, ParsedArgs, render_help};
use crate::{flag_spec, multi_spec, value_spec};
use std::io::IsTerminal;

const SPECS: &[OptSpec] = &[
    value_spec!("model", Some('m'), "Model to use", "MODEL"),
    multi_spec!(
        "option",
        Some('o'),
        "key/value options for the model",
        "KEY=VALUE"
    ),
    value_spec!(
        "system-prompt",
        Some('s'),
        "Replace the built-in system prompt",
        "TEXT"
    ),
    value_spec!(
        "append-system-prompt",
        None,
        "Append to the system prompt",
        "TEXT"
    ),
    value_spec!(
        "tools",
        None,
        "Tool subset: names or * patterns replace the default set; +name/-name entries add or remove",
        "NAMES"
    ),
    value_spec!(
        "thinking",
        None,
        "Reasoning effort: off, minimal, low, medium, high or xhigh",
        "LEVEL"
    ),
    value_spec!(
        "max-request-bytes",
        None,
        "Largest request body to send, in bytes (32MB by default)",
        "N"
    ),
    value_spec!(
        "approval",
        None,
        "Unattended approval answer: allow (hardcoded refusals still deny)",
        "MODE"
    ),
    flag_spec!(
        "no-session",
        None,
        "Don't log the conversation to the thread store"
    ),
    flag_spec!("continue", Some('c'), "Continue the most recent session"),
    value_spec!(
        "session",
        None,
        "Continue the session with the given ID",
        "ID"
    ),
    value_spec!("cid", None, "(alias of --session)", "ID"),
    flag_spec!(
        "fork",
        None,
        "Continue a session on a new branch (original untouched; combine with --session)"
    ),
    multi_spec!(
        "attachment",
        Some('a'),
        "Attachment path or URL or -",
        "ATTACHMENT"
    ),
    flag_spec!("resume", Some('r'), "Browse past sessions and continue one"),
    flag_spec!("no-stream", None, "Do not stream output"),
    flag_spec!(
        "json",
        None,
        "Write a JSON event stream (one object per line) instead of the terminal UI"
    ),
    value_spec!("key", None, "API key to use", "KEY"),
    flag_spec!("help", Some('h'), "Show this message and exit"),
];

fn help() -> String {
    render_help(
        "yak",
        "Run an agentic task with tools (bare invocation opens an interactive session)",
        SPECS,
        &[(
            "[PROMPT]",
            "Task to run; omit for interactive mode (reads stdin when piped)",
        )],
    )
}

pub fn run(argv: &[String]) -> i32 {
    let (args, code) = crate::core::args::parse_with_help(argv, SPECS, help);
    let Some(args) = args else { return code };
    match execute_mode(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

fn execute_mode(args: &ParsedArgs) -> Result<i32, String> {
    if args.flag(&["resume"]) {
        return Ok(crate::commands::threads::browse());
    }
    let mut prompt = args.positionals.join(" ");
    // an `-a -` attachment claims stdin; otherwise piped stdin is the task
    prompt = crate::core::attachments::read_piped_prompt(args, prompt)?;
    // checked before anything else so the usage error never hides behind a
    // missing model or key
    if args.flag(&["json"]) && prompt.trim().is_empty() {
        return Err(
            "--json replaces the terminal UI, so it needs a task (pass one as an argument)"
                .to_string(),
        );
    }

    let mode = if args.flag(&["json"]) {
        super::session::Mode::Json
    } else if prompt.trim().is_empty() && std::io::stdin().is_terminal() {
        super::session::Mode::Interactive
    } else {
        super::session::Mode::OneShot
    };
    let super::session::OpenedSession {
        mut session,
        attachments,
        settings,
    } = super::session::open(args, mode)?;

    // bare invocation on a terminal → interactive REPL; piped stdin without
    // a task stays an error
    if prompt.trim().is_empty() {
        if std::io::stdin().is_terminal() {
            return crate::agent::repl::repl(session, attachments, settings);
        }
        eprintln!(
            "Error: no task provided (pass an argument, pipe stdin, or run bare for interactive mode)"
        );
        return Ok(2);
    }

    let (_outcome, _reasoning) = session.run_task(&prompt, attachments)?;
    if let Some(cid) = &session.conversation_id {
        eprintln!(
            "{}Session: {cid}{}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
    }
    Ok(0)
}
