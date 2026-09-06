//! user_dir resolution and the files kept there: config.json (providers with
//! inline api keys, the "models" settings family and hand-added tables like
//! "agent").

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::jsonfmt;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    /// snapshot of aliases.json taken once per `load()`, so per-model
    /// resolution does not re-read it from disk
    #[serde(skip)]
    pub aliases: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Provider {
    pub kind: String,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

/// Platform user directory: LLM_USER_PATH env override, else ~/.llm
/// (matching the project-level .llm/ convention).
pub fn user_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LLM_USER_PATH") {
        let path = PathBuf::from(dir);
        let _ = fs::create_dir_all(&path);
        return path;
    }
    let home = std::env::var_os("HOME").or_else(|| {
        if cfg!(windows) {
            std::env::var_os("USERPROFILE")
        } else {
            None
        }
    });
    let path = match home {
        Some(home) => PathBuf::from(home).join(".llm"),
        None => PathBuf::from(".llm"),
    };
    let _ = fs::create_dir_all(&path);
    path
}

pub fn config_path() -> PathBuf {
    user_dir().join("config.json")
}

/// Prompt logging switch: the "logging" boolean in config.json (absent =
/// on).
pub fn logs_on() -> bool {
    read_root()
        .ok()
        .and_then(|root| root.get("logging").and_then(|v| v.as_bool()))
        .unwrap_or(true)
}

pub fn set_logs_enabled(on: bool) {
    let _ = edit_root(|root| {
        if let Some(map) = root.as_object_mut() {
            map.insert("logging".to_string(), serde_json::json!(on));
        }
        Ok(())
    });
}

pub fn logs_db_path() -> PathBuf {
    user_dir().join("logs.db")
}

/// Expand `${VAR}` references in provider api keys, falling back to the
/// empty string when the variable is unset.
pub fn expand_env(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        match rest.find('}') {
            Some(end) => {
                let var = &rest[..end];
                out.push_str(&std::env::var(var).unwrap_or_default());
                rest = &rest[end + 1..];
            }
            None => {
                out.push_str("${");
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn load() -> Config {
    let path = config_path();
    if !path.exists() {
        return Config::default();
    }
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("Error: cannot read {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    // an empty file behaves like a missing one (empty JSON is a parse error)
    if raw.trim().is_empty() {
        return Config::default();
    }
    let mut config: Config = serde_json::from_str(&raw).unwrap_or_else(|e| {
        eprintln!("Error: failed to parse {}: {e}", path.display());
        std::process::exit(1);
    });
    // aliases off the same bytes: a one-key view of the file (unknown
    // fields ignored), so load() costs one read + two parses, not two reads
    #[derive(Deserialize)]
    struct AliasRoot {
        #[serde(default)]
        aliases: BTreeMap<String, String>,
    }
    config.aliases = serde_json::from_str::<AliasRoot>(&raw)
        .map_err(|e| eprintln!("Warning: cannot read aliases from {}: {e}", path.display()))
        .map(|a| a.aliases)
        .unwrap_or_default();
    config
}

/// One silently-degrading read of a top-level config table (the plugin
/// tables `tools`/`mcpServers`): a missing file, unparsable JSON or
/// a missing key all yield None — optional tables are never fatal.
pub fn table(key: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let raw = fs::read_to_string(config_path()).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value.get(key)?.as_object().cloned()
}

/// Read config.json as an object for a merge-preserving rewrite: a missing
/// or empty file yields an empty object, anything unparsable or non-object
/// aborts instead of being wiped.
fn read_root() -> std::io::Result<serde_json::Value> {
    let path = config_path();
    if !path.exists() {
        return Ok(serde_json::Value::Object(Default::default()));
    }
    let raw = fs::read_to_string(&path).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("refusing to rewrite {}: {e}", path.display()),
        )
    })?;
    if raw.trim().is_empty() {
        return Ok(serde_json::Value::Object(Default::default()));
    }
    let root: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not valid JSON, not overwriting: {e}", path.display()),
        )
    })?;
    if !root.is_object() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} root is not a JSON object, not overwriting",
                path.display()
            ),
        ));
    }
    Ok(root)
}

fn write_root(root: &serde_json::Value) -> std::io::Result<()> {
    fs::write(config_path(), jsonfmt::dumps_indent(root, 2) + "\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(config_path(), fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

// model aliases — the hand-edited "aliases" object in config.json

/// Read the alias map from config.json.
pub fn load_aliases() -> BTreeMap<String, String> {
    let Ok(root) = read_root() else {
        return BTreeMap::new();
    };
    let mut map = BTreeMap::new();
    if let Some(existing) = root.get("aliases").and_then(|v| v.as_object()) {
        for (k, v) in existing {
            if let Some(id) = v.as_str() {
                map.insert(k.clone(), id.to_string());
            }
        }
    }
    map
}

pub fn save(config: &Config) -> std::io::Result<()> {
    fs::create_dir_all(user_dir())?;
    // merge into the existing file so hand-added keys ("agent" etc.) survive
    let mut root = read_root()?;
    let providers = serde_json::to_value(&config.providers).expect("providers serialize");
    if let Some(map) = root.as_object_mut() {
        map.insert("providers".to_string(), providers);
    }
    write_root(&root)
}

// the default model — the "models" object in config.json:
// {"default": "provider/model", "thinking": "high"}. One default serves
// every mode (prompt/agent/chat); -m and LLM_MODEL override per run.
// Legacy per-mode entries migrate on read: the prompt entry wins, then any
// set mode; the first write collapses the file onto the new shape.

/// The shared default model every mode runs on.
pub fn default_model() -> Option<String> {
    let value = read_root().ok()?;
    default_model_from(&value)
}

fn default_model_from(value: &serde_json::Value) -> Option<String> {
    if let Some(m) = value
        .get("models")
        .and_then(|m| m.get("default"))
        .and_then(|m| m.as_str())
    {
        return Some(m.to_string());
    }
    for mode in ["prompt", "agent", "chat"] {
        if let Some(m) = value
            .get("models")
            .and_then(|m| m.get(mode))
            .and_then(|e| e.get("model"))
            .and_then(|m| m.as_str())
        {
            return Some(m.to_string());
        }
    }
    None
}

/// The global reasoning level riding the default model.
pub fn default_thinking() -> Option<String> {
    let value = read_root().ok()?;
    default_thinking_from(&value)
}

fn default_thinking_from(value: &serde_json::Value) -> Option<String> {
    if let Some(t) = value
        .get("models")
        .and_then(|m| m.get("thinking"))
        .and_then(|t| t.as_str())
    {
        return Some(t.to_string());
    }
    for mode in ["prompt", "agent", "chat"] {
        if let Some(t) = value
            .get("models")
            .and_then(|m| m.get(mode))
            .and_then(|e| e.get("thinking"))
            .and_then(|t| t.as_str())
        {
            return Some(t.to_string());
        }
    }
    None
}

/// Write `models.default`, collapsing any legacy per-mode entries — the
/// first write moves an old install onto the new shape. Callers report
/// errors themselves (`llm models set` exits nonzero).
pub fn try_set_default_model(model: &str) -> std::io::Result<()> {
    edit_mode_default(|root| set_default_model_in(root, model))
}

fn set_default_model_in(value: &mut serde_json::Value, model: &str) {
    let map = models_map_mut(value);
    for mode in ["prompt", "agent", "chat"] {
        map.remove(mode);
    }
    map.insert(
        "default".to_string(),
        serde_json::Value::String(model.to_string()),
    );
}

/// Write (or remove, on None) `models.thinking`.
pub fn try_set_default_thinking(thinking: Option<&str>) -> std::io::Result<()> {
    edit_mode_default(|root| set_default_thinking_in(root, thinking))
}

fn set_default_thinking_in(value: &mut serde_json::Value, thinking: Option<&str>) {
    let map = models_map_mut(value);
    match thinking {
        Some(t) => {
            map.insert(
                "thinking".to_string(),
                serde_json::Value::String(t.to_string()),
            );
        }
        None => {
            map.remove("thinking");
        }
    }
}

/// Clear the default (and its thinking) when it points at `provider` —
/// used when the provider is removed, so the dangling entry cannot block
/// the first-provider auto-default. Returns Ok(true) when something was
/// cleared; the write error is surfaced instead of silently dropping it.
pub fn clear_default_for(provider: &str) -> std::io::Result<bool> {
    let mut cleared = false;
    edit_mode_default(|root| {
        cleared = clear_default_for_in(root, provider);
    })?;
    Ok(cleared)
}

/// The pure edit: drop the default/thinking/mode entries when they point at
/// `provider`. Returns true when anything was removed.
fn clear_default_for_in(root: &mut serde_json::Value, provider: &str) -> bool {
    let prefix = format!("{provider}/");
    if default_model_from(root).is_some_and(|m| m.starts_with(&prefix)) {
        if let Some(models) = root.as_object_mut().and_then(|m| m.get_mut("models"))
            && let Some(map) = models.as_object_mut()
        {
            map.remove("default");
            map.remove("thinking");
            for mode in ["prompt", "agent", "chat"] {
                map.remove(mode);
            }
        }
        true
    } else {
        false
    }
}

/// Remove the default entry entirely (`llm models unset`).
pub fn unset_default() -> std::io::Result<()> {
    edit_mode_default(|root| {
        if let Some(models) = root.as_object_mut().and_then(|m| m.get_mut("models"))
            && let Some(map) = models.as_object_mut()
        {
            map.remove("default");
            map.remove("thinking");
            for mode in ["prompt", "agent", "chat"] {
                map.remove(mode);
            }
        }
    })
}

/// Read-modify-write the whole config object, preserving every key.
fn edit_root(
    edit: impl FnOnce(&mut serde_json::Value) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut root = read_root()?;
    edit(&mut root)?;
    write_root(&root)
}

/// Read-modify-write the models table, preserving every other config key.
fn edit_mode_default(edit: impl FnOnce(&mut serde_json::Value)) -> std::io::Result<()> {
    fs::create_dir_all(user_dir())?;
    let mut root = read_root()?;
    edit(&mut root);
    write_root(&root)
}

/// The mutable `models` object, created when absent.
fn models_map_mut(
    value: &mut serde_json::Value,
) -> &mut serde_json::Map<String, serde_json::Value> {
    let root = value
        .as_object_mut()
        .expect("read_root only yields objects");
    let models = root
        .entry("models".to_string())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    if !models.is_object() {
        *models = serde_json::Value::Object(Default::default());
    }
    models.as_object_mut().expect("just ensured an object")
}

/// The mutable `models.<mode>` object, created (parents included) when absent.
fn options_from(value: &serde_json::Value) -> Option<BTreeMap<String, BTreeMap<String, String>>> {
    serde_json::from_value(value.get("models")?.get("options")?.clone()).ok()
}

fn set_options_in(
    value: &mut serde_json::Value,
    options: &BTreeMap<String, BTreeMap<String, String>>,
) {
    let models = models_map_mut(value);
    models.insert(
        "options".to_string(),
        serde_json::to_value(options).expect("options serialize"),
    );
}

// model settings — every mode's default
// per-model option table, all under config.json's "models" object

/// Per-model default options, stored as the `models.options` table.
pub fn load_model_options() -> BTreeMap<String, BTreeMap<String, String>> {
    read_root()
        .ok()
        .and_then(|root| options_from(&root))
        .unwrap_or_default()
}

pub fn save_model_options(
    options: &BTreeMap<String, BTreeMap<String, String>>,
) -> std::io::Result<()> {
    edit_mode_default(|root| set_options_in(root, options))
}

/// Remember a provider model seen in the wild (e.g. picked from a live
/// /models list) so it resolves from config.json from now on.
pub fn add_model(provider: &str, model_id: &str) {
    let mut cfg = load();
    if let Some(p) = cfg.providers.get_mut(provider) {
        if p.models.iter().any(|m| m == model_id) {
            return;
        }
        p.models.push(model_id.to_string());
    } else {
        return;
    }
    if let Err(e) = save(&cfg) {
        eprintln!(
            "Warning: failed to remember model in {}: {e}",
            config_path().display()
        );
    }
}

// logging gate — the config.json `logging` bool; a legacy marker file still counts as off

pub fn ensure_dir_exists(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
}

impl Config {
    /// Resolve a model id (either `provider/model` or a bare name/alias)
    /// to (provider_name, provider, model_id). `Ok(None)` means the name is
    /// not known; `Err` lists the candidates when a bare name is served by
    /// more than one provider, instead of silently taking the first in
    /// config order.
    pub fn resolve_model(
        &self,
        query: &str,
    ) -> Result<Option<(String, &Provider, String)>, String> {
        let query = self.aliases.get(query).map(|s| s.as_str()).unwrap_or(query);
        if let Some((prov, model)) = query.split_once('/')
            && let Some(p) = self.providers.get(prov)
        {
            return Ok(Some((prov.to_string(), p, model.to_string())));
        }
        // bare model name: every provider that lists it
        let hits: Vec<(&str, &Provider)> = self
            .providers
            .iter()
            .filter(|(_, p)| p.models.iter().any(|m| m == query))
            .map(|(n, p)| (n.as_str(), p))
            .collect();
        match hits.len() {
            0 => Ok(None),
            1 => Ok(Some((hits[0].0.to_string(), hits[0].1, query.to_string()))),
            _ => Err(format!(
                "'{query}' matches more than one provider: {} — qualify it as provider/model",
                hits.iter()
                    .map(|(n, _)| format!("{n}/{query}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// API key for a provider: its config.json api_key field, with ${VAR}
    /// references expanded.
    pub fn api_key(&self, p: &Provider) -> Option<String> {
        let raw = p.api_key.clone()?;
        let expanded = expand_env(&raw);
        if expanded.is_empty() {
            None
        } else {
            Some(expanded)
        }
    }

    /// All known model ids, qualified as provider/model.
    pub fn all_models(&self) -> Vec<(String, Vec<String>)> {
        self.providers
            .iter()
            .map(|(name, p)| (name.clone(), p.models.clone()))
            .collect()
    }
}

#[cfg(test)]
mod default_model_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn set_default_model_collapses_legacy_per_mode_entries() {
        let mut v = json!({
            "providers": {"p": {"kind": "openai-compat"}},
            "models": {
                "prompt": {"model": "old/a", "thinking": "high"},
                "agent": {"model": "old/b"},
                "options": {}
            }
        });
        set_default_model_in(&mut v, "new/m");
        assert_eq!(v["models"]["default"], json!("new/m"));
        for mode in ["prompt", "agent", "chat"] {
            assert!(v["models"].get(mode).is_none(), "{mode} survived");
        }
        // the options table is a sibling, not a mode: it stays
        assert!(v["models"].get("options").is_some());
        assert_eq!(v["providers"]["p"]["kind"], json!("openai-compat"));
    }

    #[test]
    fn set_default_thinking_writes_then_removes() {
        let mut v = json!({"models": {"default": "a/b"}});
        set_default_thinking_in(&mut v, Some("xhigh"));
        assert_eq!(v["models"]["thinking"], json!("xhigh"));
        assert_eq!(v["models"]["default"], json!("a/b"));
        set_default_thinking_in(&mut v, None);
        assert!(v["models"].get("thinking").is_none());
    }

    #[test]
    fn test_options_table_round_trips() {
        let mut v = json!({"models": {"agent": {"model": "x/y"}}});
        let mut options = BTreeMap::new();
        let mut inner = BTreeMap::new();
        inner.insert("temperature".to_string(), "0.3".to_string());
        options.insert("x/y".to_string(), inner);
        set_options_in(&mut v, &options);
        assert_eq!(options_from(&v), Some(options));
        assert_eq!(v["models"]["agent"]["model"], json!("x/y"), "sibling kept");
    }

    #[test]
    fn test_options_from_absent_is_none() {
        assert_eq!(options_from(&json!({})), None);
    }

    #[test]
    fn legacy_per_mode_defaults_migrate_on_read() {
        // the new single-default shape wins
        let root = serde_json::json!({"models": {"default": "a/x", "thinking": "high"}});
        assert_eq!(default_model_from(&root).as_deref(), Some("a/x"));
        assert_eq!(default_thinking_from(&root).as_deref(), Some("high"));
        // legacy per-mode entries: prompt wins, then any other set mode
        let root = serde_json::json!({"models": {
            "prompt": {"model": "p/x"}, "agent": {"model": "a/y", "thinking": "low"}
        }});
        assert_eq!(default_model_from(&root).as_deref(), Some("p/x"));
        assert_eq!(default_thinking_from(&root).as_deref(), Some("low"));
        let root = serde_json::json!({"models": {"chat": {"model": "c/z"}}});
        assert_eq!(default_model_from(&root).as_deref(), Some("c/z"));
        assert_eq!(default_thinking_from(&root), None);
        // nothing set anywhere
        assert_eq!(default_model_from(&serde_json::json!({})), None);
    }

    #[test]
    fn clear_default_for_removes_only_a_default_pointing_at_the_provider() {
        let mut v = json!({"models": {"default": "p/m", "thinking": "high"}});
        assert!(clear_default_for_in(&mut v, "p"));
        assert!(v["models"].get("default").is_none());
        assert!(v["models"].get("thinking").is_none());

        // a default aimed at another provider is left alone
        let mut other = json!({"models": {"default": "q/m", "thinking": "medium"}});
        assert!(!clear_default_for_in(&mut other, "p"));
        assert_eq!(other["models"]["default"], json!("q/m"));
        assert_eq!(other["models"]["thinking"], json!("medium"));
    }

    #[test]
    fn resolve_model_reports_ambiguity_instead_of_picking_a_provider() {
        let mut config = Config {
            providers: BTreeMap::new(),
            aliases: BTreeMap::new(),
        };
        config.providers.insert(
            "alpha".to_string(),
            Provider {
                kind: "openai-compat".to_string(),
                base_url: String::new(),
                api_key: None,
                models: vec!["claude-3".to_string()],
            },
        );
        config.providers.insert(
            "beta".to_string(),
            Provider {
                kind: "openai-compat".to_string(),
                base_url: String::new(),
                api_key: None,
                models: vec!["claude-3".to_string(), "gpt-4".to_string()],
            },
        );

        // a bare name served by two providers lists the candidates
        let err = config.resolve_model("claude-3").unwrap_err();
        assert!(err.contains("matches more than one provider"), "{err}");
        assert!(err.contains("alpha/claude-3"), "{err}");
        assert!(err.contains("beta/claude-3"), "{err}");

        // a unique bare name resolves to its single provider
        let (n, _, m) = config.resolve_model("gpt-4").unwrap().unwrap();
        assert_eq!(n, "beta");
        assert_eq!(m, "gpt-4");

        // a qualified id wins even for a shared model id
        let (n, _, _) = config.resolve_model("alpha/claude-3").unwrap().unwrap();
        assert_eq!(n, "alpha");

        assert!(config.resolve_model("nope").unwrap().is_none());
    }
}
