//! ChatGPT subscription authentication and endpoint selection. The Responses
//! wire format and decoder are shared with ordinary API-key providers.

use super::{PromptInput, ResolvedModel};
use crate::core::http::{Event, HttpRequest};

const CATALOG_CLIENT_VERSION: &str = "0.166.0";

pub fn fetch_models(name: &str, base_url: &str) -> Result<Vec<String>, String> {
    let token = super::chatgpt_oauth::access_token(name)?;
    let account = super::chatgpt_oauth::account_id(&token)
        .ok_or("the ChatGPT access token carries no account id — run /login again")?;
    fetch_catalog(base_url, &token, &account)
}

fn fetch_catalog(base_url: &str, token: &str, account: &str) -> Result<Vec<String>, String> {
    let base = base_url.trim_end_matches('/');
    let base = base.strip_suffix("/responses").unwrap_or(base);
    let base = if base.ends_with("/codex") {
        base.to_string()
    } else {
        format!("{base}/codex")
    };
    let url = format!("{base}/models?client_version={CATALOG_CLIENT_VERSION}");
    let mut response = crate::core::http::short_agent()
        .get(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .header("chatgpt-account-id", account)
        .header("originator", "codex_cli_rs")
        .header("Accept", "application/json")
        .call()
        .map_err(|e| format!("Codex model catalog unreachable: {e}"))?;
    let status = response.status().as_u16();
    if status >= 400 {
        return Err(format!("Codex model catalog returned HTTP {status}"));
    }
    let value: serde_json::Value = response
        .body_mut()
        .read_json()
        .map_err(|e| format!("invalid Codex model catalog: {e}"))?;
    parse_models(&value)
}

fn parse_models(value: &serde_json::Value) -> Result<Vec<String>, String> {
    let models = value["models"]
        .as_array()
        .ok_or("Codex catalog missing models array")?;
    let mut visible = Vec::new();
    for model in models {
        let visibility = model["visibility"]
            .as_str()
            .ok_or("Codex model missing visibility")?;
        let slug = model["slug"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("Codex model missing slug")?;
        if visibility == "list" {
            visible.push((
                model["priority"].as_i64().unwrap_or(i64::MAX),
                slug.to_string(),
            ));
        }
    }
    visible.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut ids = Vec::new();
    for (_, id) in visible {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

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
        HttpRequest {
            url,
            headers,
            body: std::sync::Arc::new(body),
        },
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

#[cfg(test)]
mod catalog_tests {
    #[test]
    fn catalog_filters_by_visibility_and_orders_by_priority() {
        let value = serde_json::json!({"models": [
            {"slug":"hidden", "visibility":"hide", "priority":0},
            {"slug":"second", "visibility":"list", "priority":2},
            {"slug":"first", "visibility":"list", "priority":1}
        ]});
        assert_eq!(
            super::parse_models(&value).unwrap(),
            vec!["first", "second"]
        );
    }

    #[test]
    fn catalog_request_uses_subscription_path_and_credentials() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                request.push_str(&line);
            }
            let body = r#"{"models":[{"slug":"live-model","visibility":"list","priority":1}]}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            request
        });
        let models = super::fetch_catalog(
            &format!("http://{address}/backend-api/codex/responses"),
            "test-token",
            "test-account",
        )
        .unwrap();
        assert_eq!(models, vec!["live-model"]);
        let request = server.join().unwrap().to_lowercase();
        assert!(
            request
                .starts_with("get /backend-api/codex/models?client_version=0.166.0 http/1.1\r\n")
        );
        assert!(request.contains("authorization: bearer test-token\r\n"));
        assert!(request.contains("chatgpt-account-id: test-account\r\n"));
    }

    #[test]
    fn malformed_catalog_is_rejected() {
        assert!(super::parse_models(&serde_json::json!({})).is_err());
        assert!(
            super::parse_models(&serde_json::json!({"models":[{"slug":"missing-visibility"}]}))
                .is_err()
        );
    }
}
