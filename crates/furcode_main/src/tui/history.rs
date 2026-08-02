//! Dual-tier conversation history for the frame TUI.
//!
//! # Strategy (industry patterns)
//!
//! | Tier | Store | Purpose |
//! |------|--------|---------|
//! | **Long-term** | SQLite `Conversation` | Full agent context (untouched) |
//! | **Archive (slim)** | [`HistoryArchive`] | Display rows only: no tool bodies, no system |
//! | **Working window** | `TurnView.transcript` | Last N user turns on screen |
//!
//! Inspired by LangChain buffer-window / summary-buffer memory, dual-tier
//! working vs long-term memory, and Codex-style main view vs transcript overlay.
//!
//! After extract we **drop** the fat `Conversation` so RSS does not keep the
//! full context graph for the session lifetime.

use std::path::Path;

use furcode_app::utils::format_display_path;
use furcode_domain::{
    ContextMessage, Conversation, Environment, MessageEntry, Role, TextMessage, ToolCallArguments,
    ToolCallFull,
};
use serde_json::Value;

use super::view::{TranscriptKind, TranscriptLine, push_or_collapse_tool};

// ─── Knobs ──────────────────────────────────────────────────────────────────

/// User turns shown on first resume (buffer window).
pub const INITIAL_USER_TURNS: usize = 6;
/// Extra turns loaded per "load more" / scroll-top.
pub const LOAD_MORE_TURNS: usize = 6;
/// Cap assistant prose kept in the slim archive (per message).
const MAX_ASSISTANT_CHARS: usize = 6_000;
const MAX_USER_CHARS: usize = 4_000;
const MAX_LABEL_CHARS: usize = 100;
/// Skip scanning ancient messages when building the slim archive.
const MAX_SCAN_MESSAGES: usize = 300;

// ─── Public types ───────────────────────────────────────────────────────────

/// One user turn + following agent activity (tools / prose), display-only.
#[derive(Debug, Clone)]
pub struct SlimTurn {
    /// One-line label for summaries (user prompt preview).
    pub label: String,
    pub lines: Vec<TranscriptLine>,
}

/// In-memory archive of slim turns. Cheap relative to full `Conversation`.
#[derive(Debug, Clone, Default)]
pub struct HistoryArchive {
    /// Chronological order (oldest first).
    pub turns: Vec<SlimTurn>,
    /// Index of first turn currently shown in the live transcript.
    pub shown_from: usize,
}

/// Result of the initial extract for the TUI.
#[derive(Debug, Clone)]
pub struct HistoryBootstrap {
    pub archive: HistoryArchive,
    /// Rows to put into `TurnView.transcript` now.
    pub visible: Vec<TranscriptLine>,
}

impl HistoryArchive {
    /// Build a slim archive from a full conversation, then the caller should
    /// `drop(conversation)`.
    pub fn from_conversation(conversation: &Conversation, env: &Environment) -> Self {
        let turns = build_slim_turns(conversation, env);
        let shown_from = turns.len().saturating_sub(INITIAL_USER_TURNS);
        Self { turns, shown_from }
    }

    pub fn hidden_turn_count(&self) -> usize {
        self.shown_from
    }

    pub fn has_more(&self) -> bool {
        self.shown_from > 0
    }

    /// Lines for the current working window (plus summary stub if needed).
    pub fn visible_lines(&self) -> Vec<TranscriptLine> {
        let mut out = Vec::new();
        if self.shown_from > 0 {
            out.extend(summary_stub(&self.turns[..self.shown_from]));
        }
        for turn in &self.turns[self.shown_from..] {
            out.extend(turn.lines.iter().cloned());
        }
        out
    }

    /// Reveal older turns; returns lines to **prepend** after the summary
    /// (caller replaces the leading stub + inserts these).
    ///
    /// Returns `(new_visible_prefix, still_has_more)` where prefix is
    /// `stub? + newly_revealed_turns` (does not include turns already shown).
    pub fn load_more(&mut self, n_turns: usize) -> LoadMoreResult {
        if self.shown_from == 0 || n_turns == 0 {
            return LoadMoreResult {
                lines: Vec::new(),
                has_more: false,
            };
        }
        let new_from = self.shown_from.saturating_sub(n_turns);
        let revealed = &self.turns[new_from..self.shown_from];
        self.shown_from = new_from;

        let mut lines = Vec::new();
        if self.shown_from > 0 {
            lines.extend(summary_stub(&self.turns[..self.shown_from]));
        }
        for turn in revealed {
            lines.extend(turn.lines.iter().cloned());
        }
        LoadMoreResult {
            lines,
            has_more: self.shown_from > 0,
        }
    }

    /// Full slim transcript for the history overlay (no tool bodies).
    pub fn overlay_lines(&self) -> Vec<TranscriptLine> {
        let mut out = Vec::new();
        out.push(TranscriptLine {
            kind: TranscriptKind::Info,
            text: format!(
                "Transcript · {} turn(s) · Esc close",
                self.turns.len()
            ),
        });
        out.push(TranscriptLine {
            kind: TranscriptKind::Spacer,
            text: String::new(),
        });
        for turn in &self.turns {
            out.extend(turn.lines.iter().cloned());
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct LoadMoreResult {
    /// Complete replacement for the "hidden" head of the transcript: optional
    /// new stub + newly revealed turns. Already-visible turns stay as-is.
    pub lines: Vec<TranscriptLine>,
    pub has_more: bool,
}

/// One-shot bootstrap used by `run_frame_tui` (full domain graph — avoid if possible).
pub fn bootstrap_from_conversation(
    conversation: &Conversation,
    env: &Environment,
) -> HistoryBootstrap {
    let archive = HistoryArchive::from_conversation(conversation, env);
    let visible = archive.visible_lines();
    HistoryBootstrap { archive, visible }
}

/// Preferred TUI path: parse raw context JSON only — never builds `Conversation`.
///
/// Tool `output.values` stay inside the temporary `Value` tree and are never
/// cloned into transcript rows (only `is_error` / tool call names+args).
pub fn bootstrap_from_context_json(json: &str, env: &Environment) -> HistoryBootstrap {
    let turns = build_slim_turns_from_json(json, env);
    let shown_from = turns.len().saturating_sub(INITIAL_USER_TURNS);
    let archive = HistoryArchive { turns, shown_from };
    let visible = archive.visible_lines();
    HistoryBootstrap { archive, visible }
}

// ─── Summary stub (structural, no LLM) ──────────────────────────────────────

fn summary_stub(hidden: &[SlimTurn]) -> Vec<TranscriptLine> {
    if hidden.is_empty() {
        return Vec::new();
    }
    let n = hidden.len();
    let mut tool_n = 0usize;
    for t in hidden {
        tool_n += t
            .lines
            .iter()
            .filter(|l| {
                matches!(
                    l.kind,
                    TranscriptKind::ToolDone | TranscriptKind::ToolError
                )
            })
            .count();
    }
    // Up to 3 sample labels from oldest→newest hidden prompts.
    let samples: Vec<&str> = hidden
        .iter()
        .map(|t| t.label.as_str())
        .filter(|s| !s.is_empty())
        .take(3)
        .collect();
    let sample = if samples.is_empty() {
        String::new()
    } else {
        format!(" · e.g. {}", samples.join(" · "))
    };

    vec![
        TranscriptLine {
            kind: TranscriptKind::Info,
            text: format!(
                "{n} earlier turn(s) · {tool_n} tool call(s){sample} · PgUp near top to load more"
            ),
        },
        TranscriptLine {
            kind: TranscriptKind::Spacer,
            text: String::new(),
        },
    ]
}

// ─── Slim turn builder (domain) ─────────────────────────────────────────────

fn build_slim_turns(conversation: &Conversation, env: &Environment) -> Vec<SlimTurn> {
    let Some(ctx) = conversation.context.as_ref() else {
        return Vec::new();
    };
    let messages = &ctx.messages;
    if messages.is_empty() {
        return Vec::new();
    }
    let scan_from = messages.len().saturating_sub(MAX_SCAN_MESSAGES);
    build_slim_turns_from_entries(messages[scan_from..].iter().map(|e| &e.message), env)
}

fn build_slim_turns_from_entries<'a>(
    messages: impl Iterator<Item = &'a ContextMessage>,
    env: &Environment,
) -> Vec<SlimTurn> {
    let mut turns: Vec<SlimTurn> = Vec::new();
    let mut cur = SlimTurn {
        label: String::new(),
        lines: Vec::new(),
    };

    for message in messages {
        match message {
            ContextMessage::Text(tm) if tm.role == Role::User => {
                if let Some(prompt) = user_prompt_for_display(tm) {
                    push_user_turn(&mut turns, &mut cur, prompt);
                }
            }
            ContextMessage::Text(tm) if tm.role == Role::Assistant => {
                append_assistant_domain(&mut cur.lines, tm, env);
            }
            ContextMessage::Text(_) => {}
            ContextMessage::Tool(result) if result.is_error() => {
                push_or_collapse_tool(
                    &mut cur.lines,
                    format!("{} failed", tool_name_label(result.name.as_str())),
                    true,
                );
            }
            // Success tool results: already shown via assistant tool_calls.
            ContextMessage::Tool(_) | ContextMessage::Image(_) => {}
        }
    }
    if !cur.lines.is_empty() || !cur.label.is_empty() {
        turns.push(cur);
    }
    turns
}

fn push_user_turn(turns: &mut Vec<SlimTurn>, cur: &mut SlimTurn, prompt: String) {
    if !cur.lines.is_empty() || !cur.label.is_empty() {
        turns.push(std::mem::replace(
            cur,
            SlimTurn {
                label: String::new(),
                lines: Vec::new(),
            },
        ));
    }
    cur.label = one_line_preview(&prompt, 72);
    cur.lines.push(TranscriptLine {
        kind: TranscriptKind::User,
        text: prompt,
    });
    cur.lines.push(TranscriptLine {
        kind: TranscriptKind::Spacer,
        text: String::new(),
    });
}

fn append_assistant_domain(out: &mut Vec<TranscriptLine>, tm: &TextMessage, env: &Environment) {
    if tm.reasoning_details.as_ref().is_some_and(|d| !d.is_empty()) {
        out.push(TranscriptLine {
            kind: TranscriptKind::Thought,
            text: "Thought".into(),
        });
    }
    if let Some(calls) = &tm.tool_calls {
        for call in calls {
            push_or_collapse_tool(out, format_tool_call_fast(call, env), false);
        }
    }
    let content = tm.content.trim();
    if !content.is_empty() {
        out.push(TranscriptLine {
            kind: TranscriptKind::Assistant,
            text: truncate_chars(content, MAX_ASSISTANT_CHARS),
        });
    }
}

// ─── Slim turn builder (raw JSON — preferred for TUI) ───────────────────────

fn build_slim_turns_from_json(json: &str, env: &Environment) -> Vec<SlimTurn> {
    let Ok(root) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let Some(messages) = root.get("messages").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    if messages.is_empty() {
        return Vec::new();
    }

    let scan_from = messages.len().saturating_sub(MAX_SCAN_MESSAGES);
    let mut turns: Vec<SlimTurn> = Vec::new();
    let mut cur = SlimTurn {
        label: String::new(),
        lines: Vec::new(),
    };

    for entry in &messages[scan_from..] {
        // Shape: { "message": { "text": {...} } } | { "message": { "tool": {...} } }
        let msg = entry.get("message").unwrap_or(entry);

        if let Some(tool) = msg.get("tool") {
            // Only check is_error — never touch output.values (can be 90KB+ each).
            let is_err = tool
                .pointer("/output/is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if is_err {
                let name = tool.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                push_or_collapse_tool(
                    &mut cur.lines,
                    format!("{} failed", tool_name_label(name)),
                    true,
                );
            }
            continue;
        }

        let Some(text) = msg.get("text") else {
            continue;
        };
        let role = text.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "System" | "system" => {}
            "User" | "user" => {
                if let Some(prompt) = user_prompt_from_json(text) {
                    push_user_turn(&mut turns, &mut cur, prompt);
                }
            }
            "Assistant" | "assistant" => {
                append_assistant_json(&mut cur.lines, text, env);
            }
            _ => {}
        }
    }
    if !cur.lines.is_empty() || !cur.label.is_empty() {
        turns.push(cur);
    }
    turns
}

fn user_prompt_from_json(text: &Value) -> Option<String> {
    if text.get("droppable").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    // Real typed prompts always carry raw_content.Text (see Event pipeline).
    // Injected "User" rows (summary frames, todo reminders, file watchers) do not.
    if let Some(raw) = text.get("raw_content") {
        if let Some(s) = raw
            .get("Text")
            .or_else(|| raw.get("text"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(truncate_chars(s, MAX_USER_CHARS));
        }
        // Command variant — not a chat prompt
        if raw.get("Command").is_some() || raw.get("command").is_some() {
            return None;
        }
    }
    // Legacy / test messages without raw_content: only show if clearly human text.
    let content = text.get("content").and_then(|c| c.as_str()).unwrap_or("").trim();
    human_user_content(content).map(|s| truncate_chars(&s, MAX_USER_CHARS))
}

fn append_assistant_json(out: &mut Vec<TranscriptLine>, text: &Value, env: &Environment) {
    if text
        .get("reasoning_details")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty())
    {
        out.push(TranscriptLine {
            kind: TranscriptKind::Thought,
            text: "Thought".into(),
        });
    }
    if let Some(calls) = text.get("tool_calls").and_then(|v| v.as_array()) {
        for call in calls {
            let name = call.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
            let args = call.get("arguments");
            push_or_collapse_tool(out, format_tool_from_json(name, args, env), false);
        }
    }
    if let Some(content) = text.get("content").and_then(|c| c.as_str()) {
        let content = content.trim();
        if !content.is_empty() {
            out.push(TranscriptLine {
                kind: TranscriptKind::Assistant,
                text: truncate_chars(content, MAX_ASSISTANT_CHARS),
            });
        }
    }
}

fn format_tool_from_json(name: &str, args: Option<&Value>, env: &Environment) -> String {
    let Some(args) = args else {
        return tool_name_label(name);
    };
    // Fake ToolCallFull-style via shared fast path using borrowed Value.
    let cwd = env.cwd.as_path();
    let path_field = |key: &str| -> String {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(|p| format_display_path(Path::new(p), cwd))
            .unwrap_or_default()
    };
    let str_field = |key: &str| args.get(key).and_then(|v| v.as_str());
    match name {
        "read" => {
            let p = path_field("file_path");
            if p.is_empty() {
                "Read".into()
            } else {
                format!("Read {p}")
            }
        }
        "write" => {
            let p = path_field("file_path");
            let title = if args.get("overwrite").and_then(|v| v.as_bool()) == Some(true) {
                "Overwrite"
            } else {
                "Write"
            };
            if p.is_empty() {
                title.into()
            } else {
                format!("{title} {p}")
            }
        }
        "fs_search" => {
            let pattern = str_field("pattern").unwrap_or("");
            let dir = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(|p| format_display_path(Path::new(p), cwd))
                .unwrap_or_else(|| ".".into());
            format!("Search for '{pattern}' at {dir}")
        }
        "shell" => {
            let subtitle = str_field("description")
                .filter(|s| !s.is_empty())
                .or_else(|| str_field("command"))
                .unwrap_or("");
            let subtitle = truncate_chars(subtitle, MAX_LABEL_CHARS);
            if subtitle.is_empty() {
                "Shell".into()
            } else {
                format!("Shell {subtitle}")
            }
        }
        "todo_write" => {
            let n = args
                .get("todos")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("Update Todos {n} item(s)")
        }
        "todo_read" => "Read Todos".into(),
        "patch" | "multi_patch" => {
            let p = path_field("file_path");
            if p.is_empty() {
                "Replace".into()
            } else {
                format!("Replace {p}")
            }
        }
        other => tool_name_label(other),
    }
}

fn one_line_preview(s: &str, max: usize) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    truncate_chars(line, max)
}

fn user_prompt_for_display(tm: &TextMessage) -> Option<String> {
    if tm.droppable {
        return None;
    }
    // Prefer raw typed text — never the wrapped `<task>…</task>` / system blob.
    if let Some(prompt) = tm
        .raw_content
        .as_ref()
        .and_then(|v| v.as_user_prompt())
        .map(|p| p.as_str().trim())
        .filter(|s| !s.is_empty())
    {
        return Some(truncate_chars(prompt, MAX_USER_CHARS));
    }
    if tm.raw_content.as_ref().and_then(|v| v.as_command()).is_some() {
        return None;
    }
    human_user_content(tm.content.trim()).map(|s| truncate_chars(&s, MAX_USER_CHARS))
}

/// Content that is safe to show as a user turn when `raw_content` is missing.
///
/// Returns `None` for agent-injected "User" role rows (summary frames, todo
/// nags, external file notices, etc.) so resume history only shows prompts
/// the human actually typed.
fn human_user_content(content: &str) -> Option<String> {
    let content = content.trim();
    if content.is_empty() || looks_like_injected_context(content) {
        return None;
    }
    // Prefer inner text of task/feedback wrappers when raw_content is absent.
    if let Some(inner) = extract_tag_inner(content, "task")
        .or_else(|| extract_tag_inner(content, "feedback"))
    {
        let inner = inner.trim();
        if !inner.is_empty() && !looks_like_injected_context(inner) {
            return Some(inner.to_string());
        }
    }
    Some(content.to_string())
}

fn extract_tag_inner<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&close)? + start;
    Some(&s[start..end])
}

fn looks_like_injected_context(s: &str) -> bool {
    let head = s.get(..512.min(s.len())).unwrap_or(s);
    let h = head.to_ascii_lowercase();
    h.contains("<file_content")
        || h.contains("<directory_listing")
        || h.contains("<system_reminder")
        || h.contains("<system_information")
        || h.contains("<system_date") && !h.contains("<task>") && !h.contains("<feedback>")
        || h.contains("<environment")
        || h.contains("<terminal_context")
        || h.contains("<context")
        || h.contains("<instructions")
        || h.contains("<tool_result")
        || h.contains("<furcode_")
        || h.contains("<message")
        || h.contains("<information")
        || h.contains("<critical")
        || h.contains("use the following summary frames")
        || h.contains("summary frames as the authoritative")
        || h.contains("pending todo items that must be completed")
        || h.contains("files have been modified externally")
        || h.contains("authoritative reference for all coding")
        || (s.starts_with('<') && s.len() > 200 && s.contains("</") && !h.contains("<task>") && !h.contains("<feedback>"))
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .nth(max)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    format!("{}…", &s[..end])
}

// ─── Fast tool labels ───────────────────────────────────────────────────────

fn format_tool_call_fast(call: &ToolCallFull, env: &Environment) -> String {
    let name = call.name.as_str();
    let Some(args) = args_object_borrow(&call.arguments) else {
        return tool_name_label(name);
    };
    let cwd = env.cwd.as_path();
    let path_field = |key: &str| -> String {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(|p| format_display_path(Path::new(p), cwd))
            .unwrap_or_default()
    };
    let str_field = |key: &str| args.get(key).and_then(|v| v.as_str());

    match name {
        "read" => {
            let p = path_field("file_path");
            if p.is_empty() {
                "Read".into()
            } else {
                format!("Read {p}")
            }
        }
        "write" => {
            let p = path_field("file_path");
            let title = if args.get("overwrite").and_then(|v| v.as_bool()) == Some(true) {
                "Overwrite"
            } else {
                "Write"
            };
            if p.is_empty() {
                title.into()
            } else {
                format!("{title} {p}")
            }
        }
        "fs_search" => {
            let pattern = str_field("pattern").unwrap_or("");
            let dir = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(|p| format_display_path(Path::new(p), cwd))
                .unwrap_or_else(|| ".".into());
            format!("Search for '{pattern}' at {dir}")
        }
        "sem_search" | "codebase_search" => "Codebase Search".into(),
        "remove" => {
            let p = path_field("path");
            if p.is_empty() {
                "Remove".into()
            } else {
                format!("Remove {p}")
            }
        }
        "patch" | "multi_patch" => {
            let p = path_field("file_path");
            if p.is_empty() {
                "Replace".into()
            } else {
                format!("Replace {p}")
            }
        }
        "undo" => {
            let p = path_field("path");
            if p.is_empty() {
                "Undo".into()
            } else {
                format!("Undo {p}")
            }
        }
        "shell" => {
            let subtitle = str_field("description")
                .filter(|s| !s.is_empty())
                .or_else(|| str_field("command"))
                .unwrap_or("");
            let subtitle = truncate_chars(subtitle, MAX_LABEL_CHARS);
            if subtitle.is_empty() {
                "Shell".into()
            } else {
                format!("Shell {subtitle}")
            }
        }
        "fetch" | "net_fetch" => match str_field("url") {
            Some(url) if !url.is_empty() => format!("GET {url}"),
            _ => "GET".into(),
        },
        "todo_write" => {
            let n = args
                .get("todos")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("Update Todos {n} item(s)")
        }
        "todo_read" => "Read Todos".into(),
        "skill" => match str_field("name") {
            Some(n) if !n.is_empty() => format!("Skill {n}"),
            _ => "Skill".into(),
        },
        "remember" => {
            let t = str_field("title")
                .filter(|s| !s.is_empty())
                .or_else(|| str_field("content"))
                .unwrap_or("");
            let t = truncate_chars(t, MAX_LABEL_CHARS);
            if t.is_empty() {
                "Remember".into()
            } else {
                format!("Remember {t}")
            }
        }
        "task" => match str_field("agent_id") {
            Some(id) if !id.is_empty() => format!("Task {id}"),
            _ => "Task".into(),
        },
        "followup" | "follow_up" => "Follow-up".into(),
        "plan" => "Plan".into(),
        other => tool_name_label(other),
    }
}

fn args_object_borrow(args: &ToolCallArguments) -> Option<&Value> {
    match args {
        ToolCallArguments::Parsed(v) => Some(v),
        ToolCallArguments::Unparsed(_) => None,
    }
}

fn tool_name_label(name: &str) -> String {
    let pretty = name.replace('_', " ");
    let mut chars = pretty.chars();
    match chars.next() {
        Some(c) => format!("{}{}", c.to_uppercase(), chars.as_str()),
        None => name.to_string(),
    }
}

// Silence unused import if MessageEntry only used in signatures via slice.
#[allow(dead_code)]
fn _msg_entry(_: &MessageEntry) {}

#[cfg(test)]
mod tests {
    use furcode_domain::{
        Context, ContextMessage, Conversation, Environment, TextMessage, ToolCallArguments,
        ToolCallFull, ToolName, Role,
    };
    use serde_json::json;

    use super::*;

    fn test_env() -> Environment {
        Environment {
            os: "test".into(),
            cwd: std::path::PathBuf::from("/tmp/proj"),
            home: None,
            shell: "sh".into(),
            base_path: std::path::PathBuf::from("/tmp"),
        }
    }

    #[test]
    fn bootstrap_window_and_stub() {
        let mut ctx = Context::default();
        for i in 0..12 {
            ctx = ctx
                .add_message(ContextMessage::user(format!("u{i}"), None))
                .add_message(ContextMessage::assistant(format!("a{i}"), None, None, None));
        }
        let conv = Conversation::generate().context(ctx);
        let boot = bootstrap_from_conversation(&conv, &test_env());
        assert!(boot.archive.turns.len() >= INITIAL_USER_TURNS);
        assert!(boot.archive.has_more());
        assert!(
            boot.visible
                .iter()
                .any(|l| l.kind == TranscriptKind::Info && l.text.contains("earlier turn"))
        );
        let users: Vec<_> = boot
            .visible
            .iter()
            .filter(|l| l.kind == TranscriptKind::User)
            .collect();
        assert!(users.len() <= INITIAL_USER_TURNS);
        assert_eq!(users.last().unwrap().text, "u11");
    }

    #[test]
    fn bootstrap_from_json_skips_tool_bodies() {
        // Mimic stored shape; huge tool output must not appear in transcript.
        let json = r#"{
          "messages": [
            {"message":{"text":{"role":"User","content":"hello","raw_content":{"Text":"hello"}}}},
            {"message":{"text":{"role":"Assistant","content":"","tool_calls":[{"name":"read","arguments":{"file_path":"/tmp/proj/a.rs"}}]}}},
            {"message":{"tool":{"name":"read","output":{"is_error":false,"values":[{"Text":"HUGE_PAYLOAD_SHOULD_NOT_APPEAR"}]}}}},
            {"message":{"text":{"role":"Assistant","content":"done"}}}
          ]
        }"#;
        let boot = bootstrap_from_context_json(json, &test_env());
        let flat: String = boot
            .visible
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(flat.contains("hello"));
        assert!(flat.contains("Read a.rs") || flat.contains("Read "));
        assert!(flat.contains("done"));
        assert!(!flat.contains("HUGE_PAYLOAD"));
    }

    #[test]
    fn load_more_reveals_older() {
        let mut ctx = Context::default();
        for i in 0..10 {
            ctx = ctx
                .add_message(ContextMessage::user(format!("u{i}"), None))
                .add_message(ContextMessage::assistant("ok", None, None, None));
        }
        let conv = Conversation::generate().context(ctx);
        let mut arch = HistoryArchive::from_conversation(&conv, &test_env());
        let before = arch.shown_from;
        let res = arch.load_more(3);
        assert!(arch.shown_from < before);
        assert!(!res.lines.is_empty());
    }

    #[test]
    fn overlay_has_all_turns() {
        let ctx = Context::default()
            .add_message(ContextMessage::user("a", None))
            .add_message(ContextMessage::user("b", None));
        let conv = Conversation::generate().context(ctx);
        let arch = HistoryArchive::from_conversation(&conv, &test_env());
        let ov = arch.overlay_lines();
        assert!(ov.iter().any(|l| l.text == "a"));
        assert!(ov.iter().any(|l| l.text == "b"));
    }

    #[test]
    fn formats_read() {
        let call = ToolCallFull {
            name: ToolName::new("read"),
            call_id: None,
            arguments: ToolCallArguments::from(json!({"file_path": "/tmp/proj/x.rs"})),
            thought_signature: None,
        };
        assert_eq!(format_tool_call_fast(&call, &test_env()), "Read x.rs");
    }

    #[test]
    fn hides_system() {
        let droppable =
            TextMessage::new(Role::User, "<file_content>x</file_content>").droppable(true);
        let ctx = Context::default()
            .add_message(ContextMessage::system("sys"))
            .add_message(ContextMessage::Text(droppable))
            .add_message(ContextMessage::user("hi", None));
        let conv = Conversation::generate().context(ctx);
        let boot = bootstrap_from_conversation(&conv, &test_env());
        assert!(!boot.visible.iter().any(|l| l.text.contains("sys")));
        assert!(boot.visible.iter().any(|l| l.text == "hi"));
    }

    #[test]
    fn resume_skips_injected_user_role_rows() {
        // Real DB shape: only raw_content.Text is a human prompt; other "User"
        // rows are agent/system injections (summary frames, todo nags).
        let json = r#"{
          "messages": [
            {"message":{"text":{"role":"User","content":"<task>coba buat undo</task>","raw_content":{"Text":"coba buat undo"}}}},
            {"message":{"text":{"role":"User","content":"Use the following summary frames as the authoritative reference for all coding suggestions and decisions."}}},
            {"message":{"text":{"role":"User","content":"Use the following summary frames as the authoritative reference again."}}},
            {"message":{"text":{"role":"Assistant","content":"ok","tool_calls":[]}}},
            {"message":{"text":{"role":"User","content":"<system_reminder>You have pending todo items that must be completed before finishing the task</system_reminder>"}}},
            {"message":{"text":{"role":"User","content":"<feedback>undo gagal</feedback>","raw_content":{"Text":"undo gagal"}}}},
            {"message":{"text":{"role":"User","content":"<information>files modified</information>","droppable":true}}}
          ]
        }"#;
        let boot = bootstrap_from_context_json(json, &test_env());
        let users: Vec<_> = boot
            .visible
            .iter()
            .filter(|l| l.kind == TranscriptKind::User)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(users, vec!["coba buat undo", "undo gagal"]);
        let flat = users.join("\n");
        assert!(!flat.contains("summary frames"));
        assert!(!flat.contains("pending todo"));
        assert!(!flat.contains("information"));
    }
}
