//! user_dir resolution and the files kept there: config.json (providers with
//! inline api keys, the "models" settings family and hand-added tables like
//! "agent") and the logs-off marker.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::jsonfmt;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    /// snapshot of aliases.json taken once per `load()`, so per-model
    /// resolution does not re-read it from disk
    #[serde(skip)]
    pub aliases: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            providers: BTreeMap::new(),
            aliases: load_aliases(),
        }
    }
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
/// on). A legacy `logs-off` marker file in the user directory still counts
/// as off, so old installs keep their choice until they flip it again.
pub fn logs_on() -> bool {
    if user_dir().join("logs-off").exists() {
        return false;
    }
    read_root()
        .ok()
        .and_then(|root| root.get("logging").and_then(|v| v.as_bool()))
        .unwrap_or(true)
}

pub fn set_logs_enabled(on: bool) {
    let legacy = user_dir().join("logs-off");
    if on {
        let _ = fs::remove_file(&legacy);
    } else {
        let _ = fs::write(&legacy, b"");
    }
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
    if config.aliases.is_empty() {
        config.aliases = load_legacy_aliases();
    }
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

// model aliases — the "aliases" object in config.json (a legacy standalone
// aliases.json, indent 4, folds in once and is then ignored)

/// Read the alias map from config.json, importing a legacy aliases.json on
/// first sight so upgrades keep working.
pub fn load_aliases() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Ok(root) = read_root()
        && let Some(existing) = root.get("aliases").and_then(|v| v.as_object())
    {
        for (k, v) in existing {
            if let Some(id) = v.as_str() {
                map.insert(k.clone(), id.to_string());
            }
        }
    }
    if map.is_empty() {
        map = load_legacy_aliases();
    }
    map
}

/// The legacy standalone aliases.json (indent 4); folds in once and is then
/// ignored.
fn load_legacy_aliases() -> BTreeMap<String, String> {
    match fs::read_to_string(user_dir().join("aliases.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<BTreeMap<String, String>>(&raw).ok())
    {
        Some(legacy) if !legacy.is_empty() => legacy,
        _ => BTreeMap::new(),
    }
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
/// the first-provider auto-default. Returns true when something was cleared.
pub fn clear_default_for(provider: &str) -> bool {
    let prefix = format!("{provider}/");
    let mut cleared = false;
    let _ = edit_mode_default(|root| {
        if default_model_from(root).is_some_and(|m| m.starts_with(&prefix)) {
            cleared = true;
            if let Some(models) = root.as_object_mut().and_then(|m| m.get_mut("models"))
                && let Some(map) = models.as_object_mut()
            {
                map.remove("default");
                map.remove("thinking");
                for mode in ["prompt", "agent", "chat"] {
                    map.remove(mode);
                }
            }
        }
    });
    cleared
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
    /// to (provider_name, provider, model_id).
    pub fn resolve_model(&self, query: &str) -> Option<(String, &Provider, String)> {
        let query = self.aliases.get(query).map(|s| s.as_str()).unwrap_or(query);
        if let Some((prov, model)) = query.split_once('/')
            && let Some(p) = self.providers.get(prov)
        {
            return Some((prov.to_string(), p, model.to_string()));
        }
        // bare model name: find a provider that lists it
        for (name, p) in &self.providers {
            if p.models.iter().any(|m| m == query) {
                return Some((name.clone(), p, query.to_string()));
            }
        }
        None
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
}
