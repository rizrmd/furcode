use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use colored::Colorize;
use console::style;
use convert_case::{Case, Casing};
use furcode_api::{
    API, AgentId, AnyProvider, ApiKeyRequest, AuthContextRequest, AuthContextResponse, ChatRequest,
    ChatResponse, CodeRequest, ConfigOperation, Conversation, ConversationId, DeviceCodeRequest,
    Event, InterruptionReason, ModelId, Provider, ProviderId, TextMessage, UserPrompt,
};
use furcode_app::utils::{format_display_path, truncate_key};
use furcode_app::{CommitResult, ToolResolver};
use furcode_config::{ActivityDisplay, FurcodeConfig};
use furcode_display::MarkdownFormat;
use furcode_domain::{
    AuthMethod, ChatResponseContent, ConsoleWriter, ContextMessage, Role, TitleFormat, UserCommand,
};
use furcode_fs::FurcodeFS;
use furcode_select::{FurcodeWidget, SelectRow};
use furcode_spinner::SpinnerManager;
use furcode_tracker::ToolCallPayload;
use furcode_walker::Walker;
use futures::future;
use strum::IntoEnumIterator;
use tokio_stream::StreamExt;
use url::Url;

use crate::cli::{
    Cli, CommitCommandGroup, ConversationCommand, ListCommand, McpCommand, SelectCommand,
    TopLevelCommand,
};
use crate::conversation_selector::ConversationSelector;
use crate::display_constants::{CommandType, headers, markers, status};
use crate::editor::ReadLineError;
use crate::error::UIError;
use crate::info::Info;
use crate::input::Console;
use crate::model::{AppCommand, FurcodeCommandManager};
use crate::porcelain::Porcelain;
use crate::prompt::FurcodePrompt;
use crate::state::UIState;
use crate::interrupt::{spawn_busy_listener, wait_until_flag};
use crate::live_feed::LiveActivityFeed;
use crate::stream_renderer::{SharedSpinner, StreamingWriter};
use crate::sync_display::SyncProgressDisplay;
use crate::title_display::TitleDisplayExt;
use crate::tools_display::format_tools;
use crate::update::on_update;
use crate::utils::humanize_time;
use crate::{TRACKER, banner, tracker};

// File-specific constants
const MISSING_AGENT_TITLE: &str = "<missing agent.title>";

/// Conversation dump format used by the /dump command
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct ConversationDump {
    conversation: Conversation,
    related_conversations: Vec<Conversation>,
}

/// File paths written/patched/removed by tools after `from_index` (the chosen
/// user turn). Used by double-Esc fork to undo code changes.
fn collect_tool_paths_after(
    messages: &[furcode_domain::MessageEntry],
    from_index: usize,
) -> Vec<String> {
    use furcode_domain::{ContextMessage, ToolCallArguments};

    let mut paths = Vec::new();
    let start = from_index.saturating_add(1);
    for entry in messages.iter().skip(start) {
        let ContextMessage::Text(tm) = &entry.message else {
            continue;
        };
        let Some(calls) = tm.tool_calls.as_ref() else {
            continue;
        };
        for call in calls {
            let name = call.name.as_str();
            if !matches!(name, "write" | "patch" | "multi_patch" | "remove" | "undo") {
                continue;
            }
            let path = match &call.arguments {
                ToolCallArguments::Parsed(v) => v
                    .get("file_path")
                    .or_else(|| v.get("path"))
                    .and_then(|x| x.as_str())
                    .map(str::to_string),
                ToolCallArguments::Unparsed(s) => serde_json::from_str::<serde_json::Value>(s)
                    .ok()
                    .and_then(|v| {
                        v.get("file_path")
                            .or_else(|| v.get("path"))
                            .and_then(|x| x.as_str())
                            .map(str::to_string)
                    }),
            };
            if let Some(p) = path.filter(|s| !s.is_empty()) {
                paths.push(p);
            }
        }
    }
    paths
}

/// Formats an MCP server config for display, redacting sensitive information.
/// Returns the command/URL string only.
fn format_mcp_server(server: &furcode_domain::McpServerConfig) -> String {
    match server {
        furcode_domain::McpServerConfig::Stdio(stdio) => {
            let mut output = format!("{} ", stdio.command);
            for arg in &stdio.args {
                output.push_str(&format!("{arg} "));
            }
            for key in stdio.env.keys() {
                output.push_str(&format!("{key}=*** "));
            }
            output.trim().to_string()
        }
        furcode_domain::McpServerConfig::Http(http) => http.url.clone(),
    }
}

/// Formats HTTP headers for display, redacting values.
/// Returns None if there are no headers.
fn format_mcp_headers(server: &furcode_domain::McpServerConfig) -> Option<String> {
    match server {
        furcode_domain::McpServerConfig::Stdio(_) => None,
        furcode_domain::McpServerConfig::Http(http) => {
            if http.headers.is_empty() {
                None
            } else {
                Some(
                    http.headers
                        .keys()
                        .map(|k| format!("{k}=***"))
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            }
        }
    }
}

pub struct UI<A: ConsoleWriter, F: Fn(FurcodeConfig) -> A> {
    markdown: MarkdownFormat,
    state: UIState,
    api: Arc<F::Output>,
    new_api: Arc<F>,
    console: Console,
    command: Arc<FurcodeCommandManager>,
    cli: Cli,
    spinner: SharedSpinner<A>,
    config: FurcodeConfig,
    /// Messages typed while the agent was busy (sent after the current turn).
    pending_sends: VecDeque<String>,
    #[allow(dead_code)] // The guard is kept alive by being held in the struct
    _guard: furcode_tracker::Guard,
}

impl<A: API + ConsoleWriter + 'static, F: Fn(FurcodeConfig) -> A + Send + Sync> UI<A, F> {
    /// Writes a line to the console output
    /// Takes anything that implements ToString trait
    fn writeln<T: ToString>(&mut self, content: T) -> anyhow::Result<()> {
        self.spinner.write_ln(content)
    }

    /// Writes a TitleFormat to the console output with proper formatting
    fn writeln_title(&mut self, title: TitleFormat) -> anyhow::Result<()> {
        self.spinner.write_ln(title.display())
    }

    fn writeln_to_stderr(&mut self, title: String) -> anyhow::Result<()> {
        self.spinner.ewrite_ln(title)
    }

    /// Helper to get provider for an optional agent, defaulting to the current
    /// active agent's provider
    async fn get_provider(&self, agent_id: Option<AgentId>) -> Result<Provider<Url>> {
        match agent_id {
            Some(agent_id) => self.api.get_agent_provider(agent_id).await,
            None => self.api.get_default_provider().await,
        }
    }

    /// Helper to get model for an optional agent, defaulting to the current
    /// active agent's model
    async fn get_agent_model(&self, agent_id: Option<AgentId>) -> Option<ModelId> {
        match agent_id {
            Some(agent_id) => self.api.get_agent_model(agent_id).await,
            None => self.api.get_session_config().await.map(|c| c.model),
        }
    }

    fn select_raw_row(
        &self,
        prompt: &str,
        query: Option<String>,
        rows: Vec<SelectRow>,
        header_lines: usize,
        initial_raw: Option<String>,
    ) -> Result<Option<SelectRow>> {
        FurcodeWidget::select_rows(prompt, rows)
            .query(query)
            .header_lines(header_lines)
            .initial_raw(initial_raw)
            .prompt()
    }

    fn select_row_output(
        &mut self,
        prompt: &str,
        query: Option<String>,
        rows: Vec<SelectRow>,
    ) -> Result<()> {
        if let Some(row) = self.select_raw_row(prompt, query, rows, 1, None)? {
            self.writeln(row.raw)?;
        }

        Ok(())
    }

    fn porcelain_rows(porcelain: impl ToString) -> Result<Vec<SelectRow>> {
        let porcelain = porcelain.to_string();
        let mut lines = porcelain.lines();
        let Some(header) = lines.next() else {
            return Ok(Vec::new());
        };

        let mut rows = vec![SelectRow::header(header.to_string())];
        rows.extend(lines.filter_map(|line| {
            line.split_whitespace()
                .next()
                .map(|raw| SelectRow::new(raw.to_string(), line.to_string()))
        }));

        Ok(rows)
    }

    /// Displays banner only if user is in interactive mode.
    fn display_banner(&self) -> Result<()> {
        if self.cli.is_interactive() {
            banner::display(false)?;
        }
        Ok(())
    }

    // Handle creating a new conversation
    async fn on_new(&mut self) -> Result<()> {
        let config = furcode_config::FurcodeConfig::read().unwrap_or_default();
        self.config = config.clone();
        self.api = Arc::new((self.new_api)(config));
        self.init_state(false).await?;

        // Set agent if provided via CLI
        if let Some(agent_id) = self.cli.agent.clone() {
            self.api.set_active_agent(agent_id).await?;
        }

        // Reset previously set CLI parameters by the user
        self.cli.conversation = None;
        self.cli.conversation_id = None;

        self.spinner.reset();
        self.display_banner()?;
        self.trace_user();
        self.hydrate_caches();
        Ok(())
    }

    // Set the current mode and update conversation variable
    async fn on_agent_change(&mut self, agent_id: AgentId) -> Result<()> {
        // Convert string to AgentId for validation
        let agent = self
            .api
            .get_agent_infos()
            .await?
            .into_iter()
            .find(|info| info.id == agent_id)
            .ok_or(anyhow::anyhow!("Undefined agent: {agent_id}"))?;

        // Update the app config with the new operating agent.
        self.api.set_active_agent(agent.id.clone()).await?;

        // Update model tracking to reflect the new agent's model
        let model = self.get_agent_model(Some(agent.id.clone())).await;
        self.update_model(model.clone());

        let name = agent.id.as_str().to_case(Case::UpperSnake).bold();

        let title = format!(
            "∙ {}",
            agent.title.as_deref().unwrap_or(MISSING_AGENT_TITLE)
        )
        .dimmed();

        // Show model info if agent uses a specific model
        let model_info = model
            .map(|m| format!(" ∙ model: {m}").dimmed().to_string())
            .unwrap_or_default();

        self.writeln_title(TitleFormat::action(format!("{name} {title}{model_info}")))?;

        Ok(())
    }

    /// Initialises the UI with the provided CLI arguments and API factory.
    ///
    /// # Arguments
    /// * `cli` - Parsed command-line arguments
    /// * `config` - Pre-read application configuration for the initial API
    ///   instance
    /// * `f` - Factory closure invoked once at startup and again on each `/new`
    ///   command; receives the latest [`FurcodeConfig`] so that config changes
    ///   from `furcode config set` are reflected in new conversations
    pub fn init(cli: Cli, config: FurcodeConfig, f: F) -> Result<Self> {
        // Parse CLI arguments first to get flags
        let api = Arc::new(f(config.clone()));
        let env = api.environment();
        let command = Arc::new(FurcodeCommandManager::default());
        let spinner = SharedSpinner::new(SpinnerManager::new(api.clone()));
        Ok(Self {
            state: UIState::new(env.clone()),
            api,
            new_api: Arc::new(f),
            console: Console::new(
                env.clone(),
                config.custom_history_path.clone(),
                command.clone(),
            ),
            cli,
            command,
            spinner,
            markdown: MarkdownFormat::new(),
            config,
            pending_sends: VecDeque::new(),
            _guard: furcode_tracker::init_tracing(env.log_path(), TRACKER.clone())?,
        })
    }

    /// Runs `command` while listening for Esc (interrupt) and typeahead lines
    /// that are queued as pending sends for after the turn.
    ///
    /// Returns `true` if the UI should exit.
    async fn run_command_with_typeahead(&mut self, command: AppCommand) -> Result<bool> {
        let listener = spawn_busy_listener();
        // Sticky `› ` owns the bottom row — stop the braille loader so it cannot
        // paint over the prompt (live feed keeps its own compact animation).
        let _ = self.spinner.stop(None);
        let escaped = listener.escaped.clone();
        let cancel = listener.cancel.clone();
        let pending_q = listener.pending.clone();

        let exit = tokio::select! {
            _ = wait_until_flag(escaped, cancel.clone()) => {
                // Dropping the in-flight command cancels the agent turn.
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                self.spinner.reset();
                tracing::info!("User interrupted operation with Esc");
                false
            }
            result = self.on_command(command) => {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                match result {
                    Ok(exit) => exit,
                    Err(error) => {
                        self.handle_command_error(error).await?;
                        false
                    }
                }
            }
        };

        // Merge lines queued during the turn (listener may still be flushing).
        if let Ok(mut q) = pending_q.lock() {
            while let Some(line) = q.pop_front() {
                self.pending_sends.push_back(line);
            }
        }

        let draft = listener.stop().await;
        // Catch anything pushed between drain and stop.
        if let Ok(mut q) = pending_q.lock() {
            while let Some(line) = q.pop_front() {
                self.pending_sends.push_back(line);
            }
        }
        // Never pre-fill the next prompt with terminal query residue
        // (OSC/CSI replies that leaked into the typeahead buffer).
        let draft = draft.trim();
        if !draft.is_empty()
            && !draft.contains("rgb:")
            && !draft.starts_with('?')
            && !draft.chars().all(|c| {
                c.is_ascii_digit() || matches!(c, ';' | '?' | 'c' | '/' | ':' | 'r' | 'g' | 'b')
            })
        {
            self.console.set_buffer(draft.to_string());
        }
        Ok(exit)
    }

    async fn handle_command_error(&mut self, error: anyhow::Error) -> Result<()> {
        if let Some(conversation_id) = self.state.conversation_id.as_ref()
            && let Some(conversation) = self
                .api
                .conversation(conversation_id)
                .await
                .ok()
                .flatten()
        {
            TRACKER.set_conversation(conversation).await;
        }
        tracker::error(&error);
        tracing::error!(error = ?error);
        self.spinner.stop(None)?;
        self.writeln_to_stderr(
            TitleFormat::error(format!("{error:?}"))
                .display()
                .to_string(),
        )?;
        Ok(())
    }

    /// Runs the new frame-based TUI (`FURCODE_TUI=1`) with a live agent stream.
    async fn run_frame_tui(&mut self) -> Result<()> {
        use crate::tui::{
            LOAD_MORE_TURNS, UserAction, apply_chat_response, bootstrap_from_context_json,
            join_session, start_session,
        };
        use furcode_domain::Event;
        use tokio_stream::StreamExt;

        let cwd = self
            .state
            .cwd
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("furcode");
        let header_left = format!(" 🄵  {cwd}");
        let agent = self
            .api
            .get_active_agent()
            .await
            .map(|a| a.to_string())
            .unwrap_or_else(|| "FURCODE".into());
        let model = self
            .get_agent_model(None)
            .await
            .map(|m| m.to_string())
            .unwrap_or_default();
        let header_right = format!("{agent}  {model}");

        let mut session = start_session(header_left, header_right)?;

        // Dual-tier resume via **raw context JSON** (never full Conversation decode —
        // tool output bodies alone are 50–90KB each and hung the previous path).
        //
        // Important: after clear_status the draw loop must see `transcript_gen`
        // bump (set_status/clear_status/touch) or the "Loading…" frame stays
        // painted forever.
        if let Some(id) = self.state.conversation_id {
            {
                let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
                v.set_status("Loading conversation…");
            }
            // Let the draw loop paint the spinner before we hit SQLite.
            tokio::time::sleep(Duration::from_millis(16)).await;

            let load = async {
                let raw = self.api.conversation_context_json(&id).await?;
                Ok::<_, anyhow::Error>(raw)
            };
            // Hard cap so a stuck pool/DB can never leave the UI on Loading forever.
            match tokio::time::timeout(Duration::from_secs(8), load).await {
                Ok(Ok(Some(json))) => {
                    let env = self.api.environment();
                    {
                        let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
                        v.set_status("Building transcript…");
                    }
                    // Yield so "Building…" can paint; slim parse is typically <5ms.
                    tokio::task::yield_now().await;
                    let boot = bootstrap_from_context_json(&json, &env);
                    drop(json);
                    let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
                    v.history_archive = Some(boot.archive);
                    if !boot.visible.is_empty() {
                        v.extend_transcript(boot.visible);
                        v.scroll_to_bottom();
                    }
                    v.clear_status();
                }
                Ok(Ok(None)) => {
                    tracing::debug!(%id, "no context JSON for conversation");
                    let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
                    v.clear_status();
                }
                Ok(Err(err)) => {
                    tracing::warn!(error = ?err, "failed to load context JSON for TUI resume");
                    let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
                    v.push_transcript(crate::tui::TranscriptLine {
                        kind: crate::tui::TranscriptKind::Info,
                        text: format!("Could not load history: {err:#}"),
                    });
                    v.clear_status();
                }
                Err(_) => {
                    tracing::warn!(%id, "TUI history load timed out");
                    let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
                    v.push_transcript(crate::tui::TranscriptLine {
                        kind: crate::tui::TranscriptKind::Info,
                        text: "History load timed out — continue chatting; full context still in DB"
                            .into(),
                    });
                    v.clear_status();
                }
            }
        }

        let mut pending: VecDeque<String> = VecDeque::new();

        loop {
            // Prefer messages queued during a busy turn.
            let next_msg = if let Some(msg) = pending.pop_front() {
                Some(msg)
            } else {
                match session.actions.recv().await {
                    Some(UserAction::Send(msg)) => Some(msg),
                    Some(UserAction::LoadMoreHistory) => {
                        let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(arch) = v.history_archive.as_mut() {
                            let res = arch.load_more(LOAD_MORE_TURNS);
                            if !res.lines.is_empty() {
                                v.apply_load_more(res.lines);
                            } else {
                                v.load_more_requested = false;
                            }
                        } else {
                            v.load_more_requested = false;
                        }
                        continue;
                    }
                    Some(UserAction::Interrupt) => {
                        // No in-flight turn; already handled in the draw loop.
                        continue;
                    }
                    Some(UserAction::ForkHistory) => {
                        self.frame_tui_open_fork_picker(&session).await?;
                        continue;
                    }
                    Some(UserAction::ForkSelect {
                        message_index,
                        edit_text,
                    }) => {
                        self.frame_tui_apply_fork(&session, message_index, edit_text)
                            .await?;
                        continue;
                    }
                    Some(UserAction::Quit) | None => break,
                }
            };

            let Some(msg) = next_msg else {
                break;
            };

            tracker::prompt(msg.clone());
            session.interrupt.store(false, std::sync::atomic::Ordering::SeqCst);

            let conversation_id = self.init_conversation().await?;
            let chat = ChatRequest::new(Event::new(msg), conversation_id);
            let mut stream = self.api.chat(chat).await?;

            {
                let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
                v.busy = true;
            }

            // Drive the agent stream; Esc sets interrupt and aborts this select.
            loop {
                tokio::select! {
                    biased;
                    _ = async {
                        while !session.interrupt.load(std::sync::atomic::Ordering::SeqCst) {
                            tokio::time::sleep(Duration::from_millis(40)).await;
                        }
                    } => {
                        {
                            let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
                            v.push_transcript(crate::tui::TranscriptLine {
                                kind: crate::tui::TranscriptKind::Info,
                                text: "Turn interrupted".into(),
                            });
                            for m in v.commit_pending_to_transcript() {
                                pending.push_back(m);
                            }
                            v.mark_idle();
                        }
                        // Drain any actions that arrived during the turn.
                        while let Ok(action) = session.actions.try_recv() {
                            match action {
                                UserAction::Send(m) => pending.push_back(m),
                                UserAction::LoadMoreHistory => {
                                    let mut v =
                                        session.view.lock().unwrap_or_else(|e| e.into_inner());
                                    if let Some(arch) = v.history_archive.as_mut() {
                                        let res = arch.load_more(LOAD_MORE_TURNS);
                                        if !res.lines.is_empty() {
                                            v.apply_load_more(res.lines);
                                        } else {
                                            v.load_more_requested = false;
                                        }
                                    } else {
                                        v.load_more_requested = false;
                                    }
                                }
                                UserAction::Quit => {
                                    let _ = join_session(session).await;
                                    self.print_exit_resume_hint()?;
                                    return Ok(());
                                }
                                UserAction::Interrupt
                                | UserAction::ForkHistory
                                | UserAction::ForkSelect { .. } => {}
                            }
                        }
                        break;
                    }
                    action = session.actions.recv() => {
                        match action {
                            Some(UserAction::Send(m)) => pending.push_back(m),
                            Some(UserAction::LoadMoreHistory) => {
                                let mut v =
                                    session.view.lock().unwrap_or_else(|e| e.into_inner());
                                if let Some(arch) = v.history_archive.as_mut() {
                                    let res = arch.load_more(LOAD_MORE_TURNS);
                                    if !res.lines.is_empty() {
                                        v.apply_load_more(res.lines);
                                    } else {
                                        v.load_more_requested = false;
                                    }
                                } else {
                                    v.load_more_requested = false;
                                }
                            }
                            Some(UserAction::Interrupt) => {
                                session.interrupt.store(true, std::sync::atomic::Ordering::SeqCst);
                            }
                            // Fork only while idle (after the current turn ends).
                            Some(UserAction::ForkHistory) | Some(UserAction::ForkSelect { .. }) => {}
                            Some(UserAction::Quit) | None => {
                                let _ = join_session(session).await;
                                self.print_exit_resume_hint()?;
                                return Ok(());
                            }
                        }
                    }
                    item = stream.next() => {
                        match item {
                            Some(Ok(message)) => {
                                let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
                                apply_chat_response(&mut v, message);
                            }
                            Some(Err(err)) => {
                                let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
                                v.push_transcript(crate::tui::TranscriptLine {
                                    kind: crate::tui::TranscriptKind::ToolError,
                                    text: format!("Error: {err:#}"),
                                });
                                for m in v.commit_pending_to_transcript() {
                                    pending.push_back(m);
                                }
                                v.mark_idle();
                                break;
                            }
                            None => {
                                let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
                                // Sticky queues → real user rows (after tools), then send.
                                for m in v.commit_pending_to_transcript() {
                                    pending.push_back(m);
                                }
                                v.mark_idle();
                                break;
                            }
                        }
                    }
                }
            }

            session.interrupt.store(false, std::sync::atomic::Ordering::SeqCst);
        }

        let _ = join_session(session).await;
        self.print_exit_resume_hint()?;
        Ok(())
    }

    /// Prints how to resume this session after `/exit` or Ctrl+D.
    fn print_exit_resume_hint(&mut self) -> Result<()> {
        match self.state.conversation_id {
            Some(id) => {
                let id = id.into_string();
                self.writeln_title(
                    TitleFormat::info("Session ended")
                        .sub_title(format!("furcode --conversation-id {id}")),
                )?;
            }
            None => {
                self.writeln_title(TitleFormat::info("Session ended"))?;
            }
        }
        Ok(())
    }

    /// Next command: prefer queued pending sends over interactive prompt.
    async fn next_command(&mut self) -> Result<AppCommand> {
        if let Some(msg) = self.pending_sends.pop_front() {
            // Expand `[Image N]` atoms → `@[path]` before the agent sees them.
            let msg = crate::paste::expand_image_atoms(&msg);
            tracker::prompt(msg.clone());
            let remaining = self.pending_sends.len();
            if remaining > 0 {
                let _ = self.writeln_title(
                    TitleFormat::info("Sending queued")
                        .sub_title(format!("{remaining} more in queue")),
                );
            } else {
                let preview: String = msg.chars().take(72).collect();
                let suffix = if msg.chars().count() > 72 { "…" } else { "" };
                let _ = self.writeln_title(
                    TitleFormat::info("Sending queued").sub_title(format!("{preview}{suffix}")),
                );
            }
            return Ok(AppCommand::Message(msg));
        }
        self.prompt().await
    }

    async fn prompt(&mut self) -> Result<AppCommand> {
        // Get usage from current conversation if available.
        // Use the last message's usage for token count (context window size),
        // but replace cost with the accumulated session cost so the cost
        // shown reflects the total spend rather than just the last request.
        let usage = if let Some(conversation_id) = &self.state.conversation_id {
            self.api
                .conversation(conversation_id)
                .await
                .ok()
                .flatten()
                .and_then(|conv| {
                    conv.usage().map(|mut u| {
                        u.cost = conv.accumulated_cost();
                        u
                    })
                })
        } else {
            None
        };

        // Prompt the user for input
        let agent_id = self.api.get_active_agent().await.unwrap_or_default();
        let model = self
            .get_agent_model(self.api.get_active_agent().await)
            .await;
        let reasoning_effort = self.api.get_reasoning_effort().await.ok().flatten();
        let mut furcode_prompt = FurcodePrompt::new(self.state.cwd.clone(), agent_id);
        if let Some(u) = usage {
            furcode_prompt.usage(u);
        }
        if let Some(m) = model {
            furcode_prompt.model(m);
        }
        if let Some(e) = reasoning_effort {
            furcode_prompt.reasoning_effort(e);
        }
        match self.console.prompt(&mut furcode_prompt).await? {
            crate::input::PromptOutcome::Command(cmd) => Ok(cmd),
            crate::input::PromptOutcome::ForkHistory => {
                self.on_fork_history().await?;
                // Re-prompt after fork (buffer may be pre-filled with the message).
                Box::pin(self.prompt()).await
            }
        }
    }

    /// Frame TUI: open the double-Esc user-message picker.
    async fn frame_tui_open_fork_picker(
        &mut self,
        session: &crate::tui::TuiSession,
    ) -> anyhow::Result<()> {
        use crate::tui::{ForkItem, ForkPicker};
        use furcode_domain::{ContextMessage, Role};

        let Some(conversation_id) = self.state.conversation_id else {
            let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
            v.push_transcript(crate::tui::TranscriptLine {
                kind: crate::tui::TranscriptKind::Info,
                text: "No conversation yet — send a message first, then Esc Esc to undo."
                    .into(),
            });
            return Ok(());
        };

        let conversation = match self.api.conversation(&conversation_id).await? {
            Some(c) => c,
            None => {
                let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
                v.push_transcript(crate::tui::TranscriptLine {
                    kind: crate::tui::TranscriptKind::Info,
                    text: "Conversation not found.".into(),
                });
                return Ok(());
            }
        };

        let Some(context) = conversation.context.as_ref() else {
            let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
            v.push_transcript(crate::tui::TranscriptLine {
                kind: crate::tui::TranscriptKind::Info,
                text: "No messages to undo to.".into(),
            });
            return Ok(());
        };

        let mut items: Vec<ForkItem> = context
            .messages
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let ContextMessage::Text(tm) = &entry.message else {
                    return None;
                };
                if tm.role != Role::User || tm.droppable {
                    return None;
                }
                // Only real typed prompts (raw_content) — same rule as history resume.
                let edit_text = tm
                    .raw_content
                    .as_ref()
                    .and_then(|v| v.as_user_prompt())
                    .map(|p| p.as_str().trim().to_string())
                    .filter(|s| !s.is_empty())?;
                if edit_text.trim_start().starts_with('/') {
                    return None;
                }
                let one_line = edit_text
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                let preview: String = one_line.chars().take(72).collect();
                let label = if one_line.chars().count() > 72 {
                    format!("{preview}…")
                } else {
                    preview.to_string()
                };
                Some(ForkItem {
                    message_index: index,
                    label,
                    edit_text,
                })
            })
            .collect();

        if items.is_empty() {
            let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
            v.push_transcript(crate::tui::TranscriptLine {
                kind: crate::tui::TranscriptKind::Info,
                text: "No user messages to restore.".into(),
            });
            return Ok(());
        }

        // Newest first.
        items.reverse();
        let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
        v.fork_picker = Some(ForkPicker {
            items,
            selected: 0,
        });
        v.touch_transcript();
        Ok(())
    }

    /// Frame TUI: truncate conversation at a user turn, undo file writes after
    /// it, rebuild transcript, and pre-fill the composer.
    async fn frame_tui_apply_fork(
        &mut self,
        session: &crate::tui::TuiSession,
        message_index: usize,
        edit_text: String,
    ) -> anyhow::Result<()> {
        use crate::tui::bootstrap_from_conversation;

        let Some(conversation_id) = self.state.conversation_id else {
            return Ok(());
        };

        let mut conversation = self
            .api
            .conversation(&conversation_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Conversation not found"))?;

        let Some(mut context) = conversation.context.take() else {
            return Ok(());
        };

        if message_index > context.messages.len() {
            return Ok(());
        }

        // Collect file paths touched by tools after this user message (for undo).
        let paths = collect_tool_paths_after(&context.messages, message_index);

        // Drop the selected user message and everything after it.
        context.messages.truncate(message_index);
        conversation.context = Some(context);
        self.api.upsert_conversation(conversation.clone()).await?;

        // Best-effort code undo: reverse chronological order of writes.
        let mut undone = 0usize;
        for path in paths.into_iter().rev() {
            if self.api.undo_file(path).await.is_ok() {
                undone += 1;
            }
        }

        // Rebuild frame transcript from remaining conversation.
        let env = self.api.environment();
        let boot = bootstrap_from_conversation(&conversation, &env);
        {
            let mut v = session.view.lock().unwrap_or_else(|e| e.into_inner());
            v.transcript.clear();
            v.history_archive = Some(boot.archive);
            if !boot.visible.is_empty() {
                v.extend_transcript(boot.visible);
            }
            v.composer.buffer = edit_text;
            v.composer.cursor = v.composer.buffer.len();
            v.pending.clear();
            v.busy = false;
            v.fork_picker = None;
            v.scroll_to_bottom();
            let note = if undone > 0 {
                format!(
                    "Restored conversation · undid {undone} file snapshot(s) · edit prompt & Enter"
                )
            } else {
                "Restored conversation · edit prompt & Enter to continue".into()
            };
            v.push_transcript(crate::tui::TranscriptLine {
                kind: crate::tui::TranscriptKind::Info,
                text: note,
            });
            v.touch_transcript();
        }
        Ok(())
    }

    /// Double-Esc: pick a previous user message, truncate context after it, and
    /// pre-fill the editor so the user can edit and continue (fork) from there.
    async fn on_fork_history(&mut self) -> anyhow::Result<()> {
        use furcode_domain::{ContextMessage, Role};
        use furcode_select::FurcodeWidget;

        let Some(conversation_id) = self.state.conversation_id else {
            self.writeln_title(TitleFormat::info("No conversation yet").sub_title(
                "Send a message first, then double-Esc to edit a previous turn.",
            ))?;
            return Ok(());
        };

        let mut conversation = self
            .api
            .conversation(&conversation_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Conversation not found"))?;

        let Some(context) = conversation.context.as_mut() else {
            self.writeln_title(TitleFormat::info("No messages to edit"))?;
            return Ok(());
        };

        /// One prior user turn available for fork/edit.
        #[derive(Clone)]
        struct UserTurn {
            /// Index into `context.messages` of this user message.
            index: usize,
            /// Text shown in the picker.
            label: String,
            /// Text loaded into the editor.
            edit_text: String,
        }

        impl std::fmt::Display for UserTurn {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.label)
            }
        }

        let turns: Vec<UserTurn> = context
            .messages
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let ContextMessage::Text(tm) = &entry.message else {
                    return None;
                };
                if tm.role != Role::User {
                    return None;
                }
                // Prefer raw user prompt; fall back to rendered content.
                let edit_text = tm
                    .raw_content
                    .as_ref()
                    .and_then(|v| v.as_user_prompt())
                    .map(|p| p.as_str().to_string())
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| tm.content.clone());

                if edit_text.trim().is_empty() {
                    return None;
                }

                // Skip pure slash-command turns for cleaner UX.
                if edit_text.trim_start().starts_with('/') {
                    return None;
                }

                let one_line = edit_text
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                let preview: String = one_line.chars().take(72).collect();
                let label = if one_line.chars().count() > 72 {
                    format!("{preview}…")
                } else {
                    preview
                };

                Some(UserTurn {
                    index,
                    label: format!("[{index}] {label}"),
                    edit_text,
                })
            })
            .collect();

        if turns.is_empty() {
            self.writeln_title(TitleFormat::info("No user messages to fork from"))?;
            return Ok(());
        }

        // Newest first for quicker access to recent turns.
        let mut turns = turns;
        turns.reverse();

        let selected = FurcodeWidget::select("Fork from message", turns)
            .with_help_message("↑↓ select · Enter edit & fork · Esc cancel")
            .prompt()?;

        let Some(turn) = selected else {
            return Ok(());
        };

        // Drop the selected user message and everything after it.
        context.messages.truncate(turn.index);
        conversation.context = Some(context.clone());
        self.api.upsert_conversation(conversation).await?;

        self.console.set_buffer(turn.edit_text);
        self.writeln_title(TitleFormat::info("Fork ready").sub_title(
            "Edit the message and press Enter to continue from this point.",
        ))?;
        Ok(())
    }

    pub async fn run(&mut self) {
        match self.run_inner().await {
            Ok(_) => {}
            Err(error) => {
                tracing::error!(error = ?error);

                // Display the full error chain for better debugging
                let mut error_message = error.to_string();
                let mut source = error.source();
                while let Some(err) = source {
                    error_message.push_str(&format!("\n    Caused by: {}", err));
                    source = err.source();
                }

                let _ =
                    self.writeln_to_stderr(TitleFormat::error(error_message).display().to_string());
            }
        }
    }

    async fn run_inner(&mut self) -> Result<()> {
        if let Some(cmd) = self.cli.subcommands.clone() {
            return self.handle_subcommands(cmd).await;
        }

        // Display the banner in dimmed colors since we're in interactive mode
        self.display_banner()?;
        self.init_state(true).await?;

        self.trace_user();
        self.hydrate_caches();
        self.init_conversation().await?;

        // Check for dispatch flag first
        if let Some(dispatch_json) = self.cli.event.clone() {
            return self.handle_dispatch(dispatch_json).await;
        }

        // Frame-based TUI scaffold (FURCODE_TUI=1). Chat stream wiring follows
        // in the next phase; this validates layout + live composer.
        if self.cli.is_interactive() && crate::tui::enabled() {
            return self.run_frame_tui().await;
        }

        // Handle direct prompt or piped input if provided (raw text messages)
        let input = self.cli.prompt.clone().or(self.cli.piped_input.clone());
        if let Some(input) = input {
            tracker::prompt(input.clone());
            self.spinner.start(None)?;
            let exit = self
                .run_command_with_typeahead(AppCommand::Message(input))
                .await?;
            if exit {
                return Ok(());
            }
            // Non-interactive one-shot: do not enter the REPL.
            if self.cli.prompt.is_some() || self.cli.piped_input.is_some() {
                // Drain any typeahead queued during the one-shot turn.
                while let Some(msg) = self.pending_sends.pop_front() {
                    self.spinner.start(None)?;
                    let msg = crate::paste::expand_image_atoms(&msg);
                    if self
                        .run_command_with_typeahead(AppCommand::Message(msg))
                        .await?
                    {
                        return Ok(());
                    }
                }
                return Ok(());
            }
        }

        // Prompt can fail if it doesn't have access to TTY. If it fails the first
        // time, we will stop everything and bubble up the error.
        let mut command = self.next_command().await;

        loop {
            match command {
                Ok(command) => {
                    let exit = self.run_command_with_typeahead(command).await?;
                    self.spinner.stop(None)?;
                    if exit {
                        return Ok(());
                    }
                }
                Err(error) => {
                    tracker::error(&error);
                    tracing::error!(error = ?error);
                    self.spinner.stop(None)?;

                    match error.downcast::<ReadLineError>() {
                        Ok(error) => {
                            Err(error)?;
                        }
                        Err(error) => self.writeln_to_stderr(
                            TitleFormat::error(error.to_string()).display().to_string(),
                        )?,
                    }
                }
            }
            // Prefer queued typeahead; otherwise interactive prompt.
            command = self.next_command().await;
        }
    }

    // Improve startup time by hydrating caches
    fn hydrate_caches(&self) {
        let api = self.api.clone();
        tokio::spawn(async move { api.get_models().await });
        let api = self.api.clone();
        tokio::spawn(async move { api.get_tools().await });
        let api = self.api.clone();
        tokio::spawn(async move { api.get_agent_infos().await });
        let api = self.api.clone();
        tokio::spawn(async move {
            let _ = api.hydrate_channel();
        });
    }

    async fn handle_generate_conversation_id(&mut self) -> Result<()> {
        let conversation_id = furcode_domain::ConversationId::generate();
        println!("{}", conversation_id.into_string());
        Ok(())
    }

    async fn handle_subcommands(&mut self, subcommand: TopLevelCommand) -> anyhow::Result<()> {
        match subcommand {
            TopLevelCommand::Agent(agent_group) => {
                match agent_group.command {
                    crate::cli::AgentCommand::List => {
                        self.on_show_agents(agent_group.porcelain, false).await?;
                    }
                }
                return Ok(());
            }
            TopLevelCommand::List(list_group) => {
                let porcelain = list_group.porcelain;
                match list_group.command {
                    ListCommand::Agent { custom } => {
                        self.on_show_agents(porcelain, custom).await?;
                    }
                    ListCommand::Provider { types } => {
                        self.on_show_providers(porcelain, types).await?;
                    }
                    ListCommand::Model => {
                        self.on_show_models(porcelain).await?;
                    }
                    ListCommand::Command { custom } => {
                        if custom {
                            self.on_show_custom_commands(porcelain).await?;
                        } else {
                            self.on_show_commands(porcelain).await?;
                        }
                    }
                    ListCommand::Config => {
                        self.on_show_config(porcelain).await?;
                    }
                    ListCommand::Tool { agent } => {
                        self.on_show_tools(agent, porcelain).await?;
                    }
                    ListCommand::Mcp => {
                        self.on_show_mcp_servers(porcelain).await?;
                    }
                    ListCommand::Conversation { parent } => {
                        if let Some(parent_id) = parent {
                            let parent_conv = self.validate_conversation_exists(&parent_id).await?;
                            let children = self.fetch_related_conversations(&parent_conv).await;

                            if children.is_empty() {
                                self.writeln_title(TitleFormat::info(
                                    "No child conversations found.",
                                ))?;
                            } else {
                                let mut info = Info::new();
                                for conv in children.into_iter() {
                                    let title = conv
                                        .title
                                        .as_deref()
                                        .map(|t| t.to_string())
                                        .unwrap_or_else(|| markers::EMPTY.to_string());

                                    let duration = chrono::Utc::now().signed_duration_since(
                                        conv.metadata
                                            .updated_at
                                            .unwrap_or(conv.metadata.created_at),
                                    );
                                    let duration = std::time::Duration::from_secs(
                                        (duration.num_minutes() * 60).max(0) as u64,
                                    );
                                    let time_ago = if duration.is_zero() {
                                        "now".to_string()
                                    } else {
                                        format!("{} ago", humantime::format_duration(duration))
                                    };

                                    info = info
                                        .add_title(conv.id)
                                        .add_key_value("Title", title)
                                        .add_key_value("Updated", time_ago);
                                }

                                let porcelain = Porcelain::from(&info)
                                    .drop_col(3)
                                    .truncate(1, 60)
                                    .uppercase_headers();
                                self.writeln(porcelain)?;
                            }
                        } else {
                            self.on_show_conversations(porcelain).await?;
                        }
                    }
                    ListCommand::Cmd => {
                        self.on_show_custom_commands(porcelain).await?;
                    }
                    ListCommand::Skill { custom } => {
                        self.on_show_skills(porcelain, custom).await?;
                    }
                    ListCommand::File => {
                        self.on_list_files(porcelain).await?;
                    }
                }
                return Ok(());
            }
            TopLevelCommand::Mcp(mcp_command) => match mcp_command.command {
                McpCommand::Import(import_args) => {
                    let scope: furcode_domain::Scope = import_args.scope.into();

                    // Parse the incoming MCP configuration
                    let incoming_config: furcode_domain::McpConfig = serde_json::from_str(&import_args.json)
                        .context("Failed to parse MCP configuration JSON. Expected format: {\"mcpServers\": {...}}")?;

                    // Read only the scope-specific config (not merged)
                    let mut scope_config = self.api.read_mcp_config(Some(&scope)).await?;

                    // Merge the incoming servers with scope-specific config only
                    let mut added_servers = Vec::new();
                    for (server_name, server_config) in incoming_config.mcp_servers {
                        scope_config
                            .mcp_servers
                            .insert(server_name.clone(), server_config);
                        added_servers.push(server_name);
                    }

                    // Write back to the specific scope only
                    self.api.write_mcp_config(&scope, &scope_config).await?;

                    // Log each added server after successful write
                    for server_name in added_servers {
                        self.writeln_title(TitleFormat::info(format!(
                            "Added MCP server '{server_name}'"
                        )))?;
                    }
                }
                McpCommand::List => {
                    self.on_show_mcp_servers(mcp_command.porcelain).await?;
                }
                McpCommand::Remove(rm) => {
                    let name = furcode_api::ServerName::from(rm.name);
                    let scope: furcode_domain::Scope = rm.scope.into();

                    // Read only the scope-specific config (not merged)
                    let mut scope_config = self.api.read_mcp_config(Some(&scope)).await?;

                    // Remove the server from scope-specific config only
                    scope_config.mcp_servers.remove(&name);

                    // Write back to the specific scope only
                    self.api.write_mcp_config(&scope, &scope_config).await?;

                    self.writeln_title(TitleFormat::info(format!("Removed server: {name}")))?;
                }
                McpCommand::Show(val) => {
                    let name = furcode_api::ServerName::from(val.name);
                    let config = self.api.read_mcp_config(None).await?;
                    let server = config
                        .mcp_servers
                        .get(&name)
                        .ok_or(anyhow::anyhow!("Server not found"))?;

                    // Get MCP servers to check for failures
                    let tools = self.api.get_tools().await?;

                    // Display server configuration
                    self.writeln_title(TitleFormat::info(format!(
                        "{name}: {}",
                        format_mcp_server(server)
                    )))?;

                    // Display error if the server failed to initialize
                    if let Some(error) = tools.mcp.get_failures().get(&name) {
                        self.writeln_title(TitleFormat::error(error))?;
                    }
                }
                McpCommand::Reload => {
                    self.spinner.start(Some("Reloading MCPs"))?;
                    self.api.reload_mcp().await?;
                    self.writeln_title(TitleFormat::info("MCP reloaded"))?;
                }
                McpCommand::Login(args) => {
                    self.handle_mcp_login(&args.name).await?;
                }
                McpCommand::Logout(args) => {
                    self.handle_mcp_logout(&args.name).await?;
                }
            },
            TopLevelCommand::Info { porcelain, conversation_id } => {
                // Only initialize state (agent/provider/model resolution).
                // Avoid on_new() which also spawns fire-and-forget background
                // tasks via hydrate_caches() that race with process exit and
                // cause "JoinHandle polled after completion" panics.
                self.init_state(false).await?;

                self.on_info(porcelain, conversation_id).await?;
                return Ok(());
            }
            TopLevelCommand::Banner => {
                banner::display(true)?;
                return Ok(());
            }
            TopLevelCommand::Config(config_group) => {
                self.handle_config_command(config_group.command.clone(), config_group.porcelain)
                    .await?;
                return Ok(());
            }
            TopLevelCommand::Provider(provider_group) => {
                self.handle_provider_command(provider_group).await?;
                return Ok(());
            }
            TopLevelCommand::Conversation(conversation_group) => {
                self.handle_conversation_command(conversation_group).await?;
                return Ok(());
            }
            TopLevelCommand::Suggest { prompt } => {
                self.on_cmd(UserPrompt::from(prompt)).await?;
                return Ok(());
            }
            TopLevelCommand::Cmd(run_group) => {
                let porcelain = run_group.porcelain;
                match run_group.command {
                    crate::cli::CmdCommand::List { custom } => {
                        if custom {
                            self.on_show_custom_commands(porcelain).await?;
                        } else {
                            self.on_show_commands(porcelain).await?;
                        }
                    }
                    crate::cli::CmdCommand::Execute { commands: args } => {
                        // Execute the custom command
                        self.init_state(false).await?;

                        // If conversation_id is provided, set it in CLI before initializing
                        if let Some(ref cid) = run_group.conversation_id {
                            self.cli.conversation_id = Some(*cid);
                        }

                        self.init_conversation().await?;
                        self.spinner.start(None)?;

                        // Join all args into a single command string
                        let command_str = args.join(" ");

                        // Add slash prefix if not present
                        let command_with_slash = if command_str.starts_with('/') {
                            command_str
                        } else {
                            format!("/{command_str}")
                        };
                        let command = self.command.parse(&command_with_slash)?;
                        self.on_command(command).await?;
                    }
                }
                return Ok(());
            }
            TopLevelCommand::Workspace(index_group) => {
                match index_group.command {
                    cmd => {
                        if !self.config.cloud_services_enabled() {
                            self.writeln_title(
                                TitleFormat::info("Cloud services disabled").sub_title(
                                    "Workspace commands need services_enabled=true and services_url.",
                                ),
                            )?;
                            return Ok(());
                        }
                        match cmd {
                            crate::cli::WorkspaceCommand::Sync { path, init } => {
                                self.on_index(path, init).await?;
                            }
                            crate::cli::WorkspaceCommand::List { porcelain } => {
                                self.on_list_workspaces(porcelain).await?;
                            }
                            crate::cli::WorkspaceCommand::Query {
                                query,
                                path,
                                limit,
                                top_k,
                                use_case,
                                starts_with,
                                ends_with,
                            } => {
                                let mut params = furcode_domain::SearchParams::new(
                                    &query, &use_case,
                                )
                                .limit(limit);
                                if let Some(k) = top_k {
                                    params = params.top_k(k);
                                }
                                if let Some(prefix) = starts_with {
                                    params = params.starts_with(prefix);
                                }
                                if let Some(suffix) = ends_with {
                                    params = params.ends_with(vec![suffix]);
                                }
                                self.on_query(path, params).await?;
                            }
                            crate::cli::WorkspaceCommand::Info { path } => {
                                self.on_workspace_info(path).await?;
                            }
                            crate::cli::WorkspaceCommand::Delete { workspace_ids } => {
                                self.on_delete_workspaces(workspace_ids).await?;
                            }
                            crate::cli::WorkspaceCommand::Status { path, porcelain } => {
                                self.on_workspace_status(path, porcelain).await?;
                            }
                            crate::cli::WorkspaceCommand::Init { path, yes } => {
                                self.on_workspace_init(path, yes).await?;
                            }
                        }
                    }
                }
                return Ok(());
            }
            TopLevelCommand::Commit(commit_group) => {
                self.init_state(false).await?;
                let preview = commit_group.preview;
                let result = self.handle_commit_command(commit_group).await?;
                if preview {
                    self.writeln(&result.message)?;
                } else if !result.git_output.is_empty() {
                    self.writeln_to_stderr(result.git_output.trim_end().to_string())?;
                } else {
                    self.writeln_to_stderr(result.message.trim_end().to_string())?;
                }
                return Ok(());
            }
            TopLevelCommand::Data(data_command_group) => {
                let mut stream = self.api.generate_data(data_command_group.into()).await?;
                while let Some(data) = stream.next().await {
                    self.writeln(data?)?;
                }
            }
            TopLevelCommand::Vscode(vscode_command) => {
                match vscode_command {
                    crate::cli::VscodeCommand::InstallExtension => {
                        self.on_vscode_extension_install().await?;
                    }
                }
                return Ok(());
            }
            TopLevelCommand::Update(args) => {
                let update = furcode_config::Update::default().auto_update(args.no_confirm);
                on_update(self.api.clone(), Some(&update)).await;
                return Ok(());
            }
            TopLevelCommand::Logs(args) => {
                let log_dir = self.api.environment().log_path();
                crate::logs::run(args, log_dir).await?;
                return Ok(());
            }
            TopLevelCommand::Select(cmd) => {
                if !matches!(&cmd.command, SelectCommand::File { .. }) {
                    self.init_state(false).await?;
                }

                match &cmd.command {
                    SelectCommand::File { query } => {
                        if let Some(file) =
                            crate::completer::select_workspace_file(&self.state.cwd, query.clone())?
                        {
                            self.writeln(file)?;
                        }
                    }
                    SelectCommand::Model { query } => {
                        if let Some((model_id, provider_id)) =
                            self.select_model(None, query.clone()).await?
                        {
                            self.writeln(model_id.as_str())?;
                            self.writeln(provider_id.as_ref())?;
                        }
                    }
                    SelectCommand::Agent { query } => {
                        if let Some(agent_id) = self.select_agent(query.clone()).await? {
                            self.writeln(agent_id.as_str())?;
                        }
                    }
                    SelectCommand::Provider { query, configured } => {
                        if let Some(provider) =
                            self.select_provider(query.clone(), *configured).await?
                        {
                            self.writeln(provider.id().as_ref())?;
                        }
                    }
                    SelectCommand::ReasoningEffort { query } => {
                        if let Some(effort) = self
                            .select_reasoning_effort("Reasoning Effort", query.clone())
                            .await?
                        {
                            self.writeln(effort)?;
                        }
                    }
                    SelectCommand::Command { query } => {
                        let rows = Self::porcelain_rows(self.commands_porcelain().await?)?;

                        if !rows.is_empty() {
                            self.select_row_output("Command", query.clone(), rows)?;
                        }
                    }
                    SelectCommand::Conversation { query, parent } => {
                        let conversations = if let Some(parent_id) = parent {
                            let parent_conv = self.validate_conversation_exists(parent_id).await?;
                            self.fetch_related_conversations(&parent_conv).await
                        } else {
                            let max_conversations = self.config.max_conversations;
                            let conversations =
                                self.api.get_conversations(Some(max_conversations)).await?;
                            Self::user_initiated_conversations(conversations)
                        };

                        if !conversations.is_empty()
                            && let Some(conversation) = ConversationSelector::select_conversation(
                                &conversations,
                                self.state.conversation_id,
                                query.clone(),
                            )
                            .await?
                        {
                            self.writeln(conversation.id)?;
                        }
                    }
                }
                return Ok(());
            }
        }
        Ok(())
    }

    async fn handle_conversation_command(
        &mut self,
        conversation_group: crate::cli::ConversationCommandGroup,
    ) -> anyhow::Result<()> {
        match conversation_group.command {
            ConversationCommand::List { porcelain } => {
                self.on_show_conversations(porcelain).await?;
            }
            ConversationCommand::New => {
                self.handle_generate_conversation_id().await?;
            }
            ConversationCommand::Dump { id, html } => {
                self.validate_conversation_exists(&id).await?;

                let original_id = self.state.conversation_id;
                self.state.conversation_id = Some(id);

                self.spinner.start(Some("Dumping"))?;
                self.on_dump(html).await?;

                self.state.conversation_id = original_id;
            }
            ConversationCommand::Compact { id } => {
                self.validate_conversation_exists(&id).await?;

                let original_id = self.state.conversation_id;
                self.state.conversation_id = Some(id);

                self.spinner.start(Some("Compacting"))?;
                self.on_compaction().await?;

                self.state.conversation_id = original_id;
            }
            ConversationCommand::Delete { id } => {
                let conversation_id =
                    ConversationId::parse(&id).context(format!("Invalid conversation ID: {id}"))?;

                self.validate_conversation_exists(&conversation_id).await?;

                self.on_conversation_delete(conversation_id).await?;
            }
            ConversationCommand::Retry { id } => {
                self.validate_conversation_exists(&id).await?;

                let original_id = self.state.conversation_id;
                self.state.conversation_id = Some(id);

                self.spinner.start(None)?;
                self.on_message(None).await?;

                self.state.conversation_id = original_id;
            }
            ConversationCommand::Resume { id } => {
                self.validate_conversation_exists(&id).await?;

                self.state.conversation_id = Some(id);
                self.writeln_title(TitleFormat::info(format!("Resumed conversation: {id}")))?;
                // Interactive mode will be handled by the main loop
            }
            ConversationCommand::Show { id, md } => {
                let conversation = self.validate_conversation_exists(&id).await?;

                self.on_show_last_message(conversation, md).await?;
            }
            ConversationCommand::Info { id } => {
                let conversation = self.validate_conversation_exists(&id).await?;

                self.on_show_conv_info(conversation).await?;
            }
            ConversationCommand::Stats { id, porcelain } => {
                let conversation = self.validate_conversation_exists(&id).await?;

                self.on_show_conv_stats(conversation, porcelain).await?;
            }
            ConversationCommand::Clone { id, porcelain } => {
                let conversation = self.validate_conversation_exists(&id).await?;

                self.spinner.start(Some("Cloning"))?;
                self.on_clone_conversation(conversation, porcelain).await?;
                self.spinner.stop(None)?;
            }
            ConversationCommand::Rename { id, name } => {
                self.validate_conversation_exists(&id).await?;

                let name = name.trim().to_string();
                if name.is_empty() {
                    return Err(anyhow::anyhow!(
                        "Please provide a name for the conversation."
                    ));
                }
                self.api.rename_conversation(&id, name.clone()).await?;
                self.writeln_title(TitleFormat::info(format!(
                    "Conversation renamed to '{}'",
                    name.bold()
                )))?;
            }
        }
        Ok(())
    }

    async fn validate_conversation_exists(
        &self,
        conversation_id: &ConversationId,
    ) -> anyhow::Result<Conversation> {
        let conversation = self.api.conversation(conversation_id).await?;

        conversation.ok_or_else(|| anyhow::anyhow!("Conversation '{conversation_id}' not found"))
    }

    async fn on_conversation_delete(
        &mut self,
        conversation_id: ConversationId,
    ) -> anyhow::Result<()> {
        self.spinner.start(Some("Deleting conversation"))?;
        self.api.delete_conversation(&conversation_id).await?;
        self.spinner.stop(None)?;
        self.writeln_title(TitleFormat::debug(format!(
            "Successfully deleted conversation '{}'",
            conversation_id
        )))?;
        Ok(())
    }

    /// Handle `mcp login <name>` command.
    ///
    /// Triggers the OAuth authentication flow for the specified MCP server.
    /// Uses the API layer which delegates to rmcp's OAuth state machine
    /// for metadata discovery, dynamic registration, PKCE, and token exchange.
    async fn handle_mcp_login(&mut self, name: &str) -> anyhow::Result<()> {
        let server_name = furcode_api::ServerName::from(name.to_string());
        let config = self.api.read_mcp_config(None).await?;
        let server = config.mcp_servers.get(&server_name);

        match server {
            Some(furcode_domain::McpServerConfig::Http(http)) => {
                // Check auth status first
                let status = self.api.mcp_auth_status(&http.url).await?;
                if status == "authenticated" {
                    self.writeln_title(TitleFormat::info(
                        format!("MCP server '{}' is already authenticated. Use 'mcp logout {}' first to re-authenticate.", name, name)
                    ))?;
                    return Ok(());
                }

                // Force re-auth by removing any stale credentials
                let _ = self.api.mcp_logout(Some(&http.url)).await;

                // Run the OAuth flow (opens browser, waits for callback)
                match self.api.mcp_auth(&http.url).await {
                    Ok(()) => {
                        self.writeln_title(TitleFormat::info(format!(
                            "Successfully authenticated with MCP server '{}'",
                            name
                        )))?;
                        // Reload MCP to reconnect with new credentials
                        self.spinner.start(Some("Reloading MCPs"))?;
                        match self.api.reload_mcp().await {
                            Ok(()) => {
                                self.writeln_title(TitleFormat::info("MCP reloaded"))?;
                            }
                            Err(e) => {
                                self.writeln_title(TitleFormat::error(format!(
                                    "MCP reload failed: {}",
                                    e
                                )))?;
                            }
                        }
                    }
                    Err(e) => {
                        self.writeln_title(TitleFormat::error(format!(
                            "Authentication with MCP server '{}' failed: {}",
                            name, e
                        )))?;
                    }
                }
            }
            Some(_) => {
                self.writeln_title(TitleFormat::error(format!(
                    "MCP server '{}' is not an HTTP server (OAuth only applies to HTTP servers)",
                    name
                )))?;
            }
            None => {
                self.writeln_title(TitleFormat::error(format!(
                    "MCP server '{}' not found. Use 'mcp list' to see available servers.",
                    name
                )))?;
            }
        }
        Ok(())
    }

    /// Handle `mcp logout <name>` command.
    ///
    /// Removes stored OAuth credentials for the specified MCP server
    /// or all servers if "all" is specified.
    /// Automatically reloads MCPs after logout to reflect auth state change.
    async fn handle_mcp_logout(&mut self, name: &str) -> anyhow::Result<()> {
        if name == "all" {
            self.api.mcp_logout(None).await?;
            self.writeln_title(TitleFormat::info("Removed all MCP OAuth credentials"))?;
        } else {
            let server_name = furcode_api::ServerName::from(name.to_string());
            let config = self.api.read_mcp_config(None).await?;
            let server = config.mcp_servers.get(&server_name);

            match server {
                Some(furcode_domain::McpServerConfig::Http(http)) => {
                    self.api.mcp_logout(Some(&http.url)).await?;
                    self.writeln_title(TitleFormat::info(format!(
                        "Removed OAuth credentials for MCP server '{}'",
                        name
                    )))?;
                }
                Some(_) => {
                    self.writeln_title(TitleFormat::error(format!(
                        "MCP server '{}' is not an HTTP server",
                        name
                    )))?;
                    return Ok(());
                }
                None => {
                    self.writeln_title(TitleFormat::error(format!(
                        "MCP server '{}' not found. Use 'mcp list' to see available servers.",
                        name
                    )))?;
                    return Ok(());
                }
            }
        }

        // Reload MCPs to reflect auth state change
        self.spinner.start(Some("Reloading MCPs"))?;
        self.api.reload_mcp().await?;
        self.writeln_title(TitleFormat::info("MCP reloaded"))?;

        Ok(())
    }

    async fn handle_provider_command(
        &mut self,
        provider_group: crate::cli::ProviderCommandGroup,
    ) -> anyhow::Result<()> {
        use crate::cli::ProviderCommand;

        match provider_group.command {
            ProviderCommand::Login { provider } => {
                self.handle_provider_login(provider.as_ref()).await?;
            }
            ProviderCommand::Logout { provider } => {
                self.handle_provider_logout(provider.as_ref()).await?;
            }
            ProviderCommand::List { types } => {
                self.on_show_providers(provider_group.porcelain, types)
                    .await?;
            }
        }

        Ok(())
    }

    async fn handle_provider_login(
        &mut self,
        provider_id: Option<&ProviderId>,
    ) -> anyhow::Result<()> {
        // Get the provider to login to
        let any_provider = if let Some(id) = provider_id {
            // Specific provider requested
            self.api.get_provider(id).await?
        } else {
            // Fetch all providers for selection (no type filter, like shell :login)
            let providers = self.api.get_providers().await?;

            match self.select_provider_from_list(providers, "Provider", None, None)? {
                Some(provider) => provider,
                None => {
                    self.writeln_title(TitleFormat::info("Cancelled"))?;
                    return Ok(());
                }
            }
        };

        // For login, always configure (even if already configured) to allow
        // re-authentication
        let provider = match self
            .configure_provider(any_provider.id(), any_provider.auth_methods().to_vec())
            .await?
        {
            Some(provider) => provider,
            None => return Ok(()),
        };

        // Set as default and handle model selection
        self.finalize_provider_activation(provider, None).await
    }

    async fn handle_provider_logout(
        &mut self,
        provider_id: Option<&ProviderId>,
    ) -> anyhow::Result<bool> {
        // If provider_id is specified, logout from that specific provider
        if let Some(id) = provider_id {
            let provider = self.api.get_provider(id).await?;

            if !provider.is_configured() {
                return Err(anyhow::anyhow!("Provider '{id}' is not configured"));
            }
            self.api.remove_provider(id).await?;
            self.writeln_title(TitleFormat::debug(format!(
                "Successfully logged out from {id}"
            )))?;
            return Ok(true);
        }

        // Fetch and filter configured providers (like shell :logout filters to status
        // [yes])
        let configured_providers: Vec<AnyProvider> = self
            .api
            .get_providers()
            .await?
            .into_iter()
            .filter(|p| p.is_configured())
            .collect();

        if configured_providers.is_empty() {
            self.writeln_title(TitleFormat::info("No configured providers found"))?;
            return Ok(false);
        }

        match self.select_provider_from_list(configured_providers, "Provider", None, None)? {
            Some(provider) => {
                let provider_id = provider.id();
                self.api.remove_provider(&provider_id).await?;
                self.writeln_title(TitleFormat::debug(format!(
                    "Successfully logged out from {provider_id}"
                )))?;
                return Ok(true);
            }
            None => {
                self.writeln_title(TitleFormat::info("Cancelled"))?;
            }
        }

        Ok(false)
    }

    async fn handle_commit_command(
        &mut self,
        commit_group: CommitCommandGroup,
    ) -> anyhow::Result<CommitResult> {
        self.spinner.start(Some("Creating commit"))?;

        // Convert Vec<String> to Option<String> by joining with spaces
        let additional_context = if commit_group.text.is_empty() {
            None
        } else {
            Some(commit_group.text.join(" "))
        };

        // Handle the commit command
        let result = self
            .api
            .commit(
                commit_group.preview,
                commit_group.max_diff_size,
                commit_group.diff,
                additional_context,
            )
            .await;

        match result {
            Ok(result) => {
                self.spinner.stop(None)?;
                Ok(result)
            }
            Err(e) => {
                self.spinner.stop(None)?;
                Err(e)
            }
        }
    }

    /// Builds an Info structure for agents with their details
    async fn build_agents_info(&self, custom: bool) -> anyhow::Result<Info> {
        let mut agents = self.api.get_agents().await?;
        // Sort agents alphabetically by ID
        agents.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));

        // Filter agents based on custom flag
        if custom {
            agents.retain(|agent| agent.path.is_some());
        }

        let mut info = Info::new();

        for agent in agents.iter() {
            let id = agent.id.as_str().to_string();
            let title = agent
                .title
                .as_deref()
                .map(|title| title.lines().collect::<Vec<_>>().join(" "));

            // Get provider and model for this agent
            let provider_name = match self.get_provider(Some(agent.id.clone())).await {
                Ok(p) => p.id.to_string(),
                Err(e) => format!("Error: [{}]", e),
            };

            let model_name = agent.model.as_str().to_string();

            let reasoning = if agent
                .reasoning
                .as_ref()
                .and_then(|a| a.enabled)
                .unwrap_or_default()
            {
                status::YES
            } else {
                status::NO
            };

            let location = agent
                .path
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| markers::BUILT_IN.to_string());

            info = info
                .add_title(id.to_case(Case::UpperSnake))
                .add_key_value("Id", id)
                .add_key_value("Title", title)
                .add_key_value("Location", location)
                .add_key_value("Provider", provider_name)
                .add_key_value("Model", model_name)
                .add_key_value("Reasoning Enabled", reasoning);
        }

        Ok(info)
    }

    async fn on_show_agents(&mut self, porcelain: bool, custom: bool) -> anyhow::Result<()> {
        let agents = self.api.get_agent_infos().await?;

        if agents.is_empty() {
            return Ok(());
        }

        let info = self.build_agents_info(custom).await?;

        if porcelain {
            let porcelain = Porcelain::from(&info)
                .drop_col(0)
                .truncate(3, 60)
                .uppercase_headers();
            self.writeln(porcelain)?;
        } else {
            self.writeln(info)?;
        }

        Ok(())
    }

    /// Lists all the providers
    async fn on_show_providers(
        &mut self,
        porcelain: bool,
        types: Vec<furcode_domain::ProviderType>,
    ) -> anyhow::Result<()> {
        let mut providers = self.api.get_providers().await?;

        // Filter by type if specified
        if !types.is_empty() {
            providers.retain(|p| types.contains(p.provider_type()));
        }

        if providers.is_empty() {
            return Ok(());
        }

        let mut info = Info::new();

        for provider in providers.iter() {
            let id: &str = &provider.id();
            let display_name = provider.id().to_string();
            let domain = if let Some(url) = provider.url() {
                url.domain().map(|d| d.to_string()).unwrap_or_default()
            } else {
                markers::EMPTY.to_string()
            };
            let configured = provider.is_configured();
            info = info
                .add_title(id.to_case(Case::UpperSnake))
                .add_key_value("name", display_name)
                .add_key_value("id", id)
                .add_key_value("host", domain);
            if configured {
                info = info.add_key_value("logged in", status::YES);
            };
        }

        if porcelain {
            let porcelain = Porcelain::from(&info).drop_col(0).uppercase_headers();
            self.writeln(porcelain)?;
        } else {
            self.writeln(info)?;
        }

        Ok(())
    }

    /// Lists all the models
    async fn on_show_models(&mut self, porcelain: bool) -> anyhow::Result<()> {
        self.spinner.start(Some("Fetching Models"))?;

        let mut all_provider_models = match self.api.get_all_provider_models().await {
            Ok(provider_models) => provider_models,
            Err(err) => {
                self.spinner.stop(None)?;
                return Err(err);
            }
        };

        if all_provider_models.is_empty() {
            return Ok(());
        }

        // Sort models and then providers
        all_provider_models
            .iter_mut()
            .for_each(|pm| pm.models.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str())));
        all_provider_models.sort_by(|a, b| a.provider_id.as_ref().cmp(b.provider_id.as_ref()));

        let mut info = Info::new();
        for pm in &all_provider_models {
            let provider_id: &str = &pm.provider_id;
            let provider_display = pm.provider_id.to_string();
            for model in &pm.models {
                let id = model.id.to_string();
                info = info
                    .add_title(&id)
                    .add_key_value("Model", model.name.as_ref().unwrap_or(&id))
                    .add_key_value("Provider", &provider_display)
                    .add_key_value("Provider Id", provider_id);

                // Add context length if available, otherwise use "unknown"
                if let Some(limit) = model.context_length {
                    let context = if limit >= 1_000_000 {
                        format!("{}M", limit / 1_000_000)
                    } else if limit >= 1000 {
                        format!("{}k", limit / 1000)
                    } else {
                        format!("{limit}")
                    };
                    info = info.add_key_value("Context Window", context);
                } else {
                    info = info.add_key_value("Context Window", markers::EMPTY)
                }

                // Add tools support indicator if explicitly supported
                if let Some(supported) = model.tools_supported {
                    info = info.add_key_value(
                        "Tool Supported",
                        if supported { status::YES } else { status::NO },
                    )
                } else {
                    info = info.add_key_value("Tools", markers::EMPTY)
                }

                // Add image modality support indicator
                let supports_image = model
                    .input_modalities
                    .contains(&furcode_domain::InputModality::Image);
                info = info.add_key_value(
                    "Image",
                    if supports_image {
                        status::YES
                    } else {
                        status::NO
                    },
                );
            }
        }

        if porcelain {
            self.writeln(Porcelain::from(&info).truncate(1, 40).uppercase_headers())?;
        } else {
            self.writeln(info)?;
        }

        Ok(())
    }

    async fn commands_porcelain(&self) -> Result<Porcelain> {
        let custom_commands = self.api.get_commands().await?;
        let mut info = Info::new();

        for cmd in AppCommand::iter().filter(|c| !c.is_internal() && !c.is_agent_switch()) {
            info = info
                .add_title(cmd.name())
                .add_key_value("type", CommandType::Command)
                .add_key_value("description", cmd.usage());
        }

        info = info
            .add_title("ask")
            .add_key_value("type", CommandType::Agent)
            .add_key_value(
                "description",
                "Research and investigation agent [alias for: sage]",
            )
            .add_title("plan")
            .add_key_value("type", CommandType::Agent)
            .add_key_value(
                "description",
                "Planning and strategy agent [alias for: muse]",
            );

        let agent_infos = self.api.get_agent_infos().await?;
        for agent_info in agent_infos {
            let title = agent_info
                .title
                .map(|title| title.lines().collect::<Vec<_>>().join(" "));
            info = info
                .add_title(agent_info.id.to_string())
                .add_key_value("type", CommandType::Agent)
                .add_key_value("description", title);
        }

        for command in custom_commands {
            info = info
                .add_title(command.name.clone())
                .add_key_value("type", CommandType::Custom)
                .add_key_value("description", command.description.clone());
        }

        Ok(Porcelain::from(&info)
            .uppercase_headers()
            .sort_by(&[1, 0])
            .to_case(&[1], Case::UpperSnake)
            .map_col(0, |col| {
                if col.as_deref() == Some(headers::ID) {
                    Some("COMMAND".to_string())
                } else {
                    col
                }
            }))
    }

    /// Lists all the commands
    async fn on_show_commands(&mut self, porcelain: bool) -> anyhow::Result<()> {
        let custom_commands = self.api.get_commands().await?;

        if porcelain {
            self.writeln(self.commands_porcelain().await?)?;
        } else {
            // Non-porcelain: render in the same flat format as :help in the REPL.
            let command_manager = FurcodeCommandManager::default();
            command_manager.register_all(custom_commands);
            let info = Info::from(&command_manager);
            self.writeln(info)?;
        }

        Ok(())
    }

    /// Lists only custom commands (used by `furcode run`)
    async fn on_show_custom_commands(&mut self, porcelain: bool) -> anyhow::Result<()> {
        let custom_commands = self.api.get_commands().await?;
        let mut info = Info::new();

        for command in custom_commands {
            info = info
                .add_title(command.name.clone())
                .add_key_value("description", command.description.clone());
        }

        if porcelain {
            let porcelain = Porcelain::from(&info).uppercase_headers();
            self.writeln(porcelain)?;
        } else {
            self.writeln(info)?;
        }

        Ok(())
    }

    /// Sets `activity_display` mode (`compact` or `out_loud`) and persists config.
    async fn on_activity_display(&mut self, mode: Option<String>) -> anyhow::Result<()> {
        let Some(raw) = mode.filter(|s| !s.trim().is_empty()) else {
            self.writeln_title(
                TitleFormat::info("Activity display")
                    .sub_title(format!(
                        "{} — use /activity compact | out_loud",
                        self.config.activity_display
                    )),
            )?;
            return Ok(());
        };
        let Some(parsed) = ActivityDisplay::parse(&raw) else {
            self.writeln_title(TitleFormat::error(format!(
                "Unknown mode '{raw}'. Use compact or out_loud"
            )))?;
            return Ok(());
        };
        self.config.activity_display = parsed;
        self.config.write()?;
        let hint = match parsed {
            ActivityDisplay::Compact => {
                "animated status, last 2 live lines, one-line summaries"
            }
            ActivityDisplay::OutLoud => "stream full thinking and tool output",
        };
        self.writeln_title(
            TitleFormat::action(parsed.to_string()).sub_title(hint),
        )?;
        Ok(())
    }

    /// Saves a durable harness learning to user config (`learnings.md`).
    ///
    /// When `content` is `None`, prompts the user interactively.
    async fn on_learn(&mut self, content: Option<String>) -> anyhow::Result<()> {
        let text = if let Some(text) = content {
            text
        } else {
            let input = FurcodeWidget::input("Learning to remember")
                .allow_empty(false)
                .prompt()?;
            match input {
                Some(text) if !text.trim().is_empty() => text.trim().to_string(),
                _ => {
                    self.writeln_title(TitleFormat::info("Cancelled"))?;
                    return Ok(());
                }
            }
        };

        let output = self.api.remember(text, None).await?;
        let path = format_display_path(&output.path, &self.api.environment().cwd);
        if output.duplicated {
            self.writeln_title(
                TitleFormat::info("Already known")
                    .sub_title(format!("Similar learning already in {path}")),
            )?;
        } else {
            self.writeln_title(
                TitleFormat::info("Learned")
                    .sub_title(format!("Saved to {path} (active next turn, no recompile)")),
            )?;
        }
        Ok(())
    }

    /// Lists available skills
    async fn on_show_skills(&mut self, porcelain: bool, custom: bool) -> anyhow::Result<()> {
        let skills = self.api.get_skills().await?;

        // Filter skills based on custom flag
        let skills = if custom {
            skills
                .into_iter()
                .filter(|skill| skill.path.is_some())
                .collect()
        } else {
            skills
        };

        let mut info = Info::new();
        let env = self.api.environment();

        for skill in skills {
            info = info
                .add_title(skill.name.clone().to_case(Case::Sentence).to_uppercase())
                .add_key_value("name", skill.name);

            if let Some(path) = skill.path {
                info = info.add_key_value("path", format_display_path(&path, &env.cwd));
            }

            info = info.add_key_value("description", skill.description);
        }

        if porcelain {
            let porcelain = Porcelain::from(&info)
                .drop_col(0)
                .truncate(2, 60)
                .uppercase_headers();
            self.writeln(porcelain)?;
        } else {
            self.writeln(info)?;
        }

        Ok(())
    }

    /// Lists files and directories in the current workspace.
    ///
    /// Uses the same `Walker::max_all()` configuration as the REPL file picker
    /// and the shell plugin (`fd --type f --type d --hidden --exclude .git`):
    /// hidden files included, respects `.gitignore`, directories suffixed with
    /// `/`.
    async fn on_list_files(&mut self, porcelain: bool) -> anyhow::Result<()> {
        let env = self.api.environment();
        let files = Walker::max_all()
            .cwd(env.cwd.clone())
            .get()
            .await
            .context("Failed to walk workspace files")?;

        if porcelain {
            for file in files {
                self.writeln(file.path)?;
            }
        } else {
            let mut info = Info::new();
            for file in &files {
                info = info.add_key_value("path", file.path.clone());
            }
            self.writeln(info)?;
        }

        Ok(())
    }

    /// Lists current configuration values
    async fn on_show_config(&mut self, porcelain: bool) -> anyhow::Result<()> {
        // Get the effective resolved config
        let config = &self.config;

        // Serialize to TOML pretty format
        let config_toml = toml_edit::ser::to_string_pretty(config)
            .map_err(|e| anyhow::anyhow!("Failed to serialize config: {}", e))?;

        if porcelain {
            // For porcelain mode, output raw TOML without highlighting
            self.writeln(config_toml)?;
        } else {
            // For human-readable mode, add a title and syntax-highlight the TOML
            self.writeln("\nCONFIGURATION\n".bold().dimmed())?;
            let highlighted =
                furcode_display::SyntaxHighlighter::default().highlight(&config_toml, "toml");
            self.writeln(highlighted)?;
        }

        Ok(())
    }

    /// Displays available tools for the current agent
    async fn on_show_tools(&mut self, agent_id: AgentId, porcelain: bool) -> anyhow::Result<()> {
        self.spinner.start(Some("Loading"))?;
        let all_tools = self.api.get_tools().await?;
        let agents = self.api.get_agents().await?;
        let agent = agents.into_iter().find(|agent| agent.id == agent_id);
        let agent_tools = if let Some(agent) = agent {
            let resolver = ToolResolver::new(all_tools.clone().into());
            resolver
                .resolve(&agent)
                .into_iter()
                .map(|def| def.name.clone())
                .collect()
        } else {
            Vec::new()
        };

        let info = format_tools(&agent_tools, &all_tools);
        if porcelain {
            self.writeln(
                Porcelain::from(&info)
                    .into_long()
                    .drop_col(1)
                    .uppercase_headers(),
            )?;
        } else {
            self.writeln(info)?;
        }

        Ok(())
    }

    /// Displays all MCP servers with their available tools
    async fn on_show_mcp_servers(&mut self, porcelain: bool) -> anyhow::Result<()> {
        self.spinner.start(Some("Loading MCP servers"))?;
        let mcp_servers = self.api.read_mcp_config(None).await?;
        let all_tools = self.api.get_tools().await?;

        let mut info = Info::new();

        for (name, server) in mcp_servers.mcp_servers {
            let label = match server {
                furcode_domain::McpServerConfig::Stdio(_) => "Command",
                furcode_domain::McpServerConfig::Http(_) => "URL",
            };

            info = info
                .add_title(name.to_uppercase())
                .add_key_value("Type", server.server_type())
                .add_key_value(label, format_mcp_server(&server));

            // Add headers for HTTP servers if present
            if let Some(headers) = format_mcp_headers(&server) {
                info = info.add_key_value("Headers", headers);
            }

            if server.is_disabled() {
                info = info.add_key_value("Status", status::NO);
            }

            // Add tools for this MCP server
            if let Some(tools) = all_tools.mcp.get_servers().get(&name)
                && !tools.is_empty()
            {
                info = info.add_key_value("Tools", tools.len().to_string());
                for tool in tools {
                    info = info.add_value(tool.name.to_string());
                }
            }
        }

        if porcelain {
            self.writeln(Porcelain::from(&info).uppercase_headers().truncate(3, 60))?;
        } else {
            self.writeln(info)?;
        }

        // Show failed MCP servers
        if !porcelain && !all_tools.mcp.get_failures().is_empty() {
            self.writeln("MCP FAILURES\n".dimmed().bold())?;
            for error in all_tools.mcp.get_failures().values() {
                let error = style(error).red();
                self.writeln(error)?;
            }
        }

        Ok(())
    }

    async fn on_info(
        &mut self,
        porcelain: bool,
        conversation_id: Option<ConversationId>,
    ) -> anyhow::Result<()> {
        let mut info = Info::new();

        // Fetch conversation
        let conversation = match conversation_id {
            Some(conversation_id) => self.api.conversation(&conversation_id).await.ok().flatten(),
            None => None,
        };

        // Fetch agent
        let agent = self.api.get_active_agent().await;

        // Fetch model (resolved with default model if unset)
        let model = self.get_agent_model(agent.clone()).await;

        // Fetch agent-specific provider or default provider if unset
        let agent_provider = self.get_provider(agent.clone()).await.ok();

        // Fetch default provider (could be different from the set provider)
        let default_provider = self.api.get_default_provider().await.ok();

        // Add agent information
        info = info.add_title("AGENT");
        if let Some(agent) = agent {
            info = info.add_key_value("ID", agent.as_str().to_uppercase());
        }

        // Add model information if available
        if let Some(model) = model {
            info = info.add_key_value("Model", model.as_str());
        }

        // Add provider information
        match (default_provider, agent_provider) {
            (Some(default), Some(agent_specific)) if default.id != agent_specific.id => {
                // Show both providers if they're different
                info = info.add_key_value("Agent Provider (URL)", agent_specific.url.as_str());
                if let Some(api_key) = agent_specific.api_key() {
                    info = info.add_key_value("Agent API Key", truncate_key(api_key.as_str()));
                }

                info = info.add_key_value("Default Provider (URL)", default.url.as_str());
                if let Some(api_key) = default.api_key() {
                    info = info.add_key_value("Default API Key", truncate_key(api_key.as_str()));
                }
            }
            (Some(provider), _) | (_, Some(provider)) => {
                // Show single provider (either default or agent-specific)
                info = info.add_key_value("Provider (URL)", provider.url.as_str());
                if let Some(api_key) = provider.api_key() {
                    info = info.add_key_value("API Key", truncate_key(api_key.as_str()));
                }
            }
            _ => {
                // No provider available
            }
        }

        // Add conversation information if available
        if let Some(conversation) = conversation {
            info = info.extend(Info::from(&conversation));
        } else {
            info = info.extend(Info::new().add_title("CONVERSATION").add_key("ID"));
        }

        if porcelain {
            self.writeln(Porcelain::from(&info).into_long().skip(1))?;
        } else {
            self.writeln(info)?;
        }

        Ok(())
    }

    /// Install the VS Code extension
    async fn on_vscode_extension_install(&mut self) -> anyhow::Result<()> {
        self.spinner
            .start(Some("Installing VS Code extension"))?;

        match crate::vscode::install_extension() {
            Ok(true) => {
                self.spinner.stop(None)?;
                self.writeln_title(TitleFormat::info(
                    "VS Code extension installed successfully",
                ))?;
            }
            Ok(false) => {
                self.spinner.stop(None)?;
                self.writeln_title(TitleFormat::error(
                    "Failed to install VS Code extension.",
                ))?;
            }
            Err(e) => {
                self.spinner.stop(None)?;
                self.writeln_title(TitleFormat::error(format!(
                    "Failed to install VS Code extension: {e}"
                )))?;
            }
        }

        Ok(())
    }

    /// Handle the cmd command - generates shell command from natural language
    async fn on_cmd(&mut self, prompt: UserPrompt) -> anyhow::Result<()> {
        self.spinner.start(Some("Generating"))?;

        match self.api.generate_command(prompt).await {
            Ok(command) => {
                self.spinner.stop(None)?;
                self.writeln(command)?;
                Ok(())
            }
            Err(err) => {
                self.spinner.stop(None)?;
                Err(err)
            }
        }
    }

    async fn list_conversations(&mut self) -> anyhow::Result<()> {
        self.spinner.start(Some("Loading Conversations"))?;
        let max_conversations = self.config.max_conversations;
        let conversations = self.api.get_conversations(Some(max_conversations)).await?;
        let conversations = Self::user_initiated_conversations(conversations);
        self.spinner.stop(None)?;

        if conversations.is_empty() {
            self.writeln_title(TitleFormat::error(
                "No conversations found in this workspace.",
            ))?;
            return Ok(());
        }

        if let Some(conversation) = ConversationSelector::select_conversation(
            &conversations,
            self.state.conversation_id,
            None,
        )
        .await?
        {
            let conversation_id = conversation.id;
            self.state.conversation_id = Some(conversation_id);

            // Show conversation content
            self.on_show_last_message(conversation, false).await?;

            // Print log about conversation switching
            self.writeln_title(TitleFormat::info(format!(
                "Switched to conversation {}",
                conversation_id.into_string().bold()
            )))?;

            // Show conversation info
            self.on_info(false, Some(conversation_id)).await?;
        }
        Ok(())
    }

    async fn on_show_conversations(&mut self, porcelain: bool) -> anyhow::Result<()> {
        let max_conversations = self.config.max_conversations;
        let conversations = self.api.get_conversations(Some(max_conversations)).await?;
        let conversations = Self::user_initiated_conversations(conversations);

        if conversations.is_empty() {
            return Ok(());
        }

        let mut info = Info::new();

        for conv in conversations.into_iter() {
            if conv.context.is_none() {
                continue;
            }

            let title = conv
                .title
                .as_deref()
                .map(|t| t.to_string())
                .unwrap_or_else(|| markers::EMPTY.to_string());

            // Format time using humantime library (same as conversation_selector.rs)
            let duration = chrono::Utc::now().signed_duration_since(
                conv.metadata.updated_at.unwrap_or(conv.metadata.created_at),
            );
            let duration =
                std::time::Duration::from_secs((duration.num_minutes() * 60).max(0) as u64);
            let time_ago = if duration.is_zero() {
                "now".to_string()
            } else {
                format!("{} ago", humantime::format_duration(duration))
            };

            // Add conversation: Title=<title>, Updated=<time_ago>, with ID as section title
            info = info
                .add_title(conv.id)
                .add_key_value("Title", title)
                .add_key_value("Updated", time_ago);
        }

        // In porcelain mode, skip the top-level "SESSIONS" title
        if porcelain {
            let porcelain = Porcelain::from(&info)
                .drop_col(3)
                .truncate(1, 60)
                .uppercase_headers();
            self.writeln(porcelain)?;
        } else {
            self.writeln(info)?;
        }

        Ok(())
    }

    fn user_initiated_conversations(conversations: Vec<Conversation>) -> Vec<Conversation> {
        let related_ids: HashSet<ConversationId> = conversations
            .iter()
            .flat_map(Conversation::related_conversation_ids)
            .collect();

        conversations
            .into_iter()
            .filter(|conversation| {
                conversation
                    .context
                    .as_ref()
                    .and_then(|context| context.initiator.as_deref())
                    .is_none_or(|initiator| initiator == "user")
                    && !related_ids.contains(&conversation.id)
            })
            .collect()
    }

    async fn on_command(&mut self, command: AppCommand) -> anyhow::Result<bool> {
        match command {
            AppCommand::Conversations { id } => {
                if let Some(raw_id) = id {
                    let conversation_id = ConversationId::parse(&raw_id)
                        .context(format!("Invalid conversation ID: {raw_id}"))?;
                    let conversation = self.validate_conversation_exists(&conversation_id).await?;
                    self.state.conversation_id = Some(conversation_id);
                    self.on_show_last_message(conversation, false).await?;
                    self.writeln_title(TitleFormat::info(format!(
                        "Switched to conversation {}",
                        conversation_id.into_string().bold()
                    )))?;
                    self.on_info(false, Some(conversation_id)).await?;
                } else {
                    self.list_conversations().await?;
                }
            }
            AppCommand::ConversationTree => {
                let conversation_id = self
                    .state
                    .conversation_id
                    .ok_or_else(|| anyhow::anyhow!("No active conversation"))?;
                let parent = self.validate_conversation_exists(&conversation_id).await?;
                let children = self.fetch_related_conversations(&parent).await;

                if children.is_empty() {
                    self.writeln_title(TitleFormat::info("No child conversations found."))?;
                } else if let Some(conversation) = ConversationSelector::select_conversation(
                    &children,
                    self.state.conversation_id,
                    None,
                )
                .await?
                {
                    let conversation_id = conversation.id;
                    self.state.conversation_id = Some(conversation_id);
                    self.on_show_last_message(conversation, false).await?;
                    self.writeln_title(TitleFormat::info(format!(
                        "Switched to conversation {}",
                        conversation_id.into_string().bold()
                    )))?;
                    self.on_info(false, Some(conversation_id)).await?;
                }
            }
            AppCommand::Compact => {
                self.spinner.start(Some("Compacting"))?;
                self.on_compaction().await?;
            }
            AppCommand::Delete => {
                self.handle_delete_conversation().await?;
            }
            AppCommand::Rename { ref name } => {
                self.handle_rename_conversation(name.join(" ")).await?;
            }
            AppCommand::Dump { html, .. } => {
                self.spinner.start(Some("Dumping"))?;
                self.on_dump(html).await?;
            }
            AppCommand::New => {
                self.on_new().await?;
            }
            AppCommand::Info => {
                self.on_info(false, self.state.conversation_id).await?;
            }
            AppCommand::Usage => {
                self.on_usage().await?;
            }
            AppCommand::Message(ref content) => {
                self.spinner.start(None)?;
                self.on_message(Some(content.clone())).await?;
            }
            AppCommand::Forge => {
                self.on_agent_change(AgentId::FORGE).await?;
            }
            AppCommand::Muse => {
                self.on_agent_change(AgentId::MUSE).await?;
            }
            AppCommand::Sage => {
                self.on_agent_change(AgentId::SAGE).await?;
            }
            AppCommand::Help => {
                let info = Info::from(self.command.as_ref());
                self.writeln(info)?;
            }
            AppCommand::Tools => {
                let agent_id = self.api.get_active_agent().await.unwrap_or_default();
                self.on_show_tools(agent_id, false).await?;
            }
            AppCommand::Update => {
                on_update(self.api.clone(), None).await;
            }
            AppCommand::Exit => {
                self.print_exit_resume_hint()?;
                return Ok(true);
            }

            AppCommand::Custom(event) => {
                self.spinner.start(None)?;
                self.on_custom_event(event.into()).await?;
            }
            AppCommand::Model => {
                self.on_model_selection(None).await?;
            }
            AppCommand::Shell(ref command) => {
                self.api.execute_shell_command_raw(command).await?;
            }
            AppCommand::Commit { max_diff_size, .. } => {
                let args = CommitCommandGroup {
                    preview: false,
                    max_diff_size: max_diff_size.or(Some(100_000)),
                    diff: None,
                    text: Vec::new(),
                };
                let result = self.handle_commit_command(args).await?;
                if !result.git_output.is_empty() {
                    self.writeln(result.git_output.trim_end())?;
                } else {
                    self.writeln(result.message.trim_end())?;
                }
            }
            AppCommand::Agent => {
                if let Some(selected_agent) = self.select_agent(None).await? {
                    self.on_agent_change(selected_agent).await?;
                }
            }
            AppCommand::Login => {
                self.handle_provider_login(None).await?;
            }
            AppCommand::Logout => {
                return self.handle_provider_logout(None).await;
            }
            AppCommand::Retry => {
                self.spinner.start(None)?;
                self.on_message(None).await?;
            }
            AppCommand::Index => {
                if !self.config.cloud_services_enabled() {
                    self.writeln_title(TitleFormat::info("Cloud services disabled").sub_title(
                        "Workspace indexing requires services_enabled=true and services_url.",
                    ))?;
                } else {
                    let working_dir = self.state.cwd.clone();
                    self.on_index(working_dir, false).await?;
                }
            }
            AppCommand::AgentSwitch(agent_id) => {
                // Validate that the agent exists by checking against loaded agents
                let agents = self.api.get_agent_infos().await?;
                let agent_exists = agents.iter().any(|agent| agent.id.as_str() == agent_id);

                if agent_exists {
                    self.on_agent_change(AgentId::new(agent_id)).await?;
                } else {
                    return Err(anyhow::anyhow!(
                        "Agent '{agent_id}' not found or unavailable"
                    ));
                }
            }
            AppCommand::Config => {
                self.on_show_config(false).await?;
            }
            AppCommand::ConfigModel => {
                self.on_model_selection(None).await?;
            }
            AppCommand::ConfigReload => {
                self.writeln_title(TitleFormat::info(
                    "No session overrides in REPL mode. Use :model to switch the active model.",
                ))?;
            }
            AppCommand::ReasoningEffort => {
                self.on_reasoning_effort_selection(false).await?;
            }
            AppCommand::ConfigReasoningEffort => {
                self.on_reasoning_effort_selection(true).await?;
            }
            AppCommand::ConfigCommitModel => {
                self.on_config_commit_model().await?;
            }
            AppCommand::ConfigSuggestModel => {
                self.on_config_suggest_model().await?;
            }
            AppCommand::ConfigEdit => {
                self.on_config_edit().await?;
            }
            AppCommand::Skill => {
                self.on_show_skills(false, false).await?;
            }
            AppCommand::Learn { content } => {
                let text = content.join(" ").trim().to_string();
                self.on_learn(if text.is_empty() { None } else { Some(text) })
                    .await?;
            }
            AppCommand::Activity { mode } => {
                self.on_activity_display(mode).await?;
            }
            AppCommand::Edit { content } => {
                let initial = if content.is_empty() {
                    None
                } else {
                    Some(content.join(" ").trim().to_string())
                };
                self.on_edit_buffer(initial).await?;
            }
            AppCommand::CommitPreview => {
                let args = CommitCommandGroup {
                    preview: true,
                    max_diff_size: Some(100_000),
                    diff: None,
                    text: Vec::new(),
                };
                let result = self.handle_commit_command(args).await?;
                let flags = if result.has_staged_files { "" } else { " -a" };
                let commit_command = format!("!git commit{flags} -m '{}'", result.message);
                self.console.set_buffer(commit_command);
            }
            AppCommand::Suggest { description } => {
                let desc = if description.is_empty() {
                    None
                } else {
                    Some(description.join(" ").trim().to_string())
                };
                self.on_suggest(desc).await?;
            }
            AppCommand::Clone { id } => {
                self.on_slash_clone(id).await?;
            }
            AppCommand::ConversationRename { name } => {
                let args = if name.is_empty() {
                    None
                } else {
                    Some(name.join(" ").trim().to_string())
                };
                self.on_slash_conversation_rename(args).await?;
            }
            AppCommand::Copy => {
                self.on_copy().await?;
            }
            AppCommand::WorkspaceSync
            | AppCommand::WorkspaceStatus
            | AppCommand::WorkspaceInfo
            | AppCommand::WorkspaceInit => {
                if !self.config.cloud_services_enabled() {
                    self.writeln_title(TitleFormat::info("Cloud services disabled").sub_title(
                        "Workspace indexing/sync requires services_enabled=true and services_url.",
                    ))?;
                } else {
                    match command {
                        AppCommand::WorkspaceSync => {
                            let working_dir = self.state.cwd.clone();
                            self.on_index(working_dir, true).await?;
                        }
                        AppCommand::WorkspaceStatus => {
                            let cwd = self.state.cwd.clone();
                            self.on_workspace_status(cwd, false).await?;
                        }
                        AppCommand::WorkspaceInfo => {
                            let cwd = self.state.cwd.clone();
                            self.on_workspace_info(cwd).await?;
                        }
                        AppCommand::WorkspaceInit => {
                            let cwd = self.state.cwd.clone();
                            self.on_workspace_init(cwd, false).await?;
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }

        Ok(false)
    }
    async fn on_compaction(&mut self) -> Result<(), anyhow::Error> {
        let conversation_id = self.init_conversation().await?;
        let compaction_result = self.api.compact_conversation(&conversation_id).await?;
        let token_reduction = compaction_result.token_reduction_percentage();
        let message_reduction = compaction_result.message_reduction_percentage();
        let content = TitleFormat::action(format!(
            "Context size reduced by {token_reduction:.1}% (tokens), {message_reduction:.1}% (messages)"
        ));
        self.writeln_title(content)?;
        Ok(())
    }

    async fn handle_delete_conversation(&mut self) -> anyhow::Result<()> {
        let conversation_id = self.init_conversation().await?;
        self.on_conversation_delete(conversation_id).await?;
        Ok(())
    }

    async fn handle_rename_conversation(&mut self, name: String) -> anyhow::Result<()> {
        let conversation_id = self.init_conversation().await?;
        self.api
            .rename_conversation(&conversation_id, name.clone())
            .await?;
        self.writeln_title(TitleFormat::info(format!(
            "Conversation renamed to '{}'",
            name.bold()
        )))?;
        Ok(())
    }

    /// Selects and sets the reasoning effort level interactively.
    ///
    /// # Arguments
    /// * `global` - If true, persists the change to the global config file. If
    ///   false, applies to the session (REPL has no separate session scope, so
    ///   this always writes to the config).
    async fn on_reasoning_effort_selection(&mut self, global: bool) -> anyhow::Result<()> {
        use std::str::FromStr;

        let prompt = if global {
            "Config Reasoning Effort"
        } else {
            "Reasoning Effort"
        };

        let selected = self.select_reasoning_effort(prompt, None).await?;

        if let Some(effort_str) = selected {
            let effort = furcode_domain::Effort::from_str(&effort_str)
                .map_err(|_| anyhow::anyhow!("Invalid effort level: {effort_str}"))?;
            self.api
                .update_config(vec![ConfigOperation::SetReasoningEffort(effort.clone())])
                .await?;
            self.writeln_title(
                TitleFormat::action(effort_str).sub_title("is now the reasoning effort"),
            )?;
        }

        Ok(())
    }

    async fn select_reasoning_effort(
        &self,
        prompt: &str,
        query: Option<String>,
    ) -> anyhow::Result<Option<String>> {
        let effort_levels = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
        let current_effort = self.api.get_reasoning_effort().await.ok().flatten();
        let current_str = current_effort.as_ref().map(|e| e.to_string());
        let rows = effort_levels
            .iter()
            .map(|level| SelectRow::new(*level, *level))
            .collect();

        Ok(self
            .select_raw_row(prompt, query, rows, 0, current_str)?
            .map(|row| row.raw))
    }

    /// Selects and sets the commit model via interactive model picker.
    async fn on_config_commit_model(&mut self) -> anyhow::Result<()> {
        let selection = self.select_model(None, None).await?;
        if let Some((model, provider_id)) = selection {
            let commit_config = furcode_domain::ModelConfig::new(provider_id.clone(), model.clone());
            self.api
                .update_config(vec![ConfigOperation::SetCommitConfig(Some(commit_config))])
                .await?;
            self.writeln_title(TitleFormat::action(model.as_str()).sub_title(format!(
                "is now the commit model for provider '{provider_id}'"
            )))?;
        }
        Ok(())
    }

    /// Selects and sets the suggest model via interactive model picker.
    async fn on_config_suggest_model(&mut self) -> anyhow::Result<()> {
        let selection = self.select_model(None, None).await?;
        if let Some((model, provider_id)) = selection {
            let suggest_config = furcode_domain::ModelConfig::new(provider_id.clone(), model.clone());
            self.api
                .update_config(vec![ConfigOperation::SetSuggestConfig(suggest_config)])
                .await?;
            self.writeln_title(TitleFormat::action(model.as_str()).sub_title(format!(
                "is now the suggest model for provider '{provider_id}'"
            )))?;
        }
        Ok(())
    }

    /// Opens the global config file in the system editor.
    async fn on_config_edit(&mut self) -> anyhow::Result<()> {
        let config_path = furcode_config::ConfigReader::config_path();

        // Ensure parent directory exists
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Create config file if it does not exist
        if !config_path.exists() {
            std::fs::File::create(&config_path)?;
        }

        let editor = std::env::var("FURCODE_EDITOR")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "nano".to_string());
        let editor_binary = editor
            .split_whitespace()
            .next()
            .unwrap_or("nano")
            .to_string();

        let status = std::process::Command::new(&editor_binary)
            .arg(&config_path)
            .status()
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to open editor '{}': {}. Set FURCODE_EDITOR or EDITOR.",
                    editor_binary,
                    e
                )
            })?;

        if !status.success() {
            return Err(anyhow::anyhow!("Editor exited with error code: {}", status));
        }

        self.writeln_title(TitleFormat::info(format!(
            "Config saved: {}",
            config_path.display()
        )))?;

        Ok(())
    }

    /// Opens an external editor to compose a prompt and sets it in the REPL
    /// buffer on exit.
    ///
    /// # Arguments
    /// * `initial` - Optional text to pre-populate the editor with.
    async fn on_edit_buffer(&mut self, initial: Option<String>) -> anyhow::Result<()> {
        use std::io::Write as _;

        let editor = std::env::var("FURCODE_EDITOR")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "nano".to_string());

        // Split the editor string into binary + pre-configured flags
        // (e.g. "code --wait" → binary="code", extra_args=["--wait"])
        let mut editor_parts = editor.split_whitespace();
        let editor_binary = editor_parts.next().unwrap_or("nano").to_string();
        let editor_flags: Vec<&str> = editor_parts.collect();

        // Create .furcode directory for the temp file
        let furcode_dir = self.state.cwd.join(".furcode");
        std::fs::create_dir_all(&furcode_dir)?;
        let temp_file = furcode_dir.join("FURCODE_EDITMSG.md");

        // Write initial content
        let mut file = std::fs::File::create(&temp_file)?;
        if let Some(text) = initial {
            file.write_all(text.as_bytes())?;
        }
        drop(file);

        let status = std::process::Command::new(&editor_binary)
            .args(&editor_flags)
            .arg(&temp_file)
            .status()
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to open editor '{}': {}. Set FURCODE_EDITOR or EDITOR.",
                    editor_binary,
                    e
                )
            })?;

        if !status.success() {
            return Err(anyhow::anyhow!("Editor exited with error code: {}", status));
        }

        let content = std::fs::read_to_string(&temp_file)?;
        let content = content.trim().to_string();

        if content.is_empty() {
            self.writeln_title(TitleFormat::info("Editor closed with no content"))?;
            return Ok(());
        }

        // Pre-fill the REPL buffer so the user can review/edit before sending
        self.console.set_buffer(content);

        Ok(())
    }

    /// Generates a shell command from a natural language description and sets
    /// it in the REPL buffer.
    ///
    /// # Arguments
    /// * `description` - Optional natural language description. If `None`, an
    ///   interactive prompt is shown.
    async fn on_suggest(&mut self, description: Option<String>) -> anyhow::Result<()> {
        let description = match description {
            Some(d) if !d.is_empty() => d,
            _ => {
                let input = FurcodeWidget::input("Describe the command you want")
                    .allow_empty(false)
                    .prompt()?;
                match input {
                    Some(d) if !d.is_empty() => d,
                    _ => {
                        self.writeln_title(TitleFormat::error(
                            "No description provided. Usage: :suggest <description>",
                        ))?;
                        return Ok(());
                    }
                }
            }
        };

        self.spinner.start(Some("Generating"))?;

        let prompt = UserPrompt::from(description);
        match self.api.generate_command(prompt).await {
            Ok(command) => {
                self.spinner.stop(None)?;
                self.writeln(command.clone())?;
                // Set the generated command in the buffer for review
                self.console.set_buffer(command);
                Ok(())
            }
            Err(err) => {
                self.spinner.stop(None)?;
                Err(err)
            }
        }
    }

    /// Clones a conversation (current or selected) and switches to the clone.
    ///
    /// # Arguments
    /// * `id` - Optional conversation ID to clone. If `None`, the current
    ///   conversation is used; if no active conversation, an interactive picker
    ///   is shown.
    async fn on_slash_clone(&mut self, id: Option<String>) -> anyhow::Result<()> {
        let target_id = if let Some(id_str) = id {
            ConversationId::parse(&id_str)
                .map_err(|_| anyhow::anyhow!("Invalid conversation ID: {id_str}"))?
        } else {
            // Show conversation picker
            let conversations = self
                .api
                .get_conversations(Some(self.config.max_conversations))
                .await?;

            if conversations.is_empty() {
                self.writeln_title(TitleFormat::error(
                    "No conversations found. Start a conversation first.",
                ))?;
                return Ok(());
            }

            let selected = ConversationSelector::select_conversation(
                &conversations,
                self.state.conversation_id,
                None,
            )
            .await?;

            match selected {
                Some(conv) => conv.id,
                None => return Ok(()),
            }
        };

        // Fetch the conversation to clone
        let original = self
            .api
            .conversation(&target_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Conversation '{target_id}' not found"))?;

        let original_id = original.id;

        // Create the clone
        let new_id = ConversationId::generate();
        let mut cloned = original;
        cloned.id = new_id;
        self.api.upsert_conversation(cloned).await?;

        // Switch to the cloned conversation
        self.state.conversation_id = Some(new_id);

        self.writeln_title(
            TitleFormat::info("Cloned").sub_title(format!("[{original_id} → {new_id}]")),
        )?;

        Ok(())
    }

    /// Renames any conversation interactively or by explicit ID and name.
    ///
    /// # Arguments
    /// * `args` - Optional `"<id> <name>"` string. If `None`, shows a
    ///   conversation picker and prompts for a new name.
    async fn on_slash_conversation_rename(&mut self, args: Option<String>) -> anyhow::Result<()> {
        if let Some(args) = args {
            // Parse as "<id> <name>"
            let mut parts = args.splitn(2, ' ');
            let id_str = parts.next().unwrap_or("").trim();
            let name = parts.next().unwrap_or("").trim();

            if id_str.is_empty() || name.is_empty() {
                return Err(anyhow::anyhow!("Usage: :conversation-rename <id> <name>"));
            }

            let conversation_id = ConversationId::parse(id_str)
                .map_err(|_| anyhow::anyhow!("Invalid conversation ID: {id_str}"))?;

            self.api
                .rename_conversation(&conversation_id, name.to_string())
                .await?;
            self.writeln_title(TitleFormat::info(format!(
                "Conversation '{}' renamed to '{}'",
                conversation_id.into_string().bold(),
                name.bold()
            )))?;
        } else {
            // Interactive: show picker then prompt for new name
            let conversations = self
                .api
                .get_conversations(Some(self.config.max_conversations))
                .await?;

            if conversations.is_empty() {
                self.writeln_title(TitleFormat::error("No conversations found."))?;
                return Ok(());
            }

            let selected = ConversationSelector::select_conversation(
                &conversations,
                self.state.conversation_id,
                None,
            )
            .await?;

            if let Some(conv) = selected {
                let name_result = FurcodeWidget::input("New name").allow_empty(false).prompt()?;

                if let Some(name) = name_result
                    && !name.is_empty()
                {
                    self.api.rename_conversation(&conv.id, name.clone()).await?;
                    self.writeln_title(TitleFormat::info(format!(
                        "Conversation renamed to '{}'",
                        name.bold()
                    )))?;
                }
            }
        }

        Ok(())
    }

    /// Copies the last AI response from the active conversation to the
    /// system clipboard.
    async fn on_copy(&mut self) -> anyhow::Result<()> {
        let conversation_id = match &self.state.conversation_id {
            Some(cid) => *cid,
            None => {
                self.writeln_title(TitleFormat::error(
                    "No active conversation. Start a conversation first.",
                ))?;
                return Ok(());
            }
        };

        let conversation = match self.api.conversation(&conversation_id).await? {
            Some(conv) => conv,
            None => {
                self.writeln_title(TitleFormat::error("Conversation not found."))?;
                return Ok(());
            }
        };

        let context = match &conversation.context {
            Some(ctx) => ctx.clone(),
            None => {
                self.writeln_title(TitleFormat::error("Conversation has no messages."))?;
                return Ok(());
            }
        };

        // Find the last assistant message
        let content = context.messages.iter().rev().find_map(|msg| match &**msg {
            furcode_domain::ContextMessage::Text(furcode_api::TextMessage {
                content,
                role: furcode_domain::Role::Assistant,
                ..
            }) => Some(content.clone()),
            _ => None,
        });

        match content {
            None => {
                self.writeln_title(TitleFormat::error(
                    "No assistant message found in this conversation.",
                ))?;
            }
            Some(content) => {
                #[cfg(not(target_os = "android"))]
                let copied = arboard::Clipboard::new()
                    .and_then(|mut cb| cb.set_text(content.clone()))
                    .is_ok();

                #[cfg(target_os = "android")]
                let copied = false;

                if copied {
                    let line_count = content.lines().count();
                    let byte_count = content.len();
                    self.writeln_title(TitleFormat::info(format!(
                        "Copied to clipboard [{line_count} lines, {byte_count} bytes]"
                    )))?;
                } else {
                    self.writeln_title(TitleFormat::error(
                        "Failed to copy to clipboard. Ensure xclip/xsel (Linux) or pbcopy (macOS) is available.",
                    ))?;
                }
            }
        }

        Ok(())
    }

    async fn select_agent(&self, query: Option<String>) -> Result<Option<AgentId>> {
        let rows = self.agent_select_rows().await?;
        let initial_raw = self
            .api
            .get_active_agent()
            .await
            .map(|current| current.as_str().to_string());

        Ok(self
            .select_raw_row("Agent", query, rows, 1, initial_raw)?
            .map(|row| AgentId::new(row.raw)))
    }

    async fn agent_select_rows(&self) -> Result<Vec<SelectRow>> {
        let info = self.build_agents_info(false).await?;
        let porcelain = Porcelain::from(&info)
            .drop_cols(&[0, 3])
            .truncate(3, 30)
            .uppercase_headers();

        Self::porcelain_rows(porcelain)
    }

    /// Select a model from all configured providers using porcelain-style
    /// tabular display matching the shell plugin's `:model` UI.
    ///
    /// Shows columns: MODEL, PROVIDER, CONTEXT WINDOW, TOOL SUPPORTED, IMAGE
    /// with a non-selectable header row.
    ///
    /// When `provider_filter` is `Some`, only models belonging to that provider
    /// are shown. This is used during onboarding so that after a provider is
    /// selected the model list is scoped to that provider only.
    ///
    /// # Returns
    /// - `Ok(Some((ModelId, ProviderId)))` if a model was selected, carrying
    ///   both the model and the provider it belongs to
    /// - `Ok(None)` if selection was canceled
    #[async_recursion::async_recursion]
    async fn select_model(
        &mut self,
        provider_filter: Option<ProviderId>,
        query: Option<String>,
    ) -> Result<Option<(ModelId, ProviderId)>> {
        // Check if provider is set otherwise first ask to select a provider
        if provider_filter.is_none() && self.api.get_session_config().await.is_none() {
            if !self.on_provider_selection().await? {
                return Ok(None);
            }

            // Provider activation may have already completed model selection.
            // If it did not, continue below and show the full cross-provider
            // model list.
            if self.api.get_session_config().await.is_some() {
                return Ok(None);
            }
        }

        // Fetch models from ALL configured providers (matches shell plugin's
        // `furcode list models --porcelain`), then optionally filter by provider.
        self.spinner.start(Some("Loading"))?;
        let mut all_provider_models = self.api.get_all_provider_models().await?;
        self.spinner.stop(None)?;

        // When a provider filter is specified (e.g. during onboarding after a
        // provider was just selected), restrict the list to that provider's
        // models so the user cannot accidentally pick a model from a different
        // provider.
        if let Some(ref filter_id) = provider_filter {
            all_provider_models.retain(|pm| &pm.provider_id == filter_id);
        }

        if all_provider_models.is_empty() {
            return Ok(None);
        }

        // Sort models and providers (same as on_show_models)
        all_provider_models
            .iter_mut()
            .for_each(|pm| pm.models.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str())));
        all_provider_models.sort_by(|a, b| a.provider_id.as_ref().cmp(b.provider_id.as_ref()));

        // Build the same Info structure as on_show_models, then convert to
        // Porcelain for tabular display.
        let mut info = Info::new();
        for pm in &all_provider_models {
            let provider_display = pm.provider_id.to_string();
            for model in &pm.models {
                let id = model.id.to_string();
                info = info
                    .add_title(&id)
                    .add_key_value("Model", model.name.as_ref().unwrap_or(&id))
                    .add_key_value("Provider", &provider_display);

                if let Some(limit) = model.context_length {
                    let context = if limit >= 1_000_000 {
                        format!("{}M", limit / 1_000_000)
                    } else if limit >= 1000 {
                        format!("{}k", limit / 1000)
                    } else {
                        format!("{limit}")
                    };
                    info = info.add_key_value("Context Window", context);
                } else {
                    info = info.add_key_value("Context Window", markers::EMPTY);
                }

                if let Some(supported) = model.tools_supported {
                    info = info.add_key_value(
                        "Tool Supported",
                        if supported { status::YES } else { status::NO },
                    );
                } else {
                    info = info.add_key_value("Tools", markers::EMPTY);
                }

                let supports_image = model
                    .input_modalities
                    .contains(&furcode_domain::InputModality::Image);
                info = info.add_key_value(
                    "Image",
                    if supports_image {
                        status::YES
                    } else {
                        status::NO
                    },
                );
            }
        }

        // Convert to porcelain format (same as on_show_models --porcelain)
        let porcelain_output = Porcelain::from(&info)
            .drop_col(0)
            .truncate(0, 40)
            .uppercase_headers();
        let porcelain_str = porcelain_output.to_string();

        // Split into header + data lines
        let all_lines: Vec<&str> = porcelain_str.lines().collect();
        if all_lines.is_empty() {
            return Ok(None);
        }

        // Build a flat list of (ModelId, ProviderId) for the data rows.
        // The first line is the header; data rows follow in the same order as
        // the Info entries (sorted by provider, then model within provider).
        let mut model_entries: Vec<(ModelId, ProviderId)> = Vec::new();
        for pm in &all_provider_models {
            for model in &pm.models {
                model_entries.push((model.id.clone(), pm.provider_id.clone()));
            }
        }

        let mut rows = Vec::with_capacity(all_lines.len());
        // Header row (non-selectable via header_lines=1)
        let Some(header) = all_lines.first() else {
            return Err(UIError::MissingHeaderLine.into());
        };
        rows.push(SelectRow::header(header.to_string()));
        // Data rows
        for (i, line) in all_lines.iter().skip(1).enumerate() {
            let Some((model_id, provider_id)) = model_entries.get(i) else {
                continue;
            };
            let dotted_id = model_id.as_str().replace(['-', '_'], ".");
            rows.push(SelectRow {
                raw: format!("{}\t{}", model_id.as_str(), provider_id.as_ref()),
                display: line.to_string(),
                search: format!(
                    "{} {} {} {}",
                    model_id.as_str(),
                    dotted_id,
                    provider_id.as_ref(),
                    line
                ),
                fields: vec![model_id.to_string(), provider_id.as_ref().to_string()],
            });
        }

        // Find starting cursor position for the current model.
        let current_model = self
            .get_agent_model(self.api.get_active_agent().await)
            .await;
        let current_provider = self
            .get_provider(self.api.get_active_agent().await)
            .await
            .ok()
            .map(|provider| provider.id);
        let initial_raw = current_model.as_ref().and_then(|current| {
            model_entries
                .iter()
                .find(|(model_id, provider_id)| {
                    model_id == current
                        && current_provider
                            .as_ref()
                            .map(|provider| provider_id == provider)
                            .unwrap_or(true)
                })
                .map(|(model_id, provider_id)| {
                    format!("{}\t{}", model_id.as_str(), provider_id.as_ref())
                })
        });

        let selected = self.select_raw_row("Model ❯ ", query, rows, 1, initial_raw)?;

        let Some(selected) = selected else {
            return Ok(None);
        };

        let mut parts = selected.raw.splitn(2, '\t');
        let selection = match (parts.next(), parts.next()) {
            (Some(model_id), Some(provider_id)) => Some((
                ModelId::new(model_id.to_string()),
                ProviderId::from(provider_id.to_string()),
            )),
            _ => None,
        };
        Ok(selection)
    }

    async fn handle_api_key_input(
        &mut self,
        provider_id: ProviderId,
        request: &ApiKeyRequest,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        self.spinner.stop(None)?;

        // Extract existing API key and URL params for prefilling
        let existing_url_params = request.existing_params.as_ref();

        // Collect URL parameters if required
        let url_params = request
            .required_params
            .iter()
            .map(|param| {
                let param_value = if let Some(options) = &param.options {
                    // Dropdown path: user selects from preset options
                    let starting = existing_url_params
                        .and_then(|p| p.get(&param.name))
                        .and_then(|v| options.iter().position(|o| o.as_str() == v.as_str()))
                        .unwrap_or(0);
                    FurcodeWidget::select(format!("Select {}", param.name), options.clone())
                        .with_starting_cursor(starting)
                        .prompt()?
                        .context("Parameter selection cancelled")?
                } else {
                    // Free-text path
                    let label = if param.optional {
                        format!("Enter {} (optional, press Enter to skip)", param.name)
                    } else {
                        format!("Enter {}", param.name)
                    };
                    let mut input = FurcodeWidget::input(label);

                    // Add default value if it exists in the credential
                    if let Some(params) = existing_url_params
                        && let Some(default_value) = params.get(&param.name)
                    {
                        input = input.with_default(default_value.as_str());
                    }

                    if param.optional {
                        input = input.allow_empty(true);
                    }

                    let param_value = input.prompt()?.context("Parameter input cancelled")?;

                    if !param.optional {
                        anyhow::ensure!(
                            !param_value.trim().is_empty(),
                            "{} cannot be empty",
                            param.name
                        );
                    }

                    param_value.trim_end_matches('/').to_string()
                };

                Ok((param.name.to_string(), param_value))
            })
            .collect::<anyhow::Result<HashMap<_, _>>>()?;

        let allows_local_api_key = matches!(
            provider_id.as_ref().as_ref(),
            "ollama" | "vllm" | "lm_studio" | "llama_cpp" | "jan_ai"
        );

        // Check if API key is already provided
        // For Google ADC, we use a marker to skip prompting
        // For other providers, we use the existing key as a default value (autofill)
        let api_key_str = if let Some(default_key) = &request.api_key {
            let key_str = default_key.as_ref();

            // Skip prompting for markers that indicate non-API-key auth
            if key_str == "google_adc_marker" || key_str == "aws_profile_marker" {
                key_str.to_string()
            } else if allows_local_api_key {
                let input = FurcodeWidget::input(format!(
                    "Enter your {provider_id} API key (press Enter to skip)"
                ))
                .allow_empty(true);
                let api_key = input.prompt()?.context("API key input cancelled")?;
                let api_key_str = api_key.trim();

                if api_key_str.is_empty() {
                    "local".to_string()
                } else {
                    api_key_str.to_string()
                }
            } else {
                // For other providers, show the existing key as default (autofill)
                let input = FurcodeWidget::input(format!("Enter your {provider_id} API key"))
                    .with_default(key_str);
                let api_key = input.prompt()?.context("API key input cancelled")?;
                let api_key_str = api_key.trim();
                anyhow::ensure!(!api_key_str.is_empty(), "API key cannot be empty");
                api_key_str.to_string()
            }
        } else if allows_local_api_key {
            let input = FurcodeWidget::input(format!(
                "Enter your {provider_id} API key (press Enter to skip)"
            ))
            .allow_empty(true);
            let api_key = input.prompt()?.context("API key input cancelled")?;
            let api_key_str = api_key.trim();

            if api_key_str.is_empty() {
                "local".to_string()
            } else {
                api_key_str.to_string()
            }
        } else {
            // Prompt for API key input (no existing key)
            let input = FurcodeWidget::input(format!("Enter your {provider_id} API key"));
            let api_key = input.prompt()?.context("API key input cancelled")?;
            let api_key_str = api_key.trim();
            anyhow::ensure!(!api_key_str.is_empty(), "API key cannot be empty");
            api_key_str.to_string()
        };

        // Update the context with collected data
        let response = AuthContextResponse::api_key(request.clone(), &api_key_str, url_params);

        self.api
            .complete_provider_auth(
                provider_id,
                response,
                Duration::from_secs(0), // No timeout needed since we have the data
            )
            .await?;

        Ok(())
    }

    fn display_oauth_device_info_new(
        &mut self,
        user_code: &str,
        verification_uri: &str,
        verification_uri_complete: Option<&str>,
    ) -> anyhow::Result<()> {
        use colored::Colorize;

        let display_uri = verification_uri_complete.unwrap_or(verification_uri);

        self.writeln("")?;
        self.writeln(format!(
            "{} Please visit: {}",
            "→".blue(),
            display_uri.blue().underline()
        ))?;
        // Try to copy code to clipboard automatically (not available on Android)
        #[cfg(not(target_os = "android"))]
        let clipboard_copied = arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(user_code))
            .is_ok();

        #[cfg(target_os = "android")]
        let clipboard_copied = false;

        if clipboard_copied {
            self.writeln(format!(
                "{} Code copied to clipboard: {}",
                "✓".green().bold(),
                user_code.bold().yellow()
            ))?;
        } else {
            self.writeln(format!(
                "{} Enter code: {}",
                "→".blue(),
                user_code.bold().yellow()
            ))?;
        }
        self.writeln("")?;

        // Try to open browser automatically
        if let Err(e) = open::that(display_uri) {
            self.writeln_title(TitleFormat::error(format!(
                "Failed to open browser automatically: {e}"
            )))?;
        }

        Ok(())
    }

    async fn handle_device_flow(
        &mut self,
        provider_id: ProviderId,
        request: &DeviceCodeRequest,
    ) -> Result<()> {
        use std::time::Duration;

        let user_code = request.user_code.clone();
        let verification_uri = request.verification_uri.clone();
        let verification_uri_complete = request.verification_uri_complete.clone();

        self.spinner.stop(None)?;
        // Display OAuth device information
        self.display_oauth_device_info_new(
            user_code.as_ref(),
            verification_uri.as_ref(),
            verification_uri_complete.as_ref().map(|v| v.as_ref()),
        )?;

        // Step 2: Complete authentication (polls if needed for OAuth flows)
        self.spinner.start(Some("Completing authentication..."))?;

        let response = AuthContextResponse::device_code(request.clone());

        self.api
            .complete_provider_auth(provider_id, response, Duration::from_secs(600))
            .await?;

        self.spinner.stop(None)?;

        Ok(())
    }

    async fn display_credential_success(&mut self, provider_id: ProviderId) -> anyhow::Result<()> {
        self.writeln_title(TitleFormat::info(format!(
            "{provider_id} configured successfully"
        )))?;

        Ok(())
    }

    async fn handle_code_flow(
        &mut self,
        provider_id: ProviderId,
        request: &CodeRequest,
    ) -> anyhow::Result<()> {
        use colored::Colorize;

        self.spinner.stop(None)?;

        self.writeln(format!(
            "{}",
            format!("Authenticate using your {provider_id} account").dimmed()
        ))?;

        let callback_server =
            match crate::oauth_callback::LocalhostOAuthCallbackServer::start(request) {
                Ok(Some(server)) => {
                    self.writeln(format!(
                        "{} Waiting for browser callback on {}",
                        "→".blue(),
                        server.redirect_uri().as_str().blue().underline()
                    ))?;
                    Some(server)
                }
                Ok(None) | Err(_) => {
                    // Not a localhost callback flow, or the listener could not be
                    // started — fall back to manual code paste.
                    None
                }
            };

        // Display authorization URL
        self.writeln(format!(
            "{} Please visit: {}",
            "→".blue(),
            request.authorization_url.as_str().blue().underline()
        ))?;

        // Try to open browser automatically
        if let Err(e) = open::that(request.authorization_url.as_str()) {
            self.writeln_title(TitleFormat::error(format!(
                "Failed to open browser automatically: {e}"
            )))?;
        }

        let code = if let Some(server) = callback_server {
            server.wait_for_code().await?
        } else {
            // Prompt user to paste authorization code
            let code =
                FurcodeWidget::input(format!("Paste the authorization code for {provider_id}"))
                    .prompt()?
                    .ok_or_else(|| anyhow::anyhow!("Authorization code input cancelled"))?;

            if code.trim().is_empty() {
                anyhow::bail!("Authorization code cannot be empty");
            }

            code
        };

        self.spinner
            .start(Some("Exchanging authorization code..."))?;

        let response = AuthContextResponse::code(request.clone(), &code);

        self.api
            .complete_provider_auth(
                provider_id,
                response,
                Duration::from_secs(0), // No timeout needed since we have the data
            )
            .await?;

        self.spinner.stop(None)?;

        Ok(())
    }

    /// Helper method to select an authentication method when multiple are
    /// available
    async fn select_auth_method(
        &mut self,
        provider_id: ProviderId,
        auth_methods: &[AuthMethod],
    ) -> Result<Option<AuthMethod>> {
        use colored::Colorize;

        if auth_methods.is_empty() {
            return Err(UIError::NoAuthMethodsAvailable { provider: provider_id.clone() }.into());
        }

        // If only one auth method, use it directly
        if auth_methods.len() == 1 {
            let Some(method) = auth_methods.first() else {
                return Err(
                    UIError::NoAuthMethodsAvailable { provider: provider_id.clone() }.into(),
                );
            };
            return Ok(Some(method.clone()));
        }

        // Multiple auth methods - ask user to choose
        self.spinner.stop(None)?;

        self.writeln_title(TitleFormat::action(format!("Configure {provider_id}")))?;
        self.writeln("Multiple authentication methods available".dimmed())?;

        let method_names: Vec<String> = auth_methods
            .iter()
            .map(|method| match method {
                AuthMethod::ApiKey => "API Key".to_string(),
                AuthMethod::OAuthDevice(_) => "OAuth Device Flow".to_string(),
                AuthMethod::OAuthCode(_) => "OAuth Authorization Code".to_string(),
                AuthMethod::GoogleAdc => "Google Application Default Credentials (ADC)".to_string(),
                AuthMethod::AwsProfile => "AWS Profile (SSO/IAM)".to_string(),
                AuthMethod::CodexDevice(_) => "OpenAI Codex Device Flow".to_string(),
            })
            .collect();

        match FurcodeWidget::select("Select authentication method:", method_names.clone())
            .with_help_message("Use arrow keys to navigate and Enter to select")
            .prompt()?
        {
            Some(selected_name) => {
                // Find the corresponding auth method
                let Some(index) = method_names.iter().position(|name| name == &selected_name)
                else {
                    return Err(UIError::AuthMethodNotFound.into());
                };
                let Some(method) = auth_methods.get(index) else {
                    return Err(UIError::AuthMethodNotFound.into());
                };
                Ok(Some(method.clone()))
            }
            None => Ok(None),
        }
    }

    /// Creates Furcode Services credentials if not already authenticated and
    /// displays the credentials file location to the user.
    async fn init_furcode_services(&mut self) -> Result<()> {
        if !self.config.cloud_services_enabled() {
            self.writeln_title(TitleFormat::info("Cloud services disabled").sub_title(
                "Semantic search and workspace indexing are off. Set services_enabled=true and services_url to enable.",
            ))?;
            return Ok(());
        }
        self.api.create_auth_credentials().await?;
        let env = self.api.environment();
        let credentials_path = crate::info::format_path_for_display(&env, &env.credentials_path());
        self.writeln_title(
            TitleFormat::info("Furcode Services enabled").sub_title(&credentials_path),
        )?;
        Ok(())
    }

    /// Handle authentication flow for an unavailable provider
    async fn configure_provider(
        &mut self,
        provider_id: ProviderId,
        auth_methods: Vec<AuthMethod>,
    ) -> Result<Option<Provider<Url>>> {
        if provider_id == ProviderId::FURCODE_SERVICES {
            self.init_furcode_services().await?;
            return Ok(None);
        }
        // Select auth method (or use the only one available)
        let auth_method = match self
            .select_auth_method(provider_id.clone(), &auth_methods)
            .await?
        {
            Some(method) => method,
            None => return Ok(None), // User cancelled
        };

        // Show warning for Claude Code provider about account ban risk
        if provider_id == ProviderId::CLAUDE_CODE {
            self.writeln_title(
                TitleFormat::warning(
                    "Using Claude Code subscription in third-party tools violates Anthropic's Terms of Service."
                )
                .sub_title("Your account may be suspended or banned. Continue at your own risk."),
            )?;

            let confirmed = FurcodeWidget::confirm("Do you want to continue with this provider?")
                .with_default(false)
                .prompt()?;

            if !confirmed.unwrap_or(false) {
                return Ok(None);
            }
        }

        self.spinner.start(Some("Initiating authentication..."))?;
        // Initiate the authentication flow
        let auth_request = self
            .api
            .init_provider_auth(provider_id.clone(), auth_method)
            .await?;

        // Handle the specific authentication flow based on the request type
        match auth_request {
            AuthContextRequest::ApiKey(request) => {
                self.handle_api_key_input(provider_id.clone(), &request)
                    .await?;
            }
            AuthContextRequest::DeviceCode(request) => {
                self.handle_device_flow(provider_id.clone(), &request)
                    .await?;
            }
            AuthContextRequest::Code(request) => {
                self.handle_code_flow(provider_id.clone(), &request).await?;
            }
        }

        // Verify by fetching the configured provider
        let provider = self.api.get_provider(&provider_id).await?;

        self.display_credential_success(provider_id.clone()).await?;

        Ok(provider.into_configured())
    }

    /// Builds a porcelain-style provider selection list from a set of
    /// providers, displays it in the interactive picker, and returns the
    /// selected provider.
    ///
    /// The display matches the shell plugin's `_furcode_select_provider`:
    /// columns NAME, HOST, TYPE, LOGGED IN (hiding the raw ID column).
    fn select_provider_from_list(
        &self,
        providers: Vec<AnyProvider>,
        prompt: &str,
        current_provider_id: Option<ProviderId>,
        query: Option<String>,
    ) -> Result<Option<AnyProvider>> {
        if providers.is_empty() {
            return Ok(None);
        }

        // Sort providers alphabetically by display name
        let mut sorted = providers;
        sorted.sort_by_key(|a| a.id().to_string());

        // Build Info structure (same as on_show_providers)
        let mut info = Info::new();
        for provider in &sorted {
            let id: &str = &provider.id();
            let display_name = provider.id().to_string();
            let domain = if let Some(url) = provider.url() {
                url.domain().map(|d| d.to_string()).unwrap_or_default()
            } else {
                markers::EMPTY.to_string()
            };
            let provider_type = provider.provider_type().to_string();
            let configured = provider.is_configured();
            info = info
                .add_title(id.to_case(Case::UpperSnake))
                .add_key_value("name", display_name)
                .add_key_value("id", id)
                .add_key_value("host", domain)
                .add_key_value("type", provider_type);
            if configured {
                info = info.add_key_value("logged in", status::YES);
            }
        }

        // Convert to porcelain, drop title (col 0) and raw id (col 2)
        let porcelain_output = Porcelain::from(&info)
            .drop_cols(&[0, 2])
            .uppercase_headers();
        let porcelain_str = porcelain_output.to_string();

        let all_lines: Vec<&str> = porcelain_str.lines().collect();
        if all_lines.is_empty() {
            return Ok(None);
        }

        let Some(header) = all_lines.first() else {
            return Err(UIError::MissingHeaderLine.into());
        };
        let mut rows = vec![SelectRow::header(header.to_string())];
        for (index, line) in all_lines.iter().skip(1).enumerate() {
            if let Some(provider) = sorted.get(index) {
                rows.push(SelectRow::new(
                    provider.id().as_ref().to_string(),
                    line.to_string(),
                ));
            }
        }

        let selected = self.select_raw_row(
            prompt,
            query,
            rows,
            1,
            current_provider_id.map(|current| current.as_ref().to_string()),
        )?;

        Ok(selected.and_then(|row| {
            sorted
                .into_iter()
                .find(|provider| provider.id().as_ref().as_ref() == row.raw)
        }))
    }

    /// Selects a provider, optionally configuring it if not already configured.
    async fn select_provider(
        &mut self,
        query: Option<String>,
        configured_only: bool,
    ) -> Result<Option<AnyProvider>> {
        let mut providers: Vec<AnyProvider> = self
            .api
            .get_providers()
            .await?
            .into_iter()
            .filter(|p| {
                let filter = furcode_domain::ProviderType::Llm;
                match &p {
                    AnyProvider::Url(provider) => provider.provider_type == filter,
                    AnyProvider::Template(provider) => provider.provider_type == filter,
                }
            })
            .collect();

        if configured_only {
            providers.retain(|provider| provider.is_configured());
        }

        if providers.is_empty() {
            return Err(anyhow::anyhow!("No AI provider API keys configured"));
        }

        let current_provider_id = self
            .get_provider(self.api.get_active_agent().await)
            .await
            .ok()
            .map(|p| p.id);

        self.select_provider_from_list(providers, "Provider", current_provider_id, query)
    }

    // Helper method to handle model selection and update the conversation.
    // When `provider_filter` is `Some`, only models from that provider are shown.
    // The model and provider returned by the selector are always set as one
    // atomic operation.
    #[async_recursion::async_recursion]
    async fn on_model_selection(
        &mut self,
        provider_filter: Option<ProviderId>,
    ) -> Result<Option<ModelId>> {
        // Select a model; the selector returns both the model and its provider
        let selection = self.select_model(provider_filter, None).await?;

        // If no model was selected (user canceled), return early
        let (model, provider_id) = match selection {
            Some(pair) => pair,
            None => return Ok(None),
        };

        // Set model and provider atomically as a single config operation
        self.api
            .update_config(vec![ConfigOperation::SetSessionConfig(
                furcode_domain::ModelConfig::new(provider_id, model.clone()),
            )])
            .await?;

        // Update the UI state with the new model
        self.update_model(Some(model.clone()));

        self.writeln_title(TitleFormat::action(format!("Switched to model: {model}")))?;

        Ok(Some(model))
    }

    async fn on_provider_selection(&mut self) -> Result<bool> {
        // Select a provider
        // If no provider was selected (user canceled), return early
        let any_provider = match self.select_provider(None, false).await? {
            Some(provider) => provider,
            None => return Ok(false),
        };

        self.activate_provider(any_provider).await?;
        // Check if provider was actually saved — if user cancelled model selection
        // inside activate_provider, nothing was written
        Ok(self.api.get_session_config().await.is_some())
    }

    /// Activates a provider by configuring it if needed, setting it as default,
    /// and ensuring a compatible model is selected.
    async fn activate_provider(&mut self, any_provider: AnyProvider) -> Result<()> {
        self.activate_provider_with_model(any_provider, None).await
    }

    /// Activates a provider with an optional pre-selected model.
    /// When `model` is provided, the interactive model selection prompt is
    /// skipped and the specified model is set directly.
    async fn activate_provider_with_model(
        &mut self,
        any_provider: AnyProvider,
        model: Option<ModelId>,
    ) -> Result<()> {
        // Trigger authentication for the selected provider only if not configured
        let provider = if !any_provider.is_configured() {
            match self
                .configure_provider(any_provider.id(), any_provider.auth_methods().to_vec())
                .await?
            {
                Some(provider) => provider,
                None => return Ok(()),
            }
        } else {
            // Provider is already configured, convert it
            match any_provider.into_configured() {
                Some(provider) => provider,
                None => return Ok(()),
            }
        };

        // Set as default and handle model selection
        self.finalize_provider_activation(provider, model).await
    }

    /// Finalizes provider activation by setting it as default and ensuring
    /// a compatible model is selected.
    /// When `model` is `Some`, the interactive model selection is skipped and
    /// the provided model is validated and set directly.
    async fn finalize_provider_activation(
        &mut self,
        provider: Provider<Url>,
        model: Option<ModelId>,
    ) -> Result<()> {
        // If a model was pre-selected (e.g. from :model), validate and set it
        // directly without prompting
        if let Some(model) = model {
            let model_id = self
                .validate_model(model.as_str(), Some(&provider.id))
                .await?;
            self.api
                .update_config(vec![ConfigOperation::SetSessionConfig(
                    furcode_domain::ModelConfig::new(provider.id.clone(), model_id.clone()),
                )])
                .await?;
            self.writeln_title(
                TitleFormat::action(format!("{}", provider.id))
                    .sub_title("is now the default provider"),
            )?;
            self.writeln_title(
                TitleFormat::action(model_id.as_str()).sub_title("is now the default model"),
            )?;
            return Ok(());
        }

        // Check if the current model is available for the new provider
        let current_model = self.api.get_session_config().await.map(|c| c.model);
        let (needs_model_selection, compatible_model) = match current_model {
            None => (true, None),
            Some(current_model) => {
                let provider_models = self.api.get_all_provider_models().await?;
                let model_available = provider_models
                    .iter()
                    .find(|pm| pm.provider_id == provider.id)
                    .map(|pm| pm.models.iter().any(|m| m.id == current_model))
                    .unwrap_or(false);
                if model_available {
                    (false, Some(current_model))
                } else {
                    (true, None)
                }
            }
        };

        if needs_model_selection {
            let selected = self.on_model_selection(Some(provider.id.clone())).await?;
            if selected.is_none() {
                // User cancelled — preserve existing config untouched
                return Ok(());
            }
        } else {
            // The current model is compatible with the new provider — write both
            // atomically so the session always stores a consistent pair.
            let model =
                compatible_model.expect("compatible_model is Some when !needs_model_selection");
            self.api
                .update_config(vec![ConfigOperation::SetSessionConfig(
                    furcode_domain::ModelConfig::new(provider.id.clone(), model),
                )])
                .await?;

            self.writeln_title(
                TitleFormat::action(format!("{}", provider.id))
                    .sub_title("is now the default provider"),
            )?;
        }

        Ok(())
    }

    // Handle dispatching events from the CLI
    async fn handle_dispatch(&mut self, json: String) -> Result<()> {
        // Initialize the conversation
        let conversation_id = self.init_conversation().await?;

        // Parse the JSON to determine the event name and value
        let event: UserCommand = serde_json::from_str(&json)?;

        // Create the chat request with the event
        let chat = ChatRequest::new(event.into(), conversation_id);

        self.on_chat(chat).await
    }

    /// Initializes and returns a conversation ID for the current session.
    ///
    /// Handles conversation setup for both interactive and headless modes:
    /// - **Interactive**: Reuses existing conversation, loads from file, or
    ///   creates new
    /// - **Headless**: Uses environment variables or generates new conversation
    ///
    /// Displays initialization status and updates UI state with the
    /// conversation ID.
    async fn init_conversation(&mut self) -> Result<ConversationId> {
        // Set agent if provided via CLI
        if let Some(agent_id) = self.cli.agent.clone() {
            self.api.set_active_agent(agent_id).await?;
        }

        let mut is_new = false;
        let id = if let Some(id) = self.state.conversation_id {
            id
        } else if let Some(id) = self.cli.conversation_id {
            // Use the provided conversation ID

            // Check if conversation exists, if not create it
            if self.api.conversation(&id).await?.is_none() {
                let conversation = Conversation::new(id);
                self.api.upsert_conversation(conversation).await?;
                is_new = true;
            }
            id
        } else if let Some(ref path) = self.cli.conversation {
            let content = FurcodeFS::read_utf8(path).await?;

            // Try to parse as a dump file first (with "conversation" wrapper)
            let conversation: Conversation = if let Ok(dump) =
                serde_json::from_str::<ConversationDump>(&content)
            {
                dump.conversation
            } else {
                // Fall back to parsing as direct Conversation object
                serde_json::from_str(&content)
                    .context("Failed to parse conversation file. Expected either a ConversationDump or Conversation format")?
            };

            let id = conversation.id;
            self.api.upsert_conversation(conversation).await?;
            id
        } else {
            let conversation = Conversation::generate();
            let id = conversation.id;
            is_new = true;
            self.api.upsert_conversation(conversation).await?;
            id
        };

        // Print if the state is being reinitialized
        if self.state.conversation_id.is_none() {
            self.print_conversation_status(is_new, id)?;
        }

        // Always set the conversation id in state
        self.state.conversation_id = Some(id);

        Ok(id)
    }

    fn print_conversation_status(
        &mut self,
        new_conversation: bool,
        id: ConversationId,
    ) -> Result<(), anyhow::Error> {
        let mut title = if new_conversation {
            "Initialize".to_string()
        } else {
            "Continue".to_string()
        };

        title.push_str(format!(" {}", id.into_string()).as_str());

        self.writeln_title(TitleFormat::debug(title))?;
        Ok(())
    }

    /// Initialize the state of the UI
    async fn init_state(&mut self, first: bool) -> Result<()> {
        let _ = self.handle_migrate_credentials().await;

        // Ensure we have a model selected before proceeding with initialization
        let active_agent = self.api.get_active_agent().await;

        // Validate provider is configured before loading agents
        // If provider is set in config but not configured (no credentials), prompt user
        // to login
        if self.api.get_session_config().await.is_none() && !self.on_provider_selection().await? {
            return Ok(());
        }

        let mut operating_model = self.get_agent_model(active_agent.clone()).await;
        if operating_model.is_none() {
            // Use the model returned from selection instead of re-fetching
            operating_model = self.on_model_selection(None).await?;
        }

        if first {
            // For chat, we are trying to get active agent or setting it to default.
            // So for default values, `/info` doesn't show active provider, model, etc.
            // So my default, on new, we should set the active agent.
            self.api
                .set_active_agent(active_agent.clone().unwrap_or_default())
                .await?;
            // only call on_update if this is the first initialization
            on_update(self.api.clone(), self.config.updates.as_ref()).await;
            // Apply the MCP trust gate. Servers are NOT connected here —
            // connections remain lazy and happen on first tool use.
            self.api.init_mcp().await?;
        }

        // Execute independent operations in parallel to improve performance
        let (agents_result, commands_result) =
            tokio::join!(self.api.get_agent_infos(), self.api.get_commands());

        // Register agent commands with proper error handling and user feedback
        match agents_result {
            Ok(agents) => {
                let registration_result = self.command.register_agent_commands(agents);

                // Show warning for any skipped agents due to conflicts
                for skipped_command in registration_result.skipped_conflicts {
                    self.writeln_title(TitleFormat::error(format!(
                        "Skipped agent command '{skipped_command}' due to name conflict with built-in command"
                    )))?;
                }
            }
            Err(e) => {
                self.writeln_title(TitleFormat::error(format!(
                    "Failed to load agents for command registration: {e}"
                )))?;
            }
        }

        // Register all the commands
        self.command.register_all(commands_result?);

        self.state = UIState::new(self.api.environment());
        self.update_model(operating_model);

        Ok(())
    }

    async fn on_message(&mut self, content: Option<String>) -> Result<()> {
        let conversation_id = self.init_conversation().await?;

        if self.config.auto_install_vscode_extension {
            self.install_vscode_extension();
        }

        // Track if content was provided to decide whether to use piped input as
        // additional context
        let has_content = content.is_some();

        // Create a ChatRequest with the appropriate event type
        let mut event = match content {
            Some(text) => Event::new(text),
            None => Event::empty(),
        };

        // Only use CLI piped_input as additional context when BOTH --prompt and piped
        // input are provided. This handles the case: `echo "context" | furcode -p
        // "question"` where piped input provides context and --prompt provides
        // the actual question.
        //
        // When only piped input is provided (no --prompt), it's already used as the
        // main content (passed via the `content` parameter). We must NOT add it again
        // as additional_context, otherwise the input appears twice in the
        // conversation. We detect this by checking if cli.prompt exists - if it
        // does, the content came from --prompt and piped input should be
        // additional context.
        let piped_input = self.cli.piped_input.clone();
        let has_explicit_prompt = self.cli.prompt.is_some();
        if let Some(piped) = piped_input
            && has_content
            && has_explicit_prompt
        {
            event = event.additional_context(piped);
        }

        // Create the chat request with the event
        let chat = ChatRequest::new(event, conversation_id);

        self.on_chat(chat).await
    }

    async fn on_chat(&mut self, chat: ChatRequest) -> Result<()> {
        let mut stream = self.api.chat(chat).await?;

        // Always use streaming content writer
        let mut writer = StreamingWriter::new(self.spinner.clone(), self.api.clone());
        // --verbose forces classic out-loud streaming for this process.
        let mode = if self.cli.verbose {
            ActivityDisplay::OutLoud
        } else {
            self.config.activity_display
        };
        let mut feed = LiveActivityFeed::new(mode);

        // Drive spinner/elapsed at 10 Hz even when the model is silent between
        // tokens (otherwise the compact overlay freezes mid-thought).
        let mut tick =
            tokio::time::interval(Duration::from_millis(crate::live_feed::LIVE_TICK_MS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Consume the immediate first tick so we don't double-draw on start.
        tick.tick().await;

        loop {
            tokio::select! {
                maybe = stream.next() => {
                    match maybe {
                        Some(Ok(message)) => {
                            self.handle_chat_response(message, &mut writer, &mut feed)
                                .await?;
                        }
                        Some(Err(err)) => {
                            let _ = feed.finish(&self.spinner);
                            writer.finish()?;
                            self.spinner.stop(None)?;
                            self.spinner.reset();
                            return Err(err);
                        }
                        None => break,
                    }
                }
                _ = tick.tick() => {
                    feed.tick(&self.spinner)?;
                }
            }
        }

        feed.finish(&self.spinner)?;
        writer.finish()?;
        self.spinner.stop(None)?;
        self.spinner.reset();

        Ok(())
    }

    /// Fetches related conversations for a given conversation in parallel.
    ///
    /// Returns a vector of related conversations that could be successfully
    /// fetched.
    async fn fetch_related_conversations(&self, conversation: &Conversation) -> Vec<Conversation> {
        let related_ids = conversation.related_conversation_ids();

        // Fetch all related conversations in parallel
        let related_futures: Vec<_> = related_ids
            .iter()
            .map(|id| {
                let api = self.api.clone();
                let id = *id;
                async move { api.conversation(&id).await }
            })
            .collect();

        future::join_all(related_futures)
            .await
            .into_iter()
            .filter_map(|result| result.ok().flatten())
            .collect()
    }

    /// Modified version of handle_dump that supports HTML format
    async fn on_dump(&mut self, html: bool) -> Result<()> {
        if let Some(conversation_id) = self.state.conversation_id {
            let conversation = self.api.conversation(&conversation_id).await?;
            if let Some(conversation) = conversation {
                let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");

                // Collect related conversations from agent tool calls
                let related_conversations = self.fetch_related_conversations(&conversation).await;

                if html {
                    // Create a single HTML with all conversations
                    let html_content = if related_conversations.is_empty() {
                        // No related conversations, just render the main one
                        conversation.to_html()
                    } else {
                        // Render main conversation with related conversations in the same HTML
                        conversation.to_html_with_related(&related_conversations)
                    };

                    let path = format!("{timestamp}-dump.html");
                    tokio::fs::write(path.as_str(), &html_content).await?;

                    let subtitle = if related_conversations.is_empty() {
                        path.to_string()
                    } else {
                        format!("{} (+ {} related)", path, related_conversations.len())
                    };

                    self.writeln_title(
                        TitleFormat::action("Conversation HTML dump created".to_string())
                            .sub_title(subtitle),
                    )?;

                    if self.config.auto_open_dump {
                        open::that(path.as_str()).ok();
                    }
                } else {
                    let dump_data = ConversationDump {
                        conversation: conversation.clone(),
                        related_conversations: related_conversations.clone(),
                    };

                    let path = format!("{timestamp}-dump.json");
                    let content = serde_json::to_string_pretty(&dump_data)?;
                    tokio::fs::write(path.as_str(), content).await?;

                    let subtitle = if related_conversations.is_empty() {
                        path.to_string()
                    } else {
                        format!("{} (+ {} related)", path, related_conversations.len())
                    };

                    self.writeln_title(
                        TitleFormat::action("Conversation JSON dump created".to_string())
                            .sub_title(subtitle),
                    )?;

                    if self.config.auto_open_dump {
                        open::that(path.as_str()).ok();
                    }
                };
            } else {
                return Err(anyhow::anyhow!("Could not create dump"))
                    .context(format!("Conversation: {conversation_id} was not found"));
            }
        } else {
            return Err(anyhow::anyhow!("No conversation initiated yet"))
                .context("Could not create dump");
        }
        Ok(())
    }

    async fn handle_chat_response(
        &mut self,
        message: ChatResponse,
        writer: &mut StreamingWriter<A>,
        feed: &mut LiveActivityFeed,
    ) -> Result<()> {
        if message.is_empty() {
            return Ok(());
        }
        // Do NOT flush thinking on every non-reasoning event — that created one
        // "Thought for Xs" line per model segment. Thinking stays live until a
        // tool, assistant prose, or turn end (merged into a single summary).
        let out_loud = !feed.is_compact();
        match message {
            ChatResponse::TaskMessage { content } => match content {
                ChatResponseContent::ToolInput(title) => {
                    writer.finish()?;
                    if out_loud {
                        self.writeln(title.display())?;
                    } else {
                        // begin_tool flushes/merges any open thought first.
                        feed.begin_tool(&self.spinner, title)?;
                    }
                }
                ChatResponseContent::ToolOutput(text) => {
                    if out_loud {
                        writer.finish()?;
                        self.writeln(text)?;
                    } else {
                        // Compact: only last 2 lines of output under the tool status.
                        feed.push_tool_output_chunk(&self.spinner, &text)?;
                    }
                }
                ChatResponseContent::Markdown { text, partial: _ } => {
                    // One merged Thought line, then seal so prose is not erased.
                    feed.flush_thought(&self.spinner)?;
                    feed.seal();
                    writer.write(&text)?;
                }
            },
            ChatResponse::ToolCallStart { tool_call, notifier } => {
                // Scope guard to ensure notification happens even on error.
                struct NotifyGuard<'a>(&'a tokio::sync::Notify);
                impl<'a> Drop for NotifyGuard<'a> {
                    fn drop(&mut self) {
                        self.0.notify_one();
                    }
                }
                let _guard = NotifyGuard(&notifier);

                writer.finish()?;

                // Only when streaming shell output live (out_loud): stop spinner
                // and freeze the feed so cursor/scroll stay coherent. Compact
                // mode runs shell silently and keeps the animated tool line.
                if out_loud && tool_call.requires_stdout() {
                    feed.finish(&self.spinner)?;
                    self.spinner.stop(None)?;
                }

                drop(_guard);
            }
            ChatResponse::ToolCallEnd(toolcall_result) => {
                let payload = if toolcall_result.is_error() {
                    let mut r = ToolCallPayload::new(toolcall_result.name.to_string());
                    if let Some(cause) = toolcall_result.output.as_str() {
                        r = r.with_cause(cause.to_string());
                    }
                    r
                } else {
                    ToolCallPayload::new(toolcall_result.name.to_string())
                };
                tracker::tool_call(payload);

                if toolcall_result.is_error() {
                    writer.finish()?;
                    let name = toolcall_result.name.to_string();
                    let detail = toolcall_result
                        .output
                        .as_str()
                        .map(|s| {
                            let one = s.lines().find(|l| !l.trim().is_empty()).unwrap_or(s);
                            let t: String = one.chars().take(100).collect();
                            if one.chars().count() > 100 {
                                format!("{t}…")
                            } else {
                                t
                            }
                        })
                        .filter(|s| !s.is_empty());
                    let mut title = TitleFormat::error(format!("{name} failed"));
                    if let Some(d) = detail {
                        title = title.sub_title(d);
                    }
                    if out_loud {
                        self.writeln(title.display())?;
                    } else {
                        feed.finish_tool(&self.spinner, Some(title))?;
                    }
                } else if !out_loud {
                    // Success: collapse live tool preview to static green ● line.
                    feed.finish_tool(&self.spinner, None)?;
                }

                // Compact mode: no "Contemplating" spinner after a finished tool —
                // that left a second animation next to the completed Run line.
                if out_loud {
                    self.spinner.start(None)?;
                }
            }
            ChatResponse::RetryAttempt { cause, duration: _ } => {
                if !self
                    .config
                    .retry
                    .as_ref()
                    .is_some_and(|r| r.suppress_errors)
                {
                    writer.finish()?;
                    self.spinner.start(Some("Retrying"))?;
                    let title = TitleFormat::error(cause.as_str());
                    if out_loud {
                        self.writeln_title(title)?;
                    } else {
                        feed.push_title(&self.spinner, title)?;
                    }
                }
            }
            ChatResponse::Interrupt { reason } => {
                feed.finish(&self.spinner)?;
                writer.finish()?;
                self.spinner.stop(None)?;

                let title = match reason {
                    InterruptionReason::MaxRequestPerTurnLimitReached { limit } => {
                        format!("Maximum request ({limit}) per turn achieved")
                    }
                    InterruptionReason::MaxToolFailurePerTurnLimitReached { limit, .. } => {
                        format!("Maximum tool failure limit ({limit}) reached for this turn")
                    }
                };

                self.writeln_title(TitleFormat::action(title))?;
                let continued = self.should_continue().await?;
                if !continued {
                    self.writeln_title(TitleFormat::info("Done"))?;
                }
            }
            ChatResponse::TaskReasoning { content } => {
                if out_loud {
                    writer.write_dimmed(&content)?;
                } else {
                    // Compact: animate + keep only last 2 lines of thought.
                    feed.push_reasoning_chunk(&self.spinner, &content)?;
                }
            }
            ChatResponse::TaskComplete => {
                feed.finish(&self.spinner)?;
                writer.finish()?;
                if let Some(format) = self.config.auto_dump.clone() {
                    let html = matches!(format, furcode_config::AutoDumpFormat::Html);
                    self.on_dump(html).await?;
                }
            }
        }
        Ok(())
    }

    async fn should_continue(&mut self) -> anyhow::Result<bool> {
        let should_continue = FurcodeWidget::confirm("Do you want to continue anyway?")
            .with_default(true)
            .prompt()?;

        if should_continue.unwrap_or(false) {
            self.spinner.start(None)?;
            Box::pin(self.on_message(None)).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn on_show_conv_info(&mut self, conversation: Conversation) -> anyhow::Result<()> {
        self.spinner.start(Some("Loading Summary"))?;

        let info = Info::default().extend(&conversation);
        self.writeln(info)?;
        self.spinner.stop(None)?;

        Ok(())
    }

    async fn on_show_conv_stats(
        &mut self,
        conversation: Conversation,
        porcelain: bool,
    ) -> anyhow::Result<()> {
        let mut info = Info::new().add_title("CONVERSATION");

        // Add conversation ID
        info = info.add_key_value("ID", conversation.id.to_string());

        // Calculate duration
        let created_at = conversation.metadata.created_at;
        let updated_at = conversation.metadata.updated_at.unwrap_or(created_at);
        let duration = updated_at.signed_duration_since(created_at);

        // Format duration
        let duration_str = if duration.num_hours() > 0 {
            format!("{}h {}m", duration.num_hours(), duration.num_minutes() % 60)
        } else if duration.num_minutes() > 0 {
            format!(
                "{}m {}s",
                duration.num_minutes(),
                duration.num_seconds() % 60
            )
        } else {
            format!("{}s", duration.num_seconds())
        };

        info = info.add_key_value("Total Duration", duration_str);

        // Add message statistics if context exists
        if let Some(context) = &conversation.context {
            info = info
                .add_key_value("Total Messages", context.total_messages().to_string())
                .add_key_value("User Messages", context.user_message_count().to_string())
                .add_key_value(
                    "Assistant Messages",
                    context.assistant_message_count().to_string(),
                )
                .add_key_value("Tool Calls", context.tool_call_count().to_string());
        }

        // Add token usage if available
        if let Some(usage) = conversation.usage().as_ref() {
            info = info
                .add_title("TOKEN")
                .add_key_value("Prompt Tokens", usage.prompt_tokens.to_string())
                .add_key_value("Completion Tokens", usage.completion_tokens.to_string())
                .add_key_value("Total Tokens", usage.total_tokens.to_string());
        }

        if let Some(cost) = conversation.accumulated_cost() {
            info = info.add_key_value("Cost", format!("${cost:.4}"));
        }

        if porcelain {
            use convert_case::Case;
            self.writeln(
                Porcelain::from(&info)
                    .into_long()
                    .skip(1)
                    .to_case(&[0, 1], Case::Snake)
                    .sort_by(&[0, 1]),
            )?;
        } else {
            self.writeln(info)?;
        }

        Ok(())
    }

    /// Clones a conversation with a new ID
    ///
    /// # Arguments
    /// * `original` - The conversation to clone
    /// * `porcelain` - If true, output only the new conversation ID
    async fn on_clone_conversation(
        &mut self,
        original: Conversation,
        porcelain: bool,
    ) -> anyhow::Result<()> {
        // Create a new conversation with a new ID but same content
        let new_id = ConversationId::generate();
        let mut cloned = original.clone();
        cloned.id = new_id;

        // Upsert the cloned conversation
        self.api.upsert_conversation(cloned.clone()).await?;

        // Output based on format
        if porcelain {
            println!("{new_id}");
        } else {
            self.writeln_title(
                TitleFormat::info("Cloned").sub_title(format!("[{} → {}]", original.id, cloned.id)),
            )?;
        }

        Ok(())
    }

    fn update_model(&mut self, model: Option<ModelId>) {
        if let Some(ref model) = model {
            tracker::set_model(model.to_string());
        }
    }

    async fn on_custom_event(&mut self, event: Event) -> Result<()> {
        let conversation_id = self.init_conversation().await?;
        let chat = ChatRequest::new(event, conversation_id);
        self.on_chat(chat).await
    }

    async fn on_usage(&mut self) -> anyhow::Result<()> {
        self.spinner.start(Some("Loading Usage"))?;

        // Get usage from current conversation if available
        let conversation_usage = if let Some(conversation_id) = &self.state.conversation_id {
            self.api
                .conversation(conversation_id)
                .await
                .ok()
                .flatten()
                .and_then(|conv| conv.accumulated_usage())
        } else {
            None
        };

        let mut info = if let Some(usage) = conversation_usage {
            Info::from(&usage)
        } else {
            Info::new()
        };

        if let Ok(Some(user_usage)) = self.api.user_usage().await {
            info = info.extend(Info::from(&user_usage));
        }

        self.writeln(info)?;
        self.spinner.stop(None)?;
        Ok(())
    }

    fn trace_user(&self) {
        let api = self.api.clone();
        // NOTE: Spawning required so that we don't block the user while querying user
        // info
        tokio::spawn(async move {
            if let Ok(Some(user_info)) = api.user_info().await {
                tracker::login(user_info.auth_provider_id.into_string());
            }
        });
    }

    /// Handle config command
    async fn handle_config_command(
        &mut self,
        command: crate::cli::ConfigCommand,
        porcelain: bool,
    ) -> Result<()> {
        match command {
            crate::cli::ConfigCommand::Set(args) => self.handle_config_set(args).await?,
            crate::cli::ConfigCommand::Get(args) => self.handle_config_get(args).await?,
            crate::cli::ConfigCommand::List => {
                self.on_show_config(porcelain).await?;
            }
            crate::cli::ConfigCommand::Path => {
                let path = furcode_config::ConfigReader::config_path();
                self.writeln(path.display().to_string())?;
            }
            crate::cli::ConfigCommand::Migrate => {
                self.handle_config_migrate()?;
            }
        }
        Ok(())
    }

    /// Migrate a legacy config directory to `~/.furcode`.
    ///
    /// Preference order for the source directory:
    /// 1. `~/furcode` (legacy non-dot path)
    /// 2. `~/.forge` (Furcode base directory)
    ///
    /// Errors if no source exists, if the target already exists, or if the
    /// rename fails.
    fn handle_config_migrate(&mut self) -> Result<()> {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
        let target = home.join(".furcode");

        let sources = [home.join("furcode"), home.join(".forge")];
        let legacy = sources
            .into_iter()
            .find(|p| p.exists())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No legacy directory found (looked for ~/furcode and ~/.forge) — nothing to migrate"
                )
            })?;

        if target.exists() {
            anyhow::bail!(
                "Target directory {} already exists — remove it first or migrate manually",
                target.display()
            );
        }

        std::fs::rename(&legacy, &target).map_err(|e| {
            anyhow::anyhow!(
                "Failed to rename {} to {}: {}",
                legacy.display(),
                target.display(),
                e
            )
        })?;

        self.writeln_title(TitleFormat::info("Migration Completed").sub_title(format!(
            "{} → {}",
            legacy.display(),
            target.display()
        )))?;

        Ok(())
    }

    /// Handle config set command
    async fn handle_config_set(&mut self, args: crate::cli::ConfigSetArgs) -> Result<()> {
        use crate::cli::ConfigSetField;

        match args.field {
            ConfigSetField::Model { provider, model } => {
                let provider = self.api.get_provider(&provider).await?;
                self.activate_provider_with_model(provider, Some(model))
                    .await?;
            }
            ConfigSetField::Commit { provider, model } => {
                // Validate provider exists and model belongs to that specific provider
                let validated_model = self.validate_model(model.as_str(), Some(&provider)).await?;
                let commit_config =
                    furcode_domain::ModelConfig::new(provider.clone(), validated_model.clone());
                self.api
                    .update_config(vec![ConfigOperation::SetCommitConfig(Some(commit_config))])
                    .await?;
                self.writeln_title(
                    TitleFormat::action(validated_model.as_str())
                        .sub_title(format!("is now the commit model for provider '{provider}'")),
                )?;
            }
            ConfigSetField::Suggest { provider, model } => {
                // Validate provider exists and model belongs to that specific provider
                let validated_model = self.validate_model(model.as_str(), Some(&provider)).await?;
                let suggest_config =
                    furcode_domain::ModelConfig::new(provider.clone(), validated_model.clone());
                self.api
                    .update_config(vec![ConfigOperation::SetSuggestConfig(suggest_config)])
                    .await?;
                self.writeln_title(TitleFormat::action(validated_model.as_str()).sub_title(
                    format!("is now the suggest model for provider '{provider}'"),
                ))?;
            }
            ConfigSetField::ReasoningEffort { effort } => {
                self.api
                    .update_config(vec![ConfigOperation::SetReasoningEffort(effort.clone())])
                    .await?;
                self.writeln_title(
                    TitleFormat::action(effort.to_string())
                        .sub_title("is now the reasoning effort"),
                )?;
            }
        }

        Ok(())
    }

    /// Handle config get command
    async fn handle_config_get(&mut self, args: crate::cli::ConfigGetArgs) -> Result<()> {
        use crate::cli::ConfigGetField;

        match args.field {
            ConfigGetField::Model => {
                let model = self
                    .api
                    .get_session_config()
                    .await
                    .map(|c| c.model.as_str().to_string());
                match model {
                    Some(v) => self.writeln(v.to_string())?,
                    None => self.writeln("Model: Not set")?,
                }
            }
            ConfigGetField::Provider => {
                let provider = self
                    .api
                    .get_session_config()
                    .await
                    .map(|c| c.provider.to_string());
                match provider {
                    Some(v) => self.writeln(v.to_string())?,
                    None => self.writeln("Provider: Not set")?,
                }
            }
            ConfigGetField::Commit => {
                let commit_config = self.api.get_commit_config().await?;
                match commit_config {
                    Some(config) => {
                        self.writeln(config.provider.as_ref())?;
                        self.writeln(config.model.as_str().to_string())?;
                    }
                    None => self.writeln("Commit: Not set")?,
                }
            }
            ConfigGetField::Suggest => {
                let suggest_config = self.api.get_suggest_config().await?;
                match suggest_config {
                    Some(config) => {
                        self.writeln(config.provider.as_ref())?;
                        self.writeln(config.model.as_str().to_string())?;
                    }
                    None => self.writeln("Suggest: Not set")?,
                }
            }
            ConfigGetField::ReasoningEffort => {
                let effort = self.api.get_reasoning_effort().await?;
                match effort {
                    Some(e) => self.writeln(e.to_string())?,
                    None => self.writeln("ReasoningEffort: Not set")?,
                }
            }
        }

        Ok(())
    }

    /// Validates that a model exists, optionally scoped to a specific provider.
    /// When `provider` is `None`, models are fetched from the default provider.
    async fn validate_model(
        &self,
        model_str: &str,
        provider: Option<&furcode_domain::ProviderId>,
    ) -> Result<ModelId> {
        let models = match provider {
            None => self.api.get_models().await?,
            Some(provider_id) => {
                self.api
                    .get_all_provider_models()
                    .await?
                    .into_iter()
                    .find(|pm| &pm.provider_id == provider_id)
                    .with_context(|| {
                        format!("Provider '{provider_id}' not found or returned no models")
                    })?
                    .models
            }
        };
        let model_id = ModelId::new(model_str);
        models
            .iter()
            .find(|m| m.id == model_id)
            .map(|_| model_id)
            .with_context(|| {
                let hints = models
                    .iter()
                    .take(10)
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let suggestion = if models.len() > 10 {
                    format!("{hints} (and {} more)", models.len() - 10)
                } else {
                    hints
                };
                format!("Model '{model_str}' not found. Available models: {suggestion}")
            })
    }

    /// Shows the last message from a conversation
    ///
    /// When `md` is true, the raw markdown content is printed without
    /// rendering. When `md` is false, the content is rendered through the
    /// markdown renderer.
    ///
    /// # Errors
    /// - If the conversation doesn't exist
    /// - If the conversation has no messages
    async fn on_show_last_message(&mut self, conversation: Conversation, md: bool) -> Result<()> {
        let context = conversation
            .context
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Conversation has no context"))?;

        // Find the last assistant message
        let message = context.messages.iter().rev().find_map(|msg| match &**msg {
            ContextMessage::Text(TextMessage { content, role: Role::Assistant, .. }) => {
                Some(content)
            }
            _ => None,
        });

        // Format and display the message using the message_display module
        if let Some(message) = message {
            if md {
                self.writeln(message)?;
            } else {
                self.writeln(self.markdown.render(message))?;
            }
        }

        Ok(())
    }

    async fn on_index(&mut self, path: std::path::PathBuf, init: bool) -> anyhow::Result<()> {
        use furcode_domain::SyncProgress;
        use furcode_spinner::ProgressBarManager;

        // Check if auth already exists and create if needed
        if !self.api.is_authenticated().await? {
            self.init_furcode_services().await?;
        }

        // When init is set, check if the workspace is already initialized
        // via get_workspace_info before calling init, so we only initialize
        // when a workspace does not yet exist for the given path.
        if init {
            let workspace_info = self.api.get_workspace_info(path.clone()).await?;
            if workspace_info.is_none() {
                self.on_workspace_init(path.clone(), false).await?;
                // If the workspace still does not exist after init (e.g. user
                // declined the consent prompt), abort the sync.
                let workspace_info = self.api.get_workspace_info(path.clone()).await?;
                if workspace_info.is_none() {
                    return Ok(());
                }
            }
        }

        let mut stream = self.api.sync_workspace(path.clone()).await?;
        let mut progress_bar = ProgressBarManager::default();

        while let Some(event) = stream.next().await {
            match event {
                Ok(ref progress @ SyncProgress::Completed { .. }) => {
                    progress_bar.set_position(100)?;
                    progress_bar.stop(None).await?;
                    if let Some(msg) = progress.message() {
                        self.writeln_title(TitleFormat::debug(msg))?;
                    }
                }
                Ok(ref progress @ SyncProgress::Syncing { .. }) => {
                    if !progress_bar.is_active() {
                        progress_bar.start(100, "Indexing workspace")?;
                    }
                    if let Some(msg) = progress.message() {
                        progress_bar.set_message(&msg)?;
                    }
                    if let Some(weight) = progress.weight() {
                        progress_bar.set_position(weight)?;
                    }
                }
                Ok(ref progress) => {
                    if let Some(msg) = progress.message() {
                        self.writeln_title(TitleFormat::debug(msg))?;
                    }
                }
                Err(e) => {
                    progress_bar.stop(None).await?;
                    return Err(e);
                }
            }
        }

        Ok(())
    }

    async fn on_query(
        &mut self,
        path: PathBuf,
        params: furcode_domain::SearchParams<'_>,
    ) -> anyhow::Result<()> {
        self.spinner.start(Some("Searching workspace..."))?;

        let results = match self.api.query_workspace(path.clone(), params).await {
            Ok(results) => results,
            Err(e) => {
                self.spinner.stop(None)?;
                return Err(e);
            }
        };

        self.spinner.stop(None)?;

        let mut info = Info::new().add_title(format!("FILES [{} RESULTS]", results.len()));

        for result in results.iter() {
            match &result.node {
                furcode_domain::NodeData::FileChunk(chunk) => {
                    info = info.add_key_value(
                        "File",
                        format!(
                            "{}:{}-{}",
                            chunk.file_path, chunk.start_line, chunk.end_line
                        ),
                    );
                }
                furcode_domain::NodeData::File(file) => {
                    info = info.add_key_value("File", format!("{} (full file)", file.file_path));
                }
                furcode_domain::NodeData::FileRef(file_ref) => {
                    info =
                        info.add_key_value("File", format!("{} (reference)", file_ref.file_path));
                }
                furcode_domain::NodeData::Note(note) => {
                    info = info.add_key_value("Note", &note.content);
                }
                furcode_domain::NodeData::Task(task) => {
                    info = info.add_key_value("Task", &task.task);
                }
            }
        }

        self.writeln(info)?;

        Ok(())
    }

    /// Helper function to format workspace information consistently
    fn format_workspace_info(workspace: &furcode_domain::WorkspaceInfo, is_active: bool) -> Info {
        let updated_time = workspace
            .last_updated
            .map_or("NEVER".to_string(), humanize_time);

        let mut info = Info::new();

        let title = if is_active {
            "Workspace [Current]".to_string()
        } else {
            "Workspace".to_string()
        };
        info = info.add_title(title);

        info.add_key_value("ID", workspace.workspace_id.to_string())
            .add_key_value("Path", workspace.working_dir.to_string())
            .add_key_value("Created At", humanize_time(workspace.created_at))
            .add_key_value("Updated At", updated_time)
    }

    async fn on_list_workspaces(&mut self, porcelain: bool) -> anyhow::Result<()> {
        if !porcelain {
            self.spinner.start(Some("Fetching workspaces..."))?;
        }

        // Fetch workspaces and current workspace info in parallel
        let env = self.api.environment();
        let (workspaces_result, current_workspace_result) = tokio::join!(
            self.api.list_workspaces(),
            self.api.get_workspace_info(env.cwd)
        );

        match workspaces_result {
            Ok(workspaces) => {
                if !porcelain {
                    self.spinner.stop(None)?;
                }

                // Get active workspace ID if current workspace info is available
                let current_workspace = current_workspace_result.ok().flatten();
                let active_workspace_id = current_workspace.as_ref().map(|ws| &ws.workspace_id);

                // Build Info object once
                let mut info = Info::new();

                for workspace in &workspaces {
                    let is_active = active_workspace_id == Some(&workspace.workspace_id);
                    info = info.extend(Self::format_workspace_info(workspace, is_active));
                }

                // Output based on mode
                if porcelain {
                    // Skip header row in porcelain mode (consistent with conversation list)
                    self.writeln(Porcelain::from(info).skip(1).drop_cols(&[0, 4, 5]))?;
                } else {
                    self.writeln(info)?;
                }

                Ok(())
            }
            Err(e) => {
                self.spinner.stop(None)?;
                Err(e)
            }
        }
    }

    /// Displays workspace information for a given path.
    async fn on_workspace_info(&mut self, path: std::path::PathBuf) -> anyhow::Result<()> {
        self.spinner.start(Some("Fetching workspace info..."))?;

        // Fetch workspace info and status in parallel
        let (workspace, statuses) = tokio::try_join!(
            self.api.get_workspace_info(path.clone()),
            self.api.get_workspace_status(path)
        )?;

        self.spinner.stop(None)?;

        match workspace {
            Some(workspace) => {
                // When viewing a specific workspace's info, it's implicitly the active one
                let mut info = Self::format_workspace_info(&workspace, true);

                // Add sync status summary if available

                use furcode_domain::SyncStatus;

                let in_sync = statuses
                    .iter()
                    .filter(|s| s.status == SyncStatus::InSync)
                    .count();
                let modified = statuses
                    .iter()
                    .filter(|s| s.status == SyncStatus::Modified)
                    .count();
                let added = statuses
                    .iter()
                    .filter(|s| s.status == SyncStatus::New)
                    .count();
                let deleted = statuses
                    .iter()
                    .filter(|s| s.status == SyncStatus::Deleted)
                    .count();
                let failed = statuses
                    .iter()
                    .filter(|s| s.status == SyncStatus::Failed)
                    .count();

                // Add sync status section
                info = info.add_title("Sync Status");
                info = info.add_key_value("Total Files", statuses.len().to_string());
                if in_sync > 0 {
                    info = info.add_key_value("In Sync", in_sync.to_string());
                }
                if modified > 0 {
                    info = info.add_key_value("Modified", modified.to_string());
                }
                if added > 0 {
                    info = info.add_key_value("Added", added.to_string());
                }
                if deleted > 0 {
                    info = info.add_key_value("Deleted", deleted.to_string());
                }
                if failed > 0 {
                    info = info.add_key_value("Failed", failed.to_string());
                }

                self.writeln(info)
            }
            None => self.writeln_to_stderr(
                TitleFormat::error("No workspace found")
                    .display()
                    .to_string(),
            ),
        }
    }

    async fn on_delete_workspaces(&mut self, workspace_ids: Vec<String>) -> anyhow::Result<()> {
        if workspace_ids.is_empty() {
            anyhow::bail!("At least one workspace ID is required");
        }

        // Parse all workspace IDs
        let parsed_ids: Vec<furcode_domain::WorkspaceId> = workspace_ids
            .iter()
            .map(|id| {
                furcode_domain::WorkspaceId::from_string(id)
                    .with_context(|| format!("Invalid workspace ID format: {}", id))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let total = parsed_ids.len();
        self.spinner.start(Some(&format!(
            "Deleting {} workspace{}...",
            total,
            if total > 1 { "s" } else { "" }
        )))?;

        match self.api.delete_workspaces(parsed_ids.clone()).await {
            Ok(()) => {
                self.spinner.stop(None)?;
                for id in &parsed_ids {
                    self.writeln_title(TitleFormat::debug(format!(
                        "Successfully deleted workspace {}",
                        id
                    )))?;
                }
                Ok(())
            }
            Err(e) => {
                self.spinner.stop(None)?;
                Err(e)
            }
        }
    }

    /// Displays sync status for all files in the workspace.
    async fn on_workspace_status(
        &mut self,
        path: std::path::PathBuf,
        porcelain: bool,
    ) -> anyhow::Result<()> {
        use furcode_domain::SyncStatus;

        if !porcelain {
            self.spinner.start(Some("Checking file status..."))?;
        }

        let mut statuses = self.api.get_workspace_status(path.clone()).await?;
        statuses.sort_by(|a, b| a.status.cmp(&b.status));

        if !porcelain {
            self.spinner.stop(None)?;
        }

        // Calculate out of sync count
        let out_of_sync = statuses
            .iter()
            .filter(|s| {
                s.status == SyncStatus::Modified
                    || s.status == SyncStatus::New
                    || s.status == SyncStatus::Deleted
                    || s.status == SyncStatus::Failed
            })
            .count();

        // When all files are in sync, show a simple log message
        if out_of_sync == 0 {
            if porcelain {
                // In porcelain mode, output empty result
                self.writeln(
                    Porcelain::from(Info::new())
                        .into_long()
                        .set_headers(["STATUS", "FILE"])
                        .uppercase_headers(),
                )?;
            } else {
                // Show log info message when all files are in sync
                self.writeln_title(TitleFormat::info(format!(
                    "All {} files are in sync",
                    statuses.len()
                )))?;
            }
            return Ok(());
        }

        // Build file list info only when there are files out of sync
        let mut info = Info::new().add_title(format!("File Status [{} out of sync]", out_of_sync));

        // Add file list (skip in-sync files)
        for (status, label) in statuses.iter().filter_map(|status| match status.status {
            SyncStatus::InSync => None,
            SyncStatus::Modified => Some((status, "modified")),
            SyncStatus::New => Some((status, "added")),
            SyncStatus::Deleted => Some((status, "deleted")),
            SyncStatus::Failed => Some((status, "failed")),
        }) {
            info = info.add_key_value(&status.path, label);
        }

        // Output based on mode
        if porcelain {
            self.writeln(
                Porcelain::from(info)
                    .into_long()
                    .drop_col(0)
                    .swap_cols(0, 1)
                    .set_headers(["STATUS", "FILE"])
                    .sort_by(&[0])
                    .uppercase_headers(),
            )?;
        } else {
            self.writeln(info)?;
        }

        Ok(())
    }

    /// Initialize workspace for a directory without syncing files
    async fn on_workspace_init(
        &mut self,
        path: std::path::PathBuf,
        yes: bool,
    ) -> anyhow::Result<()> {
        // Ask for user consent before syncing and sharing directory contents
        // with the Furcode Service.
        let display_path = path.display().to_string();

        let confirmed = if yes {
            Some(true)
        } else {
            FurcodeWidget::confirm(format!(
                "This will sync and share the contents of '{}' with Furcode Services. Do you wish to continue?",
                display_path
            ))
            .with_default(true)
            .prompt()?
        };

        if !confirmed.unwrap_or(false) {
            self.writeln_title(TitleFormat::info("Workspace initialization cancelled"))?;
            return Ok(());
        }

        // Check if auth already exists and create if needed
        if !self.api.is_authenticated().await? {
            self.init_furcode_services().await?;
        }

        self.spinner.start(Some("Initializing workspace"))?;

        let workspace_id = self.api.init_workspace(path.clone()).await?;

        self.spinner.stop(None)?;

        self.writeln_title(
            TitleFormat::info("Workspace initialized successfully")
                .sub_title(format!("{}", workspace_id)),
        )?;

        Ok(())
    }

    /// Handle credential migration
    async fn handle_migrate_credentials(&mut self) -> Result<()> {
        // Perform the migration
        self.spinner.start(Some("Migrating credentials"))?;
        let result = self.api.migrate_env_credentials().await?;
        self.spinner.stop(None)?;

        // Display results based on whether migration occurred
        if let Some(result) = result {
            self.writeln_title(
                TitleFormat::warning("Furcode no longer reads API keys from environment variables.")
                    .sub_title("Learn more: https://furcode.dev/docs/custom-providers/"),
            )?;

            let count = result.migrated_providers.len();
            let message = if count == 1 {
                "Migrated 1 provider from environment variables".to_string()
            } else {
                format!("Migrated {count} providers from environment variables")
            };
            self.writeln_title(TitleFormat::info(message))?;
        }
        Ok(())
    }

    /// Silently install VS Code extension if in VS Code and extension not
    /// installed.
    /// NOTE: This is a non-cancellable and a slow task. We should only run this
    /// if the user has provided a prompt because that is guaranteed to run for
    /// at least a few seconds.
    fn install_vscode_extension(&self) {
        tokio::task::spawn_blocking(|| {
            if crate::vscode::should_install_extension() {
                let _ = crate::vscode::install_extension();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    // Note: Tests for confirm_delete_conversation are disabled because
    // FurcodeSelect::confirm is not easily mockable in the current
    // architecture. The functionality is tested through integration tests
    // instead.
}
