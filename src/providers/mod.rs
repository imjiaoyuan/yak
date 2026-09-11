//! Unified provider adapter layer.

pub mod anthropic;
pub mod catalog;
pub mod message;
pub mod openai_compat;

pub use message::{
    Msg, ORPHAN_RESULT, ToolCall, ToolCallAccumulator, ToolDef, call_answered, last_result_index,
};

// the attachment type lives with its loader in core; re-exported here so
// the adapters' `Attachment` vocabulary keeps working
pub use crate::core::attachments::Attachment;

use serde_json::{Value, json};

use crate::core::config::Provider;
use crate::core::http::{self, Event, HttpRequest, StopReason, Usage};

// the conversation model: unified messages both provider adapters serialize

/// Auth header pair for a provider kind: `x-api-key` for anthropic,
/// `Authorization: Bearer` for openai-compat.
pub fn auth_header(kind: &str, key: &str) -> (String, String) {
    if kind == "anthropic" {
        ("x-api-key".to_string(), key.to_string())
    } else {
        ("Authorization".to_string(), format!("Bearer {key}"))
    }
}

/// The full auth header set for a provider kind: anthropic also needs its
/// protocol version header (the /models fetch and the messages API both
/// reject requests without it).
pub fn auth_headers(kind: &str, key: &str) -> Vec<(String, String)> {
    if kind == "anthropic" {
        vec![
            auth_header(kind, key),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
        ]
    } else {
        vec![auth_header(kind, key)]
    }
}

/// Prompt-cache hit tokens from an openai-compat usage object, in whatever
/// shape the gateway reports: DeepSeek `prompt_cache_hit_tokens`, OpenAI
/// `prompt_tokens_details.cached_tokens`, OpenRouter top-level `cached_tokens`.
pub(crate) fn cache_hit_tokens(usage: &Value) -> u64 {
    usage["prompt_cache_hit_tokens"]
        .as_u64()
        .or(usage["prompt_tokens_details"]["cached_tokens"].as_u64())
        .or(usage["cached_tokens"].as_u64())
        .unwrap_or(0)
}

/// The model a command run resolves to — the one shared chain for prompt,
/// agent and chat: `-m` > `LLM_MODEL` > `context` (a template's pinned
/// model or the conversation's last model) > the stored default. A
/// dangling default warns and drops out instead of erroring. Saved
/// per-model options ride under CLI `-o` pairs.
pub fn resolve_model_by_id(query: &str) -> Result<ResolvedModel, String> {
    use crate::core::config;
    let cfg = config::load();
    let (name, provider, model_id) = match cfg.resolve_model(query) {
        Ok(Some(v)) => v,
        Ok(None) => {
            return Err(format!(
                "Invalid model: {query}. Add it to {} or check spelling.",
                config::config_path().display()
            ));
        }
        Err(e) => return Err(e),
    };
    let api_key = cfg.api_key(provider);
    let mut model = ResolvedModel::from_config(&name, provider, &model_id, api_key);
    let qualified = model.qualified_id();
    let saved = config::load_model_options();
    model.options = saved
        .get(&qualified)
        .or_else(|| saved.get(&model_id))
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    Ok(model)
}

pub fn resolve_run_model(
    args: &crate::core::args::ParsedArgs,
    context: Option<String>,
) -> Result<ResolvedModel, String> {
    use crate::core::config;
    let cfg = config::load();
    let stored_default = config::default_model().filter(|m| {
        let resolves = cfg.resolve_model(m).ok().flatten().is_some();
        if !resolves {
            eprintln!("Warning: models.default '{m}' does not resolve, ignoring it");
        }
        resolves
    });
    let query = args
        .opt(&["model"])
        .map(str::to_string)
        .or_else(|| std::env::var("LLM_MODEL").ok())
        .or(context)
        .or(stored_default);
    let Some(query) = query else {
        return Err(
            "No default model configured. Run `llm login` in the REPL (or edit config.json), or use -m."
                .to_string(),
        );
    };
    let (name, provider, model_id) = match cfg.resolve_model(&query) {
        Ok(Some(v)) => v,
        Ok(None) => {
            return Err(format!(
                "Invalid model: {query}. Add it to {} or check spelling.",
                config::config_path().display()
            ));
        }
        Err(e) => return Err(e),
    };
    let api_key = args
        .opt(&["key"])
        .map(str::to_string)
        .or_else(|| cfg.api_key(provider));
    let mut model = ResolvedModel::from_config(&name, provider, &model_id, api_key);
    let qualified = model.qualified_id();
    let mut options: Vec<(String, String)> = Vec::new();
    let saved = config::load_model_options();
    for (k, v) in saved
        .get(&qualified)
        .or_else(|| saved.get(&model_id))
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
    {
        if !options.iter().any(|(existing, _)| *existing == k) {
            options.push((k, v));
        }
    }
    for (k, v) in crate::core::text::parse_kv(&args.multi(&["option"]))? {
        options.retain(|(existing, _)| existing != &k);
        options.push((k, v));
    }
    model.options = options;
    Ok(model)
}

/// A user message body: plain text, or content parts when attachments
/// ride. The per-wire block builder is passed in — the two adapters differ
/// only in `attachment_block`.
pub(crate) fn user_content(
    text: &str,
    attachments: &[Attachment],
    attachment_block: fn(&Attachment) -> Result<Value, String>,
) -> Result<Value, String> {
    if attachments.is_empty() {
        return Ok(json!(text));
    }
    let mut content = vec![json!({"type": "text", "text": text})];
    for a in attachments {
        content.push(attachment_block(a)?);
    }
    Ok(Value::Array(content))
}

/// Tool-result content: full parts when the model takes images, otherwise
/// the text with a note that the image was withheld.
pub(crate) fn tool_result_content(
    supports_images: bool,
    content: &str,
    attachments: &[Attachment],
    attachment_block: fn(&Attachment) -> Result<Value, String>,
) -> Result<Value, String> {
    if attachments.is_empty() || supports_images {
        user_content(content, attachments, attachment_block)
    } else {
        Ok(json!(format!(
            "{content}\n[image omitted: current model does not support images]"
        )))
    }
}

/// Merge `-o KEY=VALUE` options into a request body: JSON values pass
/// through, others ride as strings.
pub(crate) fn apply_options(body: &mut Value, options: &[(String, String)]) {
    for (k, v) in options {
        let parsed: Value = serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.clone()));
        body[k] = parsed;
    }
}

/// Whether assistant `reasoning_content` must be replayed on later turns.
/// DeepSeek's own API rejects it back ("reasoning_content ... is not
/// expected"), but gateways fronting thinking models — opencode Console Go
/// upstreams — 400 the other way ("The `reasoning_content` in the thinking
/// mode must be passed back to the API"). Table: hosts that require it;
/// everything else defaults to dropping it. `-o replay_reasoning=...`
/// (or the per-model options table) overrides either way.
pub(crate) fn replay_reasoning(m: &ResolvedModel) -> bool {
    let default = m
        .base_url
        .split('/')
        .any(|seg| seg.eq_ignore_ascii_case("opencode.ai"));
    match m
        .options
        .iter()
        .find(|(k, _)| k == "replay_reasoning")
        .map(|(_, v)| v.as_str())
    {
        Some(v) => v == "1" || v.eq_ignore_ascii_case("true"),
        None => default,
    }
}

/// The shared request dispatch: stream events through `feed`, or complete
/// once and emit through `complete`. Every adapter's `run` is url, headers,
/// body and this.
pub(crate) fn dispatch(
    req: HttpRequest,
    stream: bool,
    feed: impl Fn(&str, &Value, &mut Option<Usage>, &mut StopReason, &mut dyn FnMut(Event)),
    complete: impl Fn(&Value, &mut dyn FnMut(Event)),
    on_event: &mut dyn FnMut(Event),
) -> Result<(), String> {
    if stream {
        let mut usage: Option<Usage> = None;
        let mut stop = StopReason::default();
        // visible-output record shared with the retry policy in post_sse:
        // only real content counts, protocol prelude events do not
        let handed = std::sync::atomic::AtomicBool::new(false);
        let mut forward = |e: Event| {
            if matches!(
                e,
                Event::Delta(_) | Event::ReasoningDelta(_) | Event::ToolCallDelta { .. }
            ) {
                handed.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            on_event(e);
        };
        stream_events(&req, &handed, |event_type, chunk| {
            feed(event_type, chunk, &mut usage, &mut stop, &mut |e| {
                forward(e)
            });
        })?;
        on_event(Event::Done { usage, stop });
        Ok(())
    } else {
        let value = complete_json(&req)?;
        complete(&value, on_event);
        Ok(())
    }
}

/// A text attachment's decoded body. Attachments carry base64; text blocks
/// need the plaintext, and anything not valid UTF-8 is refused loudly.
pub(crate) fn decoded_text(a: &Attachment) -> Result<String, String> {
    let name = a.filename.as_deref().unwrap_or("?");
    let bytes = crate::b64::decode(&a.base64_data)
        .ok_or_else(|| format!("attachment {name} is not valid base64"))?;
    String::from_utf8(bytes).map_err(|_| format!("text attachment {name} must be valid UTF-8"))
}

/// Run an SSE request, parsing each event's data as JSON and handing
/// (event_type, value) to `on_value`. An `error` event aborts with its
/// message (parsed when possible, raw otherwise). A stream that closes
/// without its completion marker — `[DONE]` for openai-compat,
/// `message_stop` for anthropic — is a truncation, not a success: the
/// answer half-arrived and must surface as an error instead of being
/// logged as a finished turn.
pub fn stream_events(
    req: &HttpRequest,
    handed: &std::sync::atomic::AtomicBool,
    mut on_value: impl FnMut(&str, &Value),
) -> Result<(), String> {
    let mut stream_error: Option<String> = None;
    let mut saw_done = false;
    let result = http::post_sse(req, handed, |event_type, data| {
        if event_type == "error" {
            let msg = serde_json::from_str::<Value>(data)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(String::from))
                .unwrap_or_else(|| data.to_string());
            stream_error = Some(msg);
            return;
        }
        // OpenAI-compatible streams end with a literal `data: [DONE]`
        // sentinel, not JSON — swallow it instead of warning on every turn.
        if data.trim() == "[DONE]" || event_type == "message_stop" {
            saw_done = true;
            return;
        }
        match serde_json::from_str::<Value>(data) {
            Ok(chunk) => on_value(event_type, &chunk),
            Err(e) => eprintln!(
                "Warning: dropping unparsable SSE data ({e}): {}",
                &data[..crate::core::text::floor_boundary(data, 200)]
            ),
        }
    });
    result.map_err(|e| e.to_string())?;
    match stream_error {
        Some(err) => Err(err),
        None if !saw_done => Err(
            "stream ended without a completion marker ([DONE] / message_stop) — the answer may be truncated"
                .to_string(),
        ),
        None => Ok(()),
    }
}

/// POST for a single JSON response, failing on the wire error shape.
pub fn complete_json(req: &HttpRequest) -> Result<Value, String> {
    let body = http::post_json(req).map_err(|e| e.to_string())?;
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid response JSON: {e}"))?;
    if let Some(err) = value["error"]["message"].as_str() {
        return Err(err.to_string());
    }
    Ok(value)
}

/// A model resolved from config, ready to execute a prompt.
pub struct ResolvedModel {
    pub provider_name: String,
    pub kind: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model_id: String,
    /// -o key=value options (temperature, max_tokens, top_p, ...)
    pub options: Vec<(String, String)>,
}

/// Borrowed request inputs: history and tools stay owned by the caller so a
/// multi-turn agent loop never clones the whole conversation per turn.
pub struct PromptInput<'a> {
    pub system: Option<&'a str>,
    /// conversation history, oldest first
    pub history: &'a [Msg],
    pub prompt: &'a str,
    pub attachments: &'a [Attachment],
    /// tools offered to the model (agent mode); empty for plain prompts
    pub tools: &'a [ToolDef],
    /// reasoning effort level (minimal..xhigh); None sends no parameter
    pub reasoning: Option<&'a str>,
}

/// The reasoning-effort levels accepted by --thinking / /thinking.
pub const REASONING_LEVELS: &[&str] = &["minimal", "low", "medium", "high", "xhigh"];

/// One --thinking value: "off" (like absence) maps to None, a valid level
/// to itself, anything else to the shared error.
pub fn parse_thinking_level(raw: &str) -> Result<Option<String>, String> {
    if raw == "off" {
        return Ok(None);
    }
    if is_valid_reasoning_level(raw) {
        Ok(Some(raw.to_string()))
    } else {
        Err(format!(
            "invalid --thinking '{raw}' (off, minimal, low, medium, high, xhigh)"
        ))
    }
}

pub fn is_valid_reasoning_level(s: &str) -> bool {
    REASONING_LEVELS.contains(&s)
}

/// Anthropic thinking budget for an effort level (tokens). Unlike OpenAI's
/// named effort, thinking is enabled with an explicit token budget, and
/// max_tokens must exceed it.
pub fn thinking_budget(level: &str) -> Option<u64> {
    match level {
        "minimal" => Some(1024),
        "low" => Some(4096),
        "medium" => Some(16_384),
        "high" => Some(32_768),
        "xhigh" => Some(65_536),
        _ => None,
    }
}

impl ResolvedModel {
    pub fn from_config(
        provider_name: &str,
        p: &Provider,
        model_id: &str,
        api_key: Option<String>,
    ) -> ResolvedModel {
        ResolvedModel {
            provider_name: provider_name.to_string(),
            kind: p.kind.clone(),
            base_url: p.base_url.clone(),
            api_key,
            model_id: model_id.to_string(),
            options: Vec::new(),
        }
    }

    /// Model display id: provider/model.
    pub fn qualified_id(&self) -> String {
        format!("{}/{}", self.provider_name, self.model_id)
    }

    /// Whether this model is likely to accept image input. We have no
    /// per-model capability registry, so this is a heuristic: broad
    /// vision-capable families are recognized, unknown models default to
    /// accepting images, and a short curated text-only list is excluded so
    /// the provider is not given an image it will reject.
    pub fn supports_images(&self) -> bool {
        let id = format!("{} {}", self.provider_name, self.model_id).to_lowercase();
        const VISION: &[&str] = &[
            "claude",
            "gpt-4o",
            "gpt-5",
            "gemini",
            "qwen-vl",
            "qwen2.5-vl",
            "qwen3-vl",
            "glm-4v",
            "glm-4.5v",
            "pixtral",
            "llava",
            "vision",
            "vlm",
            "omni",
            "minimax",
            "kimi",
            "moonshot",
        ];
        if VISION.iter().any(|k| id.contains(k)) {
            return true;
        }
        const TEXT_ONLY: &[&str] = &[
            "deepseek-chat",
            "deepseek-reasoner",
            "gpt-3.5",
            "gpt-4.1-mini",
            "gpt-4.1-nano",
            "lamma-3.1",
            "llama-3.3",
            "qwen2.5-",
            "glm-4.5-",
            "glm-4.6",
            "mistral-small",
            "mistral-medium",
            "mistral-large",
        ];
        !TEXT_ONLY.iter().any(|k| id.contains(k))
    }

    /// Stream a prompt, feeding events to `on_event`. Returns when done.
    pub fn stream(
        &self,
        input: &PromptInput,
        stream: bool,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<(), String> {
        match self.kind.as_str() {
            "openai-compat" => openai_compat::run(self, input, stream, on_event),
            "anthropic" => anthropic::run(self, input, stream, on_event),
            other => Err(format!("Unknown provider kind: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn orphan_pairing_follows_the_last_result_index() {
        let history = vec![
            Msg::user("go"),
            Msg::Assistant {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "a".into(),
                    name: "ls".into(),
                    arguments: json!({}),
                }],
                reasoning: None,
            },
            Msg::Assistant {
                text: "done".into(),
                tool_calls: vec![ToolCall {
                    id: "b".into(),
                    name: "read".into(),
                    arguments: json!({}),
                }],
                reasoning: None,
            },
            Msg::tool_result("b", "read", "content"),
        ];
        let last = last_result_index(&history);
        // "a" (index 1) has no result after it → unpaired; "b" (index 2) is
        // answered by the result at index 3
        assert!(!call_answered(&last, "a", 1));
        assert!(call_answered(&last, "b", 2));
        assert_eq!(ORPHAN_RESULT, "No result provided");
        // a result sits at index 3: it pairs calls before it, never after
        assert!(!call_answered(&last, "b", 4));
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    pub(crate) fn model(kind: &str) -> ResolvedModel {
        ResolvedModel {
            provider_name: "test".into(),
            kind: kind.into(),
            base_url: "http://localhost".into(),
            api_key: None,
            model_id: "m1".into(),
            options: Vec::new(),
        }
    }

    pub(crate) fn input<'a>(history: &'a [Msg], tools: &'a [ToolDef]) -> PromptInput<'a> {
        PromptInput {
            system: None,
            history,
            prompt: "go",
            attachments: &[],
            tools,
            reasoning: None,
        }
    }

    pub(crate) fn tool_def() -> ToolDef {
        ToolDef {
            name: "read".into(),
            description: "read a file".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        }
    }

    pub(crate) fn att(mime: &str, name: Option<&str>) -> Attachment {
        Attachment {
            mime_type: mime.into(),
            base64_data: "AAAA".into(),
            filename: name.map(String::from),
            path: None,
            url: None,
        }
    }
}
