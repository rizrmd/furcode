//! Crossterm + ratatui session loop for the frame-based TUI.
//!
//! The draw loop runs on a blocking thread and shares [`TurnView`] via
//! `Arc<Mutex<_>>`. The host async task drives the agent stream and applies
//! [`ChatResponse`] events into the same view.

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Idle Enter: send this message on the agent path.
    Send(String),
    /// Esc while busy.
    Interrupt,
    /// Reveal older turns from the slim history archive.
    LoadMoreHistory,
    /// Ctrl+C / quit.
    Quit,
    /// Double-Esc in idle state: undo last file change.
    UndoHistory(Option<String>),
}

/// Shared session state between draw thread and async host.
pub struct TuiSession {
    pub view: Arc<Mutex<TurnView>>,
    /// Set by Esc; host aborts the in-flight chat future.
    pub interrupt: Arc<AtomicBool>,
    pub actions: mpsc::UnboundedReceiver<UserAction>,
    _draw: tokio::task::JoinHandle<()>,
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

    let view_draw = view.clone();
    let interrupt_draw = interrupt.clone();
    let draw = tokio::task::spawn_blocking(move || {
        if let Err(err) = draw_loop(view_draw, interrupt_draw, action_tx) {
            tracing::error!(error = ?err, "TUI draw loop failed");
        }
    });

    Ok(TuiSession {
        view,
        interrupt,
        actions: action_rx,
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
) -> Result<()> {
    let mut terminal = setup_terminal().context("failed to enter TUI terminal")?;
    let result = run_draw(&mut terminal, view, interrupt, action_tx);
    restore_terminal(&mut terminal).context("failed to restore terminal")?;
    result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Mouse capture is required for wheel / trackpad scroll. Trade-off: the
    // terminal no longer does native drag-to-select (use Cmd/Ctrl+C after
    // selecting with Shift held in some terminals, or copy from overlay).
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

fn run_draw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    view: Arc<Mutex<TurnView>>,
    interrupt: Arc<AtomicBool>,
    action_tx: mpsc::UnboundedSender<UserAction>,
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
        // Always redraw while busy / loading / live activity, after input, or
        // when the host mutates the view (transcript_gen change).
        let (busy, view_gen) = {
            let v = view.lock().unwrap_or_else(|e| e.into_inner());
            (
                v.busy || v.activity.is_busy() || v.status_line.is_some(),
                v.transcript_gen,
            )
        };
        if needs_draw || busy || view_gen != last_view_gen {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            last_view_gen = v.transcript_gen;
            terminal.draw(|f| render::draw(f, &mut v, tick))?;
            needs_draw = false;
        }

        let timeout = if busy {
            Duration::from_millis(50)
        } else {
            tick_rate.saturating_sub(last_tick.elapsed())
        };

        if !event::poll(timeout)? {
            if last_tick.elapsed() >= tick_rate {
                tick = tick.wrapping_add(1);
                last_tick = Instant::now();
                if busy {
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
                    view.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .composer
                        .insert_str(&data);
                    needs_draw = true;
                }
                Event::Mouse(me) => match me.kind {
                    // Wheel / trackpad: accumulate, apply once after the burst.
                    // ScrollUp = older history (positive scroll_by).
                    MouseEventKind::ScrollUp => {
                        scroll_delta = scroll_delta.saturating_add(3);
                    }
                    MouseEventKind::ScrollDown => {
                        scroll_delta = scroll_delta.saturating_sub(3);
                    }
                    // Click the "↓ Jump to bottom" bar to re-follow.
                    MouseEventKind::Down(event::MouseButton::Left) => {
                        let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(hit) = v.jump_bottom_hit {
                            if hit.contains(me.column, me.row) {
                                v.scroll_to_bottom();
                                needs_draw = true;
                            }
                        }
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
    // Overlay swallows most keys.
    {
        let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
        if v.overlay.is_some() {
            match (code, modifiers) {
                (KeyCode::Char('c'), KeyModifiers::CONTROL)
                | (KeyCode::Char('q'), KeyModifiers::CONTROL) => return Ok(KeyResult::Quit),
                (KeyCode::Char('t'), KeyModifiers::CONTROL)
                | (KeyCode::Esc, _)
                | (KeyCode::Char('q'), KeyModifiers::NONE) => {
                    v.close_overlay();
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
                    }
                    return Ok(KeyResult::Changed);
                }
                (KeyCode::End, _) => {
                    if let Some(ov) = v.overlay.as_mut() {
                        ov.scroll_offset = 0;
                    }
                    return Ok(KeyResult::Changed);
                }
                _ => return Ok(KeyResult::Ignored),
            }
        }
    }

    match (code, modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL)
        | (KeyCode::Char('q'), KeyModifiers::CONTROL) => Ok(KeyResult::Quit),
        // Codex-style transcript overlay.
        (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .toggle_overlay();
            Ok(KeyResult::Changed)
        }
        (KeyCode::PageUp, _) => Ok(KeyResult::ScrollPage { up: true }),
        (KeyCode::PageDown, _) => Ok(KeyResult::ScrollPage { up: false }),
        // Alt/Ctrl+Up|Down: scroll transcript even while Working (arrows alone
        // stay reserved for the composer caret).
        (KeyCode::Up, m)
            if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
        {
            Ok(KeyResult::ScrollLines { up: true, lines: 3 })
        }
        (KeyCode::Down, m)
            if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
        {
            Ok(KeyResult::ScrollLines { up: false, lines: 3 })
        }
        (KeyCode::End, m)
            if m.contains(KeyModifiers::CONTROL) || m == KeyModifiers::NONE =>
        {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            if !v.is_at_bottom() || m.contains(KeyModifiers::CONTROL) {
                v.scroll_to_bottom();
            } else {
                v.composer.move_end();
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
        // Shift+Up/Down — reliable on macOS laptops where PgUp needs Fn.
        (KeyCode::Up, m) if m.contains(KeyModifiers::SHIFT) => {
            Ok(KeyResult::ScrollLines { up: true, lines: 5 })
        }
        (KeyCode::Down, m) if m.contains(KeyModifiers::SHIFT) => {
            Ok(KeyResult::ScrollLines { up: false, lines: 5 })
        }
        (KeyCode::Esc, _) => {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            if v.busy {
                v.request_interrupt();
                interrupt.store(true, Ordering::SeqCst);
                let _ = action_tx.send(UserAction::Interrupt);
            } else if last_esc.elapsed() < Duration::from_millis(400) {
                let _ = action_tx.send(UserAction::UndoHistory(None));
                *last_esc = Instant::now() - Duration::from_secs(1);
            } else {
                *last_esc = Instant::now();
                v.composer.clear();
            }
            Ok(KeyResult::Changed)
        }
        (KeyCode::Enter, _) => {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            v.scroll_to_bottom();
            if let Some(msg) = v.submit_composer() {
                v.busy = true;
                drop(v);
                let _ = action_tx.send(UserAction::Send(msg));
            }
            Ok(KeyResult::Changed)
        }
        (KeyCode::Backspace, m)
            if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
        {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .delete_word_backward();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Delete, m)
            if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
        {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .delete_word_forward();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('\u{7f}' | '\u{8}'), m)
            if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
        {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .delete_word_backward();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Left, m)
            if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
        {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_word_left();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Right, m)
            if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
        {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_word_right();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Backspace, _) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .backspace();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Delete, _) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .delete_forward();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Left, _) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_left();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Right, _) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_right();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Home, _) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_home();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_home();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_end();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .delete_word_backward();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .backspace();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('d'), m) if m.contains(KeyModifiers::ALT) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .delete_word_forward();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('b'), m) if m.contains(KeyModifiers::ALT) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_word_left();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('f'), m) if m.contains(KeyModifiers::ALT) => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .move_word_right();
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char('v' | 'V'), m)
            if m.contains(KeyModifiers::CONTROL)
                || m.contains(KeyModifiers::ALT)
                || m.contains(KeyModifiers::SUPER) =>
        {
            if let Some(text) = clipboard_text() {
                view.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .composer
                    .insert_str(&text);
            }
            Ok(KeyResult::Changed)
        }
        (KeyCode::Char(c), m) if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT => {
            view.lock()
                .unwrap_or_else(|e| e.into_inner())
                .composer
                .insert_char(c);
            Ok(KeyResult::Changed)
        }
        _ => Ok(KeyResult::Ignored),
    }
}
