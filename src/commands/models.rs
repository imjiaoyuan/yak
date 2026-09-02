//! `llm models` — the model configuration center: per-mode defaults (wizard
//! or direct args), provider keys, listing and per-model options. Every
//! subcommand is dual-form: bare on a terminal opens its interactive flow,
//! full arguments run directly.

use std::io::IsTerminal;

use crate::core::args::{OptSpec, parse, render_help, split_subcommand};
use crate::core::config::{self, Provider};
use crate::providers::ResolvedModel;
use crate::{flag_spec, multi_spec, value_spec};

const LIST_SPECS: &[OptSpec] = &[
    multi_spec!(
        "query",
        Some('q'),
        "Filter models matching these strings",
        "QUERY"
    ),
    flag_spec!("options", None, "Show options for each model, if available"),
    flag_spec!("json", None, "Output as JSON"),
    flag_spec!("help", Some('h'), "Show this message and exit"),
];

const SIMPLE_SPECS: &[OptSpec] = &[flag_spec!("help", Some('h'), "Show this message and exit")];

const SET_SPECS: &[OptSpec] = &[
    value_spec!(
        "thinking",
        None,
        "Reasoning effort: off, minimal, low, medium, high or xhigh",
        "LEVEL"
    ),
    flag_spec!("help", Some('h'), "Show this message and exit"),
];

const KEY_SPECS: &[OptSpec] = &[
    flag_spec!("set", None, "Set the key (hidden input, or piped stdin)"),
    flag_spec!("help", Some('h'), "Show this message and exit"),
];

/// The modes with per-mode defaults.
const ALL_MODES: &[&str] = &["prompt", "agent", "chat"];

/// Mode names as accepted on the command line.
fn canonical_mode(name: &str) -> Option<&'static str> {
    match name {
        "prompt" => Some("prompt"),
        "agent" => Some("agent"),
        "chat" => Some("chat"),
        _ => None,
    }
}

pub fn run(argv: &[String]) -> i32 {
    let Some(first) = argv.first() else {
        // bare `llm models`: the wizard on a terminal, the values otherwise
        return if std::io::stdin().is_terminal() {
            wizard()
        } else {
            get(&[])
        };
    };
    let rest: Vec<String> = argv[1..].to_vec();
    match first.as_str() {
        "list" => list(&rest),
        "get" => get(&rest),
        "set" => set(&rest),
        "unset" => unset(&rest),
        "key" | "keys" => key(&rest),
        "add" => add(&rest),
        "remove" | "rm" => remove(&rest),
        "options" => options(&rest),
        "--help" | "-h" | "help" => {
            print!(
                "{}",
                render_help(
                    "llm models [COMMAND] [ARGS]...",
                    "Manage models, per-mode defaults and provider keys\n\nCommands:\n  set       Configure a mode (bare: interactive wizard)\n  get       Show the current settings\n  unset     Clear a mode's default\n  key       Show or set a provider's API key\n  add       Add a provider (bare: the login wizard)\n  remove    Remove a provider\n  list      List available models\n  options   Per-model default options",
                    SIMPLE_SPECS,
                    &[],
                )
            );
            0
        }
        other => {
            eprintln!("Error: No such command 'models {other}'.");
            2
        }
    }
}

// interactive wizard — mode → provider → model → thinking depth

/// The interactive configurator behind bare `llm models` and `llm models set`.
fn wizard() -> i32 {
    let items: Vec<String> = ALL_MODES
        .iter()
        .map(|m| {
            let current = config::mode_default(m)
                .map(|(id, _)| id)
                .unwrap_or_else(|| "(unset)".to_string());
            format!("{m:<9} {current}")
        })
        .collect();
    let Some(i) = crate::term::lineedit::pick("mode:", &items, false) else {
        return 0;
    };
    let mode = ALL_MODES[i];
    let (current_model, current_thinking) =
        config::mode_default(mode).unwrap_or((String::new(), None));
    let Some(choice) = cascade_model_picker(&current_model, current_thinking.as_deref()) else {
        return 0;
    };
    if let Err(e) = config::try_set_mode_default_model(mode, &choice.model) {
        eprintln!("Error: failed to save: {e}");
        return 1;
    }
    if let Some(thinking) = choice.thinking
        && let Err(e) = config::try_set_mode_default_thinking(mode, thinking.as_deref())
    {
        eprintln!("Error: failed to save thinking: {e}");
        return 1;
    }
    eprintln!("\x1b[2m{mode} model: {}\x1b[0m", choice.model);
    0
}

/// One selection from the cascade picker: the qualified model id plus the
/// reasoning-depth outcome. `thinking` is Some(None) when "(model default)"
/// was picked, None when the depth step was cancelled (keep the current).
pub struct ModelChoice {
    pub model: String,
    pub thinking: Option<Option<String>>,
}

/// The provider → model → thinking cascade. Cancelling any step returns
/// None: nothing is switched, nothing is persisted.
pub fn cascade_model_picker(current: &str, current_thinking: Option<&str>) -> Option<ModelChoice> {
    let cfg = config::load();
    if cfg.providers.is_empty() {
        eprintln!("\x1b[2mno models available (run `llm models add`)\x1b[0m");
        return None;
    }
    let current_provider = current.split_once('/').map(|(p, _)| p);
    // step 1: the provider (skipped when only one is configured)
    let (pname, provider) = if cfg.providers.len() == 1 {
        let (pname, provider) = cfg.providers.iter().next().expect("len checked");
        (pname.clone(), provider)
    } else {
        let items: Vec<String> = cfg
            .providers
            .iter()
            .map(|(name, p)| {
                let marker = if current_provider == Some(name.as_str()) {
                    " ←"
                } else {
                    ""
                };
                format!("{name} · {} models{marker}", p.models.len())
            })
            .collect();
        let i = crate::term::lineedit::pick("provider:", &items, false)?;
        let (pname, provider) = cfg.providers.iter().nth(i).expect("picked in range");
        (pname.clone(), provider)
    };
    // step 2: the provider's configured models merged with its live list
    eprintln!("\x1b[2mfetching models from {pname} …\x1b[0m");
    let key = cfg.api_key(&pname, provider).unwrap_or_default();
    let live = crate::commands::login::try_fetch_models(&provider.kind, &provider.base_url, &key);
    let mut ids: Vec<String> = provider.models.clone();
    for mid in live {
        if !ids.contains(&mid) {
            ids.push(mid);
        }
    }
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        eprintln!("\x1b[2mno models available (run `llm models add`)\x1b[0m");
        return None;
    }
    let current_model = current.split_once('/').map(|(_, m)| m);
    let items: Vec<String> = ids
        .iter()
        .map(|mid| {
            let marker = if current_provider == Some(pname.as_str())
                && current_model == Some(mid.as_str())
            {
                " ←"
            } else {
                ""
            };
            format!("{mid}{marker}")
        })
        .collect();
    let i = crate::term::lineedit::pick("models:", &items, false)?;
    let mid = ids[i].clone();
    // a live-list model may not be configured yet: remember it
    if !provider.models.contains(&mid) {
        config::add_model(&pname, &mid);
    }
    // step 3: reasoning depth; a cancel keeps the current level
    let thinking = thinking_picker(current_thinking);
    Some(ModelChoice {
        model: format!("{pname}/{mid}"),
        thinking,
    })
}

/// The interactive effort-level picker.
/// Returns Some(None) when "(model default)" was chosen, None on cancel.
pub fn thinking_picker(current: Option<&str>) -> Option<Option<String>> {
    let mut items: Vec<String> = Vec::with_capacity(crate::providers::REASONING_LEVELS.len() + 1);
    items.push(format!(
        "(model default){}",
        if current.is_none() { " ←" } else { "" }
    ));
    for level in crate::providers::REASONING_LEVELS {
        items.push(format!(
            "{level}{}",
            if current == Some(*level) { " ←" } else { "" }
        ));
    }
    let i = crate::term::lineedit::pick("thinking:", &items, false)?;
    Some(if i == 0 {
        None
    } else {
        Some(items[i].trim_end_matches(" ←").to_string())
    })
}

// get / set / unset — the per-mode defaults

fn print_mode_line(mode: &str) {
    match config::mode_default(mode) {
        Some((m, thinking)) => match thinking {
            Some(t) => println!("{mode:<9} {m} (thinking: {t})"),
            None => println!("{mode:<9} {m}"),
        },
        None => println!("{mode:<9} (unset)"),
    }
}

fn get(argv: &[String]) -> i32 {
    let args = match parse(argv, SIMPLE_SPECS) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if args.flag(&["help"]) {
        print!(
            "{}",
            render_help(
                "llm models get [MODE]",
                "Show the current model settings",
                SIMPLE_SPECS,
                &[]
            )
        );
        return 0;
    }
    if let Some(name) = args.first_positional() {
        let Some(mode) = canonical_mode(name) else {
            eprintln!("Error: unknown mode '{name}' (prompt, agent, chat)");
            return 2;
        };
        print_mode_line(mode);
    } else {
        for mode in ALL_MODES {
            print_mode_line(mode);
        }
    }
    0
}

fn set(argv: &[String]) -> i32 {
    let args = match parse(argv, SET_SPECS) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if args.flag(&["help"]) {
        print!(
            "{}",
            render_help(
                "llm models set [MODE] MODEL",
                "Configure a mode's model (bare: interactive wizard; no MODE: the prompt default)",
                SET_SPECS,
                &[("MODE", "prompt (default), agent or chat")],
            )
        );
        return 0;
    }
    if args.positionals.is_empty() {
        if !std::io::stdin().is_terminal() {
            eprintln!("Error: Usage: llm models set [MODE] MODEL [--thinking LEVEL]");
            return 2;
        }
        return wizard();
    }
    // a leading mode word targets that mode; a bare model targets prompt
    let (mode, model_pos) = match canonical_mode(&args.positionals[0]) {
        Some(mode) => {
            if args.positionals.len() < 2 {
                eprintln!("Error: Usage: llm models set [MODE] MODEL [--thinking LEVEL]");
                return 2;
            }
            (mode, 1)
        }
        None => ("prompt", 0),
    };
    let model = args.positionals[model_pos].clone();
    let thinking: Option<Option<String>> = match args.opt(&["thinking"]) {
        Some("off") => Some(None),
        Some(level) => {
            if crate::providers::is_valid_reasoning_level(level) {
                Some(Some(level.to_string()))
            } else {
                eprintln!(
                    "Error: invalid --thinking '{level}' (off, minimal, low, medium, high, xhigh)"
                );
                return 2;
            }
        }
        None => None,
    };
    let cfg = config::load();
    let Some((n, _, m)) = cfg.resolve_model(&model) else {
        eprintln!("Error: Unknown model: {model}");
        return 1;
    };
    let qualified = format!("{n}/{m}");
    if let Err(e) = config::try_set_mode_default_model(mode, &qualified) {
        eprintln!("Error: failed to save: {e}");
        return 1;
    }
    if let Some(t) = &thinking
        && let Err(e) = config::try_set_mode_default_thinking(mode, t.as_deref())
    {
        eprintln!("Error: failed to save thinking: {e}");
        return 1;
    }
    let depth = thinking
        .map(|t| format!(" (thinking: {})", t.unwrap_or_else(|| "off".to_string())))
        .unwrap_or_default();
    eprintln!("{mode} model: {qualified}{depth}");
    0
}

fn unset(argv: &[String]) -> i32 {
    let args = match parse(argv, SIMPLE_SPECS) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if args.flag(&["help"]) {
        print!(
            "{}",
            render_help(
                "llm models unset MODE",
                "Clear a mode's default model",
                SIMPLE_SPECS,
                &[],
            )
        );
        return 0;
    }
    let mode = if let Some(name) = args.first_positional() {
        let Some(mode) = canonical_mode(name) else {
            eprintln!("Error: unknown mode '{name}' (prompt, agent, chat)");
            return 2;
        };
        mode.to_string()
    } else if std::io::stdin().is_terminal() {
        let items: Vec<String> = ALL_MODES.iter().map(|m| m.to_string()).collect();
        let Some(i) = crate::term::lineedit::pick("mode:", &items, false) else {
            return 0;
        };
        ALL_MODES[i].to_string()
    } else {
        eprintln!("Error: Usage: llm models unset MODE");
        return 2;
    };
    let result = config::unset_mode_default(&mode);
    match result {
        Ok(()) => {
            eprintln!("{mode} default cleared");
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

// key — provider API keys (absorbs the old `llm keys` command)

fn key(argv: &[String]) -> i32 {
    let args = match parse(argv, KEY_SPECS) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if args.flag(&["help"]) {
        print!(
            "{}",
            render_help(
                "llm models key [PROVIDER [VALUE]]",
                "Show or set a provider's API key",
                KEY_SPECS,
                &[],
            )
        );
        return 0;
    }
    let cfg = config::load();
    let Some(name) = args.first_positional().map(String::from) else {
        if cfg.providers.is_empty() {
            println!("No providers configured (llm models add)");
            return 0;
        }
        for (name, p) in &cfg.providers {
            let mark = if p.api_key.is_some() {
                "key set"
            } else {
                "no key"
            };
            println!("{name:<16} {mark}");
        }
        return 0;
    };
    if !cfg.providers.contains_key(&name) {
        eprintln!("Error: No provider found with name '{name}' (add one with llm models add)");
        return 1;
    }
    if args.flag(&["set"]) {
        // hidden input on a terminal; piped stdin otherwise
        let value = if std::io::stdin().is_terminal() {
            crate::term::read_hidden("Enter key: ").unwrap_or_default()
        } else {
            let mut buf = String::new();
            use std::io::Read;
            let _ = std::io::stdin().read_to_string(&mut buf);
            buf.trim().to_string()
        };
        if value.is_empty() {
            eprintln!("Error: No value provided");
            return 1;
        }
        return set_provider_key(&name, &value);
    }
    if let Some(value) = args.positionals.get(1) {
        return set_provider_key(&name, value);
    }
    // one argument, no flag: print the resolved (env-expanded) key
    let p = cfg.providers.get(&name).expect("checked above");
    match cfg.api_key(&name, p) {
        Some(k) => {
            println!("{k}");
            0
        }
        None => {
            eprintln!("Error: No key found for provider '{name}'");
            1
        }
    }
}

fn set_provider_key(name: &str, value: &str) -> i32 {
    let mut cfg = config::load();
    cfg.providers
        .get_mut(name)
        .expect("caller checked the provider")
        .api_key = Some(value.to_string());
    match config::save(&cfg) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

// add / remove — the provider lifecycle (login/logout are the interactive
// aliases)

fn add(argv: &[String]) -> i32 {
    let args = match parse(argv, SIMPLE_SPECS) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if args.flag(&["help"]) {
        print!(
            "{}",
            render_help(
                "llm models add [NAME [KEY]]",
                "Add a provider (bare: the interactive wizard)",
                SIMPLE_SPECS,
                &[],
            )
        );
        return 0;
    }
    if args.positionals.is_empty() {
        if !std::io::stdin().is_terminal() {
            eprintln!("Error: llm models add requires a terminal, or NAME and KEY");
            return 1;
        }
        return match crate::commands::login::wizard() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Error: {e}");
                1
            }
        };
    }
    let name = args.positionals[0].clone();
    let Some(entry) = crate::providers::catalog::by_id(&name) else {
        eprintln!(
            "Error: unknown provider '{name}' (not in the catalog; custom endpoints go through the interactive `llm models add`)"
        );
        return 1;
    };
    let api_key = args.positionals.get(1).cloned();
    let fetch_key = api_key.clone().unwrap_or_default();
    eprintln!("Fetching models from {} ...", entry.base_url);
    let models = crate::commands::login::try_fetch_models(entry.kind, entry.base_url, &fetch_key);
    if models.is_empty() {
        eprintln!("could not fetch models — provider saved without models (llm scan refreshes)");
    }
    let mut cfg = config::load();
    if cfg.providers.contains_key(&name) {
        eprintln!("Error: provider '{name}' already exists (llm models remove {name} first)");
        return 1;
    }
    cfg.providers.insert(
        name.clone(),
        Provider {
            kind: entry.kind.to_string(),
            base_url: entry.base_url.to_string(),
            api_key,
            models,
        },
    );
    match config::save(&cfg) {
        Ok(()) => {
            eprintln!(
                "Provider '{name}' written to {}",
                config::config_path().display()
            );
            // the first provider's first model becomes the default, so a
            // fresh install is ready to run without another command
            if config::mode_default("prompt").is_none()
                && let Some(first) = cfg
                    .providers
                    .get(&name)
                    .and_then(|p| p.models.first().cloned())
            {
                let qualified = format!("{name}/{first}");
                if config::set_default_model_all(&qualified).is_ok() {
                    eprintln!("\x1b[2mdefault model: {qualified}\x1b[0m");
                }
            }
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

fn remove(argv: &[String]) -> i32 {
    let args = match parse(argv, SIMPLE_SPECS) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if args.flag(&["help"]) {
        print!(
            "{}",
            render_help(
                "llm models remove [NAME]",
                "Remove a provider (bare: the picker)",
                SIMPLE_SPECS,
                &[],
            )
        );
        return 0;
    }
    if args.positionals.is_empty() {
        if !std::io::stdin().is_terminal() {
            eprintln!("Error: llm models remove requires a terminal, or NAME");
            return 1;
        }
        return match crate::commands::login::logout_picker() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Error: {e}");
                1
            }
        };
    }
    let name = &args.positionals[0];
    let mut cfg = config::load();
    if cfg.providers.remove(name).is_none() {
        eprintln!("Error: No provider found with name '{name}'");
        return 1;
    }
    match config::save(&cfg) {
        Ok(()) => {
            eprintln!(
                "removed provider '{name}' (and its key) from {}",
                config::config_path().display()
            );
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

// list / options — browsing and per-model defaults

fn list(argv: &[String]) -> i32 {
    let args = match parse(argv, LIST_SPECS) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if args.flag(&["help"]) {
        print!(
            "{}",
            render_help(
                "llm models list [OPTIONS]",
                "List available models",
                LIST_SPECS,
                &[]
            )
        );
        return 0;
    }
    let cfg = config::load();
    let aliases = config::load_aliases();
    let queries = args.multi(&["query"]);
    // one options block per kind: models of a kind share the same schema
    let mut shown_kinds = std::collections::HashSet::new();

    // the per-mode defaults lead: what runs where matters more than the
    // inventory, and only shows without filters
    if !args.flag(&["json"]) && queries.is_empty() {
        for mode in ALL_MODES {
            print_mode_line(mode);
        }
        println!();
    }

    for (provider, models) in cfg.all_models() {
        if models.is_empty() {
            // an imported provider with no listed models must stay visible
            if (queries.is_empty()
                || queries
                    .iter()
                    .all(|q| provider.to_lowercase().contains(&q.to_lowercase())))
                && std::io::stdout().is_terminal()
            {
                println!("\x1b[90m{provider}/ · no models listed · `llm scan` refreshes\x1b[0m");
            }
            continue;
        }
        for m in models {
            let qualified = format!("{provider}/{m}");
            let mut names = vec![qualified.clone(), m.clone(), provider.clone()];
            let mut model_aliases: Vec<String> = Vec::new();
            for (alias, target) in &aliases {
                if target == &qualified || target == &m {
                    model_aliases.push(alias.clone());
                    names.push(alias.clone());
                }
            }
            if !queries.iter().all(|q| {
                let q = q.to_lowercase();
                names.iter().any(|n| n.to_lowercase().contains(&q))
            }) {
                continue;
            }
            let kind = cfg
                .providers
                .get(&provider)
                .map(|p| p.kind.clone())
                .unwrap_or_default();
            if args.flag(&["json"]) {
                let mut obj = serde_json::json!({
                    "model_id": qualified,
                    "aliases": model_aliases,
                    "can_stream": true,
                    "supports_schema": true,
                    "supports_tools": false,
                    "attachment_types": [],
                });
                if args.flag(&["options"]) {
                    let mut props = serde_json::Map::new();
                    for (name, type_, description) in
                        crate::providers::option_schema_for_kind(&kind)
                    {
                        let mut field = serde_json::json!({ "type": type_ });
                        if let Some(d) = description {
                            field
                                .as_object_mut()
                                .expect("object literal")
                                .insert("description".into(), d.into());
                        }
                        props.insert(name.to_string(), field);
                    }
                    obj.as_object_mut()
                        .expect("object literal")
                        .insert("options".into(), serde_json::Value::Object(props));
                }
                println!("{}", crate::jsonfmt::dumps_indent(&obj, 2));
            } else {
                let alias_suffix = if model_aliases.is_empty() {
                    String::new()
                } else {
                    format!(" (aliases: {})", model_aliases.join(", "))
                };
                println!("{qualified}{alias_suffix}");
                if args.flag(&["options"]) && shown_kinds.insert(kind.clone()) {
                    print_kind_options(&kind);
                }
            }
        }
    }
    0
}

/// Options + Features block for one model kind — shared by `llm prompt
/// --options` and `llm models list --options`. Option schemas come from a
/// per-kind static table (our stand-in for the original's per-model pydantic
/// Options).
pub fn print_kind_options(kind: &str) {
    let options = crate::providers::option_schema_for_kind(kind);
    if !options.is_empty() {
        println!("  Options:");
        for (name, type_, description) in options {
            println!("    {name}: {type_}, null");
            if let Some(description) = description {
                for line in wrap(description, 70) {
                    println!("      {line}");
                }
            }
        }
    }
    let features = crate::providers::features_for_kind(kind);
    if !features.is_empty() {
        println!("  Features:");
        for feature in features {
            println!("  - {feature}");
        }
    }
}

/// Render the model plus its options block — used by `llm prompt --options`.
pub fn render_model_with_options(model: &ResolvedModel) {
    println!("Model: {}", model.qualified_id());
    print_kind_options(&model.kind);
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.len() + word.len() + 1 > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// `llm models options [list|show|set|clear]` — per-model default -o options
/// in the models.options table of config.json.
fn options(argv: &[String]) -> i32 {
    let (sub, rest) = split_subcommand(argv, "list");
    let rest: Vec<String> = rest.to_vec();
    let args = match parse(&rest, SIMPLE_SPECS) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let mut model_options = config::load_model_options();
    match sub {
        "list" => {
            if model_options.is_empty() {
                eprintln!("No default options set for any models.");
                return 0;
            }
            for (model, opts) in &model_options {
                println!("{model}:");
                for (k, v) in opts {
                    println!("  {k}: {v}");
                }
            }
            0
        }
        "show" => {
            let Some(model) = args.first_positional() else {
                eprintln!("Error: Missing argument 'MODEL'.");
                return 2;
            };
            match model_options.get(model) {
                Some(opts) => {
                    for (k, v) in opts {
                        println!("{k}: {v}");
                    }
                    0
                }
                None => {
                    eprintln!("No default options set for model '{model}'.");
                    1
                }
            }
        }
        "set" => {
            if args.positionals.len() < 3 {
                eprintln!("Error: Usage: llm models options set MODEL KEY VALUE");
                return 2;
            }
            let (model, key, value) = (
                &args.positionals[0],
                &args.positionals[1],
                &args.positionals[2],
            );
            model_options
                .entry(model.to_string())
                .or_default()
                .insert(key.to_string(), value.to_string());
            match config::save_model_options(&model_options) {
                Ok(()) => {
                    eprintln!("Set default option {key}={value} for model {model}");
                    0
                }
                Err(e) => {
                    eprintln!("Error: failed to save options: {e}");
                    1
                }
            }
        }
        "clear" => {
            let Some(model) = args.first_positional() else {
                eprintln!("Error: Missing argument 'MODEL'.");
                return 2;
            };
            let Some(opts) = model_options.get_mut(model) else {
                eprintln!("Error: No options found for model '{model}'");
                return 1;
            };
            if let Some(key) = args.positionals.get(1) {
                if opts.remove(key).is_some() {
                    eprintln!("Cleared option '{key}' for model {model}");
                }
                if opts.is_empty() {
                    model_options.remove(model);
                }
            } else {
                let keys: Vec<String> = opts.keys().cloned().collect();
                eprintln!("Cleared {} options for model {model}", keys.join(", "));
                model_options.remove(model);
            }
            match config::save_model_options(&model_options) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("Error: failed to save options: {e}");
                    1
                }
            }
        }
        other => {
            eprintln!("Error: No such command 'models options {other}'.");
            2
        }
    }
}
