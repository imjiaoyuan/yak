//! Terminal IO primitives: raw-mode line editing, pickers, key watching and
//! hidden input — shared by the agent REPL and command-layer prompts. The
//! platform-specific implementation lives in `crate::platform`.

pub mod lineedit;
pub mod render;
pub mod ticker;

/// Shared screen state between the render thread (TaskView, the only writer)
/// and the steer watcher reading input mid-task. The watcher may not erase or
/// print over a partial answer row, and its `queued:` notice must be printed
/// by the render thread once the row is settled — otherwise the notice's
/// `\r\x1b[2K` tears the streamed text apart and the continuation lands at
/// column 0 (the "text lost its indentation" bug).
pub struct Screen {
    /// the answer left a partial row on screen; a spinner frame or a watche-r
    /// erase would retract it
    pub dangling: std::sync::atomic::AtomicBool,
    /// `queued:` notice lines the watcher deferred; drained by the render
    /// thread at the next settle point
    pub notices: std::sync::Mutex<Vec<String>>,
}

pub fn screen() -> &'static Screen {
    static SCREEN: std::sync::OnceLock<Screen> = std::sync::OnceLock::new();
    SCREEN.get_or_init(|| Screen {
        dangling: std::sync::atomic::AtomicBool::new(false),
        notices: std::sync::Mutex::new(Vec::new()),
    })
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
        eprintln!("\x1b[2m(ctrl-c again to exit)\x1b[0m");
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
