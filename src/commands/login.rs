//! The provider wizard behind `llm models add` (catalog of common presets,
//! hidden-input key capture, live model list) and the removal picker behind
//! `llm models rm`.

use std::io::{BufRead, Write};

use crate::core::config::{self, Provider};

struct Preset {
    name: String,
    kind: String,
    base_url: String,
    env_keys: Vec<&'static str>,
    needs_key: bool,
    /// interactive URL building: cloudflare-gateway | cloudflare-workers | azure
    template: Option<&'static str>,
}

/// The preset catalog: every catalogued provider plus the interactive
/// URL-template providers (Cloudflare gateways, Azure) that need an account
/// id prompted at login time.
fn presets() -> Vec<Preset> {
    let mut v: Vec<Preset> = crate::providers::catalog::ALL
        .iter()
        .map(|e| Preset {
            name: e.id.to_string(),
            kind: e.kind.to_string(),
            base_url: e.base_url.to_string(),
            env_keys: if e.env.is_empty() {
                Vec::new()
            } else {
                vec![e.env]
            },
            needs_key: !e.env.is_empty(),
            template: None,
        })
        .collect();
    v.push(Preset {
        name: "cloudflare-ai-gateway".into(),
        kind: "openai-compat".into(),
        base_url: String::new(),
        env_keys: vec!["CLOUDFLARE_API_KEY"],
        needs_key: true,
        template: Some("cloudflare-gateway"),
    });
    v.push(Preset {
        name: "cloudflare-workers-ai".into(),
        kind: "openai-compat".into(),
        base_url: String::new(),
        env_keys: vec!["CLOUDFLARE_API_KEY"],
        needs_key: true,
        template: Some("cloudflare-workers"),
    });
    v.push(Preset {
        name: "azure-openai".into(),
        kind: "openai-compat".into(),
        base_url: String::new(),
        env_keys: vec!["AZURE_OPENAI_API_KEY"],
        needs_key: true,
        template: Some("azure"),
    });
    v
}

/// Menu over the configured providers; removing one drops its config.json
/// entry, key included.
pub(crate) fn logout_picker() -> Result<(), String> {
    let mut cfg = config::load();
    if cfg.providers.is_empty() {
        eprintln!("\x1b[2mno providers configured (llm models add)\x1b[0m");
        return Ok(());
    }
    let items: Vec<String> = cfg
        .providers
        .iter()
        .map(|(n, p)| format!("{:<11} {} ({} models)", n, p.base_url, p.models.len()))
        .collect();
    let Some(i) = crate::term::lineedit::pick("remove provider:", &items, true) else {
        eprintln!("\x1b[2maborted\x1b[0m");
        return Ok(());
    };
    let name = cfg
        .providers
        .keys()
        .nth(i)
        .expect("picked in range")
        .clone();
    cfg.providers.remove(&name);
    config::save(&cfg).map_err(|e| e.to_string())?;
    eprintln!(
        "\x1b[2mremoved provider '{name}' (and its key) from {}\x1b[0m",
        config::config_path().display()
    );
    Ok(())
}

fn prompt(label: &str) -> Option<String> {
    eprint!("{label}: ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let n = std::io::stdin().lock().read_line(&mut line).ok()?;
    if n == 0 {
        return None;
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn wizard() -> Result<(), String> {
    let list = presets();
    let mut items: Vec<String> = list
        .iter()
        .map(|p| {
            let env = p
                .env_keys
                .first()
                .map(|k| format!(" (${k})"))
                .unwrap_or_default();
            let base = if p.base_url.is_empty() {
                "(URL prompted)".to_string()
            } else {
                p.base_url.clone()
            };
            format!("{:<22} {}{}", p.name, base, env)
        })
        .collect();
    items.push("custom".to_string());
    let Some(idx) = crate::term::lineedit::pick("Add a provider:", &items, true) else {
        eprintln!("\x1b[2maborted\x1b[0m");
        return Ok(());
    };
    let (preset, preset_name): (Option<&Preset>, String) = if idx == list.len() {
        (None, String::new())
    } else {
        (Some(&list[idx]), list[idx].name.clone())
    };

    let name = match &preset {
        Some(_) => prompt(&format!("Provider name (default {preset_name})"))
            .unwrap_or_else(|| preset_name.clone()),
        None => prompt("Provider name (e.g. my-proxy)").ok_or("a provider name is required")?,
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("invalid provider name: {name}"));
    }

    let kind = match &preset {
        Some(p) => p.kind.to_string(),
        None => loop {
            let k = prompt("Kind [openai-compat/anthropic] (default openai-compat)")
                .unwrap_or_else(|| "openai-compat".to_string());
            if k == "openai-compat" || k == "anthropic" {
                break k;
            }
            eprintln!("kind must be openai-compat or anthropic");
        },
    };
    let base_url = match preset.and_then(|p| p.template) {
        Some("cloudflare-gateway") => {
            let acct = prompt("Cloudflare account id").ok_or("account id required")?;
            let gw = prompt("Gateway id").ok_or("gateway id required")?;
            format!("https://gateway.ai.cloudflare.com/v1/{acct}/{gw}")
        }
        Some("cloudflare-workers") => {
            let acct = prompt("Cloudflare account id").ok_or("account id required")?;
            format!("https://api.cloudflare.com/client/v4/accounts/{acct}/ai/v1")
        }
        Some("azure") => {
            let res = prompt("Azure resource name").ok_or("resource name required")?;
            format!("https://{res}.openai.azure.com/openai/v1")
        }
        _ => match &preset {
            Some(p) => prompt(&format!("Base URL (default {})", p.base_url))
                .unwrap_or_else(|| p.base_url.clone()),
            None => prompt("Base URL (e.g. https://api.deepseek.com/v1)")
                .ok_or("a base URL is required")?,
        },
    };

    // api key: hidden input. Empty answer takes the detected env var as a
    // ${VAR} reference; a pasted literal is stored inline in config.json.
    // Returns (stored api_key, key to use for the /models fetch).
    let (api_key, fetch_key): (Option<String>, String) = if preset.is_some_and(|p| !p.needs_key) {
        (None, String::new())
    } else {
        let detected = preset
            .and_then(|p| p.env_keys.iter().find(|k| std::env::var_os(k).is_some()))
            .copied()
            .or_else(|| {
                // custom flow: check the obvious names
                ["OPENAI_COMPAT_API_KEY"]
                    .iter()
                    .find(|k| std::env::var_os(k).is_some())
                    .copied()
            });
        let hint = match detected {
            Some(var) => format!("API key (empty = use ${{{var}}})"),
            None => "API key".to_string(),
        };
        let typed = crate::term::read_hidden(&hint)
            .unwrap_or_default()
            .trim()
            .to_string();
        if typed.is_empty() {
            match detected {
                Some(var) => (
                    Some(format!("${{{var}}}")),
                    std::env::var(var).unwrap_or_default(),
                ),
                None => (None, String::new()),
            }
        } else if let Some(var) = typed.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
            (Some(typed.clone()), std::env::var(var).unwrap_or_default())
        } else {
            (Some(typed.clone()), typed)
        }
    };

    // fetch the model list (best effort); unreachable endpoints leave the
    // provider model-less until `llm scan` refreshes it
    let models = fetch_models(&kind, &base_url, &fetch_key);

    let selected: Vec<String> = if models.is_empty() {
        match prompt("Model ids (comma-separated)") {
            Some(list) => list
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            None => Vec::new(),
        }
    } else {
        let mut items = vec![format!("all ({} models)", models.len())];
        items.extend(models.iter().cloned());
        match crate::term::lineedit::pick("models:", &items, true) {
            Some(0) => models.clone(),
            Some(i) => vec![models[i - 1].clone()],
            None => Vec::new(),
        }
    };
    if selected.is_empty() {
        eprintln!("no models selected — provider saved without models");
    }

    let mut cfg = config::load();
    if cfg.providers.contains_key(&name) {
        let overwrite = prompt(format!("Provider '{name}' exists — overwrite? [y/N]").as_str())
            .map(|a| a.eq_ignore_ascii_case("y"))
            .unwrap_or(false);
        if !overwrite {
            eprintln!("aborted");
            return Ok(());
        }
    }
    cfg.providers.insert(
        name.clone(),
        Provider {
            kind,
            base_url,
            api_key,
            models: selected.clone(),
        },
    );
    config::save(&cfg).map_err(|e| e.to_string())?;
    println!(
        "Provider '{name}' written to {}",
        config::config_path().display()
    );

    // default model: menu over the selected models plus a skip entry; a
    // skip (or cancel) still auto-defaults when nothing is set, so the
    // first provider's model makes a fresh install ready to run
    if !selected.is_empty() {
        let mut items: Vec<String> = selected.iter().map(|m| format!("{name}/{m}")).collect();
        items.push("skip (keep current default)".to_string());
        let picked = crate::term::lineedit::pick("default model:", &items, true);
        let chosen = match picked {
            Some(i) if i < selected.len() => Some(selected[i].clone()),
            _ => {
                if config::get_default_model().is_none() {
                    Some(selected[0].clone())
                } else {
                    None
                }
            }
        };
        if let Some(m) = chosen {
            match config::set_default_model(&format!("{name}/{m}")) {
                Ok(()) => eprintln!("\x1b[2mdefault model: {name}/{m}\x1b[0m"),
                Err(e) => eprintln!("Warning: failed to save default model: {e}"),
            }
        }
    }
    eprintln!("\nTry it:  llm ping {name}   |   llm \"hello\"   |   llm agent \"look around\"");
    Ok(())
}

/// Fetch the provider's /models list; the caller prints progress/errors.
pub(crate) fn try_fetch_models(kind: &str, base_url: &str, api_key: &str) -> Vec<String> {
    let (url, headers) = fetch_models_url(kind, base_url, api_key);
    fetch_model_list(&url, &headers).unwrap_or_default()
}

fn fetch_models(kind: &str, base_url: &str, api_key: &str) -> Vec<String> {
    let (url, _) = fetch_models_url(kind, base_url, api_key);
    eprintln!("Fetching models from {url} ...");
    let models = try_fetch_models(kind, base_url, api_key);
    if models.is_empty() {
        eprintln!("could not fetch models — falling back to the built-in list");
    }
    models
}

pub(crate) fn fetch_models_url(
    kind: &str,
    base_url: &str,
    api_key: &str,
) -> (String, Vec<(String, String)>) {
    let mut headers = Vec::new();
    if !api_key.is_empty() {
        if kind == "anthropic" {
            headers.push(("x-api-key".to_string(), api_key.to_string()));
            headers.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
        } else {
            headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
        }
    }
    let url = if kind == "anthropic" {
        format!("{}/v1/models", base_url.trim_end_matches('/'))
    } else {
        format!("{}/models", base_url.trim_end_matches('/'))
    };
    (url, headers)
}

/// Fetch a provider's model list (OpenAI-compatible /models endpoint); the
/// wizard offers what the endpoint actually serves.
fn fetch_model_list(url: &str, headers: &[(String, String)]) -> Result<Vec<String>, String> {
    let agent = crate::core::http::short_agent();
    let mut request = agent.get(url);
    for (k, v) in headers {
        request = request.header(k, v);
    }
    let resp = request.call().map_err(|e| e.to_string())?;
    if resp.status().as_u16() >= 400 {
        return Err(format!("HTTP {}", resp.status()));
    }
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut resp.into_body().into_reader(), &mut buf)
        .map_err(|e| e.to_string())?;
    let value: serde_json::Value = serde_json::from_str(&buf).map_err(|e| e.to_string())?;
    let array = value["data"]
        .as_array()
        .or_else(|| value["models"].as_array())
        .ok_or("no model list in response")?;
    Ok(array
        .iter()
        .filter_map(|m| m["id"].as_str().map(String::from))
        .collect())
}
