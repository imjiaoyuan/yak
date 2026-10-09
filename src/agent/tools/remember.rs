use super::*;

/// The global-memory writer: when the user asks to remember or note something
/// durable ("remember that I like concise replies"), it lands as one dated line in the user
/// memory file named in the system prompt's `<user_memory>` block and is
/// injected into every future session. Write-tier: it mutates that one file
/// and runs no code.
pub(super) struct RememberTool;

const DESCRIPTION: &str = "Save one durable fact to the user's global memory (YAK.md), injected into every future session. Use when the user asks you to remember or note a preference, environment detail or long-term decision; not for task-specific details.";

impl Tool for RememberTool {
    fn name(&self) -> &str {
        "remember"
    }
    fn tier(&self) -> Tier {
        Tier::Write
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "The fact to remember, one line"}
            },
            "required": ["text"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        short(args["text"].as_str().unwrap_or("?"))
    }
    fn execute(&self, args: &Value, _cwd: &Path, _log: &mut dyn FnMut(ToolProgress)) -> ToolOutput {
        let text = args["text"].as_str().map(str::trim).unwrap_or("");
        if text.is_empty() {
            return ToolOutput::err("nothing to remember");
        }
        match crate::agent::memory::remember(text) {
            Ok(true) => ToolOutput::ok("noted — it will apply from the next session on"),
            Ok(false) => ToolOutput::ok("already in memory"),
            Err(e) => ToolOutput::err(format!("cannot write memory: {e}")),
        }
    }
}
