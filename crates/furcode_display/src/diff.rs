use console::{Style, style};
use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone)]
pub struct DiffResult {
    result: String,
    lines_added: u64,
    lines_removed: u64,
}

impl DiffResult {
    pub fn diff(&self) -> &str {
        &self.result
    }

    pub fn lines_added(&self) -> u64 {
        self.lines_added
    }

    pub fn lines_removed(&self) -> u64 {
        self.lines_removed
    }
}

pub struct DiffFormat;

impl DiffFormat {
    /// Format a line-oriented unified-style diff for terminal and TUI display.
    ///
    /// Layout (readable without color):
    /// ```text
    ///   1 │ context line
    /// - 2 │ removed line
    /// + 2 │ added line
    /// ···
    /// ```
    ///
    /// - Single line-number column (old for deletes, new for inserts/context)
    /// - Explicit `+` / `-` sign with spacing so signs are not glued to text
    /// - Green inserts, red deletes, dim context (ANSI; TUI re-colors after strip)
    pub fn format(old: &str, new: &str) -> DiffResult {
        let diff = TextDiff::from_lines(old, new);
        let ops = diff.grouped_ops(3);
        let mut output = String::new();

        let mut lines_added = 0u64;
        let mut lines_removed = 0u64;

        if ops.is_empty() {
            output.push_str(&format!("{}\n", style("No changes applied").dim()));
            return DiffResult {
                result: output,
                lines_added,
                lines_removed,
            };
        }

        // Width from line numbers that actually appear in the hunks.
        let mut max_line_number = 0usize;
        for group in &ops {
            for op in group {
                for change in diff.iter_inline_changes(op) {
                    if let Some(old_idx) = change.old_index() {
                        max_line_number = max_line_number.max(old_idx + 1);
                    }
                    if let Some(new_idx) = change.new_index() {
                        max_line_number = max_line_number.max(new_idx + 1);
                    }
                }
            }
        }
        let width = if max_line_number == 0 {
            1
        } else {
            (max_line_number as f64).log10().floor() as usize + 1
        };

        for (idx, group) in ops.iter().enumerate() {
            if idx > 0 {
                output.push_str(&format!("{}\n", style("···").dim()));
            }
            for op in group {
                for change in diff.iter_inline_changes(op) {
                    let (sign, line_no, s) = match change.tag() {
                        ChangeTag::Delete => {
                            lines_removed += 1;
                            (
                                "-",
                                change.old_index().map(|i| i + 1),
                                Style::new().red(),
                            )
                        }
                        ChangeTag::Insert => {
                            lines_added += 1;
                            (
                                "+",
                                change.new_index().map(|i| i + 1),
                                Style::new().green(),
                            )
                        }
                        ChangeTag::Equal => (
                            " ",
                            change
                                .new_index()
                                .or_else(|| change.old_index())
                                .map(|i| i + 1),
                            Style::new().dim(),
                        ),
                    };

                    let num = match line_no {
                        Some(n) => format!("{n:>width$}"),
                        None => format!("{:width$}", ""),
                    };

                    // "± N │ content" — sign column is always 1 char + space.
                    output.push_str(&format!(
                        "{} {} │ ",
                        s.apply_to(sign),
                        style(num).dim(),
                    ));

                    let content: String = change
                        .iter_strings_lossy()
                        .map(|(_, value)| value)
                        .collect();

                    let trimmed = content.trim_end_matches('\n');
                    if trimmed.contains('\n') {
                        let first_line = trimmed.split('\n').next().unwrap_or("");
                        output.push_str(first_line);
                        output.push_str("···");
                    } else {
                        for (_, value) in change.iter_strings_lossy() {
                            output.push_str(&format!("{}", s.apply_to(value)));
                        }
                    }

                    output.push('\n');
                }
            }
        }

        DiffResult {
            result: output,
            lines_added,
            lines_removed,
        }
    }
}

#[cfg(test)]
mod tests {
    use console::strip_ansi_codes;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_color_output() {
        let old = "Hello World\nThis is a test\nThird line\nFourth line";
        let new = "Hello World\nThis is a modified test\nNew line\nFourth line";
        let diff = DiffFormat::format(old, new);
        let diff_str = diff.diff();
        assert_eq!(diff.lines_added(), 2);
        assert_eq!(diff.lines_removed(), 2);
        eprintln!("\nColor Output Test:\n{diff_str}");
    }

    #[test]
    fn test_diff_printer_no_differences() {
        let content = "line 1\nline 2\nline 3";
        let diff = DiffFormat::format(content, content);
        assert_eq!(diff.lines_added(), 0);
        assert_eq!(diff.lines_removed(), 0);
        assert!(diff.diff().contains("No changes applied"));
    }

    #[test]
    fn test_file_source() {
        let old = "line 1\nline 2\nline 3\nline 4\nline 5";
        let new = "line 1\nline 2\nline 3";
        let diff = DiffFormat::format(old, new);
        let clean_diff = strip_ansi_codes(diff.diff());
        // Truncating the last two lines leaves trailing equal "line 3" as insert
        // after three deletes — counts from similar's line split.
        assert_eq!(diff.lines_added(), 1);
        assert_eq!(diff.lines_removed(), 3);
        assert_snapshot!(clean_diff);
    }

    #[test]
    fn test_diff_printer_simple_diff() {
        let old = "line 1\nline 2\nline 3\nline 5\nline 6\nline 7\nline 8\nline 9";
        let new = "line 1\nmodified line\nline 3\nline 5\nline 6\nline 7\nline 8\nline 9";
        let diff = DiffFormat::format(old, new);
        let clean_diff = strip_ansi_codes(diff.diff());
        assert_eq!(diff.lines_added(), 1);
        assert_eq!(diff.lines_removed(), 1);
        assert!(clean_diff.contains("- 2 │ line 2"));
        assert!(clean_diff.contains("+ 2 │ modified line"));
        assert_snapshot!(clean_diff);
    }

    #[test]
    fn test_dynamic_width_with_large_line_numbers() {
        let old_lines = (1..=150).map(|i| format!("line {i}")).collect::<Vec<_>>();
        let mut new_lines = old_lines.clone();
        new_lines[99] = "modified line 100".to_string();

        let old = old_lines.join("\n");
        let new = new_lines.join("\n");
        let diff = DiffFormat::format(&old, &new);
        let clean_diff = strip_ansi_codes(diff.diff());

        assert!(clean_diff.contains("100"));
        assert!(clean_diff.contains("- 100 │ line 100") || clean_diff.contains("-100 │ line 100"));
        assert_eq!(diff.lines_added(), 1);
        assert_eq!(diff.lines_removed(), 1);
    }

    #[test]
    fn test_width_based_on_diff_not_file_size() {
        // Large file but diff only at the beginning
        let old_lines = (1..=1000).map(|i| format!("line {i}")).collect::<Vec<_>>();
        let mut new_lines = old_lines.clone();
        new_lines[4] = "modified line 5".to_string();

        let old = old_lines.join("\n");
        let new = new_lines.join("\n");
        let diff = DiffFormat::format(&old, &new);
        let clean_diff = strip_ansi_codes(diff.diff());

        // Diff only shows nearby context, so width is 1 digit — not 4 for line 1000.
        assert!(clean_diff.contains("  3 │ line 3"));
        assert!(clean_diff.contains("- 5 │ line 5"));
        assert!(clean_diff.contains("+ 5 │ modified line 5"));
        assert_eq!(diff.lines_added(), 1);
        assert_eq!(diff.lines_removed(), 1);
        assert_snapshot!(clean_diff);
    }

    #[test]
    fn test_plain_format_has_space_after_sign() {
        let old = "a\nb\nc";
        let new = "a\nB\nc";
        let clean = strip_ansi_codes(DiffFormat::format(old, new).diff()).to_string();
        // Sign must not be glued to the line number or content.
        assert!(clean.lines().any(|l| l.starts_with("- ")));
        assert!(clean.lines().any(|l| l.starts_with("+ ")));
        assert!(!clean.contains("|-"));
        assert!(!clean.contains("|+"));
    }
}
