use async_trait::async_trait;
use furcode_domain::{ContextMessage, Conversation, EndPayload, EventData, EventHandle, Role};
use furcode_template::Element;

use crate::TemplateEngine;

/// Unique substring used to detect a prior premature-completion reminder.
/// Kept stable so old conversations still de-dupe correctly.
const REMINDER_MARKER: &str = "You stopped without a user-visible response after using tools";

/// Alternate marker in the current template (also counts as already-reminded).
const REMINDER_MARKER_V2: &str = "You stopped before finishing the user's task";

/// Max chars for a final reply that can still count as "intention only".
const INTENTION_MAX_CHARS: usize = 480;

/// Detects premature turn completion after tools were used this turn:
/// - stop with empty / whitespace-only content, or
/// - stop with only a short "I'll / Let me …" promise and no follow-up tools
///
/// Injects **one** system reminder so the orchestrator continues. A second
/// similar stop is allowed through (anti-loop).
///
/// Complements [`super::PendingTodosHandler`].
#[derive(Debug, Clone, Default)]
pub struct PrematureCompletionHandler;

impl PrematureCompletionHandler {
    /// Creates a new premature-completion handler.
    pub fn new() -> Self {
        Self
    }

    /// Whether the conversation already contains this reminder.
    fn already_reminded(conversation: &Conversation) -> bool {
        conversation
            .context
            .as_ref()
            .map(|ctx| {
                ctx.messages.iter().any(|entry| {
                    entry.message.content().is_some_and(|c| {
                        c.contains(REMINDER_MARKER) || c.contains(REMINDER_MARKER_V2)
                    })
                })
            })
            .unwrap_or(false)
    }

    /// True when content is empty or whitespace-only (not user-visible).
    fn is_empty_content(content: &str) -> bool {
        content.trim().is_empty()
    }

    /// True when the assistant message issued tool calls.
    fn has_tool_calls(msg: &furcode_domain::TextMessage) -> bool {
        msg.tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty())
    }

    /// Short final prose that only promises future action (common flash-model stop).
    ///
    /// Examples that match: "I'll make a targeted edit…", "Let me fix that next".
    /// Examples that do not: long status reports, "I've updated X. Tests pass."
    fn looks_like_intention_only(content: &str) -> bool {
        let t = content.trim();
        if t.is_empty() || t.chars().count() > INTENTION_MAX_CHARS {
            return false;
        }
        let lower = t.to_ascii_lowercase();
        // Drop markdown wrappers for matching.
        let lower = lower.trim_start_matches(['#', '*', '_', '`', '>', ' ']);

        let starts_as_promise = lower.starts_with("i'll ")
            || lower.starts_with("i will ")
            || lower.starts_with("i'm going to ")
            || lower.starts_with("i am going to ")
            || lower.starts_with("let me ")
            || lower.starts_with("let's ")
            || lower.starts_with("next i'll ")
            || lower.starts_with("next i will ")
            || lower.starts_with("now i'll ")
            || lower.starts_with("now i will ")
            || lower.starts_with("i plan to ")
            || lower.starts_with("i need to ")
            || lower.starts_with("i should ");

        if !starts_as_promise {
            return false;
        }

        // Past-tense / completion cues — treat as a real status, not a promise.
        let completed = lower.contains("i've ")
            || lower.contains("i have ")
            || lower.contains("done.")
            || lower.contains("done!")
            || lower.contains("fixed")
            || lower.contains("updated")
            || lower.contains("implemented")
            || lower.contains("here's ")
            || lower.contains("here is ")
            || lower.contains("tests pass")
            || lower.contains("all set");

        !completed
    }

    /// Final assistant stop looks premature given tools already ran this turn.
    fn is_premature_final(content: &str) -> bool {
        Self::is_empty_content(content) || Self::looks_like_intention_only(content)
    }

    /// Returns true if we should inject a continue reminder.
    ///
    /// Conditions (all required):
    /// - Last assistant message has no tool calls
    /// - Content is empty **or** intention-only ("I'll…")
    /// - Tools were used earlier in this user turn
    /// - We have not already injected this reminder (anti-loop)
    fn should_nudge(conversation: &Conversation) -> bool {
        if Self::already_reminded(conversation) {
            return false;
        }

        let Some(context) = conversation.context.as_ref() else {
            return false;
        };

        let mut saw_premature_final = false;
        let mut tools_in_turn = false;

        for entry in context.messages.iter().rev() {
            match &entry.message {
                ContextMessage::Text(text) => match text.role {
                    Role::Assistant => {
                        if !saw_premature_final {
                            if Self::has_tool_calls(text) {
                                return false;
                            }
                            if !Self::is_premature_final(&text.content) {
                                return false;
                            }
                            saw_premature_final = true;
                        } else if Self::has_tool_calls(text) {
                            tools_in_turn = true;
                        }
                    }
                    Role::User => {
                        let content = text.content.as_str();
                        if content.contains("<system_reminder")
                            || content.contains("system_reminder")
                            || content.contains(REMINDER_MARKER)
                            || content.contains(REMINDER_MARKER_V2)
                            || content.contains("You have pending todo items")
                        {
                            continue;
                        }
                        break;
                    }
                    Role::System => continue,
                },
                ContextMessage::Tool(_) => {
                    if saw_premature_final {
                        tools_in_turn = true;
                    }
                }
                ContextMessage::Image(_) => continue,
            }
        }

        saw_premature_final && tools_in_turn
    }
}

#[async_trait]
impl EventHandle<EventData<EndPayload>> for PrematureCompletionHandler {
    async fn handle(
        &self,
        _event: &EventData<EndPayload>,
        conversation: &mut Conversation,
    ) -> anyhow::Result<()> {
        if !Self::should_nudge(conversation) {
            return Ok(());
        }

        let reminder = TemplateEngine::default().render(
            "furcode-premature-completion-reminder.md",
            &serde_json::json!({}),
        )?;

        if let Some(context) = conversation.context.as_mut() {
            let content = Element::new("system_reminder").text(reminder);
            context
                .messages
                .push(ContextMessage::user(content, None).into());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use furcode_domain::{
        Agent, Context, Conversation, EndPayload, EventData, EventHandle, MessageEntry, ModelId,
        Role, TextMessage, ToolCallArguments, ToolCallFull, ToolOutput, ToolResult,
    };
    use pretty_assertions::assert_eq;

    use super::*;

    fn fixture_agent() -> Agent {
        Agent::new(
            "test-agent",
            "test-provider".to_string().into(),
            ModelId::new("test-model"),
        )
    }

    fn fixture_event() -> EventData<EndPayload> {
        EventData::new(fixture_agent(), ModelId::new("test-model"), EndPayload)
    }

    fn assistant(content: &str, tools: Option<Vec<ToolCallFull>>) -> TextMessage {
        TextMessage {
            role: Role::Assistant,
            content: content.to_string(),
            raw_content: None,
            tool_calls: tools,
            thought_signature: None,
            model: None,
            reasoning_details: None,
            droppable: false,
            phase: None,
        }
    }

    fn user(content: &str) -> TextMessage {
        TextMessage::new(Role::User, content)
    }

    fn tool_call(name: &str) -> ToolCallFull {
        ToolCallFull::new(name).arguments(ToolCallArguments::from_json(r#"{"path":"x"}"#))
    }

    fn tool_result(name: &str) -> ToolResult {
        ToolResult::new(name).output(Ok(ToolOutput::text("ok")))
    }

    fn conversation_from(messages: Vec<ContextMessage>) -> Conversation {
        let mut conversation = Conversation::generate();
        let entries: Vec<MessageEntry> = messages.into_iter().map(Into::into).collect();
        conversation.context = Some(Context::default().messages(entries));
        conversation
    }

    fn last_content(conversation: &Conversation) -> String {
        conversation
            .context
            .as_ref()
            .unwrap()
            .messages
            .last()
            .unwrap()
            .message
            .content()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn empty_stop_after_tools_injects_reminder() {
        let handler = PrematureCompletionHandler::new();
        let event = fixture_event();
        let mut conversation = conversation_from(vec![
            ContextMessage::Text(user("implement auto update")),
            ContextMessage::Text(assistant(
                "",
                Some(vec![tool_call("fs_read"), tool_call("shell")]),
            )),
            ContextMessage::Tool(tool_result("fs_read")),
            ContextMessage::Text(assistant("", None)),
        ]);

        handler.handle(&event, &mut conversation).await.unwrap();

        let msgs = &conversation.context.as_ref().unwrap().messages;
        assert_eq!(msgs.len(), 5);
        let last = last_content(&conversation);
        assert!(
            last.contains(REMINDER_MARKER) || last.contains(REMINDER_MARKER_V2),
            "{last}"
        );
    }

    #[tokio::test]
    async fn intention_only_stop_after_tools_injects_reminder() {
        // Screenshot case: tools ran, then "I'll make a targeted edit…" and stop.
        let handler = PrematureCompletionHandler::new();
        let event = fixture_event();
        let mut conversation = conversation_from(vec![
            ContextMessage::Text(user("fix multiline diff")),
            ContextMessage::Text(assistant("", Some(vec![tool_call("fs_read")]))),
            ContextMessage::Tool(tool_result("fs_read")),
            ContextMessage::Text(assistant(
                "I'll make a targeted edit to collapse multiline inline changes into one rendered line ending with …",
                None,
            )),
        ]);

        handler.handle(&event, &mut conversation).await.unwrap();
        let last = last_content(&conversation);
        assert!(
            last.contains(REMINDER_MARKER_V2) || last.contains(REMINDER_MARKER),
            "{last}"
        );
    }

    #[tokio::test]
    async fn second_empty_stop_does_not_duplicate() {
        let handler = PrematureCompletionHandler::new();
        let event = fixture_event();
        let mut conversation = conversation_from(vec![
            ContextMessage::Text(user("implement auto update")),
            ContextMessage::Text(assistant("", Some(vec![tool_call("fs_read")]))),
            ContextMessage::Tool(tool_result("fs_read")),
            ContextMessage::Text(assistant("", None)),
        ]);

        handler.handle(&event, &mut conversation).await.unwrap();
        let after_first = conversation.context.as_ref().unwrap().messages.len();
        assert_eq!(after_first, 5);

        conversation
            .context
            .as_mut()
            .unwrap()
            .messages
            .push(ContextMessage::Text(assistant("", None)).into());

        handler.handle(&event, &mut conversation).await.unwrap();
        let after_second = conversation.context.as_ref().unwrap().messages.len();
        assert_eq!(after_second, 6);
    }

    #[tokio::test]
    async fn substantive_status_does_not_nudge() {
        let handler = PrematureCompletionHandler::new();
        let event = fixture_event();
        let mut conversation = conversation_from(vec![
            ContextMessage::Text(user("implement auto update")),
            ContextMessage::Text(assistant("", Some(vec![tool_call("fs_read")]))),
            ContextMessage::Tool(tool_result("fs_read")),
            ContextMessage::Text(assistant(
                "I've updated the formatter to collapse multiline diffs. Tests pass.",
                None,
            )),
        ]);

        let before = conversation.context.as_ref().unwrap().messages.len();
        handler.handle(&event, &mut conversation).await.unwrap();
        let after = conversation.context.as_ref().unwrap().messages.len();
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn empty_stop_without_tools_does_not_nudge() {
        let handler = PrematureCompletionHandler::new();
        let event = fixture_event();
        let mut conversation = conversation_from(vec![
            ContextMessage::Text(user("hello")),
            ContextMessage::Text(assistant("", None)),
        ]);

        let before = conversation.context.as_ref().unwrap().messages.len();
        handler.handle(&event, &mut conversation).await.unwrap();
        let after = conversation.context.as_ref().unwrap().messages.len();
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn whitespace_only_counts_as_empty() {
        let handler = PrematureCompletionHandler::new();
        let event = fixture_event();
        let mut conversation = conversation_from(vec![
            ContextMessage::Text(user("fix the bug")),
            ContextMessage::Text(assistant("", Some(vec![tool_call("fs_read")]))),
            ContextMessage::Tool(tool_result("fs_read")),
            ContextMessage::Text(assistant("   \n\t  ", None)),
        ]);

        handler.handle(&event, &mut conversation).await.unwrap();
        let last = last_content(&conversation);
        assert!(last.contains(REMINDER_MARKER) || last.contains(REMINDER_MARKER_V2));
    }

    #[test]
    fn intention_detector_matches_screenshot_prose() {
        assert!(PrematureCompletionHandler::looks_like_intention_only(
            "I'll make a targeted edit to collapse multiline inline changes into one rendered line ending with …, while preserving line counting and sign/width behavior."
        ));
        assert!(!PrematureCompletionHandler::looks_like_intention_only(
            "I've updated the formatter. Tests pass."
        ));
        assert!(!PrematureCompletionHandler::looks_like_intention_only(""));
    }
}
