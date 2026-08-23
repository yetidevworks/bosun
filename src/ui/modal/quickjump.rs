//! QuickJumpModal — fast type-ahead session switcher.
//!
//! Opened from the main session list with `/`. Shows every live
//! bosun-managed session with a live filter. Type to narrow, ↑/↓ to
//! highlight, Enter to attach, Esc to cancel.
//!
//! On Enter the modal returns `Close(Some(Command::Attach { name }))`;
//! the app loop intercepts `Command::Attach` from a closed modal and
//! sets `pending_attach` instead of forwarding it to the tmux actor
//! (the actor doesn't handle attach — the app loop does the
//! tty-handover dance inline).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::events::Command;
use crate::ui::Theme;

use super::{center_rect, Modal, ModalResult};

const MODAL_WIDTH: u16 = 70;
const MODAL_MIN_HEIGHT: u16 = 10;
const MODAL_MAX_HEIGHT: u16 = 30;
/// Title + blank + filter + blank + 2-row inset = 6 chrome rows.
const MODAL_CHROME_ROWS: u16 = 6;

/// One row in the picker. Carries both the internal tmux name (what
/// we attach to) and the human-readable bits the user sees and filters
/// on.
#[derive(Debug, Clone)]
pub struct QuickJumpRow {
    pub internal: String,
    pub display: String,
    pub agent: Option<String>,
    pub path: Option<String>,
    pub attached: bool,
}

pub struct QuickJumpModal {
    rows: Vec<QuickJumpRow>,
    filter: String,
    /// Index into the current filtered view. Clamped on every filter
    /// change / nav key. 0 (top-most match) on open and on every
    /// keystroke that grows the filter.
    selected: usize,
}

impl QuickJumpModal {
    pub fn new(rows: Vec<QuickJumpRow>) -> Self {
        Self {
            rows,
            filter: String::new(),
            selected: 0,
        }
    }

    /// Indices into `self.rows` that pass the current filter, in the
    /// same order as `rows`. Empty filter ⇒ everyone.
    fn filtered_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.rows.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| row_matches(r, &needle))
            .map(|(i, _)| i)
            .collect()
    }

    fn selected_row(&self) -> Option<&QuickJumpRow> {
        let indices = self.filtered_indices();
        indices.get(self.selected).and_then(|i| self.rows.get(*i))
    }

    fn clamp_selection(&mut self) {
        let len = self.filtered_indices().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn move_down(&mut self) {
        let len = self.filtered_indices().len();
        if len > 0 && self.selected + 1 < len {
            self.selected += 1;
        }
    }
}

impl Modal for QuickJumpModal {
    fn id(&self) -> &'static str {
        "quickjump"
    }

    fn handle(&mut self, key: KeyEvent) -> ModalResult {
        // Ctrl+C always closes — matches other modals.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalResult::Close(None);
        }

        match key.code {
            KeyCode::Esc => ModalResult::Close(None),
            KeyCode::Enter => match self.selected_row() {
                Some(r) => ModalResult::Close(Some(Command::Attach {
                    name: r.internal.clone(),
                })),
                None => ModalResult::Consumed,
            },
            KeyCode::Up => {
                self.move_up();
                ModalResult::Consumed
            }
            KeyCode::Down => {
                self.move_down();
                ModalResult::Consumed
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.clamp_selection();
                ModalResult::Consumed
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.filter.push(c);
                // Always jump to the top-most match on filter change —
                // matches fzf/skim behavior and is what users expect.
                self.selected = 0;
                ModalResult::Consumed
            }
            _ => ModalResult::Consumed,
        }
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let desired = (self.rows.len() as u16).saturating_add(MODAL_CHROME_ROWS);
        let height = desired
            .clamp(MODAL_MIN_HEIGHT, MODAL_MAX_HEIGHT)
            .min(area.height.saturating_sub(2).max(MODAL_MIN_HEIGHT));
        let rect = center_rect(area, MODAL_WIDTH, height);
        let body_bg = theme.panel_alt;
        let buf = frame.buffer_mut();

        // Shadow behind the modal — same pattern as RecentsModal.
        if rect.x + rect.width < area.x + area.width && rect.y + rect.height < area.y + area.height
        {
            let shadow = Rect::new(rect.x + 1, rect.y + 1, rect.width, rect.height);
            let style = Style::default().bg(theme.shadow);
            crate::ui::paint::tint(buf, shadow, style);
        }

        // Body fill.
        let body_style = Style::default().bg(body_bg);
        crate::ui::paint::fill_opaque(buf, rect, body_style);

        // Left accent bar.
        let accent_style = Style::default().bg(theme.accent);
        crate::ui::paint::fill_opaque(buf, crate::ui::paint::left_edge(rect), accent_style);

        let inner = Rect::new(
            rect.x + 3,
            rect.y + 1,
            rect.width.saturating_sub(4),
            rect.height.saturating_sub(2),
        );

        let filtered = self.filtered_indices();

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(16);
        lines.push(Line::from(vec![
            Span::styled(
                "Quick switch",
                Style::default()
                    .fg(theme.text)
                    .bg(body_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "   esc · ↑↓ select · ↵ attach",
                Style::default().fg(theme.text_muted).bg(body_bg),
            ),
        ]));
        lines.push(Line::from(""));

        let filter_display = format!(" filter: {}▎", self.filter);
        lines.push(Line::from(vec![Span::styled(
            filter_display,
            Style::default().fg(theme.text).bg(theme.bg),
        )]));
        lines.push(Line::from(""));

        let max_rows = (inner.height as usize).saturating_sub(4);
        if self.rows.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no sessions to switch to)",
                Style::default().fg(theme.text_muted).bg(body_bg),
            )));
        } else if filtered.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no matches)",
                Style::default().fg(theme.text_muted).bg(body_bg),
            )));
        } else {
            for (vi, row_idx) in filtered.iter().enumerate().take(max_rows) {
                let r = &self.rows[*row_idx];
                lines.push(render_row(r, vi == self.selected, inner.width, theme));
            }
        }

        Paragraph::new(lines)
            .style(Style::default().bg(body_bg))
            .render(inner, frame.buffer_mut());
    }
}

/// Case-insensitive substring match across the bits a user is likely
/// to type — display name first, then agent, then path. Matches
/// RecentsModal's matching contract so the two pickers feel
/// identical.
fn row_matches(r: &QuickJumpRow, needle: &str) -> bool {
    if r.display.to_lowercase().contains(needle) {
        return true;
    }
    if let Some(a) = &r.agent {
        if a.to_lowercase().contains(needle) {
            return true;
        }
    }
    if let Some(p) = &r.path {
        if p.to_lowercase().contains(needle) {
            return true;
        }
    }
    false
}

fn render_row(r: &QuickJumpRow, selected: bool, width: u16, theme: &Theme) -> Line<'static> {
    let marker = if selected { "▸" } else { " " };
    let row_bg = if selected {
        theme.selection_bg
    } else {
        theme.panel_alt
    };

    let marker_style = if selected {
        Style::default().fg(theme.accent).bg(row_bg)
    } else {
        Style::default().fg(row_bg).bg(row_bg)
    };
    let name_style = if selected {
        Style::default()
            .fg(theme.text)
            .bg(row_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text).bg(row_bg)
    };
    let meta_style = Style::default().fg(theme.text_muted).bg(row_bg);
    let attached_style = Style::default().fg(theme.status_running).bg(row_bg);

    let path_short = r.path.as_deref().map(shorten_path).unwrap_or_default();
    let agent = r.agent.as_deref().unwrap_or("");
    let attached_label = if r.attached { "  •attached" } else { "" };

    let mut spans = vec![
        Span::styled(format!(" {} ", marker), marker_style),
        Span::styled(r.display.clone(), name_style),
    ];
    if !agent.is_empty() {
        spans.push(Span::styled(format!("  · {}", agent), meta_style));
    }
    if !path_short.is_empty() {
        spans.push(Span::styled(format!("  · {}", path_short), meta_style));
    }
    if !attached_label.is_empty() {
        spans.push(Span::styled(attached_label, attached_style));
    }

    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = (width as usize).saturating_sub(used);
    spans.push(Span::styled(" ".repeat(pad), Style::default().bg(row_bg)));

    Line::from(spans)
}

fn shorten_path(p: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && p.starts_with(&home) {
        format!("~{}", &p[home.len()..])
    } else {
        p.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(internal: &str, display: &str, agent: &str, path: &str) -> QuickJumpRow {
        QuickJumpRow {
            internal: internal.into(),
            display: display.into(),
            agent: Some(agent.into()),
            path: Some(path.into()),
            attached: false,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn empty_list_enter_is_noop() {
        let mut m = QuickJumpModal::new(vec![]);
        assert!(m.selected_row().is_none());
        assert!(matches!(
            m.handle(key(KeyCode::Enter)),
            ModalResult::Consumed
        ));
    }

    #[test]
    fn typing_narrows_filter_across_display_agent_path() {
        let mut m = QuickJumpModal::new(vec![
            row("bosun-work-1", "Work", "claude", "/tmp/proj"),
            row("bosun-play-1", "Play", "codex", "/tmp/play"),
            row("bosun-api-1", "API", "claude", "/srv/api"),
        ]);
        // "cod" matches only the codex entry by agent.
        for c in "cod".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(m.filtered_indices().len(), 1);
        assert_eq!(m.selected_row().unwrap().internal, "bosun-play-1");

        // Backspace widens.
        for _ in 0..3 {
            m.handle(key(KeyCode::Backspace));
        }
        assert_eq!(m.filtered_indices().len(), 3);
    }

    #[test]
    fn enter_on_selection_returns_attach_command() {
        let mut m = QuickJumpModal::new(vec![row("bosun-work-1", "Work", "claude", "/tmp")]);
        match m.handle(key(KeyCode::Enter)) {
            ModalResult::Close(Some(Command::Attach { name })) => {
                assert_eq!(name, "bosun-work-1");
            }
            _ => panic!("expected Close(Some(Command::Attach))"),
        }
    }

    #[test]
    fn nav_keys_clamp_at_bounds() {
        let mut m = QuickJumpModal::new(vec![
            row("bosun-a-1", "alpha", "claude", "/tmp"),
            row("bosun-b-1", "beta", "claude", "/tmp"),
        ]);
        assert_eq!(m.selected_row().unwrap().display, "alpha");
        m.handle(key(KeyCode::Down));
        assert_eq!(m.selected_row().unwrap().display, "beta");
        m.handle(key(KeyCode::Down));
        // Clamps at end.
        assert_eq!(m.selected_row().unwrap().display, "beta");
        m.handle(key(KeyCode::Up));
        assert_eq!(m.selected_row().unwrap().display, "alpha");
        m.handle(key(KeyCode::Up));
        // Clamps at start.
        assert_eq!(m.selected_row().unwrap().display, "alpha");
    }

    #[test]
    fn typing_resets_selection_to_top_match() {
        let mut m = QuickJumpModal::new(vec![
            row("bosun-a-1", "alpha", "claude", "/tmp"),
            row("bosun-b-1", "beta", "claude", "/tmp"),
        ]);
        m.handle(key(KeyCode::Down));
        assert_eq!(m.selected, 1);
        // Typing a character that still matches both — selection goes
        // back to 0 (the new top match).
        m.handle(key(KeyCode::Char('a')));
        assert_eq!(m.selected, 0);
    }

    #[test]
    fn esc_closes_with_no_command() {
        let mut m = QuickJumpModal::new(vec![row("bosun-a-1", "alpha", "claude", "/tmp")]);
        assert!(matches!(
            m.handle(key(KeyCode::Esc)),
            ModalResult::Close(None)
        ));
    }
}
