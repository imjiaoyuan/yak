use super::*;
use serde_json::json;

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
fn gateway_headers_gate_the_session_id() {
    let go = gateway_headers("https://opencode.ai/zen/go/v1/chat/completions");
    assert_eq!(go.len(), 1);
    assert_eq!(go[0].0, "x-opencode-session");
    assert!(!go[0].1.is_empty());
    let other = gateway_headers("https://api.openai.com/v1/chat/completions");
    assert!(other.is_empty());
}

#[test]
fn one_session_id_is_reused_for_the_whole_process() {
    assert_eq!(session_id(), session_id());
}

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
            reasoning_meta: None,
        },
        Msg::Assistant {
            text: "done".into(),
            tool_calls: vec![ToolCall {
                id: "b".into(),
                name: "read".into(),
                arguments: json!({}),
            }],
            reasoning: None,
            reasoning_meta: None,
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

fn rm(provider: &str, model_id: &str) -> ResolvedModel {
    ResolvedModel {
        provider_name: provider.into(),
        kind: "openai-compat".into(),
        base_url: "http://localhost".into(),
        api_key: None,
        model_id: model_id.into(),
        context_window: None,
        options: Vec::new(),
    }
}

#[test]
fn glm_text_models_are_text_only() {
    // z.ai's coding endpoint 400s (code 1210) on any non-text content
    // part, so glm-4.6/4.7/5.x must be treated as text-only; the -v
    // vision variants and the natively multimodal glm-5.3-flash stay
    // vision-capable
    for id in [
        "glm-4.6",
        "glm-4.7",
        "glm-4.7-flash",
        "glm-5",
        "glm-5.1",
        "glm-5.2",
        "glm-5.3",
    ] {
        assert!(!rm("zai", id).supports_images(), "{id} should be text-only");
    }
    assert!(rm("zai", "glm-4.5v").supports_images());
    assert!(rm("zai", "glm-4.6v").supports_images());
    assert!(rm("zai", "glm-5.3-flash").supports_images());
    assert!(rm("opencode-go", "glm-5.3-flash").supports_images());
}

fn attachment(name: &str, base64_len: usize) -> Attachment {
    Attachment {
        mime_type: "image/png".into(),
        base64_data: "A".repeat(base64_len),
        filename: Some(name.into()),
        path: Some(format!("/tmp/mg/{name}")),
        url: None,
    }
}

#[test]
fn a_body_within_budget_is_left_to_the_provider() {
    let history = vec![Msg::user_with("look", vec![attachment("m05.png", 4096)])];
    let input = testutil::input(&history, &[]);
    assert!(check_request_body("{}", &input).is_ok());
    // exactly at the cap is still sendable, like the per-attachment one
    let at = "x".repeat(http::MAX_REQUEST_BYTES);
    assert!(check_request_body(&at, &input).is_ok());
}

#[test]
fn an_oversized_body_is_refused_locally_with_the_offenders_named() {
    // eleven screenshots of ~3MB base64: each is far under the
    // per-attachment cap, the body is not — which is how a gateway's
    // opaque 413 happens with nothing in it to act on
    let shots: Vec<Attachment> = (0..11)
        .map(|i| attachment(&format!("m{i:02}.png"), 3 * 1024 * 1024))
        .collect();
    let history = vec![Msg::user_with("look", shots)];
    let input = testutil::input(&history, &[]);
    let body = "x".repeat(http::MAX_REQUEST_BYTES + 1);
    let err = check_request_body(&body, &input).unwrap_err();
    assert!(err.contains("over the 32.0 MB limit"), "{err}");
    assert!(err.contains("11 attachment(s) carry 33.0 MB"), "{err}");
    // the three largest are named by file name, with their own size
    assert!(err.contains("m00.png (3.0 MB)"), "{err}");
    assert!(err.contains(", …"), "{err}");
    // and the remedy: compaction cannot reclaim attachment bytes
    assert!(err.contains("start a new conversation"), "{err}");
}

#[test]
fn a_configured_ceiling_replaces_the_default() {
    // the ceiling is configuration, not a constant: a gateway in front of the
    // provider may refuse far less than the provider itself documents
    let history = vec![Msg::user("go")];
    let mut input = testutil::input(&history, &[]);
    input.max_request_bytes = 1024;
    let body = "x".repeat(1025);
    let err = check_request_body(&body, &input).unwrap_err();
    assert!(err.contains("over the 1.0 KB limit"), "{err}");
    // the default ceiling would have let the same body through
    input.max_request_bytes = http::MAX_REQUEST_BYTES;
    assert!(check_request_body(&body, &input).is_ok());
}

#[test]
fn heavy_attachments_are_flagged_before_the_request_is_built() {
    let limit = 1000;
    // comfortably inside the ceiling: nothing worth saying
    assert!(attachment_weight(&[attachment("a.png", 100)], limit).is_none());
    let near = attachment_weight(&[attachment("a.png", 800)], limit).unwrap();
    assert!(near.contains("800 B"), "{near}");
    assert!(near.contains("most of the 1000 B"), "{near}");
    // over it: the pre-flight will refuse the body, so this says so plainly
    let over = attachment_weight(&[attachment("a.png", 1200)], limit).unwrap();
    assert!(over.contains("over the 1000 B request limit"), "{over}");
    assert!(over.contains("would be refused"), "{over}");
}

#[test]
fn a_body_over_budget_without_attachments_says_so() {
    let history = vec![Msg::user("go")];
    let input = testutil::input(&history, &[]);
    let body = "x".repeat(http::MAX_REQUEST_BYTES + 1);
    let err = check_request_body(&body, &input).unwrap_err();
    assert!(err.contains("no attachment explains it"), "{err}");
}

#[test]
fn tool_result_images_flush_at_the_run_end_never_the_tail() {
    use testutil::att;
    // the "image received" loop: a screenshot rode a read result, the conversation
    // moved on — later rounds must not re-meet the image as fresh input
    let img = Msg::ToolResult {
        call_id: "r".into(),
        name: "read".into(),
        content: "screenshot".into(),
        error: None,
        attachments: vec![att("image/png", Some("paste.png"))],
    };
    let mut later = vec![Msg::user("next question")];
    later.push(Msg::assistant("answer"));
    let history = vec![Msg::user("look"), Msg::assistant(""), img]
        .into_iter()
        .chain(later)
        .collect::<Vec<_>>();
    let input = testutil::input(&history, &[]);
    let body = openai_compat::build_body(&testutil::model("openai-compat"), &input, false).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    // the image user-message sits right after its tool result, before the
    // next user turn — not appended at the tail as the newest input
    let img_pos = msgs
        .iter()
        .position(|m| {
            m["content"].is_array()
                && m["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p["type"] == "image_url")
        })
        .expect("image rides a user message");
    let tail = &msgs[img_pos + 1..];
    assert!(
        tail.iter()
            .all(|m| m["role"] != "user" || m["content"].is_string()),
        "no image-bearing user message after the run: {tail:?}"
    );
    // the image message lands between its tool result and the next user
    // turn, and the array ends on the round's own prompt, not on the image
    assert_eq!(msgs[img_pos - 1]["role"], "tool");
    assert_eq!(msgs[img_pos + 1]["content"], json!("next question"));
    assert_eq!(msgs.last().unwrap()["role"], "user");
    assert_eq!(msgs.last().unwrap()["content"], json!("go"));
}

#[test]
fn consecutive_tool_result_images_share_one_user_message() {
    use testutil::att;
    let two = |id: &str| Msg::ToolResult {
        call_id: id.into(),
        name: "read".into(),
        content: "img".into(),
        error: None,
        attachments: vec![att("image/png", Some("p.png"))],
    };
    let history = vec![Msg::user("look"), Msg::assistant(""), two("a"), two("b")];
    let input = testutil::input(&history, &[]);
    let body = openai_compat::build_body(&testutil::model("openai-compat"), &input, false).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    let img_msgs = msgs
        .iter()
        .filter(|m| {
            m["role"] == "user"
                && m["content"].is_array()
                && m["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p["type"] == "image_url")
        })
        .count();
    assert_eq!(img_msgs, 1, "one user message carries both images");
    // it lands after the run of tool results and before the prompt
    let pos = msgs
        .iter()
        .position(|m| {
            m["role"] == "user"
                && m["content"].is_array()
                && m["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p["type"] == "image_url")
        })
        .unwrap();
    assert_eq!(msgs[pos - 1]["role"], "tool");
    assert_eq!(msgs[pos + 1]["role"], "user");
}

#[test]
fn project_default_beats_the_stored_default() {
    let cfg = crate::core::config::Config {
        providers: [(
            "p".to_string(),
            crate::core::config::Provider {
                kind: "openai".to_string(),
                base_url: "https://p.test/v1".to_string(),
                api_key: None,
                models: vec!["m1".to_string(), "m2".to_string()],
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let dir = crate::core::testutil::scratch_dir("proj-model");
    let proj = dir.join("proj");
    std::fs::create_dir_all(proj.join(".yak")).unwrap();
    std::fs::write(
        proj.join(".yak/config.json"),
        r#"{"models": {"default": "p/m2"}}"#,
    )
    .unwrap();
    assert_eq!(
        project_default_model_at(&cfg, &proj),
        Some("p/m2".to_string())
    );
    // outside the project: nothing
    assert_eq!(project_default_model_at(&cfg, &dir), None);
}

#[test]
fn an_unresolvable_project_default_is_dropped() {
    let cfg = crate::core::config::Config {
        providers: [(
            "p".to_string(),
            crate::core::config::Provider {
                kind: "openai".to_string(),
                base_url: "https://p.test/v1".to_string(),
                api_key: None,
                models: vec!["m1".to_string()],
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let dir = crate::core::testutil::scratch_dir("proj-model-bad");
    std::fs::create_dir_all(dir.join(".yak")).unwrap();
    std::fs::write(
        dir.join(".yak/config.json"),
        r#"{"models": {"default": "ghost/nope"}}"#,
    )
    .unwrap();
    assert_eq!(project_default_model_at(&cfg, &dir), None);
}

#[test]
fn a_git_root_bounds_the_project_walk() {
    let cfg = crate::core::config::Config::default();
    let dir = crate::core::testutil::scratch_dir("proj-model-bound");
    // outer .yak with a default, inner git repo without one
    std::fs::create_dir_all(dir.join(".yak")).unwrap();
    std::fs::write(
        dir.join(".yak/config.json"),
        r#"{"models": {"default": "x"}}"#,
    )
    .unwrap();
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir(repo.join(".git")).unwrap();
    assert_eq!(project_default_model_at(&cfg, &repo), None);
}

#[test]
fn the_closest_path_pin_wins() {
    let cfg = crate::core::config::Config {
        providers: [(
            "p".to_string(),
            crate::core::config::Provider {
                kind: "openai".to_string(),
                base_url: "https://p.test/v1".to_string(),
                api_key: None,
                models: vec!["m1".to_string(), "m2".to_string()],
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let dir = crate::core::testutil::scratch_dir("pins-closest");
    let pins = dir.join("model-pins.json");
    std::fs::write(
        &pins,
        serde_json::json!({
            dir.join("work").display().to_string(): "p/m1",
            dir.join("work/a/deep").display().to_string(): "p/m2",
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        path_pinned_model_at(&cfg, &pins, &dir.join("work/a/deep/src")),
        Some("p/m2".to_string())
    );
    assert_eq!(
        path_pinned_model_at(&cfg, &pins, &dir.join("work/b")),
        Some("p/m1".to_string())
    );
    // outside every pin: nothing
    assert_eq!(
        path_pinned_model_at(&cfg, &pins, &dir.join("elsewhere")),
        None
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn path_pins_match_by_component_not_bytes() {
    let cfg = crate::core::config::Config::default();
    let dir = crate::core::testutil::scratch_dir("pins-bytes");
    let pins = dir.join("model-pins.json");
    std::fs::write(
        &pins,
        serde_json::json!({dir.join("a/b").display().to_string(): "p/m1"}).to_string(),
    )
    .unwrap();
    // /a/bc is a sibling, not a child, of /a/b
    assert_eq!(path_pinned_model_at(&cfg, &pins, &dir.join("a/bc")), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unresolvable_path_pin_is_dropped() {
    let cfg = crate::core::config::Config::default();
    let dir = crate::core::testutil::scratch_dir("pins-bad");
    let pins = dir.join("model-pins.json");
    std::fs::write(
        &pins,
        serde_json::json!({dir.display().to_string(): "ghost/nope"}).to_string(),
    )
    .unwrap();
    assert_eq!(path_pinned_model_at(&cfg, &pins, &dir), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn set_path_pin_round_trips_and_removes() {
    let cfg = crate::core::config::Config {
        providers: [(
            "p".to_string(),
            crate::core::config::Provider {
                kind: "openai".to_string(),
                base_url: "https://p.test/v1".to_string(),
                api_key: None,
                models: vec!["m1".to_string(), "m2".to_string()],
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let dir = crate::core::testutil::scratch_dir("pins-write");
    let pins = dir.join("model-pins.json");
    set_path_pin_at(&pins, &dir.join("work").display().to_string(), Some("p/m1")).unwrap();
    set_path_pin_at(
        &pins,
        &dir.join("work/a").display().to_string(),
        Some("p/m2"),
    )
    .unwrap();
    // insertion order holds and both pins read back by prefix
    let expected = format!(
        "{{\n  {}: \"p/m1\",\n  {}: \"p/m2\"\n}}\n",
        serde_json::to_string(&dir.join("work").display().to_string()).unwrap(),
        serde_json::to_string(&dir.join("work/a").display().to_string()).unwrap()
    );
    assert_eq!(std::fs::read_to_string(&pins).unwrap(), expected);
    assert_eq!(
        path_pinned_model_at(&cfg, &pins, &dir.join("work/a/src")),
        Some("p/m2".to_string())
    );
    set_path_pin_at(&pins, &dir.join("work/a").display().to_string(), None).unwrap();
    assert_eq!(
        path_pinned_model_at(&cfg, &pins, &dir.join("work/a/src")),
        Some("p/m1".to_string())
    );
    // removing the last pin leaves an empty table, not a missing file
    set_path_pin_at(&pins, &dir.join("work").display().to_string(), None).unwrap();
    assert_eq!(std::fs::read_to_string(&pins).unwrap(), "{}\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn set_path_pin_refuses_a_malformed_table() {
    let dir = crate::core::testutil::scratch_dir("pins-malformed");
    let pins = dir.join("model-pins.json");
    std::fs::write(&pins, "[1, 2]").unwrap();
    assert!(set_path_pin_at(&pins, "/tmp/whatever", Some("p/m1")).is_err());
    // the file is untouched, not rewritten by guesswork
    assert_eq!(std::fs::read_to_string(&pins).unwrap(), "[1, 2]");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stored_context_window_survives_the_cli_option_merge() {
    let mut cfg = crate::core::config::Config::default();
    cfg.providers.insert(
        "p".to_string(),
        crate::core::config::Provider {
            kind: "openai".to_string(),
            base_url: "https://p.test/v1".to_string(),
            api_key: None,
            models: vec!["m1".to_string()],
        },
    );
    cfg.model_options.insert(
        "p/m1".to_string(),
        [("context_window".to_string(), "20000".to_string())]
            .into_iter()
            .collect(),
    );
    // the tail resolve lifts the stored window into the model
    let model = resolve_model_tail(&cfg, "p/m1", |_| None).expect("resolves");
    assert_eq!(model.context_window, Some(20000));
    assert!(model.options.is_empty(), "metadata never rides the wire");
    // re-running the lift (what the CLI -o merge path does when no pair
    // names it) must not erase what the stored table set
    let mut again = ResolvedModel::from_config("p", cfg.providers.get("p").unwrap(), "m1", None);
    again.context_window = model.context_window;
    if again.options.iter().any(|(k, _)| k == "context_window") {
        again.context_window = ResolvedModel::take_context_window(&mut again.options).unwrap();
    }
    assert_eq!(again.context_window, Some(20000));
    // a CLI pair replaces it, not layers under it
    let mut cli = ResolvedModel::from_config("p", cfg.providers.get("p").unwrap(), "m1", None);
    cli.options
        .push(("context_window".to_string(), "999".to_string()));
    cli.context_window = ResolvedModel::take_context_window(&mut cli.options).unwrap();
    assert_eq!(cli.context_window, Some(999));
}

fn responses_stream(body: &str) -> (Result<(), String>, Vec<Value>) {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let body = body.to_owned();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        crate::core::testutil::read_request(&mut conn);
        write!(conn, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
    });
    let req = HttpRequest {
        url: format!("http://{addr}/responses"),
        headers: vec![],
        body: "{}".into(),
    };
    let mut events = Vec::new();
    let result = stream_events(
        &req,
        &std::sync::atomic::AtomicBool::new(false),
        |_, value| events.push(value.clone()),
    );
    server.join().unwrap();
    (result, events)
}

#[test]
fn responses_terminal_events_finish_without_done_sentinel() {
    for kind in ["response.done", "response.completed", "response.incomplete"] {
        let terminal = json!({"type": kind, "response": {"usage": {"input_tokens": 12}}});
        let (result, events) = responses_stream(&format!("data: {terminal}\n\n"));
        assert_eq!(result, Ok(()));
        assert_eq!(events, vec![terminal]);
    }
}

#[test]
fn responses_failure_surfaces_the_server_message() {
    let (result, _) = responses_stream(
        "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"quota exhausted\"}}}\n\n",
    );
    assert_eq!(result, Err("quota exhausted".into()));
}

#[test]
fn responses_stream_without_terminal_event_is_truncated() {
    let (result, _) = responses_stream("data: {\"type\":\"response.created\"}\n\n");
    assert_eq!(result, Err(TRUNCATED_STREAM.into()));
}
