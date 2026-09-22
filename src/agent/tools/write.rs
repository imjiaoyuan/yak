use super::edit::change_hunks;
use super::*;

/// Write through the shared atomic helper: a crash or power loss mid-write
/// used to leave the target truncated — a real risk when the model writes a
/// long document in one call. An existing file's permissions are carried
/// over (the executable bit on edited scripts must survive).
pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    crate::core::fsx::write_atomic(path, bytes, None)
}

pub(super) struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn tier(&self) -> Tier {
        Tier::Write
    }
    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. \
         Automatically creates parent directories."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to write (relative or absolute)"},
                "content": {"type": "string", "description": "Content to write to the file"}
            },
            "required": ["path", "content"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        let bytes = args["content"].as_str().map(|c| c.len()).unwrap_or(0);
        format!("{} ({} bytes)", args["path"].as_str().unwrap_or("?"), bytes)
    }
    fn diff(&self, args: &Value, cwd: &Path) -> Option<String> {
        let path = resolve_path(cwd, args["path"].as_str().unwrap_or(""));
        let new = args["content"].as_str().unwrap_or("");
        match std::fs::read_to_string(&path) {
            // overwriting: one whole-file hunk
            Ok(original) => Some(change_hunks(
                &original,
                &[(0, original.len(), new)],
                2,
                DIFF_MAX_LINES,
            )),
            // a new file: the incoming content as additions
            Err(_) => {
                let mut out: Vec<String> = Vec::new();
                for l in new.split('\n').take(DIFF_MAX_LINES) {
                    out.push(format!("+ {l}"));
                }
                if new.split('\n').count() > DIFF_MAX_LINES {
                    out.push("  · more lines not shown".to_string());
                }
                Some(out.join("\n"))
            }
        }
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let path = resolve_path(cwd, args["path"].as_str().unwrap_or(""));
        let content = args["content"].as_str().unwrap_or("");
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return ToolOutput::err(format!("cannot create {}: {e}", parent.display()));
        }
        match write_atomic(&path, content.as_bytes()) {
            Ok(()) => ToolOutput::ok(format!(
                "wrote {} bytes to {}",
                content.len(),
                path.display()
            )),
            Err(e) => ToolOutput::err(format!("write failed: {e}")),
        }
    }
}
