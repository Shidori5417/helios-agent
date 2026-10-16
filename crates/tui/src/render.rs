//! Styled (colour/modifier) rendering of transcript content. Pure functions that
//! turn markdown and diffs into bounded ratatui lines; no I/O.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::TuiTheme;

const MAX_MARKDOWN_BYTES: usize = 256 * 1024;
const MAX_DIFF_LINES: usize = 2_000;
const MAX_DIFF_BYTES: usize = 128 * 1024;

/// Semantic style tokens. `None` colours mean "terminal default / modifiers only".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub accent: Option<Color>,
    pub muted: Option<Color>,
    pub success: Option<Color>,
    pub danger: Option<Color>,
    pub code_fg: Option<Color>,
    pub code_bg: Option<Color>,
    pub border: Option<Color>,
    pub warning: Option<Color>,
}

impl Palette {
    /// Build a palette. `color` is false for NO_COLOR / `--color never`.
    pub fn new(theme: TuiTheme, color: bool) -> Self {
        if !color || theme == TuiTheme::Monochrome {
            return Self::monochrome();
        }
        match theme {
            TuiTheme::Light => Self {
                accent: Some(Color::Rgb(0, 95, 135)),
                muted: Some(Color::Rgb(100, 100, 100)),
                success: Some(Color::Rgb(0, 110, 40)),
                danger: Some(Color::Rgb(170, 0, 0)),
                code_fg: Some(Color::Rgb(135, 55, 0)),
                code_bg: Some(Color::Rgb(235, 235, 235)),
                border: Some(Color::Rgb(0, 95, 135)),
                warning: Some(Color::Rgb(150, 90, 0)),
            },
            _ => Self {
                accent: Some(Color::Cyan),
                muted: Some(Color::DarkGray),
                success: Some(Color::Green),
                danger: Some(Color::Red),
                code_fg: Some(Color::Yellow),
                code_bg: None,
                border: Some(Color::Cyan),
                warning: Some(Color::Yellow),
            },
        }
    }

    fn monochrome() -> Self {
        Self {
            accent: None,
            muted: None,
            success: None,
            danger: None,
            code_fg: None,
            code_bg: None,
            border: None,
            warning: None,
        }
    }

    fn is_mono(&self) -> bool {
        *self == Self::monochrome()
    }

    fn fg(color: Option<Color>) -> Style {
        color.map_or_else(Style::default, |c| Style::default().fg(c))
    }

    pub fn accent_style(&self) -> Style {
        Self::fg(self.accent).add_modifier(Modifier::BOLD)
    }

    pub fn danger_style(&self) -> Style {
        let style = Self::fg(self.danger);
        if self.is_mono() {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }

    pub fn muted_style(&self) -> Style {
        let style = Self::fg(self.muted);
        if self.is_mono() {
            style.add_modifier(Modifier::DIM)
        } else {
            style
        }
    }

    pub fn warning_style(&self) -> Style {
        let style = Self::fg(self.warning);
        if self.is_mono() {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }

    pub fn border_style(&self) -> Style {
        Self::fg(self.border)
    }

    fn code_style(&self) -> Style {
        let mut style = Self::fg(self.code_fg);
        if let Some(bg) = self.code_bg {
            style = style.bg(bg);
        }
        if self.is_mono() {
            style = style.add_modifier(Modifier::REVERSED);
        }
        style
    }
}

struct Builder<'a> {
    palette: &'a Palette,
    width: u16,
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    lists: Vec<Option<u64>>,
    quote: usize,
    code: bool,
    link: Option<String>,
    item_prefix_pending: Option<String>,
}

impl Builder<'_> {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, f: impl FnOnce(Style) -> Style) {
        let next = f(self.style());
        self.styles.push(next);
    }

    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let mut spans = Vec::new();
        if self.quote > 0 {
            spans.push(Span::styled(
                "│ ".repeat(self.quote),
                self.palette.muted_style(),
            ));
        }
        if let Some(marker) = self.item_prefix_pending.take() {
            spans.push(Span::styled(marker, self.palette.accent_style()));
        } else if !self.lists.is_empty() {
            spans.push(Span::raw(" ".repeat(2 * self.lists.len())));
        }
        spans.append(&mut self.spans);
        self.lines.push(Line::from(spans));
    }

    fn blank(&mut self) {
        if self.lines.last().is_some_and(|l| !l.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }

    fn text(&mut self, text: &str) {
        let style = self.style();
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                if self.code {
                    self.code_line_end();
                } else {
                    self.flush();
                }
            }
            if !part.is_empty() {
                self.spans.push(Span::styled(part.to_owned(), style));
            }
        }
    }

    fn code_line_end(&mut self) {
        let mut spans = vec![Span::styled("▌ ", self.palette.accent_style())];
        spans.append(&mut self.spans);
        self.lines.push(Line::from(spans));
    }

    fn start_item(&mut self) {
        let depth = self.lists.len().saturating_sub(1);
        let marker = match self.lists.last_mut() {
            Some(Some(n)) => {
                let m = format!("{}{}. ", "  ".repeat(depth), n);
                *n += 1;
                m
            }
            _ => format!("{}• ", "  ".repeat(depth)),
        };
        self.item_prefix_pending = Some(marker);
    }
}

/// Render markdown to styled lines. Tolerates incomplete (streaming) input:
/// an unterminated fence is rendered as code to the end.
pub fn markdown_lines(text: &str, palette: &Palette, width: u16) -> Vec<Line<'static>> {
    let text = if text.len() > MAX_MARKDOWN_BYTES {
        let mut end = MAX_MARKDOWN_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    } else {
        text
    };
    let mut b = Builder {
        palette,
        width,
        lines: Vec::new(),
        spans: Vec::new(),
        styles: vec![Style::default()],
        lists: Vec::new(),
        quote: 0,
        code: false,
        link: None,
        item_prefix_pending: None,
    };
    for event in Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { .. } => {
                    b.blank();
                    b.push_style(|_| palette.accent_style());
                }
                Tag::Strong => b.push_style(|s| s.add_modifier(Modifier::BOLD)),
                Tag::Emphasis => b.push_style(|s| s.add_modifier(Modifier::ITALIC)),
                Tag::Strikethrough => b.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT)),
                Tag::BlockQuote(_) => {
                    b.flush();
                    b.quote += 1;
                }
                Tag::CodeBlock(kind) => {
                    b.flush();
                    b.blank();
                    if let CodeBlockKind::Fenced(lang) = kind {
                        let lang = lang.split_whitespace().next().unwrap_or("");
                        if !lang.is_empty() {
                            b.lines.push(Line::from(vec![
                                Span::styled("▌ ", palette.accent_style()),
                                Span::styled(lang.to_owned(), palette.muted_style()),
                            ]));
                        }
                    }
                    b.code = true;
                    b.push_style(|_| palette.code_style());
                }
                Tag::List(start) => {
                    b.flush();
                    b.lists.push(start);
                }
                Tag::Item => {
                    b.flush();
                    b.start_item();
                }
                Tag::Link { dest_url, .. } => {
                    b.link = Some(dest_url.to_string());
                    b.push_style(|s| s.add_modifier(Modifier::UNDERLINED));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    b.styles.pop();
                    b.flush();
                    b.blank();
                }
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                    b.styles.pop();
                }
                TagEnd::Paragraph => {
                    b.flush();
                    if b.lists.is_empty() {
                        b.blank();
                    }
                }
                TagEnd::BlockQuote(_) => {
                    b.flush();
                    b.quote = b.quote.saturating_sub(1);
                    b.blank();
                }
                TagEnd::CodeBlock => {
                    if !b.spans.is_empty() {
                        b.code_line_end();
                    }
                    b.code = false;
                    b.styles.pop();
                    b.blank();
                }
                TagEnd::List(_) => {
                    b.flush();
                    b.lists.pop();
                    if b.lists.is_empty() {
                        b.blank();
                    }
                }
                TagEnd::Item => b.flush(),
                TagEnd::Link => {
                    b.styles.pop();
                    if let Some(url) = b.link.take() {
                        if !url.is_empty() {
                            let style = palette.muted_style();
                            b.spans.push(Span::styled(format!(" ({url})"), style));
                        }
                    }
                }
                _ => {}
            },
            Event::Text(t) => b.text(&t),
            Event::Code(t) => {
                let style = b.style().patch(palette.code_style());
                b.spans.push(Span::styled(t.to_string(), style));
            }
            Event::SoftBreak | Event::HardBreak => b.flush(),
            Event::Rule => {
                b.flush();
                let n = usize::from(b.width.clamp(1, 80));
                b.lines
                    .push(Line::styled("─".repeat(n), palette.muted_style()));
            }
            Event::Html(t) | Event::InlineHtml(t) => b.text(&t),
            _ => {}
        }
    }
    // Flush any dangling content (incomplete streaming input).
    if b.code && !b.spans.is_empty() {
        b.code_line_end();
    } else {
        b.flush();
    }
    while b.lines.last().is_some_and(|l| l.spans.is_empty()) {
        b.lines.pop();
    }
    b.lines
}

/// Render a unified diff with colours. Same bounds as the plain diff view.
pub fn diff_lines(diff: &str, palette: &Palette) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for line in diff.lines().take(MAX_DIFF_LINES) {
        bytes += line.len() + 1;
        if bytes > MAX_DIFF_BYTES {
            break;
        }
        let header = line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("diff ")
            || line.starts_with("index ");
        let style = if header {
            Style::default().add_modifier(Modifier::BOLD)
        } else if line.starts_with('+') {
            let s = Palette::fg(palette.success);
            if palette.is_mono() {
                s.add_modifier(Modifier::BOLD)
            } else {
                s
            }
        } else if line.starts_with('-') {
            let s = Palette::fg(palette.danger);
            if palette.is_mono() {
                s.add_modifier(Modifier::DIM)
            } else {
                s
            }
        } else if line.starts_with("@@") {
            Palette::fg(palette.accent)
        } else {
            Style::default()
        };
        out.push(Line::styled(line.to_owned(), style));
    }
    if out.is_empty() {
        out.push(Line::styled("(no diff lines)", palette.muted_style()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, widgets::Paragraph, Terminal};

    fn dark() -> Palette {
        Palette::new(TuiTheme::Dark, true)
    }

    fn all_spans<'a>(lines: &'a [Line<'static>]) -> impl Iterator<Item = &'a Span<'static>> {
        lines.iter().flat_map(|l| l.spans.iter())
    }

    fn flat(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn heading_is_bold_accent() {
        let lines = markdown_lines("# Title\n\nbody", &dark(), 80);
        let span = &lines[0].spans[0];
        assert_eq!(span.content, "Title");
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(span.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn inline_styles() {
        let lines = markdown_lines("a **b** *c* `d`", &dark(), 80);
        let find = |t: &str| all_spans(&lines).find(|s| s.content == t).unwrap().style;
        assert!(find("b").add_modifier.contains(Modifier::BOLD));
        assert!(find("c").add_modifier.contains(Modifier::ITALIC));
        assert_eq!(find("d").fg, Some(Color::Yellow));
    }

    #[test]
    fn fenced_code_has_gutter_and_language() {
        let lines = markdown_lines("```rust\nfn main() {}\n```\n", &dark(), 80);
        let text = flat(&lines);
        assert!(text.contains("▌ rust"));
        assert!(text.contains("▌ fn main() {}"));
    }

    #[test]
    fn unterminated_fence_renders_as_code() {
        let lines = markdown_lines("text\n\n```py\nprint(1)\nprint(2)", &dark(), 80);
        let text = flat(&lines);
        assert!(text.contains("▌ print(1)"));
        assert!(text.contains("▌ print(2)"));
    }

    #[test]
    fn lists_quotes_and_links() {
        let lines = markdown_lines(
            "- one\n- two\n\n1. first\n2. second\n\n> quoted\n\n[x](http://e.com)",
            &dark(),
            80,
        );
        let text = flat(&lines);
        assert!(text.contains("• one"));
        assert!(text.contains("• two"));
        assert!(text.contains("1. first"));
        assert!(text.contains("2. second"));
        assert!(text.contains("│ quoted"));
        assert!(text.contains("x (http://e.com)"));
    }

    #[test]
    fn diff_colours() {
        let lines = diff_lines("--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n ctx", &dark());
        assert!(lines[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[2].style.fg, Some(Color::Cyan));
        assert_eq!(lines[3].style.fg, Some(Color::Red));
        assert_eq!(lines[4].style.fg, Some(Color::Green));
        assert_eq!(lines[5].style.fg, None);
    }

    #[test]
    fn monochrome_has_no_colours() {
        for palette in [
            Palette::new(TuiTheme::Monochrome, true),
            Palette::new(TuiTheme::Dark, false),
        ] {
            let mut lines =
                markdown_lines("# H\n**b** `c`\n```\ncode\n```\n[l](u)\n> q", &palette, 80);
            lines.extend(diff_lines("@@ x\n-a\n+b", &palette));
            for line in &lines {
                assert_eq!(line.style.fg, None);
                assert_eq!(line.style.bg, None);
            }
            for span in all_spans(&lines) {
                assert_eq!(span.style.fg, None);
                assert_eq!(span.style.bg, None);
            }
        }
    }

    #[test]
    fn dark_and_light_differ() {
        assert_ne!(
            Palette::new(TuiTheme::Dark, true),
            Palette::new(TuiTheme::Light, true)
        );
    }

    #[test]
    fn test_backend_renders_markdown() {
        let backend = TestBackend::new(30, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        let lines = markdown_lines("# Hi\n**bold**", &dark(), 30);
        terminal
            .draw(|f| f.render_widget(Paragraph::new(lines), f.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "H");
        assert_eq!(buffer[(0, 0)].fg, Color::Cyan);
        let bold_row = (0..4)
            .find(|y| buffer[(0, *y)].symbol() == "b")
            .expect("bold row");
        assert!(buffer[(0, bold_row)].modifier.contains(Modifier::BOLD));
    }
}
