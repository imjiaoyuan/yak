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
    /// a chunk of reasoning/thinking output
    ReasoningDelta(String),
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
    /// `cache_read_input_tokens` (whose read+write also fold into `input`)
    pub cached: u64,
}

impl Usage {
    /// Cached share of the input in whole percent (0 when unknown).
    pub fn cache_percent(self) -> u64 {
        (self.cached * 100).checked_div(self.input).unwrap_or(0)
    }

    /// Fold one round's counts into a running session total. Every consumer
    /// that tracks a session (the terminal footer, the session store, the
    /// `--json` feed) accumulates the same three fields, so the operation
    /// lives here rather than as three copies of the same three additions —
    /// a new field on `Usage` must not be able to be missed at one of them.
    pub fn add(&mut self, other: Usage) {
        self.input += other.input;
        self.output += other.output;
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
            Class::ContextTooLarge => "context window exceeded",
            Class::Stream => "stream failed",
            Class::Interrupted => "interrupted",
        }
    }
}

/// Bodies across providers that all mean "the prompt does not fit": OpenAI
/// "maximum context length", Anthropic "prompt is too long", Google "input
/// length and `max_tokens` exceed context limit".
fn context_too_large(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("context_length_exceeded")
        || m.contains("maximum context length")
        || m.contains("prompt is too long")
        || m.contains("exceed context limit")
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
            return None; // auth, bad requests, window overflows and interrupts never retry
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
        eprintln!(
            "{}\r\x1b[2Kretrying in {}s ({}){}",
            crate::theme::err().dim,
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

/// The shared agent: a process-wide connection pool so repeated requests
/// (agent loops, ping, model lists) reuse TLS connections instead of paying
/// a fresh handshake per call.
pub fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            // the connect phase must fail on its own: ureq leaves these
            // unlimited, and OS defaults stretch into minutes (Linux SYN
            // retransmits ≈ 2min) or to the global ceiling (a TLS handshake
            // to a black-holed host) — minutes of dead spinner before the
            // retry loop even starts. A real stream is unaffected: these
            // bound dialing only, the 1800s global still covers generation.
            .timeout_resolve(Some(Duration::from_secs(5)))
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(1800)))
            .build()
            .into()
    })
}

/// A short-timeout variant for quick probes (/models listings, ping).
pub fn short_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into()
    })
}

/// A longer-timeout variant for the webfetch tool: pages download slowly and
/// a 10s ceiling cuts real content off mid-read; probes stay on
/// `short_agent` so a dead host still fails fast there.
pub fn fetch_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_resolve(Some(Duration::from_secs(5)))
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into()
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

/// Client identity for every provider request: a real user agent (gateways
/// triage abuse by client, and an HTTP-library default reads as a script), plus
/// the stable conversation id OpenCode's Go/Zen gateway demands in
/// `x-opencode-session` — without it every chat request is a 400
/// `MissingSessionID`, and with it the gateway can pin routing and prompt cache.
pub fn identity_headers(url: &str) -> Vec<(String, String)> {
    let mut headers = vec![(
        "user-agent".to_string(),
        format!("llm/{}", env!("CARGO_PKG_VERSION")),
    )];
    if is_opencode(url) {
        headers.push(("x-opencode-session".to_string(), session_id()));
    }
    headers
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
/// `LLM_SESSION_ID` pins it across processes for a caller that keeps one
/// conversation alive; otherwise a fresh ulid stands in for this run.
pub fn session_id() -> String {
    static SESSION: OnceLock<String> = OnceLock::new();
    SESSION
        .get_or_init(|| match std::env::var("LLM_SESSION_ID") {
            Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => crate::core::db::ulid(),
        })
        .clone()
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
    for (k, v) in req.headers.iter().chain(identity_headers(&req.url).iter()) {
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
        return Err(HttpError {
            status,
            message: body,
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
    for (k, v) in identity_headers(url) {
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
    for (k, v) in identity_headers(url) {
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
        });
        total.add(Usage {
            input: 50,
            output: 5,
            cached: 0,
        });
        assert_eq!(total.input, 150);
        assert_eq!(total.output, 25);
        assert_eq!(total.cached, 60, "a zero stays a zero, not a reset");
        // and the cached share reads off the totals as before
        assert_eq!(total.cache_percent(), 40);
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
    fn only_opencode_hosts_get_a_session_header() {
        assert!(is_opencode(
            "https://opencode.ai/zen/go/v1/chat/completions"
        ));
        assert!(is_opencode("https://opencode.ai/zen/go/v1/messages"));
        assert!(is_opencode("https://api.opencode.ai/v1"));
        assert!(!is_opencode("https://opencode.ai.evil.test/v1"));
        assert!(!is_opencode("https://evil.test/?u=opencode.ai"));
        assert!(!is_opencode("https://api.deepseek.com/chat/completions"));
    }

    #[test]
    fn identity_headers_name_the_client_and_gate_the_session() {
        let go = identity_headers("https://opencode.ai/zen/go/v1/chat/completions");
        assert_eq!(go[0].0, "user-agent");
        assert!(go[0].1.starts_with("llm/"));
        assert_eq!(go[1].0, "x-opencode-session");
        assert!(!go[1].1.is_empty());
        let other = identity_headers("https://api.openai.com/v1/chat/completions");
        assert_eq!(other.len(), 1);
    }

    #[test]
    fn one_session_id_is_reused_for_the_whole_process() {
        assert_eq!(session_id(), session_id());
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
        assert_eq!(HttpError::new(401, "bad key").class(), Class::Auth);
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
