use super::*;

pub(super) struct LsTool;

impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn description(&self) -> &str {
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for \
         directories. Includes dotfiles."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory to list (default .)"},
                "limit": {"type": "integer", "description": "Maximum entries (default 500)"}
            }
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["path"].as_str().unwrap_or(".").to_string()
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let limit = args["limit"].as_u64().unwrap_or(500) as usize;
        let dir = resolve_path(cwd, args["path"].as_str().unwrap_or("."));
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return ToolOutput::err(format!("cannot list {}", dir.display()));
        };
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if e.path().is_dir() {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .collect();
        names.sort();
        if names.len() > limit {
            let more = names.len() - limit;
            names.truncate(limit);
            names.push(format!("... ({more} more)"));
        }
        let mut out = names.join("\n");
        out.push('\n');
        ToolOutput::ok(out)
    }
}
