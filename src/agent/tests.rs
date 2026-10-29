use super::*;
use crate::agent::ext::Extensions;
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
/// speaks its dialect. The window is set: the compaction trigger is
/// window-anchored, and an unknown window would leave nothing to anchor.
fn mock_model(port: u16) -> crate::providers::ResolvedModel {
    crate::providers::ResolvedModel {
        provider_name: "mock".into(),
        kind: "openai-compat".into(),
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: Some("sk-x".into()),
        model_id: "m".into(),
        context_window: Some(2_000_000),
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
        trigger_tokens: Some(1_000),
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
    let mut no_skip = false;
    let mut updates: Vec<AgentUpdate> = Vec::new();
    {
        let mut push = |u: AgentUpdate| updates.push(u);
        let mut sink = StallSink::new(&mut warned, &mut push);
        let mut ch = CompactHooks {
            hooks: &Extensions::EMPTY,
            skip_round: &mut no_skip,
            reason: "trigger",
            focus: None,
        };
        let _ = maybe_compact(
            &model,
            &mut history,
            marker,
            Some(&cfg),
            None,
            &mut sink,
            &mut ch,
        );
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
        let mut ch = CompactHooks {
            hooks: &Extensions::EMPTY,
            skip_round: &mut no_skip,
            reason: "trigger",
            focus: None,
        };
        let _ = maybe_compact(
            &model,
            &mut history,
            marker,
            Some(&cfg),
            None,
            &mut sink,
            &mut ch,
        );
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
/// (bounded shape, guarded by compaction); every mock server here terminates
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
        steer: Box::leak(Box::new(|| None)),
    }
}

/// Read one mock-server request: past the request head plus its
/// content-length body (shared through `core::testutil`).
fn read_request(c: &mut std::net::TcpStream) {
    crate::core::testutil::read_request(c)
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
                    b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"plan before answer\",\"content\":\"partial ans\"}}]}\n\n",
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
    assert_eq!(
        outcome.history.len(),
        4,
        "prompt, partial, continuation request, answer"
    );
    assert!(
        matches!(&outcome.history[1], Msg::Assistant { text, tool_calls, reasoning, .. } if text == "partial ans" && tool_calls.is_empty() && reasoning.as_deref() == Some("plan before answer")),
        "the partial answer and reasoning ride the history as a real assistant message"
    );
}

/// Run finite SSE responses and inspect the actual next request, including
/// reasoning replay as required by the DeepSeek thinking wire.
fn stream_recovery_fixture(
    bodies: Vec<String>,
) -> (
    Result<AgentOutcome, AgentFailure>,
    Vec<serde_json::Value>,
    usize,
) {
    stream_recovery_fixture_with_kind("openai-compat", bodies)
}

fn stream_recovery_fixture_with_kind(
    kind: &str,
    bodies: Vec<String>,
) -> (
    Result<AgentOutcome, AgentFailure>,
    Vec<serde_json::Value>,
    usize,
) {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut connection, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(&mut connection);
            let mut length = None;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut request = vec![0; length.expect("request body length")];
            reader.read_exact(&mut request).unwrap();
            requests.push(serde_json::from_slice(&request).unwrap());
            write!(connection, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        requests
    });
    let mut model = mock_model(port);
    model.kind = kind.into();
    if kind == "openai-compat" {
        model
            .options
            .push(("replay_reasoning".into(), "true".into()));
    }
    let executed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let tools: Vec<Box<dyn tools::Tool>> = vec![Box::new(ProbeTool(executed.clone()))];
    let opts = test_opts();
    let mut approval = approval::ApprovalConfig::default();
    let result = run_agent(
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
    );
    let requests = server.join().unwrap();
    let count = executed.lock().unwrap().len();
    (result, requests, count)
}

fn recovery_answer() -> String {
    "data: {\"choices\":[{\"delta\":{\"content\":\"rest\"}}]}\n\ndata: [DONE]\n\n".into()
}

#[test]
fn a_recovery_request_explicitly_asks_for_continuation() {
    let first = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\",\"reasoning_content\":\"plan\"}}]}\n\n".into();
    let (result, requests, _) = stream_recovery_fixture(vec![first, recovery_answer()]);
    let outcome = result.unwrap();
    let messages = requests[1]["messages"].as_array().unwrap();
    assert_eq!(messages[1]["content"], "partial");
    assert_eq!(messages[1]["reasoning_content"], "plan");
    assert_eq!(messages.last().unwrap()["role"], "user");
    assert!(
        messages.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("Continue from where it stopped")
    );
    assert!(
        matches!(&outcome.history[2], Msg::User { text, .. } if text.contains("Continue from where it stopped"))
    );
}

#[test]
fn a_reasoning_only_drop_is_preserved_and_recovered() {
    let first =
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"unfinished plan\"}}]}\n\n".into();
    let (result, requests, _) = stream_recovery_fixture(vec![first, recovery_answer()]);
    let outcome = result.unwrap();
    assert_eq!(outcome.final_text, "rest");
    assert_eq!(
        requests[1]["messages"][1]["reasoning_content"],
        "unfinished plan"
    );
    assert!(
        matches!(&outcome.history[1], Msg::Assistant { reasoning: Some(text), .. } if text == "unfinished plan")
    );
}

#[test]
fn tools_from_a_dropped_stream_are_never_executed() {
    let chunk = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"pending","function":{"name":"probe","arguments":"{\"unfinished\":"}}]}}]});
    let (result, requests, executed) =
        stream_recovery_fixture(vec![format!("data: {chunk}\n\n"), recovery_answer()]);
    let outcome = result.unwrap();
    assert_eq!(executed, 0);
    assert!(
        matches!(&outcome.history[2], Msg::ToolResult { call_id, error: Some(crate::providers::ToolError::Interrupted), .. } if call_id == "pending")
    );
    assert_eq!(requests[1]["messages"][2]["tool_call_id"], "pending");
    assert!(
        requests[1]["messages"][2]["content"]
            .as_str()
            .unwrap()
            .contains("not executed")
    );
}

#[test]
fn a_fatal_stream_error_keeps_the_partial_text_in_the_failure() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\",\"reasoning_content\":\"plan\"}}]}\n\ndata: {\"error\":{\"message\":\"provider refused\"}}\n\n".into();
    let (result, requests, _) = stream_recovery_fixture(vec![body]);
    let failure = result.err().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(failure.message, "provider refused");
    assert_eq!(failure.final_text, "partial");
    assert!(
        matches!(&failure.history[1], Msg::Assistant { reasoning: Some(text), .. } if text == "plan")
    );
}

#[test]
fn a_failure_before_output_still_keeps_the_user_prompt() {
    let body = "data: {\"error\":{\"message\":\"provider refused\"}}\n\n".into();
    let (result, _, _) = stream_recovery_fixture(vec![body]);
    assert_eq!(result.err().unwrap().history, vec![Msg::user("go")]);
}

#[test]
fn a_dropped_gemini_stream_pairs_signed_calls_with_interrupted_results() {
    let signed_call = json!({"functionCall":{"id":"pending","name":"probe","args":{}},"thoughtSignature":"signed-call"});
    let first = json!({"candidates":[{"content":{"parts":[{"text":"partial"},signed_call]}}]});
    let last =
        json!({"candidates":[{"content":{"parts":[{"text":"rest"}]},"finishReason":"STOP"}]});
    let (result, requests, executed) = stream_recovery_fixture_with_kind(
        "gemini",
        vec![format!("data: {first}\n\n"), format!("data: {last}\n\n")],
    );
    result.unwrap();
    assert_eq!(executed, 0);
    assert_eq!(requests[1]["contents"][1]["parts"][1], signed_call);
    let response = &requests[1]["contents"][2]["parts"][0]["functionResponse"];
    assert_eq!(response["id"], "pending");
    assert!(
        response["response"]["error"]
            .as_str()
            .unwrap()
            .contains("not executed")
    );
}

#[test]
fn a_failed_stream_retains_reported_usage() {
    let chunk = json!({"usage":{"prompt_tokens":40,"completion_tokens":5,"prompt_cache_hit_tokens":6},"choices":[{"delta":{"content":"partial"}}]});
    let body = format!("data: {chunk}\n\ndata: {{\"error\":{{\"message\":\"failed\"}}}}\n\n");
    let (result, _, _) = stream_recovery_fixture(vec![body]);
    let usage = result.err().unwrap().usage.unwrap();
    assert_eq!((usage.input, usage.output, usage.cached), (40, 5, 6));
}

#[test]
fn a_failed_gemini_stream_retains_reported_usage() {
    let chunk = json!({"usageMetadata":{"promptTokenCount":40,"candidatesTokenCount":5,"thoughtsTokenCount":3},"candidates":[{"content":{"parts":[{"text":"partial"}]}}]});
    let body = format!("data: {chunk}\n\ndata: {{\"error\":{{\"message\":\"failed\"}}}}\n\n");
    let (result, _, _) = stream_recovery_fixture_with_kind("gemini", vec![body]);
    let usage = result.err().unwrap().usage.unwrap();
    assert_eq!((usage.input, usage.output), (40, 8));
}

/// A length-stopped response never runs its tool calls: streamed arguments
/// are salvaged JSON that may be silently incomplete, so every call in the
/// message fails with the re-issue message and the model goes another round.
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
        trigger_tokens: Some(0),
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
        trigger_tokens: Some(64_000),
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

/// A `compact` subscriber's replacement ends the task: the seed is rebuilt
/// around the extension's summary, the flag round never calls the model, and
/// the verdict is the final text.
#[cfg(unix)]
#[test]
fn an_extension_compaction_replaces_the_builtin_and_ends_the_task() {
    // n=0 would be the summarizer; it must never be hit because the
    // extension answers the compact event instead
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        use std::io::Write as _;
        for conn in listener.incoming().flatten() {
            let mut c = conn;
            read_request(&mut c);
            let _ =
                c.write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\n\r\n{}");
        }
    });
    let ext_host =
        crate::agent::ext::tests::compact_stub_host("replacement summary\nverdict: done");
    let tools: Vec<Box<dyn tools::Tool>> = vec![];
    let mut opts = AgentOptions {
        hooks: &ext_host,
        ..test_opts()
    };
    opts.compact = Some(compact::CompactConfig {
        trigger_tokens: Some(1000),
        keep_recent_tokens: 500,
    });
    let mut approval = approval::ApprovalConfig::default();
    // the ending RoundEnd is the turn persistence writes, so it must carry
    // the whole rebuilt history (the summary plus the kept tail), not just
    // its last message
    let mut rounds: Vec<Vec<Msg>> = Vec::new();
    let mut on_update = |u: AgentUpdate| {
        if let AgentUpdate::RoundEnd { messages, .. } = u {
            rounds.push(messages);
        }
    };
    let outcome = run_agent(
        RunRequest {
            model: &mock_model(port),
            tools: &tools,
            prompt: "go",
            attachments: vec![],
            seed: vec![Msg::user("the task"), Msg::user("x".repeat(20_000))],
            opts: &opts,
        },
        &mut approval,
        RunCallbacks {
            on_update: &mut on_update,
            on_approval: &mut |_| ApprovalResponse::Deny,
            steer: &mut || None,
        },
    )
    .expect("the extension compaction must succeed");
    drop(server);
    assert_eq!(
        outcome.final_text, "replacement summary\nverdict: done",
        "the replacement summary is the last word"
    );
    assert!(
        matches!(outcome.history.first(), Some(Msg::Summary { text }) if text.contains("replacement summary"))
    );
    assert_eq!(outcome.history.len(), 2, "the summary plus the kept tail");
    assert!(
        matches!(&outcome.history[1], Msg::User { text, .. } if text.starts_with('x')),
        "the kept tail survives the fold in memory"
    );
    let stored = rounds.last().expect("the ending RoundEnd");
    assert_eq!(
        stored.len(),
        2,
        "the ending RoundEnd carries the whole rebuilt history"
    );
    assert!(matches!(stored.first(), Some(Msg::Summary { .. })));
    assert!(!outcome.interrupted);
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
        trigger_tokens: Some(64_000),
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
        _log: &mut dyn FnMut(tools::ToolProgress),
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

/// The marker must cover the assistant the round just pushed: the provider
/// already billed its output tokens, so pricing the same message again at
/// chars/4 inflates the estimate and folds a session that still fits. The
/// marker is only observable through the gate, so a tool round is the shortest
/// run that reaches a second gate carrying a large assistant message.
#[test]
fn the_usage_marker_does_not_reprice_the_assistant_it_just_counted() {
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
            let body = if n == 0 {
                // one long assistant text plus a read call; the usage report
                // (500 tokens) covers all of it, so the next gate must not
                // add the text back at chars/4
                let text = "a".repeat(3_000);
                format!(
                    "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c1\",\"function\":{{\"name\":\"read\",\"arguments\":\"{{\\\"path\\\":\\\"/nonexistent-yak\\\"}}\"}}}}]}}}}]}}\n\n\
data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
data: {{\"usage\":{{\"prompt_tokens\":400,\"completion_tokens\":100}}}}\n\n\
data: [DONE]\n\n"
                )
            } else {
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
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
        }
    });
    let model = mock_model(port);
    let tools = tools::builtin_tools();
    let mut opts = test_opts();
    opts.compact = Some(compact::CompactConfig {
        trigger_tokens: Some(1_000),
        keep_recent_tokens: 0,
    });
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
    .expect("the run completes without a spurious compaction");
    drop(server);
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the tool round then the answer; an off-by-one marker folds in between"
    );
    assert!(
        !matches!(outcome.history.first(), Some(Msg::Summary { .. })),
        "the session still fit its trigger, so nothing folded"
    );
}

/// A stub extension that records every `event` frame it is sent to a file
/// (one JSON line each) and advertises the given event names at
/// `initialize`. Sh so it needs nothing beyond the test host; the log path
/// rides the script body.
#[cfg(unix)]
fn event_recorder(name: &str, events: &[&str], log: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let advertised = serde_json::to_string(events).unwrap();
    let body = format!(
        r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *initialize*) printf '{{"id":%s,"result":{{"events":{advertised}}}}}\n' "$id" ;;
    *'"event"'*) printf '%s\n' "$line" >> '{log}' ; printf '{{"id":%s,"result":null}}\n' "$id" ;;
    *) printf '{{"id":%s,"result":null}}\n' "$id" ;;
  esac
done
"#,
        advertised = advertised,
        log = log.display()
    );
    let dir = crate::core::testutil::scratch_dir("approval-event");
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A model that batches one read (auto-allowed) with one bash `rm` call
/// (a seeded ask-list hit, so an ask) in round one, then answers "done".
#[cfg(unix)]
fn serve_read_then_rm_then_done(listener: std::net::TcpListener) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use std::io::Write as _;
        for (n, conn) in listener.incoming().flatten().enumerate() {
            let mut c = conn;
            read_request(&mut c);
            let body = if n == 0 {
                let calls = serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":0,"id":"c1","function":{"name":"read","arguments":"{\"path\":\"x\"}"}},
                    {"index":1,"id":"c2","function":{"name":"bash","arguments":"{\"command\":\"rm /tmp/nowhere\"}"}}
                ]}}]})
                .to_string();
                format!(
                    "data: {calls}\n\n\
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
    })
}

/// A resident extension subscribed to `approval` hears every ask about to
/// surface — tool, preview, matched pattern — before the prompt blocks (the
/// desktop-notification enabler); the auto-allowed read in the same batch
/// fires nothing.
#[test]
#[cfg(unix)]
fn an_approval_ask_reaches_a_subscribed_extension_and_auto_allows_stay_silent() {
    let dir = crate::core::testutil::scratch_dir("approval-event");
    let log = dir.join("seen.jsonl");
    let host = Extensions {
        exts: vec![crate::agent::ext::tests::connect_stub(&event_recorder(
            "approvals",
            &["approval"],
            &log,
        ))],
        script_tools: Vec::new(),
    };
    assert!(host.subscribes("approval"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = serve_read_then_rm_then_done(listener);
    let model = mock_model(port);
    let tools: Vec<Box<dyn tools::Tool>> = tools::builtin_tools();
    let mut opts = test_opts();
    opts.hooks = &host;
    let mut approval = approval::ApprovalConfig {
        blacklist: crate::agent::blacklist::Blacklist::parse("rm"),
        ..approval::ApprovalConfig::default()
    };
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
        RunCallbacks {
            on_update: &mut |_| {},
            on_approval: &mut |_| ApprovalResponse::Allow,
            steer: &mut || None,
        },
    )
    .expect("the ask is answered by the callback");
    server.join().unwrap();
    assert_eq!(outcome.final_text, "done");

    let seen = std::fs::read_to_string(&log).unwrap();
    let events: Vec<serde_json::Value> = seen
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["params"].clone()
        })
        .collect();
    assert_eq!(events.len(), 1, "exactly the bash ask fired: {seen}");
    assert_eq!(events[0]["tool"], "bash");
    assert_eq!(events[0]["pattern"], "rm");
    assert!(
        events[0]["preview"].as_str().unwrap_or("").contains("rm"),
        "the preview names the command"
    );
}

/// A host with no `approval` subscriber sends no frame: `subscribes` gates
/// the payload build, so the quiet path costs nothing.
#[test]
fn an_unsubscribed_host_never_builds_approval_frames() {
    assert!(!empty_extensions().subscribes("approval"));
    assert!(!empty_extensions().subscribes("agent_end"));
}

/// `/compact` folds the seed on demand: the summarizer is called whatever the
/// trigger says, and the instructions ride into its prompt as the focus.
#[test]
fn manual_compaction_folds_the_seed_with_the_focus() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader, Write};
        for conn in listener.incoming().flatten() {
            let mut c = conn;
            let mut reader = BufReader::new(c.try_clone().unwrap());
            let mut head = String::new();
            let mut length = 0usize;
            for _ in 0..16 {
                head.clear();
                if reader.read_line(&mut head).unwrap() == 0 {
                    break;
                }
                if let Some(v) = head
                    .strip_prefix("content-length:")
                    .map(str::trim)
                    .and_then(|v| v.parse::<usize>().ok())
                {
                    length = v;
                }
                if head == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0u8; length];
            use std::io::Read as _;
            let _ = reader.read_exact(&mut body);
            seen2
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&body).to_string());
            let out = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"the summary\"}}]}\n\n\
                       data: [DONE]\n\n";
            let _ = c.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{out}",
                    out.len()
                )
                .as_bytes(),
            );
            // the run settles on the second round: stop serving so `join`
            // below can return (the pattern every mock here follows)
            if seen2.lock().unwrap().len() >= 2 {
                break;
            }
        }
    });
    let model = mock_model(port);
    let opts = test_opts();
    let cfg = compact::CompactConfig {
        trigger_tokens: Some(0),
        keep_recent_tokens: 0,
    };
    let mut history = vec![Msg::user("the task"), Msg::user("x".repeat(20))];
    let mut updates = Vec::new();
    {
        let mut push = |u: AgentUpdate| updates.push(u);
        let removed = crate::agent::compact_manual(
            &model,
            &mut history,
            &cfg,
            Some("keep the file names"),
            &opts,
            &mut push,
        )
        .expect("the manual compaction runs");
        assert_eq!(removed, 1, "the prefix folded below the summary");
    }
    assert!(
        matches!(history.first(), Some(Msg::Summary { text }) if text.contains("the summary")),
        "the prefix became the summary"
    );
    assert_eq!(history.len(), 2, "the summary plus the kept tail");
    let bodies = seen.lock().unwrap();
    assert!(
        bodies[0].contains("Additional focus: keep the file names"),
        "the focus rode into the summarizer prompt"
    );
    assert!(
        updates
            .iter()
            .any(|u| matches!(u, AgentUpdate::Compacted { .. })),
        "the fold reported itself"
    );
}

/// A seed that is only a summary is already compacted: a second fold would
/// summarize the summary, so it refuses instead.
#[test]
fn manual_compaction_refuses_an_already_compacted_seed() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut c = conn;
            read_request(&mut c);
            let _ = std::io::Write::write_all(
                &mut c,
                b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\n\r\n{}",
            );
        }
    });
    let model = mock_model(port);
    let opts = test_opts();
    let cfg = compact::CompactConfig {
        trigger_tokens: None,
        keep_recent_tokens: 0,
    };
    let mut history = vec![Msg::Summary {
        text: "already folded".to_string(),
    }];
    let err = crate::agent::compact_manual(&model, &mut history, &cfg, None, &opts, &mut |_| {})
        .expect_err("nothing is left to fold");
    assert_eq!(
        err,
        "no turn boundary can be dropped without orphaning a tool result"
    );
}

/// The extension `compact` event carries the manual reason and the
/// instructions, so a replacement compaction covers `/compact` too.
#[cfg(unix)]
#[test]
fn manual_compaction_reaches_the_extension_with_instructions() {
    let ext_host = crate::agent::ext::tests::compact_stub_host("focused replacement");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut c = conn;
            read_request(&mut c);
            let _ = std::io::Write::write_all(
                &mut c,
                b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\n\r\n{}",
            );
        }
    });
    let model = mock_model(port);
    let mut opts = test_opts();
    opts.hooks = &ext_host;
    let cfg = compact::CompactConfig {
        trigger_tokens: Some(0),
        keep_recent_tokens: 0,
    };
    let mut history = vec![Msg::user("the task"), Msg::user("y".repeat(20))];
    let removed = crate::agent::compact_manual(
        &model,
        &mut history,
        &cfg,
        Some("keep the error codes"),
        &opts,
        &mut |_| {},
    )
    .expect("the extension answered the compact event");
    assert_eq!(removed, 1);
    assert!(
        matches!(history.first(), Some(Msg::Summary { text }) if text == "focused replacement"),
        "the extension's summary replaced the prefix"
    );
}

#[test]
fn an_unknown_provider_fails_without_stream_recovery() {
    let mut model = mock_model(9);
    model.kind = "unknown-wire".into();
    let opts = test_opts();
    let mut approval = approval::ApprovalConfig::default();
    let mut recovered = 0;
    let mut update = |event| {
        if matches!(event, AgentUpdate::StreamRecovered { .. }) {
            recovered += 1;
        }
    };
    let mut deny = |_: ApprovalRequest<'_>| ApprovalResponse::Deny;
    let result = run_agent(
        RunRequest {
            model: &model,
            tools: &[],
            prompt: "go",
            attachments: vec![],
            seed: vec![],
            opts: &opts,
        },
        &mut approval,
        RunCallbacks {
            on_update: &mut update,
            on_approval: &mut deny,
            steer: &mut || None,
        },
    );
    let error = match result {
        Ok(_) => panic!("unknown provider must fail"),
        Err(error) => error,
    };
    assert_eq!(error.message, "Unknown provider kind: unknown-wire");
    assert_eq!(recovered, 0);
}

/// A steering message handed back after a tool round becomes the next
/// round's prompt: the model sees it after its tool calls, before the next
/// request — never spliced into a round in flight. A plain tool-free round
/// settles the run without polling (the next message is the caller's).
#[test]
fn a_steered_message_runs_as_the_next_rounds_prompt() {
    use std::io::{Read, Write};
    // one request per connection, like every mock here: read the head and
    // body, remember the body, answer with the scripted round
    let read_and_body = |c: &mut std::net::TcpStream| -> String {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if c.read(&mut byte).unwrap_or(0) == 0 {
                break;
            }
            buf.push(byte[0]);
            let Some(i) = buf.windows(4).rposition(|w| w == b"\r\n\r\n") else {
                continue;
            };
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
        let text = String::from_utf8_lossy(&buf).into_owned();
        text.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen2 = seen.clone();
    let server = std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut c = conn;
            let body = read_and_body(&mut c);
            seen2.lock().unwrap().push(body);
            // round 1 calls a tool (a read-tier no-op), so the loop reaches
            // the steer poll; round 2 answers plain and settles the run
            let out = if seen2.lock().unwrap().len() == 1 {
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"noop\",\"arguments\":\"{}\"}}]}}]}\n\n\
                 data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                 data: [DONE]\n\n"
            } else {
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"steered done\"}}]}\n\n\
                 data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                 data: [DONE]\n\n"
            };
            let _ = c.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{out}",
                    out.len()
                )
                .as_bytes(),
            );
            // the run settles on the second round: stop serving so `join`
            // below can return (the pattern every mock here follows)
            if seen2.lock().unwrap().len() >= 2 {
                break;
            }
        }
    });
    let model = mock_model(port);
    // the no-op read-tier tool every steered round calls
    struct Noop;
    impl tools::Tool for Noop {
        fn name(&self) -> &str {
            "noop"
        }
        fn tier(&self) -> super::approval::Tier {
            super::approval::Tier::Read
        }
        fn description(&self) -> &str {
            "no-op"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn preview(&self, _args: &serde_json::Value) -> String {
            "noop".into()
        }
        fn execute(
            &self,
            _args: &serde_json::Value,
            _cwd: &std::path::Path,
            _log: &mut dyn FnMut(tools::ToolProgress),
        ) -> tools::ToolOutput {
            tools::ToolOutput::ok("nothing")
        }
    }
    let tools: Vec<Box<dyn tools::Tool>> = vec![Box::new(Noop)];
    let opts = test_opts();
    let mut approval = approval::ApprovalConfig::default();
    // steer exactly once: the first poll (after round 1's tool) hands the
    // message over, the second (never reached — round 2 settles) would not
    let mut steered = false;
    let mut steer = || {
        if steered {
            None
        } else {
            steered = true;
            Some(("and now this".to_string(), Vec::new()))
        }
    };
    let mut update = |_: AgentUpdate| {};
    let mut deny = |_: ApprovalRequest<'_>| ApprovalResponse::Deny;
    let outcome = run_agent(
        RunRequest {
            model: &model,
            tools: &tools,
            prompt: "first",
            attachments: vec![],
            seed: vec![],
            opts: &opts,
        },
        &mut approval,
        RunCallbacks {
            on_update: &mut update,
            on_approval: &mut deny,
            steer: &mut steer,
        },
    )
    .expect("the steered run completes");
    let prompts = seen.lock().unwrap().clone();
    assert_eq!(
        prompts.len(),
        2,
        "the steered message must drive a second request: {prompts:?}"
    );
    assert!(prompts[1].contains("and now this"), "{prompts:?}");
    // both user messages are in the final history, in order
    let texts: Vec<&str> = outcome
        .history
        .iter()
        .filter_map(|m| match m {
            Msg::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["first", "and now this"]);
    server.join().unwrap();
}
