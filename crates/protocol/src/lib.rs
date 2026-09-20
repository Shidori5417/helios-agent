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
            messages: vec![Message {
                role: Role::User,
                content: "hello".into(),
            }],
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
