use super::*;

pub(super) struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn tier(&self) -> Tier {
        Tier::Exec
    }
    fn description(&self) -> &str {
        "Execute a shell command in the current working directory. Returns stdout and stderr. \
         Output is truncated to last 2000 lines or 50KB (whichever is hit first). Optionally \
         provide a timeout in seconds."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "timeout": {"type": "integer", "description": "Timeout in seconds (optional, no default timeout)"}
            },
            "required": ["command"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["command"].as_str().unwrap_or("?").to_string()
    }
    fn execute(&self, args: &Value, cwd: &Path, log: &mut dyn FnMut(&str)) -> ToolOutput {
        let command = args["command"].as_str().unwrap_or("");
        // 0 means no timeout (the reference's default: the command runs until it exits)
        let timeout = args["timeout"].as_u64().unwrap_or(0);
        let outcome = crate::platform::run_shell_stream(
            command,
            cwd,
            timeout,
            crate::core::http::interrupt_flag(),
            log,
        );
        if outcome.interrupted {
            return ToolOutput::err("command interrupted");
        }
        if outcome.timed_out {
            // the deadline and the output are independent facts: a command
            // killed at the limit usually printed what explains it already,
            // so the partial output rides the error instead of dying with
            // the process (and the full text spills to a file the model can
            // read on demand)
            let mut out =
                truncate_with_spill(&merge_process_output(&outcome.stdout, &outcome.stderr));
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!(
                "Command timed out after {timeout}s (process killed): {command}"
            ));
            return ToolOutput::err(out);
        }
        finish_process_output(outcome.stdout, outcome.stderr, outcome.code)
    }
}
