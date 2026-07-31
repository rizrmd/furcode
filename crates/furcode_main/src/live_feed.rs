//! Compact live activity feed for the interactive TUI.
//!
//! ## Compact mode (default)
//! - While thinking / a tool is running: animated status + **last 2 lines** of
//!   streamed text (redrawn in place).
//! - When a phase ends: replace the live block with a **single summary line**.
//! - Long history collapses with `◇ N more` (head + tail).
//!
//! ## Out-loud mode
//! This module is mostly idle; the UI streams full reasoning / tool output.

use std::time::Instant;

use anyhow::Result;
use colored::Colorize;
use furcode_config::ActivityDisplay;
use furcode_domain::{ConsoleWriter, TitleFormat};

use crate::stream_renderer::SharedSpinner;
use crate::title_display::TitleDisplayExt;

/// How many of the oldest summary lines to keep when collapsing history.
const COLLAPSE_HEAD: usize = 2;
/// How many of the newest summary lines to keep when collapsing history.
/// Higher than before so tool bodies (todo checklists) stay visible.
const COLLAPSE_TAIL: usize = 12;
/// Live body lines shown under the animated status (thinking / tool).
const LIVE_TAIL_LINES: usize = 4;
/// Max visible width for a live preview line.
const LIVE_LINE_WIDTH: usize = 88;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Elapsed / spinner refresh interval (matches the chat-loop ticker).
pub const LIVE_TICK_MS: u64 = 100;

/// One finalized summary line (already formatted, may include ANSI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityLine {
    pub text: String,
    /// When true, further thinking segments merge into this line's duration.
    pub is_thought: bool,
}

/// Current streaming phase drawn as a live overlay (not yet summarized).
#[derive(Debug, Clone)]
enum LivePhase {
    Idle,
    Thinking {
        started: Instant,
        /// Accumulated reasoning text for last-N line extraction.
        buffer: String,
    },
    Tool {
        started: Instant,
        /// Plain label for live status (no icon), e.g. "Read src/main.rs".
        live_label: String,
        /// Final history line when the tool completes (includes green ●).
        done_text: String,
        /// Accumulated tool output (optional).
        output: String,
        is_error: bool,
    },
}

/// Compact activity feed with animation and last-2-line live previews.
pub struct LiveActivityFeed {
    mode: ActivityDisplay,
    /// Finalized one-line summaries for this turn block.
    history: Vec<ActivityLine>,
    live: LivePhase,
    /// Sum of closed thinking segments not yet shown, merged into one line.
    thought_acc: std::time::Duration,
    /// Terminal rows occupied by the last redraw (history + live).
    printed_rows: usize,
}

impl Default for LiveActivityFeed {
    fn default() -> Self {
        Self::new(ActivityDisplay::Compact)
    }
}

impl LiveActivityFeed {
    /// Creates a feed for the given display mode.
    pub fn new(mode: ActivityDisplay) -> Self {
        Self {
            mode,
            history: Vec::new(),
            live: LivePhase::Idle,
            thought_acc: std::time::Duration::ZERO,
            printed_rows: 0,
        }
    }

    /// Whether this feed uses compact live rendering.
    pub fn is_compact(&self) -> bool {
        matches!(self.mode, ActivityDisplay::Compact)
    }

    /// Whether a thinking/tool live overlay is active (needs 0.1s ticks).
    ///
    /// Also true while typeahead is active so the `› ` draft line refreshes
    /// as the user types (display is owned by this feed).
    pub fn needs_tick(&self) -> bool {
        if !self.is_compact() {
            return false;
        }
        if !matches!(self.live, LivePhase::Idle) {
            return true;
        }
        crate::interrupt::typeahead_is_active()
    }

    /// Refreshes spinner frame + elapsed time without new content.
    ///
    /// Called on a fixed interval (~100ms) from the chat loop so the UI stays
    /// smooth even when the model is silent between tokens.
    pub fn tick<P: ConsoleWriter + 'static>(
        &mut self,
        spinner: &SharedSpinner<P>,
    ) -> Result<()> {
        if !self.needs_tick() {
            return Ok(());
        }
        self.redraw(spinner)
    }

    /// Marks reasoning start (no-op if already thinking).
    pub fn note_reasoning<P: ConsoleWriter + 'static>(
        &mut self,
        spinner: &SharedSpinner<P>,
    ) -> Result<()> {
        if !self.is_compact() {
            return Ok(());
        }
        if !matches!(self.live, LivePhase::Thinking { .. }) {
            // Stop the global spinner so it does not fight multi-line redraws.
            let _ = spinner.stop(None);
            self.live = LivePhase::Thinking {
                started: Instant::now(),
                buffer: String::new(),
            };
            self.redraw(spinner)?;
        }
        Ok(())
    }

    /// Appends a reasoning chunk; keeps only last 2 lines visible in compact mode.
    pub fn push_reasoning_chunk<P: ConsoleWriter + 'static>(
        &mut self,
        spinner: &SharedSpinner<P>,
        chunk: &str,
    ) -> Result<()> {
        if !self.is_compact() {
            return Ok(());
        }
        self.note_reasoning(spinner)?;
        if let LivePhase::Thinking { buffer, .. } = &mut self.live {
            buffer.push_str(chunk);
            // Cap buffer so very long thoughts do not grow without bound.
            if buffer.len() > 16_384 {
                let keep = buffer.len() - 8_192;
                *buffer = buffer[keep..].to_string();
            }
        }
        // Content change — redraw immediately (timer also ticks elapsed).
        self.redraw(spinner)
    }

    /// Ends the live thinking phase and merges duration into a single summary.
    ///
    /// Multiple reasoning segments in one activity block become one
    /// `Thought for Xs` line (durations summed), not N separate lines.
    pub fn flush_thought<P: ConsoleWriter + 'static>(
        &mut self,
        spinner: &SharedSpinner<P>,
    ) -> Result<()> {
        if !self.is_compact() {
            self.live = LivePhase::Idle;
            return Ok(());
        }
        if let LivePhase::Thinking { started, .. } =
            std::mem::replace(&mut self.live, LivePhase::Idle)
        {
            self.thought_acc += started.elapsed();
        }
        self.sync_thought_history_line();
        self.redraw(spinner)
    }

    /// Writes/updates the single merged thought line in history from `thought_acc`.
    fn sync_thought_history_line(&mut self) {
        if self.thought_acc.is_zero() {
            return;
        }
        let label = format_duration_label("Thought for", self.thought_acc);
        let text = TitleFormat::info(label).display().to_string();
        if let Some(last) = self.history.last_mut()
            && last.is_thought
        {
            last.text = text;
            return;
        }
        self.history
            .push(ActivityLine { text, is_thought: true });
    }

    /// Starts an animated tool phase (compact). Title is the final summary base.
    pub fn begin_tool<P: ConsoleWriter + 'static>(
        &mut self,
        spinner: &SharedSpinner<P>,
        title: TitleFormat,
    ) -> Result<()> {
        if !self.is_compact() {
            return Ok(());
        }
        // Finish any open thought first (merges into one Thought line).
        if matches!(self.live, LivePhase::Thinking { .. }) {
            self.flush_thought(spinner)?;
        } else {
            self.sync_thought_history_line();
        }
        // Live status uses plain text only — no green ● (avoids ⠋ ● double icon).
        let live_label = title_live_label(&title);
        let done_text = title.display().to_string();
        // Stop global spinner — live overlay owns animation while the tool runs.
        let _ = spinner.stop(None);
        self.live = LivePhase::Tool {
            started: Instant::now(),
            live_label,
            done_text,
            output: String::new(),
            is_error: false,
        };
        self.redraw(spinner)
    }

    /// Streams tool output into the live tool preview (last 2 lines).
    pub fn push_tool_output_chunk<P: ConsoleWriter + 'static>(
        &mut self,
        spinner: &SharedSpinner<P>,
        chunk: &str,
    ) -> Result<()> {
        if !self.is_compact() {
            return Ok(());
        }
        if let LivePhase::Tool { output, .. } = &mut self.live {
            output.push_str(chunk);
            if output.len() > 16_384 {
                let keep = output.len() - 8_192;
                *output = output[keep..].to_string();
            }
        }
        self.redraw(spinner)
    }

    /// Ends the tool phase with a summary line (and optional body, e.g. todos).
    ///
    /// Completed tools use a static green `●` only — no braille spinner.
    /// Non-empty tool output (todo checklists, diffs) is kept under the summary
    /// so the user can still see it after the live preview collapses.
    pub fn finish_tool<P: ConsoleWriter + 'static>(
        &mut self,
        spinner: &SharedSpinner<P>,
        summary: Option<TitleFormat>,
    ) -> Result<()> {
        if !self.is_compact() {
            self.live = LivePhase::Idle;
            return Ok(());
        }
        let prev = std::mem::replace(&mut self.live, LivePhase::Idle);
        let (text, output) = match (summary, prev) {
            (Some(title), LivePhase::Tool { output, .. }) => {
                (title.display().to_string(), output)
            }
            (Some(title), _) => (title.display().to_string(), String::new()),
            (None, LivePhase::Tool { done_text, output, .. }) => (done_text, output),
            (None, _) => return Ok(()),
        };
        self.history
            .push(ActivityLine { text, is_thought: false });
        // Surface tool bodies that matter (todo lists, file diffs, …).
        for line in output.lines() {
            if line.trim().is_empty() {
                continue;
            }
            self.history.push(ActivityLine {
                text: line.to_string(),
                is_thought: false,
            });
        }
        // Do not start the global "Contemplating" spinner here — that left a
        // second animation competing with the finished tool line. The chat
        // loop restarts the spinner only when waiting on the next model turn
        // if needed.
        let _ = spinner.stop(None);
        self.redraw(spinner)
    }

    /// Appends a pre-built summary line (e.g. retry / meta).
    pub fn push_title<P: ConsoleWriter + 'static>(
        &mut self,
        spinner: &SharedSpinner<P>,
        title: TitleFormat,
    ) -> Result<()> {
        if !self.is_compact() {
            return Ok(());
        }
        if matches!(self.live, LivePhase::Thinking { .. }) {
            self.flush_thought(spinner)?;
        }
        let text = title.display().to_string();
        if text.is_empty() {
            return Ok(());
        }
        self.history
            .push(ActivityLine { text, is_thought: false });
        self.redraw(spinner)
    }

    /// Leaves history on screen and clears bookkeeping (before assistant prose).
    ///
    /// Resets thought accumulation so the next thinking block starts fresh
    /// (after the user already saw a summary + answer).
    pub fn commit(&mut self) {
        self.history.clear();
        self.live = LivePhase::Idle;
        self.thought_acc = std::time::Duration::ZERO;
        self.printed_rows = 0;
    }

    /// Freeze the current activity block on screen without resetting
    /// `thought_acc` bookkeeping mid-draw — used after flushing before prose.
    ///
    /// Unlike [`commit`], this only detaches from redraw (`printed_rows = 0`)
    /// and clears history tracking; `thought_acc` is cleared so later thinking
    /// in the same turn becomes a new segment after prose (expected).
    pub fn seal(&mut self) {
        self.history.clear();
        self.live = LivePhase::Idle;
        self.thought_acc = std::time::Duration::ZERO;
        self.printed_rows = 0;
    }

    /// Flushes live phases and commits.
    pub fn finish<P: ConsoleWriter + 'static>(
        &mut self,
        spinner: &SharedSpinner<P>,
    ) -> Result<()> {
        self.flush_thought(spinner)?;
        if matches!(self.live, LivePhase::Tool { .. }) {
            self.finish_tool(spinner, None)?;
        }
        self.commit();
        Ok(())
    }

    fn redraw<P: ConsoleWriter + 'static>(&mut self, spinner: &SharedSpinner<P>) -> Result<()> {
        // Keep the global spinner fully stopped for the whole live phase — a
        // merely-paused spinner used to be re-resumed by every ewrite_ln, which
        // painted `\r` loader frames over the multi-line overlay and broke
        // clear_lines row accounting (Thinking spam).
        let _ = spinner.stop(None);

        // All rows in the previous frame ended with `\n`, so the cursor sits
        // *below* the block. clear_lines moves up N then CSI 0 J (erase down).
        if self.printed_rows > 0 {
            spinner.clear_lines(self.printed_rows)?;
        }

        let history_view = collapse_activity_lines(&self.history, COLLAPSE_HEAD, COLLAPSE_TAIL);
        let live_rows = self.render_live_rows();
        // Typeahead is the last line of this block (trailing newline) so the
        // next clear_lines count stays exact.
        let typeahead_line = crate::interrupt::typeahead_snapshot()
            .map(|t| crate::interrupt::format_typeahead_line(&t));

        for line in &history_view {
            spinner.ewrite_ln(&line.text)?;
        }
        for line in &live_rows {
            spinner.ewrite_ln(line)?;
        }
        if let Some(ref ta) = typeahead_line {
            spinner.ewrite_ln(ta)?;
        }

        self.printed_rows =
            history_view.len() + live_rows.len() + usize::from(typeahead_line.is_some());

        // Do not resume global spinner while compact live UI or typeahead owns
        // the chrome — live rows already carry their own braille frames.
        Ok(())
    }

    fn render_live_rows(&self) -> Vec<String> {
        match &self.live {
            LivePhase::Idle => Vec::new(),
            LivePhase::Thinking { started, buffer } => {
                let mut rows = Vec::with_capacity(1 + LIVE_TAIL_LINES);
                rows.push(format_status_line("Thinking", *started, false));
                for line in last_n_content_lines(buffer, LIVE_TAIL_LINES) {
                    rows.push(format_preview_line(&line));
                }
                rows
            }
            LivePhase::Tool {
                started,
                live_label,
                output,
                is_error,
                ..
            } => {
                let mut rows = Vec::with_capacity(1 + LIVE_TAIL_LINES);
                let short = if live_label.chars().count() > 60 {
                    let t: String = live_label.chars().take(57).collect();
                    format!("{t}…")
                } else {
                    live_label.clone()
                };
                rows.push(format_status_line(&short, *started, *is_error));
                for line in last_n_content_lines(output, LIVE_TAIL_LINES) {
                    rows.push(format_preview_line(&line));
                }
                rows
            }
        }
    }
}

/// Plain label for the live tool status row (no colored ● icon, no "Run " prefix).
fn title_live_label(title: &TitleFormat) -> String {
    let mut body = title
        .title
        .strip_prefix("Run ")
        .unwrap_or(title.title.as_str())
        .to_string();
    if let Some(sub) = title.sub_title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        body.push(' ');
        body.push_str(sub);
    }
    body
}

fn format_status_line(label: &str, started: Instant, is_error: bool) -> String {
    let frame = spinner_frame(started);
    let elapsed = format_elapsed_short(started.elapsed());
    if is_error {
        format!(
            "{} {} {}",
            frame.red(),
            label.red(),
            elapsed.dimmed()
        )
    } else {
        format!(
            "{} {} {}",
            frame.cyan(),
            label.dimmed(),
            elapsed.dimmed()
        )
    }
}

fn format_preview_line(line: &str) -> String {
    let truncated = truncate_chars(line, LIVE_LINE_WIDTH);
    format!("  {} {}", "│".dimmed(), truncated.dimmed())
}

fn spinner_frame(started: Instant) -> &'static str {
    let idx = ((started.elapsed().as_millis() / u128::from(LIVE_TICK_MS)) as usize)
        % SPINNER_FRAMES.len();
    SPINNER_FRAMES[idx]
}

fn format_elapsed_short(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("· {secs:.1}s")
    } else {
        let m = (secs / 60.0).floor() as u64;
        let s = secs - m as f64 * 60.0;
        format!("· {m}m{s:.0}s")
    }
}

fn format_duration_label(prefix: &str, d: std::time::Duration) -> String {
    let secs = d.as_secs_f64().max(0.1);
    if secs < 60.0 {
        format!("{prefix} {secs:.1}s")
    } else {
        let mins = (secs / 60.0).floor() as u64;
        let rem = secs - mins as f64 * 60.0;
        format!("{prefix} {mins}m{rem:.0}s")
    }
}

fn last_n_content_lines(text: &str, n: usize) -> Vec<String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let start = lines.len().saturating_sub(n);
    lines[start..]
        .iter()
        .map(|l| (*l).to_string())
        .collect()
}

fn truncate_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}

/// Builds the visible slice of activity lines with a middle collapse marker.
pub fn collapse_activity_lines(
    lines: &[ActivityLine],
    head: usize,
    tail: usize,
) -> Vec<ActivityLine> {
    let n = lines.len();
    if n == 0 {
        return Vec::new();
    }
    if n <= head.saturating_add(tail) {
        return lines.to_vec();
    }
    let middle = n - head - tail;
    if middle == 0 {
        return lines.to_vec();
    }

    let mut out = Vec::with_capacity(head + 1 + tail);
    out.extend_from_slice(&lines[..head]);
    out.push(ActivityLine {
        text: format!("{} {} more", "◇".dimmed(), middle.to_string().dimmed()),
        is_thought: false,
    });
    out.extend_from_slice(&lines[n - tail..]);
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn lines(xs: &[&str]) -> Vec<ActivityLine> {
        xs.iter()
            .map(|s| ActivityLine {
                text: (*s).to_string(),
                is_thought: false,
            })
            .collect()
    }

    #[test]
    fn test_last_n_content_lines() {
        let actual = last_n_content_lines("a\n\nb\nc\nd\n", 2);
        assert_eq!(actual, vec!["c".to_string(), "d".to_string()]);
    }

    #[test]
    fn test_merge_thought_history_single_line() {
        let mut feed = LiveActivityFeed::new(ActivityDisplay::Compact);
        feed.thought_acc = std::time::Duration::from_millis(300);
        feed.sync_thought_history_line();
        feed.thought_acc = std::time::Duration::from_millis(300 + 1000);
        feed.sync_thought_history_line();
        feed.thought_acc = std::time::Duration::from_millis(300 + 1000 + 600);
        feed.sync_thought_history_line();
        assert_eq!(feed.history.len(), 1);
        assert!(feed.history[0].is_thought);
        assert!(feed.history[0].text.contains("Thought for"));
    }

    #[test]
    fn test_collapse_inserts_more_marker() {
        let labels: Vec<String> = (0..10).map(|i| format!("L{i}")).collect();
        let fixture: Vec<_> = labels
            .iter()
            .map(|s| ActivityLine {
                text: s.clone(),
                is_thought: false,
            })
            .collect();
        let actual = collapse_activity_lines(&fixture, 2, 6);
        assert_eq!(actual.len(), 2 + 1 + 6);
        assert!(actual[2].text.contains("2 more"));
    }

    #[test]
    fn test_truncate_chars() {
        let actual = truncate_chars("hello world", 5);
        assert_eq!(actual, "hell…");
    }

    #[test]
    fn test_activity_display_parse() {
        assert_eq!(
            ActivityDisplay::parse("compact"),
            Some(ActivityDisplay::Compact)
        );
        assert_eq!(
            ActivityDisplay::parse("out_loud"),
            Some(ActivityDisplay::OutLoud)
        );
        assert_eq!(
            ActivityDisplay::parse("thinking_out_loud"),
            Some(ActivityDisplay::OutLoud)
        );
    }
}
