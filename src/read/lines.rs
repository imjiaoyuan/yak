//! Line-window machinery for the plain-text read path: files stream through
//! a BufReader and never load whole. Offsets skip lines without keeping
//! them, the window stops at its limit, and totals are exact only when the
//! lookahead hits EOF — the scan never continues just to count lines.

use std::io::BufRead;

use super::LineCount;

/// Head bytes kept for a single line while scanning; the line still counts
/// when longer, but only this head is stored (LINE_CHAR_CAP chars need at
/// most 4 bytes each, so minified files stay bounded).
const LINE_SCAN_CAP: usize = 8 * 1024;

pub(super) struct WindowResult {
    pub lines: Vec<String>,
    pub total: LineCount,
    /// 1-based file line number of `lines[0]`; 0 when offset ran past EOF.
    pub start: usize,
    /// the scan reached end-of-input inside the window
    pub eof: bool,
}

/// Skip `offset-1` lines, collect `limit`, read one line past the window to
/// learn whether the input ends.
pub(super) fn window_reader(
    r: &mut dyn BufRead,
    offset: usize,
    limit: usize,
) -> std::io::Result<WindowResult> {
    let mut lines: Vec<String> = Vec::new();
    let mut count = 0usize;
    loop {
        // honor ctrl-c mid-read: a huge windowed file must not keep chewing
        // lines after the user asked to stop (see ReadTool interrupt path)
        if crate::core::http::interrupted() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "interrupted by user",
            ));
        }
        let Some(raw) = read_line(r)? else {
            return Ok(WindowResult {
                total: LineCount::Exact(count),
                start: if lines.is_empty() { 0 } else { offset },
                eof: true,
                lines,
            });
        };
        count += 1;
        if count < offset {
            continue;
        }
        if lines.len() == limit {
            // one line exists past the window: the total is a lower bound
            return Ok(WindowResult {
                total: LineCount::AtLeast(count),
                start: offset,
                eof: false,
                lines,
            });
        }
        let mut line = decode_line(&raw, count == 1);
        cap_line(&mut line);
        lines.push(line);
    }
}

/// One raw line without its newline; `None` at end of input. Bytes past
/// LINE_SCAN_CAP are dropped, so a pathological single line costs at most
/// this much memory.
fn read_line(r: &mut dyn BufRead) -> std::io::Result<Option<Vec<u8>>> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return Ok((!out.is_empty()).then_some(out));
        }
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => {
                out.extend_from_slice(&buf[..i]);
                let n = i + 1;
                r.consume(n);
                return Ok(Some(out));
            }
            None => {
                let n = buf.len();
                let keep = n.min(LINE_SCAN_CAP.saturating_sub(out.len()));
                out.extend_from_slice(&buf[..keep]);
                r.consume(n);
            }
        }
    }
}

/// Decode one raw line: strip the UTF-8 BOM on the first line, drop a
/// trailing carriage return, then utf-8 with a latin-1 byte fallback (the
/// same semantics as a typical encoding probe). Splitting bytes on
/// `\n` is safe for multibyte sequences: 0x0A never appears inside one.
fn decode_line(raw: &[u8], first: bool) -> String {
    let mut raw = raw;
    if first {
        raw = raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(raw);
    }
    raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    decode_utf8_or_latin1(raw)
}

/// utf-8 where an incomplete multibyte char at the very end counts as a cut
/// (the budget or the line cap may have split it): decode the valid prefix
/// instead of garbling the whole buffer. Genuinely non-utf-8 input falls
/// back to latin-1 byte→char, which never fails.
fn decode_utf8_or_latin1(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&raw[..e.valid_up_to()])
            .unwrap_or_default()
            .to_string(),
        Err(_) => raw.iter().map(|&b| b as char).collect(),
    }
}

/// Cap one line at LINE_CHAR_CAP chars with an ellipsis.
fn cap_line(line: &mut String) {
    if let Some((idx, _)) = line.char_indices().nth(super::LINE_CHAR_CAP) {
        line.truncate(idx);
        line.push('…');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn result(bytes: &[u8], offset: usize, limit: usize) -> WindowResult {
        window_reader(&mut Cursor::new(bytes), offset, limit).unwrap()
    }

    #[test]
    fn test_window_reads_offset_and_limit() {
        let text = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
        let w = result(text, 3, 4);
        assert_eq!(w.lines, ["three", "four", "five", "six"]);
        assert_eq!(w.start, 3);
        assert!(matches!(w.total, LineCount::AtLeast(7)));
        assert!(!w.eof);
    }

    #[test]
    fn test_window_exact_when_eof_reached() {
        let w = result(b"a\nb\nc\n", 2, 2);
        assert_eq!(w.lines, ["b", "c"]);
        assert!(matches!(w.total, LineCount::Exact(3)));
        assert!(w.eof);
    }

    #[test]
    fn test_window_offset_past_end_signals_with_exact_count() {
        let w = result(b"a\nb\n", 5, 2);
        assert!(w.lines.is_empty());
        assert_eq!(w.start, 0);
        assert!(matches!(w.total, LineCount::Exact(2)));
    }

    #[test]
    fn test_window_empty_input() {
        let w = result(b"", 1, 10);
        assert!(w.lines.is_empty());
        assert!(matches!(w.total, LineCount::Exact(0)));
    }

    #[test]
    fn test_window_last_line_without_newline() {
        let w = result(b"a\nb", 2, 5);
        assert_eq!(w.lines, ["b"]);
        assert!(matches!(w.total, LineCount::Exact(2)));
    }

    #[test]
    fn test_decode_line_bom_crlf_and_latin1() {
        assert_eq!(decode_line(b"\xEF\xBB\xBFhi", true), "hi");
        // the BOM only strips on the first line
        assert_eq!(decode_line(b"\xEF\xBB\xBFhi", false), "\u{feff}hi");
        assert_eq!(decode_line(b"hi\r", false), "hi");
        // 0xE4 alone is invalid utf-8: latin-1 fallback
        assert_eq!(decode_line(&[0xE4, 0x62], false), "\u{e4}b");
        assert_eq!(decode_line("中文".as_bytes(), false), "中文");
    }

    #[test]
    fn test_huge_single_line_is_capped() {
        let mut text = vec![b'x'; 2 * 1024 * 1024];
        text.push(b'\n');
        text.extend_from_slice(b"after\n");
        let w = result(&text, 1, 10);
        assert_eq!(w.lines.len(), 2);
        let capped = w.lines[0].chars().count();
        assert_eq!(capped, super::super::LINE_CHAR_CAP + 1); // payload + ellipsis
        assert!(w.lines[0].ends_with('…'));
        assert_eq!(w.lines[1], "after");
        assert!(matches!(w.total, LineCount::Exact(2)));
    }

    #[test]
    fn test_incomplete_tail_decodes_valid_prefix() {
        assert_eq!(decode_utf8_or_latin1(b"ab\xe4\xb8"), "ab");
        assert_eq!(decode_utf8_or_latin1(b"\xe4\xb8\xad"), "中");
        // genuinely non-utf-8 bytes still fall back to latin-1
        assert_eq!(decode_utf8_or_latin1(&[0xE4, 0x62]), "\u{e4}b");
    }
}
