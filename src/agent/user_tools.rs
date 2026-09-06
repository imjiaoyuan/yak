//! Drop-in tool directory: executables with an embedded manifest header and
//! `*.json` manifests discovered from `~/.llm/tools/` and the nearest
//! `.llm/tools/` walking up from the working directory (project wins by
//! name). One file is one plugin, any language — the header declares the
//! tool, the file itself runs.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::script_tool::ScriptToolSpec;

/// The manifest header opener: `# --- llm-tool: name ---` (or `//` / `--`
/// comment styles; the line must start with a comment marker).
const HEADER_MARK: &str = "--- llm-tool:";

/// Discover every drop-in tool: embedded-manifest executables plus JSON
/// manifests, from both roots. Later roots (nearer the cwd) win on name
/// collisions, mirroring skills' project-over-user rule.
pub fn discover(cwd: &Path) -> Vec<ScriptToolSpec> {
    let mut specs: Vec<ScriptToolSpec> = Vec::new();
    let user = crate::core::config::user_dir().join("tools");
    for root in project_roots(cwd, &user) {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(spec) = load_one(&path) else {
                continue;
            };
            if let Some(existing) = specs.iter_mut().find(|s| s.name == spec.name) {
                *existing = spec; // nearer root overrides
            } else {
                specs.push(spec);
            }
        }
    }
    specs
}

/// The project root nearest the cwd wins: walk up collecting every
/// `.llm/tools` found (nearer later), then the user dir first (so later,
/// nearer entries override it).
fn project_roots(cwd: &Path, user: &Path) -> Vec<PathBuf> {
    let mut roots = vec![user.to_path_buf()];
    let mut dir = Some(cwd.to_path_buf());
    let mut found: Vec<PathBuf> = Vec::new();
    while let Some(d) = dir {
        let candidate = d.join(".llm").join("tools");
        if candidate.is_dir() {
            found.push(candidate);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    // nearest first already (walked up); user dir is the base so project
    // entries (appended) override on name
    roots.extend(found);
    roots
}

/// Load one file as a tool: a JSON manifest names an external command; any
/// other file must carry the embedded header and be executable (or carry a
/// shebang, which makes it runnable regardless of the executable bit on
/// some platforms).
fn load_one(path: &Path) -> Option<ScriptToolSpec> {
    if path.extension().map(|e| e == "json").unwrap_or(false) {
        return load_manifest(path);
    }
    load_embedded(path)
}

/// `name.json`: `{"description", "command", "args", "schema", "timeout"}`
/// pointing at any command — the config-table shape, as a file.
fn load_manifest(path: &Path) -> Option<ScriptToolSpec> {
    let raw = std::fs::read_to_string(path).ok()?;
    let def: Value = serde_json::from_str(&raw)
        .map_err(|e| {
            warn(path, &format!("invalid JSON: {e}"));
            e
        })
        .ok()?;
    let name = path.file_stem()?.to_string_lossy().to_string();
    let command = def
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            // no command: a sibling script with the same stem
            let sibling = path.with_file_name(&name);
            sibling.exists().then(|| format!("./{name}"))
        })?;
    // relative commands resolve against the manifest's directory
    let command = if command.starts_with('/') || command.starts_with('$') {
        crate::core::config::expand_env(&command)
    } else {
        format!(
            "{}/{}",
            path.parent().unwrap_or(Path::new(".")).display(),
            command.trim_start_matches("./")
        )
    };
    Some(ScriptToolSpec {
        description: def
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("User tool: {name}")),
        name,
        command,
        args: def
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(crate::core::config::expand_env)
                    .collect()
            })
            .unwrap_or_default(),
        schema: def
            .get("schema")
            .cloned()
            .unwrap_or(json!({"type": "object"})),
        timeout: def
            .get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(crate::agent::script_tool::DEFAULT_TIMEOUT),
    })
}

/// An executable whose leading comment block carries the manifest:
///
/// ```text
/// # --- llm-tool: wordcount ---
/// # description: count characters in the text
/// # args: text (string) the text to count
/// ```
///
/// `args:` lines build the input schema; the file itself runs on call.
fn load_embedded(path: &Path) -> Option<ScriptToolSpec> {
    let raw = std::fs::read_to_string(path).ok()?;
    let text = raw.as_str();
    let (name, description, args) = parse_header(text)?;
    let has_shebang = text.starts_with("#!");
    if !has_shebang {
        // still loadable if it parses, but warn: it cannot run
        warn(path, "no shebang; the tool will not execute");
    }
    Some(ScriptToolSpec {
        name,
        description,
        command: path.display().to_string(),
        args: Vec::new(),
        schema: args_schema(&args),
        timeout: crate::agent::script_tool::DEFAULT_TIMEOUT,
    })
}

/// Parse the manifest header: the `--- llm-tool: name ---` opener followed
/// by `key: value` comment lines until a non-comment line. Returns
/// (name, description, arg declarations).
fn parse_header(text: &str) -> Option<(String, String, Vec<ArgDecl>)> {
    let mut name = None;
    let mut description = String::new();
    let mut args: Vec<ArgDecl> = Vec::new();
    let mut in_header = false;
    for line in text.lines().take(40) {
        let trimmed = line.trim_start();
        let Some(comment) = trimmed
            .strip_prefix('#')
            .or_else(|| trimmed.strip_prefix("//"))
            .or_else(|| trimmed.strip_prefix("--"))
            .map(str::trim_start)
        else {
            if in_header {
                break; // header ends at the first non-comment line
            }
            continue; // shebang etc. before the opener
        };
        if let Some(rest) = comment.strip_prefix(HEADER_MARK) {
            in_header = true;
            name = rest.trim().trim_end_matches('-').trim().to_string().into();
            continue;
        }
        if !in_header {
            continue;
        }
        if let Some(desc) = comment.strip_prefix("description:") {
            description = desc.trim().to_string();
        } else if let Some(arg) = comment.strip_prefix("args:") {
            if let Some(decl) = parse_arg(arg.trim()) {
                args.push(decl);
            }
        } else if let Some(t) = comment.strip_prefix("timeout:") {
            let _ = t.trim().parse::<u64>(); // documented; per-spec timeout stays default
        }
    }
    let name = name.filter(|n| !n.is_empty())?;
    Some((name, description, args))
}

/// `name (type) description` — type defaults to string.
struct ArgDecl {
    name: String,
    ty: &'static str,
    description: String,
}

fn parse_arg(text: &str) -> Option<ArgDecl> {
    let text = text.trim();
    let name: String = text
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        return None;
    }
    let rest = text[name.len()..].trim_start();
    let (ty, description) = if let Some(rest) = rest.strip_prefix('(') {
        let end = rest.find(')')?;
        let ty = match rest[..end].trim() {
            "int" | "integer" | "number" => "integer",
            "float" | "number-float" => "number",
            "bool" | "boolean" => "boolean",
            _ => "string",
        };
        (ty, rest[end + 1..].trim().to_string())
    } else {
        ("string", rest.to_string())
    };
    Some(ArgDecl {
        name,
        ty,
        description,
    })
}

fn args_schema(args: &[ArgDecl]) -> Value {
    if args.is_empty() {
        return json!({"type": "object"});
    }
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();
    for a in args {
        let mut field = json!({"type": a.ty});
        if !a.description.is_empty() {
            field["description"] = json!(a.description);
        }
        props.insert(a.name.clone(), field);
        required.push(json!(a.name));
    }
    json!({
        "type": "object",
        "properties": Value::Object(props),
        "required": required,
    })
}

fn warn(path: &Path, why: &str) {
    eprintln!("Warning: skipped in tools dir {}: {why}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_parses_name_description_and_args() {
        let script = "#!/usr/bin/env python3\n\
                      # --- llm-tool: wordcount ---\n\
                      # description: count characters\n\
                      # args: text (string) the text\n\
                      # args: verbose (bool) print more\n\
                      \n\
                      import json, sys\n";
        let (name, desc, args) = parse_header(script).unwrap();
        assert_eq!(name, "wordcount");
        assert_eq!(desc, "count characters");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].name, "text");
        assert_eq!(args[0].ty, "string");
        assert_eq!(args[0].description, "the text");
        assert_eq!(args[1].ty, "boolean");
    }

    #[test]
    fn args_schema_builds_types_and_required() {
        let args = vec![
            ArgDecl {
                name: "n".into(),
                ty: "integer",
                description: String::new(),
            },
            ArgDecl {
                name: "label".into(),
                ty: "string",
                description: "a label".into(),
            },
        ];
        let s = args_schema(&args);
        assert_eq!(s["properties"]["n"]["type"], "integer");
        assert_eq!(s["properties"]["label"]["description"], "a label");
        assert_eq!(s["required"], serde_json::json!(["n", "label"]));
    }

    #[test]
    fn no_header_or_comment_styles() {
        // other comment styles parse the same
        let js = "// --- llm-tool: js_tool ---\n// description: d\n";
        assert_eq!(parse_header(js).unwrap().0, "js_tool");
        // no header → not a tool
        assert!(parse_header("#!/bin/sh\necho hi\n").is_none());
    }

    #[test]
    fn discovery_overrides_project_over_user() {
        let base = std::env::temp_dir().join(format!("llm-tools-{}", crate::core::db::ulid()));
        let user = base.join("user");
        let proj = base.join("proj");
        for d in [user.join("tools"), proj.join(".llm").join("tools")] {
            std::fs::create_dir_all(&d).unwrap();
        }
        std::fs::write(
            user.join("tools").join("hello"),
            "#!/bin/sh\n# --- llm-tool: hello ---\n# description: user version\necho '{}'\n",
        )
        .unwrap();
        std::fs::write(
            proj.join(".llm").join("tools").join("hello"),
            "#!/bin/sh\n# --- llm-tool: hello ---\n# description: project version\necho '{}'\n",
        )
        .unwrap();
        let specs = discover(&proj);
        assert_eq!(specs.len(), 1, "one name, one winner");
        assert_eq!(specs[0].description, "project version");

        // a JSON manifest beside the script names an external command
        std::fs::write(
            proj.join(".llm").join("tools").join("count.json"),
            serde_json::json!({"description": "external", "command": "/bin/true"}).to_string(),
        )
        .unwrap();
        let specs = discover(&proj);
        assert_eq!(specs.len(), 2);
        let ext = specs.iter().find(|s| s.name == "count").unwrap();
        assert_eq!(ext.command, "/bin/true");
        let _ = std::fs::remove_dir_all(&base);
    }
}
