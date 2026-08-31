//! `llm prompt` — the core one-shot command (also the default command).

use std::io::{IsTerminal, Read};

use crate::core::args::{OptSpec, ParsedArgs, parse, render_help};
use crate::core::config;
use crate::core::db::Db;
use crate::core::http::Event;
use crate::core::logstore::{self, Message, Part};
use crate::core::render::{Renderer, extract_fenced};
use crate::providers::{PromptInput, ResolvedModel};
use crate::{flag_spec, multi_spec, value_spec};

const SPECS: &[OptSpec] = &[
    value_spec!("system", Some('s'), "System prompt to use", "TEXT"),
    value_spec!("model", Some('m'), "Model to use", "MODEL"),
    value_spec!("database", Some('d'), "Path to log database", "PATH"),
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
    flag_spec!("options", None, "Show options for the selected model"),
    value_spec!(
        "schema",
        None,
        "JSON schema, filepath or the compact DSL",
        "SCHEMA"
    ),
    value_spec!(
        "schema-multi",
        None,
        "JSON schema for multiple results",
        "SCHEMA"
    ),
    multi_spec!(
        "param",
        Some('p'),
        "Parameters for a custom command's $variables",
        "KEY=VALUE"
    ),
    flag_spec!("no-stream", None, "Do not stream output"),
    flag_spec!("no-log", Some('n'), "Don't log to database"),
    flag_spec!("log", None, "Log prompt and response to the database"),
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
    value_spec!(
        "out",
        None,
        "Write media (image/audio) output to this file",
        "PATH"
    ),
    value_spec!("voice", None, "Voice for TTS models", "VOICE"),
    value_spec!("size", None, "Image size for image models", "SIZE"),
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
    let args = match parse(argv, SPECS) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return (None, 2);
        }
    };
    if args.flag(&["help"]) {
        print!("{}", help());
        return (None, 0);
    }
    (Some(args), 0)
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
    let config = config::load();
    let mut prompt_text = match preset {
        Some((_, input)) => input.to_string(),
        None => args.first_positional().unwrap_or("").to_string(),
    };

    // stdin piped → joined with a space before the argument, like the
    // original; an `-a -` attachment claims stdin instead
    if !crate::core::attachments::wants_stdin(args) && !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| e.to_string())?;
        if !buf.is_empty() {
            if prompt_text.is_empty() {
                prompt_text = buf;
            } else {
                prompt_text = format!("{buf} {prompt_text}");
            }
        }
    }

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

    // the db is opened early so continuation can read history;
    // writing is gated on the logs-on state further down
    let no_log = args.flag(&["no-log"]);
    let mut db_opt: Option<Db> = None;
    let need_db = !no_log || continue_id.is_some();
    if need_db {
        let db = match args.opt(&["database"]) {
            Some(p) => Db::open_path(std::path::Path::new(p)).map_err(|e| e.to_string())?,
            None => Db::open().map_err(|e| e.to_string())?,
        };
        db_opt = Some(db);
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
        let db = db_opt.as_ref().ok_or_else(|| {
            "Error: cannot continue a conversation when logging is disabled (-n)".to_string()
        })?;
        let cid = if cid.is_empty() {
            logstore::latest_conversation_id(db)
                .ok_or_else(|| "No conversations found".to_string())?
        } else {
            match logstore::resolve_conversation(db, &cid) {
                Some(full) => full,
                None => {
                    return Err(format!(
                        "conversation id or prefix '{cid}' matches nothing (or is ambiguous)"
                    ));
                }
            }
        };
        let turns = logstore::conversation_history(db, &cid);
        conv_system = turns.iter().find_map(|(_, _, s, _)| s.clone());
        for (prompt, response, _, _) in turns {
            history.push(crate::providers::Msg::user(prompt));
            if !response.is_empty() {
                history.push(crate::providers::Msg::assistant(response));
            }
        }
        conv_model = logstore::conversation_info(db, &cid).map(|(m, _)| m);
        conversation_id = Some(cid);
    }

    // resolve model: -m/LLM_MODEL > template.model > conversation's model > default
    let model_query = args
        .opt(&["model"])
        .map(|s| s.to_string())
        .or_else(|| std::env::var("LLM_MODEL").ok())
        .or_else(|| template.as_ref().and_then(|t| t.model.clone()))
        .or_else(|| conv_model.clone())
        .or_else(config::get_default_model);
    let Some(query) = model_query else {
        return Err(
            "No default model configured. Run `llm models set prompt <model>` or use -m."
                .to_string(),
        );
    };
    let resolved = config.resolve_model(&query);
    let Some((name, provider, model_id)) = resolved else {
        return Err(format!(
            "Invalid model: {query}. Add it to {} or check spelling.",
            crate::core::config::config_path().display()
        ));
    };

    let api_key = args
        .opt(&["key"])
        .map(|s| s.to_string())
        .or_else(|| config.api_key(&name, provider));
    let mut model = ResolvedModel::from_config(&name, provider, &model_id, api_key);
    // template options first, then per-model defaults for keys not set, CLI -o last
    let qualified = model.qualified_id();
    let mut options: Vec<(String, String)> = template
        .as_ref()
        .map(|t| {
            t.options
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    let saved_options = config::load_model_options();
    for (k, v) in saved_options
        .get(&qualified)
        .or_else(|| saved_options.get(&model_id))
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
    {
        if !options.iter().any(|(existing, _)| *existing == k) {
            options.push((k, v));
        }
    }
    let cli_options = crate::core::text::parse_kv(&args.multi(&["option"]))?;
    for (k, _) in &cli_options {
        options.retain(|(existing, _)| existing != k);
    }
    for (k, v) in cli_options {
        options.push((k, v));
    }
    model.options = options;

    // --options: render the model's options block, then exit
    if args.flag(&["options"]) {
        crate::commands::models::render_model_with_options(&model);
        return Ok(0);
    }

    // --schema / --schema-multi resolution (CLI beats the template's schema)
    let schema = if let Some(schema_input) = args.opt(&["schema-multi"]) {
        let inner = crate::core::schemas::resolve_schema(schema_input)?;
        Some(crate::core::schemas::multi_schema(&inner))
    } else if let Some(schema_input) = args.opt(&["schema"]) {
        Some(crate::core::schemas::resolve_schema(schema_input)?)
    } else {
        template.as_ref().and_then(|t| t.schema_object.clone())
    };
    if let Some(schema) = &schema {
        if model.kind != "openai-compat" {
            return Err(format!("{} does not support schemas", model.qualified_id()));
        }
        model.schema = Some(schema.clone());
    }

    // system: explicit -s > template system > conversation's first system;
    // system: explicit -s > template system > conversation's first system
    let system = args
        .opt(&["system"])
        .map(|s| s.to_string())
        .or_else(|| template_system.clone())
        .or_else(|| conv_system.clone());
    let request_attachments: Vec<crate::providers::Attachment> =
        attachments.iter().map(|a| a.request()).collect();
    let reasoning_level: Option<String> = match args.opt(&["thinking"]) {
        None | Some("off") => None,
        Some(level) => {
            if crate::providers::is_valid_reasoning_level(level) {
                Some(level.to_string())
            } else {
                return Err(format!(
                    "invalid --thinking '{level}' (off, minimal, low, medium, high, xhigh)"
                ));
            }
        }
    };
    let input = PromptInput {
        system: system.as_deref(),
        history: &history,
        prompt: &prompt_text,
        attachments: &request_attachments,
        tools: &[],
        reasoning: reasoning_level.as_deref(),
    };

    // media generation: image / tts kinds write files (or stdout) via --out
    if let Some(out_path) = args.opt(&["out"]) {
        let blobs: Vec<Vec<u8>> = match model.kind.as_str() {
            "image" => match crate::providers::media::generate_image(
                &model,
                &prompt_text,
                args.opt(&["size"]),
            ) {
                Ok(images) => images,
                Err(e) => {
                    eprintln!("Error: {e}");
                    return Ok(1);
                }
            },
            "tts" => match crate::providers::media::generate_speech(
                &model,
                &prompt_text,
                args.opt(&["voice"]),
            ) {
                Ok(bytes) => vec![bytes],
                Err(e) => {
                    eprintln!("Error: {e}");
                    return Ok(1);
                }
            },
            other => {
                eprintln!("Error: --out is only for image/tts models (kind={other})");
                return Ok(1);
            }
        };
        // extension per output: sniffed for images, response_format for tts
        let exts: Vec<&str> = if model.kind == "image" {
            blobs
                .iter()
                .map(|b| crate::providers::media::image_ext(b))
                .collect()
        } else {
            vec![crate::providers::media::speech_ext(&model.options); blobs.len()]
        };
        let stem = if model.kind == "image" {
            "image"
        } else {
            "speech"
        };
        let targets = match crate::providers::media::plan_outputs(out_path, &exts, stem) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Error: {e}");
                return Ok(1);
            }
        };
        if targets.is_empty() {
            // `--out -`: the single output goes to stdout, raw bytes
            use std::io::Write;
            std::io::stdout()
                .write_all(&blobs[0])
                .map_err(|e| format!("write failed: {e}"))?;
            return Ok(0);
        }
        for (blob, path) in blobs.iter().zip(&targets) {
            std::fs::write(path, blob).map_err(|e| format!("write failed: {e}"))?;
            println!("Wrote {} bytes to {}", blob.len(), path.display());
        }
        return Ok(0);
    }

    // extract flags force non-streaming, like the original
    let user_extract = args.flag(&["extract"]) || args.flag(&["extract-last"]);
    let extract_mode = user_extract
        || template
            .as_ref()
            .is_some_and(|t| t.extract || t.extract_last);
    let stream = !args.flag(&["no-stream"]) && !extract_mode && !args.flag(&["json"]);
    let hide_reasoning = args.flag(&["hide-reasoning"]);
    let quiet = args.flag(&["json"]) || extract_mode;
    let mut view = crate::core::render::TaskView::new(2, &model.qualified_id(), !quiet);
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
        let turn_id = log_turn(
            db_opt.as_ref(),
            &model,
            &prompt_text,
            &system,
            &renderer,
            start,
            conversation_id.as_deref(),
            &attachments,
            schema.as_ref(),
            args.flag(&["log"]),
        )?;
        let turn_id = turn_id.unwrap_or_default();
        let rows = crate::core::logstore::rows_for_ids_json(db_opt.as_ref(), &[turn_id]);
        println!("{rows}");
        return Ok(0);
    }

    let mut exit = 0;
    match result {
        Ok(()) => {
            if extract_mode {
                // original semantics: no fenced block → the full text
                let last = args.flag(&["extract-last"])
                    || template.as_ref().is_some_and(|t| t.extract_last);
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
        db_opt.as_ref(),
        &model,
        &prompt_text,
        &system,
        &renderer,
        start,
        conversation_id.as_deref(),
        &attachments,
        schema.as_ref(),
        args.flag(&["log"]),
    )?;
    Ok(exit)
}

#[allow(clippy::too_many_arguments)]
fn log_turn(
    db: Option<&Db>,
    model: &ResolvedModel,
    prompt: &str,
    system: &Option<String>,
    renderer: &Renderer,
    start: std::time::Instant,
    conversation_id: Option<&str>,
    attachments: &[LoadedAttachment],
    schema: Option<&serde_json::Value>,
    log_override: bool,
) -> Result<Option<String>, String> {
    // gate on the logs-on marker; --log overrides, -n never reaches here
    let Some(db) = db else { return Ok(None) };
    if !config::logs_on() && !log_override {
        return Ok(None);
    }
    if renderer.output.is_empty() {
        return Ok(None);
    }
    let mut user_parts: Vec<Part> = vec![Part::Text(prompt.to_string())];
    for attachment in attachments {
        user_parts.push(Part::Attachment(attachment.stored()));
    }
    let input_messages = vec![Message {
        role: "user".into(),
        parts: user_parts,
    }];

    let id = logstore::log_completed_turn(
        db,
        &logstore::CompletedTurn {
            conversation_id,
            system: system.as_deref(),
            input_messages: &input_messages,
            reasoning: if renderer.reasoning.is_empty() {
                None
            } else {
                Some(renderer.reasoning.as_str())
            },
            response_text: &renderer.output,
            model: &model.qualified_id(),
            options: &{
                let mut o = model.options.clone();
                o.push(("mode".to_string(), "prompt".to_string()));
                o
            },
            schema,
            usage: renderer.usage,
            duration_ms: start.elapsed().as_millis() as i64,
        },
    );
    Ok(Some(id))
}
