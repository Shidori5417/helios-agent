use std::cmp::Ordering;

pub const DEFAULT_TOOL_OUTPUT_BYTES: usize = 8 * 1024;

/// Per-file cap for project instruction files injected into the system prompt.
pub const PROJECT_INSTRUCTIONS_FILE_BYTES: usize = 16 * 1024;
/// Workspace-relative project instruction files, most general first.
pub const PROJECT_INSTRUCTION_FILES: &[&str] = &["AGENTS.md", ".helios/AGENTS.md"];

/// Loads bounded project instruction files (`AGENTS.md`, `.helios/AGENTS.md`)
/// from the canonical workspace root. Files that resolve outside the root
/// (e.g. through a symlink), are not regular files, or are not UTF-8 are
/// skipped. Oversized files are truncated on a char boundary with a marker.
pub fn load_project_instructions(root: &std::path::Path) -> Option<String> {
    let root = root.canonicalize().ok()?;
    let mut sections = Vec::new();
    for name in PROJECT_INSTRUCTION_FILES {
        let Ok(path) = root.join(name).canonicalize() else {
            continue;
        };
        if !path.starts_with(&root) || !path.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let mut end = text.len().min(PROJECT_INSTRUCTIONS_FILE_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let truncated = if end < text.len() {
            "\n[truncated]"
        } else {
            ""
        };
        sections.push(format!(
            "<project-instructions source=\"{name}\">\n{}{truncated}\n</project-instructions>",
            &text[..end]
        ));
    }
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum SegmentKind {
    System,
    ToolDefinitions,
    ProjectRules,
    Skill,
    Memory,
    Conversation,
    Source,
    Diff,
    ToolResult,
    UserRequest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Priority {
    Disposable,
    Normal,
    High,
    Required,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Stability {
    Volatile,
    SessionStable,
    Static,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TruncationPolicy {
    None,
    HeadTail,
    SemanticChunks,
    Summarize,
    SpillReference,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextSegment {
    pub kind: SegmentKind,
    pub priority: Priority,
    pub stability: Stability,
    pub estimated_tokens: u64,
    pub truncate_policy: TruncationPolicy,
    pub source_hash: String,
    pub content: String,
}

impl ContextSegment {
    pub fn new(
        kind: SegmentKind,
        priority: Priority,
        stability: Stability,
        content: impl Into<String>,
    ) -> Self {
        let content = content.into();
        let source_hash = fingerprint(content.as_bytes());
        Self {
            kind,
            priority,
            stability,
            estimated_tokens: estimate_tokens(&content),
            truncate_policy: TruncationPolicy::HeadTail,
            source_hash,
            content,
        }
    }
    pub fn with_estimate(mut self, estimated_tokens: u64) -> Self {
        self.estimated_tokens = estimated_tokens;
        self
    }
    pub fn with_policy(mut self, truncate_policy: TruncationPolicy) -> Self {
        self.truncate_policy = truncate_policy;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedSegment {
    pub segment: ContextSegment,
    pub selected_tokens: u64,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextPlan {
    pub segments: Vec<PlannedSegment>,
    pub dropped: Vec<ContextSegment>,
    pub budget: u64,
    pub estimated_tokens: u64,
    pub stable_prefix_fingerprint: String,
}

impl ContextPlan {
    pub fn build(mut input: Vec<ContextSegment>, budget: u64) -> Self {
        input.sort_by(canonical_order);
        let mut remaining = budget;
        let mut selected = Vec::new();
        let mut dropped = Vec::new();
        for segment in input {
            let requested = segment.estimated_tokens;
            let must_keep = segment.priority == Priority::Required;
            if requested <= remaining {
                remaining -= requested;
                selected.push(PlannedSegment {
                    segment,
                    selected_tokens: requested,
                    truncated: false,
                });
            } else if must_keep || segment.truncate_policy != TruncationPolicy::None {
                let selected_tokens = requested.min(remaining);
                remaining = remaining.saturating_sub(selected_tokens);
                let mut segment = segment;
                segment.content =
                    truncate(&segment.content, selected_tokens, segment.truncate_policy);
                segment.estimated_tokens = selected_tokens;
                selected.push(PlannedSegment {
                    segment,
                    selected_tokens,
                    truncated: selected_tokens < requested,
                });
            } else {
                dropped.push(segment);
            }
        }
        selected.sort_by(|a, b| canonical_order(&a.segment, &b.segment));
        let estimated_tokens = selected.iter().fold(0_u64, |total, item| {
            total.saturating_add(item.selected_tokens)
        });
        let stable = selected
            .iter()
            .filter(|item| item.segment.stability != Stability::Volatile)
            .map(|item| {
                format!(
                    "{:?}:{:?}:{:?}:{}:{}",
                    item.segment.kind,
                    item.segment.priority,
                    item.segment.stability,
                    item.segment.source_hash,
                    item.segment.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            segments: selected,
            dropped,
            budget,
            estimated_tokens,
            stable_prefix_fingerprint: fingerprint(stable.as_bytes()),
        }
    }
}

fn canonical_order(a: &ContextSegment, b: &ContextSegment) -> Ordering {
    a.kind
        .cmp(&b.kind)
        .then_with(|| b.priority.cmp(&a.priority))
        .then_with(|| b.stability.cmp(&a.stability))
        .then_with(|| a.source_hash.cmp(&b.source_hash))
}
fn estimate_tokens(content: &str) -> u64 {
    ((content.len() as u64).saturating_add(3)) / 4
}
fn truncate(content: &str, tokens: u64, policy: TruncationPolicy) -> String {
    let bytes = tokens.saturating_mul(4).min(content.len() as u64) as usize;
    if bytes >= content.len() || policy == TruncationPolicy::None {
        return content.to_owned();
    }
    let head = bytes / 2;
    let tail = bytes.saturating_sub(head);
    format!(
        "{}\n...[truncated]...\n{}",
        &content[..head],
        &content[content.len().saturating_sub(tail)..]
    )
}

pub fn fingerprint(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProviderUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NormalizedUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

impl ProviderUsage {
    pub fn normalize(self) -> NormalizedUsage {
        let total = self
            .input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
            .saturating_add(self.reasoning_tokens);
        NormalizedUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            reasoning_tokens: self.reasoning_tokens,
            total_tokens: total,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionRequest {
    pub objective: String,
    pub accepted_constraints: Vec<String>,
    pub decisions: Vec<String>,
    pub unresolved_failures: Vec<String>,
    pub verification_results: Vec<String>,
    pub relevant_memory_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionResult {
    pub checkpoint: CompactionRequest,
    pub summary: String,
    pub dropped_message_count: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    #[error("compaction failed: {0}")]
    Failed(String),
}

pub trait Compactor {
    fn compact(&mut self, request: CompactionRequest) -> Result<CompactionResult, CompactionError>;
}

#[derive(Default)]
pub struct LocalCompactor;
impl Compactor for LocalCompactor {
    fn compact(&mut self, request: CompactionRequest) -> Result<CompactionResult, CompactionError> {
        let summary = format!(
            "objective={} decisions={} failures={}",
            request.objective,
            request.decisions.len(),
            request.unresolved_failures.len()
        );
        Ok(CompactionResult {
            checkpoint: request,
            summary,
            dropped_message_count: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_instructions_are_bounded_and_contained() {
        let root = std::env::temp_dir().join(format!("agent-context-instr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".helios")).expect("fixture");
        assert_eq!(load_project_instructions(&root), None);
        std::fs::write(root.join("AGENTS.md"), "Run cargo test.\n").expect("agents");
        std::fs::write(
            root.join(".helios/AGENTS.md"),
            "é".repeat(PROJECT_INSTRUCTIONS_FILE_BYTES),
        )
        .expect("local");
        let loaded = load_project_instructions(&root).expect("instructions");
        assert!(loaded.starts_with("<project-instructions source=\"AGENTS.md\">\nRun cargo test."));
        assert!(loaded.contains("source=\".helios/AGENTS.md\""));
        assert!(loaded.contains("[truncated]"));
        assert!(loaded.len() < 2 * PROJECT_INSTRUCTIONS_FILE_BYTES);
        #[cfg(unix)]
        {
            let outside = root.with_extension("outside");
            std::fs::write(&outside, "secret").expect("outside");
            std::fs::remove_file(root.join("AGENTS.md")).expect("remove");
            std::os::unix::fs::symlink(&outside, root.join("AGENTS.md")).expect("symlink");
            let loaded = load_project_instructions(&root).expect("local only");
            assert!(!loaded.contains("secret"));
            let _ = std::fs::remove_file(outside);
        }
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn plan_and_fingerprint_are_deterministic() {
        let a = ContextSegment::new(
            SegmentKind::System,
            Priority::Required,
            Stability::Static,
            "stable",
        );
        let b = ContextSegment::new(
            SegmentKind::Source,
            Priority::Normal,
            Stability::Volatile,
            "source",
        );
        assert_eq!(
            ContextPlan::build(vec![b.clone(), a.clone()], 100),
            ContextPlan::build(vec![a, b], 100)
        );
        assert_eq!(fingerprint(b"stable"), fingerprint(b"stable"));
    }
    #[test]
    fn budget_drops_non_truncatable_and_truncates_required() {
        let required = ContextSegment::new(
            SegmentKind::System,
            Priority::Required,
            Stability::Static,
            "1234567890",
        )
        .with_estimate(10);
        let disposable = ContextSegment::new(
            SegmentKind::Source,
            Priority::Disposable,
            Stability::Volatile,
            "drop",
        )
        .with_estimate(10)
        .with_policy(TruncationPolicy::None);
        let plan = ContextPlan::build(vec![disposable, required], 4);
        assert!(plan.estimated_tokens <= 4);
        assert_eq!(plan.segments.len(), 1);
        assert!(plan.segments[0].truncated);
    }
    #[test]
    fn usage_total_saturates_instead_of_overflowing() {
        let usage = ProviderUsage {
            input_tokens: u64::MAX,
            output_tokens: 1,
            ..ProviderUsage::default()
        }
        .normalize();
        assert_eq!(usage.total_tokens, u64::MAX);
    }
    #[test]
    fn compaction_preserves_checkpoint_boundary() {
        let request = CompactionRequest {
            objective: "fix".into(),
            accepted_constraints: vec!["safe".into()],
            decisions: vec!["minimal".into()],
            unresolved_failures: vec![],
            verification_results: vec!["pass".into()],
            relevant_memory_ids: vec![],
        };
        let result = LocalCompactor.compact(request.clone()).expect("compact");
        assert_eq!(result.checkpoint, request);
        assert!(result.summary.contains("objective=fix"));
    }
}
