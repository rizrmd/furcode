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
                let detail = result
                    .output
                    .as_str()
                    .map(|s| {
                        let one = s.lines().find(|l| !l.trim().is_empty()).unwrap_or(s);
                        let t: String = one.chars().take(80).collect();
                        if one.chars().count() > 80 {
                            format!("{t}…")
                        } else {
                            t
                        }
                    })
                    .filter(|s| !s.is_empty());
                view.finish_tool(detail);
            } else {
                view.finish_tool(None);
            }
        }
        ChatResponse::RetryAttempt { cause, .. } => {
            view.push_transcript(super::view::TranscriptLine {
                kind: TranscriptKind::Info,
                text: format!("Retry: {}", cause.as_str()),
            });
        }
        ChatResponse::Interrupt { reason } => {
            let title = match reason {
                furcode_domain::InterruptionReason::MaxRequestPerTurnLimitReached { limit } => {
                    format!("Maximum request ({limit}) per turn achieved")
                }
                furcode_domain::InterruptionReason::MaxToolFailurePerTurnLimitReached {
                    limit,
                    ..
                } => {
                    format!("Maximum tool failure limit ({limit}) reached for this turn")
                }
            };
            view.push_transcript(super::view::TranscriptLine {
                kind: TranscriptKind::Info,
                text: title,
            });
            view.mark_idle();
        }
        ChatResponse::TaskComplete => {
            view.mark_idle();
        }
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
