//! `[agent]` settings from config.json.

use std::fs;

use crate::core::config::config_path;

/// `[agent]` settings from config.json — everything optional, defaults live
/// next to their consumers. CLI flags override whatever is set here.
#[derive(Default)]
pub struct AgentSettings {
    pub approval_mode: Option<String>,
    pub context_window: Option<u64>,
    pub reserve_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
    pub tool_policies: std::collections::BTreeMap<String, String>,
    pub roles: std::collections::BTreeMap<String, String>,
    pub model_windows: std::collections::BTreeMap<String, u64>,
    pub disabled_skills: Vec<String>,
    /// "auto" = extract memories when the REPL exits; default manual/off
    pub memory: Option<String>,
}

pub fn load() -> AgentSettings {
    let raw = fs::read_to_string(config_path()).unwrap_or_default();
    parse(&raw)
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
    s.context_window = agent.get("context_window").and_then(|v| v.as_u64());
    s.reserve_tokens = agent.get("reserve_tokens").and_then(|v| v.as_u64());
    s.keep_recent_tokens = agent.get("keep_recent_tokens").and_then(|v| v.as_u64());
    if let Some(tools) = agent.get("tools").and_then(|v| v.as_object()) {
        for (name, policy) in tools {
            if let Some(p) = policy.as_str() {
                s.tool_policies.insert(name.clone(), p.to_string());
            }
        }
    }
    if let Some(roles) = agent.get("roles").and_then(|v| v.as_object()) {
        for (role, model) in roles {
            if let Some(m) = model.as_str() {
                s.roles.insert(role.clone(), m.to_string());
            }
        }
    }
    if let Some(models) = agent.get("models").and_then(|v| v.as_object()) {
        for (model, table) in models {
            if let Some(w) = table.get("context_window").and_then(|v| v.as_u64()) {
                s.model_windows.insert(model.clone(), w);
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
    s.memory = agent
        .get("memory")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    s
}

#[cfg(test)]
mod agent_settings_tests {
    use super::*;

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
    "context_window": 5000,
    "reserve_tokens": 100,
    "keep_recent_tokens": 100,
    "disabled_skills": ["old-thing"],
    "memory": "auto",
    "tools": {"bash": "prompt"},
    "roles": {"task": "mock/m1"},
    "models": {"mock/m1": {"context_window": 9000}}
  }
}"#;
        let s = parse(raw);
        assert_eq!(s.approval_mode.as_deref(), Some("yolo"));
        assert_eq!(s.context_window, Some(5000));
        assert_eq!(s.reserve_tokens, Some(100));
        assert_eq!(s.keep_recent_tokens, Some(100));
        assert_eq!(
            s.tool_policies.get("bash").map(String::as_str),
            Some("prompt")
        );
        assert_eq!(s.roles.get("task").map(String::as_str), Some("mock/m1"));
        assert_eq!(s.model_windows.get("mock/m1"), Some(&9000));
        assert_eq!(s.disabled_skills, vec!["old-thing".to_string()]);
        assert_eq!(s.memory.as_deref(), Some("auto"));
    }

    #[test]
    fn missing_or_invalid_returns_defaults() {
        assert!(
            parse(r#"{"providers": {"x": {"kind": "openai-compat"}}}"#)
                .context_window
                .is_none()
        );
        assert!(parse("not json {{{").context_window.is_none());
    }
}
