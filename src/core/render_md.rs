//! Terminal markdown rendering: a block classifier over complete lines plus
//! an inline scanner, used by the agent REPL and chat to style streamed
//! model output. Tables pass through verbatim; malformed syntax degrades to
//! the original text rather than erroring.

use unicode_width::UnicodeWidthChar;

/// Streaming wrapper: feed deltas, whole lines are rendered as they
/// complete into the caller's chunk buffer (printed by the caller);
/// `finish` flushes a trailing partial line (with newline) and reports
/// whether anything was flushed.
pub struct MdStream {
    buf: String,
    state: BlockState,
}

impl MdStream {
    /// Content lines get `spaces` spaces of left margin (blank lines stay
    /// empty), so a streamed answer sits visually apart from the chrome.
    pub fn indented(spaces: usize) -> MdStream {
        MdStream {
            buf: String::new(),
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
            render_line(&line, &mut self.state, rendered);
        }
    }

    /// Flush a trailing partial line, terminating it with a newline. The
    /// stream never paints partials: it emits whole lines as they finish, so
    /// output is "write once" with no in-place erase/redraw flicker.
    /// Idempotent; returns false when nothing was pending.
    pub fn finish(&mut self, rendered: &mut String) -> bool {
        if self.buf.is_empty() {
            return false;
        }
        let line = std::mem::take(&mut self.buf);
        render_line(&line, &mut self.state, rendered);
        true
    }
}

/// Live verbatim streaming: the model's text is written exactly as it
/// arrives — characters, spacing and order untouched. The host owns the
/// right edge: rows hard-wrap at `wrap` terminal cells (unicode-width,
/// CJK-aware) so a continuation row carries the margin instead of the
/// terminal's soft-wrap dropping it to column 0; a space within the last
/// few cells breaks there, sparing most word middles. With `wrap` 0 nothing
/// is measured and the terminal's soft-wrap applies. Nothing is erased or
/// redrawn, so the stream runs one direction.
pub struct BlockStream {
    margin: String,
    /// margin width in cells (spaces, so its length)
    margin_cells: usize,
    at_start: bool,
    emitted: bool,
    /// row budget in terminal cells, margin excluded; 0 = never wrap
    wrap: usize,
    /// re-read the terminal width at every line start (resize-safe)
    dynamic: bool,
    /// cells printed on the current row
    cells: usize,
}

/// A space this close to the right edge is taken as the break point: the
/// next word would straddle the edge anyway.
const WRAP_EARLY: usize = 3;

impl BlockStream {
    pub fn indented(spaces: usize) -> BlockStream {
        BlockStream {
            margin: " ".repeat(spaces),
            margin_cells: spaces,
            at_start: true,
            emitted: false,
            wrap: 0,
            dynamic: false,
            cells: 0,
        }
    }

    /// Terminal mode: wrap at the live terminal width, re-read at every
    /// line start so a resize applies from the next row on.
    pub fn wrap_terminal(&mut self) {
        self.dynamic = true;
        self.refresh_width();
    }

    fn refresh_width(&mut self) {
        self.wrap = crate::term::columns()
            .saturating_sub(self.margin_cells)
            .max(20);
    }

    pub fn push_delta(&mut self, text: &str, rendered: &mut String) {
        for ch in text.chars() {
            if ch == '\n' {
                rendered.push('\n');
                self.at_start = true;
                self.emitted = true;
                self.cells = 0;
                if self.dynamic {
                    self.refresh_width();
                }
                continue;
            }
            // a tab advances to the next 8-column stop from the row's
            // absolute column (margin included); counting it 0 would let a
            // row we think fits overflow the terminal and soft-wrap to
            // column 0, losing the margin
            let mut w = if ch == '\t' {
                8 - ((self.margin_cells + self.cells) % 8)
            } else {
                char_width(ch)
            };
            if self.wrap > 0 {
                if ch == ' ' && self.cells + 1 > self.wrap.saturating_sub(WRAP_EARLY) {
                    // early break at the space: it is consumed, not printed
                    rendered.push('\n');
                    rendered.push_str(&self.margin);
                    self.at_start = false;
                    self.emitted = true;
                    self.cells = 0;
                    continue;
                }
                if self.cells + w > self.wrap && self.cells > 0 {
                    rendered.push('\n');
                    rendered.push_str(&self.margin);
                    self.cells = 0;
                    // a tab's advance depends on where it lands: recompute
                    // from the fresh row start
                    if ch == '\t' {
                        w = 8 - (self.margin_cells % 8);
                    }
                }
            }
            if self.at_start {
                rendered.push_str(&self.margin);
                self.at_start = false;
            }
            rendered.push(ch);
            self.cells += w;
            self.emitted = true;
        }
    }

    /// Idempotent: terminate a dangling row so the footer / chrome starts on
    /// its own line; returns whether a newline was added.
    pub fn finish(&mut self, rendered: &mut String) -> bool {
        if self.emitted && !self.at_start {
            rendered.push('\n');
            self.at_start = true;
            true
        } else {
            false
        }
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
        // blockquotes render with a `>` marker and green text
        emit_line(
            st,
            &format!("\x1b[32m> \x1b[0m\x1b[2m{content}\x1b[0m"),
            out,
        );
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
            "-".to_string()
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
            i = escape_end(bytes, i);
            continue;
        }
        let c = s[i..].chars().next().unwrap();
        w += char_width(c);
        i += c.len_utf8();
    }
    w
}

/// Byte offset just past the escape sequence starting at `i` (`ESC`). Handles
/// CSI (`ESC [`…final), OSC (`ESC ]`…BEL/ST), and the short two-byte forms, so
/// a stray sequence can never make us skip real content or count a control
/// byte as a cell.
fn escape_end(bytes: &[u8], i: usize) -> usize {
    let n = bytes.len();
    match bytes.get(i + 1) {
        Some(b'[') => {
            let mut j = i + 2;
            while j < n {
                if (0x40..=0x7E).contains(&bytes[j]) {
                    return j + 1;
                }
                j += 1;
            }
            n
        }
        Some(b']') => {
            let mut j = i + 2;
            while j < n {
                match bytes[j] {
                    0x07 => return j + 1,
                    0x1b if bytes.get(j + 1) == Some(&b'\\') => return j + 2,
                    _ => j += 1,
                }
            }
            n
        }
        Some(_) => i + 2,
        None => n,
    }
}

/// Terminal cell width: delegated to [`unicode-width`], with the one emulator correction for U+FF9E/U+FF9F
/// halfwidth voiced sound marks, which [`unicode-width`] reports as 0 but
/// real terminals render as 1 cell. Getting this right keeps hard-wrap widths
/// aligned with the terminal's real column count, so rows don't soft-wrap
/// into the left margin or "swallow" the last glyph.
pub(crate) fn char_width(c: char) -> usize {
    if matches!(c, '\u{FF9E}' | '\u{FF9F}') {
        1
    } else {
        UnicodeWidthChar::width(c).unwrap_or(0)
    }
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
                        // inline code renders in a warm color
                        out.push_str("\x1b[33m");
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
                            3 => out.push_str("\x1b[1m\x1b[3m\x1b[36m"),
                            2 => out.push_str("\x1b[1m\x1b[36m"),
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
    const G: &str = "\x1b[32m"; // green
    const C: &str = "\x1b[36m"; // cyan (bold/standout)
    const Y: &str = "\x1b[33m"; // yellow (inline code)
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
            format!("这是 {B}{C}加粗{R} 与 {I}斜体{R} 与 {Y}代码{R}\n")
        );
    }

    #[test]
    fn double_backtick_code_span_renders_clean() {
        assert_eq!(
            render("改 ``dedup_bam2/p1`` 目录\n"),
            format!("改 {Y}dedup_bam2/p1{R} 目录\n")
        );
        // an unmatched single tick stays literal
        assert_eq!(render("孤立 ` 反引号\n"), "孤立 ` 反引号\n");
    }

    #[test]
    fn triple_bold_italic_and_underscore_literal() {
        assert_eq!(
            render("***粗斜*** _snake_case_\n"),
            format!("{B}{I}{C}粗斜{R} _snake_case_\n")
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
            "- 一级\n  - 二级\n  - 三级\n"
        );
    }

    #[test]
    fn ordered_list_keeps_numbers() {
        assert_eq!(render("1. 甲\n2. 乙\n"), "1. 甲\n2. 乙\n");
    }

    #[test]
    fn blockquote_dim_with_bar() {
        assert_eq!(render("> 引用内容\n"), format!("{G}> {R}{D}引用内容{R}\n"));
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

    #[test]
    fn unterminated_fence_flushes_content() {
        let mut md = MdStream::indented(0);
        let mut out = String::new();
        md.push_delta("```\nab", &mut out);
        md.push_delta("c", &mut out);
        assert!(out.is_empty()); // the partial line is buffered until newline
        assert!(md.finish(&mut out));
        assert_eq!(out, format!("{D}abc{R}\n"));
    }

    #[test]
    fn stream_matches_whole_render() {
        let mut md = MdStream::indented(0);
        let mut out = String::new();
        for d in ["**bo", "ld** done\n", "第二", "行\n"] {
            md.push_delta(d, &mut out);
        }
        md.finish(&mut out);
        // whole lines are emitted as they finish; the final partial is
        // flushed by finish. No erase/redraw escapes are ever written.
        assert_eq!(out, render("**bold** done\n第二行\n"));
        assert!(!out.contains("\x1b[J") && !out.contains("\x1b[1A"));
    }

    #[test]
    fn finish_flushes_partial_with_newline_idempotent() {
        let mut md = MdStream::indented(0);
        let mut out = String::new();
        md.push_delta("尾部", &mut out);
        assert!(out.is_empty()); // partial buffered; no flicker
        assert!(md.finish(&mut out));
        assert_eq!(out, "尾部\n");
        assert!(!md.finish(&mut out));
    }

    #[test]
    fn indented_margin_prefixes_content_not_blank_lines() {
        let mut md = MdStream::indented(2);
        let mut out = String::new();
        md.push_delta("hi\n\n- a\n", &mut out);
        md.finish(&mut out);
        assert_eq!(out, "  hi\n  - a\n");
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
        // a one-shot render must never carry in-place erase escapes
        assert!(!out.contains("\x1b[J"), "got {out:?}");
        assert!(!out.contains("\x1b[1A"), "got {out:?}");
        assert_eq!(render_once("", 0), "");
    }

    #[test]
    fn empty_stream_finish_is_noop() {
        let mut md = MdStream::indented(0);
        let mut out = String::new();
        assert!(!md.finish(&mut out));
        assert_eq!(out, "");
    }

    #[test]
    fn block_stream_prints_verbatim_with_margin_on_own_lines() {
        let mut s = BlockStream::indented(2);
        let mut out = String::new();
        s.push_delta("The Rust t", &mut out);
        // the partial row prints as it arrives, not after a newline
        assert_eq!(out, "  The Rust t");
        s.push_delta("oolkit is awesome\nsecond line", &mut out);
        // no host-inserted break: only the model's newline; the terminal's
        // soft-wrap owns the right edge
        assert_eq!(out, "  The Rust toolkit is awesome\n  second line");
        assert!(s.finish(&mut out));
    }

    #[test]
    fn block_stream_prints_partial_words_without_waiting_for_a_delimiter() {
        let mut s = BlockStream::indented(2);
        let mut out = String::new();
        s.push_delta("one two", &mut out);
        // no delimiter ever arrived: everything is already on screen
        assert_eq!(out, "  one two");
        assert!(s.finish(&mut out));
        assert_eq!(out, "  one two\n");
    }

    #[test]
    fn block_stream_finish_then_continue_opens_a_fresh_row() {
        // chrome boundaries terminate a dangling row (so nothing that redraws
        // the current row can retract it); the stream then continues on its
        // own row with the margin intact
        let mut s = BlockStream::indented(2);
        let mut out = String::new();
        s.push_delta("前端只把", &mut out);
        assert_eq!(out, "  前端只把");
        assert!(s.finish(&mut out));
        s.push_delta("app.js 里的", &mut out);
        assert_eq!(out, "  前端只把\n  app.js 里的");
        assert!(s.finish(&mut out));
    }

    #[test]
    fn block_stream_wrap_keeps_margin_on_continuations() {
        let mut s = BlockStream::indented(2);
        s.wrap = 10;
        let mut out = String::new();
        s.push_delta("aaaa bbbb cccc dddd\n", &mut out);
        assert_eq!(out, "  aaaa bbbb\n  cccc dddd\n");
    }

    #[test]
    fn block_stream_breaks_early_at_an_edge_space() {
        let mut s = BlockStream::indented(2);
        s.wrap = 10;
        let mut out = String::new();
        s.push_delta("aaaa bb cccccccccc", &mut out);
        // the space after "bb" sits inside the early window: it becomes the
        // break and is consumed rather than printed at the row's end
        assert_eq!(out, "  aaaa bb\n  cccccccccc");
    }

    #[test]
    fn block_stream_wide_char_never_straddles_the_wrap() {
        let mut s = BlockStream::indented(2);
        s.wrap = 4;
        let mut out = String::new();
        s.push_delta("中中中中\n", &mut out);
        assert_eq!(out, "  中中\n  中中\n");
    }

    #[test]
    fn block_stream_overlong_token_char_breaks() {
        let mut s = BlockStream::indented(2);
        s.wrap = 6;
        let mut out = String::new();
        s.push_delta("GroupShuffleSplit\n", &mut out);
        // no space to prefer: the break lands exactly at the width
        assert_eq!(out, "  GroupS\n  huffle\n  Split\n");
    }

    #[test]
    fn block_stream_tabs_count_real_cells() {
        // wrap 8, margin 2: "a" sits at absolute col 2, the first tab jumps
        // to col 8 (6 cells); the next tab would land at col 16, past the
        // budget, so the row breaks and that tab opens the next row
        let mut s = BlockStream::indented(2);
        s.wrap = 8;
        let mut out = String::new();
        s.push_delta("a\t\tbb\n", &mut out);
        assert_eq!(out, "  a\t\n  \tbb\n");
        // a tab that fits changes nothing about the row count
        let mut s = BlockStream::indented(2);
        s.wrap = 10;
        let mut out = String::new();
        s.push_delta("aaaa\tbb\n", &mut out);
        assert_eq!(out, "  aaaa\tbb\n");
    }

    #[test]
    fn block_stream_terminal_mode_reads_live_columns() {
        let mut s = BlockStream::indented(2);
        s.wrap_terminal();
        // wiring: terminal mode derives the budget from the live width and
        // refreshes it at every line start (resize takes the next row)
        assert_eq!(s.wrap, crate::term::columns().saturating_sub(2).max(20));
        let mut out = String::new();
        s.push_delta("\nx", &mut out);
        assert_eq!(s.wrap, crate::term::columns().saturating_sub(2).max(20));
    }

    #[test]
    fn block_stream_spaces_and_blank_lines_stay_verbatim() {
        let mut s = BlockStream::indented(2);
        let mut out = String::new();
        // leading spaces of a line ride after the margin; blank lines stay
        // empty (no margin on them)
        s.push_delta("  indented\n\nnext", &mut out);
        assert_eq!(out, "    indented\n\n  next");
        assert!(s.finish(&mut out));
    }

    #[test]
    fn char_width_handles_zero_width_wide_and_emoji() {
        // zero-width: combining accent, ZWJ, variation selector
        assert_eq!(char_width('\u{0301}'), 0);
        assert_eq!(char_width('\u{200D}'), 0);
        assert_eq!(char_width('\u{FE0F}'), 0);
        // wide/fullwidth
        assert_eq!(char_width('中'), 2);
        assert_eq!(char_width('Ａ'), 2);
        // emoji pictographs are 2 cells, not 1 (this is what made rows drift)
        assert_eq!(char_width('\u{1F600}'), 2); // 😀
        assert_eq!(char_width('\u{1F3AF}'), 2); // 🎯
        // narrow / halfwidth sound marks
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('\u{FF9E}'), 1);
        assert_eq!(char_width('\u{00B7}'), 1); // ·, ambiguous
    }

    #[test]
    fn cell_width_skips_every_escape_kind_without_losing_content() {
        // CSI whose final byte is not 'm' (cursor-up), OSC 8 hyperlink, plain
        // SGR, and a clear-line CSI — none may consume a following cell.
        assert_eq!(cell_width("\x1b[1Aab"), 2);
        assert_eq!(cell_width("a\x1b[2Kb"), 2);
        assert_eq!(cell_width("\x1b[31m中\x1b[0m"), 2);
        assert_eq!(cell_width("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"), 4);
    }
}
