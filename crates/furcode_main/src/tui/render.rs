//! Ratatui drawing for [`super::view::TurnView`].
//!
//! Transcript memory model (important):
//! - Source of truth is `TurnView.transcript` (raw strings only).
//! - We **never** keep plain+rich copies of the entire history.
//! - Only assistant rows near the viewport get an md-tui render, and far
//!   rows are evicted so memory stays O(viewport), not O(history).

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar;

use super::view::{
    ActivityPhase, COMPOSER_PREFIX_COLS, HitRect, SelectablePaint, StatusTone, TextSelection,
    TranscriptKind, TranscriptLine, TurnView, UserPromptHit, composer_content_width,
    composer_cursor_cell, soft_wrap_ranges,
};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const JUMP_BOTTOM_LABEL: &str = " ↓ Jump to bottom ";
/// Screen-lines of overscan for rich markdown.
const RICH_OVERSCAN: usize = 16;
/// Hard cap on richly-rendered assistant rows kept in memory.
const MAX_RICH_ROWS: usize = 12;
/// Soft-wrapped user prompts longer than this show 2 body lines + `…`.
const USER_PROMPT_MAX_FULL_LINES: usize = 3;
/// Body lines kept when a user prompt is truncated (ellipsis is the next row).
const USER_PROMPT_TRUNCATED_BODY: usize = 2;

// ─── Lean cache ─────────────────────────────────────────────────────────────

/// Rich render of one assistant transcript row.
struct RichRow {
    text_len: usize,
    lines: Vec<Line<'static>>,
}

struct TranscriptCache {
    revision: u64,
    width: u16,
    /// Estimated height per transcript index (cheap, no string storage).
    heights: Vec<usize>,
    /// Prefix sums of heights; len = heights.len() + 1.
    offsets: Vec<usize>,
    total_lines: usize,
    /// Only near-viewport assistant rows: index → rich lines.
    rich: HashMap<usize, RichRow>,
    /// Last measured height for assistant rows (text_len, height). Survives
    /// rich eviction so scrolling away from a tall reply does not collapse
    /// total height and snap `scroll_offset` to the bottom while Working.
    measured: HashMap<usize, (usize, usize)>,
}

impl TranscriptCache {
    fn empty() -> Self {
        Self {
            revision: u64::MAX,
            width: 0,
            heights: Vec::new(),
            offsets: Vec::new(),
            total_lines: 0,
            rich: HashMap::new(),
            measured: HashMap::new(),
        }
    }

    fn recompute_offsets(&mut self) {
        self.offsets.clear();
        self.offsets.reserve(self.heights.len() + 1);
        let mut acc = 0usize;
        self.offsets.push(0);
        for &h in &self.heights {
            acc += h.max(1);
            self.offsets.push(acc);
        }
        self.total_lines = acc;
    }
}

thread_local! {
    static TRANSCRIPT_CACHE: std::cell::RefCell<TranscriptCache> =
        std::cell::RefCell::new(TranscriptCache::empty());
}

// ─── Layout ─────────────────────────────────────────────────────────────────

/// "Interrupted" bright red; resume hint white. No spinner glyph.
fn interrupted_status_line(status: &str) -> Line<'static> {
    const PREFIX: &str = "Interrupted";
    // Bright red so it cannot read as gray on dark themes.
    let red = Style::default()
        .fg(Color::Rgb(255, 85, 85))
        .add_modifier(Modifier::BOLD);
    if let Some(rest) = status.strip_prefix(PREFIX) {
        return Line::from(vec![
            Span::styled(PREFIX.to_string(), red),
            Span::styled(rest.to_string(), Style::default().fg(Color::White)),
        ]);
    }
    Line::from(Span::styled(status.to_string(), red))
}

fn activity_row_count(view: &TurnView) -> u16 {
    const MAX_PREVIEW: usize = 2;
    if view.status_line.is_some() {
        // blank + status + blank so the line is not flush against transcript/composer
        match view.status_tone {
            StatusTone::Interrupted | StatusTone::Hint => return 3,
            StatusTone::Loading | StatusTone::Interrupting => return 1,
        }
    }
    match &view.activity {
        ActivityPhase::Idle => u16::from(view.busy),
        ActivityPhase::Thinking { buffer, .. } => {
            1 + TurnView::last_preview_lines(buffer, MAX_PREVIEW).len() as u16
        }
        ActivityPhase::Tool { output, .. } => {
            1 + TurnView::last_preview_lines(output, MAX_PREVIEW).len() as u16
        }
    }
}

pub fn draw(frame: &mut Frame, view: &mut TurnView, tick: u64) {
    let area = frame.area();

    // Double-Esc fork picker (user message history).
    if view.fork_picker.is_some() {
        view.composer_hit = None;
        view.composer_dragging = false;
        draw_fork_picker(frame, area, view);
        return;
    }

    // Full-screen transcript history (double-Esc).
    if view.overlay.is_some() {
        view.composer_hit = None;
        view.composer_dragging = false;
        view.user_prompt_hits.clear();
        draw_overlay(frame, area, view);
        return;
    }

    // Click-to-expand long user prompt.
    if view.user_prompt_popup.is_some() {
        view.composer_hit = None;
        view.composer_dragging = false;
        view.user_prompt_hits.clear();
        draw_user_prompt_popup(frame, area, view);
        return;
    }

    let activity_h = activity_row_count(view);
    let pending_h = pending_row_count(view);
    let menu_h = command_menu_row_count(view, area.height);
    // Grow with soft-wrapped input; leave room for header/footer/activity.
    let composer_h = composer_row_count(&view.composer.buffer, area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                 // header
            Constraint::Min(3),                    // scrollable transcript
            Constraint::Length(pending_h),         // sticky queued prompts
            Constraint::Length(activity_h),        // live activity
            Constraint::Length(menu_h),            // slash command menu
            Constraint::Length(composer_h),        // multi-line composer
            Constraint::Length(1),                 // footer
        ])
        .split(area);

    draw_header(frame, chunks[0], view);
    draw_transcript(frame, chunks[1], view);
    if pending_h > 0 {
        draw_pending(frame, chunks[2], view);
    }
    if activity_h > 0 {
        draw_activity(frame, chunks[3], view, tick);
    }
    if menu_h > 0 {
        draw_command_menu(frame, chunks[4], view);
    }
    draw_composer(frame, chunks[5], view);
    draw_footer(frame, chunks[6], view);
    // Sonner-style pill (overlay; does not steal layout rows).
    draw_toast(frame, area, view);
}

/// Rows reserved for the slash-command menu (0 when closed).
fn command_menu_row_count(view: &TurnView, term_height: u16) -> u16 {
    let Some(picker) = view.command_picker.as_ref() else {
        return 0;
    };
    // header + items (capped) + blank separator feel
    let items = picker.filtered.len().min(10).max(1) as u16;
    let want = 1 + items; // title row + list
    // Leave room for header/composer/footer (~5) and a bit of transcript.
    let max = term_height.saturating_sub(8).max(3);
    want.min(max)
}

fn draw_command_menu(frame: &mut Frame, area: Rect, view: &TurnView) {
    let Some(picker) = view.command_picker.as_ref() else {
        return;
    };
    if area.height == 0 {
        return;
    }

    let title_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    frame.render_widget(
        Paragraph::new(Span::styled(
            " Commands · ↑↓ select · Enter run · Esc dismiss ",
            title_style,
        )),
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );

    if area.height < 2 {
        return;
    }

    let list_h = (area.height - 1) as usize;
    let n = picker.filtered.len();
    if n == 0 {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "  (no matches)",
                Style::default().fg(Color::DarkGray),
            )),
            Rect {
                x: area.x,
                y: area.y + 1,
                width: area.width,
                height: 1,
            },
        );
        return;
    }

    // Keep selection visible.
    let mut start = 0usize;
    if picker.selected >= list_h {
        start = picker.selected + 1 - list_h;
    }
    let end = (start + list_h).min(n);
    let mut lines: Vec<Line> = Vec::new();
    let name_w = 22usize.min(area.width.saturating_sub(4) as usize / 2).max(8);

    for (i, item) in picker.filtered.iter().enumerate().skip(start).take(end - start) {
        let selected = i == picker.selected;
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let dim = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mark = if selected { "› " } else { "  " };
        let name = truncate_pad(&item.name, name_w);
        let desc_budget = area.width.saturating_sub(2 + name_w as u16 + 1) as usize;
        let desc = truncate_ellipsize(&item.description, desc_budget);
        lines.push(Line::from(vec![
            Span::styled(format!("{mark}{name}"), style),
            Span::styled(format!(" {desc}"), dim),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines),
        Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: area.height - 1,
        },
    );
}

fn truncate_pad(s: &str, width: usize) -> String {
    let mut out: String = s.chars().take(width).collect();
    while out.chars().count() < width {
        out.push(' ');
    }
    out
}

fn truncate_ellipsize(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= width {
        return s.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let keep: String = s.chars().take(width - 1).collect();
    format!("{keep}…")
}

/// Floating copy/status toast — bottom-center of the frame, above the footer.
fn draw_toast(frame: &mut Frame, area: Rect, view: &TurnView) {
    let Some(toast) = view.toast.as_ref() else {
        return;
    };
    if std::time::Instant::now() >= toast.expires {
        return;
    }
    let label = format!(" ✓ {} ", toast.text);
    let width = (label.chars().count() as u16).min(area.width.saturating_sub(2)).max(1);
    let x = area.x + area.width.saturating_sub(width) / 2;
    // Sit just above the footer row.
    let y = area.y + area.height.saturating_sub(3);
    if area.height < 3 {
        return;
    }
    let rect = Rect {
        x,
        y,
        width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            label,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(52, 211, 153)) // soft mint, like Jump to bottom
                .add_modifier(Modifier::BOLD),
        )),
        rect,
    );
}

/// Max visual rows the composer may occupy (soft-wrap + explicit newlines).
const COMPOSER_MAX_ROWS: u16 = 8;

/// Screen rows for sticky pending prompts (always above activity / composer).
fn pending_row_count(view: &TurnView) -> u16 {
    if view.pending.is_empty() {
        return 0;
    }
    // Each prompt: › text (+wrap) + time line + spacer. Cap so a flood of queues
    // cannot steal the whole terminal.
    let mut rows = 0usize;
    for (i, p) in view.pending.iter().enumerate() {
        // Approximate wrap at ~80 cols for layout; draw re-wraps to real width.
        let text_rows = estimate_wrapped_lines(&p.text, 60).max(1);
        rows += text_rows + 1; // text + timestamp
        if i + 1 < view.pending.len() {
            rows += 1; // spacer between queued items
        }
    }
    rows.min(12) as u16
}

fn draw_pending(frame: &mut Frame, area: Rect, view: &TurnView) {
    if area.height == 0 || view.pending.is_empty() {
        return;
    }
    let width = area.width.max(20);
    let mut lines: Vec<Line> = Vec::new();
    for (i, p) in view.pending.iter().enumerate() {
        // Same look as a real user prompt (green), soft-capped like transcript.
        lines.extend(user_prompt_hanging_lines(&p.text, width));
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} · queued", p.time),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        if i + 1 < view.pending.len() {
            lines.push(Line::from(""));
        }
        if lines.len() as u16 >= area.height {
            break;
        }
    }
    // Clip to allocated rows.
    if lines.len() > area.height as usize {
        lines.truncate(area.height as usize);
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_fork_picker(frame: &mut Frame, area: Rect, view: &TurnView) {
    let Some(picker) = view.fork_picker.as_ref() else {
        return;
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(Span::styled(
            " Undo to message · ↑↓ select · Enter restore · Esc cancel ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        chunks[0],
    );

    let height = chunks[1].height as usize;
    let n = picker.items.len();
    // Keep selection visible.
    let mut start = 0usize;
    if picker.selected >= height {
        start = picker.selected + 1 - height;
    }
    let end = (start + height).min(n);
    let mut lines: Vec<Line> = Vec::new();
    for (i, item) in picker.items.iter().enumerate().skip(start).take(end - start) {
        let selected = i == picker.selected;
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let mark = if selected { "› " } else { "  " };
        lines.push(Line::from(Span::styled(
            format!("{mark}{}", item.label),
            style,
        )));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no user messages)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Paragraph::new(lines), chunks[1]);
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(
                " {}/{} · restores conversation + undoes file writes after this turn ",
                picker.selected.saturating_add(1).min(n.max(1)),
                n
            ),
            Style::default().fg(Color::DarkGray),
        )),
        chunks[2],
    );
}

fn draw_overlay(frame: &mut Frame, area: Rect, view: &mut TurnView) {
    let Some(ov) = view.overlay.as_ref() else {
        return;
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(Span::styled(
            " Transcript · wheel / PgUp/PgDn · drag select (auto-copy) · Esc close ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        chunks[0],
    );

    let width = chunks[1].width.max(20);
    // Expand each logical row with hanging wrap so scroll counts screen lines.
    let expanded: Vec<Line> = ov
        .lines
        .iter()
        .flat_map(|row| hanging_wrap_lines(row.kind, &row.text, width))
        .collect();
    let height = chunks[1].height as usize;
    let total = expanded.len();
    let max_scroll = total.saturating_sub(height.max(1));
    let scroll = ov.scroll_offset.min(max_scroll);
    // scroll_offset 0 = bottom (latest), matching main transcript.
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(height.max(1));
    let visible = expanded[start..end].to_vec();
    view.last_content_lines = total;
    view.last_viewport_rows = height.max(1);

    // Store for app-side drag select + copy.
    let plain: Vec<String> = visible.iter().map(line_plain_text).collect();
    view.selectable = SelectablePaint {
        area: HitRect {
            x: chunks[1].x,
            y: chunks[1].y,
            width: chunks[1].width,
            height: chunks[1].height,
        },
        rows: plain,
    };
    let painted = apply_selection_to_lines(&visible, chunks[1], view.selection.as_ref());
    frame.render_widget(Paragraph::new(painted), chunks[1]);
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(" {start}–{end} / {total} · drag select · auto-copy "),
            Style::default().fg(Color::DarkGray),
        )),
        chunks[2],
    );
}

/// Full-screen viewer for a long user prompt (opened by clicking truncated › text).
fn draw_user_prompt_popup(frame: &mut Frame, area: Rect, view: &mut TurnView) {
    let Some(popup) = view.user_prompt_popup.as_ref() else {
        return;
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(Span::styled(
            " User prompt · wheel / PgUp/PgDn · drag select · Esc close ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )),
        chunks[0],
    );

    let width = chunks[1].width.max(20);
    // Full text — never truncate inside the popup.
    let expanded = hanging_wrap_lines(TranscriptKind::User, &popup.text, width);
    let height = chunks[1].height as usize;
    let total = expanded.len();
    let max_scroll = total.saturating_sub(height.max(1));
    let scroll = popup.scroll_offset.min(max_scroll);
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(height.max(1));
    let visible = expanded[start..end].to_vec();
    view.last_content_lines = total;
    view.last_viewport_rows = height.max(1);

    let plain: Vec<String> = visible.iter().map(line_plain_text).collect();
    view.selectable = SelectablePaint {
        area: HitRect {
            x: chunks[1].x,
            y: chunks[1].y,
            width: chunks[1].width,
            height: chunks[1].height,
        },
        rows: plain,
    };
    let painted = apply_selection_to_lines(&visible, chunks[1], view.selection.as_ref());
    frame.render_widget(Paragraph::new(painted), chunks[1]);
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(" {start}–{end} / {total} · click Esc to close "),
            Style::default().fg(Color::DarkGray),
        )),
        chunks[2],
    );
}

fn line_plain_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Rebuild lines with reverse-video cells inside the active selection.
fn apply_selection_to_lines(
    lines: &[Line<'static>],
    area: Rect,
    selection: Option<&TextSelection>,
) -> Vec<Line<'static>> {
    let Some(sel) = selection else {
        return lines.to_vec();
    };
    let sel_style = Style::default()
        .fg(Color::Black)
        .bg(Color::White)
        .add_modifier(Modifier::BOLD);

    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let y = area.y.saturating_add(i as u16);
            let plain = line_plain_text(line);
            if y < sel.normalized().0.row || y > sel.normalized().1.row {
                return line.clone();
            }
            // Character-wise rebuild so selection works without Shift.
            let mut out_spans: Vec<Span<'static>> = Vec::new();
            let mut col = area.x;
            for ch in plain.chars() {
                let style = if sel.contains_cell(col, y) {
                    sel_style
                } else {
                    Style::default().fg(Color::White)
                };
                out_spans.push(Span::styled(ch.to_string(), style));
                col = col.saturating_add(1);
            }
            if out_spans.is_empty() {
                line.clone()
            } else {
                Line::from(out_spans)
            }
        })
        .collect()
}

/// Header / chrome color for built-in agents.
///
/// CODE = blue, PLAN = green, RESEARCH = purple.
fn agent_header_color(agent: &str) -> Color {
    match agent {
        "coding" | "CODE" | "CODING" => Color::Rgb(80, 160, 255), // blue
        "planning" | "PLAN" | "PLANNING" => Color::Rgb(80, 200, 120), // green
        "researching" | "RESEARCH" | "RESEARCHING" => Color::Rgb(180, 100, 255), // purple
        _ => Color::DarkGray,
    }
}

/// Short display label for built-in agents: CODE / PLAN / RESEARCH.
fn agent_display(agent: &str) -> Option<&'static str> {
    match agent {
        "coding" | "CODE" | "CODING" => Some("CODE"),
        "planning" | "PLAN" | "PLANNING" => Some("PLAN"),
        "researching" | "RESEARCH" | "RESEARCHING" => Some("RESEARCH"),
        _ => None,
    }
}

fn draw_header(frame: &mut Frame, area: Rect, view: &TurnView) {
    let left = Span::styled(
        view.header_left.as_str(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );

    let right = if let Some((agent_part, model_part)) = view
        .header_right
        .split_once("  ")
        .filter(|(a, _)| !a.is_empty())
    {
        let display = agent_display(agent_part).unwrap_or(agent_part);
        Line::from(vec![
            Span::styled(
                format!("{display}  "),
                Style::default().fg(agent_header_color(agent_part)),
            ),
            Span::styled(
                model_part.to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else if view.header_right.is_empty() {
        Line::from("")
    } else {
        Line::from(Span::styled(
            view.header_right.clone(),
            Style::default().fg(Color::DarkGray),
        ))
    };

    let width = area.width as usize;
    let right_cols: usize = right
        .spans
        .iter()
        .map(|s| s.content.chars().count())
        .sum();
    let pad = width
        .saturating_sub(view.header_left.chars().count())
        .saturating_sub(right_cols)
        .max(1);
    let mut spans = vec![left, Span::raw(" ".repeat(pad))];
    spans.extend(right.spans);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn is_tool_block(kind: TranscriptKind) -> bool {
    matches!(
        kind,
        TranscriptKind::ToolDone
            | TranscriptKind::ToolError
            | TranscriptKind::ToolBody
            | TranscriptKind::ErrorBody
    )
}

fn horizontal_rule_line(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat((width as usize).max(8)),
        Style::default().fg(Color::DarkGray),
    ))
}

// ─── Height estimation (no allocations of full line vectors) ────────────────

fn estimate_wrapped_lines(text: &str, width: usize) -> usize {
    let w = width.max(1);
    let mut total = 0usize;
    for para in text.split('\n') {
        let n = para.chars().count();
        total += if n == 0 { 1 } else { n.div_ceil(w) };
    }
    total.max(1)
}

fn kind_prefix(kind: TranscriptKind) -> (&'static str, Style) {
    match kind {
        TranscriptKind::User => ("› ", Style::default().fg(Color::Green)),
        TranscriptKind::UserTime => ("  ", Style::default().fg(Color::DarkGray)),
        TranscriptKind::Spacer => ("", Style::default()),
        TranscriptKind::Thought => ("◆ ", Style::default().fg(Color::DarkGray)),
        // Dim tool summaries — they are scaffolding, not primary content.
        TranscriptKind::ToolDone => ("· ", Style::default().fg(Color::DarkGray)),
        TranscriptKind::ToolError => ("· ", Style::default().fg(Color::Red)),
        TranscriptKind::ToolBody => ("  ", Style::default().fg(Color::White)),
        // Error detail is primary content — match assistant prose prominence.
        TranscriptKind::ErrorBody => ("  ", Style::default().fg(Color::White)),
        TranscriptKind::Info => ("◇ ", Style::default().fg(Color::DarkGray)),
        // Legacy mid-stream queue rows (no longer emitted); treat like user.
        TranscriptKind::Queued => ("› ", Style::default().fg(Color::Green)),
        TranscriptKind::Assistant => ("", Style::default().fg(Color::White)),
    }
}

/// Whether a tool-body line is a unified-style file diff row from [`DiffFormat`].
///
/// Examples: `  3 │ context`, `- 5 │ removed`, `+ 5 │ added`, `···`
fn is_diff_tool_body_line(text: &str) -> bool {
    let t = text.trim_end();
    if t == "..." || t == "···" || t == "No changes applied" {
        return true;
    }
    // "+ N │ …" / "- N │ …"
    if let Some(rest) = t
        .strip_prefix("+ ")
        .or_else(|| t.strip_prefix("- "))
    {
        return diff_line_has_number_and_sep(rest);
    }
    // Equal/context: "  N │ …" (two leading spaces, then digits)
    if let Some(rest) = t.strip_prefix("  ") {
        return diff_line_has_number_and_sep(rest.trim_start());
    }
    false
}

fn diff_line_has_number_and_sep(s: &str) -> bool {
    let mut chars = s.chars().peekable();
    if !chars.peek().is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }
    while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
        chars.next();
    }
    // optional spaces then | or │
    while chars.peek() == Some(&' ') {
        chars.next();
    }
    matches!(chars.next(), Some('|') | Some('│'))
}

/// Color for tool-body text. Diff rows get semantic red/green; todos stay white.
fn tool_body_style(text: &str) -> Style {
    if !is_diff_tool_body_line(text) {
        return Style::default().fg(Color::White);
    }
    let t = text.trim_end();
    if t == "..." || t == "···" || t == "No changes applied" {
        return Style::default().fg(Color::DarkGray);
    }
    if t.starts_with('+') {
        // Soft green — readable on dark terminals without neon blast.
        Style::default().fg(Color::Rgb(80, 200, 120))
    } else if t.starts_with('-') {
        Style::default().fg(Color::Rgb(255, 100, 100))
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Whether a blank + horizontal rule + blank should precede this transcript row.
///
/// - Assistant after a tool block (existing)
/// - User after an Info status (e.g. "Turn complete · …") so the next prompt is
///   visually separated from the finished turn
fn needs_leading_rule(kind: TranscriptKind, prev: Option<TranscriptKind>) -> bool {
    match kind {
        TranscriptKind::Assistant => prev.is_some_and(is_tool_block),
        TranscriptKind::User => matches!(prev, Some(TranscriptKind::Info)),
        _ => false,
    }
}

fn estimate_row_height(row: &TranscriptLine, width: u16, prev: Option<TranscriptKind>) -> usize {
    let rule = if needs_leading_rule(row.kind, prev) {
        3
    } else {
        0
    };
    match row.kind {
        TranscriptKind::Assistant => rule + estimate_wrapped_lines(&row.text, width as usize),
        TranscriptKind::User => {
            let (prefix, _) = kind_prefix(TranscriptKind::User);
            let content_w = (width as usize)
                .saturating_sub(display_cols(prefix))
                .max(1);
            rule + user_prompt_body_line_count(&row.text, content_w)
        }
        TranscriptKind::Spacer => 1,
        kind => {
            // Match hanging_wrap_lines exactly (word-break can exceed naive ceil).
            let is_diff_body =
                matches!(kind, TranscriptKind::ToolBody) && is_diff_tool_body_line(&row.text);
            let (hang_chars, hang_disp) = if is_diff_body {
                (0, 0)
            } else {
                list_marker_hang(kind, &row.text)
            };
            let prefix = if (hang_chars > 0 || is_diff_body)
                && matches!(
                    kind,
                    TranscriptKind::ToolBody | TranscriptKind::ErrorBody
                ) {
                ""
            } else {
                kind_prefix(kind).0
            };
            let prefix_cols = display_cols(prefix);
            let content_w = (width as usize)
                .saturating_sub(prefix_cols + hang_disp)
                .max(1);
            let body: String = row.text.chars().skip(hang_chars).collect();
            rule + wrap_text_chunks(&body, content_w).len().max(1)
        }
    }
}

/// How many screen rows a user prompt occupies after optional truncation.
fn user_prompt_body_line_count(text: &str, content_w: usize) -> usize {
    let full = wrap_text_chunks(text, content_w);
    if full.len() > USER_PROMPT_MAX_FULL_LINES {
        USER_PROMPT_TRUNCATED_BODY + 1 // 2 body lines + ellipsis
    } else {
        full.len().max(1)
    }
}

/// Whether a user prompt is shown truncated at this width (click opens full popup).
fn user_prompt_is_truncated(text: &str, width: u16) -> bool {
    let (prefix, _) = kind_prefix(TranscriptKind::User);
    let content_w = (width as usize)
        .saturating_sub(display_cols(prefix))
        .max(1);
    wrap_text_chunks(text, content_w).len() > USER_PROMPT_MAX_FULL_LINES
}

/// Soft-wrap a user prompt; if longer than 3 lines, keep 2 and add `…`.
fn user_prompt_wrap_chunks(text: &str, content_w: usize) -> (Vec<String>, bool) {
    let full = wrap_text_chunks(text, content_w);
    if full.len() > USER_PROMPT_MAX_FULL_LINES {
        let mut out: Vec<String> = full.into_iter().take(USER_PROMPT_TRUNCATED_BODY).collect();
        out.push("…".into());
        (out, true)
    } else {
        (full, false)
    }
}

/// Prompt lines for a user turn, optionally preceded by a separator rule.
fn user_prompt_lines(text: &str, width: u16, with_rule: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if with_rule {
        lines.push(Line::from(""));
        lines.push(horizontal_rule_line(width));
        lines.push(Line::from(""));
    }
    lines.extend(user_prompt_hanging_lines(text, width));
    lines
}

/// User prompt with hanging indent and 3-line soft cap (`…` on line 3 when truncated).
fn user_prompt_hanging_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    let (prefix, style) = kind_prefix(TranscriptKind::User);
    let prefix_cols = display_cols(prefix);
    let content_w = (width as usize).saturating_sub(prefix_cols).max(1);
    let cont_prefix = " ".repeat(prefix_cols);
    let (chunks, _truncated) = user_prompt_wrap_chunks(text, content_w);
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            if i == 0 {
                Line::from(vec![
                    Span::styled(prefix.to_string(), style),
                    Span::styled(chunk, style),
                ])
            } else if chunk == "…" {
                // Dim ellipsis row — hints the prompt is expandable.
                Line::from(vec![
                    Span::styled(cont_prefix.clone(), style),
                    Span::styled(
                        "…".to_string(),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled(cont_prefix.clone(), style),
                    Span::styled(chunk, style),
                ])
            }
        })
        .collect()
}

// ─── On-the-fly line builders (only for the visible window) ─────────────────

/// Word-wrap `text` into chunks of at most `width` display chars.
/// Breaks on whitespace when possible (paragraph-style).
fn wrap_text_chunks(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut rest = para;
        while !rest.is_empty() {
            let take = rest
                .char_indices()
                .nth(width)
                .map(|(i, _)| i)
                .unwrap_or(rest.len());
            let split = if take < rest.len() {
                rest[..take]
                    .rfind(char::is_whitespace)
                    .filter(|&i| i > 0)
                    .unwrap_or(take)
            } else {
                take
            };
            let (chunk, next) = rest.split_at(split);
            out.push(chunk.trim_end().to_string());
            rest = next.trim_start();
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Terminal display columns for a string. Nerd-font private-use icons often
/// paint **double-wide** even when UnicodeWidth says 1 — treat PUA as 2.
fn display_cols(s: &str) -> usize {
    s.chars().map(display_col_char).sum()
}

fn display_col_char(c: char) -> usize {
    match UnicodeWidthChar::width(c) {
        None | Some(0) => 0,
        Some(1) if is_private_use(c) => 2, // Nerd Font codicons / PUA
        Some(w) => w,
    }
}

fn is_private_use(c: char) -> bool {
    matches!(
        c as u32,
        0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD
    )
}

/// Char length + display columns of the list/checkbox prefix on a tool-body
/// line, so wrap continuations hang under the item text (aligned with the
/// first letter, e.g. `R` of `Rename` with `c` of `coding/...`).
///
/// Returns `(char_len, display_cols)` of the prefix through the space after
/// the marker. For plain bodies, returns leading-space only.
fn list_marker_hang(kind: TranscriptKind, text: &str) -> (usize, usize) {
    if !matches!(
        kind,
        TranscriptKind::ToolBody | TranscriptKind::ErrorBody
    ) {
        return (0, 0);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() && chars[i] == ' ' {
        i += 1;
    }
    let after_lead = i;
    if after_lead >= chars.len() {
        return (0, 0);
    }
    let lead_disp = after_lead; // spaces are width 1

    // One short marker token (nerd icon / ☐ / - / *) then whitespace.
    let marker_start = i;
    while i < chars.len() && !chars[i].is_whitespace() {
        i += 1;
        if i - marker_start > 2 {
            // Normal word, not a checkbox marker.
            return (after_lead, lead_disp);
        }
    }
    let marker_len = i - marker_start;
    if marker_len == 0 {
        return (after_lead, lead_disp);
    }
    if i >= chars.len() || !chars[i].is_whitespace() {
        return (after_lead, lead_disp);
    }
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    let prefix: String = chars[..i].iter().collect();
    (i, display_cols(&prefix))
}

/// Prefixed rows with **hanging indent**: wrap continuations align with the
/// first letter of the body (after `· ` / `› ` / checkbox…), not column 0.
///
/// ```text
/// · Search for 'long pattern' in rust files at
///   crates/furcode_main/src · 0.1s
///
///   󰄱 Rename agent IDs from forge/muse/sage to
///      coding/planning/researching
/// ```
fn hanging_wrap_lines(kind: TranscriptKind, text: &str, width: u16) -> Vec<Line<'static>> {
    let (default_prefix, default_style) = kind_prefix(kind);
    // Diff bodies: color by +/- and skip list-marker hang (sign is not a bullet).
    let is_diff_body =
        matches!(kind, TranscriptKind::ToolBody) && is_diff_tool_body_line(text);
    let style = if is_diff_body {
        tool_body_style(text)
    } else {
        default_style
    };
    let (hang_chars, hang_disp) = if is_diff_body {
        (0, 0)
    } else {
        list_marker_hang(kind, text)
    };
    // Checklist lines already include their own `"  󰄱 "` indent — do not stack
    // the ToolBody kind prefix on top (that double-indent made wrap hang wrong).
    // Diff rows also carry their own indent (`  N │` / `+ N │`); no extra pad.
    let prefix = if (hang_chars > 0 || is_diff_body)
        && matches!(
            kind,
            TranscriptKind::ToolBody | TranscriptKind::ErrorBody
        ) {
        ""
    } else {
        default_prefix
    };
    let prefix_cols = display_cols(prefix);
    let hang_prefix: String = text.chars().take(hang_chars).collect();
    let body: String = text.chars().skip(hang_chars).collect();
    // Available columns for the item text (after kind prefix + checkbox).
    let content_w = (width as usize)
        .saturating_sub(prefix_cols + hang_disp)
        .max(1);
    // Continuation pad = display columns of prefix + checkbox hang (so R ‖ c).
    let cont_prefix = " ".repeat(prefix_cols + hang_disp);
    let chunks = wrap_text_chunks(&body, content_w);

    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            if i == 0 {
                Line::from(vec![
                    Span::styled(prefix.to_string(), style),
                    Span::styled(hang_prefix.clone(), style),
                    Span::styled(chunk, style),
                ])
            } else {
                Line::from(vec![
                    Span::styled(cont_prefix.clone(), style),
                    Span::styled(chunk, style),
                ])
            }
        })
        .collect()
}

fn plain_assistant_lines(md: &str, width: u16, with_rule: bool) -> Vec<Line<'static>> {
    let width = (width as usize).max(20);
    let mut out = Vec::new();
    if with_rule {
        out.push(Line::from(""));
        out.push(horizontal_rule_line(width as u16));
        out.push(Line::from(""));
    }
    let stripped = strip_md_light(md);
    for para in stripped.split('\n') {
        if para.is_empty() {
            out.push(Line::from(""));
            continue;
        }
        let mut rest = para;
        while !rest.is_empty() {
            let take = rest
                .char_indices()
                .nth(width)
                .map(|(i, _)| i)
                .unwrap_or(rest.len());
            let split = if take < rest.len() {
                rest[..take]
                    .rfind(char::is_whitespace)
                    .filter(|&i| i > 0)
                    .unwrap_or(take)
            } else {
                take
            };
            let (chunk, next) = rest.split_at(split);
            out.push(Line::from(Span::styled(
                chunk.trim_end().to_string(),
                Style::default().fg(Color::White),
            )));
            rest = next.trim_start();
        }
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

fn strip_md_light(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '_' | '`' => {
                while chars.peek() == Some(&c) {
                    chars.next();
                }
            }
            '#' if out.is_empty() || out.ends_with('\n') => {
                while matches!(chars.peek(), Some('#') | Some(' ')) {
                    if chars.peek() == Some(&' ') {
                        chars.next();
                        break;
                    }
                    chars.next();
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn rich_assistant_lines(md: &str, width: u16, with_rule: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if with_rule {
        lines.push(Line::from(""));
        lines.push(horizontal_rule_line(width));
        lines.push(Line::from(""));
    }
    lines.extend(super::markdown::markdown_to_lines(md, width));
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

// ─── Cache sync / enrich / collect ──────────────────────────────────────────

fn sync_heights(cache: &mut TranscriptCache, view: &TurnView, width: u16) {
    if cache.width != width {
        cache.rich.clear();
        cache.measured.clear();
        cache.width = width;
        cache.revision = u64::MAX;
    }

    let n = view.transcript.len();
    cache.heights.resize(n, 1);

    let mut prev_kind: Option<TranscriptKind> = None;
    for (i, row) in view.transcript.iter().enumerate() {
        // Prefer measured rich height when we already rendered this row.
        let h = if let Some(rich) = cache.rich.get(&i) {
            if rich.text_len == row.text.len() && row.kind == TranscriptKind::Assistant {
                let h = rich.lines.len().max(1);
                cache.measured.insert(i, (row.text.len(), h));
                h
            } else {
                // Source changed (streaming) — drop stale rich + measured.
                cache.rich.remove(&i);
                cache.measured.remove(&i);
                estimate_row_height(row, width, prev_kind)
            }
        } else if row.kind == TranscriptKind::Assistant {
            // Keep last rich measurement after eviction (scroll away) so total
            // height does not collapse mid-session.
            if let Some(&(len, h)) = cache.measured.get(&i) {
                if len == row.text.len() {
                    h
                } else {
                    cache.measured.remove(&i);
                    estimate_row_height(row, width, prev_kind)
                }
            } else {
                estimate_row_height(row, width, prev_kind)
            }
        } else {
            estimate_row_height(row, width, prev_kind)
        };
        cache.heights[i] = h;
        prev_kind = Some(row.kind);
    }

    // Drop entries that no longer exist.
    cache.rich.retain(|&i, _| i < n);
    cache.measured.retain(|&i, _| i < n);
    cache.revision = view.transcript_gen;
    cache.recompute_offsets();
}

fn prev_kind_at(view: &TurnView, idx: usize) -> Option<TranscriptKind> {
    idx.checked_sub(1)
        .map(|i| view.transcript[i].kind)
}

fn leading_rule_at(view: &TurnView, idx: usize) -> bool {
    needs_leading_rule(view.transcript[idx].kind, prev_kind_at(view, idx))
}

fn lazy_enrich(cache: &mut TranscriptCache, view: &TurnView, width: u16, start: usize, end: usize) {
    if view.transcript.is_empty() {
        return;
    }
    let over_start = start.saturating_sub(RICH_OVERSCAN);
    let over_end = (end + RICH_OVERSCAN).min(cache.total_lines.max(end));

    // Which transcript indices intersect the overscan window?
    let mut needed: Vec<usize> = Vec::new();
    for i in 0..view.transcript.len() {
        if view.transcript[i].kind != TranscriptKind::Assistant {
            continue;
        }
        let rs = cache.offsets[i];
        let re = cache.offsets[i + 1];
        if re > over_start && rs < over_end {
            needed.push(i);
        }
    }

    // Cap how many we keep rich — prefer closest to viewport centre / bottom.
    if needed.len() > MAX_RICH_ROWS {
        // Keep the last MAX_RICH_ROWS (toward latest when at bottom).
        needed = needed.split_off(needed.len() - MAX_RICH_ROWS);
    }

    let needed_set: std::collections::HashSet<usize> = needed.iter().copied().collect();
    cache.rich.retain(|i, _| needed_set.contains(i));

    let mut changed = false;
    for &i in &needed {
        let row = &view.transcript[i];
        let stale = cache
            .rich
            .get(&i)
            .map(|r| r.text_len != row.text.len())
            .unwrap_or(true);
        if !stale {
            continue;
        }
        let lines = rich_assistant_lines(&row.text, width, leading_rule_at(view, i));
        cache.heights[i] = lines.len().max(1);
        cache.rich.insert(
            i,
            RichRow {
                text_len: row.text.len(),
                lines,
            },
        );
        changed = true;
    }
    if changed {
        cache.recompute_offsets();
    }
}

fn collect_visible(
    cache: &TranscriptCache,
    view: &TurnView,
    width: u16,
    start: usize,
    end: usize,
) -> Vec<Line<'static>> {
    if start >= end || view.transcript.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(end.saturating_sub(start));
    for i in 0..view.transcript.len() {
        let row_start = cache.offsets[i];
        let row_end = cache.offsets[i + 1];
        if row_end <= start {
            continue;
        }
        if row_start >= end {
            break;
        }
        let row = &view.transcript[i];
        // Borrow rich if present; else build plain only for this visible slice.
        let owned: Vec<Line<'static>>;
        let lines: &[Line<'static>] = if let Some(rich) = cache.rich.get(&i) {
            &rich.lines
        } else if row.kind == TranscriptKind::Assistant {
            owned = plain_assistant_lines(&row.text, width, leading_rule_at(view, i));
            // Can't return ref to local — extend directly below.
            let local_start = start.saturating_sub(row_start);
            let local_end = (end.saturating_sub(row_start)).min(owned.len());
            if local_start < local_end {
                out.extend(owned[local_start..local_end].iter().cloned());
            }
            continue;
        } else if row.kind == TranscriptKind::User {
            owned = user_prompt_lines(&row.text, width, leading_rule_at(view, i));
            let local_start = start.saturating_sub(row_start);
            let local_end = (end.saturating_sub(row_start)).min(owned.len());
            if local_start < local_end {
                out.extend(owned[local_start..local_end].iter().cloned());
            }
            continue;
        } else {
            owned = hanging_wrap_lines(row.kind, &row.text, width);
            &owned
        };

        let local_start = start.saturating_sub(row_start);
        let local_end = (end.saturating_sub(row_start)).min(lines.len());
        if local_start < local_end {
            out.extend(lines[local_start..local_end].iter().cloned());
        }
    }
    out
}

/// Build click hit-rects for truncated user prompts currently on screen.
fn collect_user_prompt_hits(
    view: &TurnView,
    cache: &TranscriptCache,
    width: u16,
    abs_start: usize,
    body: Rect,
) -> Vec<UserPromptHit> {
    if cache.offsets.len() < 2 || view.transcript.is_empty() || body.height == 0 {
        return Vec::new();
    }
    let abs_end = abs_start.saturating_add(body.height as usize);
    let mut hits = Vec::new();
    for i in 0..view.transcript.len() {
        let row = &view.transcript[i];
        if row.kind != TranscriptKind::User {
            continue;
        }
        if !user_prompt_is_truncated(&row.text, width) {
            continue;
        }
        let row_start = cache.offsets.get(i).copied().unwrap_or(0);
        let row_end = cache.offsets.get(i + 1).copied().unwrap_or(row_start);
        // Intersection of this transcript row with the visible absolute window.
        let vis_start = row_start.max(abs_start);
        let vis_end = row_end.min(abs_end);
        if vis_start >= vis_end {
            continue;
        }
        let screen_y0 = body.y.saturating_add((vis_start - abs_start) as u16);
        let screen_h = (vis_end - vis_start) as u16;
        if screen_h == 0 {
            continue;
        }
        hits.push(UserPromptHit {
            area: HitRect {
                x: body.x,
                y: screen_y0,
                width: body.width.max(1),
                height: screen_h,
            },
            full_text: row.text.clone(),
        });
    }
    hits
}

/// Prewarm only the visible tail — no full-history clone needed by the caller
/// if they pass a view locked on the draw thread. Safe to call after resume.
pub fn prewarm_transcript_cache(view: &TurnView, width: u16) {
    let width = width.max(20);
    TRANSCRIPT_CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        sync_heights(&mut cache, view, width);
        let total = cache.total_lines;
        let start = total.saturating_sub(40 + RICH_OVERSCAN);
        lazy_enrich(&mut cache, view, width, start, total);
    });
}

fn draw_transcript(frame: &mut Frame, area: Rect, view: &mut TurnView) {
    let width = area.width.max(20);
    let viewport = area.height as usize;

    let (visible, scrolled, abs_start) = TRANSCRIPT_CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        sync_heights(&mut cache, view, width);

        // Absolute viewport (`follow_bottom` / `viewport_end`) does not need
        // preserve_scroll_anchor — growth at the bottom never moves the window
        // unless the user is following.
        let content_rows_guess = viewport.saturating_sub(1).max(1);
        view.last_content_lines = cache.total_lines;
        view.last_viewport_rows = content_rows_guess;
        view.clamp_scroll();

        let scrolled0 = !view.follow_bottom && cache.total_lines > content_rows_guess;
        let content_rows = if scrolled0 {
            viewport.saturating_sub(1)
        } else {
            viewport
        };
        view.last_viewport_rows = content_rows.max(1);
        view.clamp_scroll();

        let (start, end) = view.visible_range(cache.total_lines, content_rows);
        lazy_enrich(&mut cache, view, width, start, end);

        // Heights may grow after enrich; clamp end only, never re-follow.
        view.last_content_lines = cache.total_lines;
        view.clamp_scroll();
        let (start, end) = view.visible_range(cache.total_lines, content_rows);
        let scrolled = !view.follow_bottom && cache.total_lines > content_rows;

        let visible = collect_visible(&cache, view, width, start, end);
        (visible, scrolled, start)
    });

    let body_area = if scrolled {
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: area.height.saturating_sub(1),
        }
    } else {
        area
    };

    // Click targets for truncated › prompts currently on screen.
    view.user_prompt_hits = TRANSCRIPT_CACHE.with(|cell| {
        let cache = cell.borrow();
        collect_user_prompt_hits(view, &cache, width, abs_start, body_area)
    });

    if visible.is_empty() {
        let hint = if view.status_line.is_some() {
            "Preparing transcript…"
        } else if view.busy {
            "Waiting for the model…"
        } else {
            "Type a message below and press Enter"
        };
        view.selectable = SelectablePaint {
            area: HitRect {
                x: body_area.x,
                y: body_area.y,
                width: body_area.width,
                height: body_area.height,
            },
            rows: vec![hint.to_string()],
        };
        frame.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
            body_area,
        );
    } else {
        // Store plain rows for drag-select copy; paint with selection highlight.
        let plain: Vec<String> = visible.iter().map(line_plain_text).collect();
        view.selectable = SelectablePaint {
            area: HitRect {
                x: body_area.x,
                y: body_area.y,
                width: body_area.width,
                height: body_area.height,
            },
            rows: plain,
        };
        let painted = apply_selection_to_lines(&visible, body_area, view.selection.as_ref());
        // Lines are already hard-wrapped with hanging indent for tool/user rows.
        frame.render_widget(Paragraph::new(painted), body_area);
    }

    if scrolled && area.height > 0 {
        let bar_y = area.y.saturating_add(area.height.saturating_sub(1));
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" ".repeat(
                    (area.width.saturating_sub(JUMP_BOTTOM_LABEL.chars().count() as u16) / 2)
                        as usize,
                )),
                Span::styled(
                    JUMP_BOTTOM_LABEL.to_string(),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            Rect {
                x: area.x,
                y: bar_y,
                width: area.width,
                height: 1,
            },
        );
        view.jump_bottom_hit = Some(HitRect {
            x: area.x,
            y: bar_y,
            width: area.width.max(1),
            height: 1,
        });
    } else {
        view.jump_bottom_hit = None;
    }
}

fn draw_activity(frame: &mut Frame, area: Rect, view: &TurnView, tick: u64) {
    let frame_ch = SPINNER[(tick as usize) % SPINNER.len()];
    let mut lines: Vec<Line> = Vec::new();

    if let Some(status) = view.status_line.as_deref() {
        match view.status_tone {
            // After interrupt: bright red "Interrupted", no spinner, blank-flanked.
            StatusTone::Interrupted => {
                lines.push(Line::from(""));
                lines.push(interrupted_status_line(status));
                lines.push(Line::from(""));
                frame.render_widget(Paragraph::new(lines), area);
                return;
            }
            // First empty Enter after interrupt — dim, no spinner, blank-flanked.
            StatusTone::Hint => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    status.to_string(),
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(""));
                frame.render_widget(Paragraph::new(lines), area);
                return;
            }
            // Spinner only for Interrupting… (red) and Loading… (cyan).
            StatusTone::Interrupting => {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{frame_ch} "),
                        Style::default().fg(Color::Rgb(255, 85, 85)),
                    ),
                    Span::styled(
                        status.to_string(),
                        Style::default().fg(Color::Rgb(255, 85, 85)),
                    ),
                ]));
                frame.render_widget(Paragraph::new(lines), area);
                return;
            }
            StatusTone::Loading => {
                lines.push(Line::from(vec![
                    Span::styled(format!("{frame_ch} "), Style::default().fg(Color::Cyan)),
                    Span::styled(status.to_string(), Style::default().fg(Color::DarkGray)),
                ]));
                frame.render_widget(Paragraph::new(lines), area);
                return;
            }
        }
    }

    match &view.activity {
        ActivityPhase::Idle => {
            if view.busy {
                lines.push(Line::from(Span::styled(
                    format!("{frame_ch} Working…"),
                    Style::default().fg(Color::Cyan),
                )));
            }
        }
        ActivityPhase::Thinking { started, buffer } => {
            let secs = started.elapsed().as_secs_f64();
            lines.push(Line::from(vec![
                Span::styled(format!("{frame_ch} "), Style::default().fg(Color::Cyan)),
                Span::styled("Thinking", Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!(" · {secs:.1}s"),
                    Style::default().fg(Color::White),
                ),
            ]));
            for preview in TurnView::last_preview_lines(buffer, 2) {
                let trunc: String = preview
                    .chars()
                    .take(area.width.saturating_sub(4) as usize)
                    .collect();
                lines.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(trunc, Style::default().fg(Color::DarkGray)),
                ]));
            }
        }
        ActivityPhase::Tool {
            started,
            label,
            output,
            is_error,
        } => {
            let secs = started.elapsed().as_secs_f64();
            let spin = if *is_error { Color::Red } else { Color::Cyan };
            lines.push(Line::from(vec![
                Span::styled(format!("{frame_ch} "), Style::default().fg(spin)),
                Span::styled(label.as_str(), Style::default().fg(Color::White)),
                Span::styled(
                    format!(" · {secs:.1}s"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
            for preview in TurnView::last_preview_lines(output, 2) {
                let trunc: String = preview
                    .chars()
                    .take(area.width.saturating_sub(4) as usize)
                    .collect();
                lines.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(trunc, Style::default().fg(Color::White)),
                ]));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn composer_row_count(buffer: &str, term_width: u16) -> u16 {
    let w = composer_content_width(term_width);
    let n = soft_wrap_ranges(buffer, w).len().max(1);
    (n as u16).clamp(1, COMPOSER_MAX_ROWS)
}

/// Style composer text: image atoms magenta, selection reverse-video.
///
/// `# Arguments`
/// * `chunk` — substring of the buffer for one visual row
/// * `abs_start` — byte offset of `chunk` in the full buffer
/// * `sel` — optional ordered half-open selection range in buffer bytes
fn composer_content_spans(
    chunk: &str,
    abs_start: usize,
    sel: Option<(usize, usize)>,
) -> Vec<Span<'static>> {
    let white = Style::default().fg(Color::White);
    let image = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let selected = Style::default()
        .fg(Color::Black)
        .bg(Color::White)
        .add_modifier(Modifier::BOLD);
    let selected_image = Style::default()
        .fg(Color::Magenta)
        .bg(Color::White)
        .add_modifier(Modifier::BOLD);

    if chunk.is_empty() {
        return vec![Span::styled(String::new(), white)];
    }

    // Collect split points: image-atom edges + selection edges inside this chunk.
    let abs_end = abs_start + chunk.len();
    let mut cuts: Vec<usize> = vec![0, chunk.len()];
    for &(s, e) in &crate::paste::image_atom_ranges(chunk) {
        cuts.push(s);
        cuts.push(e);
    }
    if let Some((ss, se)) = sel {
        if ss > abs_start && ss < abs_end {
            cuts.push(ss - abs_start);
        }
        if se > abs_start && se < abs_end {
            cuts.push(se - abs_start);
        }
        // Selection covering the whole chunk still needs styling via mid checks.
    }
    cuts.sort_unstable();
    cuts.dedup();

    let image_ranges = crate::paste::image_atom_ranges(chunk);
    let mut spans = Vec::new();
    for w in cuts.windows(2) {
        let a = w[0];
        let b = w[1];
        if a >= b {
            continue;
        }
        let piece = &chunk[a..b];
        let mid = abs_start + a;
        let in_sel = sel.is_some_and(|(ss, se)| mid >= ss && mid < se);
        let in_image = image_ranges
            .iter()
            .any(|&(s, e)| a >= s && a < e);
        let style = match (in_sel, in_image) {
            (true, true) => selected_image,
            (true, false) => selected,
            (false, true) => image,
            (false, false) => white,
        };
        spans.push(Span::styled(piece.to_string(), style));
    }
    if spans.is_empty() {
        spans.push(Span::styled(chunk.to_string(), white));
    }
    spans
}

fn draw_composer(frame: &mut Frame, area: Rect, view: &mut TurnView) {
    if area.height == 0 || area.width == 0 {
        view.composer_hit = None;
        return;
    }
    // Hit region for mouse drag-select in the textbox.
    view.composer_hit = Some(HitRect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height,
    });

    let content_w = composer_content_width(area.width);
    let ranges = soft_wrap_ranges(&view.composer.buffer, content_w);
    let max_rows = area.height as usize;
    let total = ranges.len().max(1);
    let first = total.saturating_sub(max_rows);
    view.composer_paint_first = first;
    let sel = view.composer.selection_range();

    let green = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let white = Style::default().fg(Color::White);

    let mut lines: Vec<Line> = Vec::with_capacity(max_rows);
    for (i, &(start, end)) in ranges.iter().enumerate().skip(first).take(max_rows) {
        let end = end.min(view.composer.buffer.len());
        let chunk = &view.composer.buffer[start..end];
        let prefix = if i == 0 { "› " } else { "  " };
        let prefix_style = if i == 0 { green } else { white };
        let mut spans = vec![Span::styled(prefix.to_string(), prefix_style)];
        spans.extend(composer_content_spans(chunk, start, sel));
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("› ".to_string(), green),
            Span::styled(String::new(), white),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);

    let cursor_byte = view.composer.cursor.min(view.composer.buffer.len());
    let (full_row, col) = composer_cursor_cell(&view.composer.buffer, cursor_byte, content_w);
    let vis_row = full_row.saturating_sub(first);
    if vis_row < max_rows {
        let x = area
            .x
            .saturating_add(COMPOSER_PREFIX_COLS as u16)
            .saturating_add(col as u16)
            .min(area.x.saturating_add(area.width.saturating_sub(1)));
        let y = area.y.saturating_add(vis_row as u16);
        frame.set_cursor_position(Position { x, y });
    }
}

/// Footer chip: `Key:action` pairs separated by ` | ` (Claude/Codex-style).
/// Shortcut keys are bright; colon + description stay dim.
fn footer_line(pairs: &[(&str, &str)]) -> Line<'static> {
    let key_style = Style::default().fg(Color::White);
    let action_style = Style::default().fg(Color::DarkGray);
    let sep_style = Style::default().fg(Color::DarkGray);
    let mut spans: Vec<Span> = Vec::with_capacity(pairs.len() * 5);
    for (i, (key, action)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" | ".to_string(), sep_style));
        }
        spans.push(Span::styled(key.to_string(), key_style));
        spans.push(Span::styled(format!(":{action}"), action_style));
    }
    Line::from(spans)
}

fn draw_footer(frame: &mut Frame, area: Rect, view: &TurnView) {
    // Short key:action chips — brighter gray than the old DarkGray prose.
    let line = if !view.is_at_bottom() {
        footer_line(&[
            ("End", "latest"),
            ("Esc ×2", "undo"),
            ("Ctrl+C", "quit"),
        ])
    } else if view.busy {
        footer_line(&[
            ("Enter", "queue"),
            ("Esc", "cancel"),
            ("Ctrl+C", "quit"),
        ])
    } else {
        footer_line(&[
            ("Enter", "send"),
            ("⌥↩", "newline"),
            ("Esc ×2", "undo"),
            ("Ctrl+C", "quit"),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::view::TranscriptKind;

    fn line_plain(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn tool_wrap_uses_hanging_indent() {
        // Long tool label like the one in the screenshot.
        let text = "Search for 'pub struct Console|impl Console|PromptOutcome|fn prompt\\(' in rust files at crates/furcode_main/src · 0.1s";
        let width = 60u16;
        let lines = hanging_wrap_lines(TranscriptKind::ToolDone, text, width);
        assert!(lines.len() >= 2, "expected wrap, got {}", lines.len());

        let first = line_plain(&lines[0]);
        let second = line_plain(&lines[1]);
        assert!(first.starts_with("· "), "first={first:?}");
        // Continuation aligns under the body (after "· "), not under column 0.
        assert!(
            second.starts_with("  ") && !second.starts_with("· "),
            "second should hang-indent, got {second:?}"
        );
        // Body of continuation starts at same column as first body char.
        let indent = "· ".chars().count();
        assert_eq!(
            second.chars().take(indent).all(|c| c == ' '),
            true,
            "indent must be {indent} spaces"
        );
        // Each rendered line fits the terminal width.
        for (i, l) in lines.iter().enumerate() {
            let n = line_plain(l).chars().count();
            assert!(n <= width as usize, "line {i} too long: {n} > {width}");
        }
    }

    #[test]
    fn short_tool_stays_one_line() {
        let lines = hanging_wrap_lines(TranscriptKind::ToolDone, "Read main.rs · 0.1s", 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(line_plain(&lines[0]), "· Read main.rs · 0.1s");
    }

    #[test]
    fn composer_soft_wraps_long_input() {
        let text = "a".repeat(50);
        let ranges = soft_wrap_ranges(&text, 20);
        assert!(ranges.len() >= 3, "expected wrap, got {}", ranges.len());
        assert_eq!(composer_row_count(&text, 22), 3.min(COMPOSER_MAX_ROWS as usize) as u16);
        // Cursor at end sits on last visual row.
        let (row, col) = composer_cursor_cell(&text, text.len(), 20);
        assert_eq!(row, ranges.len() - 1);
        assert_eq!(col, text.len() % 20);
    }

    #[test]
    fn composer_empty_is_one_row() {
        assert_eq!(composer_row_count("", 80), 1);
        let (row, col) = composer_cursor_cell("", 0, 40);
        assert_eq!((row, col), (0, 0));
    }

    #[test]
    fn height_matches_hanging_wrap() {
        let row = TranscriptLine {
            kind: TranscriptKind::ToolDone,
            text: "Search for 'async fn handle_dispatch(' at crates/furcode_main/src/ui.rs · 0.1s"
                .into(),
        };
        let width = 48u16;
        let h = estimate_row_height(&row, width, None);
        let lines = hanging_wrap_lines(row.kind, &row.text, width);
        assert_eq!(h, lines.len());
    }

    #[test]
    fn todo_body_wrap_hangs_under_item_text() {
        // Same shape as format_todo_line: leading spaces + icon + space + content.
        let text =
            "  󰄱 Rename agent IDs from forge/muse/sage to coding/planning/researching";
        let width = 48u16;
        let lines = hanging_wrap_lines(TranscriptKind::ToolBody, text, width);
        assert!(
            lines.len() >= 2,
            "expected wrap for long todo, got {}",
            lines.len()
        );

        let first = line_plain(&lines[0]);
        let second = line_plain(&lines[1]);

        let (hang_chars, hang_disp) = list_marker_hang(TranscriptKind::ToolBody, text);
        assert_eq!(hang_chars, 4, "  + icon + space (chars)");
        // PUA nerd icon counts as 2 display cols → hang display 2+2+1 = 5.
        assert_eq!(hang_disp, 5, "  + icon(2) + space");
        // No extra ToolBody kind prefix when a list marker is present.
        let content_col = hang_disp; // R and c both start here

        // Continuation is pure spaces then body text — no checkbox re-print.
        assert!(
            !second.contains('󰄱'),
            "continuation must not repeat checkbox: {second:?}"
        );
        let cont_spaces = second.chars().take_while(|c| *c == ' ').count();
        assert_eq!(
            cont_spaces, content_col,
            "continuation must align under item text (col {content_col}), got {cont_spaces}; first={first:?} second={second:?}"
        );

        // Display column of R on line 1 equals display column of c on line 2.
        let r_disp = display_cols(
            &first
                .chars()
                .take_while(|c| *c != 'R')
                .collect::<String>(),
        );
        let c_disp = cont_spaces;
        assert_eq!(
            r_disp, c_disp,
            "R (disp {r_disp}) must align with c (disp {c_disp}); first={first:?} second={second:?}"
        );
        assert!(
            first.contains('R') && second.trim_start().starts_with('c'),
            "first={first:?} second={second:?}"
        );
        // Expected shape:
        // "  󰄱 Rename … to"
        // "     coding/…"   (5 spaces — under R, not under checkbox)
        assert!(
            first.starts_with("  󰄱 "),
            "first line should be indent+icon only once: {first:?}"
        );

        // Height estimator must match paint.
        let row = TranscriptLine {
            kind: TranscriptKind::ToolBody,
            text: text.into(),
        };
        assert_eq!(estimate_row_height(&row, width, None), lines.len());
    }

    #[test]
    fn detects_diff_tool_body_lines() {
        assert!(is_diff_tool_body_line("  3 │ line 3"));
        assert!(is_diff_tool_body_line("- 5 │ line 5"));
        assert!(is_diff_tool_body_line("+ 5 │ modified line 5"));
        assert!(is_diff_tool_body_line("···"));
        assert!(is_diff_tool_body_line("No changes applied"));
        // Todos / prose must not look like diffs.
        assert!(!is_diff_tool_body_line(
            "  󰄱 Rename agent IDs from forge/muse/sage"
        ));
        assert!(!is_diff_tool_body_line("hello world"));
    }

    #[test]
    fn user_prompt_truncates_to_two_lines_plus_ellipsis() {
        let long = "line one\nline two\nline three\nline four\nline five";
        let (chunks, truncated) = user_prompt_wrap_chunks(long, 80);
        assert!(truncated);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], "line one");
        assert_eq!(chunks[1], "line two");
        assert_eq!(chunks[2], "…");
    }

    #[test]
    fn user_prompt_keeps_three_or_fewer_lines() {
        let short = "a\nb\nc";
        let (chunks, truncated) = user_prompt_wrap_chunks(short, 80);
        assert!(!truncated);
        assert_eq!(chunks, vec!["a", "b", "c"]);
    }

    #[test]
    fn user_prompt_height_matches_paint() {
        let text = "one\ntwo\nthree\nfour";
        let width = 40u16;
        let lines = user_prompt_lines(text, width, false);
        let row = TranscriptLine {
            kind: TranscriptKind::User,
            text: text.into(),
        };
        assert_eq!(estimate_row_height(&row, width, None), lines.len());
        assert_eq!(lines.len(), 3); // 2 body + ellipsis
    }

    #[test]
    fn diff_body_renders_without_extra_indent() {
        let text = "+ 2 │ modified line";
        let lines = hanging_wrap_lines(TranscriptKind::ToolBody, text, 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(line_plain(&lines[0]), text);
    }

    #[test]
    fn user_after_turn_complete_gets_leading_rule() {
        assert!(needs_leading_rule(
            TranscriptKind::User,
            Some(TranscriptKind::Info)
        ));
        assert!(!needs_leading_rule(
            TranscriptKind::User,
            Some(TranscriptKind::Assistant)
        ));
        assert!(!needs_leading_rule(TranscriptKind::User, None));

        let lines = user_prompt_lines("next prompt", 40, true);
        assert!(lines.len() >= 4, "blank + rule + blank + prompt");
        assert_eq!(line_plain(&lines[0]), "");
        assert!(
            line_plain(&lines[1]).chars().all(|c| c == '─'),
            "expected hr, got {:?}",
            line_plain(&lines[1])
        );
        assert_eq!(line_plain(&lines[2]), "");
        assert!(line_plain(&lines[3]).contains("next prompt"));

        let row = TranscriptLine {
            kind: TranscriptKind::User,
            text: "next prompt".into(),
        };
        let h = estimate_row_height(&row, 40, Some(TranscriptKind::Info));
        assert_eq!(h, lines.len());
    }
}
