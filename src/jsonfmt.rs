//! JSON serialization with the reference's indented on-disk shape
//! (`json.dumps(v, indent=n)`): newline + indentation between items, ": "
//! after keys, ASCII-escaped. The canonical/schema/compact variants died
//! with the content-addressed SQLite store.

use serde_json::Value;

/// json.dumps(v, indent=n).
pub fn dumps_indent(v: &Value, indent: usize) -> String {
    let mut out = String::new();
    write_value(v, &mut out, indent, 0);
    out
}

fn write_value(v: &Value, out: &mut String, indent: usize, depth: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                push_indent(out, indent, depth + 1);
                write_value(item, out, indent, depth + 1);
            }
            out.push('\n');
            push_indent(out, indent, depth);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                push_indent(out, indent, depth + 1);
                write_string(k, out);
                out.push_str(": ");
                write_value(val, out, indent, depth + 1);
            }
            out.push('\n');
            push_indent(out, indent, depth);
            out.push('}');
        }
    }
}

fn push_indent(out: &mut String, width: usize, level: usize) {
    for _ in 0..width * level {
        out.push(' ');
    }
}

fn write_string(s: &str, out: &mut String) {
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
            c if (c as u32) > 0x7e => {
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
    fn dumps_indent_matches_reference_shape() {
        let v = json!({"a": 1, "b": [1, 2]});
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
        assert_eq!(dumps_indent(&json!("héllo"), 0), "\"h\\u00e9llo\"");
        assert_eq!(dumps_indent(&json!("\u{1F600}"), 0), "\"\\ud83d\\ude00\"");
    }
}
