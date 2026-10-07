//! Terminal markdown rendering, the reference-styled: one style vocabulary over one
//! engine. [`StyleStream`] renders live model output
//! character-immediately without ever redrawing: the "settled prefix" of
//! each line streams as it arrives, and only the currently open inline
//! marker (an unclosed `**`, `` ` ``, `~~` or `[`) is held back until it
//! resolves or the line ends — the one physical cost of write-once
//! styled output. Malformed syntax degrades to the original text rather
//! than erroring. Tables pass through verbatim, live and replay alike
//! (the simple way: column widths need the whole table, and write-once
//! output cannot restyle what is already on screen). Session replay
//! (`render_once`) runs the same engine over a whole stored answer, with
//! the one lookahead the live path cannot afford resolved up front:
//! `resolve_setext` turns a paragraph line under a `===`/`---` run into a
//! heading before the engine ever sees it, because a stream has already
//! handed that line to the terminal and never erases.

use crate::theme::Palette;
use unicode_width::UnicodeWidthChar;

/// A space this close to the right edge is taken as the break point: the
/// next word would straddle the edge anyway.
const WRAP_EARLY: usize = 3;

/// A horizontal rule caps at 80 cells.
const HR_MAX: usize = 80;

/// An open inline marker waits for its closing run while the span it holds
/// is no longer than this many display cells, then degrades to literal text
/// so that a prose `[`, a stray backtick or a very long unclosed span cannot
/// stall the stream until the line ends. The bound is in cells, not bytes: a
/// CJK character is three bytes but one cell, and a byte bound handed a
/// Chinese sentence a third of the patience an English one got — a long
/// `**…**` streamed with its asterisks showing.
const HOLD_CAP: usize = 240;

/// Is the unsettled tail still short enough to keep waiting for its closing
/// run? Escape sequences cost no cells, so they never count against it.
fn within_hold(tail: &str) -> bool {
    cell_width(tail) <= HOLD_CAP
}

// ---------------------------------------------------------------------------
fn is_setext(t: &str) -> bool {
    let Some(c) = t.chars().next() else {
        return false;
    };
    (c == '=' || c == '-') && t.chars().all(|ch| ch == c)
}

/// The backtick run when a left-trimmed line is nothing but a run of at
/// least three backticks: a closing fence, or an opening one with no info
/// string. CommonMark lets a closing fence be *longer* than its opener, so
/// the run length is what identifies it, never an exact "```" prefix.
fn bare_fence_run(t: &str) -> Option<usize> {
    let run = t.bytes().take_while(|&b| b == b'`').count();
    (run >= 3 && t.len() == run).then_some(run)
}

/// Byte length of a line's leading whitespace run. Replay trims with
/// `str::trim`, so a full-width space (U+3000) or a no-break space counts as
/// marker padding there too, and the live stream must skip it just the same.
fn pad_run(t: &str) -> usize {
    t.chars()
        .take_while(|c| c.is_whitespace())
        .map(char::len_utf8)
        .sum()
}

/// The shared answer to every thematic-break question: `Some(markers)` when
/// the text is nothing but one repeated mark (`-`/`_`/`*`) and whitespace,
/// where `markers` is how many marks it holds; `None` as soon as any other
/// character appears. The four predicates below are all this one walk at
/// different thresholds — replay asks it of a whole line, the live stream of
/// the line so far — so the count is what decides, and the walk lives once.
/// Replay asks `str::trim`-style questions of the line, so any unicode space
/// counts as padding here too.
fn hr_markers(t: &str) -> Option<usize> {
    let mut mark: Option<char> = None;
    let mut count = 0usize;
    for c in t.chars() {
        if c.is_whitespace() {
            continue;
        }
        match mark {
            None if matches!(c, '-' | '_' | '*') => mark = Some(c),
            None => return None,
            Some(m) if m != c => return None,
            Some(_) => {}
        }
        count += 1;
    }
    (count > 0).then_some(count)
}

fn is_hr(t: &str) -> bool {
    // commonmark wants three or more *matching* markers: `-*-` is prose
    hr_markers(t).is_some_and(|n| n >= 3)
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

// ---------------------------------------------------------------------------
// Inline resolver — one decision layer, shared by the row emitters
// ---------------------------------------------------------------------------

/// The styled spans the resolver produces; each renderer maps a span to
/// palette codes its own way (`span_codes`).
#[derive(Clone, Copy)]
enum InlSpan {
    Code,
    Bold,
    Italic,
    BoldItalic,
    Strike,
    Link,
    LinkUrl,
}

/// One unit of resolved inline output: a literal character, a styled
/// run the emitter opens, prints and closes itself, or a link (its own
/// event so the renderer can wrap it in an OSC 8 hyperlink).
enum InlEvent<'a> {
    Lit(char),
    Styled(InlSpan, &'a str),
    /// markdown link: shown `text`, target `href`
    Link {
        text: &'a str,
        href: &'a str,
    },
}

/// SGR codes for a span (strong = bold, em = italic, no hues).
fn span_codes(p: &Palette, span: InlSpan) -> String {
    match span {
        InlSpan::Code => p.code.clone(),
        InlSpan::Bold => p.bold.clone(),
        InlSpan::Italic => p.italic.clone(),
        InlSpan::BoldItalic => format!("{}{}", p.bold, p.italic),
        InlSpan::Strike => p.strike.clone(),
        InlSpan::Link => format!("{}{}", p.link, p.underline),
        InlSpan::LinkUrl => p.link_url.clone(),
    }
}

/// The inline decision layer: bold/italic emphasis, strike, code spans and
/// links (`_` is never an emphasis marker), scanned from `at` and handed to
/// `emit` — the one-shot replay resolver and the streaming scanner share
/// these matching rules verbatim, so the two cannot drift. `eol` says the
/// line is complete: mid-line an unclosed marker holds — the scan stops
/// before it and returns that position so the caller resumes once more text
/// settles — while at end of line the same marker flushes literally, which
/// is all the one-shot resolver ever does. Returns the stop position.
fn scan_inline_events(
    line: &str,
    at: usize,
    eol: bool,
    emit: &mut impl FnMut(InlEvent<'_>),
) -> usize {
    let mut i = at;
    while i < line.len() {
        match line.as_bytes()[i] {
            b'`' => {
                let run = line[i..].chars().take_while(|&c| c == '`').count();
                if let Some(rel) = line[i + run..].find(&"`".repeat(run)) {
                    let content = &line[i + run..i + run + rel];
                    if !content.is_empty() {
                        emit(InlEvent::Styled(InlSpan::Code, content));
                        i += run + rel + run;
                        continue;
                    }
                }
                if !eol && within_hold(&line[i..]) {
                    return i;
                }
                for _ in 0..run {
                    emit(InlEvent::Lit('`'));
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
                        let span = match run {
                            3 => InlSpan::BoldItalic,
                            2 => InlSpan::Bold,
                            _ => InlSpan::Italic,
                        };
                        emit(InlEvent::Styled(span, content));
                        i += run + rel + run;
                        continue;
                    }
                }
                if !eol && within_hold(&line[i..]) {
                    return i;
                }
                for _ in 0..run {
                    emit(InlEvent::Lit('*'));
                }
                i += run;
            }
            b'~' => {
                let tilde_run = line[i..].chars().take_while(|&c| c == '~').count();
                if tilde_run < 2 {
                    // one `~` so far: a second may still arrive and open a
                    // strike span, so hold it until the next char shows up
                    if !eol && i + 1 >= line.len() {
                        return i;
                    }
                    emit(InlEvent::Lit('~'));
                    i += 1;
                    continue;
                }
                if let Some(rel) = line[i + 2..].find("~~") {
                    let content = &line[i + 2..i + 2 + rel];
                    if !content.is_empty() && !content.contains('~') {
                        emit(InlEvent::Styled(InlSpan::Strike, content));
                        i += 2 + rel + 2;
                        continue;
                    }
                }
                if !eol && within_hold(&line[i..]) {
                    return i;
                }
                for _ in 0..tilde_run.min(2) {
                    emit(InlEvent::Lit('~'));
                }
                i += tilde_run.min(2);
            }
            b'[' => {
                // this bracket's own `]`: a link only when `](url)` closes
                // — otherwise the `[` is settled literal text
                if let Some(rb) = line[i + 1..].find(']') {
                    let close = i + 1 + rb;
                    if close + 1 >= line.len() {
                        if !eol && within_hold(&line[i..]) {
                            return i; // ']' is the last char so far
                        }
                    } else if line.as_bytes()[close + 1] != b'(' {
                        emit(InlEvent::Lit('['));
                        i += 1;
                        continue;
                    } else if let Some(end) = line[close + 2..].find(')') {
                        let text = &line[i + 1..close];
                        let href = &line[close + 2..close + 2 + end];
                        if !text.is_empty() {
                            emit(InlEvent::Link { text, href });
                            i = close + 2 + end + 1;
                            continue;
                        }
                        emit(InlEvent::Lit('['));
                        i += 1;
                        continue;
                    } else if !eol && within_hold(&line[i..]) {
                        return i; // `](` seen, ')' pending
                    }
                } else if !eol && within_hold(&line[i..]) {
                    return i; // no ']' yet
                }
                emit(InlEvent::Lit('['));
                i += 1;
            }
            _ => {
                let ch = line[i..].chars().next().unwrap();
                emit(InlEvent::Lit(ch));
                i += ch.len_utf8();
            }
        }
    }
    i
}

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
    /// inside a heading's inline content: a whitespace run at the row's end
    /// is held (a heading trims it, every other block keeps it)
    heading: bool,
    /// the held whitespace run of a heading row
    hold: String,
    /// any non-blank row emitted yet: a blank run before it is dropped
    /// (replay's `started`, which suppresses leading blanks)
    started: bool,
    /// a source blank line seen and not yet printed: one blank survives
    /// between blocks (runs collapse) and it is flushed only when the next
    /// block starts, so a trailing blank never prints
    pending_blank: bool,
    /// row budget in terminal cells beyond the margin; 0 = never wrap
    wrap: usize,
    /// re-read the terminal width at every line start (resize-safe)
    dynamic: bool,
    /// cells printed on the current row
    cells: usize,
    /// absolute column the current row's `cells` count starts at (the margin,
    /// plus the continuation indent on a wrapped row): tabs advance to an
    /// 8-column stop of the *screen*, so a tab's width needs this column
    col0: usize,
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
            heading: false,
            hold: String::new(),
            started: false,
            pending_blank: false,
            wrap: 0,
            dynamic: false,
            cells: 0,
            col0: spaces,
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

    /// Replay mode: wrap at a fixed `width` for the whole text, so a stored
    /// answer renders the same however the window is sized later.
    pub fn wrap_at(&mut self, width: usize) {
        self.dynamic = false;
        self.wrap = width;
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
    /// Resolve a line that ended while still being classified: apply the
    /// EOL decision and print the row it implies.
    fn finish_classification(&mut self, out: &mut String) {
        let d = self.decide_eol();
        self.apply(d, out);
        // an empty heading (`##` on its own) printed nothing yet, but
        // replay emits its style codes: match that byte for byte, starting
        // the row so it carries the left margin like every other row
        if !self.line_printed && !self.open.is_empty() {
            if self.at_start {
                self.begin_row(out);
            } else {
                let codes = std::mem::take(&mut self.open);
                out.push_str(&codes);
                out.push_str(self.p.reset.as_str());
            }
            self.line_printed = true;
        }
        match self.st {
            St::Inline(at) => self.scan_inline(at, out, true),
            St::Table(at) => {
                let rest = self.line[at..].to_string();
                for ch in rest.chars() {
                    self.putc(ch, out);
                }
            }
            _ => {}
        }
    }

    fn line_end(&mut self, out: &mut String) {
        if self.in_fence {
            self.fence_line_end(out);
            return;
        }
        // a whitespace-only line is a blank line: no margin, no content
        if !self.line_printed && self.line.trim().is_empty() {
            // hold it for the next block instead of printing a row here
            self.pending_blank = self.started;
            self.reset_line();
            return;
        }
        if self.pending_blank {
            // a row that prints no char of its own (an empty heading) still
            // needs the separating blank before it
            out.push('\n');
            self.pending_blank = false;
        }
        match self.st {
            St::FenceOpen => {
                let line = self.line.clone();
                // an opening fence may be indented (inside a list item);
                // the border itself prints at the margin, like replay
                let indent = line.len() - line.trim_start().len();
                let run = line[indent..].bytes().take_while(|&b| b == b'`').count();
                let info = line[indent + run..].trim().to_string();
                let p = self.p;
                self.span_open(&p.code_border, out);
                for _ in 0..3 {
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
                // not a rule after all: nothing was printed for the held
                // line, so resolve it the way any other line ending in the
                // classifier is resolved — replay decides at EOL, and a
                // `- -）` is a list item whose content opens with a dash
                self.finish_classification(out);
            }
            St::Classify => self.finish_classification(out),
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
        self.started = true;
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
        self.heading = false;
        self.hold.clear();
        self.open.clear();
        self.ctx.clear();
        self.st = St::Classify;
        self.fence_flushed = 0;
        self.fence_indented = false;
        self.marker_open = false;
    }

    // ---- fenced code ---------------------------------------------------

    /// One content char inside a fence: everything streams in the code
    /// color under a two-space indent. A line that so far is only indent and
    /// backticks is held whole — it may still turn out to be the closing
    /// fence, which must not print as content (the indent is held with it,
    /// so a fence nested in a list item leaves no stray spaces behind).
    fn fence_feed(&mut self, out: &mut String) {
        if self.line.trim_start().chars().all(|c| c == '`') {
            return; // still only indent + backticks: hold
        }
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

    fn fence_line_end(&mut self, out: &mut String) {
        // the fence body keeps its own leading whitespace, so skip the
        // indent before looking for the closing run: an indented fence (the
        // common case inside a list item) closes like any other
        let indent = pad_run(&self.line);
        let line = &self.line[indent..];
        if bare_fence_run(line).is_some() {
            // closing fence: a border line, then leave fence mode (the
            // content ctx must not layer under the border color). Replay
            // prints three backticks whatever the run length: match it.
            self.ctx.clear();
            self.open.clear();
            let p = self.p;
            self.span_open(&p.code_border, out);
            for _ in 0..3 {
                self.putc('`', out);
            }
            self.span_close(out);
            self.in_fence = false;
            self.end_row(out);
            return;
        }
        // content line: flush anything still held (a whitespace-only line
        // prints nothing, like replay's blank inside a fence)
        if !self.line.trim().is_empty() {
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
        let i = pad_run(&self.line);
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
                    // replay trims the run after the hashes: start the content
                    // there, and wait while only padding has arrived so it
                    // never streams as content
                    let skip = pad_run(&self.line[i + hashes..]);
                    if skip == t.len() - hashes {
                        Decision::Wait
                    } else {
                        Decision::Heading {
                            level: hashes,
                            at: i + hashes + skip,
                        }
                    }
                } else {
                    Decision::Inline { at: i }
                }
            }
            b'>' => {
                // replay always starts the quote body at its first non-space
                // char (`>` alone is an empty quote, `>text` has no padding to
                // skip), and waits while nothing but padding has arrived
                let skip = pad_run(&self.line[i + 1..]);
                if skip + 1 == t.len() {
                    Decision::Wait
                } else {
                    Decision::Quote { at: i + 1 + skip }
                }
            }
            b'|' => Decision::Table,
            b'-' | b'*' | b'+' => {
                if t.len() == 1 {
                    Decision::Wait
                } else if t[1] == b' ' {
                    // a list marker — but hr-only content (`- - -`) still
                    // makes the whole line a thematic break: hold as Hr
                    if is_hr_candidate(&self.line[i..]) {
                        Decision::Hr
                    } else {
                        self.decide_list(i, t, 2)
                    }
                } else if t[0] != b'+' && is_hr_candidate(&self.line[i..]) {
                    // `--`/`**`/`-*-`-shaped: an hr candidate (aborts to
                    // inline as soon as a non-marker char arrives)
                    Decision::Hr
                } else if t[0] != b'+' && may_be_hr(&self.line[i..]) {
                    // one mark and padding so far (`-`, then a tab): still
                    // a prefix of a rule, so hold it — replay decides at the
                    // end of the line and prints a rule for `-<tab>--`
                    Decision::Wait
                } else {
                    Decision::Inline { at: i }
                }
            }
            b'_' => {
                if t.len() == 1 {
                    Decision::Wait
                } else if is_hr_candidate(&self.line[i..]) {
                    Decision::Hr
                } else if may_be_hr(&self.line[i..]) {
                    // one mark and padding so far (`_` then a tab): hold
                    Decision::Wait
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
        let i = pad_run(&self.line);
        // trailing whitespace never streams: replay trims it away, and its
        // `str::trim` counts a full-width space (U+3000) as whitespace too
        let end = i + self.line[i..].trim_end().len();
        let t = &b[i..end];
        if t.is_empty() {
            return Decision::Inline { at: i };
        }
        // a bare run of `#` is an empty heading and a bare `>` an empty
        // quote: replay renders both, so the live stream does too
        let hashes = t.iter().take_while(|&&c| c == b'#').count();
        if hashes == t.len() && hashes <= 6 {
            return Decision::Heading {
                level: hashes,
                at: b.len(),
            };
        }
        if t == b">" {
            return Decision::Quote { at: b.len() };
        }
        // replay asks `list_marker` of the whole line: a marker whose item
        // never settled (a `[` opening no task box, a line ending right
        // after the marker) is still an item, its content starting after the
        // marker's padding. `-\t` is prose to replay, so this is exact.
        if let Some((marker_len, _ordered)) = list_marker(&self.line[i..]) {
            return Decision::List {
                indent: i,
                at: i + marker_len + pad_run(&self.line[i + marker_len..]),
            };
        }
        Decision::Inline { at: i }
    }

    /// An unordered/ordered marker was seen; a `[` directly after it may
    /// open a task checkbox (`- [x] `), so the marker holds until the
    /// bracket resolves.
    fn decide_list(&self, indent: usize, t: &[u8], marker_end: usize) -> Decision {
        // replay trims the whitespace between the marker and its content
        // (`str::trim_start`, so a full-width space counts) and only then
        // looks for a task checkbox — and it waits while nothing but padding
        // has arrived, so the padding never streams as content
        let skip = pad_run(&self.line[indent + marker_end..]);
        if skip == t.len() - marker_end {
            return Decision::Wait;
        }
        let at = indent + marker_end + skip;
        let rest = &self.line[at..];
        if rest.starts_with('[') {
            let Some(rb) = rest.find(']') else {
                return Decision::Wait; // no ']' yet
            };
            if rest.len() == rb + 1 {
                return Decision::Wait; // the char after ']' has not arrived
            }
            let inner = &rest[1..rb];
            if matches!(inner, "x" | "X" | " ") && rest.as_bytes()[rb + 1] == b' ' {
                return Decision::List {
                    indent,
                    at: at + rb + 2,
                };
            }
        }
        Decision::List { indent, at }
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
                    // the `### ` prefix stays visible for deep headings
                    for _ in 0..level {
                        self.putc('#', out);
                    }
                    self.putc(' ', out);
                }
                self.heading = true;
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
                // four spaces per level; two source spaces nest
                let level = (1 + indent / 2).min(3);
                // replay scans the nesting lead and the marker as part of the
                // item's text under the item's continuation budget, so the
                // budget must be in place before the lead is written
                let cont = 4 * (level - 1);
                // replay spells the marker `-` (whatever bullet was typed),
                // or the ordinal, plus one space plus any task box — never the
                // raw run between marker and content (`-   x`, `-\tx` and
                // `-\u{3000}x` all look alike there)
                let line = &self.line[indent..];
                let (marker_len, ordered) = list_marker(line).unwrap_or((1, false));
                let display = if ordered { &line[..marker_len] } else { "-" };
                let content = &line[marker_len + pad_run(&line[marker_len..])..];
                let task = if content.starts_with("[x] ")
                    || content.starts_with("[X] ")
                    || content.starts_with("[ ] ")
                {
                    &content[..4]
                } else {
                    ""
                };
                let marker = format!("{display} {task}");
                self.cont = cell_width(&marker) + cont;
                for _ in 1..level {
                    for _ in 0..4 {
                        self.putc(' ', out);
                    }
                }
                self.span_open(&p.bullet, out);
                for ch in marker.chars() {
                    self.putc(ch, out);
                }
                self.span_close(out);
                self.st = St::Inline(at);
            }
        }
    }

    // ---- inline resolver (streaming emitter over the shared rules) -----

    /// Resolve inline markup from `at` while it is settled: the matching
    /// rules live in `scan_inline_events`, shared with the one-shot
    /// resolver; this side only routes events through the row emitter
    /// (`putc`, so wrapping and heading holds apply). An unclosed marker
    /// stops the scan until more text settles or the line ends.
    fn scan_inline(&mut self, at: usize, out: &mut String, eol: bool) {
        // the scan reads the line while `putc` mutates the stream: move it
        // out for the duration instead of cloning it on every delta
        let line = std::mem::take(&mut self.line);
        let p = self.p;
        let i = scan_inline_events(&line, at, eol, &mut |ev| match ev {
            InlEvent::Lit(ch) => self.putc(ch, out),
            InlEvent::Styled(span, content) => {
                self.span_open(&span_codes(p, span), out);
                for ch in content.chars() {
                    self.putc(ch, out);
                }
                self.span_close(out);
            }
            InlEvent::Link { text, href } => {
                // an OSC 8 hyperlink: the opener rides like a span-open (so
                // it re-opens after a row break, which is invisible and
                // correct), the link style paints the text, and the shown
                // URL follows in parens when it differs from the target
                let open = crate::theme::osc8_open(href);
                self.span_open(&format!("{}{}", open, span_codes(p, InlSpan::Link)), out);
                for ch in text.chars() {
                    self.putc(ch, out);
                }
                if text != href {
                    // close the link span first — the URL rides in its own
                    // color, the same bytes the two-span form always printed
                    self.span_close(out);
                    self.span_open(&span_codes(p, InlSpan::LinkUrl), out);
                    self.putc(' ', out);
                    for ch in format!("({href})").chars() {
                        self.putc(ch, out);
                    }
                }
                // close: end the open span, end the hyperlink, restore ctx
                self.span_close(out);
                let close = crate::theme::osc8_close();
                if !close.is_empty() && !self.at_start {
                    out.push_str(close);
                }
            }
        });
        self.st = St::Inline(i);
        // stopped early = a marker still holds its span
        self.marker_open = i < line.len();
        self.line = line;
    }

    // ---- row emitter ----------------------------------------------------

    /// Write one character: lazy row start (margin + open codes), tab
    /// stops from the absolute column, hard wrap with the continuation
    /// indent, early break at an edge space (consumed, not printed).
    fn putc(&mut self, c: char, out: &mut String) {
        // held whitespace goes first, re-decided as if it had never been
        // held: only a heading holds, and only a non-whitespace char can end
        // the hold (the end of the line drops it instead)
        if !c.is_whitespace() {
            self.replay_hold(out);
        }
        self.emit(c, out);
    }

    /// Re-decide the heading whitespace held so far, in order. A held char
    /// advances no column, so nothing can have broken the row in the
    /// meantime: this reproduces exactly what streaming the run would have
    /// done, except that the run is never held again.
    fn replay_hold(&mut self, out: &mut String) {
        if self.hold.is_empty() {
            return;
        }
        let held = std::mem::take(&mut self.hold);
        let heading = std::mem::replace(&mut self.heading, false);
        for c in held.chars() {
            self.emit(c, out);
        }
        self.heading = heading;
    }

    /// Write one character: lazy row start (margin + open codes), tab
    /// stops from the absolute column, hard wrap with the continuation
    /// indent, early break at an edge space (consumed, not printed).
    fn emit(&mut self, c: char, out: &mut String) {
        if self.heading && c.is_whitespace() {
            // a heading trims its trailing whitespace, but whether this run
            // is the line's tail is only known when the next char (or the
            // line end) arrives: hold it, and decide the wrap rules for it in
            // `replay_hold` if content follows
            self.hold.push(c);
            return;
        }
        // the lazy row start comes first: a tab's advance is measured from
        // the column it lands at, which for the first char of a row is the
        // margin, never where the previous row happened to end
        if self.at_start {
            self.begin_row(out);
        }
        let mut w = pad_width(c, self.col0 + self.cells);
        if self.wrap > 0 {
            let budget = self.wrap.saturating_sub(self.cont);
            if c == ' ' && self.cells > 0 && self.cells + 1 > budget.saturating_sub(WRAP_EARLY) {
                self.break_row(out);
                return;
            }
            // over the edge, or an opening mark that would leave the row no
            // room for what it opens (禁则処理): the scanner wraps by this
            // same rule, so a wrapped answer replays byte for byte
            if self.cells > 0
                && (self.cells + w > budget || (moves_down(c) && self.cells + w >= budget))
            {
                self.break_row(out);
                // a tab's advance depends on where it lands: recompute from
                // the fresh row start
                w = pad_width(c, self.col0);
            }
        }
        out.push(c);
        self.cells += w;
        self.line_printed = true;
    }

    fn begin_row(&mut self, out: &mut String) {
        // the blank row that separates this block from the last one is
        // printed here, not when it arrived: replay collapses a blank run
        // into one row and drops leading and trailing blanks
        if self.pending_blank {
            out.push('\n');
            self.pending_blank = false;
        }
        out.push_str(&self.margin);
        self.col0 = self.margin_cells;
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
        self.col0 = self.margin_cells + self.cont;
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
        // held whitespace is content on this row: it belongs before the span
        // that follows it
        self.replay_hold(out);
        if !self.at_start {
            out.push_str(codes);
        }
        self.open.push_str(codes);
    }

    /// Close the current span: reset, then re-open the line ctx so later
    /// text keeps the heading/quote/fence style.
    fn span_close(&mut self, out: &mut String) {
        self.replay_hold(out);
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

/// The live stream's early decision: two or more *matching* markers and
/// nothing else. Two is enough here because the run may still grow (`**`
/// becomes `***`); the decision aborts as soon as a real char arrives.
fn is_hr_candidate(t: &str) -> bool {
    hr_markers(t).is_some_and(|n| n >= 2)
}

/// The prefix may still *become* a rule (replay decides on the whole line):
/// an open run of a single mark, which more marks of that kind can extend, or
/// a run whose padding may still be followed by marks.
fn may_be_hr(t: &str) -> bool {
    hr_markers(t).is_some()
}

// ---------------------------------------------------------------------------
// shared wrapping / measuring
// ---------------------------------------------------------------------------

/// The one wrap scanner: writes `text` into `out` hard-wrapped at `width`
/// terminal cells, preferring a break just after the last space that fit.
/// The first visual row starts with `first_prefix`, continuation rows with
/// `row_prefix`; with `ansi`, an open SGR span is tracked and re-opened
/// after each break.
/// An opening mark moves down with the text it opens: a row never ends with
/// `（` or `“`, which would strand it from what it quotes.
fn moves_down(c: char) -> bool {
    matches!(
        c,
        '（' | '「' | '『' | '【' | '《' | '〈' | '“' | '‘' | '(' | '[' | '{'
    )
}

/// Cells a char occupies when it lands on the column `col`: a tab advances
/// to the next 8-column stop, everything else is its own width. The live
/// stream counts tabs this way, so the scanner must too or a row holding a
/// tab wraps at a different column live and replay.
fn pad_width(c: char, col: usize) -> usize {
    if c == '\t' {
        8 - (col % 8)
    } else {
        char_width(c)
    }
}

/// One hard row break: newline, the continuation prefix, the still-open SGR
/// span re-opened.
fn row_break(out: &mut String, row_prefix: &str, ansi: bool, active: &str) {
    out.push('\n');
    out.push_str(row_prefix);
    if ansi {
        out.push_str(active);
    }
}

/// The one wrap scanner: writes `text` into `out`, hard-wrapped at `width`
/// terminal cells by exactly the rule the live stream applies char by char
/// ([`StyleStream::putc`]) — a space is taken as the break point only when
/// it lands within [`WRAP_EARLY`] cells of the edge (a space further in
/// fills the row instead: live cannot ask for printed bytes back), an
/// opening mark moves down rather than ending a row (禁则処理), and a row
/// never exceeds `width` (a wider row would soft-wrap and lose its margin).
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
    let mut prefix_cells = cell_width(first_prefix); // absolute column the row starts at
    let mut active = String::new(); // SGR sequences opened on this row
    let mut i = 0usize;
    while i < bytes.len() {
        if ansi && bytes[i] == 0x1b {
            // any escape sequence is invisible (SGR color, OSC 8
            // hyperlinks): ride whole and cost no cells. The SGR reset
            // clears the tracked span; anything else stays open, so a row
            // break re-issues it (an OSC 8 re-open is invisible and
            // correct — the link survives the wrap).
            let end = escape_end(bytes, i);
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
        let mut w = pad_width(c, prefix_cells + cells);
        if cells > 0 && c == ' ' && cells + 1 > width.saturating_sub(WRAP_EARLY) {
            // the space itself is the break, and it is consumed
            out.push_str(&text[seg_start..i]);
            i += 1;
            row_break(out, row_prefix, ansi, &active);
            prefix_cells = cell_width(row_prefix);
            cells = 0;
            seg_start = i;
            continue;
        }
        if cells > 0 && (cells + w > width || (moves_down(c) && cells + w >= width)) {
            out.push_str(&text[seg_start..i]);
            row_break(out, row_prefix, ansi, &active);
            prefix_cells = cell_width(row_prefix);
            // a tab's advance depends on where it lands: recompute from the
            // fresh row start
            w = pad_width(c, prefix_cells);
            cells = 0;
            seg_start = i;
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
    let mut s = StyleStream::indented(indent, crate::theme::out());
    s.wrap_at(crate::term::columns().saturating_sub(indent).max(20));
    let mut out = String::new();
    s.push_delta(&resolve_setext(text), &mut out);
    s.finish(&mut out);
    out
}

/// An ATX heading line: one to six `#`, then the end of the line or a space.
fn is_atx(t: &str) -> bool {
    let hashes = t.chars().take_while(|c| *c == '#').count();
    (1..=6).contains(&hashes) && (t.len() == hashes || t.as_bytes()[hashes] == b' ')
}

/// Replay-only pre-pass: a paragraph line followed by a `===`/`---` run is a
/// setext heading, and a line's shape is otherwise only known once the next
/// one arrives. The live path cannot wait for that lookahead — it has already
/// handed the line to the terminal and never erases — so replay resolves it
/// here, before the streaming engine sees the text, and one engine serves
/// both. The line shapes mirror `render_line`'s order: fence, blank, ATX,
/// quote, table and list lines are never the heading's text.
fn resolve_setext(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    // the last emitted plain line: (offset in `out`, its trimmed text) — a
    // setext underline under it turns that line into a heading instead
    let mut held: Option<(usize, String)> = None;
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let nl = if line.ends_with('\n') { "\n" } else { "" };
        let t = body.trim_start();
        let plain = if in_fence {
            if bare_fence_run(t).is_some() {
                in_fence = false;
            }
            false
        } else if t.is_empty() {
            false
        } else if t.starts_with("```") {
            in_fence = true;
            false
        } else if held.is_some() && is_setext(t) {
            let (at, para) = held.take().unwrap();
            let level = if t.starts_with('=') { 1 } else { 2 };
            out.truncate(at);
            out.push_str(&"#".repeat(level));
            out.push(' ');
            out.push_str(&para);
            out.push('\n');
            continue;
        } else {
            !is_hr(t)
                && !is_atx(t)
                && !t.starts_with('>')
                && !t.starts_with('|')
                && list_marker(t).is_none()
        };
        let at = out.len();
        out.push_str(body);
        out.push_str(nl);
        held = plain.then(|| (at, t.to_string()));
    }
    out
}

/// Truncate to at most `max` terminal cells (CJK-aware), "…" on cut — the
/// ellipsis is charged to the budget, so the result never overshoots the
/// column it was handed. Escape sequences ride along untouched and cost no
/// cells (see [`cell_width`]), and text that already fits comes back
/// verbatim. Rows cut this way occupy exactly one terminal line, ASCII and
/// wide CJK alike.
pub(crate) fn truncate_cells(text: &str, max: usize) -> String {
    if cell_width(text) <= max {
        return text.to_string();
    }
    let budget = max.saturating_sub(1);
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut cells = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            let end = escape_end(bytes, i);
            out.push_str(&text[i..end]);
            i = end;
            continue;
        }
        let c = text[i..].chars().next().unwrap();
        let w = char_width(c);
        if cells + w > budget {
            break;
        }
        out.push(c);
        cells += w;
        i += c.len_utf8();
    }
    out.push('…');
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
pub(crate) fn escape_end(bytes: &[u8], i: usize) -> usize {
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

/// Terminal cell width: delegated to [`unicode-width`] (the same source
/// used by several terminal agents), with the one emulator correction for U+FF9E/U+FF9F
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
mod tests;
