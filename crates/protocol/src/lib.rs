use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// An image attached to a user message. `data` is standard base64 without
/// line breaks; the CLI validates the bytes and media type before attaching.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ImageInput {
    pub media_type: String,
    pub data: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Images attached to a user message, sent as native image parts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageInput>,
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
            images: Vec::new(),
        }
    }

    /// Content with tool calls rendered as text lines (`[tool call NAME] {args}`),
    /// for storage and for adapters without a native tool history.
    pub fn plain_text(&self) -> String {
        let mut text = self.content.clone();
        for image in &self.images {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("[image attached: {}]", image.media_type));
        }
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

/// Standard base64 (RFC 4648) with padding. Small and dependency-free because
/// it is only used to embed attached images in provider requests.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(char::from(
                    ALPHABET[((n >> (18 - 6 * index)) & 63) as usize],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
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
    fn base64_matches_rfc_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
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
