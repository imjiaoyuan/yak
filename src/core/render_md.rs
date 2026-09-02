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

/// Live block streaming: the answer sits in a fixed left-margin block and is
/// hard-wrapped at `wrap` cells so every visual row carries the margin. Words
/// are buffered until their trailing space/newline so lines never break mid-
/// word (no `R ust`), and rows are committed as they fill — nothing is erased
/// or redrawn, so output streams in one direction with no flicker.
pub struct BlockStream {
    margin: String,
    wrap: usize,
    row_cells: usize,
    at_start: bool,
    emitted: bool,
    /// the current word, held until its trailing delimiter
    word: String,
    /// a space between words, emitted only when the next word fits this row
    pending_space: bool,
}

impl BlockStream {
    pub fn indented(spaces: usize) -> BlockStream {
        BlockStream {
            margin: " ".repeat(spaces),
            wrap: 0,
            row_cells: 0,
            at_start: true,
            emitted: false,
            word: String::new(),
            pending_space: false,
        }
    }

    pub fn wrap_at(&mut self, width: usize) {
        self.wrap = width;
    }

    pub fn push_delta(&mut self, text: &str, rendered: &mut String) {
        for ch in text.chars() {
            // CJK / fullwidth chars have no space-separated words and can be
            // broken at any character, so stream them immediately (a Chinese
            // sentence would otherwise be buffered as one huge "word" and
            // stall until a space/punctuation). Latin words stay buffered so
            // they never split mid-word.
            if char_width(ch) >= 2 {
                self.flush_word(rendered);
                self.emit_breakable(ch, char_width(ch), rendered);
                continue;
            }
            match ch {
                '\n' => {
                    self.pending_space = false; // trailing space at line end dropped
                    self.flush_word(rendered);
                    rendered.push('\n');
                    self.at_start = true;
                    self.row_cells = 0;
                    self.emitted = true;
                }
                ' ' => {
                    self.flush_word(rendered);
                    self.pending_space = true;
                    self.emitted = true;
                }
                _ => {
                    self.word.push(ch);
                    self.emitted = true;
                }
            }
        }
    }

    fn flush_word(&mut self, rendered: &mut String) {
        if self.word.is_empty() {
            return;
        }
        let word = std::mem::take(&mut self.word);
        let wc = cell_width(&word);
        let mut lead = usize::from(self.pending_space);
        self.pending_space = false;
        if self.wrap > 0 && self.row_cells > 0 && self.row_cells + lead + wc > self.wrap {
            rendered.push('\n');
            rendered.push_str(&self.margin);
            self.row_cells = 0;
            lead = 0; // a row break drops the pending space
        }
        if self.at_start {
            rendered.push_str(&self.margin);
            self.at_start = false;
        }
        if lead == 1 {
            rendered.push(' ');
            self.row_cells += 1;
        }
        rendered.push_str(&word);
        self.row_cells += wc;
    }

    /// Emit a breakable (CJK/fullwidth) character: wrap to a fresh row first
    /// when this one would overflow, since these units may break at any cell.
    fn emit_breakable(&mut self, ch: char, w: usize, rendered: &mut String) {
        if self.wrap > 0 && self.row_cells > 0 && self.row_cells + w > self.wrap {
            rendered.push('\n');
            rendered.push_str(&self.margin);
            self.row_cells = 0;
        }
        if self.at_start {
            rendered.push_str(&self.margin);
            self.at_start = false;
        }
        rendered.push(ch);
        self.row_cells += w;
        self.emitted = true;
    }

    /// Idempotent: flush the trailing word and terminate a dangling row so the
    /// footer / chrome starts on its own line; returns whether a newline was
    /// added.
    pub fn finish(&mut self, rendered: &mut String) -> bool {
        self.flush_word(rendered);
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
    fn block_stream_never_splits_words_and_indents_every_row() {
        let mut s = BlockStream::indented(2);
        s.wrap_at(10);
        let mut out = String::new();
        s.push_delta("The Rust toolkit is awesome", &mut out);
        assert!(s.finish(&mut out));
        // words stay whole (no "Ru st"), every wrapped row has the margin
        assert_eq!(out, "  The Rust\n  toolkit is\n  awesome\n");
    }

    #[test]
    fn block_stream_streams_words_as_they_arrive() {
        let mut s = BlockStream::indented(2);
        s.wrap_at(20);
        let mut out = String::new();
        s.push_delta("one two", &mut out);
        // words are shown as their trailing space arrives; the final (dangling)
        // word is held until finish so it never breaks mid-word
        assert_eq!(out, "  one");
        assert!(s.finish(&mut out));
        assert_eq!(out, "  one two\n");
    }

    #[test]
    fn block_stream_streams_cjk_immediately_and_wraps_at_any_char() {
        let mut s = BlockStream::indented(2);
        s.wrap_at(6);
        let mut out = String::new();
        // Chinese has no spaces: chars must appear immediately (not buffered
        // into one huge "word") and may wrap at any character
        s.push_delta("中中中", &mut out);
        assert_eq!(out, "  中中中");
        s.push_delta("中中\n", &mut out);
        // 4 fit (4*2=8 > 6, so after 3 chars we wrap): margin on every row
        assert_eq!(out, "  中中中\n  中中\n");
        assert!(!s.finish(&mut out));
    }

}
