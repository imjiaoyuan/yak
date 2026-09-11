//! Anthropic Messages API native adapter, with thinking-delta streaming.

use serde_json::{Value, json};

use super::{Attachment, Msg, PromptInput, ResolvedModel};
use crate::core::http::{Event, HttpRequest, StopReason, Usage};

/// Push the buffered run of tool results as one user turn (Anthropic groups
/// consecutive tool_result blocks into a single message).
fn flush_results(messages: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if !pending.is_empty() {
        messages.push(json!({"role": "user", "content": std::mem::take(pending)}));
    }
}

/// One content block for an attachment: images, PDFs and plain-text
/// documents are native; audio has no Anthropic wire form.
fn attachment_block(a: &Attachment) -> Result<Value, String> {
    let mime = a.mime_type.as_str();
    if mime.starts_with("image/") {
        Ok(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": mime, "data": a.base64_data}
        }))
    } else if mime == "application/pdf" {
        Ok(json!({
            "type": "document",
            "source": {"type": "base64", "media_type": "application/pdf", "data": a.base64_data}
        }))
    } else if mime == "text/plain" || mime == "text/csv" {
        let text = super::decoded_text(a)?;
        Ok(json!({
            "type": "document",
            "source": {"type": "text", "media_type": mime, "data": text}
        }))
    } else {
        Err(format!(
            "anthropic models take image, PDF and text attachments, not '{mime}'"
        ))
    }
}

/// Mark the last content block of the final message as a prompt-cache
/// breakpoint: every later round resends this exact prefix, so the next
/// request reads the whole conversation back instead of re-billing it.
/// Caching is opt-in on the Messages API; without a marker nothing caches.
fn mark_conversation_tip(messages: &mut [Value]) {
    let Some(last) = messages.last_mut() else {
        return;
    };
    let content = &mut last["content"];
    if content.is_string() {
        let text = content.as_str().unwrap_or("").to_string();
        *content = json!([{
            "type": "text",
            "text": text,
            "cache_control": {"type": "ephemeral"}
        }]);
    } else if let Some(blocks) = content.as_array_mut()
        && let Some(block) = blocks.last_mut()
    {
        block["cache_control"] = json!({"type": "ephemeral"});
    }
}

pub fn build_body(
    m: &ResolvedModel,
    input: &PromptInput<'_>,
    stream: bool,
) -> Result<Value, String> {
    // orphan pairing resolved from the borrowed history: no Vec<Msg> copy
    // of the whole conversation per round
    let last_result = super::last_result_index(input.history);
    let mut messages: Vec<Value> = Vec::new();
    let mut pending_results: Vec<Value> = Vec::new();
    for (i, msg) in input.history.iter().enumerate() {
        match msg {
            Msg::User { text, attachments } => {
                flush_results(&mut messages, &mut pending_results);
                messages.push(json!({
                    "role": "user",
                    "content": super::user_content(text, attachments, attachment_block)?
                }));
            }
            Msg::Assistant {
                text,
                tool_calls,
                reasoning: _,
            } => {
                flush_results(&mut messages, &mut pending_results);
                if tool_calls.is_empty() {
                    messages.push(json!({"role": "assistant", "content": text}));
                } else {
                    let mut blocks: Vec<Value> = Vec::new();
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                    for c in tool_calls {
                        blocks.push(json!({"type": "tool_use", "id": c.id, "name": c.name, "input": c.arguments}));
                    }
                    messages.push(json!({"role": "assistant", "content": blocks}));
                }
                for call in tool_calls {
                    if !super::call_answered(&last_result, &call.id, i) {
                        pending_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": call.id,
                            "content": super::ORPHAN_RESULT,
                            "is_error": true,
                        }));
                    }
                }
            }
            Msg::ToolResult {
                call_id,
                content,
                is_error,
                attachments,
                ..
            } => {
                let content = super::tool_result_content(
                    m.supports_images(),
                    content,
                    attachments,
                    attachment_block,
                )?;
                pending_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": content,
                    "is_error": is_error,
                }));
            }
            Msg::Summary { text } => {
                flush_results(&mut messages, &mut pending_results);
                messages.push(
                    json!({"role": "user", "content": format!("<summary>\n{text}\n</summary>")}),
                );
            }
        }
    }
    flush_results(&mut messages, &mut pending_results);
    // an empty prompt with no attachments means "continue after tool results"
    if !input.prompt.is_empty() || !input.attachments.is_empty() {
        messages.push(json!({
            "role": "user",
            "content": super::user_content(input.prompt, input.attachments, attachment_block)?
        }));
    }
    // breakpoint on the conversation tip (multi-turn pattern: the marker
    // rides forward one turn each request; earlier markers stay valid).
    // First-round-only requests skip it: a one-shot prompt is never resent,
    // so marking it would buy a cache write nothing ever reads
    if !input.history.is_empty() {
        mark_conversation_tip(&mut messages);
    }

    let mut body = json!({
        "model": m.model_id,
        "max_tokens": 8192,
        "messages": messages,
        "stream": stream,
    });
    if let Some(system) = input.system {
        // a breakpoint on the last system block caches tools+system together
        body["system"] = json!([{
            "type": "text",
            "text": system,
            "cache_control": {"type": "ephemeral"}
        }]);
    }
    if !input.tools.is_empty() {
        let mut tools: Vec<Value> = input
            .tools
            .iter()
            .map(|t| {
                json!({"name": t.name, "description": t.description, "input_schema": t.parameters})
            })
            .collect();
        // no system prompt to pin: the breakpoint falls to the last tool
        if input.system.is_none()
            && let Some(last) = tools.last_mut()
        {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
        body["tools"] = Value::Array(tools);
    }
    super::apply_options(&mut body, &m.options);
    // effort level → thinking budget; applied after the -o loop so a
    // hand-set max_tokens is respected unless the budget needs more room
    if let Some(level) = input.reasoning
        && let Some(budget) = super::thinking_budget(level)
    {
        body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        let max = body["max_tokens"].as_u64().unwrap_or(8192);
        if max <= budget {
            body["max_tokens"] = json!(budget + 4096);
        }
    }
    Ok(body)
}

fn map_stop(reason: &str) -> StopReason {
    match reason {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::Length,
        _ => StopReason::Stop,
    }
}

/// Merge the fields one usage event carries into the running total.
/// Anthropic splits usage across events: `message_start` seeds `input_tokens`
/// plus the cache read/write counts, `message_delta` later carries only
/// `output_tokens`. Fields absent from an event keep their previous value;
/// gateways that repeat everything in one event work the same way.
/// `input_tokens` excludes the cache read and write, but `Usage.input` means
/// the whole prompt (the openai-compat `prompt_tokens` semantics), so token
/// counts and the `cache N%` footer stay comparable across provider kinds.
fn apply_usage(u: &Value, usage: &mut Option<Usage>) {
    let read = u["cache_read_input_tokens"].as_u64();
    let write = u["cache_creation_input_tokens"].as_u64();
    let input = u["input_tokens"].as_u64().or(u["prompt_tokens"].as_u64());
    let output = u["output_tokens"]
        .as_u64()
        .or(u["completion_tokens"].as_u64());
    if input.is_none() && output.is_none() && read.is_none() && write.is_none() {
        return;
    }
    let mut cur = usage.take().unwrap_or(Usage {
        input: 0,
        output: 0,
        cached: 0,
    });
    if let Some(p) = input {
        cur.input = p + read.unwrap_or(0) + write.unwrap_or(0);
    }
    if let Some(r) = read {
        cur.cached = r;
    }
    if let Some(c) = output {
        cur.output = c;
    }
    *usage = Some(cur);
}

/// Feed one SSE event (type + parsed data) through the request state.
pub(crate) fn feed_event(
    event_type: &str,
    chunk: &Value,
    usage: &mut Option<Usage>,
    stop: &mut StopReason,
    on_event: &mut dyn FnMut(Event),
) {
    let index = || chunk["index"].as_u64().unwrap_or(0) as usize;
    match event_type {
        "content_block_start" => {
            if chunk["content_block"]["type"] == "tool_use" {
                on_event(Event::ToolCallDelta {
                    index: index(),
                    id: chunk["content_block"]["id"].as_str().map(str::to_string),
                    name: chunk["content_block"]["name"].as_str().map(str::to_string),
                    fragment: String::new(),
                });
            }
        }
        "content_block_delta" => {
            if let Some(text) = chunk["delta"]["text"].as_str() {
                on_event(Event::Delta(text.to_string()));
            }
            if let Some(text) = chunk["delta"]["thinking"].as_str() {
                on_event(Event::ReasoningDelta(text.to_string()));
            }
            if chunk["delta"]["type"] == "input_json_delta"
                && let Some(frag) = chunk["delta"]["partial_json"].as_str()
            {
                on_event(Event::ToolCallDelta {
                    index: index(),
                    name: None,
                    id: None,
                    fragment: frag.to_string(),
                });
            }
        }
        "message_start" => {
            let u = if chunk["message"]["usage"].is_object() {
                &chunk["message"]["usage"]
            } else {
                &chunk["usage"]
            };
            apply_usage(u, usage);
        }
        "message_delta" => {
            apply_usage(&chunk["usage"], usage);
            if let Some(reason) = chunk["delta"]["stop_reason"].as_str() {
                *stop = map_stop(reason);
            }
        }
        _ => {}
    }
}

/// Emit events for a complete (non-streaming) response. Returns usage.
pub(crate) fn feed_complete(value: &Value, on_event: &mut dyn FnMut(Event)) -> Option<Usage> {
    let mut usage = None;
    apply_usage(&value["usage"], &mut usage);
    for (i, block) in value["content"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .enumerate()
    {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str() {
                    on_event(Event::Delta(text.to_string()));
                }
            }
            Some("thinking") => {
                if let Some(text) = block["thinking"].as_str() {
                    on_event(Event::ReasoningDelta(text.to_string()));
                }
            }
            Some("tool_use") => {
                on_event(Event::ToolCallDelta {
                    index: i,
                    id: block["id"].as_str().map(str::to_string),
                    name: block["name"].as_str().map(str::to_string),
                    fragment: block["input"].to_string(),
                });
            }
            _ => {}
        }
    }
    let stop = value["stop_reason"]
        .as_str()
        .map(map_stop)
        .unwrap_or_default();
    on_event(Event::Done { usage, stop });
    usage
}

pub fn run(
    m: &ResolvedModel,
    input: &PromptInput,
    stream: bool,
    on_event: &mut dyn FnMut(Event),
) -> Result<(), String> {
    let url = format!("{}/v1/messages", m.base_url.trim_end_matches('/'));
    let mut headers = vec![("Content-Type".into(), "application/json".into())];
    if let Some(key) = &m.api_key {
        headers.extend(super::auth_headers(&m.kind, key));
    }
    super::dispatch(
        HttpRequest {
            url,
            headers,
            body: build_body(m, input, stream)?.to_string(),
        },
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
    use crate::providers::testutil::{att, input};
    use crate::providers::{ToolCall, ToolCallAccumulator, ToolDef};

    fn model(kind: &str) -> ResolvedModel {
        crate::providers::testutil::model(kind)
    }
    use serde_json::json;

    #[test]
    fn pdf_attachment_rides_a_document_block() {
        let mut i = input(&[], &[]);
        let atts = [att("application/pdf", Some("doc.pdf"))];
        i.attachments = &atts;
        let body = build_body(&model("anthropic"), &i, false).unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "document");
        assert_eq!(content[1]["source"]["media_type"], "application/pdf");
        assert_eq!(content[1]["source"]["data"], "AAAA");
    }

    #[test]
    fn audio_attachment_errors() {
        let mut i = input(&[], &[]);
        let atts = [att("audio/mpeg", None)];
        i.attachments = &atts;
        let err = build_body(&model("anthropic"), &i, false).unwrap_err();
        assert!(err.contains("audio/mpeg"), "{err}");
        assert!(err.contains("image, PDF and text"), "{err}");
    }

    #[test]
    fn text_attachment_rides_a_plain_text_document() {
        let mut i = input(&[], &[]);
        // base64 of "hello notes"
        let atts = [Attachment {
            mime_type: "text/plain".into(),
            base64_data: crate::b64::encode(b"hello notes"),
            filename: Some("notes.txt".into()),
            path: None,
            url: None,
        }];
        i.attachments = &atts;
        let body = build_body(&model("anthropic"), &i, false).unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "document");
        assert_eq!(content[1]["source"]["type"], "text");
        assert_eq!(content[1]["source"]["media_type"], "text/plain");
        assert_eq!(content[1]["source"]["data"], "hello notes");
    }

    #[test]
    fn non_utf8_text_attachment_errors() {
        let mut i = input(&[], &[]);
        let atts = [Attachment {
            mime_type: "text/csv".into(),
            base64_data: crate::b64::encode(&[0xff, 0xfe, 0x00]),
            filename: Some("rows.csv".into()),
            path: None,
            url: None,
        }];
        i.attachments = &atts;
        let err = build_body(&model("anthropic"), &i, false).unwrap_err();
        assert!(err.contains("UTF-8"), "{err}");
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_turn() {
        let history = vec![
            Msg::user("run both"),
            Msg::Assistant {
                text: "thinking...".into(),
                tool_calls: vec![
                    ToolCall {
                        id: "t1".into(),
                        name: "ls".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "t2".into(),
                        name: "read".into(),
                        arguments: json!({"path":"x"}),
                    },
                ],
                reasoning: None,
            },
            Msg::tool_result("t1", "ls", "a"),
            Msg::ToolResult {
                call_id: "t2".into(),
                name: "read".into(),
                content: "boom".into(),
                is_error: true,
                attachments: Vec::new(),
            },
        ];
        let body = build_body(&model("anthropic"), &input(&history, &[]), true).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        // user, assistant(text+2 tool_use), user(2 tool_result), user(prompt)
        assert_eq!(msgs.len(), 4);
        let blocks = msgs[1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "t1");
        assert_eq!(blocks[1]["input"], json!({}));
        let results = msgs[2]["content"].as_array().unwrap();
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["tool_use_id"], "t1");
        assert_eq!(results[1]["is_error"], true);
    }

    #[test]
    fn thinking_maps_to_budget_and_raises_max_tokens() {
        let mut i = input(&[], &[]);
        i.reasoning = Some("medium");
        let body = build_body(&model("anthropic"), &i, false).unwrap();
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 16384})
        );
        // default max_tokens (8192) must exceed the budget
        assert_eq!(body["max_tokens"], json!(16384 + 4096));

        // a user-set max_tokens above the budget is left alone
        let mut m = model("anthropic");
        m.options = vec![("max_tokens".to_string(), "90000".to_string())];
        let body = build_body(&m, &i, false).unwrap();
        assert_eq!(body["max_tokens"], json!(90000));

        // unset → no thinking block at all
        let body = build_body(&model("anthropic"), &input(&[], &[]), false).unwrap();
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn tools_wire_shape() {
        let tools = vec![ToolDef {
            name: "bash".into(),
            description: "run a command".into(),
            parameters: json!({"type":"object","properties":{"command":{"type":"string"}}}),
        }];
        let body = build_body(&model("anthropic"), &input(&[], &tools), false).unwrap();
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert!(body.get("tools").unwrap().as_array().unwrap().len() == 1);
    }

    #[test]
    fn system_breakpoint_pins_tools_and_system() {
        let tools = vec![ToolDef {
            name: "ls".into(),
            description: "list files".into(),
            parameters: json!({"type": "object"}),
        }];
        let history = [Msg::user("hi")];
        let mut i = input(&history, &tools);
        i.system = Some("be brief");
        let body = build_body(&model("anthropic"), &i, false).unwrap();
        let blocks = body["system"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "be brief");
        // the marker caches tools+system together; tools carry none of their own
        assert_eq!(blocks[0]["cache_control"], json!({"type": "ephemeral"}));
        assert!(body["tools"][0].get("cache_control").is_none());
    }

    #[test]
    fn no_system_moves_the_breakpoint_to_the_last_tool() {
        let tools = vec![
            ToolDef {
                name: "ls".into(),
                description: "list files".into(),
                parameters: json!({"type": "object"}),
            },
            ToolDef {
                name: "read".into(),
                description: "read a file".into(),
                parameters: json!({"type": "object"}),
            },
        ];
        let body = build_body(
            &model("anthropic"),
            &input(&[Msg::user("hi")], &tools),
            false,
        )
        .unwrap();
        assert!(body.get("system").is_none());
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(
            body["tools"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn conversation_tip_carries_the_breakpoint() {
        let history = vec![Msg::user("hi")];
        let body = build_body(&model("anthropic"), &input(&history, &[]), false).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        // earlier turns stay plain unmarked strings
        assert_eq!(msgs[0]["content"], json!("hi"));
        let tip = msgs[1]["content"].as_array().unwrap();
        assert_eq!(tip.len(), 1);
        assert_eq!(tip[0]["type"], "text");
        assert_eq!(tip[0]["text"], "go");
        assert_eq!(tip[0]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn tool_result_tip_carries_the_breakpoint() {
        let history = vec![
            Msg::user("run"),
            Msg::Assistant {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "t1".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "ls"}),
                }],
                reasoning: None,
            },
            Msg::tool_result("t1", "bash", "out"),
        ];
        let mut i = input(&history, &[]);
        i.prompt = ""; // continue after tool results
        let body = build_body(&model("anthropic"), &i, false).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        let tip = msgs.last().unwrap()["content"].as_array().unwrap();
        assert_eq!(tip.len(), 1);
        assert_eq!(tip[0]["type"], "tool_result");
        assert_eq!(tip[0]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn first_round_prompt_stays_unmarked() {
        // no history: a one-shot prompt is never resent, so a breakpoint on
        // it would pay the cache-write premium for a read that never comes
        let body = build_body(&model("anthropic"), &input(&[], &[]), false).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"], json!("go"));
    }

    #[test]
    fn streaming_usage_merges_message_start_and_delta() {
        // the documented split: input + cache counts in message_start, only
        // output_tokens in message_delta — both must land in one Usage
        let mut usage = None;
        let mut stop = StopReason::default();
        feed_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {"usage": {"input_tokens": 25, "cache_read_input_tokens": 500,
                                       "cache_creation_input_tokens": 40}}
            }),
            &mut usage,
            &mut stop,
            &mut |_| {},
        );
        assert!(usage.is_none() || usage.as_ref().unwrap().output == 0);
        feed_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 15}
            }),
            &mut usage,
            &mut stop,
            &mut |_| {},
        );
        let u = usage.expect("usage recorded");
        assert_eq!(u.input, 565);
        assert_eq!(u.cached, 500);
        assert_eq!(u.output, 15);
        assert_eq!(stop, StopReason::Stop);
    }

    #[test]
    fn streaming_usage_accepts_everything_in_message_delta() {
        // lenient gateways repeat the full usage object on message_delta
        let mut usage = None;
        let mut stop = StopReason::default();
        feed_event(
            "message_delta",
            &json!({"usage": {"input_tokens": 7, "output_tokens": 9}}),
            &mut usage,
            &mut stop,
            &mut |_| {},
        );
        let u = usage.expect("usage recorded");
        assert_eq!((u.input, u.output), (7, 9));
    }

    #[test]
    fn usage_folds_cache_tokens_into_input() {
        let value = json!({
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_read_input_tokens": 500,
                "cache_creation_input_tokens": 40
            },
            "content": []
        });
        let usage = feed_complete(&value, &mut |_| {}).unwrap();
        assert_eq!(usage.input, 640);
        assert_eq!(usage.cached, 500);
        assert_eq!(usage.output, 20);
        // the whole-prompt denominator keeps the share at or below 100%
        assert_eq!(usage.cache_percent(), 78);
    }

    #[test]
    fn feed_event_streams_tool_use_and_stop_reason() {
        let mut usage = None;
        let mut stop = StopReason::default();
        let mut acc = ToolCallAccumulator::default();
        let mut feed = |event_type: &str, chunk: &Value| {
            feed_event(event_type, chunk, &mut usage, &mut stop, &mut |e| {
                if let Event::ToolCallDelta {
                    index,
                    name,
                    id,
                    fragment,
                } = e
                {
                    acc.push(index, id.as_deref(), name.as_deref(), &fragment);
                }
            });
        };
        feed(
            "content_block_start",
            &json!({
                "index": 1,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read", "input": {}}
            }),
        );
        feed(
            "content_block_delta",
            &json!({
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": "{\"pa"}
            }),
        );
        feed(
            "content_block_delta",
            &json!({
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": "th\":\"a\"}"}
            }),
        );
        feed(
            "message_delta",
            &json!({
                "delta": {"stop_reason": "tool_use"},
                "usage": {"input_tokens": 7, "output_tokens": 8}
            }),
        );

        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(
            usage,
            Some(Usage {
                input: 7,
                output: 8,
                cached: 0
            })
        );
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].arguments, json!({"path": "a"}));
    }

    #[test]
    fn feed_complete_handles_tool_use_blocks() {
        let value = json!({
            "content": [
                {"type": "text", "text": "doing it"},
                {"type": "tool_use", "id": "t9", "name": "bash", "input": {"command": "ls"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let mut acc = ToolCallAccumulator::default();
        let mut stop_seen = None;
        let usage = feed_complete(&value, &mut |e| match e {
            Event::ToolCallDelta {
                index,
                name,
                id,
                fragment,
            } => acc.push(index, id.as_deref(), name.as_deref(), &fragment),
            Event::Done { stop, .. } => stop_seen = Some(stop),
            _ => {}
        });
        assert_eq!(
            usage,
            Some(Usage {
                input: 1,
                output: 2,
                cached: 0
            })
        );
        assert_eq!(stop_seen, Some(StopReason::ToolUse));
        assert_eq!(acc.finish()[0].arguments, json!({"command": "ls"}));
    }
}
