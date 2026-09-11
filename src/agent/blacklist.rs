//! Command blacklist: a gitignore-style file of forbidden shell commands,
//! matched against every command position the approval resolver surfaces.
//! Two homes, project winning by later lines: `.llm/blacklist` in the
//! project (nearest walking up) and `~/.llm/blacklist` for the user.
//!
//! This file is **additive**. The commands that must never run — privilege
//! escalation, filesystem/machine destruction — are hardcoded in
//! `approval.rs` and cannot be switched off from here; this file is how you
//! refuse *more* (`rm`, a deploy script, a repo-specific foot-gun).
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

    /// The default file content written on first start: the syntax, written
    /// out so the mechanism is discoverable, and nothing enforced. The
    /// commands that must never run live in `approval.rs` and cannot be
    /// turned off from here, so seeding them into this file would only
    /// suggest an edit that has no effect.
    pub fn default_file() -> String {
        String::from(
            "# llm command blacklist — one pattern per line\n\
             #\n\
             # Add a line to refuse a command. Privilege escalation and\n\
             # filesystem/machine destruction (sudo, mkfs, dd, shutdown, …)\n\
             # are already refused by the program itself; this file is how\n\
             # you refuse more.\n\
             #\n\
             # word pattern  : matches that command word anywhere in the line\n\
             # words pattern : matches the whole command segment\n\
             # globs: * ? [...] · ! re-allows (last match wins) · # comment\n\
             # deleting this file resets it to these comments; an empty file\n\
             # is the same, since the hardcoded refusals still apply\n\
             #\n\
             # examples — uncomment to use:\n\
             # rm\n\
             # git push --force*\n\
             # !rm -rf ./build\n",
        )
    }

    /// Write the default file when the user home has none.
    pub fn ensure_default() {
        let path = crate::core::config::user_dir().join("blacklist");
        if !path.exists() {
            let _ = std::fs::write(&path, Self::default_file());
        }
    }

    /// True when the command line hits a deny pattern (and no later allow
    /// pattern re-permits it). `positions` are the command words (already
    /// expanded past wrappers and `shell -c`) and `segments` the raw
    /// compound-command segments.
    pub fn denied(&self, segments: &[String], positions: &[String]) -> String {
        let segs: Vec<String> = segments.iter().map(|s| s.to_lowercase()).collect();
        let words: Vec<String> = positions.iter().map(|s| s.to_lowercase()).collect();
        // last matching entry wins (gitignore semantics): scan all, keep
        // the newest hit — an `!` allow line later in the file re-permits
        let mut hit = String::new();
        for entry in &self.entries {
            let matched = if entry.pattern.contains(char::is_whitespace) {
                segs.iter().any(|s| self.matches(&entry.pattern, s))
            } else {
                words.iter().any(|w| self.matches(&entry.pattern, w))
            };
            if matched {
                hit = if entry.allow {
                    String::new()
                } else {
                    entry.pattern.clone()
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
        assert!(
            !b.denied(&["sudo rm -rf /".into()], &["sudo".into(), "rm".into()])
                .is_empty()
        );
        assert_eq!(
            b.denied(
                &["echo hi | sudo tee /etc/hosts".into()],
                &["echo".into(), "sudo".into()]
            ),
            "sudo"
        );
        // the word must be a command position, not an argument
        assert_eq!(
            b.denied(&["grep rm notes.txt".into()], &["grep".into()]),
            String::new()
        );
    }

    #[test]
    fn segment_pattern_matches_the_whole_segment() {
        let b = bl("git push --force*");
        assert_eq!(
            b.denied(&["git push --force origin main".into()], &["git".into()]),
            "git push --force*"
        );
        assert_eq!(
            b.denied(&["git push origin".into()], &["git".into()]),
            String::new()
        );
    }

    #[test]
    fn allow_entry_beats_an_earlier_deny() {
        let b = bl("rm\n!rm -rf ./build");
        assert_eq!(
            b.denied(&["rm -rf ./build".into()], &["rm".into()]),
            String::new()
        );
        assert_eq!(b.denied(&["rm -rf /".into()], &["rm".into()]), "rm");
    }

    #[test]
    fn globs_and_comments() {
        let b = bl("# comment\n\nmkfs*\n  shred  \n");
        assert_eq!(
            b.denied(&["mkfs.ext4 /dev/sda".into()], &["mkfs.ext4".into()]),
            "mkfs*"
        );
        assert_eq!(b.denied(&["shred x".into()], &["shred".into()]), "shred");
        assert_eq!(b.denied(&["ls".into()], &["ls".into()]), String::new());
        assert_eq!(b.entries.len(), 2);
    }

    #[test]
    fn case_insensitive() {
        let b = bl("SUDO");
        assert_eq!(b.denied(&["sudo x".into()], &["sudo".into()]), "sudo");
    }

    #[test]
    fn default_file_is_comments_only() {
        // the seeded file documents the syntax and enforces nothing: the
        // refusals that must always hold are hardcoded in approval.rs, so
        // editing this file can only ever add to them
        let b = Blacklist::parse(&Blacklist::default_file());
        assert!(
            b.entries.is_empty(),
            "the default file must enforce nothing"
        );
        assert!(
            Blacklist::default_file().contains("# rm"),
            "the syntax should be shown as a commented example"
        );
        // a dangerous word as an *argument* is not a command position:
        // `init` must not catch `npm init` or `git init`
        let wild = bl("init");
        for (seg, words) in [("npm init -y", vec!["npm"]), ("git init", vec!["git"])] {
            assert!(
                wild.denied(
                    &[seg.to_string()],
                    &words.iter().map(|w| w.to_string()).collect::<Vec<_>>()
                )
                .is_empty(),
                "{seg} must not be denied"
            );
        }
    }
}
