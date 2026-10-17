use agent_protocol::{Message, ModelRequest, ModelResponse, Role};
use serde_json::{json, Value};
use std::io::Read;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamEvent {
    TextDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        reasoning_tokens: u64,
    },
}

/// Per-session request tuning shared by all adapters. `None` keeps each
/// provider's default so existing behaviour and prompt-cache prefixes hold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestTuning {
    /// Reasoning effort (`minimal`..`max`); `auto` or `None` sends nothing.
    pub effort: Option<String>,
    /// Upper bound on generated tokens for one response.
    pub max_output_tokens: Option<u32>,
}

impl RequestTuning {
    fn effort(&self) -> Option<&str> {
        self.effort.as_deref().filter(|effort| *effort != "auto")
    }
}

/// Anthropic default output cap; large enough for multi-file patches while
/// staying under non-streaming HTTP timeouts.
const ANTHROPIC_DEFAULT_MAX_TOKENS: u32 = 16_000;

pub trait StreamSink {
    fn emit(&mut self, event: StreamEvent);
    fn is_cancelled(&self) -> bool {
        false
    }
}

pub trait ModelProvider {
    fn complete(&mut self, request: ModelRequest) -> Result<ModelResponse, ProviderError>;

    fn complete_streaming(
        &mut self,
        request: ModelRequest,
        sink: &mut dyn StreamSink,
    ) -> Result<ModelResponse, ProviderError> {
        let response = self.complete(request)?;
        if !response.message.content.is_empty() {
            sink.emit(StreamEvent::TextDelta(response.message.content.clone()));
        }
        Ok(response)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolShape {
    OpenAiResponses,
    AnthropicMessages,
    GeminiFunctions,
    OpenAiChatCompletions,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderCapabilities {
    pub tool_shape: ToolShape,
    pub context_window: Option<u64>,
    pub reasoning_levels: &'static [ReasoningLevel],
    pub prompt_caching: bool,
    pub native_compaction: bool,
    pub reasoning_controls: bool,
    pub streaming: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasoningLevel {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderProfile {
    pub name: &'static str,
    pub capabilities: ProviderCapabilities,
}

pub fn openai_profile() -> ProviderProfile {
    ProviderProfile {
        name: "openai",
        capabilities: ProviderCapabilities {
            tool_shape: ToolShape::OpenAiResponses,
            context_window: None,
            reasoning_levels: &[
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ],
            prompt_caching: true,
            native_compaction: true,
            reasoning_controls: true,
            streaming: true,
        },
    }
}
pub fn anthropic_profile() -> ProviderProfile {
    ProviderProfile {
        name: "anthropic",
        capabilities: ProviderCapabilities {
            tool_shape: ToolShape::AnthropicMessages,
            context_window: None,
            reasoning_levels: &[
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
            ],
            prompt_caching: true,
            native_compaction: false,
            reasoning_controls: true,
            streaming: true,
        },
    }
}
pub fn gemini_profile() -> ProviderProfile {
    ProviderProfile {
        name: "gemini",
        capabilities: ProviderCapabilities {
            tool_shape: ToolShape::GeminiFunctions,
            context_window: None,
            reasoning_levels: &[],
            prompt_caching: true,
            native_compaction: false,
            reasoning_controls: true,
            streaming: true,
        },
    }
}
pub fn generic_profile() -> ProviderProfile {
    ProviderProfile {
        name: "generic",
        capabilities: ProviderCapabilities {
            tool_shape: ToolShape::OpenAiChatCompletions,
            context_window: None,
            reasoning_levels: &[],
            prompt_caching: false,
            native_compaction: false,
            reasoning_controls: false,
            streaming: true,
        },
    }
}

pub fn profile_for(provider: &str) -> Option<ProviderProfile> {
    match provider {
        "openai" => Some(openai_profile()),
        "anthropic" => Some(anthropic_profile()),
        "gemini" => Some(gemini_profile()),
        "generic" | "openai-compatible" => Some(generic_profile()),
        "fake" => Some(ProviderProfile {
            name: "fake",
            capabilities: ProviderCapabilities {
                tool_shape: ToolShape::OpenAiChatCompletions,
                context_window: None,
                reasoning_levels: &[],
                prompt_caching: false,
                native_compaction: false,
                reasoning_controls: false,
                streaming: true,
            },
        }),
        _ => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider failed: {0}")]
    Failed(String),
    #[error("provider stream timed out")]
    Timeout,
    #[error("provider stream cancelled")]
    Cancelled,
    #[error("provider stream exceeded {0} bytes")]
    Oversized(usize),
    #[error("provider stream was malformed: {0}")]
    Malformed(String),
}

/// Splits the leading run of system messages (the stable, cacheable prefix)
/// from the rest. Adapters with a dedicated system field send the prefix
/// there; later system messages (memory, steering) stay in order via
/// [`system_note`] instead of being dropped.
fn split_leading_system(messages: &[Message]) -> (Option<String>, Vec<&Message>) {
    let leading = messages
        .iter()
        .take_while(|message| matches!(message.role, Role::System))
        .count();
    let system = (leading > 0).then(|| {
        messages[..leading]
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    });
    (system, messages[leading..].iter().collect())
}

/// Content for a conversation message; mid-conversation system messages are
/// labelled so models without a system role in `messages` still see them.
fn system_note(message: &Message) -> String {
    if matches!(message.role, Role::System) {
        format!("[system note]\n{}", message.content)
    } else {
        message.content.clone()
    }
}

/// Wire-safe function names. OpenAI and Anthropic accept only
/// `[A-Za-z0-9_-]{1,64}`, but MCP, LSP and plugin tools are named
/// `mcp.server.tool` / `lsp:name:tool`. Plain names pass through unchanged
/// (keeping prompts stable); others are sanitised deterministically and
/// de-duplicated so the response can be mapped back.
fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn wire_tool_names(tools: &[agent_protocol::ToolDefinition]) -> Vec<String> {
    let mut used = std::collections::BTreeSet::new();
    tools
        .iter()
        .map(|tool| {
            let base = sanitize_tool_name(&tool.name);
            let mut candidate = if base.is_empty() {
                "tool".to_owned()
            } else {
                base.clone()
            };
            let mut suffix = 1_usize;
            while !used.insert(candidate.clone()) {
                let tail = format!("_{suffix}");
                let keep = 64_usize.saturating_sub(tail.len());
                candidate = format!("{}{tail}", base.chars().take(keep).collect::<String>());
                suffix += 1;
            }
            candidate
        })
        .collect()
}

fn wire_tools(tools: &[agent_protocol::ToolDefinition]) -> Vec<agent_protocol::ToolDefinition> {
    wire_tool_names(tools)
        .into_iter()
        .zip(tools)
        .map(|(name, tool)| agent_protocol::ToolDefinition {
            name,
            ..tool.clone()
        })
        .collect()
}

/// Maps tool-call names in a response back to the registered tool names.
fn restore_tool_names(
    mut response: ModelResponse,
    tools: &[agent_protocol::ToolDefinition],
) -> ModelResponse {
    let wire = wire_tool_names(tools);
    for call in &mut response.tool_calls {
        if let Some(index) = wire.iter().position(|name| *name == call.name) {
            call.name = tools[index].name.clone();
        }
    }
    response
}

/// Renders tool linkage as plain text, for adapters without a native tool
/// history and for calls/results that lost their partner.
fn flatten_message(message: &Message) -> Message {
    match message.role {
        Role::Tool => Message::new(Role::User, format!("[tool result]\n{}", message.content)),
        Role::Assistant if !message.tool_calls.is_empty() => {
            Message::new(Role::Assistant, message.plain_text())
        }
        _ => message.clone(),
    }
}

/// Guarantees that tool linkage in `messages` is complete: each assistant
/// message with tool calls is followed by a result for every call id, and
/// every id-bearing tool result answers such a message. The context planner
/// can drop one side of a pair and resumed sessions only store flattened
/// text; strict APIs reject orphans, so those are rendered as text instead.
fn paired_history(messages: &[Message]) -> Vec<Message> {
    let mut out = Vec::with_capacity(messages.len());
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        if message.role == Role::Assistant && !message.tool_calls.is_empty() {
            let mut end = index + 1;
            while end < messages.len() && messages[end].role == Role::Tool {
                end += 1;
            }
            let results = &messages[index + 1..end];
            let answered = message.tool_calls.iter().all(|call| {
                call.id.as_deref().is_some_and(|id| {
                    results
                        .iter()
                        .any(|result| result.tool_call_id.as_deref() == Some(id))
                })
            });
            if answered {
                out.push(message.clone());
                for result in results {
                    let linked = result.tool_call_id.as_deref().is_some_and(|id| {
                        message
                            .tool_calls
                            .iter()
                            .any(|call| call.id.as_deref() == Some(id))
                    });
                    out.push(if linked {
                        result.clone()
                    } else {
                        flatten_message(result)
                    });
                }
            } else {
                out.push(flatten_message(message));
                out.extend(results.iter().map(flatten_message));
            }
            index = end;
        } else {
            out.push(if message.role == Role::Tool {
                flatten_message(message)
            } else {
                message.clone()
            });
            index += 1;
        }
    }
    out
}

/// Original tool name -> wire-safe name for one request.
fn wire_name_map(
    tools: &[agent_protocol::ToolDefinition],
) -> std::collections::BTreeMap<String, String> {
    tools
        .iter()
        .map(|tool| tool.name.clone())
        .zip(wire_tool_names(tools))
        .collect()
}

fn wire_name_for(map: &std::collections::BTreeMap<String, String>, name: &str) -> String {
    map.get(name)
        .cloned()
        .unwrap_or_else(|| sanitize_tool_name(name))
}

fn request_error() -> ProviderError {
    ProviderError::Failed("provider request failed".into())
}
fn response_error() -> ProviderError {
    ProviderError::Failed("provider response invalid".into())
}

fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("valid provider HTTP client")
}

const STREAM_LIMIT: usize = 2 * 1024 * 1024;

struct StreamAccumulator {
    text: String,
    tools: std::collections::BTreeMap<usize, (Option<String>, Option<String>, String)>,
}
impl StreamAccumulator {
    fn apply(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::TextDelta(text) => self.text.push_str(&text),
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => {
                let entry = self
                    .tools
                    .entry(index)
                    .or_insert((None, None, String::new()));
                if id.is_some() {
                    entry.0 = id;
                }
                if name.is_some() {
                    entry.1 = name;
                }
                entry.2.push_str(&arguments);
            }
            StreamEvent::Usage { .. } => {}
        }
    }
    fn finish(self) -> Result<ModelResponse, ProviderError> {
        let mut tool_calls = Vec::new();
        for (_, (id, name, arguments)) in self.tools {
            let name =
                name.ok_or_else(|| ProviderError::Malformed("tool stream has no name".into()))?;
            let arguments = serde_json::from_str(&arguments).map_err(|_| {
                ProviderError::Malformed("tool arguments are not valid JSON".into())
            })?;
            tool_calls.push(agent_protocol::ToolCall {
                id,
                name,
                arguments,
            });
        }
        Ok(ModelResponse {
            message: Message::new(Role::Assistant, self.text),
            tool_calls,
        })
    }
}

fn stream_sse<R: Read>(
    mut response: R,
    sink: &mut dyn StreamSink,
    deadline: Instant,
    parser: fn(&Value) -> Vec<StreamEvent>,
) -> Result<ModelResponse, ProviderError> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut accumulator = StreamAccumulator {
        text: String::new(),
        tools: std::collections::BTreeMap::new(),
    };
    loop {
        if sink.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(ProviderError::Timeout);
        }
        let count = response
            .read(&mut chunk)
            .map_err(|error| ProviderError::Failed(error.to_string()))?;
        if count == 0 {
            break;
        }
        if buffer.len().saturating_add(count) > STREAM_LIMIT {
            return Err(ProviderError::Oversized(STREAM_LIMIT));
        }
        buffer.extend_from_slice(&chunk[..count]);
        while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let line = buffer.drain(..=position).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line).trim().to_owned();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                continue;
            }
            let value: Value = serde_json::from_str(data)
                .map_err(|error| ProviderError::Malformed(error.to_string()))?;
            for event in parser(&value) {
                accumulator.apply(event.clone());
                sink.emit(event);
            }
        }
    }
    if !buffer.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(ProviderError::Malformed("unterminated SSE frame".into()));
    }
    accumulator.finish()
}

fn stream_response_with(
    request: reqwest::blocking::RequestBuilder,
    sink: &mut dyn StreamSink,
    parser: fn(&Value) -> Vec<StreamEvent>,
) -> Result<ModelResponse, ProviderError> {
    let response = request.send().map_err(|error| {
        if error.is_timeout() {
            ProviderError::Timeout
        } else {
            request_error()
        }
    })?;
    if !response.status().is_success() {
        return Err(ProviderError::Failed(format!(
            "provider returned HTTP {}",
            response.status()
        )));
    }
    stream_sse(
        response,
        sink,
        Instant::now() + Duration::from_secs(60),
        parser,
    )
}

pub struct OpenAiProvider {
    client: reqwest::blocking::Client,
    api_key: String,
    endpoint: String,
    model: String,
    tuning: RequestTuning,
}

impl OpenAiProvider {
    /// Applies effort / output-limit tuning to every subsequent request.
    pub fn with_tuning(mut self, tuning: RequestTuning) -> Self {
        self.tuning = tuning;
        self
    }
    pub fn with_credentials(
        model: impl Into<String>,
        api_key: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            endpoint: endpoint.into(),
            model: model.into(),
            tuning: RequestTuning::default(),
        }
    }
    pub fn from_env(model: impl Into<String>) -> Result<Self, ProviderError> {
        Self::from_env_with_base_url(model, None)
    }
    pub fn from_env_with_base_url(
        model: impl Into<String>,
        base_url: Option<String>,
    ) -> Result<Self, ProviderError> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| ProviderError::Failed("OPENAI_API_KEY is not set".into()))?;
        let endpoint = base_url
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.openai.com/v1/responses".into());
        Ok(Self {
            client: http_client(),
            api_key,
            endpoint,
            model: model.into(),
            tuning: RequestTuning::default(),
        })
    }

    fn request_body(&self, request: &ModelRequest) -> Value {
        let names = wire_name_map(&request.tools);
        let mut input: Vec<Value> = Vec::new();
        for message in paired_history(&request.messages) {
            match message.role {
                Role::Assistant => {
                    if !message.content.is_empty() {
                        input.push(json!({
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": message.content}]
                        }));
                    }
                    for call in &message.tool_calls {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": wire_name_for(&names, &call.name),
                            "arguments": call.arguments.to_string(),
                        }));
                    }
                }
                Role::Tool => input.push(json!({
                    "type": "function_call_output",
                    "call_id": message.tool_call_id,
                    "output": message.content,
                })),
                Role::System | Role::User => {
                    let mut parts: Vec<Value> = Vec::new();
                    if !message.content.is_empty() {
                        parts.push(json!({"type": "input_text", "text": message.content}));
                    }
                    for image in &message.images {
                        parts.push(json!({
                            "type": "input_image",
                            "image_url": format!("data:{};base64,{}", image.media_type, image.data)
                        }));
                    }
                    if !parts.is_empty() {
                        input.push(json!({
                            "role": if message.role == Role::System { "system" } else { "user" },
                            "content": parts
                        }));
                    }
                }
            }
        }
        let tools = wire_tools(&request.tools).iter().map(|tool| json!({
            "type": "function", "name": tool.name, "description": tool.description, "parameters": tool.input_schema
        })).collect::<Vec<_>>();
        let mut body = json!({"model": self.model, "input": input});
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if let Some(effort) = self.tuning.effort() {
            body["reasoning"] = json!({ "effort": effort });
        }
        if let Some(limit) = self.tuning.max_output_tokens {
            body["max_output_tokens"] = json!(limit);
        }
        body
    }
}

impl ModelProvider for OpenAiProvider {
    fn complete(&mut self, request: ModelRequest) -> Result<ModelResponse, ProviderError> {
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&self.request_body(&request))
            .send()
            .map_err(|_| request_error())?;
        let status = response.status();
        let body: Value = response.json().map_err(|_| response_error())?;
        if !status.is_success() {
            return Err(ProviderError::Failed(format!(
                "provider returned HTTP {status}"
            )));
        }
        parse_responses_body(body).map(|response| restore_tool_names(response, &request.tools))
    }

    fn complete_streaming(
        &mut self,
        request: ModelRequest,
        sink: &mut dyn StreamSink,
    ) -> Result<ModelResponse, ProviderError> {
        let mut body = self.request_body(&request);
        body["stream"] = json!(true);
        stream_response_with(
            self.client
                .post(&self.endpoint)
                .bearer_auth(&self.api_key)
                .json(&body),
            sink,
            parse_openai_stream,
        )
        .map(|response| restore_tool_names(response, &request.tools))
    }
}

fn parse_responses_body(body: Value) -> Result<ModelResponse, ProviderError> {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for item in body
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Failed("response has no output".into()))?
    {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        if part.get("type").and_then(Value::as_str) == Some("output_text") {
                            if let Some(value) = part.get("text").and_then(Value::as_str) {
                                text.push_str(value);
                            }
                        }
                    }
                }
            }
            Some("function_call") => {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ProviderError::Failed("function call has no name".into()))?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                let arguments = serde_json::from_str(arguments).map_err(|error| {
                    ProviderError::Failed(format!("invalid tool arguments: {error}"))
                })?;
                tool_calls.push(agent_protocol::ToolCall {
                    id: item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    name: name.into(),
                    arguments,
                });
            }
            _ => {}
        }
    }
    Ok(ModelResponse {
        message: Message::new(Role::Assistant, text),
        tool_calls,
    })
}

fn parse_openai_stream(value: &Value) -> Vec<StreamEvent> {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type == "response.completed" {
        if let Some(usage) = value.pointer("/response/usage") {
            return vec![StreamEvent::Usage {
                input_tokens: usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                output_tokens: usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_read_tokens: usage
                    .pointer("/input_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_write_tokens: 0,
                reasoning_tokens: usage
                    .pointer("/output_tokens_details/reasoning_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            }];
        }
    }
    if event_type.ends_with("output_text.delta") {
        return value
            .get("delta")
            .and_then(Value::as_str)
            .map(|text| vec![StreamEvent::TextDelta(text.into())])
            .unwrap_or_default();
    }
    if event_type.ends_with("function_call_arguments.delta") {
        return vec![StreamEvent::ToolCallDelta {
            index: value
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
            id: value
                .get("call_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            name: value.get("name").and_then(Value::as_str).map(str::to_owned),
            arguments: value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
        }];
    }
    Vec::new()
}

pub struct AnthropicProvider {
    client: reqwest::blocking::Client,
    api_key: String,
    endpoint: String,
    model: String,
    tuning: RequestTuning,
}
impl AnthropicProvider {
    /// Applies effort / output-limit tuning to every subsequent request.
    pub fn with_tuning(mut self, tuning: RequestTuning) -> Self {
        self.tuning = tuning;
        self
    }
    pub fn with_credentials(
        model: impl Into<String>,
        api_key: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            endpoint: endpoint.into(),
            model: model.into(),
            tuning: RequestTuning::default(),
        }
    }
    pub fn from_env(model: impl Into<String>) -> Result<Self, ProviderError> {
        Self::from_env_with_base_url(model, None)
    }
    pub fn from_env_with_base_url(
        model: impl Into<String>,
        base_url: Option<String>,
    ) -> Result<Self, ProviderError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| ProviderError::Failed("ANTHROPIC_API_KEY is not set".into()))?;
        let endpoint = base_url
            .or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.anthropic.com/v1/messages".into());
        Ok(Self {
            client: http_client(),
            api_key,
            endpoint,
            model: model.into(),
            tuning: RequestTuning::default(),
        })
    }
    fn request_body(&self, request: &ModelRequest) -> Value {
        // Cache breakpoints (max 4 allowed) sit on the stable prefix: the last
        // tool definition, the system prompt, and the newest message, so each
        // turn re-reads the previous turn's prefix from cache.
        let cache = json!({"type": "ephemeral"});
        let paired = paired_history(&request.messages);
        let (system, conversation) = split_leading_system(&paired);
        let names = wire_name_map(&request.tools);
        let mut messages: Vec<Value> = Vec::new();
        // True while the last message is a user turn made of tool_result blocks.
        let mut collecting_results = false;
        for message in conversation {
            match message.role {
                Role::Assistant => {
                    collecting_results = false;
                    let mut blocks = Vec::new();
                    if !message.content.is_empty() {
                        blocks.push(json!({"type": "text", "text": message.content}));
                    }
                    for call in &message.tool_calls {
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": wire_name_for(&names, &call.name),
                            "input": call.arguments,
                        }));
                    }
                    if blocks.is_empty() {
                        continue;
                    }
                    messages.push(json!({"role": "assistant", "content": blocks}));
                }
                Role::Tool => {
                    let block = json!({
                        "type": "tool_result",
                        "tool_use_id": message.tool_call_id,
                        "content": message.content,
                    });
                    match messages.last_mut() {
                        Some(last) if collecting_results => {
                            if let Some(blocks) = last["content"].as_array_mut() {
                                blocks.push(block);
                            }
                        }
                        _ => messages.push(json!({"role": "user", "content": [block]})),
                    }
                    collecting_results = true;
                }
                _ => {
                    collecting_results = false;
                    let text = system_note(message);
                    if message.images.is_empty() {
                        if !text.is_empty() {
                            messages.push(json!({"role": "user", "content": text}));
                        }
                    } else {
                        // Images first, then the text that refers to them.
                        let mut blocks: Vec<Value> = message
                            .images
                            .iter()
                            .map(|image| json!({"type": "image", "source": {"type": "base64", "media_type": image.media_type, "data": image.data}}))
                            .collect();
                        if !text.is_empty() {
                            blocks.push(json!({"type": "text", "text": text}));
                        }
                        messages.push(json!({"role": "user", "content": blocks}));
                    }
                }
            }
        }
        // Cache breakpoint on the newest message: its last block, or its
        // whole text turned into a block.
        if let Some(last) = messages.last_mut() {
            if let Some(blocks) = last["content"].as_array_mut() {
                if let Some(block) = blocks.last_mut() {
                    block["cache_control"] = cache.clone();
                }
            } else if let Some(text) = last["content"].as_str().map(str::to_owned) {
                last["content"] = json!([{"type": "text", "text": text, "cache_control": cache}]);
            }
        }
        let mut tools = wire_tools(&request.tools).iter().map(|tool| json!({"name":tool.name,"description":tool.description,"input_schema":tool.input_schema})).collect::<Vec<_>>();
        if let Some(tool) = tools.last_mut() {
            tool["cache_control"] = cache.clone();
        }
        let max_tokens = self
            .tuning
            .max_output_tokens
            .unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS);
        let mut body = json!({"model":self.model,"max_tokens":max_tokens,"messages":messages});
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if let Some(system) = system {
            body["system"] = json!([{"type": "text", "text": system, "cache_control": cache}]);
        }
        if let Some(effort) = self.tuning.effort() {
            // Messages API effort levels start at `low`.
            let effort = if effort == "minimal" { "low" } else { effort };
            body["output_config"] = json!({ "effort": effort });
        }
        body
    }
}
impl ModelProvider for AnthropicProvider {
    fn complete(&mut self, request: ModelRequest) -> Result<ModelResponse, ProviderError> {
        let response = self
            .client
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&self.request_body(&request))
            .send()
            .map_err(|_| request_error())?;
        let status = response.status();
        let body: Value = response.json().map_err(|_| response_error())?;
        if !status.is_success() {
            return Err(ProviderError::Failed(format!(
                "provider returned HTTP {status}"
            )));
        }
        parse_anthropic_body(body).map(|response| restore_tool_names(response, &request.tools))
    }

    fn complete_streaming(
        &mut self,
        request: ModelRequest,
        sink: &mut dyn StreamSink,
    ) -> Result<ModelResponse, ProviderError> {
        let mut body = self.request_body(&request);
        body["stream"] = json!(true);
        stream_response_with(
            self.client
                .post(&self.endpoint)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body),
            sink,
            parse_anthropic_stream,
        )
        .map(|response| restore_tool_names(response, &request.tools))
    }
}
fn parse_anthropic_body(body: Value) -> Result<ModelResponse, ProviderError> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for part in body
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(response_error)?
    {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(value) = part.get("text").and_then(Value::as_str) {
                    text.push_str(value);
                }
            }
            Some("tool_use") => calls.push(agent_protocol::ToolCall {
                id: part.get("id").and_then(Value::as_str).map(str::to_owned),
                name: part
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(response_error)?
                    .into(),
                arguments: part.get("input").cloned().unwrap_or_else(|| json!({})),
            }),
            _ => {}
        }
    }
    Ok(ModelResponse {
        message: Message::new(Role::Assistant, text),
        tool_calls: calls,
    })
}

fn parse_anthropic_stream(value: &Value) -> Vec<StreamEvent> {
    match value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "content_block_start" => value
            .get("content_block")
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
            .map(|block| {
                vec![StreamEvent::ToolCallDelta {
                    index: value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
                    id: block.get("id").and_then(Value::as_str).map(str::to_owned),
                    name: block.get("name").and_then(Value::as_str).map(str::to_owned),
                    arguments: String::new(),
                }]
            })
            .unwrap_or_default(),
        "content_block_delta" => {
            let delta = value.get("delta").cloned().unwrap_or_default();
            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                vec![StreamEvent::TextDelta(text.into())]
            } else if let Some(arguments) = delta.get("partial_json").and_then(Value::as_str) {
                vec![StreamEvent::ToolCallDelta {
                    index: value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
                    id: None,
                    name: None,
                    arguments: arguments.into(),
                }]
            } else {
                Vec::new()
            }
        }
        "message_start" => value
            .pointer("/message/usage")
            .map(|usage| {
                let tokens = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
                vec![StreamEvent::Usage {
                    input_tokens: tokens("input_tokens"),
                    output_tokens: tokens("output_tokens"),
                    cache_read_tokens: tokens("cache_read_input_tokens"),
                    cache_write_tokens: tokens("cache_creation_input_tokens"),
                    reasoning_tokens: 0,
                }]
            })
            .unwrap_or_default(),
        "message_delta" => value
            .get("usage")
            .map(|usage| {
                vec![StreamEvent::Usage {
                    input_tokens: 0,
                    output_tokens: usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    reasoning_tokens: 0,
                }]
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

pub struct GeminiProvider {
    client: reqwest::blocking::Client,
    api_key: String,
    endpoint: String,
    tuning: RequestTuning,
}
impl GeminiProvider {
    /// Applies effort / output-limit tuning to every subsequent request.
    pub fn with_tuning(mut self, tuning: RequestTuning) -> Self {
        self.tuning = tuning;
        self
    }
    pub fn with_credentials(
        model: impl Into<String>,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        let model = model.into();
        Self {
            client: http_client(),
            api_key: api_key.into(),
            endpoint: format!("{}/{model}:generateContent", base_url.into()),
            tuning: RequestTuning::default(),
        }
    }
    pub fn from_env(model: impl Into<String>) -> Result<Self, ProviderError> {
        Self::from_env_with_base_url(model, None)
    }
    pub fn from_env_with_base_url(
        model: impl Into<String>,
        base_url: Option<String>,
    ) -> Result<Self, ProviderError> {
        let api_key = std::env::var("GEMINI_API_KEY")
            .map_err(|_| ProviderError::Failed("GEMINI_API_KEY is not set".into()))?;
        let base = base_url
            .or_else(|| std::env::var("GEMINI_BASE_URL").ok())
            .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta/models".into());
        let model = model.into();
        let endpoint = format!("{base}/{model}:generateContent");
        Ok(Self {
            client: http_client(),
            api_key,
            endpoint,
            tuning: RequestTuning::default(),
        })
    }
    fn request_body(&self, request: &ModelRequest) -> Value {
        // Tool history is sent as text: it works on every Gemini model without
        // thought-signature bookkeeping for replayed function calls.
        let flattened = request
            .messages
            .iter()
            .map(flatten_message)
            .collect::<Vec<_>>();
        let (system, conversation) = split_leading_system(&flattened);
        let contents = conversation
            .iter()
            .map(|message| (message, system_note(message)))
            .filter(|(message, text)| !text.is_empty() || !message.images.is_empty())
            .map(|(message, text)| {
                let mut parts: Vec<Value> = message
                    .images
                    .iter()
                    .map(|image| json!({"inlineData": {"mimeType": image.media_type, "data": image.data}}))
                    .collect();
                if !text.is_empty() {
                    parts.push(json!({"text": text}));
                }
                json!({"role": if matches!(message.role, Role::Assistant) {"model"} else {"user"}, "parts": parts})
            })
            .collect::<Vec<_>>();
        let declarations = wire_tools(&request.tools).iter().map(|tool| json!({"name":tool.name,"description":tool.description,"parameters":tool.input_schema})).collect::<Vec<_>>();
        let mut body =
            json!({"contents":contents,"generationConfig":{"responseMimeType":"text/plain"}});
        if !declarations.is_empty() {
            body["tools"] = json!([{"functionDeclarations": declarations}]);
        }
        if let Some(system) = system {
            body["systemInstruction"] = json!({"parts": [{"text": system}]});
        }
        if let Some(limit) = self.tuning.max_output_tokens {
            body["generationConfig"]["maxOutputTokens"] = json!(limit);
        }
        body
    }
}
impl ModelProvider for GeminiProvider {
    fn complete(&mut self, request: ModelRequest) -> Result<ModelResponse, ProviderError> {
        let response = self
            .client
            .post(&self.endpoint)
            .query(&[("key", &self.api_key)])
            .json(&self.request_body(&request))
            .send()
            .map_err(|_| request_error())?;
        let status = response.status();
        let body: Value = response.json().map_err(|_| response_error())?;
        if !status.is_success() {
            return Err(ProviderError::Failed(format!(
                "provider returned HTTP {status}"
            )));
        }
        parse_gemini_body(body).map(|response| restore_tool_names(response, &request.tools))
    }

    fn complete_streaming(
        &mut self,
        request: ModelRequest,
        sink: &mut dyn StreamSink,
    ) -> Result<ModelResponse, ProviderError> {
        let body = self.request_body(&request);
        let endpoint = if self.endpoint.contains(":generateContent") {
            self.endpoint
                .replace(":generateContent", ":streamGenerateContent")
        } else {
            self.endpoint.clone()
        };
        stream_response_with(
            self.client
                .post(endpoint)
                .query(&[("alt", "sse")])
                .query(&[("key", &self.api_key)])
                .json(&body),
            sink,
            parse_gemini_stream,
        )
        .map(|response| restore_tool_names(response, &request.tools))
    }
}
fn parse_gemini_body(body: Value) -> Result<ModelResponse, ProviderError> {
    let parts = body
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .ok_or_else(response_error)?;
    let mut text = String::new();
    let mut calls = Vec::new();
    for part in parts {
        if let Some(value) = part.get("text").and_then(Value::as_str) {
            text.push_str(value);
        }
        if let Some(call) = part.get("functionCall") {
            calls.push(agent_protocol::ToolCall {
                id: None,
                name: call
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(response_error)?
                    .into(),
                arguments: call.get("args").cloned().unwrap_or_else(|| json!({})),
            });
        }
    }
    Ok(ModelResponse {
        message: Message::new(Role::Assistant, text),
        tool_calls: calls,
    })
}

fn parse_gemini_stream(value: &Value) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    if let Some(usage) = value.get("usageMetadata") {
        events.push(StreamEvent::Usage {
            input_tokens: usage
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_read_tokens: usage
                .get("cachedContentTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_write_tokens: 0,
            reasoning_tokens: usage
                .get("thoughtsTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        });
    }
    if let Some(parts) = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
    {
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                events.push(StreamEvent::TextDelta(text.into()));
            }
            if let Some(call) = part.get("functionCall") {
                events.push(StreamEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: call.get("name").and_then(Value::as_str).map(str::to_owned),
                    arguments: serde_json::to_string(call.get("args").unwrap_or(&json!({})))
                        .unwrap_or_else(|_| "{}".into()),
                });
            }
        }
    }
    events
}

pub struct OpenAiCompatibleProvider {
    client: reqwest::blocking::Client,
    api_key: String,
    endpoint: String,
    model: String,
    tuning: RequestTuning,
}
impl OpenAiCompatibleProvider {
    /// Applies effort / output-limit tuning to every subsequent request.
    pub fn with_tuning(mut self, tuning: RequestTuning) -> Self {
        self.tuning = tuning;
        self
    }
    pub fn with_credentials(
        model: impl Into<String>,
        api_key: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            endpoint: endpoint.into(),
            model: model.into(),
            tuning: RequestTuning::default(),
        }
    }
    pub fn from_env(model: impl Into<String>) -> Result<Self, ProviderError> {
        Self::from_env_with_base_url(model, None)
    }
    pub fn from_env_with_base_url(
        model: impl Into<String>,
        base_url: Option<String>,
    ) -> Result<Self, ProviderError> {
        let api_key = std::env::var("GENERIC_API_KEY")
            .map_err(|_| ProviderError::Failed("GENERIC_API_KEY is not set".into()))?;
        let endpoint = base_url
            .or_else(|| std::env::var("GENERIC_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.openai.com/v1/chat/completions".into());
        Ok(Self {
            client: http_client(),
            api_key,
            endpoint,
            model: model.into(),
            tuning: RequestTuning::default(),
        })
    }
    fn request_body(&self, request: &ModelRequest) -> Value {
        let names = wire_name_map(&request.tools);
        let messages = paired_history(&request.messages)
            .iter()
            .filter(|message| {
                !(message.role == Role::Assistant
                    && message.tool_calls.is_empty()
                    && message.content.is_empty())
            })
            .map(|message| match message.role {
                Role::Assistant if !message.tool_calls.is_empty() => json!({
                    "role": "assistant",
                    "content": if message.content.is_empty() { Value::Null } else { json!(message.content) },
                    "tool_calls": message.tool_calls.iter().map(|call| json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": wire_name_for(&names, &call.name),
                            "arguments": call.arguments.to_string(),
                        }
                    })).collect::<Vec<_>>(),
                }),
                Role::Tool => json!({
                    "role": "tool",
                    "tool_call_id": message.tool_call_id,
                    "content": message.content,
                }),
                Role::System => json!({"role": "system", "content": message.content}),
                Role::Assistant => json!({"role": "assistant", "content": message.content}),
                Role::User if message.images.is_empty() => json!({"role": "user", "content": message.content}),
                Role::User => {
                    let mut parts = vec![json!({"type": "text", "text": message.content})];
                    parts.extend(message.images.iter().map(|image| json!({
                        "type": "image_url",
                        "image_url": {"url": format!("data:{};base64,{}", image.media_type, image.data)}
                    })));
                    json!({"role": "user", "content": parts})
                }
            })
            .collect::<Vec<_>>();
        let tools = wire_tools(&request.tools).iter().map(|tool| json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.input_schema}})).collect::<Vec<_>>();
        let mut body = json!({"model":self.model,"messages":messages});
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if let Some(limit) = self.tuning.max_output_tokens {
            body["max_tokens"] = json!(limit);
        }
        body
    }
}
impl ModelProvider for OpenAiCompatibleProvider {
    fn complete(&mut self, request: ModelRequest) -> Result<ModelResponse, ProviderError> {
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&self.request_body(&request))
            .send()
            .map_err(|_| request_error())?;
        let status = response.status();
        let body: Value = response.json().map_err(|_| response_error())?;
        if !status.is_success() {
            return Err(ProviderError::Failed(format!(
                "provider returned HTTP {status}"
            )));
        }
        parse_chat_body(body).map(|response| restore_tool_names(response, &request.tools))
    }

    fn complete_streaming(
        &mut self,
        request: ModelRequest,
        sink: &mut dyn StreamSink,
    ) -> Result<ModelResponse, ProviderError> {
        let mut body = self.request_body(&request);
        body["stream"] = json!(true);
        stream_response_with(
            self.client
                .post(&self.endpoint)
                .bearer_auth(&self.api_key)
                .json(&body),
            sink,
            parse_chat_stream,
        )
        .map(|response| restore_tool_names(response, &request.tools))
    }
}
fn parse_chat_body(body: Value) -> Result<ModelResponse, ProviderError> {
    let message = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .and_then(|choice| choice.get("message"))
        .ok_or_else(response_error)?;
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(|call| agent_protocol::ToolCall {
                    id: call.get("id").and_then(Value::as_str).map(str::to_owned),
                    name: call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    arguments: call
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                        .and_then(|raw| serde_json::from_str(raw).ok())
                        .unwrap_or_else(|| json!({})),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ModelResponse {
        message: Message::new(Role::Assistant, text),
        tool_calls: calls,
    })
}

fn parse_chat_stream(value: &Value) -> Vec<StreamEvent> {
    let choice = value.pointer("/choices/0");
    let mut events = Vec::new();
    if let Some(usage) = value.get("usage") {
        events.push(StreamEvent::Usage {
            input_tokens: usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        });
    }
    if let Some(text) = choice
        .and_then(|choice| choice.pointer("/delta/content"))
        .and_then(Value::as_str)
    {
        events.push(StreamEvent::TextDelta(text.into()));
    }
    if let Some(calls) = choice
        .and_then(|choice| choice.pointer("/delta/tool_calls"))
        .and_then(Value::as_array)
    {
        for call in calls {
            events.push(StreamEvent::ToolCallDelta {
                index: call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
                id: call.get("id").and_then(Value::as_str).map(str::to_owned),
                name: call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                arguments: call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
            });
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    #[test]
    fn openai_maps_request_and_auth_without_exposing_key() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = format!(
            "http://{}/v1/responses",
            listener.local_addr().expect("address")
        );
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("connection");
            let mut buffer = [0_u8; 8192];
            let size = stream.read(&mut buffer).expect("request");
            let request = String::from_utf8_lossy(&buffer[..size]);
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-key"));
            assert!(request.contains("\"model\":\"test-model\""));
            let body = r#"{"output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]},{"type":"function_call","call_id":"call-1","name":"read_file","arguments":"{\"path\":\"README.md\"}"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("response");
        });
        let mut provider = OpenAiProvider::with_credentials("test-model", "test-key", endpoint);
        let response = provider
            .complete(ModelRequest {
                messages: vec![Message::new(Role::User, "hello")],
                tools: vec![],
            })
            .expect("response");
        server.join().expect("server");
        assert_eq!(response.message.content, "ok");
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(response.tool_calls[0].id.as_deref(), Some("call-1"));
    }

    fn request_for(
        response_body: &'static str,
        assertion: impl FnOnce(String) + Send + 'static,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = format!("http://{}", listener.local_addr().expect("address"));
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("connection");
            let mut buffer = [0_u8; 16_384];
            let size = stream.read(&mut buffer).expect("request");
            assertion(String::from_utf8_lossy(&buffer[..size]).into_owned());
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .expect("response");
        });
        endpoint
    }

    #[test]
    fn anthropic_maps_auth_and_tool_use() {
        let body = r#"{"content":[{"type":"text","text":"ok"},{"type":"tool_use","id":"a1","name":"read_file","input":{"path":"README.md"}}]}"#;
        let endpoint = request_for(body, |request| {
            let lower = request.to_ascii_lowercase();
            assert!(lower.contains("x-api-key: anthropic-key"));
            assert!(lower.contains("anthropic-version: 2023-06-01"));
            assert!(request.contains("\"model\":\"claude-test\""));
        });
        let mut provider =
            AnthropicProvider::with_credentials("claude-test", "anthropic-key", endpoint);
        let response = provider
            .complete(ModelRequest {
                messages: vec![Message::new(Role::User, "hello")],
                tools: vec![],
            })
            .expect("response");
        assert_eq!(response.message.content, "ok");
        assert_eq!(response.tool_calls[0].name, "read_file");
    }

    #[test]
    fn gemini_maps_query_auth_and_function_call() {
        let body = r#"{"candidates":[{"content":{"parts":[{"text":"ok"},{"functionCall":{"name":"read_file","args":{"path":"README.md"}}}]}}]}"#;
        let endpoint = request_for(body, |request| {
            assert!(request.contains("key=gemini-key"));
            assert!(request.contains("gemini-test:generateContent"));
        });
        let mut provider = GeminiProvider::with_credentials("gemini-test", "gemini-key", endpoint);
        let response = provider
            .complete(ModelRequest {
                messages: vec![Message::new(Role::User, "hello")],
                tools: vec![],
            })
            .expect("response");
        assert_eq!(response.message.content, "ok");
        assert_eq!(response.tool_calls[0].name, "read_file");
    }

    #[test]
    fn generic_maps_bearer_and_chat_tool_call() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"ok","tool_calls":[{"id":"g1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"README.md\"}"}}]}}]}"#;
        let endpoint = request_for(body, |request| {
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer generic-key"));
            assert!(request.contains("\"model\":\"generic-test\""));
        });
        let mut provider =
            OpenAiCompatibleProvider::with_credentials("generic-test", "generic-key", endpoint);
        let response = provider
            .complete(ModelRequest {
                messages: vec![Message::new(Role::User, "hello")],
                tools: vec![],
            })
            .expect("response");
        assert_eq!(response.message.content, "ok");
        assert_eq!(response.tool_calls[0].id.as_deref(), Some("g1"));
    }

    #[derive(Default)]
    struct CollectSink {
        events: Vec<StreamEvent>,
    }
    impl StreamSink for CollectSink {
        fn emit(&mut self, event: StreamEvent) {
            self.events.push(event);
        }
    }

    #[test]
    fn openai_sse_stream_assembles_partial_text_and_tool_arguments() {
        let body = r#"data: {"type":"response.output_text.delta","delta":"hel"}

data: {"type":"response.output_text.delta","delta":"lo"}

data: {"type":"response.function_call_arguments.delta","output_index":0,"call_id":"c","name":"read_file","delta":"{\"path\":\"x\"}"}

data: [DONE]

"#;
        let endpoint = request_for(Box::leak(body.to_owned().into_boxed_str()), |_| {});
        let mut provider = OpenAiProvider::with_credentials("model", "key", endpoint);
        let mut sink = CollectSink::default();
        let response = provider
            .complete_streaming(
                ModelRequest {
                    messages: vec![],
                    tools: vec![],
                },
                &mut sink,
            )
            .expect("stream");
        assert_eq!(response.message.content, "hello");
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert!(sink.events.len() >= 3);
    }

    #[test]
    fn anthropic_gemini_and_generic_sse_streams_emit_text() {
        let cases = [
            (
                "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"a\"}}\n\n",
                0_u8,
            ),
            (
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"b\"}]}}]}\n\n",
                1_u8,
            ),
            (
                "data: {\"choices\":[{\"delta\":{\"content\":\"c\"}}]}\n\n",
                2_u8,
            ),
        ];
        for (body, kind) in cases {
            let endpoint = request_for(Box::leak(body.to_owned().into_boxed_str()), |_| {});
            let mut sink = CollectSink::default();
            let response = match kind {
                0 => AnthropicProvider::with_credentials("model", "key", endpoint)
                    .complete_streaming(
                        ModelRequest {
                            messages: vec![],
                            tools: vec![],
                        },
                        &mut sink,
                    ),
                1 => GeminiProvider::with_credentials("model", "key", endpoint).complete_streaming(
                    ModelRequest {
                        messages: vec![],
                        tools: vec![],
                    },
                    &mut sink,
                ),
                _ => OpenAiCompatibleProvider::with_credentials("model", "key", endpoint)
                    .complete_streaming(
                        ModelRequest {
                            messages: vec![],
                            tools: vec![],
                        },
                        &mut sink,
                    ),
            }
            .expect("stream");
            assert_eq!(response.message.content.len(), 1);
            assert_eq!(sink.events.len(), 1);
        }
    }

    #[test]
    fn malformed_and_oversized_streams_are_rejected() {
        let malformed = stream_sse(
            "data: {not-json}\n".as_bytes(),
            &mut CollectSink::default(),
            Instant::now() + Duration::from_secs(60),
            parse_chat_stream,
        );
        assert!(matches!(malformed, Err(ProviderError::Malformed(_))));
        let oversized = vec![b'x'; STREAM_LIMIT + 1];
        let oversized = stream_sse(
            oversized.as_slice(),
            &mut CollectSink::default(),
            Instant::now() + Duration::from_secs(60),
            parse_chat_stream,
        );
        assert!(matches!(oversized, Err(ProviderError::Oversized(_))));
    }

    struct CancelSink;
    impl StreamSink for CancelSink {
        fn emit(&mut self, _event: StreamEvent) {}

        fn is_cancelled(&self) -> bool {
            true
        }
    }

    #[test]
    fn cancelled_stream_stops_before_consuming_response() {
        let result = stream_sse(
            &b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n"[..],
            &mut CancelSink,
            Instant::now() + Duration::from_secs(60),
            parse_chat_stream,
        );
        assert!(matches!(result, Err(ProviderError::Cancelled)));
    }

    #[test]
    fn capability_profiles_are_explicit_about_unknowns() {
        assert_eq!(openai_profile().capabilities.context_window, None);
        assert!(openai_profile()
            .capabilities
            .reasoning_levels
            .contains(&ReasoningLevel::XHigh));
        assert_eq!(gemini_profile().capabilities.context_window, None);
        assert!(profile_for("not-a-provider").is_none());
    }

    fn tuning_request() -> ModelRequest {
        ModelRequest {
            messages: vec![
                Message::new(Role::System, "system"),
                Message::new(Role::User, "first"),
                Message::new(Role::User, "latest"),
            ],
            tools: vec![
                agent_protocol::ToolDefinition {
                    name: "a".into(),
                    description: "a".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
                agent_protocol::ToolDefinition {
                    name: "b".into(),
                    description: "b".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            ],
        }
    }

    fn named_tool(name: &str) -> agent_protocol::ToolDefinition {
        agent_protocol::ToolDefinition {
            name: name.into(),
            description: "d".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn tool_names_are_wire_safe_unique_and_restored() {
        let tools = vec![
            named_tool("read_file"),
            named_tool("mcp.git.status"),
            named_tool("mcp:git:status"),
            named_tool("lsp:rust-analyzer:symbols"),
        ];
        let wire = wire_tool_names(&tools);
        assert_eq!(wire[0], "read_file");
        assert_eq!(wire[1], "mcp_git_status");
        assert_eq!(wire[2], "mcp_git_status_1");
        assert_eq!(wire[3], "lsp_rust-analyzer_symbols");
        assert!(wire.iter().all(|name| !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')));
        let response = ModelResponse {
            message: Message::new(Role::Assistant, String::new()),
            tool_calls: vec![agent_protocol::ToolCall {
                id: None,
                name: "mcp_git_status_1".into(),
                arguments: serde_json::json!({}),
            }],
        };
        let restored = restore_tool_names(response, &tools);
        assert_eq!(restored.tool_calls[0].name, "mcp:git:status");
        let long = vec![named_tool(&format!("a.{}", "b".repeat(100)))];
        assert_eq!(wire_tool_names(&long)[0].len(), 64);
    }

    #[test]
    fn request_bodies_use_wire_names_for_every_adapter() {
        let mut request = tuning_request();
        request.tools = vec![named_tool("mcp.fs.read")];
        let anthropic = AnthropicProvider::with_credentials("m", "k", "http://localhost")
            .request_body(&request);
        assert_eq!(anthropic["tools"][0]["name"], "mcp_fs_read");
        let openai =
            OpenAiProvider::with_credentials("m", "k", "http://localhost").request_body(&request);
        assert_eq!(openai["tools"][0]["name"], "mcp_fs_read");
        let generic = OpenAiCompatibleProvider::with_credentials("m", "k", "http://localhost")
            .request_body(&request);
        assert_eq!(generic["tools"][0]["function"]["name"], "mcp_fs_read");
    }

    /// system -> user -> assistant(calls a,b) -> results(a,b) -> user, with tools
    /// named like MCP tools so the wire-name mapping is exercised as well.
    fn tool_history() -> ModelRequest {
        let call = |id: &str, name: &str| agent_protocol::ToolCall {
            id: Some(id.into()),
            name: name.into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        };
        let mut assistant = Message::new(Role::Assistant, "");
        assistant.tool_calls = vec![call("call_1", "read_file"), call("call_2", "mcp.fs.stat")];
        ModelRequest {
            messages: vec![
                Message::new(Role::System, "rules"),
                Message::new(Role::User, "inspect"),
                assistant,
                Message::tool_result(Some("call_1".into()), "file text"),
                Message::tool_result(Some("call_2".into()), "stat text"),
                Message::new(Role::User, "continue"),
            ],
            tools: vec![named_tool("read_file"), named_tool("mcp.fs.stat")],
        }
    }

    #[test]
    fn anthropic_sends_native_tool_use_and_one_merged_result_turn() {
        let body = AnthropicProvider::with_credentials("m", "k", "http://localhost")
            .request_body(&tool_history());
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 4, "{messages:#?}");
        // assistant: tool_use blocks only (empty text is omitted), wire names
        let uses = messages[1]["content"].as_array().expect("blocks");
        assert_eq!(uses.len(), 2);
        assert_eq!(uses[0]["type"], "tool_use");
        assert_eq!(uses[0]["id"], "call_1");
        assert_eq!(uses[1]["name"], "mcp_fs_stat");
        assert_eq!(uses[1]["input"]["path"], "a.rs");
        // one user turn carries both tool_result blocks, in call order
        let results = messages[2]["content"].as_array().expect("results");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["type"], "tool_result");
        assert_eq!(results[0]["tool_use_id"], "call_1");
        assert_eq!(results[1]["tool_use_id"], "call_2");
        assert_eq!(results[1]["content"], "stat text");
        // newest message carries the cache breakpoint
        assert_eq!(
            messages[3]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn openai_responses_uses_function_call_items_and_output_text() {
        let mut request = tool_history();
        request
            .messages
            .insert(2, Message::new(Role::Assistant, "looking"));
        let body =
            OpenAiProvider::with_credentials("m", "k", "http://localhost").request_body(&request);
        let input = body["input"].as_array().expect("input");
        let kinds = input
            .iter()
            .map(|item| {
                item["type"]
                    .as_str()
                    .or(item["role"].as_str())
                    .unwrap_or("?")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                "system",
                "user",
                "assistant",
                "function_call",
                "function_call",
                "function_call_output",
                "function_call_output",
                "user"
            ]
        );
        // assistant text must be output_text; role "tool" never appears
        assert_eq!(input[2]["content"][0]["type"], "output_text");
        assert!(input.iter().all(|item| item["role"] != "tool"));
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[4]["name"], "mcp_fs_stat");
        assert_eq!(input[4]["arguments"], "{\"path\":\"a.rs\"}");
        assert_eq!(input[5]["call_id"], "call_1");
        assert_eq!(input[6]["output"], "stat text");
    }

    #[test]
    fn chat_completions_pairs_tool_calls_with_tool_call_id() {
        let body = OpenAiCompatibleProvider::with_credentials("m", "k", "http://localhost")
            .request_body(&tool_history());
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages[2]["role"], "assistant");
        assert!(messages[2]["content"].is_null());
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            messages[2]["tool_calls"][1]["function"]["name"],
            "mcp_fs_stat"
        );
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        assert_eq!(messages[4]["tool_call_id"], "call_2");
    }

    #[test]
    fn orphaned_tool_history_is_flattened_for_every_adapter() {
        // The planner dropped the assistant message, leaving only the result.
        let mut request = tool_history();
        request.messages.remove(2);
        let anthropic = AnthropicProvider::with_credentials("m", "k", "http://localhost")
            .request_body(&request)
            .to_string();
        let openai = OpenAiProvider::with_credentials("m", "k", "http://localhost")
            .request_body(&request)
            .to_string();
        let chat = OpenAiCompatibleProvider::with_credentials("m", "k", "http://localhost")
            .request_body(&request)
            .to_string();
        for body in [&anthropic, &openai, &chat] {
            assert!(!body.contains("tool_result\""), "{body}");
            assert!(!body.contains("function_call_output"), "{body}");
            assert!(!body.contains("tool_call_id"), "{body}");
            assert!(body.contains("[tool result]"), "{body}");
        }
        // A call with a missing result is flattened together with its partner.
        let mut request = tool_history();
        request.messages.remove(4);
        let chat = OpenAiCompatibleProvider::with_credentials("m", "k", "http://localhost")
            .request_body(&request)
            .to_string();
        assert!(!chat.contains("tool_calls"), "{chat}");
        assert!(chat.contains("[tool call read_file]"), "{chat}");
    }

    #[test]
    fn gemini_flattens_tool_history_and_skips_empty_parts() {
        let body = GeminiProvider::with_credentials("m", "k", "http://localhost")
            .request_body(&tool_history());
        let contents = body["contents"].as_array().expect("contents");
        assert!(contents.iter().all(|item| item["parts"][0]["text"]
            .as_str()
            .is_some_and(|text| !text.is_empty())));
        let text = body.to_string();
        assert!(text.contains("[tool call read_file]"));
        assert!(text.contains("[tool result]"));
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "rules");
        // no tools -> no empty functionDeclarations list
        let mut request = tool_history();
        request.tools.clear();
        let body =
            GeminiProvider::with_credentials("m", "k", "http://localhost").request_body(&request);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn anthropic_body_marks_stable_prefix_for_caching_and_maps_effort() {
        let provider = AnthropicProvider::with_credentials("m", "k", "http://localhost");
        let body = provider.request_body(&tuning_request());
        assert_eq!(body["max_tokens"], 16_000);
        assert!(body.get("output_config").is_none());
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["system"][0]["text"], "system");
        assert_eq!(body["messages"][0]["content"], "first");
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );

        let tuned = provider.with_tuning(RequestTuning {
            effort: Some("minimal".into()),
            max_output_tokens: Some(4_096),
        });
        let body = tuned.request_body(&tuning_request());
        assert_eq!(body["max_tokens"], 4_096);
        assert_eq!(body["output_config"]["effort"], "low");
    }

    fn image_request() -> ModelRequest {
        let mut user = Message::new(Role::User, "what is this?");
        user.images = vec![agent_protocol::ImageInput {
            media_type: "image/png".into(),
            data: "iVBORw0KGgo=".into(),
        }];
        ModelRequest {
            messages: vec![user],
            tools: vec![],
        }
    }

    #[test]
    fn attached_images_use_each_providers_native_part() {
        let request = image_request();
        let anthropic = AnthropicProvider::with_credentials("m", "k", "http://localhost")
            .request_body(&request);
        let blocks = anthropic["messages"][0]["content"]
            .as_array()
            .expect("blocks");
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["media_type"], "image/png");
        assert_eq!(blocks[0]["source"]["data"], "iVBORw0KGgo=");
        assert_eq!(blocks[1]["text"], "what is this?");
        assert_eq!(blocks[1]["cache_control"]["type"], "ephemeral");

        let openai =
            OpenAiProvider::with_credentials("m", "k", "http://localhost").request_body(&request);
        let parts = openai["input"][0]["content"].as_array().expect("parts");
        assert_eq!(parts[1]["type"], "input_image");
        assert_eq!(parts[1]["image_url"], "data:image/png;base64,iVBORw0KGgo=");

        let chat = OpenAiCompatibleProvider::with_credentials("m", "k", "http://localhost")
            .request_body(&request);
        assert_eq!(
            chat["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,iVBORw0KGgo="
        );

        let gemini =
            GeminiProvider::with_credentials("m", "k", "http://localhost").request_body(&request);
        assert_eq!(
            gemini["contents"][0]["parts"][0]["inlineData"]["mimeType"],
            "image/png"
        );
        assert_eq!(gemini["contents"][0]["parts"][1]["text"], "what is this?");
    }

    #[test]
    fn later_system_messages_are_kept_as_notes_not_dropped() {
        let mut request = tuning_request();
        request
            .messages
            .insert(0, Message::new(Role::System, "base"));
        request
            .messages
            .insert(3, Message::new(Role::System, "memory"));
        let anthropic = AnthropicProvider::with_credentials("m", "k", "http://localhost")
            .request_body(&request);
        assert_eq!(anthropic["system"][0]["text"], "base\n\nsystem");
        assert_eq!(anthropic["messages"][1]["content"], "[system note]\nmemory");
        let gemini =
            GeminiProvider::with_credentials("m", "k", "http://localhost").request_body(&request);
        assert_eq!(
            gemini["systemInstruction"]["parts"][0]["text"],
            "base\n\nsystem"
        );
        assert_eq!(gemini["contents"].as_array().map(Vec::len), Some(3));
    }

    #[test]
    fn openai_body_sends_effort_only_when_explicit() {
        let provider = OpenAiProvider::with_credentials("m", "k", "http://localhost");
        assert!(provider
            .request_body(&tuning_request())
            .get("reasoning")
            .is_none());
        let auto = OpenAiProvider::with_credentials("m", "k", "http://localhost").with_tuning(
            RequestTuning {
                effort: Some("auto".into()),
                max_output_tokens: None,
            },
        );
        assert!(auto
            .request_body(&tuning_request())
            .get("reasoning")
            .is_none());
        let high = provider.with_tuning(RequestTuning {
            effort: Some("high".into()),
            max_output_tokens: Some(2_000),
        });
        let body = high.request_body(&tuning_request());
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["max_output_tokens"], 2_000);
    }

    #[test]
    fn anthropic_message_start_reports_cache_usage() {
        let events = parse_anthropic_stream(&serde_json::json!({
            "type": "message_start",
            "message": {"usage": {
                "input_tokens": 12,
                "output_tokens": 1,
                "cache_read_input_tokens": 900,
                "cache_creation_input_tokens": 30
            }}
        }));
        assert_eq!(
            events,
            vec![StreamEvent::Usage {
                input_tokens: 12,
                output_tokens: 1,
                cache_read_tokens: 900,
                cache_write_tokens: 30,
                reasoning_tokens: 0,
            }]
        );
    }

    #[test]
    fn stream_usage_is_forwarded_as_typed_event() {
        let events = parse_chat_stream(&serde_json::json!({
            "usage": {"prompt_tokens": 3, "completion_tokens": 5}
        }));
        assert_eq!(
            events,
            vec![StreamEvent::Usage {
                input_tokens: 3,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            }]
        );
    }

    #[test]
    fn profiles_are_typed_and_errors_do_not_echo_secrets() {
        assert_eq!(
            anthropic_profile().capabilities.tool_shape,
            ToolShape::AnthropicMessages
        );
        assert_eq!(
            gemini_profile().capabilities.tool_shape,
            ToolShape::GeminiFunctions
        );
        assert_eq!(
            generic_profile().capabilities.tool_shape,
            ToolShape::OpenAiChatCompletions
        );
        let mut provider = OpenAiCompatibleProvider::with_credentials(
            "model",
            "do-not-leak",
            "http://127.0.0.1:1",
        );
        let error = provider
            .complete(ModelRequest {
                messages: vec![],
                tools: vec![],
            })
            .expect_err("connection should fail");
        assert!(!format!("{error:?}").contains("do-not-leak"));
    }
}

#[derive(Default)]
pub struct FakeProvider;

impl ModelProvider for FakeProvider {
    fn complete(&mut self, request: ModelRequest) -> Result<ModelResponse, ProviderError> {
        let content = request
            .messages
            .last()
            .map(|message| format!("fake: {}", message.content))
            .unwrap_or_else(|| "fake: empty request".into());
        Ok(ModelResponse {
            message: Message::new(Role::Assistant, content),
            tool_calls: vec![],
        })
    }
}
