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

/// Smooth-playback cadence: when the backlog holds more text than the
/// budget may print, the screen plays it out character by character, one
/// [`SMOOTH_CHARS`] installment per interval, so a bursty upstream reads
/// as the reference-style typewriter output. Pacing only engages on backlog.
const SMOOTH_MILLIS: u64 = 16;

/// Characters printed per [`SMOOTH_MILLIS`] installment (~125/s): fast
/// enough to outrun reading, slow enough to read as flow. The budget is
/// credited by elapsed time, not sampled per call: ticks only fire when a
/// delta arrives, so a delta after a stall releases everything the silence
/// paid for instead of trickling two characters per arrival.
const SMOOTH_CHARS: usize = 2;

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
    /// steady cadence, so a bursty upstream (whole paragraphs in one SSE
    /// chunk, seconds apart) reads as continuous typewriter output instead
    /// of stop-motion. `ready` holds text whose drain time has come.
    backlog: String,
    /// the terminal-visible backlog drain: one row per interval
    drain: Option<DrainState>,
}

/// Pacing state for [`Renderer::backlog`]: when the next tick may print
/// (chars flow at a fixed rate between ticks), and whether a tick landed
/// while the stream kept up (a never-pacing stream prints verbatim).
struct DrainState {
    /// when the latest installment was granted; every full
    /// [`SMOOTH_MILLIS`] elapsed since then buys [`SMOOTH_CHARS`] more
    last: std::time::Instant,
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
            drain: None,
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
            // a stream that keeps up prints verbatim: only open a paced
            // batch when text is already waiting (the upstream burst case)
            if self.drain.is_none() {
                if self.backlog.is_empty() {
                    self.pending.push_str(&self.backlog);
                } else {
                    self.drain = Some(DrainState {
                        last: std::time::Instant::now(),
                    });
                    self.drain_tick();
                }
            } else {
                self.drain_tick();
            }
        } else {
            // plain pipes stream verbatim: pacing is a tty affordance
            self.pending.push_str(text);
            if self.last_flush.elapsed() >= FLUSH_INTERVAL {
                self.flush_pending();
            }
        }
    }

    /// Print what is due: chars join the write buffer at a steady typewriter
    /// cadence (one SMOOTH_CHARS installment per interval, credited by
    /// elapsed time) only while a batch is running — the common
    /// fast-upstream case never opens one and adds no pacing at all.
    fn drain_tick(&mut self) {
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
        self.drain.is_some() || !self.pending.is_empty()
    }

    /// Move up to the time-credited budget of backlog characters into
    /// `pending`, cutting on a char boundary (never mid-UTF-8). Each full
    /// [`SMOOTH_MILLIS`] since the last grant buys one [`SMOOTH_CHARS`]
    /// installment; the open tick grants one up front. Ticks only fire when
    /// a delta arrives, so it is elapsed time that keeps a post-stall delta
    /// from trickling. Returns the visible count; an empty backlog closes
    /// the batch.
    fn take_due_chars(&mut self, now: std::time::Instant) -> usize {
        if self.backlog.is_empty() {
            self.drain = None;
            return 0;
        }
        if self.drain.is_none() {
            self.drain = Some(DrainState { last: now });
        }
        let idle = self
            .drain
            .as_ref()
            .map_or(0, |d| now.duration_since(d.last).as_millis() as u64);
        let budget = (idle / SMOOTH_MILLIS + 1) as usize * SMOOTH_CHARS;
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
        if self.backlog.is_empty() {
            self.drain = None;
        } else if let Some(d) = self.drain.as_mut() {
            d.last = now;
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
        self.drain = None;
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
/// the answer stream through the [Renderer], and a cyan `secs · ↑in ↓out`
/// footer. `indent` is the chrome margin (agent 2, prompt/chat 0); `live`
/// false disables the spinner (quiet/JSON modes).
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
    total_in: u64,
    total_out: u64,
    total_cached: u64,
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
            total_in: 0,
            total_out: 0,
            total_cached: 0,
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
        if self.pacer.is_none() && self.renderer.lock().unwrap().drain.is_some() {
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
    /// result, next round, footer) must never land over untyped text. The
    /// cap only guards a pathological giant tail — past it, finish_stream
    /// dumps the rest.
    fn settle(&mut self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
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
            r.drain.is_some()
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
            self.total_in += u.input;
            self.total_out += u.output;
            self.total_cached += u.cached;
        }
        self.close_thinking();
        self.thinking_announced = false; // next round may announce again
        self.settle();
        // rounds continue while tools are pending: restart the wait spinner
        // (footer/abort silence it when the task ends instead)
        self.relabel(&self.label.clone());
    }

    /// Cleanup without the footer (provider error, interrupt).
    pub fn abort(&mut self) {
        self.stop_ticker();
        self.stop_pacer();
        // do-not print the "thinking ... end" trace on an abnormal stop:
        // a force-interrupt must not read as thinking having finished
        self.renderer.lock().unwrap().finish_stream();
    }

    /// The `secs · ↑in ↓out` line, right before the prompt returns. When the
    /// provider reports cache hits, the cached share of the input rides along
    /// so prefix-cache health is visible at a glance.
    pub fn footer(&mut self, secs: f64) {
        self.stop_ticker();
        self.stop_pacer();
        if self.streamed_any {
            println!();
        }
        let p = crate::theme::err();
        let pad = " ".repeat(self.indent);
        if self.total_in > 0 || self.total_out > 0 {
            let cache = if self.total_cached > 0 {
                format!(
                    " · cache {}%",
                    self.total_cached * 100 / self.total_in.max(1)
                )
            } else {
                String::new()
            };
            eprintln!(
                "{}{pad}{secs:.1}s · ↑{} ↓{}{cache}{}",
                p.gray,
                humanize_tokens(self.total_in),
                humanize_tokens(self.total_out),
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

    /// A burst larger than one tick's budget paces: the first tick prints
    /// at most SMOOTH_CHARS characters and the rest waits in the backlog,
    /// to be played out char by char (the reference-style typewriter).
    #[test]
    fn burst_backlog_paces_char_by_char() {
        let mut r = Renderer::new();
        let text: String = (0..20).map(|i| format!("row {i}\n")).collect();
        // the styled path feeds the backlog (the plain path streams pipes
        // verbatim; terminal_md is TTY-gated), so seed it directly
        r.backlog = text.clone();
        r.output = text.clone();
        r.drain_tick();
        assert!(
            r.backlog.chars().count() >= text.chars().count() - SMOOTH_CHARS,
            "one tick drains at most {SMOOTH_CHARS} chars, drained {}",
            text.chars().count() - r.backlog.chars().count()
        );
        assert_eq!(r.output, text, "the full text is always accumulated");
        // repeated ticks drain everything; finish_stream settles the rest
        while !r.backlog.is_empty() {
            r.drain_tick();
            std::thread::sleep(std::time::Duration::from_millis(SMOOTH_MILLIS + 1));
        }
        r.finish_stream();
        assert!(r.pending.is_empty() && r.backlog.is_empty());
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
        r.drain = Some(DrainState {
            last: std::time::Instant::now(),
        });
        let printed = r.take_due_chars(std::time::Instant::now());
        assert!(
            printed <= SMOOTH_CHARS,
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
            r.take_due_chars(std::time::Instant::now());
        }
        assert_eq!(r.pending, text, "the drained pending holds the full text");
        r.finish_stream();
        assert!(
            r.pending.is_empty(),
            "finish_stream prints and clears pending"
        );
    }

    /// Time is credited, not sampled: a delta arriving after a long stall
    /// prints everything the silence paid for instead of two characters.
    #[test]
    fn stall_releases_accumulated_budget() {
        let mut r = Renderer::new();
        r.backlog = "x".repeat(40);
        r.drain = Some(DrainState {
            last: std::time::Instant::now(),
        });
        // ≥21 idle intervals earn ≥22 installments = ≥44 visible chars ≥ 40
        std::thread::sleep(std::time::Duration::from_millis(SMOOTH_MILLIS * 21));
        let printed = r.take_due_chars(std::time::Instant::now());
        assert_eq!(printed, 40, "a stall releases everything it paid for");
        assert!(r.backlog.is_empty() && r.pending.len() == 40);
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
