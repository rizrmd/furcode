use std::path::{Path, PathBuf};
use std::sync::Arc;

use furcode_select::{FurcodeWidget, PreviewLayout, PreviewPlacement, SelectRow};
use furcode_walker::Walker;

use crate::completer::CommandCompleter;
use crate::completer::search_term::{SearchTerm, Span};
use crate::model::FurcodeCommandManager;

pub fn select_workspace_file(cwd: &Path, query: Option<String>) -> anyhow::Result<Option<String>> {
    let files: Vec<String> = Walker::max_all()
        .cwd(cwd.to_path_buf())
        .get_blocking()
        .unwrap_or_default()
        .into_iter()
        .map(|file| file.path)
        .collect();

    if files.is_empty() {
        return Ok(None);
    }

    let has_bat = std::process::Command::new("bat")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();
    let cat_cmd = if has_bat {
        "bat --color=always --style=numbers,changes --line-range=:500"
    } else {
        "cat"
    };

    let preview_cmd = format!(
        "if [ -d {{}} ]; then ls -la --color=always {{}} 2>/dev/null || ls -la {{}}; else {cat_cmd} {{}}; fi"
    );
    let rows: Vec<SelectRow> = files
        .into_iter()
        .map(|path| SelectRow {
            raw: path.clone(),
            display: path.clone(),
            search: path.clone(),
            fields: vec![path],
        })
        .collect();

    Ok(FurcodeWidget::select_rows("File ❯ ", rows)
        .query(Some(query.unwrap_or_default()))
        .preview(Some(preview_cmd))
        .preview_layout(PreviewLayout { placement: PreviewPlacement::Bottom, percent: 75 })
        .prompt()?
        .map(|row| row.raw))
}

pub struct InputCompleter {
    cwd: PathBuf,
    command: CommandCompleter,
}

pub struct InputSuggestion {
    pub value: String,
    pub span: Span,
    pub append_whitespace: bool,
}

impl InputCompleter {
    pub fn new(cwd: PathBuf, command_manager: Arc<FurcodeCommandManager>) -> Self {
        Self { cwd, command: CommandCompleter::new(command_manager) }
    }

    pub fn complete(&mut self, line: &str, pos: usize) -> Vec<InputSuggestion> {
        if line.starts_with('/') {
            // Slash command — open the interactive command popup.
            let result = self.command.complete(line, pos);
            if !result.is_empty() {
                return result;
            }
        }

        if let Some(query) = SearchTerm::new(line, pos).process() {
            let initial_text = if !query.term.is_empty() {
                Some(query.term.to_string())
            } else {
                None
            };

            // If the @-mention term starts with `/`, treat it as a slash command
            // instead of a file search. This avoids surfacing every workspace
            // file when the user types `@[/`.
            if query.term.starts_with('/') {
                let result = self.command.complete(query.term, query.span.end);
                if !result.is_empty() {
                    return result;
                }
            }

            if let Ok(Some(selected)) = select_workspace_file(&self.cwd, initial_text) {
                let value = format!("[{}]", selected);
                return vec![InputSuggestion {
                    value,
                    span: Span::new(query.span.start, query.span.end),
                    append_whitespace: true,
                }];
            }
        }

        vec![]
    }
}
