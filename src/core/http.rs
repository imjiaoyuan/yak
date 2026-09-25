//! HTTP plumbing: POST with SSE streaming, idle timeout, classified retries.

use std::io::{BufRead, BufReader};
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ureq::ResponseExt;

pub fn request_interrupt() {
    crate::platform::interrupt::request();
}

pub fn clear_interrupt() {
    crate::platform::interrupt::clear();
}

pub fn interrupted() -> bool {
    crate::platform::interrupt::checked()
}

/// Shared handle to the cooperative interrupt flag for platform shell
/// execution. Core re-exports the platform flag so existing callers stay put.
pub fn interrupt_flag() -> &'static AtomicBool {
    crate::platform::interrupt::flag()
}

/// Events emitted while a model streams a response.
pub enum Event {
    /// a chunk of visible output text
    Delta(String),
    /// a chunk of reasoning/thinking output; `meta` rides the last chunk of
    /// a thinking block (Anthropic's `signature_delta`) and carries the
    /// opaque state the provider needs replayed with the trace
    ReasoningDelta {
        text: String,
        meta: Option<serde_json::Value>,
    },
    /// a fragment of a streamed tool call; `index` is the provider's block
    /// index, `id`/`name` ride along on the first fragment (which may be empty)
    ToolCallDelta {
        index: usize,
        name: Option<String>,
        id: Option<String>,
        fragment: String,
    },
    /// stream finished; carries token usage if reported and why the model
    /// stopped
    Done {
        usage: Option<Usage>,
        stop: StopReason,
    },
}

/// Token usage one model round reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    /// input tokens served from the provider's prompt cache, when reported:
    /// DeepSeek `prompt_cache_hit_tokens`, OpenAI `prompt_tokens_details.
    /// cached_tokens`, OpenRouter `cached_tokens`, Anthropic
    /// `cache_read_input_tokens`. Always a subset of `input`.
    pub cached: u64,
    /// input tokens written into the provider's prompt cache, when the two
    /// halves are reported separately (Anthropic
    /// `cache_creation_input_tokens`). Also a subset of `input`; kept apart
    /// from `cached` because it is what a later event carrying only
    /// `input_tokens` has to fold back in, and because a write is not a read.
    pub cached_write: u64,
}

impl Usage {
    /// Cached share of the input in whole percent (0 when unknown).
    pub fn cache_percent(self) -> u64 {
        self.share(self.cached)
    }

    /// Share of the input the provider charged a cache-write price for, in
    /// whole percent. 0 both when nothing was written and when the wire keeps
    /// no write count of its own — only Anthropic splits the two apart.
    pub fn write_percent(self) -> u64 {
        self.share(self.cached_write)
    }

    /// `n` as a whole percent of this round's input.
    fn share(self, n: u64) -> u64 {
        (n * 100).checked_div(self.input).unwrap_or(0)
    }

    /// Fold one round's counts into a running session total. Every consumer
    /// that tracks a session (the terminal footer, the session store, the
    /// `--json` feed) accumulates the same three fields, so the operation
    /// lives here rather than as three copies of the same three additions —
    /// a new field on `Usage` must not be able to be missed at one of them.
    pub fn add(&mut self, other: Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cached_write += other.cached_write;
        self.cached += other.cached;
    }
}

/// Why a model response ended. `ToolUse` is the signal an agent loop acts on;
/// transport-level failures propagate as `Err` instead of a stop reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StopReason {
    #[default]
    Stop,
    ToolUse,
    Length,
}

pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub message: String,
    /// the server's Retry-After on 429, in seconds (honored over backoff)
    pub retry_after: Option<u64>,
    /// x-request-id / cf-ray, shown in the error for bug reports
    pub request_id: Option<String>,
}

impl HttpError {
    fn new(status: u16, message: impl Into<String>) -> HttpError {
        HttpError {
            status,
            message: message.into(),
            retry_after: None,
            request_id: None,
        }
    }

    /// What went wrong, as far as the status and body say. The class decides
    /// retryability: transport failures and rate limits get another chance,
    /// auth and bad requests never do (the reference shape taxonomy, lean on
    /// purpose — the reference-spirited).
    fn class(&self) -> Class {
        match self.status {
            0 => {
                if self.message == "interrupted" {
                    Class::Interrupted
                } else if self.message.starts_with("stream") {
                    Class::Stream
                } else {
                    Class::Connection
                }
            }
            429 => Class::RateLimited,
            401 | 403 => Class::Auth,
            413 => Class::PayloadTooLarge,
            // 408 is the server giving up on a request it already accepted,
            // which has the same retry shape as a 5xx
            408 => Class::Server,
            400 if context_too_large(&self.message) => Class::ContextTooLarge,
            s if s >= 500 => Class::Server,
            _ => Class::InvalidRequest,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// transport failure before a response (DNS, TCP, TLS)
    Connection,
    /// HTTP 429
    RateLimited,
    /// HTTP 5xx
    Server,
    /// HTTP 401/403: a key problem retrying cannot fix
    Auth,
    /// other 4xx
    InvalidRequest,
    /// HTTP 413: the body the provider refuses on size, not on content
    PayloadTooLarge,
    /// 400 whose body says the prompt does not fit the model's window
    ContextTooLarge,
    /// failure after the stream started (idle timeout, dropped connection)
    Stream,
    /// the user pressed esc/ctrl-c
    Interrupted,
}

impl Class {
    fn label(self) -> &'static str {
        match self {
            Class::Connection => "connection error",
            Class::RateLimited => "rate limited",
            Class::Server => "server error",
            Class::Auth => "authentication",
            Class::InvalidRequest => "bad request",
            Class::PayloadTooLarge => "request too large",
            Class::ContextTooLarge => "context window exceeded",
            Class::Stream => "stream failed",
            Class::Interrupted => "interrupted",
        }
    }
}

/// Bodies across providers that all mean "the prompt does not fit". The list
/// is the reference's (`packages/ai/src/utils/the overflow module`) — every entry is provider or
/// gateway wording seen in the wild, translated to the substring tests a
/// regex-free build can run (the reference's `\\d` shaping only narrowed matches the
/// words already name). Second element: a needle that must also appear, for
/// the patterns anchoring on both sides.
const OVERFLOW_NEEDLES: &[(&str, Option<&str>)] = &[
    ("prompt is too long", None),                       // Anthropic
    ("request_too_large", None),                        // Anthropic 413
    ("input is too long for requested model", None),    // Amazon Bedrock
    ("exceeds the context window", None),               // OpenAI
    ("exceeds", Some("maximum context length")),        // LiteLLM & OpenAI-compatible proxies
    ("input token count", Some("exceeds the maximum")), // Google
    ("maximum prompt length is", None),                 // xAI
    ("reduce the length of the messages", None),        // Groq
    ("maximum context length is", None),                // OpenRouter
    ("maximum allowed input length", None),             // OpenRouter/Poolside
    ("is longer than the model", None),                 // Together AI
    ("exceeds the limit of", None),                     // GitHub Copilot
    ("exceeds the available context size", None),       // llama.cpp server
    ("greater than the context length", None),          // LM Studio
    ("context window exceeds limit", None),             // MiniMax
    ("exceeded model token limit", None),               // Kimi For Coding
    ("too large for model with", None),                 // Mistral
    ("the configured context size is", None),           // DS4
    ("model_context_window_exceeded", None),            // z.ai finish reason
    ("prompt too long", None),                          // Ollama
    ("range of input length should be", None),          // DashScope/Qwen
    ("context_length_exceeded", None),                  // generic
    ("context length exceeded", None), // generic (the reference: context[_ ]length[_ ]exceeded)
    ("exceed context limit", None),    // generic
    ("too many tokens", None),         // generic (the exclusions keep throttling out)
    ("token limit exceeded", None),    // generic
];

fn context_too_large(message: &str) -> bool {
    let m = message.to_lowercase();
    // the reference's non-overflow exclusions run first: throttling and rate-limit texts
    // also speak of token counts (Bedrock wraps "too many tokens" in a
    // ThrottlingException), and a throttle must retry, not compact
    if m.contains("rate limit")
        || m.contains("too many requests")
        || m.contains("throttling error")
        || m.contains("throttlingexception")
        || m.contains("service unavailable")
    {
        return false;
    }
    OVERFLOW_NEEDLES
        .iter()
        .any(|(a, b)| m.contains(a) && b.is_none_or(|b| m.contains(b)))
}

/// Does this text name a prompt that does not fit the model's window? A
/// rendered error repeats the provider's own words, so the agent loop can
/// recognize the refusal from the message it already has — and that refusal is
/// what teaches it the window it could not have known before.
pub fn context_overflow(text: &str) -> bool {
    context_too_large(text)
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.status == 0 {
            // status 0 is a transport/stream failure whose message already
            // says what broke ("stream: …", "connection refused")
            write!(f, "{}", self.message)?;
        } else {
            write!(f, "HTTP {}: {}", self.status, self.message)?;
        }
        if let Some(id) = &self.request_id {
            write!(f, " [req {id}]")?;
        }
        // a 413 body is rarely more than a gateway's own wording (a proxy in
        // front of the provider often returns prose-free JSON), so the
        // remedy rides on the error itself
        if self.status == 413 {
            write!(
                f,
                " — the request body is over the provider's limit: shrink or drop \
                 attachments, then retry (a new conversation starts over without them)"
            )?;
        }
        Ok(())
    }
}

/// Per-request retry budget. Connection failures get their own, larger
/// counter with a longer backoff (the reference's split): a dead network is worth
/// waiting out, a flaky 5xx is not. The connection budget is still bounded
/// for an attended terminal — six attempts top out around three minutes of
/// automatic fighting, then the error surfaces and a human decides; every
/// phase of the wait is esc/ctrl-c interruptible regardless.
const REQUEST_MAX_RETRIES: usize = 4;
const CONNECTION_MAX_RETRIES: usize = 6;
/// A stream silent for this long is dead, however long the generation.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

struct Retry {
    attempt: usize,
}

impl Retry {
    fn new() -> Retry {
        Retry { attempt: 0 }
    }

    /// Whether to try again after `e`, and how long to wait. `None` gives
    /// up. Stream drops are in the retryable set because `post_sse` refuses
    /// to resend once any output was handed out — the guard lives there,
    /// where "emitted" is known.
    fn next(&mut self, e: &HttpError) -> Option<Duration> {
        let class = e.class();
        if !matches!(
            class,
            Class::Connection | Class::RateLimited | Class::Server | Class::Stream
        ) {
            // auth, bad requests, oversized bodies, window overflows and
            // interrupts never retry: none of them change on a re-send
            return None;
        }
        let connection = class == Class::Connection;
        let max = if connection {
            CONNECTION_MAX_RETRIES
        } else {
            REQUEST_MAX_RETRIES
        };
        self.attempt += 1;
        if self.attempt > max {
            return None;
        }
        Some(
            e.retry_after
                .map(Duration::from_secs)
                .unwrap_or_else(|| backoff_with(self.attempt, connection, jitter01())),
        )
    }
}

/// Exponential backoff with ±10% jitter: 1s→30s for response failures,
/// 5s→60s for connection failures.
fn backoff_with(attempt: usize, connection: bool, frac: f64) -> Duration {
    let (base, cap) = if connection { (5.0, 60.0) } else { (1.0, 30.0) };
    let plain = (base * 2f64.powi(attempt as i32 - 1)).min(cap);
    Duration::from_secs_f64((plain * (0.9 + 0.2 * frac.clamp(0.0, 1.0))).max(0.05))
}

fn jitter01() -> f64 {
    // subsecond nanos are plenty of entropy for a ±10% nudge; no RNG crate
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos % 1000) / 1000.0
}

/// Decide the next retry delay and announce long waits; None gives up.
fn next_delay(retry: &mut Retry, e: &HttpError) -> Option<Duration> {
    let delay = retry.next(e)?;
    if delay >= Duration::from_secs(2) {
        // clear the row first: the spinner redraws this row without a
        // newline, and an append would glue the notice onto its frame
        // (theme::cursor() empties out when stderr is not a terminal, so a
        // piped stream never sees the escape)
        eprintln!(
            "{}{}{}retrying in {}s ({}){}",
            crate::theme::cursor().clear_line,
            crate::theme::err().dim,
            crate::theme::NOTICE,
            delay.as_secs_f32().ceil() as u64,
            e.class().label(),
            crate::theme::err().reset
        );
    }
    Some(delay)
}

/// Sleep in short slices so esc/ctrl-c still interrupts a backoff wait.
fn sleep_interruptible(d: Duration) {
    let deadline = Instant::now() + d;
    while Instant::now() < deadline {
        if interrupted() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One configured agent: dial-phase bounds so the connect phase fails on
/// its own — ureq leaves these unlimited, and OS defaults stretch into
/// minutes (Linux SYN retransmits ≈ 2min) or to the global ceiling (a TLS
/// handshake to a black-holed host) — minutes of dead spinner before the
/// retry loop even starts — plus a global ceiling. `http_status_as_error`
/// stays off: status is a value the caller reads, not a panic path.
fn mk_agent(resolve: Duration, connect: Duration, global: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_resolve(Some(resolve))
        .timeout_connect(Some(connect))
        .timeout_global(Some(global))
        .build()
        .into()
}

/// The shared agent: a process-wide connection pool so repeated requests
/// (agent loops, ping, model lists) reuse TLS connections instead of paying
/// a fresh handshake per call. The 1800s global still covers generation;
/// the dial bounds only affect a black-holed host.
pub fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        mk_agent(
            Duration::from_secs(5),
            Duration::from_secs(10),
            Duration::from_secs(1800),
        )
    })
}

/// A short-timeout variant for quick probes (/models listings, ping).
pub fn short_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        mk_agent(
            Duration::from_secs(5),
            Duration::from_secs(10),
            Duration::from_secs(10),
        )
    })
}

/// A longer-timeout variant for the webfetch tool: pages download slowly and
/// a 10s ceiling cuts real content off mid-read; probes stay on
/// `short_agent` so a dead host still fails fast there.
pub fn fetch_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        mk_agent(
            Duration::from_secs(5),
            Duration::from_secs(10),
            Duration::from_secs(30),
        )
    })
}

/// POST and hand each SSE `data:` line (with its `event:` type) to the
/// caller's parser. Retryable failures (transport, 429 with its Retry-After,
/// 5xx) resend with jittered backoff; a stream that already handed output to
/// the user never resends transparently — replaying it would duplicate the
/// answer — so mid-stream drops surface as errors instead. `handed` is the
/// caller-owned record of visible output (text/reasoning/tool deltas): the
/// first SSE event alone does not count, a drop before any real output can
/// still be retried safely.
pub fn post_sse(
    req: &HttpRequest,
    handed: &std::sync::atomic::AtomicBool,
    mut on_data: impl FnMut(&str, &str),
) -> Result<(), HttpError> {
    let a = agent();
    let mut retry = Retry::new();
    loop {
        // a press that landed between attempts must not read as a fresh try
        if interrupted() {
            return Err(HttpError::new(0, "interrupted"));
        }
        let result = send_sse(a, req, &mut |ev, data| {
            on_data(ev, data);
        });
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                if handed.load(std::sync::atomic::Ordering::Relaxed) && e.class() == Class::Stream {
                    return Err(e);
                }
                let Some(delay) = next_delay(&mut retry, &e) else {
                    return Err(e);
                };
                sleep_interruptible(delay);
                if interrupted() {
                    return Err(HttpError::new(0, "interrupted"));
                }
            }
        }
    }
}

fn send_sse(
    agent: &'static ureq::Agent,
    req: &HttpRequest,
    on_data: &mut impl FnMut(&str, &str),
) -> Result<(), HttpError> {
    let response = send_raw_interruptible_flag(agent, req, crate::platform::interrupt::flag())?;
    let reader = BufReader::new(response.into_body().into_reader());
    // the blocking read lives on its own thread while this loop polls the
    // interrupt flag between 100ms slices — a silent server (long thinking
    // stretches, a hung connection) interrupts at once instead of waiting
    // for the next SSE line, and a stream silent past the idle window is
    // declared dead however long the generation might have been
    let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
    std::thread::spawn(move || {
        for line in reader.lines() {
            if tx.send(line.map_err(|e| e.to_string())).is_err() {
                break;
            }
        }
    });
    let mut event_type = String::new();
    let mut last_byte = Instant::now();
    loop {
        let line = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                last_byte = Instant::now();
                line
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if interrupted() {
                    return Err(HttpError::new(0, "interrupted"));
                }
                if last_byte.elapsed() >= STREAM_IDLE_TIMEOUT {
                    return Err(HttpError::new(
                        0,
                        format!(
                            "stream idle for {}s (no bytes from the server)",
                            last_byte.elapsed().as_secs()
                        ),
                    ));
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        if interrupted() {
            return Err(HttpError::new(0, "interrupted"));
        }
        let line = line.map_err(|e| HttpError::new(0, format!("stream: {e}")))?;
        if line.is_empty() {
            event_type.clear();
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f.trim(), v.trim_start()),
            None => continue,
        };
        match field {
            "event" => event_type = value.to_string(),
            "data" => on_data(&event_type, value),
            _ => {}
        }
    }
}

/// Non-streaming POST returning the raw body, under the same retry policy.
pub fn post_json(req: &HttpRequest) -> Result<String, HttpError> {
    let a = agent();
    let mut retry = Retry::new();
    loop {
        if interrupted() {
            return Err(HttpError::new(0, "interrupted"));
        }
        // the whole attempt (send + body read) runs on the worker: a body
        // read stalled by a dead peer would otherwise hang to the global
        // ceiling with the interrupt flag unreachable, same as the dial
        let owned = HttpRequest {
            url: req.url.clone(),
            headers: req.headers.clone(),
            body: req.body.clone(),
        };
        let result = attempt_interruptible(crate::platform::interrupt::flag(), move || {
            send_raw(a, &owned).and_then(|r| {
                r.into_body()
                    .read_to_string()
                    .map_err(|e| HttpError::new(0, format!("stream: {e}")))
            })
        });
        match result {
            Ok(body) => return Ok(body),
            Err(e) => {
                let Some(delay) = next_delay(&mut retry, &e) else {
                    return Err(e);
                };
                sleep_interruptible(delay);
                if interrupted() {
                    return Err(HttpError::new(0, "interrupted"));
                }
            }
        }
    }
}

/// Client identity for every outbound request: a real user agent — gateways
/// triage abuse by client, and an HTTP-library default reads as a script.
/// Vendor-specific headers (OpenCode's `x-opencode-session`) are added by
/// the provider adapters that know they are talking to that vendor
/// (`providers::gateway_headers`), not sniffed here for every URL.
pub fn identity_headers() -> Vec<(String, String)> {
    vec![(
        "user-agent".to_string(),
        format!("llm/{}", env!("CARGO_PKG_VERSION")),
    )]
}

/// One blocking network attempt — DNS, TCP, TLS, body upload, response
/// headers, and whatever body read the caller chains in — runs on a worker
/// thread while this loop polls the interrupt flag in 100ms slices. A plain
/// blocking call offers no hook to reach the flag: a connect black-holed by
/// a dead network or a stalled read used to hang inside code esc/ctrl-c
/// cannot touch, and a pressed key sat ignored until the OS gave up
/// (minutes; a TLS handshake to a black-holed host, the global ceiling) —
/// the "stuck, cannot exit" report. An abandoned worker finishes alone
/// (bounded by the resolve/connect/global timeouts) and its result drops on
/// the floor when the receiver is gone.
fn attempt_interruptible<T: Send + 'static>(
    flag: &AtomicBool,
    f: impl FnOnce() -> Result<T, HttpError> + Send + 'static,
) -> Result<T, HttpError> {
    if flag.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(HttpError::new(0, "interrupted"));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // the receiver may have left (interrupt): a send error just ends
        // the worker, which is exactly what abandonment means
        let _ = tx.send(f());
    });
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => return result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(HttpError::new(0, "interrupted"));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(HttpError::new(0, "request worker died"));
            }
        }
    }
}

/// The flag is a parameter so tests can drive the short-circuit with a local
/// atomic instead of flipping the process-wide interrupt over parallel tests.
fn send_raw_interruptible_flag(
    agent: &'static ureq::Agent,
    req: &HttpRequest,
    flag: &AtomicBool,
) -> Result<ureq::http::Response<ureq::Body>, HttpError> {
    let owned = HttpRequest {
        url: req.url.clone(),
        headers: req.headers.clone(),
        body: req.body.clone(),
    };
    attempt_interruptible(flag, move || send_raw(agent, &owned))
}

fn send_raw(
    agent: &'static ureq::Agent,
    req: &HttpRequest,
) -> Result<ureq::http::Response<ureq::Body>, HttpError> {
    let mut request = agent.post(&req.url);
    for (k, v) in req.headers.iter().chain(identity_headers().iter()) {
        request = request.header(k, v);
    }
    let response = request.send(&req.body).map_err(map_error)?;
    let status = response.status().as_u16();
    if status >= 400 {
        // keep what bug reports need: the body, the server's retry hint and
        // the request id it can trace
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        let request_id = ["x-request-id", "x-oai-request-id", "cf-ray"]
            .iter()
            .find_map(|h| {
                response
                    .headers()
                    .get(*h)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            });
        let body = response
            .into_body()
            .read_to_string()
            .unwrap_or_else(|e| format!("<unreadable error body: {e}>"));
        // a 413 body is a gateway's wrapper and rarely names the number that
        // matters; the size we sent is ours to state
        let message = if status == 413 {
            format!(
                "{body} (request body was {})",
                crate::core::text::human_bytes(req.body.len() as u64)
            )
        } else {
            body
        };
        return Err(HttpError {
            status,
            message,
            retry_after,
            request_id,
        });
    }
    Ok(response)
}

/// Retry-After as seconds; the HTTP-date form is rare enough to skip.
fn parse_retry_after(v: &str) -> Option<u64> {
    v.trim().parse().ok()
}

fn map_error(e: ureq::Error) -> HttpError {
    HttpError::new(0, e.to_string())
}

/// Cap on an attachment of any kind — a URL fetch (`-a URL`, enforced in
/// `get_bytes`) and a local file (enforced in `attachments::load`): provider
/// document limits sit around 32MB, so anything past 50MB cannot ride a
/// request anyway — refuse it instead of buffering it first.
pub const MAX_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;

/// Cap on one request body. The same reasoning as the per-attachment cap, one
/// level up: Anthropic documents a 32MB request ceiling, and a gateway in
/// front of any provider refuses far less politely (an opaque 413 whose body
/// names nothing). A body past this cannot be sent, so `providers` refuses it
/// locally, while it can still name the attachments that filled it.
pub const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

/// GET and read the whole body: (bytes, content-type) after a status check.
/// The body is capped: a size-limited read refuses an oversized download
/// instead of buffering it whole first.
pub fn get_bytes(url: &str) -> Result<(Vec<u8>, Option<String>), String> {
    get_with(agent(), url)
}

/// One GET through `agent`: status check, whole body, content type. The
/// whole fetch (dial through body read) runs on a worker polled in 100ms
/// slices, so a black-holed URL attachment fetch is esc/ctrl-c interruptible
/// instead of hanging to the global timeout (send_sse's rationale, GET-flavored).
fn get_with(agent: &'static ureq::Agent, url: &str) -> Result<(Vec<u8>, Option<String>), String> {
    get_with_flag(agent, url, crate::platform::interrupt::flag())
}

/// Read a response body up to `cap` bytes: reading exactly cap+1 proves the
/// body is oversized and refuses it, so memory stays bounded no matter what
/// the server sends.
fn read_capped<R: std::io::Read>(reader: R, cap: usize, url: &str) -> Result<Vec<u8>, HttpError> {
    use std::io::Read;
    let mut buf = Vec::new();
    let n = reader
        .take(cap as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| HttpError::new(0, format!("Failed to read {url}: {e}")))?;
    if n > cap {
        return Err(HttpError::new(
            0,
            format!(
                "Failed to read {url}: body exceeds {} (got more)",
                crate::core::text::human_bytes(cap as u64)
            ),
        ));
    }
    Ok(buf)
}

/// Flag-as-parameter twin of `get_with`, same test rationale as
/// `send_raw_interruptible_flag`.
fn get_with_flag(
    agent: &'static ureq::Agent,
    url: &str,
    flag: &AtomicBool,
) -> Result<(Vec<u8>, Option<String>), String> {
    let owned = url.to_string();
    attempt_interruptible(flag, move || get_blocking(agent, &owned)).map_err(|e| e.to_string())
}

/// The blocking body of `get_with`, running on its worker thread. Errors
/// carry the full "Failed to fetch …" text; `HttpError` displays it verbatim.
fn get_blocking(
    agent: &'static ureq::Agent,
    url: &str,
) -> Result<(Vec<u8>, Option<String>), HttpError> {
    let mut request = agent.get(url);
    for (k, v) in identity_headers() {
        request = request.header(k, v);
    }
    let resp = request
        .call()
        .map_err(|e| HttpError::new(0, format!("Failed to fetch {url}: {e}")))?;
    if resp.status().as_u16() >= 400 {
        return Err(HttpError::new(
            0,
            format!("Failed to fetch {url}: HTTP {}", resp.status()),
        ));
    }
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|c| c.split(';').next().unwrap_or(c).to_string());
    let buf = read_capped(resp.into_body().into_reader(), MAX_ATTACHMENT_BYTES, url)?;
    Ok((buf, content_type))
}

/// A fetched web page, decoded to text: the final URL (after redirects),
/// the bare mime type, and the UTF-8 body.
pub struct FetchedPage {
    pub url: String,
    pub content_type: String,
    pub body: String,
}

/// Download ceiling for the webfetch tool: the tool result itself is cut to
/// the shared line/byte/token limits before it reaches the model, and HTML
/// stripping only shrinks, so 2MB of raw body is generous — a size-limited
/// read keeps memory bounded while the body streams in.
const FETCH_DOWNLOAD_MAX_BYTES: usize = 2 * 1024 * 1024;

/// GET with the short-timeout agent for the agent's webfetch tool: follows
/// redirects, carries the final URL and mime type, and decodes to UTF-8 —
/// a binary body (image/pdf/audio/…) errors naming its content type instead
/// of producing garbage text. Fails fast instead of hanging a task.
pub fn fetch_page(url: &str) -> Result<FetchedPage, String> {
    // same interruptible shape as get_with, short-agent flavored: the 10s
    // global timeout bounds a dead fetch, the worker poll bounds a ctrl-c
    let owned = url.to_string();
    attempt_interruptible(crate::platform::interrupt::flag(), move || {
        fetch_page_blocking(fetch_agent(), &owned)
    })
    .map_err(|e| e.to_string())
}

/// The blocking body of `fetch_page`, running on its worker thread.
fn fetch_page_blocking(agent: &'static ureq::Agent, url: &str) -> Result<FetchedPage, HttpError> {
    let mut request = agent.get(url);
    for (k, v) in identity_headers() {
        request = request.header(k, v);
    }
    let resp = request
        .call()
        .map_err(|e| HttpError::new(0, format!("Failed to fetch {url}: {e}")))?;
    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(HttpError::new(
            0,
            format!("Failed to fetch {url}: HTTP {status}"),
        ));
    }
    let final_url = resp.get_uri().to_string();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|c| c.split(';').next().unwrap_or(c).trim().to_string())
        .unwrap_or_default();
    let buf = read_capped(
        resp.into_body().into_reader(),
        FETCH_DOWNLOAD_MAX_BYTES,
        url,
    )?;
    let body = String::from_utf8(buf).map_err(|_| {
        HttpError::new(
            0,
            format!("Failed to read {url}: non-UTF-8 body (content-type: {content_type})"),
        )
    })?;
    Ok(FetchedPage {
        url: final_url,
        content_type,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one place a new `Usage` field has to be taught to the session
    /// totals: three call sites used to add the same three fields by hand.
    #[test]
    fn usage_adds_every_field_into_a_running_total() {
        let mut total = Usage::default();
        total.add(Usage {
            input: 100,
            output: 20,
            cached: 60,
            cached_write: 30,
        });
        total.add(Usage {
            input: 50,
            output: 5,
            cached: 0,
            cached_write: 0,
        });
        assert_eq!(total.input, 150);
        assert_eq!(total.output, 25);
        assert_eq!(total.cached, 60, "a zero stays a zero, not a reset");
        assert_eq!(total.cached_write, 30);
        // and the two cache shares read off the totals
        assert_eq!(total.cache_percent(), 40);
        assert_eq!(total.write_percent(), 20);
        // a round that reported nothing cannot divide by zero
        assert_eq!(Usage::default().cache_percent(), 0);
        assert_eq!(Usage::default().write_percent(), 0);
        // adding nothing is a no-op
        let before = total;
        total.add(Usage::default());
        assert_eq!(total, before);
    }

    #[test]
    fn capped_reads_refuse_an_oversized_body() {
        let big = vec![b'x'; 10_000];
        assert_eq!(read_capped(&big[..], 20_000, "u").unwrap().len(), 10_000);
        let e = read_capped(&big[..], 9_999, "u").unwrap_err();
        assert!(e.to_string().contains("exceeds"), "{e}");
        // exactly at the cap is fine: only cap+1 proves oversize
        assert_eq!(
            read_capped(&big[..10_000], 10_000, "u").unwrap().len(),
            10_000
        );
        assert!(read_capped(&big[..], 0, "u").is_err());
    }

    #[test]
    fn identity_headers_name_the_client() {
        let h = identity_headers();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, "user-agent");
        assert!(h[0].1.starts_with("llm/"));
    }

    #[test]
    fn statuses_classify_into_retryable_and_fatal() {
        let conn = HttpError::new(0, "tcp connect error");
        assert_eq!(conn.class(), Class::Connection);
        let drop = HttpError::new(0, "stream: connection reset");
        assert_eq!(drop.class(), Class::Stream);
        let idle = HttpError::new(0, "stream idle for 300s (no bytes from the server)");
        assert_eq!(idle.class(), Class::Stream);
        assert_eq!(HttpError::new(0, "interrupted").class(), Class::Interrupted);
        assert_eq!(HttpError::new(429, "slow down").class(), Class::RateLimited);
        assert_eq!(HttpError::new(503, "unavailable").class(), Class::Server);
        // a 408 is the server timing out, not a malformed request
        assert_eq!(
            HttpError::new(408, "request timeout").class(),
            Class::Server
        );
        assert_eq!(HttpError::new(401, "bad key").class(), Class::Auth);
        assert_eq!(
            HttpError::new(413, "payload too large").class(),
            Class::PayloadTooLarge
        );
        assert_eq!(
            HttpError::new(400, "unknown parameter").class(),
            Class::InvalidRequest
        );
    }

    #[test]
    fn context_overflow_bodies_classify_as_window_errors() {
        assert_eq!(
            HttpError::new(400, "This model's maximum context length is 8192 tokens").class(),
            Class::ContextTooLarge
        );
        assert_eq!(
            HttpError::new(400, "prompt is too long: 210000 tokens > 200000 maximum").class(),
            Class::ContextTooLarge
        );
        assert_eq!(
            HttpError::new(
                400,
                "the request's input length and `max_tokens` exceed context limit"
            )
            .class(),
            Class::ContextTooLarge
        );
        // other 400s stay ordinary bad requests
        assert_eq!(
            HttpError::new(400, "invalid model").class(),
            Class::InvalidRequest
        );
    }

    #[test]
    fn context_overflow_matches_provider_wordings_and_skips_throttling() {
        // the reference's the overflow module corpus, one entry per provider family
        for body in [
            "This endpoint's maximum context length is 64000 tokens. However, you requested about 100000 tokens",
            "Requested token count exceeds the model's maximum context length of 131072 tokens",
            "Input length 265330 exceeds the maximum allowed input length of 262144 tokens.",
            "Please reduce the length of the messages or completion",
            "This model's maximum prompt length is 131072 but the request contains 537812 tokens",
            "The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)",
            "The input (70000 tokens) is longer than the model's context length (32000 tokens)",
            "the request exceeds the available context size, try increasing it",
            "tokens to keep from the initial prompt is greater than the context length",
            "invalid params, context window exceeds limit",
            "Your request exceeded model token limit: 8192 (requested: 9000)",
            "Prompt contains 70000 tokens ... too large for model with 32000 maximum context length",
            "Prompt has 8000 tokens, but the configured context size is 4096 tokens",
            "prompt too long; exceeded max context length by 8000 tokens",
            "Range of input length should be [1, 6144]",
        ] {
            assert!(super::context_overflow(body), "should be overflow: {body}");
        }
        // throttling that mentions token counts is NOT overflow: it must
        // retry, not compact (Bedrock's ThrottlingException wording)
        assert!(!super::context_overflow(
            "ThrottlingException: Too many tokens, please wait before trying again."
        ));
        assert!(!super::context_overflow(
            "Rate limit: too many tokens in prompt, slow down"
        ));
    }

    #[test]
    fn an_interrupt_lands_even_inside_the_connect_phase() {
        // a local flag, not the process-wide one: flipping the global here
        // would race every parallel test that reads it (the read tool does)
        let flag = AtomicBool::new(true);
        let req = HttpRequest {
            url: "http://127.0.0.1:9/v1/chat/completions".into(),
            headers: vec![],
            body: "{}".into(),
        };
        let e = send_raw_interruptible_flag(agent(), &req, &flag).unwrap_err();
        assert_eq!(e.class(), Class::Interrupted);
        let g = get_with_flag(agent(), "http://127.0.0.1:9/x", &flag).unwrap_err();
        assert!(g.contains("interrupted"), "{g}");
    }

    #[test]
    fn retry_budget_honors_class_and_server_delay() {
        let mut r = Retry::new();
        // auth never retries
        assert_eq!(r.next(&HttpError::new(401, "bad key")), None);
        // server errors retry REQUEST_MAX_RETRIES times, then give up
        let mut r = Retry::new();
        for _ in 0..REQUEST_MAX_RETRIES {
            assert!(r.next(&HttpError::new(503, "unavailable")).is_some());
        }
        assert_eq!(r.next(&HttpError::new(503, "unavailable")), None);
        // connection failures get their own larger budget
        let mut r = Retry::new();
        for _ in 0..CONNECTION_MAX_RETRIES {
            assert!(r.next(&HttpError::new(0, "dns failure")).is_some());
        }
        assert_eq!(r.next(&HttpError::new(0, "dns failure")), None);
        // a server-provided Retry-After wins over computed backoff
        let mut r = Retry::new();
        let e = HttpError {
            status: 429,
            message: "slow down".into(),
            retry_after: Some(7),
            request_id: None,
        };
        assert_eq!(r.next(&e), Some(Duration::from_secs(7)));
    }

    #[test]
    fn backoff_doubles_caps_and_jitters_within_ten_percent() {
        assert_eq!(
            backoff_with(1, false, 0.0),
            Duration::from_secs_f64(1.0 * 0.9)
        );
        assert_eq!(
            backoff_with(1, false, 1.0),
            Duration::from_secs_f64(1.0 * 1.1)
        );
        // plain sequence 1,2,4,8… capped at 30 (response) / 5,10,20… 60 (connection)
        assert_eq!(backoff_with(2, false, 0.5), Duration::from_secs_f64(2.0));
        assert_eq!(backoff_with(20, false, 0.5), Duration::from_secs_f64(30.0));
        assert_eq!(backoff_with(1, true, 0.5), Duration::from_secs_f64(5.0));
        assert_eq!(backoff_with(20, true, 0.5), Duration::from_secs_f64(60.0));
    }

    #[test]
    fn retry_after_parses_seconds_only() {
        assert_eq!(parse_retry_after("7"), Some(7));
        assert_eq!(parse_retry_after(" 12 "), Some(12));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
    }

    #[test]
    fn an_oversized_body_is_fatal_and_carries_the_remedy() {
        let e = HttpError {
            status: 413,
            // the shape a proxy in front of a provider returns: a wrapped
            // server_error whose text says nothing about the size
            message: "{\"error\":{\"code\":\"server_error\"}}".into(),
            retry_after: None,
            request_id: Some("req_1".into()),
        };
        // retrying replays the same oversized body, so it never happens
        assert_eq!(Retry::new().next(&e), None);
        let text = e.to_string();
        assert!(text.starts_with("HTTP 413: "), "{text}");
        assert!(text.contains("over the provider's limit"), "{text}");
        assert!(text.contains("[req req_1]"), "{text}");
        // and the hint stays off every other status
        assert!(!HttpError::new(500, "boom").to_string().contains("limit"));
    }

    #[test]
    fn display_carries_the_request_id() {
        let e = HttpError {
            status: 500,
            message: "boom".into(),
            retry_after: None,
            request_id: Some("req_abc123".into()),
        };
        assert_eq!(e.to_string(), "HTTP 500: boom [req req_abc123]");
    }
}
