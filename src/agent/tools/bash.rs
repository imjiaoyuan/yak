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
        "Run a shell command in the working directory. Output merges stdout and \
         stderr; nonzero exits are reported as errors. Use for anything the file tools cannot do."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout": {"type": "integer", "description": "Seconds before the process group is killed (default 120)"}
            },
            "required": ["command"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["command"].as_str().unwrap_or("?").to_string()
    }
    fn execute(&self, args: &Value, cwd: &Path, log: &mut dyn FnMut(&str)) -> ToolOutput {
        let command = args["command"].as_str().unwrap_or("");
        let timeout = args["timeout"].as_u64().unwrap_or(120);
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
            // the process
            let mut out = truncate_marked(
                &merge_process_output(&outcome.stdout, &outcome.stderr),
                MAX_LINES,
                MAX_BYTES,
            );
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
