use super::*;

/// Lines longer than this are truncated in a grep hit.
pub(super) const GREP_LINE_LIMIT: usize = 500;

/// Files larger than this are skipped by grep (reading them whole would
/// spike memory; the model can target them with bash instead).
pub(super) const GREP_MAX_FILE: u64 = 32 * 1024 * 1024;

pub(super) struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn description(&self) -> &str {
        "Search file contents. Literal substring by default; `regex: true` for a regular \
         expression (needs ripgrep). Respects .gitignore. Returns path:line:text matches."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Substring (or regex with `regex: true`) to find"},
                "path": {"type": "string", "description": "File or directory to search (default .)"},
                "glob": {"type": "string", "description": "Only search files matching this glob, e.g. *.rs"},
                "ignore_case": {"type": "boolean"},
                "context": {"type": "integer", "description": "Lines of context around each match"},
                "limit": {"type": "integer", "description": "Maximum matches (default 100)"},
                "regex": {"type": "boolean", "description": "Treat `pattern` as a regular expression via ripgrep (default false)"}
            },
            "required": ["pattern"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        format!("\"{}\"", args["pattern"].as_str().unwrap_or("?"))
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        if args["regex"].as_bool().unwrap_or(false) {
            return grep_regex(args, cwd);
        }
        let pattern = args["pattern"].as_str().unwrap_or("");
        let ignore_case = args["ignore_case"].as_bool().unwrap_or(false);
        let context = args["context"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().unwrap_or(100) as usize;
        let glob = args["glob"].as_str().and_then(parse_pattern);
        let needle = if ignore_case {
            pattern.to_lowercase()
        } else {
            pattern.to_string()
        };

        let root = resolve_path(cwd, args["path"].as_str().unwrap_or("."));
        let files = gather_files(&root, glob.as_ref());
        let mut matches: Vec<String> = Vec::new();
        let mut skipped = 0usize;
        'outer: for path in files {
            if crate::read::is_binary_path(&path) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.len() > GREP_MAX_FILE {
                skipped += 1;
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            // context ranges grow monotonically with the match order, so a
            // watermark replaces the per-line dedup set (which was O(n²))
            let mut printed_to = 0usize;
            let mut i = 0usize;
            while i < lines.len() {
                let hit = if ignore_case {
                    lines[i].to_lowercase().contains(&needle)
                } else {
                    lines[i].contains(needle.as_str())
                };
                if hit {
                    let lo = i.saturating_sub(context).max(printed_to);
                    let hi = (i + context).min(lines.len() - 1);
                    for (offset, line) in lines[lo..=hi].iter().enumerate() {
                        let mut l = line.to_string();
                        crate::core::text::truncate_ellipsis(&mut l, GREP_LINE_LIMIT);
                        matches.push(format!(
                            "{}:{}: {}",
                            display_rel(cwd, &path),
                            lo + offset + 1,
                            l
                        ));
                    }
                    printed_to = hi + 1;
                    if matches.len() >= limit {
                        break 'outer;
                    }
                    i += 1;
                } else {
                    i += 1;
                }
            }
        }
        if matches.is_empty() {
            return ToolOutput::ok("no matches\n");
        }
        let mut out = matches.join("\n");
        out.push('\n');
        if skipped > 0 {
            out.push_str(&format!(
                "\n[{skipped} file(s) over {GREP_MAX_FILE} bytes skipped; use bash]\n"
            ));
        }
        ToolOutput::ok(truncate_marked(&out, MAX_LINES, MAX_BYTES))
    }
}

/// Regex search, delegated to ripgrep (the tool the model was previously told
/// to reach for via bash). Keeping it inside the tool means a regex lookup no
/// longer forces the model out of the tool surface into a shell pipeline.
/// `rg` already honors .gitignore and prints `path:line:text` with
/// `--no-heading`, so the output matches the literal path verbatim.
fn grep_regex(args: &Value, cwd: &Path) -> ToolOutput {
    let pattern = args["pattern"].as_str().unwrap_or("");
    let limit = args["limit"].as_u64().unwrap_or(100) as usize;
    let mut cmd = std::process::Command::new("rg");
    cmd.arg("--no-heading")
        .arg("--line-number")
        .arg("--color")
        .arg("never");
    if args["ignore_case"].as_bool().unwrap_or(false) {
        cmd.arg("-i");
    }
    if let Some(c) = args["context"].as_u64().filter(|c| *c > 0) {
        cmd.arg("-C").arg(c.to_string());
    }
    if let Some(g) = args["glob"].as_str().filter(|g| !g.is_empty()) {
        cmd.arg("-g").arg(g);
    }
    cmd.arg("--")
        .arg(pattern)
        .arg(args["path"].as_str().unwrap_or("."));
    cmd.current_dir(cwd);
    let Ok(out) = cmd.output() else {
        return ToolOutput::err(
            "regex search needs ripgrep (`rg`) on PATH; install it or drop `regex`",
        );
    };
    // rg exits 2 on a bad pattern; 1 means no matches, which is not an error
    if out.status.code() == Some(2) {
        let msg = String::from_utf8_lossy(&out.stderr);
        return ToolOutput::err(truncate_marked(&msg, MAX_LINES, MAX_BYTES));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return ToolOutput::ok("no matches\n");
    }
    let more = lines.len().saturating_sub(limit);
    lines.truncate(limit);
    let mut out = lines.join("\n");
    if more > 0 {
        out.push_str(&format!("\n... +{more} more matches\n"));
    }
    out.push('\n');
    ToolOutput::ok(truncate_marked(&out, MAX_LINES, MAX_BYTES))
}

pub(super) struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn description(&self) -> &str {
        "Find files by glob pattern (e.g. src/**/*.rs). Respects .gitignore."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "Directory to search (default .)"},
                "limit": {"type": "integer", "description": "Maximum results (default 1000)"}
            },
            "required": ["pattern"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["pattern"].as_str().unwrap_or("?").to_string()
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let raw = args["pattern"].as_str().unwrap_or("");
        let limit = args["limit"].as_u64().unwrap_or(1000) as usize;
        let root = resolve_path(cwd, args["path"].as_str().unwrap_or("."));
        let files = gather_files(&root, None);
        // compile once, match per file (parsing per file allocated per entry)
        let Some(pattern) = parse_pattern(raw) else {
            return ToolOutput::err(format!("invalid glob pattern '{raw}'"));
        };
        let mut hits: Vec<String> = Vec::new();
        for path in files {
            let rel = display_rel(cwd, &path);
            if pattern_matches_path(&pattern, &rel, false) {
                hits.push(rel);
                if hits.len() >= limit {
                    break;
                }
            }
        }
        if hits.is_empty() {
            return ToolOutput::ok("no matches\n");
        }
        let mut out = hits.join("\n");
        out.push('\n');
        ToolOutput::ok(out)
    }
}

pub(super) fn gather_files(root: &Path, glob: Option<&crate::gitignore::Pattern>) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let scopes = scopes_for(root);
    let mut files = Vec::new();
    collect_files(root, root, &scopes, &mut files);
    if let Some(glob) = glob {
        files.retain(|f| {
            let rel = f
                .strip_prefix(root)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            pattern_matches_path(glob, &rel, false)
        });
    }
    files.sort();
    files
}
