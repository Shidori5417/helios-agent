use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Tool calls requested by an assistant message. Empty for plain text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For `Role::Tool` messages: the id of the call this result answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    /// A plain text message with no tool linkage.
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Content with tool calls rendered as text lines (`[tool call NAME] {args}`),
    /// for storage and for adapters without a native tool history.
    pub fn plain_text(&self) -> String {
        let mut text = self.content.clone();
        for call in &self.tool_calls {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&call.summary());
        }
        text
    }

    /// The result of the tool call identified by `call_id`.
    pub fn tool_result(call_id: Option<String>, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: call_id,
            ..Self::new(Role::Tool, content)
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolCall {
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

/// Longest argument text rendered by [`ToolCall::summary`].
const SUMMARY_ARGUMENT_CHARS: usize = 2_000;

impl ToolCall {
    /// One-line text form: `[tool call NAME] {arguments}` (arguments capped).
    pub fn summary(&self) -> String {
        let arguments = self.arguments.to_string();
        let arguments = if arguments.chars().count() > SUMMARY_ARGUMENT_CHARS {
            format!(
                "{}…",
                arguments
                    .chars()
                    .take(SUMMARY_ARGUMENT_CHARS)
                    .collect::<String>()
            )
        } else {
            arguments
        };
        format!("[tool call {}] {arguments}", self.name)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ModelResponse {
    pub message: Message,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Event {
    TurnStarted,
    ModelDelta(String),
    ReasoningSummaryDelta(String),
    ToolRequested(String),
    ToolStarted(String),
    ToolOutputDelta(String),
    ToolCompleted(String),
    PatchProduced(String),
    VerificationStarted(String),
    VerificationResult(String),
    PermissionRequested(String),
    ContextCompacted(String),
    SessionReset(String),
    /// The active permission mode changed (for example `ReadOnly`).
    PermissionModeChanged(String),
    UsageUpdated(String),
    HookStarted(String),
    HookResult(String),
    McpToolDiscovered(String),
    McpToolResult(String),
    TurnCompleted,
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips_through_json() {
        let request = ModelRequest {
            messages: vec![Message::new(Role::User, "hello")],
            tools: vec![],
        };
        let encoded = serde_json::to_string(&request).expect("serialize request");
        let decoded: ModelRequest = serde_json::from_str(&encoded).expect("deserialize request");
        assert_eq!(decoded, request);
    }

    #[test]
    fn extended_event_roundtrips_through_json() {
        let events = vec![
            Event::ToolStarted("read_file".into()),
            Event::PatchProduced("a.rs".into()),
            Event::ContextCompacted("dropped".into()),
        ];
        let encoded = serde_json::to_string(&events).expect("serialize");
        let decoded: Vec<Event> = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, events);
    }
}
