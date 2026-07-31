//! Pure view model for the interactive TUI.
//!
//! No terminal I/O. Domain chat events map into this structure; the renderer
//! only reads it. That split is what removes multi-writer cursor races.

use std::time::{Duration, Instant};

/// One immutable transcript row (user, assistant, tool summary, thought, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub kind: TranscriptKind,
    pub text: String,
}

/// Semantic kind for styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptKind {
    User,
    /// Dim clock under a user prompt (`14:32`).
    UserTime,
    /// Blank separator line (after user header block).
    Spacer,
    Assistant,
    Thought,
    ToolDone,
    ToolError,
    /// Multi-line tool body (e.g. todo checklist) — no ● prefix.
    ToolBody,
    Info,
    /// @deprecated mid-stream queue rows; queued prompts live in [`TurnView::pending`].
    Queued,
}

/// A message typed while the agent is busy — sticky until the turn ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPrompt {
    pub text: String,
    /// Local clock when Enter was pressed (`HH:MM`).
    pub time: String,
}

/// Live activity under the transcript (redrawn every tick; never appended).
#[derive(Debug, Clone, PartialEq)]
pub enum ActivityPhase {
    Idle,
    Thinking {
        started: Instant,
        /// Raw reasoning buffer; UI shows last N preview lines.
        buffer: String,
    },
    Tool {
        started: Instant,
        label: String,
        output: String,
        is_error: bool,
    },
}

impl ActivityPhase {
    pub fn is_busy(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    pub fn elapsed(&self) -> Duration {
        match self {
            Self::Idle => Duration::ZERO,
            Self::Thinking { started, .. } | Self::Tool { started, .. } => started.elapsed(),
        }
    }
}

/// Bottom composer (`› …`). Always owned by the TUI surface — never by live_feed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposerState {
    pub buffer: String,
    pub cursor: usize,
}

impl ComposerState {
    pub fn insert_char(&mut self, c: char) {
        let idx = self.cursor.min(self.buffer.len());
        self.buffer.insert(idx, c);
        self.cursor = idx + c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.buffer[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.buffer.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    /// Delete the character under the caret (forward delete).
    pub fn delete_forward(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let next = self.buffer[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
            .unwrap_or(self.cursor);
        self.buffer.replace_range(self.cursor..next, "");
    }

    /// Move caret one grapheme/char left.
    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.buffer[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }

    /// Move caret one grapheme/char right.
    pub fn move_right(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        self.cursor = self.buffer[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
            .unwrap_or(self.buffer.len());
    }

    /// Move caret to the start of the buffer.
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Move caret to the end of the buffer.
    pub fn move_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    /// Jump left to the start of the previous word (emacs/bash style).
    pub fn move_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let bytes = self.buffer.as_bytes();
        let mut i = self.cursor;
        // Skip whitespace left of cursor.
        while i > 0 && bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        // Skip non-whitespace word body.
        while i > 0 && !bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        self.cursor = i;
    }

    /// Jump right to the end of the next word.
    pub fn move_word_right(&mut self) {
        let len = self.buffer.len();
        if self.cursor >= len {
            return;
        }
        let bytes = self.buffer.as_bytes();
        let mut i = self.cursor;
        // Skip non-whitespace word body.
        while i < len && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        // Skip trailing whitespace.
        while i < len && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        self.cursor = i;
    }

    /// Delete backward to the start of the previous word (Ctrl+W / Alt+Backspace).
    pub fn delete_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.cursor;
        self.move_word_left();
        let start = self.cursor;
        self.buffer.replace_range(start..end, "");
    }

    /// Delete forward to the end of the next word (Alt+D).
    pub fn delete_word_forward(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let start = self.cursor;
        self.move_word_right();
        let end = self.cursor;
        self.buffer.replace_range(start..end, "");
        self.cursor = start;
    }

    /// Insert arbitrary text at the caret (paste / bracketed paste).
    ///
    /// Newlines are normalised to spaces so the single-line composer stays flat.
    pub fn insert_str(&mut self, text: &str) {
        let cleaned = text.replace("\r\n", "\n").replace('\r', "\n");
        let flat: String = cleaned
            .chars()
            .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
            .collect();
        if flat.is_empty() {
            return;
        }
        let idx = self.cursor.min(self.buffer.len());
        self.buffer.insert_str(idx, &flat);
        self.cursor = idx + flat.len();
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
    }

    pub fn take(&mut self) -> String {
        let s = std::mem::take(&mut self.buffer);
        self.cursor = 0;
        s
    }
}

/// Clickable hit region in terminal cell coordinates (inclusive origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HitRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl HitRect {
    pub fn contains(self, col: u16, row: u16) -> bool {
        col >= self.x
            && row >= self.y
            && col < self.x.saturating_add(self.width)
            && row < self.y.saturating_add(self.height)
    }
}

/// Terminal cell (column, row) for app-driven text selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellPos {
    pub col: u16,
    pub row: u16,
}

/// Drag selection while mouse capture is on (wheel scroll still works).
///
/// Native terminal select cannot run alongside mouse capture; we paint the
/// highlight ourselves and copy plain text to the system clipboard on mouse-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSelection {
    pub anchor: CellPos,
    pub focus: CellPos,
    pub dragging: bool,
}

impl TextSelection {
    /// Ordered (start, end) by row then column.
    pub fn normalized(&self) -> (CellPos, CellPos) {
        let a = self.anchor;
        let b = self.focus;
        if (a.row, a.col) <= (b.row, b.col) {
            (a, b)
        } else {
            (b, a)
        }
    }

    /// Whether cell `(col, row)` is inside the selection (inclusive).
    pub fn contains_cell(&self, col: u16, row: u16) -> bool {
        let (a, b) = self.normalized();
        if row < a.row || row > b.row {
            return false;
        }
        if a.row == b.row {
            return col >= a.col && col <= b.col;
        }
        if row == a.row {
            return col >= a.col;
        }
        if row == b.row {
            return col <= b.col;
        }
        true
    }
}

/// Last painted selectable surface (transcript body or full-screen overlay).
#[derive(Debug, Clone, Default)]
pub struct SelectablePaint {
    pub area: HitRect,
    /// One plain-text string per screen row inside `area` (no ANSI).
    pub rows: Vec<String>,
}

/// Full interactive session view (one paint frame).
#[derive(Debug, Clone)]
pub struct TurnView {
    pub header_left: String,
    pub header_right: String,
    pub transcript: Vec<TranscriptLine>,
    pub activity: ActivityPhase,
    pub composer: ComposerState,
    /// Agent turn in flight (composer Enter queues instead of sending immediately).
    pub busy: bool,
    /// Prompts typed while busy. Rendered sticky at the bottom of the transcript
    /// area (green + timestamp) until the current turn ends, then committed as
    /// real user rows and sent.
    pub pending: Vec<PendingPrompt>,
    /// User pressed Esc while busy.
    pub interrupt_requested: bool,
    /// Accumulated thinking time for the current activity block.
    pub thought_acc: Duration,
    /// When true, viewport sticks to the latest content (auto-follow while
    /// Working). User PageUp clears this; End restores it.
    pub follow_bottom: bool,
    /// Exclusive end line of the viewport when `!follow_bottom` (absolute
    /// content-line index). Unaffected by growth at the bottom — that is what
    /// makes scroll stable during live tool spam.
    pub viewport_end: usize,
    /// Derived: lines from bottom when !follow_bottom (`0` when following).
    /// Kept for jump-bar / load-more heuristics; prefer `follow_bottom`.
    pub scroll_offset: usize,
    /// Last rendered content line count (for clamping scroll).
    pub last_content_lines: usize,
    /// Last transcript viewport height in rows.
    pub last_viewport_rows: usize,
    /// Clickable "jump to latest" control (set while scrolled up).
    pub jump_bottom_hit: Option<HitRect>,
    /// Bumped on every transcript mutation so the renderer can cache lines.
    pub transcript_gen: u64,
    /// Transient status above the composer (loading / rendering markdown).
    pub status_line: Option<String>,
    /// Slim archive of older turns (load-more + Ctrl+T overlay).
    pub history_archive: Option<super::history::HistoryArchive>,
    /// Full-screen transcript overlay (Codex-style Ctrl+T).
    pub overlay: Option<TranscriptOverlay>,
    /// Double-Esc: pick a prior user message to fork/undo to.
    pub fork_picker: Option<ForkPicker>,
    /// Host should load more archive turns (scroll near top).
    pub load_more_requested: bool,
    /// Active drag / completed text selection (app-side).
    pub selection: Option<TextSelection>,
    /// Plain text of the last painted selectable region (for copy).
    pub selectable: SelectablePaint,
}

/// Ctrl+T overlay: browse the slim archive without bloating the main view.
#[derive(Debug, Clone, Default)]
pub struct TranscriptOverlay {
    pub lines: Vec<TranscriptLine>,
    pub scroll_offset: usize,
}

/// One selectable user turn for double-Esc fork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkItem {
    /// Index into conversation `context.messages` of this user message.
    pub message_index: usize,
    /// One-line label in the picker.
    pub label: String,
    /// Text to pre-fill into the composer after fork.
    pub edit_text: String,
}

/// Full-screen picker of prior user messages (newest first).
#[derive(Debug, Clone, Default)]
pub struct ForkPicker {
    pub items: Vec<ForkItem>,
    pub selected: usize,
}

impl Default for TurnView {
    fn default() -> Self {
        Self {
            header_left: " 🄵 ".into(),
            header_right: String::new(),
            transcript: Vec::new(),
            activity: ActivityPhase::Idle,
            composer: ComposerState::default(),
            busy: false,
            pending: Vec::new(),
            interrupt_requested: false,
            thought_acc: Duration::ZERO,
            follow_bottom: true,
            viewport_end: 0,
            scroll_offset: 0,
            last_content_lines: 0,
            last_viewport_rows: 0,
            jump_bottom_hit: None,
            transcript_gen: 0,
            status_line: None,
            history_archive: None,
            overlay: None,
            fork_picker: None,
            load_more_requested: false,
            selection: None,
            selectable: SelectablePaint::default(),
        }
    }
}

impl TurnView {
    /// Begin a drag selection at a terminal cell.
    pub fn selection_begin(&mut self, col: u16, row: u16) {
        let pos = CellPos { col, row };
        self.selection = Some(TextSelection {
            anchor: pos,
            focus: pos,
            dragging: true,
        });
        self.touch_transcript();
    }

    /// Update the free end of the selection while dragging.
    pub fn selection_update(&mut self, col: u16, row: u16) {
        if let Some(sel) = self.selection.as_mut() {
            if sel.dragging {
                sel.focus = CellPos { col, row };
                self.touch_transcript();
            }
        }
    }

    /// Finish drag; returns plain text to copy (if any).
    pub fn selection_end(&mut self) -> Option<String> {
        let Some(sel) = self.selection.as_mut() else {
            return None;
        };
        sel.dragging = false;
        let text = self.selected_plain_text();
        if text.trim().is_empty() {
            self.selection = None;
            self.touch_transcript();
            None
        } else {
            self.touch_transcript();
            Some(text)
        }
    }

    pub fn clear_selection(&mut self) {
        if self.selection.take().is_some() {
            self.touch_transcript();
        }
    }

    /// Extract selected plain text from the last painted selectable rows.
    pub fn selected_plain_text(&self) -> String {
        let Some(sel) = self.selection.as_ref() else {
            return String::new();
        };
        let paint = &self.selectable;
        if paint.rows.is_empty() || paint.area.width == 0 {
            return String::new();
        }
        let (a, b) = sel.normalized();
        let mut out = String::new();
        for (i, row_text) in paint.rows.iter().enumerate() {
            let y = paint.area.y.saturating_add(i as u16);
            if y < a.row || y > b.row {
                continue;
            }
            let chars: Vec<char> = row_text.chars().collect();
            let len = chars.len();
            // Columns relative to the paint area origin.
            let rel = |abs_col: u16| -> usize {
                abs_col
                    .saturating_sub(paint.area.x)
                    .min(len.saturating_sub(0) as u16) as usize
            };
            let (start_c, end_c) = if a.row == b.row && y == a.row {
                let s = rel(a.col);
                let e = rel(b.col).min(len.saturating_sub(1));
                (s, e.saturating_add(1).min(len))
            } else if y == a.row {
                (rel(a.col), len)
            } else if y == b.row {
                (0, rel(b.col).saturating_add(1).min(len))
            } else {
                (0, len)
            };
            if start_c < end_c && start_c < len {
                let slice: String = chars[start_c..end_c.min(len)].iter().collect();
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(slice.trim_end());
            } else if a.row != b.row && y > a.row && y < b.row {
                // empty line in multi-line selection
                if !out.is_empty() {
                    out.push('\n');
                }
            }
        }
        out
    }
}

impl TurnView {
    /// Show a status line above the composer (spinner drawn by the renderer).
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_line = Some(msg.into());
        // Bump so the draw loop repaints (it only animates while status is set;
        // without a gen bump, clearing status would leave the last frame stuck).
        self.touch_transcript();
    }

    /// Clear the transient status line.
    pub fn clear_status(&mut self) {
        if self.status_line.is_none() {
            return;
        }
        self.status_line = None;
        self.touch_transcript();
    }

    /// Invalidate the renderer's line cache / signal the draw loop to repaint.
    ///
    /// Bumped on transcript edits **and** other visual state (status, idle).
    /// The draw loop compares this to its last-drawn gen so host-side updates
    /// (resume load, stream end) are painted without waiting for a keypress.
    pub fn touch_transcript(&mut self) {
        self.transcript_gen = self.transcript_gen.wrapping_add(1);
    }

    /// Append a transcript row and invalidate the render cache.
    pub fn push_transcript(&mut self, line: TranscriptLine) {
        self.transcript.push(line);
        self.touch_transcript();
    }

    /// Extend the transcript and invalidate the render cache.
    pub fn extend_transcript(&mut self, lines: impl IntoIterator<Item = TranscriptLine>) {
        self.transcript.extend(lines);
        self.touch_transcript();
    }

    /// Prepend lines (load-more older turns), preserving relative scroll.
    pub fn prepend_transcript(&mut self, lines: impl IntoIterator<Item = TranscriptLine>) {
        let extra: Vec<_> = lines.into_iter().collect();
        if extra.is_empty() {
            return;
        }
        let n = extra.len();
        let mut merged = extra;
        merged.append(&mut self.transcript);
        self.transcript = merged;
        // Absolute end shifts down by prepended row count (approx. screen lines).
        if !self.follow_bottom {
            self.viewport_end = self.viewport_end.saturating_add(n);
        }
        self.sync_scroll_offset();
        self.touch_transcript();
    }

    /// Apply a load-more result: replace leading summary stub with new stub + turns.
    pub fn apply_load_more(&mut self, new_head: Vec<TranscriptLine>) {
        if new_head.is_empty() {
            return;
        }
        // Drop existing leading Info stub + spacer if present.
        let mut rest = self.transcript.clone();
        if rest
            .first()
            .is_some_and(|l| l.kind == TranscriptKind::Info && l.text.contains("earlier turn"))
        {
            rest.remove(0);
            if rest
                .first()
                .is_some_and(|l| l.kind == TranscriptKind::Spacer)
            {
                rest.remove(0);
            }
        }
        let added = new_head.len();
        let mut merged = new_head;
        merged.append(&mut rest);
        self.transcript = merged;
        if !self.follow_bottom {
            self.viewport_end = self.viewport_end.saturating_add(added);
        }
        self.sync_scroll_offset();
        self.load_more_requested = false;
        self.touch_transcript();
    }

    pub fn open_overlay(&mut self) {
        let lines = self
            .history_archive
            .as_ref()
            .map(|a| a.overlay_lines())
            .unwrap_or_else(|| {
                let mut v = vec![TranscriptLine {
                    kind: TranscriptKind::Info,
                    text: "Transcript history · Esc close".into(),
                }];
                v.extend(self.transcript.iter().cloned());
                v
            });
        self.overlay = Some(TranscriptOverlay {
            lines,
            scroll_offset: 0,
        });
        self.touch_transcript();
    }

    pub fn close_overlay(&mut self) {
        if self.overlay.take().is_some() {
            self.touch_transcript();
        }
    }

    pub fn toggle_overlay(&mut self) {
        if self.overlay.is_some() {
            self.close_overlay();
        } else {
            self.open_overlay();
        }
    }

    /// Request host to reveal older archive turns when user scrolls near top.
    pub fn maybe_request_load_more(&mut self) {
        if self.overlay.is_some() {
            return;
        }
        let Some(arch) = self.history_archive.as_ref() else {
            return;
        };
        if !arch.has_more() || self.load_more_requested {
            return;
        }
        // Near top of absolute viewport (not following bottom).
        if self.follow_bottom {
            return;
        }
        let vp = self.last_viewport_rows.max(1);
        let start = self.viewport_end.saturating_sub(vp);
        if start <= 3 {
            self.load_more_requested = true;
        }
    }

    /// Start a thinking phase (or no-op if already thinking).
    pub fn begin_thinking(&mut self) {
        if !matches!(self.activity, ActivityPhase::Thinking { .. }) {
            self.activity = ActivityPhase::Thinking {
                started: Instant::now(),
                buffer: String::new(),
            };
        }
        self.busy = true;
    }

    /// Append reasoning text to the live thinking buffer.
    pub fn push_reasoning(&mut self, chunk: &str) {
        self.begin_thinking();
        if let ActivityPhase::Thinking { buffer, .. } = &mut self.activity {
            buffer.push_str(chunk);
            if buffer.len() > 16_384 {
                let keep = buffer.len() - 8_192;
                *buffer = buffer[keep..].to_string();
            }
        }
    }

    /// End thinking and append/update a single merged "Thought for …" line.
    ///
    /// Only emits when the activity phase was actually `Thinking`. Calling this
    /// again while idle is a no-op — that prevents one Thought row per markdown
    /// token when the model streams assistant text after reasoning.
    pub fn flush_thought(&mut self) {
        let was_thinking = matches!(self.activity, ActivityPhase::Thinking { .. });
        if let ActivityPhase::Thinking { started, .. } =
            std::mem::replace(&mut self.activity, ActivityPhase::Idle)
        {
            self.thought_acc += started.elapsed();
        }
        if !was_thinking || self.thought_acc.is_zero() {
            return;
        }
        let secs = self.thought_acc.as_secs_f64().max(0.1);
        let text = if secs < 60.0 {
            format!("Thought for {secs:.1}s")
        } else {
            let m = (secs / 60.0).floor() as u64;
            let s = secs - m as f64 * 60.0;
            format!("Thought for {m}m{s:.0}s")
        };
        // Prefer updating the most recent thought in this activity block (may
        // not be last if we already started assistant text — still only emit once
        // per thinking→idle transition).
        if let Some(last) = self.transcript.iter_mut().rev().find(|l| l.kind == TranscriptKind::Thought)
        {
            last.text = text;
        } else {
            self.transcript.push(TranscriptLine {
                kind: TranscriptKind::Thought,
                text,
            });
        }
        self.touch_transcript();
    }

    /// Append streamed assistant markdown into one transcript row.
    pub fn append_assistant(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.flush_thought();
        self.activity = ActivityPhase::Idle;
        if let Some(last) = self.transcript.last_mut()
            && last.kind == TranscriptKind::Assistant
        {
            last.text.push_str(text);
        } else {
            self.transcript.push(TranscriptLine {
                kind: TranscriptKind::Assistant,
                text: text.to_string(),
            });
        }
        self.touch_transcript();
    }

    pub fn begin_tool(&mut self, label: impl Into<String>) {
        self.flush_thought();
        self.activity = ActivityPhase::Tool {
            started: Instant::now(),
            label: label.into(),
            output: String::new(),
            is_error: false,
        };
        self.busy = true;
    }

    pub fn push_tool_output(&mut self, chunk: &str) {
        if let ActivityPhase::Tool { output, .. } = &mut self.activity {
            output.push_str(chunk);
            if output.len() > 16_384 {
                let keep = output.len() - 8_192;
                *output = output[keep..].to_string();
            }
        }
    }

    pub fn finish_tool(&mut self, error: Option<String>) {
        let (label, is_error, output, started) =
            match std::mem::replace(&mut self.activity, ActivityPhase::Idle) {
                ActivityPhase::Tool {
                    label,
                    is_error,
                    output,
                    started,
                } => (label, is_error || error.is_some(), output, started),
                other => {
                    self.activity = other;
                    return;
                }
            };
        let elapsed = format_elapsed_suffix(started.elapsed());
        let text = if let Some(err) = error {
            format!("{label} failed — {err}{elapsed}")
        } else if is_error {
            format!("{label} failed{elapsed}")
        } else {
            format!("{label}{elapsed}")
        };

        // Surface tool bodies that matter for the user (todo lists, diffs, …).
        // Strip ANSI so ratatui does not show escape noise; keep the glyphs.
        let plain = strip_ansi_simple(&output);
        let body_lines: Vec<String> = plain
            .lines()
            .map(str::trim_end)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();

        // Consecutive same-family tools without body collapse: "Read a", "Read b"
        // → "Read 2 files". Bodies force a distinct row (can't hide the content).
        if body_lines.is_empty() {
            push_or_collapse_tool(&mut self.transcript, text, is_error);
        } else {
            self.transcript.push(TranscriptLine {
                kind: if is_error {
                    TranscriptKind::ToolError
                } else {
                    TranscriptKind::ToolDone
                },
                text,
            });
            for line in body_lines {
                self.transcript.push(TranscriptLine {
                    kind: TranscriptKind::ToolBody,
                    text: line,
                });
            }
        }
        self.touch_transcript();
    }

    /// Append a completed tool summary (used by history bootstrap).
    pub fn push_tool_summary(&mut self, text: impl Into<String>, is_error: bool) {
        push_or_collapse_tool(&mut self.transcript, text.into(), is_error);
        self.touch_transcript();
    }

    pub fn push_assistant(&mut self, text: impl Into<String>) {
        self.flush_thought();
        self.activity = ActivityPhase::Idle;
        self.push_transcript(TranscriptLine {
            kind: TranscriptKind::Assistant,
            text: text.into(),
        });
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        let stamp = chrono::Local::now().format("%H:%M").to_string();
        self.push_user_at(text, stamp);
    }

    /// User prompt with a pre-captured timestamp (queued-then-sent).
    pub fn push_user_at(&mut self, text: impl Into<String>, stamp: impl Into<String>) {
        self.transcript.push(TranscriptLine {
            kind: TranscriptKind::User,
            text: text.into(),
        });
        self.transcript.push(TranscriptLine {
            kind: TranscriptKind::UserTime,
            text: stamp.into(),
        });
        self.transcript.push(TranscriptLine {
            kind: TranscriptKind::Spacer,
            text: String::new(),
        });
        self.touch_transcript();
    }

    /// Whether the viewport is pinned to the latest content.
    pub fn is_at_bottom(&self) -> bool {
        self.follow_bottom
    }

    /// Jump to the latest transcript lines and re-enable auto-follow.
    pub fn scroll_to_bottom(&mut self) {
        self.follow_bottom = true;
        self.scroll_offset = 0;
        self.viewport_end = self.last_content_lines;
    }

    /// Ensure scroll metrics allow movement even before the next paint.
    ///
    /// While Working, paint may lag a stream tick; without a floor on total
    /// lines, `scroll_by` would see `max == 0` and no-op.
    fn ensure_scroll_metrics(&mut self) {
        if self.last_viewport_rows == 0 {
            self.last_viewport_rows = 20;
        }
        // Floor: each transcript row is at least one screen line. Prefer the
        // last painted total (includes soft-wrap) when it is larger.
        let floor = self.transcript.len().max(1);
        if self.last_content_lines < floor {
            self.last_content_lines = floor;
        }
        // Never treat as "nothing to scroll" when we clearly have history.
        if self.last_content_lines <= self.last_viewport_rows && self.transcript.len() > 3 {
            // Estimate: assume ~2 visual lines per row until paint corrects.
            self.last_content_lines = self.transcript.len().saturating_mul(2).max(floor);
        }
    }

    /// Public wrapper for the draw / key path (Ctrl+Home).
    pub fn ensure_scroll_metrics_public(&mut self) {
        self.ensure_scroll_metrics();
    }

    fn sync_scroll_offset(&mut self) {
        if self.follow_bottom {
            self.scroll_offset = 0;
        } else {
            self.scroll_offset = self
                .last_content_lines
                .saturating_sub(self.viewport_end);
        }
    }

    /// Public wrapper for key handlers that mutate `viewport_end` directly.
    pub fn sync_scroll_offset_public(&mut self) {
        self.sync_scroll_offset();
    }

    /// Scroll up (positive = older) or down (negative = newer) by `delta` lines.
    ///
    /// Uses an **absolute** `viewport_end` so live appends at the bottom never
    /// yank a scrolled-up view back to the live edge.
    pub fn scroll_by(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        self.ensure_scroll_metrics();
        let total = self.last_content_lines;
        let vp = self.last_viewport_rows.max(1);
        if total <= vp {
            // Nothing to scroll — still leave follow so End is a no-op.
            return;
        }

        if self.follow_bottom {
            if delta <= 0 {
                return; // already on latest
            }
            // Detach from live edge and start from the current bottom.
            self.follow_bottom = false;
            self.viewport_end = total;
        }

        if delta > 0 {
            self.viewport_end = self.viewport_end.saturating_sub(delta as usize);
            let min_end = vp.min(total);
            if self.viewport_end < min_end {
                self.viewport_end = min_end;
            }
        } else {
            let add = (-delta) as usize;
            self.viewport_end = self.viewport_end.saturating_add(add).min(total);
            if self.viewport_end >= total {
                self.follow_bottom = true;
            }
        }
        self.sync_scroll_offset();
    }

    /// Page Up / Page Down by roughly one viewport.
    pub fn scroll_page(&mut self, up: bool) {
        self.ensure_scroll_metrics();
        let step = self.last_viewport_rows.max(3) as i32;
        self.scroll_by(if up { step } else { -step });
    }

    fn max_scroll(&self) -> usize {
        self.last_content_lines
            .saturating_sub(self.last_viewport_rows.max(1))
    }

    /// Clamp absolute viewport after content/viewport size changes (draw path).
    ///
    /// Never forces follow-mode just because height estimates are briefly low
    /// (that made PageUp a no-op while Working). Only clamps `viewport_end`.
    pub fn clamp_scroll(&mut self) {
        let total = self.last_content_lines;
        let vp = self.last_viewport_rows.max(1);
        if self.follow_bottom {
            self.viewport_end = total;
            self.scroll_offset = 0;
            return;
        }
        if total == 0 {
            self.viewport_end = 0;
            self.scroll_offset = 0;
            return;
        }
        let min_end = vp.min(total).max(1);
        if self.viewport_end < min_end {
            self.viewport_end = min_end;
        }
        if self.viewport_end > total {
            self.viewport_end = total;
        }
        // Re-attach only when the user scrolls all the way to the live edge.
        if self.viewport_end >= total {
            self.follow_bottom = true;
            self.scroll_offset = 0;
        } else {
            self.sync_scroll_offset();
        }
    }

    /// Visible `[start, end)` content lines for the current scroll state.
    pub fn visible_range(&self, total: usize, content_rows: usize) -> (usize, usize) {
        if total == 0 {
            return (0, 0);
        }
        let content_rows = content_rows.max(1).min(total);
        let end = if self.follow_bottom {
            total
        } else {
            self.viewport_end.min(total).max(content_rows.min(total))
        };
        let start = end.saturating_sub(content_rows);
        (start, end)
    }

    /// Enter while idle: return message to send. While busy: queue it sticky.
    pub fn submit_composer(&mut self) -> Option<String> {
        let text = self.composer.take();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        let owned = trimmed.to_string();
        if self.busy {
            // Sticky pending panel — do NOT insert into the live tool transcript.
            self.pending.push(PendingPrompt {
                text: owned,
                time: chrono::Local::now().format("%H:%M").to_string(),
            });
            self.touch_transcript(); // repaint sticky strip
            None
        } else {
            self.push_user(owned.clone());
            Some(owned)
        }
    }

    /// Move sticky pending prompts into the transcript as real user rows.
    ///
    /// Called when the in-flight turn ends so they land **after** tools/assistant
    /// output, then the host sends them in order. Returns the prompt texts.
    pub fn commit_pending_to_transcript(&mut self) -> Vec<String> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(self.pending.len());
        let drained: Vec<PendingPrompt> = self.pending.drain(..).collect();
        for p in drained {
            self.push_user_at(p.text.clone(), p.time);
            out.push(p.text);
        }
        // Show the newly committed user prompts before the next turn starts.
        self.scroll_to_bottom();
        out
    }

    pub fn request_interrupt(&mut self) {
        if self.busy {
            self.interrupt_requested = true;
            self.push_transcript(TranscriptLine {
                kind: TranscriptKind::Info,
                text: "Interrupt requested".into(),
            });
        }
    }

    pub fn mark_idle(&mut self) {
        self.flush_thought();
        self.activity = ActivityPhase::Idle;
        self.busy = false;
        self.interrupt_requested = false;
        self.thought_acc = Duration::ZERO;
        self.touch_transcript();
    }
}

/// Suffix like ` · 1.2s` or ` · 1m5s` for finished tool / thought lines.
fn format_elapsed_suffix(d: Duration) -> String {
    let secs = d.as_secs_f64().max(0.1);
    if secs < 60.0 {
        format!(" · {secs:.1}s")
    } else {
        let m = (secs / 60.0).floor() as u64;
        let s = secs - m as f64 * 60.0;
        format!(" · {m}m{s:.0}s")
    }
}

/// Strip common ANSI CSI / OSC sequences for plain transcript display.
fn strip_ansi_simple(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for ch in chars.by_ref() {
                        if ch.is_ascii_alphabetic() || ch == '@' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC … BEL or ST
                    while let Some(ch) = chars.next() {
                        if ch == '\u{7}' {
                            break;
                        }
                        if ch == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ─── Tool row collapsing (Read a + Read b → "Read 2 files") ─────────────────

/// Family key for consecutive-tool merge. `None` = never collapse.
fn tool_family(label: &str) -> Option<&'static str> {
    // Drop elapsed / failure suffix for classification.
    let head = label
        .split(" · ")
        .next()
        .unwrap_or(label)
        .split(" failed")
        .next()
        .unwrap_or(label)
        .trim();
    if head.starts_with("Update Todos") || head.starts_with("Update todos") {
        return Some("UpdateTodos");
    }
    if head.starts_with("Read Todos") || head.starts_with("Read todos") {
        return Some("ReadTodos");
    }
    if head.starts_with("Codebase Search") || head.starts_with("Codebase search") {
        return Some("CodebaseSearch");
    }
    if head.starts_with("Search for ") || head.starts_with("Search ") {
        return Some("Search");
    }
    match head.split_whitespace().next()? {
        "Read" => Some("Read"),
        "Write" | "Overwrite" => Some("Write"),
        "Shell" => Some("Shell"),
        "Replace" | "Patch" => Some("Replace"),
        "Remove" => Some("Remove"),
        "Undo" => Some("Undo"),
        "Fetch" => Some("Fetch"),
        // Unknown verbs keep their own row (never silently merge).
        _ => None,
    }
}

/// How many tools a summary line already represents.
fn tool_group_count(family: &str, text: &str) -> usize {
    let head = text
        .split(" · ")
        .next()
        .unwrap_or(text)
        .split(" failed")
        .next()
        .unwrap_or(text)
        .trim();

    let parse_n = |s: &str, singular: &str, plural: &str| -> Option<usize> {
        let s = s.trim();
        if let Some(rest) = s.strip_suffix(plural).or_else(|| s.strip_suffix(singular)) {
            rest.trim().parse().ok()
        } else {
            None
        }
    };

    match family {
        "Read" => head
            .strip_prefix("Read ")
            .and_then(|r| parse_n(r, "file", "files"))
            .unwrap_or(1),
        "Write" => head
            .strip_prefix("Write ")
            .and_then(|r| parse_n(r, "file", "files"))
            .unwrap_or(1),
        "Search" => {
            // "Search 3 times" or "Search for '…'"
            if let Some(rest) = head.strip_prefix("Search ") {
                if let Some(n) = parse_n(rest, "time", "times") {
                    return n;
                }
            }
            1
        }
        "Shell" => head
            .strip_prefix("Shell ")
            .and_then(|r| parse_n(r, "command", "commands"))
            .unwrap_or(1),
        "Replace" => head
            .strip_prefix("Replace ")
            .and_then(|r| parse_n(r, "file", "files"))
            .unwrap_or(1),
        "Remove" => head
            .strip_prefix("Remove ")
            .and_then(|r| parse_n(r, "file", "files"))
            .unwrap_or(1),
        "Undo" => head
            .strip_prefix("Undo ")
            .and_then(|r| parse_n(r, "file", "files"))
            .unwrap_or(1),
        "Fetch" => head
            .strip_prefix("Fetch ")
            .and_then(|r| parse_n(r, "request", "requests"))
            .unwrap_or(1),
        "UpdateTodos" => {
            // "Update Todos 3 times" or "Update Todos 4 item(s)"
            if let Some(rest) = head
                .strip_prefix("Update Todos ")
                .or_else(|| head.strip_prefix("Update todos "))
            {
                if let Some(n) = parse_n(rest, "time", "times") {
                    return n;
                }
            }
            1
        }
        "ReadTodos" | "CodebaseSearch" => 1,
        _ => 1,
    }
}

fn collapsed_tool_label(family: &str, n: usize) -> String {
    let files = |n: usize| {
        if n == 1 {
            "file"
        } else {
            "files"
        }
    };
    let times = |n: usize| {
        if n == 1 {
            "time"
        } else {
            "times"
        }
    };
    match family {
        "Read" => format!("Read {n} {}", files(n)),
        "Write" => format!("Write {n} {}", files(n)),
        "Search" => format!("Search {n} {}", times(n)),
        "Shell" => {
            if n == 1 {
                "Shell 1 command".into()
            } else {
                format!("Shell {n} commands")
            }
        }
        "Replace" => format!("Replace {n} {}", files(n)),
        "Remove" => format!("Remove {n} {}", files(n)),
        "Undo" => format!("Undo {n} {}", files(n)),
        "Fetch" => {
            if n == 1 {
                "Fetch 1 request".into()
            } else {
                format!("Fetch {n} requests")
            }
        }
        "UpdateTodos" => format!("Update Todos {n} {}", times(n)),
        "ReadTodos" => {
            if n == 1 {
                "Read Todos".into()
            } else {
                format!("Read Todos {n} {}", times(n))
            }
        }
        "CodebaseSearch" => format!("Codebase Search {n} {}", times(n)),
        _ => format!("Tool {n} {}", times(n)),
    }
}

/// Push a tool summary, merging into the previous row when the family matches.
///
/// Returns `true` when the new call was folded into an existing line.
pub fn push_or_collapse_tool(lines: &mut Vec<TranscriptLine>, text: String, is_error: bool) {
    if is_error {
        lines.push(TranscriptLine {
            kind: TranscriptKind::ToolError,
            text,
        });
        return;
    }
    let Some(family) = tool_family(&text) else {
        lines.push(TranscriptLine {
            kind: TranscriptKind::ToolDone,
            text,
        });
        return;
    };
    // Families we never want to collapse (unique labels matter).
    if matches!(family, "ReadTodos") {
        lines.push(TranscriptLine {
            kind: TranscriptKind::ToolDone,
            text,
        });
        return;
    }

    if let Some(last) = lines.last_mut()
        && last.kind == TranscriptKind::ToolDone
        && tool_family(&last.text) == Some(family)
    {
        let prev_n = tool_group_count(family, &last.text);
        let this_n = tool_group_count(family, &text);
        last.text = collapsed_tool_label(family, prev_n + this_n);
        return;
    }

    lines.push(TranscriptLine {
        kind: TranscriptKind::ToolDone,
        text,
    });
}

impl TurnView {
    /// Last non-empty lines of a buffer (for live strip preview).
    pub fn last_preview_lines(text: &str, n: usize) -> Vec<String> {
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .collect();
        if lines.is_empty() {
            return Vec::new();
        }
        let start = lines.len().saturating_sub(n);
        lines[start..].iter().map(|s| (*s).to_string()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_composer_insert_backspace() {
        let mut c = ComposerState::default();
        c.insert_char('a');
        c.insert_char('b');
        assert_eq!(c.buffer, "ab");
        c.backspace();
        assert_eq!(c.buffer, "a");
        assert_eq!(c.cursor, 1);
    }

    #[test]
    fn test_composer_caret_navigation() {
        let mut c = ComposerState::default();
        c.insert_char('a');
        c.insert_char('b');
        c.insert_char('c');
        assert_eq!(c.cursor, 3);
        c.move_left();
        assert_eq!(c.cursor, 2);
        c.move_left();
        assert_eq!(c.cursor, 1);
        c.move_home();
        assert_eq!(c.cursor, 0);
        c.move_end();
        assert_eq!(c.cursor, 3);
        c.move_left();
        c.delete_forward();
        assert_eq!(c.buffer, "ab");
        assert_eq!(c.cursor, 2);
    }

    #[test]
    fn test_composer_word_ops_and_paste() {
        let mut c = ComposerState::default();
        c.insert_str("hello world foo");
        c.move_home();
        c.move_word_right();
        assert_eq!(&c.buffer[..c.cursor], "hello ");
        c.delete_word_forward();
        assert_eq!(c.buffer, "hello foo");
        c.move_end();
        c.delete_word_backward();
        assert_eq!(c.buffer, "hello ");
        c.insert_str("bar\nbaz");
        assert_eq!(c.buffer, "hello bar baz");
    }

    #[test]
    fn test_submit_idle_sends() {
        let mut view = TurnView::default();
        view.composer.buffer = "hello".into();
        view.composer.cursor = 5;
        let actual = view.submit_composer();
        let expected = Some("hello".to_string());
        assert_eq!(actual, expected);
        // User prompt + timestamp + blank spacer.
        assert_eq!(view.transcript.len(), 3);
        assert_eq!(view.transcript[0].kind, TranscriptKind::User);
        assert_eq!(view.transcript[0].text, "hello");
        assert_eq!(view.transcript[1].kind, TranscriptKind::UserTime);
        assert!(!view.transcript[1].text.is_empty());
        assert_eq!(view.transcript[2].kind, TranscriptKind::Spacer);
        assert!(view.composer.buffer.is_empty());
    }

    /// Resume hang fix: status set/clear must bump gen so the draw loop
    /// repaints after "Loading conversation…" disappears.
    #[test]
    fn test_status_bumps_paint_gen() {
        let mut view = TurnView::default();
        let g0 = view.transcript_gen;
        view.set_status("Loading conversation…");
        assert!(view.transcript_gen > g0);
        let g1 = view.transcript_gen;
        view.clear_status();
        assert!(view.transcript_gen > g1);
        assert!(view.status_line.is_none());
        // Idempotent clear must not thrash gen.
        let g2 = view.transcript_gen;
        view.clear_status();
        assert_eq!(view.transcript_gen, g2);
    }

    #[test]
    fn test_submit_busy_queues_sticky() {
        let mut view = TurnView::default();
        view.busy = true;
        // Simulate tools already in the transcript.
        view.push_transcript(TranscriptLine {
            kind: TranscriptKind::ToolDone,
            text: "Read a.rs · 0.1s".into(),
        });
        view.composer.buffer = "later".into();
        view.composer.cursor = 5;
        let actual = view.submit_composer();
        assert_eq!(actual, None);
        assert_eq!(view.pending.len(), 1);
        assert_eq!(view.pending[0].text, "later");
        assert!(!view.pending[0].time.is_empty());
        // Must NOT appear mid-tool stream.
        assert!(
            !view
                .transcript
                .iter()
                .any(|l| l.kind == TranscriptKind::Queued || l.text.contains("later"))
        );

        // When the turn ends, pending commits after tools as a real user row.
        let sent = view.commit_pending_to_transcript();
        assert_eq!(sent, vec!["later".to_string()]);
        assert!(view.pending.is_empty());
        assert_eq!(view.transcript.last().unwrap().kind, TranscriptKind::Spacer);
        assert!(
            view.transcript
                .iter()
                .any(|l| l.kind == TranscriptKind::User && l.text == "later")
        );
        assert!(
            view.transcript
                .iter()
                .any(|l| l.kind == TranscriptKind::UserTime)
        );
    }

    #[test]
    fn test_thought_merge() {
        let mut view = TurnView::default();
        view.begin_thinking();
        view.push_reasoning("first ");
        view.flush_thought();
        view.begin_thinking();
        view.push_reasoning("second");
        view.flush_thought();
        let thoughts: Vec<_> = view
            .transcript
            .iter()
            .filter(|l| l.kind == TranscriptKind::Thought)
            .collect();
        assert_eq!(thoughts.len(), 1);
        assert!(thoughts[0].text.starts_with("Thought for"));
    }

    #[test]
    fn test_flush_thought_idle_is_noop() {
        let mut view = TurnView::default();
        view.begin_thinking();
        view.push_reasoning("x");
        view.flush_thought();
        let n = view.transcript.len();
        // Second flush while idle must not add another Thought row.
        view.flush_thought();
        view.flush_thought();
        assert_eq!(view.transcript.len(), n);
    }

    #[test]
    fn test_append_assistant_merges_tokens() {
        let mut view = TurnView::default();
        view.begin_thinking();
        view.push_reasoning("plan");
        view.append_assistant("Hello ");
        view.append_assistant("world");
        view.append_assistant("!");
        let thoughts: Vec<_> = view
            .transcript
            .iter()
            .filter(|l| l.kind == TranscriptKind::Thought)
            .collect();
        let assistants: Vec<_> = view
            .transcript
            .iter()
            .filter(|l| l.kind == TranscriptKind::Assistant)
            .collect();
        assert_eq!(thoughts.len(), 1);
        assert_eq!(assistants.len(), 1);
        assert_eq!(assistants[0].text, "Hello world!");
    }

    #[test]
    fn test_tool_finish() {
        let mut view = TurnView::default();
        view.begin_tool("Search foo");
        view.push_tool_output("hit\n");
        view.finish_tool(None);
        assert!(matches!(view.activity, ActivityPhase::Idle));
        let done = view
            .transcript
            .iter()
            .find(|l| l.kind == TranscriptKind::ToolDone)
            .expect("tool done line");
        assert!(
            done.text.starts_with("Search foo · "),
            "expected elapsed suffix, got {:?}",
            done.text
        );
        assert!(
            view.transcript
                .iter()
                .any(|l| l.kind == TranscriptKind::ToolBody && l.text == "hit")
        );
    }

    #[test]
    fn test_consecutive_reads_collapse() {
        let mut view = TurnView::default();
        for path in ["a.rs", "b.rs", "c.rs"] {
            view.begin_tool(format!("Read {path}"));
            view.finish_tool(None);
        }
        let tools: Vec<_> = view
            .transcript
            .iter()
            .filter(|l| l.kind == TranscriptKind::ToolDone)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(tools.len(), 1, "expected one collapsed row, got {tools:?}");
        assert_eq!(tools[0], "Read 3 files");
    }

    #[test]
    fn test_mixed_tools_collapse_by_family() {
        let mut lines = Vec::new();
        push_or_collapse_tool(&mut lines, "Read a.rs · 0.1s".into(), false);
        push_or_collapse_tool(&mut lines, "Read b.rs · 0.1s".into(), false);
        push_or_collapse_tool(
            &mut lines,
            "Search for 'foo' at crates/x · 0.1s".into(),
            false,
        );
        push_or_collapse_tool(
            &mut lines,
            "Search for 'bar' at crates/y · 0.2s".into(),
            false,
        );
        push_or_collapse_tool(&mut lines, "Read c.rs · 0.1s".into(), false);
        push_or_collapse_tool(&mut lines, "Shell cargo test · 1.0s".into(), false);
        let texts: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "Read 2 files",
                "Search 2 times",
                "Read c.rs · 0.1s",
                "Shell cargo test · 1.0s",
            ]
        );
    }

    #[test]
    fn test_tool_with_body_does_not_collapse() {
        let mut view = TurnView::default();
        view.begin_tool("Read a.rs");
        view.finish_tool(None);
        view.begin_tool("Read b.rs");
        view.push_tool_output("file contents here\n");
        view.finish_tool(None);
        let tools: Vec<_> = view
            .transcript
            .iter()
            .filter(|l| l.kind == TranscriptKind::ToolDone)
            .map(|l| l.text.clone())
            .collect();
        // Second has a body → stays separate; first stays as single (not yet collapsed).
        assert_eq!(tools.len(), 2);
        assert!(tools[0].starts_with("Read a.rs"));
        assert!(tools[1].starts_with("Read b.rs"));
    }

    #[test]
    fn test_todo_output_appears_in_transcript() {
        let mut view = TurnView::default();
        view.begin_tool("Update Todos 4 item(s)");
        view.push_tool_output("\n  󰄗 First task\n  󰄱 Second task\n");
        view.finish_tool(None);
        let bodies: Vec<_> = view
            .transcript
            .iter()
            .filter(|l| l.kind == TranscriptKind::ToolBody)
            .map(|l| l.text.as_str())
            .collect();
        assert!(bodies.iter().any(|l| l.contains("First task")));
        assert!(bodies.iter().any(|l| l.contains("Second task")));
        assert!(!view.transcript.iter().any(|l| l.text.starts_with("Run ")));
    }

    #[test]
    fn test_last_preview_lines() {
        let fixture = "a\n\nb\nc\nd\n";
        let actual = TurnView::last_preview_lines(fixture, 2);
        let expected = vec!["c".to_string(), "d".to_string()];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_scroll_offset_clamps_and_jump() {
        let mut view = TurnView::default();
        // Enough rows that floor metrics allow scroll before paint.
        for i in 0..100 {
            view.transcript.push(TranscriptLine {
                kind: TranscriptKind::Info,
                text: format!("row{i}"),
            });
        }
        view.last_content_lines = 100;
        view.last_viewport_rows = 20;
        view.scroll_by(50);
        assert!(!view.follow_bottom);
        assert_eq!(view.viewport_end, 50); // total 100, up 50 → end 50
        assert_eq!(view.scroll_offset, 50);
        view.scroll_by(1000); // hit top
        assert_eq!(view.viewport_end, 20); // min_end = viewport
        assert_eq!(view.scroll_offset, 80);
        view.scroll_by(-30);
        assert_eq!(view.viewport_end, 50);
        view.scroll_to_bottom();
        assert!(view.is_at_bottom());
        assert!(view.follow_bottom);
        view.scroll_page(true);
        assert!(!view.follow_bottom);
        assert_eq!(view.viewport_end, 80); // 100 - 20
        view.scroll_page(false);
        assert!(view.is_at_bottom());
    }

    #[test]
    fn test_absolute_viewport_stable_while_content_grows() {
        let mut view = TurnView::default();
        for i in 0..100 {
            view.transcript.push(TranscriptLine {
                kind: TranscriptKind::ToolDone,
                text: format!("Read {i}.rs"),
            });
        }
        view.last_content_lines = 100;
        view.last_viewport_rows = 20;
        view.scroll_by(40); // leave bottom
        let end_before = view.viewport_end;
        assert!(!view.follow_bottom);
        assert_eq!(end_before, 60);

        // Live tools append — absolute end must stay put (no preserve needed).
        view.last_content_lines = 130;
        for i in 100..130 {
            view.transcript.push(TranscriptLine {
                kind: TranscriptKind::ToolDone,
                text: format!("Read {i}.rs"),
            });
        }
        // draw path only updates last_content_lines + clamp; end unchanged.
        view.clamp_scroll();
        assert_eq!(view.viewport_end, end_before);
        assert!(!view.follow_bottom);

        // visible range still ends at the same absolute line
        let (start, end) = view.visible_range(130, 20);
        assert_eq!(end, 60);
        assert_eq!(start, 40);
    }

    #[test]
    fn test_hit_rect_contains() {
        let hit = HitRect {
            x: 10,
            y: 5,
            width: 20,
            height: 1,
        };
        assert!(hit.contains(10, 5));
        assert!(hit.contains(29, 5));
        assert!(!hit.contains(30, 5));
        assert!(!hit.contains(15, 6));
    }
}
