//! `yak --rpc` — a long-lived headless session driven over JSONL on
//! stdin/stdout: the surface a phone bridge, an editor plugin or another
//! agent supervises this process through. Where `--json` is one task and a
//! one-way event stream, rpc mode is a bidirectional protocol:
//!
//! ```text
//! → {"id":"1","type":"prompt","message":"fix the test"}    one command per line
//! ← {"id":"1","type":"response","command":"prompt","success":true,"data":{...}}
//! ← {"type":"text","text":"..."}                            events, same shapes as --json
//! ← {"type":"approval_request","id":"a1",...}              an ask answered by
//! → {"id":"2","type":"approval_response","request":"a1","answer":"allow"}
//! ```
//!
//! Commands: `prompt` (with attachments), `steer` (a message the run picks
//! up between its current tool round and the next model request),
//! `follow_up` (queued for after the run settles), `clear_queue`, `abort`,
//! `get_state`, `set_model`, `compact`, `new_session`, `switch_session`,
//! `fork`, `name`, `get_tree`, `export_html`, `interrupt`
//! (ctrl-c). Every command may carry a string `id`; its response repeats
//! it, so a client can correlate under concurrency. Events carry no id —
//! they describe session activity, not command results.
//!
//! stdout is protocol records only (diagnostics to stderr, like `--json`);
//! stdin is commands until EOF, which ends the process cleanly.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use crate::agent::approval::{ApprovalConfig, Policy};
use crate::agent::blacklist;
use crate::agent::session::Session;
use crate::core::args::{OptSpec, ParsedArgs, render_help};
use crate::providers::{Attachment, Msg};
use crate::{flag_spec, multi_spec, value_spec};

const SPECS: &[OptSpec] = &[
    flag_spec!(
        "rpc",
        None,
        "Run as a long-lived JSONL control process (commands on stdin, events on stdout)"
    ),
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
    flag_spec!("no-stream", None, "Do not stream output"),
    value_spec!("key", None, "API key to use", "KEY"),
    flag_spec!("help", Some('h'), "Show this message and exit"),
];

fn help() -> String {
    render_help(
        "yak --rpc",
        "Long-lived JSONL control mode: commands on stdin, events and responses on stdout",
        SPECS,
        &[],
    )
}

pub fn run(argv: &[String]) -> i32 {
    // `--rpc` may sit anywhere in argv (main routes on it); the spec above
    // accepts it so parsing never rejects what dispatched us here
    let argv: Vec<String> = argv
        .iter()
        .filter(|a| *a != "--rpc" && *a != "agent")
        .cloned()
        .collect();
    let (args, code) = crate::core::args::parse_with_help(&argv, SPECS, help);
    let Some(args) = args else { return code };
    match rpc(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

/// One record on stdout, flushed: a supervising client reads live.
fn emit(value: &serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

/// A command's response envelope.
fn respond(id: Option<&str>, command: &str, data: serde_json::Value) {
    emit(&serde_json::json!({
        "id": id,
        "type": "response",
        "command": command,
        "success": true,
        "data": data,
    }));
}

fn respond_err(id: Option<&str>, command: &str, message: &str) {
    emit(&serde_json::json!({
        "id": id,
        "type": "response",
        "command": command,
        "success": false,
        "error": message,
    }));
}

/// An approval ask on the wire, answered by an `approval_response` command.
fn approval_json(request_id: &str, req: &crate::agent::ApprovalRequest) -> serde_json::Value {
    let mut v = serde_json::json!({
        "type": "approval_request",
        "id": request_id,
        "tool": req.tool,
        "preview": req.preview,
        "reason": req.reason,
    });
    if let Some(pattern) = req.pattern {
        v["pattern"] = serde_json::json!(pattern);
    }
    if let Some(diff) = req.diff {
        v["diff"] = serde_json::json!(diff);
    }
    v
}

/// The shared runtime state one rpc process holds: the session, the queued
/// messages, and the pending approval (an ask blocks its run's thread until
/// answered or the process dies).
struct Rpc {
    session: Session,
    /// steer/follow_up/prompt texts with their attachments, in arrival
    /// order; each entry runs as one task when the previous settles
    queue: Vec<QueuedMessage>,
    /// commands that arrived mid-run and can wait for the idle loop
    deferred: Vec<Command>,
}

struct QueuedMessage {
    text: String,
    attachments: Vec<Attachment>,
}

impl Rpc {
    /// `get_state`'s data: everything a supervisor pins its UI to.
    fn state(&self) -> serde_json::Value {
        let used = crate::agent::compact::estimate_tokens(
            &self.session.seed,
            self.session
                .last_usage
                .map(|usage| (self.session.seed.len(), usage)),
        );
        serde_json::json!({
            "model": self.session.model.as_ref().map(|m| m.qualified_id()),
            "thinking": self.session.thinking,
            "cwd": self.session.cwd.display().to_string(),
            "thread": self.session.conversation_id,
            "name": crate::agent::names::thread_name(self.session.conversation_id.as_deref()),
            "messages": self.session.seed.len(),
            "context_tokens": used,
            "queued": self.queue.len(),
            "usage": {
                "input": self.session.usage.input,
                "output": self.session.usage.output,
                "cached": self.session.usage.cached,
                "cached_write": self.session.usage.cached_write,
            },
            "tools": self.session.tools.iter().map(|t| t.name().to_string()).collect::<Vec<_>>(),
        })
    }
}

/// Build the Session exactly the way `commands::agent` does (the same flag
/// meanings, the same store/approval/extensions wiring), minus the terminal
/// UI assumptions: rpc mode is always headless.
fn open_session(args: &ParsedArgs) -> Result<Session, String> {
    let cwd: PathBuf = std::env::current_dir().map_err(|e| e.to_string())?;
    let cwd_str = cwd.display().to_string();
    crate::core::tmp::prune_user_tmp();
    let extensions_connecting = crate::agent::ext::Extensions::connect_async(&cwd)?;
    let settings = crate::agent::settings::load();

    let mut conversation_id: Option<String> = None;
    let mut seed: Vec<Msg> = Vec::new();
    let mut resumed = false;
    let mut conv_system: Option<String> = None;
    let mut conv_model: Option<String> = None;
    let mut conv_usage = crate::core::http::Usage::default();
    let store = if args.flag(&["no-session"]) {
        None
    } else {
        Some(crate::core::threads::Store::open()?)
    };
    if let Some(raw) = args.opt(&["session", "cid"]) {
        let store = store
            .as_ref()
            .ok_or("--session requires a store (remove --no-session)?")?;
        let Some(cid) = store.resolve_thread(raw, Some(&cwd_str))? else {
            return Err(format!(
                "session id or prefix '{raw}' matches nothing (or is ambiguous)"
            ));
        };
        let turns = store.read_thread(&cid)?;
        let rebuilt = crate::agent::session::rebuild_turns(&turns)?;
        seed = rebuilt.messages;
        conv_system = rebuilt.system;
        conv_usage = rebuilt.usage;
        conv_model = turns.last().map(|t| t.model.clone());
        conversation_id = Some(cid);
        resumed = true;
    } else if args.flag(&["continue"])
        && let Some(store) = store.as_ref()
    {
        let local = store.latest_thread(Some(&cwd_str)).ok().flatten();
        let from_here = local.is_some();
        if let Some(cid) = local.or_else(|| store.latest_thread(None).ok().flatten()) {
            let turns = store.read_thread(&cid)?;
            if !from_here {
                let from = turns
                    .last()
                    .and_then(|t| t.cwd.as_deref())
                    .unwrap_or("another directory");
                eprintln!("continuing {} from {from}", &cid[..cid.len().min(6)]);
            }
            let rebuilt = crate::agent::session::rebuild_turns(&turns)?;
            seed = rebuilt.messages;
            conv_system = rebuilt.system;
            conv_usage = rebuilt.usage;
            conv_model = turns.last().map(|t| t.model.clone());
            conversation_id = Some(cid);
            resumed = true;
        }
    }
    if args.flag(&["fork"]) {
        let store = store
            .as_ref()
            .ok_or("--fork requires a store (remove --no-session)?")?;
        let source = conversation_id
            .take()
            .ok_or("--fork found no session to fork")?;
        let forked = store
            .fork_thread(&source)?
            .ok_or_else(|| format!("cannot fork session {source}"))?;
        conversation_id = Some(forked);
    }

    let model = match crate::providers::resolve_run_model(args, conv_model.clone()) {
        Ok(m) => Some(m),
        Err(e) => {
            // rpc mode is unattended from the first byte: a missing model is
            // a hard error, not a REPL that opens /login
            return Err(e);
        }
    };

    let mut approval_cfg = ApprovalConfig::default();
    for (tool, policy) in &settings.tool_policies {
        match Policy::parse(policy) {
            Some(p) => {
                approval_cfg.tool_policies.insert(tool.clone(), p);
            }
            None => eprintln!("Warning: invalid policy '{policy}' for tool '{tool}' in config"),
        }
    }
    blacklist::Blacklist::ensure_default();
    approval_cfg.blacklist = blacklist::Blacklist::load(&cwd);
    let unattended_allow = match args.opt(&["approval"]) {
        None => false,
        Some("allow") => true,
        Some(mode) => return Err(format!("invalid --approval '{mode}' (want allow)")),
    };
    let max_request_bytes: usize = args
        .opt(&["max-request-bytes"])
        .map(|s| s.parse::<usize>())
        .transpose()
        .map_err(|e| format!("invalid --max-request-bytes: {e}"))?
        .or(settings.max_request_bytes)
        .unwrap_or(crate::core::http::MAX_REQUEST_BYTES);
    let thinking: Option<String> = match args.opt(&["thinking"]) {
        Some(level) => crate::providers::parse_thinking_level(level)?,
        None => {
            let level = settings.default_thinking.clone();
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
    let wanted: Option<Vec<&str>> = args.opt(&["tools"]).map(|csv| {
        csv.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    });
    let extensions = extensions_connecting.join();
    let system = crate::agent::system_prompt::build_system_prompt(
        &cwd,
        args.opt(&["system-prompt"]),
        args.opt(&["append-system-prompt"]),
        conv_system.as_deref(),
        &extensions.system_sections(),
    );
    let mut session = Session {
        max_request_bytes,
        compact: settings.compact_config(),
        cache_ttl: settings.cache_ttl(),
        unattended_allow,
        model,
        tools: Vec::new(),
        system,
        cwd,
        stream: !args.flag(&["no-stream"]),
        store,
        approval: approval_cfg,
        cache_key: conversation_id
            .clone()
            .unwrap_or_else(crate::core::db::ulid),
        conversation_id,
        branch_parent: None,
        seed,
        thinking,
        extensions,
        usage: conv_usage,
        last_usage: None,
        json: true,
        persist_error: None,
    };
    if resumed {
        session.prune_seed_to_fit();
    }
    session.rebuild_tools();
    if let Some(wanted) = &wanted {
        let available: Vec<String> = session.tools.iter().map(|t| t.name().to_string()).collect();
        let wanted_owned: Vec<String> = wanted.iter().map(|s| s.to_string()).collect();
        let keep = crate::agent::tools::select_tools(&available, &wanted_owned)?;
        session.tools.retain(|t| keep.iter().any(|k| k == t.name()));
    }
    Ok(session)
}

/// One command off the wire, parsed: what a client asked for.
enum Command {
    Prompt {
        id: Option<String>,
        text: String,
        attachments: Vec<Attachment>,
    },
    Steer {
        id: Option<String>,
        text: String,
        attachments: Vec<Attachment>,
    },
    FollowUp {
        id: Option<String>,
        text: String,
        attachments: Vec<Attachment>,
    },
    ClearQueue {
        id: Option<String>,
    },
    Abort {
        id: Option<String>,
    },
    Interrupt {
        id: Option<String>,
    },
    GetState {
        id: Option<String>,
    },
    SetModel {
        id: Option<String>,
        model: String,
    },
    Compact {
        id: Option<String>,
        focus: Option<String>,
    },
    NewSession {
        id: Option<String>,
    },
    SwitchSession {
        id: Option<String>,
        thread: String,
    },
    Fork {
        id: Option<String>,
    },
    Name {
        id: Option<String>,
        name: Option<String>,
    },
    GetTree {
        id: Option<String>,
    },
    ExportHtml {
        id: Option<String>,
        path: Option<String>,
    },
    ApprovalResponse {
        request: String,
        answer: String,
    },
    Quit,
}

/// Parse one stdin line into a command; Err carries the message back to
/// the client as a failed `parse` response.
fn parse_command(line: &str) -> Result<Command, String> {
    let v: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("cannot parse command: {e}"))?;
    let id = v.get("id").and_then(|i| i.as_str()).map(str::to_string);
    let kind = v
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or("a command needs a \"type\" field")?
        .to_string();
    let text_of = |v: &serde_json::Value| -> String {
        v.get("message")
            .or_else(|| v.get("text"))
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string()
    };
    // attachments by local path or URL, loaded now (fail the command when a
    // source is unreadable — a silent drop would bill the model for nothing)
    let attachments_of = |v: &serde_json::Value| -> Result<Vec<Attachment>, String> {
        let Some(list) = v.get("attachments").and_then(|a| a.as_array()) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for item in list {
            let reference = item
                .as_str()
                .or_else(|| item.get("path").and_then(|p| p.as_str()))
                .or_else(|| item.get("url").and_then(|u| u.as_str()))
                .ok_or("an attachment entry must be a path or url string")?;
            let loaded = crate::core::attachments::load(reference, None)?;
            out.push(loaded.request());
        }
        Ok(out)
    };
    match kind.as_str() {
        "prompt" => Ok(Command::Prompt {
            id,
            text: text_of(&v),
            attachments: attachments_of(&v)?,
        }),
        "steer" => Ok(Command::Steer {
            id,
            text: text_of(&v),
            attachments: attachments_of(&v)?,
        }),
        "follow_up" => Ok(Command::FollowUp {
            id,
            text: text_of(&v),
            attachments: attachments_of(&v)?,
        }),
        "clear_queue" => Ok(Command::ClearQueue { id }),
        "abort" => Ok(Command::Abort { id }),
        "interrupt" => Ok(Command::Interrupt { id }),
        "get_state" => Ok(Command::GetState { id }),
        "set_model" => {
            let model = v
                .get("model")
                .and_then(|m| m.as_str())
                .ok_or("set_model needs a \"model\" string")?
                .to_string();
            Ok(Command::SetModel { id, model })
        }
        "compact" => Ok(Command::Compact {
            id,
            focus: v
                .get("instructions")
                .or_else(|| v.get("focus"))
                .and_then(|f| f.as_str())
                .filter(|f| !f.trim().is_empty())
                .map(str::to_string),
        }),
        "new_session" => Ok(Command::NewSession { id }),
        "switch_session" => {
            let thread = v
                .get("thread")
                .or_else(|| v.get("session"))
                .and_then(|s| s.as_str())
                .ok_or("switch_session needs a \"thread\" id")?
                .to_string();
            Ok(Command::SwitchSession { id, thread })
        }
        "fork" => Ok(Command::Fork { id }),
        "name" => Ok(Command::Name {
            id,
            name: v.get("name").and_then(|n| n.as_str()).map(str::to_string),
        }),
        "get_tree" => Ok(Command::GetTree { id }),
        "export_html" => Ok(Command::ExportHtml {
            id,
            path: v.get("path").and_then(|p| p.as_str()).map(str::to_string),
        }),
        "approval_response" => {
            let request = v
                .get("request")
                .and_then(|r| r.as_str())
                .ok_or("approval_response needs the \"request\" id it answers")?
                .to_string();
            let answer = v
                .get("answer")
                .and_then(|a| a.as_str())
                .unwrap_or("deny")
                .to_string();
            Ok(Command::ApprovalResponse { request, answer })
        }
        "exit" | "quit" => Ok(Command::Quit),
        other => Err(format!("unknown command type '{other}'")),
    }
}

/// The pending approval: its wire id and the reply slot. The run's approval
/// callback parks here until an answer arrives (or the run is aborted).
struct PendingApproval {
    wire_id: String,
    reply: std::sync::mpsc::Sender<crate::agent::ApprovalResponse>,
}

/// What the reader thread feeds: one parsed command per line. Err carries
/// the (salvaged) id and the parse error for a failed `parse` response.
type CmdItem = Result<Command, (Option<String>, String)>;
type CmdRx = std::sync::mpsc::Receiver<CmdItem>;

/// Set by the reader thread when stdin closes: the one way an abandoned
/// client's approval wait can end without an answer.
static STDIN_EOF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn spawn_stdin_reader() -> CmdRx {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let mut buf = String::new();
        loop {
            buf.clear();
            match lock.read_line(&mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    let line = buf.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let item = match parse_command(line) {
                        Ok(cmd) => Ok(cmd),
                        Err(msg) => {
                            let id = serde_json::from_str::<serde_json::Value>(line)
                                .ok()
                                .and_then(|v| {
                                    v.get("id").and_then(|i| i.as_str()).map(str::to_string)
                                });
                            Err((id, msg))
                        }
                    };
                    if tx.send(item).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // stdin closed (EOF or a read error): an approval waiting on the
        // wire must not park its run forever
        STDIN_EOF.store(true, std::sync::atomic::Ordering::SeqCst);
        // dropping tx closes the channel: the main loop reads the
        // disconnect as client EOF and winds down
    });
    rx
}

/// Deliver one approval answer: the waiter exists only while its ask is
/// live, so a late or unknown id is a no-op (the client may have raced the
/// run's own end).
fn resolve_approval(pending: &mut Option<PendingApproval>, request: &str, answer: &str) {
    if pending.as_ref().is_some_and(|p| p.wire_id == request)
        && let Some(p) = pending.take()
    {
        let verdict = match answer {
            "allow" => crate::agent::ApprovalResponse::Allow,
            "allow_session" | "always" => crate::agent::ApprovalResponse::AllowSession,
            _ => crate::agent::ApprovalResponse::Deny,
        };
        let _ = p.reply.send(verdict);
    }
}

/// The rpc process: commands in, events and responses out, one session.
fn rpc(args: &ParsedArgs) -> Result<i32, String> {
    let session = open_session(args)?;
    let mut rpc = Rpc {
        session,
        queue: Vec::new(),
        deferred: Vec::new(),
    };
    let rx = spawn_stdin_reader();
    // the session event up front: the thread a client may address
    emit(&serde_json::json!({
        "type": "session",
        "thread": rpc.session.conversation_id,
        "cwd": rpc.session.cwd.display().to_string(),
    }));
    // ctrl-c on the host console is a plain interrupt, not a death
    crate::term::install_sigint_handler();
    let mut exit_code = 0;
    loop {
        // a queued message runs as the next task once no run is in flight
        if rpc.queue.is_empty() {
            match rx.recv() {
                Ok(item) => {
                    if !handle_idle_item(&mut rpc, item, &mut exit_code) {
                        break;
                    }
                }
                Err(_) => break, // client closed stdin: clean exit
            }
        } else {
            let next = rpc.queue.remove(0);
            run_one_task(&mut rpc, &rx, next);
            // commands that parked while the run was live now get answers
            let deferred = std::mem::take(&mut rpc.deferred);
            for item in deferred {
                if !handle_idle_item(&mut rpc, Ok(item), &mut exit_code) {
                    return Ok(exit_code);
                }
            }
        }
    }
    crate::term::restore_sigint_handler();
    Ok(exit_code)
}

/// Handle one command while idle (or one deferred from a run). Returns
/// false when the process should exit.
fn handle_idle_item(rpc: &mut Rpc, item: CmdItem, exit_code: &mut i32) -> bool {
    let cmd = match item {
        Ok(cmd) => cmd,
        Err((id, msg)) => {
            respond_err(id.as_deref(), "parse", &msg);
            return true;
        }
    };
    dispatch_idle(rpc, cmd, exit_code)
}

/// The idle command table itself, split from [`handle_idle_item`] so the
/// early-return parse arm above reads as what it is.
fn dispatch_idle(rpc: &mut Rpc, cmd: Command, exit_code: &mut i32) -> bool {
    match cmd {
        Command::Prompt {
            id,
            text,
            attachments,
        } => {
            if text.trim().is_empty() {
                respond_err(id.as_deref(), "prompt", "the message is empty");
                return true;
            }
            respond(
                id.as_deref(),
                "prompt",
                serde_json::json!({"disposition": "started"}),
            );
            rpc.queue.push(QueuedMessage { text, attachments });
            true
        }
        Command::Steer {
            id,
            text,
            attachments,
        } => {
            // nothing is running: a steer is just the next prompt
            if text.trim().is_empty() {
                respond_err(id.as_deref(), "steer", "the message is empty");
                return true;
            }
            respond(
                id.as_deref(),
                "steer",
                serde_json::json!({"disposition": "queued"}),
            );
            rpc.queue.push(QueuedMessage { text, attachments });
            true
        }
        Command::FollowUp {
            id,
            text,
            attachments,
        } => {
            if text.trim().is_empty() {
                respond_err(id.as_deref(), "follow_up", "the message is empty");
                return true;
            }
            respond(
                id.as_deref(),
                "follow_up",
                serde_json::json!({"disposition": "queued"}),
            );
            rpc.queue.push(QueuedMessage { text, attachments });
            true
        }
        Command::ClearQueue { id } => {
            let dropped: Vec<String> = rpc.queue.drain(..).map(|q| q.text).collect();
            respond(
                id.as_deref(),
                "clear_queue",
                serde_json::json!({"cleared": dropped}),
            );
            true
        }
        Command::Abort { id } | Command::Interrupt { id } => {
            // idle: nothing to interrupt, and a stale request must not kill
            // the *next* task's first round
            crate::core::http::clear_interrupt();
            respond(
                id.as_deref(),
                "interrupt",
                serde_json::json!({"running": false}),
            );
            true
        }
        Command::GetState { id } => {
            respond(id.as_deref(), "get_state", rpc.state());
            true
        }
        Command::SetModel { id, model } => {
            match rpc.session.switch_model(&model) {
                Ok(()) => respond(
                    id.as_deref(),
                    "set_model",
                    serde_json::json!({
                        "model": rpc.session.model.as_ref().map(|m| m.qualified_id())
                    }),
                ),
                Err(e) => respond_err(id.as_deref(), "set_model", &e),
            }
            true
        }
        Command::Compact { id, focus } => {
            // the manual fold runs synchronously here; its events (the
            // `compacting` notice) stream as they happen
            match rpc.session.compact_manual(focus.as_deref()) {
                Ok(()) => respond(id.as_deref(), "compact", serde_json::json!({"done": true})),
                Err(e) => respond_err(id.as_deref(), "compact", &e),
            }
            true
        }
        Command::NewSession { id } => {
            rpc.session.clear();
            respond(
                id.as_deref(),
                "new_session",
                serde_json::json!({"thread": serde_json::Value::Null}),
            );
            true
        }
        Command::SwitchSession { id, thread } => {
            match switch_session(&mut rpc.session, &thread) {
                Ok(()) => respond(
                    id.as_deref(),
                    "switch_session",
                    serde_json::json!({"thread": rpc.session.conversation_id}),
                ),
                Err(e) => respond_err(id.as_deref(), "switch_session", &e),
            }
            true
        }
        Command::Fork { id } => {
            let forked = rpc.session.store.as_ref().and_then(|store| {
                rpc.session
                    .conversation_id
                    .as_deref()
                    .and_then(|cid| store.fork_thread(cid).ok().flatten())
            });
            match forked {
                Some(forked) => {
                    // the fork shares turns so far; this session continues
                    // on the new id
                    rpc.session.conversation_id = Some(forked.clone());
                    rpc.session.branch_parent = None;
                    respond(id.as_deref(), "fork", serde_json::json!({"thread": forked}));
                }
                None => respond_err(
                    id.as_deref(),
                    "fork",
                    "no stored session to fork (run a prompt first)",
                ),
            }
            true
        }
        Command::Name { id, name } => {
            let cleared = name.as_deref().map(str::trim).unwrap_or("").is_empty();
            match rpc.session.conversation_id.as_deref() {
                Some(cid) => {
                    let result = crate::agent::names::set_thread_name(
                        cid,
                        if cleared { None } else { name.as_deref() },
                    );
                    match result {
                        Ok(()) => respond(
                            id.as_deref(),
                            "name",
                            serde_json::json!({"name": if cleared { None } else { name }}),
                        ),
                        Err(e) => respond_err(id.as_deref(), "name", &e),
                    }
                }
                None => respond_err(id.as_deref(), "name", "no session yet"),
            }
            true
        }
        Command::GetTree { id } => {
            match rpc.session.conversation_id.as_deref() {
                Some(cid) => match tree_json(cid) {
                    Ok(tree) => respond(id.as_deref(), "get_tree", tree),
                    Err(e) => respond_err(id.as_deref(), "get_tree", &e),
                },
                None => respond_err(id.as_deref(), "get_tree", "no session yet"),
            }
            true
        }
        Command::ExportHtml { id, path } => {
            match rpc.session.conversation_id.as_deref() {
                Some(cid) => {
                    match crate::core::export::export_html_thread(
                        cid,
                        &rpc.session.cwd,
                        path.as_deref(),
                    ) {
                        Ok(written) => respond(
                            id.as_deref(),
                            "export_html",
                            serde_json::json!({"path": written.display().to_string()}),
                        ),
                        Err(e) => respond_err(id.as_deref(), "export_html", &e),
                    }
                }
                None => respond_err(id.as_deref(), "export_html", "no session yet"),
            }
            true
        }
        // no ask is live; a stray answer changes nothing
        Command::ApprovalResponse { .. } => true,
        Command::Quit => {
            *exit_code = 0;
            false
        }
    }
}

/// Load another thread into the live session (the `switch_session`
/// command): the same rebuild a `/resume` does.
fn switch_session(session: &mut Session, raw: &str) -> Result<(), String> {
    let store = session
        .store
        .as_ref()
        .ok_or("this rpc process runs --no-session")?;
    let cwd = session.cwd.display().to_string();
    let Some(cid) = store.resolve_thread(raw, Some(&cwd))? else {
        return Err(format!(
            "session id or prefix '{raw}' matches nothing (or is ambiguous)"
        ));
    };
    let turns = store.read_thread(&cid)?;
    let rebuilt = crate::agent::session::rebuild_turns(&turns)?;
    session.seed = rebuilt.messages;
    session.system = rebuilt.system;
    session.usage = rebuilt.usage;
    session.conversation_id = Some(cid);
    session.branch_parent = None;
    session.prune_seed_to_fit();
    Ok(())
}

/// `get_tree`'s payload: every turn of the file with its depth and label,
/// plus which indices form the active branch.
fn tree_json(cid: &str) -> Result<serde_json::Value, String> {
    let store = crate::core::threads::Store::open()?;
    let turns = store.read_thread(cid)?;
    if turns.is_empty() {
        return Ok(serde_json::json!({"turns": [], "active": []}));
    }
    let active = crate::core::threads::chain_indices(&turns, turns.len() - 1)?;
    let on_chain: std::collections::HashSet<usize> = active.iter().copied().collect();
    // depth per turn, mirroring the /tree view's walk
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
                _ => 0,
            },
        };
    }
    let rows: Vec<serde_json::Value> = turns
        .iter()
        .enumerate()
        .map(|(i, t)| {
            serde_json::json!({
                "index": i,
                "id": t.id,
                "ts": t.ts,
                "depth": depths[i],
                "label": label_of(t),
                "active": on_chain.contains(&i),
            })
        })
        .collect();
    Ok(serde_json::json!({"turns": rows, "active": active}))
}

/// One row's label: the round's prompt, its actions, or its answer's head.
fn label_of(t: &crate::core::threads::StoredTurn) -> String {
    let clip = |s: &str| -> String { s.replace('\n', " ").chars().take(80).collect() };
    if !t.prompt.trim().is_empty() {
        return clip(&t.prompt);
    }
    if !t.response.trim().is_empty() {
        return clip(&t.response);
    }
    "--".to_string()
}

/// Run one queued message as a task, handling the commands that matter
/// mid-run (steer, interrupt, approval answers) from the wire as they
/// arrive. Everything else waits for the idle loop (`rpc.deferred`).
fn run_one_task(rpc: &mut Rpc, rx: &CmdRx, next: QueuedMessage) {
    // the ask state both closures share, pre-split from `rpc` so the two
    // closures borrow disjoint fields: the ask itself, its id sequence, and
    // the deferred/requeue sides land in a small struct the closures own
    // through the cell
    let live = std::cell::RefCell::new(LiveRun {
        steer_queue: Vec::new(),
        pending_approval: None,
        seq: 0,
        deferred: Vec::new(),
        prompts: Vec::new(),
    });
    // the wire callback: the same event shapes as --json (RoundEnd never
    // reaches it — persistence, not stream)
    let mut wire = |event: &serde_json::Value| emit(event);

    // the steer callback the loop polls between rounds: drain what arrived
    // since the last poll, hand over the oldest steer if one is queued
    let mut on_steer = || -> Option<(String, Vec<Attachment>)> {
        drain_run_commands_shared(rx, &live);
        let mut l = live.borrow_mut();
        if l.steer_queue.is_empty() {
            None
        } else {
            let q = l.steer_queue.remove(0);
            Some((q.text, q.attachments))
        }
    };

    let unattended_allow = rpc.session.unattended_allow;
    let mut on_approval = |req: crate::agent::ApprovalRequest| -> crate::agent::ApprovalResponse {
        let (wire_id, answer_rx) = {
            let mut l = live.borrow_mut();
            l.seq += 1;
            let wire_id = format!("ap{}", l.seq);
            emit(&approval_json(&wire_id, &req));
            let (tx, answer_rx) = std::sync::mpsc::channel();
            l.pending_approval = Some(PendingApproval {
                wire_id: wire_id.clone(),
                reply: tx,
            });
            (wire_id, answer_rx)
        };
        let _ = &wire_id;
        // --approval allow answers unattended without asking the wire
        if unattended_allow && let Some(p) = live.borrow_mut().pending_approval.take() {
            let _ = p.reply.send(crate::agent::ApprovalResponse::Allow);
        }
        loop {
            // drain while waiting: interrupts and stray steers keep flowing
            drain_run_commands_shared(rx, &live);
            match answer_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(verdict) => return verdict,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if crate::core::http::interrupted() {
                        return crate::agent::ApprovalResponse::Deny;
                    }
                    // stdin closed while an ask is live: fail closed, the
                    // same way a terminal prompt without a terminal does
                    if stdin_closed() {
                        return crate::agent::ApprovalResponse::Deny;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return crate::agent::ApprovalResponse::Deny;
                }
            }
        }
    };

    let QueuedMessage { text, attachments } = next;
    let result = rpc.session.run_task_streamed_pub(
        &text,
        attachments,
        &mut wire,
        &mut on_approval,
        &mut on_steer,
    );
    if let Err(e) = result {
        emit(&serde_json::json!({
            "type": "error",
            "thread": rpc.session.conversation_id,
            "message": e,
        }));
    }
    // anything that arrived while the run was live waits for the idle loop
    drain_run_commands_shared(rx, &live);
    let l = live.into_inner();
    for q in l.steer_queue {
        rpc.queue.push(q);
    }
    for q in l.prompts {
        rpc.queue.push(q);
    }
    rpc.deferred.extend(l.deferred);
    // an unresolved ask never made it back: drop the waiter
    crate::core::http::clear_interrupt();
    // the task settled: a client that asked for the thread id gets it
    if let Some(cid) = rpc.session.conversation_id.as_deref() {
        emit(&serde_json::json!({
            "type": "session",
            "thread": cid,
            "cwd": rpc.session.cwd.display().to_string(),
        }));
    }
}

/// The state a running task and the wire share, disjoint from the session
/// borrow the task itself holds.
struct LiveRun {
    steer_queue: Vec<QueuedMessage>,
    pending_approval: Option<PendingApproval>,
    seq: u64,
    deferred: Vec<Command>,
    prompts: Vec<QueuedMessage>,
}

/// The shared drain: steer and interrupt act at once, approval answers
/// resolve their waiter, the rest parks for the idle loop (prompts join
/// the queue). Called from the steer poll and the approval wait alike.
fn drain_run_commands_shared(rx: &CmdRx, live: &std::cell::RefCell<LiveRun>) {
    while let Ok(item) = rx.try_recv() {
        let mut l = live.borrow_mut();
        match item {
            Ok(Command::Steer {
                id,
                text,
                attachments,
            }) => {
                // the ack is immediate even mid-run: the message is queued
                // for the next round boundary, and the client need not wait
                // for the run to settle to know it landed
                if text.trim().is_empty() {
                    respond_err(id.as_deref(), "steer", "the message is empty");
                } else {
                    respond(
                        id.as_deref(),
                        "steer",
                        serde_json::json!({"disposition": "queued"}),
                    );
                    l.steer_queue.push(QueuedMessage { text, attachments });
                }
            }
            Ok(Command::Prompt {
                id,
                text,
                attachments,
            })
            | Ok(Command::FollowUp {
                id,
                text,
                attachments,
            }) => {
                // a prompt sent mid-run behaves as a follow-up: the queue
                // keeps arrival order across both
                if text.trim().is_empty() {
                    respond_err(id.as_deref(), "follow_up", "the message is empty");
                } else {
                    respond(
                        id.as_deref(),
                        "follow_up",
                        serde_json::json!({"disposition": "queued"}),
                    );
                    l.prompts.push(QueuedMessage { text, attachments });
                }
            }
            Ok(Command::Interrupt { .. }) | Ok(Command::Abort { .. }) => {
                crate::core::http::request_interrupt();
            }
            Ok(Command::ApprovalResponse { request, answer }) => {
                resolve_approval(&mut l.pending_approval, &request, &answer);
            }
            Ok(other) => l.deferred.push(other),
            Err((id, msg)) => respond_err(id.as_deref(), "parse", &msg),
        }
    }
}

/// Whether stdin has closed (the reader thread ended): polled only while an
/// approval waits, so an abandoned client cannot leave a run parked forever.
fn stdin_closed() -> bool {
    STDIN_EOF.load(std::sync::atomic::Ordering::SeqCst)
}
