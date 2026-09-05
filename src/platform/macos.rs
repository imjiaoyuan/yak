//! macOS-specific platform deltas: the new-process-group syscall and the
//! clipboard. Everything else POSIX lives in `unix.rs`.

use std::process::Command;

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
