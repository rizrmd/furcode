use std::time::Duration;

/// Formats a chrono DateTime as a human-readable relative time string (e.g., "5
/// minutes ago").
pub fn humanize_time(dt: chrono::DateTime<chrono::Utc>) -> String {
    let duration = chrono::Utc::now().signed_duration_since(dt);
    let duration = Duration::from_secs((duration.num_minutes() * 60).max(0) as u64);
    if duration.is_zero() {
        "now".to_string()
    } else {
        format!("{} ago", humantime::format_duration(duration))
    }
}

/// Humanizes a number to a readable format with B/M/k suffixes.
///
/// # Arguments
/// * `n` - The number to humanize
///
/// # Examples
/// ```ignore
/// assert_eq!(humanize_number(1500), "1.5k");
/// assert_eq!(humanize_number(1500000), "1.5M");
/// assert_eq!(humanize_number(1500000000), "1.5B");
/// assert_eq!(humanize_number(500), "500");
/// ```
pub fn humanize_number(n: usize) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.1}B", n as f64 / 1_000_000_000.0),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => n.to_string(),
    }
}

/// Fixed 5-character context-window fill percentage for status chrome.
///
/// # Examples
/// - `0.65%` when under 10%
/// - `12.3%` when under 100%
/// - ` 100%` when full
///
/// Returns `None` when the limit is zero (unknown window).
pub fn format_context_percent(used: usize, limit: u64) -> Option<String> {
    if limit == 0 {
        return None;
    }
    let pct = (used as f64 / limit as f64 * 100.0).clamp(0.0, 100.0);
    let s = if pct < 10.0 {
        // Always 5 chars: "0.00%" .. "9.99%"
        format!("{pct:.2}%")
    } else if pct < 100.0 {
        // Always 5 chars: "10.0%" .. "99.9%"
        format!("{pct:4.1}%")
    } else {
        // Always 5 chars
        " 100%".to_string()
    };
    Some(s)
}

/// One-character block gauge for context fill (`░` … `█`).
///
/// Empty is `░` (never a space) so the indicator stays visible on dark chrome.
pub fn context_fill_block(ratio: f64) -> char {
    const BLOCKS: [char; 9] = ['░', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let idx = (ratio.clamp(0.0, 1.0) * 8.0).round() as usize;
    BLOCKS[idx.min(8)]
}

/// Compact context-usage indicator: one fill glyph + 5-char percent (e.g. `█0.65%`).
///
/// Returns `None` when the limit is zero.
pub fn format_context_usage(used: usize, limit: u64) -> Option<String> {
    let pct = format_context_percent(used, limit)?;
    let ratio = used as f64 / limit as f64;
    Some(format!("{}{}", context_fill_block(ratio), pct))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_humanize_number_billions() {
        let actual = humanize_number(1_500_000_000);
        let expected = "1.5B";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_humanize_number_millions() {
        let actual = humanize_number(2_300_000);
        let expected = "2.3M";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_humanize_number_thousands() {
        let actual = humanize_number(4_500);
        let expected = "4.5k";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_humanize_number_small() {
        let actual = humanize_number(999);
        let expected = "999";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_humanize_number_zero() {
        let actual = humanize_number(0);
        let expected = "0";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_format_context_percent_under_ten() {
        // 832 / 128_000 ≈ 0.65%
        let actual = format_context_percent(832, 128_000);
        let expected = Some("0.65%".to_string());
        assert_eq!(actual, expected);
        assert_eq!(actual.as_ref().map(|s| s.len()), Some(5));
    }

    #[test]
    fn test_format_context_percent_mid() {
        let actual = format_context_percent(16_000, 128_000);
        let expected = Some("12.5%".to_string());
        assert_eq!(actual, expected);
        assert_eq!(actual.as_ref().map(|s| s.len()), Some(5));
    }

    #[test]
    fn test_format_context_percent_full() {
        let actual = format_context_percent(128_000, 128_000);
        let expected = Some(" 100%".to_string());
        assert_eq!(actual, expected);
        assert_eq!(actual.as_ref().map(|s| s.len()), Some(5));
    }

    #[test]
    fn test_format_context_percent_zero_limit() {
        let actual = format_context_percent(100, 0);
        assert_eq!(actual, None);
    }

    #[test]
    fn test_format_context_usage_includes_block() {
        let actual = format_context_usage(64_000, 128_000).unwrap();
        // Half-full block + "50.0%"
        assert!(actual.ends_with("50.0%"), "got {actual}");
        assert_eq!(actual.chars().count(), 6);
    }

    #[test]
    fn test_context_fill_block_empty_and_full() {
        assert_eq!(context_fill_block(0.0), '░');
        assert_eq!(context_fill_block(1.0), '█');
    }
}
