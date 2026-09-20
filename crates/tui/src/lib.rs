use agent_protocol::Event;
use crossterm::{
    cursor::{Hide, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Text,
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};
use std::{
    io::{self, IsTerminal},
    sync::mpsc::{Receiver, SyncSender, TryRecvError},
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiModel {
    pub status: String,
    pub transcript: Vec<TranscriptItem>,
    pub tool: Option<String>,
    pub permission: Option<String>,
    pub verification: Option<String>,
    pub context: Option<String>,
    pub scroll: usize,
    pub viewport_height: usize,
    pub scroll_step: usize,
    pub composer: Composer,
    pub history: Vec<String>,
    pub history_index: Option<usize>,
    pub active_turn: bool,
    pub workspace: Option<String>,
    pub branch: Option<String>,
    pub dirty_files: Option<usize>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub context_percent: Option<u8>,
    pub permission_mode: Option<String>,
    pub changes_delta: Option<i64>,
    pub background_jobs: Option<usize>,
    pub palette: Vec<PaletteCommand>,
    pub models: Vec<ModelOption>,
    pub sessions: Vec<SessionOption>,
    pub overlay: Option<OverlayState>,
    pub theme: TuiTheme,
    pub diff: Option<String>,
    pub permission_request: Option<PermissionPrompt>,
    pub tool_cards: Vec<ToolCard>,
}

impl Default for TuiModel {
    fn default() -> Self {
        Self {
            status: String::new(),
            transcript: Vec::new(),
            tool: None,
            permission: None,
            verification: None,
            context: None,
            scroll: 0,
            viewport_height: 0,
            scroll_step: 1,
            composer: Composer::default(),
            history: Vec::new(),
            history_index: None,
            active_turn: false,
            workspace: None,
            branch: None,
            dirty_files: None,
            model: None,
            effort: None,
            context_percent: None,
            permission_mode: None,
            changes_delta: None,
            background_jobs: None,
            palette: Vec::new(),
            models: Vec::new(),
            sessions: Vec::new(),
            overlay: None,
            theme: TuiTheme::System,
            diff: None,
            permission_request: None,
            tool_cards: Vec::new(),
        }
    }
}

pub const MAX_TRANSCRIPT_ITEMS: usize = 512;
pub const MAX_TRANSCRIPT_BYTES: usize = 512 * 1024;
pub const MAX_STREAM_ITEM_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteCommand {
    pub name: String,
    pub description: String,
    pub category: String,
    pub arguments: String,
    pub local: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelOption {
    pub provider: String,
    pub model: String,
    pub configured: bool,
    pub current: bool,
    pub capabilities: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionOption {
    pub id: i64,
    pub title: String,
    pub turn_count: i64,
    pub last_activity: Option<i64>,
    pub model: Option<String>,
    pub git_branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverlayKind {
    Palette,
    Help,
    Model,
    Sessions,
    Context,
    Status,
    Permission,
    Diff,
    Theme,
    Keybindings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayState {
    pub kind: OverlayKind,
    pub query: String,
    pub selected: usize,
    pub commands: Vec<PaletteCommand>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverlayAction {
    None,
    Close,
    Open(OverlayKind),
    Execute(String),
    Fill(String),
    Permission(PermissionDecision),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    AllowOnce,
    AllowSession,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionPrompt {
    pub id: u64,
    pub action: String,
    pub risk: String,
    pub class: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCard {
    pub name: String,
    pub status: String,
    pub summary: String,
    pub output: String,
    pub expanded: bool,
}

fn parse_permission_prompt(value: &str) -> PermissionPrompt {
    let bounded = value.chars().take(512).collect::<String>();
    let mut id = 0;
    let mut class = "unknown";
    let mut action = bounded.as_str();
    let mut risk = "";
    for part in bounded.split(';') {
        if let Some((key, value)) = part.split_once('=') {
            match key {
                "id" => id = value.parse().unwrap_or(0),
                "class" => class = value,
                "risk" => risk = value,
                "action" => action = value,
                _ => {}
            }
        }
    }
    if class == "unknown" {
        let (parsed_class, parsed_action) = bounded
            .split_once(':')
            .map(|(class, action)| (class.trim(), action.trim()))
            .unwrap_or((class, action));
        class = parsed_class;
        action = parsed_action;
    }
    let risk = if !risk.is_empty() {
        risk
    } else if class.eq_ignore_ascii_case("read-only") || class.eq_ignore_ascii_case("readonly") {
        "low"
    } else if class.eq_ignore_ascii_case("workspace-write")
        || class.eq_ignore_ascii_case("workspacewrite")
    {
        "medium"
    } else {
        "high"
    };
    PermissionPrompt {
        id,
        action: action.to_owned(),
        risk: risk.into(),
        class: class.to_owned(),
    }
}

fn bounded_diff_text(diff: &str, query: &str) -> String {
    let mut lines = diff.lines().map(str::to_owned).collect::<Vec<_>>();
    if !query.is_empty() {
        let needle = query.to_ascii_lowercase();
        lines.retain(|line| line.to_ascii_lowercase().contains(&needle));
    }
    let mut output = String::new();
    for line in lines.into_iter().take(2_000) {
        let marker = if line.starts_with('+') && !line.starts_with("+++") {
            "+"
        } else if line.starts_with('-') && !line.starts_with("---") {
            "-"
        } else {
            " "
        };
        output.push_str(marker);
        output.push_str(line.get(1..).unwrap_or(&line));
        output.push('\n');
    }
    if output.is_empty() {
        "(no diff lines)".into()
    } else {
        output.chars().take(128 * 1024).collect()
    }
}

/// Safe, dependency-free markdown presentation for the terminal. It intentionally
/// keeps URLs visible; OSC8 is handled only by a capability-aware renderer.
pub fn markdown_plain(input: &str) -> String {
    input
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                "▌".to_owned()
            } else if trimmed.starts_with('#') {
                trimmed.trim_start_matches('#').trim().to_uppercase()
            } else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
                format!("• {}", trimmed[2..].replace('`', ""))
            } else {
                line.replace("**", "").replace('`', "")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuiTheme {
    Dark,
    Light,
    System,
    Monochrome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptItem {
    User(String),
    Assistant(String),
    Activity(String),
    Tool { name: String, output: String },
    FileChange(String),
    Error(String),
    Notification(String),
}

impl TranscriptItem {
    fn render(&self) -> String {
        match self {
            Self::User(text) => format!("> {text}"),
            Self::Assistant(text) => format!("assistant: {text}"),
            Self::Activity(text) => format!("· {text}"),
            Self::Tool { name, output } => format!("tool {name}: {output}"),
            Self::FileChange(path) => format!("change: {path}"),
            Self::Error(error) => format!("error: {error}"),
            Self::Notification(text) => text.clone(),
        }
    }

    fn size(&self) -> usize {
        self.render().len()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Composer {
    pub text: String,
    pub cursor: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComposerAction {
    None,
    Submit(String),
    Cancel,
    Exit,
    Redraw,
}

impl Composer {
    const MAX_INPUT_BYTES: usize = 64 * 1024;

    pub fn handle_key(&mut self, key: KeyEvent) -> ComposerAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') => ComposerAction::Cancel,
                KeyCode::Char('l') => ComposerAction::Redraw,
                KeyCode::Char('d') if self.text.is_empty() => ComposerAction::Exit,
                _ => ComposerAction::None,
            };
        }
        if key.code == KeyCode::Esc {
            return ComposerAction::Cancel;
        }
        match key.code {
            KeyCode::Enter => {
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
                {
                    self.insert('\n');
                    ComposerAction::None
                } else if self.text.trim().is_empty() {
                    ComposerAction::None
                } else {
                    let value = std::mem::take(&mut self.text);
                    self.cursor = 0;
                    ComposerAction::Submit(value)
                }
            }
            KeyCode::Char(character) => {
                self.insert(character);
                ComposerAction::None
            }
            KeyCode::Backspace => {
                self.backspace();
                ComposerAction::None
            }
            KeyCode::Delete => {
                self.delete();
                ComposerAction::None
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                ComposerAction::None
            }
            KeyCode::Right => {
                if let Some((offset, _)) = self.text[self.cursor..].char_indices().nth(1) {
                    self.cursor += offset;
                } else {
                    self.cursor = self.text.len();
                }
                ComposerAction::None
            }
            KeyCode::Home => {
                self.cursor = 0;
                ComposerAction::None
            }
            KeyCode::End => {
                self.cursor = self.text.len();
                ComposerAction::None
            }
            _ => ComposerAction::None,
        }
    }

    pub fn paste(&mut self, value: &str) {
        for character in value.chars().take(Self::MAX_INPUT_BYTES) {
            self.insert(character);
        }
    }

    fn insert(&mut self, character: char) {
        if self.text.len() >= Self::MAX_INPUT_BYTES {
            return;
        }
        self.text.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.text.drain(previous..self.cursor);
        self.cursor = previous;
    }

    fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let end = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.text.len());
        self.text.drain(self.cursor..end);
    }
}

impl TuiModel {
    pub fn set_diff(&mut self, diff: impl Into<String>) {
        self.diff = Some(diff.into().chars().take(128 * 1024).collect());
    }

    pub fn open_permission(&mut self, prompt: PermissionPrompt) {
        self.permission_request = Some(prompt);
        self.overlay = Some(OverlayState {
            kind: OverlayKind::Permission,
            query: String::new(),
            selected: 0,
            commands: Vec::new(),
        });
    }

    pub fn set_theme(&mut self, theme: TuiTheme) {
        self.theme = theme;
    }

    pub fn permission_decision(&self) -> Option<&PermissionPrompt> {
        self.permission_request.as_ref()
    }
    pub fn set_palette(&mut self, commands: Vec<PaletteCommand>) {
        self.palette = commands.clone();
        self.overlay = Some(OverlayState {
            kind: OverlayKind::Palette,
            query: String::new(),
            selected: 0,
            commands,
        });
    }

    pub fn open_overlay(&mut self, kind: OverlayKind) {
        let commands = if kind == OverlayKind::Model {
            self.models
                .iter()
                .map(|model| PaletteCommand {
                    name: format!("{}/{}", model.provider, model.model),
                    description: format!(
                        "{}{} · {}",
                        if model.current {
                            "current · "
                        } else if model.configured {
                            "configured · "
                        } else {
                            ""
                        },
                        model.provider,
                        model.capabilities
                    ),
                    category: "Model".into(),
                    arguments: String::new(),
                    local: true,
                })
                .collect()
        } else if kind == OverlayKind::Sessions {
            self.sessions
                .iter()
                .map(|session| PaletteCommand {
                    name: session.id.to_string(),
                    description: format!(
                        "{} · turns={}{}",
                        session.title,
                        session.turn_count,
                        session
                            .model
                            .as_ref()
                            .map(|model| format!(" · model={model}"))
                            .unwrap_or_default()
                    ),
                    category: "Session".into(),
                    arguments: String::new(),
                    local: true,
                })
                .collect()
        } else {
            self.palette.clone()
        };
        self.overlay = Some(OverlayState {
            kind,
            query: String::new(),
            selected: 0,
            commands,
        });
    }

    pub fn filtered_palette(&self) -> Vec<&PaletteCommand> {
        let Some(overlay) = &self.overlay else {
            return Vec::new();
        };
        let query = overlay.query.to_ascii_lowercase();
        overlay
            .commands
            .iter()
            .filter(|command| {
                query.is_empty()
                    || command.name.to_ascii_lowercase().contains(&query)
                    || command.description.to_ascii_lowercase().contains(&query)
            })
            .collect()
    }

    pub fn overlay_text(&self) -> String {
        let Some(overlay) = &self.overlay else {
            return String::new();
        };
        let title = match overlay.kind {
            OverlayKind::Palette => "Commands",
            OverlayKind::Help => "Help",
            OverlayKind::Model => "Models",
            OverlayKind::Sessions => "Sessions",
            OverlayKind::Context => "Context",
            OverlayKind::Status => "Status",
            OverlayKind::Permission => "Permission request",
            OverlayKind::Diff => "Diff (read-only)",
            OverlayKind::Theme => "Theme",
            OverlayKind::Keybindings => "Keybindings",
        };
        if overlay.kind == OverlayKind::Permission {
            if let Some(request) = &self.permission_request {
                return format!(
                    "Permission request\n{}\nrisk: {} · class: {}\n[A] allow once  [S] allow session  [D]/Esc deny",
                    request.action, request.risk, request.class
                );
            }
        }
        if overlay.kind == OverlayKind::Diff {
            return bounded_diff_text(self.diff.as_deref().unwrap_or(""), overlay.query.as_str());
        }
        if overlay.kind == OverlayKind::Theme {
            return format!(
                "Theme (current: {:?})\n> dark\n  light\n  system\n  monochrome\n\nEnter selects · Esc closes",
                self.theme
            );
        }
        if overlay.kind == OverlayKind::Keybindings {
            return "Keybindings\nEnter submit · Alt+Enter newline · Ctrl-C cancel\nEsc close · PageUp/PageDown scroll · Ctrl-L redraw".into();
        }
        let mut lines = vec![format!("{title}  /{}  (Esc close)", overlay.query)];
        let commands = self.filtered_palette();
        for (index, command) in commands.iter().enumerate() {
            let marker = if index == overlay.selected { ">" } else { " " };
            lines.push(format!(
                "{marker} /{} {} [{}] — {}",
                command.name, command.arguments, command.category, command.description
            ));
        }
        if commands.is_empty() {
            lines.push("No matching local command".into());
        }
        lines.join("\n")
    }

    pub fn handle_overlay_key(&mut self, key: KeyEvent) -> OverlayAction {
        let Some(overlay) = &mut self.overlay else {
            return OverlayAction::None;
        };
        if key.code == KeyCode::Esc {
            self.overlay = None;
            if self.permission_request.is_some() {
                self.permission_request = None;
                return OverlayAction::Permission(PermissionDecision::Deny);
            }
            return OverlayAction::Close;
        }
        if overlay.kind == OverlayKind::Permission {
            let decision = match key.code {
                KeyCode::Char('a') | KeyCode::Char('A') => Some(PermissionDecision::AllowOnce),
                KeyCode::Char('s') | KeyCode::Char('S') => Some(PermissionDecision::AllowSession),
                KeyCode::Char('d') | KeyCode::Char('D') => Some(PermissionDecision::Deny),
                _ => None,
            };
            if let Some(decision) = decision {
                self.permission_request = None;
                self.overlay = None;
                return OverlayAction::Permission(decision);
            }
            return OverlayAction::None;
        }
        if overlay.kind == OverlayKind::Theme {
            let theme = match key.code {
                KeyCode::Char('1') => Some(TuiTheme::Dark),
                KeyCode::Char('2') => Some(TuiTheme::Light),
                KeyCode::Char('3') => Some(TuiTheme::System),
                KeyCode::Char('4') => Some(TuiTheme::Monochrome),
                _ => None,
            };
            if let Some(theme) = theme {
                self.theme = theme;
                self.overlay = None;
                return OverlayAction::Close;
            }
        }
        match key.code {
            KeyCode::Up => {
                overlay.selected = overlay.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Tab => {
                let query = overlay.query.to_ascii_lowercase();
                let count = overlay
                    .commands
                    .iter()
                    .filter(|command| {
                        query.is_empty()
                            || command.name.to_ascii_lowercase().contains(&query)
                            || command.description.to_ascii_lowercase().contains(&query)
                    })
                    .count();
                if count > 0 {
                    overlay.selected = (overlay.selected + 1).min(count - 1);
                }
            }
            KeyCode::Backspace => {
                overlay.query.pop();
                overlay.selected = 0;
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                overlay.query.push(character);
                overlay.selected = 0;
            }
            KeyCode::Enter => {
                let query = overlay.query.to_ascii_lowercase();
                let selected = overlay
                    .commands
                    .iter()
                    .filter(|command| {
                        query.is_empty()
                            || command.name.to_ascii_lowercase().contains(&query)
                            || command.description.to_ascii_lowercase().contains(&query)
                    })
                    .nth(overlay.selected)
                    .cloned();
                if let Some(command) = selected {
                    if overlay.kind == OverlayKind::Palette
                        && matches!(command.name.as_str(), "model" | "sessions")
                    {
                        let target = if command.name == "model" {
                            OverlayKind::Model
                        } else {
                            OverlayKind::Sessions
                        };
                        self.overlay = None;
                        self.open_overlay(target.clone());
                        return OverlayAction::Open(target);
                    }
                    let model_overlay = overlay.kind == OverlayKind::Model;
                    let session_overlay = overlay.kind == OverlayKind::Sessions;
                    self.overlay = None;
                    return if command.local {
                        if model_overlay {
                            OverlayAction::Execute(format!("/model {}", command.name))
                        } else if session_overlay {
                            OverlayAction::Execute(format!("/session-resume {}", command.name))
                        } else {
                            OverlayAction::Execute(format!("/{}", command.name))
                        }
                    } else {
                        OverlayAction::Fill(format!("/{} ", command.name))
                    };
                }
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.scroll_step.max(1)),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(self.scroll_step.max(1)),
            _ => {}
        }
        OverlayAction::None
    }

    pub fn set_runtime_context(&mut self, context: TuiContext) {
        self.workspace = context.workspace;
        self.branch = context.branch;
        self.dirty_files = context.dirty_files;
        self.model = context.model;
        self.effort = context.effort;
        self.permission_mode = context.permission_mode;
        self.background_jobs = context.background_jobs;
        self.palette = context.palette;
        self.models = context.models;
        self.sessions = context.sessions;
        self.diff = context.diff.clone();
        if let Some(diff) = context.diff {
            self.set_diff(diff);
        }
    }

    pub fn push_item(&mut self, item: TranscriptItem) {
        self.transcript.push(item);
        while self.transcript.len() > MAX_TRANSCRIPT_ITEMS
            || self
                .transcript
                .iter()
                .map(TranscriptItem::size)
                .sum::<usize>()
                > MAX_TRANSCRIPT_BYTES
        {
            if self.transcript.len() <= 1 {
                break;
            }
            self.transcript.remove(0);
        }
    }

    fn append_assistant_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(TranscriptItem::Assistant(text)) = self.transcript.last_mut() {
            let remaining = MAX_STREAM_ITEM_BYTES.saturating_sub(text.len());
            text.extend(delta.chars().take(remaining));
            return;
        }
        self.push_item(TranscriptItem::Assistant(
            delta.chars().take(MAX_STREAM_ITEM_BYTES).collect(),
        ));
    }

    fn append_tool_delta(&mut self, delta: &str) {
        let name = self.tool.clone().unwrap_or_else(|| "tool".into());
        if let Some(TranscriptItem::Tool {
            name: current,
            output,
        }) = self.transcript.last_mut()
        {
            if *current == name {
                let remaining = MAX_STREAM_ITEM_BYTES.saturating_sub(output.len());
                output.extend(delta.chars().take(remaining));
                return;
            }
        }
        self.push_item(TranscriptItem::Tool {
            name,
            output: delta.chars().take(MAX_STREAM_ITEM_BYTES).collect(),
        });
    }

    pub fn reduce(&mut self, event: &Event) {
        match event {
            Event::TurnStarted => {
                self.status = "turn started".into();
                self.active_turn = true;
            }
            Event::ModelDelta(text) => self.append_assistant_delta(text),
            Event::ReasoningSummaryDelta(text) => {
                self.push_item(TranscriptItem::Activity(format!("reasoning: {text}")))
            }
            Event::ToolRequested(name) | Event::ToolStarted(name) => {
                self.tool = Some(name.clone());
                self.tool_cards.push(ToolCard {
                    name: name.clone(),
                    status: "running".into(),
                    summary: "tool execution".into(),
                    output: String::new(),
                    expanded: false,
                });
                if self.tool_cards.len() > 64 {
                    self.tool_cards.remove(0);
                }
            }
            Event::ToolOutputDelta(text) => {
                self.append_tool_delta(text);
                if let Some(card) = self.tool_cards.last_mut() {
                    let remaining = MAX_STREAM_ITEM_BYTES.saturating_sub(card.output.len());
                    card.output.extend(text.chars().take(remaining));
                    card.summary = card.output.chars().take(120).collect();
                }
            }
            Event::ToolCompleted(status) => {
                self.status = format!("tool: {status}");
                if let Some(card) = self.tool_cards.last_mut() {
                    card.status = status.chars().take(160).collect();
                }
                self.tool = None;
            }
            Event::PatchProduced(path) => self.push_item(TranscriptItem::FileChange(path.clone())),
            Event::VerificationStarted(command) => {
                self.verification = Some(format!("started: {command}"))
            }
            Event::VerificationResult(result) => {
                self.verification = Some(format!("result: {result}"))
            }
            Event::PermissionRequested(request) => {
                self.permission = Some(request.clone());
                self.open_permission(parse_permission_prompt(request));
            }
            Event::ContextCompacted(summary) => self.context = Some(summary.clone()),
            Event::UsageUpdated(usage) => {
                self.push_item(TranscriptItem::Activity(format!("usage: {usage}")))
            }
            Event::HookStarted(name) => {
                self.push_item(TranscriptItem::Activity(format!("hook started: {name}")))
            }
            Event::HookResult(result) => {
                self.push_item(TranscriptItem::Activity(format!("hook: {result}")))
            }
            Event::McpToolDiscovered(name) => {
                self.push_item(TranscriptItem::Activity(format!("MCP tool: {name}")))
            }
            Event::McpToolResult(result) => {
                self.push_item(TranscriptItem::Activity(format!("MCP result: {result}")))
            }
            Event::TurnCompleted => {
                self.status = "turn completed".into();
                self.active_turn = false;
            }
            Event::Error(error) => {
                self.status = format!("error: {error}");
                self.active_turn = false;
                self.push_item(TranscriptItem::Error(error.clone()));
            }
        }
    }
    pub fn as_text(&self) -> String {
        let mut lines = vec![format!("status: {}", self.status)];
        lines.extend(self.transcript.iter().map(|item| match item {
            TranscriptItem::Assistant(text) => format!("assistant: {}", markdown_plain(text)),
            _ => item.render(),
        }));
        if let Some(tool) = &self.tool {
            lines.push(format!("tool: {tool}"));
        }
        if let Some(permission) = &self.permission {
            lines.push(format!("permission: {permission}"));
        }
        if let Some(verification) = &self.verification {
            lines.push(format!("verification: {verification}"));
        }
        if let Some(context) = &self.context {
            lines.push(format!("context: {context}"));
        }
        if let Some(prompt) = &self.permission_request {
            lines.push(format!(
                "permission pending: {} ({})",
                prompt.action, prompt.risk
            ));
        }
        if let Some(card) = self.tool_cards.last() {
            lines.push(format!("tool card: {} [{}]", card.name, card.status));
        }
        let start = self.scroll.min(lines.len().saturating_sub(1));
        let limit = if self.viewport_height == 0 {
            usize::MAX
        } else {
            self.viewport_height
        };
        lines[start..]
            .iter()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn reduce_input(&mut self, input: &InputEvent) {
        match input {
            InputEvent::Resize { height, .. } => self.viewport_height = (*height).max(1) as usize,
            InputEvent::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(self.scroll_step.max(1))
            }
            InputEvent::ScrollDown => {
                self.scroll = self.scroll.saturating_add(self.scroll_step.max(1))
            }
            InputEvent::None | InputEvent::Quit => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TuiColor {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TuiSettings {
    pub tick_ms: u64,
    pub channel_capacity: usize,
    pub scroll_step: usize,
    pub color: TuiColor,
    pub alternate_screen: bool,
    pub theme: TuiTheme,
}

impl Default for TuiSettings {
    fn default() -> Self {
        Self {
            tick_ms: 50,
            channel_capacity: 128,
            scroll_step: 1,
            color: TuiColor::Auto,
            alternate_screen: true,
            theme: TuiTheme::System,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputEvent {
    None,
    Quit,
    Resize { width: u16, height: u16 },
    ScrollUp,
    ScrollDown,
}

pub fn render_event(event: &Event) -> String {
    let mut model = TuiModel::default();
    model.reduce(event);
    model.as_text()
}

pub fn bounded_event_channel(capacity: usize) -> (SyncSender<Event>, Receiver<Event>) {
    std::sync::mpsc::sync_channel(capacity.max(1))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TuiContext {
    pub workspace: Option<String>,
    pub branch: Option<String>,
    pub dirty_files: Option<usize>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub background_jobs: Option<usize>,
    pub palette: Vec<PaletteCommand>,
    pub models: Vec<ModelOption>,
    pub sessions: Vec<SessionOption>,
    pub diff: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutMode {
    Large,
    Normal,
    Narrow,
    TooSmall,
}

pub fn layout_mode(width: u16, height: u16) -> LayoutMode {
    if width < 40 || height < 8 {
        LayoutMode::TooSmall
    } else if width < 72 {
        LayoutMode::Narrow
    } else if width >= 120 || height >= 36 {
        LayoutMode::Large
    } else {
        LayoutMode::Normal
    }
}

fn color_enabled(settings: TuiSettings) -> bool {
    settings.color == TuiColor::Always
        || (settings.color == TuiColor::Auto && std::env::var_os("NO_COLOR").is_none())
}

fn header_text(model: &TuiModel, mode: LayoutMode) -> String {
    let mut parts = vec!["Helios".to_owned()];
    if let Some(workspace) = &model.workspace {
        parts.push(workspace.clone());
    }
    if let Some(branch) = &model.branch {
        let dirty = model
            .dirty_files
            .map(|count| format!(" +{count}"))
            .unwrap_or_default();
        parts.push(format!("{branch}{dirty}"));
    }
    if let Some(model_name) = &model.model {
        parts.push(format!("model:{model_name}"));
    }
    if let Some(percent) = model.context_percent {
        parts.push(format!("ctx:{percent}%"));
    }
    if let Some(permission) = &model.permission_mode {
        parts.push(format!("perm:{permission}"));
    }
    let mut text = parts.join(" · ");
    if mode == LayoutMode::Narrow {
        text = text.chars().take(70).collect();
    }
    text
}

fn composer_text(model: &TuiModel, width: u16, height: u16) -> Text<'static> {
    let mut text = model.composer.text.clone();
    let cursor = model.composer.cursor.min(text.len());
    text.insert(cursor, '▌');
    let max_bytes = usize::from(width.max(1)) * usize::from(height.max(1)) * 4;
    text.truncate(text.len().min(max_bytes));
    Text::from(text)
}

fn render_frame(frame: &mut ratatui::Frame<'_>, model: &TuiModel, settings: TuiSettings) {
    let area = frame.area();
    let mode = layout_mode(area.width, area.height);
    if mode == LayoutMode::TooSmall {
        frame.render_widget(
            Paragraph::new("Terminal too small — resize to continue")
                .block(Block::default().borders(Borders::ALL).title("Helios")),
            area,
        );
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);
    let border_style = if color_enabled(settings) && model.theme != TuiTheme::Monochrome {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    frame.render_widget(Paragraph::new(header_text(model, mode)), chunks[0]);
    let body = model.as_text();
    frame.render_widget(
        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(composer_text(
            model,
            chunks[2].width.saturating_sub(2),
            chunks[2].height.saturating_sub(2),
        ))
        .block(Block::default().borders(Borders::ALL).title("Composer"))
        .wrap(Wrap { trim: false }),
        chunks[2],
    );
    let mut footer = model.status.clone();
    if let Some(delta) = model.changes_delta {
        footer.push_str(&format!(" · changes:{delta:+}"));
    }
    if let Some(jobs) = model.background_jobs {
        footer.push_str(&format!(" · jobs:{jobs}"));
    }
    frame.render_widget(Paragraph::new(footer), chunks[3]);
    if model.overlay.is_some() {
        let width = area.width.saturating_sub(6).min(100);
        let height = area.height.saturating_sub(6).min(18);
        let overlay_area = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        frame.render_widget(
            Paragraph::new(model.overlay_text())
                .block(Block::default().borders(Borders::ALL).title("Overlay"))
                .wrap(Wrap { trim: false }),
            overlay_area,
        );
    }
}

struct TerminalGuard {
    alternate_screen: bool,
}
impl TerminalGuard {
    fn enter(alternate_screen: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        if alternate_screen {
            execute!(io::stdout(), EnterAlternateScreen)?;
        }
        execute!(io::stdout(), EnableBracketedPaste, Hide)?;
        Ok(Self { alternate_screen })
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, Show);
        if self.alternate_screen {
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
        }
    }
}

pub fn run(events: impl IntoIterator<Item = Event>) -> io::Result<()> {
    let events = events.into_iter().collect::<Vec<_>>();
    let (sender, receiver) = std::sync::mpsc::sync_channel(events.len().max(1));
    for event in events {
        if sender.send(event).is_err() {
            break;
        }
    }
    drop(sender);
    run_channel(receiver, agent_terminal::CancellationToken::default())
}

pub fn run_channel(
    receiver: Receiver<Event>,
    cancellation: agent_terminal::CancellationToken,
) -> io::Result<()> {
    run_channel_with_settings(receiver, cancellation, TuiSettings::default())
}

pub fn run_channel_with_settings(
    receiver: Receiver<Event>,
    cancellation: agent_terminal::CancellationToken,
    settings: TuiSettings,
) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "TUI requires an interactive TTY",
        ));
    }
    let _guard = TerminalGuard::enter(settings.alternate_screen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut model = TuiModel {
        viewport_height: 24,
        scroll_step: settings.scroll_step.max(1),
        theme: settings.theme,
        ..TuiModel::default()
    };
    let tick = Duration::from_millis(settings.tick_ms.clamp(10, 1000));
    let mut disconnected = false;
    while !disconnected && !cancellation.is_cancelled() {
        loop {
            match receiver.try_recv() {
                Ok(event) => model.reduce(&event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(frame.area());
            let mut block = Block::default().borders(Borders::ALL).title("Helios");
            if settings.color == TuiColor::Always {
                block = block.border_style(Style::default().fg(Color::Cyan));
            }
            frame.render_widget(Paragraph::new(model.as_text()).block(block), chunks[0]);
            frame.render_widget(
                Paragraph::new("q/Esc/Ctrl-C: cancel  ↑/↓: scroll"),
                chunks[1],
            );
        })?;
        if crossterm::event::poll(tick)? {
            let input = match crossterm::event::read()? {
                crossterm::event::Event::Resize(width, height) => {
                    InputEvent::Resize { width, height }
                }
                crossterm::event::Event::Key(key) => match key.code {
                    crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc => {
                        InputEvent::Quit
                    }
                    crossterm::event::KeyCode::Up => InputEvent::ScrollUp,
                    crossterm::event::KeyCode::Down => InputEvent::ScrollDown,
                    crossterm::event::KeyCode::Char('c')
                        if key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        InputEvent::Quit
                    }
                    _ => InputEvent::None,
                },
                _ => InputEvent::None,
            };
            model.reduce_input(&input);
            if input == InputEvent::Quit {
                cancellation.cancel();
            }
        }
    }
    Ok(())
}

pub fn run_interactive<F>(
    receiver: Receiver<Event>,
    sender: SyncSender<Event>,
    cancellation: agent_terminal::CancellationToken,
    settings: TuiSettings,
    submit: F,
) -> io::Result<()>
where
    F: FnMut(String, SyncSender<Event>, agent_terminal::CancellationToken) -> bool,
{
    run_interactive_with_context(
        receiver,
        sender,
        cancellation,
        settings,
        TuiContext::default(),
        submit,
    )
}

pub fn run_interactive_with_context<F>(
    receiver: Receiver<Event>,
    sender: SyncSender<Event>,
    cancellation: agent_terminal::CancellationToken,
    settings: TuiSettings,
    context: TuiContext,
    submit: F,
) -> io::Result<()>
where
    F: FnMut(String, SyncSender<Event>, agent_terminal::CancellationToken) -> bool,
{
    run_interactive_with_context_and_approval(
        receiver,
        sender,
        cancellation,
        settings,
        context,
        submit,
        |_id, _decision| {},
    )
}

pub fn run_interactive_with_context_and_approval<F, D>(
    receiver: Receiver<Event>,
    sender: SyncSender<Event>,
    cancellation: agent_terminal::CancellationToken,
    settings: TuiSettings,
    context: TuiContext,
    mut submit: F,
    mut decide: D,
) -> io::Result<()>
where
    F: FnMut(String, SyncSender<Event>, agent_terminal::CancellationToken) -> bool,
    D: FnMut(u64, PermissionDecision),
{
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "interactive mode requires an interactive TTY",
        ));
    }
    let _guard = TerminalGuard::enter(settings.alternate_screen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut model = TuiModel {
        viewport_height: 20,
        scroll_step: settings.scroll_step.max(1),
        theme: settings.theme,
        ..TuiModel::default()
    };
    model.set_runtime_context(context);
    let mut active_cancellation: Option<agent_terminal::CancellationToken> = None;
    let tick = Duration::from_millis(settings.tick_ms.clamp(10, 1000));
    let mut needs_draw = true;
    loop {
        while let Ok(event) = receiver.try_recv() {
            model.reduce(&event);
            needs_draw = true;
            if matches!(event, Event::TurnCompleted | Event::Error(_)) {
                active_cancellation = None;
            }
        }
        if cancellation.is_cancelled() {
            break;
        }
        if needs_draw {
            terminal.draw(|frame| render_frame(frame, &model, settings))?;
            needs_draw = false;
        }
        if crossterm::event::poll(tick)? {
            match crossterm::event::read()? {
                crossterm::event::Event::Paste(value) => {
                    model.composer.paste(&value);
                    needs_draw = true;
                }
                crossterm::event::Event::Key(key) => {
                    if model.overlay.is_some() {
                        let permission_id = model.permission_decision().map(|prompt| prompt.id);
                        let overlay_action = model.handle_overlay_key(key);
                        if let OverlayAction::Execute(command) = &overlay_action {
                            if let Some(selected) = command.strip_prefix("/model ") {
                                model.model = selected
                                    .rsplit_once('/')
                                    .map(|(_, model)| model.to_owned())
                                    .or_else(|| Some(selected.to_owned()));
                            }
                        }
                        match overlay_action {
                            OverlayAction::Execute(command) => {
                                if command == "/diff" {
                                    model.open_overlay(OverlayKind::Diff);
                                    needs_draw = true;
                                    continue;
                                }
                                if command == "/theme" {
                                    model.open_overlay(OverlayKind::Theme);
                                    needs_draw = true;
                                    continue;
                                }
                                if command == "/keybindings" {
                                    model.open_overlay(OverlayKind::Keybindings);
                                    needs_draw = true;
                                    continue;
                                }
                                let turn_cancellation =
                                    agent_terminal::CancellationToken::default();
                                active_cancellation = Some(turn_cancellation.clone());
                                model.active_turn = true;
                                if submit(command, sender.clone(), turn_cancellation) {
                                    cancellation.cancel();
                                }
                            }
                            OverlayAction::Fill(text) => {
                                model.composer.text = text;
                                model.composer.cursor = model.composer.text.len();
                            }
                            OverlayAction::Permission(decision) => {
                                if let Some(id) = permission_id {
                                    decide(id, decision);
                                }
                                if decision == PermissionDecision::Deny {
                                    if let Some(turn) = &active_cancellation {
                                        turn.cancel();
                                    }
                                }
                            }
                            OverlayAction::Open(_) | OverlayAction::Close | OverlayAction::None => {
                            }
                        }
                        needs_draw = true;
                        continue;
                    }
                    if key.code == KeyCode::Up && !model.active_turn {
                        if let Some(index) = model.history_index {
                            if index > 0 {
                                model.history_index = Some(index - 1);
                            }
                        } else if !model.history.is_empty() {
                            model.history_index = Some(model.history.len() - 1);
                        }
                        if let Some(index) = model.history_index {
                            model.composer.text = model.history[index].clone();
                            model.composer.cursor = model.composer.text.len();
                        }
                        needs_draw = true;
                        continue;
                    }
                    if key.code == KeyCode::Down && !model.active_turn {
                        if let Some(index) = model.history_index {
                            if index + 1 < model.history.len() {
                                model.history_index = Some(index + 1);
                                model.composer.text = model.history[index + 1].clone();
                            } else {
                                model.history_index = None;
                                model.composer.text.clear();
                            }
                            model.composer.cursor = model.composer.text.len();
                        }
                        needs_draw = true;
                        continue;
                    }
                    if key.code == KeyCode::PageUp {
                        model.reduce_input(&InputEvent::ScrollUp);
                        needs_draw = true;
                        continue;
                    }
                    if key.code == KeyCode::PageDown {
                        model.reduce_input(&InputEvent::ScrollDown);
                        needs_draw = true;
                        continue;
                    }
                    match model.composer.handle_key(key) {
                        ComposerAction::Submit(prompt) => {
                            needs_draw = true;
                            if prompt.trim() == "/diff" {
                                model.open_overlay(OverlayKind::Diff);
                                continue;
                            }
                            if prompt.trim() == "/theme" {
                                model.open_overlay(OverlayKind::Theme);
                                continue;
                            }
                            if prompt.trim() == "/keybindings" {
                                model.open_overlay(OverlayKind::Keybindings);
                                continue;
                            }
                            model.history.push(prompt.clone());
                            model.history_index = None;
                            model.active_turn = true;
                            let turn_cancellation = agent_terminal::CancellationToken::default();
                            active_cancellation = Some(turn_cancellation.clone());
                            if submit(prompt, sender.clone(), turn_cancellation) {
                                cancellation.cancel();
                            }
                        }
                        ComposerAction::Cancel => {
                            needs_draw = true;
                            if let Some(turn) = &active_cancellation {
                                turn.cancel();
                            } else {
                                model.composer.text.clear();
                                model.composer.cursor = 0;
                            }
                        }
                        ComposerAction::Exit => {
                            cancellation.cancel();
                        }
                        ComposerAction::Redraw => {
                            terminal.clear()?;
                            needs_draw = true;
                        }
                        ComposerAction::None => {
                            if model.composer.text == "/" {
                                model.composer.text.clear();
                                model.composer.cursor = 0;
                                model.open_overlay(OverlayKind::Palette);
                            }
                            needs_draw = true;
                        }
                    }
                }
                crossterm::event::Event::Resize(width, height) => {
                    model.reduce_input(&InputEvent::Resize { width, height });
                    needs_draw = true;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

pub fn run_headless<I>(
    receiver: Receiver<Event>,
    cancellation: agent_terminal::CancellationToken,
    inputs: I,
    max_ticks: usize,
) -> TuiModel
where
    I: IntoIterator<Item = InputEvent>,
{
    let mut model = TuiModel::default();
    for input in inputs.into_iter().take(max_ticks) {
        if input == InputEvent::Quit {
            cancellation.cancel();
        }
        model.reduce_input(&input);
        while let Ok(event) = receiver.try_recv() {
            model.reduce(&event);
        }
        if cancellation.is_cancelled() {
            break;
        }
    }
    model
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reducer_renders_stateful_events() {
        let mut model = TuiModel::default();
        model.reduce(&Event::TurnStarted);
        model.reduce(&Event::ToolStarted("read_file".into()));
        model.reduce(&Event::PermissionRequested("approval".into()));
        model.reduce(&Event::ContextCompacted("dropped 2".into()));
        let text = model.as_text();
        assert!(text.contains("read_file"));
        assert!(text.contains("approval"));
        assert!(text.contains("dropped 2"));
    }
    #[test]
    fn reducer_snapshot_is_deterministic() {
        let mut one = TuiModel::default();
        let mut two = TuiModel::default();
        for event in [
            Event::TurnStarted,
            Event::ModelDelta("ok".into()),
            Event::TurnCompleted,
        ] {
            one.reduce(&event);
            two.reduce(&event);
        }
        assert_eq!(one, two);
        assert_eq!(one.as_text(), "status: turn completed\nassistant: ok");
    }

    #[test]
    fn event_channel_is_bounded() {
        let (sender, _receiver) = bounded_event_channel(1);
        sender.try_send(Event::TurnStarted).expect("first event");
        assert!(sender.try_send(Event::TurnCompleted).is_err());
    }

    #[test]
    fn live_channel_carries_provider_events() {
        let (sender, receiver) = bounded_event_channel(4);
        sender.send(Event::ModelDelta("chunk".into())).unwrap();
        let mut model = TuiModel::default();
        model.reduce(&receiver.recv().unwrap());
        assert!(model.as_text().contains("chunk"));
    }

    #[test]
    fn headless_input_cancels_silent_provider_and_handles_resize_scroll() {
        let (sender, receiver) = bounded_event_channel(2);
        sender.send(Event::ModelDelta("one".into())).unwrap();
        let cancellation = agent_terminal::CancellationToken::default();
        let model = run_headless(
            receiver,
            cancellation.clone(),
            [
                InputEvent::Resize {
                    width: 80,
                    height: 10,
                },
                InputEvent::ScrollDown,
                InputEvent::Quit,
            ],
            3,
        );
        assert!(cancellation.is_cancelled());
        assert_eq!(model.viewport_height, 10);
        assert_eq!(model.scroll, 1);
        assert!(model.as_text().contains("one"));
    }

    #[test]
    fn settings_control_scroll_step_and_bounds() {
        let mut model = TuiModel {
            scroll_step: 3,
            ..TuiModel::default()
        };
        model.reduce_input(&InputEvent::ScrollDown);
        assert_eq!(model.scroll, 3);
        assert_eq!(TuiSettings::default().tick_ms, 50);
        assert_eq!(TuiSettings::default().channel_capacity, 128);
    }

    #[test]
    fn headless_disconnected_channel_exits_without_busy_wait() {
        let (sender, receiver) = bounded_event_channel(1);
        drop(sender);
        let model = run_headless(
            receiver,
            agent_terminal::CancellationToken::default(),
            [InputEvent::None],
            1,
        );
        assert_eq!(model.status, "");
    }

    #[test]
    fn composer_supports_editing_multiline_history_and_bounded_paste() {
        let mut composer = Composer::default();
        composer.paste("hello");
        composer.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        composer.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE));
        assert_eq!(composer.text, "hellXo");
        composer.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        assert!(composer.text.contains('\n'));
        let action = composer.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(action, ComposerAction::Cancel);
        composer.paste(&"x".repeat(70 * 1024));
        assert!(composer.text.len() <= Composer::MAX_INPUT_BYTES);
    }

    #[test]
    fn streaming_deltas_coalesce_and_transcript_stays_bounded() {
        let mut model = TuiModel::default();
        model.reduce(&Event::ModelDelta("hel".into()));
        model.reduce(&Event::ModelDelta("🙂".into()));
        assert_eq!(
            model.transcript,
            vec![TranscriptItem::Assistant("hel🙂".into())]
        );
        for index in 0..(MAX_TRANSCRIPT_ITEMS + 20) {
            model.push_item(TranscriptItem::Notification(index.to_string()));
        }
        assert!(model.transcript.len() <= MAX_TRANSCRIPT_ITEMS);
    }

    #[test]
    fn responsive_layout_and_typed_items_are_deterministic() {
        assert_eq!(layout_mode(120, 36), LayoutMode::Large);
        assert_eq!(layout_mode(80, 24), LayoutMode::Normal);
        assert_eq!(layout_mode(60, 20), LayoutMode::Narrow);
        assert_eq!(layout_mode(30, 7), LayoutMode::TooSmall);
        let mut model = TuiModel::default();
        model.set_runtime_context(TuiContext {
            workspace: Some("project".into()),
            branch: Some("main".into()),
            dirty_files: Some(2),
            model: Some("fake".into()),
            permission_mode: Some("workspace-write".into()),
            ..TuiContext::default()
        });
        model.push_item(TranscriptItem::Tool {
            name: "read".into(),
            output: "bounded".into(),
        });
        assert!(header_text(&model, LayoutMode::Normal).contains("main +2"));
        assert!(model.as_text().contains("tool read: bounded"));
    }

    #[test]
    fn palette_filters_locally_and_esc_closes_without_provider() {
        let mut model = TuiModel::default();
        model.set_palette(vec![
            PaletteCommand {
                name: "commands".into(),
                description: "list commands".into(),
                category: "Session".into(),
                arguments: String::new(),
                local: true,
            },
            PaletteCommand {
                name: "model".into(),
                description: "show model".into(),
                category: "Model".into(),
                arguments: String::new(),
                local: true,
            },
        ]);
        assert!(model.overlay.is_some());
        model.handle_overlay_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        model.handle_overlay_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert_eq!(model.filtered_palette().len(), 1);
        assert_eq!(
            model.handle_overlay_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            OverlayAction::Execute("/commands".into())
        );
        assert!(model.overlay.is_none());
        model.open_overlay(OverlayKind::Help);
        assert_eq!(
            model.handle_overlay_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            OverlayAction::Close
        );
    }

    #[test]
    fn model_and_session_overlays_select_typed_local_targets() {
        let mut model = TuiModel::default();
        model.models = vec![ModelOption {
            provider: "fake".into(),
            model: "gpt-5".into(),
            configured: true,
            current: true,
            capabilities: "offline".into(),
        }];
        model.sessions = vec![SessionOption {
            id: 7,
            title: "inspect".into(),
            turn_count: 2,
            last_activity: None,
            model: Some("gpt-5".into()),
            git_branch: None,
        }];
        model.open_overlay(OverlayKind::Model);
        assert_eq!(
            model.handle_overlay_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            OverlayAction::Execute("/model fake/gpt-5".into())
        );
        model.open_overlay(OverlayKind::Sessions);
        assert_eq!(
            model.handle_overlay_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            OverlayAction::Execute("/session-resume 7".into())
        );
    }

    #[test]
    fn permission_modal_is_bounded_and_fail_closed_on_escape() {
        let mut model = TuiModel::default();
        model.reduce(&Event::PermissionRequested(
            "WorkspaceWrite: apply_patch secret.txt".into(),
        ));
        assert!(model.overlay.is_some());
        assert!(model.overlay_text().contains("risk: medium"));
        assert_eq!(
            model.handle_overlay_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            OverlayAction::Permission(PermissionDecision::Deny)
        );
        assert!(model.permission_decision().is_none());
    }

    #[test]
    fn diff_theme_markdown_and_tool_card_are_deterministic() {
        let mut model = TuiModel::default();
        model.set_diff("@@ -1 +1 @@\n-old\n+new");
        model.open_overlay(OverlayKind::Diff);
        assert!(model.overlay_text().contains("+new"));
        model.open_overlay(OverlayKind::Theme);
        model.handle_overlay_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        assert_eq!(model.theme, TuiTheme::Monochrome);
        model.reduce(&Event::ToolStarted("shell".into()));
        model.reduce(&Event::ToolOutputDelta("bounded output".into()));
        model.reduce(&Event::ToolCompleted("ok".into()));
        assert_eq!(model.tool_cards[0].status, "ok");
        assert_eq!(model.tool_cards[0].output, "bounded output");
        assert_eq!(markdown_plain("# title\n- `code`"), "TITLE\n• code");
    }
}
