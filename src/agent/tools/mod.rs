//! Built-in agent tools: declaration schemas, argument validation, execution.
//!
//! This file is the registry and everything the tools share (the `Tool`
//! trait, `ToolOutput`, schema validation, the truncation and process-output
//! helpers); each tool's declaration, execution and private helpers live in
//! its own child module, with the tests in `tests.rs`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::approval::Tier;
use crate::gitignore::{collect_files, parse_pattern, pattern_matches_path, scopes_for};
use bash::BashTool;
use edit::EditTool;
use fetch::FetchTool;
use fs::LsTool;
use plan::PlanTool;
use read::ReadTool;
use recall::RecallTool;
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
    pub is_error: bool,
    /// attachments (e.g. an image the read tool decoded) fed back to the
    /// model as vision input on the next round
    pub attachments: Vec<crate::providers::Attachment>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> ToolOutput {
        ToolOutput {
            content: content.into(),
            is_error: false,
            attachments: Vec::new(),
        }
    }

    pub fn err(content: impl Into<String>) -> ToolOutput {
        ToolOutput {
            content: content.into(),
            is_error: true,
            attachments: Vec::new(),
        }
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
    /// `log` receives live progress lines while the tool runs (bash streams
    /// its stdout); tools without progress simply ignore it
    fn execute(&self, args: &Value, cwd: &Path, log: &mut dyn FnMut(&str)) -> ToolOutput;
}

/// The `then_run` field shared by `write` and `edit`: the fused follow-up
/// command (action fusion — the mutation and its validation are one call).
pub(crate) const THEN_RUN_DESCRIPTION: &str = "Optional command to run next, in the same tool call, after this \
     mutation succeeds — e.g. run, build, test or restart it. Skipped when the mutation fails; a \
     non-zero exit is reported but keeps the change.";

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
        Box::new(RecallTool),
    ]
}

/// Minimal schema validation: required properties present with the declared
/// primitive type. Enough to bounce malformed calls back to the model.
pub fn validate(schema: &Value, args: &Value) -> Result<(), String> {
    let Some(obj) = args.as_object() else {
        return Err("arguments must be a JSON object".to_string());
    };
    let props = schema["properties"].as_object();
    let required = schema["required"].as_array().cloned().unwrap_or_default();
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

/// Finish a spawned-command result: merge stderr under stdout, note the
/// exit code, truncate and mark. Shared by the bash and script tools.
pub(crate) fn finish_process_output(stdout: Vec<u8>, stderr: Vec<u8>, code: i32) -> ToolOutput {
    let mut out = merge_process_output(&stdout, &stderr);
    if code != 0 {
        out.push_str(&format!("\nCommand exited with code {code}"));
    }
    let out = truncate_marked(&out, MAX_LINES, MAX_BYTES);
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
        // windows homes live in USERPROFILE; HOME covers the unix world
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
        if let Some(home) = home {
            return Path::new(&home).join(rest);
        }
        arg
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

fn display_rel(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

mod bash;
mod edit;
mod fetch;
mod fs;
mod plan;
mod read;
mod recall;
mod search;
mod write;

#[cfg(test)]
mod tests;
