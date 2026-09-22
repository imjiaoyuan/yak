//! Minimal flat frontmatter parser — zero deps, no emitter (nothing writes
//! YAML).
//!
//! One consumer shape: the frontmatter of a `SKILL.md` or a commands-dir
//! prompt, reduced to flat `key: value` string pairs. `description`/`system`
//! may be multi-line (a `|`/`>` block scalar, or indented continuation
//! lines). Nested maps and lists are skipped, not modelled: nothing in-tree
//! reads them.

use std::collections::BTreeMap;

#[derive(Debug)]
pub struct YamlError(pub String);

impl std::fmt::Display for YamlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "YAML error: {}", self.0)
    }
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Split `---\n` yaml frontmatter from the body that follows the closing
/// `\n---`: returns (frontmatter, rest-after-the-delimiter). Tolerates CRLF.
/// Shared by the markdown-carried definitions (skills, user prompts/commands).
pub fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let idx = rest.find("\n---")?;
    let after = rest[idx + 4..]
        .strip_prefix('\r')
        .unwrap_or(&rest[idx + 4..]);
    Some((&rest[..idx], after))
}

/// Parse a flat frontmatter document into string pairs. Values are strings
/// throughout: `true`/`false`, integers and `null`/`~` become their string
/// forms. A top-level line without a `key:` is an error — half-parsed
/// frontmatter must not silently drop a key — while a nested map or list
/// under a key is skipped, since no consumer reads one.
pub fn parse(text: &str) -> Result<BTreeMap<String, String>, YamlError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut map = BTreeMap::new();
    let mut pos = 0;
    while pos < lines.len() {
        let line = lines[pos];
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            pos += 1;
            continue;
        }
        if indent_of(line) > 0 {
            return Err(YamlError(format!(
                "unexpected indented line at top level: {trimmed}"
            )));
        }
        let Some(colon) = find_colon(trimmed) else {
            return Err(YamlError(format!("expected 'key: value' at: {trimmed}")));
        };
        let key = trimmed[..colon].trim().trim_matches('"').trim_matches('\'');
        let raw = trimmed[colon + 1..].trim();
        pos += 1;
        let value = if raw == "|" || raw == "|-" || raw == ">" || raw == ">-" {
            parse_block_scalar(&lines, &mut pos, indent_of(line), raw.starts_with('>'))
        } else if raw.is_empty() {
            // a value on following indented lines, or nested structure the
            // flat model has no use for
            if next_is_plain(&lines, pos, indent_of(line) + 1) {
                parse_plain_block(&lines, &mut pos, indent_of(line) + 1)
            } else {
                skip_block(&lines, &mut pos, indent_of(line));
                String::new()
            }
        } else {
            // a non-empty plain scalar may continue on indented lines
            // (`description: a\n  b`): fold them in so the value is not lost
            let mut s = strip_comment(raw).to_string();
            if next_is_plain(&lines, pos, indent_of(line) + 1) {
                let more = parse_plain_block(&lines, &mut pos, indent_of(line) + 1);
                if !more.is_empty() {
                    s.push(' ');
                    s.push_str(&more);
                }
            }
            scalar_string(&s)
        };
        map.insert(key.to_string(), value);
    }
    Ok(map)
}

/// The next content line (skipping blanks and comments), as (indent, text).
fn peek_content<'a>(lines: &[&'a str], pos: usize) -> Option<(usize, &'a str)> {
    let mut i = pos;
    while i < lines.len() {
        let t = lines[i].trim();
        if t.is_empty() || t.starts_with('#') {
            i += 1;
            continue;
        }
        return Some((indent_of(lines[i]), lines[i].trim_start()));
    }
    None
}

/// True when the following indented lines are a plain-text continuation
/// (no `key: value` and no `- ` list) rather than nested structure.
fn next_is_plain(lines: &[&str], pos: usize, min_indent: usize) -> bool {
    match peek_content(lines, pos) {
        Some((ind, text)) if ind >= min_indent => {
            !text.starts_with("- ") && text != "-" && find_colon(text).is_none()
        }
        _ => false,
    }
}

/// Fold indented plain lines into one scalar: single newlines collapse to a
/// space, blank lines stay as newlines (YAML plain multiline scalars).
fn parse_plain_block(lines: &[&str], pos: &mut usize, min_indent: usize) -> String {
    let mut out = String::new();
    while *pos < lines.len() {
        let line = lines[*pos];
        if line.trim().is_empty() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            *pos += 1;
            continue;
        }
        let ind = indent_of(line);
        if ind < min_indent {
            break;
        }
        let text = line.trim_start();
        if text.starts_with("- ") || text == "-" || find_colon(text).is_some() {
            break;
        }
        if !out.is_empty() && !out.ends_with('\n') && !out.ends_with(' ') {
            out.push(' ');
        }
        out.push_str(text);
        *pos += 1;
    }
    out.trim().to_string()
}

/// Skip an unmodelled nested block: every line indented past `base`, plus
/// the blank lines between them.
fn skip_block(lines: &[&str], pos: &mut usize, base: usize) {
    while *pos < lines.len() {
        let line = lines[*pos];
        if line.trim().is_empty() {
            *pos += 1;
            continue;
        }
        if indent_of(line) <= base {
            break;
        }
        *pos += 1;
    }
}

fn find_colon(s: &str) -> Option<usize> {
    // first ':' that is followed by space or end, outside quotes
    let bytes = s.as_bytes();
    let mut in_q: Option<char> = None;
    for (i, c) in s.char_indices() {
        if let Some(q) = in_q {
            if c == q {
                in_q = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => in_q = Some(c),
            ':' if i + c.len_utf8() >= s.len() || bytes[i + c.len_utf8()] == b' ' => {
                return Some(i);
            }
            _ => {}
        }
    }
    None
}

/// Strip a trailing `# comment` outside quotes.
fn strip_comment(value: &str) -> String {
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_space = true;
    for (i, c) in value.char_indices() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if prev_space && !in_single && !in_double => {
                return value[..i].trim_end().to_string();
            }
            _ => {}
        }
        prev_space = c == ' ' || c == '\t';
    }
    value.trim_end().to_string()
}

/// A scalar as its string form: quotes stripped, booleans/null normalized,
/// everything else verbatim (integers already read as their text).
fn scalar_string(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() || t == "~" || t == "null" {
        String::new()
    } else if t == "true" || t == "True" {
        "true".to_string()
    } else if t == "false" || t == "False" {
        "false".to_string()
    } else if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

/// A `|`/`>` block scalar: literal keeps newlines, folded collapses single
/// newlines to spaces and keeps blank-line breaks. Trailing blank lines are
/// trimmed.
fn parse_block_scalar(lines: &[&str], pos: &mut usize, base: usize, folded: bool) -> String {
    let mut content_lines: Vec<String> = Vec::new();
    let mut block_indent: Option<usize> = None;
    while *pos < lines.len() {
        let line = lines[*pos];
        if line.trim().is_empty() {
            content_lines.push(String::new());
            *pos += 1;
            continue;
        }
        let ind = indent_of(line);
        if ind <= base {
            break;
        }
        if block_indent.is_none() {
            block_indent = Some(ind);
        }
        let bi = block_indent.unwrap();
        if ind < bi {
            break;
        }
        content_lines.push(line[bi..].to_string());
        *pos += 1;
    }
    while content_lines.last().is_some_and(|l| l.is_empty()) {
        content_lines.pop();
    }
    if folded {
        let mut out = String::new();
        let mut pending_break = false;
        for l in &content_lines {
            if l.is_empty() {
                out.push('\n');
                pending_break = false;
            } else {
                if pending_break {
                    out.push(' ');
                }
                out.push_str(l);
                pending_break = true;
            }
        }
        out
    } else {
        content_lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_pairs_and_block_scalars() {
        let text = "# comment\nname: pdf\ndescription: |\n  Extract tables\n  from PDFs\nsystem: \"be strict\"\ndisable_model_invocation: true\ncount: 5\n";
        let map = parse(text).unwrap();
        assert_eq!(map.get("name").map(String::as_str), Some("pdf"));
        assert_eq!(
            map.get("description").map(String::as_str),
            Some("Extract tables\nfrom PDFs")
        );
        assert_eq!(map.get("system").map(String::as_str), Some("be strict"));
        assert_eq!(
            map.get("disable_model_invocation").map(String::as_str),
            Some("true")
        );
        assert_eq!(map.get("count").map(String::as_str), Some("5"));
    }

    #[test]
    fn parses_folded_scalar() {
        let map = parse("system: >\n  one two\n  three\n").unwrap();
        assert_eq!(map.get("system").map(String::as_str), Some("one two three"));
    }

    #[test]
    fn indented_plain_continuation_folds_onto_the_value() {
        let map = parse("name: x\ndescription:\n  first line\n  second line\nother: y\n").unwrap();
        assert_eq!(
            map.get("description").map(String::as_str),
            Some("first line second line")
        );
        assert_eq!(map.get("other").map(String::as_str), Some("y"));
        // a non-empty scalar keeps its indented continuation lines
        let map = parse("description: Extract tables\n  from scanned PDFs\n  and CSV\n").unwrap();
        assert_eq!(
            map.get("description").map(String::as_str),
            Some("Extract tables from scanned PDFs and CSV")
        );
    }

    #[test]
    fn nested_maps_are_skipped_without_losing_flat_keys() {
        let map = parse("name: x\nmetadata:\n  author: vercel\n  version: '1.0.0'\n").unwrap();
        assert_eq!(map.get("name").map(String::as_str), Some("x"));
        assert_eq!(
            map.get("metadata").map(String::as_str),
            Some(""),
            "a nested map flattens to an empty value"
        );
    }

    #[test]
    fn crlf_frontmatter_and_multibyte_keys() {
        let (fm, after) = split_frontmatter("---\r\nname: m1\r\n---\r\nbody").unwrap();
        assert!(fm.contains("name"));
        assert_eq!(after.trim_start_matches('\n'), "body");
        let map = parse("描述: 你好\nother: x\n").unwrap();
        assert_eq!(map.get("描述").map(String::as_str), Some("你好"));
        assert_eq!(map.get("other").map(String::as_str), Some("x"));
    }

    #[test]
    fn a_line_without_a_key_errors_instead_of_vanishing() {
        assert!(parse("a: 1\njust text without a colon\n").is_err());
        assert!(parse("a: 1\n<garbage>\n").is_err());
    }

    #[test]
    fn a_trailing_comment_is_stripped_outside_quotes() {
        let map = parse("count: 5  # five\n").unwrap();
        assert_eq!(map.get("count").map(String::as_str), Some("5"));
    }
}
