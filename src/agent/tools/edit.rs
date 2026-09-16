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
        "Make targeted text edits. Each `oldText` must match exactly once in the original (a \
         whitespace-flexible pass rescues a miss); all edits match the original, not one another."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "then_run": {"type": "string", "description": THEN_RUN_DESCRIPTION},
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

/// Locate one edit's oldText in the original: an exact match first, then a
/// whitespace-flexible pass (the reference's fuzzy matching — per-line trailing
/// whitespace dropped, CRLF treated as LF, lookalike punctuation folded).
/// Unique matches only; the errors tell the model how to self-correct.
pub(super) fn locate_edit(
    original: &str,
    old: &str,
    path: &Path,
) -> Result<(usize, usize), String> {
    if let Some(i) = original.find(old) {
        if original[i + old.len()..].contains(old) {
            return Err(format!(
                "oldText matches more than once in {} (include surrounding lines to disambiguate):\n{old}",
                path.display()
            ));
        }
        return Ok((i, i + old.len()));
    }
    let spans = fuzzy_spans(original, old);
    match spans.len() {
        1 => Ok(spans[0]),
        0 => Err(format!(
            "oldText not found in {} (it must match the file exactly, including whitespace and newlines):\n{old}",
            path.display()
        )),
        n => Err(format!(
            "oldText matches {n} times in {} after whitespace-flexible matching (include surrounding lines to disambiguate):\n{old}",
            path.display()
        )),
    }
}

/// Fold the lookalike punctuation models routinely mistype onto ASCII.
pub(super) fn fuzzy_char(ch: char) -> char {
    match ch {
        '\u{2018}' | '\u{2019}' => '\'',
        '\u{201C}' | '\u{201D}' => '"',
        '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2007}' | '\u{202F}' | '\u{3000}' => ' ',
        other => other,
    }
}

/// A whitespace-flexible view of a text for edit matching: per-line trailing
/// whitespace dropped, `\r` dropped, lookalikes folded. Returns the folded
/// text plus, per byte of it, the original byte range that byte's char came
/// from — so a match maps back onto the original span exactly: a match
/// ending on a normal char stops at that char's original end, while one
/// ending on a line joiner consumes the original line break and any dropped
/// trailing whitespace before it.
pub(super) fn fuzzy_text(orig: &str) -> (String, Vec<(usize, usize)>) {
    let mut text = String::with_capacity(orig.len());
    let mut map: Vec<(usize, usize)> = Vec::with_capacity(orig.len() + 1);
    let mut off = 0usize;
    // original offset just past the previous line's kept content
    let mut prev_kept_end = 0usize;
    for (li, line) in orig.split('\n').enumerate() {
        let kept = &line[..line.trim_end().len()];
        if li > 0 {
            // the joiner covers the previous line's dropped tail plus '\n'
            text.push('\n');
            for _ in 0..'\n'.len_utf8() {
                map.push((prev_kept_end, off));
            }
        }
        for (i, ch) in kept.char_indices() {
            let c = fuzzy_char(ch);
            let s = off + i;
            text.push(c);
            for _ in 0..c.len_utf8() {
                map.push((s, s + ch.len_utf8()));
            }
        }
        prev_kept_end = off + kept.len();
        off += line.len() + 1; // past this line and its '\n'
    }
    (text, map)
}

/// All original-coordinate spans where `old` matches after normalization.
pub(super) fn fuzzy_spans(orig: &str, old: &str) -> Vec<(usize, usize)> {
    let (hay, map) = fuzzy_text(orig);
    let (needle, _) = fuzzy_text(old);
    let mut spans = Vec::new();
    if needle.is_empty() {
        return spans;
    }
    let mut from = 0usize;
    while let Some(pos) = hay[from..].find(&needle) {
        let s = from + pos;
        let e = s + needle.len();
        spans.push((map[s].0, map[e - 1].1));
        from = e;
    }
    spans
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
