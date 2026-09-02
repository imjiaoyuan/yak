//! Built-in agent tools: declaration schemas, argument validation, execution.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::approval::Tier;
use crate::gitignore::{collect_files, parse_pattern, pattern_matches_path, scopes_for};

/// Shared truncation constants (the reference's values).
pub(crate) const MAX_LINES: usize = 2000;
pub(crate) const MAX_BYTES: usize = 50 * 1024;
pub(crate) const GREP_LINE_LIMIT: usize = 500;
/// The read tool serves one window at a time; bash/grep keep MAX_LINES.
pub(crate) const READ_MAX_LINES: usize = 500;
/// Files larger than this are skipped by grep (reading them whole would
/// spike memory; the model can target them with bash instead).
const GREP_MAX_FILE: u64 = 32 * 1024 * 1024;
/// Images the read tool will base64 into the model request; beyond this we
/// have no downscaler, so we refuse rather than balloon the context.
const IMAGE_MAX_BYTES: usize = 8 * 1024 * 1024;

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
/// to the terminal, near-monochrome — additions default, deletions gray,
/// headers and context dim.
pub fn print_diff_block(diff: &str) {
    let width = crate::term::columns().max(20);
    for line in diff.split('\n') {
        let wrapped = crate::core::render_md::wrap_block(line, width, 2);
        let styled = match line.chars().next() {
            Some('+') => format!("  {wrapped}"),
            Some('-') => format!("\x1b[90m  {wrapped}\x1b[0m"),
            _ => format!("\x1b[2m  {wrapped}\x1b[0m"),
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

/// Same registry, but the task tool inherits the parent's model and the
/// `[agent.roles]` map for sub-agent model selection.
pub fn builtin_tools_configured(
    parent_model: Option<&str>,
    roles: &std::collections::BTreeMap<String, String>,
) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ReadTool),
        Box::new(WriteTool),
        Box::new(EditTool),
        Box::new(BashTool),
        Box::new(GrepTool),
        Box::new(GlobTool),
        Box::new(LsTool),
        Box::new(FetchTool),
        Box::new(PlanTool),
        Box::new(super::task::TaskTool {
            parent_model: parent_model.map(str::to_string),
            roles: roles.clone(),
        }),
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
pub(crate) fn truncate_tail(text: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = lines[lines.len().saturating_sub(max_lines)..].join("\n");
    let mut truncated = lines.len() > max_lines;
    if out.len() > max_bytes {
        let start = crate::core::text::ceil_boundary(&out, out.len() - max_bytes);
        out = out[start..].to_string();
        truncated = true;
    }
    (out, truncated)
}

struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn description(&self) -> &str {
        "Read a file. Text files return lines with line numbers optionally sliced by \
         offset/limit; image files (png/jpg/gif/webp/bmp) are sent to the model as a vision \
         attachment. Other binary formats are refused with a hint at local tooling \
         (pdftotext, samtools, ...)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, relative to the working directory"},
                "offset": {"type": "integer", "description": "1-based line to start from"},
                "limit": {"type": "integer", "description": "Maximum number of lines to return"},
            },
            "required": ["path"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["path"].as_str().unwrap_or("?").to_string()
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let path = resolve_path(cwd, args["path"].as_str().unwrap_or(""));
        if let Some(mime) = image_mime(&path) {
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => return ToolOutput::err(format!("cannot read {} ({e})", path.display())),
            };
            // images are sent as base64 to the model: refuse a huge file
            // rather than balloon the request (we have no image processor
            // to downscale without adding a dependency)
            if bytes.len() > IMAGE_MAX_BYTES {
                return ToolOutput::err(format!(
                    "{}: image is {} (over the {} limit); downscale it first with e.g. `python -c \"from PIL import Image; im=Image.open('{}'); im.thumbnail((2000,2000)); im.save('{}')\"`",
                    path.display(),
                    crate::core::text::human_bytes(bytes.len() as u64),
                    crate::core::text::human_bytes(IMAGE_MAX_BYTES as u64),
                    path.display(),
                    path.display(),
                ));
            }
            let mut out = ToolOutput::ok(format!(
                "Read image file [{}; {}] — passed to the model as a vision attachment.",
                mime,
                crate::core::text::human_bytes(bytes.len() as u64)
            ));
            out.attachments.push(crate::providers::Attachment {
                mime_type: mime,
                base64_data: crate::b64::encode(&bytes),
                filename: path.file_name().map(|n| n.to_string_lossy().into_owned()),
            });
            return out;
        }
        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"]
            .as_u64()
            .map(|l| l as usize)
            .unwrap_or(READ_MAX_LINES)
            .clamp(1, READ_MAX_LINES);
        let w = match crate::read::window(&path, offset, limit) {
            Ok(w) => w,
            Err(crate::read::Error::Io(e)) => {
                return ToolOutput::err(format!("cannot read {} ({e})", path.display()));
            }
            Err(crate::read::Error::Binary { ext }) => {
                let hint = crate::read::binary_hint(&ext)
                    .map(|h| format!(": try `{h}`"))
                    .unwrap_or_default();
                return ToolOutput::err(format!(
                    "{}: binary format{hint}, use bash",
                    path.display()
                ));
            }
        };
        if w.lines.is_empty() {
            return match w.total {
                crate::read::LineCount::Exact(0) => ToolOutput::ok("(empty file)"),
                crate::read::LineCount::Exact(n) => ToolOutput::err(format!(
                    "offset {offset} is past the end of the file ({n} lines)"
                )),
                crate::read::LineCount::AtLeast(_) => {
                    ToolOutput::err(format!("offset {offset} is past the end of the file"))
                }
            };
        }
        let numbered: Vec<String> = w
            .lines
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{}: {}", w.start + i, l))
            .collect();
        // assemble under the byte cap at line boundaries, so the note can
        // point at the first line the model has not actually seen
        let mut out = String::new();
        let mut kept = 0usize;
        for line in &numbered {
            if out.len() + line.len() + 1 > MAX_BYTES {
                break;
            }
            if kept > 0 {
                out.push('\n');
            }
            out.push_str(line);
            kept += 1;
        }
        let byte_cut = kept < numbered.len();
        let last = w.start + kept - 1;
        if !w.eof {
            out.push_str(&format!(
                "\n[Showing lines {}-{} of {}. Use offset={} to continue.]\n",
                w.start,
                last,
                w.total.describe(),
                last + 1
            ));
        } else if byte_cut {
            out.push_str("\n[output truncated]\n");
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let header = format!(
            "[{} · text · {} lines · {} · showing {}-{}]\n",
            name,
            w.total.describe(),
            crate::core::text::human_bytes(w.size),
            w.start,
            last
        );
        ToolOutput::ok(format!("{header}{out}"))
    }
}

struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn tier(&self) -> Tier {
        Tier::Write
    }
    fn description(&self) -> &str {
        "Write a file, creating parent directories as needed. Overwrites existing content."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        let bytes = args["content"].as_str().map(|c| c.len()).unwrap_or(0);
        format!(
            "write {} ({} bytes)",
            args["path"].as_str().unwrap_or("?"),
            bytes
        )
    }
    fn diff(&self, args: &Value, cwd: &Path) -> Option<String> {
        let path = resolve_path(cwd, args["path"].as_str().unwrap_or(""));
        let new = args["content"].as_str().unwrap_or("");
        match std::fs::read_to_string(&path) {
            // overwriting: one whole-file hunk
            Ok(original) => Some(change_hunks(
                &original,
                &[(0, original.len(), new)],
                2,
                DIFF_MAX_LINES,
            )),
            // a new file: the incoming content as additions
            Err(_) => {
                let mut out: Vec<String> = Vec::new();
                for l in new.split('\n').take(DIFF_MAX_LINES) {
                    out.push(format!("+ {l}"));
                }
                if new.split('\n').count() > DIFF_MAX_LINES {
                    out.push("  · more lines not shown".to_string());
                }
                Some(out.join("\n"))
            }
        }
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let path = resolve_path(cwd, args["path"].as_str().unwrap_or(""));
        let content = args["content"].as_str().unwrap_or("");
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return ToolOutput::err(format!("cannot create {}: {e}", parent.display()));
        }
        match std::fs::write(&path, content) {
            Ok(()) => ToolOutput::ok(format!(
                "wrote {} bytes to {}",
                content.len(),
                path.display()
            )),
            Err(e) => ToolOutput::err(format!("write failed: {e}")),
        }
    }
}

struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn tier(&self) -> Tier {
        Tier::Write
    }
    fn description(&self) -> &str {
        "Apply exact-match text edits to a file. Each oldText must appear exactly once in the \
         original; all edits match the original text, not each other's results."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {"type": "string"},
                            "newText": {"type": "string"}
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        let n = args["edits"].as_array().map(|a| a.len()).unwrap_or(0);
        format!(
            "{} ({} change{})",
            args["path"].as_str().unwrap_or("?"),
            n,
            if n == 1 { "" } else { "s" }
        )
    }
    fn diff(&self, args: &Value, cwd: &Path) -> Option<String> {
        let path = resolve_path(cwd, args["path"].as_str().unwrap_or(""));
        let original = std::fs::read_to_string(path).ok()?;
        let edits = args["edits"].as_array()?;
        // locate each oldText like execute() will; the first match is
        // enough for a preview
        let mut spans: Vec<(usize, usize, &str)> = Vec::new();
        for e in edits {
            let (old, new) = (e["oldText"].as_str()?, e["newText"].as_str()?);
            let start = original.find(old)?;
            spans.push((start, start + old.len(), new));
        }
        spans.sort_by_key(|(s, _, _)| *s);
        Some(change_hunks(&original, &spans, 2, DIFF_MAX_LINES))
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let path = resolve_path(cwd, args["path"].as_str().unwrap_or(""));
        let Ok(original) = std::fs::read_to_string(&path) else {
            return ToolOutput::err(format!("cannot read {}", path.display()));
        };
        let Some(edits) = args["edits"].as_array() else {
            return ToolOutput::err("edits must be an array");
        };
        if edits.is_empty() {
            return ToolOutput::err("edits must not be empty");
        }
        // locate every edit against the original text; require unique matches
        let mut spans: Vec<(usize, usize, &str)> = Vec::new();
        for e in edits {
            let (Some(old), Some(new)) = (e["oldText"].as_str(), e["newText"].as_str()) else {
                return ToolOutput::err("each edit needs oldText and newText strings");
            };
            if old.is_empty() {
                return ToolOutput::err("oldText must not be empty");
            }
            let first = match original.find(old) {
                Some(i) => i,
                None => {
                    return ToolOutput::err(format!(
                        "oldText not found in {}:\n{old}",
                        path.display()
                    ));
                }
            };
            if original[first + old.len()..].contains(old) {
                return ToolOutput::err(format!(
                    "oldText matches more than once in {} (include surrounding lines to disambiguate):\n{old}",
                    path.display()
                ));
            }
            spans.push((first, first + old.len(), new));
        }
        // apply back-to-front so earlier byte offsets stay valid
        spans.sort_by_key(|(s, _, _)| *s);
        for pair in spans.windows(2) {
            if pair[0].1 > pair[1].0 {
                return ToolOutput::err("edits overlap");
            }
        }
        let mut text = original.clone();
        for (start, end, new) in spans.iter().rev() {
            text.replace_range(start..end, new);
        }
        match std::fs::write(&path, &text) {
            Ok(()) => ToolOutput::ok(format!(
                "applied {} edits to {}",
                spans.len(),
                path.display()
            )),
            Err(e) => ToolOutput::err(format!("write failed: {e}")),
        }
    }
}

/// A unified-diff style preview of exact-match edits: one linear walk over
/// the original lines, emitting `@@` headers, context, `-` and `+` rows
/// (plain text; colors are applied at print time). Distant changes become
/// separate hunks; the output is capped at `max_lines` rows.
pub(crate) fn change_hunks(
    original: &str,
    spans: &[(usize, usize, &str)],
    context: usize,
    max_lines: usize,
) -> String {
    if spans.is_empty() || max_lines == 0 {
        return String::new();
    }
    let lines: Vec<&str> = original.split('\n').collect();
    let mut line_starts: Vec<usize> = Vec::with_capacity(lines.len());
    let mut off = 0usize;
    for l in &lines {
        line_starts.push(off);
        off += l.len() + 1;
    }
    let line_of = |byte: usize| -> usize {
        match line_starts.binary_search(&byte) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    };

    let mut out: Vec<String> = Vec::new();
    let mut shown = 0usize;
    let mut cursor = 0usize; // next original line not yet emitted
    let mut hunk_open = false;
    // the empty split artifact after a trailing newline is not a line
    let real = |i: usize| !(i + 1 == lines.len() && lines[i].is_empty());
    for &(start, end, new) in spans {
        let start_line = line_of(start);
        // the last removed line contains byte end-1
        let end_line = if end == 0 { 0 } else { line_of(end - 1) + 1 };
        let ctx_to = (end_line + context).saturating_sub(1).min(lines.len() - 1);
        let mut ctx_from = start_line.saturating_sub(context);
        // a distant change opens a new hunk; a near one continues the last
        if !hunk_open || ctx_from > cursor {
            if shown >= max_lines {
                break;
            }
            out.push(format!("@@ line {}", ctx_from + 1));
            shown += 1;
            hunk_open = true;
        } else {
            ctx_from = cursor;
        }
        let mut i = ctx_from;
        while i < start_line && shown < max_lines {
            if real(i) {
                out.push(format!("  {}", lines[i]));
                shown += 1;
            }
            i += 1;
        }
        let mut r = start_line;
        while r < end_line && shown < max_lines {
            if real(r) {
                out.push(format!("- {}", lines[r]));
                shown += 1;
            }
            r += 1;
        }
        for l in new.split('\n').filter(|_| !new.is_empty()) {
            if shown >= max_lines {
                break;
            }
            out.push(format!("+ {l}"));
            shown += 1;
        }
        let mut a = end_line;
        while a <= ctx_to && shown < max_lines {
            if real(a) {
                out.push(format!("  {}", lines[a]));
                shown += 1;
            }
            a += 1;
        }
        cursor = ctx_to + 1;
        if shown >= max_lines {
            break;
        }
    }
    if shown >= max_lines {
        out.push("  · more lines not shown".to_string());
    }
    out.join("\n")
}

struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn tier(&self) -> Tier {
        Tier::Exec
    }
    fn description(&self) -> &str {
        "Run a shell command in the working directory. Output merges stdout and \
         stderr; nonzero exits are reported as errors. Use for anything the file tools cannot do."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout": {"type": "integer", "description": "Seconds before the process group is killed (default 120)"}
            },
            "required": ["command"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["command"].as_str().unwrap_or("?").to_string()
    }
    fn execute(&self, args: &Value, cwd: &Path, log: &mut dyn FnMut(&str)) -> ToolOutput {
        let command = args["command"].as_str().unwrap_or("");
        let timeout = args["timeout"].as_u64().unwrap_or(120);
        let outcome = crate::platform::run_shell_stream(
            command,
            cwd,
            timeout,
            crate::core::http::interrupt_flag(),
            log,
        );
        if outcome.interrupted {
            return ToolOutput::err("command interrupted");
        }
        if outcome.timed_out {
            return ToolOutput::err(format!("command timed out after {timeout}s: {command}"));
        }
        let stdout = outcome.stdout;
        let stderr = outcome.stderr;
        let mut out = String::from_utf8_lossy(&stdout).into_owned();
        let err_text = String::from_utf8_lossy(&stderr);
        if !err_text.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&err_text);
        }
        let code = outcome.code;
        if code != 0 {
            out.push_str(&format!("\nCommand exited with code {code}"));
        }
        let (mut out, truncated) = truncate_tail(&out, MAX_LINES, MAX_BYTES);
        if truncated {
            out.push_str("\n[output truncated]\n");
        }
        if code == 0 {
            ToolOutput::ok(out)
        } else {
            ToolOutput::err(out)
        }
    }
}

struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn description(&self) -> &str {
        "Search file contents for a LITERAL substring (no regex — prefer `rg` via the bash tool, \
         which is faster and supports regex). \
         Respects .gitignore. Returns path:line:text matches."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Literal substring to find"},
                "path": {"type": "string", "description": "File or directory to search (default .)"},
                "glob": {"type": "string", "description": "Only search files matching this glob, e.g. *.rs"},
                "ignore_case": {"type": "boolean"},
                "context": {"type": "integer", "description": "Lines of context around each match"},
                "limit": {"type": "integer", "description": "Maximum matches (default 100)"}
            },
            "required": ["pattern"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        format!("\"{}\"", args["pattern"].as_str().unwrap_or("?"))
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let pattern = args["pattern"].as_str().unwrap_or("");
        let ignore_case = args["ignore_case"].as_bool().unwrap_or(false);
        let context = args["context"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().unwrap_or(100) as usize;
        let glob = args["glob"].as_str().and_then(parse_pattern);
        let needle = if ignore_case {
            pattern.to_lowercase()
        } else {
            pattern.to_string()
        };

        let root = resolve_path(cwd, args["path"].as_str().unwrap_or("."));
        let files = gather_files(&root, glob.as_ref());
        let mut matches: Vec<String> = Vec::new();
        let mut skipped = 0usize;
        'outer: for path in files {
            if crate::read::is_binary_path(&path) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.len() > GREP_MAX_FILE {
                skipped += 1;
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            // context ranges grow monotonically with the match order, so a
            // watermark replaces the per-line dedup set (which was O(n²))
            let mut printed_to = 0usize;
            let mut i = 0usize;
            while i < lines.len() {
                let hit = if ignore_case {
                    lines[i].to_lowercase().contains(&needle)
                } else {
                    lines[i].contains(needle.as_str())
                };
                if hit {
                    let lo = i.saturating_sub(context).max(printed_to);
                    let hi = (i + context).min(lines.len() - 1);
                    for (offset, line) in lines[lo..=hi].iter().enumerate() {
                        let mut l = line.to_string();
                        truncate_in_place(&mut l, GREP_LINE_LIMIT);
                        matches.push(format!(
                            "{}:{}: {}",
                            display_rel(cwd, &path),
                            lo + offset + 1,
                            l
                        ));
                    }
                    printed_to = hi + 1;
                    if matches.len() >= limit {
                        break 'outer;
                    }
                    i += 1;
                } else {
                    i += 1;
                }
            }
        }
        if matches.is_empty() {
            return ToolOutput::ok("no matches\n");
        }
        let mut out = matches.join("\n");
        out.push('\n');
        if skipped > 0 {
            out.push_str(&format!(
                "\n[{skipped} file(s) over {GREP_MAX_FILE} bytes skipped; use bash]\n"
            ));
        }
        let (out, truncated) = truncate_tail(&out, MAX_LINES, MAX_BYTES);
        let mut out = out;
        if truncated {
            out.push_str("\n[output truncated]\n");
        }
        ToolOutput::ok(out)
    }
}

struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn description(&self) -> &str {
        "List files whose paths match a glob pattern (e.g. src/**/*.rs). Respects .gitignore."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "Directory to search (default .)"},
                "limit": {"type": "integer", "description": "Maximum results (default 1000)"}
            },
            "required": ["pattern"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["pattern"].as_str().unwrap_or("?").to_string()
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let raw = args["pattern"].as_str().unwrap_or("");
        let limit = args["limit"].as_u64().unwrap_or(1000) as usize;
        let root = resolve_path(cwd, args["path"].as_str().unwrap_or("."));
        let files = gather_files(&root, None);
        // compile once, match per file (parsing per file allocated per entry)
        let Some(pattern) = parse_pattern(raw) else {
            return ToolOutput::err(format!("invalid glob pattern '{raw}'"));
        };
        let mut hits: Vec<String> = Vec::new();
        for path in files {
            let rel = display_rel(cwd, &path);
            if pattern_matches_path(&pattern, &rel, false) {
                hits.push(rel);
                if hits.len() >= limit {
                    break;
                }
            }
        }
        if hits.is_empty() {
            return ToolOutput::ok("no matches\n");
        }
        let mut out = hits.join("\n");
        out.push('\n');
        ToolOutput::ok(out)
    }
}

struct LsTool;

impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn description(&self) -> &str {
        "List a directory's entries, alphabetical, directories suffixed with /."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory to list (default .)"},
                "limit": {"type": "integer", "description": "Maximum entries (default 500)"}
            }
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["path"].as_str().unwrap_or(".").to_string()
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let limit = args["limit"].as_u64().unwrap_or(500) as usize;
        let dir = resolve_path(cwd, args["path"].as_str().unwrap_or("."));
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return ToolOutput::err(format!("cannot list {}", dir.display()));
        };
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if e.path().is_dir() {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .collect();
        names.sort();
        if names.len() > limit {
            let more = names.len() - limit;
            names.truncate(limit);
            names.push(format!("... ({more} more)"));
        }
        let mut out = names.join("\n");
        out.push('\n');
        ToolOutput::ok(out)
    }
}

/// Cap on the response body returned to the model (bytes).
const FETCH_MAX_BYTES: usize = 256 * 1024;

/// Fetch a URL and hand the model plain text: strips HTML tags/scripts for
/// web pages, returns other text bodies as-is. Reads are Tier::Read, so the
/// agent can fetch freely without an approval prompt (like the reference's webfetch).
struct FetchTool;

impl Tool for FetchTool {
    fn name(&self) -> &str {
        "webfetch"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn description(&self) -> &str {
        "Fetch a URL and return its content as plain text. Use for docs, articles, \
         API pages and other web resources. HTML is stripped to text; the body is \
         capped at 256KB."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "http(s) URL to fetch"}
            },
            "required": ["url"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["url"].as_str().unwrap_or("").to_string()
    }
    fn execute(&self, args: &Value, _cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let Some(url) = args["url"].as_str() else {
            return ToolOutput::err("missing string argument 'url'");
        };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return ToolOutput::err(format!("only http(s) URLs are allowed (refusing {url})"));
        }
        let body = match crate::core::http::get_text_short(url) {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(e),
        };
        let text = html_to_text(&body);
        if text.len() > FETCH_MAX_BYTES {
            let end = crate::core::text::floor_boundary(&text, FETCH_MAX_BYTES);
            return ToolOutput::ok(format!(
                "{}…\n[truncated: {} bytes, showing first {}]",
                &text[..end],
                text.len(),
                end
            ));
        }
        ToolOutput::ok(text)
    }
}

struct PlanTool;

impl Tool for PlanTool {
    fn name(&self) -> &str {
        "update_plan"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn description(&self) -> &str {
        "Update the task plan. Provide an optional explanation and a list of plan items, each \
         with a step and a status (pending, in_progress, completed). At most one step can be \
         in_progress at a time. Keep a step-by-step plan for a multi-step task and update it \
         after each step you finish."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "explanation": {"type": "string", "description": "Optional explanation for this plan update"},
                "plan": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "step": {"type": "string", "description": "Task step text"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["step", "status"]
                    }
                }
            },
            "required": ["plan"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        let n = args["plan"].as_array().map(|a| a.len()).unwrap_or(0);
        format!("{n} step(s)")
    }
    fn execute(&self, args: &Value, _cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let Some(plan) = args["plan"].as_array() else {
            return ToolOutput::err("missing 'plan' array");
        };
        if plan.is_empty() {
            return ToolOutput::err("plan must not be empty");
        }
        for item in plan {
            if item["step"].as_str().is_none() || item["status"].as_str().is_none() {
                return ToolOutput::err("each plan item needs 'step' and 'status'");
            }
        }
        ToolOutput::ok("plan updated")
    }
}

/// Strip markup down to readable text without pulling in an HTML parser:
/// drop script/style blocks, remove tags, decode common entities, collapse
/// runs of blank lines. Good enough for docs and articles.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut skip_depth = 0i32; // inside <script>/<style>
    let mut rest = html;
    while let Some(pos) = rest.find('<') {
        if skip_depth == 0 {
            out.push_str(&rest[..pos]);
        }
        let tag_start = pos;
        let tag_end = rest[pos..]
            .find('>')
            .map(|i| pos + i + 1)
            .unwrap_or(rest.len());
        let tag = &rest[tag_start..tag_end];
        let lower = tag.to_ascii_lowercase();
        let name: String = lower
            .trim_start_matches('<')
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '>')
            .next()
            .unwrap_or("")
            .to_string();
        match name.as_str() {
            "script" | "style" => {
                if lower.contains("</") {
                    skip_depth = skip_depth.saturating_sub(1);
                } else {
                    skip_depth += 1;
                }
            }
            "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "pre"
                if skip_depth == 0 =>
            {
                out.push('\n');
            }
            _ => {}
        }
        rest = &rest[tag_end..];
        // consume the text until the next tag, skipping script/style bodies
        if let Some(end) = rest.find('<') {
            if skip_depth == 0 {
                out.push_str(&rest[..end]);
            }
            rest = &rest[end..];
        } else {
            if skip_depth == 0 {
                out.push_str(rest);
            }
            rest = "";
        }
    }
    out.push_str(rest);
    // decode the common entities (&amp; last: it must not re-decode the
    // text the other expansions just produced)
    let mut decoded = out.replace("&nbsp;", " ");
    decoded = decoded.replace("&lt;", "<").replace("&gt;", ">");
    decoded = decoded.replace("&quot;", "\"").replace("&#39;", "'");
    decoded = decoded.replace("&amp;", "&");
    // collapse blank-line runs and trim
    let mut clean = String::new();
    let mut blank = 0;
    for line in decoded.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank += 1;
            if blank <= 1 {
                clean.push('\n');
            }
            continue;
        }
        blank = 0;
        clean.push_str(t);
        clean.push('\n');
    }
    clean.trim().to_string()
}

/// Resolve a tool path argument against cwd with `~` expansion.
/// Display verb for the `$` chrome line: tool ids read as actions.
pub fn display_verb(name: &str) -> &str {
    match name {
        "bash" => "run",
        n if n.starts_with("mcp__") => "mcp",
        other => other,
    }
}

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

/// The mime type for a file extension that the read tool can send to a
/// vision-capable model; `None` for anything else.
fn image_mime(path: &Path) -> Option<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    Some(
        match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            _ => return None,
        }
        .to_string(),
    )
}

fn display_rel(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

fn gather_files(root: &Path, glob: Option<&crate::gitignore::Pattern>) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let scopes = scopes_for(root);
    let mut files = Vec::new();
    collect_files(root, root, &scopes, &mut files);
    if let Some(glob) = glob {
        files.retain(|f| {
            let rel = f
                .strip_prefix(root)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            pattern_matches_path(glob, &rel, false)
        });
    }
    files.sort();
    files
}

fn truncate_in_place(s: &mut String, max: usize) {
    if s.len() > max {
        s.truncate(crate::core::text::floor_boundary(s, max));
        s.push('…');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_change_hunks_single_edit_with_context() {
        let original = "one\ntwo\nthree\nfour\nfive\n";
        let start = original.find("three").unwrap();
        let spans = [(start, start + 5, "THREE")];
        let out = change_hunks(original, &spans, 1, 30);
        assert_eq!(out, "@@ line 2\n  two\n- three\n+ THREE\n  four");
    }

    #[test]
    fn test_change_hunks_distant_edits_open_two_hunks() {
        let original = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let s1 = original.find("b").unwrap();
        let s2 = original.find("i").unwrap();
        let spans = [(s1, s1 + 1, "B"), (s2, s2 + 1, "I")];
        let out = change_hunks(original, &spans, 1, 30);
        assert_eq!(
            out,
            "@@ line 1\n  a\n- b\n+ B\n  c\n@@ line 8\n  h\n- i\n+ I\n  j"
        );
    }

    #[test]
    fn test_change_hunks_deletion_has_no_empty_addition() {
        let original = "x\ny\nz\n";
        let start = original.find("y").unwrap();
        let spans = [(start, start + 2, "")]; // "y\n" removed
        let out = change_hunks(original, &spans, 0, 30);
        assert_eq!(out, "@@ line 2\n- y");
    }

    #[test]
    fn test_change_hunks_caps_output() {
        let original: String = (0..50).map(|i| format!("line{i}\n")).collect();
        let spans = [(0, original.len(), "new content")];
        let out = change_hunks(&original, &spans, 2, 5);
        assert_eq!(out.lines().count(), 6); // 5 rows + the marker
        assert!(out.ends_with("· more lines not shown"));
    }

    #[test]
    fn test_change_hunks_multiline_replacement() {
        let original = "fn a() {}\nfn b() {}\n";
        let start = original.find("fn b() {}").unwrap();
        let spans = [(start, start + 10, "fn b() {\n    todo!();\n}")];
        let out = change_hunks(original, &spans, 1, 30);
        assert_eq!(
            out,
            "@@ line 1\n  fn a() {}\n- fn b() {}\n+ fn b() {\n+     todo!();\n+ }"
        );
    }

    #[test]
    fn test_change_hunks_trailing_newline_no_artifact() {
        let original = "first line\nhello world\nlast line\n";
        let start = original.find("hello world").unwrap();
        let spans = [(start, start + 11, "hello diff preview")];
        let out = change_hunks(original, &spans, 2, 30);
        println!("{out}");
        assert_eq!(
            out,
            "@@ line 1\n  first line\n- hello world\n+ hello diff preview\n  last line"
        );
    }

    #[test]
    fn truncate_tail_caps_lines_and_bytes() {
        let text = (1..=3000)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let (tail, truncated) = truncate_tail(&text, MAX_LINES, MAX_BYTES);
        assert!(truncated);
        assert!(tail.lines().next().unwrap().parse::<usize>().unwrap() > 1000);
        let wide = vec!["x".repeat(300); 3000].join("\n");
        let (tail, truncated) = truncate_tail(&wide, MAX_LINES, MAX_BYTES);
        assert!(truncated);
        assert!(tail.len() <= MAX_BYTES + 300);
    }

    #[test]
    fn validation_bounces_bad_args() {
        let schema = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}, "n": {"type": "integer"}},
            "required": ["path"]
        });
        assert!(validate(&schema, &json!({"path": "x"})).is_ok());
        assert!(validate(&schema, &json!({})).is_err());
        assert!(validate(&schema, &json!({"path": 3})).is_err());
        assert!(validate(&schema, &json!({"path": "x", "n": 5})).is_ok());
        assert!(validate(&schema, &json!({"path": "x", "n": "five"})).is_err());
        assert!(validate(&schema, &json!("not an object")).is_err());
    }

    #[test]
    fn html_to_text_strips_markup_and_scripts() {
        let html = "<html><head><style>p { color: red }</style></head><body>\
                    <h1>Hello</h1><p>Some <b>bold</b> text.</p>\
                    <script>var leak = \"secret <hidden>\";</script>\
                    <p>After &amp; before.</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("Some bold text."));
        assert!(text.contains("After & before."));
        assert!(!text.contains("secret"));
        assert!(!text.contains("color: red"));
    }

    #[test]
    fn webfetch_refuses_non_http_schemes() {
        let tool = FetchTool;
        let out = tool.execute(
            &json!({"url": "file:///etc/passwd"}),
            Path::new("."),
            &mut |_| {},
        );
        assert!(out.is_error);
        let out = tool.execute(
            &json!({"url": "ftp://example.com/x"}),
            Path::new("."),
            &mut |_| {},
        );
        assert!(out.is_error);
        let out = tool.execute(&json!({}), Path::new("."), &mut |_| {});
        assert!(out.is_error);
    }

    #[test]
    fn edit_requires_unique_matches_and_no_overlap() {
        let dir = std::env::temp_dir().join(format!("llm-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

        let dup = EditTool.execute(
            &json!({"path": file.display().to_string(), "edits": [{"oldText": "o", "newText": "0"}]}),
            Path::new("."),
            &mut |_| {},
        );
        assert!(dup.is_error, "'o' appears twice and must be rejected");

        let missing = EditTool.execute(
            &json!({"path": file.display().to_string(), "edits": [{"oldText": "nope", "newText": "x"}]}),
            Path::new("."),
            &mut |_| {},
        );
        assert!(missing.is_error);

        let overlap = EditTool.execute(
            &json!({"path": file.display().to_string(), "edits": [
                {"oldText": "two", "newText": "2"},
                {"oldText": "one\ntwo", "newText": "A"}
            ]}),
            Path::new("."),
            &mut |_| {},
        );
        assert!(overlap.is_error, "overlapping spans must be rejected");

        let ok = EditTool.execute(
            &json!({"path": file.display().to_string(), "edits": [
                {"oldText": "one", "newText": "1"},
                {"oldText": "three", "newText": "3"}
            ]}),
            Path::new("."),
            &mut |_| {},
        );
        assert!(!ok.is_error, "{}", ok.content);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "1\ntwo\n3\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_literal_and_ignore_case() {
        let dir = std::env::temp_dir().join(format!("llm-grep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("one.txt"), "Hello world\nbye\n").unwrap();
        std::fs::write(dir.join("two.txt"), "nope\n").unwrap();

        let out = GrepTool.execute(
            &json!({"pattern": "hello", "path": dir.display().to_string(), "ignore_case": true}),
            Path::new("."),
            &mut |_| {},
        );
        assert!(!out.is_error);
        assert!(
            out.content.contains("one.txt:1: Hello world"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("two.txt"));

        let exact = GrepTool.execute(
            &json!({"pattern": "hello", "path": dir.display().to_string()}),
            Path::new("."),
            &mut |_| {},
        );
        assert_eq!(exact.content, "no matches\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bash_output_beyond_pipe_buffer_does_not_deadlock() {
        // ~108KB of output used to fill the 64KiB pipe and hang until timeout
        let out = BashTool.execute(
            &json!({"command": "seq 1 20000", "timeout": 30}),
            Path::new("."),
            &mut |_| {},
        );
        assert!(!out.is_error, "{}", out.content);
        // truncated to the last MAX_LINES lines (plus the truncation marker)
        assert!(out.content.lines().count() < 20000);
        assert!(out.content.contains("20000"));
    }

    #[test]
    fn glob_finds_matching_files() {
        let dir = std::env::temp_dir().join(format!("llm-glob-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "").unwrap();
        std::fs::write(dir.join("b.txt"), "").unwrap();
        let out = GlobTool.execute(
            &json!({"pattern": "**/*.rs", "path": dir.display().to_string()}),
            Path::new("."),
            &mut |_| {},
        );
        assert!(out.content.contains("a.rs"), "{}", out.content);
        assert!(!out.content.contains("b.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_context_dedups_overlapping_matches() {
        let dir = std::env::temp_dir().join(format!("llm-grepctx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // hits on adjacent lines with context 1: every line appears exactly
        // once, shared context included
        std::fs::write(
            dir.join("app.txt"),
            "one\nhit alpha\nhit beta\nhit gamma\nfour\n",
        )
        .unwrap();
        let out = GrepTool.execute(
            &json!({"pattern": "hit", "path": dir.display().to_string(), "context": 1}),
            Path::new("."),
            &mut |_| {},
        );
        let line_numbers: Vec<usize> = out
            .content
            .lines()
            .filter(|l| l.contains("app.txt"))
            .map(|l| {
                // `path:line: text`, where the path may carry a drive-letter
                // colon (C:/...) and the text may too — so peel from the right
                let (head, _) = l.rsplit_once(": ").expect("num: text tail");
                let (_, num) = head.rsplit_once(':').expect("path:num");
                num.parse().unwrap()
            })
            .collect();
        assert_eq!(line_numbers, vec![1, 2, 3, 4, 5], "{}", out.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_survives_a_directory_symlink_cycle() {
        let dir = std::env::temp_dir().join(format!("llm-greplink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("needle.txt"), "find me\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&dir, dir.join("loop")).unwrap();
        // the walk must treat `loop` as a file (not follow it into a cycle
        // and blow the stack)
        let out = GrepTool.execute(
            &json!({"pattern": "find me", "path": dir.display().to_string()}),
            Path::new("."),
            &mut |_| {},
        );
        assert!(out.content.contains("needle.txt:1"), "{}", out.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn read_execute(dir: &std::path::Path, args: Value) -> ToolOutput {
        ReadTool.execute(&args, dir, &mut |_| {})
    }

    #[test]
    fn read_tool_windows_with_meta_header_and_note() {
        let dir = std::env::temp_dir().join(format!("llm-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        let body: Vec<String> = (1..=10).map(|i| format!("line-{i}")).collect();
        std::fs::write(&file, body.join("\n")).unwrap();

        let out = read_execute(&dir, json!({"path": "notes.txt", "offset": 3, "limit": 4}));
        assert!(!out.is_error);
        assert!(
            out.content.starts_with("[notes.txt · text · ≥7 lines · "),
            "{}",
            out.content
        );
        assert!(out.content.contains("3: line-3"));
        assert!(out.content.contains("6: line-6"));
        assert!(!out.content.contains("7: line-7"));
        assert!(
            out.content
                .contains("[Showing lines 3-6 of ≥7. Use offset=7 to continue.]"),
            "{}",
            out.content
        );

        // a window that reaches EOF reports exact totals and no note
        let out = read_execute(&dir, json!({"path": "notes.txt", "offset": 8, "limit": 5}));
        assert!(out.content.contains("· 10 lines ·"), "{}", out.content);
        assert!(!out.content.contains("Use offset="), "{}", out.content);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_tool_caps_at_read_max_lines() {
        let dir = std::env::temp_dir().join(format!("llm-readcap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let body: Vec<String> = (1..=1200).map(|i| format!("row-{i}")).collect();
        std::fs::write(dir.join("big.txt"), body.join("\n")).unwrap();
        let out = read_execute(&dir, json!({"path": "big.txt"}));
        assert!(out.content.contains("1: row-1"), "{}", out.content);
        assert!(out.content.contains("500: row-500"), "{}", out.content);
        assert!(!out.content.contains("501: row-501"));
        assert!(out.content.contains("Use offset=501"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_tool_byte_cut_note_points_at_unseen_lines() {
        // 600 wide lines ≈ 184KB: the 500-line window cuts at line 500 and
        // the 50KB byte cap cuts around line ~170 — the note must resume at
        // the first line the model has not actually seen, not at 501
        let dir = std::env::temp_dir().join(format!("llm-readcut-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let body: Vec<String> = (0..600).map(|_| "x".repeat(300)).collect();
        std::fs::write(dir.join("wide.txt"), body.join("\n")).unwrap();
        let out = read_execute(&dir, json!({"path": "wide.txt"}));
        assert!(!out.is_error);
        assert!(out.content.contains("150: "), "{}", out.content);
        assert!(!out.content.contains("250: "), "{}", out.content);
        assert!(!out.content.contains("Use offset=501"), "{}", out.content);
        let resume = out
            .content
            .split("Use offset=")
            .nth(1)
            .and_then(|rest| {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                digits.parse::<usize>().ok()
            })
            .expect("note present");
        assert!(resume > 150 && resume < 250, "resume={resume}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_tool_refuses_binary_with_hint() {
        let dir = std::env::temp_dir().join(format!("llm-readbin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("aln.bam"), b"BAM\x01data").unwrap();
        std::fs::write(dir.join("paper.pdf"), b"%PDF-1.4 fake").unwrap();
        std::fs::write(dir.join("blob.dat"), b"xx\0yy").unwrap();

        let out = read_execute(&dir, json!({"path": "aln.bam"}));
        assert!(out.is_error);
        assert!(out.content.contains("binary format"), "{}", out.content);
        assert!(out.content.contains("samtools"), "{}", out.content);
        assert!(out.content.contains("use bash"), "{}", out.content);

        let out = read_execute(&dir, json!({"path": "paper.pdf"}));
        assert!(out.is_error);
        assert!(out.content.contains("pdftotext"), "{}", out.content);

        // unknown binary extension gets the generic wording without a hint
        let out = read_execute(&dir, json!({"path": "blob.dat"}));
        assert!(out.is_error);
        assert!(out.content.contains("binary format"), "{}", out.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_tool_attaches_image_for_vision_models() {
        let dir = std::env::temp_dir().join(format!("llm-readimg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // a tiny PNG signature; the read tool routes by extension, so the
        // exact body is irrelevant to the test
        std::fs::write(dir.join("pic.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        let out = read_execute(&dir, json!({"path": "pic.png"}));
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("vision attachment"), "{}", out.content);
        assert_eq!(out.attachments.len(), 1);
        assert_eq!(out.attachments[0].mime_type, "image/png");
        assert!(!out.attachments[0].base64_data.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_tool_offset_past_end_and_empty_file() {
        let dir = std::env::temp_dir().join(format!("llm-readend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("small.txt"), b"a\nb\n").unwrap();
        std::fs::write(dir.join("empty.txt"), b"").unwrap();

        let out = read_execute(&dir, json!({"path": "small.txt", "offset": 9}));
        assert!(out.is_error);
        assert_eq!(
            out.content,
            "offset 9 is past the end of the file (2 lines)"
        );

        let out = read_execute(&dir, json!({"path": "empty.txt"}));
        assert!(!out.is_error);
        assert_eq!(out.content, "(empty file)");

        let out = read_execute(&dir, json!({"path": "missing.txt"}));
        assert!(out.is_error);
        assert!(out.content.starts_with("cannot read"), "{}", out.content);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
