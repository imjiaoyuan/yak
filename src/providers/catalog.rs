//! The built-in provider catalog, mirroring the provider registry: one row
//! per known API-key provider with its canonical id, endpoint and env var.
//! `yak models add` builds its wizard from it. OAuth-only and
//! cloud-signature (Bedrock, Vertex) endpoints are not
//! catalogued.

/// One catalogued provider.
pub struct Entry {
    pub id: &'static str,
    /// our adapter kind: openai-compat | anthropic
    pub kind: &'static str,
    /// the endpoint the request paths hang off, any version segment
    /// included: the adapters glue `/chat/completions` or `/v1/messages`
    /// onto it verbatim, they never insert a version themselves
    pub base_url: &'static str,
    /// the conventional env var for the API key
    pub env: &'static str,
}

const fn e(
    id: &'static str,
    kind: &'static str,
    base_url: &'static str,
    env: &'static str,
) -> Entry {
    Entry {
        id,
        kind,
        base_url,
        env,
    }
}

pub const ALL: &[Entry] = &[
    // anthropic-protocol vendors
    e(
        "anthropic",
        "anthropic",
        "https://api.anthropic.com",
        "ANTHROPIC_API_KEY",
    ),
    e(
        "minimax",
        "anthropic",
        "https://api.minimax.io/anthropic",
        "MINIMAX_API_KEY",
    ),
    e(
        "minimax-cn",
        "anthropic",
        "https://api.minimaxi.com/anthropic",
        "MINIMAX_CN_API_KEY",
    ),
    // openai-protocol vendors
    e(
        "openai",
        "openai-compat",
        "https://api.openai.com/v1",
        "OPENAI_API_KEY",
    ),
    // the ChatGPT Plus/Pro subscription login (OAuth, no API key): the
    // Codex channel
    e("chatgpt", "codex", "https://chatgpt.com/backend-api", ""),
    // anthropic-named, but it serves the OpenAI shape: /v1/chat/completions
    // answers while /v1/messages is an unrouted 404
    e(
        "ant-ling",
        "openai-compat",
        "https://api.ant-ling.com/v1",
        "ANT_LING_API_KEY",
    ),
    // OpenCode's Go (and Zen) subscription gateway: one key, two wire
    // formats. Most models ride the OpenAI /chat/completions path; a few
    // (MiniMax and friends) speak the Anthropic /v1/messages shape, so the
    // same endpoint is catalogued under both kinds.
    e(
        "opencode-go",
        "openai-compat",
        "https://opencode.ai/zen/go/v1",
        "OPENCODE_API_KEY",
    ),
    e(
        "opencode-go-anthropic",
        "anthropic",
        "https://opencode.ai/zen/go",
        "OPENCODE_API_KEY",
    ),
    // the same gateway without the Go subscription: Zen
    e(
        "opencode",
        "openai-compat",
        "https://opencode.ai/zen/v1",
        "OPENCODE_API_KEY",
    ),
    e(
        "opencode-anthropic",
        "anthropic",
        "https://opencode.ai/zen",
        "OPENCODE_API_KEY",
    ),
    e(
        "deepseek",
        "openai-compat",
        "https://api.deepseek.com",
        "DEEPSEEK_API_KEY",
    ),
    // google's native api is generative-ai; we speak its openai bridge
    e(
        "google",
        "openai-compat",
        "https://generativelanguage.googleapis.com/v1beta/openai",
        "GEMINI_API_KEY",
    ),
    e(
        "groq",
        "openai-compat",
        "https://api.groq.com/openai/v1",
        "GROQ_API_KEY",
    ),
    e(
        "mistral",
        "openai-compat",
        "https://api.mistral.ai/v1",
        "MISTRAL_API_KEY",
    ),
    e(
        "cerebras",
        "openai-compat",
        "https://api.cerebras.ai/v1",
        "CEREBRAS_API_KEY",
    ),
    e(
        "nvidia",
        "openai-compat",
        "https://integrate.api.nvidia.com/v1",
        "NVIDIA_API_KEY",
    ),
    e(
        "huggingface",
        "openai-compat",
        "https://router.huggingface.co/v1",
        "HF_TOKEN",
    ),
    e(
        "together",
        "openai-compat",
        "https://api.together.ai/v1",
        "TOGETHER_API_KEY",
    ),
    e(
        "baseten",
        "openai-compat",
        "https://inference.baseten.co/v1",
        "BASETEN_API_KEY",
    ),
    e(
        "fireworks",
        "openai-compat",
        "https://api.fireworks.ai/inference/v1",
        "FIREWORKS_API_KEY",
    ),
    e("xai", "openai-compat", "https://api.x.ai/v1", "XAI_API_KEY"),
    e(
        "openrouter",
        "openai-compat",
        "https://openrouter.ai/api/v1",
        "OPENROUTER_API_KEY",
    ),
    e(
        "meta",
        "openai-compat",
        "https://api.meta.ai/v1",
        "META_API_KEY",
    ),
    e(
        "moonshotai",
        "openai-compat",
        "https://api.moonshot.ai/v1",
        "MOONSHOT_API_KEY",
    ),
    e(
        "moonshotai-cn",
        "openai-compat",
        "https://api.moonshot.cn/v1",
        "MOONSHOT_API_KEY",
    ),
    e(
        "kimi-coding",
        "openai-compat",
        "https://api.kimi.com/coding/v1",
        "KIMI_API_KEY",
    ),
    e(
        "zai",
        "openai-compat",
        "https://api.z.ai/api/coding/paas/v4",
        "ZAI_API_KEY",
    ),
    e(
        "zai-coding-cn",
        "openai-compat",
        "https://open.bigmodel.cn/api/coding/paas/v4",
        "ZAI_CODING_CN_API_KEY",
    ),
    e(
        "qwen-token-plan",
        "openai-compat",
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
        "QWEN_TOKEN_PLAN_API_KEY",
    ),
    // the same endpoint as qwen-token-plan: the registry lists both the
    // personal and the organization subscription under one alias each, so
    // an id survives either spelling
    e(
        "qwen-token-plan-individual",
        "openai-compat",
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
        "QWEN_TOKEN_PLAN_API_KEY",
    ),
    e(
        "qwen-token-plan-cn",
        "openai-compat",
        "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
        "QWEN_TOKEN_PLAN_CN_API_KEY",
    ),
    e(
        "xiaomi",
        "openai-compat",
        "https://api.xiaomimimo.com/v1",
        "XIAOMI_API_KEY",
    ),
    e(
        "xiaomi-token-plan-cn",
        "openai-compat",
        "https://token-plan-cn.xiaomimimo.com/v1",
        "XIAOMI_TOKEN_PLAN_CN_API_KEY",
    ),
    e(
        "xiaomi-token-plan-ams",
        "openai-compat",
        "https://token-plan-ams.xiaomimimo.com/v1",
        "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
    ),
    e(
        "xiaomi-token-plan-sgp",
        "openai-compat",
        "https://token-plan-sgp.xiaomimimo.com/v1",
        "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
    ),
    e(
        "vercel-ai-gateway",
        "openai-compat",
        "https://ai-gateway.vercel.sh/v1",
        "AI_GATEWAY_API_KEY",
    ),
    // kept additions beyond the registry
    e(
        "siliconflow",
        "openai-compat",
        "https://api.siliconflow.cn/v1",
        "SILICONFLOW_API_KEY",
    ),
    e(
        "siliconflow-com",
        "openai-compat",
        "https://api.siliconflow.com/v1",
        "SILICONFLOW_API_KEY",
    ),
    e(
        "zhipu",
        "openai-compat",
        "https://open.bigmodel.cn/api/paas/v4",
        "ZHIPU_API_KEY",
    ),
    // local runtimes
    e("ollama", "openai-compat", "http://localhost:11434/v1", ""),
    e("lm-studio", "openai-compat", "http://localhost:1234/v1", ""),
    e("llama.cpp", "openai-compat", "http://localhost:8080/v1", ""),
    e("vllm", "openai-compat", "http://localhost:8000/v1", ""),
];

/// The `/models` endpoint a provider kind serves. Anthropic mounts it under
/// `/v1/models`; the openai-compat half talks to bare `/models`.
pub fn models_url(kind: &str, base_url: &str) -> String {
    if kind == "anthropic" {
        format!("{}/v1/models", base_url.trim_end_matches('/'))
    } else {
        format!("{}/models", base_url.trim_end_matches('/'))
    }
}

/// The models endpoint plus its auth headers (an empty key sends no
/// credentials — local runtimes like ollama accept that). The gateway
/// identity headers ride along too: an opencode.ai host answers `/models`
/// with `403 Forbidden` without its `x-opencode-session`, which is why a
/// live list silently stayed empty for those providers.
pub fn fetch_models_url(
    kind: &str,
    base_url: &str,
    api_key: &str,
) -> (String, Vec<(String, String)>) {
    let url = models_url(kind, base_url);
    let mut headers = if api_key.is_empty() {
        Vec::new()
    } else {
        super::auth_headers(kind, api_key)
    };
    headers.extend(crate::core::http::identity_headers());
    headers.extend(super::gateway_headers(&url));
    (url, headers)
}

/// Fetch a provider's model list (OpenAI-compatible `/models` endpoint); the
/// wizard offers what the endpoint actually serves.
pub(crate) fn fetch_model_list(
    url: &str,
    headers: &[(String, String)],
) -> Result<Vec<String>, String> {
    let agent = crate::core::http::short_agent();
    let mut request = agent.get(url);
    for (k, v) in headers {
        request = request.header(k, v);
    }
    let resp = request.call().map_err(|e| e.to_string())?;
    if resp.status().as_u16() >= 400 {
        return Err(format!("HTTP {}", resp.status()));
    }
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut resp.into_body().into_reader(), &mut buf)
        .map_err(|e| e.to_string())?;
    let value: serde_json::Value = serde_json::from_str(&buf).map_err(|e| e.to_string())?;
    let array = value["data"]
        .as_array()
        .or_else(|| value["models"].as_array())
        .ok_or("no model list in response")?;
    Ok(array
        .iter()
        .filter_map(|m| m["id"].as_str().map(String::from))
        .collect())
}

/// Fetch the model list; the reason a provider served none is returned, not
/// swallowed — a rejected key and a provider that publishes no list look
/// identical once the error is gone, and "no models available" hides which
/// one it was.
pub fn fetch_models(kind: &str, base_url: &str, api_key: &str) -> Result<Vec<String>, String> {
    let (url, headers) = fetch_models_url(kind, base_url, api_key);
    fetch_model_list(&url, &headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn every_catalogued_base_url_is_requestable() {
        // a URL with a space reaches ureq as "http: invalid uri character",
        // which the wizard only reports as a failed model fetch — every row
        // must be a URL the /models request can actually be built from
        for entry in ALL {
            assert!(
                entry.base_url.starts_with("http://") || entry.base_url.starts_with("https://"),
                "{}: not an http(s) url: {}",
                entry.id,
                entry.base_url
            );
            assert!(
                !entry.base_url.contains(char::is_whitespace),
                "{}: base url contains whitespace: {}",
                entry.id,
                entry.base_url
            );
            let url = models_url(entry.kind, entry.base_url);
            assert!(
                !url.contains(char::is_whitespace),
                "{}: models url contains whitespace: {}",
                entry.id,
                url
            );
        }
    }

    #[test]
    fn the_zai_family_points_at_the_coding_endpoints() {
        let url_of = |id: &str| {
            ALL.iter()
                .find(|e| e.id == id)
                .unwrap_or_else(|| panic!("missing catalog entry {id}"))
                .base_url
        };
        assert_eq!(url_of("zai"), "https://api.z.ai/api/coding/paas/v4");
        assert_eq!(
            url_of("zai-coding-cn"),
            "https://open.bigmodel.cn/api/coding/paas/v4"
        );
        assert_eq!(url_of("zhipu"), "https://open.bigmodel.cn/api/paas/v4");
    }

    #[test]
    fn a_row_whose_endpoint_is_versioned_carries_the_version() {
        // the adapters glue "/models" and "/chat/completions" onto base_url,
        // so a row whose endpoint lives under a version segment must carry
        // it: without it the wizard's probe answers 404, which is how
        // mistral, fireworks, kimi-coding and vercel-ai-gateway shipped
        let models = |id: &str| {
            let entry = ALL
                .iter()
                .find(|e| e.id == id)
                .unwrap_or_else(|| panic!("missing catalog entry {id}"));
            models_url(entry.kind, entry.base_url)
        };
        for (id, expected) in [
            ("ant-ling", "https://api.ant-ling.com/v1/models"),
            ("mistral", "https://api.mistral.ai/v1/models"),
            ("fireworks", "https://api.fireworks.ai/inference/v1/models"),
            ("kimi-coding", "https://api.kimi.com/coding/v1/models"),
            (
                "vercel-ai-gateway",
                "https://ai-gateway.vercel.sh/v1/models",
            ),
            ("meta", "https://api.meta.ai/v1/models"),
            ("siliconflow-com", "https://api.siliconflow.com/v1/models"),
            ("opencode", "https://opencode.ai/zen/v1/models"),
            ("opencode-anthropic", "https://opencode.ai/zen/v1/models"),
        ] {
            assert_eq!(models(id), expected, "{id}");
        }
    }

    #[test]
    fn an_anthropic_row_does_not_repeat_the_version_segment() {
        // models_url appends /v1/models for the anthropic kind, and the
        // messages adapter appends /v1/messages the same way, so a row that
        // already ends in /v1 would be requested at .../v1/v1/...
        for entry in ALL.iter().filter(|e| e.kind == "anthropic") {
            assert!(
                !entry.base_url.trim_end_matches('/').ends_with("/v1"),
                "{}: anthropic base url already carries the version: {}",
                entry.id,
                entry.base_url
            );
        }
    }

    #[test]
    fn the_models_request_carries_the_gateway_identity_headers() {
        // opencode.ai refuses /models with 403 unless the session header is
        // there — a bare "Bearer + user-agent" request cannot list them
        let (url, headers) =
            fetch_models_url("openai-compat", "https://opencode.ai/zen/go/v1", "sk-test");
        assert_eq!(url, "https://opencode.ai/zen/go/v1/models");
        assert!(header(&headers, "x-opencode-session").is_some());
        assert!(header(&headers, "user-agent").is_some());
        assert_eq!(header(&headers, "authorization"), Some("Bearer sk-test"));

        // ...and ordinary hosts get no session header
        let (url, headers) =
            fetch_models_url("openai-compat", "https://api.deepseek.com", "sk-test");
        assert_eq!(url, "https://api.deepseek.com/models");
        assert!(header(&headers, "x-opencode-session").is_none());
        assert_eq!(header(&headers, "authorization"), Some("Bearer sk-test"));

        // anthropic hangs its list off /v1 and keeps its version header
        let (url, headers) = fetch_models_url("anthropic", "https://api.anthropic.com", "k");
        assert_eq!(url, "https://api.anthropic.com/v1/models");
        assert_eq!(header(&headers, "anthropic-version"), Some("2023-06-01"));

        // no key at all (ollama): no credentials, still identified
        let (_, headers) = fetch_models_url("openai-compat", "http://localhost:11434/v1", "");
        assert!(header(&headers, "authorization").is_none());
        assert!(header(&headers, "user-agent").is_some());
    }
}
