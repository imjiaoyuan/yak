//! Exact-text editing with the reference's recovery behavior (`the reference edit module`): edits
//! match against the LF-normalized file, a failed exact match falls back to
//! fuzzy matching (unicode quotes/dashes/spaces folded to ASCII, fullwidth
//! forms folded, trailing whitespace stripped), CRLF line endings and a
//! leading BOM survive the write, and a fuzzy hit rewrites only the lines it
//! touched — every other line keeps its original bytes. Unmatched, ambiguous
//! or overlapping edits fail with the reference's own error texts so the model can
//! self-correct.

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
    fn prepare_arguments(&self, args: &Value) -> Value {
        prepare_edit_args(args)
    }
    fn description(&self) -> &str {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a \
         unique, non-overlapping region of the original file. If two changes affect the same \
         block or nearby lines, merge them into one edit instead of emitting overlapping edits. \
         Do not include large unchanged regions just to connect distant changes."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to edit (relative or absolute)"},
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {"type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."},
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
        let args = prepare_edit_args(args);
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
        let raw = std::fs::read_to_string(&path).ok()?;
        let args = prepare_edit_args(args);
        let pairs = edit_pairs(&args)?;
        // an unlocatable edit simply yields no preview (execute reports it)
        let plan = plan_edits(&raw, &pairs, &path).ok()?;
        let spans: Vec<(usize, usize, &str)> = plan
            .spans
            .iter()
            .map(|s| (s.start, s.end, s.new_text.as_str()))
            .collect();
        Some(change_hunks(&plan.match_base, &spans, 2, DIFF_MAX_LINES))
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let path = resolve_path(cwd, args["path"].as_str().unwrap_or(""));
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(format!("cannot read {} ({e})", path.display())),
        };
        let args = prepare_edit_args(args);
        let Some(pairs) = edit_pairs(&args) else {
            return ToolOutput::err("edits must be an array");
        };
        let plan = match plan_edits(&raw, &pairs, &path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(e),
        };
        let merged = match apply_edits(&plan) {
            Ok(m) => m,
            Err(e) => return ToolOutput::err(e),
        };
        if merged == plan.normalized {
            return ToolOutput::err(no_change_error(&path, plan.spans.len()));
        }
        // write the file back the way it reads: its own dominant line ending
        // and its BOM (the reference restores both after working in LF space)
        let final_text = format!("{}{}", plan.bom, restore_line_endings(&merged, plan.ending));
        match write_atomic(&path, final_text.as_bytes()) {
            Ok(()) => ToolOutput::ok(format!(
                "Successfully replaced {} block(s) in {}.",
                plan.spans.len(),
                path.display()
            )),
            Err(e) => ToolOutput::err(format!("write failed: {e}")),
        }
    }
}

/// One edit located in the match space: `start..end` names the matched
/// `oldText`, `new_text` replaces it, `edit_index` keeps the caller's
/// ordering so error messages can name `edits[i]` the way the model sent it.
pub(super) struct LocatedEdit {
    edit_index: usize,
    start: usize,
    end: usize,
    new_text: String,
}

/// Everything `execute` and `diff` need to apply a batch of edits. Built by
/// [`plan_edits`]; `spans` index into `match_base`.
pub(super) struct EditPlan<'a> {
    /// whether the raw file began with a BOM (re-attached on write)
    bom: &'a str,
    /// the file's dominant line ending, detected before normalization
    ending: &'static str,
    /// LF-normalized original (BOM stripped): the no-change baseline and the
    /// untouched-line source for a fuzzy write-back
    normalized: String,
    /// the text the spans index into: `normalized`, or its fuzzy-normalized
    /// form when any edit needed the fallback
    match_base: String,
    /// located edits, sorted by position
    spans: Vec<LocatedEdit>,
    /// whether any edit matched only after normalization
    used_fuzzy: bool,
}

/// Extract the edit pairs from prepared arguments. `None` when `edits` is
/// missing or not an array of objects (the caller reports that shape).
fn edit_pairs(args: &Value) -> Option<Vec<(&str, &str)>> {
    let edits = args["edits"].as_array()?;
    edits
        .iter()
        .map(|e| Some((e["oldText"].as_str()?, e["newText"].as_str()?)))
        .collect()
}

/// the reference's `prepareEditArguments`: models sometimes stringify `edits` or use the
/// legacy flat single-edit shape; salvage both before validation instead of
/// spending a round on a type error.
fn prepare_edit_args(args: &Value) -> Value {
    let mut args = args.clone();
    match args.get("edits") {
        // a stringified array (or single edit) is parsed
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(items)) => args["edits"] = Value::Array(items),
            Ok(single @ Value::Object(_)) if single.get("oldText").is_some() => {
                args["edits"] = json!([single]);
            }
            _ => {}
        },
        // a single edit object where the array was expected
        Some(single @ Value::Object(_)) if single.get("oldText").is_some() => {
            args["edits"] = json!([single]);
        }
        _ => {}
    }
    // legacy flat {oldText, newText} rides alongside or instead of `edits`
    if let (Some(Value::String(o)), Some(Value::String(n))) =
        (args.get("oldText"), args.get("newText"))
    {
        let (o, n) = (o.clone(), n.clone());
        let mut list = match args.get("edits") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        list.push(json!({"oldText": o, "newText": n}));
        if let Some(obj) = args.as_object_mut() {
            obj.insert("edits".to_string(), Value::Array(list));
            obj.remove("oldText");
            obj.remove("newText");
        }
    }
    args
}

/// Plan a batch of edits against `raw` (the file as read): strip the BOM,
/// detect the line ending, normalize to LF, locate every edit exactly or
/// fuzzily, and refuse empty, missing, ambiguous or overlapping matches.
pub(super) fn plan_edits<'a>(
    raw: &'a str,
    edits: &[(&str, &str)],
    path: &Path,
) -> Result<EditPlan<'a>, String> {
    if edits.is_empty() {
        return Err(
            "Edit tool input is invalid. edits must contain at least one replacement.".into(),
        );
    }
    let (bom, content) = match raw.strip_prefix('\u{feff}') {
        Some(rest) => (&raw[..raw.len() - rest.len()], rest),
        None => ("", raw),
    };
    let ending = detect_line_ending(content);
    let normalized = normalize_to_lf(content);
    // the reference normalizes each oldText/newText to LF too: a model quoting a CRLF
    // file back still matches
    let normalized_edits: Vec<(String, String)> = edits
        .iter()
        .enumerate()
        .map(|(i, (old, new))| {
            let old = normalize_to_lf(old);
            if old.is_empty() {
                return Err(empty_error(path, i, edits.len()));
            }
            Ok((old, normalize_to_lf(new)))
        })
        .collect::<Result<_, _>>()?;

    // first pass: does any edit need the fuzzy fallback? Then ALL matches run
    // in fuzzy-normalized space (the reference's rule) and only touched lines are
    // written back normalized.
    let fuzzy_base = normalize_for_fuzzy_match(&normalized);
    let used_fuzzy = normalized_edits.iter().any(|(old, _)| {
        fuzzy_find(&normalized, old, &fuzzy_base).is_some_and(|(_, _, fuzzy)| fuzzy)
    });
    let match_base = if used_fuzzy {
        fuzzy_base.clone()
    } else {
        normalized.clone()
    };

    // second pass: locate everything in the match space
    let mut spans: Vec<LocatedEdit> = Vec::with_capacity(normalized_edits.len());
    for (i, (old, new)) in normalized_edits.iter().enumerate() {
        let Some((start, len, _)) = fuzzy_find(&match_base, old, &fuzzy_base) else {
            return Err(not_found_error(path, i, normalized_edits.len()));
        };
        // ambiguity is judged in fuzzy space (the reference's countOccurrences): two
        // occurrences differing only in trailing whitespace are already two
        // regions the model cannot tell apart
        let occurrences = count_occurrences(&fuzzy_base, &normalize_for_fuzzy_match(old));
        if occurrences > 1 {
            return Err(duplicate_error(
                path,
                i,
                normalized_edits.len(),
                occurrences,
            ));
        }
        spans.push(LocatedEdit {
            edit_index: i,
            start,
            end: start + len,
            new_text: new.clone(),
        });
    }
    spans.sort_by_key(|s| s.start);
    for pair in spans.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.end > b.start {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {}. Merge them into one edit or target \
                 disjoint regions.",
                a.edit_index,
                b.edit_index,
                path.display()
            ));
        }
    }
    Ok(EditPlan {
        bom,
        ending,
        normalized,
        match_base,
        spans,
        used_fuzzy,
    })
}

/// Apply a planned batch: exact matches replace back-to-front on the match
/// text; a fuzzy batch overlays its touched lines onto the original so
/// unchanged lines keep their bytes.
pub(super) fn apply_edits(plan: &EditPlan<'_>) -> Result<String, String> {
    if !plan.used_fuzzy {
        let mut out = plan.match_base.clone();
        for s in plan.spans.iter().rev() {
            out.replace_range(s.start..s.end, &s.new_text);
        }
        Ok(out)
    } else {
        preserve_unchanged_lines(&plan.normalized, &plan.match_base, &plan.spans)
    }
}

/// the reference's `fuzzyFindText`: exact match first, then both sides fuzzy-normalized.
/// Returns `(start, len, fuzzy)`; `len` is always the matched needle's own
/// length in the space the caller searches.
fn fuzzy_find(content: &str, old: &str, fuzzy_content: &str) -> Option<(usize, usize, bool)> {
    if let Some(i) = content.find(old) {
        return Some((i, old.len(), false));
    }
    let fuzzy_old = normalize_for_fuzzy_match(old);
    let i = fuzzy_content.find(fuzzy_old.as_str())?;
    Some((i, fuzzy_old.len(), true))
}

/// the reference's `countOccurrences`: both sides normalized, non-overlapping count.
fn count_occurrences(fuzzy_content: &str, fuzzy_old: &str) -> usize {
    if fuzzy_old.is_empty() {
        // an all-whitespace oldText normalizes to empty; the split-count then
        // finds a boundary at every char. The empty-oldText case is refused
        // before this runs, so this only shapes an error message.
        return fuzzy_content.chars().count() + 1;
    }
    fuzzy_content.split(fuzzy_old).count() - 1
}

/// Normalize text for fuzzy matching, the reference's `normalizeForFuzzyMatch`: strip
/// trailing whitespace per line, fold smart quotes, dashes and special spaces
/// to ASCII. Where the reference runs full NFKC this folds the compatibility set that
/// matters in source files — the fullwidth ASCII forms (U+FF01–U+FF5E, the
/// CJK-input-method spelling of `！Ａｂｃ：`), which NFKC maps by a fixed
/// offset — and leaves rarer compatibility chars (ligatures, circled digits)
/// exact-only: the fallback only fires when the exact match failed, so an
/// uncovered case behaves as it did before, with a clear error.
fn normalize_for_fuzzy_match(text: &str) -> String {
    let mapped: String = text
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            c if ('\u{FF01}'..='\u{FF5E}').contains(&c) => {
                char::from_u32(c as u32 - 0xFEE0).unwrap_or(c)
            }
            c => c,
        })
        .collect();
    // split/join, not lines(): a trailing newline must survive so offsets
    // stay comparable with the original
    mapped
        .split('\n')
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `\r\n`/`\r` → `\n` (the reference's `normalizeToLF`).
fn normalize_to_lf(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// the reference's `detectLineEnding`: CRLF wins when the first CRLF precedes the first
/// bare LF (a CRLF implies an LF, so "LF only" is every other case).
fn detect_line_ending(content: &str) -> &'static str {
    match (content.find("\r\n"), content.find('\n')) {
        (Some(c), Some(l)) if c < l => "\r\n",
        _ => "\n",
    }
}

/// the reference's `restoreLineEndings`: the whole file takes its dominant ending back.
fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

/// Lines with their trailing `\n` kept (the last may lack one) — the reference's
/// `splitLinesWithEndings`.
fn split_lines_with_endings(content: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' {
            lines.push(&content[start..=i]);
            start = i + 1;
        }
    }
    if start < content.len() {
        lines.push(&content[start..]);
    }
    lines
}

/// Byte span of every line, trailing `\n` included.
fn line_spans(content: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut off = 0usize;
    for line in split_lines_with_endings(content) {
        spans.push((off, off + line.len()));
        off += line.len();
    }
    spans
}

/// The line range one replacement actually touches (the reference's
/// `getReplacementLineRange`): from the line holding the match start through
/// the line holding its end.
fn replacement_line_range(
    spans: &[(usize, usize)],
    start: usize,
    len: usize,
) -> Result<(usize, usize), String> {
    let start_line = spans
        .iter()
        .position(|&(s, e)| start >= s && start < e)
        .ok_or("Replacement range is outside the base content.")?;
    let mut end_line = start_line;
    while end_line < spans.len() && spans[end_line].1 < start + len {
        end_line += 1;
    }
    if end_line >= spans.len() {
        return Err("Replacement range is outside the base content.".into());
    }
    Ok((start_line, end_line + 1))
}

/// the reference's `applyReplacementsPreservingUnchangedLines`: replacements were matched
/// in fuzzy space, but the file only takes the lines they touch from that
/// space — every other line is copied back verbatim, so a fuzzy hit cannot
/// silently re-whitespace the whole file. `original` and `base` (`base` =
/// `fuzzy(original)`) always split into the same lines: the normalization is
/// a one-to-one char map plus per-line trims.
fn preserve_unchanged_lines(
    original: &str,
    base: &str,
    spans: &[LocatedEdit],
) -> Result<String, String> {
    let original_lines = split_lines_with_endings(original);
    let base_spans = line_spans(base);
    if original_lines.len() != base_spans.len() {
        // unreachable by the invariant above; writing the fully normalized
        // text as a fallback would rewrite line endings file-wide, so refuse
        return Err(
            "Cannot preserve unchanged lines: the base content has a different line count.".into(),
        );
    }
    // group replacements into touching line ranges
    struct Group {
        start: usize,
        end: usize,
        spans: Vec<usize>,
    }
    let mut groups: Vec<Group> = Vec::new();
    for (i, s) in spans.iter().enumerate() {
        let (start_line, end_line) = replacement_line_range(&base_spans, s.start, s.end - s.start)?;
        match groups.last_mut() {
            Some(g) if start_line < g.end => {
                g.end = g.end.max(end_line);
                g.spans.push(i);
            }
            _ => groups.push(Group {
                start: start_line,
                end: end_line,
                spans: vec![i],
            }),
        }
    }
    let mut out = String::with_capacity(base.len());
    let mut cursor = 0usize;
    for g in groups {
        out.push_str(&original_lines[cursor..g.start].concat());
        let group_start = base_spans[g.start].0;
        let group_end = base_spans[g.end - 1].1;
        let mut chunk = base[group_start..group_end].to_string();
        for i in g.spans.iter().rev() {
            let s = &spans[*i];
            chunk.replace_range(s.start - group_start..s.end - group_start, &s.new_text);
        }
        out.push_str(&chunk);
        cursor = g.end;
    }
    out.push_str(&original_lines[cursor..].concat());
    Ok(out)
}

fn not_found_error(path: &Path, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "Could not find the exact text in {}. The old text must match exactly including all \
             whitespace and newlines.",
            path.display()
        )
    } else {
        format!(
            "Could not find edits[{edit_index}] in {}. The oldText must match exactly including \
             all whitespace and newlines.",
            path.display()
        )
    }
}

fn duplicate_error(
    path: &Path,
    edit_index: usize,
    total_edits: usize,
    occurrences: usize,
) -> String {
    if total_edits == 1 {
        format!(
            "Found {occurrences} occurrences of the text in {}. The text must be unique. Please \
             provide more context to make it unique.",
            path.display()
        )
    } else {
        format!(
            "Found {occurrences} occurrences of edits[{edit_index}] in {}. Each oldText must be \
             unique. Please provide more context to make it unique.",
            path.display()
        )
    }
}

fn empty_error(path: &Path, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!("oldText must not be empty in {}.", path.display())
    } else {
        format!(
            "edits[{edit_index}].oldText must not be empty in {}.",
            path.display()
        )
    }
}

fn no_change_error(path: &Path, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "No changes made to {}. The replacement produced identical content. This might \
             indicate an issue with special characters or the text not existing as expected.",
            path.display()
        )
    } else {
        format!(
            "No changes made to {}. The replacements produced identical content.",
            path.display()
        )
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
