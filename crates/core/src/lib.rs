use agent_context::{
    CompactionRequest, Compactor, ContextPlan, ContextSegment, Priority, SegmentKind, Stability,
};
use agent_protocol::{Event, Message, ModelRequest, Role};
use agent_providers::{ModelProvider, ProviderError, StreamEvent, StreamSink};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("tool loop exceeded iteration limit ({0})")]
    IterationLimit(usize),
    #[error("tool execution failed: {0}")]
    Tool(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleEvent {
    SessionStart,
    PreToolUse,
    PostToolUse,
    PreEdit,
    PostEdit,
    PreCompact,
    Stop,
}

pub trait HookRunner: Send {
    fn run(&mut self, event: LifecycleEvent) -> Result<String, String>;
}

#[derive(Debug, PartialEq, Eq)]
pub struct TurnResult {
    pub response: String,
    pub events: Vec<Event>,
}

#[derive(Clone, Debug, Default)]
pub struct SessionState {
    pub objective: String,
    pub messages: Vec<Message>,
    pub events: Vec<Event>,
    pub completed_steps: Vec<String>,
    pub changes: Vec<String>,
    pub verification_results: Vec<String>,
    pub open_failures: Vec<String>,
    pub context_plan: Option<ContextPlan>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetrievedMemory {
    pub id: String,
    pub key: String,
    pub content: String,
}

pub trait MemoryRetriever {
    fn retrieve(
        &self,
        scope: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RetrievedMemory>, String>;
}

pub trait EventSink {
    fn emit(&mut self, event: Event);
    fn is_cancelled(&self) -> bool {
        false
    }
}

struct ProviderEventForwarder<'a> {
    sink: &'a mut dyn EventSink,
    events: &'a mut Vec<Event>,
}
impl StreamSink for ProviderEventForwarder<'_> {
    fn is_cancelled(&self) -> bool {
        self.sink.is_cancelled()
    }
    fn emit(&mut self, event: StreamEvent) {
        let event = match event {
            StreamEvent::TextDelta(text) => Event::ModelDelta(text),
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
            } => {
                let total = input_tokens
                    .saturating_add(output_tokens)
                    .saturating_add(cache_read_tokens)
                    .saturating_add(cache_write_tokens)
                    .saturating_add(reasoning_tokens);
                Event::UsageUpdated(format!("input={input_tokens} output={output_tokens} cache_read={cache_read_tokens} cache_write={cache_write_tokens} reasoning={reasoning_tokens} total={total}"))
            }
            StreamEvent::ToolCallDelta { .. } => return,
        };
        self.sink.emit(event.clone());
        self.events.push(event);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolExecution {
    pub status: String,
    pub output: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolFailureKind {
    Recoverable,
    PermissionDenied,
    PathViolation,
    InvalidInput,
    Fatal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolFailure {
    pub kind: ToolFailureKind,
    pub message: String,
}

pub trait ToolExecutor {
    fn definitions(&self) -> Vec<agent_protocol::ToolDefinition> {
        Vec::new()
    }
    fn execute(&mut self, call: &agent_protocol::ToolCall) -> Result<ToolExecution, String>;

    fn execute_typed(
        &mut self,
        call: &agent_protocol::ToolCall,
    ) -> Result<ToolExecution, ToolFailure> {
        self.execute(call).map_err(|message| ToolFailure {
            kind: ToolFailureKind::Recoverable,
            message,
        })
    }
}

#[derive(Default)]
pub struct VecEventSink {
    pub events: Vec<Event>,
}

impl EventSink for VecEventSink {
    fn emit(&mut self, event: Event) {
        self.events.push(event);
    }
}

pub struct AgentLoop<P> {
    provider: P,
    pub session: SessionState,
    context_budget: u64,
    memory_retriever: Option<Box<dyn MemoryRetriever>>,
    memory_scope: Option<String>,
    memory_limit: usize,
    compactor: Option<Box<dyn Compactor>>,
    hooks: Option<Box<dyn HookRunner>>,
}

impl<P> AgentLoop<P>
where
    P: ModelProvider,
{
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            session: SessionState::default(),
            context_budget: 16_000,
            memory_retriever: None,
            memory_scope: None,
            memory_limit: 8,
            compactor: None,
            hooks: None,
        }
    }

    pub fn with_context_budget(mut self, budget: u64) -> Self {
        self.context_budget = budget;
        self
    }

    pub fn with_memory_retriever(mut self, retriever: Box<dyn MemoryRetriever>) -> Self {
        self.memory_retriever = Some(retriever);
        self
    }

    pub fn with_memory_scope(mut self, scope: impl Into<String>) -> Self {
        self.memory_scope = Some(scope.into());
        self
    }

    pub fn with_memory_limit(mut self, limit: usize) -> Self {
        self.memory_limit = limit.min(32);
        self
    }

    pub fn with_compactor(mut self, compactor: Box<dyn Compactor>) -> Self {
        self.compactor = Some(compactor);
        self
    }

    pub fn with_hooks(mut self, hooks: Box<dyn HookRunner>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    fn run_hook(
        &mut self,
        event: LifecycleEvent,
        sink: &mut impl EventSink,
    ) -> Result<(), CoreError> {
        let Some(hooks) = self.hooks.as_mut() else {
            return Ok(());
        };
        let name = format!("{event:?}");
        let started = Event::HookStarted(name.clone());
        sink.emit(started.clone());
        self.session.events.push(started);
        match hooks.run(event) {
            Ok(output) => {
                let result = Event::HookResult(format!("{name}: {output}"));
                sink.emit(result.clone());
                self.session.events.push(result);
                Ok(())
            }
            Err(error) => {
                let result = Event::HookResult(format!("{name}: failed"));
                sink.emit(result.clone());
                self.session.events.push(result);
                Err(CoreError::Tool(format!("hook {name} failed: {error}")))
            }
        }
    }

    pub fn restore_session(&mut self, objective: impl Into<String>, messages: Vec<Message>) {
        self.session.objective = objective.into();
        self.session.messages = messages;
        self.session.events.clear();
        self.session.completed_steps.clear();
        self.session.changes.clear();
        self.session.verification_results.clear();
        self.session.open_failures.clear();
        self.session.context_plan = None;
    }

    pub fn run_once(&mut self, user_input: impl Into<String>) -> Result<TurnResult, CoreError> {
        let mut sink = VecEventSink::default();
        self.run_once_streaming(user_input, &mut sink)?;
        Ok(TurnResult {
            response: sink
                .events
                .iter()
                .find_map(|event| match event {
                    Event::ModelDelta(text) => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default(),
            events: sink.events,
        })
    }

    pub fn run_once_streaming(
        &mut self,
        user_input: impl Into<String>,
        sink: &mut impl EventSink,
    ) -> Result<String, CoreError> {
        self.session.objective = user_input.into();
        self.session.messages.push(Message {
            role: Role::User,
            content: self.session.objective.clone(),
        });
        let start = Event::TurnStarted;
        sink.emit(start.clone());
        self.session.events.push(start);
        self.run_hook(LifecycleEvent::SessionStart, sink)?;
        let request = ModelRequest {
            messages: self.session.messages.clone(),
            tools: vec![],
        };
        let mut forwarder = ProviderEventForwarder {
            sink,
            events: &mut self.session.events,
        };
        let response = self.provider.complete_streaming(request, &mut forwarder)?;
        self.run_hook(LifecycleEvent::Stop, sink)?;
        let completed = Event::TurnCompleted;
        sink.emit(completed.clone());
        self.session.events.push(completed);
        self.session.messages.push(response.message);
        Ok(self
            .session
            .events
            .last()
            .and_then(|_| self.session.messages.last())
            .map(|message| message.content.clone())
            .unwrap_or_default())
    }

    pub fn run_coding_loop(
        &mut self,
        user_input: impl Into<String>,
        executor: &mut impl ToolExecutor,
        max_iterations: usize,
        sink: &mut impl EventSink,
    ) -> Result<String, CoreError> {
        let limit = max_iterations.clamp(1, 16);
        let mut recovery_attempts = 0_usize;
        self.session.objective = user_input.into();
        self.session.messages.push(Message {
            role: Role::User,
            content: self.session.objective.clone(),
        });
        let start = Event::TurnStarted;
        sink.emit(start.clone());
        self.session.events.push(start);
        self.run_hook(LifecycleEvent::SessionStart, sink)?;
        let startup_memory = if let (Some(retriever), Some(scope)) =
            (self.memory_retriever.as_ref(), self.memory_scope.as_deref())
        {
            retriever
                .retrieve(scope, &self.session.objective, self.memory_limit)
                .map_err(CoreError::Tool)?
                .into_iter()
                .take(self.memory_limit)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for iteration in 0..limit {
            let definitions = executor.definitions();
            let mut segments = self
                .session
                .messages
                .iter()
                .map(|message| {
                    let priority = match message.role {
                        Role::System | Role::User => Priority::Required,
                        _ => Priority::Normal,
                    };
                    ContextSegment::new(
                        SegmentKind::Conversation,
                        priority,
                        Stability::SessionStable,
                        &message.content,
                    )
                })
                .collect::<Vec<_>>();
            segments.extend(startup_memory.iter().map(|memory| {
                ContextSegment::new(
                    SegmentKind::Memory,
                    Priority::Normal,
                    Stability::Static,
                    format!("[{}] {}", memory.key, memory.content),
                )
                .with_policy(agent_context::TruncationPolicy::None)
            }));
            let input_tokens = segments.iter().fold(0_u64, |total, segment| {
                total.saturating_add(segment.estimated_tokens)
            });
            let plan = ContextPlan::build(segments, self.context_budget);
            let selected = plan
                .segments
                .iter()
                .map(|item| {
                    (
                        item.segment.source_hash.clone(),
                        item.segment.content.clone(),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>();
            let mut request_messages = self
                .session
                .messages
                .iter()
                .filter_map(|message| {
                    let priority = match message.role {
                        Role::System | Role::User => Priority::Required,
                        _ => Priority::Normal,
                    };
                    let hash = ContextSegment::new(
                        SegmentKind::Conversation,
                        priority,
                        Stability::SessionStable,
                        &message.content,
                    )
                    .source_hash;
                    selected.get(&hash).map(|content| Message {
                        role: message.role.clone(),
                        content: content.clone(),
                    })
                })
                .collect::<Vec<_>>();
            request_messages.extend(startup_memory.iter().filter_map(|memory| {
                let content = format!("[{}] {}", memory.key, memory.content);
                let hash = ContextSegment::new(
                    SegmentKind::Memory,
                    Priority::Normal,
                    Stability::Static,
                    &content,
                )
                .source_hash;
                selected.get(&hash).map(|content| Message {
                    role: Role::System,
                    content: content.clone(),
                })
            }));
            if input_tokens > self.context_budget || !plan.dropped.is_empty() {
                let compact_request = CompactionRequest {
                    objective: self.session.objective.clone(),
                    accepted_constraints: Vec::new(),
                    decisions: self.session.completed_steps.clone(),
                    unresolved_failures: self.session.open_failures.clone(),
                    verification_results: self.session.verification_results.clone(),
                    relevant_memory_ids: startup_memory
                        .iter()
                        .map(|memory| memory.id.clone())
                        .collect(),
                };
                if self.compactor.is_some() {
                    self.run_hook(LifecycleEvent::PreCompact, sink)?;
                }
                if let Some(compactor) = self.compactor.as_mut() {
                    match compactor.compact(compact_request) {
                        Ok(result) => {
                            let summary = result.summary.chars().take(2_000).collect::<String>();
                            let checkpoint = format!(
                                "compaction checkpoint: objective={} decisions={}",
                                result.checkpoint.objective,
                                result.checkpoint.decisions.len()
                            );
                            request_messages.insert(
                                0,
                                Message {
                                    role: Role::System,
                                    content: format!("{checkpoint}\nsummary: {summary}"),
                                },
                            );
                            let event = Event::ContextCompacted(format!(
                                "dropped {} context segments; compaction applied",
                                plan.dropped.len()
                            ));
                            sink.emit(event.clone());
                            self.session.events.push(event);
                        }
                        Err(error) => {
                            let event = Event::Error(format!("compaction failed safely: {error}"));
                            sink.emit(event.clone());
                            self.session.events.push(event);
                        }
                    }
                } else {
                    let event = Event::ContextCompacted(format!(
                        "dropped {} context segments",
                        plan.dropped.len()
                    ));
                    sink.emit(event.clone());
                    self.session.events.push(event);
                }
            }
            self.session.context_plan = Some(plan);
            let request = ModelRequest {
                messages: request_messages,
                tools: definitions,
            };
            let mut forwarder = ProviderEventForwarder {
                sink,
                events: &mut self.session.events,
            };
            let response = self.provider.complete_streaming(request, &mut forwarder)?;
            self.session.messages.push(response.message.clone());
            if response.tool_calls.is_empty() {
                self.run_hook(LifecycleEvent::Stop, sink)?;
                let completed = Event::TurnCompleted;
                sink.emit(completed.clone());
                self.session.events.push(completed);
                return Ok(response.message.content);
            }
            for call in response.tool_calls {
                self.run_hook(LifecycleEvent::PreToolUse, sink)?;
                if call.name == "apply_patch" {
                    self.run_hook(LifecycleEvent::PreEdit, sink)?;
                }
                if let Some(name) = call.name.strip_prefix("mcp:") {
                    let event = Event::McpToolDiscovered(name.to_owned());
                    sink.emit(event.clone());
                    self.session.events.push(event);
                }
                let requested = Event::ToolRequested(call.name.clone());
                sink.emit(requested.clone());
                self.session.events.push(requested);
                let started = Event::ToolStarted(call.name.clone());
                sink.emit(started.clone());
                self.session.events.push(started);
                let result = match executor.execute_typed(&call) {
                    Ok(result) => result,
                    Err(error)
                        if error.kind == ToolFailureKind::Recoverable && recovery_attempts < 2 =>
                    {
                        recovery_attempts += 1;
                        let feedback = format!(
                            "tool error (recovery attempt {recovery_attempts}/2): {}",
                            error.message
                        );
                        let output = Event::ToolOutputDelta(feedback.clone());
                        sink.emit(output.clone());
                        self.session.events.push(output);
                        let completed =
                            Event::ToolCompleted(format!("{}: error; retrying", call.name));
                        sink.emit(completed.clone());
                        self.session.events.push(completed);
                        self.session.messages.push(Message {
                            role: Role::Tool,
                            content: feedback,
                        });
                        continue;
                    }
                    Err(error) => {
                        let event = if error.kind == ToolFailureKind::Recoverable {
                            Event::Error(format!(
                                "{}: recovery budget exhausted: {}",
                                call.name, error.message
                            ))
                        } else {
                            Event::PermissionRequested(format!("{}: {}", call.name, error.message))
                        };
                        sink.emit(event.clone());
                        self.session.events.push(event);
                        let completed = Event::ToolCompleted(format!("{}: error", call.name));
                        sink.emit(completed.clone());
                        self.session.events.push(completed);
                        return Err(CoreError::Tool(error.message));
                    }
                };
                let output = Event::ToolOutputDelta(result.output.clone());
                sink.emit(output.clone());
                self.session.events.push(output);
                if call.name.starts_with("mcp:") {
                    let event = Event::McpToolResult(result.output.clone());
                    sink.emit(event.clone());
                    self.session.events.push(event);
                }
                if call.name == "apply_patch" {
                    let event = Event::PatchProduced(call.name.clone());
                    sink.emit(event.clone());
                    self.session.events.push(event);
                }
                if call.name == "verify" || call.name == "verification" {
                    let event = Event::VerificationResult(result.status.clone());
                    sink.emit(event.clone());
                    self.session.events.push(event);
                }
                let completed = Event::ToolCompleted(format!("{}: {}", call.name, result.status));
                sink.emit(completed.clone());
                self.session.events.push(completed);
                self.session.messages.push(Message {
                    role: Role::Tool,
                    content: result.output,
                });
                self.run_hook(LifecycleEvent::PostToolUse, sink)?;
                if call.name == "apply_patch" {
                    self.run_hook(LifecycleEvent::PostEdit, sink)?;
                }
            }
            self.session
                .completed_steps
                .push(format!("iteration {}", iteration + 1));
        }
        Err(CoreError::IterationLimit(limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_context::{CompactionError, CompactionResult};
    use agent_protocol::{ModelResponse, ToolCall};
    use agent_providers::FakeProvider;

    #[test]
    fn fake_provider_completes_one_turn_and_emits_events() {
        let mut agent = AgentLoop::new(FakeProvider);
        let result = agent.run_once("inspect repository").expect("fake turn");
        assert_eq!(result.response, "fake: inspect repository");
        assert_eq!(
            result.events,
            vec![
                Event::TurnStarted,
                Event::ModelDelta("fake: inspect repository".into()),
                Event::TurnCompleted,
            ]
        );
    }

    struct ScriptedProvider {
        responses: Vec<ModelResponse>,
    }
    impl ModelProvider for ScriptedProvider {
        fn complete(&mut self, _request: ModelRequest) -> Result<ModelResponse, ProviderError> {
            Ok(self.responses.remove(0))
        }
    }
    struct RecordingExecutor {
        calls: usize,
    }
    impl ToolExecutor for RecordingExecutor {
        fn execute(&mut self, _call: &ToolCall) -> Result<ToolExecution, String> {
            self.calls += 1;
            Ok(ToolExecution {
                status: "ok".into(),
                output: "tool result".into(),
            })
        }
    }

    #[test]
    fn coding_loop_feeds_tool_result_and_stops() {
        let provider = ScriptedProvider {
            responses: vec![
                ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: "calling".into(),
                    },
                    tool_calls: vec![ToolCall {
                        id: Some("1".into()),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"path":"x"}),
                    }],
                },
                ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: "done".into(),
                    },
                    tool_calls: vec![],
                },
            ],
        };
        let mut agent = AgentLoop::new(provider);
        let mut executor = RecordingExecutor { calls: 0 };
        let mut sink = VecEventSink::default();
        let result = agent
            .run_coding_loop("inspect", &mut executor, 4, &mut sink)
            .expect("loop");
        assert_eq!(result, "done");
        assert_eq!(executor.calls, 1);
        assert!(sink
            .events
            .iter()
            .any(|event| matches!(event, Event::ToolRequested(name) if name == "read_file")));
    }

    #[test]
    fn coding_loop_has_bounded_iterations() {
        let response = ModelResponse {
            message: Message {
                role: Role::Assistant,
                content: "again".into(),
            },
            tool_calls: vec![ToolCall {
                id: None,
                name: "process".into(),
                arguments: serde_json::json!({}),
            }],
        };
        let provider = ScriptedProvider {
            responses: vec![response.clone(), response.clone()],
        };
        let mut agent = AgentLoop::new(provider);
        let mut executor = RecordingExecutor { calls: 0 };
        let mut sink = VecEventSink::default();
        assert!(matches!(
            agent.run_coding_loop("loop", &mut executor, 2, &mut sink),
            Err(CoreError::IterationLimit(2))
        ));
    }

    struct ContextRecordingProvider {
        seen: Vec<ModelRequest>,
    }
    impl ModelProvider for ContextRecordingProvider {
        fn complete(&mut self, request: ModelRequest) -> Result<ModelResponse, ProviderError> {
            self.seen.push(request);
            Ok(ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: "done".into(),
                },
                tool_calls: vec![],
            })
        }
    }

    #[test]
    fn context_budget_is_applied_to_provider_request() {
        let provider = ContextRecordingProvider { seen: Vec::new() };
        let mut agent = AgentLoop::new(provider).with_context_budget(4);
        agent.restore_session(
            "objective",
            vec![
                Message {
                    role: Role::User,
                    content: "short".into(),
                },
                Message {
                    role: Role::Assistant,
                    content: "a very long assistant transcript that should be truncated".into(),
                },
            ],
        );
        let mut executor = RecordingExecutor { calls: 0 };
        let mut sink = VecEventSink::default();
        agent
            .run_coding_loop("next", &mut executor, 1, &mut sink)
            .expect("loop");
        assert!(
            agent.provider.seen[0]
                .messages
                .iter()
                .map(|message| message.content.len())
                .sum::<usize>()
                < agent
                    .session
                    .messages
                    .iter()
                    .map(|message| message.content.len())
                    .sum::<usize>()
        );
    }

    struct RecoveringExecutor {
        failures: usize,
        calls: usize,
        kind: ToolFailureKind,
    }
    impl ToolExecutor for RecoveringExecutor {
        fn execute(&mut self, _call: &ToolCall) -> Result<ToolExecution, String> {
            Ok(ToolExecution {
                status: "ok".into(),
                output: "patched".into(),
            })
        }
        fn execute_typed(&mut self, _call: &ToolCall) -> Result<ToolExecution, ToolFailure> {
            self.calls += 1;
            if self.failures > 0 {
                self.failures -= 1;
                Err(ToolFailure {
                    kind: self.kind,
                    message: "patch conflict".into(),
                })
            } else {
                Ok(ToolExecution {
                    status: "ok".into(),
                    output: "patched".into(),
                })
            }
        }
    }

    fn tool_response(name: &str) -> ModelResponse {
        ModelResponse {
            message: Message {
                role: Role::Assistant,
                content: "tool call".into(),
            },
            tool_calls: vec![ToolCall {
                id: Some(name.into()),
                name: name.into(),
                arguments: serde_json::json!({}),
            }],
        }
    }

    #[test]
    fn recoverable_patch_conflict_is_fed_back_and_corrected_patch_succeeds() {
        let provider = ScriptedProvider {
            responses: vec![
                tool_response("apply_patch"),
                tool_response("apply_patch"),
                ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: "done".into(),
                    },
                    tool_calls: vec![],
                },
            ],
        };
        let mut agent = AgentLoop::new(provider);
        let mut executor = RecoveringExecutor {
            failures: 1,
            calls: 0,
            kind: ToolFailureKind::Recoverable,
        };
        let mut sink = VecEventSink::default();
        assert_eq!(
            agent
                .run_coding_loop("patch", &mut executor, 4, &mut sink)
                .expect("recovered"),
            "done"
        );
        assert_eq!(executor.calls, 2);
        assert!(agent
            .session
            .messages
            .iter()
            .any(|message| message.content.contains("recovery attempt")));
    }

    #[test]
    fn repeated_recoverable_errors_hit_hard_recovery_budget() {
        let provider = ScriptedProvider {
            responses: vec![
                tool_response("apply_patch"),
                tool_response("apply_patch"),
                tool_response("apply_patch"),
            ],
        };
        let mut agent = AgentLoop::new(provider);
        let mut executor = RecoveringExecutor {
            failures: 9,
            calls: 0,
            kind: ToolFailureKind::Recoverable,
        };
        let mut sink = VecEventSink::default();
        assert!(matches!(
            agent.run_coding_loop("patch", &mut executor, 8, &mut sink),
            Err(CoreError::Tool(_))
        ));
        assert_eq!(executor.calls, 3);
    }

    #[test]
    fn permission_denial_is_not_retried() {
        let provider = ScriptedProvider {
            responses: vec![tool_response("shell")],
        };
        let mut agent = AgentLoop::new(provider);
        let mut executor = RecoveringExecutor {
            failures: 9,
            calls: 0,
            kind: ToolFailureKind::PermissionDenied,
        };
        let mut sink = VecEventSink::default();
        assert!(matches!(
            agent.run_coding_loop("run", &mut executor, 8, &mut sink),
            Err(CoreError::Tool(_))
        ));
        assert_eq!(executor.calls, 1);
        assert!(sink
            .events
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested(_))));
    }

    struct RecordingHooks {
        events: Vec<LifecycleEvent>,
        fail_on: Option<LifecycleEvent>,
    }
    impl HookRunner for RecordingHooks {
        fn run(&mut self, event: LifecycleEvent) -> Result<String, String> {
            self.events.push(event);
            if self.fail_on == Some(event) {
                Err("blocked".into())
            } else {
                Ok("ok".into())
            }
        }
    }

    #[test]
    fn lifecycle_hooks_are_ordered_and_observable() {
        let provider = ContextRecordingProvider { seen: Vec::new() };
        let mut agent = AgentLoop::new(provider).with_hooks(Box::new(RecordingHooks {
            events: Vec::new(),
            fail_on: None,
        }));
        let mut executor = RecordingExecutor { calls: 0 };
        let mut sink = VecEventSink::default();
        agent
            .run_coding_loop("inspect", &mut executor, 1, &mut sink)
            .expect("hooked turn");
        assert!(sink.events.iter().any(|event| matches!(
            event,
            Event::HookStarted(name) if name == "SessionStart"
        )));
        assert!(sink.events.iter().any(|event| matches!(
            event,
            Event::HookStarted(name) if name == "Stop"
        )));
    }

    #[test]
    fn fail_closed_hook_stops_turn() {
        let provider = ContextRecordingProvider { seen: Vec::new() };
        let mut agent = AgentLoop::new(provider).with_hooks(Box::new(RecordingHooks {
            events: Vec::new(),
            fail_on: Some(LifecycleEvent::SessionStart),
        }));
        let mut executor = RecordingExecutor { calls: 0 };
        let mut sink = VecEventSink::default();
        assert!(matches!(
            agent.run_coding_loop("blocked", &mut executor, 1, &mut sink),
            Err(CoreError::Tool(message)) if message.contains("hook SessionStart failed")
        ));
    }

    struct OneMemory;
    impl MemoryRetriever for OneMemory {
        fn retrieve(
            &self,
            scope: &str,
            _query: &str,
            _limit: usize,
        ) -> Result<Vec<RetrievedMemory>, String> {
            assert_eq!(scope, "/workspace");
            Ok(vec![RetrievedMemory {
                id: "m1".into(),
                key: "build-command".into(),
                content: "cargo test --workspace".into(),
            }])
        }
    }

    struct RecordingCompactor {
        calls: usize,
        fail: bool,
    }
    impl Compactor for RecordingCompactor {
        fn compact(
            &mut self,
            request: CompactionRequest,
        ) -> Result<CompactionResult, CompactionError> {
            self.calls += 1;
            if self.fail {
                return Err(CompactionError::Failed("mock failure".into()));
            }
            Ok(CompactionResult {
                checkpoint: request,
                summary: "keep the verification checkpoint".into(),
                dropped_message_count: 1,
            })
        }
    }

    #[test]
    fn bounded_memory_retrieval_influences_provider_request() {
        let provider = ContextRecordingProvider { seen: Vec::new() };
        let mut agent = AgentLoop::new(provider)
            .with_memory_scope("/workspace")
            .with_memory_limit(1)
            .with_memory_retriever(Box::new(OneMemory));
        let mut executor = RecordingExecutor { calls: 0 };
        let mut sink = VecEventSink::default();
        agent
            .run_coding_loop("inspect", &mut executor, 1, &mut sink)
            .expect("memory turn");
        assert!(agent.provider.seen[0]
            .messages
            .iter()
            .any(|message| message.content.contains("cargo test --workspace")));
        assert_eq!(agent.session.messages.len(), 2);
    }

    #[test]
    fn compactor_is_applied_without_journaling_summary() {
        let provider = ContextRecordingProvider { seen: Vec::new() };
        let mut agent = AgentLoop::new(provider)
            .with_context_budget(2)
            .with_compactor(Box::new(RecordingCompactor {
                calls: 0,
                fail: false,
            }));
        agent.restore_session(
            "objective",
            vec![Message {
                role: Role::Assistant,
                content: "a long historical response that exceeds the tiny budget".into(),
            }],
        );
        let mut executor = RecordingExecutor { calls: 0 };
        let mut sink = VecEventSink::default();
        agent
            .run_coding_loop("next", &mut executor, 1, &mut sink)
            .expect("compacted turn");
        assert!(agent.provider.seen[0]
            .messages
            .iter()
            .any(|message| message.content.contains("compaction checkpoint")));
        assert!(!agent
            .session
            .messages
            .iter()
            .any(|message| message.content.contains("keep the verification checkpoint")));
        assert!(sink.events.iter().any(|event| matches!(
            event,
            Event::ContextCompacted(message) if message.contains("compaction applied")
        )));
    }

    #[test]
    fn compaction_failure_is_safe_and_bounded() {
        let provider = ContextRecordingProvider { seen: Vec::new() };
        let mut agent = AgentLoop::new(provider)
            .with_context_budget(2)
            .with_compactor(Box::new(RecordingCompactor {
                calls: 0,
                fail: true,
            }));
        let mut executor = RecordingExecutor { calls: 0 };
        let mut sink = VecEventSink::default();
        agent
            .run_coding_loop(
                "a request that is larger than budget",
                &mut executor,
                1,
                &mut sink,
            )
            .expect("safe fallback");
        assert!(sink
            .events
            .iter()
            .any(|event| matches!(event, Event::Error(message) if message.contains("compaction failed safely"))));
    }
}
