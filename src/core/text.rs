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

/// Remove ANSI escape sequences from process output before it becomes a
/// tool result: OSC (`ESC ]` … BEL/ST) and CSI-style sequences (an ESC or
/// C1 U+009B introducer, optional intermediates and params, one final
/// byte) go, the text between them stays. The pattern shape is the one
/// ansi-regex uses (ansi-regex is its source); an unterminated
/// sequence at the very end is dropped — the process was cut mid-escape,
/// and passing the bare introducer on would hand the terminal an unbound
/// sequence.
pub fn strip_ansi(text: &str) -> String {
    if !text.as_bytes().contains(&0x1b) && !text.as_bytes().contains(&0xc2) {
        return text.to_string();
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let introducer = b == 0x1b || (b == 0xc2 && bytes.get(i + 1) == Some(&0x9b));
        if introducer {
            let Some(end) = escape_len(bytes, i) else {
                break; // unterminated: drop the tail
            };
            i = end;
            continue;
        }
        // copy the full UTF-8 character, never a lone byte
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Byte offset just past the escape sequence starting at `i`, or None when
/// the input ends before a terminator arrives. CSI/C1: parameter and
/// intermediate bytes then the one final byte; OSC: everything up to BEL
/// or `ESC \`; the two-byte forms close at the byte after the introducer.
/// The one escape parser for the whole tree: terminal width accounting
/// (`render_md`) strips with the same rule the transcript cleaner
/// (`strip_ansi`) does, so the two can never drift apart again.
pub(crate) fn escape_len(bytes: &[u8], i: usize) -> Option<usize> {
    let n = bytes.len();
    match bytes[i] {
        // the C1 CSI introducer arrives as U+009B: two bytes in UTF-8
        0xc2 if bytes.get(i + 1) == Some(&0x9b) => csi_end(bytes, i + 2),
        0x1b => match bytes.get(i + 1) {
            Some(b']') => {
                let mut j = i + 2;
                while j < n {
                    match bytes[j] {
                        0x07 => return Some(j + 1),
                        0x1b if bytes.get(j + 1) == Some(&b'\\') => return Some(j + 2),
                        _ => j += 1,
                    }
                }
                None
            }
            Some(b'[') => csi_end(bytes, i + 2),
            // the two-byte forms close at the byte after the introducer, but
            // only when the second byte really is the sequence's own ASCII
            // payload: a lone ESC in front of a multibyte character (binary
            // output, a mangled paste) would otherwise hand back an offset
            // inside that character and panic the caller's slicing
            Some(b) if b.is_ascii() => Some(i + 2),
            Some(_) => Some(i + 1),
            None => None,
        },
        _ => Some(i + 1),
    }
}

/// End of a CSI body starting at `j` (the first byte after `ESC [` or the
/// C1 introducer): parameter bytes (0x30..=0x3f — digits and separators
/// plus the private-parameter `<=>?` prefixes, so `ESC[?25l` and friends
/// strip whole), intermediate bytes (0x20..=0x2f), then the one final byte
/// (0x40..=0x7e).
fn csi_end(bytes: &[u8], mut j: usize) -> Option<usize> {
    let n = bytes.len();
    while j < n && (0x30..=0x3f).contains(&bytes[j]) {
        j += 1;
    }
    while j < n && (0x20..=0x2f).contains(&bytes[j]) {
        j += 1;
    }
    (j < n && (0x40..=0x7e).contains(&bytes[j])).then_some(j + 1)
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

    #[test]
    fn strip_ansi_removes_sequences_and_keeps_the_text_between() {
        // the common case: SGR color around plain words
        assert_eq!(
            strip_ansi("\x1b[31mred\x1b[0m and \x1b[1;32mgreen\x1b[0m"),
            "red and green"
        );
        // no introducer: an untouched fast path
        assert_eq!(strip_ansi("plain 中文 text"), "plain 中文 text");
        // cursor movement and window-title OSC both go whole
        assert_eq!(strip_ansi("a\x1b[2Kbc\x1b]0;title\x07d"), "abcd");
        assert_eq!(
            strip_ansi("a\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\b"),
            "alinkb"
        );
        // multibyte characters on both sides of a sequence survive whole
        assert_eq!(strip_ansi("中\x1b[K文"), "中文");
        // an unterminated tail is dropped, not passed on
        assert_eq!(strip_ansi("done\x1b[12"), "done");
        assert_eq!(strip_ansi("done\x1b"), "done");
        // C1 CSI (0x9b) is an introducer too
        assert_eq!(strip_ansi("a\u{9b}31mb"), "ab");
        // a lone ESC in front of a multibyte character must not panic nor
        // eat it: the character survives whole
        assert_eq!(strip_ansi("a\u{1b}中b"), "a中b");
        assert_eq!(strip_ansi("中\u{1b}文"), "中文");
        // private-parameter CSI (`?`/`>`/`=`/`<` prefixes: cursor visibility,
        // alt-screen, bracketed paste, primary-device attributes) strips
        // whole and never eats the tail that follows
        assert_eq!(strip_ansi("before \x1b[?25l after"), "before  after");
        assert_eq!(strip_ansi("\x1b[?1049halt\x1b[?1049l"), "alt");
        assert_eq!(strip_ansi("\x1b[?2004h bp\x1b[?2004l"), " bp");
        assert_eq!(strip_ansi("\x1b[>0;1;0c"), "");
        assert_eq!(strip_ansi("m\x1b[=3hf"), "mf");
    }
}
