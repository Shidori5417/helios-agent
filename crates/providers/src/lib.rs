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
fn wire_tool_names(tools: &[agent_protocol::ToolDefinition]) -> Vec<String> {
    let mut used = std::collections::BTreeSet::new();
    tools
        .iter()
        .map(|tool| {
            let base = tool
                .name
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                        ch
                    } else {
                        '_'
                    }
                })
                .take(64)
                .collect::<String>();
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
            message: Message {
                role: Role::Assistant,
                content: self.text,
            },
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
        let input = request.messages.iter().map(|message| {
            json!({
                "role": match message.role { Role::System => "system", Role::User => "user", Role::Assistant => "assistant", Role::Tool => "tool" },
                "content": [{"type": "input_text", "text": message.content}]
            })
        }).collect::<Vec<_>>();
        let tools = wire_tools(&request.tools).iter().map(|tool| json!({
            "type": "function", "name": tool.name, "description": tool.description, "parameters": tool.input_schema
        })).collect::<Vec<_>>();
        let mut body = json!({"model": self.model, "input": input, "tools": tools});
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
        message: Message {
            role: Role::Assistant,
            content: text,
        },
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
        let (system, conversation) = split_leading_system(&request.messages);
        let last = conversation.len().saturating_sub(1);
        let messages = conversation
            .iter()
            .enumerate()
            .map(|(index, message)| {
                let role = match message.role {
                    Role::Assistant => "assistant",
                    _ => "user",
                };
                let text = system_note(message);
                if index == last && !text.is_empty() {
                    json!({"role": role, "content": [{"type": "text", "text": text, "cache_control": cache}]})
                } else {
                    json!({"role": role, "content": text})
                }
            })
            .collect::<Vec<_>>();
        let mut tools = wire_tools(&request.tools).iter().map(|tool| json!({"name":tool.name,"description":tool.description,"input_schema":tool.input_schema})).collect::<Vec<_>>();
        if let Some(tool) = tools.last_mut() {
            tool["cache_control"] = cache.clone();
        }
        let max_tokens = self
            .tuning
            .max_output_tokens
            .unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS);
        let mut body =
            json!({"model":self.model,"max_tokens":max_tokens,"messages":messages,"tools":tools});
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
        message: Message {
            role: Role::Assistant,
            content: text,
        },
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
        let (system, conversation) = split_leading_system(&request.messages);
        let contents = conversation.iter().map(|message| json!({"role": if matches!(message.role, Role::Assistant) {"model"} else {"user"},"parts":[{"text":system_note(message)}]})).collect::<Vec<_>>();
        let declarations = wire_tools(&request.tools).iter().map(|tool| json!({"name":tool.name,"description":tool.description,"parameters":tool.input_schema})).collect::<Vec<_>>();
        let mut body = json!({"contents":contents,"tools":[{"functionDeclarations":declarations}],"generationConfig":{"responseMimeType":"text/plain"}});
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
        message: Message {
            role: Role::Assistant,
            content: text,
        },
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
        let messages = request.messages.iter().map(|message| json!({"role":match message.role { Role::System=>"system",Role::Assistant=>"assistant",Role::Tool=>"tool",Role::User=>"user"},"content":message.content})).collect::<Vec<_>>();
        let tools = wire_tools(&request.tools).iter().map(|tool| json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.input_schema}})).collect::<Vec<_>>();
        let mut body = json!({"model":self.model,"messages":messages,"tools":tools});
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
        message: Message {
            role: Role::Assistant,
            content: text,
        },
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
                messages: vec![Message {
                    role: Role::User,
                    content: "hello".into(),
                }],
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
                messages: vec![Message {
                    role: Role::User,
                    content: "hello".into(),
                }],
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
                messages: vec![Message {
                    role: Role::User,
                    content: "hello".into(),
                }],
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
                messages: vec![Message {
                    role: Role::User,
                    content: "hello".into(),
                }],
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
                Message {
                    role: Role::System,
                    content: "system".into(),
                },
                Message {
                    role: Role::User,
                    content: "first".into(),
                },
                Message {
                    role: Role::User,
                    content: "latest".into(),
                },
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
            message: Message {
                role: Role::Assistant,
                content: String::new(),
            },
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

    #[test]
    fn later_system_messages_are_kept_as_notes_not_dropped() {
        let mut request = tuning_request();
        request.messages.insert(
            0,
            Message {
                role: Role::System,
                content: "base".into(),
            },
        );
        request.messages.insert(
            3,
            Message {
                role: Role::System,
                content: "memory".into(),
            },
        );
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
            message: Message {
                role: Role::Assistant,
                content,
            },
            tool_calls: vec![],
        })
    }
}
