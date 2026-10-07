use super::*;

/// The model-owned checklist (the `update_plan` shape: each step carries
/// a `status`, at most one `in_progress` at a time). The plan needs no
/// harness-side state: it lives in the assistant's own tool call, so it is
/// re-read from the transcript every turn and survives a resume for free.
/// Its value is exactly the failure it prevents — a multi-item task losing
/// track of what is left, re-deriving it, and re-exploring.
pub(super) struct PlanTool;

const DESCRIPTION: &str = "Update the task plan: a list of steps, each with a `step` and a \
    `status` (`pending`, `in_progress`, `completed`); at most one `in_progress` at a time.";

impl Tool for PlanTool {
    fn name(&self) -> &str {
        "update_plan"
    }
    fn tier(&self) -> Tier {
        // a pure state update: it touches no files and runs nothing, so it
        // never needs approval
        Tier::Read
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "array",
                                        "items": {
                        "type": "object",
                        "properties": {
                            "step": {"type": "string", "description": "Step text"},
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Status"
                            }
                        },
                        "required": ["step", "status"]
                    }
                }
            },
            "required": ["plan"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        summary(args)
    }
    fn execute(&self, args: &Value, _cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        match validate_plan(args) {
            // the model reads its own latest plan back on the next turn: the
            // checklist rides the result, not just the call that carried it
            Ok(()) => ToolOutput::ok(render_plan(args)),
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// Deep validation, since the shared `validate` only checks that `plan` is an
/// array. A malformed status is bounced back as an error result rather than
/// stored.
fn validate_plan(args: &Value) -> Result<(), String> {
    let Some(plan) = args.get("plan").and_then(Value::as_array) else {
        return Err("`plan` must be an array of {step, status} objects".to_string());
    };
    if plan.is_empty() {
        return Err("`plan` must contain at least one step".to_string());
    }
    let mut in_progress = 0;
    for (i, item) in plan.iter().enumerate() {
        let n = i + 1;
        if item
            .get("step")
            .and_then(Value::as_str)
            .is_none_or(|s| s.trim().is_empty())
        {
            return Err(format!("plan step {n} is missing its `step` text"));
        }
        match item.get("status").and_then(Value::as_str) {
            Some("pending") | Some("completed") => {}
            Some("in_progress") => in_progress += 1,
            Some(other) => {
                return Err(format!(
                    "plan step {n} has unknown status '{other}' (use pending, in_progress or completed)"
                ));
            }
            None => return Err(format!("plan step {n} is missing its `status`")),
        }
    }
    if in_progress > 1 {
        return Err(format!(
            "at most one step may be in_progress at a time, got {in_progress}"
        ));
    }
    Ok(())
}

/// The checklist as the model reads it back: one line per step, `[x]` done,
/// `[>]` in progress, `[ ]` pending.
fn render_plan(args: &Value) -> String {
    let Some(plan) = args.get("plan").and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for item in plan {
        let mark = match item.get("status").and_then(Value::as_str) {
            Some("completed") => "[x]",
            Some("in_progress") => "[>]",
            _ => "[ ]",
        };
        let step = item
            .get("step")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("{mark} {step}"));
    }
    out
}

/// One-line chrome preview: step counts plus the step in progress.
fn summary(args: &Value) -> String {
    let Some(plan) = args.get("plan").and_then(Value::as_array) else {
        return String::new();
    };
    let done = plan
        .iter()
        .filter(|s| s.get("status").and_then(Value::as_str) == Some("completed"))
        .count();
    let active = plan
        .iter()
        .find(|s| s.get("status").and_then(Value::as_str) == Some("in_progress"))
        .and_then(|s| s.get("step").and_then(Value::as_str));
    let n = plan.len();
    let word = if n == 1 { "step" } else { "steps" };
    let mut out = format!("{n} {word} · {done} done");
    if let Some(active) = active {
        out.push_str(&format!(" · now: {}", short(active.trim())));
    }
    out
}
