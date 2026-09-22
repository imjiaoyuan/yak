//! `[agent]` settings from config.json.

use std::fs;

use crate::agent::compact::CompactConfig;
use crate::core::config::config_path;

/// `[agent]` settings from config.json — everything optional, defaults live
/// next to their consumers. CLI flags override whatever is set here.
#[derive(Default)]
pub struct AgentSettings {
    pub approval_mode: Option<String>,
    /// the first occupancy that triggers auto-compaction, in tokens; it doubles
    /// after each compaction the session runs
    pub compact_at_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
    /// ceiling on one serialized request body, in bytes; a gateway in front of
    /// the model may refuse far less than the provider documents, and the
    /// refusal that comes back names nothing usable
    pub max_request_bytes: Option<usize>,
    /// how long a provider should hold this conversation's prompt-cache
    /// entries: `5m` (what every provider gives by default) or `1h`. Only
    /// Anthropic's Messages API takes a TTL; the openai-compat wire caches
    /// automatically, with nothing to set.
    pub cache_ttl: Option<String>,
    pub tool_policies: std::collections::BTreeMap<String, String>,
    pub disabled_skills: Vec<String>,
}

impl AgentSettings {
    /// Compaction limits for a run. The trigger is `compact_at_tokens` (64k by
    /// default): one number, the same whatever model serves the run, picked
    /// without knowing anything about the model behind the gateway — and the
    /// loop doubles it after each compaction, so the ladder climbs as the
    /// session is summarized. `keep_recent_tokens` is the tail each compaction
    /// keeps, clamped to half the trigger by `effective_keep_recent`.
    pub fn compact_config(&self) -> CompactConfig {
        CompactConfig {
            trigger_tokens: self.compact_at_tokens.unwrap_or(64_000),
            keep_recent_tokens: self.keep_recent_tokens.unwrap_or(32_000),
        }
    }
}

/// Load the `[agent]` section of config.json. A missing file (or a missing
/// `agent` object) yields defaults; an unreadable or unparsable file fails
/// loudly — the same file, the same rules as `core::config::load`.
pub fn load() -> AgentSettings {
    let path = config_path();
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return AgentSettings::default(),
        Err(e) => {
            eprintln!("Error: cannot read {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    // an empty file behaves like a missing one; anything else must parse —
    // silently defaulting here would hide a corrupt config from the run
    if !raw.trim().is_empty() && serde_json::from_str::<serde_json::Value>(&raw).is_err() {
        eprintln!("Error: failed to parse {}: not valid JSON", path.display());
        std::process::exit(1);
    }
    let settings = parse(&raw);
    // a lifetime no provider knows is a typo, and the request it shapes looks
    // exactly like one that asked for nothing: refuse it here, where the file
    // is read, rather than quietly caching for five minutes
    if let Some(ttl) = settings.cache_ttl.as_deref()
        && !matches!(ttl, "5m" | "1h")
    {
        eprintln!("Error: agent.cache_ttl is '{ttl}' (5m or 1h)");
        std::process::exit(1);
    }
    settings
}

pub fn parse(raw: &str) -> AgentSettings {
    let mut s = AgentSettings::default();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return s;
    };
    let Some(agent) = value.get("agent") else {
        return s;
    };
    s.approval_mode = agent
        .get("approval_mode")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    s.compact_at_tokens = agent.get("compact_at_tokens").and_then(|v| v.as_u64());
    s.keep_recent_tokens = agent.get("keep_recent_tokens").and_then(|v| v.as_u64());
    s.max_request_bytes = agent
        .get("max_request_bytes")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    s.cache_ttl = agent
        .get("cache_ttl")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    if let Some(tools) = agent.get("tools").and_then(|v| v.as_object()) {
        for (name, policy) in tools {
            if let Some(p) = policy.as_str() {
                s.tool_policies.insert(name.clone(), p.to_string());
            }
        }
    }
    if let Some(list) = agent.get("disabled_skills").and_then(|v| v.as_array()) {
        for v in list {
            if let Some(name) = v.as_str() {
                s.disabled_skills.push(name.to_string());
            }
        }
    }
    s
}

#[cfg(test)]
mod agent_settings_tests {
    use super::*;

    /// The ceiling is a gateway property: absent unless configured.
    #[test]
    fn a_request_ceiling_comes_from_the_agent_section() {
        assert_eq!(parse("{}").max_request_bytes, None);
        let s = parse(r#"{"agent": {"max_request_bytes": 8000000}}"#);
        assert_eq!(s.max_request_bytes, Some(8_000_000));
    }

    /// The prompt-cache lifetime is absent unless configured — the provider's
    /// own default is then what applies, and the request says nothing about it.
    #[test]
    fn a_cache_ttl_comes_from_the_agent_section() {
        assert_eq!(parse("{}").cache_ttl, None);
        let s = parse(r#"{"agent": {"cache_ttl": "1h"}}"#);
        assert_eq!(s.cache_ttl.as_deref(), Some("1h"));
        let s = parse(r#"{"agent": {"cache_ttl": "5m"}}"#);
        assert_eq!(s.cache_ttl.as_deref(), Some("5m"));
    }

    #[test]
    fn parses_agent_object() {
        let raw = r#"{
  "providers": {
    "mock": {
      "kind": "openai-compat",
      "base_url": "http://x/v1",
      "models": ["m1"]
    }
  },
  "agent": {
    "approval_mode": "yolo",
    "compact_at_tokens": 250000,
    "keep_recent_tokens": 100,
    "disabled_skills": ["old-thing"],
    "tools": {"bash": "prompt"}
  }
}"#;
        let s = parse(raw);
        assert_eq!(s.approval_mode.as_deref(), Some("yolo"));
        assert_eq!(s.compact_at_tokens, Some(250_000));
        assert_eq!(s.keep_recent_tokens, Some(100));
        assert_eq!(
            s.tool_policies.get("bash").map(String::as_str),
            Some("prompt")
        );
        assert_eq!(s.disabled_skills, vec!["old-thing".to_string()]);
    }

    #[test]
    fn missing_or_invalid_returns_defaults() {
        assert!(
            parse(r#"{"providers": {"x": {"kind": "openai-compat"}}}"#)
                .compact_at_tokens
                .is_none()
        );
        assert!(parse("not json {{{").compact_at_tokens.is_none());
    }
}
