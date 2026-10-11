//! Terminal IO primitives: raw-mode line editing, pickers, key watching and
//! hidden input — shared by the agent REPL and command-layer prompts. The
//! platform-specific implementation lives in `crate::platform`.

pub mod lineedit;
pub mod render;
pub mod ticker;

/// Shared screen state between the render thread (TaskView, the only writer)
/// and the key watcher reading input mid-task: the watcher must not have the
/// render thread erase a partial answer row while the user is reading it.
pub struct Screen {
    /// the answer left a partial row on screen; a spinner frame's erase
    /// would retract it
    pub dangling: std::sync::atomic::AtomicBool,
    /// the user pressed a key mid-stream — they are watching, so the render
    /// thread flushes its pacing backlog on the next tick instead of typing
    /// it out (write-once holds; pacing only decides when bytes are written)
    pub flush_now: std::sync::atomic::AtomicBool,
}

pub fn screen() -> &'static Screen {
    static SCREEN: std::sync::OnceLock<Screen> = std::sync::OnceLock::new();
    SCREEN.get_or_init(|| Screen {
        dangling: std::sync::atomic::AtomicBool::new(false),
        flush_now: std::sync::atomic::AtomicBool::new(false),
    })
}

/// Focus state, as far as the focus-report mode (`ESC[?1004h`, held by the
/// raw readers) can tell: updated by the `ESC[I`/`ESC[O` reports the
/// readers swallow. Terminals report only *changes*, never the state at
/// mode-enable, so an interactive reader that turns the mode on calls
/// [`assume_focused`] and starts from focused. Non-interactive paths (a
/// pipe, one-shot mode) never turn the mode on: no report, no assumption,
/// and callers that report the state (`agent_end`, `approval` payloads)
/// omit the field, which a notification gate counts as unfocused.
static FOCUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static FOCUS_KNOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn focused() -> bool {
    FOCUSED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether the mode is on and the state is actually known (a report
/// arrived, or an interactive reader assumed it).
pub fn focus_known() -> bool {
    FOCUS_KNOWN.load(std::sync::atomic::Ordering::Relaxed)
}

/// Record one focus report (`true` = gained, `false` = lost).
pub fn set_focused(focused: bool) {
    FOCUSED.store(focused, std::sync::atomic::Ordering::Relaxed);
    FOCUS_KNOWN.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Assume the terminal is focused, called by a raw reader the moment it
/// enables the mode. The user is at the prompt it is about to draw and
/// focus reports only arrive on later changes, so a guess is the only way
/// to know; without it a session that starts focused and never switches
/// windows stays unknown and every notification fires anyway. A real
/// `ESC[I`/`ESC[O` overwrites the guess.
pub fn assume_focused() {
    FOCUSED.store(true, std::sync::atomic::Ordering::Relaxed);
    FOCUS_KNOWN.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Hidden input, backed by the platform console implementation. Falling back
/// to visible input is preserved as a non-platform I/O mode.
pub fn read_hidden(prompt: &str) -> std::io::Result<String> {
    crate::platform::read_hidden(prompt)
}

/// Double ctrl-c exits: the first press clears the line, a second press
/// within two seconds asks the caller to exit. Shared by every REPL.
pub struct DoubleInterrupt(Option<std::time::Instant>);

impl DoubleInterrupt {
    pub fn new() -> DoubleInterrupt {
        DoubleInterrupt(None)
    }

    /// Record one press. Returns true when the REPL should exit.
    pub fn pressed(&mut self) -> bool {
        let now = std::time::Instant::now();
        let twice = self
            .0
            .map(|t| now.duration_since(t).as_secs() < 2)
            .unwrap_or(false);
        if twice {
            return true;
        }
        self.0 = Some(now);
        eprintln!(
            "{}(ctrl-c again to exit){}",
            crate::theme::err().dim,
            crate::theme::err().reset
        );
        false
    }

    /// Any accepted line resets the window.
    pub fn reset(&mut self) {
        self.0 = None;
    }
}

/// Route SIGINT to the cooperative interrupt flag instead of killing the
/// process, so ctrl-c during a running turn only interrupts the turn.
pub fn install_sigint_handler() {
    crate::platform::install_sigint();
}

/// Restore the default interrupt handler and clear the cooperative flag.
pub fn restore_sigint_handler() {
    crate::platform::restore_sigint();
}

/// Terminal size: $COLUMNS/$LINES, then the platform query, then 80x24.
fn winsize() -> (usize, usize) {
    let size = crate::platform::term_size();
    (size.cols, size.rows)
}

pub fn columns() -> usize {
    winsize().0
}

pub fn rows() -> usize {
    winsize().1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reader_assumes_focus_until_a_report_says_otherwise() {
        // A reader opening the mode has no report to go on; the guess must
        // count as known, or the gate stays silent while the user watches.
        set_focused(false);
        assume_focused();
        assert!(focus_known());
        assert!(focused());
        // A real loss still wins over the guess.
        set_focused(false);
        assert!(focus_known());
        assert!(!focused());
    }
}
