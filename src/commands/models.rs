//! `llm models` — the model configuration center: per-mode defaults (wizard
//! or direct args), provider keys, listing and per-model options. Every
//! subcommand is dual-form: bare on a terminal opens its interactive flow,
//! full arguments run directly.

use std::io::IsTerminal;

use crate::core::args::{OptSpec, render_help, split_subcommand};
use crate::core::config::{self};
use crate::{flag_spec, multi_spec, value_spec};

const LIST_SPECS: &[OptSpec] = &[
    multi_spec!(
        "query",
        Some('q'),
        "Filter models matching these strings",
        "QUERY"
    ),
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
        "options" => options(&rest),
        "--help" | "-h" | "help" => {
            print!(
                "{}",
                render_help(
                    "llm models [COMMAND] [ARGS]...",
                    "Manage the default model and per-model options\n\nCommands:\n  set       Set the default model (bare: interactive wizard)\n  get       Show the default\n  unset     Clear the default\n  key       Show or set a provider's API key\n  list      List available models\n  options   Per-model default options\n\nProviders are added and removed with `llm login` and `llm logout`.",
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

/// The interactive configurator behind bare `llm models` and `llm models set`:
/// the provider → model → thinking cascade, saved as the shared default.
fn wizard() -> i32 {
    let current = config::default_model().unwrap_or_default();
    let current_thinking = config::default_thinking();
    let Some(choice) = cascade_model_picker(&current, current_thinking.as_deref()) else {
        return 0;
    };
    if let Err(e) = config::try_set_default_model(&choice.model) {
        eprintln!("Error: failed to save: {e}");
        return 1;
    }
    if let Some(thinking) = choice.thinking
        && let Err(e) = config::try_set_default_thinking(thinking.as_deref())
    {
        eprintln!("Error: failed to save thinking: {e}");
        return 1;
    }
    eprintln!("\x1b[2mdefault model: {}\x1b[0m", choice.model);
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
        eprintln!("\x1b[2mno models available (run `llm login`)\x1b[0m");
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
    let key = cfg.api_key(provider).unwrap_or_default();
    let live =
        crate::providers::catalog::try_fetch_models(&provider.kind, &provider.base_url, &key);
    let mut ids: Vec<String> = provider.models.clone();
    for mid in live {
        if !ids.contains(&mid) {
            ids.push(mid);
        }
    }
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        eprintln!("\x1b[2mno models available (run `llm login`)\x1b[0m");
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

fn get(argv: &[String]) -> i32 {
    let (args, code) = crate::core::args::parse_with_help(argv, SIMPLE_SPECS, || {
        render_help(
            "llm models get",
            "Show the default model",
            SIMPLE_SPECS,
            &[],
        )
    });
    let Some(_) = args else { return code };
    println!("{}", default_line());
    0
}

/// The shared "default model" line used by `models get` and the `list`
/// header: the stored default with its thinking depth, or the unset hint.
fn default_line() -> String {
    match (config::default_model(), config::default_thinking()) {
        (Some(m), Some(t)) => format!("default    {m} (thinking: {t})"),
        (Some(m), None) => format!("default    {m}"),
        (None, _) => "default    (unset — llm models set)".to_string(),
    }
}

fn set(argv: &[String]) -> i32 {
    let (args, code) = crate::core::args::parse_with_help(argv, SET_SPECS, || {
        render_help(
            "llm models set MODEL",
            "Set the default model (bare: interactive wizard)",
            SET_SPECS,
            &[("MODEL", "provider/model id, alias or bare name")],
        )
    });
    let Some(args) = args else { return code };
    if args.positionals.is_empty() {
        if !std::io::stdin().is_terminal() {
            eprintln!("Error: Usage: llm models set MODEL [--thinking LEVEL]");
            return 2;
        }
        return wizard();
    }
    let model = args.positionals[0].clone();
    let thinking: Option<Option<String>> = match args.opt(&["thinking"]) {
        Some(level) => match crate::providers::parse_thinking_level(level) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("Error: {e}");
                return 2;
            }
        },
        None => None,
    };
    let cfg = config::load();
    let (n, _, m) = match cfg.resolve_model(&model) {
        Ok(Some(v)) => v,
        Ok(None) => {
            eprintln!("Error: Unknown model: {model}");
            return 1;
        }
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let qualified = format!("{n}/{m}");
    if let Err(e) = config::try_set_default_model(&qualified) {
        eprintln!("Error: failed to save: {e}");
        return 1;
    }
    if let Some(t) = &thinking
        && let Err(e) = config::try_set_default_thinking(t.as_deref())
    {
        eprintln!("Error: failed to save thinking: {e}");
        return 1;
    }
    let depth = thinking
        .map(|t| format!(" (thinking: {})", t.unwrap_or_else(|| "off".to_string())))
        .unwrap_or_default();
    eprintln!("default model: {qualified}{depth}");
    0
}

fn unset(argv: &[String]) -> i32 {
    let (args, code) = crate::core::args::parse_with_help(argv, SIMPLE_SPECS, || {
        render_help(
            "llm models unset",
            "Clear the default model",
            SIMPLE_SPECS,
            &[],
        )
    });
    let Some(_) = args else { return code };
    match config::unset_default() {
        Ok(()) => {
            eprintln!("default model cleared");
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

// key — provider API keys

fn key(argv: &[String]) -> i32 {
    let (args, code) = crate::core::args::parse_with_help(argv, KEY_SPECS, || {
        render_help(
            "llm models key [PROVIDER [VALUE]]",
            "Show or set a provider's API key",
            KEY_SPECS,
            &[],
        )
    });
    let Some(args) = args else { return code };
    let cfg = config::load();
    let Some(name) = args.first_positional().map(String::from) else {
        if cfg.providers.is_empty() {
            println!("No providers configured (llm login)");
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
        eprintln!("Error: No provider found with name '{name}' (add one with llm login)");
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
    match cfg.api_key(p) {
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

// list / options — browsing and per-model defaults

fn list(argv: &[String]) -> i32 {
    let (args, code) = crate::core::args::parse_with_help(argv, LIST_SPECS, || {
        render_help(
            "llm models list [OPTIONS]",
            "List available models",
            LIST_SPECS,
            &[],
        )
    });
    let Some(args) = args else { return code };
    let cfg = config::load();
    let aliases = config::load_aliases();
    let queries = args.multi(&["query"]);

    // the stored default leads: what runs matters more than the inventory,
    // and only shows without filters
    if queries.is_empty() {
        println!("{}", default_line());
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
                println!(
                    "\x1b[90m{provider}/ · no models listed · `llm login` fetches them\x1b[0m"
                );
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
            let alias_suffix = if model_aliases.is_empty() {
                String::new()
            } else {
                format!(" (aliases: {})", model_aliases.join(", "))
            };
            println!("{qualified}{alias_suffix}");
        }
    }
    0
}

/// `llm models options [list|show|set|clear]` — per-model default -o options
/// in the models.options table of config.json.
fn options(argv: &[String]) -> i32 {
    let (sub, rest) = split_subcommand(argv, "list");
    let rest: Vec<String> = rest.to_vec();
    let (args, code) = crate::core::args::parse_with_help(&rest, SIMPLE_SPECS, || {
        render_help(
            "llm models options [list|show|set|clear]",
            "Per-model default -o options, in the models.options table",
            SIMPLE_SPECS,
            &[("MODEL", "provider/model id, alias or bare name")],
        )
    });
    let Some(args) = args else { return code };
    let mut model_options = config::load_model_options();
    // the options table is keyed by qualified id: resolve alias/bare names
    // so a hand-typed "deepseek-chat" lands where `llm prompt` reads it
    let qualified_model = |args: &crate::core::args::ParsedArgs| -> String {
        let Some(query) = args.first_positional() else {
            return String::new();
        };
        let cfg = config::load();
        cfg.resolve_model(query)
            .ok()
            .flatten()
            .map(|(n, _, m)| format!("{n}/{m}"))
            .unwrap_or_else(|| query.to_string())
    };
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
            let Some(_) = args.first_positional() else {
                eprintln!("Error: Missing argument 'MODEL'.");
                return 2;
            };
            let model = qualified_model(&args);
            match model_options.get(&model) {
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
                &qualified_model(&args),
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
            let Some(_) = args.first_positional() else {
                eprintln!("Error: Missing argument 'MODEL'.");
                return 2;
            };
            let model = qualified_model(&args);
            let Some(opts) = model_options.get_mut(&model) else {
                eprintln!("Error: No options found for model '{model}'");
                return 1;
            };
            if let Some(key) = args.positionals.get(1) {
                match opts.remove(key) {
                    Some(_) => eprintln!("Cleared option '{key}' for model {model}"),
                    None => eprintln!("no option '{key}' set for model {model}"),
                }
                if opts.is_empty() {
                    model_options.remove(&model);
                }
            } else {
                let keys: Vec<String> = opts.keys().cloned().collect();
                eprintln!("Cleared {} options for model {model}", keys.join(", "));
                model_options.remove(&model);
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
