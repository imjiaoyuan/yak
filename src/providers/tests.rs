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
fn a_body_over_budget_without_attachments_says_so() {
    let history = vec![Msg::user("go")];
    let input = testutil::input(&history, &[]);
    let body = "x".repeat(http::MAX_REQUEST_BYTES + 1);
    let err = check_request_body(&body, &input).unwrap_err();
    assert!(err.contains("no attachment explains it"), "{err}");
}
