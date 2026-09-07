//! Shared POSIX implementation for Linux and macOS: everything except the
//! termios memory layout, the new-process-group syscall, and the clipboard.
//! The layout differences live in `termios_impl`; the behavioral differences
//! stay in `linux.rs` / `macos.rs`.

use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::io::AsRawFd;

use super::{RawByte, TermSize};

mod ffi {
    // one declaration per libc symbol, shared by everything below
    unsafe extern "C" {
        pub fn tcgetattr(fd: i32, termios_p: *mut u8) -> i32;
        pub fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const u8) -> i32;
        pub fn read(fd: i32, buf: *mut u8, count: usize) -> i32;
        pub fn ioctl(fd: i32, request: u64, ...) -> i32;
        pub fn signal(signum: i32, handler: usize) -> usize;
        pub fn killpg(pgid: i32, sig: i32) -> i32;
    }
}

#[cfg(target_os = "linux")]
mod termios_impl {
    /// Linux lays termios out as an opaque byte block; the offsets below are
    /// c_lflag (12), the c_iflag low/high bytes (0/1) and VTIME/VMIN (22/23).
    pub type Buf = [u8; 64];

    pub fn zeroed() -> Buf {
        [0u8; 64]
    }

    pub fn ptr(b: &mut Buf) -> *mut u8 {
        b.as_mut_ptr()
    }

    pub fn rawify(b: &mut Buf, vtime: u8, vmin: u8) {
        const ICANON: u8 = 0o2;
        const ECHO: u8 = 0o10;
        const ISIG: u8 = 0o1;
        // c_iflag (first u32, little endian): clear CR/LF translation so Enter
        // stays \r while ctrl+j arrives as \n — the two must stay distinct
        // for the line editor's submit-vs-newline split
        const IGNCR: u8 = 0o200; // bit 7 of the low byte
        const INLCR: u8 = 0o100; // bit 6
        const ICRNL_LOW: u8 = 0o1; // ICRNL is 0x100: bit 0 of the high byte
        b[12] &= !(ICANON | ECHO | ISIG);
        b[0] &= !(INLCR | IGNCR);
        b[1] &= !ICRNL_LOW;
        b[22] = vtime;
        b[23] = vmin;
    }

    pub fn echo_off(b: &mut Buf) {
        b[12] &= !0o10; // ECHO in c_lflag
    }
}

#[cfg(target_os = "macos")]
mod termios_impl {
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Termios {
        c_iflag: u64,
        c_oflag: u64,
        c_cflag: u64,
        c_lflag: u64,
        c_cc: [u8; 20],
        c_ispeed: u64,
        c_ospeed: u64,
    }

    pub type Buf = Termios;

    pub fn zeroed() -> Buf {
        Termios {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: 0,
            c_cc: [0; 20],
            c_ispeed: 0,
            c_ospeed: 0,
        }
    }

    pub fn ptr(b: &mut Buf) -> *mut u8 {
        b as *mut Buf as *mut u8
    }

    pub fn rawify(b: &mut Buf, vtime: u8, vmin: u8) {
        // macOS c_lflag bits (unlike Linux's: ICANON 0x2, ISIG 0x1)
        const ICANON: u64 = 0x100;
        const ECHO: u64 = 0x8;
        const ISIG: u64 = 0x80;
        // keep CR/LF untranslated: Enter stays \r, ctrl+j stays \n
        const ICRNL: u64 = 0x100;
        const INLCR: u64 = 0x40;
        const IGNCR: u64 = 0x80;
        b.c_lflag &= !(ICANON | ECHO | ISIG);
        b.c_iflag &= !(ICRNL | INLCR | IGNCR);
        // macOS c_cc indices: VMIN = 16, VTIME = 17
        b.c_cc[17] = vtime;
        b.c_cc[16] = vmin;
    }

    pub fn echo_off(b: &mut Buf) {
        b.c_lflag &= !0x8; // ECHO
    }
}

use termios_impl::{Buf, echo_off, ptr, rawify, zeroed};

#[cfg(target_os = "linux")]
const TIOCGWINSZ: u64 = 0x5413;
#[cfg(target_os = "macos")]
const TIOCGWINSZ: u64 = 0x4008_7468;

/// POSIX raw terminal state. `tty` keeps a `/dev/tty` file alive when the
/// approval prompt needs to read from the controlling terminal while stdin is
/// redirected.
pub struct RawTerm {
    saved: Buf,
    active: bool,
    fd: i32,
    tty: Option<std::fs::File>,
}

impl RawTerm {
    /// Raw mode on stdin, polled with the given VMIN/VTIME.
    pub fn acquire(vtime: u8, vmin: u8) -> Option<RawTerm> {
        Self::acquire_fd_raw(0, vtime, vmin)
    }

    /// Raw mode on stdin, or on `/dev/tty` when stdin is not a terminal.
    pub fn acquire_console(vtime: u8, vmin: u8) -> Option<RawTerm> {
        if std::io::stdin().is_terminal() {
            return Self::acquire(vtime, vmin);
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        let fd = file.as_raw_fd();
        let mut term = Self::acquire_fd_raw(fd, vtime, vmin)?;
        term.tty = Some(file);
        Some(term)
    }

    fn acquire_fd_raw(fd: i32, vtime: u8, vmin: u8) -> Option<RawTerm> {
        let mut saved = zeroed();
        if unsafe { ffi::tcgetattr(fd, ptr(&mut saved)) } != 0 {
            return None;
        }
        let mut raw = saved;
        rawify(&mut raw, vtime, vmin);
        if unsafe { ffi::tcsetattr(fd, 0, ptr(&mut raw)) } != 0 {
            return None;
        }
        Some(RawTerm {
            saved,
            active: true,
            fd,
            tty: None,
        })
    }

    pub fn next_byte(&mut self) -> RawByte {
        let mut b = [0u8; 1];
        let got = unsafe { ffi::read(self.fd, b.as_mut_ptr(), 1) };
        if got > 0 {
            RawByte::Key(b[0])
        } else {
            RawByte::Timeout
        }
    }
}

unsafe impl Send for RawTerm {}

impl Drop for RawTerm {
    fn drop(&mut self) {
        if self.active {
            let _ = unsafe { ffi::tcsetattr(self.fd, 0, ptr(&mut self.saved)) };
        }
    }
}

/// Prompt on stderr with terminal echo disabled. If the terminal cannot be
/// configured, the existing platform behavior is preserved: visible input.
pub fn read_hidden(prompt: &str) -> std::io::Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;

    struct EchoGuard {
        saved: Buf,
    }
    impl Drop for EchoGuard {
        fn drop(&mut self) {
            let _ = unsafe { ffi::tcsetattr(0, 0, ptr(&mut self.saved)) };
        }
    }

    let mut saved = zeroed();
    let ok = unsafe { ffi::tcgetattr(0, ptr(&mut saved)) } == 0;
    if ok {
        let _guard = EchoGuard { saved };
        let mut quiet = _guard.saved;
        echo_off(&mut quiet);
        let _ = unsafe { ffi::tcsetattr(0, 0, ptr(&mut quiet)) };
        let mut line = String::new();
        let n = std::io::stdin().lock().read_line(&mut line)?;
        eprintln!();
        if n == 0 {
            return Ok(String::new());
        }
        return Ok(line.trim_end_matches(['\r', '\n']).to_string());
    }

    // Visible fallback, matching the pre-platform behavior.
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

pub fn term_size() -> TermSize {
    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }
    let winsize = |fd: i32| -> Option<(usize, usize)> {
        let mut ws = Winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let ok = unsafe { ffi::ioctl(fd, TIOCGWINSZ, &mut ws as *mut Winsize) } == 0;
        ok.then_some((ws.ws_col as usize, ws.ws_row as usize))
    };
    // Prefer the live window from stdout; fall back to the controlling
    // terminal when stdout is redirected, so a stale $COLUMNS can't make us
    // hard-wrap past the real edge and soft-wrap the left margin.
    let size = winsize(1).or_else(|| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()
            .and_then(|f| {
                use std::os::unix::io::AsRawFd;
                winsize(f.as_raw_fd())
            })
    });
    let (c, r) = if let Some((c, r)) = size {
        (c, r)
    } else {
        let cols = std::env::var("COLUMNS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok());
        let rows = std::env::var("LINES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok());
        (cols.unwrap_or(80), rows.unwrap_or(24))
    };
    TermSize {
        cols: c.max(1),
        rows: r.max(1),
    }
}

pub fn install_sigint() {
    unsafe extern "C" fn on_sigint(_: i32) {
        crate::platform::interrupt::request();
    }
    const SIGINT: i32 = 2;
    let _ = unsafe { ffi::signal(SIGINT, on_sigint as *const () as usize) };
}

pub fn restore_sigint() {
    const SIGINT: i32 = 2;
    const SIG_DFL: usize = 0;
    let _ = unsafe { ffi::signal(SIGINT, SIG_DFL) };
    crate::platform::interrupt::clear();
}

pub fn init_console() {}

pub fn restore_console() {}

pub fn kill_process_tree(pid: u32) {
    const SIGTERM: i32 = 15;
    let _ = unsafe { ffi::killpg(pid as i32, SIGTERM) };
}

pub fn default_editor() -> &'static str {
    "vi"
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn run_shell_uses_platform_spec() {
        let outcome = super::super::run_shell_stream(
            "printf hi",
            Path::new("."),
            30,
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert_eq!(outcome.code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "hi");
        assert!(!outcome.interrupted);
        assert!(!outcome.timed_out);
    }

    #[test]
    fn run_shell_stream_delivers_lines_live() {
        let mut lines = Vec::new();
        let outcome = super::super::run_shell_stream(
            "printf 'one\ntwo\nthree'",
            Path::new("."),
            30,
            &AtomicBool::new(false),
            &mut |l: &str| lines.push(l.to_string()),
        );
        assert_eq!(outcome.code, 0);
        assert_eq!(lines, vec!["one", "two", "three"]);
        // the full output is still captured for the tool result
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one\ntwo\nthree");
    }

    #[test]
    fn backgrounded_grandchild_does_not_hang_the_call() {
        // the shell exits at once; the backgrounded sleep inherits the
        // stdout pipe and would hold it open for 30s without the grace kill
        let started = std::time::Instant::now();
        let outcome = super::super::run_shell_stream(
            "sleep 30 & echo done",
            Path::new("."),
            60,
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert_eq!(outcome.code, 0);
        assert!(String::from_utf8_lossy(&outcome.stdout).contains("done"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "call took {:?}, the grandchild held the pipe",
            started.elapsed()
        );
    }

    #[test]
    fn run_shell_stream_keeps_multibyte_chars_across_chunk_edges() {
        // ~26KB of CJK lines: the 8KiB pipe chunks land inside multibyte
        // chars, and the per-line decode must still come out clean
        let mut lines = Vec::new();
        let outcome = super::super::run_shell_stream(
            "yes 中文字 | head -n 9000",
            Path::new("."),
            30,
            &AtomicBool::new(false),
            &mut |l: &str| lines.push(l.to_string()),
        );
        assert_eq!(outcome.code, 0);
        assert_eq!(lines.len(), 9000);
        assert!(
            lines
                .iter()
                .all(|l| l == "中文字" && !l.contains('\u{fffd}')),
            "streamed lines garbled at a chunk boundary: {:?}",
            lines.iter().find(|l| l.contains('\u{fffd}'))
        );
    }

    #[test]
    fn term_size_is_positive() {
        let size = super::term_size();
        assert!(size.cols > 0);
        assert!(size.rows > 0);
    }
}
