use nu_ansi_term::{Color, Style};

pub(crate) struct StyledText {
    pub(crate) buffer: Vec<(Style, String)>,
}

impl StyledText {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    pub fn push(&mut self, value: (Style, String)) {
        self.buffer.push(value);
    }
}

/// Syntax highlighter for the furcode readline prompt.
///
/// Applies visual styles to recognised input patterns as the user types:
/// - Commands (`:foo` or `/foo` for backward compatibility) are rendered in
///   yellow bold.
/// - File mentions (`@[path]`) are rendered in cyan bold.
/// - Clipboard image atoms (`[Image N]`) are rendered in magenta bold.
/// - Shell pass-through commands (`!cmd`) are rendered in magenta.
/// - All other text is rendered in the default terminal style.
pub struct FurcodeHighlighter;

impl FurcodeHighlighter {
    pub(crate) fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::new();

        if line.is_empty() {
            return styled;
        }

        // Command: highlight the command token (e.g. `/compact`) in yellow bold,
        // then the remainder (arguments) without special styling.
        if line.starts_with('/') {
            let end = line.find(|c: char| c.is_whitespace()).unwrap_or(line.len());
            styled.push((
                Style::new().bold().fg(Color::Yellow),
                line.get(..end).unwrap_or(line).to_string(),
            ));
            if end < line.len()
                && let Some(args) = line.get(end..)
            {
                highlight_segments(args, &mut styled);
            }
            return styled;
        }

        // Shell pass-through: `!<cmd>` rendered in magenta.
        if line.starts_with('!') {
            styled.push((Style::new().fg(Color::Magenta), line.to_string()));
            return styled;
        }

        // General message text — scan for `@[...]` mentions and `[Image N]` atoms.
        highlight_segments(line, &mut styled);

        styled
    }
}

/// Walk through `line` and emit styled segments for `@[...]` file mentions
/// (cyan bold) and `[Image N]` clipboard atoms (magenta bold).
fn highlight_segments(line: &str, styled: &mut StyledText) {
    let mut remaining = line;

    while !remaining.is_empty() {
        let at_pos = remaining.find("@[");
        let img_pos = remaining.find("[Image ");

        let next = match (at_pos, img_pos) {
            (None, None) => {
                styled.push((Style::new(), remaining.to_string()));
                break;
            }
            (Some(a), None) => (a, true),
            (None, Some(i)) => (i, false),
            (Some(a), Some(i)) if a <= i => (a, true),
            (Some(_), Some(i)) => (i, false),
        };

        let (start, is_at_mention) = next;
        if start > 0
            && let Some(before) = remaining.get(..start)
        {
            styled.push((Style::new(), before.to_string()));
        }

        let after = match remaining.get(start..) {
            Some(s) => s,
            None => {
                styled.push((Style::new(), remaining.to_string()));
                break;
            }
        };

        if is_at_mention {
            // `@[...]` — cyan bold (ZSH-compatible: requires closing `]`).
            match after.get(2..).and_then(|s| s.find(']')) {
                None => {
                    styled.push((Style::new(), after.to_string()));
                    break;
                }
                Some(rel_close) => {
                    let close = 2 + rel_close;
                    let mention = after.get(..=close).unwrap_or(after);
                    styled.push((Style::new().bold().fg(Color::Cyan), mention.to_string()));
                    remaining = after.get(close + 1..).unwrap_or("");
                    if remaining.is_empty() {
                        break;
                    }
                }
            }
        } else {
            // `[Image N]` — magenta bold
            match parse_image_atom_len(after) {
                Some(len) => {
                    let atom = after.get(..len).unwrap_or(after);
                    styled.push((Style::new().bold().fg(Color::Magenta), atom.to_string()));
                    remaining = after.get(len..).unwrap_or("");
                    if remaining.is_empty() {
                        break;
                    }
                }
                None => {
                    // Literal `[Image ` that isn't a full atom — emit `[` and continue.
                    styled.push((Style::new(), "[".to_string()));
                    remaining = after.get(1..).unwrap_or("");
                }
            }
        }
    }
}

/// If `s` starts with `[Image N]`, returns the byte length of that atom.
fn parse_image_atom_len(s: &str) -> Option<usize> {
    let rest = s.strip_prefix("[Image ")?;
    let digits_end = rest
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .last()
        .map(|(i, c)| i + c.len_utf8())?;
    if digits_end == 0 {
        return None;
    }
    if !rest[digits_end..].starts_with(']') {
        return None;
    }
    Some("[Image ".len() + digits_end + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(styled: &StyledText) -> String {
        styled.buffer.iter().map(|(_, s)| s.as_str()).collect()
    }

    fn styles(styled: &StyledText) -> Vec<Style> {
        styled.buffer.iter().map(|(style, _)| *style).collect()
    }

    #[test]
    fn test_slash_command_highlighted() {
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight("/compact", 0);
        assert_eq!(render(&actual), "/compact");
        assert_eq!(styles(&actual)[0], Style::new().bold().fg(Color::Yellow));
    }

    #[test]
    fn test_colon_prefix_is_plain_text() {
        // `:` is no longer a command sentinel.
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight(":compact", 0);
        assert_eq!(render(&actual), ":compact");
        assert_eq!(styles(&actual)[0], Style::new());
    }

    #[test]
    fn test_slash_command_with_args() {
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight("/commit some message", 0);
        let parts: Vec<_> = actual
            .buffer
            .iter()
            .map(|(s, t)| (*s, t.as_str()))
            .collect();
        assert_eq!(parts[0], (Style::new().bold().fg(Color::Yellow), "/commit"));
        assert_eq!(parts[1].1, " some message");
    }

    #[test]
    fn test_shell_command_highlighted() {
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight("!ls -la", 0);
        assert_eq!(render(&actual), "!ls -la");
        assert_eq!(styles(&actual)[0], Style::new().fg(Color::Magenta));
    }

    #[test]
    fn test_file_mention_highlighted() {
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight("explain @[src/main.rs] please", 0);
        let parts: Vec<_> = actual
            .buffer
            .iter()
            .map(|(s, t)| (*s, t.as_str()))
            .collect();
        assert_eq!(parts[0], (Style::new(), "explain "));
        assert_eq!(
            parts[1],
            (Style::new().bold().fg(Color::Cyan), "@[src/main.rs]")
        );
        assert_eq!(parts[2], (Style::new(), " please"));
    }

    #[test]
    fn test_image_atom_highlighted() {
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight("see [Image 1] please", 0);
        let parts: Vec<_> = actual
            .buffer
            .iter()
            .map(|(s, t)| (*s, t.as_str()))
            .collect();
        assert_eq!(parts[0], (Style::new(), "see "));
        assert_eq!(
            parts[1],
            (Style::new().bold().fg(Color::Magenta), "[Image 1]")
        );
        assert_eq!(parts[2], (Style::new(), " please"));
    }

    #[test]
    fn test_multiple_file_mentions() {
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight("@[a.rs] and @[b.rs]", 0);
        let texts: Vec<_> = actual.buffer.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(texts, vec!["@[a.rs]", " and ", "@[b.rs]"]);
        assert_eq!(actual.buffer[0].0, Style::new().bold().fg(Color::Cyan));
        assert_eq!(actual.buffer[2].0, Style::new().bold().fg(Color::Cyan));
    }

    #[test]
    fn test_plain_text_unstyled() {
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight("hello world", 0);
        assert_eq!(render(&actual), "hello world");
        assert_eq!(styles(&actual)[0], Style::new());
    }

    #[test]
    fn test_empty_input() {
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight("", 0);
        assert!(actual.buffer.is_empty());
    }

    // --- ZSH parity tests ---

    #[test]
    fn test_file_mention_with_line_range() {
        // @[path:start:end] — same as ZSH pattern, content inside can contain colons
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight("@[src/main.rs:10:20]", 0);
        assert_eq!(render(&actual), "@[src/main.rs:10:20]");
        assert_eq!(styles(&actual)[0], Style::new().bold().fg(Color::Cyan));
    }

    #[test]
    fn test_file_mention_with_symbol() {
        // @[path#symbol] — content inside can contain `#`
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight("check @[src/lib.rs#my_fn] here", 0);
        let parts: Vec<_> = actual
            .buffer
            .iter()
            .map(|(s, t)| (*s, t.as_str()))
            .collect();
        assert_eq!(
            parts[1],
            (Style::new().bold().fg(Color::Cyan), "@[src/lib.rs#my_fn]")
        );
    }

    #[test]
    fn test_unterminated_mention_not_highlighted() {
        // ZSH requires closing `]`, so unterminated tags are plain text.
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight("see @[src/main.rs", 0);
        assert_eq!(render(&actual), "see @[src/main.rs");
        // No cyan segment
        assert!(
            styles(&actual)
                .iter()
                .all(|s| *s != Style::new().bold().fg(Color::Cyan))
        );
    }

    #[test]
    fn test_slash_command_with_mention_in_args() {
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight("/review @[src/main.rs] please", 0);
        let parts: Vec<_> = actual
            .buffer
            .iter()
            .map(|(s, t)| (*s, t.as_str()))
            .collect();
        assert_eq!(parts[0], (Style::new().bold().fg(Color::Yellow), "/review"));
        assert_eq!(parts[1], (Style::new(), " "));
        assert_eq!(
            parts[2],
            (Style::new().bold().fg(Color::Cyan), "@[src/main.rs]")
        );
        assert_eq!(parts[3], (Style::new(), " please"));
    }

    #[test]
    fn test_colon_prefix_with_mention_is_not_command() {
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight(":review @[src/lib.rs]", 0);
        let parts: Vec<_> = actual
            .buffer
            .iter()
            .map(|(s, t)| (*s, t.as_str()))
            .collect();
        // Plain text until the @[...] mention.
        assert_eq!(parts[0], (Style::new(), ":review "));
        assert_eq!(
            parts[1],
            (Style::new().bold().fg(Color::Cyan), "@[src/lib.rs]")
        );
    }

    #[test]
    fn test_bare_at_sign_not_highlighted() {
        // A bare `@word` (no brackets) is plain text — matches ZSH pattern behaviour.
        let fixture = FurcodeHighlighter;
        let actual = fixture.highlight("email@example.com", 0);
        assert_eq!(render(&actual), "email@example.com");
        assert_eq!(styles(&actual)[0], Style::new());
    }
}
