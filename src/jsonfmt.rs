//! Reference-compatible JSON serialization. The reference's on-disk bytes
//! and hash inputs depend on exact serializer behavior, so the variants live
//! here instead of relying on serde_json's defaults:
//!
//! - `canonical_json` — sorted keys, compact separators, non-ascii raw
//!   (the message-hashing contract)
//! - `schema_compact` — compact separators, ascii-escaped, insertion order
//!   (make_schema_id)
//! - `dumps` — spaced ", " / ": " separators, ascii-escaped
//! - `dumps_indent` — newline-indented, ": " after keys

use serde_json::Value;

#[derive(Clone, Copy, PartialEq)]
enum Style {
    /// separators=(",", ":"), no spaces
    Tight,
    /// spaced separators=(", ", ": ")
    Spaced,
    /// indent=n — newline + indentation between items, ": " after keys
    Indent(usize),
}

/// Canonical form used for hashing: keys sorted recursively, no whitespace,
/// non-ASCII left as-is (ensure_ascii=False). Sorting happens while
/// rendering — no deep copy of the value tree.
pub fn canonical_json(v: &Value) -> String {
    render(v, Style::Tight, false, true)
}

/// Compact form for schema ids: insertion order, no whitespace, ASCII-escaped.
pub fn schema_compact(v: &Value) -> String {
    render(v, Style::Tight, true, false)
}

/// json.dumps(v) with default settings.
pub fn dumps(v: &Value) -> String {
    render(v, Style::Spaced, true, false)
}

/// json.dumps(v, indent=n).
pub fn dumps_indent(v: &Value, indent: usize) -> String {
    render(v, Style::Indent(indent), true, false)
}

fn render(v: &Value, style: Style, ensure_ascii: bool, sort_keys: bool) -> String {
    let mut out = String::new();
    write_value(v, &mut out, style, ensure_ascii, sort_keys, 0);
    out
}

fn write_value(
    v: &Value,
    out: &mut String,
    style: Style,
    ensure_ascii: bool,
    sort_keys: bool,
    depth: usize,
) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_string(s, out, ensure_ascii),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            match style {
                Style::Indent(n) => {
                    for (i, item) in items.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        out.push('\n');
                        push_indent(out, n, depth + 1);
                        write_value(item, out, style, ensure_ascii, sort_keys, depth + 1);
                    }
                    out.push('\n');
                    push_indent(out, n, depth);
                }
                _ => {
                    for (i, item) in items.iter().enumerate() {
                        if i > 0 {
                            out.push_str(match style {
                                Style::Tight => ",",
                                _ => ", ",
                            });
                        }
                        write_value(item, out, style, ensure_ascii, sort_keys, depth);
                    }
                }
            }
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            if sort_keys {
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            match style {
                Style::Indent(n) => {
                    for (i, (k, val)) in entries.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        out.push('\n');
                        push_indent(out, n, depth + 1);
                        write_string(k, out, ensure_ascii);
                        out.push_str(": ");
                        write_value(val, out, style, ensure_ascii, sort_keys, depth + 1);
                    }
                    out.push('\n');
                    push_indent(out, n, depth);
                }
                _ => {
                    for (i, (k, val)) in entries.iter().enumerate() {
                        if i > 0 {
                            out.push_str(match style {
                                Style::Tight => ",",
                                _ => ", ",
                            });
                        }
                        write_string(k, out, ensure_ascii);
                        out.push(':');
                        if style == Style::Spaced {
                            out.push(' ');
                        }
                        write_value(val, out, style, ensure_ascii, sort_keys, depth);
                    }
                }
            }
            out.push('}');
        }
    }
}

fn push_indent(out: &mut String, width: usize, level: usize) {
    for _ in 0..width * level {
        out.push(' ');
    }
}

fn write_string(s: &str, out: &mut String, ensure_ascii: bool) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if ensure_ascii && (c as u32) > 0x7e => {
                let cp = c as u32;
                if cp > 0xffff {
                    // surrogate pair for astral codepoints
                    let v = cp - 0x10000;
                    let hi = 0xd800 + (v >> 10);
                    let lo = 0xdc00 + (v & 0x3ff);
                    out.push_str(&format!("\\u{:04x}\\u{:04x}", hi, lo));
                } else {
                    out.push_str(&format!("\\u{:04x}", cp));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_matches_reference() {
        let v = json!({"b": 1, "a": {"z": [1, 2], "é": "ü"}});
        // sorted keys, compact separators, non-ascii kept raw
        assert_eq!(canonical_json(&v), r#"{"a":{"z":[1,2],"é":"ü"},"b":1}"#);
    }

    #[test]
    fn dumps_variants() {
        let v = json!({"a": 1, "b": [1, 2]});
        assert_eq!(dumps(&v), r#"{"a": 1, "b": [1, 2]}"#);
        assert_eq!(
            dumps_indent(&v, 2),
            "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2\n  ]\n}"
        );
        assert_eq!(
            dumps_indent(&v, 4),
            "{\n    \"a\": 1,\n    \"b\": [\n        1,\n        2\n    ]\n}"
        );
    }

    #[test]
    fn ascii_escaping() {
        assert_eq!(dumps(&json!("héllo")), "\"h\\u00e9llo\"");
        assert_eq!(canonical_json(&json!("héllo")), "\"héllo\"");
        assert_eq!(dumps(&json!("\u{1F600}")), "\"\\ud83d\\ude00\"");
    }
}
