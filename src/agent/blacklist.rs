//! Command blacklist: a gitignore-style file of forbidden shell commands,
//! matched against every command position the approval resolver surfaces.
//! Two homes, project winning by later lines: `.llm/blacklist` in the
//! project (nearest walking up) and `~/.llm/blacklist` for the user.
//!
//! This file is an **ask-list**, not a refusal list. The commands that must
//! never run — privilege escalation, filesystem/machine destruction — are
//! hardcoded in `approval.rs` and cannot be switched off from here; a
//! pattern matched here forces the approval prompt in either mode (yolo
//! included), so `rm` or a force-push always waits for a keystroke. `!`
//! re-allows a pattern; answering `a` spares it for the session.
//!
//! Semantics (deliberately simpler than gitignore — patterns match words,
//! not paths):
//! - one pattern per line; `#` comments and blanks are skipped
//! - `!pattern` re-allows (last matching line wins, so a project file can
//!   punch holes in the user one)
//! - a pattern with no whitespace matches a **single command word** at any
//!   position: `sudo` denies `sudo rm x` and `echo hi | sudo tee f`
//! - a pattern with whitespace matches the **whole segment** from its first
//!   word: `rm -rf /` denies `rm -rf /` but not `rm notes.txt`
//! - globs `*`, `?`, `[...]` work within a word: `mkfs*`, `git push --force*`
//!
//! A hit asks for approval — even in yolo; the command runs only after
//! the user allows it (`a` spares the pattern for the session).

use std::path::Path;

/// One blacklist entry, already lowered for case-insensitive matching.
#[derive(Debug, Clone)]
pub struct Entry {
    /// lowercase pattern body (no `!` prefix)
    pub pattern: String,
    /// `!` — re-allow (last match wins)
    pub allow: bool,
}

/// The outcome of matching one command line against the file.
#[derive(Debug, PartialEq, Eq)]
pub enum Match {
    /// no line matched
    None,
    /// the last matching line is a deny: the command asks for approval
    Deny(String),
    /// the last matching line is a `!` re-allow: the pattern is exempt
    Allow,
}

/// The compiled blacklist: entries in file order, user file first so
/// project lines win.
#[derive(Debug, Default, Clone)]
pub struct Blacklist {
    entries: Vec<Entry>,
}

impl Blacklist {
    /// Parse the text of one blacklist file.
    pub fn parse(text: &str) -> Blacklist {
        let mut entries = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (allow, body) = match line.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, line),
            };
            if body.is_empty() {
                continue;
            }
            entries.push(Entry {
                pattern: body.to_lowercase(),
                allow,
            });
        }
        Blacklist { entries }
    }

    /// Load both homes (user, then project). Missing files contribute
    /// nothing (deleting the seeded file is how its rules are reset); a file
    /// that exists but cannot be read is reported and skipped — a guard file
    /// must never take the agent down, but it must not vanish silently
    /// either.
    pub fn load(cwd: &Path) -> Blacklist {
        // project last: its lines win by last-match semantics
        let user = crate::core::config::user_dir().join("blacklist");
        let mut files = vec![user.clone()];
        if let Some(dir) = crate::core::paths::nearest_dir_up(cwd, ".llm", true) {
            let project = dir.join("blacklist");
            if project != user {
                files.push(project);
            }
        }
        let mut merged = Blacklist::default();
        for path in files {
            // a missing file contributes nothing (deleting it is how the
            // defaults are reset); an existing file that cannot be read is a
            // guard the user believes is on, so it says so
            if !path.exists() {
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) => {
                    eprintln!("Warning: cannot read {}: {e}", path.display());
                    continue;
                }
            };
            let one = Blacklist::parse(&text);
            merged.entries.extend(one.entries);
        }
        merged
    }

    /// The default file content written on first start: the two rules every
    /// install should confirm interactively (`rm`, force-pushes) plus the
    /// syntax, written out so the mechanism is discoverable. The commands
    /// that must never run live in `approval.rs` and cannot be turned off
    /// from here, which is why they are not seeded here.
    pub fn default_file() -> String {
        String::from(
            "# llm command ask-list — one pattern per line\n\
             #\n\
             # A command matched here always asks for your approval before\n\
             # running, even in yolo mode. The truly dangerous ones (sudo,\n\
             # mkfs, dd, shutdown, rm -rf /, …) are refused by the program\n\
             # itself and cannot be re-enabled; this file gates commands you\n\
             # want to confirm, not ban.\n\
             #\n\
             # word pattern  : matches that command word anywhere in the line\n\
             # words pattern : matches the whole command segment\n\
             # globs: * ? [...] · ! re-allows (last match wins) · # comment\n\
             # answer a at the prompt to spare a pattern for this session\n\
             # deleting this file resets it to these rules\n\
             #\n\
             rm\n\
             git push --force*\n",
        )
    }

    /// Seed the user's file with the two default rules when there is none.
    /// A failed seed says so: the guard is weaker than the user thinks, and
    /// a silent miss would only surface as a command that never asked.
    pub fn ensure_default() {
        let path = crate::core::config::user_dir().join("blacklist");
        if !path.exists()
            && let Err(e) = std::fs::write(&path, Self::default_file())
        {
            eprintln!("Warning: cannot write {}: {e}", path.display());
        }
    }

    /// Match the command line against the file. `positions` are the command
    /// words (already expanded past wrappers and `shell -c`) and `segments`
    /// the raw compound-command segments.
    pub fn evaluate(&self, segments: &[String], positions: &[String]) -> Match {
        let segs: Vec<String> = segments.iter().map(|s| s.to_lowercase()).collect();
        let words: Vec<String> = positions.iter().map(|s| s.to_lowercase()).collect();
        // last matching entry wins (gitignore semantics): scan all, keep
        // the newest hit — an `!` allow line later in the file re-permits
        let mut hit = Match::None;
        for entry in &self.entries {
            let matched = if entry.pattern.contains(char::is_whitespace) {
                segs.iter().any(|s| self.matches(&entry.pattern, s))
            } else {
                words.iter().any(|w| self.matches(&entry.pattern, w))
            };
            if matched {
                hit = if entry.allow {
                    Match::Allow
                } else {
                    Match::Deny(entry.pattern.clone())
                };
            }
        }
        hit
    }

    /// Word-level glob match: `*` `?` `[...]`, no `**` (words have no
    /// depth).
    fn matches(&self, pattern: &str, word: &str) -> bool {
        word_matches(pattern, word)
    }
}

/// Word-level glob: `*` `?` `[...]`, no `**` (words have no depth).
fn word_matches(pattern: &str, word: &str) -> bool {
    seg_match(pattern, word)
}

/// Segment matcher over `&str` slices: no per-call allocation (this runs for
/// every command word against every pattern).
fn seg_match(pat: &str, text: &str) -> bool {
    let Some(p0) = pat.chars().next() else {
        return text.is_empty();
    };
    let rest_pat = &pat[p0.len_utf8()..];
    match p0 {
        '*' => {
            seg_match(rest_pat, text)
                || match text.chars().next() {
                    Some(t0) => seg_match(pat, &text[t0.len_utf8()..]),
                    None => false,
                }
        }
        '?' => match text.chars().next() {
            Some(t0) => seg_match(rest_pat, &text[t0.len_utf8()..]),
            None => false,
        },
        '\\' if !rest_pat.is_empty() => {
            let esc = rest_pat.chars().next().expect("checked non-empty");
            match text.chars().next() {
                Some(t0) if t0 == esc => {
                    seg_match(&rest_pat[esc.len_utf8()..], &text[t0.len_utf8()..])
                }
                _ => false,
            }
        }
        '[' => {
            let Some((hit, after_class)) = match_class(rest_pat, text) else {
                return false; // unterminated class
            };
            match (hit, text.chars().next()) {
                (true, Some(t0)) => seg_match(after_class, &text[t0.len_utf8()..]),
                _ => false,
            }
        }
        c => match text.chars().next() {
            Some(t0) if t0 == c => seg_match(rest_pat, &text[t0.len_utf8()..]),
            _ => false,
        },
    }
}

/// Evaluate a `[...]` class (optional leading `!`/`^` negation, `a-z`
/// ranges, `]` literal when first) against `text`'s first char. Returns
/// (hit, pattern past the closing bracket), or None when it never closes.
fn match_class<'p>(pat: &'p str, text: &str) -> Option<(bool, &'p str)> {
    let t0 = text.chars().next();
    let mut idx = 0usize;
    let mut negate = false;
    if matches!(pat.chars().next(), Some('!') | Some('^')) {
        negate = true;
        idx += 1;
    }
    let mut hit = false;
    let mut first = true;
    loop {
        let Some(c) = pat[idx..].chars().next() else {
            return None; // unterminated class
        };
        if c == ']' && !first {
            idx += 1;
            break;
        }
        first = false;
        let after_lo = idx + c.len_utf8();
        // a range `a-z`: the '-' is one byte, so after_lo + 1 is a boundary
        if let (Some('-'), Some(hi)) = (
            pat[after_lo..].chars().next(),
            pat[after_lo + 1..].chars().next(),
        ) && hi != ']'
        {
            if let Some(t) = t0
                && t >= c
                && t <= hi
            {
                hit = true;
            }
            idx = after_lo + 1 + hi.len_utf8();
            continue;
        }
        if let Some(t) = t0
            && t == c
        {
            hit = true;
        }
        idx = after_lo;
    }
    Some((hit != negate, &pat[idx..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bl(text: &str) -> Blacklist {
        Blacklist::parse(text)
    }

    #[test]
    fn word_pattern_matches_any_command_position() {
        let b = bl("sudo\nrm");
        // both words hit; the later line (rm) wins — either deny is correct
        assert!(matches!(
            b.evaluate(&["sudo rm -rf /".into()], &["sudo".into(), "rm".into()]),
            Match::Deny(_)
        ));
        assert_eq!(
            b.evaluate(
                &["echo hi | sudo tee /etc/hosts".into()],
                &["echo".into(), "sudo".into()]
            ),
            Match::Deny("sudo".into())
        );
        // the word must be a command position, not an argument
        assert_eq!(
            b.evaluate(&["grep rm notes.txt".into()], &["grep".into()]),
            Match::None
        );
    }

    #[test]
    fn segment_pattern_matches_the_whole_segment() {
        let b = bl("git push --force*");
        assert_eq!(
            b.evaluate(&["git push --force origin main".into()], &["git".into()]),
            Match::Deny("git push --force*".into())
        );
        assert_eq!(
            b.evaluate(&["git push origin".into()], &["git".into()]),
            Match::None
        );
    }

    #[test]
    fn allow_entry_beats_an_earlier_deny() {
        let b = bl("rm\n!rm -rf ./build");
        assert_eq!(
            b.evaluate(&["rm -rf ./build".into()], &["rm".into()]),
            Match::Allow
        );
        assert_eq!(
            b.evaluate(&["rm -rf /".into()], &["rm".into()]),
            Match::Deny("rm".into())
        );
    }

    #[test]
    fn globs_and_comments() {
        let b = bl("# comment\n\nmkfs*\n  shred  \n");
        assert_eq!(
            b.evaluate(&["mkfs.ext4 /dev/sda".into()], &["mkfs.ext4".into()]),
            Match::Deny("mkfs*".into())
        );
        assert_eq!(
            b.evaluate(&["shred x".into()], &["shred".into()]),
            Match::Deny("shred".into())
        );
        assert_eq!(b.evaluate(&["ls".into()], &["ls".into()]), Match::None);
        assert_eq!(b.entries.len(), 2);
    }

    #[test]
    fn question_and_char_classes_match_within_a_word() {
        assert!(word_matches("file?.txt", "file1.txt"));
        assert!(!word_matches("file?.txt", "file12.txt"));
        assert!(word_matches("[abc].txt", "b.txt"));
        assert!(!word_matches("[abc].txt", "d.txt"));
        assert!(word_matches("[a-c].txt", "c.txt"));
        assert!(word_matches("rm", "rm"));
        assert!(!word_matches("rm", "rmdir"));
    }

    #[test]
    fn case_insensitive() {
        let b = bl("SUDO");
        assert_eq!(
            b.evaluate(&["sudo x".into()], &["sudo".into()]),
            Match::Deny("sudo".into())
        );
    }

    #[test]
    fn default_file_gates_rm_and_force_pushes() {
        // the seeded rules are the two every install should confirm: any rm
        // (the word, so rmdir stays free) and any force-push
        let b = Blacklist::parse(&Blacklist::default_file());
        assert_eq!(b.entries.len(), 2, "rm and git push --force*");
        assert_eq!(b.entries[0].pattern, "rm");
        assert!(!b.entries[0].allow);
        assert_eq!(b.entries[1].pattern, "git push --force*");
        assert!(
            Blacklist::default_file().contains("# deleting this file resets it to these rules")
        );
        // a dangerous word as an *argument* is not a command position:
        // `init` must not catch `npm init` or `git init`
        let wild = bl("init");
        for (seg, words) in [("npm init -y", vec!["npm"]), ("git init", vec!["git"])] {
            assert_eq!(
                wild.evaluate(
                    &[seg.to_string()],
                    &words.iter().map(|w| w.to_string()).collect::<Vec<_>>()
                ),
                Match::None,
                "{seg} must not be denied"
            );
        }
    }
}
