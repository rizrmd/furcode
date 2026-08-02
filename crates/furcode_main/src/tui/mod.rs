//! Frame-based interactive TUI (Codex-style layout).
//!
//! This is the **default and only** interactive UI. Non-interactive one-shots
//! (`-p`, piped input, subcommands) still use the line/spinner path.
//!
//! ## Architecture
//!
//! - [`view`] — pure view model (`TurnView`, composer, activity phase)
//! - [`apply`] — `ChatResponse` → `TurnView`
//! - [`render`] — ratatui draw of one frame
//! - [`session`] — crossterm draw loop + host action channel

mod apply;
pub(crate) mod error_format;
mod history;
mod markdown;
mod render;
mod session;
mod view;

pub use apply::apply_chat_response;
pub use history::{
    HistoryArchive, HistoryBootstrap, LOAD_MORE_TURNS, bootstrap_from_context_json,
    bootstrap_from_conversation,
};
pub use session::{
    SessionOutcome, TuiSession, TuiSuspendGuard, UserAction, join_session, start_session,
};
pub use view::{
    ActivityPhase, CommandItem, CommandPicker, ComposerState, ForkItem, ForkPicker, PendingPrompt,
    StatusTone, TranscriptKind, TranscriptLine, TranscriptOverlay, TurnView, push_or_collapse_tool,
};

/// Interactive sessions always use the frame TUI.
///
/// Kept for call sites / docs; always returns `true`. The old `FURCODE_TUI=1`
/// opt-in and line-oriented interactive REPL have been removed.
pub fn enabled() -> bool {
    true
}
