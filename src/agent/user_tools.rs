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
/// collisions, mirroring skills' project-over-user rule. Drop-in names live
/// under the same rules as config-table tools: the charset and mcp__
/// reservation of the provider schema, and no shadowing of built-ins.
pub fn discover(cwd: &Path) -> Vec<ScriptToolSpec> {
    discover_in(cwd, &crate::core::config::user_dir().join("tools"))
}

/// `discover` with an explicit user tools dir, so tests stay hermetic.
pub fn discover_in(cwd: &Path, user: &Path) -> Vec<ScriptToolSpec> {
    let mut specs: Vec<ScriptToolSpec> = Vec::new();
    for root in project_roots(cwd, user) {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(spec) = load_one(&path) else {
                continue;
            };
            if !crate::agent::script_tool::valid_name(&spec.name) {
                warn(
                    &path,
                    &format!(
                        "name '{}' must match [A-Za-z0-9_-]{{1,64}} and not start with mcp__",
                        spec.name
                    ),
                );
                continue;
            }
            if crate::agent::script_tool::BUILTIN_TOOL_NAMES.contains(&spec.name.as_str()) {
                warn(&path, "collides with a built-in tool");
                continue;
            }
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
/// `.llm/tools` found, then the user dir first and the found roots behind
/// it farthest-first, so the nearest project dir loads last and overrides
/// on name.
fn project_roots(cwd: &Path, user: &Path) -> Vec<PathBuf> {
    let mut roots = vec![user.to_path_buf()];
    // dirs_up collects nearest-first; loading farthest-last-but-one makes
    // the nearest dir the final word on a name
    roots.extend(
        crate::core::paths::dirs_up(cwd, ".llm/tools")
            .into_iter()
            .rev(),
    );
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
    // absolute and $VAR commands expand as-is; ./relative resolves against
    // the manifest's directory; a bare name resolves through $PATH like a
    // shell would (falling back to the directory for the error message)
    let command = if command.starts_with('/') || command.starts_with('$') {
        crate::core::config::expand_env(&command)
    } else if !command.contains('/') {
        match crate::platform::find_in_path(&command) {
            Some(found) => found.display().to_string(),
            None => format!(
                "{}/{}",
                path.parent().unwrap_or(Path::new(".")).display(),
                command
            ),
        }
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
/// # timeout: 30
/// ```
///
/// `args:` lines build the input schema; the file itself runs on call.
fn load_embedded(path: &Path) -> Option<ScriptToolSpec> {
    let raw = std::fs::read_to_string(path).ok()?;
    let text = raw.as_str();
    let (name, description, args, timeout) = parse_header(text)?;
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
        timeout,
    })
}

/// Parse the manifest header: the `--- llm-tool: name ---` opener followed
/// by `key: value` comment lines until a non-comment line. Returns
/// (name, description, arg declarations, timeout seconds).
fn parse_header(text: &str) -> Option<(String, String, Vec<ArgDecl>, u64)> {
    let mut name = None;
    let mut description = String::new();
    let mut args: Vec<ArgDecl> = Vec::new();
    let mut timeout = crate::agent::script_tool::DEFAULT_TIMEOUT;
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
        } else if let Some(t) = comment.strip_prefix("timeout:")
            && let Ok(secs) = t.trim().parse::<u64>()
        {
            timeout = secs;
        }
    }
    let name = name.filter(|n| !n.is_empty())?;
    Some((name, description, args, timeout))
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
        let (name, desc, args, timeout) = parse_header(script).unwrap();
        assert_eq!(name, "wordcount");
        assert_eq!(desc, "count characters");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].name, "text");
        assert_eq!(args[0].ty, "string");
        assert_eq!(args[0].description, "the text");
        assert_eq!(args[1].ty, "boolean");
        assert_eq!(timeout, crate::agent::script_tool::DEFAULT_TIMEOUT);
    }

    #[test]
    fn header_timeout_overrides_the_default() {
        let script = "#!/bin/sh\n# --- llm-tool: slow ---\n# timeout: 30\necho hi\n";
        let (_, _, _, timeout) = parse_header(script).unwrap();
        assert_eq!(timeout, 30);
        // a non-numeric or missing value keeps the default
        let bad = "#!/bin/sh\n# --- llm-tool: slow ---\n# timeout: soon\necho hi\n";
        assert_eq!(
            parse_header(bad).unwrap().3,
            crate::agent::script_tool::DEFAULT_TIMEOUT
        );
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
    fn discovery_overrides_nearest_project_then_project_over_user() {
        let base = std::env::temp_dir().join(format!("llm-tools-{}", crate::core::db::ulid()));
        let user = base.join("user").join("tools");
        let outer = base.join("proj").join(".llm").join("tools");
        let inner = base.join("proj").join("deep").join(".llm").join("tools");
        for d in [&user, &outer, &inner] {
            std::fs::create_dir_all(d).unwrap();
        }
        for (dir, variant) in [
            (&user, "user"),
            (&outer, "outer project"),
            (&inner, "inner project"),
        ] {
            std::fs::write(
                dir.join("hello"),
                format!("#!/bin/sh\n# --- llm-tool: hello ---\n# description: {variant}\necho\n"),
            )
            .unwrap();
        }
        // from inside the nested project dir, the NEAREST .llm/tools wins
        let specs = discover_in(&base.join("proj").join("deep"), &user);
        assert_eq!(specs.len(), 1, "one name, one winner");
        assert_eq!(specs[0].description, "inner project");
        // one level up, the outer project dir is the nearest
        let specs = discover_in(&base.join("proj"), &user);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].description, "outer project");
        // outside any project dir the user version stands
        let specs = discover_in(&base.join("elsewhere"), &user);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].description, "user");

        // a JSON manifest beside the script names an external command
        std::fs::write(
            outer.join("count.json"),
            serde_json::json!({"description": "external", "command": "/bin/true"}).to_string(),
        )
        .unwrap();
        let specs = discover_in(&base.join("proj"), &user);
        assert_eq!(specs.len(), 2);
        let ext = specs.iter().find(|s| s.name == "count").unwrap();
        assert_eq!(ext.command, "/bin/true");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn discovery_skips_builtin_and_invalid_names() {
        let base = std::env::temp_dir().join(format!("llm-tools-{}", crate::core::db::ulid()));
        let user = base.join("tools");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(
            user.join("bash"),
            "#!/bin/sh\n# --- llm-tool: bash ---\n# description: not this one\necho\n",
        )
        .unwrap();
        std::fs::write(
            user.join("bad"),
            "#!/bin/sh\n# --- llm-tool: has spaces ---\necho\n",
        )
        .unwrap();
        std::fs::write(
            user.join("spoof"),
            "#!/bin/sh\n# --- llm-tool: mcp__fake__echo ---\necho\n",
        )
        .unwrap();
        let specs = discover_in(&base.join("cwd"), &user);
        assert!(specs.is_empty(), "all three must be skipped: {specs:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn manifest_bare_command_resolves_through_path() {
        let base = std::env::temp_dir().join(format!("llm-tools-{}", crate::core::db::ulid()));
        let user = base.join("tools");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(
            user.join("shout.json"),
            serde_json::json!({"description": "shout", "command": "sh"}).to_string(),
        )
        .unwrap();
        let specs = discover_in(&base.join("cwd"), &user);
        let spec = specs.iter().find(|s| s.name == "shout").unwrap();
        // resolved to the real sh on PATH, not <tools dir>/sh
        assert!(spec.command.ends_with("/sh"), "command: {}", spec.command);
        assert!(!spec.command.contains(&user.display().to_string()));
        let _ = std::fs::remove_dir_all(&base);
    }
}
