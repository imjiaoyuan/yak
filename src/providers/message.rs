//! The conversation model: messages, tool definitions and calls, request
//! inputs — the vocabulary both provider adapters serialize.

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
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Unified conversation message; each provider adapter serializes these to its
/// own wire format. `text` may be empty on an Assistant that only calls tools.
#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    User {
        text: String,
        /// images/PDFs/audio riding this message (multimodal input)
        attachments: Vec<Attachment>,
    },
    Assistant {
        text: String,
        tool_calls: Vec<ToolCall>,
    },
    ToolResult {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
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
            is_error: false,
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

    /// Bytes of the argument streamed in so far.
    pub fn len(&self, index: usize) -> usize {
        self.slots.get(&index).map(|s| s.2.len()).unwrap_or(0)
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
