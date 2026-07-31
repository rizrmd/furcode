use std::{fmt, io};

use colored::Colorize;
use furcode_tracker::VERSION;

/// Renders messages into a styled box with border characters.
struct DisplayBox {
    messages: Vec<String>,
}

impl DisplayBox {
    fn new(messages: Vec<String>) -> Self {
        Self { messages }
    }
}

impl fmt::Display for DisplayBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let visible_len = |s: &str| console::measure_text_width(s);
        let width: usize = self
            .messages
            .iter()
            .map(|s| visible_len(s))
            .max()
            .unwrap_or(0)
            + 4;
        let top = format!("┌{}┐", "─".repeat(width.saturating_sub(2)));
        let bottom = format!("└{}┘", "─".repeat(width.saturating_sub(2)));
        let fmt_line = |s: &str| {
            let padding = width.saturating_sub(4).saturating_sub(visible_len(s));
            format!("│ {}{} │", s, " ".repeat(padding))
        };

        writeln!(f, "{}", top)?;
        for msg in &self.messages {
            writeln!(f, "{}", fmt_line(msg))?;
        }
        write!(f, "{}", bottom)
    }
}

/// Displays the Codex-inspired session header for interactive mode.
///
/// Layout goals (inspired by OpenAI Codex CLI):
/// - no large ASCII art mascot
/// - compact product line + version
/// - short, scannable command hints
/// - quiet chrome so conversation is the focus
///
/// # Arguments
///
/// * `cli_mode` - If true, shows a shorter tip set for non-interactive usage.
pub fn display(cli_mode: bool) -> io::Result<()> {
    let product = format!("{} {}", "furcode".bold().white(), format!("v{VERSION}").dimmed());
    println!("{product}");
    println!("{}", "coding agent · terminal session".dimmed());
    println!();

    let tips: Vec<(&str, &str)> = if cli_mode {
        vec![
            ("new session", "/new"),
            ("status", "/info"),
            ("model", "/model"),
            ("provider", "/provider"),
            ("agent", "/agent"),
        ]
    } else {
        vec![
            ("new session", "/new"),
            ("status", "/info · /usage · /help"),
            ("model", "/model"),
            ("agent", "/agent"),
            ("quit", "/exit · Ctrl+D"),
        ]
    };

    let max_width = tips.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, value) in &tips {
        println!(
            "  {}  {}",
            format!("{key:<max_width$}").dimmed(),
            value.cyan()
        );
    }
    println!();

    Ok(())
}
