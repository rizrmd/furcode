use std::fmt;

use colored::Colorize;
use furcode_domain::{Category, TitleFormat};

/// Compact live-activity line (Codex-style), without timestamps.
///
/// Examples:
/// - `● Read src/main.rs` (tool)
/// - `◆ Thought for 2.1s` (meta / info)
/// - `● ERROR: …` (failure)
pub struct TitleDisplay {
    inner: TitleFormat,
    with_colors: bool,
}

impl TitleDisplay {
    pub fn new(title: TitleFormat) -> Self {
        Self { inner: title, with_colors: true }
    }

    pub fn with_colors(mut self, with_colors: bool) -> Self {
        self.with_colors = with_colors;
        self
    }

    fn format_with_colors(&self) -> String {
        let (icon, body) = self.parts_colored();
        format!("{icon} {body}")
    }

    fn format_plain(&self) -> String {
        let (icon, body) = self.parts_plain();
        format!("{icon} {body}")
    }

    fn body_text(&self) -> String {
        // Tool titles are already verb phrases ("Read", "Update Todos") — no
        // extra "Run " prefix. Strip a legacy prefix if a caller still sends one.
        let mut body = self
            .inner
            .title
            .strip_prefix("Run ")
            .unwrap_or(self.inner.title.as_str())
            .to_string();
        if let Some(ref sub) = self.inner.sub_title {
            let sub = sub.trim();
            if !sub.is_empty() {
                body.push(' ');
                body.push_str(sub);
            }
        }
        body
    }

    fn parts_colored(&self) -> (String, String) {
        let body = self.body_text();
        match self.inner.category {
            Category::Debug => {
                // Successful / in-progress tool activity
                ("●".green().to_string(), body.dimmed().to_string())
            }
            Category::Completion => {
                ("●".green().to_string(), body.dimmed().to_string())
            }
            Category::Action => {
                ("●".yellow().to_string(), body.white().to_string())
            }
            Category::Info => {
                // Meta: thoughts, session notes
                ("◆".dimmed().to_string(), body.dimmed().to_string())
            }
            Category::Error => {
                ("●".red().to_string(), body.red().to_string())
            }
            Category::Warning => {
                (
                    "◆".bright_yellow().to_string(),
                    body.bright_yellow().to_string(),
                )
            }
        }
    }

    fn parts_plain(&self) -> (String, String) {
        let body = self.body_text();
        let icon = match self.inner.category {
            Category::Debug | Category::Completion | Category::Action => "●",
            Category::Error => "●",
            Category::Info | Category::Warning => "◆",
        };
        (icon.to_string(), body)
    }
}

impl fmt::Display for TitleDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.with_colors {
            write!(f, "{}", self.format_with_colors())
        } else {
            write!(f, "{}", self.format_plain())
        }
    }
}

/// Extension trait to easily convert TitleFormat to displayable form
pub trait TitleDisplayExt {
    fn display(self) -> TitleDisplay;
    fn display_with_colors(self, with_colors: bool) -> TitleDisplay;
}

impl TitleDisplayExt for TitleFormat {
    fn display(self) -> TitleDisplay {
        TitleDisplay::new(self)
    }

    fn display_with_colors(self, with_colors: bool) -> TitleDisplay {
        TitleDisplay::new(self).with_colors(with_colors)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_compact_tool_line_no_timestamp() {
        let actual = TitleFormat::debug("Read")
            .sub_title("src/main.rs")
            .display_with_colors(false)
            .to_string();
        let expected = "● Read src/main.rs";
        assert_eq!(actual, expected);
        assert!(!actual.contains('['));
    }

    #[test]
    fn test_info_uses_diamond() {
        let actual = TitleFormat::info("Thought for 2.1s")
            .display_with_colors(false)
            .to_string();
        let expected = "◆ Thought for 2.1s";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_legacy_run_prefix_stripped() {
        let actual = TitleFormat::debug("Run Shell")
            .sub_title("ls")
            .display_with_colors(false)
            .to_string();
        let expected = "● Shell ls";
        assert_eq!(actual, expected);
    }
}
