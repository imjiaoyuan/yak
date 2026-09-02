//! user_dir resolution and the files kept there: config.json (providers with
//! inline api keys, the "models" settings family and hand-added tables like
//! "agent"), aliases.json, the logs-off marker — plus one-time migrations
//! from the old layouts.

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

#[derive(Debug, Deserialize, Serialize)]
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
/// tables `tools`/`mcpServers`/`hooks`): a missing file, unparsable JSON or
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

// per-mode model defaults — top-level "models" object in config.json

/// The stored default (model, thinking) for a REPL mode ("agent"/"chat").
pub fn mode_default(mode: &str) -> Option<(String, Option<String>)> {
    let value = read_root().ok()?;
    mode_default_from(&value, mode)
}

fn mode_default_from(value: &serde_json::Value, mode: &str) -> Option<(String, Option<String>)> {
    let entry = value.get("models")?.get(mode)?;
    Some((
        entry.get("model")?.as_str()?.to_string(),
        entry
            .get("thinking")
            .and_then(|v| v.as_str())
            .map(String::from),
    ))
}

/// Write `models.<mode>.model`, preserving every other key in the file;
/// callers report errors themselves (`llm models set` exits nonzero).
pub fn try_set_mode_default_model(mode: &str, model: &str) -> std::io::Result<()> {
    edit_mode_default(|root| set_mode_default_model_in(root, mode, model))
}

/// Write (or remove, on None) `models.<mode>.thinking`.
pub fn try_set_mode_default_thinking(mode: &str, thinking: Option<&str>) -> std::io::Result<()> {
    edit_mode_default(|root| set_mode_default_thinking_in(root, mode, thinking))
}

/// Remove a mode's whole entry (`llm models unset`).
pub fn unset_mode_default(mode: &str) -> std::io::Result<()> {
    edit_mode_default(|root| {
        if let Some(models) = root.as_object_mut().and_then(|m| m.get_mut("models"))
            && let Some(map) = models.as_object_mut()
        {
            map.remove(mode);
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

fn edit_mode_default(edit: impl FnOnce(&mut serde_json::Value)) -> std::io::Result<()> {
    fs::create_dir_all(user_dir())?;
    let mut root = read_root()?;
    edit(&mut root);
    write_root(&root)
}

fn set_mode_default_model_in(value: &mut serde_json::Value, mode: &str, model: &str) {
    mode_entry(value, mode).insert(
        "model".to_string(),
        serde_json::Value::String(model.to_string()),
    );
}

fn set_mode_default_thinking_in(value: &mut serde_json::Value, mode: &str, thinking: Option<&str>) {
    let entry = mode_entry(value, mode);
    match thinking {
        Some(t) => {
            entry.insert(
                "thinking".to_string(),
                serde_json::Value::String(t.to_string()),
            );
        }
        None => {
            entry.remove("thinking");
        }
    }
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
fn mode_entry<'a>(
    value: &'a mut serde_json::Value,
    mode: &str,
) -> &'a mut serde_json::Map<String, serde_json::Value> {
    let entry = models_map_mut(value)
        .entry(mode.to_string())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    if !entry.is_object() {
        *entry = serde_json::Value::Object(Default::default());
    }
    entry.as_object_mut().expect("just ensured an object")
}

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

// aliases.json — flat map, indent=4, created as "{}\n".

// model settings — every mode's default
// per-model option table, all under config.json's "models" object

/// The prompt mode's default — the global default of bare `llm` prompts.
pub fn get_default_model() -> Option<String> {
    mode_default("prompt").map(|(m, _)| m)
}

/// Set the same default model for every mode (prompt, agent, chat) — used
/// when the first provider's first model is chosen on a fresh install, so
/// `llm`, `llm agent` and `llm chat` all start working without a second
/// command.
pub fn set_default_model_all(model_id: &str) -> std::io::Result<()> {
    edit_mode_default(|root| set_default_model_all_in(root, model_id))
}

fn set_default_model_all_in(value: &mut serde_json::Value, model_id: &str) {
    for mode in ["prompt", "agent", "chat"] {
        set_mode_default_model_in(value, mode, model_id);
    }
}

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

// logging gate — marker file, exactly like the original

// legacy migration from the pre-alignment layout (~/.config/llm)

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
    pub fn api_key(&self, _provider: &str, p: &Provider) -> Option<String> {
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
mod mode_default_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_mode_default_reads_model_and_thinking() {
        let v = json!({"models": {"chat": {"model": "p/m", "thinking": "high"}}});
        assert_eq!(
            mode_default_from(&v, "chat"),
            Some(("p/m".to_string(), Some("high".to_string())))
        );
    }

    #[test]
    fn test_mode_default_without_thinking_key_is_none() {
        let v = json!({"models": {"chat": {"model": "p/m"}}});
        assert_eq!(
            mode_default_from(&v, "chat"),
            Some(("p/m".to_string(), None))
        );
    }

    #[test]
    fn test_mode_default_missing_mode_or_section_is_none() {
        assert_eq!(mode_default_from(&json!({"models": {}}), "chat"), None);
        assert_eq!(mode_default_from(&json!({}), "agent"), None);
    }

    #[test]
    fn test_set_mode_default_model_preserves_siblings() {
        let mut v = json!({
            "providers": {"p": {"kind": "openai-compat"}},
            "agent": {"thinking": "low"},
            "models": {"chat": {"model": "a/b", "thinking": "high"}}
        });
        set_mode_default_model_in(&mut v, "agent", "x/y");
        assert_eq!(v["agent"]["thinking"], json!("low"));
        assert_eq!(v["models"]["chat"]["model"], json!("a/b"));
        assert_eq!(v["models"]["agent"]["model"], json!("x/y"));
    }

    #[test]
    fn test_set_mode_default_thinking_writes_then_removes() {
        let mut v = json!({"models": {"chat": {"model": "a/b"}}});
        set_mode_default_thinking_in(&mut v, "chat", Some("xhigh"));
        assert_eq!(v["models"]["chat"]["thinking"], json!("xhigh"));
        set_mode_default_thinking_in(&mut v, "chat", None);
        assert!(v["models"]["chat"].get("thinking").is_none());
        assert_eq!(v["models"]["chat"]["model"], json!("a/b"));
    }

    #[test]
    fn test_set_default_model_all_covers_every_mode() {
        let mut v = json!({"models": {"agent": {"model": "old/a"}}});
        set_default_model_all_in(&mut v, "new/m");
        for mode in ["prompt", "agent", "chat"] {
            assert_eq!(v["models"][mode]["model"], json!("new/m"));
        }
        assert!(v["models"].get("options").is_none());
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
}
