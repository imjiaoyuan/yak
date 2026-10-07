//! The provider lifecycle behind the REPL's `/login` and `/logout`: the
//! wizard (pick a provider, paste the key, pick the default model
//! — esc cancels at every step) and the removal picker. The
//! `yak login`/`yak logout` CLI is gone — this is library code only.

use std::io::{BufRead, Write};

use crate::core::config::{self, Provider};

/// Remove a provider from config and clear any default that pointed at it,
/// reporting the outcome to stderr. One save covers the provider removal;
/// the default clear is a separate read-modify-write so a failure there
/// warns without rolling back the removal.
fn remove_provider(cfg: &mut config::Config, name: &str) -> Result<(), String> {
    config::save(cfg).map_err(|e| e.to_string())?;
    eprintln!(
        "{}removed provider '{name}' (and its key) from {}{}",
        crate::theme::err().dim,
        config::config_path().display(),
        crate::theme::err().reset
    );
    match config::clear_default_for(name) {
        Ok(true) => eprintln!(
            "{}cleared the default model (pointed at {name}){}",
            crate::theme::err().dim,
            crate::theme::err().reset
        ),
        Ok(false) => {}
        Err(e) => eprintln!(
            "{}Warning: could not clear the default model pointed at {name}: {e}{}",
            crate::theme::err().dim,
            crate::theme::err().reset
        ),
    }
    Ok(())
}

struct Preset {
    name: String,
    kind: String,
    base_url: String,
    /// interactive URL building: cloudflare-gateway | cloudflare-workers | azure
    template: Option<&'static str>,
}

/// The preset catalog: every catalogued provider plus the interactive
/// URL-template providers (Cloudflare gateways, Azure) that need an account
/// id prompted at login time. Alphabetical — the picker is a menu, not a
/// registry dump.
fn presets() -> Vec<Preset> {
    let mut v: Vec<Preset> = crate::providers::catalog::ALL
        .iter()
        .map(|e| Preset {
            name: e.id.to_string(),
            kind: e.kind.to_string(),
            base_url: e.base_url.to_string(),
            template: None,
        })
        .collect();
    v.push(Preset {
        name: "cloudflare-ai-gateway".into(),
        kind: "openai-compat".into(),
        base_url: String::new(),
        template: Some("cloudflare-gateway"),
    });
    v.push(Preset {
        name: "cloudflare-workers-ai".into(),
        kind: "openai-compat".into(),
        base_url: String::new(),
        template: Some("cloudflare-workers"),
    });
    v.push(Preset {
        name: "azure-openai".into(),
        kind: "openai-compat".into(),
        base_url: String::new(),
        template: Some("azure"),
    });
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// Menu over the configured providers; removing one drops its config.json
/// entry, key included.
pub(crate) fn logout_picker() -> Result<(), String> {
    let mut cfg = config::load();
    if cfg.providers.is_empty() {
        eprintln!(
            "{}no providers configured (yak login){}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
        return Ok(());
    }
    let items: Vec<String> = cfg
        .providers
        .iter()
        .map(|(n, p)| format!("{:<11} {} ({} models)", n, p.base_url, p.models.len()))
        .collect();
    let Some(i) = crate::term::lineedit::pick("remove provider:", &items, true) else {
        eprintln!(
            "{}aborted{}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
        return Ok(());
    };
    let name = cfg
        .providers
        .keys()
        .nth(i)
        .expect("picked in range")
        .clone();
    cfg.providers.remove(&name);
    remove_provider(&mut cfg, &name)
}

/// The name to prefill for a catalog preset: `base` when free, else the
/// first free `base-2`, `base-3`, … — the second subscription to one
/// provider needs its own name, and `taken` is the configured provider set.
fn next_free_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    (2..10_000)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| base.to_string())
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

/// `prompt` with a shown default: enter (empty answer) takes it, esc/EOF
/// cancels.
fn prompt_with_default(label: &str, default: &str) -> Option<String> {
    prompt(&format!("{label} (default {default})")).or_else(|| Some(default.to_string()))
}

pub(crate) fn wizard() -> Result<(), String> {
    // step 1 — provider. Alphabetical, plus `custom` for a base URL we do
    // not know. esc here (or at any later step) aborts the whole thing: no
    // partial config is written.
    let list = presets();
    let mut items: Vec<String> = list
        .iter()
        .map(|p| {
            let base = if p.base_url.is_empty() {
                "(URL prompted)".to_string()
            } else {
                p.base_url.clone()
            };
            format!("{:<26} {}", p.name, base)
        })
        .collect();
    items.push("custom".to_string());
    let Some(idx) = crate::term::lineedit::pick("Add a provider:", &items, true) else {
        eprintln!(
            "{}aborted{}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
        return Ok(());
    };
    let (preset, preset_name): (Option<&Preset>, String) = if idx == list.len() {
        (None, String::new())
    } else {
        (Some(&list[idx]), list[idx].name.clone())
    };
    // a second subscription to the same provider (two OpenCode Go keys) is a
    // second provider entry: prefill the next free name so the new one never
    // silently overwrites the first
    let preset_name = if preset.is_some() {
        let cfg = config::load();
        next_free_name(&preset_name, |n| cfg.providers.contains_key(n))
    } else {
        preset_name
    };

    // name and URL are decided before the key so esc during the key step
    // still has nothing to undo — nothing is written until the key is in
    let name = match &preset {
        Some(_) => prompt_with_default("Provider name", &preset_name).ok_or("cancelled")?,
        None => prompt("Provider name (e.g. my-proxy)").ok_or("cancelled")?,
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("invalid provider name: {name}"));
    }
    let kind = match &preset {
        Some(p) => p.kind.clone(),
        None => loop {
            let Some(k) = prompt("Kind [openai-compat/anthropic] (default openai-compat)") else {
                return cancelled();
            };
            let k = if k.is_empty() {
                "openai-compat".to_string()
            } else {
                k
            };
            if k == "openai-compat" || k == "anthropic" {
                break k;
            }
            eprintln!("kind must be openai-compat or anthropic");
        },
    };
    let base_url = match preset.and_then(|p| p.template) {
        Some("cloudflare-gateway") => {
            let Some(acct) = prompt("Cloudflare account id") else {
                return cancelled();
            };
            let Some(gw) = prompt("Gateway id") else {
                return cancelled();
            };
            format!("https://gateway.ai.cloudflare.com/v1/{acct}/{gw}")
        }
        Some("cloudflare-workers") => {
            let Some(acct) = prompt("Cloudflare account id") else {
                return cancelled();
            };
            format!("https://api.cloudflare.com/client/v4/accounts/{acct}/ai/v1")
        }
        Some("azure") => {
            let Some(res) = prompt("Azure resource name") else {
                return cancelled();
            };
            format!("https://{res}.openai.azure.com/openai/v1")
        }
        _ => match &preset {
            Some(p) => prompt_with_default("Base URL", &p.base_url).ok_or("cancelled")?,
            None => prompt("Base URL (e.g. https://api.deepseek.com/v1)").ok_or("cancelled")?,
        },
    };

    // step 2 — the key, hidden. Empty answer takes the detected env var as a
    // ${VAR} reference. esc aborts without writing anything.
    let detected = preset
        .and_then(|p| env_for(&p.name))
        .filter(|k| std::env::var_os(k).is_some())
        .or_else(|| {
            // custom flow: check the obvious names for the chosen kind
            let vars: &[&str] = if kind == "anthropic" {
                &["ANTHROPIC_API_KEY"]
            } else {
                &["OPENAI_COMPAT_API_KEY", "OPENAI_API_KEY"]
            };
            vars.iter().find(|k| std::env::var_os(k).is_some()).copied()
        });
    let hint = match detected {
        Some(var) => format!("API key (empty = use ${{{var}}}, esc cancels): "),
        None => "API key (esc cancels): ".to_string(),
    };
    let typed = crate::term::read_hidden(&hint)
        .unwrap_or_default()
        .trim()
        .to_string();
    if typed.is_empty() && detected.is_none() {
        return Err("no API key entered".into());
    }
    let api_key: Option<String> = if typed.is_empty() {
        Some(format!("${{{}}}", detected.expect("checked above")))
    } else {
        Some(typed)
    };
    let fetch_key = match api_key.as_deref() {
        Some(k) if k.starts_with("${") && k.ends_with('}') => {
            std::env::var(&k[2..k.len() - 1]).unwrap_or_default()
        }
        Some(k) => k.to_string(),
        None => String::new(),
    };

    // step 3 — the default model, fetched live. Nothing has been written
    // yet; the single save below lands provider + default together.
    let models = fetch_models(&kind, &base_url, &fetch_key);
    let default = if models.is_empty() {
        let Some(m) = prompt("Model id (the default; e.g. deepseek-chat)") else {
            return cancelled();
        };
        Some(m)
    } else {
        let items: Vec<String> = models.clone();
        crate::term::lineedit::pick("default model:", &items, true).map(|i| models[i].clone())
    };
    let Some(default_model_id) = default else {
        return cancelled();
    };

    let mut cfg = config::load();
    cfg.providers.insert(
        name.clone(),
        Provider {
            kind,
            base_url,
            api_key,
            models: vec![default_model_id.clone()],
        },
    );
    config::save(&cfg).map_err(|e| e.to_string())?;
    match config::try_set_default_model(&format!("{name}/{default_model_id}")) {
        Ok(()) => {}
        Err(e) => eprintln!("Warning: failed to save the default model: {e}"),
    }
    eprintln!(
        "{}default model: {name}/{default_model_id}{}",
        crate::theme::err().dim,
        crate::theme::err().reset
    );
    println!(
        "Provider '{name}' written to {}",
        config::config_path().display()
    );
    eprintln!("\nTry it:  yak -m {name} \"hello\"   |   yak  (bare = interactive session)");
    Ok(())
}

fn cancelled() -> Result<(), String> {
    eprintln!(
        "{}aborted — nothing written{}",
        crate::theme::err().dim,
        crate::theme::err().reset
    );
    Ok(())
}

/// The catalog entry's env var, matching the registry (AZURE_OPENAI_API_KEY
/// for the azure preset, CLOUDFLARE_API_KEY for both cloudflare ones).
fn env_for(preset_name: &str) -> Option<&'static str> {
    crate::providers::catalog::ALL
        .iter()
        .find(|e| e.id == preset_name)
        .map(|e| e.env)
        .filter(|e| !e.is_empty())
        .or(match preset_name {
            "azure-openai" => Some("AZURE_OPENAI_API_KEY"),
            "cloudflare-ai-gateway" | "cloudflare-workers-ai" => Some("CLOUDFLARE_API_KEY"),
            _ => None,
        })
}

fn fetch_models(kind: &str, base_url: &str, api_key: &str) -> Vec<String> {
    let (url, _) = crate::providers::catalog::fetch_models_url(kind, base_url, api_key);
    eprintln!("Fetching models from {url} ...");
    let mut models = crate::providers::catalog::fetch_models(kind, base_url, api_key)
        .unwrap_or_else(|e| {
            eprintln!("could not fetch models ({e}) — falling back to the built-in list");
            Vec::new()
        });
    if models.is_empty() {
        eprintln!("the provider listed no models — falling back to the built-in list");
    }
    // the wizard's list is alphabetical regardless of the endpoint's order
    models.sort();
    models.dedup();
    models
}

#[cfg(test)]
mod tests {
    use super::next_free_name;
    use std::collections::BTreeSet;

    fn taken(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_second_subscription_gets_the_next_free_name() {
        let set = taken(&["opencode-go"]);
        assert_eq!(
            next_free_name("opencode-go", |n| set.contains(n)),
            "opencode-go-2"
        );

        // the suffix walks past every name already in use
        let set = taken(&["opencode-go", "opencode-go-2", "opencode-go-3"]);
        assert_eq!(
            next_free_name("opencode-go", |n| set.contains(n)),
            "opencode-go-4"
        );

        // a free name is left as the caller typed it
        let empty = taken(&[]);
        assert_eq!(
            next_free_name("deepseek", |n| empty.contains(n)),
            "deepseek"
        );
    }
}
