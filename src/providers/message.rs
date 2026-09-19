//! The conversation model: messages, tool definitions and calls, request
//! inputs — the vocabulary both provider adapters serialize.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Attachment;

/// A tool offered to the model.
#[derive(Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON schema for the arguments object
    pub parameters: Value,
}

/// A tool call emitted by the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Why a tool result is an error; `None` is a success. The wire shapes only
/// carry a boolean, but the kind survives into the transcript so a consumer
/// — an extension, an editor, the model itself — can tell a call that never
/// ran from a tool that ran and failed, without matching prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolError {
    /// the tool ran and reported failure (non-zero exit, no match, ...)
    Failed,
    /// the call was refused before running: approval, an extension gate, or
    /// arguments the tool's own schema rejects
    Denied,
    /// no tool of that name is mounted
    UnknownTool,
    /// the user (or a dropped stream) cancelled the run
    Interrupted,
}

/// A stored `is_error` bool predates the kinds: `true` reads as `Failed`.
#[derive(Deserialize)]
#[serde(untagged)]
enum WireError {
    Kind(ToolError),
    Flag(bool),
}

fn error_from_wire<'de, D>(de: D) -> Result<Option<ToolError>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<WireError>::deserialize(de)? {
        None | Some(WireError::Flag(false)) => None,
        Some(WireError::Flag(true)) => Some(ToolError::Failed),
        Some(WireError::Kind(kind)) => Some(kind),
    })
}

/// Unified conversation message; each provider adapter serializes these to its
/// own wire format, and the thread store persists the same struct (one
/// vocabulary, no second translation layer). `text` may be empty on an
/// Assistant that only calls tools.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Msg {
    User {
        text: String,
        /// images/PDFs/audio riding this message (multimodal input)
        #[serde(default)]
        attachments: Vec<Attachment>,
    },
    Assistant {
        text: String,
        #[serde(default)]
        tool_calls: Vec<ToolCall>,
        /// reasoning trace the model produced with this message (thinking
        /// models via gateways that require it back on replay). None on
        /// non-thinking models and legacy histories.
        #[serde(default)]
        reasoning: Option<String>,
    },
    ToolResult {
        call_id: String,
        name: String,
        content: String,
        /// the failure class; the old stored spelling `is_error` still reads
        #[serde(default, alias = "is_error", deserialize_with = "error_from_wire")]
        error: Option<ToolError>,
        /// images/PDFs a vision-capable read produced, riding this result
        #[serde(default)]
        attachments: Vec<Attachment>,
    },
    /// compaction summary replacing the dropped conversation prefix
    Summary { text: String },
}

impl Msg {
    pub fn user(text: impl Into<String>) -> Msg {
        Msg::user_with(text, Vec::new())
    }

    pub fn user_with(text: impl Into<String>, attachments: Vec<Attachment>) -> Msg {
        Msg::User {
            text: text.into(),
            attachments,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Msg {
        Msg::Assistant {
            text: text.into(),
            tool_calls: Vec::new(),
            reasoning: None,
        }
    }

    #[cfg(test)]
    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Msg {
        Msg::ToolResult {
            call_id: call_id.into(),
            name: name.into(),
            content: content.into(),
            error: None,
            attachments: Vec::new(),
        }
    }
}

/// Accumulates streamed tool-call fragments (keyed by the provider's block
/// index) into complete calls. Fragments carry `id`/`name` on first sight;
/// argument JSON is parsed once at the end, defaulting to `{}` on failure.
#[derive(Default)]
pub struct ToolCallAccumulator {
    slots: std::collections::BTreeMap<usize, (String, String, String)>,
}

impl ToolCallAccumulator {
    pub fn push(&mut self, index: usize, id: Option<&str>, name: Option<&str>, fragment: &str) {
        let slot = self.slots.entry(index).or_default();
        if let Some(id) = id
            && !id.is_empty()
        {
            slot.0 = id.to_string();
        }
        if let Some(name) = name
            && !name.is_empty()
        {
            slot.1 = name.to_string();
        }
        slot.2.push_str(fragment);
    }

    /// The call's name if any fragment carried one yet.
    pub fn name(&self, index: usize) -> Option<&str> {
        self.slots.get(&index).and_then(|s| {
            let name = &s.1;
            (!name.is_empty()).then_some(name.as_str())
        })
    }

    pub fn finish(self) -> Vec<ToolCall> {
        self.slots
            .into_iter()
            .map(|(index, (id, name, args))| ToolCall {
                id: if id.is_empty() {
                    format!("{name}-{index}")
                } else {
                    id
                },
                name,
                arguments: serde_json::from_str(&args).unwrap_or_else(|_| json!({})),
            })
            .collect()
    }
}

/// For every call id, the index of its LAST ToolResult in the history.
/// Serializers use it to spot unpaired calls — a call at index i is answered
/// iff its last result sits at some j > i (the exact "a result exists later"
/// rule, resolved in O(1) instead of rescanning the tail per call, which was
/// quadratic in conversation length and ran every round).
pub fn last_result_index(history: &[Msg]) -> std::collections::HashMap<&str, usize> {
    let mut last: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (i, msg) in history.iter().enumerate() {
        if let Msg::ToolResult { call_id, .. } = msg {
            last.insert(call_id.as_str(), i);
        }
    }
    last
}

/// The synthetic result an unpaired call is serialized with.
pub const ORPHAN_RESULT: &str = "No result provided";

/// True when the tool call at `index` has a ToolResult after it.
pub fn call_answered(
    last: &std::collections::HashMap<&str, usize>,
    id: &str,
    index: usize,
) -> bool {
    last.get(id).is_some_and(|&j| j > index)
}
