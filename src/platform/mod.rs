//! Cross-platform terminal, shell and desktop primitives.
//!
//! The public entry point is this module. Linux, macOS and Windows each
//! provide a complete implementation behind the same surface; there is no
//! platform fallback that silently weakens a feature.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux::configure_shell_command;
#[cfg(target_os = "macos")]
use macos::configure_shell_command;
#[cfg(target_os = "windows")]
use windows::configure_shell_command;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("llm supports Linux, macOS and Windows only");

/// One raw byte from the terminal, or a poll timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawByte {
    Key(u8),
    Timeout,
}

impl RawByte {
    /// The keystroke, if this read produced one.
    pub fn key(self) -> Option<u8> {
        match self {
            RawByte::Key(b) => Some(b),
            RawByte::Timeout => None,
        }
    }
}

/// Terminal dimensions in cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermSize {
    pub cols: usize,
    pub rows: usize,
}

/// The shell used for agent commands and REPL passthrough.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellSpec {
    pub program: String,
    pub prefix_args: Vec<String>,
    pub powershell: bool,
}

impl ShellSpec {
    pub fn platform_default() -> ShellSpec {
        #[cfg(target_os = "windows")]
        {
            ShellSpec {
                program: "powershell.exe".into(),
                prefix_args: vec!["-NoProfile".into(), "-Command".into()],
                powershell: true,
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            ShellSpec {
                program: "sh".into(),
                prefix_args: vec!["-c".into()],
                powershell: false,
            }
        }
    }

    /// Parse `LLM_SHELL`.
    ///
    /// The basename decides the argument style; names that are not known shell
    /// programs use the POSIX `-c` convention. This is an explicit user
    /// override, so unknown programs are kept verbatim rather than replaced.
    pub fn from_name(name: &str) -> ShellSpec {
        let program = name.trim().to_string();
        let base = std::path::Path::new(&program)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&program)
            .to_ascii_lowercase();
        match base.as_str() {
            "cmd" | "cmd.exe" => ShellSpec {
                program,
                prefix_args: vec!["/C".into()],
                powershell: false,
            },
            "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => ShellSpec {
                program,
                prefix_args: vec!["-NoProfile".into(), "-Command".into()],
                powershell: true,
            },
            _ => ShellSpec {
                program,
                prefix_args: vec!["-c".into()],
                powershell: false,
            },
        }
    }
}

/// Raw result of a non-interactive shell command. `BashTool` maps this to the
/// agent-visible `ToolOutput`; the platform layer never depends on `agent`.
pub struct ShellOutcome {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub interrupted: bool,
    pub timed_out: bool,
}

/// LLM_SHELL is a configuration knob, not a fallback: only an explicit value
/// changes the shell, and it is honored identically on every platform.
pub fn shell_spec() -> ShellSpec {
    match std::env::var_os("LLM_SHELL") {
        Some(v) => ShellSpec::from_name(&v.into_string().unwrap_or_default()),
        None => ShellSpec::platform_default(),
    }
}

/// Like run_shell, but complete stdout lines are handed to `on_stdout_line`
/// live while the command runs (the agent streams long command output).
pub(crate) fn run_shell_streaming_with_spec(
    spec: &ShellSpec,
    cmd: &str,
    cwd: &Path,
    timeout: u64,
    interrupt: &AtomicBool,
    on_stdout_line: &mut dyn FnMut(&str),
) -> ShellOutcome {
    run_shell_prepared(spec, cmd, cwd, None, timeout, interrupt, on_stdout_line)
}

fn run_shell_prepared(
    spec: &ShellSpec,
    cmd: &str,
    cwd: &Path,
    stdin_data: Option<&str>,
    timeout: u64,
    interrupt: &AtomicBool,
    on_stdout_line: &mut dyn FnMut(&str),
) -> ShellOutcome {
    let mut command = build_shell_command(spec, cmd, cwd);
    configure_shell_command(&mut command);
    command
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_with_progress(command, stdin_data, timeout, interrupt, on_stdout_line)
}

pub(crate) fn build_shell_command(spec: &ShellSpec, cmd: &str, cwd: &Path) -> Command {
    let mut command = Command::new(&spec.program);
    command.args(&spec.prefix_args);
    if spec.powershell {
        // PowerShell only propagates a native command's exit code when it is
        // the last statement. Appending the explicit exit makes nonzero codes
        // visible to the agent error branch; `$LASTEXITCODE` is initially 0.
        let mut full = cmd.to_string();
        if full.is_empty() {
            full.push_str("exit $LASTEXITCODE");
        } else {
            full.push_str("; exit $LASTEXITCODE");
        }
        command.arg(full);
    } else {
        command.arg(cmd);
    }
    command.current_dir(cwd);
    command
}

/// Run a prepared command to completion: `stdin_data` (if any) is written
/// once and stdin closed, complete stdout lines stream to the callback.
/// Used by script tools, whose commands arrive pre-built rather than as a
/// shell line. The caller supplies the interrupt flag to honor ctrl+c.
pub(crate) fn run_with_progress(
    mut child_command: Command,
    stdin_data: Option<&str>,
    timeout: u64,
    interrupt: &AtomicBool,
    on_stdout_line: &mut dyn FnMut(&str),
) -> ShellOutcome {
    let mut child = match child_command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ShellOutcome {
                code: -1,
                stdout: Vec::new(),
                stderr: format!("cannot spawn command: {e}").into_bytes(),
                interrupted: false,
                timed_out: false,
            };
        }
    };

    let mut pipes = Pipes::new(child.stdout.take(), child.stderr.take());
    // write the payload only after the reader threads exist, so a child
    // that answers before consuming stdin cannot deadlock on a full pipe
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        if let Some(data) = stdin_data {
            let _ = stdin.write_all(data.as_bytes());
            let _ = stdin.flush();
        }
    }
    let deadline = Instant::now() + Duration::from_secs(timeout);

    let status = loop {
        pipes.drain(on_stdout_line);
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let timed_out = Instant::now() >= deadline;
                if interrupt.load(Ordering::Relaxed) || timed_out {
                    let interrupted = interrupt.load(Ordering::Relaxed);
                    kill_process_tree(child.id());
                    let _ = child.kill();
                    let _ = child.wait();
                    pipes.finish(on_stdout_line);
                    let Pipes { stdout, stderr, .. } = pipes;
                    return ShellOutcome {
                        code: -1,
                        stdout,
                        stderr,
                        interrupted,
                        timed_out: !interrupted,
                    };
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                kill_process_tree(child.id());
                let _ = child.kill();
                let _ = child.wait();
                pipes.finish(on_stdout_line);
                let Pipes {
                    stdout, mut stderr, ..
                } = pipes;
                stderr.extend_from_slice(format!("wait failed: {e}").as_bytes());
                return ShellOutcome {
                    code: -1,
                    stdout,
                    stderr,
                    interrupted: false,
                    timed_out: false,
                };
            }
        }
    };

    pipes.finish(on_stdout_line);
    let Pipes { stdout, stderr, .. } = pipes;
    ShellOutcome {
        code: status.code().unwrap_or(-1),
        stdout,
        stderr,
        interrupted: false,
        timed_out: false,
    }
}

/// Captured-output ceiling: buffers keep only the last `PIPE_TAIL_CAP` bytes.
/// Every consumer truncates to the tail anyway, and an unbounded capture
/// would let a chatty command eat all memory.
const PIPE_TAIL_CAP: usize = 512 * 1024;

/// The child's two output pipes: reader threads forward chunks through
/// channels; the main loop drains them into buffers and hands complete
/// stdout lines to the progress callback as they arrive.
struct Pipes {
    out_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    err_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    handles: Vec<std::thread::JoinHandle<()>>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// trailing partial stdout line, flushed when its newline arrives; kept
    /// as bytes so a multibyte char split across pipe chunks stays intact
    residue: Vec<u8>,
}

/// Keep at most the last `cap` bytes of `buf`.
fn tail_cap(buf: &mut Vec<u8>, cap: usize) {
    if buf.len() > cap {
        let drop = buf.len() - cap;
        buf.drain(..drop);
    }
}

impl Pipes {
    fn new(
        stdout: Option<std::process::ChildStdout>,
        stderr: Option<std::process::ChildStderr>,
    ) -> Pipes {
        use std::sync::mpsc;
        let (out_tx, out_rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();
        let handles = vec![
            stdout.map(|r| spawn_pipe_sender(r, out_tx)),
            stderr.map(|r| spawn_pipe_sender(r, err_tx)),
        ]
        .into_iter()
        .flatten()
        .collect();
        Pipes {
            out_rx: Some(out_rx),
            err_rx: Some(err_rx),
            handles,
            stdout: Vec::new(),
            stderr: Vec::new(),
            residue: Vec::new(),
        }
    }

    fn drain(&mut self, on_stdout_line: &mut dyn FnMut(&str)) {
        if let Some(rx) = &self.out_rx {
            for chunk in rx.try_iter() {
                tail_cap(&mut self.stdout, PIPE_TAIL_CAP);
                self.stdout.extend_from_slice(&chunk);
                self.residue.extend_from_slice(&chunk);
                // a child flooding output without a newline must not grow the
                // partial-line buffer without bound
                tail_cap(&mut self.residue, PIPE_TAIL_CAP);
                while let Some(nl) = self.residue.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = self.residue.drain(..=nl).collect();
                    let text = String::from_utf8_lossy(&line);
                    on_stdout_line(text.trim_end_matches(['\n', '\r']));
                }
            }
        }
        if let Some(rx) = &self.err_rx {
            for chunk in rx.try_iter() {
                tail_cap(&mut self.stderr, PIPE_TAIL_CAP);
                self.stderr.extend_from_slice(&chunk);
            }
        }
    }

    /// Join the reader threads, drain what they sent, flush a trailing
    /// partial stdout line.
    fn finish(&mut self, on_stdout_line: &mut dyn FnMut(&str)) {
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
        self.drain(on_stdout_line);
        if !self.residue.is_empty() {
            let text = String::from_utf8_lossy(&self.residue);
            on_stdout_line(text.trim_end_matches(['\n', '\r']));
            self.residue.clear();
        }
    }
}

/// Pipe reader thread: forwards each chunk through the channel; the channel
/// closing (receiver dropped) ends the read early.
fn spawn_pipe_sender<R: std::io::Read + Send + 'static>(
    mut r: R,
    tx: std::sync::mpsc::Sender<Vec<u8>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_spec_recognizes_builtin_names() {
        assert_eq!(
            ShellSpec::from_name("/usr/bin/bash"),
            ShellSpec {
                program: "/usr/bin/bash".into(),
                prefix_args: vec!["-c".into()],
                powershell: false,
            }
        );
        assert_eq!(
            ShellSpec::from_name("powershell.exe"),
            ShellSpec {
                program: "powershell.exe".into(),
                prefix_args: vec!["-NoProfile".into(), "-Command".into()],
                powershell: true,
            }
        );
        assert_eq!(
            ShellSpec::from_name("cmd"),
            ShellSpec {
                program: "cmd".into(),
                prefix_args: vec!["/C".into()],
                powershell: false,
            }
        );
        assert_eq!(
            ShellSpec::from_name("python3"),
            ShellSpec {
                program: "python3".into(),
                prefix_args: vec!["-c".into()],
                powershell: false,
            }
        );
    }
}
