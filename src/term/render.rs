//! Output rendering: typewriter stream, reasoning buffering, usage
//! footer formatting, fenced-block extraction.

use std::io::{IsTerminal, Write};

use crate::core::http::Usage;

/// Streamed-output flush cadence: writing+flushing per delta costs two
/// syscalls per token on char-by-char streams; one frame per interval is
/// imperceptible next to network latency and bounds the syscalls. Chrome
/// paths flush first (TaskView::pause), so nothing interleaves mid-line.
/// ~250Hz keep-up: near-instant for local model streams; still amortizes the
/// per-frame syscalls (one write+flush each frame, no per-character write).
const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(4);

/// Typewriter cadence: one pacing tick every [`TICK_MILLIS`] — the pacer
/// thread polls at 8ms but grants are time-gated to this cadence, so the
/// feel is identical however often ticks fire.
const TICK_MILLIS: u64 = 16;

/// Characters per tick when caught up (2/16ms ≈ 125 chars/s): fast enough
/// to outrun reading, slow enough to read as a typewriter.
const BASE_CHARS: usize = 2;

/// Backlog acceleration: one extra character per tick for every
/// [`ACCEL_DIV`] characters waiting, so a burst plays out as fast typing
/// that decelerates into the readable base rate as the backlog clears —
/// the pen never lags far behind the stream.
const ACCEL_DIV: usize = 24;

/// Per-tick grant ceiling (~24 chars per 16ms ≈ 1500 chars/s while far
/// behind): no single frame may pop a visible chunk. This is the anti-dump
/// rule — a stall never buys a big installment, catch-up happens over
/// several accelerated ticks instead.
const MAX_GRANT: usize = 24;

/// How long the settle wait plays the paced backlog out before flushing the
/// rest. That wait runs on the *agent* thread (`turn_end` settles before the
/// next tool round starts, `pause` before chrome prints), so whatever it
/// costs is latency real work pays for a cosmetic effect — at the drain rate
/// a 10k-char backlog would stall the loop for ~7s, and the old cap let it
/// run to 30. Sized to let a typical answer tail finish typing; past it the
/// remainder is written at once, which changes only *when* bytes land, not
/// what lands (write-once holds either way).
const SETTLE_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

pub struct Renderer {
    /// accumulated visible output
    pub output: String,
    /// accumulated reasoning output
    pub reasoning: String,
    /// live styled streaming (terminal modes opt in via terminal_md): the
    /// answer renders the reference-style markdown inside a left-margin block,
    /// write-once (the settled prefix streams, open inline markers hold
    /// until they resolve), hard-wrapped at the terminal width so
    /// continuation rows keep the margin
    md: Option<crate::core::render_md::StyleStream>,
    /// bytes not yet written (frame batching)
    pending: String,
    /// a partial row is on screen without its terminating newline — the state
    /// a spinner frame's `\r\x1b[2K` would retract, so the spinner must not
    /// (re)start while it holds
    dangling: bool,
    last_flush: std::time::Instant,
    /// arrival-side queue: deltas land here and drain onto the screen at a
    /// steady typewriter cadence (base rate readable, accelerating with
    /// backlog, capped per tick) so a bursty upstream (whole paragraphs in
    /// one SSE chunk, seconds apart) reads as continuous typing — never a
    /// chunk pop, never stop-motion.
    backlog: String,
    /// true while the backlog is being played out (a paced batch is open)
    draining: bool,
    /// when the latest installment was granted (cadence gating)
    last_grant: std::time::Instant,
}

impl Default for Renderer {
    fn default() -> Self {
        Renderer::new()
    }
}

impl Renderer {
    pub fn new() -> Renderer {
        Renderer {
            output: String::new(),
            reasoning: String::new(),
            md: None,
            pending: String::new(),
            dangling: false,
            // start "already due" so the very first delta flushes immediately
            // instead of waiting one interval before anything appears
            last_flush: std::time::Instant::now()
                .checked_sub(FLUSH_INTERVAL)
                .unwrap_or_else(std::time::Instant::now),
            backlog: String::new(),
            draining: false,
            // pre-aged so the very first installment grants immediately
            last_grant: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_millis(TICK_MILLIS))
                .unwrap_or_else(std::time::Instant::now),
        }
    }

    /// Terminal live-stream mode, TTY-gated: pipes and quiet mode keep raw
    /// output. The answer streams styled with a left margin on each of the
    /// model's own lines; rows hard-wrap at the terminal width so wrapped
    /// continuation rows keep the margin too (the width is re-read at
    /// every line start, so a resize applies from the next row on).
    pub fn terminal_md(&mut self, indent: usize) -> bool {
        if !std::io::stdout().is_terminal() {
            return false;
        }
        let mut md = crate::core::render_md::StyleStream::indented(indent, crate::theme::out());
        md.wrap_terminal();
        self.md = Some(md);
        true
    }

    /// Append answer text, printing it (hard-wrapped inside the block when
    /// streaming). Output is written at most once per FLUSH_INTERVAL.
    ///
    /// Text first lands in the backlog; the backlog drains onto the screen
    /// at a steady row cadence (see [`Renderer::drain`]), so upstream burst
    /// size shapes nothing the eye can see.
    pub fn push_delta(&mut self, text: &str) {
        self.output.push_str(text);
        if let Some(md) = self.md.as_mut() {
            md.push_delta(text, &mut self.backlog);
            // pacing opens exactly when settled text is waiting; a stream
            // that keeps up drains each delta the moment it lands
            self.drain_tick();
        } else {
            // plain pipes stream verbatim: pacing is a tty affordance
            self.pending.push_str(text);
            if self.last_flush.elapsed() >= FLUSH_INTERVAL {
                self.flush_pending();
            }
        }
    }

    /// Print what is due: one time-gated typewriter installment per
    /// [`TICK_MILLIS`] while a batch is running — the common fast-upstream
    /// case never opens one and adds no pacing at all.
    fn drain_tick(&mut self) {
        if crate::term::screen()
            .flush_now
            .swap(false, std::sync::atomic::Ordering::Relaxed)
            && !self.backlog.is_empty()
        {
            // the user pressed a key mid-stream: they are watching — print
            // everything waiting at once. Write-once holds; pacing only
            // ever decided when bytes were written, never what is written.
            self.pending.push_str(&self.backlog);
            self.backlog.clear();
            self.draining = false;
        }
        let now = std::time::Instant::now();
        if !self.backlog.is_empty() {
            self.take_due_chars(now);
        }
        if self.last_flush.elapsed() >= FLUSH_INTERVAL {
            self.flush_pending();
        }
    }

    /// Heartbeat-side tick ([`crate::term::ticker::DrainTicker`]): grant due
    /// installments and flush the write buffer from a timer, so paced text
    /// keeps flowing while no delta arrives. True while the batch is open or
    /// `pending` still holds unprinted text — the pacer lives exactly that
    /// long.
    pub(crate) fn pump_due(&mut self) -> bool {
        self.drain_tick();
        self.draining || !self.pending.is_empty()
    }

    /// Move one typewriter installment of backlog characters into
    /// `pending`, cutting on a char boundary (never mid-UTF-8). The grant
    /// is time-gated to one per [`TICK_MILLIS``] and sized by the backlog:
    /// [`BASE_CHARS`] caught up, one more per [`ACCEL_DIV`] waiting, capped
    /// at [`MAX_GRANT`] — so bursts play out as fast-but-continuous typing
    /// and no frame ever dumps a chunk, however long the upstream stalled
    /// before it. Escape sequences (`\x1b[...m`) ride whole and cost no
    /// budget. Returns the visible count; an empty backlog closes the batch.
    fn take_due_chars(&mut self, now: std::time::Instant) -> usize {
        if self.backlog.is_empty() {
            self.draining = false;
            return 0;
        }
        self.draining = true;
        if now.duration_since(self.last_grant) < std::time::Duration::from_millis(TICK_MILLIS) {
            return 0; // cadence gate: installments land on the tick, not on
            // however often a tick happens to fire
        }
        let behind = self.backlog.chars().count();
        let budget = (BASE_CHARS + behind / ACCEL_DIV).min(MAX_GRANT);
        // never cut an escape sequence (`\x1b[...m`) in half: emit up to
        // and including it, the sequence itself is invisible on screen
        let mut end = 0usize;
        let mut n = 0usize;
        let bytes = self.backlog.as_bytes();
        while end < bytes.len() {
            let ch = self.backlog[end..].chars().next().unwrap();
            if ch == '\x1b' {
                // an SGR span is invisible on screen: it rides whole (never
                // split mid-sequence) and costs no budget
                end = crate::core::render_md::escape_end(bytes, end);
                continue;
            }
            if n >= budget {
                break;
            }
            n += 1;
            end += ch.len_utf8();
        }
        self.pending.push_str(&self.backlog[..end]);
        self.backlog.drain(..end);
        self.last_grant = now;
        if self.backlog.is_empty() {
            self.draining = false;
        }
        n
    }

    /// Streaming cadence: write everything that has arrived, the trailing
    /// partial row included — characters appear as they land, not row by row.
    /// A partial row left on screen (`dangling`) means the spinner must not
    /// (re)start: its `\r\x1b[2K` frame would retract those characters.
    fn flush_pending(&mut self) {
        if !self.pending.is_empty() {
            let out = std::mem::take(&mut self.pending);
            print!("{out}");
            self.dangling = !out.ends_with('\n');
            crate::term::screen()
                .dangling
                .store(self.dangling, std::sync::atomic::Ordering::Relaxed);
        }
        let _ = std::io::stdout().flush();
        self.last_flush = std::time::Instant::now();
    }

    /// Whether a partial row sits on screen without its terminating newline —
    /// the state a spinner frame's `\r\x1b[2K` would retract, so no spinner
    /// may (re)start while it holds. The model's own newline, a wrap at the
    /// right edge, or the chrome-boundary `finish_stream` clears it.
    pub fn has_dangling(&self) -> bool {
        self.dangling
    }

    /// Accumulate reasoning without printing it.
    pub fn push_reasoning_buffered(&mut self, text: &str) {
        self.reasoning.push_str(text);
    }
    /// Flush a pending partial line — terminating it first when dangling in
    /// terminal-markdown mode — so later rounds and chrome start on their own
    /// line (plain pipes flush verbatim, without the terminator). A paced
    /// backlog drains fully first: the smooth cadence must not leak stale
    /// text into later rounds or chrome.
    pub fn finish_stream(&mut self) {
        if let Some(md) = self.md.as_mut() {
            let mut chunk = String::new();
            if md.finish(&mut chunk) {
                self.backlog.push_str(&chunk);
            }
        }
        self.draining = false;
        self.pending.push_str(&self.backlog);
        self.backlog.clear();
        self.flush_pending();
    }
}

/// 138000 → "138k", 1200000 → "1.2M" (usage footer).
pub fn humanize_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{}k", n / 1000)
    } else {
        format!("{}.{}M", n / 1_000_000, n % 1_000_000 / 100_000)
    }
}

// TaskView: the agent-style task presentation shared by every mode

/// Spinner with phase relabel, a single dim `thinking ... end` trace line,
/// the answer stream through the [Renderer], and a dim `secs · this task:
/// input N · output N · cache N%` footer. `indent` is the chrome margin
/// (agent 2, prompt/chat 0); `live` false disables the spinner (quiet/JSON
/// modes).
pub struct TaskView {
    /// shared with the [`crate::term::ticker::DrainTicker`] heartbeat, which
    /// drains paced text from a timer thread while no delta arrives
    renderer: std::sync::Arc<std::sync::Mutex<Renderer>>,
    /// the heartbeat, alive only while a paced batch is open
    pacer: Option<crate::term::ticker::DrainTicker>,
    ticker: Option<crate::term::ticker::Ticker>,
    label: String,
    indent: usize,
    live: bool,
    show_trace: bool,
    streamed_any: bool,
    thinking_announced: bool,
    thinking_trace_shown: bool,
    total: Usage,
}

impl TaskView {
    pub fn new(indent: usize, label: &str, live: bool) -> TaskView {
        TaskView {
            renderer: std::sync::Arc::new(std::sync::Mutex::new(Renderer::new())),
            pacer: None,
            ticker: if live {
                Some(crate::term::ticker::Ticker::start(label))
            } else {
                None
            },
            label: label.to_string(),
            indent,
            live,
            show_trace: true,
            streamed_any: false,
            thinking_announced: false,
            thinking_trace_shown: false,
            total: Usage::default(),
        }
    }

    /// Opt the answer stream into terminal-markdown rendering (the
    /// tty-gated the reference-style path).
    pub fn terminal_md(&self, indent: usize) {
        self.renderer.lock().unwrap().terminal_md(indent);
    }

    /// Reclaim the renderer (accumulated output/reasoning/usage).
    pub fn into_renderer(mut self) -> Renderer {
        self.stop_pacer();
        match std::sync::Arc::try_unwrap(self.renderer) {
            Ok(m) => m.into_inner().unwrap(),
            Err(arc) => std::mem::take(&mut *arc.lock().unwrap()),
        }
    }

    /// -R: keep buffering reasoning but never print the trace line.
    fn stop_ticker(&mut self) {
        if let Some(mut t) = self.ticker.take() {
            t.stop();
        }
    }

    /// Start the typewriter heartbeat once a paced batch is open (never in
    /// plain-pipe mode, where pacing does not engage).
    fn ensure_pacer(&mut self) {
        if self.pacer.is_none() && self.renderer.lock().unwrap().draining {
            self.pacer = Some(crate::term::ticker::DrainTicker::start(
                std::sync::Arc::clone(&self.renderer),
            ));
        }
    }

    fn stop_pacer(&mut self) {
        if let Some(mut p) = self.pacer.take() {
            p.stop();
        }
    }

    /// Type the paced backlog out and stop the heartbeat: chrome (tool
    /// result, next round, footer) must never land over untyped text. Bounded
    /// by [`SETTLE_GRACE`] — the wait is on the agent thread, so a long tail
    /// must not hold the next round hostage; whatever remains is flushed by
    /// `finish_stream`.
    fn settle(&mut self) {
        let deadline = std::time::Instant::now() + SETTLE_GRACE;
        loop {
            let more = self.renderer.lock().unwrap().pump_due();
            if !more || std::time::Instant::now() > deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        self.stop_pacer();
        self.renderer.lock().unwrap().finish_stream();
    }

    fn relabel(&mut self, label: &str) {
        // hot-swap the label when a ticker is already running; only a true
        // restart after a pause may reset the clock
        if let Some(ticker) = &self.ticker {
            ticker.set_phase(label);
            return;
        }
        // never start a spinner over a dangling answer row: the frame's
        // `\r\x1b[2K` would retract live text, and settling (a newline)
        // would break the stream mid-word — the streaming text itself
        // already shows liveness
        if self.live && !self.renderer.lock().unwrap().has_dangling() {
            self.ticker = Some(crate::term::ticker::Ticker::start(label));
        }
    }

    /// Stop the spinner: once a tool's output starts streaming, the spinner
    /// frame would collide with the lines being printed on the same row, so
    /// it is dropped for the remainder of the tool.
    pub fn spin_pause(&mut self) {
        self.stop_ticker();
    }

    /// One trace line per task, printed before whatever follows the thinking.
    /// Rounds that end in pure tool calls emit no text deltas, so this is
    /// also where the thinking spinner must be silenced.
    fn close_thinking(&mut self) {
        self.stop_ticker();
        if self.thinking_announced {
            // the latch stays: interleaved reasoning bursts (thinking,
            // partial text, more thinking) must not re-announce the spinner
            // mid-answer; turn_end resets it for the next round
            if !self.thinking_trace_shown {
                self.thinking_trace_shown = true;
                if self.show_trace {
                    let pad = " ".repeat(self.indent);
                    let p = crate::theme::err();
                    eprintln!("{}{pad}thinking ... end{}", p.gray, p.reset);
                }
            }
        }
    }

    /// Print the steer watcher's deferred `queued:` notices: the watcher
    /// defers when the answer owns the current row; here the row can be
    /// settled first, so the notice lands on its own line instead of
    /// tearing the streamed text apart.
    fn flush_notices(&mut self) {
        let notices: Vec<String> = match crate::term::screen().notices.lock() {
            Ok(mut n) => std::mem::take(&mut *n),
            Err(_) => return,
        };
        if notices.is_empty() {
            return;
        }
        self.pause();
        for line in notices {
            eprintln!("{}", crate::theme::edim(&line));
        }
    }

    pub fn delta(&mut self, text: &str) {
        // providers emit empty content deltas between thinking bursts;
        // treating one as "the answer started" killed the spinner and reset
        // the thinking trace mid-round
        if text.is_empty() {
            return;
        }
        self.flush_notices();
        self.stop_ticker();
        self.close_thinking();
        self.streamed_any = true;
        let open = {
            let mut r = self.renderer.lock().unwrap();
            r.push_delta(text);
            r.draining
        };
        if open {
            self.ensure_pacer();
        }
    }

    /// Reasoning is never streamed: buffer it and relabel the spinner once.
    /// Interleaved reasoning bursts must not touch the visible answer: no
    /// newline, no spinner start while a partial row is live (`relabel`
    /// guards the start; the label swap on a running ticker is row-safe).
    pub fn reasoning_delta(&mut self, text: &str) {
        self.renderer.lock().unwrap().push_reasoning_buffered(text);
        if !self.thinking_announced {
            self.thinking_announced = true;
            self.relabel("thinking ...");
        }
    }

    /// Chrome is about to print (tool result, approval, compaction):
    /// settle the streaming partial line so chrome never lands mid-line,
    /// silence the spinner and close any pending thinking trace.
    pub fn pause(&mut self) {
        self.stop_ticker();
        self.settle();
        self.close_thinking();
    }

    /// Live label while a tool call's arguments stream in (hot-swap). The
    /// answer may have left a partial row on screen when the model switched
    /// from text to tool arguments: no spinner may start over it (the tool
    /// chrome that follows settles the row through `pause` first).
    pub fn receiving(&mut self, label: &str) {
        self.relabel(label);
    }

    /// A tool started: the `$` chrome line follows; spinner restarts labelled.
    pub fn tool_started(&mut self) {
        self.pause();
        // the spinner is restarted by `resume_running` AFTER the caller has
        // printed the `$ <verb> <cmd>` chrome line, so the spinner's
        // in-place frame cannot collide with that line
    }

    /// Restart the spinner with a plain "running" phase after chrome output
    /// has been printed (see `tool_started`).
    pub fn resume_running(&mut self) {
        if self.live && self.ticker.is_none() {
            self.ticker = Some(crate::term::ticker::Ticker::start("running"));
        }
    }

    /// The tool chrome is done; spin again while the next model round is
    /// awaited, so the time-to-first-token is not a silent dead window.
    pub fn resume_wait(&mut self) {
        self.relabel(&self.label.clone());
    }

    /// A model round ended: accumulate usage, close the trace, terminate a
    /// partial markdown line so the next chrome row starts on its own line.
    pub fn turn_end(&mut self, usage: Option<Usage>) {
        self.flush_notices();
        if let Some(u) = usage {
            self.total.add(u);
        }
        self.close_thinking();
        self.thinking_announced = false; // next round may announce again
        self.settle();
        // a wait spinner already up keeps ticking under its new-ish label;
        // starting one here is wrong both ways: when the round was the whole
        // task (no tool follows) it would blink a frame under the answer that
        // just settled — a redraw of printed text — and when a tool does
        // follow, `ToolEnd`/`resume_wait` starts the next wait itself
        if self.ticker.is_some() {
            self.relabel(&self.label.clone());
        }
    }

    /// Cleanup without the footer (provider error, interrupt).
    pub fn abort(&mut self) {
        self.stop_ticker();
        self.stop_pacer();
        // do-not print the "thinking ... end" trace on an abnormal stop:
        // a force-interrupt must not read as thinking having finished
        self.renderer.lock().unwrap().finish_stream();
    }

    /// The `secs · this task: input N · output N · cache N%` line, right
    /// before the prompt returns. Every field is this task's own: every round
    /// re-sends the whole prompt, so the input is what the run summed across
    /// its rounds, never a context size. Session-lifetime counters and the
    /// window's occupancy live in `/status`, so the two are never read as the
    /// same number. When the provider reports cache hits, the cached share of
    /// the input rides along so prefix-cache health is visible at a glance.
    pub fn footer(&mut self, secs: f64) {
        self.stop_ticker();
        self.stop_pacer();
        if self.streamed_any {
            println!();
        }
        let p = crate::theme::err();
        let pad = " ".repeat(self.indent);
        let total = self.total;
        if total.input > 0 || total.output > 0 {
            let cache = if total.cached > 0 {
                format!(" · cache {}%", total.cached * 100 / total.input.max(1))
            } else {
                String::new()
            };
            eprintln!(
                "{}{pad}{secs:.1}s · this task: input {} · output {}{cache}{}",
                p.gray,
                humanize_tokens(total.input),
                humanize_tokens(total.output),
                p.reset
            );
        } else {
            eprintln!("{}{pad}{secs:.1}s{}", p.gray, p.reset);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A burst paces: the first tick drains exactly one installment
    /// (base rate plus backlog acceleration) and the rest waits in the
    /// backlog, played out tick by tick (the reference-style typewriter).
    #[test]
    fn burst_backlog_paces_char_by_char() {
        let mut r = Renderer::new();
        let text: String = (0..20).map(|i| format!("row {i}\n")).collect();
        // the styled path feeds the backlog (the plain path streams pipes
        // verbatim; terminal_md is TTY-gated), so seed it directly
        r.backlog = text.clone();
        r.output = text.clone();
        r.drain_tick();
        let behind = text.chars().count();
        let drained = behind - r.backlog.chars().count();
        assert_eq!(
            drained,
            (BASE_CHARS + behind / ACCEL_DIV).min(MAX_GRANT),
            "one tick drains exactly one installment"
        );
        assert_eq!(r.output, text, "the full text is always accumulated");
        // repeated ticks drain everything; finish_stream settles the rest
        while !r.backlog.is_empty() {
            r.drain_tick();
            std::thread::sleep(std::time::Duration::from_millis(TICK_MILLIS + 1));
        }
        r.finish_stream();
        assert!(r.pending.is_empty() && r.backlog.is_empty());
    }

    /// The settle wait is on the agent thread, so it must stay bounded: a
    /// giant backlog is flushed rather than typed out at cosmetic cadence.
    /// 20k chars would take ~15s at the drain rate; the grace caps it.
    #[test]
    fn settle_never_holds_the_agent_thread() {
        // `live: false` keeps a spinner thread out of the test
        let mut v = TaskView::new(0, "", false);
        let text: String = "x".repeat(20_000);
        {
            let mut r = v.renderer.lock().unwrap();
            r.backlog = text.clone();
            r.output = text.clone();
        }
        let started = std::time::Instant::now();
        v.settle();
        let waited = started.elapsed();
        assert!(
            waited < SETTLE_GRACE + std::time::Duration::from_millis(600),
            "settle must not type a 20k backlog out: waited {waited:?}"
        );
        // write-once still holds: nothing is lost, the rest lands at once
        let r = v.into_renderer();
        assert!(r.backlog.is_empty(), "the remainder is flushed");
        assert_eq!(r.output, text, "every byte is still accumulated");
    }

    /// An escape sequence is never cut in half by the char budget: the
    /// whole sequence rides with the tick that touches it.
    #[test]
    fn escape_sequences_never_split() {
        const BOLD: &str = "\x1b[1m";
        let mut r = Renderer::new();
        let text = format!("a{BOLD}b");
        r.backlog = text.clone();
        r.pending.clear();
        let printed = r.take_due_chars(std::time::Instant::now());
        assert!(
            printed <= BASE_CHARS,
            "one tick prints its budget, got {printed}"
        );
        assert!(r.pending.starts_with('a'), "visible chars lead the budget");
        // no split sequence in whatever was printed: escape_end of the last
        // ESC lands exactly at the end of the printed prefix
        if let Some(pos) = r.pending.find('\x1b') {
            let e = crate::core::render_md::escape_end(r.pending.as_bytes(), pos);
            assert!(
                e <= r.pending.len(),
                "an escape sequence is never cut in half"
            );
        }
        // the rest of the backlog settles with no split sequences either
        while !r.backlog.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(TICK_MILLIS + 1));
            r.take_due_chars(std::time::Instant::now());
        }
        assert_eq!(r.pending, text, "the drained pending holds the full text");
        r.finish_stream();
        assert!(
            r.pending.is_empty(),
            "finish_stream prints and clears pending"
        );
    }

    /// A stall never dumps: however long the upstream was silent before a
    /// burst, one tick pops at most MAX_GRANT characters — catch-up happens
    /// over several accelerated ticks, visible as fast typing, not a chunk.
    #[test]
    fn a_stall_never_dumps_a_chunk() {
        let mut r = Renderer::new();
        r.backlog = "x".repeat(400);
        std::thread::sleep(std::time::Duration::from_millis(80));
        let printed = r.take_due_chars(std::time::Instant::now());
        assert!(
            printed <= MAX_GRANT,
            "one tick pops at most {MAX_GRANT}, got {printed}"
        );
        let mut ticks = 0;
        while !r.backlog.is_empty() && ticks < 500 {
            std::thread::sleep(std::time::Duration::from_millis(TICK_MILLIS + 1));
            let printed = r.take_due_chars(std::time::Instant::now());
            assert!(printed <= MAX_GRANT, "no tick may dump: {printed}");
            ticks += 1;
        }
        assert!(
            r.backlog.is_empty(),
            "the backlog drains fully ({ticks} ticks)"
        );
    }

    /// Backlog acceleration is capped: even thousands of waiting characters
    /// never buy a dump — every tick stays at or under MAX_GRANT.
    #[test]
    fn huge_backlog_catches_up_without_ever_dumping() {
        let mut r = Renderer::new();
        r.backlog = "y".repeat(5000);
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(TICK_MILLIS + 1));
            let printed = r.take_due_chars(std::time::Instant::now());
            assert!(printed <= MAX_GRANT, "capped grant exceeded: {printed}");
        }
        assert!(!r.backlog.is_empty(), "5000 chars cannot land in 10 ticks");
    }

    /// A stream that keeps up never paces: the backlog never grows, every
    /// delta is settled the moment it lands.
    #[test]
    fn caught_up_stream_never_holds_backlog() {
        let mut r = Renderer::new();
        for i in 0..5 {
            r.push_delta(&format!("line {i}\n"));
            assert!(r.backlog.is_empty(), "a settled row is never withheld");
        }
        assert_eq!(r.output.matches('\n').count(), 5);
        r.finish_stream();
        assert!(!r.dangling, "the last row ends with a newline");
    }
}

#[cfg(test)]
mod dbg3 {
    use super::*;
    #[test]
    fn probe_hold() {
        let mut r = Renderer::new();
        for i in 0..5 {
            r.push_delta(&format!("line {i}\n"));
            let held = r.backlog.trim_end_matches('\n').to_string();
            eprintln!(
                "after line {i}: held={:?} dangling={} pending_nl={}",
                held,
                r.dangling,
                r.pending.matches('\n').count()
            );
        }
    }
}
