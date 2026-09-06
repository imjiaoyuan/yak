//! Linux-specific platform deltas: the new-process-group syscall and the
//! clipboard. Everything else POSIX lives in `unix.rs`.

use std::process::Command;

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

/// Image bytes from the clipboard, if any: wl-paste first, xclip fallback,
/// both speaking the image/png target. The caller sniffs the bytes to pick
/// the extension.
pub fn paste_clipboard_image() -> Option<Vec<u8>> {
    let candidates: Vec<Vec<&str>> = vec![
        vec!["wl-paste", "-t", "image/png"],
        vec!["xclip", "-selection", "clipboard", "-t", "image/png", "-o"],
    ];
    for cmd in candidates {
        if let Ok(out) = Command::new(cmd[0]).args(&cmd[1..]).output()
            && out.status.success()
            && !out.stdout.is_empty()
        {
            return Some(out.stdout);
        }
    }
    None
}
