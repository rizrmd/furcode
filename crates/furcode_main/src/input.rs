use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use furcode_api::Environment;

use crate::editor::{FurcodeEditor, ReadResult};
use crate::model::{AppCommand, FurcodeCommandManager};
use crate::prompt::FurcodePrompt;
use crate::tracker;

/// Console implementation for handling user input via command line.
pub struct Console {
    command: Arc<FurcodeCommandManager>,
    editor: Mutex<FurcodeEditor>,
}

impl Console {
    /// Creates a new instance of `Console`.
    pub fn new(
        env: Environment,
        custom_history_path: Option<PathBuf>,
        command: Arc<FurcodeCommandManager>,
    ) -> Self {
        let editor = Mutex::new(FurcodeEditor::new(env, custom_history_path, command.clone()));
        Self { command, editor }
    }
}

/// Outcome of a console prompt interaction.
pub enum PromptOutcome {
    /// Parsed slash command or free-form message.
    Command(AppCommand),
    /// User double-tapped Esc — open previous-message fork UI.
    ForkHistory,
}

impl Console {
    pub async fn prompt(&self, prompt: &mut FurcodePrompt) -> anyhow::Result<PromptOutcome> {
        loop {
            let mut furcode_editor = self.editor.lock().unwrap();
            let user_input = furcode_editor.prompt(prompt)?;

            drop(furcode_editor);
            match user_input {
                ReadResult::Continue => continue,
                ReadResult::Exit => return Ok(PromptOutcome::Command(AppCommand::Exit)),
                ReadResult::Empty => continue,
                ReadResult::ForkHistory => return Ok(PromptOutcome::ForkHistory),
                ReadResult::Success(text) => {
                    tracker::prompt(text.clone());
                    return Ok(PromptOutcome::Command(self.command.parse(&text)?));
                }
            }
        }
    }

    /// Sets the buffer content for the next prompt
    pub fn set_buffer(&self, content: String) {
        let mut editor = self.editor.lock().unwrap();
        editor.set_buffer(content);
    }
}
