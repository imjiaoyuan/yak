//! Unified provider adapter layer.

pub mod anthropic;
pub mod catalog;
pub mod message;
pub mod openai_compat;

pub use message::{
    Msg, ORPHAN_RESULT, ToolCall, ToolCallAccumulator, ToolDef, ToolError, call_answered,
    last_result_index,
};

// the attachment type lives with its loader in core; re-exported here so
// the adapters' `Attachment` vocabulary keeps working
pub use crate::core::attachments::Attachment;

use serde_json::{Value, json};

use crate::core::config::Provider;
use crate::core::http::{self, Event, HttpRequest, StopReason, Usage};
use crate::core::text::human_bytes;

// the conversation model: unified messages both provider adapters serialize

/// Auth header pair for a provider kind: `x-api-key` for anthropic,
/// `Authorization: Bearer` for openai-compat.
fn auth_header(kind: &str, key: &str) -> (String, String) {
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
    let mut headers = vec![auth_header(kind, key)];
    if kind == "anthropic" {
        headers.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
    }
    headers
}

/// Vendor-gateway headers for a request URL — the vendor-specific half of
/// the client identity (the generic `user-agent` rides in `http.rs`).
/// OpenCode's Go/Zen gateway demands a stable conversation id in
/// `x-opencode-session`: without it every chat request is a 400
/// `MissingSessionID`, with it the gateway can pin routing and prompt cache.
/// One id per process; `LLM_SESSION_ID` pins it across processes for a
/// caller that keeps one conversation alive.
pub fn gateway_headers(url: &str) -> Vec<(String, String)> {
    if !is_opencode(url) {
        return Vec::new();
    }
    vec![("x-opencode-session".to_string(), session_id())]
}

/// The opencode.ai host, wherever it sits in the URL (zen, go, a path prefix).
fn is_opencode(url: &str) -> bool {
    let authority = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = authority.split(['/', '?', '#']).next().unwrap_or(authority);
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    host.eq_ignore_ascii_case("opencode.ai") || host.ends_with(".opencode.ai")
}

/// One session id per process, so every turn and retry of a run shares it.
fn session_id() -> String {
    static SESSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SESSION
        .get_or_init(|| match std::env::var("LLM_SESSION_ID") {
            Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => crate::core::db::ulid(),
        })
        .clone()
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

/// The model a command run resolves to — the one shared chain for every
/// agent entry point: `-m` > `LLM_MODEL` > `context` (the conversation's
/// last model, when one is resumed) > the stored default. A dangling
/// default warns and drops out instead of erroring. Saved per-model
/// options ride under CLI `-o` pairs.
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
    model.context_window = ResolvedModel::take_context_window(&mut model.options)?;
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
            "No default model configured. Run `llm` and use /login (or edit config.json), or pass -m."
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
    model.context_window = ResolvedModel::take_context_window(&mut model.options)?;
    Ok(model)
}

/// A user message body: plain text, or content parts when attachments
/// ride and the model can see them — a text-only model gets the text with
/// an omission note instead (its API rejects non-text parts wholesale).
pub(crate) fn user_content(
    supports_images: bool,
    text: &str,
    attachments: &[Attachment],
    attachment_block: fn(&Attachment) -> Result<Value, String>,
) -> Result<Value, String> {
    if attachments.is_empty() {
        return Ok(json!(text));
    }
    if !supports_images {
        let names: Vec<&str> = attachments
            .iter()
            .map(|a| a.filename.as_deref().unwrap_or("(image)"))
            .collect();
        return Ok(json!(format!(
            "{text}\n[image omitted: current model does not support images: {}]",
            names.join(", ")
        )));
    }
    let mut content = vec![json!({"type": "text", "text": text})];
    for a in attachments {
        content.push(attachment_block(a)?);
    }
    Ok(Value::Array(content))
}

/// Tool-result content: always a plain string on the wire — the images
/// ride a follow-up user message instead (`tool_result_images`). Gateways
/// fronting OpenAI-shaped APIs (opencode Console Go) reject non-text parts
/// inside a `tool` message, so the tool content stays text-only even for
/// vision models; the text notes that the image was withheld when the model
/// cannot see it at all.
pub(crate) fn tool_result_content(
    supports_images: bool,
    content: &str,
    attachments: &[Attachment],
) -> Result<Value, String> {
    if attachments.is_empty() {
        return Ok(json!(content));
    }
    if supports_images {
        // images follow in their own user message; keep any text here
        Ok(json!(content))
    } else {
        Ok(json!(format!(
            "{content}\n[image omitted: current model does not support images]"
        )))
    }
}

/// The parts of a tool result's images (no leading label): the caller
/// batches them into a follow-up user message after the run of tool
/// results. Empty — `None` — when nothing rides (no attachments, or the
/// model would reject them).
pub(crate) fn tool_result_images(
    supports_images: bool,
    attachments: &[Attachment],
    attachment_block: fn(&Attachment) -> Result<Value, String>,
) -> Result<Option<Vec<Value>>, String> {
    if attachments.is_empty() || !supports_images {
        return Ok(None);
    }
    let mut parts = Vec::with_capacity(attachments.len());
    for a in attachments {
        parts.push(attachment_block(a)?);
    }
    Ok(Some(parts))
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
                Event::Delta(_) | Event::ReasoningDelta { .. } | Event::ToolCallDelta { .. }
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

/// `agent.max_request_bytes` in config.json overrides it; the ceiling is a
/// property of the gateway, not of any one provider.
///
/// How full the request ceiling already is, when the attachments the user just
/// asked for are heavy enough to be worth saying out loud: the pre-flight will
/// refuse an oversized body, so the one moment the fix is cheap is here. Fires
/// past a fifth of the limit (the conversation adds more on top) and names the
/// numbers either way.
pub fn attachment_weight(attachments: &[Attachment], limit: usize) -> Option<String> {
    let bytes: usize = attachments.iter().map(|a| a.base64_data.len()).sum();
    if bytes * 5 < limit {
        return None;
    }
    let count = attachments.len();
    let total = crate::core::text::human_bytes(bytes as u64);
    let cap = crate::core::text::human_bytes(limit as u64);
    Some(if bytes > limit {
        format!(
            "{count} attachment(s) carry {total}, over the {cap} request limit: this request \
             would be refused before it is sent — drop an attachment or lower its size"
        )
    } else {
        format!(
            "{count} attachment(s) carry {total}, most of the {cap} request limit; a few more \
             images in this conversation will push a request past it"
        )
    })
}

/// Pre-flight budget on the serialized request body. Providers answer an
/// oversized body with a 413 whose text names nothing useful — a gateway in
/// front of the model wraps it beyond recognition — and by then the bytes are
/// already on the wire. Refusing the same body locally costs nothing and can
/// still say which attachments filled it. Attachment bytes are never
/// reclaimed: compaction prunes tool results, not attachment payloads, and a
/// resumed thread reloads every attachment it stored, so the remedy is a
/// smaller file or a fresh conversation.
pub(crate) fn check_request_body(body: &str, input: &PromptInput<'_>) -> Result<(), String> {
    if body.len() <= input.max_request_bytes {
        return Ok(());
    }
    let carried: Vec<&Attachment> = input
        .attachments
        .iter()
        .chain(input.history.iter().flat_map(message_attachments))
        .filter(|a| !a.base64_data.is_empty())
        .collect();
    let total = human_bytes(body.len() as u64);
    let limit = human_bytes(input.max_request_bytes as u64);
    if carried.is_empty() {
        return Err(format!(
            "request body is {total} (over the {limit} limit) and no attachment explains it: \
             the conversation itself is that large — start a new one"
        ));
    }
    let mut biggest: Vec<(&Attachment, usize)> =
        carried.iter().map(|a| (*a, a.base64_data.len())).collect();
    biggest.sort_by_key(|x| std::cmp::Reverse(x.1));
    // three names are enough to recognize the set; the count carries the rest
    let named = biggest
        .iter()
        .take(3)
        .map(|(a, n)| format!("{} ({})", reference(a), human_bytes(*n as u64)))
        .collect::<Vec<_>>()
        .join(", ");
    let rest = if biggest.len() > 3 { ", …" } else { "" };
    let bytes: usize = carried.iter().map(|a| a.base64_data.len()).sum();
    Err(format!(
        "request body is {total} (over the {limit} limit): {} attachment(s) carry {}\n  \
         largest first: {named}{rest}\n  \
         shrink them (resize or re-encode images) or start a new conversation — a resumed \
         thread reloads every attachment it stored",
        carried.len(),
        human_bytes(bytes as u64),
    ))
}

/// The attachments riding one message: user input and whatever images a tool
/// result carried. Assistant turns and summaries have none.
fn message_attachments(m: &Msg) -> &[Attachment] {
    match m {
        Msg::User { attachments, .. } | Msg::ToolResult { attachments, .. } => attachments,
        Msg::Assistant { .. } | Msg::Summary { .. } => &[],
    }
}

/// How an attachment is named in a report: its display name, else where it
/// came from, else its mime type.
fn reference(a: &Attachment) -> &str {
    a.filename
        .as_deref()
        .or(a.path.as_deref())
        .or(a.url.as_deref())
        .unwrap_or(&a.mime_type)
}

/// A text attachment's decoded body. Attachments carry base64; text blocks
/// need the plaintext, and anything not valid UTF-8 is refused loudly.
pub(crate) fn decoded_text(a: &Attachment) -> Result<String, String> {
    let name = a.filename.as_deref().unwrap_or("?");
    let bytes = crate::b64::decode(&a.base64_data)
        .ok_or_else(|| format!("attachment {name} is not valid base64"))?;
    String::from_utf8(bytes).map_err(|_| format!("text attachment {name} must be valid UTF-8"))
}

/// The error a stream that closes without its completion marker produces:
/// the answer half-arrived, so the round is not a clean turn, but the link
/// did not fail either. Callers that show it mark it as a warning (`!`) about
/// the answer, not as a broken run.
pub const TRUNCATED_STREAM: &str = "stream ended without a completion marker ([DONE] / message_stop) — the answer may be truncated";

/// Whether a stream error is that truncation rather than a wire failure.
pub fn is_truncation(error: &str) -> bool {
    error == TRUNCATED_STREAM
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
        None if !saw_done => Err(TRUNCATED_STREAM.to_string()),
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

/// How long a provider should keep this conversation's prompt-cache entry
/// (`agent.cache_ttl`). Only Anthropic's Messages API takes a lifetime: its
/// own default is five minutes, which is what `Default` names, so a config
/// that says `5m` leaves the request byte-identical to one that says nothing.
/// The hour exists for the gaps interactive work makes — an approval prompt,
/// a long test run — after which a lapsed entry would be re-written whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheTtl {
    /// what the provider gives on its own (Anthropic: five minutes)
    Default,
    /// the long-lived entry, billed at a higher write rate
    Hour,
}

impl CacheTtl {
    /// The config spellings, `5m` and `1h`; None for anything else, so the
    /// caller that read the file can refuse it loudly.
    pub fn parse(raw: &str) -> Option<CacheTtl> {
        match raw {
            "5m" => Some(CacheTtl::Default),
            "1h" => Some(CacheTtl::Hour),
            _ => None,
        }
    }

    /// The `cache_control.ttl` the wire carries, or None for the provider's
    /// own lifetime — the field is then absent, never `"5m"`.
    pub fn field(self) -> Option<&'static str> {
        match self {
            CacheTtl::Default => None,
            CacheTtl::Hour => Some("1h"),
        }
    }
}

/// A model resolved from config, ready to execute a prompt.
pub struct ResolvedModel {
    pub provider_name: String,
    pub kind: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model_id: String,
    /// the model's context window in tokens, when the user has recorded one
    /// (per-model option `context_window`). The agent loop anchors compaction
    /// to it (`compact::effective_trigger`); a gateway that never publishes
    /// its window stays None and the loop falls back to `agent.compact_at_tokens`
    /// plus the provider's own overflow refusal.
    pub context_window: Option<u64>,
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
    /// how much of `history` the previous request in this conversation
    /// already carried, as a prefix length (None on the first request).
    /// Cache breakpoints are placed from it: the prefix is stable, the tail
    /// is not, and a provider that caches by input prefix (Anthropic
    /// explicit markers, OpenAI-compatible automatic caching) reads the
    /// stable part back instead of rewriting the whole prompt every round.
    pub cache_anchor: Option<usize>,
    /// opaque conversation id for providers that route by it (OpenAI-style
    /// `prompt_cache_key`): keeps one conversation on one cache replica
    pub cache_key: Option<&'a str>,
    /// how long the provider should hold this prompt's cache entry, for the
    /// wires that take one (`agent.cache_ttl`). None means "no opinion" and
    /// leaves the request to the provider's own lifetime.
    pub cache_ttl: Option<CacheTtl>,
    /// the largest body this request may serialize to; refusing an oversized
    /// body locally beats paying for the upload only to read a gateway's
    /// opaque 413 back (`agent.max_request_bytes`)
    pub max_request_bytes: usize,
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
            context_window: None,
            options: Vec::new(),
        }
    }

    /// Pull `context_window` out of the merged option list and into the model.
    /// It is metadata, not a wire option: sending it would be a request body
    /// the provider did not document. An unparseable value fails loudly — the
    /// window is the one number the compaction gate trusts, so a typo must not
    /// silently switch the gate off.
    pub(crate) fn take_context_window(
        options: &mut Vec<(String, String)>,
    ) -> Result<Option<u64>, String> {
        let Some(index) = options.iter().position(|(k, _)| k == "context_window") else {
            return Ok(None);
        };
        let (_, raw) = options.remove(index);
        let raw = raw.trim();
        let n = raw
            .parse::<u64>()
            .map_err(|_| format!("invalid context_window '{raw}' (a token count)"))?;
        if n == 0 {
            return Err("context_window must be a positive token count".to_string());
        }
        Ok(Some(n))
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
            "glm-4.6v",
            // glm-5.3-flash is natively multimodal on the z.ai coding
            // endpoint (no `v` marker) per the current catalog
            "glm-5.3-flash",
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
            "glm-4.7",
            // glm-5.x on the z.ai coding endpoint is text-only (the API 400s
            // with code 1210 on any non-text content part) — except the
            // natively multimodal glm-5.3-flash whitelisted above
            "glm-5",
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
mod tests;

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
            context_window: None,
            options: Vec::new(),
        }
    }

    pub(crate) fn input<'a>(history: &'a [Msg], tools: &'a [ToolDef]) -> PromptInput<'a> {
        PromptInput {
            max_request_bytes: crate::core::http::MAX_REQUEST_BYTES,
            system: None,
            history,
            prompt: "go",
            attachments: &[],
            tools,
            reasoning: None,
            cache_anchor: None,
            cache_key: None,
            cache_ttl: None,
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
