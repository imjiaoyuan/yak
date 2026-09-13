use super::*;

/// Discovered extension entries, split by form: resident executables
/// (spawned once, speaking the protocol) and manifest script tools (one
/// comment header on any script in any language; the host runs the
/// protocol around each call). Project wins by stem name across both.
/// Discovered extension entries, split by form: resident executables
/// (spawned once, speaking the protocol) and manifest script tools (one
pub struct Discovered {
    pub resident: Vec<PathBuf>,
    pub script_tools: Vec<crate::agent::ext::ExecToolSpec>,
}

/// Scan the home directories for extension files.
pub fn discover(cwd: &Path) -> Discovered {
    let disabled = crate::core::config::disabled_extensions();
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Discovered {
        resident: Vec::new(),
        script_tools: Vec::new(),
    };
    for dir in discover_dirs(cwd) {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if disabled.iter().any(|d| d == stem) || !seen.insert(stem.to_string()) {
                continue;
            }
            // a package install can keep individual extensions dormant
            let file = path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or_default();
            if !crate::commands::pkg::extension_kept(&dir, file) {
                continue;
            }
            // a `--- llm-tool:` manifest header makes any script a tool —
            // no exec bit needed (the host runs it through the declared
            // interpreter), which also makes the form work on Windows
            if let Some(spec) = exec_tool_manifest(&path) {
                out.script_tools.push(spec);
            } else if is_executable(&path) {
                out.resident.push(path);
            }
        }
    }
    out
}

/// A `--- llm-tool:` manifest parsed from a script's leading comment block.
#[derive(Clone, Debug)]
pub struct ExecToolSpec {
    pub path: PathBuf,
    pub name: String,
    pub description: String,
    pub schema: Value,
    /// single-argument form: the one declared argument arrives as argv[1]
    /// (plain text, no JSON) — shell scripts never need to parse stdin
    pub arg_mode_argv: bool,
    /// explicit interpreter (`# interpreter: python`); None = the file runs
    /// itself (shebang / exec bit / Windows association)
    pub interpreter: Option<String>,
    pub timeout: u64,
    /// declared trust tier (`# tier: write`); defaults to exec
    pub tier: Tier,
}

/// Parse the manifest header off a script. A manifest starts at a comment
/// line carrying `--- llm-tool: <name>` (`#` or `//` prefix) and extends
/// over the following comment lines; the first non-comment line ends it.
/// Fields: `description:`, `args: name (type) desc` (repeatable),
/// `arg-mode: argv`, `interpreter: <prog>`, `timeout: <secs>`,
/// `tier: read|write|exec`.
pub fn parse_tool_manifest(text: &str, path: &Path) -> Option<ExecToolSpec> {
    let mut name: Option<String> = None;
    let mut description = String::new();
    let mut properties = serde_json::Map::new();
    let mut required: Vec<Value> = Vec::new();
    let mut arg_mode_argv = false;
    let mut interpreter: Option<String> = None;
    let mut timeout: Option<u64> = None;
    let mut tier = Tier::Exec;
    let mut in_header = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        let comment = trimmed
            .strip_prefix('#')
            .or_else(|| trimmed.strip_prefix("//"))
            .map(str::trim);
        let Some(comment) = comment else {
            if in_header {
                break; // first non-comment line ends the header
            }
            continue;
        };
        if !in_header {
            // lines before the marker (shebang, license header) are skipped,
            // not fatal — only a non-comment line ends the scan
            let Some(rest) = comment.strip_prefix("--- llm-tool:") else {
                continue;
            };
            name = Some(rest.trim().to_string());
            in_header = true;
            continue;
        }
        if let Some(rest) = comment.strip_prefix("description:") {
            description = rest.trim().to_string();
        } else if let Some(rest) = comment.strip_prefix("args:") {
            parse_arg_field(rest, &mut properties, &mut required);
        } else if matches!(comment.strip_prefix("arg-mode:"), Some(v) if v.trim() == "argv") {
            arg_mode_argv = true;
        } else if let Some(rest) = comment.strip_prefix("interpreter:") {
            let prog = rest.trim();
            if !prog.is_empty() {
                interpreter = Some(crate::core::config::expand_env(prog));
            }
        } else if let Some(rest) = comment.strip_prefix("timeout:") {
            timeout = rest.trim().parse::<u64>().ok().filter(|t| *t > 0);
        } else if let Some(rest) = comment.strip_prefix("tier:") {
            // an unknown tier stays exec (the safe default)
            tier = Tier::parse(rest).unwrap_or(Tier::Exec);
        }
    }
    Some(ExecToolSpec {
        path: path.to_path_buf(),
        name: name?,
        description,
        schema: json!({
            "type": "object",
            "properties": Value::Object(properties),
            "required": required,
        }),
        arg_mode_argv,
        interpreter,
        timeout: timeout.unwrap_or_else(|| crate::core::config::extension_tool_timeout().as_secs()),
        tier,
    })
}

/// `args: name (type) the description` → one schema property + required.
pub(super) fn parse_arg_field(
    rest: &str,
    properties: &mut serde_json::Map<String, Value>,
    required: &mut Vec<Value>,
) {
    let Some((name, rest)) = rest.trim().split_once(char::is_whitespace) else {
        return;
    };
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    let (type_, description) = match rest.trim().split_once(char::is_whitespace) {
        Some((t, d)) if t.starts_with('(') && t.ends_with(')') => {
            (&t[1..t.len() - 1], d.trim().to_string())
        }
        _ => ("string", rest.trim().to_string()),
    };
    let type_ = match type_ {
        "int" | "integer" => "integer",
        "number" | "float" => "number",
        "bool" | "boolean" => "boolean",
        "list" | "array" => "array",
        _ => "string",
    };
    let mut prop = serde_json::Map::new();
    prop.insert("type".to_string(), json!(type_));
    if !description.is_empty() {
        prop.insert("description".to_string(), json!(description));
    }
    properties.insert(name.to_string(), Value::Object(prop));
    required.push(json!(name));
}

/// Read a file's head and parse its manifest, if it carries one.
pub(super) fn exec_tool_manifest(path: &Path) -> Option<ExecToolSpec> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; 4096];
    let n = f.read(&mut head).unwrap_or(0);
    let text = String::from_utf8_lossy(&head[..n]);
    parse_tool_manifest(&text, path).filter(|s| !s.name.is_empty())
}

#[cfg(unix)]
pub(super) fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub(super) fn is_executable(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("exe") | Some("bat") | Some("cmd") | Some("ps1")
    )
}

// ============================================================================
// one extension process
// ============================================================================
