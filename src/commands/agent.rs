//! `llm agent` — interactive CLI agent (terminal-agent style) plus its one-shot
//! (`llm agent "task"`) form.

use std::io::IsTerminal;
use std::path::PathBuf;

use crate::agent::approval::{self, ApprovalConfig, Policy};
use crate::core::args::{OptSpec, ParsedArgs, render_help};
use crate::core::config;
use crate::core::threads;
use crate::providers::Msg;
use crate::{flag_spec, multi_spec, value_spec};

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
        "Comma-separated tool subset (default: all)",
        "NAMES"
    ),
    value_spec!("approval-mode", None, "ask or yolo (default: yolo)", "MODE"),
    value_spec!(
        "thinking",
        None,
        "Reasoning effort: off, minimal, low, medium, high or xhigh",
        "LEVEL"
    ),
    flag_spec!("yolo", None, "Alias for --approval-mode yolo"),
    value_spec!(
        "max-turns",
        None,
        "Maximum agent turns per task (default 50)",
        "N"
    ),
    flag_spec!(
        "no-session",
        None,
        "Don't log the conversation to the thread store"
    ),
    flag_spec!(
        "continue",
        Some('c'),
        "Continue the most recent agent session"
    ),
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
    crate::two_value_spec!("at", "Attachment with explicit mimetype", "PATH MIMETYPE"),
    flag_spec!("resume", Some('r'), "Browse past sessions and continue one"),
    flag_spec!("no-stream", None, "Do not stream output"),
    value_spec!("key", None, "API key to use", "KEY"),
    flag_spec!("help", Some('h'), "Show this message and exit"),
];

fn help() -> String {
    render_help(
        "llm",
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
        return Ok(crate::commands::logs::browse());
    }
    let mut prompt = args.positionals.join(" ");
    // an `-a -` attachment claims stdin; otherwise piped stdin is the task
    prompt = crate::core::attachments::read_piped_prompt(args, prompt)?;

    let settings = crate::agent::settings::load();

    // attachments: -a path|URL|- and --at path mimetype ride the first task
    let attachments: Vec<crate::providers::Attachment> = crate::core::attachments::load_args(args)?
        .into_iter()
        .map(|l| l.request())
        .collect();

    // session continuation: -c = most recent (this directory's, else the
    // newest anywhere), --session/--cid = given id
    let cwd: PathBuf = std::env::current_dir().map_err(|e| e.to_string())?;
    let cwd_str = cwd.display().to_string();
    let mut conversation_id: Option<String> = None;
    let mut seed: Vec<Msg> = Vec::new();
    let mut conv_system: Option<String> = None;
    let mut conv_model: Option<String> = None;
    let store = open_store(args)?;
    if let Some(raw) = args.opt(&["session", "cid"]) {
        let store = store
            .as_ref()
            .ok_or("--session requires a store (remove -n)?")?;
        let Some(cid) = store.resolve_thread(raw, Some(&cwd_str))? else {
            return Err(format!(
                "session id or prefix '{raw}' matches nothing (or is ambiguous)"
            ));
        };
        let (msgs, system) = crate::agent::session::rebuild_thread(store, &cid);
        seed = msgs;
        conv_system = system;
        conv_model = store
            .read_thread(&cid)
            .ok()
            .and_then(|turns| turns.last().map(|t| t.model.clone()));
        conversation_id = Some(cid);
    } else if args.flag(&["continue"])
        && let Some(store) = store.as_ref()
        && let Some(cid) = store
            .latest_thread(Some(&cwd_str))
            .ok()
            .flatten()
            .or_else(|| store.latest_thread(None).ok().flatten())
    {
        let (msgs, system) = crate::agent::session::rebuild_thread(store, &cid);
        seed = msgs;
        conv_system = system;
        conv_model = store
            .read_thread(&cid)
            .ok()
            .and_then(|turns| turns.last().map(|t| t.model.clone()));
        conversation_id = Some(cid);
    }

    // --fork: branch the loaded session onto a fresh thread id sharing the
    // same turns so far; the original keeps its own from here on
    if args.flag(&["fork"]) {
        let store = store
            .as_ref()
            .ok_or("--fork requires a store (remove -n)?")?;
        let source = conversation_id
            .take()
            .ok_or("--fork found no session to fork")?;
        let forked = store
            .fork_thread(&source)?
            .ok_or_else(|| format!("cannot fork session {source}"))?;
        eprintln!(
            "{}forked {} → {}{}",
            crate::theme::err().dim,
            &source[..source.len().min(10)],
            &forked[..forked.len().min(10)],
            crate::theme::err().reset
        );
        conversation_id = Some(forked);
    }

    // model resolution: -m > LLM_MODEL > session's model > the default
    let model = crate::providers::resolve_run_model(args, conv_model.clone())?;

    // approval config: CLI > [agent] settings > yolo default
    let mode_str = if args.flag(&["yolo"]) {
        "yolo".to_string()
    } else {
        args.opt(&["approval-mode"])
            .map(str::to_string)
            .or_else(|| settings.approval_mode.clone())
            .unwrap_or_else(|| "yolo".to_string())
    };
    let mode = approval::Mode::parse(&mode_str)
        .ok_or_else(|| format!("invalid --approval-mode '{mode_str}' (ask or yolo)"))?;
    let mut approval_cfg = ApprovalConfig {
        mode,
        ..Default::default()
    };
    for (tool, policy) in &settings.tool_policies {
        match Policy::parse(policy) {
            Some(p) => {
                approval_cfg.tool_policies.insert(tool.clone(), p);
            }
            None => eprintln!("Warning: invalid policy '{policy}' for tool '{tool}' in config"),
        }
    }

    let max_turns: usize = args
        .opt(&["max-turns"])
        .map(|s| s.parse::<usize>())
        .transpose()
        .map_err(|e| format!("invalid --max-turns: {e}"))?
        .unwrap_or(50);

    // reasoning effort: CLI > the stored global; invalid values are a hard
    // error on the CLI and a warning from config
    let thinking: Option<String> = match args.opt(&["thinking"]) {
        Some(level) => crate::providers::parse_thinking_level(level)?,
        None => {
            let level = config::default_thinking();
            match level.as_deref() {
                Some(level) if crate::providers::is_valid_reasoning_level(level) => {
                    Some(level.to_string())
                }
                Some(level) => {
                    eprintln!("Warning: invalid thinking '{level}' in config, ignored");
                    None
                }
                None => None,
            }
        }
    };

    let cwd: PathBuf = std::env::current_dir().map_err(|e| e.to_string())?;

    // extensions: user executables registering tools (and, later, commands
    // and event hooks). A failed extension warns and mounts nothing, never
    // aborting the session
    let wanted: Option<Vec<&str>> = args.opt(&["tools"]).map(|csv| {
        csv.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    });
    let extensions = crate::agent::ext::Extensions::connect(&cwd);
    let skills = crate::agent::skills::discover(
        &crate::core::config::user_dir(),
        &cwd,
        &settings.disabled_skills,
    );
    let system = crate::agent::system_prompt::build_system_prompt(
        &cwd,
        args.opt(&["system-prompt"]),
        args.opt(&["append-system-prompt"]),
        conv_system.as_deref(),
        &skills,
    );

    let mut session = crate::agent::session::Session {
        compact: settings.compact_config(&model.qualified_id(), &model.model_id),
        model,
        tools: Vec::new(),
        system,
        cwd,
        max_turns,
        stream: !args.flag(&["no-stream"]),
        no_session: args.flag(&["no-session"]),
        store,
        approval: approval_cfg,
        conversation_id,
        seed,
        thinking,
        steer_queue: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        extensions,
        tokens: (0, 0),
        tokens_cached: 0,
    };

    // built-ins plus plugin tools, all through the shared rebuild path;
    // --tools filters the combined registry by name
    session.rebuild_tools();
    if let Some(wanted) = &wanted {
        let available: Vec<String> = session.tools.iter().map(|t| t.name().to_string()).collect();
        for want in wanted {
            if !available.iter().any(|a| a == want) {
                return Err(format!(
                    "unknown tool '{want}' (available: {})",
                    available.join(", ")
                ));
            }
        }
        session.tools.retain(|t| wanted.contains(&t.name()));
        if session.tools.is_empty() {
            return Err("--tools selected nothing".to_string());
        }
    }

    // bare invocation on a terminal → interactive REPL; piped stdin without
    // a task stays an error
    if prompt.trim().is_empty() {
        if std::io::stdin().is_terminal() {
            return crate::agent::repl::repl(session, skills, attachments);
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

/// Open the thread store unless --no-session.
fn open_store(args: &ParsedArgs) -> Result<Option<threads::Store>, String> {
    if args.flag(&["no-session"]) {
        return Ok(None);
    }
    Ok(Some(threads::Store::open()?))
}
