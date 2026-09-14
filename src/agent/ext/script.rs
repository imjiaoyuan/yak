use super::*;

/// One manifest-carrying script mounted as a tool: the host runs the
/// protocol around each call (spawn, feed arguments, collect output,
/// timeout), the script itself only prints its result.
pub(super) struct ScriptTool {
    pub(super) spec: ExecToolSpec,
    pub(super) description: String,
    /// registry name; differs from `spec.name` only on a collision
    pub(super) exposed: String,
}

impl Tool for ScriptTool {
    fn name(&self) -> &str {
        &self.exposed
    }
    fn tier(&self) -> Tier {
        self.spec.tier
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        self.spec.schema.clone()
    }
    fn preview(&self, args: &Value) -> String {
        crate::agent::tools::args_preview(&self.spec.name, args)
    }
    fn execute(&self, args: &Value, cwd: &Path, log: &mut dyn FnMut(&str)) -> ToolOutput {
        let mut command = match self.spec.interpreter.as_deref() {
            Some(prog) => {
                let mut c = std::process::Command::new(prog);
                c.arg(&self.spec.path);
                c
            }
            None => std::process::Command::new(&self.spec.path),
        };
        command
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // argv mode: the single declared argument rides as plain argv[1],
        // so shell scripts never need to parse JSON
        let stdin_payload = if self.spec.arg_mode_argv {
            let value = args
                .as_object()
                .and_then(|o| o.values().next())
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => crate::jsonfmt::dumps_indent(other, 0),
                })
                .unwrap_or_default();
            command.arg(value);
            None
        } else {
            Some(format!(
                "{}\n",
                serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string())
            ))
        };
        let outcome = crate::platform::run_with_progress(
            command,
            stdin_payload.as_deref(),
            self.spec.timeout,
            crate::core::http::interrupt_flag(),
            log,
        );
        if outcome.interrupted {
            return ToolOutput::err("script tool interrupted");
        }
        if outcome.timed_out {
            return ToolOutput::err(format!(
                "script tool timed out after {}s: {}",
                self.spec.timeout,
                self.spec.path.display()
            ));
        }
        crate::agent::tools::finish_process_output(outcome.stdout, outcome.stderr, outcome.code)
    }
}

// ============================================================================
// one manifest script tool
// ============================================================================
