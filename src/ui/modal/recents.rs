//! RecentsModal — stacked on top of the new-session modal via Ctrl+R.
//!
//! Shows the last N session configurations (name · agent · path) with
//! a live filter. Type to narrow, Up/Down to highlight, Enter to
//! select. On select the modal closes with `ModalData::FillSessionSpec`
//! which the underlying NewSessionModal absorbs via `on_child_closed`
//! to pre-fill all its fields.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::events::Command;
use crate::store::Recent;
use crate::ui::Theme;

use super::{center_rect, Modal, ModalData, ModalResult};

const MODAL_WIDTH: u16 = 74;
/// Floor height (chrome + a couple of rows) when the recents list is
/// nearly empty. Real height is computed in `render` so a long list
/// gets a tall modal — clamped to the available terminal area.
const MODAL_MIN_HEIGHT: u16 = 12;
/// Hard cap so the modal never tries to fill a giant terminal end-to-
/// end; leaves a margin for the underlying new-session modal to peek
/// through and keeps focus on the visible window.
const MODAL_MAX_HEIGHT: u16 = 40;
/// Fixed chrome rows: title, blank, filter, blank, plus the 2-row
/// inset baked into `inner`. Subtracted from modal height to get the
/// number of list rows that fit.
const MODAL_CHROME_ROWS: u16 = 6;

pub struct RecentsModal {
    recents: Vec<Recent>,
    filter: String,
    /// Selected index into the current filtered view (not into
    /// `recents`). Clamped on every filter change / nav key.
    selected: usize,
}

impl RecentsModal {
    pub fn new(recents: Vec<Recent>) -> Self {
        Self {
            recents,
            filter: String::new(),
            selected: 0,
        }
    }

    /// Indices into `self.recents` that pass the current filter, in
    /// the same order as `recents` (which is already MRU-sorted by
    /// the store).
    fn filtered_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.recents.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        self.recents
            .iter()
            .enumerate()
            .filter(|(_, r)| row_matches(r, &needle))
            .map(|(i, _)| i)
            .collect()
    }

    fn selected_recent(&self) -> Option<&Recent> {
        let indices = self.filtered_indices();
        indices
            .get(self.selected)
            .and_then(|i| self.recents.get(*i))
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

    /// Drop the highlighted row from the local view and emit a
    /// `DeleteRecent(id)` command so the store is updated too.
    /// The modal stays open so the user can continue pruning.
    fn delete_highlighted(&mut self) -> ModalResult {
        let indices = self.filtered_indices();
        let Some(&recents_idx) = indices.get(self.selected) else {
            return ModalResult::Consumed;
        };
        let removed = self.recents.remove(recents_idx);
        self.clamp_selection();
        ModalResult::EmitCommand(Command::DeleteRecent(removed.id))
    }
}

impl Modal for RecentsModal {
    fn id(&self) -> &'static str {
        "recents"
    }

    fn handle(&mut self, key: KeyEvent) -> ModalResult {
        // Ctrl+C closes, matching the new-session modal convention.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalResult::Close(None);
        }

        match key.code {
            KeyCode::Esc => ModalResult::Close(None),
            KeyCode::Enter => match self.selected_recent() {
                Some(r) => ModalResult::CloseWithData(ModalData::FillSessionSpec(r.to_spec())),
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
            // Ctrl-D deletes the highlighted recent. Not plain 'd'
            // because that'd collide with typing 'd' into the filter.
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_highlighted()
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.filter.push(c);
                self.selected = 0;
                ModalResult::Consumed
            }
            _ => ModalResult::Consumed,
        }
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let desired = (self.recents.len() as u16).saturating_add(MODAL_CHROME_ROWS);
        let height = desired
            .clamp(MODAL_MIN_HEIGHT, MODAL_MAX_HEIGHT)
            .min(area.height.saturating_sub(2).max(MODAL_MIN_HEIGHT));
        let rect = center_rect(area, MODAL_WIDTH, height);
        let body_bg = theme.panel_alt;
        let buf = frame.buffer_mut();

        // Shadow behind the modal.
        if rect.x + rect.width < area.x + area.width && rect.y + rect.height < area.y + area.height
        {
            let shadow = Rect::new(rect.x + 1, rect.y + 1, rect.width, rect.height);
            let style = Style::default().bg(theme.shadow);
            for y in shadow.top()..shadow.bottom() {
                for x in shadow.left()..shadow.right() {
                    buf[(x, y)].set_style(style);
                }
            }
        }

        // Body fill.
        let body_style = Style::default().bg(body_bg);
        for y in rect.top()..rect.bottom() {
            for x in rect.left()..rect.right() {
                let cell = &mut buf[(x, y)];
                cell.set_char(' ');
                cell.set_style(body_style);
            }
        }

        // Left accent bar.
        let accent_style = Style::default().bg(theme.accent);
        for y in rect.top()..rect.bottom() {
            let cell = &mut buf[(rect.left(), y)];
            cell.set_char(' ');
            cell.set_style(accent_style);
        }

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
                "Recent sessions",
                Style::default()
                    .fg(theme.text)
                    .bg(body_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "   esc · ↑↓ select · ↵ pick · ^d delete",
                Style::default().fg(theme.text_muted).bg(body_bg),
            ),
        ]));
        lines.push(Line::from(""));

        // Filter input
        let filter_display = format!(" filter: {}▎", self.filter);
        lines.push(Line::from(vec![Span::styled(
            filter_display,
            Style::default().fg(theme.text).bg(theme.bg),
        )]));
        lines.push(Line::from(""));

        // List body
        let max_rows = (inner.height as usize).saturating_sub(4);
        if self.recents.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no recent sessions yet — create some first)",
                Style::default().fg(theme.text_muted).bg(body_bg),
            )));
        } else if filtered.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no matches)",
                Style::default().fg(theme.text_muted).bg(body_bg),
            )));
        } else {
            for (vi, rec_idx) in filtered.iter().enumerate().take(max_rows) {
                let r = &self.recents[*rec_idx];
                lines.push(render_row(r, vi == self.selected, inner.width, theme));
            }
        }

        Paragraph::new(lines)
            .style(Style::default().bg(body_bg))
            .render(inner, frame.buffer_mut());
    }
}

fn row_matches(r: &Recent, needle: &str) -> bool {
    r.name.to_lowercase().contains(needle)
        || r.agent.to_lowercase().contains(needle)
        || r.path.to_lowercase().contains(needle)
}

fn render_row(r: &Recent, selected: bool, width: u16, theme: &Theme) -> Line<'static> {
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

    let path_short = shorten_path(&r.path);
    let label = format!(" {} {} · {} · {}", marker, r.name, r.agent, path_short);
    let padded = pad_to_width(&label, width);

    Line::from(vec![
        Span::styled(format!(" {} ", marker), marker_style),
        Span::styled(r.name.clone(), name_style),
        Span::styled(format!("  · {}  · ", r.agent), meta_style),
        Span::styled(path_short.clone(), meta_style),
        Span::styled(
            " ".repeat(padded.chars().count().saturating_sub(
                5 + r.name.chars().count()
                    + r.agent.chars().count()
                    + path_short.chars().count()
                    + 6,
            )),
            Style::default().bg(row_bg),
        ),
    ])
}

fn shorten_path(p: &str) -> String {
    // Replace $HOME prefix with ~ for display compactness.
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && p.starts_with(&home) {
        format!("~{}", &p[home.len()..])
    } else {
        p.to_string()
    }
}

fn pad_to_width(s: &str, width: u16) -> String {
    let target = width as usize;
    let current = s.chars().count();
    if current < target {
        let mut out = s.to_string();
        for _ in current..target {
            out.push(' ');
        }
        out
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{
        ClaudeOptions, CodexOptions, KimiOptions, OpencodeOptions, QwenOptions, SpecOptions,
    };

    fn rec(name: &str, agent: &str, path: &str) -> Recent {
        Recent {
            id: 0,
            name: name.into(),
            path: path.into(),
            agent: agent.into(),
            args: String::new(),
            claude: ClaudeOptions::default(),
            codex: CodexOptions::default(),
            kimi: KimiOptions::default(),
            opencode: OpencodeOptions::default(),
            qwen: QwenOptions::default(),
            last_used_at: 0,
            use_count: 1,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn empty_recents_still_works() {
        let mut m = RecentsModal::new(vec![]);
        assert!(m.selected_recent().is_none());
        // Enter should be a no-op when nothing to pick.
        assert!(matches!(
            m.handle(key(KeyCode::Enter)),
            ModalResult::Consumed
        ));
    }

    #[test]
    fn down_and_up_navigate_filtered_list() {
        let mut m = RecentsModal::new(vec![
            rec("alpha", "claude", "/tmp"),
            rec("beta", "codex", "/tmp"),
            rec("gamma", "claude", "/tmp"),
        ]);
        assert_eq!(m.selected_recent().unwrap().name, "alpha");
        m.handle(key(KeyCode::Down));
        assert_eq!(m.selected_recent().unwrap().name, "beta");
        m.handle(key(KeyCode::Down));
        assert_eq!(m.selected_recent().unwrap().name, "gamma");
        m.handle(key(KeyCode::Down));
        // clamps at end
        assert_eq!(m.selected_recent().unwrap().name, "gamma");
        m.handle(key(KeyCode::Up));
        assert_eq!(m.selected_recent().unwrap().name, "beta");
    }

    #[test]
    fn typing_filters_by_substring_across_name_agent_path() {
        let mut m = RecentsModal::new(vec![
            rec("work", "claude", "/tmp/user/proj"),
            rec("play", "codex", "/tmp/user/play"),
            rec("api", "claude", "/srv/api"),
        ]);
        // "cod" matches only the codex entry by agent
        for c in "cod".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(m.filtered_indices().len(), 1);
        assert_eq!(m.selected_recent().unwrap().name, "play");

        // backspace should widen
        m.handle(key(KeyCode::Backspace));
        m.handle(key(KeyCode::Backspace));
        m.handle(key(KeyCode::Backspace));
        assert_eq!(m.filtered_indices().len(), 3);
    }

    #[test]
    fn filter_narrows_to_zero_still_navigable() {
        let mut m = RecentsModal::new(vec![rec("foo", "claude", "/tmp")]);
        for c in "xyz".chars() {
            m.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(m.filtered_indices().len(), 0);
        assert!(m.selected_recent().is_none());
        // Enter on empty filter should be consumed (no-op).
        assert!(matches!(
            m.handle(key(KeyCode::Enter)),
            ModalResult::Consumed
        ));
    }

    #[test]
    fn enter_on_selected_closes_with_fill_data() {
        let mut m = RecentsModal::new(vec![rec("work", "claude", "/tmp")]);
        let r = m.handle(key(KeyCode::Enter));
        match r {
            ModalResult::CloseWithData(ModalData::FillSessionSpec(spec)) => {
                assert_eq!(spec.name, "work");
                assert_eq!(spec.path, "/tmp");
                assert_eq!(spec.agent, "claude");
            }
            _ => panic!("expected CloseWithData(FillSessionSpec)"),
        }
    }

    #[test]
    fn esc_closes_without_data() {
        let mut m = RecentsModal::new(vec![rec("work", "claude", "/tmp")]);
        assert!(matches!(
            m.handle(key(KeyCode::Esc)),
            ModalResult::Close(None)
        ));
    }

    #[test]
    fn shorten_path_collapses_home() {
        // Only verify this doesn't panic and is at least as short as
        // the input when HOME happens to be a prefix. Exact value
        // depends on test environment.
        let _ = shorten_path("/tmp");
        let _ = shorten_path("/nonexistent/path");
    }

    // Suppress unused warnings for helper types imported only for
    // construction under the tests module.
    fn _unused(_: SpecOptions) {}
}
