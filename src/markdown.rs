//! Markdown → styled terminal lines, and word wrapping for styled lines.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme;

pub fn render(text: &str) -> Vec<Line<'static>> {
    let mut r = Renderer::default();
    // TeX math first: the parser would strip the backslashes from `\[`, `\(` and `\,` before we could see them.
    let text = crate::math::render_math(text);
    for event in Parser::new_ext(&text, Options::ENABLE_STRIKETHROUGH) {
        r.event(event);
    }
    r.flush();
    while r.lines.last().is_some_and(|l| l.spans.is_empty()) {
        r.lines.pop();
    }
    r.lines
}

#[derive(Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    styles: Vec<Style>,
    lists: Vec<Option<u64>>,
    in_code: bool,
    prefix: Vec<Span<'static>>,
}

impl Renderer {
    fn style(&self) -> Style {
        self.styles.iter().fold(Style::default(), |acc, s| acc.patch(*s))
    }

    fn push(&mut self, text: String, style: Style) {
        if self.current.is_empty() && !self.prefix.is_empty() {
            self.current.extend(self.prefix.clone());
            // The list marker is only shown on the item's first line.
            for span in self.prefix.iter_mut() {
                let width = span.content.width();
                span.content = " ".repeat(width).into();
            }
        }
        self.current.push(Span::styled(text, style));
    }

    fn flush(&mut self) {
        if !self.current.is_empty() {
            self.lines.push(Line::from(std::mem::take(&mut self.current)));
        }
    }

    fn blank(&mut self) {
        self.flush();
        if self.lines.last().is_some_and(|l| !l.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) if self.in_code => {
                for line in text.lines() {
                    self.lines.push(Line::from(vec![
                        Span::styled("  │ ", theme::dim()),
                        Span::styled(line.to_string(), theme::code()),
                    ]));
                }
            }
            Event::Text(text) => {
                let style = self.style();
                self.push(text.into_string(), style);
            }
            Event::Code(code) => self.push(code.into_string(), theme::inline_code()),
            Event::SoftBreak => {
                let style = self.style();
                self.push(" ".into(), style);
            }
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.blank();
                self.lines.push(Line::styled("─".repeat(40), theme::dim()));
            }
            Event::TaskListMarker(done) => self.push(if done { "[x] " } else { "[ ] " }.into(), theme::dim()),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.blank();
                let style = match level {
                    HeadingLevel::H1 | HeadingLevel::H2 => theme::accent().add_modifier(Modifier::BOLD),
                    _ => Style::default().add_modifier(Modifier::BOLD),
                };
                self.styles.push(style);
            }
            Tag::CodeBlock(kind) => {
                self.blank();
                self.in_code = true;
                if let CodeBlockKind::Fenced(lang) = kind {
                    if !lang.is_empty() {
                        self.lines.push(Line::styled(format!("  ╭ {lang}"), theme::dim()));
                    }
                }
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.blank();
                } else {
                    self.flush();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    _ => "• ".to_string(),
                };
                self.prefix = vec![Span::raw("  ".repeat(depth)), Span::styled(marker, theme::accent())];
            }
            Tag::Emphasis => self.styles.push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.styles.push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { .. } => self.styles.push(theme::link()),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if self.lists.is_empty() {
                    self.blank();
                } else {
                    self.flush();
                }
            }
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.flush();
            }
            TagEnd::CodeBlock => {
                self.in_code = false;
                self.lines.push(Line::default());
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                self.prefix.clear();
                if self.lists.is_empty() {
                    self.lines.push(Line::default());
                }
            }
            TagEnd::Item => {
                self.flush();
                self.prefix.clear();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.styles.pop();
            }
            _ => {}
        }
    }
}

/// Word-wrap styled lines to `width` columns. Continuation lines keep the line's leading indentation.
pub fn wrap(lines: &[Line<'static>], width: u16) -> Vec<Line<'static>> {
    let width = width.max(10) as usize;
    let mut out = Vec::new();
    for line in lines {
        if line.width() <= width {
            out.push(line.clone());
            continue;
        }
        let indent = hanging_indent(line).min(width / 2);
        let mut current: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        for span in &line.spans {
            for token in split_keep_spaces(&span.content) {
                let mut token = token.to_string();
                loop {
                    let w = token.width();
                    if used + w <= width {
                        used += w;
                        current.push(Span::styled(token, span.style));
                        break;
                    }
                    if token.trim().is_empty() {
                        break; // drop spaces at a wrap point
                    }
                    if used > indent {
                        out.push(Line::from(std::mem::take(&mut current)));
                        current.push(Span::raw(" ".repeat(indent)));
                        used = indent;
                        continue;
                    }
                    // A single token wider than the line: hard-split it.
                    let (head, rest) = split_at_width(&token, width - used);
                    current.push(Span::styled(head, span.style));
                    out.push(Line::from(std::mem::take(&mut current)));
                    current.push(Span::raw(" ".repeat(indent)));
                    used = indent;
                    token = rest;
                }
            }
        }
        if !current.is_empty() {
            out.push(Line::from(current));
        }
    }
    out
}

/// Width of a line's leading gutter (indentation, bullets, list numbers, code bars), so wrapped
/// continuation lines line up under the text instead of the margin.
fn hanging_indent(line: &Line) -> usize {
    let mut width = 0;
    for span in &line.spans {
        let text = span.content.as_ref();
        let t = text.trim();
        let numbered = t.len() > 1 && t.ends_with('.') && t[..t.len() - 1].chars().all(|c| c.is_ascii_digit());
        if t.is_empty() || numbered || matches!(t, "•" | "●" | "│" | "-" | "*" | "›" | "▸" | "○" | "✓") {
            width += text.width();
        } else {
            width += text.len() - text.trim_start().len();
            break;
        }
    }
    width
}

fn split_keep_spaces(s: &str) -> Vec<&str> {
    let mut tokens = vec![];
    let mut start = 0;
    let mut in_space = None;
    for (i, c) in s.char_indices() {
        let space = c == ' ';
        if in_space.is_some_and(|prev| prev != space) {
            tokens.push(&s[start..i]);
            start = i;
        }
        in_space = Some(space);
    }
    if start < s.len() {
        tokens.push(&s[start..]);
    }
    tokens
}

fn split_at_width(s: &str, width: usize) -> (String, String) {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > width.max(1) {
            return (s[..i].to_string(), s[i..].to_string());
        }
        used += w;
    }
    (s.to_string(), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    #[test]
    fn renders_lists_and_code() {
        let lines = render("Intro\n\n- one\n- two\n\n```rust\nfn x() {}\n```\n");
        let t = text(&lines);
        assert!(t.contains(&"• one".to_string()) && t.contains(&"• two".to_string()));
        assert!(t.contains(&"  │ fn x() {}".to_string()));
    }

    #[test]
    fn wraps_words_with_indent() {
        let lines = wrap(&[Line::from("  alpha beta gamma delta")], 14);
        assert_eq!(text(&lines), vec!["  alpha beta ", "  gamma delta"]);
        let item = Line::from(vec![Span::raw("● "), Span::raw("• "), Span::raw("one two three four")]);
        assert_eq!(text(&wrap(&[item], 14)), vec!["● • one two ", "    three four"]);
    }
}
