use super::*;

/// How much of an archived tool result one `recall` call returns. Paged, so
/// a follow-up call continues exactly where the first stopped.
pub(super) const RECALL_CHARS: usize = 12_000;

pub(super) struct RecallTool;

impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn description(&self) -> &str {
        "Read back a tool result compaction replaced with a placeholder: pass the observation id \
         and a char `offset`; the reply's `next_offset` continues."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "Observation id from a placeholder marker"},
                "offset": {"type": "integer", "description": "Character offset to start at (default 0)"}
            },
            "required": ["id"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        format!(
            "observation {} from char {}",
            args["id"].as_str().unwrap_or("?"),
            args["offset"].as_u64().unwrap_or(0)
        )
    }
    fn execute(&self, args: &Value, _cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let id = args["id"].as_str().unwrap_or("");
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let dir = crate::agent::compact::observation_dir();
        let Some(path) = crate::agent::compact::observation_path(&dir, id) else {
            return ToolOutput::err(format!("not an observation id: {id}"));
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => {
                return ToolOutput::err(format!(
                    "unknown observation id: {id} (archives live in {})",
                    dir.display()
                ));
            }
        };
        let (start, end, eof, chunk) = recall_page(&text, offset);
        ToolOutput::ok(format!(
            "[recall id={id} offset={start} next_offset={end} eof={eof} total_chars={}]\n{chunk}",
            text.chars().count()
        ))
    }
}

/// One page of an archived observation, as (start, next_offset, eof, text).
/// Char offsets, not bytes: a CJK slice must not split a codepoint, and
/// `next_offset` feeds straight back into the next `recall` call.
pub(super) fn recall_page(text: &str, offset: usize) -> (usize, usize, bool, String) {
    let chars: Vec<char> = text.chars().collect();
    let start = offset.min(chars.len());
    let end = (start + RECALL_CHARS).min(chars.len());
    let chunk: String = chars[start..end].iter().collect();
    (start, end, end >= chars.len(), chunk)
}
