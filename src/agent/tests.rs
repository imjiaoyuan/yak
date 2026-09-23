use super::{RunCallbacks, RunRequest, usable_anchor};
use serde_json::json;

/// A continuation names the anchor for its first request; an anchor the
/// history does not have (a caller that counted the seed before a resume
/// pruned it, or a nonsense 0) is dropped rather than sent as a breakpoint
/// nothing can match.
#[test]
fn a_cache_anchor_is_only_used_when_the_history_has_it() {
    assert_eq!(usable_anchor(None, 10), None);
    assert_eq!(usable_anchor(Some(10), 10), Some(10));
    assert_eq!(usable_anchor(Some(3), 10), Some(3));
    assert_eq!(
        usable_anchor(Some(0), 10),
        None,
        "an empty prefix is no hint"
    );
    assert_eq!(
        usable_anchor(Some(11), 10),
        None,
        "a pruned seed is shorter than the caller counted"
    );
    assert_eq!(usable_anchor(Some(1), 0), None);
}

#[test]
fn summarize_shows_ten_lines_plus_count() {
    assert_eq!(summarize("a\nb\nc\nd\ne\n"), "a\nb\nc\nd\ne");
    assert_eq!(
        summarize("a\n".repeat(12).as_str()),
        "a\na\na\na\na\na\na\na\na\na … +2 lines"
    );
    assert_eq!(summarize("only"), "only");
    assert_eq!(summarize(""), "");
    assert_eq!(summarize("\n\n1\n2\n"), "1\n2");
}

#[test]
fn steering_joins_into_one_user_message() {
    let m = merge_steering(None, vec!["first".into(), "second".into()]).unwrap();
    assert!(matches!(m, Msg::User { ref text, .. } if text == "first\n\nsecond"));
    let m = merge_steering(Some(Msg::user("task")), vec!["steer".into()]).unwrap();
    assert!(matches!(m, Msg::User { ref text, .. } if text == "task\n\nsteer"));
    // nothing queued → pending untouched
    assert!(merge_steering(None, vec![]).is_none());
}

use super::*;

#[test]
fn accumulator_falls_back_on_bad_json() {
    let mut acc = ToolCallAccumulator::default();
    acc.push(0, Some("c1"), Some("ls"), "not json at all");
    let calls = acc.finish();
    assert_eq!(calls[0].id, "c1");
    assert_eq!(calls[0].arguments, json!({}));
}

#[test]
fn accumulator_synthesizes_missing_ids_and_orders_by_index() {
    let mut acc = ToolCallAccumulator::default();
    acc.push(2, None, Some("read"), "{}");
    acc.push(0, None, Some("ls"), "{}");
    let calls = acc.finish();
    assert_eq!(calls[0].name, "ls");
    assert_eq!(calls[0].id, "ls-0");
    assert_eq!(calls[1].id, "read-2");
}

#[test]
fn later_id_and_name_do_not_clobber_earlier() {
    let mut acc = ToolCallAccumulator::default();
    acc.push(0, Some("first"), Some("bash"), "{\"a\"");
    acc.push(0, Some(""), Some(""), ":1}");
    let calls = acc.finish();
    assert_eq!(calls[0].id, "first");
    assert_eq!(calls[0].name, "bash");
    assert_eq!(calls[0].arguments, json!({"a": 1}));
}

/// A mock openai-compat model served on `port`: every test server below
/// speaks its dialect.
fn mock_model(port: u16) -> crate::providers::ResolvedModel {
    crate::providers::ResolvedModel {
        provider_name: "mock".into(),
        kind: "openai-compat".into(),
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: Some("sk-x".into()),
        model_id: "m".into(),
        context_window: None,
        options: vec![],
    }
}

/// A compaction that cannot run must be reported, not silently skipped: the
/// session would otherwise keep growing past the window with nobody told, and
/// the notice fires once per run (a summarizer that keeps failing would
/// otherwise repeat itself every round).
#[test]
fn a_stalled_compaction_is_reported_once() {
    use std::io::Write as _;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut c = conn;
            read_request(&mut c);
            // refused outright: a 400 is fatal, so the summarizer call fails at
            // once instead of burning the connection-retry budget
            let _ = c.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 2\r\n\r\n{}");
        }
    });
    let model = mock_model(port);
    let cfg = compact::CompactConfig {
        trigger_tokens: 1_000,
        keep_recent_tokens: 0,
    };
    let marker = Some((
        0,
        Usage {
            input: 10_000,
            output: 0,
            cached: 0,
            cached_write: 0,
        },
    ));
    let mut history = vec![Msg::user("the task"), Msg::user("more")];
    let mut warned = false;
    let mut updates: Vec<AgentUpdate> = Vec::new();
    {
        let mut push = |u: AgentUpdate| updates.push(u);
        let mut sink = StallSink::new(&mut warned, &mut push);
        let _ = maybe_compact(&model, &mut history, marker, Some(&cfg), None, &mut sink);
    }
    assert_eq!(
        history.len(),
        2,
        "a stalled compaction leaves the seed alone"
    );
    let stalled = |updates: &Vec<AgentUpdate>| {
        updates
            .iter()
            .filter(|u| matches!(u, AgentUpdate::CompactStalled { .. }))
            .count()
    };
    assert_eq!(stalled(&updates), 1);
    // a second over-window round still tries, and stays quiet about it
    {
        let mut push = |u: AgentUpdate| updates.push(u);
        let mut sink = StallSink::new(&mut warned, &mut push);
        let _ = maybe_compact(&model, &mut history, marker, Some(&cfg), None, &mut sink);
    }
    assert_eq!(stalled(&updates), 1);
}

/// The empty extension host every inline-server test runs under; shared as
/// one leaked instance since `AgentOptions` borrows it.
fn empty_extensions() -> &'static crate::agent::ext::Extensions {
    static E: std::sync::OnceLock<crate::agent::ext::Extensions> = std::sync::OnceLock::new();
    E.get_or_init(crate::agent::ext::Extensions::empty)
}

/// The options every inline-server test runs with. The loop runs unbounded
/// (the reference's shape, guarded by compaction); every mock server here terminates
/// its own run by answering a plain, tool-free round last.
fn test_opts() -> AgentOptions<'static> {
    AgentOptions {
        max_request_bytes: crate::core::http::MAX_REQUEST_BYTES,
        system: None,
        cwd: std::env::temp_dir(),
        stream: true,
        compact: None,
        reasoning: None,
        hooks: empty_extensions(),
        cache_key: None,
        cache_anchor: None,
        cache_ttl: None,
    }
}

/// Callbacks that stay silent and deny every approval. Each call hands back
/// fresh closures (leaked — the test binary is short-lived), so every test
/// borrows its own.
fn deny_callbacks() -> RunCallbacks<'static> {
    RunCallbacks {
        on_update: Box::leak(Box::new(|_| {})),
        on_approval: Box::leak(Box::new(|_| ApprovalResponse::Deny)),
        steer: Box::leak(Box::new(std::vec::Vec::new)),
    }
}

/// Read one mock-server request: past the request head plus its
/// content-length body. Shared by the inline SSE servers below.
fn read_request(c: &mut std::net::TcpStream) {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        use std::io::Read;
        if c.read(&mut byte).unwrap_or(0) == 0 {
            break;
        }
        buf.push(byte[0]);
        let head_end = buf.windows(4).rposition(|w| w == b"\r\n\r\n");
        if let Some(i) = head_end {
            let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() - i - 4 >= len {
                break;
            }
        }
    }
}

/// A mock SSE server that drops the first connection after one delta
/// (no [DONE]) and completes the second: the agent loop must keep the
/// partial answer as a real assistant message and finish the task on
/// the continued request — the order stays prompt, partial, continuation.
#[test]
fn a_dropped_stream_continues_from_its_partial_answer() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits2 = hits.clone();
    let server = std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let n = hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut c = conn;
            // read past the request head + body (content-length)
            read_request(&mut c);
            if n == 0 {
                // one delta, then the connection dies mid-answer
                let _ = c.write_all(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n".as_bytes(),
                );
                let _ = c.write_all(
                    b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial ans\"}}]}\n\n",
                );
                drop(c); // no [DONE]: the stream is cut
            } else {
                let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ued cleanly\"}}]}\n\n\
                            data: [DONE]\n\n";
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                break;
            }
        }
    });
    use std::io::Write as _;

    let model = mock_model(port);
    let tools: Vec<Box<dyn tools::Tool>> = vec![];
    let opts = test_opts();
    let mut approval = approval::ApprovalConfig::default();
    let outcome = run_agent(
        RunRequest {
            model: &model,
            tools: &tools,
            prompt: "go",
            attachments: vec![],
            seed: vec![],
            opts: &opts,
        },
        &mut approval,
        deny_callbacks(),
    )
    .expect("the run must recover from the dropped stream");
    server.join().unwrap();
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "exactly one recovery request"
    );
    assert!(!outcome.interrupted);
    assert_eq!(outcome.final_text, "ued cleanly");
    assert_eq!(outcome.history.len(), 3, "prompt, partial, continuation");
    assert!(
        matches!(&outcome.history[1], Msg::Assistant { text, tool_calls, .. } if text == "partial ans" && tool_calls.is_empty()),
        "the partial answer rides the history as a real assistant message"
    );
}

/// A length-stopped response never runs its tool calls: streamed arguments
/// are salvaged JSON that may be silently incomplete, so every call in the
/// message fails with the reference's re-issue message and the model goes another round.
#[test]
fn a_length_stopped_round_fails_its_tool_calls_with_the_reissue_message() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits2 = hits.clone();
    let server = std::thread::spawn(move || {
        use std::io::Write as _;
        for conn in listener.incoming().flatten() {
            let n = hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut c = conn;
            read_request(&mut c);
            if n == 0 {
                let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"ls\",\"arguments\":\"{\\\"path\\\":\\\".\\\"}\"}}]}}]}\n\n\
                            data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n\
                            data: [DONE]\n\n";
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            } else {
                let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"}}]}\n\n\
                            data: [DONE]\n\n";
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                break;
            }
        }
    });
    let model = mock_model(port);
    let tools: Vec<Box<dyn tools::Tool>> = vec![];
    let opts = test_opts();
    let mut approval = approval::ApprovalConfig::default();
    let outcome = run_agent(
        RunRequest {
            model: &model,
            tools: &tools,
            prompt: "go",
            attachments: vec![],
            seed: vec![],
            opts: &opts,
        },
        &mut approval,
        deny_callbacks(),
    )
    .expect("the run continues past the truncated round");
    server.join().unwrap();
    assert_eq!(outcome.final_text, "done");
    let (name, content) = outcome
        .history
        .iter()
        .find_map(|m| match m {
            Msg::ToolResult { name, content, .. } => Some((name, content)),
            _ => None,
        })
        .expect("the truncated round must leave an error tool result");
    assert_eq!(name, "ls");
    assert!(content.contains("arguments may be truncated"), "{content}");
    assert!(
        content.contains("Re-issue the tool call with complete arguments"),
        "{content}"
    );
}

/// A provider refusing a prompt for not fitting its window is answered with a
/// forced compaction below the cut and a retry — the run survives the wall it
/// could not see (the window is not known here, so the refusal is the only
/// signal there is).
#[test]
fn a_refused_prompt_forces_a_compaction_and_the_round_is_retried() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits2 = hits.clone();
    let server = std::thread::spawn(move || {
        use std::io::Write as _;
        for conn in listener.incoming().flatten() {
            let n = hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut c = conn;
            read_request(&mut c);
            if n == 0 {
                let body =
                    r#"{"error":{"message":"This model's maximum context length is 4096 tokens"}}"#;
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            } else {
                let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n\
                            data: [DONE]\n\n";
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        }
    });
    let model = mock_model(port);
    let tools: Vec<Box<dyn tools::Tool>> = vec![];
    let mut opts = test_opts();
    // the window is exactly what is not known here: nothing is configured
    opts.compact = Some(compact::CompactConfig {
        trigger_tokens: 0,
        keep_recent_tokens: 0,
    });
    let mut approval = approval::ApprovalConfig::default();
    let outcome = run_agent(
        RunRequest {
            model: &model,
            tools: &tools,
            prompt: "go",
            attachments: vec![],
            seed: vec![Msg::user("the long task"), Msg::user("more")],
            opts: &opts,
        },
        &mut approval,
        deny_callbacks(),
    )
    .expect("the refusal must be answered with a compaction, not an error");
    // the mock keeps accepting, so the thread is left to the process exit
    drop(server);
    let hits = hits.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        hits >= 3,
        "the refusal, a summary of what it refused, then the retry: {hits} requests"
    );
    assert!(
        matches!(outcome.history.first(), Some(Msg::Summary { .. })),
        "the prefix below the cut became a summary"
    );
    assert_eq!(outcome.final_text, "ok");
}

/// A model whose `context_window` is recorded compacts against the window: a
/// seed over the anchored trigger is summarized *before* the
/// first request of the task goes out, so a resumed over-window thread never
/// sends its raw history once.
#[test]
fn a_known_window_compacts_before_the_first_request() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits2 = hits.clone();
    let server = std::thread::spawn(move || {
        use std::io::Write as _;
        for conn in listener.incoming().flatten() {
            let n = hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut c = conn;
            read_request(&mut c);
            // n=0 is the summarizer (forced by the pre-request check), n=1 the
            // first real round
            let content = if n == 0 { "compressed" } else { "ok" };
            let body = format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\n\
                 data: [DONE]\n\n"
            );
            let _ = c.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    let mut model = mock_model(port);
    model.context_window = Some(20_000);
    let tools: Vec<Box<dyn tools::Tool>> = vec![];
    let mut opts = test_opts();
    opts.compact = Some(compact::CompactConfig {
        trigger_tokens: 64_000,
        keep_recent_tokens: 0,
    });
    let mut approval = approval::ApprovalConfig::default();
    // 20k window minus the 16384 reserve anchors the trigger at 3616; a seed
    // of ~5000 tokens is over it, so the first request already sees a summary
    let outcome = run_agent(
        RunRequest {
            model: &model,
            tools: &tools,
            prompt: "go",
            attachments: vec![],
            seed: vec![Msg::user("the task"), Msg::user("x".repeat(20_000))],
            opts: &opts,
        },
        &mut approval,
        deny_callbacks(),
    )
    .expect("the pre-request compaction must succeed");
    drop(server);
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "one summary, then the first request"
    );
    assert!(
        matches!(outcome.history.first(), Some(Msg::Summary { .. })),
        "the over-window seed was summarized before the first request"
    );
    assert_eq!(outcome.final_text, "ok");
}

/// A gateway that reports usage past the known window without a 400 (z.ai's
/// silent swallow) is answered with a forced compaction and retry, exactly like
/// an outright refusal — the run must not quietly carry on over its window.
#[test]
fn a_silent_overflow_forces_a_compaction_and_the_round_is_retried() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits2 = hits.clone();
    let server = std::thread::spawn(move || {
        use std::io::Write as _;
        for conn in listener.incoming().flatten() {
            let n = hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut c = conn;
            read_request(&mut c);
            if n == 0 {
                // a successful round that still reports 50000 input tokens —
                // past the 20000 window it ran under, with no refusal
                let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\n\
                            data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                            data: {\"usage\":{\"prompt_tokens\":50000,\"completion_tokens\":5}}\n\n\
                            data: [DONE]\n\n";
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            } else {
                // n=1 is the summarizer; n=2 is the retried round
                let content = if n == 1 { "compressed" } else { "ok" };
                let body = format!(
                    "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\n\
                     data: [DONE]\n\n"
                );
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        }
    });
    let mut model = mock_model(port);
    model.context_window = Some(20_000);
    let tools: Vec<Box<dyn tools::Tool>> = vec![];
    let mut opts = test_opts();
    opts.compact = Some(compact::CompactConfig {
        trigger_tokens: 64_000,
        keep_recent_tokens: 0,
    });
    let mut approval = approval::ApprovalConfig::default();
    let outcome = run_agent(
        RunRequest {
            model: &model,
            tools: &tools,
            prompt: "go",
            attachments: vec![],
            seed: vec![Msg::user("the task"), Msg::user("more")],
            opts: &opts,
        },
        &mut approval,
        deny_callbacks(),
    )
    .expect("the silent overflow must be answered with a compaction, not a drift");
    drop(server);
    assert!(
        hits.load(std::sync::atomic::Ordering::SeqCst) >= 3,
        "the overflow round, a summary of it, then the retry"
    );
    assert!(
        matches!(outcome.history.first(), Some(Msg::Summary { .. })),
        "the prefix below the cut became a summary"
    );
    assert_eq!(outcome.final_text, "ok");
}

/// A mock SSE server that cuts the first connection before any content
/// arrives at all (no [DONE], no delta): the round produced nothing, so a
/// resend cannot duplicate anything on screen and the run must retry instead
/// of dying with the truncation error.
#[test]
fn a_stream_cut_before_any_output_is_resent() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits2 = hits.clone();
    let server = std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let n = hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut c = conn;
            // read past the request head + body (content-length)
            read_request(&mut c);
            use std::io::Write as _;
            if n == 0 {
                // headers, then the connection dies before one data event
                let _ = c.write_all(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n".as_bytes(),
                );
                drop(c);
            } else {
                let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"fresh answer\"}}]}\n\n\
                            data: [DONE]\n\n";
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                break;
            }
        }
    });

    let model = mock_model(port);
    let tools: Vec<Box<dyn tools::Tool>> = vec![];
    let opts = test_opts();
    let mut approval = approval::ApprovalConfig::default();
    let outcome = run_agent(
        RunRequest {
            model: &model,
            tools: &tools,
            prompt: "go",
            attachments: vec![],
            seed: vec![],
            opts: &opts,
        },
        &mut approval,
        deny_callbacks(),
    )
    .expect("an empty drop must be resent, not fatal");
    server.join().unwrap();
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "exactly one resend"
    );
    assert_eq!(outcome.final_text, "fresh answer");
    assert_eq!(outcome.history.len(), 2, "prompt, answer — no ghost turns");
}

/// A read-tier probe that records the thread it ran on, so a batched
/// read-only turn can be shown to execute off the calling thread.
struct ProbeTool(std::sync::Arc<std::sync::Mutex<Vec<std::thread::ThreadId>>>);
impl tools::Tool for ProbeTool {
    fn name(&self) -> &str {
        "probe"
    }
    fn tier(&self) -> super::approval::Tier {
        super::approval::Tier::Read
    }
    fn description(&self) -> &str {
        "probe"
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }
    fn preview(&self, _args: &serde_json::Value) -> String {
        "probe".into()
    }
    fn execute(
        &self,
        _args: &serde_json::Value,
        _cwd: &std::path::Path,
        _log: &mut dyn FnMut(&str),
    ) -> tools::ToolOutput {
        self.0.lock().unwrap().push(std::thread::current().id());
        tools::ToolOutput::ok("probed")
    }
}

/// Two read-only calls in one assistant message run on scope threads
/// (not the caller's) and their results still land in call order.
#[test]
fn batched_readonly_calls_run_off_the_calling_thread_in_order() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        for (n, conn) in listener.incoming().flatten().enumerate() {
            let mut c = conn;
            read_request(&mut c);
            let body = if n == 0 {
                let first = serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":0,"id":"c1","function":{"name":"probe","arguments":"{}"}},
                    {"index":1,"id":"c2","function":{"name":"probe","arguments":"{}"}}
                ]}}]})
                .to_string();
                format!(
                    "data: {first}\n\n\
                     data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
                     data: [DONE]\n\n"
                )
            } else {
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"}}]}\n\n\
                 data: [DONE]\n\n"
                    .to_string()
            };
            let _ = c.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
            if n >= 1 {
                break;
            }
        }
    });
    use std::io::Write as _;

    let model = mock_model(port);
    let threads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let tools: Vec<Box<dyn tools::Tool>> = vec![Box::new(ProbeTool(threads.clone()))];
    let opts = test_opts();
    let caller = std::thread::current().id();
    let mut approval = approval::ApprovalConfig::default();
    let outcome = run_agent(
        RunRequest {
            model: &model,
            tools: &tools,
            prompt: "go",
            attachments: vec![],
            seed: vec![],
            opts: &opts,
        },
        &mut approval,
        deny_callbacks(),
    )
    .expect("a batched read-only turn is not a failure");
    server.join().unwrap();
    assert_eq!(outcome.final_text, "done");
    // both calls ran, each on its own scope thread — never the caller's
    let ids = threads.lock().unwrap().clone();
    assert_eq!(ids.len(), 2, "both calls executed");
    assert!(
        ids.iter().all(|id| *id != caller),
        "read-only calls must not run on the calling thread"
    );
    assert_ne!(ids[0], ids[1], "the two calls ran concurrently");
    // results stay in call order
    let results: Vec<(&str, &str)> = outcome
        .history
        .iter()
        .filter_map(|m| match m {
            Msg::ToolResult {
                call_id, content, ..
            } => Some((call_id.as_str(), content.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(results, [("c1", "probed"), ("c2", "probed")]);
}

/// The marker prices the covered prefix at the provider's reported count and
/// only the tail at the chars/4 estimate: the number the compaction gate acts
/// on must be the reported total, not a chars/4 rescan of history the provider
/// already counted.
#[test]
fn the_usage_marker_total_reaches_the_compaction_gate() {
    let history = vec![Msg::user("hi"), Msg::user("there")];
    let marker = Some((
        history.len(),
        Usage {
            input: 7_000,
            output: 0,
            cached: 0,
            cached_write: 0,
        },
    ));
    let used = compact::estimate_tokens(&history, marker);
    assert_eq!(used, 7_000, "the marker's reported total must survive");
    assert!(compact::should_compact(used, 7_000));
    assert!(!compact::should_compact(used, 7_001));
}
