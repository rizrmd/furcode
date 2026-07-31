//! Frame-based interactive TUI (Codex-style layout).
//!
//! Enable with `FURCODE_TUI=1` (or `FURCODE_TUI=true`). The legacy line-oriented
//! UI remains the default until this path is promoted.
//!
//! ## Architecture
//!
//! - [`view`] — pure view model (`TurnView`, composer, activity phase)
//! - [`apply`] — `ChatResponse` → `TurnView`
//! - [`render`] — ratatui draw of one frame
//! - [`session`] — crossterm draw loop + host action channel

mod apply;
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
pub use session::{SessionOutcome, TuiSession, UserAction, join_session, start_session};
pub use view::{
    ActivityPhase, ComposerState, ForkItem, ForkPicker, PendingPrompt, TranscriptKind,
    TranscriptLine, TranscriptOverlay, TurnView, push_or_collapse_tool,
};

/// Returns true when the new frame TUI should be used for interactive sessions.
pub fn enabled() -> bool {
    match std::env::var("FURCODE_TUI") {
        Ok(v) => {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
        }
        Err(_) => false,
    }
}
