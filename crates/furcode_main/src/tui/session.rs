//! Crossterm + ratatui session loop for the frame-based TUI.
//!
//! The draw loop runs on a blocking thread and shares [`TurnView`] via
//! `Arc<Mutex<_>>`. The host async task drives the agent stream and applies
//! [`ChatResponse`] events into the same view.

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use super::render;
use super::view::TurnView;

/// Commands from the TUI draw thread → async host.
#[derive(Debug, Clone)]
pub enum UserAction {
    /// Idle Enter: send this message on the agent path (or slash-command path).
    Send(String),
    /// Esc while busy.
    Interrupt,
    /// Reveal older turns from the slim history archive.
    LoadMoreHistory,
    /// Double-Esc idle: open user-message history picker (fork/undo).
    ForkHistory,
    /// User confirmed a fork target from the picker.
    ForkSelect {
        /// Index into conversation context.messages of the chosen user turn.
        message_index: usize,
        /// Prompt text to pre-fill after truncating.
        edit_text: String,
    },
    /// Ctrl+C / quit.
    Quit,
}

/// Control messages from the host → draw thread (suspend for interactive widgets).
enum DrawCtrl {
    /// Leave alt-screen + raw mode so host can run `FurcodeWidget` pickers.
    Suspend {
        done: std_mpsc::SyncSender<()>,
    },
    /// Re-enter alt-screen after a suspend.
    Resume {
        done: std_mpsc::SyncSender<()>,
    },
}

/// Shared session state between draw thread and async host.
pub struct TuiSession {
    pub view: Arc<Mutex<TurnView>>,
    /// Set by Esc; host aborts the in-flight chat future.
    pub interrupt: Arc<AtomicBool>,
    pub actions: mpsc::UnboundedReceiver<UserAction>,
    draw_ctrl: std_mpsc::Sender<DrawCtrl>,
    _draw: tokio::task::JoinHandle<()>,
}

/// Guard that keeps the draw loop suspended until dropped.
pub struct TuiSuspendGuard {
    draw_ctrl: std_mpsc::Sender<DrawCtrl>,
}

impl Drop for TuiSuspendGuard {
    fn drop(&mut self) {
        let (done_tx, done_rx) = std_mpsc::sync_channel(0);
        if self.draw_ctrl.send(DrawCtrl::Resume { done: done_tx }).is_ok() {
            let _ = done_rx.recv_timeout(Duration::from_secs(5));
        }
    }
}

impl TuiSession {
    /// Leave the alt-screen so interactive prompts (`FurcodeWidget`) can run.
    ///
    /// The draw loop stops painting and stops reading keys until the guard is
    /// dropped.
    ///
    /// # Errors
    ///
    /// Returns an error if the draw thread is gone or does not acknowledge.
    pub fn suspend_terminal(&self) -> Result<TuiSuspendGuard> {
        let (done_tx, done_rx) = std_mpsc::sync_channel(0);
        self.draw_ctrl
            .send(DrawCtrl::Suspend { done: done_tx })
            .context("TUI draw thread is not running")?;
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .context("timed out waiting for TUI suspend")?;
        Ok(TuiSuspendGuard {
            draw_ctrl: self.draw_ctrl.clone(),
        })
    }
}

/// Result of a full TUI session after the draw thread exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOutcome {
    Quit,
}

/// Start the frame TUI (alt-screen + draw loop). Returns handles for the host.
pub fn start_session(header_left: String, header_right: String) -> Result<TuiSession> {
    let view = Arc::new(Mutex::new(TurnView {
        header_left,
        header_right,
        ..TurnView::default()
    }));

    let interrupt = Arc::new(AtomicBool::new(false));
    let (action_tx, action_rx) = mpsc::unbounded_channel();
    let (ctrl_tx, ctrl_rx) = std_mpsc::channel();

    let view_draw = view.clone();
    let interrupt_draw = interrupt.clone();
    let draw = tokio::task::spawn_blocking(move || {
        if let Err(err) = draw_loop(view_draw, interrupt_draw, action_tx, ctrl_rx) {
            tracing::error!(error = ?err, "TUI draw loop failed");
        }
    });

    Ok(TuiSession {
        view,
        interrupt,
        actions: action_rx,
        draw_ctrl: ctrl_tx,
        _draw: draw,
    })
}

/// Block until the draw thread finishes (after Quit).
pub async fn join_session(session: TuiSession) -> Result<SessionOutcome> {
    let _ = session._draw.await;
    // Ensure terminal is restored even if draw panicked mid-way.
    let _ = restore_terminal_best_effort();
    Ok(SessionOutcome::Quit)
}

fn draw_loop(
    view: Arc<Mutex<TurnView>>,
    interrupt: Arc<AtomicBool>,
    action_tx: mpsc::UnboundedSender<UserAction>,
    ctrl_rx: std_mpsc::Receiver<DrawCtrl>,
) -> Result<()> {
    let mut terminal = setup_terminal().context("failed to enter TUI terminal")?;
    let result = run_draw(&mut terminal, view, interrupt, action_tx, ctrl_rx);
    // After suspend, terminal may already be restored; best-effort clean up.
    let _ = restore_terminal(&mut terminal);
    result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Mouse capture: wheel scroll + app-side drag select (auto-copies).
    // Native terminal select cannot share the mouse with capture; we paint the
    // highlight and push text to the system clipboard on mouse-up instead.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn restore_terminal_best_effort() -> Result<()> {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, DisableMouseCapture, DisableBracketedPaste);
    let _ = disable_raw_mode();
    let _ = execute!(stdout, LeaveAlternateScreen);
    Ok(())
}

/// Best-effort system clipboard text (Ctrl/Cmd+V fallback when not bracketed).
fn clipboard_text() -> Option<String> {
    #[cfg(not(target_os = "android"))]
    {
        let mut cb = arboard::Clipboard::new().ok()?;
        cb.get_text().ok().filter(|s| !s.is_empty())
    }
    #[cfg(target_os = "android")]
    {
        None
    }
}

/// Best-effort write to the system clipboard (drag-select copy).
fn clipboard_set(text: &str) {
    #[cfg(not(target_os = "android"))]
    {
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text.to_string());
        }
    }
    let _ = text;
}

fn run_draw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    view: Arc<Mutex<TurnView>>,
    interrupt: Arc<AtomicBool>,
    action_tx: mpsc::UnboundedSender<UserAction>,
    ctrl_rx: std_mpsc::Receiver<DrawCtrl>,
) -> Result<()> {
    let mut tick: u64 = 0;
    // Spinner ~10 Hz. Scroll/input redraw immediately on the next loop turn.
    let tick_rate = Duration::from_millis(100);
    let mut last_tick = Instant::now();
    let mut needs_draw = true;
    // Host updates (resume load, stream apply) bump `transcript_gen` from
    // another task. Without watching it, the draw loop only repaints while
    // busy/status — so clearing "Loading conversation…" left that frame stuck.
    let mut last_view_gen = u64::MAX;
    let mut last_esc = Instant::now();

    loop {
        // Host may ask us to leave alt-screen for interactive FurcodeWidget.
        while let Ok(ctrl) = ctrl_rx.try_recv() {
            match ctrl {
                DrawCtrl::Suspend { done } => {
                    let _ = restore_terminal(terminal);
                    let _ = done.send(());
                    // Block until Resume (ignore nested Suspend).
                    loop {
                        match ctrl_rx.recv() {
                            Ok(DrawCtrl::Resume { done }) => {
                                *terminal =
                                    setup_terminal().context("failed to re-enter TUI after suspend")?;
                                needs_draw = true;
                                last_view_gen = u64::MAX;
                                let _ = done.send(());
                                break;
                            }
                            Ok(DrawCtrl::Suspend { done }) => {
                                // Already suspended.
                                let _ = done.send(());
                            }
                            Err(_) => return Ok(()),
                        }
                    }
                }
                DrawCtrl::Resume { done } => {
                    // Spurious resume while active — ack so host does not hang.
                    let _ = done.send(());
                }
            }
        }

        // Always redraw while busy / loading / live activity, after input, or
        // when the host mutates the view (transcript_gen change).
        let (busy, view_gen, toast_on) = {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            let toast_on = v.toast_active();
            (
                v.busy
                    || v.activity.is_busy()
                    || v.status_line.is_some()
                    || v.fork_picker.is_some()
                    || v.command_picker.is_some()
                    || toast_on,
                v.transcript_gen,
                toast_on,
            )
        };
        if needs_draw || busy || view_gen != last_view_gen {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            let _ = v.toast_active();
            last_view_gen = v.transcript_gen;
            terminal.draw(|f| render::draw(f, &mut v, tick))?;
            needs_draw = false;
        }

        let timeout = if busy || toast_on {
            Duration::from_millis(50)
        } else {
            tick_rate.saturating_sub(last_tick.elapsed())
        };

        if !event::poll(timeout)? {
            if last_tick.elapsed() >= tick_rate {
                tick = tick.wrapping_add(1);
                last_tick = Instant::now();
                // ~every 500ms: detect screenshot on clipboard and toast
                // "Image ready · Ctrl+V to paste" (Cmd+V often never arrives).
                if tick % 10 == 0 {
                    let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
                    let before = v.transcript_gen;
                    v.poll_clipboard_image_hint();
                    if v.transcript_gen != before || v.toast.is_some() {
                        needs_draw = true;
                    }
                }
                if busy || toast_on {
                    needs_draw = true;
                }
            }
            continue;
        }

        // Drain the event queue so a wheel burst becomes one scroll + one paint.
        let mut scroll_delta: i32 = 0;
        loop {
            match event::read()? {
                // Press + Repeat: holding PageUp must keep scrolling while busy.
                Event::Key(key)
                    if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat =>
                {
                    // Flush pending wheel scroll before handling keys that
                    // depend on current offset (End / jump).
                    if scroll_delta != 0 {
                        view.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .scroll_by(scroll_delta);
                        scroll_delta = 0;
                    }
                    match handle_key(
                        key.code,
                        key.modifiers,
                        &view,
                        &interrupt,
                        &action_tx,
                        &mut last_esc,
                    )? {
                        KeyResult::Quit => return Ok(()),
                        KeyResult::Changed => {
                            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
                            v.maybe_request_load_more();
                            if v.load_more_requested {
                                let _ = action_tx.send(UserAction::LoadMoreHistory);
                            }
                            needs_draw = true;
                        }
                        KeyResult::ScrollPage { up } => {
                            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
                            let step = v.last_viewport_rows.max(3);
                            if let Some(ov) = v.overlay.as_mut() {
                                if up {
                                    ov.scroll_offset = ov.scroll_offset.saturating_add(step);
                                } else {
                                    ov.scroll_offset = ov.scroll_offset.saturating_sub(step);
                                }
                            } else if let Some(popup) = v.user_prompt_popup.as_mut() {
                                if up {
                                    popup.scroll_offset =
                                        popup.scroll_offset.saturating_add(step);
                                } else {
                                    popup.scroll_offset =
                                        popup.scroll_offset.saturating_sub(step);
                                }
                            } else {
                                v.scroll_page(up);
                                v.maybe_request_load_more();
                                if v.load_more_requested {
                                    let _ = action_tx.send(UserAction::LoadMoreHistory);
                                }
                            }
                            needs_draw = true;
                        }
                        KeyResult::ScrollLines { up, lines } => {
                            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
                            let delta = if up { lines as i32 } else { -(lines as i32) };
                            if let Some(ov) = v.overlay.as_mut() {
                                if up {
                                    ov.scroll_offset = ov.scroll_offset.saturating_add(lines);
                                } else {
                                    ov.scroll_offset = ov.scroll_offset.saturating_sub(lines);
                                }
                            } else {
                                v.scroll_by(delta);
                                v.maybe_request_load_more();
                                if v.load_more_requested {
                                    let _ = action_tx.send(UserAction::LoadMoreHistory);
                                }
                            }
                            needs_draw = true;
                        }
                        KeyResult::Ignored => {}
                    }
                }
                Event::Paste(data) => {
                    if scroll_delta != 0 {
                        view.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .scroll_by(scroll_delta);
                        scroll_delta = 0;
                    }
                    // Cmd+V with a screenshot: terminal may deliver empty
                    // bracketed paste (or nothing). Prefer image when body is
                    // empty; otherwise keep text paste (don't steal paths).
                    let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
                    let prefer_image = data.trim().is_empty();
                    if !v.try_paste_clipboard_image(prefer_image) {
                        if !data.is_empty() {
                            let wrapped = crate::paste::wrap_pasted_text(&data);
                            v.composer.insert_str(&wrapped);
                        }
                    }
                    v.sync_command_picker_from_composer();
                    needs_draw = true;
                }
                Event::Mouse(me) => match me.kind {
                    // Wheel / trackpad — accumulate, apply once after the burst.
                    MouseEventKind::ScrollUp => {
                        // Clear selection when scrolling so highlight stays sane.
                        view.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clear_selection();
                        scroll_delta = scroll_delta.saturating_add(3);
                    }
                    MouseEventKind::ScrollDown => {
                        view.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clear_selection();
                        scroll_delta = scroll_delta.saturating_sub(3);
                    }
                    MouseEventKind::Down(event::MouseButton::Left) => {
                        let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(hit) = v.jump_bottom_hit {
                            if hit.contains(me.column, me.row) {
                                v.scroll_to_bottom();
                                v.clear_selection();
                                v.composer_dragging = false;
                                needs_draw = true;
                                // skip starting a selection on the jump bar
                            } else if v.hit_composer(me.column, me.row) {
                                v.composer_mouse_down(me.column, me.row);
                                needs_draw = true;
                            } else if let Some(full) = v.hit_user_prompt(me.column, me.row) {
                                // Expand truncated › prompt (3+ soft-wrap lines).
                                let text = full.to_string();
                                v.open_user_prompt_popup(text);
                                needs_draw = true;
                            } else {
                                v.selection_begin(me.column, me.row);
                                needs_draw = true;
                            }
                        } else if v.hit_composer(me.column, me.row) {
                            v.composer_mouse_down(me.column, me.row);
                            needs_draw = true;
                        } else if let Some(full) = v.hit_user_prompt(me.column, me.row) {
                            let text = full.to_string();
                            v.open_user_prompt_popup(text);
                            needs_draw = true;
                        } else {
                            v.selection_begin(me.column, me.row);
                            needs_draw = true;
                        }
                    }
                    MouseEventKind::Drag(event::MouseButton::Left) => {
                        let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
                        if v.composer_dragging {
                            v.composer_mouse_drag(me.column, me.row);
                        } else {
                            v.selection_update(me.column, me.row);
                        }
                        needs_draw = true;
                    }
                    MouseEventKind::Up(event::MouseButton::Left) => {
                        let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
                        if v.composer_dragging {
                            // Keep selection for delete / copy / paste-replace.
                            // (Unlike transcript drag, do not auto-copy.)
                            v.composer_mouse_up();
                        } else if let Some(text) = v.selection_end() {
                            clipboard_set(&text);
                            v.toast_copied(&text);
                        }
                        needs_draw = true;
                    }
                    _ => {}
                },
                Event::Resize(_, _) => needs_draw = true,
                _ => {}
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
        if scroll_delta != 0 {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ov) = v.overlay.as_mut() {
                if scroll_delta > 0 {
                    ov.scroll_offset = ov.scroll_offset.saturating_add(scroll_delta as usize);
                } else {
                    ov.scroll_offset = ov
                        .scroll_offset
                        .saturating_sub((-scroll_delta) as usize);
                }
            } else if let Some(popup) = v.user_prompt_popup.as_mut() {
                if scroll_delta > 0 {
                    popup.scroll_offset =
                        popup.scroll_offset.saturating_add(scroll_delta as usize);
                } else {
                    popup.scroll_offset = popup
                        .scroll_offset
                        .saturating_sub((-scroll_delta) as usize);
                }
            } else {
                v.scroll_by(scroll_delta);
                v.maybe_request_load_more();
                if v.load_more_requested {
                    let _ = action_tx.send(UserAction::LoadMoreHistory);
                }
            }
            needs_draw = true;
        }

        if last_tick.elapsed() >= tick_rate {
            tick = tick.wrapping_add(1);
            last_tick = Instant::now();
            if busy {
                needs_draw = true;
            }
        }
    }
}

enum KeyResult {
    Quit,
    Changed,
    ScrollPage { up: bool },
    /// Fine-grained scroll (Alt+Up/Down or Ctrl+Y/E style line step).
    ScrollLines { up: bool, lines: usize },
    Ignored,
}

fn handle_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    view: &Arc<Mutex<TurnView>>,
    interrupt: &Arc<AtomicBool>,
    action_tx: &mpsc::UnboundedSender<UserAction>,
    last_esc: &mut Instant,
) -> Result<KeyResult> {
    // Slash-command menu swallows navigation / select first.
    {
        let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
        if v.command_picker.is_some() {
            match (code, modifiers) {
                (KeyCode::Char('c'), KeyModifiers::CONTROL)
                | (KeyCode::Char('q'), KeyModifiers::CONTROL) => return Ok(KeyResult::Quit),
                (KeyCode::Esc, _) => {
                    // Dismiss menu; leave whatever is in the composer (`/query`).
                    v.command_picker = None;
                    v.touch_transcript();
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::Up, _)
                | (KeyCode::PageUp, _)
                | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                    if let Some(picker) = v.command_picker.as_mut() {
                        if picker.selected == 0 {
                            picker.selected = picker.filtered.len().saturating_sub(1);
                        } else {
                            picker.selected -= 1;
                        }
                    }
                    v.touch_transcript();
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::Down, _)
                | (KeyCode::PageDown, _)
                | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                    if let Some(picker) = v.command_picker.as_mut() {
                        if picker.selected + 1 >= picker.filtered.len() {
                            picker.selected = 0;
                        } else {
                            picker.selected += 1;
                        }
                    }
                    v.touch_transcript();
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::Home, _) => {
                    if let Some(picker) = v.command_picker.as_mut() {
                        picker.selected = 0;
                    }
                    v.touch_transcript();
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::End, _) => {
                    if let Some(picker) = v.command_picker.as_mut() {
                        picker.selected = picker.filtered.len().saturating_sub(1);
                    }
                    v.touch_transcript();
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::Tab, _) => {
                    // Complete selected command into the composer (do not run yet).
                    if let Some(item) = v
                        .command_picker
                        .as_ref()
                        .and_then(|p| p.selected_item())
                        .cloned()
                    {
                        let insert = format!("{} ", item.name);
                        v.composer.set_text(insert);
                        v.command_picker = None;
                        v.touch_transcript();
                    }
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::Enter, _) => {
                    // Prefer the highlighted command; otherwise submit the typed buffer.
                    let selected = v
                        .command_picker
                        .as_ref()
                        .and_then(|p| p.selected_item())
                        .map(|i| i.name.clone());
                    v.command_picker = None;
                    if let Some(name) = selected {
                        // Put the chosen command in the composer and submit via the
                        // normal path (handles busy → sticky queue correctly).
                        v.composer.set_text(name);
                    }
                    v.scroll_to_bottom();
                    if let Some(msg) = v.submit_composer() {
                        if !TurnView::composer_is_slash_command(&msg) {
                            v.busy = true;
                        }
                        drop(v);
                        let _ = action_tx.send(UserAction::Send(msg));
                    }
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::Backspace, m)
                    if m == KeyModifiers::NONE
                        || m.contains(KeyModifiers::ALT)
                        || m.contains(KeyModifiers::CONTROL) =>
                {
                    if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) {
                        v.composer.delete_word_backward();
                    } else {
                        v.composer.backspace();
                    }
                    v.sync_command_picker_from_composer();
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::Char(c), m)
                    if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT =>
                {
                    if v.resume_prompt.is_some()
                        || matches!(
                            v.status_tone,
                            crate::tui::StatusTone::Interrupted | crate::tui::StatusTone::Hint
                        )
                    {
                        v.disarm_interrupt_resume();
                    }
                    v.composer.insert_char(c);
                    v.sync_command_picker_from_composer();
                    return Ok(KeyResult::Changed);
                }
                // Left/Right still move the caret inside the `/query` token.
                (KeyCode::Left, m) if m == KeyModifiers::NONE => {
                    v.composer.move_left(false);
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::Right, m) if m == KeyModifiers::NONE => {
                    v.composer.move_right(false);
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::Left, m) if m == KeyModifiers::SHIFT => {
                    v.composer.move_left(true);
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::Right, m) if m == KeyModifiers::SHIFT => {
                    v.composer.move_right(true);
                    return Ok(KeyResult::Changed);
                }
                _ => return Ok(KeyResult::Ignored),
            }
        }
    }

    // Fork picker (double-Esc) swallows navigation keys first.
    {
        let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(picker) = v.fork_picker.as_mut() {
            match (code, modifiers) {
                (KeyCode::Char('c'), KeyModifiers::CONTROL)
                | (KeyCode::Char('q'), KeyModifiers::CONTROL) => return Ok(KeyResult::Quit),
                (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => {
                    v.fork_picker = None;
                    v.touch_transcript();
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::Up, _) | (KeyCode::PageUp, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                    if picker.selected > 0 {
                        picker.selected -= 1;
                    }
                    v.touch_transcript();
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::Down, _)
                | (KeyCode::PageDown, _)
                | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                    if picker.selected + 1 < picker.items.len() {
                        picker.selected += 1;
                    }
                    v.touch_transcript();
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::Home, _) => {
                    picker.selected = 0;
                    v.touch_transcript();
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::End, _) => {
                    picker.selected = picker.items.len().saturating_sub(1);
                    v.touch_transcript();
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::Enter, _) => {
                    if let Some(item) = picker.items.get(picker.selected).cloned() {
                        v.fork_picker = None;
                        v.touch_transcript();
                        drop(v);
                        let _ = action_tx.send(UserAction::ForkSelect {
                            message_index: item.message_index,
                            edit_text: item.edit_text,
                        });
                    }
                    return Ok(KeyResult::Changed);
                }
                _ => return Ok(KeyResult::Ignored),
            }
        }
    }

    // Overlay / user-prompt popup swallow most keys.
    {
        let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
        if v.overlay.is_some() || v.user_prompt_popup.is_some() {
            match (code, modifiers) {
                (KeyCode::Char('c'), KeyModifiers::CONTROL)
                | (KeyCode::Char('q'), KeyModifiers::CONTROL) => return Ok(KeyResult::Quit),
                (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => {
                    if v.user_prompt_popup.is_some() {
                        v.close_user_prompt_popup();
                    } else {
                        v.close_overlay();
                    }
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::PageUp, _) | (KeyCode::Up, _) => {
                    return Ok(KeyResult::ScrollPage { up: true });
                }
                (KeyCode::PageDown, _) | (KeyCode::Down, _) => {
                    return Ok(KeyResult::ScrollPage { up: false });
                }
                (KeyCode::Home, _) => {
                    if let Some(ov) = v.overlay.as_mut() {
                        let max = ov.lines.len().saturating_sub(1);
                        ov.scroll_offset = max;
                    } else if let Some(popup) = v.user_prompt_popup.as_mut() {
                        popup.scroll_offset = usize::MAX / 4; // clamp in draw
                    }
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::End, _) => {
                    if let Some(ov) = v.overlay.as_mut() {
                        ov.scroll_offset = 0;
                    } else if let Some(popup) = v.user_prompt_popup.as_mut() {
                        popup.scroll_offset = 0;
                    }
                    return Ok(KeyResult::Changed);
                }
                _ => return Ok(KeyResult::Ignored),
            }
        }
    }

    match (code, modifiers) {
        (KeyCode::Char('q'), KeyModifiers::CONTROL) => Ok(KeyResult::Quit),
        // Ctrl+C: copy composer selection when present; otherwise quit.
        (KeyCode::Char('c' | 'C'), m)
            if m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::SUPER) =>
        {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(text) = v.composer.selected_text().map(str::to_owned) {
                drop(v);
                clipboard_set(&text);
                Ok(KeyResult::Changed)
            } else if m.contains(KeyModifiers::CONTROL)
                && !m.contains(KeyModifiers::SUPER)
                && !m.contains(KeyModifiers::ALT)
                && !m.contains(KeyModifiers::SHIFT)
            {
                Ok(KeyResult::Quit)
            } else {
                Ok(KeyResult::Ignored)
            }
        }
        // Cut selection (Ctrl/Cmd+X).
        (KeyCode::Char('x' | 'X'), m)
            if m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::SUPER) =>
        {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(text) = v.composer.selected_text().map(str::to_owned) {
                clipboard_set(&text);
                v.composer.delete_selection();
                v.sync_command_picker_from_composer();
                Ok(KeyResult::Changed)
            } else {
                Ok(KeyResult::Ignored)
            }
        }
        // Select all (Ctrl/Cmd+A). Home still jumps to buffer start.
        (KeyCode::Char('a' | 'A'), m)
            if m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::SUPER) =>
        {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .select_all();
            Ok(KeyResult::Changed)
        }
        (KeyCode::PageUp, _) => Ok(KeyResult::ScrollPage { up: true }),
        (KeyCode::PageDown, _) => Ok(KeyResult::ScrollPage { up: false }),
        // Plain ↑/↓: move caret. Shift+↑/↓: extend selection. Other modifiers
        // scroll the transcript. On the first/last visual row, jump to
        // start/end; only scroll when already at that edge.
        (KeyCode::Up, m) if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT => {
            let extend = m.contains(KeyModifiers::SHIFT);
            let content_w = composer_term_content_width();
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            if v.composer.move_up(content_w, extend) {
                Ok(KeyResult::Changed)
            } else if extend {
                Ok(KeyResult::Changed)
            } else {
                Ok(KeyResult::ScrollLines { up: true, lines: 3 })
            }
        }
        (KeyCode::Down, m) if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT => {
            let extend = m.contains(KeyModifiers::SHIFT);
            let content_w = composer_term_content_width();
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            if v.composer.move_down(content_w, extend) {
                Ok(KeyResult::Changed)
            } else if extend {
                Ok(KeyResult::Changed)
            } else {
                Ok(KeyResult::ScrollLines {
                    up: false,
                    lines: 3,
                })
            }
        }
        (KeyCode::Up, _) => Ok(KeyResult::ScrollLines { up: true, lines: 3 }),
        (KeyCode::Down, _) => Ok(KeyResult::ScrollLines { up: false, lines: 3 }),
        (KeyCode::End, m) if m.contains(KeyModifiers::SHIFT) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_end(true);
            Ok(KeyResult::Changed)
        }
        (KeyCode::End, m)
            if m.contains(KeyModifiers::CONTROL) || m == KeyModifiers::NONE =>
        {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            if !v.is_at_bottom() || m.contains(KeyModifiers::CONTROL) {
                v.scroll_to_bottom();
            } else {
                v.composer.move_end(false);
            }
            Ok(KeyResult::Changed)
        }
        (KeyCode::Home, m) if m.contains(KeyModifiers::CONTROL) => {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            v.ensure_scroll_metrics_public();
            v.follow_bottom = false;
            v.viewport_end = v.last_viewport_rows.max(1).min(v.last_content_lines.max(1));
            v.sync_scroll_offset_public();
            v.maybe_request_load_more();
            if v.load_more_requested {
                let _ = action_tx.send(UserAction::LoadMoreHistory);
            }
            Ok(KeyResult::Changed)
        }
        (KeyCode::Esc, _) => {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            // Esc priority: clear composer text first; only then interrupt / fork.
            if !v.composer.buffer.is_empty() {
                // Any draft (including whitespace-only) is discarded.
                // Do not arm double-Esc or interrupt while text remains.
                *last_esc = Instant::now() - Duration::from_secs(1);
                v.composer.clear();
            } else if v.busy {
                // Empty composer + in-flight turn → interrupt.
                v.request_interrupt();
                interrupt.store(true, Ordering::SeqCst);
                let _ = action_tx.send(UserAction::Interrupt);
            } else if last_esc.elapsed() < Duration::from_millis(900) {
                // Double-Esc on empty composer → pick a prior user message to
                // restore conversation (+ undo file writes after that turn).
                if v.overlay.is_some() {
                    v.close_overlay();
                }
                let _ = action_tx.send(UserAction::ForkHistory);
                *last_esc = Instant::now() - Duration::from_secs(1);
            } else {
                // First Esc on empty: arm double-Esc window (no-op UI).
                *last_esc = Instant::now();
            }
            Ok(KeyResult::Changed)
        }
        // Alt/Option+Enter inserts a hard newline (multi-line draft).
        (KeyCode::Enter, m) if m.contains(KeyModifiers::ALT) => {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            v.composer.insert_char('\n');
            v.sync_command_picker_from_composer();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Enter, _) => {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            v.command_picker = None;
            v.scroll_to_bottom();
            if let Some(msg) = v.submit_composer() {
                // Slash / shell commands are handled by the host without busy chrome.
                if !TurnView::composer_is_slash_command(&msg) {
                    v.busy = true;
                }
                drop(v);
                let _ = action_tx.send(UserAction::Send(msg));
            }
            Ok(KeyResult::Changed)
        }
        (KeyCode::Backspace, m)
            if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
        {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            v.composer.delete_word_backward();
            v.sync_command_picker_from_composer();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Delete, m)
            if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
        {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            v.composer.delete_word_forward();
            v.sync_command_picker_from_composer();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('\u{7f}' | '\u{8}'), m)
            if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
        {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            v.composer.delete_word_backward();
            v.sync_command_picker_from_composer();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Left, m)
            if (m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL))
                && m.contains(KeyModifiers::SHIFT) =>
        {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_word_left(true);
            Ok(KeyResult::Changed)
        }
        (KeyCode::Right, m)
            if (m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL))
                && m.contains(KeyModifiers::SHIFT) =>
        {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_word_right(true);
            Ok(KeyResult::Changed)
        }
        (KeyCode::Left, m)
            if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
        {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_word_left(false);
            Ok(KeyResult::Changed)
        }
        (KeyCode::Right, m)
            if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
        {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_word_right(false);
            Ok(KeyResult::Changed)
        }
        (KeyCode::Backspace, _) => {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            v.composer.backspace();
            v.sync_command_picker_from_composer();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Delete, _) => {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            v.composer.delete_forward();
            v.sync_command_picker_from_composer();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Left, m) if m.contains(KeyModifiers::SHIFT) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_left(true);
            Ok(KeyResult::Changed)
        }
        (KeyCode::Right, m) if m.contains(KeyModifiers::SHIFT) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_right(true);
            Ok(KeyResult::Changed)
        }
        (KeyCode::Left, _) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_left(false);
            Ok(KeyResult::Changed)
        }
        (KeyCode::Right, _) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_right(false);
            Ok(KeyResult::Changed)
        }
        (KeyCode::Home, m) if m.contains(KeyModifiers::SHIFT) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_home(true);
            Ok(KeyResult::Changed)
        }
        (KeyCode::Home, _) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_home(false);
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_end(false);
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            v.composer.delete_word_backward();
            v.sync_command_picker_from_composer();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            v.composer.backspace();
            v.sync_command_picker_from_composer();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('d'), m) if m.contains(KeyModifiers::ALT) => {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            v.composer.delete_word_forward();
            v.sync_command_picker_from_composer();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('b'), m) if m.contains(KeyModifiers::ALT) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_word_left(false);
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('f'), m) if m.contains(KeyModifiers::ALT) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_word_right(false);
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('v' | 'V'), m)
            if m.contains(KeyModifiers::CONTROL)
                || m.contains(KeyModifiers::ALT)
                || m.contains(KeyModifiers::SUPER) =>
        {
            // Key-event paste (when the terminal does not emit Event::Paste).
            // Prefer image: terminals rarely forward Cmd+V for image-only
            // clipboards, so Ctrl+V is the reliable attach chord. PreferImage
            // also wins when the toast just said "Image ready".
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            if !v.try_paste_clipboard_image(true) {
                if let Some(text) = clipboard_text() {
                    let wrapped = crate::paste::wrap_pasted_text(&text);
                    v.composer.insert_str(&wrapped);
                }
            }
            v.sync_command_picker_from_composer();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char(c), m) if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT => {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            // Typing a new draft dismisses Interrupted / resume hint chrome.
            if v.resume_prompt.is_some()
                || matches!(
                    v.status_tone,
                    crate::tui::StatusTone::Interrupted | crate::tui::StatusTone::Hint
                )
            {
                v.disarm_interrupt_resume();
            }
            v.composer.insert_char(c);
            // Opening `/` at the start of the buffer (or refining `/query`)
            // shows the slash-command menu.
            v.sync_command_picker_from_composer();
            Ok(KeyResult::Changed)
        }
        _ => Ok(KeyResult::Ignored),
    }
}

/// Content width for composer caret math, matching the draw layout.
fn composer_term_content_width() -> usize {
    let term_w = crossterm::terminal::size()
        .map(|(w, _)| w)
        .unwrap_or(80);
    super::view::composer_content_width(term_w)
}
