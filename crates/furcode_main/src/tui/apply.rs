//! Map domain [`ChatResponse`] events into [`TurnView`] (compact semantics).

use furcode_domain::{ChatResponse, ChatResponseContent, TitleFormat};

use super::view::{ActivityPhase, TranscriptKind, TurnView};

/// Apply one stream event to the view (compact / frame-TUI path).
///
/// Side effects that must run for protocol correctness (e.g. tool start
/// notifier) are handled here.
pub fn apply_chat_response(view: &mut TurnView, message: ChatResponse) {
    if message.is_empty() {
        return;
    }
    match message {
        ChatResponse::TaskReasoning { content } => {
            view.push_reasoning(&content);
        }
        ChatResponse::TaskMessage { content } => match content {
            ChatResponseContent::ToolInput(title) => {
                let label = title_live_label(&title);
                view.begin_tool(label);
            }
            ChatResponseContent::ToolOutput(text) => {
                view.push_tool_output(&text);
            }
            ChatResponseContent::Markdown { text, partial: _ } => {
                // One Thought summary (if leaving thinking), then accumulate all
                // markdown tokens into a single assistant row — never re-emit
                // Thought between tokens.
                view.append_assistant(&text);
            }
        },
        ChatResponse::ToolCallStart { notifier, .. } => {
            // Always release the tool executor — frame UI does not stream shell
            // into a separate spinner path.
            notifier.notify_one();
        }
        ChatResponse::ToolCallEnd(result) => {
            if result.is_error() {
                // Prefer a short human detail; drop protocol noise like
                // `<tool_call_error>` so the row stays "Search … failed · 0.2s".
                let detail = result
                    .output
                    .as_str()
                    .and_then(tool_error_detail_for_display);
                view.finish_tool(detail);
            } else {
                view.finish_tool(None);
            }
        }
        ChatResponse::RetryAttempt { cause, .. } => {
            // Compact: "Retry · rate limited (429)" + short hint — not raw JSON.
            for line in super::error_format::format_retry_cause(cause.as_str()) {
                view.push_transcript(line);
            }
        }
        ChatResponse::Interrupt { reason } => {
            view.push_transcript(super::view::TranscriptLine {
                kind: TranscriptKind::Info,
                text: reason.user_message(),
            });
            view.mark_idle();
        }
        ChatResponse::TaskComplete => {
            // Model finished without tools — never go idle silently.
            view.push_transcript(super::view::TranscriptLine {
                kind: TranscriptKind::Info,
                text: "Turn complete · model finished (no more tools) · send a message to continue"
                    .into(),
            });
            view.mark_idle();
        }
    }
}

/// Short, user-facing tool failure detail — or `None` to show only "failed".
fn tool_error_detail_for_display(raw: &str) -> Option<String> {
    let one = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .trim();
    if one.is_empty() {
        return None;
    }
    let lower = one.to_ascii_lowercase();
    // Internal / protocol placeholders — not useful in the transcript.
    if lower == "<tool_call_error>"
        || lower == "tool_call_error"
        || lower.contains("<tool_call_error")
        || lower == "<error>"
        || lower == "error"
        || lower.starts_with("<tool_") && lower.ends_with('>')
    {
        return None;
    }
    let t: String = one.chars().take(80).collect();
    if one.chars().count() > 80 {
        Some(format!("{t}…"))
    } else {
        Some(t)
    }
}

/// Plain tool label for the frame TUI (no legacy "Run " prefix).
fn title_live_label(title: &TitleFormat) -> String {
    let mut body = title.title.clone();
    // Strip a leading "Run " if the title generator already added one.
    if let Some(rest) = body.strip_prefix("Run ") {
        body = rest.to_string();
    }
    if let Some(sub) = title
        .sub_title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        body.push(' ');
        body.push_str(sub);
    }
    body
}
