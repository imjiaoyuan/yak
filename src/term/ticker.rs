//! The spinner: redraws `[3s · phase]` on stderr while a task waits.

use std::io::Write;

/// ~16fps spinner: fast enough to feel alive without hammering the terminal.
const TICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(60);

/// Redraws `[3s · phase]` on stderr while stopped=false, only while nothing
/// else prints (model wait, tool execution); stop() erases the line.
pub struct Ticker {
    flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    handle: Option<std::thread::JoinHandle<()>>,
    phase: std::sync::Arc<std::sync::Mutex<String>>,
    /// a frame actually reached the screen: nothing was written otherwise,
    /// and `stop` must leave the row alone (an erase over settled text —
    /// or over an empty row — is a redraw nobody asked for)
    painted: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Ticker {
    pub fn start(phase: &str) -> Ticker {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f2 = flag.clone();
        let painted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let p2 = painted.clone();
        let phase = std::sync::Arc::new(std::sync::Mutex::new(phase.to_string()));
        let phase_text = phase.clone();
        let handle = std::thread::spawn(move || {
            const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            // short sleep slices so stop() joins almost immediately; the
            // spinner itself only redraws every TICK_MS
            let t0 = std::time::Instant::now();
            // the first frame waits a full interval: a wait that ends right
            // away (the round after a settled answer) must not blink a frame
            // under text that is already printed
            let mut last_draw = std::time::Instant::now();
            let mut frame = 0usize;
            let p = crate::theme::err();
            while !f2.load(std::sync::atomic::Ordering::Relaxed) {
                if last_draw.elapsed() >= TICK_INTERVAL {
                    let secs = t0.elapsed().as_secs_f32();
                    let spin = SPINNER[frame % SPINNER.len()];
                    frame += 1;
                    let phase = phase_text.lock().map(|p| p.clone()).unwrap_or_default();
                    eprint!("\r\x1b[2K{}{spin} {secs:.0}s · {phase}{}", p.gray, p.reset);
                    let _ = std::io::stderr().flush();
                    p2.store(true, std::sync::atomic::Ordering::Relaxed);
                    last_draw = std::time::Instant::now();
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        });
        Ticker {
            flag: Some(flag),
            handle: Some(handle),
            phase,
            painted,
        }
    }

    /// Swap the label without restarting the clock: a relabel must never
    /// read as the wait starting over.
    pub fn set_phase(&self, phase: &str) {
        if let Ok(mut p) = self.phase.lock() {
            *p = phase.to_string();
        }
    }

    pub fn stop(&mut self) {
        if let (Some(flag), Some(handle)) = (self.flag.take(), self.handle.take()) {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = handle.join();
            // only retract a row this ticker actually wrote: an erase under a
            // settled answer, or on an empty row, is a redraw of its own
            if self.painted.load(std::sync::atomic::Ordering::Relaxed) {
                eprint!("\r\x1b[2K");
                let _ = std::io::stderr().flush();
            }
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Typewriter heartbeat: grants due pacing installments and flushes the
/// renderer's write buffer on a timer, so text flows at a steady rate even
/// while no SSE delta arrives. Exits on its own once the batch is drained;
/// chrome boundaries `stop` it first so nothing prints over their output.
pub struct DrainTicker {
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DrainTicker {
    pub fn start(
        renderer: std::sync::Arc<std::sync::Mutex<crate::term::render::Renderer>>,
    ) -> DrainTicker {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f2 = flag.clone();
        let handle = std::thread::spawn(move || {
            while !f2.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(8));
                let more = renderer.lock().map(|mut r| r.pump_due()).unwrap_or(false);
                if !more {
                    break;
                }
            }
        });
        DrainTicker {
            flag,
            handle: Some(handle),
        }
    }

    /// Signal the loop to end and join it: after `stop` returns, the
    /// renderer is guaranteed idle, so chrome can print safely.
    pub fn stop(&mut self) {
        self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for DrainTicker {
    fn drop(&mut self) {
        self.stop();
    }
}
