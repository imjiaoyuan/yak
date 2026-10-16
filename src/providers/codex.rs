//! The ChatGPT subscription's Codex channel adapter (the Responses API):
//! kind `codex`. Endpoint `chatgpt.com/backend-api/codex/responses`, the
//! ChatGPT OAuth token as Bearer, the account id in `chatgpt-account-id`.
//! The message shape is the Responses item stream (role message /
//! function_call / function_call_output / reasoning); the system prompt
//! rides the top-level `instructions`. Streamed item deltas are reconciled
//! with final snapshots before the next round is built.

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

fn user_content(
    supports_images: bool,
    text: &str,
    attachments: &[Attachment],
) -> Result<Value, String> {
    if !supports_images && !attachments.is_empty() {
        let note = super::user_content(false, text, attachments, attachment_part)?;
        return Ok(json!([{ "type": "input_text", "text": note }]));
    }
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
    _stream: bool,
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
                    "content": user_content(m.supports_images(), text, attachments)?
                }));
            }
            Msg::Assistant {
                text,
                tool_calls,
                reasoning: _,
                reasoning_meta,
            } => {
                flush_images(&mut items, &mut pending_images);
                // Encrypted reasoning is replayable even without a visible
                // summary. Keep every item's id and opaque payload together.
                if let Some(meta) = reasoning_meta {
                    if let Some(items_meta) = meta["items"].as_array() {
                        items.extend(items_meta.iter().cloned());
                    } else if meta.get("encrypted_content").is_some() {
                        let mut item = meta.clone();
                        item["type"] = json!("reasoning");
                        item["summary"] = json!([]);
                        items.push(item);
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
                    let (call_id, item_id) = call
                        .id
                        .split_once('|')
                        .map_or((call.id.as_str(), None), |(call, item)| (call, Some(item)));
                    let mut item = json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": call.name,
                        "arguments": call.arguments.to_string(),
                    });
                    if let Some(id) = item_id.filter(|id| id.starts_with("fc_")) {
                        item["id"] = json!(id);
                    }
                    items.push(item);
                    if !super::call_answered(&last_result, &call.id, i) {
                        items.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
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
                    "call_id": call_id.split('|').next().unwrap_or(call_id),
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
            "content": user_content(m.supports_images(), input.prompt, input.attachments)?,
        }));
    }

    let mut body = json!({
        "model": m.model_id,
        "input": items,
        "stream": true,
        "store": false,
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "text": {"verbosity": "low"},
        "instructions": input.system.unwrap_or("You are a helpful assistant."),
    });
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
                        "strict": null,
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

/// Responses sends each item as deltas and then repeats it in item/response
/// snapshots. Track each output index so every byte and call is emitted once.
#[derive(Default)]
struct ResponseStream {
    items: std::collections::BTreeMap<usize, OutputItem>,
    reasoning_items: std::collections::BTreeMap<usize, Value>,
}

#[derive(Default)]
struct OutputItem {
    text: std::collections::BTreeMap<usize, String>,
    reasoning: std::collections::BTreeMap<usize, String>,
    arguments: String,
    call: Option<(String, String)>,
    done: bool,
    raw_reasoning: bool,
}

/// A final snapshot may supply missing deltas, but cannot rewrite bytes we
/// already delivered to the agent (especially executable tool arguments).
fn snapshot_tail<'a>(seen: &str, snapshot: &'a str) -> Result<&'a str, String> {
    snapshot
        .strip_prefix(seen)
        .ok_or_else(|| "Codex response snapshot disagrees with streamed content".to_string())
}

impl ResponseStream {
    fn text(
        &mut self,
        index: usize,
        part: usize,
        text: &str,
        snapshot: bool,
        reasoning: bool,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<(), String> {
        let item = self.items.entry(index).or_default();
        let seen = if reasoning {
            &mut item.reasoning
        } else {
            &mut item.text
        }
        .entry(part)
        .or_default();
        let tail = if snapshot {
            snapshot_tail(seen, text)?
        } else {
            text
        };
        if !tail.is_empty() {
            // Separate summary parts once, instead of replaying the complete
            // summary at output_item.done after streaming the same text.
            let separator = reasoning && part > 0 && seen.is_empty();
            seen.push_str(tail);
            if reasoning {
                on_event(Event::ReasoningDelta {
                    text: if separator {
                        format!("\n\n{tail}")
                    } else {
                        tail.into()
                    },
                    meta: None,
                });
            } else {
                on_event(Event::Delta(tail.into()));
            }
        }
        Ok(())
    }

    fn arguments(
        &mut self,
        index: usize,
        text: &str,
        snapshot: bool,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<(), String> {
        let item = self.items.entry(index).or_default();
        let tail = if snapshot {
            snapshot_tail(&item.arguments, text)?
        } else {
            text
        };
        if !tail.is_empty() {
            item.arguments.push_str(tail);
            on_event(Event::ToolCallDelta {
                index,
                id: None,
                name: None,
                fragment: tail.into(),
            });
        }
        Ok(())
    }

    fn item(
        &mut self,
        index: usize,
        item: &Value,
        done: bool,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<(), String> {
        if self.items.get(&index).is_some_and(|i| i.done)
            && item["type"].as_str() != Some("reasoning")
        {
            return Ok(());
        }
        match item["type"].as_str() {
            Some("function_call") => {
                let id = item["call_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or("Codex function call has no call_id")?;
                let name = item["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or("Codex function call has no name")?;
                let state = self.items.entry(index).or_default();
                if let Some((old_id, old_name)) = &state.call {
                    if old_id != id || old_name != name {
                        return Err("Codex function call identity changed during streaming".into());
                    }
                } else {
                    state.call = Some((id.into(), name.into()));
                    on_event(Event::ToolCallDelta {
                        index,
                        id: Some(
                            item["id"].as_str().map_or_else(
                                || id.to_string(),
                                |item_id| format!("{id}|{item_id}"),
                            ),
                        ),
                        name: Some(name.into()),
                        fragment: String::new(),
                    });
                }
                if let Some(arguments) = item["arguments"].as_str() {
                    self.arguments(index, arguments, true, on_event)?;
                }
            }
            Some("message") if done => {
                if let Some(parts) = item["content"].as_array() {
                    for (part, value) in parts.iter().enumerate() {
                        let text = match value["type"].as_str() {
                            Some("output_text") => value["text"].as_str(),
                            Some("refusal") => value["refusal"].as_str(),
                            _ => None,
                        };
                        if let Some(text) = text {
                            self.text(index, part, text, true, false, on_event)?;
                        }
                    }
                }
            }
            Some("reasoning") if done => {
                if !self.items.get(&index).is_some_and(|i| i.raw_reasoning)
                    && let Some(parts) = item["summary"].as_array()
                {
                    for (part, value) in parts.iter().enumerate() {
                        if let Some(text) = value["text"].as_str() {
                            self.text(index, part, text, true, true, on_event)?;
                        }
                    }
                }
                if item["encrypted_content"].is_string() {
                    let meta = item.clone();
                    if self.reasoning_items.get(&index) == Some(&meta) {
                        return Ok(());
                    }
                    self.reasoning_items.insert(index, meta);
                    on_event(Event::ReasoningDelta {
                        text: String::new(),
                        meta: Some(
                            json!({"items": self.reasoning_items.values().collect::<Vec<_>>()}),
                        ),
                    });
                }
            }
            Some("custom_tool_call") => {
                return Err("Codex returned an unsupported custom tool call".into());
            }
            _ => {}
        }
        if done {
            self.items.entry(index).or_default().done = true;
        }
        Ok(())
    }

    fn feed(
        &mut self,
        event_type: &str,
        ev: &Value,
        usage: &mut Option<Usage>,
        stop: &mut StopReason,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<(), String> {
        let kind = ev["type"].as_str().unwrap_or(event_type);
        let indexed = kind.starts_with("response.output_item.")
            || kind.starts_with("response.function_call_arguments.")
            || matches!(
                kind,
                "response.output_text.delta"
                    | "response.refusal.delta"
                    | "response.reasoning_summary_text.delta"
                    | "response.reasoning_text.delta"
            );
        let index = if indexed {
            ev["output_index"]
                .as_u64()
                .ok_or_else(|| format!("Codex event '{kind}' has no output_index"))?
                as usize
        } else {
            0
        };
        match kind {
            "response.output_item.added" | "response.output_item.done" => {
                self.item(index, &ev["item"], kind.ends_with(".done"), on_event)?;
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                self.text(
                    index,
                    ev["content_index"].as_u64().unwrap_or(0) as usize,
                    ev["delta"].as_str().unwrap_or(""),
                    false,
                    false,
                    on_event,
                )?;
            }
            "response.reasoning_summary_text.delta" => {
                self.text(
                    index,
                    ev["summary_index"].as_u64().unwrap_or(0) as usize,
                    ev["delta"].as_str().unwrap_or(""),
                    false,
                    true,
                    on_event,
                )?;
            }
            "response.reasoning_text.delta" => {
                self.items.entry(index).or_default().raw_reasoning = true;
                self.text(
                    index,
                    0,
                    ev["delta"].as_str().unwrap_or(""),
                    false,
                    true,
                    on_event,
                )?;
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                let snapshot = kind.ends_with(".done");
                self.arguments(
                    index,
                    ev[if snapshot { "arguments" } else { "delta" }]
                        .as_str()
                        .unwrap_or(""),
                    snapshot,
                    on_event,
                )?;
            }
            "response.done" | "response.completed" | "response.incomplete" => {
                let resp = &ev["response"];
                if let Some(output) = resp["output"].as_array() {
                    for (index, item) in output.iter().enumerate() {
                        self.item(index, item, true, on_event)?;
                    }
                }
                if let Some(u) = resp.get("usage").filter(|u| u.is_object()) {
                    *usage = Some(Usage {
                        input: u["input_tokens"].as_u64().unwrap_or(0),
                        output: u["output_tokens"].as_u64().unwrap_or(0),
                        cached: u["input_tokens_details"]["cached_tokens"]
                            .as_u64()
                            .unwrap_or(0),
                        cached_write: u["input_tokens_details"]["cache_write_tokens"]
                            .as_u64()
                            .unwrap_or(0),
                    });
                }
                let status = resp["status"]
                    .as_str()
                    .unwrap_or(if kind == "response.incomplete" {
                        "incomplete"
                    } else {
                        "completed"
                    });
                if status == "incomplete" {
                    let reason = resp["incomplete_details"]["reason"].as_str();
                    if reason != Some("max_output_tokens") {
                        return Err(format!(
                            "Codex response incomplete: {}",
                            reason.unwrap_or("unknown reason")
                        ));
                    }
                    *stop = StopReason::Length;
                    return Ok(());
                }
                if status != "completed" {
                    return Err(format!("Codex response ended with status '{status}'"));
                }
                let mut has_calls = false;
                let mut has_text = false;
                for item in self.items.values() {
                    has_text |= item.text.values().any(|s| !s.is_empty());
                    if item.call.is_some() {
                        if !item.done {
                            return Err(
                                "Codex response completed with an unfinished tool call".into()
                            );
                        }
                        let args: Value = serde_json::from_str(&item.arguments)
                            .map_err(|e| format!("invalid Codex function call arguments: {e}"))?;
                        if !args.is_object() {
                            return Err(
                                "Codex function call arguments must be a JSON object".into()
                            );
                        }
                        has_calls = true;
                    } else if !item.arguments.is_empty() {
                        return Err(
                            "Codex function call arguments have no matching call identity".into(),
                        );
                    }
                }
                if !has_text && !has_calls {
                    return Err("Codex response completed without answer text or tool calls".into());
                }
                *stop = if has_calls {
                    StopReason::ToolUse
                } else {
                    StopReason::Stop
                };
            }
            _ => {}
        }
        Ok(())
    }
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
    let body = build_body(m, input, stream)?.to_string();
    super::check_request_body(&body, input)?;
    let mut decoder = ResponseStream::default();
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
                reasoning_meta: Some(json!({"id": "rs_1", "encrypted_content": "opaque"})),
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
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "ok"}]}],
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "input_tokens_details": {"cached_tokens": 40}
                }
            }
        });
        let mut usage = None;
        let mut stop = StopReason::default();
        ResponseStream::default()
            .feed("", &ev, &mut usage, &mut stop, &mut |_| {})
            .unwrap();
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
    fn completed_tool_call_round_keeps_tool_use_stop() {
        let mut usage = None;
        let mut stop = StopReason::default();
        let mut calls = super::super::ToolCallAccumulator::default();
        let mut decoder = ResponseStream::default();
        for ev in [
            json!({"type": "response.output_item.added", "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "read"}}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                "delta": "{\"path\":\"README.md\"}"}),
            json!({"type": "response.output_item.done", "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "read",
                    "arguments": "{\"path\":\"README.md\"}"}}),
            json!({"type": "response.completed", "response": {"status": "completed"}}),
        ] {
            decoder
                .feed("", &ev, &mut usage, &mut stop, &mut |event| {
                    if let Event::ToolCallDelta {
                        index,
                        id,
                        name,
                        fragment,
                    } = event
                    {
                        calls.push(index, id.as_deref(), name.as_deref(), &fragment);
                    }
                })
                .unwrap();
        }
        assert_eq!(stop, StopReason::ToolUse);
        let calls = calls.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].arguments, json!({"path": "README.md"}));
    }

    #[test]
    fn incomplete_tool_call_round_stops_at_output_limit() {
        let mut usage = None;
        let mut stop = StopReason::ToolUse;
        ResponseStream::default()
            .feed(
                "",
                &json!({"type": "response.incomplete", "response": {
                    "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}
                }}),
                &mut usage,
                &mut stop,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(stop, StopReason::Length);
    }

    #[test]
    fn text_delta_forwards() {
        let ev = json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hello"});
        let mut got = Vec::new();
        ResponseStream::default()
            .feed("", &ev, &mut None, &mut StopReason::default(), &mut |e| {
                if let Event::Delta(t) = e {
                    got.push(t);
                }
            })
            .unwrap();
        assert_eq!(got, vec!["hello".to_string()]);
    }
    type Decoded = (
        String,
        String,
        Option<Value>,
        Vec<crate::providers::ToolCall>,
        StopReason,
        Option<Usage>,
    );

    fn decode(events: &[Value]) -> Result<Decoded, String> {
        let mut decoder = ResponseStream::default();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut meta = None;
        let mut calls = crate::providers::ToolCallAccumulator::default();
        let mut stop = StopReason::default();
        let mut usage = None;
        for event in events {
            decoder.feed("", event, &mut usage, &mut stop, &mut |event| match event {
                Event::Delta(delta) => text.push_str(&delta),
                Event::ReasoningDelta { text, meta: m } => {
                    reasoning.push_str(&text);
                    if m.is_some() {
                        meta = m;
                    }
                }
                Event::ToolCallDelta {
                    index,
                    id,
                    name,
                    fragment,
                } => calls.push(index, id.as_deref(), name.as_deref(), &fragment),
                _ => {}
            })?;
        }
        Ok((text, reasoning, meta, calls.finish(), stop, usage))
    }

    #[test]
    fn message_snapshots_do_not_repeat_streamed_text_or_refusals() {
        let item = json!({"type": "message", "content": [
            {"type": "output_text", "text": "你好"}, {"type": "refusal", "refusal": "no"}]});
        let (text, _, _, _, stop, usage) = decode(&[
            json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": "你"}),
            json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": "好"}),
            json!({"type": "response.refusal.delta", "output_index": 0, "content_index": 1, "delta": "no"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
            json!({"type": "response.done", "response": {"status": "completed", "output": [item],
                "usage": {"input_tokens": 10, "output_tokens": 4}}}),
        ]).unwrap();
        assert_eq!(text, "你好no");
        assert_eq!(stop, StopReason::Stop);
        assert_eq!(usage.unwrap().cached, 0);
    }

    #[test]
    fn final_response_can_supply_missing_text_and_tool_deltas() {
        let (text, _, _, calls, stop, _) = decode(&[
            json!({"type": "response.completed", "response": {"output": [
                {"type": "message", "content": [{"type": "output_text", "text": "reading"}]},
                {"type": "function_call", "call_id": "c1", "name": "read", "arguments": "{\"path\":\"a\"}"},
                {"type": "function_call", "call_id": "c2", "name": "read", "arguments": "{\"path\":\"b\"}"}
            ]}}),
        ]).unwrap();
        assert_eq!(text, "reading");
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments, json!({"path": "a"}));
        assert_eq!(calls[1].arguments, json!({"path": "b"}));
    }

    #[test]
    fn reasoning_snapshots_do_not_repeat_summaries_and_backfill_encryption() {
        let item = json!({"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "inspect"}]});
        let mut encrypted = item.clone();
        encrypted["encrypted_content"] = json!("opaque");
        let (_, reasoning, meta, _, _, _) = decode(&[
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "inspect"}),
            json!({"type": "response.reasoning_summary_part.done", "output_index": 0}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
            json!({"type": "response.completed", "response": {"output": [encrypted,
                {"type": "message", "content": [{"type": "output_text", "text": "ok"}]}]}}),
        ]).unwrap();
        assert_eq!(reasoning, "inspect");
        assert_eq!(meta.unwrap()["items"][0], encrypted);
    }

    #[test]
    fn hidden_reasoning_items_survive_replay_without_visible_summary() {
        let items = json!([
            {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "one"},
            {"type": "reasoning", "id": "rs_2", "summary": [], "encrypted_content": "two"},
        ]);
        let mut output = items.as_array().unwrap().clone();
        output.push(json!({"type": "message", "content": [{"type": "output_text", "text": "ok"}]}));
        let (_, reasoning, meta, _, _, _) =
            decode(&[json!({"type": "response.completed", "response": {"output": output}})])
                .unwrap();
        assert!(reasoning.is_empty());
        let history = [Msg::Assistant {
            text: "ok".into(),
            tool_calls: vec![],
            reasoning: None,
            reasoning_meta: meta,
        }];
        let body = build_body(&codex_model(), &input(&history, &[]), true).unwrap();
        assert_eq!(body["input"][0], items[0]);
        assert_eq!(body["input"][1], items[1]);
    }

    #[test]
    fn completed_but_unfinished_tool_call_is_refused() {
        let result = decode(&[
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "call_id": "c", "name": "read"}}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{}"}),
            json!({"type": "response.completed", "response": {"status": "completed"}}),
        ]);
        assert!(result.err().unwrap().contains("unfinished tool call"));
    }

    #[test]
    fn malformed_tool_arguments_never_become_an_empty_object() {
        let result = decode(&[
            json!({"type": "response.completed", "response": {"output": [
                {"type": "function_call", "call_id": "c", "name": "read", "arguments": "{\"path\":"}
            ]}}),
        ]);
        assert!(
            result
                .err()
                .unwrap()
                .contains("invalid Codex function call arguments")
        );
    }

    #[test]
    fn tool_events_require_an_output_index() {
        let result = decode(&[
            json!({"type": "response.output_item.added", "item": {"type": "function_call", "call_id": "c", "name": "read"}}),
        ]);
        assert!(result.err().unwrap().contains("no output_index"));
    }

    #[test]
    fn reasoning_only_completion_and_interruption_fail_loudly() {
        assert!(
            decode(&[
                json!({"type": "response.completed", "response": {"output": [
                    {"type": "reasoning", "summary": [], "encrypted_content": "opaque"}
                ]}})
            ])
            .err()
            .unwrap()
            .contains("without answer text or tool calls")
        );
        assert!(decode(&[json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "interrupted"}}})]).err().unwrap().contains("interrupted"));
    }

    #[test]
    fn conflicting_argument_snapshots_are_refused() {
        let result = decode(&[
            json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"path\":\"a\"}"}),
            json!({"type": "response.function_call_arguments.done", "output_index": 0, "arguments": "{\"path\":\"b\"}"}),
        ]);
        assert!(result.err().unwrap().contains("disagrees"));
    }

    #[test]
    fn optional_tool_parameters_remain_optional_and_transport_always_streams() {
        let tools = [tool_def()];
        let body = build_body(&codex_model(), &input(&[], &tools), false).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0].get("strict"), Some(&Value::Null));
        assert_eq!(body["instructions"], "You are a helpful assistant.");
    }
    #[test]
    fn history_images_use_responses_content_types_on_every_round() {
        let history = [Msg::User {
            text: "look".into(),
            attachments: vec![att("image/png", Some("shot.png"))],
        }];
        let body = build_body(&codex_model(), &input(&history, &[]), true).unwrap();
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
    }

    #[test]
    fn function_item_id_and_call_id_are_preserved_in_their_wire_fields() {
        let (_, _, _, calls, _, _) = decode(&[json!({"type": "response.completed", "response": {"output": [
            {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": "{}"}
        ]}})]).unwrap();
        assert_eq!(calls[0].id, "call_1|fc_1");
        let history = [
            Msg::Assistant {
                text: String::new(),
                tool_calls: calls,
                reasoning: None,
                reasoning_meta: None,
            },
            Msg::tool_result("call_1|fc_1", "read", "contents"),
        ];
        let mut request = input(&history, &[]);
        request.prompt = "";
        let body = build_body(&codex_model(), &request, true).unwrap();
        assert_eq!(body["input"][0]["id"], "fc_1");
        assert_eq!(body["input"][0]["call_id"], "call_1");
        assert_eq!(body["input"][1]["call_id"], "call_1");
        assert_eq!(body["input"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn multiple_reasoning_parts_have_one_separator_and_no_snapshot_duplicates() {
        let (_, reasoning, _, _, _, _) = decode(&[
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "summary_index": 0, "delta": "first"}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "summary_index": 1, "delta": "second"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "reasoning", "summary": [{"text": "first"}, {"text": "second"}]}}),
            json!({"type": "response.completed", "response": {"output": [
                {"type": "reasoning", "summary": [{"text": "first"}, {"text": "second"}]},
                {"type": "message", "content": [{"type": "output_text", "text": "ok"}]}
            ]}}),
        ]).unwrap();
        assert_eq!(reasoning, "first\n\nsecond");
    }
}
