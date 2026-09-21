//! Linux-specific platform deltas: the new-process-group syscall and the
//! clipboard. Everything else POSIX lives in `unix.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::Clip;
use crate::core::attachments::sniff_mime;

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

/// The image types to ask for, best first: the two a screenshot tool
/// re-encodes to, plus the two others some sources offer, so a clipboard
/// that carries only one of them still lands.
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Why one clipboard tool could not deliver.
enum Failure {
    /// The tool is absent, or cannot talk to the display server (no
    /// `wl-paste` outside Wayland, no `DISPLAY` for `xclip`).
    Unavailable(String),
    /// The tool answered, and the selection holds no image.
    NoImage(String),
}

impl Failure {
    fn message(self) -> String {
        match self {
            Failure::Unavailable(why) | Failure::NoImage(why) => why,
        }
    }
}

/// Image bytes, or the path of a copied image file, from the clipboard.
///
/// `wl-paste` serves a Wayland session, `xclip` an X11 one; both are asked
/// what the selection offers before anything is fetched, because requesting a
/// type the selection does not carry fails outright — a browser's copy can be
/// `image/jpeg`, a file manager's copy is a `text/uri-list` and no pixels at
/// all. The first tool that can speak for this session decides: its "no image"
/// verdict is returned as-is rather than overridden by a second tool's
/// absence. `Err` carries the reason for the caller's failure notice.
pub fn paste_clipboard_image() -> Result<Clip, String> {
    let mut unavailable: Vec<String> = Vec::new();
    let tools: [(&str, &[&str], &[&str]); 2] = [
        ("wl-paste", &["--list-types"], &["-t"]),
        (
            "xclip",
            &["-selection", "clipboard", "-o", "-t", "TARGETS"],
            &["-selection", "clipboard", "-o", "-t"],
        ),
    ];
    for (program, list_args, fetch_args) in tools {
        match paste_with(program, list_args, fetch_args) {
            Ok(clip) => return Ok(clip),
            Err(Failure::NoImage(why)) => return Err(why),
            Err(Failure::Unavailable(why)) => unavailable.push(why),
        }
    }
    Err(unavailable.join("; "))
}

/// List the offered types, then take the best image the selection carries —
/// or, failing that, the image *file* it names.
fn paste_with(program: &str, list_args: &[&str], fetch_args: &[&str]) -> Result<Clip, Failure> {
    let types = match run(program, list_args) {
        Ok(raw) => offered_types(&raw),
        Err(why) => {
            // a compositor or an X server can refuse the listing while a plain
            // request still works, so the one historical request stays as a
            // last resort
            return match fetch(program, fetch_args, "image/png") {
                Ok(bytes) => Ok(Clip::Bytes(bytes)),
                Err(_) => Err(why),
            };
        }
    };
    // an offered image that would not transfer is the tool's own failure, not
    // a missing image: report it rather than the type list
    let mut failed: Option<String> = None;
    for mime in image_candidates(&types) {
        match fetch(program, fetch_args, &mime) {
            Ok(bytes) => return Ok(Clip::Bytes(bytes)),
            Err(why) => failed = Some(why.message()),
        }
    }
    if types.iter().any(|t| t == "text/uri-list")
        && let Ok(raw) = fetch(program, fetch_args, "text/uri-list")
        && let Some(path) = image_file_in(&String::from_utf8_lossy(&raw))
    {
        return Ok(Clip::File(path));
    }
    match failed {
        Some(why) => Err(Failure::NoImage(why)),
        None => Err(Failure::NoImage(no_image(&types))),
    }
}

/// Fetch `mime`, keeping only bytes that really are an image (a source may
/// serve a conversion it cannot make) and never fewer than one byte.
fn fetch(program: &str, fetch_args: &[&str], mime: &str) -> Result<Vec<u8>, Failure> {
    let mut args = fetch_args.to_vec();
    args.push(mime);
    let bytes = run(program, &args)?;
    if bytes.is_empty() || sniff_mime(&bytes).is_none() {
        return Err(Failure::NoImage(format!("{program}: no {mime} data")));
    }
    Ok(bytes)
}

/// Run a clipboard tool to completion. A missing binary is a distinct reason
/// from a tool that ran and refused.
fn run(program: &str, args: &[&str]) -> Result<Vec<u8>, Failure> {
    match Command::new(program).args(args).output() {
        Ok(out) if out.status.success() => Ok(out.stdout),
        Ok(out) => Err(Failure::Unavailable(tool_error(program, &out.stderr))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(Failure::Unavailable(format!("{program} is not installed")))
        }
        Err(e) => Err(Failure::Unavailable(format!("{program}: {e}"))),
    }
}

/// The tool's own first stderr line, which says more than a guess would
/// ("Failed to connect to a Wayland server", "Clipboard content is not
/// available as requested type ...").
fn tool_error(program: &str, stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("failed");
    format!("{program}: {line}")
}

fn offered_types(raw: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(raw)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// The selection's image types to try, most preferred first: the known
/// formats, then any other `image/*` the source offers.
fn image_candidates(types: &[String]) -> Vec<String> {
    let mut out: Vec<String> = IMAGE_TYPES
        .iter()
        .filter(|p| types.iter().any(|t| t == *p))
        .map(|p| (*p).to_string())
        .collect();
    out.extend(
        types
            .iter()
            .filter(|t| t.starts_with("image/") && !IMAGE_TYPES.contains(&t.as_str()))
            .cloned(),
    );
    out
}

/// The first `file://` URI of a `text/uri-list` that names a local image file
/// — the shape a file manager's copy takes.
fn image_file_in(uri_list: &str) -> Option<PathBuf> {
    uri_list.lines().map(str::trim).find_map(|line| {
        let path = uri_to_path(line)?;
        let head = head_bytes(&path, 64)?;
        sniff_mime(&head).is_some().then_some(path)
    })
}

/// `file:///home/you/a%20b.png` -> `/home/you/a b.png`; a URI naming another
/// host is not a local file.
fn uri_to_path(line: &str) -> Option<PathBuf> {
    let rest = line.strip_prefix("file://")?;
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(i) if &rest[..i] == "localhost" => &rest[i..],
        _ => return None,
    };
    Some(PathBuf::from(percent_decode(path)))
}

fn head_bytes(path: &Path, n: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; n];
    let read = file.read(&mut buf).ok()?;
    buf.truncate(read);
    Some(buf)
}

/// Undo the `%XX` escapes a `file://` URI carries; a byte that is not valid
/// UTF-8 becomes a lossy char rather than dropping the path.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// "no image on the clipboard — offered: text/plain, text/uri-list": the
/// selection's own list, so the reason is never a guess.
fn no_image(types: &[String]) -> String {
    if types.is_empty() {
        return "no image on the clipboard".to_string();
    }
    let shown = types.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
    match types.len() {
        1..=4 => format!("no image on the clipboard — offered: {shown}"),
        _ => format!("no image on the clipboard — offered: {shown}, …"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_candidates_prefer_the_known_formats_over_the_rest() {
        let types = offered_types(b"text/plain\nimage/avif\nimage/jpeg\nimage/png\n");
        assert_eq!(
            image_candidates(&types),
            vec!["image/png", "image/jpeg", "image/avif"]
        );
    }

    #[test]
    fn image_candidates_ignore_a_selection_that_carries_no_image() {
        let types = offered_types(b"text/uri-list\ntext/plain\nTARGETS\n");
        assert!(image_candidates(&types).is_empty());
    }

    #[test]
    fn uri_to_path_keeps_local_paths_and_drops_remote_hosts() {
        assert_eq!(
            uri_to_path("file:///home/you/My%20Shots/a.png"),
            Some(PathBuf::from("/home/you/My Shots/a.png"))
        );
        assert_eq!(
            uri_to_path("file://localhost/tmp/a.png"),
            Some(PathBuf::from("/tmp/a.png"))
        );
        assert_eq!(uri_to_path("file://other/tmp/a.png"), None);
        assert_eq!(uri_to_path("https://example.com/a.png"), None);
    }

    #[test]
    fn image_file_in_sniffs_the_copied_file_rather_than_trusting_its_name() {
        let dir = crate::core::testutil::scratch_dir("clipboard_uri_list");
        let png = dir.join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();
        let note = dir.join("note.png");
        std::fs::write(&note, b"# not an image").unwrap();

        let list = format!("file://{}\nfile://{}\n", note.display(), png.display());
        assert_eq!(image_file_in(&list), Some(png));
        assert_eq!(image_file_in(&format!("file://{}\n", note.display())), None);
        assert_eq!(image_file_in(""), None);
    }

    #[test]
    fn no_image_names_what_the_selection_offered() {
        let types = offered_types(b"text/plain\ntext/html\n");
        assert_eq!(
            no_image(&types),
            "no image on the clipboard — offered: text/plain, text/html"
        );
        let many = offered_types(b"a\nb\nc\nd\ne\n");
        assert!(no_image(&many).ends_with(", …"), "{}", no_image(&many));
        assert_eq!(no_image(&[]), "no image on the clipboard");
    }
}
