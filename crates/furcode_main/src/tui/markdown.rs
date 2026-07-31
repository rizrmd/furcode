//! Render assistant markdown via [`md_tui`] into ratatui [`Line`]s.
//!
//! The frame TUI stores assistant text as raw markdown and only formats it at
//! draw time so streaming tokens stay cheap to append.

use md_tui::nodes::textcomponent::TextNode;
use md_tui::nodes::word::{MetaData, Word, WordType};
use md_tui::parser::parse_markdown;
use md_tui::util::colors::{color_config, heading_colors};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Convert a markdown document into styled terminal lines for the given width.
///
/// On parse failure (or empty result) falls back to plain white text lines so
/// partial / broken streaming markdown still shows something useful.
pub fn markdown_to_lines(md: &str, width: u16) -> Vec<Line<'static>> {
    let width = width.max(20);
    let trimmed = md.trim_end();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let root = parse_markdown(None, trimmed, width);
    let mut lines: Vec<Line<'static>> = Vec::new();

    for comp in root.components() {
        if comp.is_hidden() {
            continue;
        }
        match comp.kind() {
            TextNode::LineBreak => {
                lines.push(Line::from(Span::raw("")));
            }
            TextNode::HorizontalSeparator => {
                let bar = "─".repeat(width as usize);
                lines.push(Line::from(Span::styled(
                    bar,
                    Style::default().fg(Color::DarkGray),
                )));
            }
            TextNode::Heading => {
                let level = heading_level(comp.meta_info());
                for row in comp.content() {
                    let spans: Vec<Span<'static>> = row
                        .iter()
                        .filter(|w| w.is_renderable())
                        .map(|w| style_heading_word(w, level))
                        .collect();
                    if !spans.is_empty() {
                        lines.push(Line::from(spans));
                    }
                }
            }
            TextNode::CodeBlock => {
                let bg = color_config().code_block_bg_color;
                for row in comp.content() {
                    let mut spans: Vec<Span<'static>> = row
                        .iter()
                        .filter(|w| w.is_renderable())
                        .map(style_word)
                        .collect();
                    if spans.is_empty() {
                        spans.push(Span::styled(
                            " ",
                            Style::default().bg(bg),
                        ));
                    } else {
                        for s in &mut spans {
                            s.style = s.style.bg(bg);
                        }
                    }
                    lines.push(Line::from(spans));
                }
            }
            TextNode::Quote => {
                for row in comp.content() {
                    let mut spans = vec![Span::styled(
                        "│ ",
                        Style::default().fg(color_config().quote_default),
                    )];
                    spans.extend(
                        row.iter()
                            .filter(|w| w.is_renderable())
                            .map(style_word),
                    );
                    lines.push(Line::from(spans));
                }
            }
            TextNode::Task => {
                let marker = task_marker(comp.meta_info());
                for (i, row) in comp.content().iter().enumerate() {
                    let mut spans = Vec::new();
                    if i == 0 {
                        spans.push(Span::styled(
                            marker,
                            Style::default().fg(Color::White),
                        ));
                    } else {
                        spans.push(Span::raw("  "));
                    }
                    spans.extend(
                        row.iter()
                            .filter(|w| w.is_renderable())
                            .map(style_word),
                    );
                    lines.push(Line::from(spans));
                }
            }
            TextNode::Footnote | TextNode::Image | TextNode::DetailsSummary { .. } => {
                for row in comp.content() {
                    let spans: Vec<Span<'static>> = row
                        .iter()
                        .filter(|w| w.is_renderable())
                        .map(style_word)
                        .collect();
                    if !spans.is_empty() {
                        lines.push(Line::from(spans));
                    }
                }
            }
            TextNode::Paragraph | TextNode::List | TextNode::Table(_, _) => {
                for row in comp.content() {
                    let spans: Vec<Span<'static>> = row
                        .iter()
                        .filter(|w| w.is_renderable())
                        .map(style_word)
                        .collect();
                    // Keep empty rows so paragraph spacing matches the source.
                    lines.push(Line::from(spans));
                }
            }
        }
    }

    if lines.is_empty() {
        return plain_fallback(trimmed);
    }
    lines
}

fn plain_fallback(md: &str) -> Vec<Line<'static>> {
    md.lines()
        .map(|line| {
            Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::White),
            ))
        })
        .collect()
}

fn heading_level(meta: &[Word]) -> u8 {
    meta.iter()
        .find_map(|w| match w.kind() {
            WordType::MetaInfo(MetaData::HeadingLevel(level)) => Some(level),
            _ => None,
        })
        .unwrap_or(1)
}

fn task_marker(meta: &[Word]) -> &'static str {
    // md-tui stores open/closed task state in meta; default to unchecked.
    let closed = meta.iter().any(|w| {
        matches!(
            w.kind(),
            WordType::MetaInfo(MetaData::Other) if w.content().contains('x') || w.content().contains('X')
        ) || w.content() == "[x]"
            || w.content() == "[X]"
    });
    if closed {
        "☑ "
    } else {
        "☐ "
    }
}

fn style_heading_word(word: &Word, level: u8) -> Span<'static> {
    let colors = color_config();
    let heads = heading_colors();
    let fg = match level {
        1 => colors.heading_fg_color,
        2 => heads.level_2,
        3 => heads.level_3,
        4 => heads.level_4,
        5 => heads.level_5,
        _ => heads.level_6,
    };
    let mut style = Style::default().fg(fg).add_modifier(Modifier::BOLD);
    if level == 1 {
        style = style.bg(colors.heading_bg_color);
    }
    Span::styled(word.content().to_string(), style)
}

fn style_word(word: &Word) -> Span<'static> {
    let colors = color_config();
    let content = word.content().to_string();
    match word.kind() {
        WordType::MetaInfo(_) | WordType::LinkData | WordType::FootnoteData => {
            Span::raw(String::new())
        }
        WordType::Selected => Span::styled(
            content,
            Style::default()
                .fg(colors.link_selected_fg_color)
                .bg(colors.link_selected_bg_color),
        ),
        WordType::Normal | WordType::White | WordType::ListMarker => {
            Span::styled(content, Style::default().fg(Color::White))
        }
        WordType::Code => Span::styled(
            content,
            Style::default()
                .fg(colors.code_fg_color)
                .bg(colors.code_bg_color),
        ),
        WordType::Link | WordType::FootnoteInline => {
            Span::styled(content, Style::default().fg(colors.link_color))
        }
        WordType::Italic => Span::styled(
            content,
            Style::default()
                .fg(colors.italic_color)
                .add_modifier(Modifier::ITALIC),
        ),
        WordType::Bold => Span::styled(
            content,
            Style::default()
                .fg(colors.bold_color)
                .add_modifier(Modifier::BOLD),
        ),
        WordType::Strikethrough | WordType::Footnote => Span::styled(
            content,
            Style::default()
                .fg(colors.striketrough_color)
                .add_modifier(Modifier::CROSSED_OUT),
        ),
        WordType::BoldItalic => Span::styled(
            content,
            Style::default()
                .fg(colors.bold_italic_color)
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::ITALIC),
        ),
        WordType::CodeBlock(c) => Span::styled(content, Style::default().fg(c)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_bold_without_markers() {
        let lines = markdown_to_lines("Hello **world**!", 80);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(flat.contains("world"), "got {flat:?}");
        assert!(!flat.contains("**"), "raw markers leaked: {flat:?}");
    }

    #[test]
    fn renders_inline_code() {
        let lines = markdown_to_lines("Use `cargo build`", 80);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(flat.contains("cargo build"), "got {flat:?}");
        assert!(!flat.contains('`'), "backticks leaked: {flat:?}");
    }

    #[test]
    fn empty_input_is_empty() {
        assert!(markdown_to_lines("", 80).is_empty());
        assert!(markdown_to_lines("   \n", 80).is_empty());
    }
}
