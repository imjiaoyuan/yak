//! gitignore matching engine and gitignore-aware file walking, shared by
//! `llm pack` and the agent's grep/glob tools.

use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Build the ignore scopes for a tree rooted at `root`: ancestor .gitignore
/// files (outermost first, up to and including the git root, never above
/// it), plus the always-on builtin set. Nested .gitignore files are picked
/// up during the walk.
pub(crate) fn scopes_for(root: &Path) -> Vec<Rc<IgnoreScope>> {
    let mut scopes: Vec<Rc<IgnoreScope>> = Vec::new();
    let mut dir = if root.is_dir() {
        root
    } else {
        root.parent().unwrap_or(root)
    };
    let mut gitignore_dirs = vec![dir.to_path_buf()];
    if !dir.join(".git").exists() {
        while let Some(parent) = dir.parent() {
            gitignore_dirs.push(parent.to_path_buf());
            dir = parent;
            if dir.join(".git").exists() {
                break;
            }
        }
    }
    for d in gitignore_dirs.iter().rev() {
        if let Some(scope) = IgnoreScope::load(d) {
            scopes.push(Rc::new(scope));
        }
    }
    scopes.push(Rc::new(IgnoreScope::builtin()));
    scopes
}

/// Depth-first walk collecting non-ignored files under `dir`, relative to
/// `root`, honoring nested .gitignore scopes.
pub(crate) fn collect_files(
    dir: &Path,
    root: &Path,
    scopes: &[Rc<IgnoreScope>],
    out: &mut Vec<PathBuf>,
) {
    // nested .gitignore: scoped to this directory, popped on the way out;
    // scopes clone as Rc pointers, not pattern vectors
    let pushed = IgnoreScope::load(dir).map(|scope| {
        let mut all = scopes.to_vec();
        all.push(Rc::new(scope));
        all
    });
    let effective: &[Rc<IgnoreScope>] = pushed.as_deref().unwrap_or(scopes);
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.filter_map(|e| e.ok()) {
        let path = e.path();
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        // the dirent's own type: a symlink to a directory is a file here, so
        // `ln -s ..` cycles cannot loop the walk (and each entry saves the
        // stat that path.is_dir() would pay)
        let is_dir = e.file_type().is_ok_and(|ft| ft.is_dir());
        if is_ignored(effective, &rel, is_dir) {
            continue;
        }
        if is_dir {
            collect_files(&path, root, effective, out);
        } else {
            out.push(path);
        }
    }
}

/// One .gitignore file's patterns, anchored at its directory.
#[derive(Clone)]
pub(crate) struct IgnoreScope {
    patterns: Vec<Pattern>,
}

/// A compiled glob pattern, ready to match without re-parsing (the glob tool
/// checks one pattern against every file in a tree).
#[derive(Clone)]
pub(crate) struct Pattern {
    segments: Vec<String>,
    /// `["**", ...segments]` for unanchored patterns, precomputed so the
    /// per-file match path allocates nothing
    star_segments: Vec<String>,
    negated: bool,
    dir_only: bool,
    anchored: bool,
}

impl IgnoreScope {
    pub(crate) fn load(dir: &Path) -> Option<IgnoreScope> {
        let text = std::fs::read_to_string(dir.join(".gitignore")).ok()?;
        Some(IgnoreScope {
            patterns: text.lines().filter_map(parse_pattern).collect(),
        })
    }

    /// the always-on defaults pack adds on top of gitignore files
    pub(crate) fn builtin() -> IgnoreScope {
        IgnoreScope {
            patterns: [".git/", "target/", "node_modules/"]
                .iter()
                .filter_map(|p| parse_pattern(p))
                .collect(),
        }
    }
}

pub(crate) fn parse_pattern(line: &str) -> Option<Pattern> {
    let line = line.trim_end();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (negated, body) = match line.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let (dir_only, body) = match body.strip_suffix('/') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let (anchored, body) = match body.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    // a pattern containing a slash anywhere else is anchored too
    let anchored = anchored || body.contains('/');
    let segments: Vec<String> = body.split('/').map(String::from).collect();
    if segments.is_empty() || segments.iter().all(|s| s.is_empty()) {
        return None;
    }
    let mut star_segments = vec!["**".to_string()];
    star_segments.extend(segments.iter().cloned());
    Some(Pattern {
        segments,
        star_segments,
        negated,
        dir_only,
        anchored,
    })
}

/// gitignore semantics: last matching pattern across scopes wins; deeper
/// scopes (and later lines) come after shallower ones.
pub(crate) fn is_ignored(scopes: &[Rc<IgnoreScope>], rel: &str, is_dir: bool) -> bool {
    let mut result = false;
    let segs: Vec<&str> = rel.split('/').collect();
    for scope in scopes {
        for pattern in &scope.patterns {
            if pattern_matches(pattern, &segs, is_dir) {
                result = !pattern.negated;
            }
        }
    }
    result
}

fn pattern_matches(pattern: &Pattern, segs: &[&str], is_dir: bool) -> bool {
    if pattern.dir_only && !is_dir {
        return false;
    }
    if pattern.anchored {
        glob_match(&pattern.segments, segs)
    } else {
        // unanchored: match the basename at any depth, i.e. **/pattern
        glob_match(&pattern.star_segments, segs)
    }
}

/// Match a compiled pattern against one `/`-separated relative path.
pub(crate) fn pattern_matches_path(pattern: &Pattern, rel: &str, is_dir: bool) -> bool {
    let segs: Vec<&str> = rel.split('/').collect();
    pattern_matches(pattern, &segs, is_dir)
}

/// Segment-wise glob: `**` spans segments, `*`/`?`/`[...]` stay within one.
pub(crate) fn glob_match(pat: &[String], path: &[&str]) -> bool {
    if pat.is_empty() {
        return path.is_empty();
    }
    if pat[0] == "**" {
        for skip in 0..=path.len() {
            if glob_match(&pat[1..], &path[skip..]) {
                return true;
            }
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    seg_match(&pat[0], path[0]) && glob_match(&pat[1..], &path[1..])
}

/// Segment matcher over `&str` slices: no per-call allocation (this runs for
/// every (file, pattern) pair during a tree walk).
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

/// Match a glob pattern against a `/`-separated relative path.
#[cfg(test)]
pub(crate) fn path_matches_pattern(pattern: &str, rel: &str) -> bool {
    let Some(parsed) = parse_pattern(pattern) else {
        return false;
    };
    pattern_matches_path(&parsed, rel, false)
}

#[cfg(test)]
mod gitignore_tests {
    use super::*;

    fn ignored_with(patterns: &[&str], path: &str, is_dir: bool) -> bool {
        let scope = IgnoreScope {
            patterns: patterns.iter().filter_map(|p| parse_pattern(p)).collect(),
        };
        is_ignored(&[Rc::new(scope)], path, is_dir)
    }

    #[test]
    fn scopes_include_git_root_but_stop_above_it() {
        let tmp = std::env::temp_dir().join(format!("llm-scopes-{}", std::process::id()));
        let repo = tmp.join("repo");
        let sub = repo.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".gitignore"), "secret.txt\n").unwrap();
        std::fs::write(tmp.join(".gitignore"), "OUTSIDE_MARKER\n").unwrap();

        let mentions = |scopes: &[Rc<IgnoreScope>], pat: &str| {
            scopes.iter().any(|s| {
                s.patterns
                    .iter()
                    .any(|p| p.segments.iter().any(|seg| seg == pat))
            })
        };
        let from_root = scopes_for(&repo);
        assert_eq!(from_root.len(), 2); // repo .gitignore + builtin
        assert!(
            !mentions(&from_root, "OUTSIDE_MARKER"),
            "nothing above the git root"
        );
        let from_sub = scopes_for(&sub);
        assert_eq!(from_sub.len(), 2); // src adds none, repo's is included
        assert!(
            mentions(&from_sub, "secret.txt"),
            "repo root .gitignore applies from subdirs"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn basics() {
        assert!(ignored_with(&["*.log"], "debug.log", false));
        assert!(ignored_with(&["*.log"], "debug.log", true)); // matches dirs too
        assert!(ignored_with(&["build/"], "build", true));
        assert!(!ignored_with(&["build/"], "build", false));
        assert!(!ignored_with(&["build/"], "build/output.txt", false)); // pruning handles files under
        assert!(ignored_with(&["/secret.txt"], "secret.txt", false));
        assert!(!ignored_with(&["/secret.txt"], "sub/secret.txt", false));
        assert!(ignored_with(&["docs"], "a/b/docs", true));
        assert!(ignored_with(&["docs"], "docs", true));
    }

    #[test]
    fn double_star() {
        assert!(ignored_with(&["**/temp"], "a/b/temp", false));
        assert!(ignored_with(&["**/temp"], "temp", false));
        assert!(ignored_with(&["a/**/b"], "a/x/y/b", false));
        assert!(ignored_with(&["a/**/b"], "a/b", false));
        assert!(!ignored_with(&["a/**/b"], "x/a/b", false));
        assert!(ignored_with(&["logs/**"], "logs/a/b.txt", false));
    }

    #[test]
    fn negation_last_wins() {
        assert!(!ignored_with(&["*.log", "!keep.log"], "keep.log", false));
        assert!(ignored_with(&["*.log", "!keep.log"], "other.log", false));
        assert!(ignored_with(&["!keep.log", "*.log"], "keep.log", false));
    }

    #[test]
    fn char_classes_and_question() {
        assert!(ignored_with(&["file?.txt"], "file1.txt", false));
        assert!(!ignored_with(&["file?.txt"], "file12.txt", false));
        assert!(ignored_with(&["[abc].txt"], "b.txt", false));
        assert!(!ignored_with(&["[abc].txt"], "d.txt", false));
        assert!(ignored_with(&["[a-c].txt"], "c.txt", false));
    }

    #[test]
    fn path_matches_unanchored_at_any_depth() {
        assert!(path_matches_pattern("*.rs", "src/main.rs"));
        assert!(path_matches_pattern("*.rs", "main.rs"));
        assert!(!path_matches_pattern("*.rs", "src/main.toml"));
        assert!(path_matches_pattern("src/**/*.rs", "src/a/b/c.rs"));
    }
}
