//! Hand-rolled raw-mode line editor for the agent REPL: history, cursor
//! movement and bash-style tab completion, backed by the platform RawTerm
//! implementation. Falls back to a plain read when the terminal cannot be put
//! in raw mode.

use crate::platform::{RawByte, RawTerm};
use std::io::Write;

pub enum LineResult {
    /// a completed line (without the trailing newline); may contain embedded
    /// newlines from alt+enter or backslash continuation
    Line(String),
    /// ctrl-d on an empty line
    Eof,
    /// ctrl-c: caller clears the line and re-prompts
    Interrupt,
}

const HISTORY_LIMIT: usize = 200;
const LISTING_LIMIT: usize = 16;

pub struct LineEditor {
    history: Vec<String>,
}

impl LineEditor {
    pub fn new() -> LineEditor {
        LineEditor {
            history: Vec::new(),
        }
    }

    /// Read one line with editing. Tab completes bash-style via `completer`:
    /// a single candidate is inserted, several extend to the common prefix
    /// and then list the options. Lines ending in a backslash continue onto
    /// the next input; alt+enter inserts a newline.
    pub fn read_line(
        &mut self,
        prompt: &str,
        help: &str,
        completer: &dyn Fn(&str) -> Vec<String>,
    ) -> LineResult {
        let mut term = match RawTerm::acquire(1, 0) {
            Some(t) => t,
            None => return plain_read(prompt),
        };
        let _paste = PasteMode::new();
        let mut out = std::io::stderr();
        let _ = write!(out, "{prompt}");
        let _ = out.flush();
        let mut line = InputLine::new();

        let mut buf = String::new();
        let mut cursor = 0usize; // byte offset into buf
        let mut history_index = self.history.len();
        loop {
            let b = loop {
                // a SIGINT that landed outside raw mode only sets the flag;
                // the poll timeout is the chance to notice it
                if crate::core::http::interrupted() {
                    buf.clear();
                    line.settle(&mut out, prompt, "");
                    return LineResult::Interrupt;
                }
                match term.next_byte() {
                    RawByte::Key(b) => break b,
                    RawByte::Timeout => {}
                }
            };
            match b {
                // enter submits. Both \r and \n: terminals and ptys with
                // ICRNL deliver the key as \n, so treating \n as "insert a
                // newline" would swallow every enter (the ctrl+j idea did
                // exactly that). alt+enter and paste insert newlines.
                b'\r' | b'\n' => {
                    // backslash at end of line: continue reading the same input
                    if buf.ends_with('\\') && !buf.ends_with("\\\\") {
                        buf.pop();
                        cursor = buf.len();
                        line.settle(&mut out, prompt, &buf);
                        let _ = write!(out, "\x1b[2m…\x1b[0m ");
                        let _ = out.flush();
                        continue;
                    }
                    if !buf.trim().is_empty() {
                        self.history.push(buf.clone());
                        if self.history.len() > HISTORY_LIMIT {
                            self.history.remove(0);
                        }
                    }
                    line.settle(&mut out, prompt, &buf);
                    return LineResult::Line(buf);
                }
                0x16 => {
                    // ctrl+v: pull the clipboard image as a temp-file path
                    // inserted into the line (the path auto-attaches on
                    // submit); a text clipboard pastes via the terminal's
                    // own bracketed paste instead
                    match crate::platform::paste_clipboard_image() {
                        Some(bytes) => {
                            let ext = match crate::core::attachments::sniff_mime(&bytes) {
                                Some("image/jpeg") => "jpg",
                                Some("image/gif") => "gif",
                                Some("image/webp") => "webp",
                                _ => "png",
                            };
                            let dir = crate::core::config::user_dir().join("tmp");
                            let _ = std::fs::create_dir_all(&dir);
                            let path =
                                dir.join(format!("paste-{}.{}", crate::core::db::ulid(), ext));
                            if std::fs::write(&path, &bytes).is_ok() {
                                let text = path.display().to_string();
                                buf.insert_str(cursor, &text);
                                cursor += text.len();
                                line.draw(&mut out, prompt, &buf, cursor);
                            }
                        }
                        None => {
                            let _ = writeln!(out, "\x1b[2m(no image on the clipboard)\x1b[0m");
                            line.rows = 0;
                            line.draw(&mut out, prompt, &buf, cursor);
                        }
                    }
                }
                0x0f => {
                    // ctrl+o: full help page
                    let _ = writeln!(out);
                    let _ = writeln!(out, "{help}");
                    line.rows = 0; // the region restarts below the help text
                    line.draw(&mut out, prompt, &buf, cursor);
                }
                0x03 => {
                    // ctrl-c clears the line; the caller treats two
                    // consecutive presses as "exit"
                    buf.clear();
                    line.settle(&mut out, prompt, "");
                    return LineResult::Interrupt;
                }
                0x04 => {
                    // ctrl-d: eof on empty, otherwise delete forward
                    if buf.is_empty() {
                        line.settle(&mut out, prompt, "");
                        return LineResult::Eof;
                    }
                    if cursor < buf.len() {
                        let rest = buf[cursor..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                        buf.replace_range(cursor..cursor + rest, "");
                        line.draw(&mut out, prompt, &buf, cursor);
                    }
                }
                0x15 => {
                    // ctrl-u clears before the cursor
                    buf.replace_range(..cursor, "");
                    cursor = 0;
                    line.draw(&mut out, prompt, &buf, cursor);
                }
                0x7f | 0x08 => {
                    // backspace: delete the char before the cursor
                    if cursor > 0 {
                        let mut start = cursor - 1;
                        while start > 0 && buf.as_bytes()[start] & 0xC0 == 0x80 {
                            start -= 1;
                        }
                        buf.replace_range(start..cursor, "");
                        cursor = start;
                        line.draw(&mut out, prompt, &buf, cursor);
                    }
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
                            line.draw(&mut out, prompt, &buf, cursor);
                        }
                        None if candidates.len() > 1 => {
                            let _ = writeln!(out);
                            let listing: Vec<&str> = candidates
                                .iter()
                                .map(|s| s.as_str())
                                .take(LISTING_LIMIT)
                                .collect();
                            let _ = writeln!(out, "\x1b[2m  {}\x1b[0m", listing.join("  "));
                            line.rows = 0; // the region restarts below the listing
                            line.draw(&mut out, prompt, &buf, cursor);
                        }
                        None => {}
                    }
                }
                0x1b => match term.escape_seq() {
                    Some(Esc::AltEnter) => {
                        buf.insert(cursor, '\n');
                        cursor = buf.len();
                        let _ = writeln!(out);
                        line.rows = 0; // the region restarts on the fresh row
                        line.draw(&mut out, prompt, &buf, cursor);
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
                            buf.insert_str(cursor, &chunk);
                            cursor += chunk.len();
                            line.draw(&mut out, prompt, &buf, cursor);
                        }
                    }
                    Some(Esc::PasteEnd) => {}
                    Some(Esc::Up) => {
                        if history_index > 0 {
                            history_index -= 1;
                            buf = self.history[history_index].clone();
                            cursor = buf.len();
                            line.draw(&mut out, prompt, &buf, cursor);
                        }
                    }
                    Some(Esc::Down) => {
                        if history_index + 1 < self.history.len() {
                            history_index += 1;
                            buf = self.history[history_index].clone();
                        } else {
                            history_index = self.history.len();
                            buf.clear();
                        }
                        cursor = buf.len();
                        line.draw(&mut out, prompt, &buf, cursor);
                    }
                    Some(Esc::Left) => {
                        while cursor > 0 && buf.as_bytes()[cursor - 1] & 0xC0 == 0x80 {
                            cursor -= 1;
                        }
                        cursor = cursor.saturating_sub(1);
                        line.draw(&mut out, prompt, &buf, cursor);
                    }
                    Some(Esc::Right) => {
                        if cursor < buf.len() {
                            cursor += 1;
                            while cursor < buf.len() && buf.as_bytes()[cursor] & 0xC0 == 0x80 {
                                cursor += 1;
                            }
                        }
                        line.draw(&mut out, prompt, &buf, cursor);
                    }
                    Some(Esc::Home) => {
                        cursor = 0;
                        line.draw(&mut out, prompt, &buf, cursor);
                    }
                    Some(Esc::End) => {
                        cursor = buf.len();
                        line.draw(&mut out, prompt, &buf, cursor);
                    }
                    Some(Esc::Delete) => {
                        // zero when the cursor sits at the end (nothing to delete)
                        let rest = buf[cursor..].chars().next().map_or(0, |c| c.len_utf8());
                        if rest > 0 {
                            buf.replace_range(cursor..cursor + rest, "");
                            line.draw(&mut out, prompt, &buf, cursor);
                        }
                    }
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
                        line.draw(&mut out, prompt, &buf, cursor);
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
    /// bracketed-paste start marker (`ESC[200~`)
    PasteStart,
    /// bracketed-paste end marker (`ESC[201~`)
    PasteEnd,
}

/// Redraw the input row.
/// The interactive input region (prompt + buffer), terminal-row aware so
/// wrapped input — CJK reaches the margin fast — redraws cleanly instead of
/// smearing rows: each draw returns to the region top, repaints, clears to
/// the screen end and parks the cursor by cell offset.
struct InputLine {
    /// row offset of the cursor within the region (how far up to return)
    rows: usize,
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
        let _ = write!(out, "\x1b[?2004h");
        let _ = out.flush();
        PasteMode
    }
}

impl Drop for PasteMode {
    fn drop(&mut self) {
        let mut out = std::io::stderr();
        let _ = write!(out, "\x1b[?2004l");
        let _ = out.flush();
    }
}

impl InputLine {
    fn new() -> InputLine {
        InputLine { rows: 0 }
    }

    fn draw(&mut self, out: &mut std::io::Stderr, prompt: &str, buf: &str, cursor: usize) {
        if self.rows > 0 {
            let _ = write!(out, "\x1b[{}A", self.rows);
        }
        let _ = write!(out, "\r");
        let (crow, ccol, last) = self.render(out, prompt, buf, cursor);
        let _ = write!(out, "\x1b[J");
        if last > crow {
            let _ = write!(out, "\x1b[{}A", last - crow);
        }
        let _ = write!(out, "\x1b[{}G", ccol + 1);
        let _ = out.flush();
        self.rows = crow;
    }

    /// Final render plus newline; the region ends and row tracking resets.
    fn settle(&mut self, out: &mut std::io::Stderr, prompt: &str, buf: &str) {
        if self.rows > 0 {
            let _ = write!(out, "\x1b[{}A", self.rows);
        }
        let _ = write!(out, "\r");
        let _ = self.render(out, prompt, buf, buf.len());
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
    ) -> (usize, usize, usize) {
        // slash commands echo bold, matching the `>` prompt
        let style = |s: &str| {
            if buf.starts_with('/') {
                format!("\x1b[1m{s}\x1b[0m")
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
        // a dim `>` prompt, matching the main `>` at column 0
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
            let w = crate::core::render_md::cell_width(&ch.to_string());
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
                let _ = write!(out, "\n\x1b[2m>\x1b[0m ");
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
    // raw mode disables echo: echo each accepted letter as it is typed so the
    // user sees their selection, but do not commit until Enter (or a cancel).
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
            0x03 | 0x04 | 0x1b => break ApprovalKey::Deny,
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
    /// After ESC: parse `[ X` / `[ N ~` sequences; alt+enter is a newline.
    fn escape_seq(&mut self) -> Option<Esc>;
}

impl RawTermExt for RawTerm {
    fn escape_seq(&mut self) -> Option<Esc> {
        let b = self.next_byte().key()?;
        if b == b'\r' || b == b'\n' {
            return Some(Esc::AltEnter);
        }
        if b != b'[' {
            return None;
        }
        let b = self.next_byte().key()?;
        match b {
            b'A' => Some(Esc::Up),
            b'B' => Some(Esc::Down),
            b'C' => Some(Esc::Right),
            b'D' => Some(Esc::Left),
            b'H' => Some(Esc::Home),
            b'F' => Some(Esc::End),
            b'0'..=b'9' => {
                let mut num: u32 = 0;
                let mut n = b;
                while n.is_ascii_digit() {
                    num = num * 10 + u32::from(n - b'0');
                    n = self.next_byte().key()?;
                }
                if n != b'~' {
                    return None;
                }
                match num {
                    3 => Some(Esc::Delete),
                    200 => Some(Esc::PasteStart),
                    201 => Some(Esc::PasteEnd),
                    _ => None,
                }
            }
            _ => None,
        }
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
                "\x1b[90m  (no matches — keep typing or backspace)\x1b[0m"
            );
            printed += 1;
        } else {
            if top > 0 {
                let _ = writeln!(out, "\x1b[90m  · {top} more ↑\x1b[0m");
                printed += 1;
            }
            for (r, idx) in matched[top..top + visible].iter().enumerate() {
                let _ = write!(out, "{}", row(&items[*idx], top + r == sel));
                printed += 1;
            }
            if matched.len() > top + visible {
                let _ = writeln!(
                    out,
                    "\x1b[90m  · {} more ↓\x1b[0m",
                    matched.len() - top - visible
                );
                printed += 1;
            }
        }
        let _ = writeln!(
            out,
            "\x1b[90mfilter: {query}▏  (enter select · ↑↓ move · esc cancel)\x1b[0m"
        );
        let _ = out.flush();
        printed + 1
    };
    let erase = |out: &mut std::io::Stderr, printed: usize, restore_cursor: bool| {
        // back above the menu, clear down; the cursor is only shown again on
        // exit so it never flickers/jumps between menu redraws
        let _ = write!(out, "\x1b[{printed}A\r\x1b[J");
        if restore_cursor {
            let _ = write!(out, "\x1b[?25h");
        }
        let _ = out.flush();
    };

    // hide the cursor for the whole menu, before the first paint
    let _ = write!(out, "\x1b[?25l");
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
                    let _ = writeln!(out, "\x1b[2m{title}\x1b[0m {}", items[matched[sel]]);
                }
                return Some(matched[sel]);
            }
            0x08 | 0x7f => {
                // pop one full char (skip UTF-8 continuation bytes)
                while let Some(&last) = query_bytes.last()
                    && last & 0xC0 == 0x80
                {
                    query_bytes.pop();
                }
                query_bytes.pop();
            }
            0x03 | 0x04 => {
                erase(&mut out, printed, true);
                return None;
            }
            0x1b => {
                let delta = match term.escape_seq() {
                    Some(Esc::Up) => -1i64,
                    Some(Esc::Down) => 1,
                    _ => {
                        erase(&mut out, printed, true);
                        return None;
                    }
                };
                if matched.is_empty() {
                    continue;
                }
                let n = matched.len();
                sel = ((sel as i64 + delta).rem_euclid(n as i64)) as usize;
                let visible = matched.len().min(budget);
                if sel < top {
                    top = sel;
                } else if sel >= top + visible {
                    top = sel + 1 - visible;
                }
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
        top = top.min(sel);
        erase(&mut out, printed, false);
        printed = draw(&mut out, &matched, sel, top, &query);
    }
}

/// Truncate to at most `max` terminal cells (CJK-aware), "…" on cut.
fn fit_cells(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut cells = 0usize;
    for c in text.chars() {
        let w = crate::core::render_md::char_width(c);
        if cells + w > max.saturating_sub(1) {
            out.push('…');
            return out;
        }
        out.push(c);
        cells += w;
    }
    out
}

/// One rendered menu row; selected rows get the bold cursor marker.
fn row(item: &str, selected: bool) -> String {
    format!("{}\n", row_body(item, selected))
}

fn row_body(item: &str, selected: bool) -> String {
    // marker + space + item must fit one line or the cursor math breaks
    let width = crate::term::columns().saturating_sub(1);
    let shown = fit_cells(item, width.saturating_sub(2));
    if selected {
        format!("\x1b[1m❯ {shown}\x1b[0m")
    } else {
        format!("\x1b[2m  {shown}\x1b[0m")
    }
}

/// Watches stdin during a running task: a bare ESC (0x1b) requests the same
/// cooperative interrupt as ctrl-c, and any line typed and entered is pushed
/// onto the steering queue shared with the session (the agent loop delivers
/// it to the model at the next tool-round boundary). Polls with
/// VMIN=0/VTIME=1 so stop() joins within ~100ms; restores cooked mode on the
/// way out.
pub struct KeyWatcher {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl KeyWatcher {
    pub fn start_with(queue: std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> KeyWatcher {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let Some(mut term) = RawTerm::acquire(1, 0) else {
            return KeyWatcher { stop, handle: None };
        };
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut buf: Vec<u8> = Vec::new();
            loop {
                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                match term.next_byte() {
                    RawByte::Timeout => continue,
                    RawByte::Key(b) => match b {
                        // raw mode disables ISIG, so ctrl-c arrives here as 0x03;
                        // an interrupt also discards the half-typed line
                        0x1b | 0x03 => {
                            buf.clear();
                            crate::core::http::request_interrupt();
                        }
                        // enter: queue the line. No per-character echo — it would
                        // interleave with the streaming answer and tear lines
                        // apart; this dim notice is the confirmation instead.
                        b'\r' | b'\n' => {
                            let line = String::from_utf8_lossy(&buf).trim().to_string();
                            if !line.is_empty() {
                                if let Ok(mut q) = queue.lock() {
                                    q.push(line.clone());
                                }
                                // clear the spinner frame first so the
                                // notice lands on its own line
                                eprint!("\r\x1b[2K");
                                eprintln!("\x1b[2mqueued: {line}\x1b[0m");
                            }
                            buf.clear();
                        }
                        0x7f | 0x08 => {
                            // pop one full char (skip UTF-8 continuation
                            // bytes, like the picker's filter does)
                            while let Some(&last) = buf.last()
                                && last & 0xC0 == 0x80
                            {
                                buf.pop();
                            }
                            buf.pop();
                        }
                        c if c >= 0x20 => buf.push(c),
                        _ => {}
                    },
                }
            }
            drop(term); // restores cooked mode
        });
        KeyWatcher {
            stop,
            handle: Some(handle),
        }
    }

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
mod tests {}
