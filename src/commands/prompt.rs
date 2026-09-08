//! `llm prompt` — the core one-shot command (also the default command).

use crate::core::args::{OptSpec, ParsedArgs, render_help};
use crate::core::config;
use crate::core::http::Event;
use crate::core::threads::{self, StoredMsg, StoredTurn};
use crate::providers::{PromptInput, ResolvedModel};
use crate::term::render::{Renderer, extract_fenced};
use crate::{flag_spec, multi_spec, value_spec};

const SPECS: &[OptSpec] = &[
    value_spec!("system", Some('s'), "System prompt to use", "TEXT"),
    value_spec!("model", Some('m'), "Model to use", "MODEL"),
    value_spec!("database", Some('d'), "Path to thread directory", "PATH"),
    multi_spec!(
        "attachment",
        Some('a'),
        "Attachment path or URL or -",
        "ATTACHMENT"
    ),
    crate::two_value_spec!("at", "Attachment with explicit mimetype", "PATH MIMETYPE"),
    multi_spec!(
        "option",
        Some('o'),
        "key/value options for the model",
        "KEY=VALUE"
    ),
    value_spec!(
        "param",
        Some('p'),
        "Parameters for a custom command's $variables",
        "KEY=VALUE"
    ),
    flag_spec!("no-stream", None, "Do not stream output"),
    flag_spec!("no-log", Some('n'), "Don't log to the thread store"),
    flag_spec!("log", None, "Log prompt and response to the thread store"),
    flag_spec!("hide-reasoning", Some('R'), "Hide reasoning output"),
    value_spec!(
        "thinking",
        None,
        "Reasoning effort: minimal, low, medium, high or xhigh",
        "LEVEL"
    ),
    flag_spec!(
        "continue",
        Some('c'),
        "Continue the most recent conversation"
    ),
    value_spec!(
        "conversation",
        None,
        "Continue the conversation with the given ID",
        "ID"
    ),
    value_spec!("cid", None, "(alias of --conversation)", "ID"),
    value_spec!("key", None, "API key to use", "KEY"),
    flag_spec!("extract", Some('x'), "Extract first fenced code block"),
    flag_spec!("extract-last", None, "Extract last fenced code block"),
    flag_spec!(
        "json",
        None,
        "Output the response as JSON, same format as llm logs --json"
    ),
    flag_spec!("help", Some('h'), "Show this message and exit"),
];

fn help() -> String {
    render_help(
        "llm prompt",
        "Execute a prompt",
        SPECS,
        &[("[PROMPT]", "Prompt to send to the model")],
    )
}

/// Parse argv with the prompt specs, handling parse errors and -h.
/// Returns (args, exit-code-to-use-when-args-is-None).
fn parse_prompt_args(argv: &[String]) -> (Option<ParsedArgs>, i32) {
    crate::core::args::parse_with_help(argv, SPECS, help)
}

pub fn run(argv: &[String]) -> i32 {
    let (args, code) = parse_prompt_args(argv);
    let Some(args) = args else { return code };
    match execute(&args, None) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

/// `llm <name> [args...]` where `<name>` resolved to a commands-dir file:
/// the file's body becomes the prompt template and the trailing args its
/// input; every prompt flag (-m, -p, -o, -a, ...) still applies.
pub fn run_command(cmd: &crate::core::commands_md::CommandMd, rest: &[String]) -> i32 {
    let (args, code) = parse_prompt_args(rest);
    let Some(args) = args else { return code };
    let input = args.positionals.join(" ");
    let template = crate::core::commands_md::template(cmd);
    match execute(&args, Some((&template, &input))) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

/// Attachment with provenance: feeds both the request and the log store
/// (loaded through `core::attachments`, shared with chat and agent).
use crate::core::attachments::Loaded as LoadedAttachment;

fn execute(
    args: &ParsedArgs,
    preset: Option<(&crate::core::templates::Template, &str)>,
) -> Result<i32, String> {
    let mut prompt_text = match preset {
        Some((_, input)) => input.to_string(),
        None => args.positionals.join(" "),
    };

    // stdin piped → joined with a space before the argument, like the
    // original; an `-a -` attachment claims stdin instead
    prompt_text = crate::core::attachments::read_piped_prompt(args, prompt_text)?;

    if prompt_text.trim().is_empty() && preset.is_none() {
        eprintln!("Error: no prompt provided (pass an argument or pipe stdin)");
        return Ok(2);
    }

    if args.flag(&["no-log"]) && args.flag(&["log"]) {
        return Err("--log and --no-log are mutually exclusive".to_string());
    }

    // a commands-dir preset applies as one template: $input substitution
    // plus -p parameters, exactly like the old -t path
    let params: std::collections::BTreeMap<String, String> =
        crate::core::text::parse_kv(&args.multi(&["param"]))?
            .into_iter()
            .collect();
    let mut template: Option<crate::core::templates::Template> = None;
    let mut template_system: Option<String> = None;
    let resolved = preset.map(|(t, _)| t.clone());
    if let Some(t) = resolved {
        let (tprompt, tsystem) = crate::core::templates::apply(&t, &prompt_text, &params)?;
        if let Some(p) = tprompt {
            prompt_text = p;
        }
        template_system = tsystem;
        template = Some(t);
    }

    // conversation continuation: -c = most recent, --cid/--conversation = given id
    let continue_id = if let Some(cid) = args.opt(&["conversation", "cid"]) {
        Some(cid.to_string())
    } else if args.flag(&["continue"]) {
        Some(String::new()) // resolved to latest below
    } else {
        None
    };

    // the store is opened early so continuation can read history;
    // writing is gated on the logs-on state further down
    let no_log = args.flag(&["no-log"]);
    let mut store_opt: Option<threads::Store> = None;
    if no_log && continue_id.is_some() {
        return Err("cannot continue a conversation when logging is disabled (-n)".to_string());
    }
    if !no_log {
        store_opt = Some(threads::Store::open_from_arg(args.opt(&["database"]))?);
    }

    // attachments: template-declared ones first (original
    // _merge_template_attachments), then -a path|URL|- and --at path mimetype
    let mut attachments: Vec<LoadedAttachment> = Vec::new();
    if let Some(t) = template.as_ref() {
        for reference in &t.attachments {
            attachments.push(crate::core::attachments::load(reference, None)?);
        }
        for (reference, mime) in &t.attachment_types {
            attachments.push(crate::core::attachments::load(reference, Some(mime))?);
        }
    }
    attachments.extend(crate::core::attachments::load_args(args)?);

    let mut history: Vec<crate::providers::Msg> = Vec::new();
    let mut conversation_id: Option<String> = None;
    let mut conv_system: Option<String> = None;
    let mut conv_model: Option<String> = None;
    if let Some(cid) = continue_id {
        // the early -n refusal above guarantees the store is open here
        let store = store_opt.as_ref().expect("store open for continuation");
        let cid = if cid.is_empty() {
            store
                .latest_thread()?
                .ok_or_else(|| "No conversations found".to_string())?
        } else {
            match store.resolve_thread(&cid)? {
                Some(full) => full,
                None => {
                    return Err(format!(
                        "conversation id or prefix '{cid}' matches nothing (or is ambiguous)"
                    ));
                }
            }
        };
        let turns = store.read_thread(&cid)?;
        conv_system = turns.iter().find_map(|t| t.system.clone());
        for t in &turns {
            history.push(crate::providers::Msg::user(t.prompt.clone()));
            if !t.response.is_empty() {
                history.push(crate::providers::Msg::assistant(t.response.clone()));
            }
        }
        conv_model = turns.last().map(|t| t.model.clone());
        conversation_id = Some(cid);
    }

    // resolve model: -m/LLM_MODEL > template.model > conversation's model > default
    let model = crate::providers::resolve_run_model(
        args,
        template
            .as_ref()
            .and_then(|t| t.model.clone())
            .or_else(|| conv_model.clone()),
    )?;

    // system: explicit -s > template system > conversation's first system
    let system = args
        .opt(&["system"])
        .map(|s| s.to_string())
        .or_else(|| template_system.clone())
        .or_else(|| conv_system.clone());
    let request_attachments: Vec<crate::providers::Attachment> =
        attachments.iter().map(|a| a.request()).collect();
    let reasoning_level = match args.opt(&["thinking"]) {
        None => None,
        Some(level) => crate::providers::parse_thinking_level(level)?,
    };
    let input = PromptInput {
        system: system.as_deref(),
        history: &history,
        prompt: &prompt_text,
        attachments: &request_attachments,
        tools: &[],
        reasoning: reasoning_level.as_deref(),
    };

    // extract flags force non-streaming, like the original
    let extract_mode = args.flag(&["extract"]) || args.flag(&["extract-last"]);
    let stream = !args.flag(&["no-stream"]) && !extract_mode && !args.flag(&["json"]);
    let hide_reasoning = args.flag(&["hide-reasoning"]);
    let quiet = args.flag(&["json"]) || extract_mode;
    let mut view = crate::term::render::TaskView::new(2, &model.qualified_id(), !quiet);
    {
        let r = view.renderer_mut();
        r.set_quiet(quiet);
        r.terminal_md(2);
    }
    view.set_show_trace(!hide_reasoning);

    let start = std::time::Instant::now();
    let result = model.stream(&input, stream, &mut |event: Event| match event {
        Event::Delta(t) => view.delta(&t),
        Event::ReasoningDelta(t) => view.reasoning_delta(&t),
        Event::Done { usage, .. } => view.done(usage),
        Event::ToolCallDelta { .. } => {}
    });
    // agent-style footer on success; cleanup only on error (quiet modes
    // parse stdout, so they get no footer)
    match &result {
        Ok(()) if !quiet => view.finish(start.elapsed().as_secs_f64()),
        _ => view.abort(),
    }
    let renderer = view.into_renderer();

    if args.flag(&["json"]) {
        let turn = log_turn(
            store_opt.as_ref(),
            &model,
            &prompt_text,
            &system,
            &renderer,
            start,
            conversation_id.as_deref(),
            &attachments,
            args.flag(&["log"]),
        )?;
        if let Some(turn) = turn {
            println!(
                "{}",
                crate::jsonfmt::dumps_indent(
                    &serde_json::to_value(&turn).unwrap_or(serde_json::json!({})),
                    2
                )
            );
        }
        return Ok(0);
    }

    let mut exit = 0;
    match result {
        Ok(()) => {
            if extract_mode {
                // original semantics: no fenced block → the full text
                let last = args.flag(&["extract-last"]);
                let block = extract_fenced(&renderer.output, last)
                    .unwrap_or_else(|| renderer.output.clone());
                print!("{block}");
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            exit = 1;
        }
    }

    log_turn(
        store_opt.as_ref(),
        &model,
        &prompt_text,
        &system,
        &renderer,
        start,
        conversation_id.as_deref(),
        &attachments,
        args.flag(&["log"]),
    )?;
    Ok(exit)
}

#[allow(clippy::too_many_arguments)]
fn log_turn(
    store: Option<&threads::Store>,
    model: &ResolvedModel,
    prompt: &str,
    system: &Option<String>,
    renderer: &Renderer,
    start: std::time::Instant,
    conversation_id: Option<&str>,
    attachments: &[LoadedAttachment],
    log_override: bool,
) -> Result<Option<StoredTurn>, String> {
    // gate on the logs-on state; --log overrides, -n never reaches here
    let Some(store) = store else { return Ok(None) };
    if !config::logs_on() && !log_override {
        return Ok(None);
    }
    if renderer.output.is_empty() {
        return Ok(None);
    }
    let turn = StoredTurn {
        id: crate::core::db::ulid(),
        ts: crate::core::db::now_turn_datetime(),
        mode: "prompt".to_string(),
        model: model.qualified_id(),
        cwd: None,
        system: system.clone(),
        prompt: prompt.to_string(),
        response: renderer.output.clone(),
        reasoning: if renderer.reasoning.is_empty() {
            None
        } else {
            Some(renderer.reasoning.clone())
        },
        usage: renderer.usage.map(|u| (u.input, u.output)),
        duration_ms: Some(start.elapsed().as_millis() as i64),
        options: model.options.clone(),
        tools: None,
        messages: vec![StoredMsg::User {
            text: prompt.to_string(),
            attachments: attachments.iter().map(|a| a.stored()).collect(),
        }],
    };
    let _ = store.append_turn(conversation_id, &turn)?;
    Ok(Some(turn))
}
