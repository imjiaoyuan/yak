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
    // the "图收到" loop: a screenshot rode a read result, the conversation
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
