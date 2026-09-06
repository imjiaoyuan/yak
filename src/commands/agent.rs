//! `llm agent` — interactive CLI agent (terminal-agent style) plus its one-shot
//! (`llm agent "task"`) and JSONL (`--mode json`) forms.

use std::io::IsTerminal;
use std::path::PathBuf;

use crate::agent::approval::{self, ApprovalConfig, Policy};
use crate::core::args::{OptSpec, ParsedArgs, render_help};
use crate::core::config;
use crate::core::db::Db;
use crate::core::logstore::{self};
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
    value_spec!("approval-mode", None, "ask or yolo", "MODE"),
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
    value_spec!(
        "mode",
        None,
        "text (default) or json (one JSON event per line)",
        "MODE"
    ),
    flag_spec!(
        "no-session",
        None,
        "Don't log the conversation to the database"
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
    value_spec!("database", Some('d'), "Path to log database", "PATH"),
    flag_spec!("no-stream", None, "Do not stream output"),
    value_spec!("key", None, "API key to use", "KEY"),
    flag_spec!("help", Some('h'), "Show this message and exit"),
];

fn help() -> String {
    render_help(
        "llm agent",
        "Run an agentic task with tools (bare invocation opens an interactive session)",
        SPECS,
        &[(
            "[PROMPT]",
            "Task to run; omit for interactive mode (reads stdin when piped)",
        )],
    )
}

/// Chat parses the same specs; the help row set is the conversational
/// subset (tools/approval/session flags do not apply).
fn chat_help() -> String {
    let subset: Vec<OptSpec> = SPECS
        .iter()
        .filter(|s| {
            matches!(
                s.long,
                "model"
                    | "option"
                    | "system-prompt"
                    | "attachment"
                    | "at"
                    | "continue"
                    | "fork"
                    | "session"
                    | "cid"
                    | "database"
                    | "no-stream"
                    | "thinking"
                    | "key"
                    | "help"
            )
        })
        .cloned()
        .collect();
    render_help(
        "llm chat",
        "Hold an ongoing conversation with a model (tool-less agent session)",
        &subset,
        &[(
            "[PROMPT]",
            "Optional first message; omit for interactive mode",
        )],
    )
}

pub fn run(argv: &[String]) -> i32 {
    run_mode(argv, false)
}

/// `llm chat`: the same machinery as `llm agent` in the tool-less
/// conversational preset (mode default from `models.chat`, no tools,
/// turns stamped mode "chat").
pub fn run_chat(argv: &[String]) -> i32 {
    run_mode(argv, true)
}

fn run_mode(argv: &[String], chat: bool) -> i32 {
    let (args, code) =
        crate::core::args::parse_with_help(argv, SPECS, || if chat { chat_help() } else { help() });
    let Some(args) = args else { return code };
    match execute_mode(&args, chat) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

fn execute_mode(args: &ParsedArgs, chat: bool) -> Result<i32, String> {
    let mut prompt = args.positionals.join(" ");
    // an `-a -` attachment claims stdin; otherwise piped stdin is the task
    prompt = crate::core::attachments::read_piped_prompt(args, prompt)?;

    let settings = crate::agent::settings::load();

    // attachments: -a path|URL|- and --at path mimetype ride the first task
    let attachments: Vec<crate::providers::Attachment> = crate::core::attachments::load_args(args)?
        .into_iter()
        .map(|l| l.request())
        .collect();

    // session continuation: -c = most recent, --session/--cid = given id
    let mut conversation_id: Option<String> = None;
    let mut seed: Vec<Msg> = Vec::new();
    let mut conv_system: Option<String> = None;
    let mut conv_model: Option<String> = None;
    let db = open_db(args)?;
    if let Some(raw) = args.opt(&["session", "cid"]) {
        let db = db
            .as_ref()
            .ok_or("--session requires a database (remove -n)?")?;
        let Some(cid) = logstore::resolve_conversation(db, raw) else {
            return Err(format!(
                "session id or prefix '{raw}' matches nothing (or is ambiguous)"
            ));
        };
        let (msgs, system) = crate::agent::session::rebuild_thread(db, &cid);
        seed = msgs;
        conv_system = system;
        conv_model = logstore::conversation_info(db, &cid).map(|(m, _)| m);
        conversation_id = Some(cid);
    } else if args.flag(&["continue"])
        && let Some(db) = db.as_ref()
        && let Some(cid) = logstore::latest_conversation_id(db)
    {
        let (msgs, system) = crate::agent::session::rebuild_thread(db, &cid);
        seed = msgs;
        conv_system = system;
        conv_model = logstore::conversation_info(db, &cid).map(|(m, _)| m);
        conversation_id = Some(cid);
    }

    // --fork: branch the loaded session onto a fresh thread id sharing the
    // same message-chain tip; the original keeps its own tip from here on
    if args.flag(&["fork"]) {
        let db = db
            .as_ref()
            .ok_or("--fork requires a database (remove -n)?")?;
        let source = conversation_id
            .take()
            .ok_or("--fork found no session to fork")?;
        let forked = logstore::fork_thread(db, &source)
            .ok_or_else(|| format!("cannot fork session {source}"))?;
        eprintln!(
            "\x1b[2mforked {} → {}\x1b[0m",
            &source[..source.len().min(10)],
            &forked[..forked.len().min(10)]
        );
        conversation_id = Some(forked);
    }

    // model resolution: -m > LLM_MODEL > session's model > the default
    let model = crate::providers::resolve_run_model(args, conv_model.clone())?;

    // approval config: CLI > [agent] settings > defaults
    let mode_str = if args.flag(&["yolo"]) {
        "yolo".to_string()
    } else {
        args.opt(&["approval-mode"])
            .map(str::to_string)
            .or_else(|| settings.approval_mode.clone())
            .unwrap_or_else(|| "always-ask".to_string())
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

    let json_mode = match args.opt(&["mode"]) {
        Some("json") => true,
        Some("text") | None => false,
        Some(m) => return Err(format!("invalid --mode '{m}' (text or json)")),
    };

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

    if chat && args.opt(&["tools"]).is_some() {
        return Err("chat has no tools (run `llm agent` for the tool session)".to_string());
    }
    if chat && args.opt(&["append-system-prompt"]).is_some() {
        return Err(
            "chat does not accept --append-system-prompt (run `llm agent` for an appended system prompt)"
                .to_string(),
        );
    }
    // plugin tools: script tools from the config `tools` table and MCP
    // servers from `mcpServers`. Connecting spawns every configured
    // server, so a --tools subset with no mcp__ names skips it entirely
    // (sub-agents pass exactly such subsets); a failed server warns and
    // mounts nothing, never aborting the session
    let script_specs = if chat {
        Vec::new()
    } else {
        crate::agent::script_tool::load()
    };
    let wanted: Option<Vec<&str>> = args.opt(&["tools"]).map(|csv| {
        csv.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    });
    let need_mcp = !chat
        && wanted
            .as_ref()
            .is_none_or(|w| w.iter().any(|n| n.starts_with("mcp__")));
    let mcp = if need_mcp {
        let registry = crate::agent::mcp::McpRegistry::connect(&crate::agent::mcp::load(), &cwd);
        for row in registry.rows() {
            if !row.ready {
                eprintln!(
                    "\x1b[2mmcp server '{}' failed: {} (its tools are not mounted)\x1b[0m",
                    row.name, row.reason
                );
            }
        }
        std::sync::Arc::new(registry)
    } else {
        std::sync::Arc::new(crate::agent::mcp::McpRegistry::empty())
    };
    let (system, agents, skills) = if chat {
        (
            crate::agent::system_prompt::chat_system_prompt(
                conv_system,
                args.opt(&["system-prompt"]),
            ),
            Vec::new(),
            Vec::new(),
        )
    } else {
        let agents = crate::agent::task::discover(&crate::core::config::user_dir(), &cwd);
        let skills = crate::agent::skills::discover(
            &crate::core::config::user_dir(),
            &cwd,
            &settings.disabled_skills,
        );
        (
            crate::agent::system_prompt::build_system_prompt(
                &cwd,
                args.opt(&["system-prompt"]),
                args.opt(&["append-system-prompt"]),
                conv_system.as_deref(),
                &agents,
                &skills,
            ),
            agents,
            skills,
        )
    };

    let mut session = crate::agent::session::Session {
        compact: settings.compact_config(&model.qualified_id(), &model.model_id),
        model,
        tools: Vec::new(),
        chat_mode: chat,
        system,
        cwd,
        max_turns,
        stream: !args.flag(&["no-stream"]),
        json_mode,
        no_session: args.flag(&["no-session"]),
        db,
        approval: approval_cfg,
        conversation_id,
        seed,
        thinking,
        steer_queue: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        script_tools: script_specs,
        mcp,
        tokens: (0, 0),
        tokens_cached: 0,
        checkpoints: None,
    };

    // built-ins plus plugin tools, all through the shared rebuild path;
    // --tools filters the combined registry by name
    session.rebuild_tools(&settings.roles);
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
    // a task stays an error; --mode json always requires a task
    if prompt.trim().is_empty() {
        if std::io::stdin().is_terminal() && !json_mode {
            return crate::agent::repl::repl(session, agents, skills, attachments);
        }
        eprintln!(
            "Error: no task provided (pass an argument, pipe stdin, or run bare for interactive mode)"
        );
        return Ok(2);
    }

    let (outcome, _reasoning) = session.run_task(&prompt, attachments)?;
    if session.json_mode {
        crate::agent::emit_json(&serde_json::json!({
            "type": "done",
            "text": outcome.final_text,
            "usage": outcome.usage.map(|u| serde_json::json!([u.input, u.output, u.cached])),
        }));
    }
    if !session.json_mode
        && let Some(cid) = &session.conversation_id
    {
        eprintln!("\x1b[2mSession: {cid}\x1b[0m");
    }
    Ok(0)
}

/// Open the session DB unless --no-session; -d picks a custom path.
fn open_db(args: &ParsedArgs) -> Result<Option<Db>, String> {
    if args.flag(&["no-session"]) {
        return Ok(None);
    }
    Ok(Some(Db::open_from_arg(args.opt(&["database"]))?))
}
