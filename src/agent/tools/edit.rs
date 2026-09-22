use super::write::write_atomic;
use super::*;

pub(super) struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn tier(&self) -> Tier {
        Tier::Write
    }
    fn description(&self) -> &str {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a \
         unique, non-overlapping region of the original file."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to edit (relative or absolute)"},
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements, matched against the original file, not incrementally.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {"type": "string", "description": "Exact text for one targeted replacement; must be unique in the file."},
                            "newText": {"type": "string", "description": "Replacement text for this targeted edit."}
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
        let original = std::fs::read_to_string(&path).ok()?;
        let edits = args["edits"].as_array()?;
        // locate each oldText like execute() will; an unlocatable edit
        // simply yields no preview (execute reports the error)
        let mut spans: Vec<(usize, usize, &str)> = Vec::new();
        for e in edits {
            let (old, new) = (e["oldText"].as_str()?, e["newText"].as_str()?);
            let (start, end) = locate_edit(&original, old, &path).ok()?;
            spans.push((start, end, new));
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
            let (start, end) = match locate_edit(&original, old, &path) {
                Ok(span) => span,
                Err(e) => return ToolOutput::err(e),
            };
            spans.push((start, end, new));
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
        match write_atomic(&path, text.as_bytes()) {
            Ok(()) => ToolOutput::ok(format!(
                "applied {} edits to {}",
                spans.len(),
                path.display()
            )),
            Err(e) => ToolOutput::err(format!("write failed: {e}")),
        }
    }
}

/// Locate one edit's oldText in the original: exact, unique match only (the reference's
/// rule). The errors tell the model how to self-correct.
pub(super) fn locate_edit(
    original: &str,
    old: &str,
    path: &Path,
) -> Result<(usize, usize), String> {
    match original.find(old) {
        Some(i) => {
            if original[i + old.len()..].contains(old) {
                return Err(format!(
                    "oldText matches more than once in {} (include surrounding lines to disambiguate):\n{old}",
                    path.display()
                ));
            }
            Ok((i, i + old.len()))
        }
        None => Err(format!(
            "oldText not found in {} (it must match the file exactly, including whitespace and newlines):\n{old}",
            path.display()
        )),
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
