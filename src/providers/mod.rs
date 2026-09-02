//! Unified provider adapter layer.

pub mod anthropic;
pub mod catalog;
pub mod media;
pub mod message;
pub mod openai_compat;

pub use message::{
    Msg, ORPHAN_RESULT, ToolCall, ToolCallAccumulator, ToolDef, call_answered, last_result_index,
};

use serde_json::Value;

use crate::core::config::Provider;
use crate::core::http::{self, Event, HttpRequest};

// the conversation model: unified messages both provider adapters serialize

/// Static option schema per provider kind — our stand-in for the original's
/// per-model pydantic Options classes, used by `--options` rendering.
pub fn option_schema_for_kind(
    kind: &str,
) -> Vec<(&'static str, &'static str, Option<&'static str>)> {
    match kind {
        "openai-compat" => vec![
            ("temperature", "float", Some("Temperature for the model")),
            (
                "max_tokens",
                "int",
                Some("Maximum number of tokens to generate"),
            ),
            ("top_p", "float", Some("Top-p sampling")),
            ("frequency_penalty", "float", Some("Frequency penalty")),
            ("presence_penalty", "float", Some("Presence penalty")),
            (
                "json_object",
                "bool",
                Some("Request a JSON object response"),
            ),
        ],
        "anthropic" => vec![
            (
                "max_tokens",
                "int",
                Some("Maximum number of tokens to generate"),
            ),
            ("temperature", "float", Some("Temperature for the model")),
            ("top_p", "float", Some("Top-p sampling")),
            ("top_k", "int", Some("Top-k sampling")),
        ],
        "image" => vec![
            ("size", "str", Some("Image size, e.g. 1024x1024")),
            ("quality", "str", Some("Image quality")),
            (
                "n",
                "int",
                Some("How many images to generate (numbered --out files)"),
            ),
        ],
        "tts" => vec![
            ("voice", "str", Some("Voice to speak with (also --voice)")),
            (
                "response_format",
                "str",
                Some("Audio format: mp3, opus, wav, aac or flac"),
            ),
            ("speed", "float", Some("Speech speed")),
        ],
        _ => Vec::new(),
    }
}

/// Feature list per kind for `--options` rendering.
pub fn features_for_kind(kind: &str) -> Vec<&'static str> {
    match kind {
        "openai-compat" => vec!["streaming", "schemas"],
        "anthropic" => vec!["streaming"],
        "image" => vec!["images"],
        "tts" => vec!["speech"],
        _ => vec![],
    }
}

/// Auth header pair for a provider kind: `x-api-key` for anthropic,
/// `Authorization: Bearer` for openai-compat.
pub fn auth_header(kind: &str, key: &str) -> (String, String) {
    if kind == "anthropic" {
        ("x-api-key".to_string(), key.to_string())
    } else {
        ("Authorization".to_string(), format!("Bearer {key}"))
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
/// message (parsed when possible, raw otherwise).
pub fn stream_events(
    req: &HttpRequest,
    mut on_value: impl FnMut(&str, &Value),
) -> Result<(), String> {
    let mut stream_error: Option<String> = None;
    let result = http::post_sse(req, |event_type, data| {
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
        if data.trim() == "[DONE]" {
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
    /// --schema structured output (openai-compat only)
    pub schema: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attachment {
    pub mime_type: String,
    pub base64_data: String,
    /// display name for file-typed wire blocks; None for stdin/clipboard bytes
    pub filename: Option<String>,
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
            schema: None,
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
            // media kinds never stream chat events; callers use the media API
            other => Err(format!(
                "Unknown provider kind: {other} (use --out for media kinds)"
            )),
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
            },
            Msg::Assistant {
                text: "done".into(),
                tool_calls: vec![ToolCall {
                    id: "b".into(),
                    name: "read".into(),
                    arguments: json!({}),
                }],
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
