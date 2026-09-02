//! Windows console terminal, shell and desktop implementations.
//!
//! All Win32 calls are handwritten FFI into kernel32. No `windows` or
//! `windows-sys` crate is added.

use std::ffi::c_void;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::sync::OnceLock;
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

const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;

const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
const ENABLE_WRAP_AT_EOL_OUTPUT: u32 = 0x0002;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
const ENABLE_LINE_INPUT: u32 = 0x0002;
const ENABLE_ECHO_INPUT: u32 = 0x0004;
const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const WAIT_OBJECT_0: u32 = 0;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetStdHandle(n: u32) -> *mut c_void;
    fn GetConsoleMode(h: *mut c_void, mode: *mut u32) -> i32;
    fn SetConsoleMode(h: *mut c_void, mode: u32) -> i32;
    fn ReadConsoleW(h: *mut c_void, buf: *mut u16, len: u32, read: *mut u32, _: *mut c_void)
    -> i32;
    fn GetConsoleScreenBufferInfo(h: *mut c_void, info: *mut ConsoleScreenBufferInfo) -> i32;
    fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
    fn WaitForSingleObject(h: *mut c_void, ms: u32) -> u32;
}

#[repr(C)]
struct ConsoleCoord {
    x: i16,
    y: i16,
}

#[repr(C)]
struct SmallRect {
    left: i16,
    top: i16,
    right: i16,
    bottom: i16,
}

#[repr(C)]
struct ConsoleScreenBufferInfo {
    size: ConsoleCoord,
    cursor_position: ConsoleCoord,
    attributes: u16,
    window: SmallRect,
    maximum_window_size: ConsoleCoord,
}

/// Win32 raw terminal state.
pub struct RawTerm {
    handle: *mut c_void,
    saved_mode: u32,
    active: bool,
    pending: Option<u16>,
    queue: std::collections::VecDeque<u8>,
}

// The console input handle is process-global and this RawTerm instance is
// moved into the KeyWatcher thread for exclusive single-threaded access.
unsafe impl Send for RawTerm {}

impl RawTerm {
    pub fn acquire(_vtime: u8, _vmin: u8) -> Option<RawTerm> {
        let handle = console_input_handle()?;
        let mut saved_mode = 0;
        if unsafe { GetConsoleMode(handle, &mut saved_mode) } == 0 {
            return None;
        }
        let raw_mode = (saved_mode
            & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_EXTENDED_FLAGS
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        if unsafe { SetConsoleMode(handle, raw_mode) } == 0 {
            return None;
        }
        Some(RawTerm {
            handle,
            saved_mode,
            active: true,
            pending: None,
            queue: std::collections::VecDeque::new(),
        })
    }

    pub fn acquire_console(vtime: u8, vmin: u8) -> Option<RawTerm> {
        Self::acquire(vtime, vmin)
    }

    pub fn next_byte(&mut self) -> RawByte {
        if let Some(b) = self.queue.pop_front() {
            return RawByte::Key(b);
        }
        let Some(unit) = self.read_code_unit() else {
            return RawByte::Timeout;
        };
        self.push_unit(unit)
    }

    fn read_code_unit(&mut self) -> Option<u16> {
        if unsafe { WaitForSingleObject(self.handle, 100) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut unit = 0u16;
        let mut read = 0u32;
        let ok =
            unsafe { ReadConsoleW(self.handle, &mut unit, 1, &mut read, std::ptr::null_mut()) };
        if ok == 0 || read == 0 {
            return None;
        }
        Some(unit)
    }

    fn push_unit(&mut self, unit: u16) -> RawByte {
        if unit < 0x80 {
            return RawByte::Key(unit as u8);
        }
        if let Some(high) = self.pending.take() {
            if (0xDC00..=0xDFFF).contains(&unit) {
                let scalar =
                    0x10000 + (((high as u32 - 0xD800) << 10) | (u32::from(unit) - 0xDC00));
                if let Some(ch) = char::from_u32(scalar) {
                    return self.push_char(ch);
                }
            }
            self.pending = None;
            self.enqueue_char('\u{FFFD}');
            self.enqueue_unit(unit);
            return self.pop_queued_byte();
        }
        if (0xD800..=0xDBFF).contains(&unit) {
            self.pending = Some(unit);
            return self.next_byte();
        }
        if (0xDC00..=0xDFFF).contains(&unit) {
            return self.push_char('\u{FFFD}');
        }
        if let Some(ch) = char::from_u32(u32::from(unit)) {
            self.push_char(ch)
        } else {
            self.push_char('\u{FFFD}')
        }
    }

    fn enqueue_unit(&mut self, unit: u16) {
        if let Some(ch) = char::from_u32(u32::from(unit)) {
            self.enqueue_char(ch);
        } else {
            self.enqueue_char('\u{FFFD}');
        }
    }

    fn enqueue_char(&mut self, ch: char) {
        let mut buf = [0u8; 4];
        let bytes = ch.encode_utf8(&mut buf).as_bytes();
        self.queue.extend(bytes.iter().copied());
    }

    fn push_char(&mut self, ch: char) -> RawByte {
        let mut buf = [0u8; 4];
        let bytes = ch.encode_utf8(&mut buf).as_bytes();
        self.queue.extend(bytes.iter().copied().skip(1));
        RawByte::Key(bytes[0])
    }

    fn pop_queued_byte(&mut self) -> RawByte {
        match self.queue.pop_front() {
            Some(b) => RawByte::Key(b),
            None => RawByte::Timeout,
        }
    }
}

impl Drop for RawTerm {
    fn drop(&mut self) {
        if self.active {
            let _ = unsafe { SetConsoleMode(self.handle, self.saved_mode) };
        }
    }
}

fn console_input_handle() -> Option<*mut c_void> {
    let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let invalid = h.is_null() || h as usize == usize::MAX;
    (!invalid).then_some(h)
}

/// Prompt on stderr with console echo disabled. If the console cannot be
/// configured, the existing platform behavior is preserved: visible input.
pub fn read_hidden(prompt: &str) -> std::io::Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;

    if let Some(handle) = console_input_handle() {
        let mut saved_mode = 0;
        if unsafe { GetConsoleMode(handle, &mut saved_mode) } != 0 {
            let quiet_mode = saved_mode & !ENABLE_ECHO_INPUT;
            if unsafe { SetConsoleMode(handle, quiet_mode) } != 0 {
                let line = read_console_line(handle);
                let _ = unsafe { SetConsoleMode(handle, saved_mode) };
                if let Some(line) = line {
                    eprintln!();
                    return Ok(line);
                }
            }
        }
    }

    // Visible fallback, matching the pre-platform behavior.
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn read_console_line(handle: *mut c_void) -> Option<String> {
    let mut units = [0u16; 4096];
    let mut read = 0u32;
    let ok = unsafe {
        ReadConsoleW(
            handle,
            units.as_mut_ptr(),
            units.len() as u32,
            &mut read,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || read == 0 {
        return None;
    }
    let end = units[..read as usize]
        .iter()
        .position(|u| *u == b'\r' as u16 || *u == b'\n' as u16)
        .unwrap_or(read as usize);
    Some(String::from_utf16_lossy(&units[..end]))
}

pub fn term_size() -> TermSize {
    let cols = std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    let rows = std::env::var("LINES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    #[cfg(windows)]
    let ioctl_size = {
        let h = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        let invalid = h.is_null() || h as usize == usize::MAX;
        if !invalid {
            let mut info = unsafe { std::mem::zeroed::<ConsoleScreenBufferInfo>() };
            if unsafe { GetConsoleScreenBufferInfo(h, &mut info) } != 0 {
                Some((
                    (i32::from(info.window.right) - i32::from(info.window.left) + 1).max(1)
                        as usize,
                    (i32::from(info.window.bottom) - i32::from(info.window.top) + 1).max(1)
                        as usize,
                ))
            } else {
                None
            }
        } else {
            None
        }
    };
    // the console buffer is the live size; env vars only fall back (see linux)
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

static VT_STATE: OnceLock<Mutex<Option<(u32, u32)>>> = OnceLock::new();

pub fn init_console() {
    let state = VT_STATE.get_or_init(|| Mutex::new(None));
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return;
    }
    let out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let err = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    let mut out_valid = !out.is_null() && out as usize != usize::MAX;
    let mut err_valid = !err.is_null() && err as usize != usize::MAX;
    let mut out_mode = 0;
    let mut err_mode = 0;
    if out_valid && unsafe { GetConsoleMode(out, &mut out_mode) } != 0 {
        let enabled = out_mode
            | ENABLE_PROCESSED_OUTPUT
            | ENABLE_VIRTUAL_TERMINAL_PROCESSING
            | ENABLE_WRAP_AT_EOL_OUTPUT;
        let _ = unsafe { SetConsoleMode(out, enabled) };
    } else {
        out_valid = false;
    }
    if err_valid && unsafe { GetConsoleMode(err, &mut err_mode) } != 0 {
        let enabled = err_mode
            | ENABLE_PROCESSED_OUTPUT
            | ENABLE_VIRTUAL_TERMINAL_PROCESSING
            | ENABLE_WRAP_AT_EOL_OUTPUT;
        let _ = unsafe { SetConsoleMode(err, enabled) };
    } else {
        err_valid = false;
    }
    let saved = if out_valid || err_valid {
        Some((out_mode, err_mode))
    } else {
        None
    };
    *guard = saved;
}

pub fn restore_console() {
    let state = VT_STATE.get();
    if let Some(state) = state {
        if let Ok(mut guard) = state.lock() {
            if let Some((out_mode, err_mode)) = guard.take() {
                let out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
                let err = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
                if (!out.is_null() && out as usize != usize::MAX) && out_mode != 0 {
                    let _ = unsafe { SetConsoleMode(out, out_mode) };
                }
                if (!err.is_null() && err as usize != usize::MAX) && err_mode != 0 {
                    let _ = unsafe { SetConsoleMode(err, err_mode) };
                }
            }
        }
    }
}

extern "system" fn ctrl_event(_: u32) -> i32 {
    crate::core::http::request_interrupt();
    1
}

pub fn install_sigint() {
    let _ = unsafe { SetConsoleCtrlHandler(Some(ctrl_event), 1) };
}

pub fn restore_sigint() {
    let _ = unsafe { SetConsoleCtrlHandler(None, 0) };
    crate::core::http::clear_interrupt();
}

/// Create a fresh Windows process group so `taskkill /T` can clean up the
/// whole shell tree on timeout or interrupt.
pub(super) fn configure_shell_command(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

pub fn kill_process_tree(pid: u32) {
    let _ = Command::new("taskkill")
        .arg("/PID")
        .arg(pid.to_string())
        .arg("/T")
        .arg("/F")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

pub fn default_editor() -> &'static str {
    "notepad"
}

/// Image bytes from the clipboard, if any: PowerShell saves the image to a
/// temp PNG (-STA: clipboard access needs a single-threaded apartment).
pub fn paste_clipboard_image() -> Option<Vec<u8>> {
    let path = std::env::temp_dir().join(format!("llm-paste-{}.png", std::process::id()));
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         $i = [System.Windows.Forms.Clipboard]::GetImage(); \
         if ($i) {{ $i.Save('{}', [System.Drawing.Imaging.ImageFormat]::Png) }}",
        path.display()
    );
    let ok = Command::new("powershell")
        .args(["-NoProfile", "-STA", "-Command", &script])
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
            "Write-Output hi",
            Path::new("."),
            30,
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert_eq!(outcome.code, 0);
        assert!(String::from_utf8_lossy(&outcome.stdout).contains("hi"));
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
