//! Small string utilities shared across modules.

/// Largest byte index <= `idx` that is a char boundary.
pub fn floor_boundary(s: &str, mut idx: usize) -> usize {
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// [A-Za-z0-9_-]{1,max}: the name class shared by the plugin surfaces
/// (script tools, commands-dir lookups); callers layer their extra rules.
pub fn valid_plugin_name(name: &str, max: usize) -> bool {
    !name.is_empty()
        && name.len() <= max
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn ceil_boundary(s: &str, mut idx: usize) -> usize {
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// Truncate to at most `max` chars, appending `…` when anything was cut.
/// Chars, not cells: callers that care about terminal width use
/// [`crate::core::render_md::truncate_cells`] instead.
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// Cap in place at `max` bytes, floored to a char boundary, appending `…`
/// when anything was cut. The shared ending for previews, summaries and
/// error echoes; the byte cap keeps worst-case cost proportional.
pub fn truncate_ellipsis(s: &mut String, max: usize) {
    if s.len() > max {
        s.truncate(floor_boundary(s, max));
        s.push('…');
    }
}

/// Pop one full UTF-8 character off the end of a byte buffer (raw-mode
/// backspace over multibyte input skips the continuation bytes first).
pub fn pop_utf8_char(buf: &mut Vec<u8>) {
    while let Some(&last) = buf.last()
        && last & 0xC0 == 0x80
    {
        buf.pop();
    }
    buf.pop();
}

/// Parse KEY=VALUE items, rejecting entries without `=`.
pub fn parse_kv(items: &[String]) -> Result<Vec<(String, String)>, String> {
    items
        .iter()
        .map(|item| {
            item.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| format!("expected KEY=VALUE, got '{item}'"))
        })
        .collect()
}

/// Human file size with a space and one decimal above bytes ("12 B",
/// "340 KB", "2.1 GB").
pub fn human_bytes(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < units.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{size:.1} {}", units[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales_with_space() {
        assert_eq!(human_bytes(12), "12 B");
        assert_eq!(human_bytes(340 * 1024), "340.0 KB");
        assert_eq!(human_bytes(0), "0 B");
    }

    #[test]
    fn boundaries_clamp_to_char_edges() {
        let s = "中a中"; // byte boundaries at 0, 3, 4, 7
        assert_eq!(floor_boundary(s, 2), 0);
        assert_eq!(floor_boundary(s, 4), 4);
        assert_eq!(ceil_boundary(s, 1), 3);
        assert_eq!(ceil_boundary(s, 5), 7);
        assert_eq!(floor_boundary(s, 0), 0);
        assert_eq!(floor_boundary(s, s.len()), s.len());
    }

    #[test]
    fn truncate_appends_ellipsis() {
        assert_eq!(truncate_chars("abcd", 3), "abc…");
        assert_eq!(truncate_chars("ab", 3), "ab");
    }
}
