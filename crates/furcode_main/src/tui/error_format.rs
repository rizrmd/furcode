//! Compact, user-facing formatting for provider / transport errors.
//!
//! Raw dumps look like:
//! ```text
//! Retry: POST https://… Caused by: 0: 429 … Reason: {"error":{…huge json…}}
//! ```
//! We extract status, code, and a short human message for the transcript.

use super::view::{TranscriptKind, TranscriptLine};

/// Pretty lines for a fatal stream / chat error.
pub fn format_chat_error(err: &anyhow::Error) -> Vec<TranscriptLine> {
    // Prefer the full chain so JSON buried in causes is still parseable.
    prettify_error_text(&format!("{err:#}"))
}

/// Pretty lines for a retry attempt (same heuristics as fatal errors).
pub fn format_retry_cause(cause: &str) -> Vec<TranscriptLine> {
    let lines = prettify_error_text(cause);
    // Prefix title with "Retry · " when it is an error-style title.
    lines
        .into_iter()
        .enumerate()
        .map(|(i, mut line)| {
            if i == 0 && line.kind == TranscriptKind::Info || line.kind == TranscriptKind::ToolError
            {
                // Retry rows stay Info (dim diamond), not red tool-error.
                line.kind = TranscriptKind::Info;
                if !line.text.starts_with("Retry") {
                    line.text = format!("Retry · {}", strip_error_prefix(&line.text));
                }
            }
            line
        })
        .collect()
}

fn strip_error_prefix(s: &str) -> &str {
    s.strip_prefix("Error · ")
        .or_else(|| s.strip_prefix("Error: "))
        .unwrap_or(s)
}

fn prettify_error_text(raw: &str) -> Vec<TranscriptLine> {
    let raw = raw.trim();
    if raw.is_empty() {
        return vec![line(
            TranscriptKind::ToolError,
            "Error · unknown failure".into(),
        )];
    }

    // ── Known structured cases ──────────────────────────────────────────
    if let Some(lines) = try_context_length(raw) {
        return lines;
    }
    if let Some(lines) = try_rate_limit(raw) {
        return lines;
    }
    if let Some(lines) = try_http_json_message(raw) {
        return lines;
    }
    if let Some(lines) = try_transport(raw) {
        return lines;
    }

    // ── Generic fallback: short title + one clean detail ────────────────
    let title = generic_title(raw);
    let detail = generic_detail(raw);
    let mut out = vec![line(TranscriptKind::ToolError, title)];
    if let Some(d) = detail {
        out.push(line(TranscriptKind::ErrorBody, d));
    }
    out
}

fn line(kind: TranscriptKind, text: String) -> TranscriptLine {
    TranscriptLine { kind, text }
}

// ─── Detectors ──────────────────────────────────────────────────────────────

fn try_context_length(raw: &str) -> Option<Vec<TranscriptLine>> {
    let lower = raw.to_ascii_lowercase();
    if !(lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || (lower.contains("context length") && lower.contains("tokens")))
    {
        return None;
    }
    let mut out = vec![line(
        TranscriptKind::ToolError,
        "Error · context length exceeded".into(),
    )];
    // Best-effort: "maximum context length is 131072" / "request has 143481"
    let limit = capture_number_after(raw, &["maximum context length is ", "context length is "]);
    let used = capture_number_after(
        raw,
        &[
            "request has ",
            "your request has ",
            "However, your request has ",
        ],
    );
    match (limit, used) {
        (Some(lim), Some(use_n)) => {
            let over = use_n.saturating_sub(lim);
            out.push(line(
                TranscriptKind::ErrorBody,
                format!(
                    "Limit {} · used {} (+{} over)",
                    fmt_int(lim),
                    fmt_int(use_n),
                    fmt_int(over)
                ),
            ));
        }
        (Some(lim), None) => {
            out.push(line(
                TranscriptKind::ErrorBody,
                format!("Model limit {} tokens", fmt_int(lim)),
            ));
        }
        _ => {}
    }
    out.push(line(
        TranscriptKind::ErrorBody,
        "Shorten history (/compact) or start a new conversation, then retry.".into(),
    ));
    Some(out)
}

fn try_rate_limit(raw: &str) -> Option<Vec<TranscriptLine>> {
    let lower = raw.to_ascii_lowercase();
    let is_429 = lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("rate-limited")
        || lower.contains("rate limited")
        || lower.contains("rate_limit");
    if !is_429 {
        return None;
    }

    let mut out = vec![line(
        TranscriptKind::ToolError,
        "Error · rate limited (429)".into(),
    )];

    if let Some(msg) = extract_json_message(raw).or_else(|| extract_raw_metadata_message(raw)) {
        let short = first_sentence(&msg, 120);
        if !short.is_empty() {
            out.push(line(TranscriptKind::ErrorBody, short));
        }
    } else if lower.contains("temporarily") {
        out.push(line(
            TranscriptKind::ErrorBody,
            "Upstream provider is temporarily rate-limited. Retrying shortly.".into(),
        ));
    } else {
        out.push(line(
            TranscriptKind::ErrorBody,
            "Too many requests. Wait a moment or switch provider/key.".into(),
        ));
    }
    Some(out)
}

fn try_http_json_message(raw: &str) -> Option<Vec<TranscriptLine>> {
    let status = capture_http_status(raw);
    let msg = extract_json_message(raw)?;
    let code = extract_json_code(raw);

    let title = match (status, code.as_deref()) {
        (Some(s), Some(c)) => format!("Error · {s} ({c})"),
        (Some(s), None) => format!("Error · HTTP {s}"),
        (None, Some(c)) => format!("Error · {c}"),
        (None, None) => "Error · request failed".into(),
    };

    let mut out = vec![line(TranscriptKind::ToolError, title)];
    let short = first_sentence(&msg, 140);
    if !short.is_empty() {
        out.push(line(TranscriptKind::ErrorBody, short));
    }
    Some(out)
}

fn try_transport(raw: &str) -> Option<Vec<TranscriptLine>> {
    let lower = raw.to_ascii_lowercase();
    let kind = if lower.contains("http2") || lower.contains("stream error") {
        Some(("connection failed", "HTTP/2 stream error talking to the provider."))
    } else if lower.contains("error sending request") || lower.contains("sendrequest") {
        Some(("connection failed", "Could not send request to the provider."))
    } else if lower.contains("connection reset") || lower.contains("broken pipe") {
        Some(("connection lost", "Network connection dropped mid-request."))
    } else if lower.contains("timed out") || lower.contains("timeout") {
        Some(("timeout", "The provider did not respond in time."))
    } else if lower.contains("dns") || lower.contains("name or service not known") {
        Some(("DNS failed", "Could not resolve the provider host."))
    } else {
        None
    }?;

    Some(vec![
        line(TranscriptKind::ToolError, format!("Error · {}", kind.0)),
        line(TranscriptKind::ErrorBody, kind.1.into()),
    ])
}

fn generic_title(raw: &str) -> String {
    // Prefer first non-empty line, strip "Retry: POST …" boilerplate.
    let first = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("unknown failure");

    let mut t = first.to_string();
    if let Some(rest) = t.strip_prefix("Retry: ") {
        t = rest.to_string();
    }
    // Drop long URLs in title.
    if t.contains("http://") || t.contains("https://") {
        if t.contains("POST ") {
            return "Error · request failed".into();
        }
        t = "Error · request failed".into();
        return t;
    }
    // "LLM provider error: …" → keep short head
    if let Some(rest) = t.strip_prefix("LLM provider error: ") {
        let short = first_sentence(rest, 80);
        return format!("Error · {short}");
    }
    format!("Error · {}", first_sentence(&t, 80))
}

fn generic_detail(raw: &str) -> Option<String> {
    // Prefer JSON message if any.
    if let Some(msg) = extract_json_message(raw) {
        let s = first_sentence(&msg, 140);
        if !s.is_empty() {
            return Some(s);
        }
    }
    // First "Caused by" leaf that is not pure JSON.
    for line in raw.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('{') {
            continue;
        }
        if l.starts_with("Caused by") || l.starts_with("0:") || l.starts_with("1:") {
            let cleaned = l
                .trim_start_matches(|c: char| c.is_ascii_digit() || c == ':' || c.is_whitespace())
                .trim();
            // Skip if it's mostly JSON.
            if cleaned.starts_with('{') {
                continue;
            }
            let s = first_sentence(cleaned, 140);
            if s.len() > 12 {
                return Some(s);
            }
        }
    }
    None
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn fmt_int(n: u64) -> String {
    // 131072 → 131,072
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

fn capture_number_after(raw: &str, needles: &[&str]) -> Option<u64> {
    let lower = raw.to_ascii_lowercase();
    for n in needles {
        let n_low = n.to_ascii_lowercase();
        if let Some(pos) = lower.find(&n_low) {
            let after = &raw[pos + n.len()..];
            let digits: String = after
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(v) = digits.parse::<u64>() {
                return Some(v);
            }
        }
    }
    None
}

fn capture_http_status(raw: &str) -> Option<u16> {
    // "status 400" / "HTTP 429" / "Invalid Status Code: 429"
    for needle in ["status ", "Status Code: ", "HTTP ", "status code "] {
        if let Some(pos) = raw.find(needle) {
            let after = &raw[pos + needle.len()..];
            let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(v) = digits.parse::<u16>()
                && (100..600).contains(&v)
            {
                return Some(v);
            }
        }
    }
    // Bare "429 Too Many"
    for code in [429u16, 401, 403, 404, 500, 502, 503, 400] {
        if raw.contains(&code.to_string()) {
            // Avoid matching tokens like 131072
            let pat = format!("{code} ");
            if raw.contains(&pat) || raw.contains(&format!(": {code}")) {
                return Some(code);
            }
        }
    }
    None
}

/// Pull `error.message` from the first JSON object found in `raw`.
fn extract_json_message(raw: &str) -> Option<String> {
    let v = find_json_value(raw)?;
    // OpenAI / OpenRouter shape: { "error": { "message": "..." } }
    if let Some(msg) = v
        .pointer("/error/message")
        .and_then(|m| m.as_str())
        .map(str::to_string)
    {
        return Some(msg);
    }
    if let Some(msg) = v.get("message").and_then(|m| m.as_str()).map(str::to_string) {
        return Some(msg);
    }
    // Nested metadata.raw sometimes holds plain text
    if let Some(raw_s) = v
        .pointer("/error/metadata/raw")
        .and_then(|m| m.as_str())
        .map(str::to_string)
    {
        return Some(raw_s);
    }
    None
}

fn extract_json_code(raw: &str) -> Option<String> {
    let v = find_json_value(raw)?;
    if let Some(c) = v.pointer("/error/code") {
        if let Some(s) = c.as_str() {
            return Some(s.to_string());
        }
        if let Some(n) = c.as_u64() {
            return Some(n.to_string());
        }
    }
    v.get("code").and_then(|c| {
        c.as_str()
            .map(str::to_string)
            .or_else(|| c.as_u64().map(|n| n.to_string()))
    })
}

/// OpenRouter sometimes embeds the useful sentence in metadata.raw outside
/// well-formed JSON after a broken parse — also try a simple regex-like scan.
fn extract_raw_metadata_message(raw: &str) -> Option<String> {
    // "raw":"poolside/… free is temporarily rate-limited…"
    const MARKER: &str = "\"raw\":\"";
    let pos = raw.find(MARKER)?;
    let after = &raw[pos + MARKER.len()..];
    let end = after.find('"')?;
    let s = &after[..end];
    // Drop leading "provider/model:" prefix if present
    let s = s
        .rsplit_once(':')
        .map(|(_, rest)| rest.trim())
        .unwrap_or(s)
        .trim();
    if s.len() < 8 {
        return None;
    }
    Some(s.to_string())
}

fn find_json_value(raw: &str) -> Option<serde_json::Value> {
    // Scan for `{` and try parse progressively shorter / longer slices.
    let bytes = raw.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'{' {
            continue;
        }
        // Try from this brace to end, then back off.
        let slice = &raw[i..];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) {
            return Some(v);
        }
        // Balanced-brace extract
        if let Some(end) = find_balanced_json_end(slice)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&slice[..=end])
        {
            return Some(v);
        }
    }
    None
}

fn find_balanced_json_end(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, c) in s.char_indices() {
        if in_str {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn first_sentence(s: &str, max: usize) -> String {
    let s = s.trim();
    // Cut at first newline or sentence end after 40 chars.
    let cut = s
        .find('\n')
        .into_iter()
        .chain(
            s.char_indices()
                .filter(|(i, c)| *i > 40 && matches!(c, '.' | '!' | '?'))
                .map(|(i, _)| i + 1),
        )
        .next()
        .unwrap_or(s.len());
    let piece = s[..cut.min(s.len())].trim();
    if piece.chars().count() <= max {
        piece.to_string()
    } else {
        let t: String = piece.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_429_is_compact() {
        let raw = r#"POST https://openrouter.ai/api/v1/chat/completions

Caused by:
    0: 429 Too Many Requests Reason: {"error":{"message":"Provider returned error","code":429,"metadata":{"raw":"poolside/laguna-s-2.1:free is temporarily rate-limited upstream. Please retry shortly, or add your own key to accumulate your rate limits: https://openrouter.ai/settings/integrations","provider_name":"Poolside"}},"user_id":"user_xxx"}
    1: Invalid Status Code: 429"#;
        let lines = format_retry_cause(raw);
        assert!(
            lines[0].text.contains("rate limited") || lines[0].text.contains("429"),
            "title={:?}",
            lines[0].text
        );
        assert!(lines[0].text.starts_with("Retry"));
        // No raw JSON dump, no user_id.
        let joined = lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.contains("user_id"));
        assert!(!joined.contains("Caused by"));
        assert!(joined.chars().count() < 280, "too long: {joined}");
    }

    #[test]
    fn context_length_parses_counts() {
        let raw = r#"Error: 0: LLM provider error: Request failed with status 400 Bad Request: {"error":{"message":"This model's maximum context length is 131072 tokens. However, your request has 143481 input tokens (136339 of text input). Please reduce the length.","type":"invalid_request_error","code":"context_length_exceeded"},"status":400}"#;
        let lines = format_chat_error(&anyhow::anyhow!(raw.to_string()));
        assert!(lines[0].text.contains("context length"));
        let body = lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(body.contains("131,072") || body.contains("131072"));
        assert!(body.contains("143,481") || body.contains("143481"));
        assert!(!body.contains("\"error\""));
    }

    #[test]
    fn transport_http2() {
        let raw = "error sending request for url (https://x)\nclient error (SendRequest)\nhttp2 error\nstream error received: unspecific protocol error detected";
        let lines = prettify_error_text(raw);
        assert!(lines[0].text.contains("connection"), "{:?}", lines[0].text);
    }
}
