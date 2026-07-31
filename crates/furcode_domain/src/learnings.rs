//! Helpers for durable harness learnings stored in user config.

/// Header written when creating a new learnings file.
pub const LEARNINGS_FILE_HEADER: &str = "# Furcode Learnings\n\n\
<!-- Durable harness improvements. Newest sections are appended at the end. \
Loaded into the agent system prompt at runtime — no recompile required. -->\n";

/// Formats a single learnings section for append.
///
/// # Arguments
/// * `date` - ISO date string (`YYYY-MM-DD`)
/// * `title` - Optional short section title
/// * `content` - Rule or note body (trimmed)
pub fn format_learning_section(date: &str, title: Option<&str>, content: &str) -> String {
    let body = content.trim();
    match title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(title) => format!("\n## {date} — {title}\n{body}\n"),
        None => format!("\n## {date}\n{body}\n"),
    }
}

/// Builds the full file content after appending a section.
///
/// Creates a header when `existing` is empty. Ensures a blank line before the
/// new section when appending to existing content.
pub fn append_learning_content(existing: &str, section: &str) -> String {
    let section = section.trim_start_matches('\n');
    if existing.trim().is_empty() {
        format!("{LEARNINGS_FILE_HEADER}{section}")
    } else {
        let mut out = existing.trim_end().to_string();
        out.push_str("\n\n");
        out.push_str(section);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }
}

/// Returns true when `content` already appears (case-insensitive, collapsed
/// whitespace) inside `existing`.
pub fn is_near_duplicate(existing: &str, content: &str) -> bool {
    let needle = normalize_for_dedupe(content);
    if needle.is_empty() {
        return true;
    }
    let haystack = normalize_for_dedupe(existing);
    haystack.contains(&needle)
}

fn normalize_for_dedupe(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Wraps learnings body for system-prompt injection, truncating to `max_chars`.
///
/// When truncating, keeps the **end** of the file so newer appends remain.
pub fn wrap_learnings_for_prompt(body: &str, max_chars: usize) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let truncated = if max_chars == 0 || trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let start = trimmed
            .char_indices()
            .map(|(i, _)| i)
            .nth(trimmed.chars().count().saturating_sub(max_chars))
            .unwrap_or(0);
        format!("…(truncated)\n{}", &trimmed[start..])
    };

    format!(
        "## Durable harness learnings (user config)\n\
These rules were saved to the user config and apply across sessions. Follow them.\n\n{truncated}"
    )
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_format_learning_section_with_title() {
        let actual = format_learning_section("2026-07-30", Some("tool calling"), "- Use native tools");
        let expected = "\n## 2026-07-30 — tool calling\n- Use native tools\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_format_learning_section_without_title() {
        let actual = format_learning_section("2026-07-30", None, "Prefer parallel reads");
        let expected = "\n## 2026-07-30\nPrefer parallel reads\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_append_learning_content_creates_header() {
        let section = format_learning_section("2026-07-30", Some("a"), "rule");
        let actual = append_learning_content("", &section);
        assert!(actual.starts_with("# Furcode Learnings"));
        assert!(actual.contains("## 2026-07-30 — a"));
        assert!(actual.contains("rule"));
    }

    #[test]
    fn test_append_learning_content_appends() {
        let fixture = "# Furcode Learnings\n\n## old\nold rule\n";
        let section = format_learning_section("2026-07-30", Some("new"), "new rule");
        let actual = append_learning_content(fixture, &section);
        let expected = "# Furcode Learnings\n\n## old\nold rule\n\n## 2026-07-30 — new\nnew rule\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_is_near_duplicate() {
        let existing = "## 2026-07-30\nPrefer parallel  reads\n";
        assert!(is_near_duplicate(existing, "prefer parallel reads"));
        assert!(!is_near_duplicate(existing, "never invent tool XML"));
    }

    #[test]
    fn test_wrap_learnings_for_prompt_truncates_from_start() {
        let body = "AAAA\nBBBB\nCCCC";
        let actual = wrap_learnings_for_prompt(body, 8);
        assert!(actual.contains("…(truncated)"));
        assert!(actual.contains("CCCC") || actual.ends_with("CCCC") || actual.contains("BBBB"));
        assert!(actual.contains("Durable harness learnings"));
    }

    #[test]
    fn test_wrap_learnings_empty() {
        let actual = wrap_learnings_for_prompt("  \n", 100);
        assert_eq!(actual, "");
    }
}
