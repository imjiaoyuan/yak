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
    // a codex provider carries its token in auth.json, not api_key: the
    // logout must drop both or the dead entry keeps refreshing a login the
    // user believes is gone
    if cfg.providers.get(name).map(|p| p.kind.as_str()) == Some("codex")
        && let Err(e) = crate::providers::chatgpt_oauth::remove_token(name)
    {
        eprintln!(
            "{}Warning: could not remove the saved login from {}: {e}{}",
            crate::theme::err().dim,
            crate::providers::chatgpt_oauth::auth_file_display().display(),
            crate::theme::err().reset
        );
    }
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
    if trimmed.is_empty() || trimmed == "\u{1b}" {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// `prompt` with a shown default: enter (empty answer) takes it, esc/EOF
/// cancels.
fn prompt_with_default(label: &str, default: &str) -> Option<String> {
    eprint!("{label} (default {default}): ");
    std::io::stderr().flush().ok()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).ok()? == 0 {
        return None;
    }
    default_answer(&line, default)
}

fn default_answer(line: &str, default: &str) -> Option<String> {
    match line.trim() {
        "" => Some(default.to_string()),
        "\u{1b}" => None,
        value => Some(value.to_string()),
    }
}

pub(crate) fn wizard(preset_arg: Option<&str>) -> Result<(), String> {
    // step 1 — provider. Alphabetical, plus `custom` for a base URL we do
    // not know. esc here (or at any later step) aborts the whole thing: no
    // partial config is written. A provider named on the command line
    // (`/login openai`) answers this step up front.
    let list = presets();
    let fast = preset_arg.is_some_and(|name| name != "custom");
    let picked: Option<usize> = match preset_arg {
        // `custom` has no catalog values to prefill: it still asks
        Some("custom") => None,
        Some(wanted) => Some(
            list.iter()
                .position(|p| p.name == wanted)
                .ok_or_else(|| format!("unknown provider '{wanted}' — run /login to pick one"))?,
        ),
        None => {
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
            (idx < list.len()).then_some(idx)
        }
    };
    let preset: Option<&Preset> = picked.map(|i| &list[i]);
    // a second subscription to the same provider (two OpenCode Go keys) is a
    // second provider entry: prefill the next free name so the new one never
    // silently overwrites the first
    let preset_name = match preset {
        Some(p) => {
            let cfg = config::load();
            next_free_name(&p.name, |n| cfg.providers.contains_key(n))
        }
        None => String::new(),
    };

    // name and URL are decided before the key so esc during the key step
    // still has nothing to undo — nothing is written until the key is in.
    // The fast path takes the catalog entry as typed instead of asking for
    // values it already knows.
    let name = match &preset {
        Some(_) if fast => preset_name.clone(),
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
            let Some(k) = prompt_with_default(
                "Kind [openai-compat/anthropic/responses/gemini]",
                "openai-compat",
            ) else {
                return cancelled();
            };
            if matches!(
                k.as_str(),
                "openai-compat" | "anthropic" | "responses" | "gemini"
            ) {
                break k;
            }
            eprintln!("kind must be openai-compat, anthropic, responses or gemini");
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
            Some(p) if fast => p.base_url.clone(),
            Some(p) => prompt_with_default("Base URL", &p.base_url).ok_or("cancelled")?,
            None => prompt("Base URL (e.g. https://api.deepseek.com/v1)").ok_or("cancelled")?,
        },
    };
    // refuse a URL that cannot be requested here, at the prompt the user is
    // looking at: otherwise the failure surfaces later as an opaque "invalid
    // uri character" from the model fetch, and the wizard would happily write
    // the broken base URL to config once a model id was typed
    validate_base_url(&base_url)?;
    if fast {
        eprintln!(
            "{}provider {name} · {kind} · {base_url}{}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
    }

    // step 2 — a ChatGPT subscription login (kind codex) takes no API key:
    // the OAuth browser flow owns every step from here
    if kind == "codex" {
        return chatgpt_login(&name, &base_url);
    }

    // step 2 — the key, hidden. Empty answer takes the detected env var as a
    // ${VAR} reference. esc aborts without writing anything.
    let detected = preset
        .and_then(|p| env_for(&p.name))
        .filter(|k| std::env::var_os(k).is_some())
        .or_else(|| {
            // custom flow: check the obvious names for the chosen kind
            let vars: &[&str] = if kind == "anthropic" {
                &["ANTHROPIC_API_KEY"]
            } else if kind == "gemini" {
                &["GEMINI_API_KEY", "GOOGLE_API_KEY"]
            } else {
                &["OPENAI_COMPAT_API_KEY", "OPENAI_API_KEY"]
            };
            vars.iter().find(|k| std::env::var_os(k).is_some()).copied()
        });
    let hint = match detected {
        Some(var) => format!("API key (empty = use ${{{var}}}, esc cancels): "),
        None => "API key (esc cancels): ".to_string(),
    };
    let typed = match crate::term::read_hidden(&hint) {
        Ok(value) => value.trim().to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return cancelled(),
        Err(e) => return Err(format!("cannot read API key: {e}")),
    };
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

/// A base URL the `/models` request can be built from: an http(s) scheme and
/// no whitespace (ureq rejects a space with "http: invalid uri character").
fn validate_base_url(base_url: &str) -> Result<(), String> {
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err(format!(
            "invalid base URL (must start with http:// or https://): {base_url}"
        ));
    }
    if base_url.contains(char::is_whitespace) {
        return Err(format!(
            "invalid base URL (contains whitespace): {base_url}"
        ));
    }
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

/// The ChatGPT subscription login: start the local callback server, open
/// the browser, exchange the token, write auth.json, then write the
/// kind=codex provider entry (the models list is the Codex channel's
/// public catalog; /model switches later).
fn chatgpt_login(name: &str, base_url: &str) -> Result<(), String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Err("ChatGPT login needs a terminal (the browser callback flow)".into());
    }
    // the callback server comes first: the authorize URL has to carry the
    // port it actually bound (1455 when free, any free loopback port else)
    let server = crate::providers::chatgpt_oauth::spawn_callback_server()?;
    let redirect = crate::providers::chatgpt_oauth::redirect_uri(server.port());
    let flow = crate::providers::chatgpt_oauth::start_login(&redirect)?;
    eprintln!(
        "{}opening the browser to sign in with ChatGPT …{}",
        crate::theme::err().dim,
        crate::theme::err().reset
    );
    eprintln!(
        "{}(no browser? open this URL manually:) {}{}",
        crate::theme::err().dim,
        flow.url,
        crate::theme::err().reset
    );
    crate::platform::open_url(&flow.url);
    eprintln!(
        "{}waiting for the login callback on {redirect}{}",
        crate::theme::err().dim,
        crate::theme::err().reset
    );
    // whichever comes first ends the wait: the browser callback (no
    // keystroke needed) or a redirect URL pasted here. esc/ctrl-c cancel.
    // The server is dropped on every exit path below, freeing its port.
    let Some(url) = wait_for_login(server.urls())? else {
        return cancelled();
    };
    let token = crate::providers::chatgpt_oauth::manual_complete(flow, &url)?;
    let models = crate::providers::codex::CHANNEL_MODELS;
    crate::providers::chatgpt_oauth::write_token(name, &token)?;
    let mut cfg = config::load();
    cfg.providers.insert(
        name.to_string(),
        Provider {
            kind: "codex".into(),
            base_url: base_url.to_string(),
            api_key: None,
            models: models.iter().map(|s| s.to_string()).collect(),
        },
    );
    config::save(&cfg).map_err(|e| e.to_string())?;
    config::try_set_default_model(&format!("{name}/{}", models[0]))
        .map_err(|e| format!("provider saved but the default model was not: {e}"))?;
    eprintln!(
        "{}default model: {name}/{}{}",
        crate::theme::err().dim,
        models[0],
        crate::theme::err().reset
    );
    println!(
        "ChatGPT login saved to {}",
        crate::providers::chatgpt_oauth::auth_file_display().display()
    );
    Ok(())
}

/// How long one login may sit waiting for the browser.
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Wait for whichever finishes the login first: the browser callback or a
/// redirect URL pasted here. Both are polled from a raw-mode read, so the
/// callback never waits for a keystroke (a cooked-mode `read_line` blocked
/// past the browser's redirect, leaving the terminal waiting after the page
/// already said "sign-in complete") and esc cancels without Enter.
/// Typed bytes are echoed; backspace edits; enter submits the pasted URL.
fn wait_for_login(urls: &std::sync::mpsc::Receiver<String>) -> Result<Option<String>, String> {
    let p = crate::theme::err();
    let prompt = format!(
        "{}Redirect URL (or just wait for the browser — esc cancels): {}",
        p.dim, p.reset
    );
    let Some(mut term) = crate::platform::RawTerm::acquire(1, 0) else {
        // no raw mode (a terminal that refuses termios): the browser is the
        // only path left, so wait on it alone
        eprintln!("{prompt}");
        eprintln!(
            "{}(keyboard input unavailable — waiting for the browser){}",
            p.dim, p.reset
        );
        return urls
            .recv_timeout(LOGIN_TIMEOUT)
            .map(Some)
            .map_err(|_| "login timed out (10 min) — run /login again".to_string());
    };
    let mut out = std::io::stderr();
    let _ = write!(out, "{prompt}");
    let _ = out.flush();
    wait_for_input(urls, &mut || term.next_byte(), &mut out, LOGIN_TIMEOUT)
}

/// The race itself, with the key source and the echo sink injected so the
/// terminal-less test drives the same loop the prompt does.
fn wait_for_input(
    callback: &std::sync::mpsc::Receiver<String>,
    next: &mut impl FnMut() -> crate::platform::RawByte,
    out: &mut impl Write,
    timeout: std::time::Duration,
) -> Result<Option<String>, String> {
    use crate::platform::RawByte;
    let deadline = std::time::Instant::now() + timeout;
    let mut buffer = String::new();
    loop {
        // the browser first: a callback that already landed must never wait
        // for a keystroke — that was the bug this loop replaces
        if let Ok(url) = callback.try_recv() {
            let _ = writeln!(out);
            return Ok(Some(url));
        }
        // a SIGINT that cannot become a key (raw mode keeps it out of ISIG)
        // still sets the cooperative flag; treat it as the cancel it is
        if crate::core::http::interrupted() {
            crate::core::http::clear_interrupt();
            let _ = writeln!(out);
            return Ok(None);
        }
        match next() {
            RawByte::Key(0x03) | RawByte::Key(0x04) => {
                let _ = writeln!(out);
                return Ok(None);
            }
            RawByte::Key(0x1b) => {
                // a lone esc cancels; an arrow or home/end key (ESC + CSI)
                // is a stray keypress, so swallow the sequence and keep
                // waiting instead of tearing the login down
                match next() {
                    RawByte::Timeout => {
                        let _ = writeln!(out);
                        return Ok(None);
                    }
                    RawByte::Key(b'[' | b'O') => loop {
                        match next() {
                            RawByte::Key(b) if (0x40..=0x7e).contains(&b) => break,
                            RawByte::Key(_) => {}
                            RawByte::Timeout => break,
                        }
                    },
                    RawByte::Key(_) => {}
                }
            }
            RawByte::Key(b'\r' | b'\n') => {
                let _ = writeln!(out);
                let value = buffer.trim().to_string();
                return Ok((!value.is_empty()).then_some(value));
            }
            RawByte::Key(0x7f | 0x08) => {
                if buffer.pop().is_some() {
                    let _ = write!(out, "{}", crate::theme::cursor().erase_char);
                    let _ = out.flush();
                }
            }
            RawByte::Key(b) if b.is_ascii_graphic() || b == b' ' => {
                buffer.push(b as char);
                let _ = out.write_all(&[b]);
                let _ = out.flush();
            }
            RawByte::Key(_) => {}
            RawByte::Timeout => {
                if std::time::Instant::now() >= deadline {
                    let _ = writeln!(out);
                    return Err("login timed out (10 min) — run /login again".into());
                }
            }
        }
    }
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
    use super::{default_answer, next_free_name, validate_base_url, wait_for_input};
    use crate::platform::RawByte;
    use std::collections::BTreeSet;
    use std::time::Duration;

    /// Feeds a fixed byte script to the wait loop, one key per call; a spent
    /// script times out like an idle terminal.
    fn keys(script: &[u8]) -> impl FnMut() -> RawByte {
        let script = script.to_vec();
        let mut at = 0usize;
        move || {
            let Some(&b) = script.get(at) else {
                return RawByte::Timeout;
            };
            at += 1;
            RawByte::Key(b)
        }
    }

    /// The regression: the browser page said "sign-in complete" while the
    /// terminal still waited for a keypress, because the prompt read stdin
    /// before it ever looked at the callback channel.
    #[test]
    fn a_browser_callback_ends_the_wait_without_a_keystroke() {
        let (tx, rx) = std::sync::mpsc::channel();
        let url = "http://127.0.0.1:1455/auth/callback?code=x&state=y";
        tx.send(url.to_string()).unwrap();
        let got = wait_for_input(
            &rx,
            &mut || unreachable!("the callback must be read before any key"),
            &mut Vec::new(),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(got.as_deref(), Some(url));
    }

    #[test]
    fn a_pasted_redirect_url_is_submitted_on_enter() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let url = "http://127.0.0.1:1455/auth/callback?code=x&state=y";
        let mut script = url.as_bytes().to_vec();
        script.push(b'\r');
        let got = wait_for_input(
            &rx,
            &mut keys(&script),
            &mut Vec::new(),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(got.as_deref(), Some(url));
    }

    #[test]
    fn esc_ctrl_c_and_an_empty_enter_cancel_the_login() {
        for script in [&[0x1b][..], &[0x03][..], &[0x04][..], b"\r"] {
            let (_tx, rx) = std::sync::mpsc::channel();
            let got = wait_for_input(
                &rx,
                &mut keys(script),
                &mut Vec::new(),
                Duration::from_secs(1),
            )
            .unwrap();
            assert_eq!(got, None, "{script:?} must cancel");
        }
    }

    #[test]
    fn backspace_edits_the_pasted_url() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let got = wait_for_input(
            &rx,
            &mut keys(b"httx\x7f\r"),
            &mut Vec::new(),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(got.as_deref(), Some("htt"));
    }

    #[test]
    fn an_arrow_key_is_not_an_esc_cancel() {
        // the esc prefix of a CSI key must not tear the login down: only a
        // bare esc (no sequence behind it) cancels
        let (_tx, rx) = std::sync::mpsc::channel();
        let got = wait_for_input(
            &rx,
            &mut keys(b"\x1b[Aurl\r"),
            &mut Vec::new(),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(got.as_deref(), Some("url"));
    }

    #[test]
    fn a_silent_login_times_out() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let err = wait_for_input(
            &rx,
            &mut || RawByte::Timeout,
            &mut Vec::new(),
            Duration::from_millis(0),
        )
        .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn an_unknown_provider_name_is_refused_before_any_prompt() {
        // `/login nope` fails loudly instead of falling back to the menu
        let err = super::wizard(Some("nope")).unwrap_err();
        assert!(err.contains("unknown provider 'nope'"), "{err}");
        assert!(err.contains("/login"), "the error names the menu: {err}");
    }

    #[test]
    fn base_url_validation_refuses_whitespace_and_missing_schemes() {
        // the regression: the catalog shipped ".../aterminal coding/paas/v4"
        // and the wizard accepted it, failing later as "invalid uri character"
        assert_eq!(
            validate_base_url("https://api.z.ai/aterminal coding/paas/v4").unwrap_err(),
            "invalid base URL (contains whitespace): https://api.z.ai/aterminal coding/paas/v4"
        );
        assert!(validate_base_url("api.deepseek.com/v1").is_err());
        assert!(validate_base_url("ftp://example.com").is_err());
        assert!(validate_base_url("https://api.z.ai/api/coding/paas/v4").is_ok());
        assert!(validate_base_url("http://localhost:11434/v1").is_ok());
    }

    #[test]
    fn a_default_prompt_distinguishes_enter_from_escape() {
        assert_eq!(default_answer("\n", "preset"), Some("preset".into()));
        assert_eq!(default_answer("\u{1b}\n", "preset"), None);
        assert_eq!(default_answer("custom\n", "preset"), Some("custom".into()));
    }

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
