//! Output rendering: batched stream, reasoning buffering, usage
//! footer formatting, fenced-block extraction.

use std::io::{IsTerminal, Write};

use crate::core::http::Usage;

/// Streamed-output flush cadence: writing+flushing per delta costs two
/// syscalls per token on char-by-char streams; one frame per interval is
/// imperceptible next to network latency and bounds the syscalls. Chrome
/// paths flush first (TaskView::pause), so nothing interleaves mid-line.
/// ~250Hz keep-up: near-instant for local model streams; still amortizes the
/// per-frame syscalls (one write+flush each frame, no per-character write).
const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);

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

    /// Append answer text and flush settled bytes at most once per interval.
    pub fn push_delta(&mut self, text: &str) {
        self.output.push_str(text);
        if let Some(md) = self.md.as_mut() {
            md.push_delta(text, &mut self.pending);
        } else {
            self.pending.push_str(text);
        }
        self.pump_due();
    }

    /// Timer-side flush: no character budget or artificial output delay.
    pub(crate) fn pump_due(&mut self) {
        if !self.pending.is_empty() && self.last_flush.elapsed() >= FLUSH_INTERVAL {
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
        self.dangling || !self.pending.is_empty()
    }

    /// Accumulate reasoning without printing it.
    pub fn push_reasoning_buffered(&mut self, text: &str) {
        self.reasoning.push_str(text);
    }
    /// Finish the markdown span and flush immediately before chrome or a new round.
    pub fn finish_stream(&mut self) {
        if let Some(md) = self.md.as_mut() {
            md.finish(&mut self.pending);
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

/// `cache 96% read · 3% write` — what a usage total's prompt tokens cost. Every
/// one of them counts toward `input`, cached reads included, so the absolute
/// figure is never the bill; these shares are. The write share rides along only
/// when the wire reported writes (Anthropic bills them, the OpenAI-compatible
/// ones cache server-side and report none), and the rest of the input is the
/// uncached share, billed at full price. None when nothing cached at all.
pub fn cache_label(u: Usage) -> Option<String> {
    (u.cached > 0 || u.cached_write > 0).then(|| {
        let mut label = format!("cache {}% read", u.cache_percent());
        if u.cached_write > 0 {
            label.push_str(&format!(" · {}% write", u.write_percent()));
        }
        label
    })
}

/// The `/status` cache note: the session's cache shares, plus the last round's
/// read share whenever it differs. A round that read nothing back reports 0%
/// here, which is the figure worth seeing the moment it happens — a compaction
/// or a prefix change invalidates the whole cached prefix — while the session
/// average lags behind it.
#[cfg(test)]
pub fn cache_note(total: Usage, last: Option<Usage>) -> String {
    let Some(label) = cache_label(total) else {
        return String::new();
    };
    let last = last.filter(|u| u.input > 0).map(|u| u.cache_percent());
    match last.filter(|l| *l != total.cache_percent()) {
        Some(l) => format!(" · {label} (last round {l}%)"),
        None => format!(" · {label}"),
    }
}

// TaskView: the agent-style task presentation shared by every mode

/// Spinner with phase relabel, a single dim `thinking ... end` trace line,
/// the answer stream through the [Renderer], and a dim `secs · this task:
/// input N · output N · cache N%` footer. `indent` is the chrome margin
/// (agent 2, prompt/chat 0); `live` false disables the spinner (quiet/JSON
/// modes).
pub struct TaskView {
    /// Shared with the flush timer so a final buffered delta does not wait
    /// for another network event.
    renderer: std::sync::Arc<std::sync::Mutex<Renderer>>,
    flusher: Option<crate::term::ticker::FlushTicker>,
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
            flusher: None,
            ticker: if live {
                // the opening label is the model id: the task's first wait is
                // the provider's, so a slow prefill explains itself from the
                // first frame that earns the hint
                let t = crate::term::ticker::Ticker::start(label);
                t.set_waiting(true);
                Some(t)
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
        crate::core::sync::lock(&self.renderer).terminal_md(indent);
    }

    /// Reclaim the renderer (accumulated output/reasoning/usage).
    pub fn into_renderer(mut self) -> Renderer {
        self.stop_flusher();
        match std::sync::Arc::try_unwrap(self.renderer) {
            Ok(m) => m.into_inner().unwrap(),
            Err(arc) => std::mem::take(&mut *crate::core::sync::lock(&arc)),
        }
    }

    /// -R: keep buffering reasoning but never print the trace line.
    fn stop_ticker(&mut self) {
        if let Some(mut t) = self.ticker.take() {
            t.stop();
        }
    }

    fn ensure_flusher(&mut self) {
        if self.flusher.is_none() {
            self.flusher = Some(crate::term::ticker::FlushTicker::start(
                std::sync::Arc::clone(&self.renderer),
            ));
        }
    }

    fn stop_flusher(&mut self) {
        if let Some(mut timer) = self.flusher.take() {
            timer.stop();
        }
    }

    /// No cosmetic waiting at a tool, approval or round boundary.
    fn settle(&mut self) {
        self.stop_flusher();
        crate::core::sync::lock(&self.renderer).finish_stream();
    }

    pub(crate) fn relabel(&mut self, label: &str) {
        // a relabel is a new phase; only the provider-wait paths re-flag it
        self.waiting(false);
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
        if self.live && !crate::core::sync::lock(&self.renderer).has_dangling() {
            self.ticker = Some(crate::term::ticker::Ticker::start(label));
        }
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

    pub fn delta(&mut self, text: &str) {
        // providers emit empty content deltas between thinking bursts;
        // treating one as "the answer started" killed the spinner and reset
        // the thinking trace mid-round
        if text.is_empty() {
            return;
        }
        self.stop_ticker();
        self.close_thinking();
        self.streamed_any = true;
        crate::core::sync::lock(&self.renderer).push_delta(text);
        self.ensure_flusher();
    }

    /// Reasoning is never streamed: buffer it and relabel the spinner once.
    /// Interleaved reasoning bursts must not touch the visible answer: no
    /// newline, no spinner start while a partial row is live (`relabel`
    /// guards the start; the label swap on a running ticker is row-safe).
    pub fn reasoning_delta(&mut self, text: &str) {
        crate::core::sync::lock(&self.renderer).push_reasoning_buffered(text);
        if !self.thinking_announced {
            self.thinking_announced = true;
            self.relabel("thinking ...");
        } else if let Some(ticker) = &self.ticker {
            // tokens are flowing: whatever wait the hint described is over
            ticker.set_waiting(false);
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
    /// awaited, so the time-to-first-token is not a silent dead window. That
    /// wait is the provider's (queueing + prefill), so a long one says so.
    pub fn resume_wait(&mut self) {
        self.relabel(&self.label.clone());
        self.waiting(true);
    }

    /// Flag or clear the prefill hint on a live ticker: provider wait vs
    /// local work is the only thing the long-wait frame needs to know.
    fn waiting(&mut self, waiting: bool) {
        if let Some(ticker) = &self.ticker {
            ticker.set_waiting(waiting);
        }
    }

    /// A model round ended: accumulate usage, close the trace, terminate a
    /// partial markdown line so the next chrome row starts on its own line.
    pub fn turn_end(&mut self, usage: Option<Usage>) {
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
        self.stop_flusher();
        // do-not print the "thinking ... end" trace on an abnormal stop:
        // a force-interrupt must not read as thinking having finished
        crate::core::sync::lock(&self.renderer).finish_stream();
    }

    /// The `secs · this task: input N · output N · cache N%` line, right
    /// before the prompt returns. Every field is this task's own: every round
    /// re-sends the whole prompt, so the input is what the run summed across
    /// its rounds, never a context size. Session-lifetime counters and the
    /// window's occupancy live in `/status`, so the two are never read as the
    /// same number. When the provider reported cache traffic, the read and
    /// write shares of that input ride along (`cache_label`), which is what
    /// prefix-cache health and the bill are made of.
    pub fn footer(&mut self, secs: f64) {
        self.stop_ticker();
        self.stop_flusher();
        if self.streamed_any {
            println!();
        }
        let p = crate::theme::err();
        let pad = " ".repeat(self.indent);
        let total = self.total;
        if total.input > 0 || total.output > 0 {
            let cache = match cache_label(total) {
                Some(label) => format!(" · {label}"),
                None => String::new(),
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

    /// The two cache shares the footer and `/status` show. Every prompt token
    /// counts toward `input`, cached reads included, so these percentages are
    /// what the round actually cost — and the write share rides only when the
    /// wire reported writes at all (the OpenAI-compatible ones keep none).
    #[test]
    fn cache_shares_name_the_read_and_the_write() {
        let mixed = Usage {
            input: 100,
            output: 5,
            cached: 96,
            cached_write: 3,
        };
        assert_eq!(
            cache_label(mixed).as_deref(),
            Some("cache 96% read · 3% write")
        );
        // a wire that reports no writes never shows a write share
        let reads_only = Usage {
            cached_write: 0,
            ..mixed
        };
        assert_eq!(cache_label(reads_only).as_deref(), Some("cache 96% read"));
        // a cold round that only filled the cache is all write
        let writes_only = Usage {
            input: 40,
            output: 1,
            cached: 0,
            cached_write: 40,
        };
        assert_eq!(
            cache_label(writes_only).as_deref(),
            Some("cache 0% read · 100% write")
        );
        // no cache traffic at all, nothing to say
        assert_eq!(cache_label(Usage::default()), None);
        assert_eq!(
            cache_label(Usage {
                input: 10,
                output: 1,
                cached: 0,
                cached_write: 0
            }),
            None
        );
    }

    /// `/status` adds the last round's read share only when it differs from
    /// the session's: it is where a compaction or a prefix change shows up
    /// first, while the average lags behind it.
    #[test]
    fn the_status_note_adds_the_last_round_only_when_it_differs() {
        let session = Usage {
            input: 1000,
            output: 20,
            cached: 960,
            cached_write: 30,
        };
        let note = " · cache 96% read · 3% write";
        let same = Usage {
            input: 100,
            output: 2,
            cached: 96,
            cached_write: 3,
        };
        assert_eq!(cache_note(session, Some(same)), note);
        assert_eq!(cache_note(session, None), note);
        // a round that read nothing back says so, even under a healthy average
        let missed = Usage {
            input: 100,
            output: 2,
            cached: 0,
            cached_write: 100,
        };
        assert_eq!(
            cache_note(session, Some(missed)),
            format!("{note} (last round 0%)")
        );
        assert_eq!(cache_note(Usage::default(), Some(missed)), "");
    }

    #[test]
    fn a_burst_is_batched_without_a_character_budget() {
        let mut r = Renderer::new();
        r.md = Some(crate::core::render_md::StyleStream::indented(
            2,
            crate::theme::out(),
        ));
        r.last_flush = std::time::Instant::now();
        let text = "你好 streaming text\n".repeat(200);
        r.push_delta(&text);
        assert_eq!(r.output, text);
        assert!(r.pending.contains("你好 streaming text"));
        assert_eq!(r.pending.matches("streaming text").count(), 200);
    }

    #[test]
    fn a_timer_flushes_a_final_delta_without_another_network_event() {
        let mut r = Renderer::new();
        r.last_flush = std::time::Instant::now();
        r.push_delta("last delta");
        assert_eq!(r.pending, "last delta");
        r.last_flush -= FLUSH_INTERVAL;
        r.pump_due();
        assert!(r.pending.is_empty());
        assert_eq!(r.output, "last delta");
    }

    #[test]
    fn a_tool_boundary_flushes_pending_text_immediately() {
        let mut view = TaskView::new(0, "", false);
        {
            let mut r = crate::core::sync::lock(&view.renderer);
            r.last_flush = std::time::Instant::now();
            r.push_delta("boundary text");
        }
        view.pause();
        let r = view.into_renderer();
        assert!(r.pending.is_empty());
        assert_eq!(r.output, "boundary text");
    }
}
