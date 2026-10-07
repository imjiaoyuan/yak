use super::*;

/// Lines longer than this are truncated in a grep hit .
pub(super) const GREP_LINE_LIMIT: usize = 500;

pub(super) struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn description(&self) -> &str {
        "Search file contents for a pattern (needs ripgrep). Returns matching lines with file \
         paths and line numbers. Respects .gitignore."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Search pattern (regex or literal string)"},
                "path": {"type": "string", "description": "Directory or file to search (default .)"},
                "glob": {"type": "string", "description": "Only search files matching this glob, e.g. *.rs"},
                "ignoreCase": {"type": "boolean", "description": "Case-insensitive search"},
                "context": {"type": "integer", "description": "Lines of context around each match"},
                "limit": {"type": "integer", "description": "Max matches (default 100)"},
                "literal": {"type": "boolean", "description": "Treat pattern as a literal string"}
            },
            "required": ["pattern"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        format!("\"{}\"", args["pattern"].as_str().unwrap_or("?"))
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let pattern = args["pattern"].as_str().unwrap_or("");
        let limit = args["limit"].as_u64().unwrap_or(100) as usize;
        let root = resolve_path(cwd, args["path"].as_str().unwrap_or("."));
        let mut cmd = std::process::Command::new("rg");
        cmd.arg("--no-heading")
            .arg("--with-filename")
            .arg("--line-number")
            .arg("--color")
            .arg("never")
            .arg("--hidden");
        if args["literal"].as_bool().unwrap_or(false) {
            cmd.arg("--fixed-strings");
        }
        if args["ignoreCase"].as_bool().unwrap_or(false) {
            cmd.arg("-i");
        }
        if let Some(c) = args["context"].as_u64().filter(|c| *c > 0) {
            cmd.arg("-C").arg(c.to_string());
        }
        if let Some(g) = args["glob"].as_str().filter(|g| !g.is_empty()) {
            cmd.arg("-g").arg(g);
        }
        cmd.arg("--").arg(pattern).arg(&root);
        cmd.current_dir(cwd);
        let Ok(out) = cmd.output() else {
            return ToolOutput::err("grep needs ripgrep (`rg`) on PATH");
        };
        // rg exits 2 on a bad pattern; 1 means no matches, which is not an error
        if out.status.code() == Some(2) {
            let msg = String::from_utf8_lossy(&out.stderr);
            return ToolOutput::err(truncate_marked(&msg, MAX_LINES, MAX_BYTES));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            return ToolOutput::ok("No matches found");
        }
        let more = lines.len().saturating_sub(limit);
        lines.truncate(limit);
        let mut out = lines
            .into_iter()
            .map(|l| {
                // bound a minified line: the path:line prefix is short, so the
                // surviving ~500 chars are content, not path
                let mut l = l.to_string();
                crate::core::text::truncate_ellipsis(&mut l, GREP_LINE_LIMIT);
                l
            })
            .collect::<Vec<_>>()
            .join("\n");
        if more > 0 {
            out.push_str(&format!("\n... +{more} more matches"));
        }
        ToolOutput::ok(truncate_marked(&out, MAX_LINES, MAX_BYTES))
    }
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
        "Search for files by glob pattern. Returns matching file paths relative to the search \
         directory. Respects .gitignore. Needs ripgrep."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern to match files, e.g. '*.rs', '**/*.json', or 'src/**/*.rs'"},
                "path": {"type": "string", "description": "Directory to search in (default: current directory)"},
                "limit": {"type": "integer", "description": "Maximum number of results (default: 1000)"}
            },
            "required": ["pattern"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["pattern"].as_str().unwrap_or("?").to_string()
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let pattern = args["pattern"].as_str().unwrap_or("");
        let limit = args["limit"].as_u64().unwrap_or(1000) as usize;
        let root = resolve_path(cwd, args["path"].as_str().unwrap_or("."));
        // search cwd-relative so results read `src/x.rs`, not `./src/x.rs`
        let search = match root.strip_prefix(cwd) {
            Ok(rel) if rel.as_os_str().is_empty() => std::path::PathBuf::from("."),
            Ok(rel) => rel.to_path_buf(),
            Err(_) => root.clone(),
        };
        let mut cmd = std::process::Command::new("rg");
        cmd.arg("--files")
            .arg("--hidden")
            .arg("-g")
            .arg(pattern)
            .arg(&search);
        cmd.current_dir(cwd);
        let Ok(out) = cmd.output() else {
            return ToolOutput::err("glob needs ripgrep (`rg`) on PATH");
        };
        if out.status.code() == Some(2) {
            let msg = String::from_utf8_lossy(&out.stderr);
            return ToolOutput::err(truncate_marked(&msg, MAX_LINES, MAX_BYTES));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut hits: Vec<String> = Vec::new();
        for line in text.lines() {
            let l = line.trim();
            if l.is_empty() {
                continue;
            }
            let l = l.strip_prefix("./").unwrap_or(l);
            hits.push(l.replace('\\', "/"));
            if hits.len() >= limit {
                break;
            }
        }
        if hits.is_empty() {
            return ToolOutput::ok("No files found matching pattern");
        }
        let mut out = hits.join("\n");
        out.push('\n');
        ToolOutput::ok(out)
    }
}
