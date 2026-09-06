//! OpenAI-compatible chat/completions adapter (OpenAI, DeepSeek, Ollama,
//! OpenRouter, Gemini's compat endpoint, ...).

use serde_json::{Value, json};

use super::{Attachment, Msg, PromptInput, ResolvedModel};
use crate::core::http::{Event, HttpRequest, StopReason, Usage};

/// One content-part block for an attachment, by mime: images ride
/// `image_url` data URIs, PDFs `file` blocks, wav/mp3 `input_audio`.
fn attachment_block(a: &Attachment) -> Result<Value, String> {
    let mime = a.mime_type.as_str();
    if mime.starts_with("image/") {
        Ok(json!({
            "type": "image_url",
            "image_url": {"url": format!("data:{mime};base64,{}", a.base64_data)}
        }))
    } else if mime == "application/pdf" {
        let mut file = json!({
            "file_data": format!("data:application/pdf;base64,{}", a.base64_data)
        });
        if let Some(name) = &a.filename {
            file["filename"] = json!(name);
        }
        Ok(json!({"type": "file", "file": file}))
    } else if mime.starts_with("text/") {
        // no file block for plain text on this wire form: the decoded body
        // rides as an extra text part, headed by its file name
        let text = super::decoded_text(a)?;
        let header = match &a.filename {
            Some(name) => format!("{name}\n"),
            None => String::new(),
        };
        Ok(json!({"type": "text", "text": format!("{header}{text}")}))
    } else if let Some(format) = audio_format(mime) {
        Ok(json!({
            "type": "input_audio",
            "input_audio": {"data": a.base64_data, "format": format}
        }))
    } else {
        Err(format!(
            "openai-compat models take image, PDF, text and wav/mp3 attachments, not '{mime}'"
        ))
    }
}

/// The `format` field of an `input_audio` block; only wav and mp3 exist.
fn audio_format(mime: &str) -> Option<&'static str> {
    match mime {
        "audio/wav" | "audio/wave" | "audio/x-wav" => Some("wav"),
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        _ => None,
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
    if let Some(system) = input.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    for (i, msg) in input.history.iter().enumerate() {
        match msg {
            Msg::User { text, attachments } => {
                messages.push(json!({"role": "user", "content": super::user_content(text, attachments, attachment_block)?}));
            }
            Msg::Assistant { text, tool_calls } => {
                if tool_calls.is_empty() {
                    messages.push(json!({"role": "assistant", "content": text}));
                } else {
                    let calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": {"name": c.name, "arguments": c.arguments.to_string()}
                            })
                        })
                        .collect();
                    let content = if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text)
                    };
                    messages.push(
                        json!({"role": "assistant", "content": content, "tool_calls": calls}),
                    );
                }
                for call in tool_calls {
                    if !super::call_answered(&last_result, &call.id, i) {
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call.id,
                            "content": super::ORPHAN_RESULT,
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
                // a text-only model never sees an image it would reject
                let content = super::tool_result_content(
                    m.supports_images(),
                    content,
                    attachments,
                    attachment_block,
                )?;
                messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": content}));
            }
            Msg::Summary { text } => {
                messages.push(
                    json!({"role": "user", "content": format!("<summary>\n{text}\n</summary>")}),
                );
            }
        }
    }
    // an empty prompt with no attachments means "continue after tool results";
    // don't append an empty user message
    if !input.prompt.is_empty() || !input.attachments.is_empty() {
        messages.push(json!({
            "role": "user",
            "content": super::user_content(input.prompt, input.attachments, attachment_block)?
        }));
    }

    let mut body = json!({
        "model": m.model_id,
        "messages": messages,
        "stream": stream,
    });
    if stream {
        body["stream_options"] = json!({"include_usage": true});
    }
    if let Some(schema) = &m.schema {
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": {"name": "output", "schema": schema},
        });
    }
    if !input.tools.is_empty() {
        body["tools"] = Value::Array(
            input.tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {"name": t.name, "description": t.description, "parameters": t.parameters}
                    })
                })
                .collect(),
        );
    } else if messages.iter().any(|m| m["role"] == "tool") {
        // proxies fronting Anthropic reject tool-result history without the
        // key — checked on the serialized messages so a synthetic orphan
        // result counts too
        body["tools"] = json!([]);
    }
    // reasoning effort: set before the -o loop so an explicit
    // -o reasoning_effort=... still overrides it
    if let Some(effort) = input.reasoning {
        body["reasoning_effort"] = json!(effort);
    }
    // apply -o options; json values pass through, others are sent as strings
    // (OpenAI accepts numbers-as-numbers; we try numeric parsing first)
    super::apply_options(&mut body, &m.options);
    Ok(body)
}

fn map_stop(reason: &str) -> StopReason {
    match reason {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::Length,
        _ => StopReason::Stop,
    }
}

/// Feed one streaming data-chunk (already parsed) through the request state.
pub(crate) fn feed_chunk(
    chunk: &Value,
    usage: &mut Option<Usage>,
    stop: &mut StopReason,
    on_event: &mut dyn FnMut(Event),
) {
    if let (Some(p), Some(c)) = (
        chunk["usage"]["prompt_tokens"].as_u64(),
        chunk["usage"]["completion_tokens"].as_u64(),
    ) {
        *usage = Some(Usage {
            input: p,
            output: c,
            cached: super::cache_hit_tokens(&chunk["usage"]),
        });
    }
    let Some(choice) = chunk["choices"].get(0) else {
        return;
    };
    if let Some(reason) = choice["finish_reason"].as_str() {
        *stop = map_stop(reason);
    }
    let delta = &choice["delta"];
    if let Some(text) = delta["content"].as_str()
        && !text.is_empty()
    {
        on_event(Event::Delta(text.to_string()));
    }
    // DeepSeek reasoner / OpenRouter style reasoning
    let reasoning = delta["reasoning_content"]
        .as_str()
        .or_else(|| delta["reasoning"].as_str());
    if let Some(text) = reasoning
        && !text.is_empty()
    {
        on_event(Event::ReasoningDelta(text.to_string()));
    }
    if let Some(calls) = delta["tool_calls"].as_array() {
        for (i, call) in calls.iter().enumerate() {
            on_event(Event::ToolCallDelta {
                index: call["index"].as_u64().unwrap_or(i as u64) as usize,
                id: call["id"].as_str().map(str::to_string),
                name: call["function"]["name"].as_str().map(str::to_string),
                fragment: call["function"]["arguments"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
}

/// Emit events for a complete (non-streaming) response. Returns usage.
pub(crate) fn feed_complete(value: &Value, on_event: &mut dyn FnMut(Event)) -> Option<Usage> {
    let usage = match (
        value["usage"]["prompt_tokens"].as_u64(),
        value["usage"]["completion_tokens"].as_u64(),
    ) {
        (Some(p), Some(c)) => Some(Usage {
            input: p,
            output: c,
            cached: super::cache_hit_tokens(&value["usage"]),
        }),
        _ => None,
    };
    let message = value["choices"].get(0).cloned().unwrap_or(Value::Null);
    if let Some(text) = message["message"]["content"].as_str() {
        on_event(Event::Delta(text.to_string()));
    }
    if let Some(text) = message["message"]["reasoning_content"].as_str() {
        on_event(Event::ReasoningDelta(text.to_string()));
    }
    if let Some(calls) = message["message"]["tool_calls"].as_array() {
        for (i, call) in calls.iter().enumerate() {
            if call["function"]["name"].as_str().is_some() {
                on_event(Event::ToolCallDelta {
                    index: i,
                    id: call["id"].as_str().map(str::to_string),
                    name: call["function"]["name"].as_str().map(str::to_string),
                    fragment: call["function"]["arguments"]
                        .as_str()
                        .unwrap_or("{}")
                        .to_string(),
                });
            }
        }
    }
    let stop = message["finish_reason"]
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
    let url = format!("{}/chat/completions", m.base_url.trim_end_matches('/'));
    let mut headers = vec![("Content-Type".into(), "application/json".into())];
    if let Some(key) = &m.api_key {
        headers.push(super::auth_header(&m.kind, key));
    }
    super::dispatch(
        HttpRequest {
            url,
            headers,
            body: build_body(m, input, stream)?.to_string(),
        },
        stream,
        |_event_type, chunk, usage, stop, on_event| feed_chunk(chunk, usage, stop, on_event),
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
    use crate::providers::{ToolCall, ToolCallAccumulator};

    fn model(kind: &str) -> ResolvedModel {
        crate::providers::testutil::model(kind)
    }
    use serde_json::json;

    #[test]
    fn pdf_attachment_rides_a_file_block() {
        let mut i = input(&[], &[]);
        let atts = [att("application/pdf", Some("doc.pdf"))];
        i.attachments = &atts;
        let body = build_body(&model("openai-compat"), &i, false).unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "file");
        assert_eq!(content[1]["file"]["filename"], "doc.pdf");
        assert_eq!(
            content[1]["file"]["file_data"],
            "data:application/pdf;base64,AAAA"
        );
    }

    #[test]
    fn audio_attachment_rides_input_audio() {
        let mut i = input(&[], &[]);
        let atts = [att("audio/mpeg", None)];
        i.attachments = &atts;
        let body = build_body(&model("openai-compat"), &i, false).unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "input_audio");
        assert_eq!(content[1]["input_audio"]["format"], "mp3");
    }

    #[test]
    fn history_attachments_reserialize() {
        let history = vec![Msg::user_with(
            "look",
            vec![att("image/png", Some("shot.png"))],
        )];
        let body = build_body(&model("openai-compat"), &input(&history, &[]), false).unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn tool_result_with_image_attachment_serializes_as_parts() {
        let history = vec![
            Msg::assistant("read the image"),
            Msg::ToolResult {
                call_id: "c1".into(),
                name: "read".into(),
                content: "Read image file [image/png]".into(),
                is_error: false,
                attachments: vec![att("image/png", Some("shot.png"))],
            },
        ];
        let body = build_body(&model("openai-compat"), &input(&history, &[]), false).unwrap();
        // the assistant precedes, so the only tool message is index 1
        let content = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn text_only_model_omits_image_and_notes_it() {
        let mut m = model("openai-compat");
        m.model_id = "deepseek-chat".into();
        let history = vec![Msg::ToolResult {
            call_id: "c1".into(),
            name: "read".into(),
            content: "Read image file".into(),
            is_error: false,
            attachments: vec![att("image/png", Some("shot.png"))],
        }];
        let body = build_body(&m, &input(&history, &[]), false).unwrap();
        assert_eq!(
            body["messages"][0]["content"],
            "Read image file\n[image omitted: current model does not support images]"
        );
    }

    #[test]
    fn unsupported_attachment_mime_errors() {
        let mut i = input(&[], &[]);
        let atts = [att("application/zip", None)];
        i.attachments = &atts;
        let err = build_body(&model("openai-compat"), &i, false).unwrap_err();
        assert!(err.contains("application/zip"), "{err}");
        assert!(err.contains("image, PDF, text and wav/mp3"), "{err}");
    }

    #[test]
    fn text_attachment_rides_an_extra_text_part() {
        let mut i = input(&[], &[]);
        let atts = [Attachment {
            mime_type: "text/plain".into(),
            base64_data: crate::b64::encode(b"see inside"),
            filename: Some("notes.md".into()),
            path: None,
            url: None,
        }];
        i.attachments = &atts;
        let body = build_body(&model("openai-compat"), &i, false).unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "go");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "notes.md\nsee inside");
    }

    #[test]
    fn plain_history_serializes_as_before() {
        let body = build_body(
            &model("openai-compat"),
            &input(&[Msg::user("hi"), Msg::assistant("ho")], &[]),
            true,
        )
        .unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0], json!({"role":"user","content":"hi"}));
        assert_eq!(msgs[1], json!({"role":"assistant","content":"ho"}));
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn tool_history_and_tools_wire_shapes() {
        let history = vec![
            Msg::user("list files"),
            Msg::Assistant {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "ls".into(),
                    arguments: json!({"path": "."}),
                }],
            },
            Msg::tool_result("call_1", "ls", "a\nb"),
        ];
        let body = build_body(
            &model("openai-compat"),
            &input(&history, &[tool_def()]),
            false,
        )
        .unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], Value::Null);
        assert_eq!(msgs[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "ls");
        assert_eq!(
            msgs[1]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"."}"#
        );
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "call_1");
        assert_eq!(msgs[2]["content"], "a\nb");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn reasoning_effort_sent_when_set() {
        let mut i = input(&[], &[]);
        i.reasoning = Some("high");
        let body = build_body(&model("openai-compat"), &i, true).unwrap();
        assert_eq!(body["reasoning_effort"], json!("high"));
        // unset → the parameter is absent, byte-compatible with before
        let body = build_body(&model("openai-compat"), &input(&[], &[]), true).unwrap();
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn empty_tools_array_when_history_has_calls() {
        let history = vec![Msg::Assistant {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "ls".into(),
                arguments: json!({}),
            }],
        }];
        let body = build_body(&model("openai-compat"), &input(&history, &[]), true).unwrap();
        assert_eq!(body["tools"], json!([]));
    }

    #[test]
    fn orphaned_call_gets_a_synthetic_tool_result() {
        // an unpaired call gains a synthetic error result right after its
        // assistant message; a paired one gains nothing
        let orphan = vec![Msg::Assistant {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "ls".into(),
                arguments: json!({}),
            }],
        }];
        let body = build_body(
            &model("openai-compat"),
            &input(&orphan, &[tool_def()]),
            false,
        )
        .unwrap();
        let msgs = body["messages"].as_array().unwrap();
        // assistant, its synthetic result, then the "go" prompt
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "c1");
        assert_eq!(msgs[1]["content"], "No result provided");

        let paired = vec![
            Msg::Assistant {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "ls".into(),
                    arguments: json!({}),
                }],
            },
            Msg::tool_result("c1", "ls", "a\nb"),
        ];
        let body = build_body(
            &model("openai-compat"),
            &input(&paired, &[tool_def()]),
            false,
        )
        .unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "no synthetic injected for a paired call");
        assert_eq!(msgs[1]["content"], "a\nb");
    }

    #[test]
    fn feed_chunk_accumulates_tool_call_fragments() {
        let mut usage = None;
        let mut stop = StopReason::default();
        let mut acc = ToolCallAccumulator::default();
        let mut text = String::new();
        let mut feed = |chunk: &Value| {
            feed_chunk(chunk, &mut usage, &mut stop, &mut |e| match e {
                Event::Delta(t) => text.push_str(&t),
                Event::ToolCallDelta {
                    index,
                    name,
                    id,
                    fragment,
                } => acc.push(index, id.as_deref(), name.as_deref(), &fragment),
                _ => {}
            });
        };
        feed(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_9","function":{"name":"read","arguments":"{\"pa"}}
        ]}}]}));
        feed(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"th\":\"x\"}"}}
        ]}}]}));
        feed(
            &json!({"choices":[{"finish_reason":"tool_calls","delta":{}}],
                     "usage":{"prompt_tokens":10,"completion_tokens":5}}),
        );

        assert_eq!(text, "");
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(
            usage,
            Some(Usage {
                input: 10,
                output: 5,
                cached: 0
            })
        );
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_9");
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].arguments, json!({"path":"x"}));
    }

    #[test]
    fn feed_complete_emits_full_tool_calls() {
        let value = json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {"content": null, "tool_calls": [
                    {"id":"c1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}
                ]}
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4}
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
                input: 3,
                output: 4,
                cached: 0
            })
        );
        assert_eq!(stop_seen, Some(StopReason::ToolUse));
        let calls = acc.finish();
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments, json!({"command":"ls"}));
    }

    #[test]
    fn cache_hit_tokens_parse_from_deepseek_and_openai_shapes() {
        // DeepSeek reports the cache hit at the top level of usage
        let mut usage = None;
        feed_chunk(
            &json!({"usage": {"prompt_tokens": 100, "completion_tokens": 5,
                              "prompt_cache_hit_tokens": 90, "prompt_cache_miss_tokens": 10}}),
            &mut usage,
            &mut StopReason::default(),
            &mut |_| {},
        );
        assert_eq!(usage.unwrap().cached, 90);
        // OpenAI nests it under prompt_tokens_details
        let mut usage = None;
        feed_chunk(
            &json!({"usage": {"prompt_tokens": 100, "completion_tokens": 5,
                              "prompt_tokens_details": {"cached_tokens": 40}}}),
            &mut usage,
            &mut StopReason::default(),
            &mut |_| {},
        );
        assert_eq!(usage.unwrap().cached, 40);
        // OpenRouter reports it as a bare top-level cached_tokens
        let mut usage = None;
        feed_chunk(
            &json!({"usage": {"prompt_tokens": 100, "completion_tokens": 5,
                              "cached_tokens": 60}}),
            &mut usage,
            &mut StopReason::default(),
            &mut |_| {},
        );
        assert_eq!(usage.unwrap().cached, 60);
    }

    #[test]
    fn usage_cache_percent_is_safe_at_zero() {
        assert_eq!(Usage::default().cache_percent(), 0);
        assert_eq!(
            Usage {
                input: 200,
                output: 0,
                cached: 150
            }
            .cache_percent(),
            75
        );
    }
}
