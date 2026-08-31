//! Script tools: user-declared commands from the config `tools` table.
//!
//! Each call spawns the command once, writes the tool arguments as one JSON
//! line to stdin, and returns stdout as the tool result — a plugin with zero
//! protocol and no resident process. Anything executable is a plugin.

use std::path::Path;

use serde_json::{Value, json};

use super::approval::Tier;
use super::tools::{MAX_BYTES, MAX_LINES, Tool, ToolOutput, truncate_tail};
use crate::core::config::expand_env;

/// Default per-call timeout in seconds.
const DEFAULT_TIMEOUT: u64 = 60;

/// Built-in tool names; script tools must not shadow these. Tied to the
/// actual registry by a test.
const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read", "write", "edit", "bash", "grep", "glob", "ls", "webfetch", "task",
];

#[derive(Clone, Debug)]
pub struct ScriptToolSpec {
    pub name: String,
    pub description: String,
    pub command: String,
    pub args: Vec<String>,
    pub schema: Value,
    pub timeout: u64,
}

pub fn load() -> Vec<ScriptToolSpec> {
    let Some(table) = crate::core::config::table("tools") else {
        return Vec::new();
    };
    parse_table(&table)
}

#[cfg(test)]
pub fn parse(raw: &str) -> Vec<ScriptToolSpec> {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let Some(table) = value.get("tools").and_then(Value::as_object) else {
        return Vec::new();
    };
    parse_table(table)
}

fn parse_table(table: &serde_json::Map<String, Value>) -> Vec<ScriptToolSpec> {
    let mut out = Vec::new();
    let builtin = BUILTIN_TOOL_NAMES;
    for (name, def) in table {
        let Some(command) = def.get("command").and_then(Value::as_str) else {
            eprintln!("Warning: script tool '{name}' has no command; skipped");
            continue;
        };
        if !valid_name(name) {
            eprintln!(
                "Warning: script tool name '{name}' must match [A-Za-z0-9_-]{{1,64}} and not start with mcp__; skipped"
            );
            continue;
        }
        if builtin.contains(&name.as_str()) {
            eprintln!("Warning: script tool '{name}' collides with a built-in tool; skipped");
            continue;
        }
        let command = expand_env(command);
        out.push(ScriptToolSpec {
            name: name.clone(),
            command: command.clone(),
            description: def
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("User script tool: runs `{command}`")),
            args: def
                .get("args")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(expand_env).collect())
                .unwrap_or_default(),
            schema: def
                .get("schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"})),
            timeout: def
                .get("timeout")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT),
        });
    }
    out
}

/// Script names share the tool-name space the providers expect; the `mcp__`
/// prefix is reserved for mounted MCP tools.
fn valid_name(name: &str) -> bool {
    crate::core::text::valid_plugin_name(name, 64) && !name.starts_with("mcp__")
}

struct ScriptTool {
    spec: ScriptToolSpec,
}

/// Wrap one config entry as an agent tool.
pub fn mount(spec: &ScriptToolSpec) -> Box<dyn Tool> {
    Box::new(ScriptTool { spec: spec.clone() })
}

impl Tool for ScriptTool {
    fn name(&self) -> &str {
        &self.spec.name
    }
    fn tier(&self) -> Tier {
        Tier::Exec
    }
    fn description(&self) -> &str {
        &self.spec.description
    }
    fn parameters(&self) -> Value {
        self.spec.schema.clone()
    }
    fn preview(&self, args: &Value) -> String {
        format!(
            "{} {}",
            self.spec.command,
            super::task::short(&serde_json::to_string(args).unwrap_or_default())
        )
    }
    fn execute(&self, args: &Value, cwd: &Path, log: &mut dyn FnMut(&str)) -> ToolOutput {
        let mut command = std::process::Command::new(&self.spec.command);
        command
            .args(&self.spec.args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let payload = format!(
            "{}\n",
            serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string())
        );
        let outcome = crate::platform::run_with_progress(
            command,
            Some(&payload),
            self.spec.timeout,
            crate::core::http::interrupt_flag(),
            log,
        );
        if outcome.interrupted {
            return ToolOutput::err("script tool interrupted");
        }
        let timeout = self.spec.timeout;
        if outcome.timed_out {
            return ToolOutput::err(format!(
                "script tool timed out after {timeout}s: {}",
                self.spec.command
            ));
        }
        let mut out = String::from_utf8_lossy(&outcome.stdout).into_owned();
        let err_text = String::from_utf8_lossy(&outcome.stderr);
        if !err_text.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&err_text);
        }
        let code = outcome.code;
        if code != 0 {
            out.push_str(&format!("\nCommand exited with code {code}"));
        }
        let (mut out, truncated) = truncate_tail(&out, MAX_LINES, MAX_BYTES);
        if truncated {
            out.push_str("\n[output truncated]\n");
        }
        if code == 0 {
            ToolOutput::ok(out)
        } else {
            ToolOutput::err(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_names_const_matches_registry() {
        let registry: Vec<String> =
            super::super::tools::builtin_tools_configured(None, &Default::default())
                .iter()
                .map(|t| t.name().to_string())
                .collect();
        assert_eq!(registry, BUILTIN_TOOL_NAMES);
    }

    #[test]
    fn parses_tools_table_with_defaults() {
        let raw = r#"{
          "tools": {
            "jira-ticket": {
              "command": "python3",
              "args": ["/home/me/jira.py", "--project", "LLM"]
            }
          }
        }"#;
        let specs = parse(raw);
        assert_eq!(specs.len(), 1);
        let s = &specs[0];
        assert_eq!(s.name, "jira-ticket");
        assert_eq!(s.command, "python3");
        assert_eq!(
            s.args,
            vec![
                "/home/me/jira.py".to_string(),
                "--project".to_string(),
                "LLM".to_string()
            ]
        );
        assert_eq!(s.schema, json!({"type": "object"}));
        assert_eq!(s.timeout, DEFAULT_TIMEOUT);
        assert!(s.description.contains("python3"));
    }

    #[test]
    fn takes_schema_timeout_and_description_from_config() {
        let raw = r#"{
          "tools": {
            "calc": {
              "description": "Evaluate arithmetic",
              "command": "calc",
              "schema": {"type": "object", "properties": {"expr": {"type": "string"}}, "required": ["expr"]},
              "timeout": 5
            }
          }
        }"#;
        let specs = parse(raw);
        assert_eq!(specs[0].description, "Evaluate arithmetic");
        assert_eq!(specs[0].timeout, 5);
        assert_eq!(
            specs[0].schema["properties"]["expr"]["type"],
            json!("string")
        );
    }

    #[test]
    fn drops_builtin_collision_invalid_name_and_missing_command() {
        let raw = r#"{
          "tools": {
            "bash": {"command": "echo"},
            "bad name!": {"command": "echo"},
            "mcp__x": {"command": "echo"},
            "no-command": {"description": "x"},
            "fine": {"command": "echo"}
          }
        }"#;
        let specs = parse(raw);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "fine");
    }

    #[test]
    fn missing_or_invalid_config_yields_nothing() {
        assert!(parse("{}").is_empty());
        assert!(parse("not json {{{").is_empty());
        assert!(parse(r#"{"tools": "nope"}"#).is_empty());
    }

    #[test]
    fn expands_env_vars_in_command_and_args() {
        // SAFETY: single-threaded test process, unique variable name
        unsafe { std::env::set_var("LLM_TEST_SCRIPT_BIN", "/opt/tool") };
        let raw = r#"{
          "tools": {"t": {"command": "${LLM_TEST_SCRIPT_BIN}", "args": ["${LLM_TEST_SCRIPT_BIN}/helper"]}}
        }"#;
        let specs = parse(raw);
        assert_eq!(specs[0].command, "/opt/tool");
        assert_eq!(specs[0].args, vec!["/opt/tool/helper".to_string()]);
    }

    #[test]
    fn mounted_tool_reports_spec_metadata() {
        let spec = ScriptToolSpec {
            name: "demo".to_string(),
            description: "runs things".to_string(),
            command: "echo".to_string(),
            args: vec![],
            schema: json!({"type": "object"}),
            timeout: 3,
        };
        let tool = mount(&spec);
        assert_eq!(tool.name(), "demo");
        assert_eq!(tool.description(), "runs things");
        assert_eq!(tool.parameters(), json!({"type": "object"}));
        assert!(matches!(tool.tier(), Tier::Exec));
        assert!(tool.preview(&json!({"a": 1})).starts_with("echo "));
    }
}
