//! Hand-rolled raw-mode line editor for the agent REPL: history, cursor
//! movement and bash-style tab completion, backed by the platform RawTerm
//! implementation. Falls back to a plain read when the terminal cannot be put
//! in raw mode.
//!
//! The buffer is multiline: ctrl+j (a raw `\n`, distinct from Enter's `\r`
//! because the platform clears ICRNL), alt+enter, shift/ctrl+enter (via the
//! kitty keyboard protocol), a lone `\` before Enter (the Linux continuation
//! gesture) and bracketed paste all insert real newlines; only Enter submits.
//! Arrows move through the multiline text and only recall history from the
//! top line with the cursor parked at its start.

use crate::platform::{RawByte, RawTerm};
use std::io::Write;
use std::path::Path;

pub enum LineResult {
    /// a completed line (without the trailing newline); may contain embedded
    /// newlines from ctrl+j, alt+enter or a lone backslash before Enter
    Line(String),
    /// ctrl-d on an empty line
    Eof,
    /// ctrl-c: caller clears the line and re-prompts
    Interrupt,
}

const HISTORY_LIMIT: usize = 200;
const LISTING_LIMIT: usize = 16;
/// the persisted history file keeps more than the in-memory window; past
/// this it is rewritten down to a soft cap so trimming doesn't refire on
/// every submit (the 0.8 watermark shape)
const HISTORY_FILE_LIMIT: usize = 2000;
const HISTORY_FILE_KEEP: usize = 1600;
/// a pasted chunk at or beyond either bound becomes an atomic placeholder
/// token instead of flooding the buffer
const PASTE_LINES_LIMIT: usize = 10;
const PASTE_CHARS_LIMIT: usize = 1000;

/// History/navigation state for one read: where in history the buffer is
/// (`history_len` = the live nav.draft), the stashed nav.draft for the way back past
/// the newest entry, and the sticky column for vertical motion.
struct Nav {
    history_index: usize,
    draft: Option<String>,
    preferred: Option<usize>,
}

impl Nav {
    fn live(history_len: usize) -> Nav {
        Nav {
            history_index: history_len,
            draft: None,
            preferred: None,
        }
    }

    /// Leave history browsing (any buffer mutation does) and forget the column.
    fn reset(&mut self, history_len: usize) {
        self.history_index = history_len;
        self.draft = None;
        self.preferred = None;
    }
}

pub struct LineEditor {
    history: Vec<String>,
}

impl LineEditor {
    pub fn new() -> LineEditor {
        LineEditor {
            history: load_history(),
        }
    }

    /// Read one line with editing. Tab completes bash-style via `completer`:
    /// a single candidate is inserted, several extend to the common prefix
    /// and then list the options. `is_command` shapes the echo highlight:
    /// true paints the whole line bold (slash commands and `!` shell lines
    /// stand out from task text), false prints plain. A line ending in a
    /// lone backslash turns Enter into a newline; ctrl+j / alt+enter /
    /// shift+enter insert one directly.
    pub fn read_line(
        &mut self,
        prompt: &str,
        help: &str,
        completer: &dyn Fn(&str) -> Vec<String>,
        is_command: &dyn Fn(&str) -> bool,
    ) -> LineResult {
        let mut term = match RawTerm::acquire(1, 0) {
            Some(t) => t,
            None => return plain_read(prompt),
        };
        let mut paste_mode = Some(PasteMode::new());
        let mut kitty = Some(KittyKeys::new());
        let mut focus_mode = Some(FocusReport::new());
        let mut out = std::io::stderr();
        let _ = write!(out, "{prompt}");
        let _ = out.flush();
        let mut line = InputLine::new();

        let mut buf = String::new();
        let mut cursor = 0usize; // byte offset into buf
        let mut nav = Nav::live(self.history.len());
        // single-entry kill buffer: ctrl+k/u/w and alt+d/backspace write,
        // ctrl+y pastes it back (it survives submit and clear)
        let mut kill = String::new();
        // (token, payload): large pastes keep their text here and only the
        // token rides in the buffer, expanded at submit
        let mut pastes: Vec<(String, String)> = Vec::new();
        // monotonic token numbering: deleting a token and pasting again must
        // not mint a second token with identical text (token_span matches by
        // first occurrence and would then edit the wrong block)
        let mut paste_seq = 0usize;

        loop {
            let mut b = loop {
                // a SIGINT that landed outside raw mode only sets the flag;
                // the poll timeout is the chance to notice it
                if crate::core::http::interrupted() {
                    buf.clear();
                    line.settle(&mut out, prompt, "", is_command);
                    return LineResult::Interrupt;
                }
                match term.next_byte() {
                    RawByte::Key(b) => break b,
                    RawByte::Timeout => {}
                }
            };
            // kitty-protocol terminals re-encode plain ctrl+letter keys as
            // CSI u (`ctrl+c` arrives as CSI 99;5u): fold them back to the
            // C0 byte so every ctrl binding keeps working. A lone ESC (no
            // sequence follows) or the kitty CSI 27u form interrupts like
            // ctrl-c — one press clears the line, two exit.
            let mut pending_esc: Option<Esc> = None;
            if b == 0x1b {
                match term.next_byte() {
                    RawByte::Timeout => {
                        line.settle(&mut out, prompt, "", is_command);
                        return LineResult::Interrupt;
                    }
                    // a second ESC inside one poll slice: the first press
                    // was a lone ESC (nothing real starts with ESC ESC)
                    RawByte::Key(0x1b) => {
                        line.settle(&mut out, prompt, "", is_command);
                        return LineResult::Interrupt;
                    }
                    RawByte::Key(first) => match term.escape_from(first) {
                        Some(Esc::Key(cp, m))
                            if (m == 5 || m == 6) && (97u32..=122).contains(&cp) =>
                        {
                            b = (cp - 96) as u8;
                        }
                        Some(Esc::FocusIn) => {
                            crate::term::set_focused(true);
                            continue;
                        }
                        Some(Esc::FocusOut) => {
                            crate::term::set_focused(false);
                            continue;
                        }
                        other => pending_esc = other,
                    },
                }
            }
            match b {
                // enter submits (raw mode clears ICRNL, so enter is always
                // \r; a raw \n is ctrl+j and inserts a newline below)
                b'\r' => {
                    // a lone trailing backslash escapes the enter into a
                    // newline (the Linux continuation gesture); a doubled
                    // backslash submits literally
                    if enter_breaks_line(&buf) {
                        buf.pop();
                        buf.push('\n');
                        cursor = buf.len();
                        nav.reset(self.history.len());
                        line.draw(&mut out, prompt, &buf, cursor, is_command);
                        continue;
                    }
                    let text = expand_pastes(&buf, &pastes);
                    if record_history(&mut self.history, &text) {
                        append_history(&text);
                    }
                    line.settle(&mut out, prompt, &buf, is_command);
                    return LineResult::Line(text);
                }
                // ctrl+j: newline (any terminal; the byte is unambiguous)
                b'\n' => {
                    buf.insert(cursor, '\n');
                    cursor += 1;
                    nav.reset(self.history.len());
                    line.draw(&mut out, prompt, &buf, cursor, is_command);
                }
                0x07 => {
                    // ctrl+g: round-trip the nav.draft through $VISUAL/$EDITOR
                    line.settle(&mut out, prompt, &buf, is_command);
                    let _ = writeln!(
                        out,
                        "{}(editing in $EDITOR — save and exit to return){}",
                        crate::theme::err().dim,
                        crate::theme::err().reset
                    );
                    // hand the terminal back: guards off, cooked mode on
                    drop(kitty.take());
                    drop(paste_mode.take());
                    drop(focus_mode.take());
                    drop(term);
                    let dir = crate::core::config::user_dir().join("tmp");
                    let _ = std::fs::create_dir_all(&dir);
                    let path = dir.join(format!("edit-{}.md", crate::core::db::ulid()));
                    if std::fs::write(&path, &buf).is_ok() {
                        let editor = std::env::var("VISUAL")
                            .or_else(|_| std::env::var("EDITOR"))
                            .unwrap_or_else(|_| crate::platform::default_editor().to_string());
                        let mut words = editor.split_whitespace();
                        let prog = words.next().unwrap_or("vi");
                        let status = std::process::Command::new(prog)
                            .args(words)
                            .arg(&path)
                            .status();
                        if matches!(status, Ok(s) if s.success())
                            && let Ok(text) = std::fs::read_to_string(&path)
                        {
                            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
                            let text = text.strip_suffix('\n').unwrap_or(text);
                            buf = text.to_string();
                            cursor = buf.len();
                            nav.reset(self.history.len());
                        }
                        let _ = std::fs::remove_file(&path);
                    }
                    match RawTerm::acquire(1, 0) {
                        Some(t) => term = t,
                        None => return LineResult::Interrupt,
                    }
                    paste_mode = Some(PasteMode::new());
                    kitty = Some(KittyKeys::new());
                    focus_mode = Some(FocusReport::new());
                    line.rows = 0; // the region restarts below the editor's output
                    line.draw(&mut out, prompt, &buf, cursor, is_command);
                }
                0x16 => {
                    // ctrl+v: pull the clipboard image as a temp-file path
                    // inserted into the line (the path auto-attaches on
                    // submit); a text clipboard pastes via the terminal's
                    // own bracketed paste instead
                    let mut notice: Option<String> = None;
                    match crate::platform::paste_clipboard_image() {
                        Ok(crate::platform::Clip::Bytes(bytes)) => {
                            match crate::core::attachments::sniff_mime(&bytes) {
                                Some(mime) => {
                                    let paste = if mime == "image/bmp" {
                                        // the APIs take png, not bmp: decode
                                        // and re-encode losslessly before the
                                        // paste ever lands on disk
                                        match crate::core::bmp::decode(&bytes).and_then(|img| {
                                            crate::core::pngenc::encode_rgba(
                                                img.width, img.height, &img.rgba,
                                            )
                                        }) {
                                            Ok(png) => Some(("png", png)),
                                            Err(e) => {
                                                notice =
                                                    Some(format!("clipboard bmp rejected: {e}"));
                                                None
                                            }
                                        }
                                    } else {
                                        let ext = match mime {
                                            "image/jpeg" => "jpg",
                                            "image/gif" => "gif",
                                            "image/webp" => "webp",
                                            _ => "png",
                                        };
                                        Some((ext, bytes.clone()))
                                    };
                                    if let Some((ext, payload)) = paste {
                                        let dir = crate::core::config::user_dir().join("tmp");
                                        let _ = std::fs::create_dir_all(&dir);
                                        let path = dir.join(format!(
                                            "paste-{}.{}",
                                            crate::core::db::ulid(),
                                            ext
                                        ));
                                        if std::fs::write(&path, &payload).is_ok() {
                                            // the path still lands in the submitted
                                            // text (auto-attach scans for it), but the
                                            // buffer shows a short atomic token — the
                                            // same placeholder big text pastes use
                                            paste_seq += 1;
                                            let token = format!("[paste #{paste_seq} image]");
                                            pastes
                                                .push((token.clone(), path.display().to_string()));
                                            buf.insert_str(cursor, &token);
                                            cursor += token.len();
                                            nav.reset(self.history.len());
                                            line.draw(&mut out, prompt, &buf, cursor, is_command);
                                        } else {
                                            notice =
                                                Some(format!("cannot write {}", path.display()));
                                        }
                                    }
                                }
                                None => notice = Some("no image on the clipboard".into()),
                            }
                        }
                        // a copied image *file* (a file manager's copy): its own
                        // path is the useful text, and auto-attach reads the
                        // file from there — no temp copy needed
                        Ok(crate::platform::Clip::File(path)) => {
                            let text = path.display().to_string();
                            buf.insert_str(cursor, &text);
                            cursor += text.len();
                            nav.reset(self.history.len());
                            line.draw(&mut out, prompt, &buf, cursor, is_command);
                        }
                        Err(why) => notice = Some(why),
                    }
                    if let Some(why) = notice {
                        let _ = writeln!(
                            out,
                            "{}({why}){}",
                            crate::theme::err().dim,
                            crate::theme::err().reset
                        );
                        line.rows = 0;
                        line.draw(&mut out, prompt, &buf, cursor, is_command);
                    }
                }
                0x0f => {
                    // ctrl+o: full help page
                    let _ = writeln!(out);
                    let _ = writeln!(out, "{help}");
                    line.rows = 0; // the region restarts below the help text
                    line.draw(&mut out, prompt, &buf, cursor, is_command);
                }
                0x03 => {
                    // ctrl-c clears the line; the caller treats two
                    // consecutive presses as "exit"
                    buf.clear();
                    line.settle(&mut out, prompt, "", is_command);
                    return LineResult::Interrupt;
                }
                0x04 => {
                    // ctrl-d: eof on empty, otherwise delete forward
                    if buf.is_empty() {
                        line.settle(&mut out, prompt, "", is_command);
                        return LineResult::Eof;
                    }
                    if let Some((i, a, b)) = token_span(&buf, &pastes, cursor)
                        && a == cursor
                    {
                        pastes.remove(i);
                        buf.replace_range(a..b, "");
                    } else if cursor < buf.len() {
                        let rest = buf[cursor..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                        buf.replace_range(cursor..cursor + rest, "");
                    } else {
                        continue;
                    }
                    nav.history_index = self.history.len();
                    nav.draft = None;
                    line.draw(&mut out, prompt, &buf, cursor, is_command);
                }
                0x01 => {
                    // ctrl-a: line start; already there means end of the
                    // previous line
                    let start = line_start(&buf, cursor);
                    cursor = if cursor > start {
                        start
                    } else {
                        start.saturating_sub(1)
                    };
                    nav.preferred = None;
                    line.draw(&mut out, prompt, &buf, cursor, is_command);
                }
                0x05 => {
                    // ctrl-e: line end; already there means start of the next
                    let end = line_end(&buf, cursor);
                    cursor = if cursor < end {
                        end
                    } else {
                        (end + 1).min(buf.len())
                    };
                    nav.preferred = None;
                    line.draw(&mut out, prompt, &buf, cursor, is_command);
                }
                0x0b => {
                    // ctrl+k: kill from the cursor to the line end
                    let end = line_end(&buf, cursor);
                    if end > cursor {
                        kill = buf[cursor..end].to_string();
                        buf.replace_range(cursor..end, "");
                        nav.reset(self.history.len());
                        line.draw(&mut out, prompt, &buf, cursor, is_command);
                    }
                }
                0x15 => {
                    // ctrl-u: kill from the line start to the cursor
                    let start = line_start(&buf, cursor);
                    if cursor > start {
                        kill = buf[start..cursor].to_string();
                        buf.replace_range(start..cursor, "");
                        cursor = start;
                        nav.reset(self.history.len());
                        line.draw(&mut out, prompt, &buf, cursor, is_command);
                    }
                }
                0x17 => {
                    // ctrl-w: kill the word before the cursor
                    kill_word_back(&mut buf, &mut cursor, &mut kill);
                    nav.reset(self.history.len());
                    line.draw(&mut out, prompt, &buf, cursor, is_command);
                }
                0x19 => {
                    // ctrl+y: yank the kill buffer
                    if !kill.is_empty() {
                        buf.insert_str(cursor, &kill);
                        cursor += kill.len();
                        nav.reset(self.history.len());
                        line.draw(&mut out, prompt, &buf, cursor, is_command);
                    }
                }
                0x7f | 0x08 => {
                    // backspace: delete the char before the cursor, a whole
                    // paste token when inside one
                    if let Some((i, a, b)) = token_span(&buf, &pastes, cursor)
                        && a < cursor
                    {
                        pastes.remove(i);
                        buf.replace_range(a..b, "");
                        cursor = a;
                    } else if cursor > 0 {
                        let mut start = cursor - 1;
                        while start > 0 && buf.as_bytes()[start] & 0xC0 == 0x80 {
                            start -= 1;
                        }
                        buf.replace_range(start..cursor, "");
                        cursor = start;
                    } else {
                        continue;
                    }
                    nav.reset(self.history.len());
                    line.draw(&mut out, prompt, &buf, cursor, is_command);
                }
                b'\t' => {
                    // bash-style: one candidate completes; several first
                    // extend to the common prefix, then (pressed again at the
                    // prefix) list the options beneath the line
                    let candidates = completer(&buf);
                    let word_start = buf
                        .rfind(|c: char| c.is_whitespace())
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    let current = &buf[word_start..];
                    let insert = match candidates.len() {
                        0 => None,
                        1 => Some(candidates[0].clone()),
                        _ => {
                            let prefix = common_prefix(&candidates);
                            (prefix.len() > current.len()).then_some(prefix)
                        }
                    };
                    match insert {
                        Some(completion) => {
                            buf.truncate(word_start);
                            buf.push_str(&completion);
                            cursor = buf.len();
                            nav.reset(self.history.len());
                            line.draw(&mut out, prompt, &buf, cursor, is_command);
                        }
                        None if candidates.len() > 1 => {
                            let _ = writeln!(out);
                            let listing: Vec<&str> = candidates
                                .iter()
                                .map(|s| s.as_str())
                                .take(LISTING_LIMIT)
                                .collect();
                            let _ = writeln!(
                                out,
                                "{}  {}{}",
                                crate::theme::err().dim,
                                listing.join("  "),
                                crate::theme::err().reset
                            );
                            line.rows = 0; // the region restarts below the listing
                            line.draw(&mut out, prompt, &buf, cursor, is_command);
                        }
                        None => {}
                    }
                }
                0x1b => match pending_esc.take() {
                    Some(Esc::AltEnter) => {
                        buf.insert(cursor, '\n');
                        cursor += 1;
                        nav.reset(self.history.len());
                        line.draw(&mut out, prompt, &buf, cursor, is_command);
                    }
                    Some(Esc::Key(cp, m)) => {
                        // kitty CSI-u keys: modified enter, alt+letter and
                        // alt+backspace land here (legacy ESC-prefixed alt
                        // keys arrive as (char, 3) through the same shape)
                        if cp == 27 {
                            // the kitty-reported plain ESC key
                            line.settle(&mut out, prompt, "", is_command);
                            return LineResult::Interrupt;
                        } else if cp == 13 {
                            // shift/ctrl/alt+enter: all just break the line
                            buf.insert(cursor, '\n');
                            cursor += 1;
                            nav.reset(self.history.len());
                            line.draw(&mut out, prompt, &buf, cursor, is_command);
                        } else if cp == 127 && m >= 3 {
                            kill_word_back(&mut buf, &mut cursor, &mut kill);
                            nav.reset(self.history.len());
                            line.draw(&mut out, prompt, &buf, cursor, is_command);
                        } else if m == 3 {
                            match u8::try_from(cp).ok() {
                                Some(b'b') => {
                                    // alt+b: back one word
                                    cursor = word_back(&buf, cursor);
                                    nav.preferred = None;
                                    line.draw(&mut out, prompt, &buf, cursor, is_command);
                                }
                                Some(b'f') => {
                                    // alt+f: forward one word
                                    cursor = word_fwd(&buf, cursor);
                                    nav.preferred = None;
                                    line.draw(&mut out, prompt, &buf, cursor, is_command);
                                }
                                Some(b'd') => {
                                    // alt+d: kill the word after the cursor
                                    let end = word_fwd(&buf, cursor);
                                    if end > cursor {
                                        kill = buf[cursor..end].to_string();
                                        buf.replace_range(cursor..end, "");
                                        nav.reset(self.history.len());
                                    }
                                    line.draw(&mut out, prompt, &buf, cursor, is_command);
                                }
                                _ => {}
                            }
                        } else if (m <= 2 || m == 7) && (0x20..0x7f).contains(&cp) {
                            // AltGr (ctrl+alt) and stray plain CSI-u chars
                            // insert literally, like the AltGr rule
                            if let Some(ch) = char::from_u32(cp) {
                                buf.insert(cursor, ch);
                                cursor += ch.len_utf8();
                                nav.reset(self.history.len());
                                line.draw(&mut out, prompt, &buf, cursor, is_command);
                            }
                        }
                    }
                    Some(Esc::Mod(f, m)) => {
                        // modified arrows (ctrl/alt+left/right): word motion
                        if m >= 3 && (f == b'C' || f == b'D') {
                            cursor = if f == b'C' {
                                word_fwd(&buf, cursor)
                            } else {
                                word_back(&buf, cursor)
                            };
                            nav.preferred = None;
                            line.draw(&mut out, prompt, &buf, cursor, is_command);
                        }
                    }
                    Some(Esc::PasteStart) => {
                        // the pasted chunk lands as buffer text: embedded
                        // newlines become hard newlines, never a submit
                        let mut bytes: Vec<u8> = Vec::new();
                        let mut idle = 0usize;
                        loop {
                            if idle > 50 {
                                break; // no end marker within ~5s: keep what came
                            }
                            match term.next_byte() {
                                RawByte::Key(0x1b) => {
                                    idle = 0;
                                    if matches!(term.escape_seq(), Some(Esc::PasteEnd)) {
                                        break;
                                    }
                                }
                                RawByte::Key(b'\r') => {
                                    idle = 0;
                                    bytes.push(b'\n');
                                }
                                RawByte::Key(b) if b >= 0x20 || b == b'\t' => {
                                    idle = 0;
                                    bytes.push(b);
                                }
                                RawByte::Key(_) => {}
                                RawByte::Timeout => idle += 1,
                            }
                        }
                        let chunk = String::from_utf8_lossy(&bytes).into_owned();
                        if !chunk.is_empty() {
                            if should_placeholder(&chunk) {
                                // too big to edit comfortably: keep the text
                                // aside and insert an atomic token instead
                                paste_seq += 1;
                                let token = paste_token(paste_seq, &chunk);
                                pastes.push((token.clone(), chunk));
                                buf.insert_str(cursor, &token);
                                cursor += token.len();
                            } else {
                                buf.insert_str(cursor, &chunk);
                                cursor += chunk.len();
                            }
                            nav.reset(self.history.len());
                            line.draw(&mut out, prompt, &buf, cursor, is_command);
                        }
                    }
                    Some(Esc::PasteEnd) => {}
                    Some(Esc::Up) => {
                        let browsing = nav.history_index < self.history.len();
                        if nav.history_index > 0 && recall_on_up(&buf, cursor, browsing) {
                            if nav.history_index == self.history.len() {
                                nav.draft = Some(buf.clone()); // the nav.draft survives recall
                            }
                            nav.history_index -= 1;
                            buf = self.history[nav.history_index].clone();
                            cursor = buf.len();
                            nav.preferred = None;
                        } else {
                            // otherwise the arrow walks the multiline text
                            let col = *nav.preferred.get_or_insert(char_col(&buf, cursor));
                            cursor = up_line(&buf, cursor, Some(col));
                        }
                        line.draw(&mut out, prompt, &buf, cursor, is_command);
                    }
                    Some(Esc::Down) => {
                        if nav.history_index < self.history.len() {
                            nav.history_index += 1;
                            if nav.history_index == self.history.len() {
                                // back past the newest entry: the nav.draft returns
                                buf = nav.draft.take().unwrap_or_default();
                            } else {
                                buf = self.history[nav.history_index].clone();
                            }
                            cursor = buf.len();
                            nav.preferred = None;
                        } else {
                            let col = *nav.preferred.get_or_insert(char_col(&buf, cursor));
                            cursor = down_line(&buf, cursor, Some(col));
                        }
                        line.draw(&mut out, prompt, &buf, cursor, is_command);
                    }
                    Some(Esc::Left) => {
                        if let Some((_, a, b)) = token_span(&buf, &pastes, cursor)
                            && b == cursor
                        {
                            cursor = a; // step over a paste token whole
                        } else {
                            while cursor > 0 && buf.as_bytes()[cursor - 1] & 0xC0 == 0x80 {
                                cursor -= 1;
                            }
                            cursor = cursor.saturating_sub(1);
                        }
                        nav.preferred = None;
                        line.draw(&mut out, prompt, &buf, cursor, is_command);
                    }
                    Some(Esc::Right) => {
                        if let Some((_, a, b)) = token_span(&buf, &pastes, cursor)
                            && a == cursor
                        {
                            cursor = b;
                        } else if cursor < buf.len() {
                            cursor += 1;
                            while cursor < buf.len() && buf.as_bytes()[cursor] & 0xC0 == 0x80 {
                                cursor += 1;
                            }
                        }
                        nav.preferred = None;
                        line.draw(&mut out, prompt, &buf, cursor, is_command);
                    }
                    Some(Esc::Home) => {
                        cursor = line_start(&buf, cursor);
                        nav.preferred = None;
                        line.draw(&mut out, prompt, &buf, cursor, is_command);
                    }
                    Some(Esc::End) => {
                        cursor = line_end(&buf, cursor);
                        nav.preferred = None;
                        line.draw(&mut out, prompt, &buf, cursor, is_command);
                    }
                    Some(Esc::Delete) => {
                        // zero when the cursor sits at the end (nothing to delete)
                        if let Some((i, a, b)) = token_span(&buf, &pastes, cursor)
                            && a == cursor
                        {
                            pastes.remove(i);
                            buf.replace_range(a..b, "");
                            nav.history_index = self.history.len();
                            nav.draft = None;
                            line.draw(&mut out, prompt, &buf, cursor, is_command);
                        } else {
                            let rest = buf[cursor..].chars().next().map_or(0, |c| c.len_utf8());
                            if rest > 0 {
                                buf.replace_range(cursor..cursor + rest, "");
                                nav.history_index = self.history.len();
                                nav.draft = None;
                                line.draw(&mut out, prompt, &buf, cursor, is_command);
                            }
                        }
                    }
                    Some(Esc::Alone) => {
                        // a lone escape in the line editor keeps the
                        // meaning: the same cooperative interrupt as ctrl+c
                        line.settle(&mut out, prompt, "", is_command);
                        return LineResult::Interrupt;
                    }
                    // focus reports are consumed at the escape_from call
                    // above (they never reach pending_esc); listed so the
                    // match stays exhaustive
                    Some(Esc::FocusIn | Esc::FocusOut) => {}
                    None => {}
                },
                _ if b < 0x20 => {
                    // other control bytes: ignore
                }
                _ => {
                    // printable or UTF-8 lead byte: gather the full char
                    let extra = if b < 0x80 {
                        0
                    } else if b & 0xE0 == 0xC0 {
                        1
                    } else if b & 0xF0 == 0xE0 {
                        2
                    } else {
                        3
                    };
                    let mut bytes = vec![b];
                    for _ in 0..extra {
                        if let Some(nb) = term.next_byte().key() {
                            bytes.push(nb);
                        }
                    }
                    if let Ok(s) = std::str::from_utf8(&bytes) {
                        buf.insert_str(cursor, s);
                        cursor += s.len();
                        nav.reset(self.history.len());
                        line.draw(&mut out, prompt, &buf, cursor, is_command);
                    }
                }
            }
        }
    }
}

impl Default for LineEditor {
    fn default() -> LineEditor {
        LineEditor::new()
    }
}

enum Esc {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    AltEnter,
    /// a bare escape key (the parser timed out with nothing after ESC)
    Alone,
    /// bracketed-paste start marker (`ESC[200~`)
    PasteStart,
    /// bracketed-paste end marker (`ESC[201~`)
    PasteEnd,
    /// focus gained report (`ESC[I`, the focus-report mode's first byte)
    FocusIn,
    /// focus lost report (`ESC[O`)
    FocusOut,
    /// kitty CSI-u key: (codepoint, modifier value where 2=shift, 3=alt,
    /// 5=ctrl, 7=ctrl+alt). Legacy ESC-prefixed alt keys arrive here too,
    /// as (char, 3); modified delete as (127, m).
    Key(u32, u8),
    /// CSI letter with a modifier (ctrl/alt+arrows): (final byte, modifier
    /// value), e.g. `ESC[1;5C` = ctrl+right
    Mod(u8, u8),
}

// pure buffer helpers

/// Byte offset of the start of the logical line containing `cursor`.
fn line_start(buf: &str, cursor: usize) -> usize {
    buf[..cursor].rfind('\n').map_or(0, |i| i + 1)
}

/// Byte offset of the line's end (its newline, or the buffer end).
fn line_end(buf: &str, cursor: usize) -> usize {
    buf[cursor..].find('\n').map_or(buf.len(), |i| cursor + i)
}

/// Char column of `cursor` within its logical line.
fn char_col(buf: &str, cursor: usize) -> usize {
    buf[line_start(buf, cursor)..cursor].chars().count()
}

/// Byte offset `col` chars into `line`, clamped to its end.
fn col_offset(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(i, _)| i)
}

/// One logical line up from `cursor`, keeping the nav.preferred char column (the
/// top line parks at its start, the boundary behavior).
fn up_line(buf: &str, cursor: usize, preferred: Option<usize>) -> usize {
    let start = line_start(buf, cursor);
    if start == 0 {
        return 0;
    }
    let prev_end = start - 1; // the newline byte
    let prev_start = line_start(buf, prev_end);
    let col = preferred.unwrap_or_else(|| char_col(buf, cursor));
    prev_start + col_offset(&buf[prev_start..prev_end], col)
}

/// One logical line down from `cursor`, keeping the nav.preferred char column
/// (the bottom line parks at the buffer end).
fn down_line(buf: &str, cursor: usize, preferred: Option<usize>) -> usize {
    let end = line_end(buf, cursor);
    if end == buf.len() {
        return buf.len();
    }
    let next_start = end + 1;
    let next_end = line_end(buf, next_start);
    let col = preferred.unwrap_or_else(|| char_col(buf, cursor));
    next_start + col_offset(&buf[next_start..next_end], col)
}

/// Back to the start of the word before `cursor` (skipping any whitespace).
fn word_back(buf: &str, cursor: usize) -> usize {
    let bytes = buf.as_bytes();
    let mut i = cursor;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    while i > 0 && !bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    i
}

/// Forward to the end of the word after `cursor` (skipping any whitespace).
fn word_fwd(buf: &str, cursor: usize) -> usize {
    let bytes = buf.as_bytes();
    let mut i = cursor;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Kill (cut) the word before the cursor into the kill buffer.
fn kill_word_back(buf: &mut String, cursor: &mut usize, kill: &mut String) {
    let start = word_back(buf, *cursor);
    if start < *cursor {
        *kill = buf[start..*cursor].to_string();
        buf.replace_range(start..*cursor, "");
        *cursor = start;
    }
}

/// up recalls history only while browsing, from an empty
/// buffer, or with the cursor parked at the very start of the top line;
/// every other position moves through the multiline text instead.
fn recall_on_up(buf: &str, cursor: usize, browsing: bool) -> bool {
    browsing || buf.is_empty() || cursor == 0
}

/// Enter's disposition at the buffer end: a lone trailing backslash escapes
/// the enter into a newline instead of submitting; a doubled backslash (or
/// no backslash) submits.
fn enter_breaks_line(buf: &str) -> bool {
    buf.ends_with('\\') && !buf.ends_with("\\\\")
}

/// Push `text` onto the in-memory history (adjacent duplicates and blank
/// lines skipped); returns whether it was recorded.
fn record_history(history: &mut Vec<String>, text: &str) -> bool {
    if text.trim().is_empty() || history.last().is_some_and(|l| l == text) {
        return false;
    }
    history.push(text.to_string());
    if history.len() > HISTORY_LIMIT {
        history.remove(0);
    }
    true
}

/// A pasted chunk at/over either bound becomes a placeholder token.
fn should_placeholder(chunk: &str) -> bool {
    chunk.matches('\n').count() + 1 > PASTE_LINES_LIMIT || chunk.chars().count() > PASTE_CHARS_LIMIT
}

/// The atomic token standing in for a large pasted chunk.
fn paste_token(n: usize, chunk: &str) -> String {
    let lines = chunk.matches('\n').count() + 1;
    if lines > 1 {
        format!("[paste #{n} +{lines} lines]")
    } else {
        format!("[paste #{n} {} chars]", chunk.chars().count())
    }
}

/// Submit-time expansion: tokens still present give way to their payloads;
/// deleted tokens (and their text) are simply gone.
fn expand_pastes(buf: &str, pastes: &[(String, String)]) -> String {
    let mut out = buf.to_string();
    for (token, payload) in pastes {
        if let Some(i) = out.find(token.as_str()) {
            out.replace_range(i..i + token.len(), payload);
        }
    }
    out
}

/// Span `(index, start, end)` of the paste token whose text touches `pos`
/// (either edge or interior); None when no token sits there.
fn token_span(buf: &str, pastes: &[(String, String)], pos: usize) -> Option<(usize, usize, usize)> {
    pastes
        .iter()
        .position(|(token, _)| match buf.find(token.as_str()) {
            Some(a) => {
                let b = a + token.len();
                a <= pos && pos <= b
            }
            None => false,
        })
        .map(|i| {
            let token = &pastes[i].0;
            let a = buf.find(token.as_str()).unwrap_or(0);
            (i, a, a + token.len())
        })
}

// history persistence

fn history_path() -> std::path::PathBuf {
    crate::core::config::user_dir().join("history.jsonl")
}

/// Load the persisted input history (the in-memory window is its tail).
fn load_history() -> Vec<String> {
    load_history_from(&history_path())
}

fn load_history_from(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > HISTORY_FILE_LIMIT {
        // rewrite down to the soft cap (raw lines, timestamps intact) so the
        // trim doesn't refire on every start; atomic write so a concurrent
        // appender never sees a torn or half-truncated file — and 0600 like
        // the appender itself, so a trim must not widen the file's mode
        let kept = &lines[lines.len() - HISTORY_FILE_KEEP..];
        let mut out = String::new();
        for l in kept {
            out.push_str(l);
            out.push('\n');
        }
        let _ = crate::core::fsx::write_atomic(path, out.as_bytes(), Some(0o600));
    }
    let start = lines.len().saturating_sub(HISTORY_LIMIT);
    lines[start..]
        .iter()
        .filter_map(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .ok()
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
        })
        .collect()
}

/// Append one submission as a single write (a single write so
/// concurrent processes never interleave a line).
fn append_history(text: &str) {
    append_history_to(&history_path(), text);
}

fn append_history_to(path: &Path, text: &str) {
    let line = serde_json::json!({
        "ts": crate::core::db::now_turn_datetime(),
        "text": text,
    })
    .to_string();
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(path) {
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
}

// rendering

/// Redraw the input row.
/// The interactive input region (prompt + buffer), terminal-row aware so
/// wrapped input — CJK reaches the margin fast — redraws cleanly instead of
/// smearing rows: each draw returns to the region top, repaints, clears to
/// the screen end and parks the cursor by cell offset.
struct InputLine {
    /// row offset of the cursor within the region (how far up to return)
    rows: usize,
    /// terminal width the region was painted at; a resize reflows every row,
    /// so a mismatch means `rows` no longer maps the screen — repaint from a
    /// fresh region instead of walking a stale count upward
    width: usize,
}

/// Hanging indent for wrapped input rows, matching the chrome column.
const WRAP_INDENT: &str = "  ";

/// Holds bracketed paste for the duration of one read, so a multi-line
/// paste arrives as one guarded chunk instead of a keystroke stream that
/// would submit at the first embedded newline.
struct PasteMode;

impl PasteMode {
    fn new() -> PasteMode {
        let mut out = std::io::stderr();
        let _ = write!(out, "{}", crate::theme::cursor().paste_on());
        let _ = out.flush();
        PasteMode
    }
}

impl Drop for PasteMode {
    fn drop(&mut self) {
        let mut out = std::io::stderr();
        let _ = write!(out, "{}", crate::theme::cursor().paste_off());
        let _ = out.flush();
    }
}

/// Holds the kitty keyboard protocol's disambiguate flag for one read, so
/// shift/ctrl+enter arrive as CSI-u keys instead of an indistinguishable
/// `\r`. Terminals that never heard of the protocol ignore the push — it is
/// never queried — and every other key keeps its legacy encoding. Scoped to
/// the editor: the watcher and approval prompts keep plain-ESC semantics.
struct KittyKeys;

impl KittyKeys {
    fn new() -> KittyKeys {
        let mut out = std::io::stderr();
        let _ = write!(out, "{}", crate::theme::cursor().kitty_push());
        let _ = out.flush();
        KittyKeys
    }
}

impl Drop for KittyKeys {
    fn drop(&mut self) {
        let mut out = std::io::stderr();
        let _ = write!(out, "{}", crate::theme::cursor().kitty_pop()); // pop exactly the flags we pushed
        let _ = out.flush();
    }
}

/// Holds the focus-report mode for the duration of one reader's life, so
/// `ESC[I`/`ESC[O` reports arrive and fold into `term::set_focused` —
/// the state notification gating reads. Enabling it also assumes the
/// terminal is focused (terminals report only changes, never the state at
/// mode-enable). Terminals that never heard of the mode ignore it, and
/// reporting turns off with the reader: a child process (editor, pager)
/// must not inherit focus reports on its stdin. Not re-entrant across
/// threads: the watcher stops around approval prompts, which read the same
/// stdin.
struct FocusReport;

impl FocusReport {
    fn new() -> FocusReport {
        let _guard = crate::theme::row_lock().lock();
        let on = crate::theme::cursor().focus_on();
        if !on.is_empty() {
            // The mode is live on a real terminal: take the prompt we are
            // about to draw as focused, and let an `ESC[O` correct it.
            super::assume_focused();
        }
        let mut out = std::io::stderr();
        let _ = write!(out, "{on}");
        let _ = out.flush();
        FocusReport
    }
}

impl Drop for FocusReport {
    fn drop(&mut self) {
        let _guard = crate::theme::row_lock().lock();
        let mut out = std::io::stderr();
        let _ = write!(out, "{}", crate::theme::cursor().focus_off());
        let _ = out.flush();
    }
}

impl InputLine {
    fn new() -> InputLine {
        InputLine { rows: 0, width: 0 }
    }

    fn draw(
        &mut self,
        out: &mut std::io::Stderr,
        prompt: &str,
        buf: &str,
        cursor: usize,
        is_command: &dyn Fn(&str) -> bool,
    ) {
        let cols = crate::term::columns();
        if self.rows > 0 && self.width != 0 && self.width != cols {
            let _ = writeln!(out);
            self.rows = 0;
        }
        self.width = cols;
        let c = crate::theme::cursor();
        if self.rows > 0 {
            let _ = write!(out, "{}", c.up(self.rows));
        }
        let _ = write!(out, "\r");
        let (crow, ccol, last) = self.render(out, prompt, buf, cursor, is_command);
        let _ = write!(out, "{}", c.clear_below());
        if last > crow {
            let _ = write!(out, "{}", c.up(last - crow));
        }
        let _ = write!(out, "{}", c.column(ccol + 1));
        let _ = out.flush();
        self.rows = crow;
    }

    /// Final render plus newline; the region ends and row tracking resets.
    fn settle(
        &mut self,
        out: &mut std::io::Stderr,
        prompt: &str,
        buf: &str,
        is_command: &dyn Fn(&str) -> bool,
    ) {
        if self.rows > 0 {
            let _ = write!(out, "{}", crate::theme::cursor().up(self.rows));
        }
        let _ = write!(out, "\r");
        let _ = self.render(out, prompt, buf, buf.len(), is_command);
        let _ = writeln!(out);
        let _ = out.flush();
        self.rows = 0;
    }

    /// Paint prompt + buffer as hard-wrapped rows (the prompt fills the
    /// first row, a hanging indent every wrapped one) so the terminal never
    /// soft-wraps. Returns (cursor row, cursor col, last row).
    fn render(
        &mut self,
        out: &mut std::io::Stderr,
        prompt: &str,
        buf: &str,
        cursor: usize,
        is_command: &dyn Fn(&str) -> bool,
    ) -> (usize, usize, usize) {
        // slash commands and `!` shell lines echo bold, distinguishing them
        // from ordinary task text — the REPL decides what counts as a
        // command (registered names only, so a `/`-prefixed path stays plain)
        let style = |s: &str| {
            if is_command(buf) {
                let p = crate::theme::err();
                format!("{}{s}{}", p.bold, p.reset)
            } else {
                s.to_string()
            }
        };
        let _ = write!(out, "{prompt}");
        let cols = crate::term::columns().max(4);
        // wrap one cell early: terminals disagree on immediate vs deferred
        // wrap at exactly the margin
        let limit = cols - 1;
        // every continuation row (hard newline or soft wrap) renders under
        // the prompt's own marker: same glyph, same styling, so a draft that
        // wrapped reads exactly like the first row instead of going gray
        let (mark, _) = prompt.split_once('>').unwrap_or(("", ""));
        let reset = &crate::theme::err().reset;
        let mut rows: Vec<String> = vec![String::new()];
        let mut col = crate::core::render_md::cell_width(prompt);
        let (mut crow, mut ccol) = (0, col);
        let mut off = 0usize;
        for ch in buf.chars() {
            let at_cursor = off == cursor;
            off += ch.len_utf8();
            if ch == '\n' {
                if at_cursor {
                    (crow, ccol) = (rows.len(), WRAP_INDENT.len());
                }
                rows.push(String::new());
                col = WRAP_INDENT.len();
                continue;
            }
            let w = crate::core::render_md::char_width(ch);
            if col + w > limit && col > WRAP_INDENT.len() {
                if at_cursor {
                    (crow, ccol) = (rows.len(), WRAP_INDENT.len());
                }
                rows.push(String::new());
                col = WRAP_INDENT.len();
            }
            if at_cursor {
                (crow, ccol) = (rows.len() - 1, col);
            }
            // a char wider than a whole row still places, never stalls
            rows.last_mut().expect("rows starts with one").push(ch);
            col += w;
        }
        if cursor >= buf.len() {
            (crow, ccol) = (rows.len() - 1, col);
        }
        for (i, r) in rows.iter().enumerate() {
            if i > 0 {
                let _ = write!(out, "\n{mark}>{reset} ");
            }
            let _ = write!(out, "{}", style(r));
        }
        (crow, ccol, rows.len() - 1)
    }
}

/// Longest common prefix of all candidates (bash-style prefix completion).
fn common_prefix(candidates: &[String]) -> String {
    let mut prefix = String::new();
    let Some(first) = candidates.first() else {
        return prefix;
    };
    let first_chars: Vec<char> = first.chars().collect();
    'outer: for (idx, ch) in first_chars.iter().enumerate() {
        for cand in candidates.iter().skip(1) {
            if cand.chars().nth(idx) != Some(*ch) {
                break 'outer;
            }
        }
        prefix.push(*ch);
    }
    prefix
}

/// Two-step approval input: type `y`/`n`/`a` to pick an option (echoed live),
/// then press Enter to confirm; a bare Enter keeps the default yes.
/// Esc/ctrl-c/ctrl-d cancels (Deny) immediately. The prompt banner has
/// already been printed by the caller. Returns None when raw mode is
/// unavailable (the caller fails closed).
pub fn read_approval_key() -> Option<ApprovalKey> {
    let mut term = RawTerm::acquire_console(1, 0)?;
    let mut choice: Option<ApprovalKey> = None;
    let key = loop {
        let b = match term.next_byte() {
            RawByte::Key(b) => b,
            RawByte::Timeout => continue,
        };
        match b {
            b'y' | b'Y' => {
                choice = Some(ApprovalKey::Yes);
                eprint!("y");
                let _ = std::io::stderr().flush();
            }
            b'n' | b'N' => {
                choice = Some(ApprovalKey::No);
                eprint!("n");
                let _ = std::io::stderr().flush();
            }
            b'a' | b'A' => {
                choice = Some(ApprovalKey::Always);
                eprint!("a");
                let _ = std::io::stderr().flush();
            }
            b'\r' | b'\n' => break choice.unwrap_or(ApprovalKey::Yes),
            0x03 => {
                // ctrl-c cancels the whole task, not just this call: raise
                // the interrupt flag the agent loop and streams poll
                crate::core::http::request_interrupt();
                break ApprovalKey::Deny;
            }
            0x1b => {
                // a lone ESC cancels like ctrl-c; arrow keys and other
                // sequences are swallowed whole so their tail bytes can
                // never land on the y/n/a answers
                match term.escape_seq() {
                    Some(Esc::Alone) | None => {
                        crate::core::http::request_interrupt();
                        break ApprovalKey::Deny;
                    }
                    Some(_) => continue,
                }
            }
            0x04 => break ApprovalKey::Deny,
            _ => {}
        }
    };
    // Close the prompt line: a typed letter is already echoed (just newline);
    // a bare Enter shows the default yes; a cancel shows the caret.
    match key {
        ApprovalKey::Deny => eprint!("^"),
        ApprovalKey::Yes if choice.is_none() => eprint!("y"),
        _ => {}
    }
    eprintln!();
    let _ = std::io::stderr().flush();
    Some(key)
}

#[derive(Clone, Copy)]
pub enum ApprovalKey {
    Yes,
    No,
    Always,
    Deny,
}

/// Extension trait so the escape-sequence parser stays local to the line
/// editor while the raw terminal backend lives in `platform`.
trait RawTermExt {
    /// After ESC: parse `[ X` / `[ N ~` sequences, CSI-u keys and
    /// alt-prefixed keys; alt+enter is a newline. A lone escape key
    /// (nothing followed within VTIME) is `Esc::Alone`.
    fn escape_seq(&mut self) -> Option<Esc>;
    /// Same parse, but the caller already consumed the byte after ESC
    /// (letting it tell a lone ESC — timeout — from a sequence).
    fn escape_from(&mut self, first: u8) -> Option<Esc>;
}

impl RawTermExt for RawTerm {
    fn escape_seq(&mut self) -> Option<Esc> {
        match self.next_byte() {
            RawByte::Key(b) => self.escape_from(b),
            // nothing followed the ESC within VTIME: the escape key itself
            RawByte::Timeout => Some(Esc::Alone),
        }
    }

    fn escape_from(&mut self, b: u8) -> Option<Esc> {
        if b == b'\r' || b == b'\n' {
            return Some(Esc::AltEnter);
        }
        if b != b'[' {
            // ESC + printable/backspace: a legacy alt+key report
            return ((0x20..=0x7f).contains(&b)).then_some(Esc::Key(u32::from(b), 3));
        }
        let mut params = [0u32; 2];
        let mut cur = 0;
        let final_byte;
        loop {
            let b = self.next_byte().key()?;
            if b.is_ascii_digit() {
                params[cur] = params[cur]
                    .saturating_mul(10)
                    .saturating_add(u32::from(b - b'0'));
            } else if b == b';' && cur == 0 {
                cur = 1;
            } else {
                final_byte = b;
                break;
            }
        }
        let [n1, n2] = params;
        csi_final(final_byte, n1, n2)
    }
}

/// Map one parsed CSI sequence's final byte and parameters to its meaning.
/// Pure so the mapping is testable without a terminal.
fn csi_final(final_byte: u8, n1: u32, n2: u32) -> Option<Esc> {
    match (final_byte, n2) {
        (b'A', 0) => Some(Esc::Up),
        (b'B', 0) => Some(Esc::Down),
        (b'C', 0) => Some(Esc::Right),
        (b'D', 0) => Some(Esc::Left),
        (b'H', 0) => Some(Esc::Home),
        (b'F', 0) => Some(Esc::End),
        // focus reports (no parameters): I gained, O lost. `O` as a
        // final byte never collides with the SS3 keypad prefix (`ESC O
        // P`): the `[` already took this branch.
        (b'I', 0) => Some(Esc::FocusIn),
        (b'O', 0) => Some(Esc::FocusOut),
        (b'C' | b'D', m) => Some(Esc::Mod(final_byte, m.clamp(1, 16) as u8)),
        (b'u', _) => Some(Esc::Key(n1.max(1), n2.clamp(1, 16) as u8)),
        (b'~', 0) => match n1 {
            3 => Some(Esc::Delete),
            200 => Some(Esc::PasteStart),
            201 => Some(Esc::PasteEnd),
            _ => None,
        },
        // modified delete (`3;m~`): treat as a modified backspace
        (b'~', m) if n1 == 3 => Some(Esc::Key(127, m.clamp(1, 16) as u8)),
        _ => None,
    }
}

/// Interactive menu: ↑/↓ move, enter selects, typing filters (space-
/// separated terms AND-match), esc/ctrl-c cancels. The list is deleted
/// from the screen once a choice is made and replaced by a one-line recap.
/// Returns the chosen index, or None when cancelled.
/// Upper bound on visible picker rows even on huge terminals.
const PICK_MAX_ROWS: usize = 12;

/// The one picker: an arrow-key menu with a type-to-filter line (the
/// fuzzy-filter `/resume` shape). Printable bytes append to the query —
/// UTF-8 accumulates across bytes, so CJK filters work — and
/// space-separated terms must all appear in an item case-insensitively.
/// Arrows move within the filtered view, enter returns the ORIGINAL index,
/// esc/ctrl-c cancel. `echo` prints the choice as one line afterwards.
pub fn pick(title: &str, items: &[String], echo: bool) -> Option<usize> {
    let mut term = RawTerm::acquire(1, 0)?;
    let mut out = std::io::stderr();
    // cap the menu height so long lists scroll instead of flooding the
    // screen: terminal height minus chrome, bounded to a readable window
    let budget = crate::term::rows()
        .saturating_sub(5)
        .clamp(5, PICK_MAX_ROWS);
    let mut query_bytes: Vec<u8> = Vec::new();
    let mut matched: Vec<usize> = (0..items.len()).collect();
    let mut sel = 0usize;
    let mut top = 0usize;

    let apply_filter = |query: &str, matched: &mut Vec<usize>| {
        let terms: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
        *matched = items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                let lower = item.to_lowercase();
                terms.iter().all(|t| lower.contains(t.as_str()))
            })
            .map(|(i, _)| i)
            .collect();
    };

    // the "· N more ↑" indicator appears only once the window moved down,
    // so the menu grows by one line on the first scroll; erase works off
    // the row count of the last draw
    let draw = |out: &mut std::io::Stderr,
                matched: &[usize],
                sel: usize,
                top: usize,
                query: &str|
     -> usize {
        let _ = writeln!(out, "{title}");
        let mut printed = 1;
        let visible = matched.len().min(budget);
        if matched.is_empty() {
            let _ = writeln!(
                out,
                "{}  (no matches — keep typing or backspace){}",
                crate::theme::err().gray,
                crate::theme::err().reset
            );
            printed += 1;
        } else {
            if top > 0 {
                let _ = writeln!(
                    out,
                    "{}  · {top} more ↑{}",
                    crate::theme::err().dim,
                    crate::theme::err().reset
                );
                printed += 1;
            }
            for (r, idx) in matched[top..top + visible].iter().enumerate() {
                let _ = write!(out, "{}", row(&items[*idx], top + r == sel));
                printed += 1;
            }
            if matched.len() > top + visible {
                let _ = writeln!(
                    out,
                    "{}  · {} more ↓{}",
                    crate::theme::err().gray,
                    matched.len() - top - visible,
                    crate::theme::err().reset
                );
                printed += 1;
            }
        }
        let _ = writeln!(
            out,
            "{}filter: {query}▏  (enter select · ↑↓ move · esc cancel){}",
            crate::theme::err().gray,
            crate::theme::err().reset
        );
        let _ = out.flush();
        printed + 1
    };
    let erase = |out: &mut std::io::Stderr, printed: usize, restore_cursor: bool| {
        // back above the menu, clear down; the cursor is only shown again on
        // exit so it never flickers/jumps between menu redraws
        let c = crate::theme::cursor();
        let _ = write!(out, "{}\r{}", c.up(printed), c.clear_below());
        if restore_cursor {
            let _ = write!(out, "{}", c.show());
        }
        let _ = out.flush();
    };

    // hide the cursor for the whole menu, before the first paint
    let _ = write!(out, "{}", crate::theme::cursor().hide());
    let _ = out.flush();
    let mut printed = draw(&mut out, &matched, sel, top, "");
    loop {
        let b = match term.next_byte() {
            RawByte::Key(b) => b,
            RawByte::Timeout => continue,
        };
        match b {
            b'\r' | b'\n' => {
                if matched.is_empty() {
                    continue;
                }
                erase(&mut out, printed, true);
                if echo {
                    let _ = writeln!(
                        out,
                        "{}{title}{} {}",
                        crate::theme::err().dim,
                        crate::theme::err().reset,
                        items[matched[sel]]
                    );
                }
                return Some(matched[sel]);
            }
            0x08 | 0x7f => {
                crate::core::text::pop_utf8_char(&mut query_bytes);
            }
            0x03 | 0x04 => {
                erase(&mut out, printed, true);
                return None;
            }
            0x1b => {
                let delta = match term.escape_seq() {
                    Some(Esc::Up) => -1i64,
                    Some(Esc::Down) => 1,
                    Some(Esc::Alone) | None => {
                        // esc (or a lone escape key) closes the menu
                        erase(&mut out, printed, true);
                        return None;
                    }
                    // edit keys (home/end/delete/arrows) and F-keys are not
                    // bound in the menu: ignore them instead of cancelling
                    // and throwing the selection away
                    Some(_) => continue,
                };
                if matched.is_empty() {
                    continue;
                }
                let n = matched.len();
                sel = ((sel as i64 + delta).rem_euclid(n as i64)) as usize;
                top = clamp_top(sel, matched.len(), budget, top);
                erase(&mut out, printed, false);
                printed = draw(
                    &mut out,
                    &matched,
                    sel,
                    top,
                    &String::from_utf8_lossy(&query_bytes),
                );
                continue;
            }
            b if b >= 0x20 => query_bytes.push(b),
            _ => continue,
        }
        // the query changed (enter handled above): refilter and redraw
        let query = String::from_utf8_lossy(&query_bytes).into_owned();
        apply_filter(&query, &mut matched);
        sel = sel.min(matched.len().saturating_sub(1));
        // the shrunken list may no longer hold the old window: clamp it
        // fully (a top past the end would panic the row slice), not just
        // under the selection
        top = clamp_top(sel, matched.len(), budget, top);
        erase(&mut out, printed, false);
        printed = draw(&mut out, &matched, sel, top, &query);
    }
}

/// One rendered menu row; selected rows get the bold cursor marker.
fn row(item: &str, selected: bool) -> String {
    format!("{}\n", row_body(item, selected))
}

fn row_body(item: &str, selected: bool) -> String {
    // marker + space + item must fit one line or the cursor math breaks
    let width = crate::term::columns().saturating_sub(1);
    let shown = crate::core::render_md::truncate_cells(item, width.saturating_sub(2));
    if selected {
        let p = crate::theme::err();
        format!("{}❯ {shown}{}", p.bold, p.reset)
    } else {
        let p = crate::theme::err();
        format!("{}  {shown}{}", p.dim, p.reset)
    }
}

/// Keep the scroll window honest: the selection stays visible, the window
/// never starts past the list's end, and it never spans past the last item
/// when the list fits the budget — both the arrow path and the filter path
/// shrink lists, and a stale `top` would panic the row slice.
fn clamp_top(sel: usize, len: usize, budget: usize, top: usize) -> usize {
    let visible = len.min(budget);
    let mut top = top.min(len.saturating_sub(visible));
    if sel < top {
        top = sel;
    } else if visible > 0 && sel >= top + visible {
        top = sel + 1 - visible;
    }
    top
}

/// Watches stdin during a running task: a bare ESC (0x1b) requests the same
/// cooperative interrupt as ctrl-c; every other keystroke is swallowed.
/// There is no input surface while a task runs (no echo, no buffer, no
/// queue): typing mid-run does nothing, and the next message is composed
/// after the task ends in the full line editor. Polls with VMIN=0/VTIME=1
/// so stop() joins within ~100ms; restores cooked mode on the way out.
pub struct KeyWatcher {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl KeyWatcher {
    pub fn start() -> KeyWatcher {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let Some(mut term) = RawTerm::acquire(1, 0) else {
            return KeyWatcher { stop, handle: None };
        };
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            // focus reports ride the watcher's own stdin read: the mode is
            // held exactly while this thread reads the terminal, never while
            // a child process owns it
            let _focus = FocusReport::new();
            loop {
                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                match term.next_byte() {
                    RawByte::Timeout => continue,
                    RawByte::Key(b) => {
                        // any non-interrupt keystroke means the user is
                        // watching: flush the pacing backlog so they see
                        // everything streamed so far, right now
                        if !matches!(b, 0x03 | 0x1b) {
                            super::screen()
                                .flush_now
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        match b {
                            // raw mode disables ISIG, so ctrl-c arrives here
                            // as 0x03
                            0x03 => crate::core::http::request_interrupt(),
                            // a lone ESC interrupts like ctrl-c, but arrow and
                            // edit keys also start with ESC — swallow whole
                            // sequences so their tails cannot raise the flag;
                            // focus reports update the flag instead
                            0x1b => match term.escape_seq() {
                                Some(Esc::FocusIn) => super::set_focused(true),
                                Some(Esc::FocusOut) => super::set_focused(false),
                                None | Some(Esc::Alone) => crate::core::http::request_interrupt(),
                                _ => {}
                            },
                            _ => {}
                        }
                    }
                }
            }
            drop(term); // restores cooked mode
        });
        KeyWatcher {
            stop,
            handle: Some(handle),
        }
    }

    /// Stop the watcher and join its thread. Keystrokes typed mid-run were
    /// swallowed by design, so there is nothing to hand back.
    pub fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for KeyWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn plain_read(prompt: &str) -> LineResult {
    use std::io::BufRead;
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let n = std::io::stdin().lock().read_line(&mut line).unwrap_or(0);
    if n == 0 {
        return LineResult::Eof;
    }
    LineResult::Line(line.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_breaks_after_a_lone_trailing_backslash() {
        assert!(enter_breaks_line("foo\\"));
        assert!(enter_breaks_line("\\"));
        assert!(enter_breaks_line("foo\nbar\\"));
        assert!(!enter_breaks_line("foo"));
        assert!(!enter_breaks_line("foo\\\\")); // doubled submits literally
    }

    #[test]
    fn focus_reports_map_to_their_variants_and_stay_parameterless() {
        assert!(matches!(csi_final(b'I', 0, 0), Some(Esc::FocusIn)));
        assert!(matches!(csi_final(b'O', 0, 0), Some(Esc::FocusOut)));
        // a parameterized I/O final byte is not a focus report
        assert!(csi_final(b'I', 1, 5).is_none());
        assert!(csi_final(b'O', 0, 5).is_none());
    }

    #[test]
    fn recall_only_while_browsing_empty_or_parked_at_start() {
        assert!(recall_on_up("", 0, false));
        assert!(recall_on_up("", 0, true));
        assert!(recall_on_up("abc", 0, false));
        assert!(!recall_on_up("abc", 1, false));
        assert!(!recall_on_up("ab\ncd", 2, false)); // start of the second line
        assert!(recall_on_up("ab\ncd", 3, true)); // already browsing
    }

    #[test]
    fn a_shrunken_filter_result_never_leaves_the_window_past_the_end() {
        // scrolled to top=2 in a 30-item window of 12, then the filter
        // leaves 5 matches: the window must start at 0 — the old code kept
        // top=2 and the row slice panicked on matched[2..7]
        assert_eq!(clamp_top(4, 5, 12, 2), 0);
        // a selection beyond the shrunken window's end pulls it down
        assert_eq!(clamp_top(9, 10, 5, 8), 5);
        // the window itself never opens past the list's end
        assert_eq!(clamp_top(0, 3, 12, 2), 0);
        assert_eq!(clamp_top(0, 0, 12, 2), 0);
        // unchanged cases: a selection inside the window stays put
        assert_eq!(clamp_top(3, 30, 12, 2), 2);
        assert_eq!(clamp_top(2, 30, 12, 8), 2);
        assert_eq!(clamp_top(11, 30, 12, 0), 0);
        assert_eq!(clamp_top(12, 30, 12, 0), 1);
    }

    #[test]
    fn up_line_keeps_the_preferred_column() {
        let buf = "ab\ncdef\nghi";
        // from the end of "cdef" (cursor 7) up onto "ab" clamps to its end
        assert_eq!(up_line(buf, 7, None), 2);
        // a nav.preferred column beyond the target line clamps to its end
        assert_eq!(up_line(buf, 7, Some(9)), 2);
        // a nav.preferred column short of the line lands mid-line
        assert_eq!(up_line(buf, buf.len(), Some(1)), 4); // 'd' in "cdef"
        // the top line parks at its start
        assert_eq!(up_line(buf, 2, None), 0);
    }

    #[test]
    fn down_line_clamps_and_parks() {
        let buf = "abcd\nef\n";
        // from col 3 of line 0 down onto "ef" (2 chars) clamps to its end
        assert_eq!(down_line(buf, 3, None), 7);
        // the last line parks at the buffer end
        assert_eq!(down_line(buf, 7, None), buf.len());
    }

    #[test]
    fn word_motion_skips_whitespace_runs() {
        let buf = "foo  bar baz";
        assert_eq!(word_back(buf, buf.len()), 9); // back onto "baz" start
        assert_eq!(word_back(buf, 9), 5); // then over the gap onto "foo" start
        assert_eq!(word_back(buf, 3), 0);
        assert_eq!(word_fwd(buf, 0), 3);
        assert_eq!(word_fwd(buf, 3), 8); // skips the gap, ends after "bar"
    }

    #[test]
    fn line_bounds_track_multiline_cursors() {
        let buf = "one\ntwo\nthree";
        assert_eq!(line_start(buf, 5), 4);
        assert_eq!(line_end(buf, 5), 7);
        assert_eq!(char_col(buf, 6), 2);
        assert_eq!(col_offset("two", 9), 3); // clamps past the end
    }

    #[test]
    fn big_pastes_placeholder_small_ones_do_not() {
        assert!(!should_placeholder("short text"));
        assert!(!should_placeholder(&format!("{}l", "l\n".repeat(9)))); // 10 lines
        assert!(should_placeholder(&format!("{}x", "l\n".repeat(10)))); // 11
        assert!(!should_placeholder(&"a".repeat(1000)));
        assert!(should_placeholder(&"a".repeat(1001)));
    }

    #[test]
    fn paste_tokens_expand_and_vanish() {
        let chunk = "x\n".repeat(20);
        let token = paste_token(1, &chunk);
        assert!(token.contains("+21 lines"));
        let pastes = vec![(token.clone(), chunk)];
        let buf = format!("before {} after", token);
        assert_eq!(
            expand_pastes(&buf, &pastes),
            format!("before {} after", "x\n".repeat(20))
        );
        // a token whose text was deleted leaves nothing behind
        assert_eq!(expand_pastes("plain", &pastes), "plain");
        // token text is atomic: its span covers the interior
        let inside = format!("pre {}", token);
        let off = inside.find(&token).unwrap() + 2;
        assert!(token_span(&inside, &pastes, off).is_some());
        assert!(token_span(&inside, &pastes, 0).is_none());
    }

    #[test]
    fn history_records_skip_blanks_and_adjacent_duplicates() {
        let mut hist: Vec<String> = vec!["old".into()];
        assert!(record_history(&mut hist, "one"));
        assert!(!record_history(&mut hist, "one")); // adjacent dup
        assert!(!record_history(&mut hist, "  "));
        assert!(record_history(&mut hist, "two"));
        assert_eq!(hist, ["old", "one", "two"]);
    }

    #[test]
    fn history_file_round_trims_and_loads_the_tail() {
        let dir = crate::core::testutil::scratch_dir("hist");
        let path = dir.join("history.jsonl");
        append_history_to(&path, "first");
        append_history_to(&path, "second");
        assert_eq!(load_history_from(&path), ["first", "second"]);
        // over the file cap: trimmed to the soft cap, newest kept
        for i in 0..(HISTORY_FILE_LIMIT + 5) {
            append_history_to(&path, &format!("e{i}"));
        }
        let loaded = load_history_from(&path);
        assert_eq!(loaded.len(), HISTORY_LIMIT);
        // 2007 lines on disk -> kept e405..e2004 -> window is the last 200
        assert_eq!(loaded.first().map(String::as_str), Some("e1805"));
        assert_eq!(
            loaded.last().cloned(),
            Some(format!("e{}", HISTORY_FILE_LIMIT + 4))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn history_trim_keeps_the_file_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::core::testutil::scratch_dir("hist");
        let path = dir.join("history.jsonl");
        for i in 0..(HISTORY_FILE_LIMIT + 5) {
            append_history_to(&path, &format!("e{i}"));
        }
        // the trim rewrite must not widen the mode: this file is the record
        // of everything the user typed
        load_history_from(&path);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "trim rewrote history.jsonl non-private");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
