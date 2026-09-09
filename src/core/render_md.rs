//! Terminal markdown rendering, the reference-styled: one style vocabulary over two
//! engines. [`MdStream`] renders complete lines (session replay, stored
//! responses). [`StyleStream`] renders live model output
//! character-immediately without ever redrawing: the "settled prefix" of
//! each line streams as it arrives, and only the currently open inline
//! marker (an unclosed `**`, `` ` ``, `~~` or `[`) is held back until it
//! resolves or the line ends — the one physical cost of write-once
//! styled output. Malformed syntax degrades to the original text rather
//! than erroring. Tables pass through verbatim, live and replay alike
//! (the simple way: column widths need the whole table, and write-once
//! output cannot restyle what is already on screen).

use crate::theme::Palette;
use unicode_width::UnicodeWidthChar;

/// A space this close to the right edge is taken as the break point: the
/// next word would straddle the edge anyway.
const WRAP_EARLY: usize = 3;

/// the reference caps a horizontal rule at 80 cells.
const HR_MAX: usize = 80;

/// An unclosed inline marker holds at most this many bytes before it
/// degrades to literal text: a prose `[`, a stray backtick or a very long
/// unclosed span must not stall the stream until the line ends. Spans
/// that close within the cap render styled (span text is short in
/// practice); longer ones stream verbatim.
const HOLD_CAP: usize = 80;

// ---------------------------------------------------------------------------
// MdStream: complete-line rendering (replay)
// ---------------------------------------------------------------------------

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
    /// empty), so a rendered answer sits visually apart from the chrome.
    pub fn indented(spaces: usize, p: &'static Palette) -> MdStream {
        MdStream {
            buf: String::new(),
            state: BlockState::new(" ".repeat(spaces), p),
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

    /// Flush a trailing partial line, terminating it with a newline, then
    /// settle any held block (setext paragraph, buffered table). The
    /// stream never paints partials: it emits whole lines as they finish,
    /// so output is "write once" with no in-place erase/redraw flicker.
    /// Idempotent; returns false when nothing was pending.
    pub fn finish(&mut self, rendered: &mut String) -> bool {
        let had = !self.buf.is_empty();
        if had {
            let line = std::mem::take(&mut self.buf);
            render_line(&line, &mut self.state, rendered);
        }
        flush_pending(&mut self.state, rendered);
        self.state.pending_blank = false;
        had
    }
}

struct BlockState {
    in_fence: bool,
    /// any block emitted yet (suppress leading blanks)
    started: bool,
    /// a source blank line awaits the next block: one blank survives
    /// between blocks (runs collapse), so replay matches what the live
    /// stream printed for the same text
    pending_blank: bool,
    /// one paragraph line held back: the next line may be a setext
    /// underline (`====` / `----`) that turns it into a heading
    pending_para: Option<String>,
    /// left margin prepended to content lines
    margin: String,
    /// wrap width in terminal cells (0 = no wrapping)
    wrap: usize,
    p: &'static Palette,
}

impl BlockState {
    fn new(margin: String, p: &'static Palette) -> BlockState {
        BlockState {
            in_fence: false,
            started: false,
            pending_blank: false,
            pending_para: None,
            margin,
            wrap: 0,
            p,
        }
    }
}

fn render_line(raw: &str, st: &mut BlockState, out: &mut String) {
    let p = st.p;
    let t = raw.trim_start();
    let indent = raw.len() - t.len();

    if st.in_fence {
        if t.starts_with("```") && t[3..].trim().is_empty() {
            st.in_fence = false;
            emit_line(st, &format!("{}{}```{}", p.code_border, "", p.reset), out);
        } else if raw.trim().is_empty() {
            out.push('\n');
        } else {
            // code content: colored, two-space indent carried onto wrapped
            // continuation rows (the reference's codeBlockIndent)
            emit_cont(st, &format!("{}  {raw}{}", p.code_block, p.reset), 2, out);
        }
        return;
    }

    if t.is_empty() {
        // blank: closes whatever is pending; one blank survives to
        // separate the next block
        flush_pending(st, out);
        st.pending_blank = st.started;
        return;
    }

    if let Some(rest) = t.strip_prefix("```") {
        flush_pending(st, out);
        sep(st, out);
        let info = rest.trim();
        emit_line(st, &format!("{}```{}{}", p.code_border, info, p.reset), out);
        st.in_fence = true;
        block_done(st);
        return;
    }

    // setext underline: `====`/`----` directly under the held paragraph
    // line turns it into a heading
    if st.pending_para.is_some() && is_setext(t) {
        let para = st.pending_para.take().unwrap();
        let level = if t.starts_with('=') { 1 } else { 2 };
        heading_line(st, level, &para, out);
        block_done(st);
        return;
    }

    if is_hr(t) {
        flush_pending(st, out);
        sep(st, out);
        let n = if st.wrap == 0 {
            HR_MAX
        } else {
            st.wrap.min(HR_MAX)
        };
        emit_line(st, &format!("{}{}{}", p.hr, "─".repeat(n), p.reset), out);
        block_done(st);
        return;
    }

    let hashes = t.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && (t.len() == hashes || t.as_bytes()[hashes] == b' ') {
        flush_pending(st, out);
        sep(st, out);
        heading_line(st, hashes, t[hashes..].trim(), out);
        block_done(st);
        return;
    }

    if let Some(after) = t.strip_prefix('>') {
        flush_pending(st, out);
        sep(st, out);
        let content = after.trim_start();
        let codes = format!("{}{}", p.quote, p.italic);
        let layered = layer_style(&render_inline_pal(content, p), &codes);
        emit_cont(
            st,
            &format!("{}│ {}{}{}", p.quote_border, p.reset, layered, p.reset),
            2,
            out,
        );
        block_done(st);
        return;
    }

    if t.starts_with('|') {
        // tables pass through verbatim, the simple way (live and replay
        // alike: column widths need the whole table, and write-once
        // streaming cannot restyle what is already on screen)
        flush_para(st, out);
        sep(st, out);
        emit_line(st, raw, out);
        block_done(st);
        return;
    }

    if let Some((marker_len, ordered)) = list_marker(t) {
        sep(st, out);
        let level = (1 + indent / 2).min(3);
        let lead = " ".repeat(4 * (level - 1));
        let marker_display = if ordered {
            t[..marker_len].to_string()
        } else {
            "-".to_string()
        };
        let mut content = t[marker_len..].trim_start().to_string();
        // `- [x] `/`- [ ] ` ride inside the colored marker (the reference keeps the
        // literal checkbox)
        let task = if content.starts_with("[x] ")
            || content.starts_with("[X] ")
            || content.starts_with("[ ] ")
        {
            content.drain(..4).collect::<String>()
        } else {
            String::new()
        };
        let head = format!("{lead}{}{marker_display} {task}{}", p.bullet, p.reset);
        let cont = cell_width(&format!("{lead}{marker_display} {task}"));
        let inner = render_inline_pal(&content, p);
        emit_cont(st, &format!("{head}{inner}"), cont, out);
        block_done(st);
        return;
    }

    // paragraph line: hold for setext lookahead
    flush_para(st, out);
    st.pending_para = Some(t.to_string());
}

/// the reference heading styles: h1 = heading color + bold + underline, h2 = color +
/// bold, h3+ keep their `### ` prefix, styled the same.
fn heading_line(st: &mut BlockState, level: usize, content: &str, out: &mut String) {
    let p = st.p;
    let mut codes = format!("{}{}", p.heading, p.bold);
    if level == 1 {
        codes.push_str(&p.underline);
    }
    let text = if level >= 3 {
        format!("{} {content}", "#".repeat(level))
    } else {
        content.to_string()
    };
    let layered = layer_style(&render_inline_pal(&text, p), &codes);
    emit_line(st, &format!("{layered}{}", p.reset), out);
}

/// Emit one blank separator when a source blank line is pending.
fn sep(st: &mut BlockState, out: &mut String) {
    if st.pending_blank && st.started {
        out.push('\n');
    }
    st.pending_blank = false;
}

fn block_done(st: &mut BlockState) {
    st.started = true;
    st.pending_blank = false;
}

/// Flush the setext-lookahead paragraph line as a plain paragraph.
fn flush_para(st: &mut BlockState, out: &mut String) {
    if let Some(prev) = st.pending_para.take() {
        sep(st, out);
        emit_line(st, &render_inline_pal(&prev, st.p), out);
        block_done(st);
    }
}

fn flush_pending(st: &mut BlockState, out: &mut String) {
    flush_para(st, out);
}

/// Append one rendered content line: the margin, then the content
/// hard-wrapped at `st.wrap` terminal cells so soft-wrapping cannot break
/// the margin; any open SGR span is re-opened after each break.
fn emit_line(st: &BlockState, content: &str, out: &mut String) {
    emit_cont(st, content, 0, out);
}

/// [`emit_line`] with a continuation indent: wrapped rows restart at the
/// margin plus `cont` spaces (list markers, quote bars, code indent).
fn emit_cont(st: &BlockState, content: &str, cont: usize, out: &mut String) {
    let width = st.wrap.saturating_sub(cont);
    if width == 0 || cell_width(content) <= width {
        out.push_str(&st.margin);
        out.push_str(content);
        out.push('\n');
        return;
    }
    let cont_pad = format!("{}{}", st.margin, " ".repeat(cont));
    wrap_scan(content, width, &st.margin, &cont_pad, true, out);
    out.push('\n');
}

fn is_setext(t: &str) -> bool {
    let Some(c) = t.chars().next() else {
        return false;
    };
    (c == '=' || c == '-') && t.chars().all(|ch| ch == c)
}

fn is_hr(t: &str) -> bool {
    let marks = t.chars().filter(|c| !c.is_whitespace()).count();
    marks >= 3 && t.chars().all(|c| matches!(c, '-' | '*' | '_' | ' ' | '\t'))
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

/// Apply `codes` around an already-styled string and re-open them after
/// every inner reset, so nested inline spans keep the outer style (the
/// trick the reference uses for quotes and headings).
fn layer_style(inner: &str, codes: &str) -> String {
    if codes.is_empty() {
        return inner.to_string();
    }
    let reset = "\x1b[0m";
    let patched = if inner.contains(reset) {
        inner.replace(reset, &format!("{reset}{codes}"))
    } else {
        inner.to_string()
    };
    format!("{codes}{patched}")
}

// ---------------------------------------------------------------------------
// Inline resolver: one-shot (replay) — the streaming mirror lives in
// StyleStream::scan_inline; the two must stay behaviorally identical.
// ---------------------------------------------------------------------------

/// Inline scanner over a complete string: bold/italic emphasis, strike,
/// code spans, links (the styles: strong = bold, em = italic, no hues).
/// Unterminated markers stay literal; `_` is never an emphasis marker.
fn render_inline_pal(s: &str, p: &Palette) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s.as_bytes()[i] {
            b'`' => {
                let run = s[i..].chars().take_while(|&c| c == '`').count();
                if let Some(rel) = s[i + run..].find(&"`".repeat(run)) {
                    let content = &s[i + run..i + run + rel];
                    if !content.is_empty() {
                        out.push_str(&p.code);
                        out.push_str(content);
                        out.push_str(&p.reset);
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
                if let Some(rel) = s[i + run..].find(&"*".repeat(run)) {
                    let content = &s[i + run..i + run + rel];
                    if !content.is_empty()
                        && !content.starts_with(' ')
                        && !content.ends_with(' ')
                        && !content.contains('*')
                    {
                        match run {
                            3 => out.push_str(&format!("{}{}", p.bold, p.italic)),
                            2 => out.push_str(&p.bold),
                            _ => out.push_str(&p.italic),
                        }
                        out.push_str(content);
                        out.push_str(&p.reset);
                        i += run + rel + run;
                        continue;
                    }
                }
                for _ in 0..run {
                    out.push('*');
                }
                i += run;
            }
            b'~' => {
                let run = s[i..].chars().take_while(|&c| c == '~').count().min(2);
                if run == 2
                    && let Some(rel) = s[i + 2..].find("~~")
                {
                    let content = &s[i + 2..i + 2 + rel];
                    if !content.is_empty() && !content.contains('~') {
                        out.push_str(&p.strike);
                        out.push_str(content);
                        out.push_str(&p.reset);
                        i += 2 + rel + 2;
                        continue;
                    }
                }
                for _ in 0..s[i..].chars().take_while(|&c| c == '~').count().min(2) {
                    out.push('~');
                }
                i += run;
            }
            b'[' => {
                // this bracket's own `]`: a link only when `](url)` closes
                if let Some(rb) = s[i + 1..].find(']') {
                    let close = i + 1 + rb;
                    if s.as_bytes().get(close + 1) == Some(&b'(')
                        && let Some(end) = s[close + 2..].find(')')
                    {
                        let text = &s[i + 1..close];
                        let href = &s[close + 2..close + 2 + end];
                        if !text.is_empty() {
                            out.push_str(&p.link);
                            out.push_str(&p.underline);
                            out.push_str(text);
                            out.push_str(&p.reset);
                            if text != href {
                                out.push_str(&p.link_url);
                                out.push_str(&format!(" ({href})"));
                                out.push_str(&p.reset);
                            }
                            i = close + 2 + end + 1;
                            continue;
                        }
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

// ---------------------------------------------------------------------------
// StyleStream: live write-once styled streaming
// ---------------------------------------------------------------------------

/// Live styled streaming: model text renders the reference-style as it arrives and
/// is written exactly once — no erase, no redraw, no cursor motion. Each
/// line is classified from its first characters (heading, quote, list,
/// fence, table, rule) and the settled prefix streams; an unclosed inline
/// marker holds only its own span until it closes or the line ends (then
/// it flushes literally). Rows hard-wrap at the terminal width
/// (CJK-aware, re-read at every line start so a resize applies from the
/// next row on) so continuation rows carry the margin; open SGR spans
/// re-open after each break.
pub struct StyleStream {
    p: &'static Palette,
    // row emitter
    margin: String,
    margin_cells: usize,
    /// visual row not started: the margin is not yet written
    at_start: bool,
    /// anything printed at all (drives `finish`)
    emitted: bool,
    /// row budget in terminal cells beyond the margin; 0 = never wrap
    wrap: usize,
    /// re-read the terminal width at every line start (resize-safe)
    dynamic: bool,
    /// cells printed on the current row
    cells: usize,
    /// continuation rows printed for the current line
    rows: usize,
    /// extra indent on continuation rows (list marker / quote bar / code
    /// indent width)
    cont: usize,
    /// SGR codes open on the current row (line ctx + span), re-opened
    /// after each break
    open: String,
    // line assembly
    /// raw bytes of the current line arrived so far
    line: String,
    /// anything of this line printed (blank lines print no margin)
    line_printed: bool,
    /// whole-line style codes (heading/quote/fence content), open from
    /// the first emitted char to the line end
    ctx: String,
    st: St,
    in_fence: bool,
    /// fence content bytes already printed (a leading backtick run is
    /// held while it may still be a closing fence)
    fence_flushed: usize,
    /// the two-space fence indent written for this content line
    fence_indented: bool,
    /// an unclosed inline marker is holding its span (plain chars can
    /// then skip the scan pass entirely)
    marker_open: bool,
}

enum St {
    /// classifying from byte 0 (only marker-ish chars so far)
    Classify,
    /// inline-resolving from the byte offset
    Inline(usize),
    /// `|` row streaming verbatim from the byte offset
    Table(usize),
    /// whole line held: HR decision at line end
    Hr,
    /// opening fence line held until line end
    FenceOpen,
}

impl StyleStream {
    pub fn indented(spaces: usize, p: &'static Palette) -> StyleStream {
        StyleStream {
            p,
            margin: " ".repeat(spaces),
            margin_cells: spaces,
            at_start: true,
            emitted: false,
            wrap: 0,
            dynamic: false,
            cells: 0,
            rows: 0,
            cont: 0,
            open: String::new(),
            line: String::new(),
            line_printed: false,
            ctx: String::new(),
            st: St::Classify,
            in_fence: false,
            fence_flushed: 0,
            fence_indented: false,
            marker_open: false,
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
                self.line_end(rendered);
            } else {
                self.line.push(ch);
                if self.in_fence {
                    self.fence_feed(rendered);
                } else {
                    self.advance(rendered);
                }
            }
        }
    }

    /// Idempotent: settle a dangling line so the footer / chrome starts
    /// on its own line; returns whether anything was flushed.
    pub fn finish(&mut self, rendered: &mut String) -> bool {
        if self.line.is_empty() && !self.line_printed {
            return false;
        }
        self.line_end(rendered);
        true
    }

    // ---- line lifecycle ------------------------------------------------

    fn advance(&mut self, out: &mut String) {
        if matches!(self.st, St::Classify) {
            let d = self.decide();
            self.apply(d, out);
        }
        match self.st {
            St::Table(at) => {
                if at < self.line.len() {
                    let rest = self.line[at..].to_string();
                    for ch in rest.chars() {
                        self.putc(ch, out);
                    }
                    self.st = St::Table(self.line.len());
                }
            }
            St::Inline(at) if at < self.line.len() => {
                // fast path: a plain char outside any open marker streams
                // straight through — no scan pass, no allocation
                if !self.marker_open
                    && !matches!(self.line.as_bytes()[at], b'`' | b'*' | b'~' | b'[')
                {
                    let ch = self.line[at..].chars().next().unwrap();
                    self.putc(ch, out);
                    self.st = St::Inline(at + ch.len_utf8());
                } else {
                    self.scan_inline(at, out, false);
                }
            }
            _ => {}
        }
    }

    /// The line ended: settle whatever is still held (a partial marker, an
    /// open inline span, an HR or fence candidate), then close the row.
    fn line_end(&mut self, out: &mut String) {
        if self.in_fence {
            self.fence_line_end(out);
            return;
        }
        // a whitespace-only line is a blank line: no margin, no content
        if !self.line_printed && self.line.trim().is_empty() {
            self.end_row(out);
            return;
        }
        match self.st {
            St::FenceOpen => {
                let line = self.line.clone();
                let run = line.bytes().take_while(|&b| b == b'`').count();
                let info = line[run..].trim().to_string();
                let p = self.p;
                self.span_open(&p.code_border, out);
                for _ in 0..run {
                    self.putc('`', out);
                }
                for ch in info.chars() {
                    self.putc(ch, out);
                }
                self.span_close(out);
                // end_row arms the code-content ctx for the next line
                self.in_fence = true;
                self.end_row(out);
                return;
            }
            St::Hr => {
                if is_hr(&self.line) {
                    let n = self.hr_width();
                    let p = self.p;
                    self.span_open(&p.hr, out);
                    for _ in 0..n {
                        self.putc('─', out);
                    }
                    self.span_close(out);
                    self.end_row(out);
                    return;
                }
                // not a rule after all: resolve as inline
                self.scan_inline(0, out, true);
            }
            St::Classify => {
                let d = self.decide_eol();
                self.apply(d, out);
                if let St::Inline(at) = self.st {
                    self.scan_inline(at, out, true);
                }
            }
            St::Table(at) => {
                let rest = self.line[at..].to_string();
                for ch in rest.chars() {
                    self.putc(ch, out);
                }
            }
            St::Inline(at) => self.scan_inline(at, out, true),
        }
        self.end_row(out);
    }

    fn hr_width(&self) -> usize {
        if self.wrap == 0 {
            HR_MAX
        } else {
            self.wrap.min(HR_MAX)
        }
    }

    /// Close the current row: reset styles, terminate the line, reset all
    /// per-line state; arm the fence content ctx when still inside a
    /// fence.
    fn end_row(&mut self, out: &mut String) {
        // only a still-open span (heading/quote/fence ctx) needs closing;
        // a closed inline span already reset and re-opened nothing
        if self.line_printed && !self.open.is_empty() {
            out.push_str(self.p.reset.as_str());
        }
        out.push('\n');
        self.emitted = true;
        self.reset_line();
        if self.dynamic {
            self.refresh_width();
        }
        if self.in_fence {
            let p = self.p;
            self.ctx = p.code_block.clone();
            self.open = self.ctx.clone();
            self.cont = 2;
        }
    }

    /// Clear every per-line state without terminating the row (a buffered
    /// table row prints nothing yet).
    fn reset_line(&mut self) {
        self.line.clear();
        self.line_printed = false;
        self.cells = 0;
        self.rows = 0;
        self.cont = 0;
        self.at_start = true;
        self.open.clear();
        self.ctx.clear();
        self.st = St::Classify;
        self.fence_flushed = 0;
        self.fence_indented = false;
        self.marker_open = false;
    }

    // ---- fenced code ---------------------------------------------------

    /// One content char inside a fence: everything streams in the code
    /// color under a two-space indent, except a leading backtick run —
    /// held while the whole line so far is backticks, because it may
    /// still turn out to be the closing fence.
    fn fence_feed(&mut self, out: &mut String) {
        let b = self.line.as_bytes();
        let run = b.iter().take_while(|&&c| c == b'`').count();
        if run == b.len() {
            return; // all backticks so far: hold
        }
        if !self.fence_indented {
            self.fence_indented = true;
            self.putc(' ', out);
            self.putc(' ', out);
        }
        while self.fence_flushed < run {
            self.putc('`', out);
            self.fence_flushed += 1;
        }
        let rest = self.line[self.fence_flushed..].to_string();
        let ch = rest.chars().next().unwrap();
        self.putc(ch, out);
        self.fence_flushed += ch.len_utf8();
    }

    fn fence_line_end(&mut self, out: &mut String) {
        let b = self.line.as_bytes();
        let run = b.iter().take_while(|&&c| c == b'`').count();
        if run == b.len() && run >= 3 {
            // closing fence: a border line, then leave fence mode (the
            // content ctx must not layer under the border color)
            self.ctx.clear();
            self.open.clear();
            let p = self.p;
            self.span_open(&p.code_border, out);
            for _ in 0..run {
                self.putc('`', out);
            }
            self.span_close(out);
            self.in_fence = false;
            self.end_row(out);
            return;
        }
        // content line: flush anything still held
        if !b.is_empty() {
            if !self.fence_indented {
                self.fence_indented = true;
                self.putc(' ', out);
                self.putc(' ', out);
            }
            while self.fence_flushed < self.line.len() {
                let rest = self.line[self.fence_flushed..].to_string();
                let ch = rest.chars().next().unwrap();
                self.putc(ch, out);
                self.fence_flushed += ch.len_utf8();
            }
        }
        self.end_row(out);
    }

    // ---- classification --------------------------------------------------

    fn decide(&self) -> Decision {
        let b = self.line.as_bytes();
        let mut i = 0;
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        let t = &b[i..];
        if t.is_empty() {
            return Decision::Wait;
        }
        match t[0] {
            b'`' => {
                let run = t.iter().take_while(|&&c| c == b'`').count();
                if t.len() == run {
                    if run >= 3 {
                        Decision::FenceOpen
                    } else {
                        Decision::Wait
                    }
                } else if run >= 3 {
                    Decision::FenceOpen // backticks + info string
                } else {
                    Decision::Inline { at: i } // an open code-span marker
                }
            }
            b'#' => {
                let hashes = t.iter().take_while(|&&c| c == b'#').count();
                if t.len() == hashes {
                    Decision::Wait
                } else if hashes > 6 {
                    Decision::Inline { at: i }
                } else if t[hashes] == b' ' {
                    Decision::Heading {
                        level: hashes,
                        at: i + hashes + 1,
                    }
                } else {
                    Decision::Inline { at: i }
                }
            }
            b'>' => {
                if t.len() == 1 {
                    Decision::Wait
                } else if t[1] == b' ' {
                    Decision::Quote { at: i + 2 }
                } else {
                    Decision::Quote { at: i + 1 }
                }
            }
            b'|' => Decision::Table,
            b'-' | b'*' | b'+' => {
                if t.len() == 1 {
                    Decision::Wait
                } else if t[1] == b' ' {
                    // a list marker — but hr-only content (`- - -`) still
                    // makes the whole line a thematic break: hold as Hr
                    if is_hr_so_far(t) {
                        Decision::Hr
                    } else {
                        self.decide_list(i, t, 2)
                    }
                } else if t[0] != b'+' && t[1] == t[0] && is_hr_so_far(t) {
                    // `--`/`**`-shaped: an hr candidate (aborts to inline
                    // as soon as a non-marker char arrives)
                    Decision::Hr
                } else {
                    Decision::Inline { at: i }
                }
            }
            b'_' => {
                if t.len() == 1 {
                    Decision::Wait
                } else if is_hr_so_far(t) {
                    Decision::Hr
                } else {
                    Decision::Inline { at: i }
                }
            }
            b'0'..=b'9' => {
                let digits = t.iter().take_while(|c| c.is_ascii_digit()).count();
                if t.len() == digits {
                    Decision::Wait
                } else if t[digits] != b'.' {
                    Decision::Inline { at: i }
                } else if t.len() == digits + 1 {
                    Decision::Wait
                } else if t[digits + 1] == b' ' {
                    self.decide_list(i, t, digits + 2)
                } else {
                    Decision::Inline { at: i }
                }
            }
            _ => Decision::Inline { at: i },
        }
    }

    /// EOL flavor of [`StyleStream::decide`]: a line that ended while
    /// still classifying resolves its partial marker.
    fn decide_eol(&self) -> Decision {
        if let d @ (Decision::Heading { .. } | Decision::Quote { .. } | Decision::FenceOpen) =
            self.decide()
        {
            return d;
        }
        let b = self.line.as_bytes();
        let mut i = 0;
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        let t = &b[i..];
        if t.is_empty() {
            return Decision::Inline { at: i };
        }
        // a bare list marker is an empty item
        if matches!(t[0], b'-' | b'*' | b'+') && (t.len() == 1 || t[1] == b' ') {
            return Decision::List {
                indent: i,
                at: i + t.len().min(2),
            };
        }
        if t[0].is_ascii_digit() {
            let digits = t.iter().take_while(|c| c.is_ascii_digit()).count();
            if t.len() == digits + 1 && t[digits] == b'.' {
                return Decision::List {
                    indent: i,
                    at: i + digits + 1,
                };
            }
        }
        Decision::Inline { at: i }
    }

    /// An unordered/ordered marker was seen; a `[` directly after it may
    /// open a task checkbox (`- [x] `), so the marker holds until the
    /// bracket resolves.
    fn decide_list(&self, indent: usize, t: &[u8], marker_end: usize) -> Decision {
        if t.len() == marker_end {
            // the char after the marker has not arrived: it may open a
            // task checkbox (`- [x] `)
            return Decision::Wait;
        }
        if t.get(marker_end) == Some(&b'[') {
            if let Some(rb) = t[marker_end..].iter().position(|&c| c == b']') {
                let close = marker_end + rb;
                if t.len() > close + 1 {
                    let inner = &t[marker_end + 1..close];
                    if matches!(inner, b"x" | b"X" | b" ") && t[close + 1] == b' ' {
                        return Decision::List {
                            indent,
                            at: indent + close + 2,
                        };
                    }
                    return Decision::List {
                        indent,
                        at: indent + marker_end,
                    };
                }
                return Decision::Wait; // the char after ']' has not arrived
            }
            return Decision::Wait; // no ']' yet
        }
        Decision::List {
            indent,
            at: indent + marker_end,
        }
    }

    fn apply(&mut self, d: Decision, out: &mut String) {
        let p = self.p;
        match d {
            Decision::Wait => {}
            Decision::Hr => self.st = St::Hr,
            Decision::FenceOpen => self.st = St::FenceOpen,
            Decision::Inline { at } => self.st = St::Inline(at),
            Decision::Table => self.st = St::Table(0),
            Decision::Heading { level, at } => {
                let mut codes = format!("{}{}", p.heading, p.bold);
                if level == 1 {
                    codes.push_str(&p.underline);
                }
                self.ctx = codes;
                self.open = self.ctx.clone();
                if level >= 3 {
                    // the reference keeps the `### ` prefix visible for deep headings
                    for _ in 0..level {
                        self.putc('#', out);
                    }
                    self.putc(' ', out);
                }
                self.st = St::Inline(at);
            }
            Decision::Quote { at } => {
                self.span_open(&p.quote_border, out);
                self.putc('│', out);
                self.putc(' ', out);
                self.span_close(out);
                // the quote style opens for the content that follows
                let codes = format!("{}{}", p.quote, p.italic);
                self.ctx = codes.clone();
                self.span_open(&codes, out);
                self.cont = 2;
                self.st = St::Inline(at);
            }
            Decision::List { indent, at } => {
                // the reference nests four spaces per level; two source spaces nest
                let level = (1 + indent / 2).min(3);
                for _ in 1..level {
                    for _ in 0..4 {
                        self.putc(' ', out);
                    }
                }
                let raw = self.line[indent..at].to_string();
                let mut marker: String = {
                    let mut cs = raw.chars();
                    match cs.next() {
                        Some('-' | '*' | '+') => format!("-{}", cs.as_str()),
                        _ => raw,
                    }
                };
                // a bare marker renders with its trailing space (`- `, `1. `)
                if !marker.ends_with(' ') {
                    marker.push(' ');
                }
                self.cont = cell_width(&marker) + 4 * (level - 1);
                self.span_open(&p.bullet, out);
                for ch in marker.chars() {
                    self.putc(ch, out);
                }
                self.span_close(out);
                self.st = St::Inline(at);
            }
        }
    }

    // ---- inline resolver (streaming mirror of render_inline_pal) -------

    /// Resolve inline markup from `at` while it is settled: plain text
    /// streams char-by-char; a closed code/emphasis/link span emits as
    /// one styled burst; an unclosed marker holds until `eol`, where it
    /// flushes literally and the scan continues (matching the one-shot
    /// resolver byte for byte).
    fn scan_inline(&mut self, at: usize, out: &mut String, eol: bool) {
        let line = self.line.clone();
        let p = self.p;
        let mut i = at;
        while i < line.len() {
            match line.as_bytes()[i] {
                b'`' => {
                    let run = line[i..].chars().take_while(|&c| c == '`').count();
                    if let Some(rel) = line[i + run..].find(&"`".repeat(run)) {
                        let content = &line[i + run..i + run + rel];
                        if !content.is_empty() {
                            self.span_open(&p.code, out);
                            for ch in content.chars() {
                                self.putc(ch, out);
                            }
                            self.span_close(out);
                            i += run + rel + run;
                            continue;
                        }
                    }
                    if !eol && line.len() - i <= HOLD_CAP {
                        break;
                    }
                    for _ in 0..run {
                        self.putc('`', out);
                    }
                    i += run;
                }
                b'*' => {
                    let run = line[i..].chars().take_while(|&c| c == '*').count().min(3);
                    if let Some(rel) = line[i + run..].find(&"*".repeat(run)) {
                        let content = &line[i + run..i + run + rel];
                        if !content.is_empty()
                            && !content.starts_with(' ')
                            && !content.ends_with(' ')
                            && !content.contains('*')
                        {
                            let codes = match run {
                                3 => format!("{}{}", p.bold, p.italic),
                                2 => p.bold.clone(),
                                _ => p.italic.clone(),
                            };
                            self.span_open(&codes, out);
                            for ch in content.chars() {
                                self.putc(ch, out);
                            }
                            self.span_close(out);
                            i += run + rel + run;
                            continue;
                        }
                    }
                    if !eol && line.len() - i <= HOLD_CAP {
                        break;
                    }
                    for _ in 0..run {
                        self.putc('*', out);
                    }
                    i += run;
                }
                b'~' => {
                    let tilde_run = line[i..].chars().take_while(|&c| c == '~').count();
                    if tilde_run < 2 {
                        self.putc('~', out);
                        i += 1;
                        continue;
                    }
                    if let Some(rel) = line[i + 2..].find("~~") {
                        let content = &line[i + 2..i + 2 + rel];
                        if !content.is_empty() && !content.contains('~') {
                            self.span_open(&p.strike, out);
                            for ch in content.chars() {
                                self.putc(ch, out);
                            }
                            self.span_close(out);
                            i += 2 + rel + 2;
                            continue;
                        }
                    }
                    if !eol && line.len() - i <= HOLD_CAP {
                        break;
                    }
                    for _ in 0..tilde_run.min(2) {
                        self.putc('~', out);
                    }
                    i += tilde_run.min(2);
                }
                b'[' => {
                    // this bracket's own `]`: a link only when `](url)`
                    // closes — otherwise the `[` is settled literal text
                    if let Some(rb) = line[i + 1..].find(']') {
                        let close = i + 1 + rb;
                        if close + 1 >= line.len() {
                            if !eol && line.len() - i <= HOLD_CAP {
                                break; // ']' is the last char so far
                            }
                        } else if line.as_bytes()[close + 1] != b'(' {
                            self.putc('[', out);
                            i += 1;
                            continue;
                        } else if let Some(end) = line[close + 2..].find(')') {
                            let text = &line[i + 1..close];
                            let href = &line[close + 2..close + 2 + end];
                            if !text.is_empty() {
                                let codes = format!("{}{}", p.link, p.underline);
                                self.span_open(&codes, out);
                                for ch in text.chars() {
                                    self.putc(ch, out);
                                }
                                self.span_close(out);
                                if text != href {
                                    let tail = format!(" ({href})");
                                    self.span_open(&p.link_url, out);
                                    for ch in tail.chars() {
                                        self.putc(ch, out);
                                    }
                                    self.span_close(out);
                                }
                                i = close + 2 + end + 1;
                                continue;
                            }
                            self.putc('[', out);
                            i += 1;
                            continue;
                        } else if !eol && line.len() - i <= HOLD_CAP {
                            break; // `](` seen, ')' pending
                        }
                    } else if !eol && line.len() - i <= HOLD_CAP {
                        break; // no ']' yet
                    }
                    self.putc('[', out);
                    i += 1;
                }
                _ => {
                    let ch = line[i..].chars().next().unwrap();
                    self.putc(ch, out);
                    i += ch.len_utf8();
                }
            }
        }
        self.st = St::Inline(i);
        // broke early = a marker still holds its span
        self.marker_open = i < line.len();
    }

    // ---- row emitter ----------------------------------------------------

    /// Write one character: lazy row start (margin + open codes), tab
    /// stops from the absolute column, hard wrap with the continuation
    /// indent, early break at an edge space (consumed, not printed).
    fn putc(&mut self, c: char, out: &mut String) {
        let mut w = if c == '\t' {
            8 - ((self.margin_cells + self.cells) % 8)
        } else {
            char_width(c)
        };
        if self.wrap > 0 {
            let budget = self.wrap.saturating_sub(self.cont);
            if c == ' ' && self.cells + 1 > budget.saturating_sub(WRAP_EARLY) {
                self.break_row(out);
                return;
            }
            if self.cells + w > budget && self.cells > 0 {
                self.break_row(out);
                if c == '\t' {
                    w = 8 - ((self.margin_cells + self.cont) % 8);
                }
            }
        }
        if self.at_start {
            self.begin_row(out);
        }
        out.push(c);
        self.cells += w;
        self.line_printed = true;
        self.emitted = true;
    }

    fn begin_row(&mut self, out: &mut String) {
        out.push_str(&self.margin);
        self.at_start = false;
        if !self.open.is_empty() {
            out.push_str(&self.open);
        }
    }

    /// Start a continuation row: newline, margin, continuation indent,
    /// re-opened SGR span.
    fn break_row(&mut self, out: &mut String) {
        out.push('\n');
        out.push_str(&self.margin);
        if self.cont > 0 {
            out.push_str(&" ".repeat(self.cont));
        }
        self.cells = 0;
        self.rows += 1;
        self.at_start = false;
        if !self.open.is_empty() {
            out.push_str(&self.open);
        }
    }

    /// Open a styled span on top of the line ctx; codes re-open after
    /// row breaks.
    fn span_open(&mut self, codes: &str, out: &mut String) {
        if codes.is_empty() {
            return;
        }
        if !self.at_start {
            out.push_str(codes);
        }
        self.open.push_str(codes);
    }

    /// Close the current span: reset, then re-open the line ctx so later
    /// text keeps the heading/quote/fence style.
    fn span_close(&mut self, out: &mut String) {
        if self.at_start {
            self.open = self.ctx.clone();
            return;
        }
        out.push_str(self.p.reset.as_str());
        if !self.ctx.is_empty() {
            out.push_str(&self.ctx);
        }
        self.open = self.ctx.clone();
    }
}

enum Decision {
    /// need more characters to classify
    Wait,
    Inline {
        at: usize,
    },
    Heading {
        level: usize,
        at: usize,
    },
    Quote {
        at: usize,
    },
    List {
        indent: usize,
        at: usize,
    },
    Table,
    Hr,
    FenceOpen,
}

/// The line so far consists only of thematic-break characters (spaces
/// allowed between them): it may still be an HR.
fn is_hr_so_far(t: &[u8]) -> bool {
    !t.is_empty()
        && t.iter().all(|&c| matches!(c, b'-' | b'_' | b'*' | b' '))
        && t.iter().filter(|&&c| c != b' ').count() >= 2
}

// ---------------------------------------------------------------------------
// shared wrapping / measuring
// ---------------------------------------------------------------------------

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
    let mut md = MdStream::indented(indent, crate::theme::out());
    md.wrap_at(crate::term::columns().saturating_sub(indent).max(20));
    let mut out = String::new();
    md.push_delta(text, &mut out);
    md.finish(&mut out);
    out
}

/// Truncate to at most `max` terminal cells (CJK-aware), "…" on cut.
pub(crate) fn truncate_cells(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut cells = 0usize;
    for c in text.chars() {
        let w = char_width(c);
        if cells + w > max.saturating_sub(1) {
            out.push('…');
            return out;
        }
        out.push(c);
        cells += w;
    }
    out
}

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

#[cfg(test)]
mod tests {

    use super::*;

    fn p() -> &'static Palette {
        crate::theme::ansi256()
    }

    fn render(text: &str) -> String {
        let mut md = MdStream::indented(0, p());
        let mut out = String::new();
        md.push_delta(text, &mut out);
        md.finish(&mut out);
        out
    }

    /// Stream `text` through StyleStream char-by-char (margin 0).
    fn live(text: &str) -> String {
        let mut s = StyleStream::indented(0, p());
        let mut out = String::new();
        for ch in text.chars() {
            s.push_delta(&ch.to_string(), &mut out);
        }
        s.finish(&mut out);
        out
    }

    const B: &str = "\x1b[1m"; // bold
    const I: &str = "\x1b[3m"; // italic
    const U: &str = "\x1b[4m"; // underline
    const H: &str = "\x1b[38;5;222m"; // heading
    const L: &str = "\x1b[38;5;110m"; // link
    const LU: &str = "\x1b[38;5;242m"; // link url
    const C: &str = "\x1b[38;5;109m"; // code + bullet (the reference accent)
    const CB: &str = "\x1b[38;5;143m"; // code block (the reference green)
    const G: &str = "\x1b[38;5;244m"; // gray (quote/border/hr)
    const S: &str = "\x1b[9m"; // strike
    const R: &str = "\x1b[0m";

    // ---- MdStream: headings ------------------------------------------

    #[test]
    fn headings_render_styled_by_level() {
        assert_eq!(render("# 标题\n"), format!("{H}{B}{U}标题{R}\n"));
        assert_eq!(render("## 小节\n"), format!("{H}{B}小节{R}\n"));
        // h3+ keep their prefix, styled like the heading
        assert_eq!(render("### 深级\n"), format!("{H}{B}### 深级{R}\n"));
    }

    #[test]
    fn setext_underlines_make_headings() {
        assert_eq!(render("标题\n=====\n"), format!("{H}{B}{U}标题{R}\n"));
        assert_eq!(render("标题\n-----\n"), format!("{H}{B}标题{R}\n"));
        // a ---- with no paragraph above stays a rule
        assert_eq!(render("---\n"), format!("{G}{}{R}\n", "─".repeat(80)));
    }

    #[test]
    fn one_blank_survives_between_blocks() {
        assert_eq!(render("a\n\nb\n"), "a\n\nb\n");
        // runs collapse to one
        assert_eq!(render("a\n\n\n\nb\n"), "a\n\nb\n");
        // no source blank: no injected blank (matches the live stream)
        assert_eq!(render("a\nb\n"), "a\nb\n");
        // leading and trailing blanks drop
        assert_eq!(render("\n\na\n\n"), "a\n");
    }

    // ---- MdStream: inline --------------------------------------------

    #[test]
    fn inline_emphasis_code_and_strike() {
        assert_eq!(
            render("这是 **加粗** 与 *斜体* 与 `代码` 与 ~~删除~~\n"),
            format!("这是 {B}加粗{R} 与 {I}斜体{R} 与 {C}代码{R} 与 {S}删除{R}\n")
        );
        assert_eq!(render("***粗斜***\n"), format!("{B}{I}粗斜{R}\n"));
    }

    #[test]
    fn double_backtick_code_span_renders_clean() {
        assert_eq!(
            render("改 ``dedup_bam2/p1`` 目录\n"),
            format!("改 {C}dedup_bam2/p1{R} 目录\n")
        );
        assert_eq!(render("孤立 ` 反引号\n"), "孤立 ` 反引号\n");
    }

    #[test]
    fn underscore_is_never_emphasis() {
        assert_eq!(render("_snake_case_\n"), "_snake_case_\n");
    }

    #[test]
    fn link_underlines_and_shows_a_differing_href() {
        assert_eq!(
            render("见 [文档](https://example.com) 说明\n"),
            format!("见 {L}{U}文档{R}{LU} (https://example.com){R} 说明\n")
        );
        // text == href: no duplicate
        assert_eq!(render("[x](x)\n"), format!("{L}{U}x{R}\n"));
    }

    #[test]
    fn unterminated_markers_stay_literal() {
        assert_eq!(render("未闭合 **加粗\n"), "未闭合 **加粗\n");
        assert_eq!(
            render("未闭合 [链接](https://x\n"),
            "未闭合 [链接](https://x\n"
        );
        // a closed span inside an unclosed one still renders
        assert_eq!(render("a **b *c* d\n"), format!("a **b {I}c{R} d\n"));
    }

    // ---- MdStream: fences, quotes, lists, rules ----------------------

    #[test]
    fn fence_shows_borders_and_indents_content() {
        assert_eq!(
            render("```rust\nfn main() {}\n```\n"),
            format!("{G}```rust{R}\n{CB}  fn main() {{}}{R}\n{G}```{R}\n")
        );
        // blank separation from surrounding paragraphs
        assert_eq!(
            render("para\n\n```rust\nlet x;\n```\n\nafter\n"),
            format!("para\n\n{G}```rust{R}\n{CB}  let x;{R}\n{G}```{R}\n\nafter\n")
        );
    }

    #[test]
    fn unterminated_fence_flushes_content() {
        assert_eq!(render("```\nabc"), format!("{G}```{R}\n{CB}  abc{R}\n"));
    }

    #[test]
    fn fence_content_not_inline_parsed() {
        assert_eq!(
            render("```\n**not bold**\n```\n"),
            format!("{G}```{R}\n{CB}  **not bold**{R}\n{G}```{R}\n")
        );
    }

    #[test]
    fn quote_uses_bar_and_italic_gray() {
        assert_eq!(
            render("> 引用内容\n"),
            format!("{G}│ {R}{G}{I}引用内容{R}\n")
        );
    }

    #[test]
    fn lists_nest_four_spaces_and_color_markers() {
        assert_eq!(
            render("- 一级\n  - 二级\n      - 三级\n"),
            format!("{C}- {R}一级\n    {C}- {R}二级\n        {C}- {R}三级\n")
        );
    }

    #[test]
    fn ordered_list_keeps_numbers() {
        assert_eq!(
            render("1. 甲\n2. 乙\n"),
            format!("{C}1. {R}甲\n{C}2. {R}乙\n")
        );
    }

    #[test]
    fn task_lists_keep_the_literal_checkbox() {
        assert_eq!(
            render("- [x] 完成\n- [ ] 待办\n"),
            format!("{C}- [x] {R}完成\n{C}- [ ] {R}待办\n")
        );
    }

    #[test]
    fn hr_renders_dim_rule_capped_at_80() {
        assert_eq!(render("---\n"), format!("{G}{}{R}\n", "─".repeat(80)));
        let mut md = MdStream::indented(0, p());
        md.wrap_at(40);
        let mut out = String::new();
        md.push_delta("---\n", &mut out);
        assert_eq!(out, format!("{G}{}{R}\n", "─".repeat(40)));
    }

    // ---- MdStream: tables ---------------------------------------------

    #[test]
    fn table_rows_pass_through_verbatim() {
        let t = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        assert_eq!(render(t), t);
    }

    // ---- MdStream: margins and wrapping --------------------------------

    #[test]
    fn indented_margin_prefixes_content_not_blank_lines() {
        let mut md = MdStream::indented(2, p());
        let mut out = String::new();
        md.push_delta("hi\n\n- a\n", &mut out);
        md.finish(&mut out);
        assert_eq!(out, format!("  hi\n\n  {C}- {R}a\n"));
    }

    #[test]
    fn wrapped_lines_carry_the_margin() {
        let mut md = MdStream::indented(2, p());
        md.wrap_at(10);
        let mut out = String::new();
        md.push_delta("aaaa bbbb cccc dddd\n", &mut out);
        md.finish(&mut out); // the paragraph line is held for setext lookahead
        assert_eq!(out, "  aaaa bbbb\n  cccc dddd\n");
    }

    #[test]
    fn wrapped_list_continuations_align_under_content() {
        let mut md = MdStream::indented(2, p());
        md.wrap_at(12);
        let mut out = String::new();
        md.push_delta("- aaaa bbbb cccc\n", &mut out);
        // cont 2: continuation rows indent past the marker
        assert_eq!(out, format!("  {C}- {R}aaaa\n    bbbb cccc\n"));
    }

    // ---- StyleStream: live streaming ------------------------------------

    #[test]
    fn live_plain_text_streams_verbatim_with_margin() {
        let mut s = StyleStream::indented(2, p());
        let mut out = String::new();
        s.push_delta("The Rust t", &mut out);
        assert_eq!(out, "  The Rust t"); // printed as it arrives
        s.push_delta("oolkit\nsecond line", &mut out);
        assert_eq!(out, "  The Rust toolkit\n  second line");
        assert!(s.finish(&mut out));
        assert_eq!(out, "  The Rust toolkit\n  second line\n");
    }

    #[test]
    fn live_blank_lines_stay_empty() {
        assert_eq!(live("a\n\nb\n"), "a\n\nb\n");
    }

    #[test]
    fn live_heading_opens_style_and_streams_chars() {
        let mut s = StyleStream::indented(0, p());
        let mut out = String::new();
        s.push_delta("# 标", &mut out);
        assert_eq!(out, format!("{H}{B}{U}标")); // style opens immediately
        s.push_delta("题\n", &mut out);
        assert_eq!(out, format!("{H}{B}{U}标题{R}\n"));
    }

    #[test]
    fn live_deep_heading_keeps_prefix() {
        assert_eq!(live("### 深级\n"), format!("{H}{B}### 深级{R}\n"));
    }

    #[test]
    fn live_bold_holds_only_the_span() {
        let mut s = StyleStream::indented(0, p());
        let mut out = String::new();
        s.push_delta("a **bo", &mut out);
        // the open marker holds its own tail; the prefix streamed
        assert_eq!(out, "a ");
        s.push_delta("ld** x\n", &mut out);
        assert_eq!(out, format!("a {B}bold{R} x\n"));
    }

    #[test]
    fn live_inline_code_and_links_burst_on_close() {
        assert_eq!(live("use `foo` here\n"), format!("use {C}foo{R} here\n"));
        assert_eq!(
            live("见 [文档](https://x.com) 吗\n"),
            format!("见 {L}{U}文档{R}{LU} (https://x.com){R} 吗\n")
        );
    }

    #[test]
    fn live_unclosed_marker_flushes_literally_at_line_end() {
        assert_eq!(live("a **b\n"), "a **b\n");
        // nested closed italic inside the unclosed bold still renders
        assert_eq!(live("a **b *c* d\n"), format!("a **b {I}c{R} d\n"));
    }

    #[test]
    fn live_quote_streams_with_bar() {
        let mut s = StyleStream::indented(0, p());
        let mut out = String::new();
        s.push_delta("> 引用", &mut out);
        assert_eq!(out, format!("{G}│ {R}{G}{I}引用"));
        s.push_delta("内容\n", &mut out);
        assert_eq!(out, format!("{G}│ {R}{G}{I}引用内容{R}\n"));
    }

    #[test]
    fn live_lists_color_the_marker() {
        assert_eq!(live("- 一级\n"), format!("{C}- {R}一级\n"));
        assert_eq!(live("  - 二级\n"), format!("    {C}- {R}二级\n"));
        assert_eq!(live("1. 甲\n"), format!("{C}1. {R}甲\n"));
        assert_eq!(live("- [x] 完成\n"), format!("{C}- [x] {R}完成\n"));
    }

    #[test]
    fn live_fence_streams_content_immediately() {
        let mut s = StyleStream::indented(0, p());
        let mut out = String::new();
        s.push_delta("```rust\nfn m", &mut out);
        // fence content chars land as they arrive, in the code color
        assert_eq!(out, format!("{G}```rust{R}\n{CB}  fn m"));
        s.push_delta("ain() {}\n```\nafter\n", &mut out);
        assert_eq!(
            out,
            format!("{G}```rust{R}\n{CB}  fn main() {{}}{R}\n{G}```{R}\nafter\n")
        );
    }

    #[test]
    fn live_fence_close_needs_the_full_line() {
        // ``` inside content (with text after) is content, not a close
        assert_eq!(
            live("```\n``x\n```\nend\n"),
            format!("{G}```{R}\n{CB}  ``x{R}\n{G}```{R}\nend\n")
        );
    }

    #[test]
    fn live_hr_decides_at_line_end() {
        assert_eq!(live("---\n"), format!("{G}{}{R}\n", "─".repeat(80)));
        // `--` (only two) is not a rule: literal
        assert_eq!(live("--\n"), "--\n");
        // `**bold**` at line start aborts the HR candidate and resolves
        assert_eq!(live("**注意**：\n"), format!("{B}注意{R}：\n"));
    }

    #[test]
    fn live_table_rows_pass_through_verbatim() {
        // the simple way: live and replay both pass `|` rows through
        let t = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        assert_eq!(live(t), t);
        let mut s = StyleStream::indented(2, p());
        s.wrap_terminal();
        let mut out = String::new();
        s.push_delta(t, &mut out);
        s.finish(&mut out);
        assert!(out.starts_with("  | a | b |\n"));
        assert!(!out.contains('\r')); // never any in-place rewrite
    }

    #[test]
    fn live_tables_without_separator_fall_back_verbatim() {
        let t = "| a | b |\n| 1 | 2 |\n";
        assert_eq!(live(t), t);
    }

    #[test]
    fn long_unclosed_marker_degrades_within_the_cap() {
        // a prose bracket or stray backtick must not hold the rest of the
        // line: past HOLD_CAP the marker streams literally and the scan
        // continues (a later closed span still renders)
        let xs = "x".repeat(100);
        let doc = format!("see [note {xs} and `code` too\n");
        assert_eq!(live(&doc), format!("see [note {xs} and {C}code{R} too\n"));
        // and it matches the one-shot resolver for never-closed markers
        assert_eq!(live(&doc), render(&doc));
    }

    #[test]
    fn live_bare_markers_at_eol_render_as_items() {
        assert_eq!(live("-\n"), format!("{C}- {R}\n"));
        assert_eq!(live("1.\n"), format!("{C}1. {R}\n"));
    }

    #[test]
    fn live_wraps_carry_margin_and_reopen_styles() {
        let mut s = StyleStream::indented(2, p());
        s.wrap = 10;
        let mut out = String::new();
        s.push_delta("aaaa bbbb cccc dddd\n", &mut out);
        assert_eq!(out, "  aaaa bbbb\n  cccc dddd\n");
        // an open heading style re-opens after a break
        s.push_delta("**aaaaaaaaaa bbbb**\n", &mut out);
        assert!(
            out.contains("aaaa\n  \x1b[1ma") || out.contains("\x1b[1maaaa"),
            "got {out:?}"
        );
    }

    #[test]
    fn live_cjk_never_straddles_the_wrap() {
        let mut s = StyleStream::indented(2, p());
        s.wrap = 4;
        let mut out = String::new();
        s.push_delta("中中中中\n", &mut out);
        assert_eq!(out, "  中中\n  中中\n");
    }

    #[test]
    fn live_finish_is_idempotent_and_write_once() {
        let mut s = StyleStream::indented(2, p());
        let mut out = String::new();
        s.push_delta("尾部", &mut out);
        assert!(s.finish(&mut out));
        assert_eq!(out, "  尾部\n");
        assert!(!s.finish(&mut out));
        assert!(!out.contains('\r') && !out.contains("\x1b[2K") && !out.contains("\x1b[1A"));
    }

    #[test]
    fn live_matches_itself_regardless_of_delta_boundaries() {
        let doc = "# T\n\npara `code` and **bold** text\n\n- item one\n- [x] done\n\n> quote line\n\n```rust\nlet x = 1;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        // char-by-char, word-by-word and whole-blob feeds agree byte for
        // byte: settlement depends only on line content, never on chunks
        let chars = live(doc);
        let mut words = StyleStream::indented(0, p());
        let mut wout = String::new();
        for w in doc.split_inclusive([' ', '\n']) {
            words.push_delta(w, &mut wout);
        }
        words.finish(&mut wout);
        assert_eq!(chars, wout);
        // no in-place erase/redraw ever
        assert!(!chars.contains('\r'));
        assert!(!chars.contains("\x1b[2K"));
        assert!(!chars.contains("\x1b[1A"));
        assert!(!chars.contains("\x1b[J"));
    }

    #[test]
    fn live_and_replay_agree_on_well_formed_markdown() {
        // single-blank-separated documents: the streamed answer and the
        // replayed transcript render byte-identically
        let doc = "# 标题\n\n段落 `code` 与 **加粗**。\n\n- 甲\n- [x] 乙\n\n> 引用\n\n---\n\n```rust\nfn x() {}\n```\n";
        assert_eq!(live(doc), render(doc));
    }

    #[test]
    fn live_tabs_count_real_cells() {
        let mut s = StyleStream::indented(2, p());
        s.wrap = 8;
        let mut out = String::new();
        s.push_delta("a\t\tbb\n", &mut out);
        assert_eq!(out, "  a\t\n  \tbb\n");
    }

    // ---- shared helpers -------------------------------------------------

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
        assert!(out.starts_with("  hi"), "got {out:?}");
        assert!(out.ends_with('\n'), "got {out:?}");
        assert!(!out.contains("\x1b[J"), "got {out:?}");
        assert!(!out.contains("\x1b[1A"), "got {out:?}");
        assert_eq!(render_once("", 0), "");
    }

    #[test]
    fn char_width_handles_zero_width_wide_and_emoji() {
        assert_eq!(char_width('\u{0301}'), 0);
        assert_eq!(char_width('\u{200D}'), 0);
        assert_eq!(char_width('\u{FE0F}'), 0);
        assert_eq!(char_width('中'), 2);
        assert_eq!(char_width('Ａ'), 2);
        assert_eq!(char_width('\u{1F600}'), 2);
        assert_eq!(char_width('\u{1F3AF}'), 2);
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('\u{FF9E}'), 1);
        assert_eq!(char_width('\u{00B7}'), 1);
    }

    #[test]
    fn cell_width_skips_every_escape_kind_without_losing_content() {
        assert_eq!(cell_width("\x1b[1Aab"), 2);
        assert_eq!(cell_width("a\x1b[2Kb"), 2);
        assert_eq!(cell_width("\x1b[31m中\x1b[0m"), 2);
        assert_eq!(cell_width("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"), 4);
    }
}
