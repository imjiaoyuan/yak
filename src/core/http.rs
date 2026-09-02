//! HTTP plumbing: POST with SSE streaming, timeout, retry on 429/5xx.

use std::io::{BufRead, BufReader};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Cooperative interrupt flag set by the agent REPL's SIGINT handler: an
/// in-flight stream aborts at the next chunk boundary instead of dying.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub fn request_interrupt() {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

pub fn clear_interrupt() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Shared handle to the cooperative interrupt flag for platform shell
/// execution. The platform layer only reads this flag; it does not depend on
/// the agent module.
pub fn interrupt_flag() -> &'static AtomicBool {
    &INTERRUPTED
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
    /// cached_tokens`, Anthropic `cache_read_input_tokens`
    pub cached: u64,
}

impl Usage {
    /// Cached share of the input in whole percent (0 when unknown).
    pub fn cache_percent(self) -> u64 {
        (self.cached * 100).checked_div(self.input).unwrap_or(0)
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
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.status == 0 {
            // status 0 is a transport/TLS failure, not an HTTP response
            write!(f, "connection error: {}", self.message)
        } else {
            write!(f, "HTTP {}: {}", self.status, self.message)
        }
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
            .timeout_global(Some(Duration::from_secs(600)))
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

/// POST and hand each SSE `data:` line (with its `event:` type) to the
/// caller's parser. Retries once on 429/5xx with backoff.
pub fn post_sse(req: &HttpRequest, mut on_data: impl FnMut(&str, &str)) -> Result<(), HttpError> {
    let a = agent();
    send_sse(a, req, &mut on_data).or_else(|e| {
        if e.status == 429 || e.status >= 500 {
            std::thread::sleep(Duration::from_secs(2));
            send_sse(a, req, &mut on_data)
        } else {
            Err(e)
        }
    })
}

fn send_sse(
    agent: &ureq::Agent,
    req: &HttpRequest,
    on_data: &mut impl FnMut(&str, &str),
) -> Result<(), HttpError> {
    let response = send_raw(agent, req)?;
    let reader = BufReader::new(response.into_body().into_reader());
    // the blocking read lives on its own thread while this loop polls the
    // interrupt flag between 100ms slices — a silent server (long thinking
    // stretches, a hung connection) interrupts at once instead of waiting
    // for the next SSE line
    let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
    std::thread::spawn(move || {
        for line in reader.lines() {
            if tx.send(line.map_err(|e| e.to_string())).is_err() {
                break;
            }
        }
    });
    let mut event_type = String::new();
    loop {
        let line = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => line,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if interrupted() {
                    return Err(HttpError {
                        status: 0,
                        message: "interrupted".to_string(),
                    });
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        if interrupted() {
            return Err(HttpError {
                status: 0,
                message: "interrupted".to_string(),
            });
        }
        let line = line.map_err(|e| HttpError {
            status: 0,
            message: e,
        })?;
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

/// Non-streaming POST returning the raw body.
pub fn post_json(req: &HttpRequest) -> Result<String, HttpError> {
    let a = agent();
    send_raw(a, req)?
        .into_body()
        .read_to_string()
        .map_err(|e| HttpError {
            status: 0,
            message: e.to_string(),
        })
}

fn send_raw(
    agent: &ureq::Agent,
    req: &HttpRequest,
) -> Result<ureq::http::Response<ureq::Body>, HttpError> {
    let mut request = agent.post(&req.url);
    for (k, v) in &req.headers {
        request = request.header(k, v);
    }
    let response = request.send(&req.body).map_err(map_error)?;
    let status = response.status().as_u16();
    if status >= 400 {
        let body = response
            .into_body()
            .read_to_string()
            .unwrap_or_else(|e| format!("<unreadable error body: {e}>"));
        return Err(HttpError {
            status,
            message: body,
        });
    }
    Ok(response)
}

fn map_error(e: ureq::Error) -> HttpError {
    HttpError {
        status: 0,
        message: e.to_string(),
    }
}

fn get_text_with(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("Failed to fetch {url}: {e}"))?;
    if resp.status().as_u16() >= 400 {
        return Err(format!("Failed to fetch {url}: HTTP {}", resp.status()));
    }
    resp.into_body()
        .read_to_string()
        .map_err(|e| format!("Failed to read {url}: {e}"))
}

/// GET with a bounded timeout, for the agent's webfetch tool: fails fast
/// instead of hanging a task for minutes. Proxies still come from env vars.
pub fn get_text_short(url: &str) -> Result<String, String> {
    get_text_with(short_agent(), url)
}
