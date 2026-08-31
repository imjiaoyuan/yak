//! Minimal YAML subset parser/emitter — enough for template files, zero deps.
//!
//! Supported: nested maps, string/int/bool/null scalars, quoted strings,
//! block scalars (`|` and `>`), `- item` string lists, comments.
//! Not supported: anchors, aliases, flow collections, tags, multi-doc.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Yaml {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Map(Vec<(String, Yaml)>),
    List(Vec<Yaml>),
}

impl Yaml {
    pub fn get(&self, key: &str) -> Option<&Yaml> {
        match self {
            Yaml::Map(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Yaml::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<BTreeMap<String, String>> {
        match self {
            Yaml::Map(pairs) => Some(
                pairs
                    .iter()
                    .map(|(k, v)| {
                        let val = match v {
                            Yaml::Str(s) => s.clone(),
                            Yaml::Int(i) => i.to_string(),
                            Yaml::Bool(b) => b.to_string(),
                            Yaml::Null => String::new(),
                            other => yaml_scalar_repr(other),
                        };
                        (k.clone(), val)
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    pub fn as_str_list(&self) -> Option<Vec<String>> {
        match self {
            Yaml::List(items) => Some(
                items
                    .iter()
                    .map(|i| match i {
                        Yaml::Str(s) => s.clone(),
                        other => yaml_scalar_repr(other),
                    })
                    .collect(),
            ),
            _ => None,
        }
    }
}

fn yaml_scalar_repr(y: &Yaml) -> String {
    match y {
        Yaml::Str(s) => s.clone(),
        Yaml::Int(i) => i.to_string(),
        Yaml::Bool(b) => b.to_string(),
        Yaml::Null => "null".into(),
        _ => String::new(),
    }
}

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

fn strip_comment(value: &str) -> String {
    // remove trailing comment outside quotes
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

fn parse_scalar(raw: &str) -> Yaml {
    let s = raw.trim();
    if s.is_empty() || s == "~" || s == "null" {
        return Yaml::Null;
    }
    if s == "true" || s == "True" {
        return Yaml::Bool(true);
    }
    if s == "false" || s == "False" {
        return Yaml::Bool(false);
    }
    if let Ok(i) = s.parse::<i64>() {
        return Yaml::Int(i);
    }
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        return Yaml::Str(s[1..s.len() - 1].to_string());
    }
    Yaml::Str(s.to_string())
}

/// Parse a YAML subset document.
/// Split `---\n` yaml frontmatter from the body that follows the closing
/// `\n---`: returns (frontmatter, rest-after-the-delimiter). Shared by the
/// markdown-carried definitions (skills, sub-agents, user commands).
pub fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    // tolerate CRLF files (Windows editors): both delimiter spellings
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let idx = rest.find("\n---")?;
    let after = rest[idx + 4..]
        .strip_prefix('\r')
        .unwrap_or(&rest[idx + 4..]);
    Some((&rest[..idx], after))
}

pub fn parse(text: &str) -> Result<Yaml, YamlError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut pos = 0;
    let value = parse_block(&lines, &mut pos, 0)?;
    Ok(value)
}

fn parse_block(lines: &[&str], pos: &mut usize, min_indent: usize) -> Result<Yaml, YamlError> {
    // skip blanks/comments
    while *pos < lines.len()
        && (lines[*pos].trim().is_empty() || lines[*pos].trim_start().starts_with('#'))
    {
        *pos += 1;
    }
    if *pos >= lines.len() || indent_of(lines[*pos]) < min_indent {
        return Ok(Yaml::Null);
    }
    let base = indent_of(lines[*pos]);
    if lines[*pos].trim_start().starts_with("- ") || lines[*pos].trim_end() == "-" {
        return parse_list(lines, pos, base);
    }
    parse_map(lines, pos, base)
}

fn parse_list(lines: &[&str], pos: &mut usize, base: usize) -> Result<Yaml, YamlError> {
    let mut items = Vec::new();
    while *pos < lines.len() {
        let line = lines[*pos];
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            *pos += 1;
            continue;
        }
        if indent_of(line) != base {
            break;
        }
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("- ") {
            items.push(parse_scalar(&strip_comment(rest)));
            *pos += 1;
        } else if trimmed == "-" {
            *pos += 1;
            let nested = parse_block(lines, pos, base + 1)?;
            items.push(nested);
        } else {
            break;
        }
    }
    Ok(Yaml::List(items))
}

fn parse_map(lines: &[&str], pos: &mut usize, base: usize) -> Result<Yaml, YamlError> {
    let mut pairs: Vec<(String, Yaml)> = Vec::new();
    while *pos < lines.len() {
        let line = lines[*pos];
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            *pos += 1;
            continue;
        }
        if indent_of(line) != base {
            break;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("- ") {
            break;
        }
        let Some(colon) = find_colon(trimmed) else {
            return Err(YamlError(format!("expected 'key:' at: {trimmed}")));
        };
        let key_raw = trimmed[..colon].trim();
        let key = key_raw.trim_matches('"').trim_matches('\'').to_string();
        let value_raw = strip_comment(&trimmed[colon + 1..]);
        *pos += 1;
        let value_raw = value_raw.trim();
        let value =
            if value_raw == "|" || value_raw == "|-" || value_raw == ">" || value_raw == ">-" {
                parse_block_scalar(lines, pos, base, value_raw.starts_with('>'))
            } else if value_raw.is_empty() {
                if text_block_ahead(lines, *pos, base + 1) {
                    parse_plain_block(lines, pos, base + 1)
                } else {
                    parse_block(lines, pos, base + 1)?
                }
            } else {
                parse_scalar(value_raw)
            };
        pairs.push((key, value));
    }
    Ok(Yaml::Map(pairs))
}

/// True when the upcoming indented lines are a plain-text block (no
/// `key: value` maps and no `- ` lists) rather than nested structure.
fn text_block_ahead(lines: &[&str], pos: usize, min_indent: usize) -> bool {
    let mut i = pos;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            i += 1;
            continue;
        }
        if indent_of(line) < min_indent {
            return false;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("- ") || trimmed == "-" {
            return false;
        }
        if find_colon(trimmed).is_some() {
            return false;
        }
        return true;
    }
    false
}

/// Fold indented plain lines into one scalar: single newlines collapse to
/// a space, blank lines stay as newlines (YAML plain multiline scalars).
fn parse_plain_block(lines: &[&str], pos: &mut usize, min_indent: usize) -> Yaml {
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
        if !out.is_empty() && !out.ends_with('\n') && !out.ends_with(' ') {
            out.push(' ');
        }
        out.push_str(line[ind..].trim());
        *pos += 1;
    }
    Yaml::Str(out.trim().to_string())
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

fn parse_block_scalar(lines: &[&str], pos: &mut usize, base: usize, folded: bool) -> Yaml {
    let mut content_lines: Vec<String> = Vec::new();
    let mut block_indent: Option<usize> = None;
    while *pos < lines.len() {
        let line = lines[*pos];
        if line.trim().is_empty() {
            // blank line: part of the scalar if more content follows
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
    // trim trailing blank lines
    while content_lines.last().is_some_and(|l| l.is_empty()) {
        content_lines.pop();
    }
    let text = if folded {
        // fold single newlines into spaces, keep blank-line breaks
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
    };
    Yaml::Str(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_template_file() {
        let text = "\
# comment
model: deepseek/deepseek-chat
prompt: |
  You are a code reviewer.
  Review this: $input
system: \"be strict\"
extract: true
defaults:
  style: terse
options:
  temperature: 0.2
items:
  - a.md
  - b.md
";
        let y = parse(text).unwrap();
        assert_eq!(
            y.get("model").unwrap().as_str(),
            Some("deepseek/deepseek-chat")
        );
        assert_eq!(
            y.get("prompt").unwrap().as_str(),
            Some("You are a code reviewer.\nReview this: $input")
        );
        assert_eq!(y.get("system").unwrap().as_str(), Some("be strict"));
        assert_eq!(*y.get("extract").unwrap(), Yaml::Bool(true));
        assert_eq!(
            y.get("defaults").unwrap().as_map().unwrap().get("style"),
            Some(&"terse".to_string())
        );
        assert_eq!(
            y.get("items").unwrap().as_str_list().unwrap(),
            vec!["a.md", "b.md"]
        );
    }

    #[test]
    fn parses_folded_scalar() {
        let text = "system: >\n  one two\n  three\n";
        let y = parse(text).unwrap();
        assert_eq!(y.get("system").unwrap().as_str(), Some("one two three"));
    }

    #[test]
    fn parses_indented_multiline_plain_scalar() {
        let text = "name: x\ndescription:\n  first line\n  second line\nother: y\n";
        let y = parse(text).unwrap();
        assert_eq!(y.get("name").unwrap().as_str(), Some("x"));
        assert_eq!(
            y.get("description").unwrap().as_str(),
            Some("first line second line")
        );
        assert_eq!(y.get("other").unwrap().as_str(), Some("y"));
    }

    #[test]
    fn nested_map_after_multiline_scalar_still_parses() {
        let text = "description:\n  text line\nmetadata:\n  author: vercel\n  version: '1.0.0'\n";
        let y = parse(text).unwrap();
        assert_eq!(y.get("description").unwrap().as_str(), Some("text line"));
        assert_eq!(
            y.get("metadata").unwrap().as_map().unwrap().get("author"),
            Some(&"vercel".to_string())
        );
    }

    #[test]
    fn parses_int_and_trailing_comment() {
        let y = parse("count: 5  # five\n").unwrap();
        assert_eq!(*y.get("count").unwrap(), Yaml::Int(5));
    }

    #[test]
    fn frontmatter_tolerates_crlf_files() {
        let text = "---\r\nmodel: m1\r\n---\r\nbody here";
        let (fm, after) = split_frontmatter(text).expect("crlf frontmatter splits");
        assert!(fm.contains("model"));
        let y = parse(fm).unwrap();
        assert_eq!(y.get("model").and_then(|v| v.as_str()), Some("m1"));
        assert_eq!(after.trim_start_matches('\n'), "body here");
    }

    #[test]
    fn multibyte_keys_do_not_panic() {
        let y = parse("描述: 你好\nother: x\n").unwrap();
        assert_eq!(y.get("描述").unwrap().as_str(), Some("你好"));
        assert_eq!(y.get("other").unwrap().as_str(), Some("x"));
    }
}
