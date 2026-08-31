//! The spinner: redraws `[3s · phase]` on stderr while a task waits.

use std::io::Write;

// doc moved above: redraws `[3s · phase]` on stderr while a task waits.

pub const TICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(120);

/// Redraws `[3s · phase]` on stderr while stopped=false, only while nothing
/// else prints (model wait, tool execution); stop() erases the line.
pub struct Ticker {
    flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    handle: Option<std::thread::JoinHandle<()>>,
    phase: Option<std::sync::Arc<std::sync::Mutex<String>>>,
}

impl Ticker {
    pub fn start(phase: &str) -> Ticker {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f2 = flag.clone();
        let phase = std::sync::Arc::new(std::sync::Mutex::new(phase.to_string()));
        let phase_text = phase.clone();
        let handle = std::thread::spawn(move || {
            const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            // short sleep slices so stop() joins almost immediately; the
            // spinner itself only redraws every TICK_MS
            let t0 = std::time::Instant::now();
            let mut last_draw = std::time::Instant::now()
                .checked_sub(TICK_INTERVAL)
                .unwrap_or_else(std::time::Instant::now); // draw frame zero at once
            let mut frame = 0usize;
            while !f2.load(std::sync::atomic::Ordering::Relaxed) {
                if last_draw.elapsed() >= TICK_INTERVAL {
                    let secs = t0.elapsed().as_secs_f32();
                    let spin = SPINNER[frame % SPINNER.len()];
                    frame += 1;
                    let phase = phase_text.lock().map(|p| p.clone()).unwrap_or_default();
                    eprint!("\r\x1b[2K\x1b[90m{spin} {secs:.0}s · {phase}\x1b[0m");
                    let _ = std::io::stderr().flush();
                    last_draw = std::time::Instant::now();
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        });
        Ticker {
            flag: Some(flag),
            handle: Some(handle),
            phase: Some(phase),
        }
    }

    /// Swap the label without restarting the clock: a relabel must never
    /// read as the wait starting over.
    pub fn set_phase(&self, phase: &str) {
        if let Some(p) = &self.phase
            && let Ok(mut p) = p.lock()
        {
            *p = phase.to_string();
        }
    }

    pub fn stop(&mut self) {
        if let (Some(flag), Some(handle)) = (self.flag.take(), self.handle.take()) {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = handle.join();
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop();
    }
}
