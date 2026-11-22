use super::*;

/// Discovered extension entries, split by form: resident executables
/// (spawned once, speaking the protocol) and manifest script tools (one
/// comment header on any script in any language; the host runs the
/// protocol around each call). Project wins by stem name across both.
#[derive(Debug)]
pub struct Discovered {
    pub resident: Vec<ResidentFile>,
    pub script_tools: Vec<crate::agent::ext::ExecToolSpec>,
}

/// One resident candidate: its file plus the optional sidecar manifest
/// (`<stem>.yak.json`) that declares version and an environment whitelist.
#[derive(Debug)]
pub struct ResidentFile {
    pub path: PathBuf,
    pub manifest: Option<ExtManifest>,
}

/// The optional sidecar manifest beside an extension file: `<stem>.yak.json`
/// in the same directory. Everything in it is opt-in — a file without one
/// runs exactly as before, inheriting the full environment.
#[derive(Clone, Debug, Default)]
pub struct ExtManifest {
    /// informational; `/status` and the banner show it as `stem@version`
    pub version: Option<String>,
    /// glob patterns (`*` wildcard) naming the environment variables the
    /// extension may see; when present, the child gets a small baseline
    /// (PATH, HOME, TMPDIR, LANG, TERM) plus the matches, and nothing else.
    /// Absent = inherit the full environment, the historical behavior.
    pub env: Option<Vec<String>>,
}

impl ExtManifest {
    /// Parse a sidecar. Unknown keys are ignored (forward compatibility);
    /// wrong-typed known keys fail loudly — a manifest is a security
    /// declaration, and guessing one into a weaker shape is wrong.
    pub(super) fn parse(text: &str, path: &Path) -> Result<ExtManifest, String> {
        let value: Value = serde_json::from_str(text)
            .map_err(|e| format!("invalid manifest {}: {e}", path.display()))?;
        let mut out = ExtManifest::default();
        if let Some(v) = value.get("version") {
            out.version = Some(
                v.as_str()
                    .ok_or_else(|| {
                        format!("manifest {}: \"version\" must be a string", path.display())
                    })?
                    .to_string(),
            );
        }
        if let Some(v) = value.get("env") {
            let list = v.as_array().ok_or_else(|| {
                format!(
                    "manifest {}: \"env\" must be a list of variable-name patterns",
                    path.display()
                )
            })?;
            let mut patterns = Vec::new();
            for item in list {
                patterns.push(
                    item.as_str()
                        .ok_or_else(|| {
                            format!(
                                "manifest {}: \"env\" entries must be strings",
                                path.display()
                            )
                        })?
                        .to_string(),
                );
            }
            out.env = Some(patterns);
        }
        Ok(out)
    }
}

/// The sidecar manifest path for an extension file: `<stem>.yak.json`
/// beside it. None when the stem is not representable (never in practice).
pub(super) fn sidecar_path(path: &Path) -> Option<PathBuf> {
    let stem = path.file_stem()?.to_str()?;
    Some(path.with_file_name(format!("{stem}.yak.json")))
}

/// Read and parse the sidecar manifest, if one sits beside the file.
/// A malformed manifest fails discovery loudly: it declares a security
/// boundary, and half-applying one is worse than refusing the plugin.
pub(super) fn sidecar_manifest(path: &Path) -> Result<Option<ExtManifest>, String> {
    let Some(sidecar) = sidecar_path(path) else {
        return Ok(None);
    };
    let text = match std::fs::read_to_string(&sidecar) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read manifest {}: {e}", sidecar.display())),
    };
    ExtManifest::parse(&text, &sidecar).map(Some)
}

/// Environment variables every extension sees even under a whitelist: a
/// child that cannot find `PATH` or learn the terminal shape is broken,
/// not contained.
const ENV_BASELINE: &[&str] = &["PATH", "HOME", "TMPDIR", "LANG", "TERM"];

/// `true` when `name` matches `pattern` (`*` matches any run of characters).
pub(super) fn env_pattern_matches(pattern: &str, name: &str) -> bool {
    fn inner(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => inner(&p[1..], s) || (!s.is_empty() && inner(p, &s[1..])),
            (Some(&c), Some(&d)) if c == d => inner(&p[1..], &s[1..]),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), name.as_bytes())
}

/// Apply the manifest's environment declaration to a child process. `None`
/// (no declaration) inherits everything; `Some(patterns)` narrows the child
/// to the baseline plus the variables the patterns name.
pub(super) fn apply_env_filter(command: &mut std::process::Command, env: Option<&[String]>) {
    let Some(patterns) = env else {
        return;
    };
    command.env_clear();
    for key in ENV_BASELINE {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    for (key, value) in std::env::vars_os() {
        let Some(key) = key.to_str() else { continue };
        if patterns.iter().any(|p| env_pattern_matches(p, key)) {
            command.env(key, value);
        }
    }
}

/// Scan the home directories for extension files. `extensions.disabled`
/// matches a file's stem and a script tool's declared name alike: a
/// package file whose stem says nothing about its tools (or two scripts
/// declaring the same name) can be disabled by either spelling. The
/// config-derived inputs (`disabled` names, the fallback tool timeout)
/// read once here, not once per candidate script.
pub fn discover(cwd: &Path) -> Result<Discovered, String> {
    discover_in(
        &discover_dirs(cwd)?,
        &crate::core::config::disabled_extensions(),
        crate::core::config::extension_tool_timeout().as_secs(),
    )
}

/// The scan itself over explicit homes, so a test points it at a scratch
/// directory instead of the real `~/.yak` (the same seam `http.rs` gives
/// its interrupt flag); `cwd` only picks the homes, it plays no part in
/// matching.
pub(super) fn discover_in(
    dirs: &[PathBuf],
    disabled: &[String],
    default_timeout: u64,
) -> Result<Discovered, String> {
    let is_disabled = |name: &str| disabled.iter().any(|d| d == name);
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Discovered {
        resident: Vec::new(),
        script_tools: Vec::new(),
    };
    for dir in dirs {
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(format!(
                    "cannot read extension directory {}: {e}",
                    dir.display()
                ));
            }
        };
        for entry in rd {
            let entry =
                entry.map_err(|e| format!("cannot read entry in {}: {e}", dir.display()))?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            // the sidecar manifests themselves are not extensions: they
            // declare one, and must not claim the stem or shadow anything
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".yak.json"))
            {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if is_disabled(stem) || seen.contains(stem) {
                continue;
            }
            // a `--- yak-tool:` manifest header makes any script a tool —
            // no exec bit needed (the host runs it through the declared
            // interpreter), which also makes the form work on Windows
            let spec = exec_tool_manifest(&path, default_timeout)?;
            if spec.is_none() && !is_executable(&path) {
                // neither a manifest script tool nor an executable: not an
                // extension, so it must not claim the stem and shadow a
                // real one in a lower-priority home (a sidecar manifest
                // without its extension file stays inert the same way)
                continue;
            }
            let manifest = sidecar_manifest(&path)?;
            seen.insert(stem.to_string());
            if let Some(mut spec) = spec {
                if spec.manifest.is_none() {
                    spec.manifest = manifest;
                }
                if !is_disabled(&spec.name) {
                    out.script_tools.push(spec);
                }
            } else {
                out.resident.push(ResidentFile { path, manifest });
            }
        }
    }
    Ok(out)
}

/// A `--- yak-tool:` manifest parsed from a script's leading comment block.
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
    /// the sidecar manifest (`<stem>.yak.json`), when one sits beside the
    /// script: version display and the environment whitelist
    pub manifest: Option<ExtManifest>,
}

/// Parse the manifest header off a script. A manifest starts at a comment
/// line carrying `--- yak-tool: <name>` (`#` or `//` prefix) and extends
/// over the following comment lines; the first non-comment line ends it.
/// Fields: `description:`, `args: name (type) desc` (repeatable),
/// `arg-mode: argv`, `interpreter: <prog>`, `timeout: <secs>`,
/// `tier: read|write|exec`.
pub fn parse_tool_manifest(text: &str, path: &Path, default_timeout: u64) -> Option<ExecToolSpec> {
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
            let Some(rest) = comment.strip_prefix("--- yak-tool:") else {
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
    if arg_mode_argv && properties.len() != 1 {
        return None;
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
        timeout: timeout.unwrap_or(default_timeout),
        tier,
        manifest: None, // the sidecar is attached by the discovery scan
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
pub(super) fn exec_tool_manifest(
    path: &Path,
    default_timeout: u64,
) -> Result<Option<ExecToolSpec>, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)
        .map_err(|e| format!("cannot read extension {}: {e}", path.display()))?;
    let mut head = vec![0u8; 4096];
    let n = f
        .read(&mut head)
        .map_err(|e| format!("cannot read extension {}: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&head[..n]);
    let spec = parse_tool_manifest(&text, path, default_timeout).filter(|s| !s.name.is_empty());
    if spec.is_none() && text.contains("--- yak-tool:") {
        return Err(format!(
            "invalid tool manifest in {} (argv mode requires exactly one declared argument)",
            path.display()
        ));
    }
    Ok(spec)
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
