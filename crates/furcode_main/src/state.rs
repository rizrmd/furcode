use std::path::PathBuf;

use derive_setters::Setters;
use furcode_api::{ConversationId, Environment, ModelId};

//TODO: UIState and FurcodePrompt seem like the same thing and can be merged
/// State information for the UI
#[derive(Debug, Default, Clone, Setters)]
#[setters(strip_option)]
pub struct UIState {
    pub cwd: PathBuf,
    pub conversation_id: Option<ConversationId>,
    /// Cached context window (tokens) for the active model — drives context %.
    pub context_length: Option<u64>,
    /// Model id the `context_length` cache was resolved for.
    pub context_length_model: Option<ModelId>,
}

impl UIState {
    pub fn new(env: Environment) -> Self {
        Self {
            cwd: env.cwd,
            conversation_id: Default::default(),
            context_length: None,
            context_length_model: None,
        }
    }
}
