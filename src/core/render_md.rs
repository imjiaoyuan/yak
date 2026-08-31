//! Terminal markdown rendering: a block classifier over complete lines plus
//! an inline scanner, used by the agent REPL and chat to style streamed
//! model output. Tables pass through verbatim; malformed syntax degrades to
//! the original text rather than erroring.

/// Streaming wrapper: feed deltas, whole lines are rendered as they
/// complete into the caller's chunk buffer (printed by the caller);
/// `finish` flushes a trailing partial line (with newline) and reports
/// whether anything was flushed.
pub struct MdStream {
    buf: String,
    state: BlockState,
    /// how much of the current line's raw prefix is already on screen
    /// (character-level streaming), as byte length and cell width counters —
    /// erased in place when the line completes. Counters, not a copy of the
    /// prefix: per-delta cloning made char-by-char streaming quadratic in
    /// line length.
    shown_len: usize,
    shown_cells: usize,
}

impl MdStream {
    /// Content lines get `spaces` spaces of left margin (blank lines stay
    /// empty), so a streamed answer sits visually apart from the chrome.
    pub fn indented(spaces: usize) -> MdStream {
        MdStream {
            buf: String::new(),
            shown_len: 0,
            shown_cells: 0,
            state: BlockState {
                margin: " ".repeat(spaces),
                ..Default::default()
            },
        }
    }

    /// Hard-wrap rendered content at `width` terminal cells (0 = off), so
    /// terminal soft-wrapping cannot break the left margin.
    pub fn wrap_at(&mut self, width: usize) {
        self.state.wrap = width;
    }

    pub fn push_delta(&mut self, text: &str, rendered: &mut String) {
        self.buf.push_str(text);
        while let Some(nl) = self.buf.find('\n') {
            let line = self.buf[..nl].to_string();
            self.buf.drain(..nl + 1);
            self.erase_shown(rendered);
            render_line(&line, &mut self.state, rendered);
        }
        if !self.buf.is_empty() {
            // print the unseen suffix of the current line the moment it
            // arrives, raw; styling snaps in when the line completes and is
            // redrawn in place
            if self.shown_len == 0 {
                rendered.push_str(&self.state.margin);
            }
            let added = &self.buf[self.shown_len..];
            self.shown_cells += cell_width(added);
            self.shown_len = self.buf.len();
            rendered.push_str(added);
        }
    }

    /// Erase the raw partial line: move up to its first visual row and
    /// clear to the end of the screen (stream output is the newest thing
    /// on screen, so nothing below is lost).
    fn erase_shown(&mut self, out: &mut String) {
        if self.shown_len == 0 {
            return;
        }
        let cells = self.state.margin.len() + self.shown_cells;
        let rows = if self.state.wrap > 0 {
            cells.div_ceil(self.state.wrap).clamp(1, 64)
        } else {
            1
        };
        if rows > 1 {
            out.push_str(&format!("\x1b[{}A", rows - 1));
        }
        out.push_str("\r\x1b[J");
        self.shown_len = 0;
        self.shown_cells = 0;
    }

    /// Flush a trailing partial line, terminating it with a newline.
    /// Idempotent; returns false when nothing was pending.
    pub fn finish(&mut self, rendered: &mut String) -> bool {
        if self.buf.is_empty() {
            return false;
        }
        self.erase_shown(rendered);
        let line = std::mem::take(&mut self.buf);
        render_line(&line, &mut self.state, rendered);
        true
    }
}

#[derive(Default)]
struct BlockState {
    in_fence: bool,
    /// last rendered line was blank (collapse runs of blanks)
    prev_blank: bool,
    /// any line rendered yet (suppress leading blanks)
    started: bool,
    /// left margin prepended to content lines
    margin: String,
    /// wrap width in terminal cells (0 = no wrapping)
    wrap: usize,
}

const HR_WIDTH: usize = 40;

fn render_line(raw: &str, st: &mut BlockState, out: &mut String) {
    let t = raw.trim_start();
    let indent = raw.len() - t.len();

    if st.in_fence {
        if t.starts_with("```") {
            st.in_fence = false; // closing fence is suppressed
        } else {
            emit_line(st, &format!("\x1b[2m{t}\x1b[0m"), out);
        }
        st.started = true;
        st.prev_blank = false;
        return;
    }

    if t.is_empty() {
        // blank lines render as nothing: headings and fences carry their
        // own separation, everything else flows tight
        return;
    }

    if t.starts_with("```") {
        // the fence marker itself never prints: a blank line separates the
        // block, the dim body already says "this is code"
        st.in_fence = true;
        if st.started && !st.prev_blank {
            out.push('\n');
        }
        st.started = true;
        st.prev_blank = true;
        return;
    }

    if is_hr(t) {
        emit_line(st, &format!("\x1b[2m{}\x1b[0m", "─".repeat(HR_WIDTH)), out);
        st.started = true;
        st.prev_blank = false;
        return;
    }

    let hashes = t.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && (t.len() == hashes || t.as_bytes()[hashes] == b' ') {
        let content = t[hashes..].trim();
        if st.started && !st.prev_blank {
            out.push('\n');
        }
        let style = "\x1b[1m";
        emit_line(
            st,
            &format!("{style}{}\x1b[0m", render_inline(content)),
            out,
        );
        st.started = true;
        st.prev_blank = false;
        return;
    }

    if let Some(content) = t.strip_prefix('>').map(str::trim_start) {
        emit_line(st, &format!("\x1b[2m▎ {content}\x1b[0m"), out);
        st.started = true;
        st.prev_blank = false;
        return;
    }

    if t.starts_with('|') {
        emit_line(st, raw, out); // table rows pass through verbatim
        st.started = true;
        st.prev_blank = false;
        return;
    }

    if let Some((marker_len, ordered)) = list_marker(t) {
        let level = if indent <= 1 { 1 } else { 2 };
        let lead = if level == 2 { "  " } else { "" };
        let marker = if ordered {
            t[..marker_len].to_string()
        } else {
            "•".to_string()
        };
        let content = render_inline(t[marker_len..].trim_start());
        emit_line(st, &format!("{lead}{marker} {content}"), out);
        st.started = true;
        st.prev_blank = false;
        return;
    }

    emit_line(st, &render_inline(t), out);
    st.started = true;
    st.prev_blank = false;
}

/// Append one rendered content line: the margin, then the content hard-
/// wrapped at st.wrap terminal cells so soft-wrapping cannot break the
/// margin; any open SGR span is re-opened after each break.
fn emit_line(st: &BlockState, content: &str, out: &mut String) {
    if st.wrap == 0 || cell_width(content) <= st.wrap {
        out.push_str(&st.margin);
        out.push_str(content);
        out.push('\n');
        return;
    }
    wrap_scan(content, st.wrap, &st.margin, &st.margin, true, out);
    out.push('\n');
}

/// The one wrap scanner: writes `text` into `out` hard-wrapped at `width`
/// terminal cells, preferring a break just after the last space that fit.
/// The first visual row starts with `first_prefix`, continuation rows with
/// `row_prefix`; with `ansi`, an open SGR span is tracked and re-opened
/// after each break.
fn wrap_scan(
    text: &str,
    width: usize,
    first_prefix: &str,
    row_prefix: &str,
    ansi: bool,
    out: &mut String,
) {
    out.push_str(first_prefix);
    let bytes = text.as_bytes();
    let mut cells = 0usize;
    let mut seg_start = 0usize; // byte offset the current visual row starts at
    let mut last_break: Option<usize> = None; // byte offset just past a space
    let mut active = String::new(); // SGR sequences opened on this row
    let mut i = 0usize;
    while i < bytes.len() {
        if ansi && bytes[i] == 0x1b {
            let end = text[i..].find('m').map_or(text.len(), |p| i + p + 1);
            let seq = &text[i..end];
            if seq == "\x1b[0m" {
                active.clear();
            } else {
                active.push_str(seq);
            }
            i = end;
            continue;
        }
        let c = text[i..].chars().next().unwrap();
        let w = char_width(c);
        if cells + w > width && i > seg_start {
            let cut = last_break.filter(|&b| b > seg_start).unwrap_or(i);
            let emit_end = if cut > seg_start && bytes[cut - 1] == b' ' {
                cut - 1
            } else {
                cut
            };
            out.push_str(&text[seg_start..emit_end]);
            out.push('\n');
            out.push_str(row_prefix);
            if ansi {
                out.push_str(&active);
            }
            let mut next = cut;
            if next < bytes.len() && bytes[next] == b' ' {
                next += 1;
            }
            if next <= i && next == seg_start {
                // never stall: force one char onto this row
                next = i;
            }
            seg_start = next;
            i = next;
            cells = 0;
            last_break = None;
            continue;
        }
        if c == ' ' && cells > 0 {
            last_break = Some(i + 1);
        }
        cells += w;
        i += c.len_utf8();
    }
    out.push_str(&text[seg_start..]);
}

/// Wrap plain text (e.g. a tool-command preview) at `width` terminal
/// cells; continuation lines — from wrapping or from embedded newlines —
/// are prefixed with `margin` spaces. The first output line gets no
/// margin (the caller prefixes it with the `$` marker).
pub fn wrap_plain(text: &str, width: usize, margin: usize) -> String {
    if width == 0 {
        return text.to_string();
    }
    let pad = " ".repeat(margin);
    let mut out = String::new();
    for (i, seg) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
            out.push_str(&pad);
        }
        wrap_scan(seg, width, "", &pad, false, &mut out);
    }
    out
}

/// Wrap plain text with the margin on every line, the first included —
/// the block shape session replay uses for user prompts. (`wrap_plain`
/// leaves the first line bare so a `$ tool ` prefix can sit before it.)
pub fn wrap_block(text: &str, width: usize, margin: usize) -> String {
    if width <= margin {
        return text.to_string();
    }
    let mut wrapped = wrap_plain(text, width - margin, margin);
    if !wrapped.is_empty() {
        wrapped.insert_str(0, &" ".repeat(margin));
    }
    wrapped
}

/// Render a complete markdown text in one shot (session replay, stored
/// responses) at `indent` columns of left margin.
pub fn render_once(text: &str, indent: usize) -> String {
    let mut md = MdStream::indented(indent);
    md.wrap_at(crate::term::columns().saturating_sub(indent).max(20));
    let mut out = String::new();
    md.push_delta(text, &mut out);
    md.finish(&mut out);
    out
}

/// Terminal cell width, ANSI escapes excluded.
pub(crate) fn cell_width(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut w = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += s[i..].find('m').map_or(bytes.len() - i, |p| p + 1);
            continue;
        }
        let c = s[i..].chars().next().unwrap();
        w += char_width(c);
        i += c.len_utf8();
    }
    w
}

/// 2 for CJK and other fullwidth ranges, 1 otherwise.
pub(crate) fn char_width(c: char) -> usize {
    let u = c as u32;
    let wide = matches!(u,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

fn is_hr(t: &str) -> bool {
    t.len() >= 3 && t.chars().all(|c| c == '-' || c == '*' || c == '_')
}

/// Returns (marker length including trailing space, is ordered) when the
/// line starts with a list marker; marker text itself is kept verbatim.
fn list_marker(t: &str) -> Option<(usize, bool)> {
    let b = t.as_bytes();
    if matches!(b.first(), Some(b'-' | b'*' | b'+')) && (b.len() == 1 || b[1] == b' ') {
        return Some((1, false));
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits >= 1
        && b.get(digits) == Some(&b'.')
        && (b.len() == digits + 1 || b.get(digits + 1) == Some(&b' '))
    {
        return Some((digits + 1, true));
    }
    None
}

/// Inline scanner: bold/italic emphasis, code spans, link text. Unterminated
/// markers stay literal; `_` is never an emphasis marker (snake_case).
fn render_inline(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'`' => {
                // a code span opens with N backticks and closes with the
                // same N (CommonMark), so ``x`` and `x` both render clean
                let run = s[i..].chars().take_while(|&c| c == '`').count();
                if let Some(rel) = s[i + run..].find(&"`".repeat(run)) {
                    let content = &s[i + run..i + run + rel];
                    if !content.is_empty() {
                        out.push_str("\x1b[1m");
                        out.push_str(content);
                        out.push_str("\x1b[0m");
                        i += run + rel + run;
                        continue;
                    }
                }
                for _ in 0..run {
                    out.push('`');
                }
                i += run;
            }
            b'*' => {
                let run = s[i..].chars().take_while(|&c| c == '*').count().min(3);
                let close = &s[i..i + run];
                if let Some(rel) = s[i + run..].find(close) {
                    let content = &s[i + run..i + run + rel];
                    if !content.is_empty()
                        && !content.starts_with(' ')
                        && !content.ends_with(' ')
                        && !content.contains('*')
                    {
                        match run {
                            3 => out.push_str("\x1b[1m\x1b[3m"),
                            2 => out.push_str("\x1b[1m"),
                            _ => out.push_str("\x1b[3m"),
                        }
                        out.push_str(content);
                        out.push_str("\x1b[0m");
                        i += run + rel + run;
                        continue;
                    }
                }
                for _ in 0..run {
                    out.push('*');
                }
                i += run;
            }
            b'[' => {
                // [text](url) -> text
                if let Some(cb) = s[i..].find("](")
                    && let Some(end) = s[i + cb + 2..].find(')')
                {
                    let text_part = &s[i + 1..i + cb];
                    if !text_part.is_empty() {
                        out.push_str(text_part);
                        i += cb + 2 + end + 1;
                        continue;
                    }
                }
                out.push('[');
                i += 1;
            }
            _ => {
                let ch = s[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {

    use super::*;

    fn render(text: &str) -> String {
        let mut md = MdStream::indented(0);
        let mut out = String::new();
        md.push_delta(text, &mut out);
        md.finish(&mut out);
        out
    }

    const B: &str = "\x1b[1m"; // bold
    const I: &str = "\x1b[3m"; // italic
    const D: &str = "\x1b[2m"; // dim
    const R: &str = "\x1b[0m"; // reset

    #[test]
    fn headings_render_bold() {
        assert_eq!(render("# 标题\n"), format!("{B}标题{R}\n"));
        assert_eq!(render("## 小节\n"), format!("{B}小节{R}\n"));
        assert_eq!(render("### 深级\n"), format!("{B}深级{R}\n"));
    }

    #[test]
    fn heading_gets_blank_line_before() {
        assert_eq!(render("段落\n# 标题\n"), format!("段落\n\n{B}标题{R}\n"));
    }

    #[test]
    fn inline_emphasis_and_code() {
        assert_eq!(
            render("这是 **加粗** 与 *斜体* 与 `代码`\n"),
            format!("这是 {B}加粗{R} 与 {I}斜体{R} 与 {B}代码{R}\n")
        );
    }

    #[test]
    fn double_backtick_code_span_renders_clean() {
        assert_eq!(
            render("改 ``dedup_bam2/p1`` 目录\n"),
            format!("改 {B}dedup_bam2/p1{R} 目录\n")
        );
        // an unmatched single tick stays literal
        assert_eq!(render("孤立 ` 反引号\n"), "孤立 ` 反引号\n");
    }

    #[test]
    fn triple_bold_italic_and_underscore_literal() {
        assert_eq!(
            render("***粗斜*** _snake_case_\n"),
            format!("{B}{I}粗斜{R} _snake_case_\n")
        );
    }

    #[test]
    fn link_shows_text_only() {
        assert_eq!(
            render("见 [文档](https://example.com) 说明\n"),
            "见 文档 说明\n"
        );
    }

    #[test]
    fn unordered_lists_two_levels_cap() {
        assert_eq!(
            render("- 一级\n  - 二级\n    - 三级\n"),
            "• 一级\n  • 二级\n  • 三级\n"
        );
    }

    #[test]
    fn ordered_list_keeps_numbers() {
        assert_eq!(render("1. 甲\n2. 乙\n"), "1. 甲\n2. 乙\n");
    }

    #[test]
    fn blockquote_dim_with_bar() {
        assert_eq!(render("> 引用内容\n"), format!("{D}▎ 引用内容{R}\n"));
    }

    #[test]
    fn fence_dim_indented_close_suppressed() {
        assert_eq!(
            render("```rust\nfn main() {}\n```\n"),
            format!("{D}fn main() {{}}{R}\n")
        );
    }

    #[test]
    fn fence_content_not_inline_parsed() {
        assert_eq!(
            render("```\n**not bold**\n```\n"),
            format!("{D}**not bold**{R}\n")
        );
    }

    #[test]
    fn hr_renders_dim_rule() {
        assert_eq!(render("---\n"), format!("{D}{}{R}\n", "─".repeat(40)));
    }

    #[test]
    fn table_rows_pass_through_verbatim() {
        let t = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        assert_eq!(render(t), t);
    }

    #[test]
    fn blank_runs_collapse() {
        // paragraph blanks are dropped entirely (tight flow)
        assert_eq!(render("a\n\n\n\nb\n"), "a\nb\n");
    }

    #[test]
    fn leading_blanks_suppressed() {
        assert_eq!(render("\n\na\n"), "a\n");
    }

    #[test]
    fn unterminated_markers_stay_literal() {
        assert_eq!(render("未闭合 **加粗\n"), "未闭合 **加粗\n");
        assert_eq!(render("孤立 ` 反引号\n"), "孤立 ` 反引号\n");
        assert_eq!(
            render("未闭合 [链接](https://x\n"),
            "未闭合 [链接](https://x\n"
        );
    }

    /// Strip the transient character-stream artifacts: everything from the
    /// start of a raw partial line through its in-place erase, leaving only
    /// the settled rendered lines.
    fn settle(s: &str) -> String {
        let marker = "\r\u{1b}[J";
        let parts: Vec<&str> = s.split(marker).collect();
        let mut out = String::new();
        for (i, seg) in parts.iter().enumerate() {
            if i + 1 == parts.len() {
                out.push_str(seg); // nothing follows: no transient to drop
            } else if let Some(nl) = seg.rfind('\n') {
                out.push_str(&seg[..nl + 1]); // drop the erased tail
            }
        }
        out
    }

    #[test]
    fn unterminated_fence_flushes_content() {
        let mut md = MdStream::indented(0);
        let mut out = String::new();
        md.push_delta("```\nab", &mut out);
        md.push_delta("c", &mut out);
        assert!(out.contains("ab")); // streamed, not buffered
        assert!(md.finish(&mut out));
        assert_eq!(settle(&out), format!("{D}abc{R}\n"));
    }

    #[test]
    fn stream_matches_whole_render() {
        let mut md = MdStream::indented(0);
        let mut out = String::new();
        for d in ["**bo", "ld** done\n", "第二", "行\n"] {
            md.push_delta(d, &mut out);
        }
        md.finish(&mut out);
        // characters were visible as they arrived, then the line settled
        assert!(out.contains("**bo") && out.contains("第二"));
        assert_eq!(settle(&out), render("**bold** done\n第二行\n"));
        // single-shot feed through the same stream must agree
    }

    #[test]
    fn finish_flushes_partial_with_newline_idempotent() {
        let mut md = MdStream::indented(0);
        let mut out = String::new();
        md.push_delta("尾部", &mut out);
        assert!(out.contains("尾部")); // streamed, not buffered
        assert!(md.finish(&mut out));
        assert_eq!(settle(&out), "尾部\n");
        assert!(!md.finish(&mut out));
    }

    #[test]
    fn indented_margin_prefixes_content_not_blank_lines() {
        let mut md = MdStream::indented(2);
        let mut out = String::new();
        md.push_delta("hi\n\n- a\n", &mut out);
        md.finish(&mut out);
        assert_eq!(out, "  hi\n  • a\n");
    }

    #[test]
    fn wrapped_lines_carry_the_margin() {
        let mut md = MdStream::indented(2);
        md.wrap_at(10);
        let mut out = String::new();
        md.push_delta("aaaa bbbb cccc dddd\n", &mut out);
        assert_eq!(out, "  aaaa bbbb\n  cccc dddd\n");
    }

    #[test]
    fn cjk_chars_count_two_cells_and_hard_break() {
        let mut md = MdStream::indented(2);
        md.wrap_at(8);
        let mut out = String::new();
        md.push_delta("中中中中中\n", &mut out);
        // 5 wide chars = 10 cells > 8: four fit, the fifth starts a new row
        assert_eq!(out, "  中中中中\n  中\n");
    }

    #[test]
    fn ansi_span_reopens_after_a_wrap() {
        let mut md = MdStream::indented(2);
        md.wrap_at(10);
        let mut out = String::new();
        md.push_delta("**aaaaaaaaaa bbbb**\n", &mut out);
        // the bold span survives the break onto the second visual line
        assert!(
            out.contains("\x1b[1maaaaaaaa\n  \x1b[1ma bbbb") || out.contains("aaaa\n  \x1b[1m"),
            "got {out:?}"
        );
    }

    #[test]
    fn wrap_block_indents_every_line() {
        assert_eq!(wrap_block("aaaa bbbb cccc", 12, 2), "  aaaa bbbb\n  cccc");
        assert_eq!(wrap_block("one\ntwo", 12, 2), "  one\n  two");
        assert_eq!(wrap_block("", 12, 2), "");
    }

    #[test]
    fn wrap_plain_indents_continuations_and_keeps_embedded_breaks() {
        assert_eq!(wrap_plain("aaaa bbbb cccc", 10, 2), "aaaa bbbb\n  cccc");
        assert_eq!(wrap_plain("one\ntwo", 10, 2), "one\n  two");
        assert_eq!(wrap_plain("中中中中中", 8, 2), "中中中中\n  中");
        assert_eq!(wrap_plain("short", 10, 2), "short");
    }

    #[test]
    fn render_once_indents_and_ends_with_newline() {
        let out = render_once("# hi\n\nbody", 2);
        assert!(out.starts_with("  \x1b[1m"), "got {out:?}");
        assert!(out.ends_with('\n'), "got {out:?}");
        assert_eq!(render_once("", 0), "");
    }

    #[test]
    fn empty_stream_finish_is_noop() {
        let mut md = MdStream::indented(0);
        let mut out = String::new();
        assert!(!md.finish(&mut out));
        assert_eq!(out, "");
    }
}
