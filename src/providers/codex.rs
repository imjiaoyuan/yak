//! ChatGPT subscription authentication and endpoint selection. The Responses
//! wire format and decoder are shared with ordinary API-key providers.

use super::{PromptInput, ResolvedModel};
use crate::core::http::{Event, HttpRequest};

/// The Codex channel's model catalog, newest first. The channel serves no
/// public /models endpoint, so the list is carried here (and written into
/// the provider entry at login; hand-edit config.json to add one).
pub const CHANNEL_MODELS: &[&str] = &[
    "gpt-6.1-sol",
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.3-codex-spark",
];

pub fn run(
    m: &ResolvedModel,
    input: &PromptInput,
    stream: bool,
    on_event: &mut dyn FnMut(Event),
) -> Result<(), String> {
    let token = super::chatgpt_oauth::access_token(&m.provider_name)?;
    let account = super::chatgpt_oauth::account_id(&token)
        .ok_or("the ChatGPT access token carries no account id — run /login again")?;
    let base = m.base_url.trim_end_matches('/');
    let url = if base.ends_with("/responses") {
        base.to_string()
    } else if base.ends_with("/codex") {
        format!("{base}/responses")
    } else {
        format!("{base}/codex/responses")
    };
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "text/event-stream".to_string()),
        ("Authorization".to_string(), format!("Bearer {token}")),
        ("chatgpt-account-id".to_string(), account),
        (
            "OpenAI-Beta".to_string(),
            "responses=experimental".to_string(),
        ),
        ("originator".to_string(), "yak".to_string()),
    ];
    if let Some(key) = input.cache_key {
        headers.push(("session-id".to_string(), key.to_string()));
        headers.push(("x-client-request-id".to_string(), key.to_string()));
    }
    let body = super::responses::build_body(m, input, stream)?.to_string();
    super::check_request_body(&body, input)?;
    let mut decoder = super::responses::ResponseStream::default();
    let mut buffered = Vec::new();
    let mut sink = |event| {
        if stream {
            on_event(event);
        } else {
            buffered.push(event);
        }
    };
    // The subscription endpoint requires SSE even when the user asks to
    // buffer the answer. Only presentation changes with --no-stream.
    let result = super::dispatch(
        HttpRequest { url, headers, body },
        true,
        |event_type, chunk, usage, stop, on_event| {
            decoder.feed(event_type, chunk, usage, stop, on_event)
        },
        |_, _| {},
        &mut sink,
    );
    result?;
    for event in buffered {
        on_event(event);
    }
    Ok(())
}
