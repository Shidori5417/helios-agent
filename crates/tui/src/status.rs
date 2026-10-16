//! Live status, scrolling and tool-card helpers for the TUI.
//!
//! Everything here is deterministic local computation: no threads, no timers.
//! Time is injected by the caller so tests do not sleep.

use ratatui::{
    text::Line,
    widgets::{Paragraph, Wrap},
};
use std::time::{Duration, Instant};

pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// A completed turn longer than this rings the terminal bell.
pub const NOTIFY_AFTER: Duration = Duration::from_secs(10);
pub const WHEEL_ROWS: usize = 3;
pub const MAX_EXPANDED_LINES: usize = 20;
pub const THINKING_TAIL_LINES: usize = 2;

// ---------------------------------------------------------------- live status

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveStatus {
    pub started: Option<Instant>,
    pub last_tick: Option<Instant>,
    pub frame: usize,
    pub turn_tokens: u64,
    pub session_tokens: u64,
}

impl LiveStatus {
    pub fn start_turn(&mut self, now: Instant) {
        self.started = Some(now);
        self.last_tick = Some(now);
        self.frame = 0;
        self.turn_tokens = 0;
    }

    /// Ends the turn; returns true when the turn was long enough to notify.
    pub fn finish_turn(&mut self, now: Instant) -> bool {
        let long = self
            .started
            .is_some_and(|start| now.saturating_duration_since(start) > NOTIFY_AFTER);
        self.started = None;
        self.last_tick = None;
        long
    }

    /// Advances the spinner at most once per `tick`. Returns true if a redraw is due.
    pub fn on_tick(&mut self, now: Instant, active: bool, tick: Duration) -> bool {
        if !active {
            self.started = None;
            self.last_tick = None;
            return false;
        }
        self.started.get_or_insert(now);
        let due = self
            .last_tick
            .map(|last| now.saturating_duration_since(last) >= tick)
            .unwrap_or(true);
        if due {
            self.last_tick = Some(now);
            self.frame = (self.frame + 1) % SPINNER_FRAMES.len();
        }
        due
    }

    pub fn add_usage(&mut self, usage: &str) {
        if let Some(total) = parse_total_tokens(usage) {
            self.turn_tokens = self.turn_tokens.saturating_add(total);
            self.session_tokens = self.session_tokens.saturating_add(total);
        }
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        self.started
            .map(|start| now.saturating_duration_since(start))
            .unwrap_or_default()
    }
}

/// Extracts `total=N` from a usage event string such as `input=1 output=2 total=3`.
pub fn parse_total_tokens(usage: &str) -> Option<u64> {
    usage.split_whitespace().find_map(|part| {
        part.strip_prefix("total=")
            .and_then(|value| value.parse().ok())
    })
}

pub fn format_tokens(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=9_999 => trim_decimal(tokens as f64 / 1_000.0, 'k'),
        10_000..=999_999 => format!("{}k", tokens / 1_000),
        _ => trim_decimal(tokens as f64 / 1_000_000.0, 'M'),
    }
}

fn trim_decimal(value: f64, suffix: char) -> String {
    let text = format!("{:.1}", (value * 10.0).floor() / 10.0);
    format!("{}{suffix}", text.trim_end_matches(".0"))
}

pub fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// e.g. `⠋ working 12s · 3.4k tok · Esc to interrupt`
pub fn spinner_text(live: &LiveStatus, now: Instant) -> String {
    format!(
        "{} working {} · {} tok · Esc to interrupt",
        SPINNER_FRAMES[live.frame % SPINNER_FRAMES.len()],
        format_elapsed(live.elapsed(now)),
        format_tokens(live.turn_tokens)
    )
}

/// Inputs for the status line, kept separate from `TuiModel` so this is testable.
pub struct StatusParts<'a> {
    pub model: Option<&'a str>,
    pub permission: Option<&'a str>,
    pub context_percent: Option<u8>,
    pub session_tokens: u64,
    pub cost: Option<&'a str>,
    pub branch: Option<&'a str>,
}

/// `model · perm · ctx N% · N tok · cost · branch`, dropping the lowest priority
/// (rightmost) segments until it fits, then hard-truncating with an ellipsis.
pub fn status_line(parts: &StatusParts<'_>, width: usize) -> String {
    let mut segments: Vec<String> = Vec::new();
    if let Some(model) = parts.model {
        segments.push(model.to_owned());
    }
    if let Some(permission) = parts.permission {
        segments.push(permission.to_owned());
    }
    if let Some(percent) = parts.context_percent {
        segments.push(format!("ctx {percent}%"));
    }
    if parts.session_tokens > 0 {
        segments.push(format!("{} tok", format_tokens(parts.session_tokens)));
    }
    if let Some(cost) = parts.cost {
        segments.push(cost.to_owned());
    }
    if let Some(branch) = parts.branch {
        segments.push(branch.to_owned());
    }
    while segments.len() > 1 && joined_len(&segments) > width {
        segments.pop();
    }
    truncate_chars(&segments.join(" · "), width)
}

fn joined_len(segments: &[String]) -> usize {
    segments.iter().map(|s| s.chars().count()).sum::<usize>() + 3 * segments.len().saturating_sub(1)
}

pub fn truncate_chars(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}

fn tail_chars(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_owned();
    }
    let keep = width.saturating_sub(1);
    let mut out = String::from("…");
    out.extend(text.chars().skip(count - keep));
    out
}

// ------------------------------------------------------------------ scrolling

/// Last measured body geometry, in wrapped rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BodyMetrics {
    pub total_rows: usize,
    pub height: usize,
}

impl BodyMetrics {
    pub fn valid(&self) -> bool {
        self.height > 0
    }
    pub fn max_top(&self) -> usize {
        self.total_rows.saturating_sub(self.height)
    }
}

/// Top row actually shown.
pub fn effective_top(scroll: usize, follow: bool, metrics: BodyMetrics) -> usize {
    if follow {
        metrics.max_top()
    } else {
        scroll.min(metrics.max_top())
    }
}

/// Rows hidden below the viewport when not following.
pub fn rows_below(scroll: usize, follow: bool, metrics: BodyMetrics) -> usize {
    if follow {
        0
    } else {
        metrics.max_top() - effective_top(scroll, false, metrics)
    }
}

/// Scrolls toward the top by `rows` and disengages follow.
pub fn scroll_up(scroll: &mut usize, follow: &mut bool, metrics: BodyMetrics, rows: usize) {
    if metrics.valid() {
        *scroll = effective_top(*scroll, *follow, metrics);
    }
    *scroll = scroll.saturating_sub(rows);
    *follow = false;
}

/// Scrolls toward the bottom; reaching the bottom re-engages follow once the
/// geometry is known (without geometry it is plain line scrolling).
pub fn scroll_down(scroll: &mut usize, follow: &mut bool, metrics: BodyMetrics, rows: usize) {
    if !metrics.valid() {
        *scroll = scroll.saturating_add(rows);
        *follow = false;
        return;
    }
    let top = effective_top(*scroll, *follow, metrics).saturating_add(rows);
    if top >= metrics.max_top() {
        *scroll = metrics.max_top();
        *follow = true;
    } else {
        *scroll = top;
        *follow = false;
    }
}

/// Number of display rows `lines` occupy at `width` (matches what Paragraph renders).
pub fn wrapped_rows(lines: &[Line<'_>], width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width)
}

pub fn more_indicator(rows: usize) -> Option<String> {
    (rows > 0).then(|| format!(" ↓ {rows} more · End "))
}

// ---------------------------------------------------------------- tool cards

pub fn card_icon(status: &str) -> &'static str {
    let lower = status.to_ascii_lowercase();
    if lower == "running" || lower.is_empty() {
        "…"
    } else if ["err", "fail", "denied", "cancel", "timeout", "timed out"]
        .iter()
        .any(|bad| lower.contains(bad))
    {
        "✗"
    } else {
        "✓"
    }
}

/// One compact line: `✓ name · arg summary · 1.2s`
pub fn card_line(
    name: &str,
    status: &str,
    summary: &str,
    duration: Option<Duration>,
    width: usize,
) -> String {
    let mut line = format!("{} {name}", card_icon(status));
    let summary = summary.lines().next().unwrap_or("").trim();
    if !summary.is_empty() && summary != "tool execution" {
        line.push_str(" · ");
        line.push_str(&truncate_chars(summary, 48));
    }
    if let Some(duration) = duration {
        line.push_str(&format!(" · {:.1}s", duration.as_secs_f64()));
    }
    truncate_chars(&line, width)
}

/// Bounded expanded output: at most `MAX_EXPANDED_LINES` lines including an
/// "earlier lines" marker, each clipped to `width`.
pub fn expanded_lines(output: &str, width: usize) -> Vec<String> {
    let all: Vec<&str> = output.lines().collect();
    let mut out = Vec::new();
    let keep = if all.len() > MAX_EXPANDED_LINES {
        let keep = MAX_EXPANDED_LINES - 1;
        out.push(format!("  … {} earlier lines", all.len() - keep));
        keep
    } else {
        all.len()
    };
    for line in &all[all.len() - keep..] {
        out.push(truncate_chars(&format!("  │ {line}"), width));
    }
    out
}

/// Collapsed reasoning: the last `THINKING_TAIL_LINES` non-empty lines.
pub fn thinking_tail(text: &str, width: usize) -> Vec<String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let start = lines.len().saturating_sub(THINKING_TAIL_LINES);
    lines[start..]
        .iter()
        .map(|line| tail_chars(&format!("  {line}"), width))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(total: usize, height: usize) -> BodyMetrics {
        BodyMetrics {
            total_rows: total,
            height,
        }
    }

    #[test]
    fn follow_disengages_on_scroll_up_and_reengages_at_bottom() {
        let m = metrics(100, 10);
        let (mut scroll, mut follow) = (0usize, true);
        scroll_up(&mut scroll, &mut follow, m, 3);
        assert!(!follow);
        assert_eq!(scroll, 87);
        assert_eq!(rows_below(scroll, follow, m), 3);
        scroll_down(&mut scroll, &mut follow, m, 2);
        assert!(!follow);
        assert_eq!(scroll, 89);
        scroll_down(&mut scroll, &mut follow, m, 5);
        assert!(follow);
        assert_eq!(rows_below(scroll, follow, m), 0);
    }

    #[test]
    fn unknown_geometry_scrolls_by_lines() {
        let (mut scroll, mut follow) = (0usize, true);
        scroll_down(&mut scroll, &mut follow, BodyMetrics::default(), 3);
        assert_eq!((scroll, follow), (3, false));
    }

    #[test]
    fn wrapped_rows_counts_wrapping() {
        let lines = vec![Line::from("a".repeat(25)), Line::from("short")];
        assert_eq!(wrapped_rows(&lines, 10), 4);
        assert_eq!(wrapped_rows(&lines, 80), 2);
        assert_eq!(wrapped_rows(&lines, 0), 0);
    }

    #[test]
    fn spinner_text_shows_elapsed_tokens_and_hint() {
        let now = Instant::now();
        let mut live = LiveStatus::default();
        live.start_turn(now);
        live.add_usage("input=1 output=2 total=3400");
        let text = spinner_text(&live, now + Duration::from_secs(12));
        assert_eq!(text, "⠋ working 12s · 3.4k tok · Esc to interrupt");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1m05s");
        assert_eq!(format_tokens(12_345), "12k");
        assert_eq!(format_tokens(950), "950");
    }

    #[test]
    fn spinner_advances_once_per_tick_and_notifies_after_ten_seconds() {
        let now = Instant::now();
        let tick = Duration::from_millis(50);
        let mut live = LiveStatus::default();
        live.start_turn(now);
        assert!(!live.on_tick(now + Duration::from_millis(10), true, tick));
        assert!(live.on_tick(now + Duration::from_millis(60), true, tick));
        assert_eq!(live.frame, 1);
        assert!(!live.clone().finish_turn(now + Duration::from_secs(10)));
        assert!(live.finish_turn(now + Duration::from_secs(11)));
    }

    #[test]
    fn status_line_truncates_gracefully_at_40_columns() {
        let parts = StatusParts {
            model: Some("claude-sonnet"),
            permission: Some("workspace-write"),
            context_percent: Some(42),
            session_tokens: 3400,
            cost: Some("$0.12"),
            branch: Some("main"),
        };
        let wide = status_line(&parts, 120);
        assert_eq!(
            wide,
            "claude-sonnet · workspace-write · ctx 42% · 3.4k tok · $0.12 · main"
        );
        let narrow = status_line(&parts, 40);
        assert!(narrow.chars().count() <= 40, "{narrow}");
        assert!(narrow.starts_with("claude-sonnet"));
        assert_eq!(status_line(&parts, 8).chars().count(), 8);
    }

    #[test]
    fn card_lines_and_expansion_are_bounded() {
        let line = card_line(
            "read_file",
            "ok",
            "src/lib.rs",
            Some(Duration::from_millis(1200)),
            80,
        );
        assert_eq!(line, "✓ read_file · src/lib.rs · 1.2s");
        assert!(card_line("bash", "running", "tool execution", None, 80).starts_with("… bash"));
        assert!(card_line("bash", "error: boom", "", None, 80).starts_with('✗'));
        let output = (0..100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let lines = expanded_lines(&output, 80);
        assert_eq!(lines.len(), MAX_EXPANDED_LINES);
        assert!(lines[0].contains("earlier"));
        assert!(lines.last().unwrap().ends_with("99"));
    }

    #[test]
    fn thinking_tail_keeps_last_two_lines() {
        let tail = thinking_tail("one\n\ntwo\nthree", 40);
        assert_eq!(tail, vec!["  two".to_owned(), "  three".to_owned()]);
    }
}
