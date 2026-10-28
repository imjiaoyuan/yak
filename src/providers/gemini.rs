//! Native Gemini GenerateContent transport. Signed response parts are kept
//! in reasoning_meta and replayed in their original order and boundaries.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use super::{Attachment, Msg, PromptInput, ResolvedModel};
use crate::core::attachments::{Kind, kind_of};
use crate::core::http::{Event, HttpRequest, StopReason, Usage};

fn attachment_part(a: &Attachment) -> Result<Value, String> {
    if a.base64_data.is_empty() {
        return Ok(
            json!({"text":format!("[{}: attached earlier in this conversation, bytes not available]", super::reference(a))}),
        );
    }
    match kind_of(&a.mime_type) {
        Some(Kind::Text) => Ok(
            json!({"text":format!("{}\n{}", a.filename.as_deref().unwrap_or("attachment"), super::decoded_text(a)?)}),
        ),
        Some(Kind::Image | Kind::Pdf | Kind::Audio(_)) => {
            Ok(json!({"inlineData":{"mimeType":a.mime_type, "data":a.base64_data}}))
        }
        _ => Err(format!(
            "Gemini does not support attachment type '{}'",
            a.mime_type
        )),
    }
}

fn user_parts(
    m: &ResolvedModel,
    text: &str,
    attachments: &[Attachment],
) -> Result<Vec<Value>, String> {
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(json!({"text":text}));
    }
    for attachment in attachments {
        if kind_of(&attachment.mime_type) == Some(Kind::Image) && !m.supports_images() {
            parts.push(json!({"text":format!("[{}: image omitted: current model does not support images]", super::reference(attachment))}));
        } else {
            parts.push(attachment_part(attachment)?);
        }
    }
    Ok(parts)
}

fn push_content(contents: &mut Vec<Value>, role: &str, mut parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut().filter(|c| c["role"] == role) {
        last["parts"]
            .as_array_mut()
            .expect("content parts")
            .append(&mut parts);
    } else {
        contents.push(json!({"role":role, "parts":parts}));
    }
}

fn result_part(name: &str, id: Option<&str>, content: &str, is_error: bool) -> Value {
    let mut response = json!({"name":name, "response":if is_error {json!({"error":content})} else {json!({"output":content})}});
    if let Some(id) = id {
        response["id"] = json!(id);
    }
    json!({"functionResponse":response})
}

pub fn build_body(m: &ResolvedModel, input: &PromptInput<'_>) -> Result<Value, String> {
    let last_result = super::last_result_index(input.history);
    let mut contents = Vec::new();
    // Local ids may be synthesized when a model supplies none. Only replay
    // an upstream id in functionResponse if its functionCall carried one.
    let mut call_ids = BTreeMap::<String, Option<String>>::new();
    for (index, message) in input.history.iter().enumerate() {
        match message {
            Msg::User { text, attachments } => {
                push_content(&mut contents, "user", user_parts(m, text, attachments)?)
            }
            Msg::Summary { text } => push_content(
                &mut contents,
                "user",
                vec![json!({"text":format!("<summary>\n{text}\n</summary>")})],
            ),
            Msg::Assistant {
                text,
                tool_calls,
                reasoning_meta,
                ..
            } => {
                let raw = reasoning_meta
                    .as_ref()
                    .filter(|meta| meta["gemini_model"] == m.model_id)
                    .and_then(|meta| meta["gemini_parts"].as_array());
                let parts = if let Some(raw) = raw {
                    let native_calls = raw.iter().filter_map(|part| part.get("functionCall"));
                    for (call, native) in tool_calls.iter().zip(native_calls) {
                        call_ids.insert(call.id.clone(), native["id"].as_str().map(String::from));
                    }
                    raw.clone()
                } else {
                    let mut parts = Vec::new();
                    if !text.is_empty() {
                        parts.push(json!({"text":text}));
                    }
                    for call in tool_calls {
                        call_ids.insert(call.id.clone(), Some(call.id.clone()));
                        parts.push(json!({"functionCall":{"id":call.id, "name":call.name, "args":call.arguments}, "thoughtSignature":"skip_thought_signature_validator"}));
                    }
                    parts
                };
                push_content(&mut contents, "model", parts);
                let missing = tool_calls
                    .iter()
                    .filter(|call| !super::call_answered(&last_result, &call.id, index))
                    .map(|call| {
                        result_part(
                            &call.name,
                            call_ids.get(&call.id).and_then(|id| id.as_deref()),
                            super::ORPHAN_RESULT,
                            true,
                        )
                    })
                    .collect();
                push_content(&mut contents, "user", missing);
            }
            Msg::ToolResult {
                call_id,
                name,
                content,
                error,
                attachments,
            } => {
                let mut parts = vec![result_part(
                    name,
                    call_ids.get(call_id).and_then(|id| id.as_deref()),
                    content,
                    error.is_some(),
                )];
                parts.extend(user_parts(m, "", attachments)?);
                push_content(&mut contents, "user", parts);
            }
        }
    }
    push_content(
        &mut contents,
        "user",
        user_parts(m, input.prompt, input.attachments)?,
    );
    let mut body = json!({"contents":contents});
    if let Some(system) = input.system {
        body["systemInstruction"] = json!({"parts":[{"text":system}]});
    }
    if !input.tools.is_empty() {
        body["tools"] = json!([{"functionDeclarations":input.tools.iter().map(|tool| json!({"name":tool.name, "description":tool.description, "parametersJsonSchema":tool.parameters})).collect::<Vec<_>>()}]);
    }
    let mut generation = json!({"candidateCount":1});
    if let Some(effort) = input.reasoning {
        let id = m.model_id.trim_start_matches("models/");
        let thinking = if id.starts_with("gemini-2.5") {
            json!({"thinkingBudget":super::thinking_budget(effort).ok_or_else(|| format!("unsupported Gemini thinking level '{effort}'"))?, "includeThoughts":true})
        } else {
            json!({"thinkingLevel":effort, "includeThoughts":true})
        };
        generation["thinkingConfig"] = thinking;
    }
    body["generationConfig"] = generation;
    // Gemini's tuning parameters live inside generationConfig. Protocol
    // objects remain top-level; an explicit generationConfig replaces ours.
    for (key, raw) in &m.options {
        if super::CLIENT_ONLY_OPTIONS.contains(&key.as_str()) {
            continue;
        }
        let value = serde_json::from_str(raw).unwrap_or_else(|_| json!(raw));
        match key.as_str() {
            "generationConfig" | "safetySettings" | "tools" | "toolConfig" | "cachedContent"
            | "systemInstruction" => {
                body[key] = value;
            }
            _ => {
                if !body["generationConfig"].is_object() {
                    return Err("Gemini generationConfig must be an object".into());
                }
                body["generationConfig"][key] = value;
            }
        }
    }
    if !body["generationConfig"].is_object() {
        return Err("Gemini generationConfig must be an object".into());
    }
    if body["generationConfig"]
        .get("candidateCount")
        .is_some_and(|v| v != 1)
    {
        return Err("Gemini requires candidateCount=1 for the agent loop".into());
    }
    Ok(body)
}

#[derive(Default)]
struct Decoder {
    parts: Vec<Value>,
    ids: BTreeSet<String>,
    calls: usize,
    has_text: bool,
    finish: Option<StopReason>,
    usage: Option<Usage>,
    output: u64,
    thoughts: u64,
}

impl Decoder {
    fn feed(
        &mut self,
        model: &str,
        chunk: &Value,
        sink: &mut dyn FnMut(Event),
    ) -> Result<(), String> {
        if let Some(reason) = chunk["promptFeedback"]["blockReason"]
            .as_str()
            .filter(|r| *r != "BLOCK_REASON_UNSPECIFIED")
        {
            return Err(format!("Gemini blocked the prompt: {reason}"));
        }
        if let Some(usage) = chunk.get("usageMetadata").filter(|v| v.is_object()) {
            let current = self.usage.get_or_insert_default();
            if let Some(n) = usage["promptTokenCount"].as_u64() {
                current.input = n;
            }
            if let Some(n) = usage["cachedContentTokenCount"].as_u64() {
                current.cached = n;
            }
            if let Some(n) = usage["candidatesTokenCount"].as_u64() {
                self.output = n;
            }
            if let Some(n) = usage["thoughtsTokenCount"].as_u64() {
                self.thoughts = n;
            }
            current.output = self.output + self.thoughts;
        }
        let candidates = chunk["candidates"].as_array();
        if candidates.is_some_and(|c| c.len() > 1) {
            return Err("Gemini returned multiple candidates".into());
        }
        let Some(candidate) = candidates.and_then(|c| c.first()) else {
            return Ok(());
        };
        if candidate["index"].as_u64().is_some_and(|index| index != 0) {
            return Err("Gemini returned an unexpected candidate index".into());
        }
        if let Some(parts) = candidate["content"]["parts"].as_array() {
            if self.finish.is_some() && !parts.is_empty() {
                return Err("Gemini sent content after finishReason".into());
            }
            for part in parts {
                if let Some(text) = part["text"].as_str().filter(|text| !text.is_empty()) {
                    if part["thought"] == true {
                        sink(Event::ReasoningDelta {
                            text: text.into(),
                            meta: None,
                        });
                    } else {
                        self.has_text = true;
                        sink(Event::Delta(text.into()));
                    }
                } else if let Some(call) = part.get("functionCall") {
                    let name = call["name"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .ok_or("Gemini function call has no name")?;
                    if call.get("partialArgs").is_some() {
                        return Err(
                            "Gemini incremental function arguments are not supported".into()
                        );
                    }
                    let arguments = call.get("args").cloned().unwrap_or_else(|| json!({}));
                    if !arguments.is_object() {
                        return Err("Gemini function arguments must be an object".into());
                    }
                    let id = call["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map(String::from)
                        .unwrap_or_else(|| format!("call_{}", crate::core::db::ulid()));
                    if !self.ids.insert(id.clone()) {
                        return Err("Gemini repeated a function call id".into());
                    }
                    sink(Event::ToolCallDelta {
                        index: self.calls,
                        name: Some(name.into()),
                        id: Some(id),
                        fragment: arguments.to_string(),
                    });
                    self.calls += 1;
                } else if part.get("inlineData").is_some() || part.get("fileData").is_some() {
                    return Err("Gemini returned unsupported non-text output".into());
                }
                self.parts.push(part.clone());
            }
            if !parts.is_empty() {
                sink(Event::ReasoningDelta {
                    text: String::new(),
                    meta: Some(json!({"gemini_model":model, "gemini_parts":self.parts})),
                });
            }
        }
        if let Some(reason) = candidate["finishReason"]
            .as_str()
            .filter(|r| *r != "FINISH_REASON_UNSPECIFIED")
        {
            self.finish = Some(match reason {
                "STOP" => {
                    if self.calls > 0 {
                        StopReason::ToolUse
                    } else {
                        StopReason::Stop
                    }
                }
                "MAX_TOKENS" => StopReason::Length,
                _ => return Err(format!("Gemini generation stopped: {reason}")),
            });
        }
        Ok(())
    }

    fn done(&self, sink: &mut dyn FnMut(Event)) -> Result<(), String> {
        let stop = self.finish.ok_or(super::TRUNCATED_STREAM)?;
        if stop != StopReason::Length && !self.has_text && self.calls == 0 {
            return Err("Gemini completed without answer text or tool calls".into());
        }
        sink(Event::Done {
            usage: self.usage,
            stop,
        });
        Ok(())
    }
}

pub fn run(
    m: &ResolvedModel,
    input: &PromptInput,
    stream: bool,
    sink: &mut dyn FnMut(Event),
) -> Result<(), String> {
    let model = m.model_id.trim_start_matches("models/");
    if model.is_empty()
        || !model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!("invalid Gemini model id '{}'", m.model_id));
    }
    let base = m.base_url.trim_end_matches('/');
    let method = if stream {
        "streamGenerateContent?alt=sse"
    } else {
        "generateContent"
    };
    let url = format!("{base}/models/{model}:{method}");
    let mut headers = vec![("Content-Type".into(), "application/json".into())];
    if let Some(key) = m.api_key.as_deref().filter(|key| !key.is_empty()) {
        headers.extend(super::auth_headers("gemini", key));
    }
    headers.extend(super::gateway_headers(&url));
    let body = build_body(m, input)?.to_string();
    super::check_request_body(&body, input)?;
    let request = HttpRequest { url, headers, body };
    let mut decoder = Decoder::default();
    if stream {
        let handed = AtomicBool::new(false);
        let mut error = None;
        let result = super::stream_events_with_terminal(
            &request,
            &handed,
            |_, chunk| {
                chunk["candidates"]
                    .get(0)
                    .and_then(|c| c["finishReason"].as_str())
                    .filter(|r| *r != "FINISH_REASON_UNSPECIFIED")
                    .is_some()
            },
            |_, chunk| {
                if error.is_none() {
                    let result = decoder.feed(&m.model_id, chunk, &mut |event| {
                        handed.store(true, Ordering::Relaxed);
                        sink(event);
                    });
                    if let Err(e) = result {
                        error = Some(e);
                    }
                }
            },
        );
        if let Some(error) = error {
            return Err(error);
        }
        result?;
    } else {
        decoder.feed(&m.model_id, &super::complete_json(&request)?, sink)?;
    }
    decoder.done(sink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::testutil::{att, input, model, tool_def};

    #[test]
    fn signed_parts_and_parallel_calls_replay_without_rearrangement() {
        let m = model("gemini");
        let parts = json!([
            {"text":"inspect", "thought":true, "thoughtSignature":"thinking-signature"},
            {"functionCall":{"name":"read", "args":{"path":"a"}}, "thoughtSignature":"call-signature"},
            {"functionCall":{"id":"upstream-2", "name":"read", "args":{"path":"b"}}}
        ]);
        let mut decoder = Decoder::default();
        let mut accumulator = super::super::ToolCallAccumulator::default();
        let mut meta = None;
        decoder
            .feed(
                &m.model_id,
                &json!({"candidates":[{"content":{"parts":parts}, "finishReason":"STOP"}]}),
                &mut |event| match event {
                    Event::ReasoningDelta {
                        meta: Some(value), ..
                    } => meta = Some(value),
                    Event::ToolCallDelta {
                        index,
                        name,
                        id,
                        fragment,
                    } => accumulator.push(index, id.as_deref(), name.as_deref(), &fragment),
                    _ => {}
                },
            )
            .unwrap();
        assert_eq!(decoder.finish, Some(StopReason::ToolUse));
        let calls = accumulator.finish();
        assert_eq!(calls.len(), 2);
        let history = [
            Msg::Assistant {
                text: String::new(),
                reasoning: Some("inspect".into()),
                reasoning_meta: meta,
                tool_calls: calls.clone(),
            },
            Msg::tool_result(&calls[0].id, "read", "a contents"),
            Msg::tool_result(&calls[1].id, "read", "b contents"),
        ];
        let mut request = input(&history, &[]);
        request.prompt = "";
        let body = build_body(&m, &request).unwrap();
        assert_eq!(body["contents"][0]["parts"], parts);
        assert_eq!(body["contents"][1]["parts"].as_array().unwrap().len(), 2);
        assert!(
            body["contents"][1]["parts"][0]["functionResponse"]
                .get("id")
                .is_none()
        );
        assert_eq!(
            body["contents"][1]["parts"][1]["functionResponse"]["id"],
            "upstream-2"
        );
    }

    #[test]
    fn signature_only_stream_parts_are_preserved() {
        let mut decoder = Decoder::default();
        let mut meta = None;
        for chunk in [
            json!({"candidates":[{"content":{"parts":[{"text":"answer"}]}}]}),
            json!({"candidates":[{"content":{"parts":[{"text":"", "thoughtSignature":"tail-signature"}]}, "finishReason":"STOP"}]}),
        ] {
            decoder
                .feed("m", &chunk, &mut |event| {
                    if let Event::ReasoningDelta {
                        meta: Some(value), ..
                    } = event
                    {
                        meta = Some(value)
                    }
                })
                .unwrap();
        }
        assert_eq!(
            meta.unwrap()["gemini_parts"][1]["thoughtSignature"],
            "tail-signature"
        );
        decoder.done(&mut |_| {}).unwrap();
    }

    #[test]
    fn gemini_usage_merges_cache_and_thinking_without_double_counting() {
        let mut decoder = Decoder::default();
        decoder.feed("m", &json!({"usageMetadata":{"promptTokenCount":120, "cachedContentTokenCount":80, "thoughtsTokenCount":9}}), &mut |_| {}).unwrap();
        decoder
            .feed(
                "m",
                &json!({"usageMetadata":{"promptTokenCount":120, "candidatesTokenCount":11}}),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(
            decoder.usage,
            Some(Usage {
                input: 120,
                output: 20,
                cached: 80,
                cached_write: 0
            })
        );
        assert_eq!(
            decoder.done(&mut |_| {}),
            Err(super::super::TRUNCATED_STREAM.into())
        );
    }

    #[test]
    fn blocked_empty_and_malformed_gemini_replies_fail() {
        for chunk in [
            json!({"promptFeedback":{"blockReason":"SAFETY"}}),
            json!({"candidates":[{"finishReason":"MALFORMED_FUNCTION_CALL"}]}),
            json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"read", "args":"broken"}}]}}]}),
            json!({"candidates":[{}, {}]}),
        ] {
            assert!(
                Decoder::default().feed("m", &chunk, &mut |_| {}).is_err(),
                "{chunk}"
            );
        }
        let mut decoder = Decoder::default();
        decoder
            .feed(
                "m",
                &json!({"candidates":[{"finishReason":"STOP"}]}),
                &mut |_| {},
            )
            .unwrap();
        assert!(
            decoder
                .done(&mut |_| {})
                .unwrap_err()
                .contains("without answer")
        );
    }

    #[test]
    fn gemini_output_limit_does_not_authorize_tool_execution() {
        let mut decoder = Decoder::default();
        decoder.feed("m", &json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"read", "args":{}}}]}, "finishReason":"MAX_TOKENS"}]}), &mut |_| {}).unwrap();
        assert_eq!(decoder.finish, Some(StopReason::Length));
    }

    #[test]
    fn gemini_sse_requires_finish_reason_even_with_usage_or_done() {
        use std::io::Write;
        for tail in ["", "data: [DONE]\n\n"] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let body = format!(
                "data: {{\"usageMetadata\":{{\"promptTokenCount\":10}},\"candidates\":[{{\"finishReason\":\"FINISH_REASON_UNSPECIFIED\"}}]}}\n\n{tail}"
            );
            let server = std::thread::spawn(move || {
                let (mut connection, _) = listener.accept().unwrap();
                crate::core::testutil::read_request(&mut connection);
                write!(connection, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            });
            let mut m = model("gemini");
            m.base_url = format!("http://{address}");
            let mut done = false;
            let result = run(&m, &input(&[], &[]), true, &mut |event| {
                done |= matches!(event, Event::Done { .. })
            });
            server.join().unwrap();
            assert_eq!(result, Err(super::super::TRUNCATED_STREAM.into()));
            assert!(!done);
        }
    }

    #[test]
    fn foreign_tool_history_has_explicit_signature_bypass_and_orphan_result() {
        let m = model("gemini");
        let history = [Msg::Assistant {
            text: String::new(),
            reasoning: None,
            reasoning_meta: None,
            tool_calls: vec![super::super::ToolCall {
                id: "call_foreign".into(),
                name: "read".into(),
                arguments: json!({}),
            }],
        }];
        let body = build_body(&m, &input(&history, &[])).unwrap();
        assert_eq!(
            body["contents"][0]["parts"][0]["thoughtSignature"],
            "skip_thought_signature_validator"
        );
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["response"]["error"],
            super::super::ORPHAN_RESULT
        );
    }

    #[test]
    fn gemini_tools_attachments_system_and_options_use_native_fields() {
        let mut m = model("gemini");
        m.options = vec![
            ("temperature".into(), "0.2".into()),
            ("maxOutputTokens".into(), "2048".into()),
        ];
        let tools = [tool_def()];
        let attachments = [
            att("image/png", None),
            att("application/pdf", None),
            att("audio/wav", None),
        ];
        let mut request = input(&[], &tools);
        request.system = Some("system");
        request.reasoning = Some("low");
        request.attachments = &attachments;
        let body = build_body(&m, &request).unwrap();
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "system");
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
            tools[0].parameters
        );
        assert_eq!(
            body["contents"][0]["parts"][2]["inlineData"]["mimeType"],
            "application/pdf"
        );
        assert_eq!(body["generationConfig"]["temperature"], 0.2);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "low"
        );
        m.options = vec![("generationConfig".into(), "5".into())];
        assert!(build_body(&m, &request).is_err());
    }
}
