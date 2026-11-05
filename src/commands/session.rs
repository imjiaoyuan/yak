//! Shared startup for terminal, one-shot and RPC sessions.

use std::path::PathBuf;

use crate::agent::approval::{ApprovalConfig, Policy};
use crate::agent::blacklist;
use crate::agent::session::Session;
use crate::agent::settings::AgentSettings;
use crate::core::args::ParsedArgs;
use crate::core::threads;
use crate::providers::{Attachment, Msg};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Interactive,
    OneShot,
    Json,
    Rpc,
}

pub(super) struct OpenedSession {
    pub session: Session,
    pub attachments: Vec<Attachment>,
    pub settings: AgentSettings,
}

pub(super) fn open(args: &ParsedArgs, mode: Mode) -> Result<OpenedSession, String> {
    let cwd: PathBuf = std::env::current_dir().map_err(|e| e.to_string())?;
    let cwd_str = cwd.display().to_string();
    // housekeeping: the editor's scratch dir (pasted images, ctrl+g buffers)
    // has no other reaper, so every agent start sweeps it
    crate::core::tmp::prune_user_tmp();
    // extensions start connecting now, on a background thread: discovery is
    // a few readdirs (synchronous), but the spawn+handshake is slow — the
    // attachments, thread store and system prompt below load while the
    // children boot. The join settles the handshake before the tool registry
    let extensions_connecting = crate::agent::ext::Extensions::connect_async(&cwd)?;

    let root = crate::core::config::read_root_or_die();
    let settings = crate::agent::settings::from_value(&root);
    settings.validate()?;
    let config = crate::core::config::from_root(&root);

    // attachments: -a path|URL|- ride the first task
    let loaded = if mode == Mode::Rpc {
        Vec::new()
    } else {
        crate::core::attachments::load_args(args)?
    };
    let attachments: Vec<crate::providers::Attachment> =
        loaded.into_iter().map(|l| l.request()).collect();

    // session continuation: -c = most recent (this directory's, else the
    // newest anywhere), --session/--cid = given id
    let mut conversation_id: Option<String> = None;
    let mut seed: Vec<Msg> = Vec::new();
    // a continued thread is projected down once its Session exists (the
    // window comes from the resolved model's settings)
    let mut resumed = false;
    let mut conv_system: Option<String> = None;
    let mut conv_model: Option<String> = None;
    // what the continued thread already spent, so the session's usage totals
    // (and `/status`) describe the conversation, not this process
    let mut conv_usage = crate::core::http::Usage::default();
    let store = if args.flag(&["no-session"]) {
        None
    } else {
        Some(threads::Store::open()?)
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
        // only the active branch rebuilds: a thread that jumped branches
        // carries its abandoned siblings too, and resuming must land on
        // the branch the thread ended on
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
        // no history in this directory: the newest anywhere, said out loud so
        // a stray `-c` cannot silently continue another project's session
        if let Some(cid) = local.or_else(|| store.latest_thread(None).ok().flatten()) {
            let turns = store.read_thread(&cid)?;
            if !from_here {
                let p = crate::theme::err();
                let from = turns
                    .last()
                    .and_then(|t| t.cwd.as_deref())
                    .unwrap_or("another directory");
                eprintln!(
                    "{}continuing {} from {from}{}",
                    p.dim,
                    &cid[..cid.len().min(6)],
                    p.reset
                );
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

    // --fork: branch the loaded session onto a fresh thread id sharing the
    // same turns so far; the original keeps its own from here on
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
        if mode != Mode::Rpc {
            eprintln!(
                "{}forked {} → {}{}",
                crate::theme::err().dim,
                &source[..source.len().min(10)],
                &forked[..forked.len().min(10)],
                crate::theme::err().reset
            );
        }
        conversation_id = Some(forked);
    }

    // model resolution: -m > YAK_MODEL > session's model > the default. A
    // bare interactive start with none of those still opens the REPL: /login
    // and /model live there, and failing here would dead-end a fresh install
    // before it could configure a provider.
    let model = match crate::providers::resolve_run_model(args, conv_model.clone(), &config) {
        Ok(m) => Some(m),
        Err(e) => {
            let bare_interactive = mode == Mode::Interactive
                && args.opt(&["model"]).is_none()
                && std::env::var_os("YAK_MODEL").is_none()
                && conv_model.is_none();
            if !bare_interactive {
                return Err(e);
            }
            eprintln!(
                "{}no model configured — use /login to add a provider, then /model{}",
                crate::theme::err().dim,
                crate::theme::err().reset
            );
            None
        }
    };

    // approval config: per-tool policies from [agent] settings, plus the
    // command blacklist. There is no ask mode — calls run unless a policy or
    // the blacklist says otherwise.
    let mut approval_cfg = ApprovalConfig::default();
    for (tool, policy) in &settings.tool_policies {
        match Policy::parse(policy) {
            Some(p) => {
                approval_cfg.tool_policies.insert(tool.clone(), p);
            }
            None => eprintln!("Warning: invalid policy '{policy}' for tool '{tool}' in config"),
        }
    }
    // command blacklist: ship the default file on first start, then load
    // user + project homes (an empty file allows everything)
    blacklist::Blacklist::ensure_default();
    approval_cfg.blacklist = blacklist::Blacklist::load(&cwd);

    // --approval allow: the unattended answer for approval prompts (a
    // subagent child has no terminal to answer one). Only the ask layer is
    // answered — the hardcoded refusals in `forbidden_command` still deny,
    // whatever this flag says
    let unattended_allow = match args.opt(&["approval"]) {
        None => false,
        Some("allow") => true,
        Some(mode) => return Err(format!("invalid --approval '{mode}' (want allow)")),
    };

    // one request body's ceiling: a gateway in front of the model may refuse
    // far less than the provider documents, so the local pre-flight can be
    // tightened per run (the flag beats the config key)
    let max_request_bytes: usize = args
        .opt(&["max-request-bytes"])
        .map(|s| s.parse::<usize>())
        .transpose()
        .map_err(|e| format!("invalid --max-request-bytes: {e}"))?
        .or(settings.max_request_bytes)
        .unwrap_or(crate::core::http::MAX_REQUEST_BYTES);

    // heavy attachments are the one thing that reaches that ceiling: say so
    // now, while the fix is still in the user's hands
    if let Some(warning) = crate::providers::attachment_weight(&attachments, max_request_bytes) {
        eprintln!("Warning: {warning}");
    }

    // reasoning effort: CLI > the stored global; invalid values are a hard
    // error on the CLI and a warning from config
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

    // extensions: the background connect from the top joins here. A failed
    // extension warns and mounts nothing, never aborting the session. The
    // system prompt waits for the join: a plugin may contribute a section
    // (the skills plugin's <available_skills> block), and the prompt must
    // embed what actually mounted
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

    let mut session = crate::agent::session::Session {
        max_request_bytes,
        // the auto-compaction trigger (agent.compact_at_tokens, used only when
        // the window is unknown) and the tail each compaction keeps: a provider
        // that refuses a prompt still forces one
        compact: settings.compact_config(),
        // how long the provider holds this conversation's cache entries
        // (agent.cache_ttl); None leaves its own default in place
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
        json: matches!(mode, Mode::Json | Mode::Rpc),
        persist_error: None,
    };

    // a continued thread may already exceed its window: project it down
    // before the first request so the first turn does not have to
    if resumed {
        session.prune_seed_to_fit();
    }

    // built-ins plus plugin tools, all through the shared rebuild path;
    // --tools selects the subset: plain names/patterns replace the registry,
    // +name/-name entries edit it (the --tools semantics)
    session.rebuild_tools();
    if let Some(wanted) = &wanted {
        let available: Vec<String> = session.tools.iter().map(|t| t.name().to_string()).collect();
        let wanted_owned: Vec<String> = wanted.iter().map(|s| s.to_string()).collect();
        let keep = crate::agent::tools::select_tools(&available, &wanted_owned)?;
        session.tools.retain(|t| keep.iter().any(|k| k == t.name()));
    }

    Ok(OpenedSession {
        session,
        attachments,
        settings,
    })
}
