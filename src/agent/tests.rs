use super::{RunCallbacks, RunRequest, advance_seed_boundary, usable_anchor};
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

/// Compaction drops `cut` messages and inserts one summary, so the run's
/// own region moves; a boundary consumed by the cut lands on the summary.
#[test]
fn compaction_shifts_the_seed_boundary() {
    assert_eq!(advance_seed_boundary(10, 4), 7); // 6 seed messages survive
    assert_eq!(advance_seed_boundary(4, 4), 1); // the whole seed was cut
    assert_eq!(advance_seed_boundary(2, 9), 1);
    assert_eq!(advance_seed_boundary(0, 3), 1);
}

/// A throwaway cwd for the fusion tests (they never touch the repo).
fn fuse_dir() -> std::path::PathBuf {
    let dir = crate::core::testutil::scratch_dir("fuse");
    std::fs::write(dir.join("x.txt"), "hello\n").unwrap();
    dir
}

fn edit_call(then_run: &str) -> ToolCall {
    ToolCall {
        id: "1".into(),
        name: "edit".into(),
        arguments: json!({
            "path": "x.txt",
            "edits": [{"oldText": "hello", "newText": "world"}],
            "then_run": then_run
        }),
    }
}

#[test]
fn then_run_fuses_the_command_into_the_mutation_result() {
    let dir = fuse_dir();
    let tools = tools::builtin_tools();
    let mut approval = approval::ApprovalConfig::default();
    let out = fuse_then_run(
        tools::ToolOutput::ok("applied 1 edit"),
        &edit_call("cat x.txt"),
        &tools,
        &dir,
        &mut approval,
        &mut |_| panic!("yolo mode must not prompt for the fused command"),
    );
    assert!(!out.is_error(), "the applied edit keeps its status");
    assert!(out.content.contains("applied 1 edit"));
    assert!(
        out.content.contains("[then_run] $ cat x.txt"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("hello"),
        "the command output rides the same result: {}",
        out.content
    );
}

#[test]
fn then_run_never_fires_on_a_failed_mutation_or_a_plain_call() {
    let dir = fuse_dir();
    let tools = tools::builtin_tools();
    let mut approval = approval::ApprovalConfig::default();
    // the edit failed (bad match): running its validation would be noise
    let out = fuse_then_run(
        tools::ToolOutput::err("oldText did not match"),
        &edit_call("touch ran.txt"),
        &tools,
        &dir,
        &mut approval,
        &mut |_| panic!("a failed mutation must not run its follow-up"),
    );
    assert!(out.is_error());
    assert!(!out.content.contains("[then_run]"), "{}", out.content);
    assert!(!dir.join("ran.txt").exists(), "nothing ran");
    // a bash call carrying then_run is not recursively fused
    let bash = ToolCall {
        id: "2".into(),
        name: "bash".into(),
        arguments: json!({"command": "echo hi", "then_run": "touch ran.txt"}),
    };
    let out = fuse_then_run(
        tools::ToolOutput::ok("hi\n"),
        &bash,
        &tools,
        &dir,
        &mut approval,
        &mut |_| panic!("only edit/write fuse"),
    );
    assert_eq!(out.content, "hi\n");
}

#[test]
fn then_run_still_passes_the_approval_gate() {
    // ask mode: the fused command is gated like any other exec call, and
    // a denial is reported without failing the applied edit
    let dir = fuse_dir();
    let tools = tools::builtin_tools();
    let mut approval = approval::ApprovalConfig::default();
    approval.mode = approval::Mode::AlwaysAsk;
    let out = fuse_then_run(
        tools::ToolOutput::ok("applied 1 edit"),
        &edit_call("touch ran.txt"),
        &tools,
        &dir,
        &mut approval,
        &mut |_| ApprovalResponse::Deny,
    );
    assert!(!out.is_error(), "the edit still landed");
    assert!(out.content.contains("not run: denied"), "{}", out.content);
    assert!(
        !dir.join("ran.txt").exists(),
        "a denied follow-up did not run"
    );
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

#[test]
fn identical_repeats_remind_at_escalating_counts() {
    let mut g = RepeatGuard {
        last: None,
        count: 0,
    };
    let args = json!({"command": "ls -la", "path": "."});
    assert!(
        g.observe("bash", &args).is_none(),
        "the first call is not a repeat"
    );
    assert!(
        g.observe("bash", &args).is_none(),
        "the second is still quiet"
    );
    let note = g.observe("bash", &args).expect("the third repeat reminds");
    assert!(note.contains("3rd identical bash call"), "{note}");
    assert!(
        g.observe("bash", &args).is_none(),
        "quiet between thresholds"
    );
    let note = g.observe("bash", &args).expect("the fifth repeat reminds");
    assert!(note.contains("5th identical bash call"), "{note}");
    assert!(
        note.contains("ls -la"),
        "the detailed note names the repeated arguments: {note}"
    );
}

#[test]
fn repeat_tracking_ignores_key_order_and_resets_on_change() {
    let mut g = RepeatGuard {
        last: None,
        count: 0,
    };
    assert!(g.observe("bash", &json!({"a": 1, "b": 2})).is_none());
    assert!(
        g.observe("bash", &json!({"b": 2, "a": 1})).is_none(),
        "same arguments in a different key order are the same call"
    );
    let note = g
        .observe("bash", &json!({"a": 1, "b": 2}))
        .expect("third identical call");
    assert!(note.contains("3rd"));
    // a different call restarts the streak from one
    assert!(g.observe("bash", &json!({"a": 9})).is_none());
    // a new user message clears the tracker entirely
    g.reset();
    assert!(g.observe("bash", &json!({"a": 9})).is_none());
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
        let _ = maybe_compact(&model, &mut history, marker, Some(&cfg), 0, None, &mut sink);
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
        let _ = maybe_compact(&model, &mut history, marker, Some(&cfg), 0, None, &mut sink);
    }
    assert_eq!(stalled(&updates), 1);
}

/// The empty extension host every inline-server test runs under; shared as
/// one leaked instance since `AgentOptions` borrows it.
fn empty_extensions() -> &'static crate::agent::ext::Extensions {
    static E: std::sync::OnceLock<crate::agent::ext::Extensions> = std::sync::OnceLock::new();
    E.get_or_init(crate::agent::ext::Extensions::empty)
}

/// The options every inline-server test runs with; `max_turns` and
/// `token_budget` are the two dials a test actually turns, so the rest
/// starts here and is overridden in place.
fn test_opts() -> AgentOptions<'static> {
    AgentOptions {
        max_request_bytes: crate::core::http::MAX_REQUEST_BYTES,
        system: None,
        cwd: std::env::temp_dir(),
        max_turns: 0,
        token_budget: 0,
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
    let mut opts = test_opts();
    opts.max_turns = 4;
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
    assert!(!outcome.budget_exhausted);
    assert_eq!(outcome.final_text, "ued cleanly");
    assert_eq!(outcome.history.len(), 3, "prompt, partial, continuation");
    assert!(
        matches!(&outcome.history[1], Msg::Assistant { text, tool_calls, .. } if text == "partial ans" && tool_calls.is_empty()),
        "the partial answer rides the history as a real assistant message"
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
    opts.max_turns = 4;
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

/// A model whose `context_window` is recorded compacts against the window, not
/// the raw ladder: a seed over the anchored trigger is summarized *before* the
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
    let mut opts = test_opts();
    opts.max_turns = 4;
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

/// The token budget stops a task like a turn cap would, but on the
/// metric that actually prices a runaway loop: cumulative input tokens.
/// Past 80% a wrap-up note rides the pending prompt; at 100% the loop
/// breaks and the outcome says so. A plain answer (no tool calls) ends
/// the run normally, so the server needs one tool call per round to
/// keep the loop alive until the budget bites.
#[test]
fn token_budget_warns_then_stops_the_run() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits2 = hits.clone();
    let server = std::thread::spawn(move || {
        let mut n = 0usize;
        for conn in listener.incoming().flatten() {
            if n >= 10 {
                break;
            }
            let mut c = conn;
            read_request(&mut c);
            let _ = hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            n += 1;
            // each round reports 1000 input tokens and asks for a tool;
            // budget 2500: warn after round 2 (2000 ≥ 80%), stop before 4
            let body = format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c{n}\",\"function\":{{\"name\":\"echo\",\"arguments\":\"{{}}\"}}}}]}}}}]}}\n\n\
                 data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
                 data: {{\"usage\":{{\"prompt_tokens\":1000,\"completion_tokens\":5}}}}\n\n\
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
    use std::io::Write as _;

    let model = mock_model(port);
    let tools: Vec<Box<dyn tools::Tool>> = vec![Box::new(EchoTool)];
    let mut opts = test_opts();
    opts.token_budget = 2500;
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
    .expect("a budget stop is a normal outcome, not a failure");
    // let a hypothetical erroneous 4th request land before counting
    std::thread::sleep(std::time::Duration::from_millis(150));
    drop(server);
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "round 4 never starts: 3000 spent ≥ 2500 budget"
    );
    assert!(outcome.budget_exhausted);
    // the wrap-up note rode the round-3 prompt (2400 ≥ 2400 soft line)
    let warned = outcome
        .history
        .iter()
        .any(|m| matches!(m, Msg::User { text, .. } if text.contains("token budget")));
    assert!(warned, "the soft-line note must be in the history");
}

struct EchoTool;
impl tools::Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn tier(&self) -> super::approval::Tier {
        super::approval::Tier::Read
    }
    fn description(&self) -> &str {
        "echo"
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }
    fn preview(&self, _args: &serde_json::Value) -> String {
        "echo".into()
    }
    fn execute(
        &self,
        _args: &serde_json::Value,
        _cwd: &std::path::Path,
        _log: &mut dyn FnMut(&str),
    ) -> tools::ToolOutput {
        tools::ToolOutput::ok("echoed")
    }
}

/// Loop hygiene end to end: three identical echo calls in a row, and
/// the third result carries the advisory reminder; a plain answer then
/// ends the run normally.
#[test]
fn a_third_identical_tool_call_reminds_the_model_to_change_approach() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits2 = hits.clone();
    let server = std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let n = hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut c = conn;
            read_request(&mut c);
            let body = if n < 3 {
                format!(
                    "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c{n}\",\"function\":{{\"name\":\"echo\",\"arguments\":\"{{}}\"}}}}]}}}}]}}\n\n\
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
            if n >= 3 {
                break;
            }
        }
    });
    use std::io::Write as _;

    let model = mock_model(port);
    let tools: Vec<Box<dyn tools::Tool>> = vec![Box::new(EchoTool)];
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
    .expect("identical echoes are not a failure");
    server.join().unwrap();
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        4,
        "three tool rounds plus the plain answer"
    );
    assert_eq!(outcome.final_text, "done");
    let results: Vec<&str> = outcome
        .history
        .iter()
        .filter_map(|m| match m {
            Msg::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 3);
    assert!(!results[0].contains("[System]"));
    assert!(!results[1].contains("[System]"));
    assert!(
        results[2].contains("3rd identical echo call"),
        "the reminder rides the third result: {}",
        results[2]
    );
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

/// Budget awareness: the note reports the task's input-token room,
/// wrapped so the model reads it as a system note rather than a user turn.
#[test]
fn context_note_reports_the_room_left() {
    let mut opts = AgentOptions {
        max_request_bytes: crate::core::http::MAX_REQUEST_BYTES,
        system: None,
        cwd: std::env::temp_dir(),
        max_turns: 0,
        token_budget: 10_000,
        stream: false,
        compact: None,
        reasoning: None,
        hooks: &crate::agent::ext::Extensions::empty(),
        cache_key: None,
        cache_anchor: None,
        cache_ttl: None,
    };
    let note = context_note(&opts, 3_000).unwrap();
    assert!(
        note.starts_with("<context>") && note.ends_with("</context>"),
        "{note}"
    );
    assert!(
        note.contains("7000 of this task's input-token budget left"),
        "{note}"
    );
    // no task budget: no note at all — how long the conversation may run is
    // the auto-compaction ladder's business, not a number handed to the model
    opts.token_budget = 0;
    assert!(context_note(&opts, 0).is_none());
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
