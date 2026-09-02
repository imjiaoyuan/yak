//! macOS terminal, shell and desktop implementations.

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

#[derive(Clone, Copy)]
#[repr(C)]
struct Termios {
    c_iflag: u64,
    c_oflag: u64,
    c_cflag: u64,
    c_lflag: u64,
    c_line: u8,
    c_cc: [u8; 20],
    c_ispeed: u64,
    c_ospeed: u64,
}

impl Termios {
    fn zeroed() -> Termios {
        Termios {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: 0,
            c_line: 0,
            c_cc: [0; 20],
            c_ispeed: 0,
            c_ospeed: 0,
        }
    }
}

/// Darwin raw terminal state. `tty` keeps a `/dev/tty` file alive when the
/// approval prompt needs to read from the controlling terminal while stdin is
/// redirected.
pub struct RawTerm {
    saved: Termios,
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
        const ICANON: u64 = 0x2;
        const ECHO: u64 = 0x8;
        const ISIG: u64 = 0x1;
        const VMIN: usize = 6;
        const VTIME: usize = 5;

        unsafe extern "C" {
            fn tcgetattr(fd: i32, termios_p: *mut Termios) -> i32;
            fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const Termios) -> i32;
        }
        let mut saved = Termios::zeroed();
        if unsafe { tcgetattr(fd, &mut saved) } != 0 {
            return None;
        }
        let mut raw = saved;
        raw.c_lflag &= !(ICANON | ECHO | ISIG);
        raw.c_cc[VMIN] = vmin;
        raw.c_cc[VTIME] = vtime;
        if unsafe { tcsetattr(fd, 0, &raw) } != 0 {
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
                fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const Termios) -> i32;
            }
            let _ = unsafe { tcsetattr(self.fd, 0, &self.saved) };
        }
    }
}

/// Prompt on stderr with terminal echo disabled. If the terminal cannot be
/// configured, the existing platform behavior is preserved: visible input.
pub fn read_hidden(prompt: &str) -> std::io::Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;

    struct EchoGuard {
        saved: Termios,
    }
    impl Drop for EchoGuard {
        fn drop(&mut self) {
            unsafe extern "C" {
                fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const Termios) -> i32;
            }
            let _ = unsafe { tcsetattr(0, 0, &self.saved) };
        }
    }

    const ECHO: u64 = 0x8;
    let mut saved = Termios::zeroed();
    unsafe extern "C" {
        fn tcgetattr(fd: i32, termios_p: *mut Termios) -> i32;
        fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const Termios) -> i32;
    }
    let ok = unsafe { tcgetattr(0, &mut saved) } == 0;
    if ok {
        let _guard = EchoGuard { saved };
        let mut quiet = saved;
        quiet.c_lflag &= !ECHO;
        let _ = unsafe { tcsetattr(0, 0, &quiet) };
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
    unsafe extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }
    const TIOCGWINSZ: u64 = 0x4008_7468;
    let winsize = |fd: i32| -> Option<(usize, usize)> {
        let mut ws = Winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let ok = unsafe { ioctl(fd, TIOCGWINSZ, &mut ws as *mut Winsize) } == 0;
        ok.then_some((ws.ws_col as usize, ws.ws_row as usize))
    };
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

/// Start the shell in a new process group. Darwin's `setsid` symbol has
/// historical link availability risk, so `setpgid(0, 0)` is the conservative
/// POSIX choice here. The tree is still group-killable.
pub(super) fn configure_shell_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe extern "C" {
        fn setpgid(pid: i32, pgid: i32) -> i32;
    }
    unsafe {
        command.pre_exec(|| {
            if setpgid(0, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
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

/// Image bytes from the clipboard, if any: AppleScript writes the PNGf
/// record to a temp file (osascript cannot emit binary to stdout).
pub fn paste_clipboard_image() -> Option<Vec<u8>> {
    let path = std::env::temp_dir().join(format!("llm-paste-{}.png", std::process::id()));
    let script = format!(
        "set theFile to (open for access (POSIX file \"{}\") with write permission)\n\
         write (the clipboard as \u{ab}class PNGf\u{bb}) to theFile\n\
         close access theFile",
        path.display()
    );
    let ok = Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let bytes = std::fs::read(&path).ok();
    let _ = std::fs::remove_file(&path);
    bytes.filter(|b| crate::core::attachments::sniff_mime(b).is_some())
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
    fn term_size_is_positive() {
        let size = term_size();
        assert!(size.cols > 0);
        assert!(size.rows > 0);
    }
}
