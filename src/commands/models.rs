//! The model pickers behind the REPL's `/model` and `/thinking`: the
//! provider → model → thinking cascade saved as the shared default, and the
//! reasoning-effort picker. The `llm models` CLI is gone — this is library
//! code only.

use crate::core::config;

// interactive wizard — mode → provider → model → thinking depth

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
        eprintln!(
            "{}no models available (run /login){}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
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
    eprintln!(
        "{}fetching models from {pname} …{}",
        crate::theme::err().dim,
        crate::theme::err().reset
    );
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
        eprintln!(
            "{}no models available (run /login){}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
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
