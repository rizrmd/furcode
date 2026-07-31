use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use console::{measure_text_width, strip_ansi_codes};
use furcode_api::Environment;
use nu_ansi_term::Style;
use rustyline::completion::{Completer, Pair};
use rustyline::config::{ColorMode, CompletionType, Config};
use rustyline::error::ReadlineError as RustyReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hinter, HistoryHinter};
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{
    Cmd, ConditionalEventHandler, Context as RustylineContext, Editor, Event, EventContext,
    EventHandler, Helper, KeyCode, KeyEvent, Modifiers, Movement, Prompt as RustylinePrompt,
    RepeatCount,
};

use super::completer::{InputCompleter, pick_command};
use super::paste::{
    expand_image_atoms, image_atom_for_backspace, image_atom_for_delete,
    try_paste_clipboard_image, wrap_pasted_text,
};
use crate::highlighter::FurcodeHighlighter;
use crate::model::FurcodeCommandManager;
use crate::prompt::FurcodePrompt;

const HISTORY_CAPACITY: usize = 1024 * 1024;
/// Window in which two Esc presses count as "double Esc" for message fork.
///
/// Must be longer than rustyline's Esc keyseq timeout so the second Esc is
/// still accepted after the first Esc was delivered as a lone key.
const DOUBLE_ESC_WINDOW: Duration = Duration::from_millis(900);
/// Esc sequence disambiguation timeout (ms).
///
/// Emacs-mode default is `None` (wait forever for the next byte after Esc).
/// That makes a second Esc get absorbed into the first key's escape sequence
/// (`Esc Esc` → one Esc event), so double-Esc can never fire. A zero timeout
/// delivers a lone Esc immediately when no CSI/`[` is already buffered (arrow
/// keys still work — they arrive as a burst).
const ESC_KEYSEQ_TIMEOUT_MS: u16 = 0;

/// Interactive terminal editor used by the Furcode prompt.
pub struct FurcodeEditor {
    editor: Editor<FurcodeHelper, DefaultHistory>,
    history_file: PathBuf,
    pending_buffer: Option<String>,
    /// Set when the user double-taps Esc on an empty line (fork/edit history).
    fork_requested: Arc<AtomicBool>,
    /// Timestamp of the last lone Esc on an empty line (for double-Esc).
    last_esc: Arc<Mutex<Option<Instant>>>,
}

/// Result of reading one prompt interaction from the terminal.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadResult {
    Success(String),
    Empty,
    Continue,
    Exit,
    /// User double-tapped Esc — open previous-message picker and fork.
    ForkHistory,
}

impl FurcodeEditor {
    /// Creates a new interactive editor with history, completion, and
    /// highlighting.
    pub fn new(
        env: Environment,
        custom_history_path: Option<PathBuf>,
        manager: Arc<FurcodeCommandManager>,
    ) -> Self {
        let history_file = env.history_path(custom_history_path.as_ref());
        let helper = FurcodeHelper::new(env.cwd, manager.clone());
        let config = Config::builder()
            .max_history_size(HISTORY_CAPACITY)
            .expect("rustyline history capacity should be valid")
            .completion_type(CompletionType::List)
            .completion_show_all_if_ambiguous(true)
            .color_mode(ColorMode::Forced)
            .enable_signals(true)
            // Required for reliable Esc / double-Esc (see ESC_KEYSEQ_TIMEOUT_MS).
            .keyseq_timeout(Some(ESC_KEYSEQ_TIMEOUT_MS))
            .build();
        let mut editor = Editor::<FurcodeHelper, DefaultHistory>::with_config(config)
            .expect("rustyline editor should initialize for an interactive terminal");
        editor.bind_sequence(
            KeyEvent(KeyCode::Enter, Modifiers::ALT),
            EventHandler::Simple(Cmd::Newline),
        );
        editor.bind_sequence(
            KeyEvent(KeyCode::Char('k'), Modifiers::CTRL),
            EventHandler::Simple(Cmd::ClearScreen),
        );
        editor.bind_sequence(
            KeyEvent(KeyCode::Char('K'), Modifiers::CTRL),
            EventHandler::Simple(Cmd::ClearScreen),
        );

        // Typing `/` on an empty (or `/`-only) line opens the command popup
        // immediately — no need to press Enter/Tab first.
        let slash_handler = CommandMenuKeyHandler {
            commands: manager.clone(),
        };
        editor.bind_sequence(
            KeyEvent(KeyCode::Char('/'), Modifiers::NONE),
            EventHandler::Conditional(Box::new(slash_handler)),
        );

        // Double-Esc on an empty line → fork/edit a previous user message.
        let fork_requested = Arc::new(AtomicBool::new(false));
        let last_esc = Arc::new(Mutex::new(None));
        let esc_handler = DoubleEscHandler {
            last_esc: last_esc.clone(),
            fork_requested: fork_requested.clone(),
        };
        editor.bind_sequence(
            KeyEvent(KeyCode::Esc, Modifiers::NONE),
            EventHandler::Conditional(Box::new(esc_handler)),
        );

        // Image paste:
        // - Ctrl/Alt+V: key chord (when the terminal delivers it)
        // - BracketedPasteStart: Cmd+V on macOS (terminal injects paste, never
        //   Super+V). If clipboard has an image, insert `@[png]` and drain the
        //   paste body via Event::Any so empty/garbage image pastes don't leak.
        let drain_paste = Arc::new(AtomicBool::new(false));
        let image_paste = ImagePasteHandler {
            drain_paste: drain_paste.clone(),
        };
        editor.bind_sequence(
            KeyEvent(KeyCode::Char('v'), Modifiers::CTRL),
            EventHandler::Conditional(Box::new(image_paste.clone())),
        );
        editor.bind_sequence(
            KeyEvent(KeyCode::Char('V'), Modifiers::CTRL),
            EventHandler::Conditional(Box::new(image_paste.clone())),
        );
        // Meta/Alt+V — some terminals map Cmd+V-like chords here.
        editor.bind_sequence(
            KeyEvent(KeyCode::Char('v'), Modifiers::ALT),
            EventHandler::Conditional(Box::new(image_paste.clone())),
        );
        editor.bind_sequence(
            KeyEvent(KeyCode::Char('V'), Modifiers::ALT),
            EventHandler::Conditional(Box::new(image_paste.clone())),
        );
        // Cmd+V → terminal bracketed paste (not a Super key event).
        editor.bind_sequence(
            KeyEvent(KeyCode::BracketedPasteStart, Modifiers::NONE),
            EventHandler::Conditional(Box::new(image_paste.clone())),
        );
        // While draining an image paste body, swallow keys until PasteEnd.
        editor.bind_sequence(
            Event::Any,
            EventHandler::Conditional(Box::new(PasteDrainHandler { drain_paste })),
        );

        // Backspace / Delete remove a whole `[Image N]` atom in one keystroke.
        editor.bind_sequence(
            KeyEvent(KeyCode::Backspace, Modifiers::NONE),
            EventHandler::Conditional(Box::new(ImageAtomBackspaceHandler)),
        );
        editor.bind_sequence(
            KeyEvent(KeyCode::Delete, Modifiers::NONE),
            EventHandler::Conditional(Box::new(ImageAtomDeleteHandler)),
        );

        editor.set_helper(Some(helper));
        let _ = editor.load_history(&history_file);
        Self {
            editor,
            history_file,
            pending_buffer: None,
            fork_requested,
            last_esc,
        }
    }

    fn normalize_result(&mut self, buffer: String) -> ReadResult {
        let trimmed = buffer.trim();
        if trimmed.is_empty() {
            return ReadResult::Empty;
        }
        // History keeps the short `[Image N]` form for readability.
        let _ = self.editor.add_history_entry(trimmed);
        let _ = self.editor.save_history(&self.history_file);
        // Expand atoms → `@[path]` so attachments load for the agent.
        let expanded = expand_image_atoms(trimmed);
        ReadResult::Success(wrap_pasted_text(&expanded))
    }

    /// Reads one logical input from the terminal.
    pub fn prompt(&mut self, prompt: &mut FurcodePrompt) -> anyhow::Result<ReadResult> {
        self.fork_requested.store(false, Ordering::SeqCst);
        // Drop a half-completed double-Esc from a previous prompt.
        if let Ok(mut guard) = self.last_esc.lock() {
            *guard = None;
        }
        let prompt_text = render_prompt(prompt);
        let initial = self.pending_buffer.take().unwrap_or_default();
        let readline = if initial.is_empty() {
            self.editor.readline(&prompt_text)
        } else {
            self.editor
                .readline_with_initial(&prompt_text, (&initial, ""))
        };
        prompt.refresh();

        match readline {
            Ok(buffer) => Ok(self.normalize_result(buffer)),
            Err(RustyReadlineError::Interrupted) => {
                if self.fork_requested.swap(false, Ordering::SeqCst) {
                    Ok(ReadResult::ForkHistory)
                } else {
                    Ok(ReadResult::Continue)
                }
            }
            Err(RustyReadlineError::Eof) => Ok(ReadResult::Exit),
            Err(error) => Err(anyhow::anyhow!(ReadLineError(error))),
        }
    }

    /// Sets the buffer content to be pre-filled on the next prompt.
    pub fn set_buffer(&mut self, content: String) {
        self.pending_buffer = Some(content);
    }
}

#[derive(Debug, thiserror::Error)]
#[error("failed to read line from terminal: {0}")]
pub struct ReadLineError(RustyReadlineError);

fn normalize_result_text(buffer: String) -> ReadResult {
    let trimmed = buffer.trim();
    if trimmed.is_empty() {
        return ReadResult::Empty;
    }
    // Expand `[Image N]` display atoms → `@[path]` for the attachment pipeline.
    let expanded = expand_image_atoms(trimmed);
    ReadResult::Success(wrap_pasted_text(&expanded))
}

/// Builds a kill command that removes `line[start..end)` relative to `pos`.
///
/// Uses a single [`Cmd::Kill`] when the cursor sits at one edge of the range
/// (the common case after paste). Falls back to whole-buffer rewrite otherwise
/// so a mid-atom cursor still drops the full atom.
fn kill_byte_range(line: &str, pos: usize, start: usize, end: usize) -> Cmd {
    let start = start.min(line.len());
    let end = end.min(line.len()).max(start);
    if pos == end {
        // Cursor at end of range → kill backward (ASCII atoms: byte == char count).
        let n = (end - start) as RepeatCount;
        return Cmd::Kill(Movement::BackwardChar(n.max(1)));
    }
    if pos == start {
        let n = (end - start) as RepeatCount;
        return Cmd::Kill(Movement::ForwardChar(n.max(1)));
    }
    // Cursor inside the atom — rewrite the buffer without it.
    let mut new_line = String::with_capacity(line.len().saturating_sub(end - start));
    new_line.push_str(&line[..start]);
    new_line.push_str(&line[end..]);
    Cmd::Replace(Movement::WholeBuffer, Some(new_line))
}

fn render_prompt(prompt: &FurcodePrompt) -> ResponsivePrompt {
    let left = prompt.render_prompt_left();
    let indicator = prompt.render_prompt_indicator();
    let right = prompt.render_prompt_right();
    let right = right.trim_start();

    // `raw` is what rustyline measures to position the cursor; `styled` is what
    // it prints. `raw` MUST be free of ANSI escapes: rustyline's Windows
    // console backend computes cursor columns by counting grapheme widths of
    // `raw` (it cannot interpret escape sequences and debug-asserts against
    // them), so any styling left in `raw` is counted as visible width and
    // pushes the cursor past where the text actually is. The left prompt and
    // indicator are styled via `nu_ansi_term`, so strip those codes for `raw`.
    // The right prompt is positioned off to the side with cursor save/restore
    // and is not part of the input-line geometry, so it is excluded from `raw`
    // entirely.
    if right.trim().is_empty() {
        let styled = format!("{left}{indicator}");
        let raw = strip_ansi_codes(&styled).into_owned();
        return ResponsivePrompt { raw, styled };
    }

    if let Some((first_line, remaining)) = left.split_once('\n') {
        let right = render_right_prompt(right);
        let raw = strip_ansi_codes(&format!("{first_line}\n{remaining}{indicator}")).into_owned();
        return ResponsivePrompt {
            raw,
            styled: format!("{first_line}{right}\n{remaining}{indicator}"),
        };
    }

    let right = render_right_prompt(right);
    let raw = strip_ansi_codes(&format!("{left}{indicator}")).into_owned();
    ResponsivePrompt { raw, styled: format!("{left}{right}{indicator}") }
}

fn render_right_prompt(right: &str) -> String {
    let width = measure_text_width(strip_ansi_codes(right).as_ref());
    format!("\x1b[s\x1b[999C\x1b[{width}D{right}\x1b[K\x1b[u")
}

struct ResponsivePrompt {
    raw: String,
    styled: String,
}

impl RustylinePrompt for ResponsivePrompt {
    fn raw(&self) -> &str {
        &self.raw
    }

    fn styled(&self) -> &str {
        &self.styled
    }
}

/// Opens the slash command popup when `/` is typed at the start of the input.
#[derive(Clone)]
struct CommandMenuKeyHandler {
    commands: Arc<FurcodeCommandManager>,
}

impl ConditionalEventHandler for CommandMenuKeyHandler {
    fn handle(
        &self,
        evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        match evt {
            Event::KeySeq(keys)
                if keys.len() == 1
                    && matches!(keys[0], KeyEvent(KeyCode::Char('/'), Modifiers::NONE)) => {}
            _ => return None,
        }

        let line = ctx.line();
        let pos = ctx.pos();

        // Only auto-open at the end of the buffer when it is empty or already
        // just `/` (re-open after a previous Esc dismiss).
        if pos != line.len() {
            return None;
        }
        let trimmed = line.trim();
        if !(trimmed.is_empty() || trimmed == "/") {
            return None;
        }

        let initial_query = trimmed.trim_start_matches('/');
        match pick_command(&self.commands, initial_query) {
            Some(name) => Some(Cmd::Replace(
                Movement::WholeBuffer,
                Some(format!("{name} ")),
            )),
            None => {
                // Esc dismissed the popup — leave `/` on the line so the user
                // can type a path or refine with Tab later.
                if trimmed.is_empty() {
                    Some(Cmd::SelfInsert(1, '/'))
                } else {
                    Some(Cmd::Noop)
                }
            }
        }
    }
}

/// Intercepts paste chords / bracketed paste and inserts a clipboard image as
/// `@[path]` when present; returns `None` so rustyline falls through to normal
/// text paste.
///
/// When handling [`KeyCode::BracketedPasteStart`] with an image, sets
/// `drain_paste` so [`PasteDrainHandler`] swallows the paste body (the terminal
/// may still stream empty text or garbage for an image clipboard).
#[derive(Clone)]
struct ImagePasteHandler {
    drain_paste: Arc<AtomicBool>,
}

impl ConditionalEventHandler for ImagePasteHandler {
    fn handle(
        &self,
        evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        let is_bracketed_start = matches!(
            evt,
            Event::KeySeq(keys)
                if keys.len() == 1
                    && matches!(
                        keys[0],
                        KeyEvent(KeyCode::BracketedPasteStart, Modifiers::NONE)
                    )
        );
        let is_key_paste = matches!(
            evt,
            Event::KeySeq(keys) if keys.len() == 1 && matches!(
                keys[0],
                KeyEvent(KeyCode::Char('v' | 'V'), Modifiers::CTRL)
                    | KeyEvent(KeyCode::Char('v' | 'V'), Modifiers::ALT)
            )
        );
        if !is_bracketed_start && !is_key_paste {
            return None;
        }

        match try_paste_clipboard_image() {
            Ok(Some(atom)) => {
                // Bracketed paste leaves its body in the TTY stream when we do
                // not call rustyline's read_pasted_text — mark drain so
                // Event::Any swallows until BracketedPasteEnd.
                if is_bracketed_start {
                    self.drain_paste.store(true, Ordering::SeqCst);
                }
                // Short atom `[Image N] ` — expanded to `@[path]` on submit.
                return Some(Cmd::Insert(1, format!("{atom} ")));
            }
            Ok(None) => {
                // No image (or clipboard prefers text). For BracketedPasteStart
                // return None so rustyline's default paste reader inserts text.
            }
            Err(err) => eprintln!("paste image failed: {err:#}"),
        }

        None
    }
}

/// Backspace deletes a whole `[Image N]` atom (plus trailing space) in one go.
#[derive(Clone)]
struct ImageAtomBackspaceHandler;

impl ConditionalEventHandler for ImageAtomBackspaceHandler {
    fn handle(
        &self,
        evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if !matches!(
            evt,
            Event::KeySeq(keys)
                if keys.len() == 1
                    && matches!(keys[0], KeyEvent(KeyCode::Backspace, Modifiers::NONE))
        ) {
            return None;
        }
        let line = ctx.line();
        let pos = ctx.pos();
        let (start, end) = image_atom_for_backspace(line, pos)?;
        Some(kill_byte_range(line, pos, start, end))
    }
}

/// Delete (forward) removes a whole `[Image N]` atom when the cursor is on it.
#[derive(Clone)]
struct ImageAtomDeleteHandler;

impl ConditionalEventHandler for ImageAtomDeleteHandler {
    fn handle(
        &self,
        evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if !matches!(
            evt,
            Event::KeySeq(keys)
                if keys.len() == 1
                    && matches!(keys[0], KeyEvent(KeyCode::Delete, Modifiers::NONE))
        ) {
            return None;
        }
        let line = ctx.line();
        let pos = ctx.pos();
        let (start, end) = image_atom_for_delete(line, pos)?;
        Some(kill_byte_range(line, pos, start, end))
    }
}

/// Swallows key events while an image paste body is being drained after
/// [`ImagePasteHandler`] handled `BracketedPasteStart`.
#[derive(Clone)]
struct PasteDrainHandler {
    drain_paste: Arc<AtomicBool>,
}

impl ConditionalEventHandler for PasteDrainHandler {
    fn handle(
        &self,
        evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if !self.drain_paste.load(Ordering::SeqCst) {
            return None;
        }
        // Stop draining at paste end (or if something else clears the stream).
        if matches!(
            evt,
            Event::KeySeq(keys)
                if keys.len() == 1
                    && matches!(
                        keys[0],
                        KeyEvent(KeyCode::BracketedPasteEnd, Modifiers::NONE)
                    )
        ) {
            self.drain_paste.store(false, Ordering::SeqCst);
        }
        Some(Cmd::Noop)
    }
}

/// Detects double-Esc on an empty line to open the message-fork picker.
#[derive(Clone)]
struct DoubleEscHandler {
    last_esc: Arc<Mutex<Option<Instant>>>,
    fork_requested: Arc<AtomicBool>,
}

/// Returns true when `evt` is a lone Esc key (no modifiers / multi-key seq).
fn is_lone_esc_event(evt: &Event) -> bool {
    match evt {
        Event::KeySeq(keys) if keys.len() == 1 => {
            matches!(keys[0], KeyEvent(KeyCode::Esc, Modifiers::NONE))
                || keys[0] == KeyEvent::from('\x1b')
        }
        _ => false,
    }
}

/// Decides whether this Esc is the second of a double-Esc within `window`.
///
/// Updates `last_esc` in place. Returns `true` when the caller should open the
/// fork picker.
fn register_esc_tap(
    last_esc: &mut Option<Instant>,
    now: Instant,
    window: Duration,
) -> bool {
    let is_double = last_esc
        .map(|t| now.duration_since(t) <= window)
        .unwrap_or(false);
    if is_double {
        *last_esc = None;
        true
    } else {
        *last_esc = Some(now);
        false
    }
}

impl ConditionalEventHandler for DoubleEscHandler {
    fn handle(
        &self,
        evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if !is_lone_esc_event(evt) {
            return None;
        }

        // Only on an empty line at the prompt (not mid-edit).
        // Cursor may be at end of empty/whitespace-only buffer.
        if !ctx.line().trim().is_empty() {
            // Non-empty: clear armed double-Esc and let default Esc (Abort/no-op).
            if let Ok(mut last) = self.last_esc.lock() {
                *last = None;
            }
            return None;
        }

        let now = Instant::now();
        let mut last = self.last_esc.lock().expect("esc mutex poisoned");
        if register_esc_tap(&mut last, now, DOUBLE_ESC_WINDOW) {
            self.fork_requested.store(true, Ordering::SeqCst);
            // Interrupt readline so the UI can open the history picker.
            Some(Cmd::Interrupt)
        } else {
            // Swallow the first Esc so it does not fall through to Abort.
            Some(Cmd::Noop)
        }
    }
}

struct FurcodeHelper {
    completer: Mutex<InputCompleter>,
    highlighter: FurcodeHighlighter,
    hinter: HistoryHinter,
}

impl FurcodeHelper {
    fn new(cwd: PathBuf, command_manager: Arc<FurcodeCommandManager>) -> Self {
        Self {
            completer: Mutex::new(InputCompleter::new(cwd, command_manager)),
            highlighter: FurcodeHighlighter,
            hinter: HistoryHinter {},
        }
    }
}

impl Helper for FurcodeHelper {}

impl Completer for FurcodeHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &RustylineContext<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let mut completer = self
            .completer
            .lock()
            .expect("input completer mutex poisoned");
        let suggestions = completer.complete(line, pos);
        let start = suggestions
            .iter()
            .map(|suggestion| suggestion.span.start)
            .min()
            .unwrap_or(pos);
        let pairs = suggestions
            .into_iter()
            .map(|suggestion| {
                let replacement = if suggestion.append_whitespace {
                    format!("{} ", suggestion.value)
                } else {
                    suggestion.value
                };
                Pair { display: replacement.clone(), replacement }
            })
            .collect();
        Ok((start, pairs))
    }
}

impl Hinter for FurcodeHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, ctx: &RustylineContext<'_>) -> Option<Self::Hint> {
        self.hinter.hint(line, pos, ctx)
    }
}

impl Highlighter for FurcodeHelper {
    fn highlight<'l>(&self, line: &'l str, pos: usize) -> Cow<'l, str> {
        let styled = self.highlighter.highlight(line, pos);
        if styled.buffer.is_empty() {
            return Cow::Borrowed(line);
        }

        let default_style = Style::new();
        let mut rendered = String::with_capacity(line.len());
        for (style, text) in styled.buffer {
            if style == default_style {
                rendered.push_str(&text);
            } else {
                rendered.push_str(&style.paint(text).to_string());
            }
        }
        Cow::Owned(rendered)
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(Style::new().dimmed().paint(hint).to_string())
    }
}

impl Validator for FurcodeHelper {}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_register_esc_tap_double_within_window() {
        let mut last = None;
        let t0 = Instant::now();
        let actual_first = register_esc_tap(&mut last, t0, DOUBLE_ESC_WINDOW);
        assert_eq!(actual_first, false);
        assert!(last.is_some());

        let actual_second =
            register_esc_tap(&mut last, t0 + Duration::from_millis(100), DOUBLE_ESC_WINDOW);
        assert_eq!(actual_second, true);
        assert_eq!(last, None);
    }

    #[test]
    fn test_register_esc_tap_expires_outside_window() {
        let mut last = None;
        let t0 = Instant::now();
        assert_eq!(register_esc_tap(&mut last, t0, DOUBLE_ESC_WINDOW), false);

        let actual = register_esc_tap(
            &mut last,
            t0 + DOUBLE_ESC_WINDOW + Duration::from_millis(1),
            DOUBLE_ESC_WINDOW,
        );
        assert_eq!(actual, false);
        assert!(last.is_some());
    }

    #[test]
    fn test_is_lone_esc_event() {
        assert!(is_lone_esc_event(&Event::from(KeyEvent(
            KeyCode::Esc,
            Modifiers::NONE
        ))));
        assert!(!is_lone_esc_event(&Event::from(KeyEvent(
            KeyCode::Enter,
            Modifiers::NONE
        ))));
        assert!(!is_lone_esc_event(&Event::KeySeq(vec![
            KeyEvent(KeyCode::Esc, Modifiers::NONE),
            KeyEvent(KeyCode::Esc, Modifiers::NONE),
        ])));
    }

    #[test]
    fn test_normalize_result_wraps_existing_pasted_path() {
        let fixture = "/usr/bin/env".to_string();

        let actual = normalize_result_text(fixture);

        let expected = ReadResult::Success("@[/usr/bin/env]".to_string());
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_render_prompt_raw_has_no_ansi_escapes() {
        use std::path::PathBuf;

        use furcode_api::{AgentId, ModelId};

        // rustyline measures `raw()` to position the cursor and, on Windows,
        // cannot interpret ANSI escapes (it counts their bytes as visible
        // columns). `raw()` must therefore be free of escape sequences even
        // though the visible prompt is styled.
        let mut prompt = FurcodePrompt::new(PathBuf::from("project"), AgentId::default());
        prompt.model(ModelId::new("anthropic/claude-opus-4"));

        let rendered = render_prompt(&prompt);

        assert!(
            !rendered.raw.contains('\u{1b}'),
            "raw prompt must not contain ANSI escape sequences: {:?}",
            rendered.raw
        );
        // The styled prompt, by contrast, does carry styling for display.
        assert!(rendered.styled.contains('\u{1b}'));
    }
}
