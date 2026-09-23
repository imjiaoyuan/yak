//! Built-in agent tools: declaration schemas, argument validation, execution.
//!
//! This file is the registry and everything the tools share (the `Tool`
//! trait, `ToolOutput`, schema validation, the truncation and process-output
//! helpers); each tool's declaration, execution and private helpers live in
//! its own child module, with the tests in `tests.rs`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::approval::Tier;
use crate::providers::ToolError;
use bash::BashTool;
use edit::EditTool;
use fetch::FetchTool;
use fs::LsTool;
use plan::PlanTool;
use read::ReadTool;
use search::{GlobTool, GrepTool};
use write::WriteTool;

/// Shared truncation constants (the reference's values).
pub(crate) const MAX_LINES: usize = 2000;
pub(crate) const MAX_BYTES: usize = 50 * 1024;
/// Token-estimate cap on one tool result, applied after the byte cut. The
/// byte cap alone is not a bound on cost: 50KB of ASCII is ~12k tokens, but
/// 50KB of Chinese is ~17k (see `compact::text_tokens`, which counts CJK at
/// ~1 token/char), and that result is re-sent every subsequent round. The
/// cut takes from the front so the tail — where a command's failure lands —
/// survives.
pub(crate) const MAX_TOKENS: u64 = 12_000;

pub struct ToolOutput {
    pub content: String,
    /// the failure class, `None` on success
    pub error: Option<ToolError>,
    /// attachments (e.g. an image the read tool decoded) fed back to the
    /// model as vision input on the next round
    pub attachments: Vec<crate::providers::Attachment>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> ToolOutput {
        ToolOutput {
            content: content.into(),
            error: None,
            attachments: Vec::new(),
        }
    }

    /// The tool ran and reported failure.
    pub fn err(content: impl Into<String>) -> ToolOutput {
        ToolOutput::err_kind(ToolError::Failed, content)
    }

    /// A failure of a specific class: a refused call is not a broken tool.
    pub fn err_kind(kind: ToolError, content: impl Into<String>) -> ToolOutput {
        ToolOutput {
            content: content.into(),
            error: Some(kind),
            attachments: Vec::new(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

/// Cap on previewed diff rows; the block folds beyond this.
const DIFF_MAX_LINES: usize = 30;

/// Print a diff block from `change_hunks` under a `$` action line: wrapped
/// to the terminal: additions green, deletions red, hunk headers and
/// context gray (the reference's toolDiff colors).
pub fn print_diff_block(diff: &str) {
    let width = crate::term::columns().max(20);
    let p = crate::theme::err();
    for line in diff.split('\n') {
        let wrapped = crate::core::render_md::wrap_block(line, width, 2);
        let styled = match line.chars().next() {
            // the legacy chrome look: additions default, deletions gray,
            // hunk headers and context dim
            Some('+') => format!("  {wrapped}"),
            Some('-') => format!("{}  {wrapped}{}", p.diff_del, p.reset),
            _ => format!("{}  {wrapped}{}", p.diff_ctx, p.reset),
        };
        eprintln!("{styled}");
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn tier(&self) -> Tier;
    fn description(&self) -> &str;
    /// JSON schema for the arguments object
    fn parameters(&self) -> Value;
    /// one-line human summary shown in the approval prompt
    fn preview(&self, args: &Value) -> String;
    /// Optional change preview (unified-diff style) printed under the `$`
    /// action line before the approval prompt; file-mutating tools override.
    fn diff(&self, _args: &Value, _cwd: &Path) -> Option<String> {
        None
    }
    /// Salvage malformed argument shapes before validation (the reference's
    /// `prepareArguments`, which the loop runs ahead of the schema check): a
    /// tool may normalize what the model sent. Must be idempotent.
    fn prepare_arguments(&self, args: &Value) -> Value {
        args.clone()
    }
    /// `log` receives live progress lines while the tool runs (bash streams
    /// its stdout); tools without progress simply ignore it
    fn execute(&self, args: &Value, cwd: &Path, log: &mut dyn FnMut(&str)) -> ToolOutput;

    /// Execute with the full call in hand. The default drops the call (a
    /// tool's arguments are all it needs); extension tools override it so
    /// their script sees the model's `tool_call_id` and can key state on it
    /// across the concurrent read-only batch.
    fn execute_call(
        &self,
        call: &crate::providers::ToolCall,
        cwd: &Path,
        log: &mut dyn FnMut(&str),
    ) -> ToolOutput {
        self.execute(&call.arguments, cwd, log)
    }
}

/// The built-in tool registry: ten handwritten tools.
pub fn builtin_tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(PlanTool),
        Box::new(ReadTool),
        Box::new(WriteTool),
        Box::new(EditTool),
        Box::new(BashTool),
        Box::new(GrepTool),
        Box::new(GlobTool),
        Box::new(LsTool),
        Box::new(FetchTool),
    ]
}

/// Validate tool arguments against the schema, coercing in place first
/// (the reference's `Value.Convert` + `normalizeOptionalNulls`): numeric/boolean strings
/// become their declared types, and `null` on an optional property is
/// dropped — models routinely send `"offset": "50"` or null optional fields,
/// and a wasted round over a type spelling is the worst outcome. Unknown
/// properties pass through untouched; enough to bounce malformed calls back
/// to the model.
pub fn validate(schema: &Value, args: &mut Value) -> Result<(), String> {
    let Some(obj) = args.as_object_mut() else {
        return Err("arguments must be a JSON object".to_string());
    };
    let props = schema["properties"].as_object();
    let required = schema["required"].as_array().cloned().unwrap_or_default();
    if let Some(props) = props {
        let keys: Vec<String> = obj.keys().cloned().collect();
        for key in keys {
            let Some(spec) = props.get(&key) else {
                continue;
            };
            let is_required = required.iter().any(|r| r.as_str() == Some(key.as_str()));
            let value = obj.get_mut(&key).expect("key from own keys");
            if value.is_null() && !is_required {
                obj.shift_remove(&key);
                continue;
            }
            coerce_value(value, spec);
        }
    }
    for name in &required {
        let missing = name.as_str().map(|n| !obj.contains_key(n)).unwrap_or(true);
        if missing {
            return Err(format!(
                "missing required argument '{}'",
                name.as_str().unwrap_or("?")
            ));
        }
    }
    for (name, spec) in props.into_iter().flatten() {
        if let (Some(value), Some(ty)) = (obj.get(name), spec["type"].as_str()) {
            let ok = match ty {
                "string" => value.is_string(),
                "integer" => value.is_i64() || value.is_u64(),
                "number" => value.is_number(),
                "boolean" => value.is_boolean(),
                "array" => value.is_array(),
                "object" => value.is_object(),
                _ => true,
            };
            if !ok {
                return Err(format!("argument '{name}' must be of type {ty}"));
            }
        }
    }
    Ok(())
}

/// Coerce one value toward its schema type, recursing into arrays and
/// objects. A value that cannot coerce is left alone — the type check below
/// reports it with the argument name.
fn coerce_value(value: &mut Value, spec: &Value) {
    match spec["type"].as_str() {
        Some("number") | Some("integer") => {
            if let Some(text) = value.as_str()
                && let Ok(n) = text.parse::<f64>()
                && n.is_finite()
            {
                if spec["type"] == "integer" && n.fract() == 0.0 && n.abs() <= 9.0e15 {
                    *value = json!(n as i64);
                } else {
                    *value = json!(n);
                }
            }
        }
        Some("boolean") => {
            if let Some(text) = value.as_str() {
                match text {
                    "true" => *value = json!(true),
                    "false" => *value = json!(false),
                    _ => {}
                }
            }
        }
        Some("array") => {
            if let (Some(items), Some(arr)) = (spec.get("items"), value.as_array_mut()) {
                for item in arr {
                    coerce_value(item, items);
                }
            }
        }
        Some("object") if spec.get("properties").is_some() => {
            coerce_object(spec, value);
        }
        _ => {}
    }
}

/// Coerce an object's declared properties in place (the recursion
/// `coerce_value` and `validate` share).
fn coerce_object(schema: &Value, args: &mut Value) {
    let Some(props) = schema["properties"].as_object() else {
        return;
    };
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    for key in obj.keys().cloned().collect::<Vec<_>>() {
        let Some(spec) = props.get(&key) else {
            continue;
        };
        let is_required = required.contains(&key.as_str());
        let value = obj.get_mut(&key).expect("key from own keys");
        if value.is_null() && !is_required {
            obj.shift_remove(&key);
            continue;
        }
        coerce_value(value, spec);
    }
}

/// Keep the last `max_lines` lines / `max_bytes` bytes.
/// The `$ <verb> <preview>` chrome line with its green preview, wrapped to
/// the terminal, plus the optional diff block. Shared by the approval
/// prompt and the session's ToolStart echo so the two render identically.
pub(crate) fn print_action_line(verb: &str, preview: &str, diff: Option<&str>) {
    let width = crate::term::columns().max(20);
    let vis = 2 + verb.chars().count() + 1;
    let wrapped = crate::core::render_md::wrap_plain(preview, width.saturating_sub(vis), 2);
    // same shape as the tool activity line: bold $, the command in green
    let p = crate::theme::err();
    eprintln!(
        "{}${} {verb} {}{}{}{}",
        p.bold, p.reset, p.bold, p.green, wrapped, p.reset
    );
    if let Some(diff) = diff {
        print_diff_block(diff);
    }
}

/// Print one tool-output block to stderr: each line wrapped to the terminal
/// at a 2-cell margin, painted in the chrome gray. The plain-`ToolEnd`
/// summary and the live `ToolLog` head both print here, so a result looks the
/// same whether it streamed as it ran or arrived in one piece.
pub(crate) fn print_output_block(text: &str) {
    let width = crate::term::columns().max(20);
    let p = crate::theme::err();
    for line in text.lines() {
        let wrapped = crate::core::render_md::wrap_block(line, width, 2);
        eprintln!("{}{wrapped}{}", p.gray, p.reset);
    }
}

pub(crate) fn truncate_tail(text: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = lines[lines.len().saturating_sub(max_lines)..].join("\n");
    let mut truncated = lines.len() > max_lines;
    if out.len() > max_bytes {
        let start = crate::core::text::ceil_boundary(&out, out.len() - max_bytes);
        out = out[start..].to_string();
        truncated = true;
    }
    // the byte cap is not a token cap: a CJK dump of the same size costs
    // 3-4x more, so enforce the estimate too (binary search over char
    // boundaries keeps this cheap and never splits a codepoint)
    let tokens = crate::agent::compact::text_tokens;
    if tokens(&out) > MAX_TOKENS {
        let bounds: Vec<usize> = out.char_indices().map(|(i, _)| i).collect();
        let (mut lo, mut hi) = (0usize, bounds.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if tokens(&out[bounds[mid]..]) <= MAX_TOKENS {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        if lo < bounds.len() && lo > 0 {
            out = out[bounds[lo]..].to_string();
            truncated = true;
        }
    }
    (out, truncated)
}

/// Tail-truncate and mark, the shared ending for process-shaped tool output.
pub(crate) fn truncate_marked(text: &str, max_lines: usize, max_bytes: usize) -> String {
    let (mut out, truncated) = truncate_tail(text, max_lines, max_bytes);
    if truncated {
        out.push_str("\n[output truncated]\n");
    }
    out
}

/// Head-truncate and mark: the ending for content whose *beginning* is the
/// part worth keeping (a fetched page reads top-down; a command's error reads
/// bottom-up, which is why `truncate_marked` cuts the tail instead). Applies
/// the same three caps as the tail version — lines, bytes, and the token
/// estimate that makes the bound hold for CJK too — so no tool can hand the
/// model a result the others are capped below.
pub(crate) fn truncate_head_marked(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = lines[..lines.len().min(MAX_LINES)].join("\n");
    let mut truncated = lines.len() > MAX_LINES;
    if out.len() > MAX_BYTES {
        let end = crate::core::text::floor_boundary(&out, MAX_BYTES);
        out.truncate(end);
        truncated = true;
    }
    let tokens = crate::agent::compact::text_tokens;
    if tokens(&out) > MAX_TOKENS {
        // largest char-boundary prefix that fits. The token count is
        // nondecreasing in prefix length, so the valid cuts are a prefix of
        // the boundary list: binary-search the first that overflows and cut
        // one boundary back. `out.len()` is in the list to make the full
        // string searchable, and the guard above rules it out as the answer.
        let bounds: Vec<usize> = out
            .char_indices()
            .map(|(i, _)| i)
            .chain([out.len()])
            .collect();
        let (mut lo, mut hi) = (0usize, bounds.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if tokens(&out[..bounds[mid]]) <= MAX_TOKENS {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo > 0 {
            out.truncate(bounds[lo - 1]);
            truncated = true;
        }
    }
    if truncated {
        out.push_str("\n[output truncated]\n");
    }
    out
}

/// Merge captured process output into one stream: stderr rides under
/// stdout. Shared by the bash tool's normal and timed-out endings.
fn merge_process_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut out = String::from_utf8_lossy(stdout).into_owned();
    let err_text = String::from_utf8_lossy(stderr);
    if !err_text.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&err_text);
    }
    out
}

/// Tail-truncate process output and, when anything was cut, spill the full
/// text to the user tmp dir and name the file in the note (the reference's shape:
/// `[Showing lines X-Y of N. Full output: path]`). The model can read the
/// rest on demand instead of re-running the command.
pub(crate) fn truncate_with_spill(text: &str) -> String {
    truncate_with_spill_in(&crate::core::config::user_dir().join("tmp"), text)
}

/// [`truncate_with_spill`] against an explicit scratch dir (the test seam).
pub(crate) fn truncate_with_spill_in(dir: &Path, text: &str) -> String {
    let (out, truncated) = truncate_tail(text, MAX_LINES, MAX_BYTES);
    if !truncated {
        return out;
    }
    match spill_full_output(dir, text) {
        Some(path) => {
            let total = text.lines().count();
            let kept = out.lines().count().max(1);
            let start = total.saturating_sub(kept) + 1;
            let mut out = out;
            out.push_str(&format!(
                "\n\n[Showing lines {start}-{total} of {total}. Full output: {path}]"
            ));
            out
        }
        None => {
            let mut out = out;
            out.push_str("\n[output truncated]\n");
            out
        }
    }
}

/// Save the full output of a truncated command to the scratch dir (the
/// weekly sweep ages it out like every other scratch file there); the
/// truncation note names the file, the reference's spill. `None` when the write fails —
/// the caller falls back to the plain truncation mark rather than losing the
/// output twice.
fn spill_full_output(dir: &Path, text: &str) -> Option<String> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("bash-{}.log", crate::core::db::ulid()));
    std::fs::write(&path, text).ok()?;
    Some(path.display().to_string())
}

/// Finish a spawned-command result: merge stderr under stdout, note the
/// exit code, truncate and mark. Shared by the bash and script tools.
pub(crate) fn finish_process_output(stdout: Vec<u8>, stderr: Vec<u8>, code: i32) -> ToolOutput {
    let mut out = merge_process_output(&stdout, &stderr);
    if code != 0 {
        out.push_str(&format!("\nCommand exited with code {code}"));
    }
    let out = truncate_with_spill(&out);
    if code == 0 {
        ToolOutput::ok(out)
    } else {
        ToolOutput::err(out)
    }
}

/// Truncate to 60 chars with the single ellipsis glyph (the plugin preview
/// shape, now shared by script/MCP tool chrome).
pub(crate) fn short(s: &str) -> String {
    crate::core::text::truncate_chars(s, 60)
}

/// `{prefix} {args-as-json-truncated}` — the preview line for plugin tools.
pub(crate) fn args_preview(prefix: &str, args: &Value) -> String {
    format!(
        "{prefix} {}",
        short(&serde_json::to_string(args).unwrap_or_default())
    )
}

/// Display verb for the `$` chrome line: tool ids read as actions.
pub fn display_verb(name: &str) -> &str {
    match name {
        "bash" => "run",
        other => other,
    }
}

/// Resolve a tool path argument against cwd with `~` expansion.
pub(crate) fn resolve_path(cwd: &Path, arg: &str) -> PathBuf {
    let expanded = if let Some(rest) = arg.strip_prefix("~/") {
        let Some(home) = crate::core::paths::home_dir() else {
            return PathBuf::from(arg);
        };
        return home.join(rest);
    } else {
        arg
    };
    let p = Path::new(expanded);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

mod bash;
mod edit;
mod fetch;
mod fs;
mod plan;
mod read;
mod search;
mod write;

#[cfg(test)]
mod tests;
