use std::sync::Arc;

use furcode_select::FurcodeWidget;

use crate::completer::input_completer::InputSuggestion;
use crate::completer::search_term::Span;
use crate::model::{FurcodeCommand, FurcodeCommandManager};

/// A display wrapper for `FurcodeCommand` that renders the name and description
/// side-by-side for the interactive picker.
#[derive(Clone)]
struct CommandRow(FurcodeCommand);

impl std::fmt::Display for CommandRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:<28} {}", self.0.name, self.0.description)
    }
}

#[derive(Clone)]
pub struct CommandCompleter(Arc<FurcodeCommandManager>);

impl CommandCompleter {
    pub fn new(command_manager: Arc<FurcodeCommandManager>) -> Self {
        Self(command_manager)
    }
}

/// Opens the interactive command picker for `/` slash commands.
///
/// Returns the selected command display name (including `/`), or `None` if the
/// user dismisses the popup (Esc / cancel).
///
/// # Arguments
/// * `manager` - Command registry
/// * `initial_query` - Pre-filled filter text (without the leading `/`)
pub fn pick_command(manager: &FurcodeCommandManager, initial_query: &str) -> Option<String> {
    let commands: Vec<CommandRow> = manager
        .list()
        .into_iter()
        .map(|cmd| {
            let display_name = if cmd.name.starts_with('!') {
                cmd.name.clone()
            } else {
                format!("/{}", cmd.name)
            };
            CommandRow(FurcodeCommand {
                name: display_name,
                description: cmd.description,
                value: cmd.value,
            })
        })
        .collect();

    if commands.is_empty() {
        return None;
    }

    let mut builder = FurcodeWidget::select("Command", commands)
        .with_help_message("type to filter · ↑↓ / mouse scroll · Enter select · Esc dismiss");
    if !initial_query.is_empty() {
        builder = builder.with_initial_text(initial_query);
    }

    match builder.prompt() {
        Ok(Some(row)) => Some(row.0.name),
        _ => None,
    }
}

pub fn pick_file_command(manager: &FurcodeCommandManager, initial_query: &str) -> Option<String> {
    let popular: Vec<&'static str> = vec![
        "new",
        "compact",
        "model",
        "config-model",
        "config",
        "info",
        "usage",
        "tools",
        "skill",
        "learn",
        "commit",
        "commit-preview",
        "suggest",
        "edit",
        "clone",
        "conversation-rename",
        "copy",
        "conversation",
        "conversation-tree",
        "workspace-sync",
        "workspace-status",
        "workspace-info",
        "activity",
        "help",
        "forge",
        "muse",
        "sage",
        "index",
        "exit",
    ];

    let lookup: std::collections::HashSet<_> = popular.iter().copied().collect();

    let commands: Vec<CommandRow> = manager
        .list()
        .into_iter()
        .filter(|cmd| lookup.contains(cmd.name.as_str()))
        .map(|cmd| {
            let display_name = if cmd.name.starts_with('!') {
                cmd.name.clone()
            } else {
                format!("/{}", cmd.name)
            };
            CommandRow(FurcodeCommand {
                name: display_name,
                description: cmd.description,
                value: cmd.value,
            })
        })
        .collect();

    if commands.is_empty() {
        return None;
    }

    let mut builder = FurcodeWidget::select("Command", commands)
        .with_help_message("type to filter · ↑↓ / mouse scroll · Enter select · Esc dismiss");
    if !initial_query.is_empty() {
        builder = builder.with_initial_text(initial_query);
    }

    match builder.prompt() {
        Ok(Some(row)) => Some(row.0.name),
        _ => None,
    }
}

impl CommandCompleter {
    /// Completes a slash command via the interactive picker (Tab path).
    pub fn complete(&mut self, line: &str, _: usize) -> Vec<InputSuggestion> {
        let initial_query = line.strip_prefix('/').unwrap_or(line);

        match pick_command(&self.0, initial_query) {
            Some(name) => vec![InputSuggestion {
                value: name,
                span: Span::new(0, line.len()),
                append_whitespace: true,
            }],
            None => vec![],
        }
    }
}
