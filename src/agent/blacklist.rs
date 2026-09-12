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
//! A hit is a hard deny: no approval prompt, the model receives the reason.

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
    /// nothing; unreadable files degrade silently — a guard file must never
    /// take the agent down.
    pub fn load(cwd: &Path) -> Blacklist {
        let mut entries = Vec::new();
        // project last: its lines win by last-match semantics
        let user = crate::core::config::user_dir().join("blacklist");
        if let Ok(text) = std::fs::read_to_string(&user) {
            entries.extend(Blacklist::parse(&text).entries);
        }
        if let Some(dir) = crate::core::paths::nearest_dir_up(cwd, ".llm", true) {
            let project = dir.join("blacklist");
            if project != user
                && let Ok(text) = std::fs::read_to_string(&project)
            {
                entries.extend(Blacklist::parse(&text).entries);
            }
        }
        Blacklist { entries }
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
             # deleting this file resets it to these two rules\n\
             #\n\
             rm\n\
             git push --force*\n",
        )
    }

    /// Seed the user's file with the two default rules when there is none.
    pub fn ensure_default() {
        let path = crate::core::config::user_dir().join("blacklist");
        if !path.exists() {
            let _ = std::fs::write(&path, Self::default_file());
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
    /// depth). Delegates to the gitignore segment matcher.
    fn matches(&self, pattern: &str, word: &str) -> bool {
        crate::gitignore::word_matches(pattern, word)
    }
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
            Blacklist::default_file().contains("# deleting this file resets it to these two rules")
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
