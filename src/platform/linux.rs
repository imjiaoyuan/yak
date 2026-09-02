//! Linux terminal, shell and desktop implementations.

use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use super::{RawByte, TermSize};

/// Like run_shell, but complete stdout lines stream to the callback live.
pub fn run_shell_stream(
    cmd: &str,
    cwd: &Path,
    timeout: u64,
    interrupt: &AtomicBool,
    on_stdout_line: &mut dyn FnMut(&str),
) -> super::ShellOutcome {
    let spec = super::shell_spec();
    super::run_shell_streaming_with_spec(&spec, cmd, cwd, timeout, interrupt, on_stdout_line)
}

/// Run a shell command interactively with inherited stdio.
pub fn run_shell_interactive(cmd: &str, cwd: &Path) -> std::io::Result<std::process::ExitStatus> {
    let spec = super::shell_spec();
    super::build_shell_command(&spec, cmd, cwd).status()
}

/// Linux raw terminal state. `tty` keeps a `/dev/tty` file alive when the
/// approval prompt needs to read from the controlling terminal while stdin is
/// redirected.
pub struct RawTerm {
    saved: [u8; 64],
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
        unsafe extern "C" {
            fn tcgetattr(fd: i32, termios_p: *mut u8) -> i32;
            fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const u8) -> i32;
        }
        const ICANON: u8 = 0o2;
        const ECHO: u8 = 0o10;
        const ISIG: u8 = 0o1;
        let mut saved = [0u8; 64];
        if unsafe { tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
            return None;
        }
        let mut raw = saved;
        raw[12] &= !(ICANON | ECHO | ISIG);
        raw[22] = vtime;
        raw[23] = vmin;
        if unsafe { tcsetattr(fd, 0, raw.as_ptr()) } != 0 {
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
        unsafe extern "C" {
            fn read(fd: i32, buf: *mut u8, count: usize) -> i32;
        }
        let got = unsafe { read(self.fd, b.as_mut_ptr(), 1) };
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
            unsafe extern "C" {
                fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const u8) -> i32;
            }
            let _ = unsafe { tcsetattr(self.fd, 0, self.saved.as_ptr()) };
        }
    }
}

/// Prompt on stderr with terminal echo disabled. If the terminal cannot be
/// configured, the existing platform behavior is preserved: visible input.
pub fn read_hidden(prompt: &str) -> std::io::Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;

    struct EchoGuard {
        saved: [u8; 64],
    }
    impl Drop for EchoGuard {
        fn drop(&mut self) {
            unsafe extern "C" {
                fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const u8) -> i32;
            }
            let _ = unsafe { tcsetattr(0, 0, self.saved.as_ptr()) };
        }
    }

    const ECHO: u8 = 0o10;
    let mut saved = [0u8; 64];
    unsafe extern "C" {
        fn tcgetattr(fd: i32, termios_p: *mut u8) -> i32;
        fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const u8) -> i32;
    }
    let ok = unsafe { tcgetattr(0, saved.as_mut_ptr()) } == 0;
    if ok {
        let _guard = EchoGuard { saved };
        let mut quiet = saved;
        quiet[12] &= !ECHO;
        let _ = unsafe { tcsetattr(0, 0, quiet.as_ptr()) };
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
    let cols = std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    let rows = std::env::var("LINES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    unsafe extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }
    const TIOCGWINSZ: u64 = 0x5413;
    let mut ws = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ioctl_ok = unsafe { ioctl(1, TIOCGWINSZ, &mut ws as *mut Winsize) } == 0;
    let ioctl_size = ioctl_ok.then_some((ws.ws_col as usize, ws.ws_row as usize));
    // the ioctl window is the live size (a stale $COLUMNS from a resized
    // shell would otherwise make us hard-wrap past the real edge, clipping
    // long lines on the right); env vars are only a fallback when ioctl
    // cannot answer (redirected output, etc.)
    let (c, r) = if let Some((ic, ir)) = ioctl_size {
        (ic, ir)
    } else {
        (cols.unwrap_or(80), rows.unwrap_or(24))
    };
    TermSize {
        cols: c.max(1),
        rows: r.max(1),
    }
}

pub fn install_sigint() {
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    unsafe extern "C" fn on_sigint(_: i32) {
        crate::core::http::request_interrupt();
    }
    const SIGINT: i32 = 2;
    let _ = unsafe { signal(SIGINT, on_sigint as *const () as usize) };
}

pub fn restore_sigint() {
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const SIGINT: i32 = 2;
    const SIG_DFL: usize = 0;
    let _ = unsafe { signal(SIGINT, SIG_DFL) };
    crate::core::http::clear_interrupt();
}

pub fn init_console() {}

pub fn restore_console() {}

/// Configure a non-interactive command so it runs in a new session and process
/// group without a controlling terminal. Interactive prompts fail fast instead
/// of hanging the agent, and the tree remains group-killable.
pub(super) fn configure_shell_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe extern "C" {
        fn setsid() -> i32;
    }
    unsafe {
        command.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
}

pub fn kill_process_tree(pid: u32) {
    unsafe extern "C" {
        fn killpg(pgid: i32, sig: i32) -> i32;
    }
    const SIGTERM: i32 = 15;
    let _ = unsafe { killpg(pid as i32, SIGTERM) };
}

pub fn default_editor() -> &'static str {
    "vi"
}

/// Image bytes from the clipboard, if any: wl-paste first, xclip fallback,
/// validated by magic sniffing (both speak the image/png target).
pub fn paste_clipboard_image() -> Option<Vec<u8>> {
    let candidates: Vec<Vec<&str>> = vec![
        vec!["wl-paste", "-t", "image/png"],
        vec!["xclip", "-selection", "clipboard", "-t", "image/png", "-o"],
    ];
    for cmd in candidates {
        if let Ok(out) = Command::new(cmd[0]).args(&cmd[1..]).output()
            && out.status.success()
            && !out.stdout.is_empty()
            && crate::core::attachments::sniff_mime(&out.stdout).is_some()
        {
            return Some(out.stdout);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn run_shell_uses_platform_spec() {
        let outcome = run_shell_stream(
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
        let outcome = run_shell_stream(
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
    fn run_shell_stream_keeps_multibyte_chars_across_chunk_edges() {
        // ~26KB of CJK lines: the 8KiB pipe chunks land inside multibyte
        // chars, and the per-line decode must still come out clean
        let mut lines = Vec::new();
        let outcome = run_shell_stream(
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
        let size = term_size();
        assert!(size.cols > 0);
        assert!(size.rows > 0);
    }
}
