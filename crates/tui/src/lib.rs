use agent_protocol::Event;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};
use std::{
    io::{self, IsTerminal},
    sync::mpsc::{Receiver, SyncSender, TryRecvError},
    time::Duration,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TuiModel {
    pub status: String,
    pub transcript: Vec<String>,
    pub tool: Option<String>,
    pub permission: Option<String>,
    pub verification: Option<String>,
    pub context: Option<String>,
    pub scroll: usize,
    pub viewport_height: usize,
    pub scroll_step: usize,
}

impl TuiModel {
    pub fn reduce(&mut self, event: &Event) {
        match event {
            Event::TurnStarted => self.status = "turn started".into(),
            Event::ModelDelta(text) => self.transcript.push(format!("model: {text}")),
            Event::ReasoningSummaryDelta(text) => {
                self.transcript.push(format!("reasoning: {text}"))
            }
            Event::ToolRequested(name) | Event::ToolStarted(name) => self.tool = Some(name.clone()),
            Event::ToolOutputDelta(text) => self.transcript.push(format!("tool output: {text}")),
            Event::ToolCompleted(status) => {
                self.status = format!("tool: {status}");
                self.tool = None;
            }
            Event::PatchProduced(path) => self.transcript.push(format!("patch: {path}")),
            Event::VerificationStarted(command) => {
                self.verification = Some(format!("started: {command}"))
            }
            Event::VerificationResult(result) => {
                self.verification = Some(format!("result: {result}"))
            }
            Event::PermissionRequested(request) => self.permission = Some(request.clone()),
            Event::ContextCompacted(summary) => self.context = Some(summary.clone()),
            Event::UsageUpdated(usage) => self.transcript.push(format!("usage: {usage}")),
            Event::HookStarted(name) => self.transcript.push(format!("hook started: {name}")),
            Event::HookResult(result) => self.transcript.push(format!("hook: {result}")),
            Event::McpToolDiscovered(name) => self.transcript.push(format!("MCP tool: {name}")),
            Event::McpToolResult(result) => self.transcript.push(format!("MCP result: {result}")),
            Event::TurnCompleted => self.status = "turn completed".into(),
            Event::Error(error) => self.status = format!("error: {error}"),
        }
    }
    pub fn as_text(&self) -> String {
        let mut lines = vec![format!("status: {}", self.status)];
        lines.extend(self.transcript.clone());
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
}

impl Default for TuiSettings {
    fn default() -> Self {
        Self {
            tick_ms: 50,
            channel_capacity: 128,
            scroll_step: 1,
            color: TuiColor::Auto,
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

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
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
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut model = TuiModel {
        viewport_height: 24,
        scroll_step: settings.scroll_step.max(1),
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
            let mut block = Block::default().borders(Borders::ALL).title("agent-cli");
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
        assert_eq!(one.as_text(), "status: turn completed\nmodel: ok");
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
}
