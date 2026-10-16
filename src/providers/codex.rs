//! The ChatGPT subscription's Codex channel adapter (the Responses API):
//! kind `codex`. Endpoint `chatgpt.com/backend-api/codex/responses`, the
//! ChatGPT OAuth token as Bearer, the account id in `chatgpt-account-id`.
//! The message shape is the Responses item stream (role message /
//! function_call / function_call_output / reasoning); the system prompt
//! rides the top-level `instructions`.

use serde_json::{Value, json};

use super::{Attachment, Msg, PromptInput, ResolvedModel, last_result_index};
use crate::core::attachments::{Kind, kind_of};
use crate::core::http::{Event, HttpRequest, StopReason, Usage};

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

/// One content part for an attachment: images ride `input_image` data URIs.
fn attachment_part(a: &Attachment) -> Result<Value, String> {
    if a.base64_data.is_empty() {
        return Ok(json!({
            "type": "input_text",
            "text": format!(
                "[{}: attached earlier in this conversation, bytes not available]",
                super::reference(a)
            )
        }));
    }
    let mime = a.mime_type.as_str();
    match kind_of(mime) {
        Some(Kind::Image) => Ok(json!({
            "type": "input_image",
            "image_url": format!("data:{mime};base64,{}", a.base64_data)
        })),
        // the Responses input takes no inline PDF/audio blocks: a text file
        // decodes into a text part, the rest is refused (the subscription
        // channel serves images for now)
        Some(Kind::Text) => {
            let text = super::decoded_text(a)?;
            let header = match &a.filename {
                Some(name) => format!("{name}\n"),
                None => String::new(),
            };
            Ok(json!({"type": "input_text", "text": format!("{header}{text}")}))
        }
        _ => Err(format!(
            "the ChatGPT (codex) channel takes image and text attachments, not '{mime}'"
        )),
    }
}

fn user_content(text: &str, attachments: &[Attachment]) -> Result<Value, String> {
    if attachments.is_empty() {
        return Ok(json!([{"type": "input_text", "text": text}]));
    }
    let mut parts = vec![json!({"type": "input_text", "text": text})];
    for a in attachments {
        parts.push(attachment_part(a)?);
    }
    Ok(Value::Array(parts))
}

pub fn build_body(
    m: &ResolvedModel,
    input: &PromptInput<'_>,
    stream: bool,
) -> Result<Value, String> {
    let last_result = last_result_index(input.history);
    let mut items: Vec<Value> = Vec::new();
    // tool-result images batch into a following user message (a Responses
    // function_call_output takes plain-text output only)
    let mut pending_images: Vec<Value> = Vec::new();
    let flush_images = |items: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if pending.is_empty() {
            return;
        }
        let mut parts = vec![json!({
            "type": "input_text",
            "text": "Attached image(s) from tool result:"
        })];
        parts.append(pending);
        items.push(json!({"role": "user", "content": parts}));
    };
    for (i, msg) in input.history.iter().enumerate() {
        match msg {
            Msg::User { text, attachments } => {
                flush_images(&mut items, &mut pending_images);
                items.push(json!({
                    "role": "user",
                    "content": super::user_content(m.supports_images(), text, attachments, attachment_part)?
                }));
            }
            Msg::Assistant {
                text,
                tool_calls,
                reasoning,
                reasoning_meta,
            } => {
                flush_images(&mut items, &mut pending_images);
                // a reasoning item: the encrypted content (how the official
                // client replays it) or the visible summary
                if let Some(r) = reasoning
                    && !r.is_empty()
                {
                    if let Some(meta) = reasoning_meta
                        && meta.get("encrypted_content").is_some()
                    {
                        let mut item = meta.clone();
                        if let Some(obj) = item.as_object_mut() {
                            obj.insert("type".into(), json!("reasoning"));
                        }
                        items.push(item);
                    } else {
                        items.push(json!({
                            "type": "reasoning",
                            "summary": [{"type": "summary_text", "text": r}],
                        }));
                    }
                }
                if !text.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
                for call in tool_calls {
                    items.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments.to_string(),
                    }));
                    if !super::call_answered(&last_result, &call.id, i) {
                        items.push(json!({
                            "type": "function_call_output",
                            "call_id": call.id,
                            "output": super::ORPHAN_RESULT,
                        }));
                    }
                }
            }
            Msg::ToolResult {
                call_id,
                content,
                attachments,
                ..
            } => {
                let mut output = content.clone();
                if !attachments.is_empty() && !m.supports_images() {
                    output.push_str("\n[image omitted: current model does not support images]");
                }
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                }));
                if let Some(parts) =
                    super::tool_result_images(m.supports_images(), attachments, attachment_part)?
                {
                    pending_images.extend(parts);
                }
            }
            Msg::Summary { text } => {
                flush_images(&mut items, &mut pending_images);
                items.push(json!({
                    "role": "user",
                    "content": [{"type": "input_text", "text": format!("<summary>\n{text}\n</summary>")}],
                }));
            }
        }
    }
    flush_images(&mut items, &mut pending_images);
    if !input.prompt.is_empty() || !input.attachments.is_empty() {
        items.push(json!({
            "role": "user",
            "content": user_content(input.prompt, input.attachments)?,
        }));
    }

    let mut body = json!({
        "model": m.model_id,
        "input": items,
        "stream": stream,
        "store": false,
        "parallel_tool_calls": true,
    });
    if let Some(system) = input.system {
        body["instructions"] = json!(system);
    }
    if !input.tools.is_empty() {
        body["tools"] = Value::Array(
            input
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    })
                })
                .collect(),
        );
    }
    if let Some(effort) = input.reasoning {
        body["reasoning"] = json!({"effort": effort, "summary": "auto"});
    } else {
        body["reasoning"] = json!({"effort": "medium", "summary": "auto"});
    }
    body["include"] = json!(["reasoning.encrypted_content"]);
    if let Some(key) = input.cache_key {
        body["prompt_cache_key"] = json!(key);
    }
    super::apply_options(&mut body, &m.options);
    Ok(body)
}

fn map_stop(status: &str, incomplete_reason: Option<&str>) -> StopReason {
    match incomplete_reason {
        Some("max_output_tokens") => StopReason::Length,
        _ if status == "incomplete" => StopReason::Length,
        _ => StopReason::Stop,
    }
}

/// Feed one Responses SSE event (the data JSON) through the request state.
pub(crate) fn feed_event(
    event_type: &str,
    ev: &Value,
    usage: &mut Option<Usage>,
    stop: &mut StopReason,
    on_event: &mut dyn FnMut(Event),
) {
    // the event name rides the data's `type` field (OpenAI Responses sends
    // bare data frames, no `event:` lines)
    let kind = ev["type"].as_str().unwrap_or(event_type);
    match kind {
        "response.output_item.done" => {
            let item = &ev["item"];
            match item["type"].as_str() {
                Some("message") => {
                    if let Some(text) = item["content"][0]["text"].as_str()
                        && !text.is_empty()
                    {
                        on_event(Event::Delta(text.to_string()));
                    }
                }
                Some("reasoning") => {
                    let text = item["summary"]
                        .as_array()
                        .map(|s| {
                            s.iter()
                                .filter_map(|p| p["text"].as_str())
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        })
                        .unwrap_or_default();
                    if !text.is_empty() {
                        on_event(Event::ReasoningDelta {
                            text,
                            meta: item["encrypted_content"]
                                .as_str()
                                .map(|enc| json!({"encrypted_content": enc})),
                        });
                    }
                }
                _ => {}
            }
        }
        "response.output_text.delta" => {
            if let Some(text) = ev["delta"].as_str()
                && !text.is_empty()
            {
                on_event(Event::Delta(text.to_string()));
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(text) = ev["delta"].as_str()
                && !text.is_empty()
            {
                on_event(Event::ReasoningDelta {
                    text: text.to_string(),
                    meta: None,
                });
            }
        }
        "response.reasoning_summary_part.done" => {
            on_event(Event::ReasoningDelta {
                text: "\n\n".to_string(),
                meta: None,
            });
        }
        "response.function_call_arguments.delta" => {
            on_event(Event::ToolCallDelta {
                index: ev["output_index"].as_u64().unwrap_or(0) as usize,
                id: None,
                name: None,
                fragment: ev["delta"].as_str().unwrap_or("").to_string(),
            });
        }
        "response.output_item.added" => {
            let item = &ev["item"];
            if item["type"].as_str() == Some("function_call") {
                on_event(Event::ToolCallDelta {
                    index: ev["output_index"].as_u64().unwrap_or(0) as usize,
                    id: item["call_id"].as_str().map(String::from),
                    name: item["name"].as_str().map(String::from),
                    fragment: String::new(),
                });
            }
        }
        "response.completed" | "response.incomplete" => {
            let resp = &ev["response"];
            if let Some(u) = resp["usage"].as_object() {
                let input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                let output = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                let cached = u["input_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                *usage = Some(Usage {
                    input,
                    output,
                    cached,
                    cached_write: 0,
                });
            }
            let status = resp["status"].as_str().unwrap_or("completed");
            let reason = resp["incomplete_details"]["reason"].as_str();
            if status == "incomplete" && reason != Some("interrupted") {
                *stop = map_stop(status, reason);
            } else if status == "completed" {
                *stop = StopReason::Stop;
            }
        }
        _ => {}
    }
}

/// Emit one complete (non-streaming) response. Returns usage.
pub(crate) fn feed_complete(value: &Value, on_event: &mut dyn FnMut(Event)) -> Option<Usage> {
    let mut usage = None;
    feed_event(
        "response.completed",
        value,
        &mut usage,
        &mut StopReason::default(),
        on_event,
    );
    usage
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
        headers.push(("session_id".to_string(), key.to_string()));
    }
    let body = build_body(m, input, stream)?.to_string();
    super::check_request_body(&body, input)?;
    super::dispatch(
        HttpRequest { url, headers, body },
        stream,
        |event_type, chunk, usage, stop, on_event| {
            feed_event(event_type, chunk, usage, stop, on_event)
        },
        |value, on_event| {
            feed_complete(value, on_event);
        },
        on_event,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::testutil::{att, input, tool_def};
    use serde_json::json;

    fn codex_model() -> ResolvedModel {
        ResolvedModel {
            provider_name: "chatgpt".into(),
            kind: "codex".into(),
            base_url: "https://chatgpt.com/backend-api".into(),
            api_key: None,
            model_id: "gpt-5.2-codex".into(),
            context_window: None,
            options: Vec::new(),
        }
    }

    #[test]
    fn history_becomes_response_items() {
        let history = vec![
            Msg::user("hi"),
            Msg::Assistant {
                text: "reading".into(),
                tool_calls: vec![crate::providers::ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: json!({"path": "x"}),
                }],
                reasoning: Some("thinking…".into()),
                reasoning_meta: None,
            },
            Msg::tool_result("call_1", "read", "contents"),
        ];
        let tools = [tool_def()];
        let mut i = input(&history, &tools);
        i.system = Some("be brief");
        let body = build_body(&codex_model(), &i, true).unwrap();
        assert_eq!(body["instructions"], json!("be brief"));
        let items = body["input"].as_array().unwrap();
        // user → reasoning → assistant message → function_call → output
        assert_eq!(items[0]["role"], json!("user"));
        assert_eq!(items[1]["type"], json!("reasoning"));
        assert_eq!(items[2]["type"], json!("message"));
        assert_eq!(items[2]["role"], json!("assistant"));
        assert_eq!(items[3]["type"], json!("function_call"));
        assert_eq!(items[3]["call_id"], json!("call_1"));
        assert_eq!(items[4]["type"], json!("function_call_output"));
        assert_eq!(items[4]["call_id"], json!("call_1"));
        assert_eq!(body["tools"][0]["type"], json!("function"));
        assert_eq!(body["store"], json!(false));
    }

    #[test]
    fn image_attachment_rides_input_image() {
        let mut i = input(&[], &[]);
        let atts = [att("image/png", Some("shot.png"))];
        i.attachments = &atts;
        let body = build_body(&codex_model(), &i, false).unwrap();
        let content = body["input"][0]["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], json!("input_image"));
        assert!(
            content[1]["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
    }

    #[test]
    fn completed_event_maps_usage_and_stop() {
        let ev = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "input_tokens_details": {"cached_tokens": 40}
                }
            }
        });
        let mut usage = None;
        let mut stop = StopReason::default();
        feed_event("", &ev, &mut usage, &mut stop, &mut |_| {});
        assert_eq!(
            usage.unwrap(),
            Usage {
                input: 100,
                output: 20,
                cached: 40,
                cached_write: 0
            }
        );
        assert_eq!(stop, StopReason::Stop);
    }

    #[test]
    fn text_delta_forwards() {
        let ev = json!({"type": "response.output_text.delta", "delta": "hello"});
        let mut got = Vec::new();
        feed_event("", &ev, &mut None, &mut StopReason::default(), &mut |e| {
            if let Event::Delta(t) = e {
                got.push(t);
            }
        });
        assert_eq!(got, vec!["hello".to_string()]);
    }
}
