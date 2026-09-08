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

pub struct Renderer {
    /// accumulated visible output
    pub output: String,
    /// accumulated reasoning output
    pub reasoning: String,
    /// live block streaming (terminal modes opt in via terminal_md): the
    /// answer flows verbatim inside a left-margin block, hard-wrapped at
    /// the terminal width so continuation rows keep the margin
    md: Option<crate::core::render_md::BlockStream>,
    /// bytes not yet written (frame batching)
    pending: String,
    /// a partial row is on screen without its terminating newline — the state
    /// a spinner frame's `\r\x1b[2K` would retract, so the spinner must not
    /// (re)start while it holds
    dangling: bool,
    last_flush: std::time::Instant,
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
        }
    }

    /// Terminal live-stream mode, TTY-gated: pipes and quiet mode keep raw
    /// output. The answer streams verbatim with a left margin on each of the
    /// model's own lines; rows hard-wrap at the terminal width so wrapped
    /// continuation rows carry the margin too (the width is re-read at every
    /// line start, so a resize applies from the next row on).
    pub fn terminal_md(&mut self, indent: usize) -> bool {
        if !std::io::stdout().is_terminal() {
            return false;
        }
        let mut md = crate::core::render_md::BlockStream::indented(indent);
        md.wrap_terminal();
        self.md = Some(md);
        true
    }

    /// Append answer text, printing it (hard-wrapped inside the block when
    /// streaming). Output is written at most once per FLUSH_INTERVAL.
    pub fn push_delta(&mut self, text: &str) {
        self.output.push_str(text);
        if let Some(md) = self.md.as_mut() {
            md.push_delta(text, &mut self.pending);
        } else {
            self.pending.push_str(text);
        }
        if self.last_flush.elapsed() >= FLUSH_INTERVAL {
            self.flush_pending();
        }
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
    /// line (plain pipes flush verbatim, without the terminator).
    pub fn finish_stream(&mut self) {
        if let Some(md) = self.md.as_mut() {
            let mut chunk = String::new();
            if md.finish(&mut chunk) {
                self.pending.push_str(&chunk);
            }
        }
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
    renderer: Renderer,
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
            renderer: Renderer::new(),
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

    pub fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// Reclaim the renderer (accumulated output/reasoning/usage).
    pub fn into_renderer(self) -> Renderer {
        self.renderer
    }

    /// -R: keep buffering reasoning but never print the trace line.
    fn stop_ticker(&mut self) {
        if let Some(mut t) = self.ticker.take() {
            t.stop();
        }
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
        if self.live && !self.renderer.has_dangling() {
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
                    eprintln!("\x1b[90m{pad}thinking ... end\x1b[0m");
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
            eprintln!("\x1b[2m{line}\x1b[0m");
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
        self.renderer.push_delta(text);
    }

    /// Reasoning is never streamed: buffer it and relabel the spinner once.
    /// Interleaved reasoning bursts must not touch the visible answer: no
    /// newline, no spinner start while a partial row is live (`relabel`
    /// guards the start; the label swap on a running ticker is row-safe).
    pub fn reasoning_delta(&mut self, text: &str) {
        self.renderer.push_reasoning_buffered(text);
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
        self.renderer.finish_stream();
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
        self.renderer.finish_stream();
        // rounds continue while tools are pending: restart the wait spinner
        // (footer/abort silence it when the task ends instead)
        self.relabel(&self.label.clone());
    }

    /// Cleanup without the footer (provider error, interrupt).
    pub fn abort(&mut self) {
        self.stop_ticker();
        // do-not print the "thinking ... end" trace on an abnormal stop:
        // a force-interrupt must not read as thinking having finished
        self.renderer.finish_stream();
    }

    /// The `secs · ↑in ↓out` line, right before the prompt returns. When the
    /// provider reports cache hits, the cached share of the input rides along
    /// so prefix-cache health is visible at a glance.
    pub fn footer(&mut self, secs: f64) {
        self.stop_ticker();
        if self.streamed_any {
            println!();
        }
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
                "\x1b[90m{pad}{secs:.1}s · ↑{} ↓{}{cache}\x1b[0m",
                humanize_tokens(self.total_in),
                humanize_tokens(self.total_out)
            );
        } else {
            eprintln!("\x1b[90m{pad}{secs:.1}s\x1b[0m");
        }
    }
}
